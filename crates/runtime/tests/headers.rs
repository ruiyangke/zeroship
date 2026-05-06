//! Hand-written tests for the native `Headers` class and its iterator.
//!
//! These tests drive the JS surface through a real V8 isolate. WPT
//! conformance lives in `wpt_headers.rs`; this file covers the
//! highest-risk algorithm-shape regressions from the JS polyfill
//! (case-preserve, normalize-then-validate, Set-Cookie special cases).
//!
//! Pattern matches `wpt_text_encoding.rs` / `v8_class_smoke.rs`: we use
//! a shared `run_in_v8` harness, install `globalThis.Headers`, and
//! evaluate JS that asserts back via `assert_eq!` on a stringified
//! result.
#![allow(unsafe_code)]

use zeroship_runtime::headers;
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
    headers::install_global(scope, global);

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
fn empty_headers_get_returns_null() {
    let s = run_in_v8(
        r#"
        const h = new Headers();
        JSON.stringify({ a: h.get("x"), b: h.has("x") });
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, r#"{"a":null,"b":false}"#);
}

#[test]
fn construct_from_sequence() {
    let s = run_in_v8(
        r#"
        const h = new Headers([["a", "1"], ["b", "2"]]);
        JSON.stringify({ a: h.get("a"), b: h.get("b") });
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, r#"{"a":"1","b":"2"}"#);
}

#[test]
fn construct_from_record() {
    let s = run_in_v8(
        r#"
        const h = new Headers({a: "1", b: "2"});
        JSON.stringify({ a: h.get("a"), b: h.get("b") });
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, r#"{"a":"1","b":"2"}"#);
}

#[test]
fn construct_from_other_headers_via_iterable() {
    // MISSING-4: new Headers(otherHeaders) works via the iterable branch.
    let s = run_in_v8(
        r#"
        const a = new Headers([["x-foo", "bar"]]);
        const b = new Headers(a);
        JSON.stringify({ x: b.get("x-foo") });
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, r#"{"x":"bar"}"#);
}

#[test]
fn construct_with_null_throws() {
    let s = run_in_v8(
        r#"
        let kind;
        try { new Headers(null); }
        catch (e) { kind = e.constructor.name; }
        kind || "no-throw";
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, "TypeError");
}

#[test]
fn construct_with_number_throws() {
    let s = run_in_v8(
        r#"
        let kind;
        try { new Headers(42); }
        catch (e) { kind = e.constructor.name; }
        kind || "no-throw";
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, "TypeError");
}

#[test]
fn construct_with_non_pair_sequence_throws() {
    let s = run_in_v8(
        r#"
        let kind;
        try { new Headers([["a"]]); }
        catch (e) { kind = e.constructor.name; }
        kind || "no-throw";
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, "TypeError");
}

// ---------------------------------------------------------------------------
// append / set / get / has / delete
// ---------------------------------------------------------------------------

#[test]
fn append_combines_with_comma_space() {
    let s = run_in_v8(
        r#"
        const h = new Headers();
        h.append("a", "1");
        h.append("a", "2");
        h.get("a");
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, "1, 2");
}

#[test]
fn set_replaces_all_existing() {
    let s = run_in_v8(
        r#"
        const h = new Headers();
        h.append("a", "1");
        h.append("a", "2");
        h.set("a", "9");
        h.get("a");
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, "9");
}

#[test]
fn delete_removes_all() {
    let s = run_in_v8(
        r#"
        const h = new Headers();
        h.append("a", "1");
        h.append("a", "2");
        h.delete("a");
        JSON.stringify({ has: h.has("a"), get: h.get("a") });
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, r#"{"has":false,"get":null}"#);
}

// ---------------------------------------------------------------------------
// Validate
// ---------------------------------------------------------------------------

#[test]
fn append_invalid_name_throws_typeerror() {
    let s = run_in_v8(
        r#"
        let kind;
        try { new Headers().append("invalid name", "x"); }
        catch (e) { kind = e.constructor.name; }
        kind || "no-throw";
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, "TypeError");
}

#[test]
fn append_empty_name_throws_typeerror() {
    let s = run_in_v8(
        r#"
        let kind;
        try { new Headers().append("", "x"); }
        catch (e) { kind = e.constructor.name; }
        kind || "no-throw";
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, "TypeError");
}

#[test]
fn append_value_with_nul_throws() {
    let s = run_in_v8(
        r#"
        let kind;
        try { new Headers().append("x", "bad\u0000value"); }
        catch (e) { kind = e.constructor.name; }
        kind || "no-throw";
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, "TypeError");
}

#[test]
fn append_value_with_inner_crlf_throws() {
    // Embedded CRLF: stripping outer ws leaves the inner CRLF, which is
    // forbidden anywhere.
    let s = run_in_v8(
        r#"
        let kind;
        try { new Headers().append("x", "bad\r\nvalue"); }
        catch (e) { kind = e.constructor.name; }
        kind || "no-throw";
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, "TypeError");
}

// Validate header names on delete/has/get.

#[test]
fn delete_invalid_name_throws() {
    let s = run_in_v8(
        r#"
        let kind;
        try { new Headers().delete("invalid name"); }
        catch (e) { kind = e.constructor.name; }
        kind || "no-throw";
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, "TypeError");
}

#[test]
fn has_invalid_name_throws() {
    let s = run_in_v8(
        r#"
        let kind;
        try { new Headers().has("invalid name"); }
        catch (e) { kind = e.constructor.name; }
        kind || "no-throw";
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, "TypeError");
}

#[test]
fn get_invalid_name_throws() {
    let s = run_in_v8(
        r#"
        let kind;
        try { new Headers().get("invalid name"); }
        catch (e) { kind = e.constructor.name; }
        kind || "no-throw";
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, "TypeError");
}

// ByteString boundary — code units > 0xFF throw TypeError.

#[test]
fn bytestring_high_codeunit_throws_at_construct() {
    let s = run_in_v8(
        r#"
        let kind;
        try { new Headers({"x-foo": "\u0100"}); }
        catch (e) { kind = e.constructor.name; }
        kind || "no-throw";
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, "TypeError");
}

#[test]
fn bytestring_high_codeunit_throws_at_append_value() {
    let s = run_in_v8(
        r#"
        let kind;
        try { new Headers().append("x-foo", "\u0100"); }
        catch (e) { kind = e.constructor.name; }
        kind || "no-throw";
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, "TypeError");
}

#[test]
fn bytestring_high_codeunit_throws_at_append_name() {
    let s = run_in_v8(
        r#"
        let kind;
        try { new Headers().append("\u0100", "x"); }
        catch (e) { kind = e.constructor.name; }
        kind || "no-throw";
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, "TypeError");
}

// ---------------------------------------------------------------------------
// Normalize-then-validate
// ---------------------------------------------------------------------------

#[test]
fn append_strips_outer_whitespace() {
    let s = run_in_v8(
        r#"
        const h = new Headers();
        h.append("x", "  hi  ");
        h.get("x");
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, "hi");
}

#[test]
fn append_strips_crlf_at_ends_only() {
    // After normalize, "  hello\r\n" becomes "hello"; validation passes.
    let s = run_in_v8(
        r#"
        const h = new Headers();
        h.append("x", "  hello\r\n");
        h.get("x");
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, "hello");
}

// ---------------------------------------------------------------------------
// Casing
// ---------------------------------------------------------------------------

#[test]
fn case_preserve_first_in_list_match() {
    // Per Fetch §2.2.1: when appending, if list contains a byte-case-
    // insensitive match, reuse that match's name casing.
    // Iteration order: lowercase per sort-and-combine.
    let s = run_in_v8(
        r#"
        const h = new Headers();
        h.append("X-Foo", "1");
        h.append("x-foo", "2");
        // get always returns the joined value (case-insensitive lookup).
        // Iteration always emits lowercase per sort-and-combine.
        const k = [];
        for (const [n, v] of h) k.push(n + "=" + v);
        JSON.stringify({ joined: h.get("x-foo"), iter: k });
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, r#"{"joined":"1, 2","iter":["x-foo=1, 2"]}"#);
}

#[test]
fn case_reset_after_delete_then_append() {
    // Casing tracks first currently-in-list match, NOT historical:
    // append("X-Foo") then delete then append("y-foo") should store
    // "y-foo" — there's nothing in the list anymore matching "X-Foo".
    let s = run_in_v8(
        r#"
        const h = new Headers();
        h.append("X-Foo", "1");
        h.delete("X-Foo");
        h.append("y-foo", "3");
        h.append("Y-Foo", "4");
        const k = [];
        for (const [n, v] of h) k.push(n + "=" + v);
        JSON.stringify({ iter: k });
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, r#"{"iter":["y-foo=3, 4"]}"#);
}

// ---------------------------------------------------------------------------
// Set-Cookie (five places of divergence — items 1-3 in v1)
// ---------------------------------------------------------------------------

#[test]
fn get_set_cookie_returns_unjoined_array() {
    let s = run_in_v8(
        r#"
        const h = new Headers();
        h.append("set-cookie", "a=1");
        h.append("set-cookie", "b=2");
        const arr = h.getSetCookie();
        JSON.stringify(arr);
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, r#"["a=1","b=2"]"#);
}

#[test]
fn get_of_set_cookie_still_joins() {
    // Counter-intuitive but spec-mandated: Headers.prototype.get
    // joins set-cookie values with ", " just like every other header.
    // header-setcookie.any.js verifies this.
    let s = run_in_v8(
        r#"
        const h = new Headers();
        h.append("set-cookie", "a=1");
        h.append("set-cookie", "b=2");
        h.get("set-cookie");
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, "a=1, b=2");
}

#[test]
fn iteration_emits_each_set_cookie_separately() {
    let s = run_in_v8(
        r#"
        const h = new Headers([
            ["accept", "*/*"],
            ["set-cookie", "a=1"],
            ["set-cookie", "b=2"],
            ["x-foo", "bar"],
        ]);
        const out = [];
        for (const [n, v] of h) out.push(n + "=" + v);
        JSON.stringify(out);
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(
        s,
        r#"["accept=*/*","set-cookie=a=1","set-cookie=b=2","x-foo=bar"]"#
    );
}

// ---------------------------------------------------------------------------
// Iteration: keys / values / entries / forEach + LIVE iteration
// ---------------------------------------------------------------------------

#[test]
fn iteration_emits_lowercase_sorted() {
    let s = run_in_v8(
        r#"
        const h = new Headers([
            ["X-Foo", "1"],
            ["A-Bar", "2"],
        ]);
        const k = [];
        for (const x of h.keys()) k.push(x);
        JSON.stringify(k);
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, r#"["a-bar","x-foo"]"#);
}

#[test]
fn for_each_visits_each_pair() {
    let s = run_in_v8(
        r#"
        const h = new Headers([
            ["X-Foo", "1"],
            ["A-Bar", "2"],
        ]);
        const out = [];
        h.forEach((v, k, ref) => {
            out.push(k + "=" + v);
        });
        JSON.stringify(out);
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, r#"["a-bar=2","x-foo=1"]"#);
}

#[test]
fn iterator_to_string_tag_is_headers_iterator() {
    // MISSING-2: @@toStringTag = "Headers Iterator".
    let s = run_in_v8(
        r#"
        Object.prototype.toString.call(new Headers().entries());
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, "[object Headers Iterator]");
}

#[test]
fn iterator_proto_chain_to_iterator_prototype() {
    // MISSING-3: [[Prototype]] of iterator's prototype is %Iterator.prototype%.
    let s = run_in_v8(
        r#"
        const it = new Headers().entries();
        const ourProto = Object.getPrototypeOf(it);
        const next = Object.getPrototypeOf(ourProto);
        const iterProto = Object.getPrototypeOf(Object.getPrototypeOf([][Symbol.iterator]()));
        next === iterProto ? "yes" : "no";
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, "yes");
}

// Live iteration. The two header-setcookie.any.js cases
// cited verbatim in the design doc.

#[test]
fn live_iteration_after_append_set_cookie() {
    // From `header-setcookie.any.js`, paraphrased for this harness.
    // Initial: [["fizz","buzz"], ["X-Header","test"]] -> sorted lowercase:
    //   "fizz"=buzz, "x-header"=test
    // it.next() -> ["fizz", "buzz"]
    // append("Set-Cookie","a=b") -> sort positions: "fizz", "set-cookie", "x-header"
    // it.next() -> at index 1 of new sort: "set-cookie"=a=b
    let s = run_in_v8(
        r#"
        const h = new Headers([["fizz","buzz"], ["X-Header","test"]]);
        const it = h[Symbol.iterator]();
        const e1 = it.next().value;
        h.append("Set-Cookie","a=b");
        const e2 = it.next().value;
        h.append("Accept","text/html");
        const e3 = it.next().value;
        JSON.stringify({ e1, e2, e3 });
        "#,
        |val, scope| js_string(val, scope),
    );
    // Live: index advances past 1, then 2; mutation between calls
    // changes what each next() emits.
    // After e1: index=1; list lowered = [fizz, x-header].
    // append set-cookie. Sort: [fizz, set-cookie, x-header]. e2 = pos 1 = set-cookie/a=b.
    // append accept. Sort: [accept, fizz, set-cookie, x-header]. index now 2 → set-cookie/a=b.
    // No wait — the actual WPT behavior verifies that index counts up; e3 = pos 2 of new
    // sort = "set-cookie"=a=b. But the design comment notes this.
    assert_eq!(
        s,
        r#"{"e1":["fizz","buzz"],"e2":["set-cookie","a=b"],"e3":["set-cookie","a=b"]}"#
    );
}

#[test]
fn live_iteration_with_set_cookie_replacements() {
    // From header-setcookie.any.js paraphrase:
    // h = [["set-cookie","a"],["set-cookie","b"],["set-cookie","c"]]
    // it.next() -> ["set-cookie","a"]   (index 0 → 1)
    // delete("set-cookie") + append d, e, f
    // After mutation list = [d, e, f]; sort emits them in insertion order
    //   inside the set-cookie cluster.
    // it.next() -> at index 1 of new list = "set-cookie"="e"
    let s = run_in_v8(
        r#"
        const h = new Headers([
            ["set-cookie","a"],
            ["set-cookie","b"],
            ["set-cookie","c"],
        ]);
        const it = h[Symbol.iterator]();
        const e1 = it.next().value;
        h.delete("set-cookie");
        h.append("set-cookie","d");
        h.append("set-cookie","e");
        h.append("set-cookie","f");
        const e2 = it.next().value;
        JSON.stringify({ e1, e2 });
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(
        s,
        r#"{"e1":["set-cookie","a"],"e2":["set-cookie","e"]}"#
    );
}

// ---------------------------------------------------------------------------
// Symbol-keyed record
// ---------------------------------------------------------------------------

#[test]
fn symbol_key_record_throws_typeerror() {
    // Record path uses [[OwnPropertyKeys]] which preserves Symbol keys.
    // ByteString conversion of a Symbol throws TypeError per ECMA-262.
    let s = run_in_v8(
        r#"
        let kind;
        try {
            const obj = {};
            obj[Symbol("x")] = "v";
            new Headers(obj);
        } catch (e) { kind = e.constructor.name; }
        kind || "no-throw";
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, "TypeError");
}

// GetMethod semantics — non-callable @@iterator throws TypeError.

#[test]
fn non_callable_iterator_throws_typeerror() {
    let s = run_in_v8(
        r#"
        let kind;
        try {
            new Headers({[Symbol.iterator]: 5});
        } catch (e) { kind = e.constructor.name; }
        kind || "no-throw";
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, "TypeError");
}

// ---------------------------------------------------------------------------
// Misc
// ---------------------------------------------------------------------------

#[test]
fn headers_to_string_tag() {
    let s = run_in_v8(
        r#"
        Object.prototype.toString.call(new Headers());
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, "[object Headers]");
}

#[test]
fn iterator_symbol_iterator_returns_self() {
    // Per WebIDL §3.7.10, the default iterator object's
    // %IteratorPrototype%[Symbol.iterator]() returns `this`.
    let s = run_in_v8(
        r#"
        const it = new Headers().entries();
        it[Symbol.iterator]() === it ? "self" : "other";
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, "self");
}
