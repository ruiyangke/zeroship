//! `node:path` (POSIX) registered as a native V8 SyntheticModule.
//!
//! Mirrors `node_module_registration.rs` in shape — assertions go
//! through `dispatch` so we exercise the full module-resolution path
//! (eager import-graph walk + V8 synthetic-module instantiation).

mod common;
use common::{dispatch, m};

#[test]
fn join_concatenates_segments() {
    let r = dispatch(
        m(r#"
        import { join } from "node:path";
        export function test() { return join("a", "b", "c"); }
        "#),
        "test",
        "[]",
    )
    .unwrap();
    assert_eq!(r.json, r#""a/b/c""#);
}

#[test]
fn join_normalizes_dotdot() {
    let r = dispatch(
        m(r#"
        import { join } from "node:path";
        export function test() { return join("/a", "b", "../c"); }
        "#),
        "test",
        "[]",
    )
    .unwrap();
    assert_eq!(r.json, r#""/a/c""#);
}

#[test]
fn resolve_absolute() {
    let r = dispatch(
        m(r#"
        import { resolve } from "node:path";
        export function test() { return resolve("/a", "b"); }
        "#),
        "test",
        "[]",
    )
    .unwrap();
    assert_eq!(r.json, r#""/a/b""#);
}

#[test]
fn resolve_uses_root_as_cwd() {
    // V8 has no real cwd; resolve falls back to "/" — Node would use
    // `process.cwd()` here but our runtime is rootless.
    let r = dispatch(
        m(r#"
        import { resolve } from "node:path";
        export function test() { return resolve("foo", "bar"); }
        "#),
        "test",
        "[]",
    )
    .unwrap();
    assert_eq!(r.json, r#""/foo/bar""#);
}

#[test]
fn dirname_strips_final_segment() {
    let r = dispatch(
        m(r#"
        import { dirname } from "node:path";
        export function test() { return dirname("/a/b/c"); }
        "#),
        "test",
        "[]",
    )
    .unwrap();
    assert_eq!(r.json, r#""/a/b""#);
}

#[test]
fn basename_strips_extension() {
    let r = dispatch(
        m(r#"
        import { basename } from "node:path";
        export function test() { return basename("/a/b/c.txt", ".txt"); }
        "#),
        "test",
        "[]",
    )
    .unwrap();
    assert_eq!(r.json, r#""c""#);
}

#[test]
fn extname_returns_dot_suffix() {
    let r = dispatch(
        m(r#"
        import { extname } from "node:path";
        export function test() {
            return {
                txt: extname("/a/b/c.txt"),
                dot: extname("."),
                dotfile: extname(".bashrc"),
                none: extname("/a/b/c"),
            };
        }
        "#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(r.json.contains(r#""txt":".txt""#), "got: {}", r.json);
    assert!(r.json.contains(r#""dot":"""#), "got: {}", r.json);
    assert!(r.json.contains(r#""dotfile":"""#), "got: {}", r.json);
    assert!(r.json.contains(r#""none":"""#), "got: {}", r.json);
}

#[test]
fn parse_returns_segments() {
    let r = dispatch(
        m(r#"
        import { parse } from "node:path";
        export function test() { return parse("/a/b/c.txt"); }
        "#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(r.json.contains(r#""root":"/""#), "got: {}", r.json);
    assert!(r.json.contains(r#""dir":"/a/b""#), "got: {}", r.json);
    assert!(r.json.contains(r#""base":"c.txt""#), "got: {}", r.json);
    assert!(r.json.contains(r#""name":"c""#), "got: {}", r.json);
    assert!(r.json.contains(r#""ext":".txt""#), "got: {}", r.json);
}

#[test]
fn normalize_collapses_dots() {
    let r = dispatch(
        m(r#"
        import { normalize } from "node:path";
        export function test() {
            return {
                a: normalize("/a/./b/../c"),
                trail: normalize("/a/b/"),
                rel: normalize("./a/b"),
            };
        }
        "#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(r.json.contains(r#""a":"/a/c""#), "got: {}", r.json);
    assert!(r.json.contains(r#""trail":"/a/b/""#), "got: {}", r.json);
    assert!(r.json.contains(r#""rel":"a/b""#), "got: {}", r.json);
}

#[test]
fn is_absolute_distinguishes_root() {
    let r = dispatch(
        m(r#"
        import { isAbsolute } from "node:path";
        export function test() {
            return { abs: isAbsolute("/foo"), rel: isAbsolute("foo") };
        }
        "#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(r.json.contains(r#""abs":true"#), "got: {}", r.json);
    assert!(r.json.contains(r#""rel":false"#), "got: {}", r.json);
}

#[test]
fn relative_walks_back_then_forward() {
    let r = dispatch(
        m(r#"
        import { relative } from "node:path";
        export function test() { return relative("/a/b", "/a/c"); }
        "#),
        "test",
        "[]",
    )
    .unwrap();
    assert_eq!(r.json, r#""../c""#);
}

#[test]
fn posix_self_reference() {
    let r = dispatch(
        m(r#"
        import path, { posix } from "node:path";
        export function test() {
            return {
                same: posix === path,
                posixSep: posix.sep,
                posixDelim: posix.delimiter,
            };
        }
        "#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(r.json.contains(r#""same":true"#), "got: {}", r.json);
    assert!(r.json.contains(r#""posixSep":"/""#), "got: {}", r.json);
    assert!(r.json.contains(r#""posixDelim":":""#), "got: {}", r.json);
}

#[test]
fn sep_and_delimiter_constants() {
    let r = dispatch(
        m(r#"
        import { sep, delimiter } from "node:path";
        export function test() { return { sep, delimiter }; }
        "#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(r.json.contains(r#""sep":"/""#), "got: {}", r.json);
    assert!(r.json.contains(r#""delimiter":":""#), "got: {}", r.json);
}

#[test]
fn format_inverse_of_parse() {
    let r = dispatch(
        m(r#"
        import { format, parse } from "node:path";
        export function test() {
            const p = "/a/b/c.txt";
            return format(parse(p));
        }
        "#),
        "test",
        "[]",
    )
    .unwrap();
    assert_eq!(r.json, r#""/a/b/c.txt""#);
}

#[test]
fn default_import_returns_namespace_object() {
    let r = dispatch(
        m(r#"
        import path from "node:path";
        export function test() {
            return {
                hasJoin: typeof path.join === "function",
                hasResolve: typeof path.resolve === "function",
                sep: path.sep,
            };
        }
        "#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(r.json.contains(r#""hasJoin":true"#), "got: {}", r.json);
    assert!(r.json.contains(r#""hasResolve":true"#), "got: {}", r.json);
    assert!(r.json.contains(r#""sep":"/""#), "got: {}", r.json);
}

#[test]
fn win32_stub_throws_on_call() {
    // Linux V8 runtime — `path.win32.join(...)` etc. throw a clear
    // `ERR_METHOD_NOT_IMPLEMENTED`. Property reads (`sep`, `delimiter`)
    // remain readable so npm packages probing those don't crash.
    let r = dispatch(
        m(r#"
        import { win32 } from "node:path";
        export function test() {
            let threw = false;
            let code;
            try { win32.join("a", "b"); }
            catch (e) { threw = true; code = e.code; }
            return { threw, code, sep: win32.sep, delim: win32.delimiter };
        }
        "#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(r.json.contains(r#""threw":true"#), "got: {}", r.json);
    assert!(
        r.json.contains(r#""code":"ERR_METHOD_NOT_IMPLEMENTED""#),
        "got: {}",
        r.json
    );
    assert!(r.json.contains(r#""sep":"\\""#), "got: {}", r.json);
    assert!(r.json.contains(r#""delim":";""#), "got: {}", r.json);
}
