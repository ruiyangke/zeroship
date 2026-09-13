//! Native `zeroship` exports read isolate and continuation state directly.

#![allow(unsafe_code)]

use crate::rpc::{ProcedureKind, with_kind};
use crate::state::SharedState;

#[derive(Clone, Copy)]
enum Export {
    Env,
    WaitUntil,
    GetRequest,
    GetRequestContext,
    Run(ProcedureKind),
    ContextField(&'static str),
}

const EXPORTS: &[(&str, Export)] = &[
    ("env", Export::Env),
    ("waitUntil", Export::WaitUntil),
    ("getRequest", Export::GetRequest),
    ("getRequestContext", Export::GetRequestContext),
    ("runQuery", Export::Run(ProcedureKind::Query)),
    ("runMutation", Export::Run(ProcedureKind::Mutation)),
    ("currentUser", Export::ContextField("user")),
    ("currentRequestId", Export::ContextField("requestId")),
    ("currentTraceId", Export::ContextField("traceId")),
    ("currentSignal", Export::ContextField("signal")),
    ("currentHeaders", Export::ContextField("headers")),
    (
        "currentIdempotencyKey",
        Export::ContextField("idempotencyKey"),
    ),
];

pub(super) fn synthetic_module<'s>(scope: &mut v8::PinScope<'s, '_>) -> v8::Local<'s, v8::Module> {
    let name = v8::String::new(scope, "zeroship").unwrap();
    let exports: Vec<_> = EXPORTS
        .iter()
        .map(|(name, _)| v8::String::new(scope, name).unwrap())
        .collect();
    v8::Module::create_synthetic_module(scope, name, &exports, evaluate)
}

fn evaluate<'s>(
    context: v8::Local<'s, v8::Context>,
    module: v8::Local<'s, v8::Module>,
) -> Option<v8::Local<'s, v8::Value>> {
    v8::callback_scope!(unsafe scope, context);
    let env = scope
        .get_slot::<SharedState>()
        .and_then(|state| state.borrow().env_obj.clone());
    let Some(env) = env else {
        throw_error(scope, "zeroship: environment is not initialized");
        return None;
    };
    for (index, (name, export)) in EXPORTS.iter().enumerate() {
        let name = v8::String::new(scope, name)?;
        let value = match export {
            Export::Env => v8::Local::new(scope, &env).into(),
            _ => {
                let data = v8::Integer::new_from_unsigned(scope, index as u32);
                let function = v8::Function::builder(export_callback)
                    .data(data.into())
                    .build(scope)?;
                function.set_name(name);
                function.into()
            }
        };
        module.set_synthetic_module_export(scope, name, value)?;
    }
    Some(v8::undefined(scope).into())
}

fn export_callback<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    args: v8::FunctionCallbackArguments<'s>,
    mut rv: v8::ReturnValue,
) {
    let index = args
        .data()
        .uint32_value(scope)
        .expect("native export index");
    let (name, export) = EXPORTS[index as usize];
    match export {
        Export::Env => unreachable!("env is exported as an object"),
        Export::WaitUntil => wait_until(scope, args.get(0)),
        Export::GetRequest => {
            let state = scope
                .get_slot::<SharedState>()
                .expect("runtime state")
                .clone();
            let request = super::invocation::current_request_id(scope, &state)
                .and_then(|id| state.borrow().request_by_id.get(&id).cloned());
            if let Some(request) = request {
                rv.set(v8::Local::new(scope, request).into());
            } else {
                throw_error(
                    scope,
                    "getRequest called outside a fetch handler (RPC fast-path has no Request)",
                );
            }
        }
        Export::GetRequestContext => {
            if let Some(context) = crate::rpc::dispatch::current_rpc_ctx_object(scope) {
                rv.set(context.into());
            }
        }
        Export::ContextField(field) => {
            let Some(context) = crate::rpc::dispatch::current_rpc_ctx_object(scope) else {
                throw_error(scope, &format!("{name}: called outside a request handler"));
                return;
            };
            let key = v8::String::new(scope, field).unwrap();
            if let Some(value) = context.get(scope, key.into()) {
                rv.set(value);
            }
        }
        Export::Run(kind) => {
            if let Some(promise) = run(scope, kind, args.get(0), args.get(1)) {
                rv.set(promise.into());
            }
        }
    }
}

fn wait_until(scope: &mut v8::PinScope, value: v8::Local<v8::Value>) {
    let Ok(promise) = v8::Local::<v8::Promise>::try_from(value) else {
        let message = v8::String::new(scope, "waitUntil expects a Promise").unwrap();
        let error = v8::Exception::type_error(scope, message);
        scope.throw_exception(error);
        return;
    };
    let state = scope
        .get_slot::<SharedState>()
        .expect("runtime state")
        .clone();
    if let Some(id) = super::invocation::current_request_id(scope, &state) {
        let promise = v8::Global::new(scope, promise);
        state.borrow_mut().register_wait_until(id, promise);
    }
}

fn run<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    kind: ProcedureKind,
    procedure: v8::Local<'s, v8::Value>,
    input: v8::Local<'s, v8::Value>,
) -> Option<v8::Local<'s, v8::Promise>> {
    let resolver = v8::PromiseResolver::new(scope)?;
    let promise = resolver.get_promise(scope);
    let Ok(procedure) = v8::Local::<v8::Function>::try_from(procedure) else {
        let message = v8::String::new(
            scope,
            "runQuery/runMutation: first arg must be a procedure function",
        )?;
        let error = v8::Exception::type_error(scope, message);
        resolver.reject(scope, error);
        return Some(promise);
    };
    with_kind(scope, kind, |scope| {
        v8::tc_scope!(let scope, scope);
        let receiver = v8::undefined(scope).into();
        match procedure.call(scope, receiver, &[input]) {
            Some(result) => {
                // Adopt promises and thenables while the inner frame is active:
                // their assimilation jobs must inherit that procedure's kind.
                resolver.resolve(scope, result);
            }
            None => {
                if let Some(error) = scope.exception() {
                    scope.reset();
                    resolver.reject(scope, error);
                }
            }
        }
    });
    Some(promise)
}

fn throw_error(scope: &mut v8::PinScope, message: &str) {
    let message = v8::String::new(scope, message).unwrap();
    let error = v8::Exception::error(scope, message);
    scope.throw_exception(error);
}
