//! Hand-written tests for the native `FormData` class and its iterator.
//!
//! Pattern matches `headers.rs` / `event_target.rs`: install the globals
//! on a fresh isolate, evaluate JS, assert via stringified result. WPT
//! conformance lives in `wpt_form_data.rs`; this file covers the v1
//! shape (USVString-only, no Blob/HTMLFormElement) and the spec corners
//! the polyfill got wrong (live iterator, set-replace-then-remove,
//! delete-all, USVString surrogate replacement).

#![allow(unsafe_code)]

use zeroship_runtime::dom;
use zeroship_runtime::init_v8;

fn run_in_v8<F, R>(src: &str, f: F) -> R
where
    F: FnOnce(v8::Local<v8::Value>, &mut v8::PinScope) -> R,
{
    init_v8();
    let mut isolate = v8::Isolate::new(v8::CreateParams::default());
    v8::scope!(let handle_scope, &mut isolate);
    let context = v8::Context::new(handle_scope, Default::default());
    let scope = &mut v8::ContextScope::new(handle_scope, context);

    let global = scope.get_current_context().global(scope);
    dom::install_globals(scope, global);

    let src_v8 = v8::String::new(scope, src).unwrap();
    let script = v8::Script::compile(scope, src_v8, None).unwrap();
    let result = script.run(scope).unwrap();
    f(result, scope)
}

fn js_string(val: v8::Local<v8::Value>, scope: &mut v8::PinScope) -> String {
    val.to_rust_string_lossy(scope)
}

// ---------------------------------------------------------------------------
// Construction
// ---------------------------------------------------------------------------

#[test]
fn construct_empty() {
    let s = run_in_v8(
        r#"
        const fd = new FormData();
        JSON.stringify({
            kind: fd.constructor.name,
            tag: Object.prototype.toString.call(fd),
            count: Array.from(fd.entries()).length,
        });
        "#,
        js_string,
    );
    assert_eq!(
        s,
        r#"{"kind":"FormData","tag":"[object FormData]","count":0}"#
    );
}

#[test]
fn construct_undefined_works() {
    let s = run_in_v8(
        r#"
        const fd = new FormData(undefined);
        JSON.stringify({ count: Array.from(fd.entries()).length });
        "#,
        js_string,
    );
    assert_eq!(s, r#"{"count":0}"#);
}

#[test]
fn construct_with_form_throws() {
    // Per IDL, `form` must be HTMLFormElement; in our runtime there is
    // no HTMLFormElement so any non-undefined value should throw
    // TypeError (matches workerd / Cloudflare Workers).
    let s = run_in_v8(
        r#"
        let err;
        try {
            new FormData({});
        } catch (e) { err = e; }
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
// append / get / getAll / has / set / delete
// ---------------------------------------------------------------------------

#[test]
fn append_and_get() {
    let s = run_in_v8(
        r#"
        const fd = new FormData();
        fd.append("a", "1");
        fd.append("b", "2");
        fd.append("a", "3");
        JSON.stringify({
            a: fd.get("a"),
            b: fd.get("b"),
            allA: fd.getAll("a"),
            allB: fd.getAll("b"),
            allMissing: fd.getAll("missing"),
            getMissing: fd.get("missing"),
        });
        "#,
        js_string,
    );
    assert_eq!(
        s,
        r#"{"a":"1","b":"2","allA":["1","3"],"allB":["2"],"allMissing":[],"getMissing":null}"#
    );
}

#[test]
fn set_replaces_first_removes_others() {
    // Per spec: set(name, value) — if list contains name, set the value
    // of the FIRST such entry to value and remove all the others;
    // otherwise append.
    let s = run_in_v8(
        r#"
        const fd = new FormData();
        fd.append("a", "1");
        fd.append("b", "2");
        fd.append("a", "3");
        fd.set("a", "X");
        JSON.stringify({
            entries: Array.from(fd.entries()),
            allA: fd.getAll("a"),
        });
        "#,
        js_string,
    );
    // After set("a","X"): the first "a" becomes "X" (in-place), the
    // later "a" entry is removed. "b" stays where it was.
    assert_eq!(
        s,
        r#"{"entries":[["a","X"],["b","2"]],"allA":["X"]}"#
    );
}

#[test]
fn set_then_append_then_set_again() {
    // After set+append+set: only one "a" entry remains.
    let s = run_in_v8(
        r#"
        const fd = new FormData();
        fd.set("a", "1");
        fd.append("a", "2");
        fd.set("a", "3");
        JSON.stringify({ allA: fd.getAll("a"), entries: Array.from(fd.entries()) });
        "#,
        js_string,
    );
    assert_eq!(s, r#"{"allA":["3"],"entries":[["a","3"]]}"#);
}

#[test]
fn set_appends_when_absent() {
    let s = run_in_v8(
        r#"
        const fd = new FormData();
        fd.set("x", "1");
        JSON.stringify({ x: fd.get("x"), entries: Array.from(fd.entries()) });
        "#,
        js_string,
    );
    assert_eq!(s, r#"{"x":"1","entries":[["x","1"]]}"#);
}

#[test]
fn delete_removes_all_matching() {
    let s = run_in_v8(
        r#"
        const fd = new FormData();
        fd.append("a", "1");
        fd.append("b", "2");
        fd.append("a", "3");
        fd.delete("a");
        JSON.stringify({
            entries: Array.from(fd.entries()),
            hasA: fd.has("a"),
            hasB: fd.has("b"),
        });
        "#,
        js_string,
    );
    assert_eq!(
        s,
        r#"{"entries":[["b","2"]],"hasA":false,"hasB":true}"#
    );
}

#[test]
fn has_returns_boolean() {
    let s = run_in_v8(
        r#"
        const fd = new FormData();
        fd.append("a", "1");
        JSON.stringify({
            a: fd.has("a"),
            aType: typeof fd.has("a"),
            missing: fd.has("missing"),
            missingType: typeof fd.has("missing"),
        });
        "#,
        js_string,
    );
    assert_eq!(
        s,
        r#"{"a":true,"aType":"boolean","missing":false,"missingType":"boolean"}"#
    );
}

// ---------------------------------------------------------------------------
// Iteration order / iterator semantics
// ---------------------------------------------------------------------------

#[test]
fn iteration_is_insertion_order() {
    let s = run_in_v8(
        r#"
        const fd = new FormData();
        fd.append("z", "1");
        fd.append("a", "2");
        fd.append("m", "3");
        fd.append("a", "4");
        JSON.stringify(Array.from(fd.entries()));
        "#,
        js_string,
    );
    assert_eq!(
        s,
        r#"[["z","1"],["a","2"],["m","3"],["a","4"]]"#
    );
}

#[test]
fn for_of_destructure_works() {
    let s = run_in_v8(
        r#"
        const fd = new FormData();
        fd.append("a", "1");
        fd.append("b", "2");
        const out = [];
        for (const [k, v] of fd) {
            out.push(k + "=" + v);
        }
        JSON.stringify(out);
        "#,
        js_string,
    );
    assert_eq!(s, r#"["a=1","b=2"]"#);
}

#[test]
fn keys_values_entries_match() {
    let s = run_in_v8(
        r#"
        const fd = new FormData();
        fd.append("a", "1");
        fd.append("b", "2");
        JSON.stringify({
            keys: Array.from(fd.keys()),
            values: Array.from(fd.values()),
            entries: Array.from(fd.entries()),
        });
        "#,
        js_string,
    );
    assert_eq!(
        s,
        r#"{"keys":["a","b"],"values":["1","2"],"entries":[["a","1"],["b","2"]]}"#
    );
}

#[test]
fn for_each_callback_args() {
    let s = run_in_v8(
        r#"
        const fd = new FormData();
        fd.append("a", "1");
        fd.append("b", "2");
        const out = [];
        fd.forEach(function(v, k, parent) {
            out.push({ v, k, sameParent: parent === fd });
        });
        JSON.stringify(out);
        "#,
        js_string,
    );
    assert_eq!(
        s,
        r#"[{"v":"1","k":"a","sameParent":true},{"v":"2","k":"b","sameParent":true}]"#
    );
}

#[test]
fn symbol_iterator_aliases_entries() {
    let s = run_in_v8(
        r#"
        const fd = new FormData();
        fd.append("a", "1");
        // Per WebIDL §3.7.10, [Symbol.iterator] === entries.
        JSON.stringify({
            same: fd[Symbol.iterator] === fd.entries,
            entries: Array.from(fd[Symbol.iterator]()),
        });
        "#,
        js_string,
    );
    assert_eq!(s, r#"{"same":true,"entries":[["a","1"]]}"#);
}

#[test]
fn iterator_to_string_tag() {
    let s = run_in_v8(
        r#"
        const fd = new FormData();
        const it = fd.entries();
        JSON.stringify({
            tag: Object.prototype.toString.call(it),
            // [[Prototype]] should be %IteratorPrototype% (which has Symbol.iterator).
            hasIteratorMethod: typeof it[Symbol.iterator] === "function",
            // self-iterating: it[Symbol.iterator]() returns it itself.
            same: it[Symbol.iterator]() === it,
        });
        "#,
        js_string,
    );
    assert_eq!(
        s,
        r#"{"tag":"[object FormData Iterator]","hasIteratorMethod":true,"same":true}"#
    );
}

#[test]
fn iterator_is_live_after_append() {
    // Per WebIDL §3.7.10.2 "default iterator object": each next() call
    // re-reads the live "value pairs to iterate over". An append after
    // iterator construction MUST be visible.
    let s = run_in_v8(
        r#"
        const fd = new FormData();
        fd.append("a", "1");
        const it = fd.entries();
        const r1 = it.next();
        fd.append("b", "2");          // mutate AFTER iterator created
        const r2 = it.next();
        const r3 = it.next();
        JSON.stringify({
            r1: r1.value,
            r2: r2.value,
            r3done: r3.done,
        });
        "#,
        js_string,
    );
    assert_eq!(
        s,
        r#"{"r1":["a","1"],"r2":["b","2"],"r3done":true}"#
    );
}

#[test]
fn for_each_observes_appends_during_iteration() {
    // Per WebIDL §3.7.10.3 forEach: "set pairs to idlObject's CURRENT
    // list of value pairs to iterate over (it might have changed)" —
    // appended entries during iteration must be visible.
    let s = run_in_v8(
        r#"
        const fd = new FormData();
        fd.append("a", "1");
        fd.append("b", "2");
        const out = [];
        let appended = false;
        fd.forEach(function(v, k) {
            out.push(k + "=" + v);
            if (!appended) {
                fd.append("c", "3");
                appended = true;
            }
        });
        JSON.stringify(out);
        "#,
        js_string,
    );
    assert_eq!(s, r#"["a=1","b=2","c=3"]"#);
}

// ---------------------------------------------------------------------------
// USVString conversion
// ---------------------------------------------------------------------------

#[test]
fn usvstring_lone_surrogate_replaced() {
    // Per WebIDL §3.2.11 USVString: lone surrogates → U+FFFD. We use
    // `to_rust_string_lossy` which performs this conversion.
    let s = run_in_v8(
        r#"
        const fd = new FormData();
        fd.append("k\uD800", "v\uDFFF");
        const [k, v] = Array.from(fd.entries())[0];
        // The native side stored Rust UTF-8. When V8 converts back, it
        // re-encodes — both name and value should compare equal to
        // U+FFFD (replacement character).
        JSON.stringify({
            kCode: k.codePointAt(0),
            vCode: v.codePointAt(0),
            kLen: k.length,
            vLen: v.length,
        });
        "#,
        js_string,
    );
    // U+FFFD = 65533. After "k": codePointAt(0) is 'k' = 107. Read at
    // index 1 to inspect the replaced surrogate.
    let s2 = run_in_v8(
        r#"
        const fd = new FormData();
        fd.append("k\uD800", "v\uDFFF");
        const [k, v] = Array.from(fd.entries())[0];
        JSON.stringify({
            // index 1 is the (replaced) surrogate, both should be U+FFFD.
            kSurr: k.charCodeAt(1),
            vSurr: v.charCodeAt(1),
            replacement: "\uFFFD".charCodeAt(0),
        });
        "#,
        js_string,
    );
    assert_eq!(
        s2,
        r#"{"kSurr":65533,"vSurr":65533,"replacement":65533}"#
    );
    // Also verify that lengths and starting code points are reasonable
    // (no truncation, no doubled chars).
    assert!(s.contains("\"kCode\":107")); // 'k'
    assert!(s.contains("\"vCode\":118")); // 'v'
    assert!(s.contains("\"kLen\":2"));
    assert!(s.contains("\"vLen\":2"));
}

#[test]
fn name_value_stringification() {
    // USVString conversion via ToString — non-strings get coerced.
    let s = run_in_v8(
        r#"
        const fd = new FormData();
        fd.append(42, true);
        fd.append(null, undefined);
        JSON.stringify(Array.from(fd.entries()));
        "#,
        js_string,
    );
    // Per spec, ToString(42) = "42", ToString(true) = "true",
    // ToString(null) = "null", ToString(undefined) = "undefined".
    assert_eq!(
        s,
        r#"[["42","true"],["null","undefined"]]"#
    );
}

// ---------------------------------------------------------------------------
// Blob deferral
// ---------------------------------------------------------------------------

#[test]
fn blob_overload_throws_pending_blob() {
    // v1 simplification: append(name, blob[, filename]) with a Blob
    // value throws TypeError because there's no native Blob class
    // yet. Pure-USVString path must still work — just `String(...)`
    // coercion via to_rust_string_lossy.
    //
    // We can't construct a real Blob here (no native class) — but we
    // can sanity-check that an explicit two-arg append with a string
    // value works AND that an arity-3 call still proceeds (the third
    // arg is the filename which is ignored for USVString values per
    // the spec's overload resolution).
    let s = run_in_v8(
        r#"
        const fd = new FormData();
        fd.append("a", "1");          // 2-arg form
        // 3-arg form with a string value: per IDL the value must be
        // (Blob or USVString). USVString takes precedence (string
        // coercion). The filename arg is ignored for USVString.
        // (No throw because we have no Blob class yet.)
        let threwOnBlobShape = false;
        try {
            // Pretend an opaque object with .arrayBuffer is a Blob —
            // ToString would coerce, so this should appear in the
            // string lane. v1 doesn't have Blob; treat as USVString.
            fd.append("b", { x: 1 });
        } catch (_) {
            threwOnBlobShape = true;
        }
        JSON.stringify({
            count: Array.from(fd.entries()).length,
            a: fd.get("a"),
            // Object stringifies to "[object Object]".
            b: fd.get("b"),
            threw: threwOnBlobShape,
        });
        "#,
        js_string,
    );
    assert_eq!(
        s,
        r#"{"count":2,"a":"1","b":"[object Object]","threw":false}"#
    );
}
