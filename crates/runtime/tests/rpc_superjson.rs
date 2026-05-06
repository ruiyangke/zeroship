//! V8 ↔ superjson Envelope round-trip tests.
//!
//! `crates/core/src/superjson.rs` defines the wire envelope and its
//! byte serializer; `crates/runtime/src/rpc/superjson.rs` is the V8
//! half that encodes V8 values into envelopes and revives envelopes
//! back to V8 values. These tests pin both directions plus the
//! cross-side byte equality with the npm fixtures.
//!
//! Test harness mirrors `tests/v8_iterable_brand_check_smoke.rs` — fresh
//! isolate per test, `install_globals` the URL class so the URL detection
//! path has a real native URL to look at.

#![allow(unsafe_code)]

use indexmap::IndexMap;
use zeroship_core::superjson::{self, MetaTag};
use zeroship_runtime::init_v8;
use zeroship_runtime::rpc::superjson::{
    decode_from_bytes, decode_to_v8, encode_to_bytes, encode_to_envelope,
};

// ---------------------------------------------------------------------------
// Test harness
// ---------------------------------------------------------------------------

/// Run `f` inside a fresh isolate with URL natively installed (so the
/// encoder's URL detection path can brand-check). `f` receives the
/// scope plus the global object so it can build the V8 value of
/// interest, then can call encode/decode and inspect the result.
fn with_v8<F, R>(f: F) -> R
where
    F: for<'s> FnOnce(&mut v8::PinScope<'s, '_>, v8::Local<'s, v8::Object>) -> R,
{
    init_v8();
    let mut isolate = v8::Isolate::new(v8::CreateParams::default());
    v8::scope!(let handle_scope, &mut isolate);
    let context = v8::Context::new(handle_scope, Default::default());
    let scope = &mut v8::ContextScope::new(handle_scope, context);
    let global = scope.get_current_context().global(scope);
    // Native URL installed so URL::is_instance fires inside the encoder.
    zeroship_runtime::url_native::install_globals(scope, global);
    f(scope, global)
}

/// Eval `src` in the current scope and return the resulting value.
fn eval<'s>(scope: &mut v8::PinScope<'s, '_>, src: &str) -> v8::Local<'s, v8::Value> {
    let src_v8 = v8::String::new(scope, src).unwrap();
    let script = v8::Script::compile(scope, src_v8, None).unwrap();
    script.run(scope).unwrap()
}

/// Read fixture bytes from the core crate's superjson_fixtures directory.
fn fx(name: &str) -> Vec<u8> {
    // Tests run with CARGO_MANIFEST_DIR = crates/runtime; reach into the
    // sibling crate.
    let path = format!(
        "{}/../core/tests/superjson_fixtures/{name}.json",
        env!("CARGO_MANIFEST_DIR")
    );
    std::fs::read(&path).unwrap_or_else(|_| panic!("fixture missing: {path}"))
}

/// JSON-stringify a V8 value (sanity-check shape from the test side).
fn json_stringify(scope: &mut v8::PinScope, v: v8::Local<v8::Value>) -> String {
    let s = v8::json::stringify(scope, v).expect("stringify");
    s.to_rust_string_lossy(scope)
}

// ---------------------------------------------------------------------------
// 1. Plain JSON round-trip
// ---------------------------------------------------------------------------

#[test]
fn plain_json_round_trip() {
    with_v8(|scope, _| {
        let v = eval(scope, r#"({a: 1, b: [2, 3], c: "x"})"#);
        let bytes = encode_to_bytes(scope, v).unwrap();
        // Plain JSON => no meta key on the wire.
        let s = std::str::from_utf8(&bytes).unwrap();
        assert!(!s.contains("\"meta\""), "leaked meta: {s}");
        let back = decode_from_bytes(scope, &bytes).unwrap();
        let json = json_stringify(scope, back);
        assert_eq!(json, r#"{"a":1,"b":[2,3],"c":"x"}"#);
    });
}

// ---------------------------------------------------------------------------
// 2. Date round-trip
// ---------------------------------------------------------------------------

#[test]
fn date_round_trip() {
    with_v8(|scope, _| {
        let v = eval(scope, r#"new Date("2026-01-01T00:00:00.000Z")"#);
        let env = encode_to_envelope(scope, v).unwrap();
        // Wire: `{json: "2026-01-01...", meta: {values: ["Date"], v: 1}}`
        assert_eq!(env.json, serde_json::json!("2026-01-01T00:00:00.000Z"));
        let meta = env.meta.as_ref().expect("Date must have meta");
        assert_eq!(meta.root, Some(MetaTag::Date));

        let back = decode_to_v8(scope, env).unwrap();
        assert!(back.is_date(), "decoded value must be Date");
        // Round-trip the iso string out via toISOString.
        let iso_call = eval(
            scope,
            r#"((d) => d.toISOString())"#,
        );
        let f: v8::Local<v8::Function> = iso_call.try_into().unwrap();
        let recv: v8::Local<v8::Value> = v8::undefined(scope).into();
        let result = f.call(scope, recv, &[back]).unwrap();
        assert_eq!(
            result.to_rust_string_lossy(scope),
            "2026-01-01T00:00:00.000Z"
        );
    });
}

// ---------------------------------------------------------------------------
// 3. BigInt round-trip
// ---------------------------------------------------------------------------

#[test]
fn bigint_round_trip() {
    with_v8(|scope, _| {
        // Number.MAX_SAFE_INTEGER + 2 — beyond JS Number precision, so the
        // BigInt-decimal-string round-trip is load-bearing.
        let v = eval(scope, r#"9007199254740993n"#);
        let env = encode_to_envelope(scope, v).unwrap();
        assert_eq!(env.json, serde_json::json!("9007199254740993"));
        assert_eq!(env.meta.as_ref().unwrap().root, Some(MetaTag::BigInt));

        let back = decode_to_v8(scope, env).unwrap();
        assert!(back.is_big_int());
        // Calling toString on the BigInt — verify the decimal text.
        let s = back.to_rust_string_lossy(scope);
        assert_eq!(s, "9007199254740993");
    });
}

#[test]
fn bigint_negative_round_trip() {
    with_v8(|scope, _| {
        let v = eval(scope, r#"(-9007199254740993n)"#);
        let env = encode_to_envelope(scope, v).unwrap();
        assert_eq!(env.json, serde_json::json!("-9007199254740993"));
        let back = decode_to_v8(scope, env).unwrap();
        assert_eq!(back.to_rust_string_lossy(scope), "-9007199254740993");
    });
}

// ---------------------------------------------------------------------------
// 4. Map round-trip
// ---------------------------------------------------------------------------

#[test]
fn map_round_trip() {
    with_v8(|scope, _| {
        let v = eval(scope, r#"new Map([["k", "v"]])"#);
        let env = encode_to_envelope(scope, v).unwrap();
        assert_eq!(env.json, serde_json::json!([["k", "v"]]));
        assert_eq!(env.meta.as_ref().unwrap().root, Some(MetaTag::Map));

        let back = decode_to_v8(scope, env).unwrap();
        assert!(back.is_map());
        let probe = eval(
            scope,
            r#"((m) => [m instanceof Map, m.size, m.get("k")])"#,
        );
        let f: v8::Local<v8::Function> = probe.try_into().unwrap();
        let recv: v8::Local<v8::Value> = v8::undefined(scope).into();
        let result = f.call(scope, recv, &[back]).unwrap();
        let s = json_stringify(scope, result);
        assert_eq!(s, r#"[true,1,"v"]"#);
    });
}

// ---------------------------------------------------------------------------
// 5. Set round-trip
// ---------------------------------------------------------------------------

#[test]
fn set_round_trip() {
    with_v8(|scope, _| {
        let v = eval(scope, r#"new Set([1, 2])"#);
        let env = encode_to_envelope(scope, v).unwrap();
        assert_eq!(env.json, serde_json::json!([1, 2]));
        assert_eq!(env.meta.as_ref().unwrap().root, Some(MetaTag::Set));

        let back = decode_to_v8(scope, env).unwrap();
        assert!(back.is_set());
        let probe = eval(
            scope,
            r#"((s) => [s instanceof Set, s.size, [...s]])"#,
        );
        let f: v8::Local<v8::Function> = probe.try_into().unwrap();
        let recv: v8::Local<v8::Value> = v8::undefined(scope).into();
        let result = f.call(scope, recv, &[back]).unwrap();
        let s = json_stringify(scope, result);
        assert_eq!(s, r#"[true,2,[1,2]]"#);
    });
}

// ---------------------------------------------------------------------------
// 6. RegExp round-trip
// ---------------------------------------------------------------------------

#[test]
fn regexp_round_trip() {
    with_v8(|scope, _| {
        let v = eval(scope, r#"/abc/g"#);
        let env = encode_to_envelope(scope, v).unwrap();
        assert_eq!(env.json, serde_json::json!("/abc/g"));
        assert_eq!(env.meta.as_ref().unwrap().root, Some(MetaTag::Regexp));

        let back = decode_to_v8(scope, env).unwrap();
        assert!(back.is_reg_exp());
        let probe = eval(
            scope,
            r#"((r) => [r.source, r.flags, r.global])"#,
        );
        let f: v8::Local<v8::Function> = probe.try_into().unwrap();
        let recv: v8::Local<v8::Value> = v8::undefined(scope).into();
        let result = f.call(scope, recv, &[back]).unwrap();
        let s = json_stringify(scope, result);
        assert_eq!(s, r#"["abc","g",true]"#);
    });
}

// ---------------------------------------------------------------------------
// 7. URL round-trip
// ---------------------------------------------------------------------------

#[test]
fn url_round_trip() {
    with_v8(|scope, _| {
        let v = eval(scope, r#"new URL("https://x.test/p?q=1")"#);
        let env = encode_to_envelope(scope, v).unwrap();
        assert_eq!(env.json, serde_json::json!("https://x.test/p?q=1"));
        assert_eq!(env.meta.as_ref().unwrap().root, Some(MetaTag::Url));

        let back = decode_to_v8(scope, env).unwrap();
        // Probe via URL.prototype.href getter.
        let probe = eval(
            scope,
            r#"((u) => [u instanceof URL, u.href])"#,
        );
        let f: v8::Local<v8::Function> = probe.try_into().unwrap();
        let recv: v8::Local<v8::Value> = v8::undefined(scope).into();
        let result = f.call(scope, recv, &[back]).unwrap();
        let s = json_stringify(scope, result);
        assert_eq!(s, r#"[true,"https://x.test/p?q=1"]"#);
    });
}

// ---------------------------------------------------------------------------
// 8. Uint8Array round-trip
// ---------------------------------------------------------------------------

#[test]
fn uint8_array_round_trip() {
    with_v8(|scope, _| {
        let v = eval(scope, r#"new Uint8Array([1,2,3,4])"#);
        let env = encode_to_envelope(scope, v).unwrap();
        assert_eq!(env.json, serde_json::json!([1, 2, 3, 4]));
        match env.meta.as_ref().unwrap().root {
            Some(MetaTag::TypedArray(ref ctor)) => assert_eq!(ctor, "Uint8Array"),
            ref other => panic!("expected typed-array tag, got {other:?}"),
        }

        let back = decode_to_v8(scope, env).unwrap();
        assert!(back.is_uint8_array());
        let probe = eval(
            scope,
            r#"((u) => [u instanceof Uint8Array, u.length, [...u]])"#,
        );
        let f: v8::Local<v8::Function> = probe.try_into().unwrap();
        let recv: v8::Local<v8::Value> = v8::undefined(scope).into();
        let result = f.call(scope, recv, &[back]).unwrap();
        let s = json_stringify(scope, result);
        assert_eq!(s, r#"[true,4,[1,2,3,4]]"#);
    });
}

// ---------------------------------------------------------------------------
// 9. Composite round-trip — matches composite.json fixture
// ---------------------------------------------------------------------------

#[test]
fn composite_round_trip() {
    with_v8(|scope, _| {
        // Mirror the composite.json fixture's source object, in order.
        let v = eval(
            scope,
            r#"({
                d: new Date("2026-01-01T00:00:00.000Z"),
                n: 9007199254740993n,
                m: new Map([["k","v"]]),
                st: new Set([1,2]),
                r: /abc/g,
                u: new URL("https://x.test/p?q=1"),
                b: new Uint8Array([1,2,3]),
                un: undefined,
                plain: { a: 1, b: [2, 3] },
            })"#,
        );
        let env = encode_to_envelope(scope, v).unwrap();
        let meta = env.meta.as_ref().expect("composite must carry meta");
        assert!(meta.root.is_none(), "root is plain object");
        let vals = &meta.values;
        assert_eq!(vals.get("d"), Some(&MetaTag::Date));
        assert_eq!(vals.get("n"), Some(&MetaTag::BigInt));
        assert_eq!(vals.get("m"), Some(&MetaTag::Map));
        assert_eq!(vals.get("st"), Some(&MetaTag::Set));
        assert_eq!(vals.get("r"), Some(&MetaTag::Regexp));
        assert_eq!(vals.get("u"), Some(&MetaTag::Url));
        assert_eq!(vals.get("un"), Some(&MetaTag::Undefined));
        match vals.get("b") {
            Some(MetaTag::TypedArray(ctor)) => assert_eq!(ctor, "Uint8Array"),
            other => panic!("expected typed-array tag for `b`, got {other:?}"),
        }
        // `plain` carries no tag (pure JSON).
        assert!(vals.get("plain").is_none());
    });
}

// ---------------------------------------------------------------------------
// 10. Cross-decode — load composite.json and revive into V8.
// ---------------------------------------------------------------------------

#[test]
fn cross_decode_composite_fixture() {
    with_v8(|scope, _| {
        let bytes = fx("composite");
        let back = decode_from_bytes(scope, &bytes).unwrap();
        let probe = eval(
            scope,
            r#"((o) => ({
                dIso:    o.d instanceof Date && o.d.toISOString(),
                nStr:    typeof o.n === "bigint" && o.n.toString(),
                mGet:    o.m instanceof Map && o.m.get("k"),
                stHas:   o.st instanceof Set && o.st.has(1) && o.st.has(2),
                rSrc:    o.r instanceof RegExp && o.r.source + "/" + o.r.flags,
                uHref:   o.u instanceof URL && o.u.href,
                bArr:    o.u && o.b instanceof Uint8Array && [...o.b],
                unUndef: o.un === undefined && Object.prototype.hasOwnProperty.call(o, "un"),
                plain:   o.plain.a === 1 && o.plain.b[1] === 3,
            }))"#,
        );
        let f: v8::Local<v8::Function> = probe.try_into().unwrap();
        let recv: v8::Local<v8::Value> = v8::undefined(scope).into();
        let result = f.call(scope, recv, &[back]).unwrap();
        let s = json_stringify(scope, result);
        // Spot-check each field. We compare a stringified shape so a single
        // assertion captures the full round-trip — easier to debug than
        // many per-field asserts.
        assert_eq!(
            s,
            r#"{"dIso":"2026-01-01T00:00:00.000Z","nStr":"9007199254740993","mGet":"v","stHas":true,"rSrc":"abc/g","uHref":"https://x.test/p?q=1","bArr":[1,2,3],"unUndef":true,"plain":true}"#,
        );
    });
}

// ---------------------------------------------------------------------------
// 11. Cross-encode — V8 composite ⇒ bytes equal to composite.json fixture.
// ---------------------------------------------------------------------------

#[test]
fn cross_encode_composite_fixture_bytes() {
    with_v8(|scope, _| {
        let v = eval(
            scope,
            r#"({
                d: new Date("2026-01-01T00:00:00.000Z"),
                n: 9007199254740993n,
                m: new Map([["k","v"]]),
                st: new Set([1,2]),
                r: /abc/g,
                u: new URL("https://x.test/p?q=1"),
                b: new Uint8Array([1,2,3]),
                un: undefined,
                plain: { a: 1, b: [2, 3] },
            })"#,
        );
        let bytes = encode_to_bytes(scope, v).unwrap();
        let expected = fx("composite");
        assert_eq!(
            bytes,
            expected,
            "wire bytes diverge from npm fixture\n  ours: {}\n  npm:  {}",
            String::from_utf8_lossy(&bytes),
            String::from_utf8_lossy(&expected),
        );
    });
}

// ---------------------------------------------------------------------------
// 12. Undefined object value — omitted from json, tagged in meta, revived
//     as undefined on decode.
// ---------------------------------------------------------------------------

#[test]
fn undefined_object_value_round_trip() {
    with_v8(|scope, _| {
        let v = eval(scope, r#"({ a: 1, b: undefined })"#);
        let env = encode_to_envelope(scope, v).unwrap();
        // npm wire: keeps `b` with a `null` value in the JSON shadow and
        // tags it `["undefined"]` in meta. (Composite fixture confirms.)
        assert_eq!(env.json, serde_json::json!({ "a": 1, "b": null }));
        let meta = env.meta.as_ref().unwrap();
        assert_eq!(meta.values.get("b"), Some(&MetaTag::Undefined));

        let back = decode_to_v8(scope, env).unwrap();
        // Probe: own property `b` exists and is === undefined.
        let probe = eval(
            scope,
            r#"((o) => Object.prototype.hasOwnProperty.call(o, "b") && o.b === undefined && o.a === 1)"#,
        );
        let f: v8::Local<v8::Function> = probe.try_into().unwrap();
        let recv: v8::Local<v8::Value> = v8::undefined(scope).into();
        let result = f.call(scope, recv, &[back]).unwrap();
        assert!(result.is_true(), "got: {}", json_stringify(scope, result));
    });
}

// ---------------------------------------------------------------------------
// 13. Special floats — NaN / Infinity / -Infinity, top-level + nested.
// ---------------------------------------------------------------------------

#[test]
fn special_floats_round_trip() {
    with_v8(|scope, _| {
        // Top-level NaN.
        {
            let v = eval(scope, r#"NaN"#);
            let env = encode_to_envelope(scope, v).unwrap();
            assert_eq!(env.json, serde_json::json!("NaN"));
            assert_eq!(env.meta.as_ref().unwrap().root, Some(MetaTag::Number));
            let back = decode_to_v8(scope, env).unwrap();
            assert!(back.is_number());
            // V8's Number.isNaN check.
            let probe = eval(scope, r#"((n) => Number.isNaN(n))"#);
            let f: v8::Local<v8::Function> = probe.try_into().unwrap();
            let recv: v8::Local<v8::Value> = v8::undefined(scope).into();
            assert!(f.call(scope, recv, &[back]).unwrap().is_true());
        }
        // Top-level Infinity / -Infinity.
        for (src, sentinel) in [("Infinity", "Infinity"), ("-Infinity", "-Infinity")] {
            let v = eval(scope, src);
            let env = encode_to_envelope(scope, v).unwrap();
            assert_eq!(env.json, serde_json::json!(sentinel));
            let back = decode_to_v8(scope, env).unwrap();
            assert_eq!(back.to_rust_string_lossy(scope), sentinel);
        }
        // Nested in an object.
        {
            let v = eval(
                scope,
                r#"({ x: NaN, y: Infinity, z: -Infinity, ok: 1 })"#,
            );
            let env = encode_to_envelope(scope, v).unwrap();
            assert_eq!(
                env.json,
                serde_json::json!({
                    "x": "NaN",
                    "y": "Infinity",
                    "z": "-Infinity",
                    "ok": 1,
                }),
            );
            let m = env.meta.as_ref().unwrap();
            assert_eq!(m.values.get("x"), Some(&MetaTag::Number));
            assert_eq!(m.values.get("y"), Some(&MetaTag::Number));
            assert_eq!(m.values.get("z"), Some(&MetaTag::Number));
            assert!(m.values.get("ok").is_none());
        }
    });
}

// ---------------------------------------------------------------------------
// 14. WithChildren root — `new Set([new Date(...)])`.
// ---------------------------------------------------------------------------

#[test]
fn with_children_root_set_of_date() {
    with_v8(|scope, _| {
        let v = eval(
            scope,
            r#"new Set([new Date("2026-01-01T00:00:00.000Z"), "foo"])"#,
        );
        let env = encode_to_envelope(scope, v).unwrap();
        assert_eq!(env.json, serde_json::json!(["2026-01-01T00:00:00.000Z", "foo"]));
        let meta = env.meta.as_ref().unwrap();
        match &meta.root {
            Some(MetaTag::WithChildren(inner, children)) => {
                assert_eq!(**inner, MetaTag::Set);
                assert_eq!(children.get("0"), Some(&MetaTag::Date));
                assert!(children.get("1").is_none());
            }
            other => panic!("expected WithChildren wrapping Set, got {other:?}"),
        }

        // Round-trip.
        let bytes = superjson::to_bytes(&env);
        let back = decode_from_bytes(scope, &bytes).unwrap();
        let probe = eval(
            scope,
            r#"((s) => {
                const arr = [...s];
                return [
                    s instanceof Set,
                    arr.length === 2,
                    arr[0] instanceof Date && arr[0].toISOString(),
                    arr[1],
                ];
            })"#,
        );
        let f: v8::Local<v8::Function> = probe.try_into().unwrap();
        let recv: v8::Local<v8::Value> = v8::undefined(scope).into();
        let result = f.call(scope, recv, &[back]).unwrap();
        let s = json_stringify(scope, result);
        assert_eq!(s, r#"[true,true,"2026-01-01T00:00:00.000Z","foo"]"#);
    });
}

// ---------------------------------------------------------------------------
// 15. Empty / bare values — null / undefined / 42 / bare envelope.
// ---------------------------------------------------------------------------

#[test]
fn root_null_round_trip() {
    with_v8(|scope, _| {
        let v: v8::Local<v8::Value> = v8::null(scope).into();
        let bytes = encode_to_bytes(scope, v).unwrap();
        // Plain null => `{"json":null}` (no meta).
        assert_eq!(bytes, br#"{"json":null}"#.to_vec());
        let back = decode_from_bytes(scope, &bytes).unwrap();
        assert!(back.is_null());
    });
}

#[test]
fn root_undefined_round_trip() {
    with_v8(|scope, _| {
        let v: v8::Local<v8::Value> = v8::undefined(scope).into();
        let env = encode_to_envelope(scope, v).unwrap();
        // Per the npm wire, top-level undefined ⇒ json null + meta ["undefined"].
        assert_eq!(env.json, serde_json::json!(null));
        assert_eq!(env.meta.as_ref().unwrap().root, Some(MetaTag::Undefined));
        let bytes = superjson::to_bytes(&env);
        // Should match `undefined.json` byte-for-byte.
        assert_eq!(bytes, fx("undefined"));
        let back = decode_from_bytes(scope, &bytes).unwrap();
        assert!(back.is_undefined());
    });
}

#[test]
fn root_number_round_trip() {
    with_v8(|scope, _| {
        let v = eval(scope, "42");
        let bytes = encode_to_bytes(scope, v).unwrap();
        assert_eq!(bytes, br#"{"json":42}"#.to_vec());
        let back = decode_from_bytes(scope, &bytes).unwrap();
        assert_eq!(back.to_rust_string_lossy(scope), "42");
    });
}

#[test]
fn bare_value_envelope_decodes() {
    with_v8(|scope, _| {
        // Legacy clients may send bare JSON without the {json,meta} wrapper.
        // Our decoder must accept that and revive it without meta.
        let back = decode_from_bytes(scope, b"42").unwrap();
        assert_eq!(back.to_rust_string_lossy(scope), "42");

        let back = decode_from_bytes(scope, b"[1,2,3]").unwrap();
        assert!(back.is_array());

        let back = decode_from_bytes(scope, b"null").unwrap();
        assert!(back.is_null());
    });
}

// ---------------------------------------------------------------------------
// Bonus — Sets/Maps with non-numeric key paths exercising "0.0" / "0.1".
// ---------------------------------------------------------------------------

#[test]
fn map_with_rich_key_round_trip() {
    with_v8(|scope, _| {
        let v = eval(
            scope,
            r#"new Map([[new Date("2026-01-01T00:00:00.000Z"), "v"]])"#,
        );
        let env = encode_to_envelope(scope, v).unwrap();
        assert_eq!(env.json, serde_json::json!([["2026-01-01T00:00:00.000Z", "v"]]));
        match &env.meta.as_ref().unwrap().root {
            Some(MetaTag::WithChildren(inner, children)) => {
                assert_eq!(**inner, MetaTag::Map);
                assert_eq!(children.get("0.0"), Some(&MetaTag::Date));
            }
            other => panic!("expected WithChildren wrapping Map, got {other:?}"),
        }

        let bytes = superjson::to_bytes(&env);
        // Byte equivalence with `map_rich_key.json`.
        assert_eq!(bytes, fx("map_rich_key"));
        let back = decode_from_bytes(scope, &bytes).unwrap();
        let probe = eval(
            scope,
            r#"((m) => {
                const [[k, v]] = [...m.entries()];
                return [m instanceof Map, k instanceof Date, k.toISOString(), v];
            })"#,
        );
        let f: v8::Local<v8::Function> = probe.try_into().unwrap();
        let recv: v8::Local<v8::Value> = v8::undefined(scope).into();
        let result = f.call(scope, recv, &[back]).unwrap();
        let s = json_stringify(scope, result);
        assert_eq!(s, r#"[true,true,"2026-01-01T00:00:00.000Z","v"]"#);
    });
}

// ---------------------------------------------------------------------------
// Sanity helper used in encode-side bytes equivalence — make sure the
// fixture bytes parse via the core crate identically to ours.
// ---------------------------------------------------------------------------

#[test]
fn fixture_meta_parses_consistently() {
    // No V8 needed — pure cross-check that our IndexMap shape matches.
    let env = superjson::from_bytes(&fx("composite")).unwrap();
    let meta = env.meta.unwrap();
    let mut expected: IndexMap<String, MetaTag> = IndexMap::new();
    expected.insert("d".into(), MetaTag::Date);
    expected.insert("n".into(), MetaTag::BigInt);
    expected.insert("m".into(), MetaTag::Map);
    expected.insert("st".into(), MetaTag::Set);
    expected.insert("r".into(), MetaTag::Regexp);
    expected.insert("u".into(), MetaTag::Url);
    expected.insert("b".into(), MetaTag::TypedArray("Uint8Array".into()));
    expected.insert("un".into(), MetaTag::Undefined);
    let got_keys: Vec<_> = meta.values.keys().cloned().collect();
    let expected_keys: Vec<_> = expected.keys().cloned().collect();
    assert_eq!(got_keys, expected_keys);
}

// ---------------------------------------------------------------------------
// Encoder rejection — non-Uint8Array typed arrays in the current ABI.
// ---------------------------------------------------------------------------

#[test]
fn non_uint8_typed_array_rejected_phase1() {
    with_v8(|scope, _| {
        // Int32Array is a typed array but not Uint8Array. The current
        // ABI only supports Uint8Array, so reject explicitly.
        let v = eval(scope, r#"new Int32Array([1, 2, 3])"#);
        let err = encode_to_envelope(scope, v).expect_err("should reject");
        let msg = err.message.to_lowercase();
        assert!(
            msg.contains("uint8array") || msg.contains("typed array"),
            "expected typed-array rejection, got: {}",
            err.message,
        );
    });
}
