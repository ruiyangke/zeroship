use super::super::call::{CallProgress, RpcCall};
use super::*;

fn with_scope(test: impl FnOnce(&mut v8::PinScope)) {
    crate::init_v8();
    let mut isolate = v8::Isolate::new(Default::default());
    isolate.set_microtasks_policy(v8::MicrotasksPolicy::Explicit);
    v8::scope!(let scope, &mut isolate);
    let context = v8::Context::new(scope, Default::default());
    let scope = &mut v8::ContextScope::new(scope, context);
    let key = v8::String::new(scope, "currentKind").unwrap();
    let function = v8::Function::new(scope, current_kind).unwrap();
    context
        .global(scope)
        .set(scope, key.into(), function.into());
    let key = v8::String::new(scope, "currentContext").unwrap();
    let function = v8::Function::new(scope, current_context).unwrap();
    context
        .global(scope)
        .set(scope, key.into(), function.into());
    test(scope);
}

fn current_kind(
    scope: &mut v8::PinScope,
    _args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let kind = format!("{:?}", crate::rpc::current_kind(scope));
    rv.set(v8::String::new(scope, &kind).unwrap().into());
}

fn current_context(
    scope: &mut v8::PinScope,
    _args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    if let Some(context) = crate::rpc::dispatch::current_rpc_ctx_object(scope) {
        rv.set(context.into());
    }
}

fn eval<'s>(scope: &mut v8::PinScope<'s, '_>, source: &str) -> v8::Local<'s, v8::Value> {
    let source = v8::String::new(scope, source).unwrap();
    v8::Script::compile(scope, source, None)
        .unwrap()
        .run(scope)
        .unwrap()
}

fn json(scope: &mut v8::PinScope, value: v8::Local<v8::Value>) -> serde_json::Value {
    let json = v8::json::stringify(scope, value)
        .unwrap()
        .to_rust_string_lossy(scope);
    serde_json::from_str(&json).unwrap()
}

fn ready(registry: &ProcedureRegistry, scope: &mut v8::PinScope, name: &str) -> Procedure {
    match registry.resolve(scope, name).unwrap() {
        Resolution::Ready(procedure) => procedure,
        _ => panic!("expected resolved procedure {name}"),
    }
}

fn pending(
    registry: &ProcedureRegistry,
    scope: &mut v8::PinScope,
    name: &str,
) -> v8::Global<v8::Promise> {
    match registry.resolve(scope, name).unwrap() {
        Resolution::Pending(promise) => promise,
        _ => panic!("expected pending procedure {name}"),
    }
}

#[test]
fn snapshots_keep_string_ids_and_ignore_inherited_targets() {
    with_scope(|scope| {
        let dictionary = eval(
            scope,
            r#"
            globalThis.dictionary = Object.assign(Object.create({inherited() {}}), {
                'notes.list': () => 'original',
                'constructor': () => 'own constructor',
            });
            dictionary
        "#,
        );
        let first = ProcedureRegistry::snapshot(scope, dictionary).unwrap();
        assert!(matches!(
            first.resolve(scope, "inherited").unwrap(),
            Resolution::Missing
        ));
        assert!(matches!(
            first.resolve(scope, "toString").unwrap(),
            Resolution::Missing
        ));
        eval(scope, "dictionary['notes.list'] = () => 'replacement'");
        let second = ProcedureRegistry::snapshot(scope, dictionary).unwrap();
        for (registry, expected) in [(&first, "original"), (&second, "replacement")] {
            let procedure = ready(registry, scope, "notes.list");
            let undefined = v8::undefined(scope).into();
            let invoked = procedure.invoke(scope, undefined, undefined).unwrap();
            let promise = v8::Local::new(scope, invoked.promise);
            let result = promise.result(scope);
            assert_eq!(result.to_rust_string_lossy(scope), expected);
        }
        let _ = ready(&first, scope, "constructor");
    });
}

#[test]
fn snapshot_rejects_alternate_dispatchers_and_malformed_entries() {
    with_scope(|scope| {
        for source in [
            "(() => {})",
            "[]",
            "null",
            "({bad: 1})",
            "({bad: {load: 1}})",
            "({'': () => {}})",
            "({bad: Object.assign(() => {}, {config: {kind: 'other'}})})",
        ] {
            let value = eval(scope, source);
            assert!(
                matches!(
                    ProcedureRegistry::snapshot(scope, value),
                    Err(InvocationError::InvalidTarget(_))
                ),
                "{source}"
            );
        }
        let value = eval(scope, "({})");
        let empty = ProcedureRegistry::snapshot(scope, value).unwrap();
        assert!(matches!(
            empty.resolve(scope, "absent").unwrap(),
            Resolution::Missing
        ));
    });
}

#[test]
fn validation_preserves_receiver_and_transforms_input_under_the_procedure_frame() {
    with_scope(|scope| {
        let dictionary = eval(
            scope,
            r#"
            ({test: Object.assign(function(input, ctx) {
                'use strict';
                return { input, ctx, kind: currentKind(), receiver: this === undefined };
            }, {config: {kind: 'query', input: {
                prefix: 'validated:', parse(input) {
                    if (currentKind() !== 'Some(Query)') throw new Error('wrong validator frame');
                    return this.prefix + input;
                },
            }}})})
        "#,
        );
        let registry = ProcedureRegistry::snapshot(scope, dictionary).unwrap();
        let procedure = ready(&registry, scope, "test");
        let input = v8::String::new(scope, "input").unwrap().into();
        let context = eval(scope, "({identity: 'request'})");
        let invoked = procedure.invoke(scope, input, context).unwrap();
        let promise = v8::Local::new(scope, &invoked.promise);
        let result = promise.result(scope);
        assert_eq!(
            json(scope, result),
            serde_json::json!({
                "input":"validated:input", "ctx":{"identity":"request"},
                "kind":"Some(Query)", "receiver":true,
            })
        );
        assert_eq!(crate::rpc::current_kind(scope), None);
        invoked.with_frame(scope, |scope| {
            assert_eq!(crate::rpc::current_kind(scope), Some(ProcedureKind::Query))
        });
        assert_eq!(crate::rpc::current_kind(scope), None);
    });
}

#[test]
fn validation_and_handler_failures_preserve_original_exceptions() {
    with_scope(|scope| {
        let dictionary = eval(
            scope,
            r#"
            globalThis.marker = {code:'CUSTOM', details:{reason:'original'}};
            globalThis.called = false;
            ({input: Object.assign(() => { called = true; }, {config:{input:{parse() {throw marker;}}}}),
              handler() {throw marker;}})
        "#,
        );
        let registry = ProcedureRegistry::snapshot(scope, dictionary).unwrap();
        let undefined = v8::undefined(scope).into();
        let error = match ready(&registry, scope, "input").invoke(scope, undefined, undefined) {
            Err(InvokeFailure {
                stage: FailureStage::Input,
                error: InvocationError::JavaScript(error),
                ..
            }) => error,
            _ => panic!("expected original input error"),
        };
        let marker = eval(scope, "marker");
        assert!(v8::Local::new(scope, error).strict_equals(marker));
        assert!(eval(scope, "called === false").is_true());
        let error = match ready(&registry, scope, "handler").invoke(scope, undefined, undefined) {
            Err(InvokeFailure {
                stage: FailureStage::Handler,
                error: InvocationError::JavaScript(error),
                ..
            }) => error,
            _ => panic!("expected original handler error"),
        };
        assert!(v8::Local::new(scope, error).strict_equals(marker));
        assert_eq!(crate::rpc::current_kind(scope), None);
    });
}

#[test]
fn lazy_resolution_shares_pending_work_and_caches_each_generation() {
    with_scope(|scope| {
        let dictionary = eval(
            scope,
            r#"
            globalThis.calls = 0;
            globalThis.finish = undefined;
            globalThis.dictionary = {lazy: {
                own: true, load() {
                    if (!this.own) throw new Error('loader receiver lost');
                    calls++;
                    return new Promise(resolve => { finish = resolve; });
                },
            }};
            dictionary
        "#,
        );
        let first = ProcedureRegistry::snapshot(scope, dictionary).unwrap();
        assert_eq!(eval(scope, "calls").int32_value(scope), Some(0));
        let a = pending(&first, scope, "lazy");
        let b = pending(&first.clone(), scope, "lazy");
        let a = v8::Local::new(scope, a);
        let b = v8::Local::new(scope, b);
        assert!(a.strict_equals(b.into()));
        assert_eq!(eval(scope, "calls").int32_value(scope), Some(1));
        eval(
            scope,
            "finish(Object.assign(() => 'loaded', {config: {kind:'mutation'}}))",
        );
        scope.perform_microtask_checkpoint();
        assert_eq!(
            ready(&first, scope, "lazy").kind,
            Some(ProcedureKind::Mutation)
        );
        assert_eq!(
            ready(&first, scope, "lazy").kind,
            Some(ProcedureKind::Mutation)
        );
        assert_eq!(eval(scope, "calls").int32_value(scope), Some(1));
        let second = ProcedureRegistry::snapshot(scope, dictionary).unwrap();
        let _ = pending(&second, scope, "lazy");
        assert_eq!(eval(scope, "calls").int32_value(scope), Some(2));
        assert_eq!(
            ready(&first, scope, "lazy").kind,
            Some(ProcedureKind::Mutation)
        );
    });
}

#[test]
fn lazy_failure_is_retained_without_retry_until_a_new_snapshot() {
    with_scope(|scope| {
        let dictionary = eval(
            scope,
            r#"
            globalThis.calls = 0;
            globalThis.marker = new Error('load failed');
            ({lazy: {load() { calls++; return Promise.reject(marker); }}})
        "#,
        );
        let registry = ProcedureRegistry::snapshot(scope, dictionary).unwrap();
        let _ = pending(&registry, scope, "lazy");
        scope.perform_microtask_checkpoint();
        for _ in 0..2 {
            let error = match registry.resolve(scope, "lazy") {
                Err(InvocationError::JavaScript(error)) => error,
                _ => panic!("expected cached loader error"),
            };
            let marker = eval(scope, "marker");
            assert!(v8::Local::new(scope, error).strict_equals(marker));
        }
        assert_eq!(eval(scope, "calls").int32_value(scope), Some(1));
        let next = ProcedureRegistry::snapshot(scope, dictionary).unwrap();
        let _ = pending(&next, scope, "lazy");
        assert_eq!(eval(scope, "calls").int32_value(scope), Some(2));
    });
}

#[test]
fn pending_calls_share_loading_but_keep_their_own_context_and_result() {
    with_scope(|scope| {
        let dictionary = eval(
            scope,
            r#"
            globalThis.loads = 0;
            globalThis.invocations = 0;
            globalThis.releaseLoad = undefined;
            globalThis.releases = [];
            ({lazy: {load() {
                loads++;
                return new Promise(resolve => { releaseLoad = resolve; });
            }}})
        "#,
        );
        let registry = ProcedureRegistry::snapshot(scope, dictionary).unwrap();
        let mut calls = Vec::new();
        for label in ["alpha", "beta"] {
            let context = eval(scope, &format!("({{identity:'{label}'}})"));
            let context = v8::Local::<v8::Object>::try_from(context).unwrap();
            let input = v8::String::new(scope, label).unwrap().into();
            calls.push(crate::rpc::with_rpc_context(scope, context, |scope| {
                RpcCall::new(
                    scope,
                    registry.clone(),
                    "lazy".into(),
                    input,
                    context.into(),
                )
            }));
        }
        let pending: Vec<_> = calls
            .iter_mut()
            .map(|call| match call.poll(scope).unwrap() {
                CallProgress::Pending(promise) => promise,
                _ => panic!("loader should be pending"),
            })
            .collect();
        let first = v8::Local::new(scope, &pending[0]);
        let second = v8::Local::new(scope, &pending[1]);
        assert!(first.strict_equals(second.into()));
        assert_eq!(eval(scope, "loads").int32_value(scope), Some(1));
        eval(
            scope,
            r#"
            releaseLoad(Object.assign(async (input, ctx) => {
                invocations++;
                await new Promise(resolve => releases.push(resolve));
                return { input, identity: currentContext().identity,
                    sameContext: ctx === currentContext(), kind: currentKind() };
            }, {config: {kind: 'query'}}))
        "#,
        );
        scope.perform_microtask_checkpoint();
        for call in &mut calls {
            assert!(matches!(
                call.poll(scope).unwrap(),
                CallProgress::Pending(_)
            ));
        }
        assert_eq!(eval(scope, "invocations").int32_value(scope), Some(2));
        eval(scope, "releases.forEach(resolve => resolve())");
        scope.perform_microtask_checkpoint();
        for (call, label) in calls.iter_mut().zip(["alpha", "beta"]) {
            let CallProgress::Complete { invocation, value } = call.poll(scope).unwrap() else {
                panic!("handler should be complete");
            };
            let value = v8::Local::new(scope, value);
            assert_eq!(
                json(scope, value),
                serde_json::json!({
                    "input":label, "identity":label, "sameContext":true, "kind":"Some(Query)",
                })
            );
            invocation.with_frame(scope, |scope| {
                assert_eq!(
                    eval(scope, "currentContext().identity").to_rust_string_lossy(scope),
                    label
                );
            });
            assert!(matches!(
                call.poll(scope),
                Err(InvokeFailure {
                    error: InvocationError::Engine(_),
                    ..
                })
            ));
        }
        assert_eq!(eval(scope, "invocations").int32_value(scope), Some(2));
        assert_eq!(crate::rpc::current_kind(scope), None);
        assert!(crate::rpc::dispatch::current_rpc_ctx_object(scope).is_none());
    });
}

#[test]
fn dropping_a_waiter_does_not_cancel_shared_resolution_or_invoke_its_handler() {
    with_scope(|scope| {
        let dictionary = eval(
            scope,
            r#"
            globalThis.invocations = 0;
            globalThis.finish = undefined;
            ({lazy: {load() { return new Promise(resolve => { finish = resolve; }); }}})
        "#,
        );
        let registry = ProcedureRegistry::snapshot(scope, dictionary).unwrap();
        let undefined = v8::undefined(scope).into();
        let mut cancelled =
            RpcCall::new(scope, registry.clone(), "lazy".into(), undefined, undefined);
        assert!(matches!(
            cancelled.poll(scope).unwrap(),
            CallProgress::Pending(_)
        ));
        drop(cancelled);
        let mut live = RpcCall::new(scope, registry, "lazy".into(), undefined, undefined);
        assert!(matches!(
            live.poll(scope).unwrap(),
            CallProgress::Pending(_)
        ));
        eval(scope, "finish(() => ++invocations)");
        scope.perform_microtask_checkpoint();
        assert!(matches!(
            live.poll(scope).unwrap(),
            CallProgress::Complete { .. }
        ));
        assert_eq!(eval(scope, "invocations").int32_value(scope), Some(1));
    });
}

#[test]
fn promised_iterators_keep_identity_and_the_procedure_frame_for_pulling() {
    with_scope(|scope| {
        let dictionary = eval(
            scope,
            r#"
            globalThis.iterator = Object.freeze({
                async next() { await Promise.resolve(); return {done:false, value:currentKind()}; },
                [Symbol.asyncIterator]() { return this; },
            });
            ({stream: Object.assign(async () => iterator,
                {config:{kind:'stream', outputIsString:true}})})
        "#,
        );
        let registry = ProcedureRegistry::snapshot(scope, dictionary).unwrap();
        let undefined = v8::undefined(scope).into();
        let mut call = RpcCall::new(scope, registry, "stream".into(), undefined, undefined);
        assert!(matches!(
            call.poll(scope).unwrap(),
            CallProgress::Pending(_)
        ));
        scope.perform_microtask_checkpoint();
        let CallProgress::Complete { invocation, value } = call.poll(scope).unwrap() else {
            panic!("expected original iterator");
        };
        let iterator = eval(scope, "iterator");
        assert!(v8::Local::new(scope, value).strict_equals(iterator));
        assert!(invocation.output_is_string);
        assert!(invocation.output.is_none());
        let next = invocation.with_frame(scope, |scope| {
            let value = eval(scope, "iterator.next()");
            v8::Global::new(scope, value)
        });
        scope.perform_microtask_checkpoint();
        let next = v8::Local::new(scope, next);
        let next = v8::Local::<v8::Promise>::try_from(next).unwrap();
        assert_eq!(next.state(), v8::PromiseState::Fulfilled);
        let result = next.result(scope);
        assert_eq!(
            json(scope, result),
            serde_json::json!({"done":false,"value":"Some(Stream)"})
        );
        assert_eq!(crate::rpc::current_kind(scope), None);
    });
}
