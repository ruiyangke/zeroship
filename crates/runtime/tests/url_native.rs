//! Hand-written tests for the native `URL` and `URLSearchParams` classes.
//!
//! Drives the JS surface through a real V8 isolate. WPT conformance lives
//! in `wpt_url.rs`; this file focuses on the design's BLOCKER list:
//!
//! - Spec-correct setters (host, protocol, etc.) via ada-url's mutation
//!   API rather than the polyfill's naive string-splitting.
//! - Live two-way sync between `url.search` and `url.searchParams`.
//! - `URL.parse(input, base?)` static method (newer WHATWG spec; was
//!   missing from the polyfill).
//! - `URLSearchParams.delete(name, value?)` and `has(name, value?)` —
//!   2-arg forms (added to the spec in 2023; polyfill ignored value).
//! - `[SameObject]` invariant: `url.searchParams === url.searchParams`.
//! - USVString lone-surrogate replacement on construction.
//! - `Symbol.toStringTag` set to "URL" / "URLSearchParams".

#![allow(unsafe_code)]

use zeroship_runtime::init_v8;
use zeroship_runtime::url_native;

// ---------------------------------------------------------------------------
// Test harness
// ---------------------------------------------------------------------------

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
    url_native::install_globals(scope, global);

    let src_v8 = v8::String::new(scope, src).unwrap();
    let script = v8::Script::compile(scope, src_v8, None).unwrap();
    let result = script.run(scope).unwrap();
    f(result, scope)
}

fn js_string(val: v8::Local<v8::Value>, scope: &mut v8::PinScope) -> String {
    val.to_rust_string_lossy(scope)
}

// ===========================================================================
// URL — construction, getters
// ===========================================================================

#[test]
fn url_basic_construction() {
    let s = run_in_v8(
        r#"
        const u = new URL("https://user:pass@example.com:8080/path?q=1#frag");
        JSON.stringify({
            href: u.href,
            protocol: u.protocol,
            username: u.username,
            password: u.password,
            host: u.host,
            hostname: u.hostname,
            port: u.port,
            pathname: u.pathname,
            search: u.search,
            hash: u.hash,
            origin: u.origin,
        });
        "#,
        js_string,
    );
    let v: serde_json::Value = serde_json::from_str(&s).expect("json");
    assert_eq!(v["protocol"], "https:");
    assert_eq!(v["username"], "user");
    assert_eq!(v["password"], "pass");
    assert_eq!(v["host"], "example.com:8080");
    assert_eq!(v["hostname"], "example.com");
    assert_eq!(v["port"], "8080");
    assert_eq!(v["pathname"], "/path");
    assert_eq!(v["search"], "?q=1");
    assert_eq!(v["hash"], "#frag");
    assert_eq!(v["origin"], "https://example.com:8080");
}

#[test]
fn url_invalid_throws_typeerror() {
    let s = run_in_v8(
        r#"
        let kind, msg;
        try { new URL("not a url"); }
        catch (e) { kind = e.constructor.name; msg = e.message; }
        JSON.stringify({ kind, msg });
        "#,
        js_string,
    );
    assert!(s.contains(r#""kind":"TypeError""#), "got: {s}");
}

#[test]
fn url_with_base() {
    let s = run_in_v8(
        r#"
        const u = new URL("/api/v1/users", "https://example.com");
        u.href;
        "#,
        js_string,
    );
    assert_eq!(s, "https://example.com/api/v1/users");
}

#[test]
fn url_no_args_throws() {
    let s = run_in_v8(
        r#"
        let kind;
        try { new URL(); }
        catch (e) { kind = e.constructor.name; }
        kind || "no-throw";
        "#,
        js_string,
    );
    assert_eq!(s, "TypeError");
}

// ===========================================================================
// URL static methods — canParse, parse
// ===========================================================================

#[test]
fn url_can_parse_returns_boolean() {
    let s = run_in_v8(
        r#"
        JSON.stringify({
            valid: URL.canParse("https://example.com"),
            invalid: URL.canParse("not a url"),
            with_base: URL.canParse("/path", "https://example.com"),
            invalid_with_base: URL.canParse("not a url", "not a base"),
        });
        "#,
        js_string,
    );
    let v: serde_json::Value = serde_json::from_str(&s).expect("json");
    assert_eq!(v["valid"], true);
    assert_eq!(v["invalid"], false);
    assert_eq!(v["with_base"], true);
    assert_eq!(v["invalid_with_base"], false);
}

#[test]
fn url_parse_returns_url_or_null() {
    let s = run_in_v8(
        r#"
        const ok = URL.parse("https://example.com");
        const fail = URL.parse("not a url");
        JSON.stringify({
            ok_kind: ok && ok.constructor.name,
            ok_href: ok && ok.href,
            fail_is_null: fail === null,
        });
        "#,
        js_string,
    );
    let v: serde_json::Value = serde_json::from_str(&s).expect("json");
    assert_eq!(v["ok_kind"], "URL");
    assert_eq!(v["ok_href"], "https://example.com/");
    assert_eq!(v["fail_is_null"], true);
}

#[test]
fn url_parse_with_base() {
    let s = run_in_v8(
        r#"
        const u = URL.parse("/api", "https://example.com");
        u && u.href;
        "#,
        js_string,
    );
    assert_eq!(s, "https://example.com/api");
}

// ===========================================================================
// URL setters — spec-correct via ada-url
// ===========================================================================

#[test]
fn url_setter_protocol() {
    let s = run_in_v8(
        r#"
        const u = new URL("http://example.com");
        u.protocol = "https";
        u.href;
        "#,
        js_string,
    );
    assert_eq!(s, "https://example.com/");
}

#[test]
fn url_setter_host_with_port_spec_compliant() {
    // BLOCKER: polyfill split on ":" in `set host`, getting wrong
    // results for any host containing ":" mid-token. ada-url runs the
    // WHATWG host-parser state machine.
    let s = run_in_v8(
        r#"
        const u = new URL("http://example.com");
        u.host = "localhost:3000";
        JSON.stringify({ host: u.host, hostname: u.hostname, port: u.port });
        "#,
        js_string,
    );
    let v: serde_json::Value = serde_json::from_str(&s).expect("json");
    assert_eq!(v["host"], "localhost:3000");
    assert_eq!(v["hostname"], "localhost");
    assert_eq!(v["port"], "3000");
}

#[test]
fn url_setter_hostname() {
    let s = run_in_v8(
        r#"
        const u = new URL("http://example.com:8080");
        u.hostname = "other.test";
        u.host;  // Port preserved.
        "#,
        js_string,
    );
    assert_eq!(s, "other.test:8080");
}

#[test]
fn url_setter_port() {
    let s = run_in_v8(
        r#"
        const u = new URL("http://example.com");
        u.port = "9090";
        u.href;
        "#,
        js_string,
    );
    assert_eq!(s, "http://example.com:9090/");
}

#[test]
fn url_setter_pathname() {
    let s = run_in_v8(
        r#"
        const u = new URL("http://example.com");
        u.pathname = "/foo/bar";
        u.href;
        "#,
        js_string,
    );
    assert_eq!(s, "http://example.com/foo/bar");
}

#[test]
fn url_setter_search() {
    let s = run_in_v8(
        r#"
        const u = new URL("http://example.com");
        u.search = "?a=1&b=2";
        JSON.stringify({ search: u.search, href: u.href });
        "#,
        js_string,
    );
    let v: serde_json::Value = serde_json::from_str(&s).expect("json");
    assert_eq!(v["search"], "?a=1&b=2");
    assert_eq!(v["href"], "http://example.com/?a=1&b=2");
}

#[test]
fn url_setter_hash() {
    let s = run_in_v8(
        r#"
        const u = new URL("http://example.com");
        u.hash = "section-1";
        u.href;
        "#,
        js_string,
    );
    assert_eq!(s, "http://example.com/#section-1");
}

#[test]
fn url_setter_username_password() {
    let s = run_in_v8(
        r#"
        const u = new URL("http://example.com");
        u.username = "alice";
        u.password = "secret";
        u.href;
        "#,
        js_string,
    );
    assert_eq!(s, "http://alice:secret@example.com/");
}

#[test]
fn url_setter_href_full_reparse() {
    let s = run_in_v8(
        r#"
        const u = new URL("http://example.com/foo");
        u.href = "https://other.test/bar";
        JSON.stringify({
            host: u.host,
            pathname: u.pathname,
            protocol: u.protocol,
        });
        "#,
        js_string,
    );
    let v: serde_json::Value = serde_json::from_str(&s).expect("json");
    assert_eq!(v["host"], "other.test");
    assert_eq!(v["pathname"], "/bar");
    assert_eq!(v["protocol"], "https:");
}

// ===========================================================================
// searchParams — [SameObject] + live two-way sync
// ===========================================================================

#[test]
fn url_search_params_same_object() {
    // Per IDL [SameObject] — every read returns the same object.
    let s = run_in_v8(
        r#"
        const u = new URL("http://example.com/?a=1");
        const p1 = u.searchParams;
        const p2 = u.searchParams;
        p1 === p2;
        "#,
        |val, scope| val.boolean_value(scope),
    );
    assert!(s, "url.searchParams must be [SameObject]");
}

#[test]
fn url_search_params_initial_entries() {
    let s = run_in_v8(
        r#"
        const u = new URL("http://example.com/?a=1&b=2");
        JSON.stringify({
            a: u.searchParams.get("a"),
            b: u.searchParams.get("b"),
            missing: u.searchParams.get("x"),
        });
        "#,
        js_string,
    );
    let v: serde_json::Value = serde_json::from_str(&s).expect("json");
    assert_eq!(v["a"], "1");
    assert_eq!(v["b"], "2");
    assert_eq!(v["missing"], serde_json::Value::Null);
}

#[test]
fn url_search_params_mutation_reflects_in_url_search() {
    // The killer test: appending to searchParams must update url.search.
    let s = run_in_v8(
        r#"
        const u = new URL("http://example.com");
        u.searchParams.append("a", "1");
        u.searchParams.append("b", "2");
        JSON.stringify({ search: u.search, href: u.href });
        "#,
        js_string,
    );
    let v: serde_json::Value = serde_json::from_str(&s).expect("json");
    assert_eq!(v["search"], "?a=1&b=2");
    assert_eq!(v["href"], "http://example.com/?a=1&b=2");
}

#[test]
fn url_search_setter_reflects_in_search_params() {
    // Inverse direction: setting url.search must update searchParams.
    let s = run_in_v8(
        r#"
        const u = new URL("http://example.com");
        const p = u.searchParams;
        u.search = "?x=10&y=20";
        JSON.stringify({ x: p.get("x"), y: p.get("y") });
        "#,
        js_string,
    );
    let v: serde_json::Value = serde_json::from_str(&s).expect("json");
    assert_eq!(v["x"], "10");
    assert_eq!(v["y"], "20");
}

// ===========================================================================
// URLSearchParams construction
// ===========================================================================

#[test]
fn search_params_from_string() {
    let s = run_in_v8(
        r#"
        const p = new URLSearchParams("a=1&b=2");
        JSON.stringify({ a: p.get("a"), b: p.get("b"), size: p.size });
        "#,
        js_string,
    );
    let v: serde_json::Value = serde_json::from_str(&s).expect("json");
    assert_eq!(v["a"], "1");
    assert_eq!(v["b"], "2");
    assert_eq!(v["size"], 2);
}

#[test]
fn search_params_strips_leading_question() {
    let s = run_in_v8(
        r#"
        const p = new URLSearchParams("?a=1");
        p.toString();
        "#,
        js_string,
    );
    assert_eq!(s, "a=1");
}

#[test]
fn search_params_from_array() {
    let s = run_in_v8(
        r#"
        const p = new URLSearchParams([["a", "1"], ["b", "2"]]);
        p.toString();
        "#,
        js_string,
    );
    assert_eq!(s, "a=1&b=2");
}

#[test]
fn search_params_from_record() {
    let s = run_in_v8(
        r#"
        const p = new URLSearchParams({ a: "1", b: "2" });
        // Record iteration order is insertion order in modern JS.
        p.toString();
        "#,
        js_string,
    );
    assert_eq!(s, "a=1&b=2");
}

#[test]
fn search_params_empty_construction() {
    let s = run_in_v8(
        r#"
        JSON.stringify({
            no_args: new URLSearchParams().toString(),
            empty_str: new URLSearchParams("").toString(),
            null: new URLSearchParams(null).toString(),
            undef: new URLSearchParams(undefined).toString(),
        });
        "#,
        js_string,
    );
    let v: serde_json::Value = serde_json::from_str(&s).expect("json");
    assert_eq!(v["no_args"], "");
    assert_eq!(v["empty_str"], "");
    assert_eq!(v["null"], "");
    assert_eq!(v["undef"], "");
}

// ===========================================================================
// URLSearchParams 2-arg delete / has (newer spec)
// ===========================================================================

#[test]
fn search_params_delete_2arg() {
    let s = run_in_v8(
        r#"
        const p = new URLSearchParams("a=1&a=2&b=3");
        p.delete("a", "1");
        p.toString();
        "#,
        js_string,
    );
    assert_eq!(s, "a=2&b=3");
}

#[test]
fn search_params_has_2arg() {
    let s = run_in_v8(
        r#"
        const p = new URLSearchParams("a=1&a=2");
        JSON.stringify({
            has_a_1: p.has("a", "1"),
            has_a_2: p.has("a", "2"),
            has_a_3: p.has("a", "3"),
        });
        "#,
        js_string,
    );
    let v: serde_json::Value = serde_json::from_str(&s).expect("json");
    assert_eq!(v["has_a_1"], true);
    assert_eq!(v["has_a_2"], true);
    assert_eq!(v["has_a_3"], false);
}

// ===========================================================================
// USVString conversion — lone surrogate → U+FFFD
// ===========================================================================

#[test]
fn search_params_usv_string_lone_surrogate() {
    let s = run_in_v8(
        r#"
        // \uD83D is a lone high surrogate. Per WebIDL USVString, it
        // gets replaced with U+FFFD (0xFFFD).
        const p = new URLSearchParams();
        p.append("\uD83D", "x");
        // U+FFFD as UTF-8 = EF BF BD; percent-encoded form is %EF%BF%BD.
        p.toString();
        "#,
        js_string,
    );
    assert_eq!(s, "%EF%BF%BD=x");
}

// ===========================================================================
// toString / toJSON / Symbol.toStringTag
// ===========================================================================

#[test]
fn url_to_string_returns_href() {
    let s = run_in_v8(
        r#"
        const u = new URL("https://example.com/foo?bar=baz");
        u.toString() === u.href;
        "#,
        |val, scope| val.boolean_value(scope),
    );
    assert!(s);
}

#[test]
fn url_to_json_returns_href() {
    let s = run_in_v8(
        r#"
        const u = new URL("https://example.com/foo");
        JSON.stringify(u);
        "#,
        js_string,
    );
    // JSON.stringify wraps in quotes — but it also calls toJSON.
    assert_eq!(s, r#""https://example.com/foo""#);
}

#[test]
fn url_to_string_tag() {
    let s = run_in_v8(
        r#"
        Object.prototype.toString.call(new URL("http://example.com"));
        "#,
        js_string,
    );
    assert_eq!(s, "[object URL]");
}

#[test]
fn search_params_to_string_tag() {
    let s = run_in_v8(
        r#"
        Object.prototype.toString.call(new URLSearchParams());
        "#,
        js_string,
    );
    assert_eq!(s, "[object URLSearchParams]");
}

// ===========================================================================
// URLSearchParams iteration
// ===========================================================================

#[test]
fn search_params_iteration_order() {
    let s = run_in_v8(
        r#"
        const p = new URLSearchParams("a=1&b=2&a=3");
        const out = [];
        for (const [k, v] of p) out.push(`${k}=${v}`);
        out.join(",");
        "#,
        js_string,
    );
    assert_eq!(s, "a=1,b=2,a=3");
}

#[test]
fn search_params_keys_values_entries() {
    let s = run_in_v8(
        r#"
        const p = new URLSearchParams("a=1&b=2");
        JSON.stringify({
            keys: [...p.keys()],
            values: [...p.values()],
            entries: [...p.entries()],
        });
        "#,
        js_string,
    );
    let v: serde_json::Value = serde_json::from_str(&s).expect("json");
    assert_eq!(v["keys"], serde_json::json!(["a", "b"]));
    assert_eq!(v["values"], serde_json::json!(["1", "2"]));
    assert_eq!(
        v["entries"],
        serde_json::json!([["a", "1"], ["b", "2"]])
    );
}

#[test]
fn search_params_for_each() {
    let s = run_in_v8(
        r#"
        const p = new URLSearchParams("a=1&b=2");
        const out = [];
        p.forEach((value, key, parent) => {
            out.push(`${key}=${value}:parent_is_self=${parent === p}`);
        });
        out.join(",");
        "#,
        js_string,
    );
    assert_eq!(s, "a=1:parent_is_self=true,b=2:parent_is_self=true");
}

#[test]
fn search_params_sort() {
    let s = run_in_v8(
        r#"
        const p = new URLSearchParams("c=3&a=1&b=2");
        p.sort();
        p.toString();
        "#,
        js_string,
    );
    assert_eq!(s, "a=1&b=2&c=3");
}

// ===========================================================================
// URLSearchParams set / append / get / getAll
// ===========================================================================

#[test]
fn search_params_set_replaces_or_appends() {
    let s = run_in_v8(
        r#"
        const p = new URLSearchParams("a=1&a=2&b=3");
        p.set("a", "X");  // Removes other "a"s, sets the first to X
        p.toString();
        "#,
        js_string,
    );
    assert_eq!(s, "a=X&b=3");
}

#[test]
fn search_params_get_all() {
    let s = run_in_v8(
        r#"
        const p = new URLSearchParams("a=1&a=2&b=3");
        JSON.stringify(p.getAll("a"));
        "#,
        js_string,
    );
    assert_eq!(s, r#"["1","2"]"#);
}

// ===========================================================================
// URLSearchParams size getter
// ===========================================================================

#[test]
fn search_params_size_reflects_entries() {
    let s = run_in_v8(
        r#"
        const p = new URLSearchParams("a=1&b=2&a=3");
        const before = p.size;
        p.append("c", "4");
        p.delete("a");  // 1-arg form: remove all "a"
        const after = p.size;
        JSON.stringify({ before, after });
        "#,
        js_string,
    );
    let v: serde_json::Value = serde_json::from_str(&s).expect("json");
    assert_eq!(v["before"], 3);
    assert_eq!(v["after"], 2);
}

// ===========================================================================
// Live size on bound URLSearchParams
// ===========================================================================

#[test]
fn url_search_params_live_size() {
    let s = run_in_v8(
        r#"
        const u = new URL("http://example.com/?a=1");
        const p = u.searchParams;
        const before = p.size;
        u.search = "?a=1&b=2&c=3";
        const after = p.size;
        JSON.stringify({ before, after });
        "#,
        js_string,
    );
    let v: serde_json::Value = serde_json::from_str(&s).expect("json");
    assert_eq!(v["before"], 1);
    assert_eq!(v["after"], 3);
}
