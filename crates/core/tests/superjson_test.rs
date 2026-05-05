//! Wire-format tests for `zeroship_core::superjson`.
//!
//! Fixtures in `superjson_fixtures/*.json` were generated against the npm
//! `superjson@2.x` library — they ARE the wire-format contract.

use serde_json::{json, Value};
use zeroship_core::superjson::{self, Envelope, Meta, MetaTag};

fn fx(name: &str) -> Vec<u8> {
    let path = format!(
        "{}/tests/superjson_fixtures/{name}.json",
        env!("CARGO_MANIFEST_DIR")
    );
    std::fs::read(&path).unwrap_or_else(|_| panic!("fixture missing: {path}"))
}

// ---- to_bytes / from_bytes byte-exact round-trip against npm fixtures ----

fn assert_round_trip(name: &str) {
    let bytes = fx(name);
    let env = superjson::from_bytes(&bytes).expect("parse");
    let out = superjson::to_bytes(&env);
    assert_eq!(
        out,
        bytes,
        "wire format diverges from npm fixture {name}\n  ours: {}\n  npm:  {}",
        String::from_utf8_lossy(&out),
        String::from_utf8_lossy(&bytes)
    );
}

#[test]
fn fixture_round_trip_date() {
    assert_round_trip("date");
}

#[test]
fn fixture_round_trip_bigint() {
    assert_round_trip("bigint");
}

#[test]
fn fixture_round_trip_map() {
    assert_round_trip("map");
}

#[test]
fn fixture_round_trip_set() {
    assert_round_trip("set");
}

#[test]
fn fixture_round_trip_regexp() {
    assert_round_trip("regexp");
}

#[test]
fn fixture_round_trip_url() {
    assert_round_trip("url");
}

#[test]
fn fixture_round_trip_uint8array() {
    assert_round_trip("uint8array");
}

#[test]
fn fixture_round_trip_undefined() {
    assert_round_trip("undefined");
}

#[test]
fn fixture_round_trip_composite() {
    assert_round_trip("composite");
}

#[test]
fn fixture_round_trip_nested_path() {
    assert_round_trip("nested_path");
}

#[test]
fn fixture_round_trip_array_indices() {
    assert_round_trip("array_indices");
}

#[test]
fn fixture_round_trip_map_rich_key() {
    assert_round_trip("map_rich_key");
}

#[test]
fn fixture_round_trip_set_rich_values() {
    assert_round_trip("set_rich_values");
}

#[test]
fn fixture_round_trip_infinity() {
    assert_round_trip("infinity");
}

#[test]
fn fixture_round_trip_nan() {
    assert_round_trip("nan");
}

#[test]
fn fixture_round_trip_plain_object() {
    assert_round_trip("plain_object");
}

#[test]
fn fixture_round_trip_plain_array() {
    assert_round_trip("plain_array");
}

#[test]
fn fixture_round_trip_plain_number() {
    assert_round_trip("plain_number");
}

// ---- meta semantics ----

#[test]
fn meta_omitted_when_no_rich_types() {
    // Plain JSON values produce no meta field on the wire.
    let env = superjson::from_bytes(&fx("plain_object")).unwrap();
    assert!(env.meta.is_none(), "plain payload must not carry meta");

    let bytes = superjson::to_bytes(&env);
    let s = std::str::from_utf8(&bytes).unwrap();
    assert!(!s.contains("\"meta\""), "wire output leaked meta: {s}");
}

#[test]
fn meta_paths_match_composite_fixture() {
    let env = superjson::from_bytes(&fx("composite")).unwrap();
    let meta = env.meta.expect("composite must have meta");

    // Root is a plain object — root tag is None.
    assert!(meta.root.is_none());

    let v = &meta.values;
    assert_eq!(v.get("d"), Some(&MetaTag::Date));
    assert_eq!(v.get("n"), Some(&MetaTag::BigInt));
    assert_eq!(v.get("m"), Some(&MetaTag::Map));
    assert_eq!(v.get("st"), Some(&MetaTag::Set));
    assert_eq!(v.get("r"), Some(&MetaTag::Regexp));
    assert_eq!(v.get("u"), Some(&MetaTag::Url));
    assert_eq!(v.get("un"), Some(&MetaTag::Undefined));

    match v.get("b") {
        Some(MetaTag::TypedArray(ctor)) => assert_eq!(ctor, "Uint8Array"),
        other => panic!("expected typed-array tag for `b`, got {other:?}"),
    }
}

#[test]
fn root_rich_type_uses_root_tag() {
    let env = superjson::from_bytes(&fx("date")).unwrap();
    let meta = env.meta.expect("root Date must have meta");
    assert_eq!(meta.root, Some(MetaTag::Date));
    assert!(meta.values.is_empty());
}

#[test]
fn root_set_with_rich_children_packs_meta() {
    let env = superjson::from_bytes(&fx("set_rich_values")).unwrap();
    let meta = env.meta.expect("rich set must have meta");
    // Root has the "set" tag AND nested children at index 0.
    match meta.root {
        Some(MetaTag::WithChildren(ref root_tag, ref children)) => {
            assert_eq!(**root_tag, MetaTag::Set);
            assert_eq!(children.get("0"), Some(&MetaTag::Date));
        }
        ref other => panic!("expected WithChildren wrapping Set, got {other:?}"),
    }
}

#[test]
fn special_floats_use_number_tag() {
    let env = superjson::from_bytes(&fx("infinity")).unwrap();
    let meta = env.meta.expect("Infinity must have meta");
    assert_eq!(meta.root, Some(MetaTag::Number));
    assert_eq!(env.json, json!("Infinity"));
}

// ---- bare-value compat (legacy clients) ----

#[test]
fn bare_value_parsed_as_envelope() {
    let env = superjson::from_bytes(b"42").unwrap();
    assert_eq!(env.json, json!(42));
    assert!(env.meta.is_none());
}

#[test]
fn bare_object_parsed_as_envelope() {
    let env = superjson::from_bytes(br#"{"hello":"world"}"#).unwrap();
    // {hello:"world"} has no `json` key → treated as a bare value.
    assert_eq!(env.json, json!({"hello": "world"}));
    assert!(env.meta.is_none());
}

#[test]
fn bare_array_parsed_as_envelope() {
    let env = superjson::from_bytes(b"[1,2,3]").unwrap();
    assert_eq!(env.json, json!([1, 2, 3]));
    assert!(env.meta.is_none());
}

#[test]
fn bare_null_parsed_as_envelope() {
    let env = superjson::from_bytes(b"null").unwrap();
    assert_eq!(env.json, Value::Null);
    assert!(env.meta.is_none());
}

// ---- public API: encode / decode ----

#[test]
fn encode_plain_value_yields_no_meta() {
    let env = superjson::encode(json!({"a": 1, "b": [2, 3]}));
    assert!(env.meta.is_none());
    assert_eq!(env.json, json!({"a": 1, "b": [2, 3]}));
}

#[test]
fn decode_strips_meta_returns_shadow() {
    let env = superjson::from_bytes(&fx("composite")).unwrap();
    let shadow = superjson::decode(env).unwrap();
    // Shadow is the raw JSON-side projection — Date is still an ISO string,
    // BigInt is still a decimal string, etc. Wave C reconstructs in V8.
    assert_eq!(shadow["d"], json!("2026-01-01T00:00:00.000Z"));
    assert_eq!(shadow["n"], json!("9007199254740993"));
}

#[test]
fn encode_decode_round_trip_for_plain_json() {
    let original = json!({"a": 1, "b": [2, 3], "c": {"nested": true}});
    let env = superjson::encode(original.clone());
    let back = superjson::decode(env).unwrap();
    assert_eq!(back, original);
}

// ---- malformed envelope error paths ----

#[test]
fn from_bytes_rejects_invalid_json() {
    let err = superjson::from_bytes(b"not json").unwrap_err();
    assert!(matches!(err, superjson::Error::InvalidJson(_)));
}

#[test]
fn from_bytes_rejects_envelope_with_non_object_meta() {
    // `meta` MUST be an object (or absent). A bare string is invalid.
    let err = superjson::from_bytes(br#"{"json":1,"meta":"oops"}"#).unwrap_err();
    assert!(matches!(err, superjson::Error::MalformedMeta(_)));
}

#[test]
fn from_bytes_rejects_unknown_tag() {
    let err =
        superjson::from_bytes(br#"{"json":1,"meta":{"values":["banana"],"v":1}}"#).unwrap_err();
    assert!(matches!(err, superjson::Error::UnknownTag(_)));
}

// ---- npm-side deserialize compatibility (shells out to node) ----

#[cfg(test)]
fn node_available() -> bool {
    std::process::Command::new("node")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Round-trip our `to_bytes` output through `superjson.deserialize` in npm
/// to prove the receiving end accepts what we emit.
#[test]
fn npm_deserialize_accepts_our_bytes() {
    if !node_available() {
        eprintln!("skipping: node not available");
        return;
    }
    // Use a tmpdir that already has superjson installed (created in the
    // setup phase). If not, skip.
    let tmp = "/tmp/sj-fixture";
    if !std::path::Path::new(&format!("{tmp}/node_modules/superjson")).exists() {
        eprintln!("skipping: /tmp/sj-fixture/node_modules/superjson missing");
        return;
    }

    for name in ["composite", "date", "bigint", "map", "set", "url", "regexp"] {
        let bytes = fx(name);
        let env = superjson::from_bytes(&bytes).unwrap();
        let our = superjson::to_bytes(&env);
        let our_str = std::str::from_utf8(&our).unwrap();

        // Verify npm can deserialize our output without error.
        let out = std::process::Command::new("node")
            .current_dir(tmp)
            .arg("-e")
            .arg(
                "const s=require('superjson');\
                 const env=JSON.parse(process.argv[1]);\
                 const v=s.deserialize(env);\
                 process.stdout.write('ok');",
            )
            .arg(our_str)
            .output()
            .expect("spawn node");
        assert!(
            out.status.success(),
            "npm rejected our bytes for {name}: stderr={}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

// ---- structural sanity: Meta default + helpers ----

#[test]
fn empty_meta_round_trips_as_no_meta() {
    // An envelope with explicitly empty Meta should serialize WITHOUT
    // a meta key — that's the "common case 80%" that the npm wire elides.
    let env = Envelope {
        json: json!(42),
        meta: Some(Meta::default()),
    };
    let bytes = superjson::to_bytes(&env);
    let s = std::str::from_utf8(&bytes).unwrap();
    assert!(!s.contains("\"meta\""), "empty Meta leaked onto the wire: {s}");
}
