//! Common native RPC result and error materialization.

use crate::core::invocation::with_captured_context;
use crate::http::ResponseInfo;
use crate::state::DispatchResult;

use super::procedure::{
    FailureStage, Invocation, InvocationError, InvokeFailure, attempt, property,
};

pub(crate) fn error_value(message: String, status: u16, code: &str) -> DispatchResult {
    DispatchResult::ErrorValue {
        message,
        name: "Error".into(),
        stack: None,
        status,
        code: Some(code.into()),
        details_json: None,
        retryable: None,
    }
}

pub(crate) fn failure(scope: &mut v8::PinScope, failure: InvokeFailure) -> DispatchResult {
    with_captured_context(scope, &failure.frame, |scope| {
        if failure.stage == FailureStage::Input {
            return validation_error(scope, &failure.error, true);
        }
        exception(scope, &failure.error)
    })
}

pub(super) fn exception(scope: &mut v8::PinScope, error: &InvocationError) -> DispatchResult {
    match error {
        InvocationError::JavaScript(error) => {
            // Error getters are creator code too. A secondary exception must
            // not escape into another pending request's continuation.
            v8::tc_scope!(let tc, scope);
            let error = v8::Local::new(tc, error);
            let result = crate::dispatch::v8_exception_to_error_value(tc, error);
            if tc.has_caught() {
                error_value("could not read procedure error".into(), 500, "INTERNAL")
            } else {
                result
            }
        }
        InvocationError::InvalidTarget(message) => error_value(message.clone(), 500, "INTERNAL"),
        InvocationError::Engine(message) => DispatchResult::Error((*message).into()),
    }
}

fn validation_error(
    scope: &mut v8::PinScope,
    error: &InvocationError,
    input: bool,
) -> DispatchResult {
    let issues = match error {
        InvocationError::JavaScript(error) => {
            let value = v8::Local::new(scope, error);
            let extracted = attempt(scope, |scope| {
                let object = v8::Local::<v8::Object>::try_from(value).ok()?;
                for name in ["issues", "errors"] {
                    let key = v8::String::new(scope, name)?;
                    let value = object.get(scope, key.into())?;
                    if value.is_array() {
                        return v8::json::stringify(scope, value)
                            .map(|json| json.to_rust_string_lossy(scope));
                    }
                }
                Some("[]".into())
            });
            extracted.unwrap_or_else(|_| "[]".into())
        }
        _ => "[]".into(),
    };
    let mut result = if input {
        error_value("Invalid input".into(), 400, "INVALID_ARGUMENT")
    } else {
        error_value("Invalid handler output".into(), 500, "INTERNAL")
    };
    if let DispatchResult::ErrorValue { details_json, .. } = &mut result {
        *details_json = Some(format!("{{\"issues\":{issues}}}"));
    }
    result
}

pub(crate) fn classify(
    scope: &mut v8::PinScope,
    invocation: Invocation,
    value: &v8::Global<v8::Value>,
    validate_output: bool,
) -> DispatchResult {
    invocation.with_frame(scope, |scope| {
        let value = v8::Local::new(scope, value);
        let result = classify_inner(scope, &invocation, value, validate_output);
        match result {
            Ok(result) => result,
            Err(error) => exception(scope, &error),
        }
    })
}

fn classify_inner<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    invocation: &Invocation,
    value: v8::Local<'s, v8::Value>,
    validate_output: bool,
) -> Result<DispatchResult, InvocationError> {
    if value.is_object() {
        let response = attempt(scope, |scope| {
            Some(crate::http::looks_like_response(scope, value))
        })?;
        if response {
            return attempt(scope, |scope| {
                Some(crate::http::inspect_response(scope, value))
            })?
            .map(DispatchResult::HttpResponse)
            .map_err(InvocationError::InvalidTarget);
        }
        let object = v8::Local::<v8::Object>::try_from(value).unwrap();
        let method = attempt(scope, |scope| {
            let key = v8::Symbol::get_async_iterator(scope);
            object.get(scope, key.into())
        })?;
        if method.is_function() {
            let next = property(scope, object, "next")?;
            if let Ok(next) = v8::Local::<v8::Function>::try_from(next) {
                return super::stream::start(scope, object, next, invocation.output_is_string)
                    .map(DispatchResult::HttpResponse);
            }
        }
    }
    if validate_output
        && let Some(output) = &invocation.output
        && let Err(error) = output.parse(scope, value)
    {
        return Ok(validation_error(scope, &error, false));
    }
    let body = crate::rpc::encode_to_bytes(scope, value)
        .map_err(|error| InvocationError::InvalidTarget(error.message))?;
    Ok(DispatchResult::HttpResponse(ResponseInfo::Complete {
        status: 200,
        headers: vec![("content-type".into(), "application/json".into())],
        body,
    }))
}

pub(crate) fn into_http(result: DispatchResult, request_id: u64) -> Result<ResponseInfo, String> {
    match result {
        DispatchResult::HttpResponse(info) => Ok(info),
        DispatchResult::ErrorValue {
            message,
            name,
            stack,
            status,
            code,
            details_json,
            retryable,
        } => {
            let extras = crate::dispatch::ErrorExtras {
                stack: stack.as_deref(),
                code: code.as_deref(),
                details_json: details_json.as_deref(),
                retryable,
            };
            Ok(ResponseInfo::Complete {
                status,
                headers: vec![("content-type".into(), "application/json".into())],
                body: crate::dispatch::build_error_body(
                    status, request_id, &message, &name, extras,
                )
                .into_bytes(),
            })
        }
        DispatchResult::Error(message) => Err(message),
    }
}
