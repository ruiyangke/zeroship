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

// ---------------------------------------------------------------------------
// `#[webidl_enum(case_insensitive)]` — ASCII case-insensitive matching.
// Spec consumer: WebCrypto `HashAlgo` accepts "SHA-256" / "sha-256" /
// "Sha-256" all as the same algorithm (per WebCrypto §15 algorithm
// normalisation). Default behaviour is case-sensitive (preserved); the
// flag opts in.
// ---------------------------------------------------------------------------

#[derive(WebIdlEnum, Debug, PartialEq, Eq, Clone, Copy)]
#[webidl_enum(case_insensitive)]
enum HashAlgo {
    #[webidl_name = "SHA-1"]
    Sha1,
    #[webidl_name = "SHA-256"]
    Sha256,
    #[webidl_name = "SHA-384"]
    Sha384,
    #[webidl_name = "SHA-512"]
    Sha512,
}

#[test]
fn case_insensitive_from_str_exact() {
    assert_eq!(HashAlgo::from_str("SHA-256"), Some(HashAlgo::Sha256));
}

#[test]
fn case_insensitive_from_str_lower() {
    assert_eq!(HashAlgo::from_str("sha-256"), Some(HashAlgo::Sha256));
}

#[test]
fn case_insensitive_from_str_mixed() {
    assert_eq!(HashAlgo::from_str("Sha-256"), Some(HashAlgo::Sha256));
    assert_eq!(HashAlgo::from_str("sHa-256"), Some(HashAlgo::Sha256));
}

#[test]
fn case_insensitive_unknown_returns_none() {
    assert_eq!(HashAlgo::from_str("md5"), None);
    assert_eq!(HashAlgo::from_str("SHA-256-OOPS"), None);
}

#[test]
fn case_insensitive_from_v8_lowercase() {
    let m = run_with_value(r#""sha-512""#, |val, scope| {
        HashAlgo::from_v8(scope, val).unwrap()
    });
    assert_eq!(m, HashAlgo::Sha512);
}

#[test]
fn case_insensitive_from_v8_uppercase() {
    let m = run_with_value(r#""SHA-1""#, |val, scope| {
        HashAlgo::from_v8(scope, val).unwrap()
    });
    assert_eq!(m, HashAlgo::Sha1);
}

#[test]
fn case_insensitive_from_v8_mixed() {
    let m = run_with_value(r#""Sha-384""#, |val, scope| {
        HashAlgo::from_v8(scope, val).unwrap()
    });
    assert_eq!(m, HashAlgo::Sha384);
}

#[test]
fn case_insensitive_from_v8_unknown_throws() {
    let err = run_with_value(r#""md5""#, |val, scope| {
        HashAlgo::from_v8(scope, val).err()
    });
    assert!(err.is_some(), "expected TypeError on unknown");
}

// Without `case_insensitive`, the existing `RequestMode` enum stays
// strict — verify (regression guard).
#[test]
fn case_sensitive_default_rejects_uppercase() {
    assert_eq!(RequestMode::from_str("CORS"), None);
    let err = run_with_value(r#""CORS""#, |val, scope| {
        RequestMode::from_v8(scope, val).err()
    });
    assert!(err.is_some(), "case-sensitive enum should reject uppercase");
}

// Mixed: a kebab-case variant with case_insensitive matches the
// upper-cased input (`LONG-NAME` → variant `Long-name`).
#[derive(WebIdlEnum, Debug, PartialEq, Eq, Clone, Copy)]
#[webidl_enum(case_insensitive)]
enum MultiWord {
    LongName,
    AnotherOne,
}

#[test]
fn case_insensitive_kebab_default_uppercase() {
    // Default kebab-case naming: `LongName` → "long-name". With
    // case_insensitive, "LONG-NAME" should match.
    assert_eq!(MultiWord::LongName.as_str(), "long-name");
    assert_eq!(MultiWord::from_str("LONG-NAME"), Some(MultiWord::LongName));
    assert_eq!(MultiWord::from_str("Long-Name"), Some(MultiWord::LongName));
    assert_eq!(MultiWord::from_str("ANOTHER-ONE"), Some(MultiWord::AnotherOne));
}

// ---------------------------------------------------------------------------
// `#[webidl_enum(silent_default)]` — fall through to default on
// unknown rather than throwing TypeError. Spec consumers: Fetch
// `RedirectMode`, `CredentialsMode`, WebSocket `BinaryType`. The
// derive REQUIRES `Self: Default` (compile error if the impl is
// missing) since the codegen invokes `<Self as Default>::default()`.
// ---------------------------------------------------------------------------

#[derive(WebIdlEnum, Debug, PartialEq, Eq, Clone, Copy, Default)]
#[webidl_enum(silent_default)]
enum RedirectMode {
    #[default]
    Follow,
    Error,
    Manual,
}

#[test]
fn silent_default_from_str_known() {
    // Known values still resolve to the corresponding variant.
    assert_eq!(RedirectMode::from_str("follow"), Some(RedirectMode::Follow));
    assert_eq!(RedirectMode::from_str("error"), Some(RedirectMode::Error));
    assert_eq!(RedirectMode::from_str("manual"), Some(RedirectMode::Manual));
}

#[test]
fn silent_default_from_str_unknown_returns_default() {
    // Unknown → Some(default), NOT None. Lets the consumer use
    // `.unwrap()` confidently.
    assert_eq!(
        RedirectMode::from_str("garbage-value"),
        Some(RedirectMode::Follow)
    );
    assert_eq!(RedirectMode::from_str(""), Some(RedirectMode::Follow));
}

#[test]
fn silent_default_from_v8_unknown_returns_default_no_throw() {
    // Unknown JS string → default variant, no throw.
    let m = run_with_value(r#""garbage-value""#, |val, scope| {
        RedirectMode::from_v8(scope, val).unwrap()
    });
    assert_eq!(m, RedirectMode::Follow);
}

#[test]
fn silent_default_from_v8_known() {
    let m = run_with_value(r#""error""#, |val, scope| {
        RedirectMode::from_v8(scope, val).unwrap()
    });
    assert_eq!(m, RedirectMode::Error);
}

#[test]
fn silent_default_from_v8_symbol_returns_default() {
    // Symbol → ToString throws. silent_default treats the throw as
    // "not one of the listed values" and returns the default.
    let m = run_with_value(r#"Symbol("x")"#, |val, scope| {
        RedirectMode::from_v8(scope, val).unwrap()
    });
    assert_eq!(m, RedirectMode::Follow);
}

#[test]
fn silent_default_from_v8_number_tostring_unknown() {
    // 42 ToStrings to "42" — unknown → default.
    let m = run_with_value(r#"42"#, |val, scope| {
        RedirectMode::from_v8(scope, val).unwrap()
    });
    assert_eq!(m, RedirectMode::Follow);
}

// silent_default + case_insensitive — combinable.
#[derive(WebIdlEnum, Debug, PartialEq, Eq, Clone, Copy, Default)]
#[webidl_enum(silent_default, case_insensitive)]
enum BinaryType {
    #[default]
    Blob,
    #[webidl_name = "arraybuffer"]
    ArrayBuffer,
}

#[test]
fn silent_default_and_case_insensitive_combine() {
    // Known with mixed case
    assert_eq!(BinaryType::from_str("BLOB"), Some(BinaryType::Blob));
    assert_eq!(
        BinaryType::from_str("ArrayBuffer"),
        Some(BinaryType::ArrayBuffer)
    );
    // Unknown → default (no throw, no Err)
    assert_eq!(BinaryType::from_str("string"), Some(BinaryType::Blob));

    let m = run_with_value(r#""nope""#, |val, scope| {
        BinaryType::from_v8(scope, val).unwrap()
    });
    assert_eq!(m, BinaryType::Blob);

    let m = run_with_value(r#""ARRAYBUFFER""#, |val, scope| {
        BinaryType::from_v8(scope, val).unwrap()
    });
    assert_eq!(m, BinaryType::ArrayBuffer);
}

// Without silent_default, regression guard: existing `RequestMode`
// still throws on unknown.
#[test]
fn without_silent_default_unknown_still_throws() {
    let err = run_with_value(r#""invalid""#, |val, scope| {
        RequestMode::from_v8(scope, val).err()
    });
    assert!(err.is_some(), "default behaviour must throw on unknown");
}
