//! Native `RpcError` tests.
//!
//! Mirrors the DOMException test harness pattern. The class is installed
//! by the runtime's `setup_globals` path; we drive it through the
//! synthetic-entry shim so the wire shape matches production.
//!
//! See `docs/proposals/rpc.md` §RpcError surface for the contract.

mod common;
use common::{dispatch, m};

#[test]
fn name_is_rpc_error() {
    let r = dispatch(
        m(r#"export function test() {
            const e = new RpcError("INTERNAL", "boom");
            return { name: e.name, message: e.message, code: e.code };
        }"#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(r.json.contains("\"name\":\"RpcError\""), "got: {}", r.json);
    assert!(r.json.contains("\"message\":\"boom\""), "got: {}", r.json);
    assert!(r.json.contains("\"code\":\"INTERNAL\""), "got: {}", r.json);
}

#[test]
fn instance_inherits_from_error() {
    let r = dispatch(
        m(r#"export function test() {
            const e = new RpcError("INTERNAL", "boom");
            return {
                isError: e instanceof Error,
                isRpcError: e instanceof RpcError,
            };
        }"#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(r.json.contains("\"isError\":true"), "got: {}", r.json);
    assert!(r.json.contains("\"isRpcError\":true"), "got: {}", r.json);
}

#[test]
fn code_to_status_mapping() {
    let r = dispatch(
        m(r#"export function test() {
            return {
                unavailable: new RpcError("UNAVAILABLE", "x").status,
                notFound: new RpcError("NOT_FOUND", "x").status,
                unauthenticated: new RpcError("UNAUTHENTICATED", "x").status,
                permissionDenied: new RpcError("PERMISSION_DENIED", "x").status,
                invalidArgument: new RpcError("INVALID_ARGUMENT", "x").status,
                alreadyExists: new RpcError("ALREADY_EXISTS", "x").status,
                resourceExhausted: new RpcError("RESOURCE_EXHAUSTED", "x").status,
                aborted: new RpcError("ABORTED", "x").status,
                cancelled: new RpcError("CANCELLED", "x").status,
                timeout: new RpcError("TIMEOUT", "x").status,
                internal: new RpcError("INTERNAL", "x").status,
                unimplemented: new RpcError("UNIMPLEMENTED", "x").status,
                unknown: new RpcError("UNKNOWN", "x").status,
                outOfRange: new RpcError("OUT_OF_RANGE", "x").status,
                failedPrecondition: new RpcError("FAILED_PRECONDITION", "x").status,
            };
        }"#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(r.json.contains("\"unavailable\":503"), "got: {}", r.json);
    assert!(r.json.contains("\"notFound\":404"), "got: {}", r.json);
    assert!(r.json.contains("\"unauthenticated\":401"), "got: {}", r.json);
    assert!(r.json.contains("\"permissionDenied\":403"), "got: {}", r.json);
    assert!(r.json.contains("\"invalidArgument\":400"), "got: {}", r.json);
    assert!(r.json.contains("\"alreadyExists\":409"), "got: {}", r.json);
    assert!(r.json.contains("\"resourceExhausted\":429"), "got: {}", r.json);
    assert!(r.json.contains("\"aborted\":499"), "got: {}", r.json);
    assert!(r.json.contains("\"cancelled\":499"), "got: {}", r.json);
    assert!(r.json.contains("\"timeout\":504"), "got: {}", r.json);
    assert!(r.json.contains("\"internal\":500"), "got: {}", r.json);
    assert!(r.json.contains("\"unimplemented\":500"), "got: {}", r.json);
    assert!(r.json.contains("\"unknown\":500"), "got: {}", r.json);
    assert!(r.json.contains("\"outOfRange\":400"), "got: {}", r.json);
    assert!(r.json.contains("\"failedPrecondition\":400"), "got: {}", r.json);
}

#[test]
fn default_retryable_derivation() {
    let r = dispatch(
        m(r#"export function test() {
            return {
                timeoutRetryable: new RpcError("TIMEOUT", "x").retryable,
                resourceExhaustedRetryable: new RpcError("RESOURCE_EXHAUSTED", "x").retryable,
                unavailableRetryable: new RpcError("UNAVAILABLE", "x").retryable,
                internalRetryable: new RpcError("INTERNAL", "x").retryable,
                notFoundRetryable: new RpcError("NOT_FOUND", "x").retryable,
            };
        }"#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(r.json.contains("\"timeoutRetryable\":true"), "got: {}", r.json);
    assert!(r.json.contains("\"resourceExhaustedRetryable\":true"), "got: {}", r.json);
    assert!(r.json.contains("\"unavailableRetryable\":true"), "got: {}", r.json);
    assert!(r.json.contains("\"internalRetryable\":false"), "got: {}", r.json);
    assert!(r.json.contains("\"notFoundRetryable\":false"), "got: {}", r.json);
}

#[test]
fn retryable_can_be_overridden() {
    let r = dispatch(
        m(r#"export function test() {
            return {
                forced: new RpcError("INTERNAL", "x", { retryable: true }).retryable,
                suppressed: new RpcError("TIMEOUT", "x", { retryable: false }).retryable,
            };
        }"#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(r.json.contains("\"forced\":true"), "got: {}", r.json);
    assert!(r.json.contains("\"suppressed\":false"), "got: {}", r.json);
}

#[test]
fn expose_message_default_false_overridable() {
    let r = dispatch(
        m(r#"export function test() {
            return {
                defaultExpose: new RpcError("INTERNAL", "x").exposeMessage,
                forced: new RpcError("INTERNAL", "x", { exposeMessage: true }).exposeMessage,
            };
        }"#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(r.json.contains("\"defaultExpose\":false"), "got: {}", r.json);
    assert!(r.json.contains("\"forced\":true"), "got: {}", r.json);
}

#[test]
fn details_round_trip() {
    // serde_json's workspace-level `preserve_order` feature (enabled by
    // the superjson wire format) makes Value preserve insertion
    // order through every Rust-side re-serialization step — so V8's
    // insertion order survives all the way to the wire byte-identical.
    let r = dispatch(
        m(r#"export function test() {
            const e = new RpcError("INVALID_ARGUMENT", "x", { details: { path: ["a"], n: 7 } });
            return { details: e.details };
        }"#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(
        r.json.contains("\"details\":{\"path\":[\"a\"],\"n\":7}"),
        "got: {}",
        r.json
    );
}

#[test]
fn details_default_undefined() {
    let r = dispatch(
        m(r#"export function test() {
            const e = new RpcError("INTERNAL", "x");
            return { hasDetails: e.details !== undefined && e.details !== null };
        }"#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(r.json.contains("\"hasDetails\":false"), "got: {}", r.json);
}

#[test]
fn to_string_tag_is_rpc_error() {
    let r = dispatch(
        m(r#"export function test() {
            const e = new RpcError("INTERNAL", "x");
            return { tag: Object.prototype.toString.call(e) };
        }"#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(
        r.json.contains("\"tag\":\"[object RpcError]\""),
        "got: {}",
        r.json
    );
}

#[test]
fn code_constants_on_constructor() {
    let r = dispatch(
        m(r#"export function test() {
            return {
                UNAUTHENTICATED: RpcError.UNAUTHENTICATED,
                NOT_FOUND: RpcError.NOT_FOUND,
                INTERNAL: RpcError.INTERNAL,
                UNAVAILABLE: RpcError.UNAVAILABLE,
                TIMEOUT: RpcError.TIMEOUT,
                INVALID_ARGUMENT: RpcError.INVALID_ARGUMENT,
                PERMISSION_DENIED: RpcError.PERMISSION_DENIED,
                FAILED_PRECONDITION: RpcError.FAILED_PRECONDITION,
                ALREADY_EXISTS: RpcError.ALREADY_EXISTS,
                RESOURCE_EXHAUSTED: RpcError.RESOURCE_EXHAUSTED,
                ABORTED: RpcError.ABORTED,
                CANCELLED: RpcError.CANCELLED,
                OUT_OF_RANGE: RpcError.OUT_OF_RANGE,
                UNIMPLEMENTED: RpcError.UNIMPLEMENTED,
                UNKNOWN: RpcError.UNKNOWN,
            };
        }"#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(r.json.contains("\"UNAUTHENTICATED\":\"UNAUTHENTICATED\""), "got: {}", r.json);
    assert!(r.json.contains("\"NOT_FOUND\":\"NOT_FOUND\""), "got: {}", r.json);
    assert!(r.json.contains("\"INTERNAL\":\"INTERNAL\""), "got: {}", r.json);
    assert!(r.json.contains("\"UNAVAILABLE\":\"UNAVAILABLE\""), "got: {}", r.json);
    assert!(r.json.contains("\"TIMEOUT\":\"TIMEOUT\""), "got: {}", r.json);
    assert!(r.json.contains("\"INVALID_ARGUMENT\":\"INVALID_ARGUMENT\""), "got: {}", r.json);
    assert!(r.json.contains("\"PERMISSION_DENIED\":\"PERMISSION_DENIED\""), "got: {}", r.json);
    assert!(r.json.contains("\"FAILED_PRECONDITION\":\"FAILED_PRECONDITION\""), "got: {}", r.json);
    assert!(r.json.contains("\"ALREADY_EXISTS\":\"ALREADY_EXISTS\""), "got: {}", r.json);
    assert!(r.json.contains("\"RESOURCE_EXHAUSTED\":\"RESOURCE_EXHAUSTED\""), "got: {}", r.json);
    assert!(r.json.contains("\"ABORTED\":\"ABORTED\""), "got: {}", r.json);
    assert!(r.json.contains("\"CANCELLED\":\"CANCELLED\""), "got: {}", r.json);
    assert!(r.json.contains("\"OUT_OF_RANGE\":\"OUT_OF_RANGE\""), "got: {}", r.json);
    assert!(r.json.contains("\"UNIMPLEMENTED\":\"UNIMPLEMENTED\""), "got: {}", r.json);
    assert!(r.json.contains("\"UNKNOWN\":\"UNKNOWN\""), "got: {}", r.json);
}

#[test]
fn rpc_error_can_be_thrown_and_caught() {
    let r = dispatch(
        m(r#"export function test() {
            try {
                throw new RpcError("NOT_FOUND", "todo missing", { details: { id: "123" } });
            } catch (err) {
                return {
                    code: err.code,
                    message: err.message,
                    status: err.status,
                    name: err.name,
                    details: err.details,
                    isError: err instanceof Error,
                    isRpc: err instanceof RpcError,
                };
            }
        }"#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(r.json.contains("\"code\":\"NOT_FOUND\""), "got: {}", r.json);
    assert!(r.json.contains("\"message\":\"todo missing\""), "got: {}", r.json);
    assert!(r.json.contains("\"status\":404"), "got: {}", r.json);
    assert!(r.json.contains("\"name\":\"RpcError\""), "got: {}", r.json);
    assert!(r.json.contains("\"details\":{\"id\":\"123\"}"), "got: {}", r.json);
    assert!(r.json.contains("\"isError\":true"), "got: {}", r.json);
    assert!(r.json.contains("\"isRpc\":true"), "got: {}", r.json);
}

#[test]
fn unknown_code_throws_type_error() {
    // Per WebIDL §3.13.7 step 4: unknown enum value MUST TypeError.
    let r = dispatch(
        m(r#"export function test() {
            try {
                new RpcError("NOT_A_CODE", "x");
                return { thrown: false };
            } catch (e) {
                return { thrown: true, name: e.name };
            }
        }"#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(r.json.contains("\"thrown\":true"), "got: {}", r.json);
    assert!(r.json.contains("\"name\":\"TypeError\""), "got: {}", r.json);
}

// ---------------------------------------------------------------------------
// `cause` passthrough — matches the standard `Error` shape (ECMAScript
// §20.5.6.1.1): when `cause` is given in opts, the instance gets a
// non-enumerable own data property `cause`; when absent, no property is
// emitted (`'cause' in err === false`, mirroring `new Error("x")`).
// ---------------------------------------------------------------------------

#[test]
fn cause_round_trip_error_instance() {
    let r = dispatch(
        m(r#"export function test() {
            const e = new RpcError("INTERNAL", "boom", { cause: new TypeError("inner") });
            return {
                isType: e.cause instanceof TypeError,
                isError: e.cause instanceof Error,
                msg: e.cause.message,
            };
        }"#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(r.json.contains("\"isType\":true"), "got: {}", r.json);
    assert!(r.json.contains("\"isError\":true"), "got: {}", r.json);
    assert!(r.json.contains("\"msg\":\"inner\""), "got: {}", r.json);
}

#[test]
fn cause_round_trip_string() {
    let r = dispatch(
        m(r#"export function test() {
            const e = new RpcError("INTERNAL", "x", { cause: "string-cause" });
            return { eq: e.cause === "string-cause", typ: typeof e.cause };
        }"#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(r.json.contains("\"eq\":true"), "got: {}", r.json);
    assert!(r.json.contains("\"typ\":\"string\""), "got: {}", r.json);
}

#[test]
fn cause_round_trip_object() {
    let r = dispatch(
        m(r#"export function test() {
            const e = new RpcError("INTERNAL", "x", { cause: { code: 42 } });
            return { code: e.cause.code };
        }"#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(r.json.contains("\"code\":42"), "got: {}", r.json);
}

#[test]
fn cause_default_undefined() {
    // Match `new Error("x")`: when `cause` is absent, no own property.
    let r = dispatch(
        m(r#"export function test() {
            const a = new RpcError("INTERNAL", "x");
            const b = new RpcError("INTERNAL", "x", {});
            const c = new RpcError("INTERNAL", "x", { cause: undefined });
            return {
                aHas: "cause" in a,
                bHas: "cause" in b,
                cHas: "cause" in c,
                // The dispatch path is superjson, which encodes a returned
                // `undefined` *value* as `null` + a meta entry — so asserting
                // wire-absence of `aVal` is wrong. Assert the read result in
                // JS instead: a missing prop reads as `undefined`.
                aReadsUndefined: a.cause === undefined,
            };
        }"#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(r.json.contains("\"aHas\":false"), "got: {}", r.json);
    assert!(r.json.contains("\"bHas\":false"), "got: {}", r.json);
    assert!(r.json.contains("\"cHas\":false"), "got: {}", r.json);
    assert!(r.json.contains("\"aReadsUndefined\":true"), "got: {}", r.json);
}

#[test]
fn cause_non_enumerable() {
    // Per ECMA §20.5.6.1.1, the auto-attached `cause` property is
    // [[Enumerable]] = false — Object.keys must not list it.
    let r = dispatch(
        m(r#"export function test() {
            const e = new RpcError("INTERNAL", "x", { cause: "z" });
            const desc = Object.getOwnPropertyDescriptor(e, "cause");
            return {
                keys: Object.keys(e),
                enumerable: desc.enumerable,
                writable: desc.writable,
                configurable: desc.configurable,
            };
        }"#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(r.json.contains("\"keys\":[]"), "got: {}", r.json);
    assert!(r.json.contains("\"enumerable\":false"), "got: {}", r.json);
    assert!(r.json.contains("\"writable\":true"), "got: {}", r.json);
    assert!(r.json.contains("\"configurable\":true"), "got: {}", r.json);
}

// ---------------------------------------------------------------------------
// Rust-side brand-check: RpcError::is_instance returns true for native
// instances and false for plain Error / non-class objects.
// ---------------------------------------------------------------------------

#[test]
fn rust_is_instance_brand_check() {
    use zeroship_runtime::init_v8;
    use zeroship_runtime::rpc::RpcError;

    init_v8();
    let mut isolate = v8::Isolate::new(v8::CreateParams::default());
    v8::scope!(let handle_scope, &mut isolate);
    let context = v8::Context::new(handle_scope, Default::default());
    let scope = &mut v8::ContextScope::new(handle_scope, context);
    let global = scope.get_current_context().global(scope);
    zeroship_runtime::rpc::install_global(scope, global);

    // A native RpcError.
    let src_rpc = v8::String::new(scope, r#"new RpcError("INTERNAL", "boom")"#).unwrap();
    let script = v8::Script::compile(scope, src_rpc, None).unwrap();
    let rpc_val = script.run(scope).unwrap();
    assert!(
        RpcError::is_instance(scope, rpc_val),
        "native RpcError should pass is_instance"
    );

    // A plain Error.
    let src_err = v8::String::new(scope, r#"new Error("boom")"#).unwrap();
    let script = v8::Script::compile(scope, src_err, None).unwrap();
    let err_val = script.run(scope).unwrap();
    assert!(
        !RpcError::is_instance(scope, err_val),
        "plain Error should NOT pass is_instance"
    );

    // A plain object.
    let src_obj = v8::String::new(scope, r#"({ code: "INTERNAL" })"#).unwrap();
    let script = v8::Script::compile(scope, src_obj, None).unwrap();
    let obj_val = script.run(scope).unwrap();
    assert!(
        !RpcError::is_instance(scope, obj_val),
        "plain object should NOT pass is_instance"
    );
}
