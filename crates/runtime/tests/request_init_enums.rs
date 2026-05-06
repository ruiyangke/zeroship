//! Tests for the typed-enum migration of `RequestInit` / `ResponseInit`.
//!
//! Covers:
//!   - Each WebIDL enum (`RequestMode`, `RequestCredentials`,
//!     `RequestCache`, `RequestRedirect`, `ReferrerPolicy`,
//!     `ResponseType`) round-trips spec-canonical values.
//!   - The constructor rejects unknown enum values with `TypeError`
//!     per WebIDL §3.13.7. **Behaviour change** vs the previous
//!     `RefCell<String>` storage which silently accepted any string.
//!   - Defaults match the spec.

#![allow(unsafe_code)]

use zeroship_runtime::dom;
use zeroship_runtime::fetch_request;
use zeroship_runtime::fetch_response;
use zeroship_runtime::headers;
use zeroship_runtime::init_v8;
use zeroship_runtime::streams;

fn run_in_v8<F, R>(src: &str, f: F) -> R
where
    F: FnOnce(v8::Local<v8::Value>, &mut v8::PinScope) -> R,
{
    init_v8();
    let mut isolate = v8::Isolate::new(v8::CreateParams::default());
    v8::scope!(let handle_scope, &mut isolate);
    let context = v8::Context::new(handle_scope, Default::default());
    let scope = &mut v8::ContextScope::new(handle_scope, context);

    install_globals(scope);

    let src_v8 = v8::String::new(scope, src).unwrap();
    let script = v8::Script::compile(scope, src_v8, None).unwrap();
    let result = script.run(scope).unwrap();
    scope.perform_microtask_checkpoint();
    f(result, scope)
}

fn install_globals(scope: &mut v8::PinScope) {
    let global = scope.get_current_context().global(scope);
    streams::install_native_streams(scope, global);
    streams::strategies::install_byte_length_queuing_strategy(scope, global);
    streams::strategies::install_count_queuing_strategy(scope, global);
    headers::install_global(scope, global);
    dom::install_globals(scope, global);
    fetch_request::install_global(scope, global);
    fetch_response::install_global(scope, global);
}

fn js_string(val: v8::Local<v8::Value>, scope: &mut v8::PinScope) -> String {
    val.to_rust_string_lossy(scope)
}

// ---------------------------------------------------------------------------
// 1. mode_typed_enum_set_get
// ---------------------------------------------------------------------------

#[test]
fn mode_typed_enum_set_get() {
    let s = run_in_v8(
        r#"
        const r = new Request("https://example.com", { mode: "cors" });
        r.mode;
        "#,
        js_string,
    );
    assert_eq!(s, "cors");
}

// ---------------------------------------------------------------------------
// 2. mode_unknown_throws_type_error
// ---------------------------------------------------------------------------

#[test]
fn mode_unknown_throws_type_error() {
    // Behaviour change: previously the bogus value was silently stored
    // as a string. Now WebIDL §3.13.7 step 4 throws TypeError.
    let s = run_in_v8(
        r#"
        let err = null;
        try {
            new Request("https://example.com", { mode: "bogus" });
        } catch (e) {
            err = e;
        }
        JSON.stringify({
            isTE: err instanceof TypeError,
            name: err && err.name,
        });
        "#,
        js_string,
    );
    assert_eq!(s, r#"{"isTE":true,"name":"TypeError"}"#);
}

// ---------------------------------------------------------------------------
// 3. credentials_default
// ---------------------------------------------------------------------------

#[test]
fn credentials_default() {
    let s = run_in_v8(
        r#"
        const r = new Request("https://example.com");
        r.credentials;
        "#,
        js_string,
    );
    assert_eq!(s, "same-origin");
}

#[test]
fn credentials_unknown_throws_type_error() {
    let s = run_in_v8(
        r#"
        let err = null;
        try {
            new Request("https://example.com", { credentials: "always" });
        } catch (e) {
            err = e;
        }
        err && err.name;
        "#,
        js_string,
    );
    assert_eq!(s, "TypeError");
}

// ---------------------------------------------------------------------------
// 4. redirect_default
// ---------------------------------------------------------------------------

#[test]
fn redirect_default() {
    let s = run_in_v8(
        r#"
        const r = new Request("https://example.com");
        r.redirect;
        "#,
        js_string,
    );
    assert_eq!(s, "follow");
}

#[test]
fn redirect_unknown_throws_type_error() {
    let s = run_in_v8(
        r#"
        let err = null;
        try {
            new Request("https://example.com", { redirect: "loop" });
        } catch (e) {
            err = e;
        }
        err && err.name;
        "#,
        js_string,
    );
    assert_eq!(s, "TypeError");
}

// ---------------------------------------------------------------------------
// 5. Spec tour: each enum's documented values round-trip.
// ---------------------------------------------------------------------------

#[test]
fn request_mode_spec_tour() {
    let s = run_in_v8(
        r#"
        const values = ["no-cors", "cors", "same-origin"];
        // Note: "navigate" is reserved per spec — Chrome rejects it
        // even though it's a documented variant. We don't gate it
        // here; the WebIdlEnum derive accepts all variants.
        JSON.stringify(values.map(v =>
            new Request("https://example.com", { mode: v }).mode
        ));
        "#,
        js_string,
    );
    assert_eq!(s, r#"["no-cors","cors","same-origin"]"#);
}

#[test]
fn request_credentials_spec_tour() {
    let s = run_in_v8(
        r#"
        const values = ["same-origin", "include", "omit"];
        JSON.stringify(values.map(v =>
            new Request("https://example.com", { credentials: v }).credentials
        ));
        "#,
        js_string,
    );
    assert_eq!(s, r#"["same-origin","include","omit"]"#);
}

#[test]
fn request_cache_spec_tour() {
    let s = run_in_v8(
        r#"
        const values = ["default", "no-store", "reload", "no-cache",
                        "force-cache", "only-if-cached"];
        // "only-if-cached" requires mode: "same-origin" per spec, but
        // we don't enforce that constraint at construction time — only
        // the enum membership is validated. Test all six.
        JSON.stringify(values.map(v =>
            new Request("https://example.com", { cache: v }).cache
        ));
        "#,
        js_string,
    );
    assert_eq!(s, r#"["default","no-store","reload","no-cache","force-cache","only-if-cached"]"#);
}

#[test]
fn request_redirect_spec_tour() {
    let s = run_in_v8(
        r#"
        const values = ["follow", "error", "manual"];
        JSON.stringify(values.map(v =>
            new Request("https://example.com", { redirect: v }).redirect
        ));
        "#,
        js_string,
    );
    assert_eq!(s, r#"["follow","error","manual"]"#);
}

#[test]
fn referrer_policy_spec_tour() {
    let s = run_in_v8(
        r#"
        const values = [
            "", "no-referrer", "no-referrer-when-downgrade",
            "same-origin", "origin", "strict-origin",
            "origin-when-cross-origin", "strict-origin-when-cross-origin",
            "unsafe-url",
        ];
        JSON.stringify(values.map(v =>
            new Request("https://example.com", { referrerPolicy: v }).referrerPolicy
        ));
        "#,
        js_string,
    );
    assert_eq!(
        s,
        r#"["","no-referrer","no-referrer-when-downgrade","same-origin","origin","strict-origin","origin-when-cross-origin","strict-origin-when-cross-origin","unsafe-url"]"#
    );
}

#[test]
fn referrer_policy_unknown_throws_type_error() {
    let s = run_in_v8(
        r#"
        let err = null;
        try {
            new Request("https://example.com", { referrerPolicy: "extreme" });
        } catch (e) {
            err = e;
        }
        err && err.name;
        "#,
        js_string,
    );
    assert_eq!(s, "TypeError");
}

#[test]
fn cache_unknown_throws_type_error() {
    let s = run_in_v8(
        r#"
        let err = null;
        try {
            new Request("https://example.com", { cache: "skip-cache" });
        } catch (e) {
            err = e;
        }
        err && err.name;
        "#,
        js_string,
    );
    assert_eq!(s, "TypeError");
}

// ---------------------------------------------------------------------------
// 6. response_type_basic
// ---------------------------------------------------------------------------

#[test]
fn response_type_basic() {
    // Per Fetch §5.5: a response built via `new Response()` has
    // type "default".
    let s = run_in_v8(
        r#"
        new Response().type;
        "#,
        js_string,
    );
    assert_eq!(s, "default");
}

// ---------------------------------------------------------------------------
// 7. response_type_error_static
// ---------------------------------------------------------------------------

#[test]
fn response_type_error_static() {
    let s = run_in_v8(
        r#"
        Response.error().type;
        "#,
        js_string,
    );
    assert_eq!(s, "error");
}

// ---------------------------------------------------------------------------
// Defaults sanity — the mode default is "cors" for JS-constructed
// Requests (the constructor's explicit initial; the enum's storage
// `Default` is "no-cors" for kernel-built Requests). This verifies
// the JS-observable default matches workerd / Deno.
// ---------------------------------------------------------------------------

#[test]
fn mode_default_is_cors_for_js_construction() {
    let s = run_in_v8(
        r#"
        new Request("https://example.com").mode;
        "#,
        js_string,
    );
    assert_eq!(s, "cors");
}

#[test]
fn cache_default_is_default() {
    let s = run_in_v8(
        r#"
        new Request("https://example.com").cache;
        "#,
        js_string,
    );
    assert_eq!(s, "default");
}

#[test]
fn referrer_policy_default_is_empty() {
    let s = run_in_v8(
        r#"
        new Request("https://example.com").referrerPolicy;
        "#,
        js_string,
    );
    assert_eq!(s, "");
}

#[test]
fn destination_default_is_empty() {
    let s = run_in_v8(
        r#"
        new Request("https://example.com").destination;
        "#,
        js_string,
    );
    assert_eq!(s, "");
}

// ---------------------------------------------------------------------------
// Inheritance from input Request — the typed-enum slots survive the
// "input is a Request" copy path (Fetch §5.4 step 6).
// ---------------------------------------------------------------------------

#[test]
fn enums_inherit_from_input_request() {
    let s = run_in_v8(
        r#"
        const a = new Request("https://example.com", {
            mode: "same-origin",
            credentials: "include",
            cache: "no-store",
            redirect: "manual",
            referrerPolicy: "no-referrer",
        });
        const b = new Request(a);
        JSON.stringify({
            mode: b.mode,
            credentials: b.credentials,
            cache: b.cache,
            redirect: b.redirect,
            referrerPolicy: b.referrerPolicy,
        });
        "#,
        js_string,
    );
    assert_eq!(
        s,
        r#"{"mode":"same-origin","credentials":"include","cache":"no-store","redirect":"manual","referrerPolicy":"no-referrer"}"#
    );
}
