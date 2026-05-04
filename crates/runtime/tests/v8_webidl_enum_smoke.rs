//! Smoke tests for `#[derive(WebIdlEnum)]` — WebIDL §3.7.10 enums.
//!
//! Coverage:
//!   - Default kebab-case naming: `NoCors` → `"no-cors"`,
//!     `Navigate` → `"navigate"`.
//!   - `#[webidl_name = "..."]` override per variant.
//!   - `from_str` round-trips on every variant (including overrides).
//!   - `from_str` returns `None` on unknown name.
//!   - `as_str` round-trips with `from_str`.
//!   - `WebIdlConvertible::from_v8` accepts known JS strings.
//!   - `from_v8` throws TypeError on unknown JS string with both the
//!     offending value AND the accepted set in the message.
//!   - `from_v8` ToString-coerces (number / bool both work if the
//!     coerced string matches a variant; otherwise reject).
#![allow(unsafe_code)]

use zeroship_runtime::convert::WebIdlConvertible;
use zeroship_runtime::init_v8;
use zeroship_runtime_macros::WebIdlEnum;

// ---------------------------------------------------------------------------
// Test harness
// ---------------------------------------------------------------------------

fn run_with_value<F, R>(src: &str, f: F) -> R
where
    F: FnOnce(v8::Local<v8::Value>, &mut v8::PinScope) -> R,
{
    init_v8();
    let mut isolate = v8::Isolate::new(v8::CreateParams::default());
    v8::scope!(let handle_scope, &mut isolate);
    let context = v8::Context::new(handle_scope, Default::default());
    let scope = &mut v8::ContextScope::new(handle_scope, context);

    let src_v8 = v8::String::new(scope, src).unwrap();
    let script = v8::Script::compile(scope, src_v8, None).unwrap();
    let result = script.run(scope).unwrap();
    f(result, scope)
}

// ---------------------------------------------------------------------------
// RequestMode — the canonical example from the spec.
// ---------------------------------------------------------------------------

#[derive(WebIdlEnum, Debug, PartialEq, Eq, Clone, Copy, Default)]
enum RequestMode {
    #[default]
    Cors,
    #[webidl_name = "no-cors"]
    NoCors,
    #[webidl_name = "same-origin"]
    SameOrigin,
    Navigate,
}

#[test]
fn from_str_known_names() {
    assert_eq!(RequestMode::from_str("cors"), Some(RequestMode::Cors));
    assert_eq!(RequestMode::from_str("no-cors"), Some(RequestMode::NoCors));
    assert_eq!(RequestMode::from_str("same-origin"), Some(RequestMode::SameOrigin));
    assert_eq!(RequestMode::from_str("navigate"), Some(RequestMode::Navigate));
}

#[test]
fn from_str_unknown_returns_none() {
    assert_eq!(RequestMode::from_str("nope"), None);
    assert_eq!(RequestMode::from_str("CORS"), None); // case-sensitive
    assert_eq!(RequestMode::from_str(""), None);
    assert_eq!(RequestMode::from_str("no_cors"), None); // underscore vs hyphen
}

#[test]
fn as_str_round_trips() {
    for m in [
        RequestMode::Cors,
        RequestMode::NoCors,
        RequestMode::SameOrigin,
        RequestMode::Navigate,
    ] {
        let s = m.as_str();
        assert_eq!(RequestMode::from_str(s), Some(m), "round-trip failed for {:?}", m);
    }
}

#[test]
fn as_str_kebab_case_default() {
    // Navigate has no #[webidl_name] so it kebab-cases the ident.
    assert_eq!(RequestMode::Navigate.as_str(), "navigate");
}

#[test]
fn as_str_override() {
    // NoCors / SameOrigin override the default kebab-case rule, but
    // happen to land on the same spelling — verify the override is
    // wired anyway.
    assert_eq!(RequestMode::NoCors.as_str(), "no-cors");
    assert_eq!(RequestMode::SameOrigin.as_str(), "same-origin");
}

// ---------------------------------------------------------------------------
// from_v8: JS string → variant
// ---------------------------------------------------------------------------

#[test]
fn from_v8_known_string() {
    let m = run_with_value(r#""cors""#, |val, scope| {
        RequestMode::from_v8(scope, val).unwrap()
    });
    assert_eq!(m, RequestMode::Cors);
}

#[test]
fn from_v8_known_string_with_override() {
    let m = run_with_value(r#""no-cors""#, |val, scope| {
        RequestMode::from_v8(scope, val).unwrap()
    });
    assert_eq!(m, RequestMode::NoCors);
}

#[test]
fn from_v8_unknown_throws_typeerror_with_value_and_set() {
    let err = run_with_value(r#""nope""#, |val, scope| {
        RequestMode::from_v8(scope, val).err()
    });
    let err = err.expect("expected TypeError on unknown");
    let msg = &err.message;
    assert!(msg.contains("nope"), "message should include offending value: {}", msg);
    assert!(msg.contains("RequestMode"), "message should include enum name: {}", msg);
    assert!(msg.contains("'cors'"), "message should list 'cors': {}", msg);
    assert!(msg.contains("'no-cors'"), "message should list 'no-cors': {}", msg);
}

#[test]
fn from_v8_tostring_coercion() {
    // Number 42 ToStrings to "42" — not a valid variant → TypeError.
    let err = run_with_value(r#"42"#, |val, scope| {
        RequestMode::from_v8(scope, val).err()
    });
    let err = err.expect("expected TypeError on numeric");
    assert!(err.message.contains("42"));
}

#[test]
fn from_v8_symbol_throws() {
    // Symbol values cannot ToString — should surface as TypeError.
    let err = run_with_value(r#"Symbol("x")"#, |val, scope| {
        RequestMode::from_v8(scope, val).err()
    });
    assert!(err.is_some(), "expected TypeError on Symbol");
}

// ---------------------------------------------------------------------------
// Default-kebab-case test enum, all variants without #[webidl_name].
// ---------------------------------------------------------------------------

#[derive(WebIdlEnum, Debug, PartialEq, Eq, Clone, Copy)]
enum CacheMode {
    Default,
    NoStore,
    Reload,
    NoCache,
    ForceCache,
    OnlyIfCached,
}

#[test]
fn cache_mode_kebab_naming() {
    assert_eq!(CacheMode::Default.as_str(), "default");
    assert_eq!(CacheMode::NoStore.as_str(), "no-store");
    assert_eq!(CacheMode::Reload.as_str(), "reload");
    assert_eq!(CacheMode::NoCache.as_str(), "no-cache");
    assert_eq!(CacheMode::ForceCache.as_str(), "force-cache");
    assert_eq!(CacheMode::OnlyIfCached.as_str(), "only-if-cached");
}

#[test]
fn cache_mode_from_str_round_trip() {
    for v in [
        CacheMode::Default,
        CacheMode::NoStore,
        CacheMode::Reload,
        CacheMode::NoCache,
        CacheMode::ForceCache,
        CacheMode::OnlyIfCached,
    ] {
        assert_eq!(CacheMode::from_str(v.as_str()), Some(v));
    }
}

#[test]
fn cache_mode_from_v8_known() {
    let m = run_with_value(r#""only-if-cached""#, |val, scope| {
        CacheMode::from_v8(scope, val).unwrap()
    });
    assert_eq!(m, CacheMode::OnlyIfCached);
}

// ---------------------------------------------------------------------------
// Single-variant enum (degenerate case — should still work).
// ---------------------------------------------------------------------------

#[derive(WebIdlEnum, Debug, PartialEq, Eq, Clone, Copy)]
enum SingleVariant {
    OnlyOne,
}

#[test]
fn single_variant_works() {
    assert_eq!(SingleVariant::OnlyOne.as_str(), "only-one");
    assert_eq!(SingleVariant::from_str("only-one"), Some(SingleVariant::OnlyOne));
    assert_eq!(SingleVariant::from_str("nope"), None);
}
