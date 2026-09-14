//! A retained procedure iterator adapted to the runtime response forwarder.
//! Callback data is owned by V8, so it does not root the iterator through a
//! Rust capture cycle while waiting for a creator promise.

use crate::core::invocation::{capture_context, with_captured_context};
use crate::http::ResponseInfo;

use super::procedure::{InvocationError, attempt, property};

const ITERATOR: u32 = 0;
const NEXT: u32 = 1;
const STRING_OUTPUT: u32 = 2;
const FINISHED: u32 = 3;
const FRAME: u32 = 4;
const RETURNED: u32 = 5;

pub(super) fn start<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    iterator: v8::Local<'s, v8::Object>,
    next: v8::Local<'s, v8::Function>,
    output_is_string: bool,
) -> Result<ResponseInfo, InvocationError> {
    let frame = capture_context(scope);
    let frame = v8::Local::new(scope, frame);
    let hint = v8::Boolean::new(scope, output_is_string);
    let finished = v8::Boolean::new(scope, false);
    let data = v8::Array::new_with_elements(
        scope,
        &[
            iterator.into(),
            next.into(),
            hint.into(),
            finished.into(),
            frame,
            finished.into(),
        ],
    );
    let reader = v8::Object::new(scope);
    let read = v8::FunctionTemplate::builder(read_callback)
        .data(data.into())
        .build(scope)
        .get_function(scope)
        .ok_or(InvocationError::Engine(
            "could not create RPC stream reader",
        ))?;
    let cancel = v8::FunctionTemplate::builder(cancel_callback)
        .data(data.into())
        .build(scope)
        .get_function(scope)
        .ok_or(InvocationError::Engine(
            "could not create RPC stream cancellation",
        ))?;
    for (name, function) in [("read", read), ("cancel", cancel)] {
        let key = v8::String::new(scope, name).unwrap();
        reader.create_data_property(scope, key.into(), function.into());
    }
    let stream_id = crate::streams::response_forwarder::begin_forward_reader(
        scope,
        reader,
        crate::streams::response_forwarder::ForwardMode::Rpc,
    )
    .map_err(InvocationError::InvalidTarget)?;
    Ok(ResponseInfo::Stream {
        status: 200,
        headers: vec![
            ("content-type".into(), "text/event-stream".into()),
            ("cache-control".into(), "no-cache, no-transform".into()),
            ("x-accel-buffering".into(), "no".into()),
        ],
        stream_id,
    })
}

fn in_frame<'s, T>(
    scope: &mut v8::PinScope<'s, '_>,
    data: v8::Local<'s, v8::Array>,
    body: impl FnOnce(&mut v8::PinScope<'s, '_>) -> T,
) -> T {
    let frame = data.get_index(scope, FRAME).unwrap();
    let frame = v8::Global::new(scope, frame);
    with_captured_context(scope, &frame, body)
}

fn finished(scope: &mut v8::PinScope, data: v8::Local<v8::Array>) -> bool {
    data.get_index(scope, FINISHED)
        .is_some_and(|value| value.is_true())
}

fn finish(scope: &mut v8::PinScope, data: v8::Local<v8::Array>) {
    let value = v8::Boolean::new(scope, true);
    data.set_index(scope, FINISHED, value.into());
}

fn read_result<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    bytes: Option<&[u8]>,
) -> Option<v8::Local<'s, v8::Value>> {
    let result = v8::Object::new(scope);
    let done = v8::Boolean::new(scope, bytes.is_none());
    let key = v8::String::new(scope, "done")?;
    result.create_data_property(scope, key.into(), done.into())?;
    if let Some(bytes) = bytes {
        let value = v8::String::new_from_utf8(scope, bytes, v8::NewStringType::Normal)?;
        let key = v8::String::new(scope, "value")?;
        result.create_data_property(scope, key.into(), value.into())?;
    }
    Some(result.into())
}

fn error_result<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    data: v8::Local<'s, v8::Array>,
    error: InvocationError,
) -> Option<v8::Local<'s, v8::Value>> {
    finish(scope, data);
    let request_id = crate::core::invocation::current_context(scope)
        .and_then(|context| context.request_id)
        .unwrap_or_default();
    let error = super::response::exception(scope, &error);
    let bytes = terminal_error(error, request_id).ok()?;
    close_iterator(scope, data);
    read_result(scope, Some(&bytes))
}

fn read_callback<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    args: v8::FunctionCallbackArguments<'s>,
    mut rv: v8::ReturnValue,
) {
    let Ok(data) = v8::Local::<v8::Array>::try_from(args.data()) else {
        return;
    };
    let result = in_frame(scope, data, |scope| {
        let resolver = v8::PromiseResolver::new(scope)?;
        let promise = resolver.get_promise(scope);
        promise.mark_as_handled();
        if finished(scope, data) {
            let result = read_result(scope, None)?;
            resolver.resolve(scope, result)?;
            return Some(promise);
        }
        let result = attempt(scope, |scope| {
            let iterator = data.get_index(scope, ITERATOR)?;
            let next = v8::Local::<v8::Function>::try_from(data.get_index(scope, NEXT)?).ok()?;
            next.call(scope, iterator, &[])
        });
        let value = match result {
            Ok(value) => value,
            Err(error) => {
                let result = error_result(scope, data, error)?;
                resolver.resolve(scope, result)?;
                return Some(promise);
            }
        };
        resolver.resolve(scope, value)?;
        let on_result = v8::FunctionTemplate::builder(result_callback)
            .data(data.into())
            .build(scope)
            .get_function(scope)?;
        let on_error = v8::FunctionTemplate::builder(error_callback)
            .data(data.into())
            .build(scope)
            .get_function(scope)?;
        promise.then2(scope, on_result, on_error)
    });
    if let Some(result) = result {
        rv.set(result.into());
    }
}

fn result_callback<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    args: v8::FunctionCallbackArguments<'s>,
    mut rv: v8::ReturnValue,
) {
    let Ok(data) = v8::Local::<v8::Array>::try_from(args.data()) else {
        return;
    };
    let result = in_frame(scope, data, |scope| {
        if finished(scope, data) {
            return read_result(scope, None);
        }
        let encoded = encode_step(scope, data, args.get(0));
        match encoded {
            Ok(bytes) => read_result(scope, Some(&bytes)),
            Err(error) => error_result(scope, data, error),
        }
    });
    if let Some(result) = result {
        rv.set(result);
    }
}

fn encode_step<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    data: v8::Local<'s, v8::Array>,
    step: v8::Local<'s, v8::Value>,
) -> Result<Vec<u8>, InvocationError> {
    let step = v8::Local::<v8::Object>::try_from(step)
        .map_err(|_| InvocationError::InvalidTarget("iterator result must be an object".into()))?;
    if property(scope, step, "done")?.boolean_value(scope) {
        finish(scope, data);
        close_iterator(scope, data);
        return Ok(b"d:{}\n".to_vec());
    }
    let value = property(scope, step, "value")?;
    let text = data
        .get_index(scope, STRING_OUTPUT)
        .is_some_and(|value| value.is_true())
        || value.is_string();
    let value = if text {
        attempt(scope, |scope| value.to_string(scope).map(Into::into))?
    } else {
        value
    };
    let json = attempt(scope, |scope| {
        if value.is_undefined() || value.is_function() || value.is_symbol() {
            return Some("undefined".into());
        }
        v8::json::stringify(scope, value).map(|json| json.to_rust_string_lossy(scope))
    })?;
    Ok(if text {
        format!("0:{json}\n")
    } else {
        format!("2:[{json}]\n")
    }
    .into_bytes())
}

fn error_callback<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    args: v8::FunctionCallbackArguments<'s>,
    mut rv: v8::ReturnValue,
) {
    let Ok(data) = v8::Local::<v8::Array>::try_from(args.data()) else {
        return;
    };
    let error = InvocationError::JavaScript(v8::Global::new(scope, args.get(0)));
    let result = in_frame(scope, data, |scope| {
        if finished(scope, data) {
            read_result(scope, None)
        } else {
            error_result(scope, data, error)
        }
    });
    if let Some(result) = result {
        rv.set(result);
    }
}

fn cancel_callback<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    args: v8::FunctionCallbackArguments<'s>,
    mut rv: v8::ReturnValue,
) {
    let Ok(data) = v8::Local::<v8::Array>::try_from(args.data()) else {
        return;
    };
    let result = in_frame(scope, data, |scope| {
        finish(scope, data);
        close_iterator(scope, data)
    });
    if let Some(result) = result {
        rv.set(result.into());
    }
}

fn close_iterator<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    data: v8::Local<'s, v8::Array>,
) -> Option<v8::Local<'s, v8::Promise>> {
    if data
        .get_index(scope, RETURNED)
        .is_some_and(|value| value.is_true())
    {
        return None;
    }
    let returned = v8::Boolean::new(scope, true);
    data.set_index(scope, RETURNED, returned.into());
    attempt(scope, |scope| {
        let iterator = v8::Local::<v8::Object>::try_from(data.get_index(scope, ITERATOR)?).ok()?;
        let key = v8::String::new(scope, "return")?;
        let method = v8::Local::<v8::Function>::try_from(iterator.get(scope, key.into())?).ok()?;
        let result = method.call(scope, iterator.into(), &[])?;
        let resolver = v8::PromiseResolver::new(scope)?;
        resolver.get_promise(scope).mark_as_handled();
        resolver.resolve(scope, result)?;
        Some(resolver.get_promise(scope))
    })
    .ok()
}

pub(crate) fn terminal_error(
    result: crate::state::DispatchResult,
    request_id: u64,
) -> Result<Vec<u8>, String> {
    let result = match result {
        crate::state::DispatchResult::Error(message) => {
            super::response::error_value(message, 500, "INTERNAL")
        }
        result => result,
    };
    let ResponseInfo::Complete { body, .. } = super::response::into_http(result, request_id)?
    else {
        return Err("RPC error did not produce an error envelope".into());
    };
    let mut bytes = b"e:".to_vec();
    bytes.extend_from_slice(&body);
    bytes.extend_from_slice(b"\nd:{}\n");
    Ok(bytes)
}
