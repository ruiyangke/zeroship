//! TransformStream strategy + start-throw error propagation.
//!
//! Pins the user-exception passthrough behavior unlocked by #199's
//! refactor: `parse_strategy_local` and
//! `set_up_transform_stream_default_controller_from_transformer` now
//! return `Result<_, OpError>` and capture user-thrown JS values via
//! `OpError::js_value` (the JsValue passthrough variant). The hand-rolled
//! constructor's `throw_op_error` re-throws the captured value verbatim,
//! so a user-thrown Error subclass / `e.code` / custom properties survive
//! the parse-strategy and start-throw paths.
//!
//! Pre-refactor: the start-throw path went through a sentinel-string
//! detection — the helper `scope.throw_exception`d the user error AND
//! returned `Err("TransformStream: start threw synchronously")`; the
//! constructor matched the sentinel to suppress a second TypeError. Now
//! the JsValue variant carries the exception and the constructor's
//! `throw_op_error` rethrows once, no sentinel.

#![allow(unsafe_code)]

use zeroship_runtime::init_v8;
use zeroship_runtime::streams::install_native_streams;
use zeroship_runtime::streams::strategies::{
    install_byte_length_queuing_strategy, install_count_queuing_strategy,
};

fn run_with_streams<R>(
    src: &str,
    f: impl FnOnce(v8::Local<v8::Value>, &mut v8::PinScope) -> R,
) -> R {
    init_v8();
    let mut isolate = v8::Isolate::new(v8::CreateParams::default());
    v8::scope!(let handle_scope, &mut isolate);
    let context = v8::Context::new(handle_scope, Default::default());
    let scope = &mut v8::ContextScope::new(handle_scope, context);
    let global = scope.get_current_context().global(scope);

    install_byte_length_queuing_strategy(scope, global);
    install_count_queuing_strategy(scope, global);
    install_native_streams(scope, global);

    let src_v8 = v8::String::new(scope, src).unwrap();
    let script = v8::Script::compile(scope, src_v8, None).unwrap();
    let result = script.run(scope).unwrap();
    for _ in 0..16 {
        scope.perform_microtask_checkpoint();
    }
    f(result, scope)
}

#[test]
fn nan_high_water_mark_throws_range_error() {
    // Spec §5.2.4 + §7.2: highWaterMark must not be NaN. The strategy
    // converter dresses this as RangeError.
    let r = run_with_streams(
        r#"
        let kind, msg;
        try {
            new TransformStream(undefined, { highWaterMark: NaN });
        } catch (e) {
            kind = e.constructor.name;
            msg = e.message;
        }
        ({ kind: kind ?? "no-throw", msg: msg ?? "" })
        "#,
        |val, scope| {
            let obj = v8::Local::<v8::Object>::try_from(val).unwrap();
            let kind_key = v8::String::new(scope, "kind").unwrap();
            let msg_key = v8::String::new(scope, "msg").unwrap();
            let kind = obj.get(scope, kind_key.into()).unwrap().to_rust_string_lossy(scope);
            let msg = obj.get(scope, msg_key.into()).unwrap().to_rust_string_lossy(scope);
            (kind, msg)
        },
    );
    assert_eq!(r.0, "RangeError", "expected RangeError, got {} ({})", r.0, r.1);
    assert!(r.1.contains("NaN"), "expected NaN message, got {}", r.1);
}

#[test]
fn user_thrown_strategy_size_propagates_verbatim() {
    // The strategy.size getter throws a user Error("oops"). The brand
    // check in `parse_strategy_local`'s tc_scope wraps it as
    // OpError::JsValue, then `throw_op_error` re-throws verbatim — so
    // the catch sees the original Error("oops"), not a TypeError dressed
    // around it.
    //
    // Spec ordering: writableStrategy is converted before readableStrategy,
    // so we hit the writable side's size getter first. We use a plain
    // Error so the assert reads naturally; the macro's JsValue passthrough
    // preserves any subclass equally.
    let r = run_with_streams(
        r#"
        let kind, msg;
        try {
            new TransformStream(undefined, undefined, {
                get size() { throw new Error("oops"); }
            });
        } catch (e) {
            kind = e.constructor.name;
            msg = e.message;
        }
        ({ kind: kind ?? "no-throw", msg: msg ?? "" })
        "#,
        |val, scope| {
            let obj = v8::Local::<v8::Object>::try_from(val).unwrap();
            let kind_key = v8::String::new(scope, "kind").unwrap();
            let msg_key = v8::String::new(scope, "msg").unwrap();
            let kind = obj.get(scope, kind_key.into()).unwrap().to_rust_string_lossy(scope);
            let msg = obj.get(scope, msg_key.into()).unwrap().to_rust_string_lossy(scope);
            (kind, msg)
        },
    );
    assert_eq!(r.0, "Error", "expected user Error, got {} ({})", r.0, r.1);
    assert_eq!(r.1, "oops", "expected verbatim 'oops', got {}", r.1);
}

#[test]
fn user_thrown_start_propagates_verbatim() {
    // The transformer.start() throws synchronously. Pre-refactor the
    // helper returned Err("TransformStream: start threw synchronously")
    // and the constructor relied on sentinel-string suppression to avoid
    // double-throwing. Post-refactor, the helper captures the user
    // exception via tc_scope into OpError::JsValue, and the constructor's
    // throw_op_error rethrows the original value verbatim — no sentinel,
    // no double-throw.
    let r = run_with_streams(
        r#"
        let kind, msg;
        try {
            new TransformStream({
                start() { throw new Error("start failed"); }
            });
        } catch (e) {
            kind = e.constructor.name;
            msg = e.message;
        }
        ({ kind: kind ?? "no-throw", msg: msg ?? "" })
        "#,
        |val, scope| {
            let obj = v8::Local::<v8::Object>::try_from(val).unwrap();
            let kind_key = v8::String::new(scope, "kind").unwrap();
            let msg_key = v8::String::new(scope, "msg").unwrap();
            let kind = obj.get(scope, kind_key.into()).unwrap().to_rust_string_lossy(scope);
            let msg = obj.get(scope, msg_key.into()).unwrap().to_rust_string_lossy(scope);
            (kind, msg)
        },
    );
    assert_eq!(r.0, "Error", "expected user Error, got {} ({})", r.0, r.1);
    assert_eq!(r.1, "start failed", "expected verbatim 'start failed', got {}", r.1);
}
