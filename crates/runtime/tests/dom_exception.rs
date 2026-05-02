//! Native DOMException tests. WebIDL §3.14
//! (https://webidl.spec.whatwg.org/#idl-DOMException).

mod common;
use common::{dispatch, m};

#[test]
fn default_constructor_yields_empty_message_and_default_name() {
    let r = dispatch(
        m(r#"export function test() {
            const e = new DOMException();
            return { message: e.message, name: e.name, code: e.code };
        }"#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(r.json.contains("\"message\":\"\""), "got: {}", r.json);
    assert!(r.json.contains("\"name\":\"Error\""), "got: {}", r.json);
    assert!(r.json.contains("\"code\":0"), "got: {}", r.json);
}

#[test]
fn message_passed_to_constructor_is_returned() {
    let r = dispatch(
        m(r#"export function test() {
            const e = new DOMException("boom");
            return { message: e.message, name: e.name, code: e.code };
        }"#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(r.json.contains("\"message\":\"boom\""), "got: {}", r.json);
    assert!(r.json.contains("\"name\":\"Error\""), "got: {}", r.json);
    assert!(r.json.contains("\"code\":0"), "got: {}", r.json);
}

#[test]
fn abort_error_has_legacy_code_20() {
    let r = dispatch(
        m(r#"export function test() {
            const e = new DOMException("aborted", "AbortError");
            return { message: e.message, name: e.name, code: e.code };
        }"#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(r.json.contains("\"message\":\"aborted\""), "got: {}", r.json);
    assert!(r.json.contains("\"name\":\"AbortError\""), "got: {}", r.json);
    assert!(r.json.contains("\"code\":20"), "got: {}", r.json);
}

#[test]
fn timeout_error_has_legacy_code_23() {
    let r = dispatch(
        m(r#"export function test() {
            const e = new DOMException("timed out", "TimeoutError");
            return { name: e.name, code: e.code };
        }"#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(r.json.contains("\"name\":\"TimeoutError\""), "got: {}", r.json);
    assert!(r.json.contains("\"code\":23"), "got: {}", r.json);
}

#[test]
fn unknown_name_returns_code_zero() {
    let r = dispatch(
        m(r#"export function test() {
            const e = new DOMException("boom", "MadeUpError");
            return { name: e.name, code: e.code };
        }"#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(r.json.contains("\"name\":\"MadeUpError\""), "got: {}", r.json);
    assert!(r.json.contains("\"code\":0"), "got: {}", r.json);
}

#[test]
fn instance_inherits_from_error() {
    let r = dispatch(
        m(r#"export function test() {
            const e = new DOMException("x", "AbortError");
            return {
                isError: e instanceof Error,
                isDomException: e instanceof DOMException,
            };
        }"#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(r.json.contains("\"isError\":true"), "got: {}", r.json);
    assert!(r.json.contains("\"isDomException\":true"), "got: {}", r.json);
}

#[test]
fn constructor_has_legacy_code_constants() {
    let r = dispatch(
        m(r#"export function test() {
            return {
                INDEX_SIZE_ERR: DOMException.INDEX_SIZE_ERR,
                ABORT_ERR: DOMException.ABORT_ERR,
                TIMEOUT_ERR: DOMException.TIMEOUT_ERR,
                DATA_CLONE_ERR: DOMException.DATA_CLONE_ERR,
                NETWORK_ERR: DOMException.NETWORK_ERR,
            };
        }"#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(r.json.contains("\"INDEX_SIZE_ERR\":1"), "got: {}", r.json);
    assert!(r.json.contains("\"ABORT_ERR\":20"), "got: {}", r.json);
    assert!(r.json.contains("\"TIMEOUT_ERR\":23"), "got: {}", r.json);
    assert!(r.json.contains("\"DATA_CLONE_ERR\":25"), "got: {}", r.json);
    assert!(r.json.contains("\"NETWORK_ERR\":19"), "got: {}", r.json);
}

#[test]
fn prototype_has_legacy_code_constants() {
    let r = dispatch(
        m(r#"export function test() {
            const e = new DOMException();
            // Per WebIDL §3.7.5: legacy constants on the prototype too.
            return {
                proto_INDEX_SIZE_ERR: DOMException.prototype.INDEX_SIZE_ERR,
                inst_INDEX_SIZE_ERR: e.INDEX_SIZE_ERR,
            };
        }"#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(r.json.contains("\"proto_INDEX_SIZE_ERR\":1"), "got: {}", r.json);
    assert!(r.json.contains("\"inst_INDEX_SIZE_ERR\":1"), "got: {}", r.json);
}

#[test]
fn message_coerces_non_string_input_via_to_string() {
    let r = dispatch(
        m(r#"export function test() {
            const e = new DOMException(42, "AbortError");
            return { message: e.message, name: e.name };
        }"#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(r.json.contains("\"message\":\"42\""), "got: {}", r.json);
    assert!(r.json.contains("\"name\":\"AbortError\""), "got: {}", r.json);
}

#[test]
fn dom_exception_can_be_thrown_and_caught() {
    let r = dispatch(
        m(r#"export function test() {
            try {
                throw new DOMException("nope", "InvalidStateError");
            } catch (err) {
                return {
                    name: err.name,
                    message: err.message,
                    code: err.code,
                    isError: err instanceof Error,
                    isDom: err instanceof DOMException,
                };
            }
        }"#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(r.json.contains("\"name\":\"InvalidStateError\""), "got: {}", r.json);
    assert!(r.json.contains("\"code\":11"), "got: {}", r.json);
    assert!(r.json.contains("\"isError\":true"), "got: {}", r.json);
    assert!(r.json.contains("\"isDom\":true"), "got: {}", r.json);
}

#[test]
fn dom_exception_to_string_tag_is_dom_exception() {
    let r = dispatch(
        m(r#"export function test() {
            const e = new DOMException("x", "AbortError");
            return { tag: Object.prototype.toString.call(e) };
        }"#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(r.json.contains("\"tag\":\"[object DOMException]\""), "got: {}", r.json);
}

#[test]
fn abort_signal_uses_native_dom_exception() {
    // AbortSignal's default abort reason is a real native DOMException
    // (post-rewire). signal.reason instanceof DOMException must be true,
    // matching WPT abort-event expectations.
    let r = dispatch(
        m(r#"export function test() {
            const c = new AbortController();
            c.abort();
            const reason = c.signal.reason;
            return {
                isDom: reason instanceof DOMException,
                isError: reason instanceof Error,
                name: reason?.name,
                code: reason?.code,
            };
        }"#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(r.json.contains("\"isDom\":true"), "got: {}", r.json);
    assert!(r.json.contains("\"isError\":true"), "got: {}", r.json);
    assert!(r.json.contains("\"name\":\"AbortError\""), "got: {}", r.json);
    assert!(r.json.contains("\"code\":20"), "got: {}", r.json);
}
