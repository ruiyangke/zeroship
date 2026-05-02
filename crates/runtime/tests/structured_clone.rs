//! Native structuredClone tests. WHATWG HTML §2.7.3
//! (https://html.spec.whatwg.org/multipage/structured-data.html#dom-structuredclone).
//!
//! Validates real WHATWG structured clone, not the JSON-roundtrip
//! polyfill the old fetch.js shipped.

mod common;
use common::{dispatch, m};

#[test]
fn clones_simple_object() {
    let r = dispatch(
        m(r#"export function test() {
            const obj = { a: 1, b: [2, 3], c: { d: "hello" } };
            const clone = structuredClone(obj);
            clone.a = 99;
            clone.b.push(4);
            return {
                origA: obj.a,
                cloneA: clone.a,
                origLen: obj.b.length,
                cloneLen: clone.b.length,
            };
        }"#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(r.json.contains("\"origA\":1"), "got: {}", r.json);
    assert!(r.json.contains("\"cloneA\":99"), "got: {}", r.json);
    assert!(r.json.contains("\"origLen\":2"), "got: {}", r.json);
    assert!(r.json.contains("\"cloneLen\":3"), "got: {}", r.json);
}

#[test]
fn clones_map_preserving_entries() {
    // The JSON-roundtrip polyfill yielded `{}` for Map. Native must
    // preserve entries.
    let r = dispatch(
        m(r#"export function test() {
            const orig = new Map([["a", 1], ["b", 2]]);
            const clone = structuredClone(orig);
            return {
                isMap: clone instanceof Map,
                size: clone.size,
                a: clone.get("a"),
                b: clone.get("b"),
            };
        }"#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(r.json.contains("\"isMap\":true"), "got: {}", r.json);
    assert!(r.json.contains("\"size\":2"), "got: {}", r.json);
    assert!(r.json.contains("\"a\":1"), "got: {}", r.json);
    assert!(r.json.contains("\"b\":2"), "got: {}", r.json);
}

#[test]
fn clones_set_preserving_entries() {
    let r = dispatch(
        m(r#"export function test() {
            const orig = new Set([1, 2, 3]);
            const clone = structuredClone(orig);
            return {
                isSet: clone instanceof Set,
                size: clone.size,
                hasOne: clone.has(1),
                hasFour: clone.has(4),
            };
        }"#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(r.json.contains("\"isSet\":true"), "got: {}", r.json);
    assert!(r.json.contains("\"size\":3"), "got: {}", r.json);
    assert!(r.json.contains("\"hasOne\":true"), "got: {}", r.json);
    assert!(r.json.contains("\"hasFour\":false"), "got: {}", r.json);
}

#[test]
fn clones_date_preserving_value() {
    // The polyfill stringified Date. Native must clone as Date.
    let r = dispatch(
        m(r#"export function test() {
            const orig = new Date(1700000000000);
            const clone = structuredClone(orig);
            return {
                isDate: clone instanceof Date,
                ts: clone.getTime(),
                isSame: clone !== orig,
            };
        }"#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(r.json.contains("\"isDate\":true"), "got: {}", r.json);
    assert!(r.json.contains("\"ts\":1700000000000"), "got: {}", r.json);
    assert!(r.json.contains("\"isSame\":true"), "got: {}", r.json);
}

#[test]
fn clones_array_buffer() {
    // ArrayBuffer is cloneable per spec.
    let r = dispatch(
        m(r#"export function test() {
            const orig = new ArrayBuffer(8);
            const view = new Uint8Array(orig);
            view[0] = 0xAA;
            view[7] = 0xBB;
            const clone = structuredClone(orig);
            const cv = new Uint8Array(clone);
            cv[0] = 0xCC; // mutate clone
            return {
                isAB: clone instanceof ArrayBuffer,
                size: clone.byteLength,
                origUntouched: view[0],
                cloneFirst: cv[0],
                cloneLast: cv[7],
            };
        }"#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(r.json.contains("\"isAB\":true"), "got: {}", r.json);
    assert!(r.json.contains("\"size\":8"), "got: {}", r.json);
    assert!(r.json.contains("\"origUntouched\":170"), "got: {}", r.json); // 0xAA = 170
    assert!(r.json.contains("\"cloneFirst\":204"), "got: {}", r.json); // 0xCC = 204
    assert!(r.json.contains("\"cloneLast\":187"), "got: {}", r.json); // 0xBB = 187
}

#[test]
fn clones_typed_array() {
    let r = dispatch(
        m(r#"export function test() {
            const orig = new Uint8Array([1, 2, 3, 4]);
            const clone = structuredClone(orig);
            clone[0] = 99;
            return {
                isU8: clone instanceof Uint8Array,
                origFirst: orig[0],
                cloneFirst: clone[0],
                len: clone.length,
            };
        }"#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(r.json.contains("\"isU8\":true"), "got: {}", r.json);
    assert!(r.json.contains("\"origFirst\":1"), "got: {}", r.json);
    assert!(r.json.contains("\"cloneFirst\":99"), "got: {}", r.json);
}

#[test]
fn throws_data_clone_error_on_function() {
    // Functions are not cloneable per spec; must throw DataCloneError.
    let r = dispatch(
        m(r#"export function test() {
            try {
                structuredClone(() => 1);
                return { threw: false };
            } catch (e) {
                return {
                    threw: true,
                    name: e.name,
                    isDom: e instanceof DOMException,
                    code: e.code,
                };
            }
        }"#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(r.json.contains("\"threw\":true"), "got: {}", r.json);
    assert!(r.json.contains("\"name\":\"DataCloneError\""), "got: {}", r.json);
    assert!(r.json.contains("\"isDom\":true"), "got: {}", r.json);
    assert!(r.json.contains("\"code\":25"), "got: {}", r.json);
}

#[test]
fn throws_data_clone_error_on_symbol() {
    let r = dispatch(
        m(r#"export function test() {
            try {
                structuredClone(Symbol("x"));
                return { threw: false };
            } catch (e) {
                return { threw: true, name: e.name };
            }
        }"#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(r.json.contains("\"threw\":true"), "got: {}", r.json);
    assert!(r.json.contains("\"name\":\"DataCloneError\""), "got: {}", r.json);
}

#[test]
fn handles_circular_reference() {
    // The polyfill threw "circular structure" because JSON.stringify
    // can't handle cycles. Native should preserve the cycle.
    let r = dispatch(
        m(r#"export function test() {
            const obj = { a: 1 };
            obj.self = obj;
            const clone = structuredClone(obj);
            return {
                a: clone.a,
                selfIsCycle: clone.self === clone,
                origUntouched: obj.self === obj,
            };
        }"#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(r.json.contains("\"a\":1"), "got: {}", r.json);
    assert!(r.json.contains("\"selfIsCycle\":true"), "got: {}", r.json);
    assert!(r.json.contains("\"origUntouched\":true"), "got: {}", r.json);
}

#[test]
fn preserves_undefined_and_null() {
    let r = dispatch(
        m(r#"export function test() {
            return {
                undef: structuredClone(undefined) === undefined,
                nul: structuredClone(null) === null,
                num: structuredClone(42),
                str: structuredClone("hi"),
                bool: structuredClone(true),
            };
        }"#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(r.json.contains("\"undef\":true"), "got: {}", r.json);
    assert!(r.json.contains("\"nul\":true"), "got: {}", r.json);
    assert!(r.json.contains("\"num\":42"), "got: {}", r.json);
    assert!(r.json.contains("\"str\":\"hi\""), "got: {}", r.json);
    assert!(r.json.contains("\"bool\":true"), "got: {}", r.json);
}

#[test]
fn deep_clones_nested_array() {
    let r = dispatch(
        m(r#"export function test() {
            const inner = [1, 2];
            const orig = [inner, [3, 4]];
            const clone = structuredClone(orig);
            clone[0][0] = 99;
            return {
                origUntouched: orig[0][0],
                cloneMutated: clone[0][0],
                noShare: orig[0] !== clone[0],
            };
        }"#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(r.json.contains("\"origUntouched\":1"), "got: {}", r.json);
    assert!(r.json.contains("\"cloneMutated\":99"), "got: {}", r.json);
    assert!(r.json.contains("\"noShare\":true"), "got: {}", r.json);
}

#[test]
fn preserves_shared_object_identity_in_clone() {
    // A → B and A → C, where B === C. After clone, B' and C' should
    // also be ===, not two separate clones.
    let r = dispatch(
        m(r#"export function test() {
            const shared = { v: 1 };
            const orig = { a: shared, b: shared };
            const clone = structuredClone(orig);
            clone.a.v = 99;
            return {
                identitySharedInClone: clone.a === clone.b,
                bSawMutation: clone.b.v,
            };
        }"#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(r.json.contains("\"identitySharedInClone\":true"), "got: {}", r.json);
    assert!(r.json.contains("\"bSawMutation\":99"), "got: {}", r.json);
}
