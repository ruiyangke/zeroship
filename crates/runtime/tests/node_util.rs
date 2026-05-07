//! `node:util` registered as a native V8 SyntheticModule.
//!
//! Covers the most-used 90% of the Node 22 surface: `format`,
//! `inspect` (primitives / arrays / objects / circulars), `promisify`,
//! `callbackify`, `deprecate`, `types.*` predicates,
//! `isDeepStrictEqual`, `parseArgs` (long-form `--flag value`), and
//! `TextEncoder` / `TextDecoder` re-exports.

mod common;
use common::{dispatch, m};

#[test]
fn import_format_works() {
    let r = dispatch(
        m(r#"
        import { format } from "node:util";
        export function test() {
            return format("hello %s %d", "world", 42);
        }
        "#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(r.json.contains("hello world 42"), "got: {}", r.json);
}

#[test]
fn import_format_extra_args_appended() {
    let r = dispatch(
        m(r#"
        import { format } from "node:util";
        export function test() {
            // "%s a" + extra "b" becomes "X a b" (Node behavior).
            return format("%s a", "X", "b");
        }
        "#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(r.json.contains("X a b"), "got: {}", r.json);
}

#[test]
fn import_inspect_object() {
    let r = dispatch(
        m(r#"
        import { inspect } from "node:util";
        export function test() {
            return inspect({ a: 1, b: "hi" });
        }
        "#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(r.json.contains("a: 1"), "got: {}", r.json);
    assert!(r.json.contains(r#"b: \"hi\""#), "got: {}", r.json);
}

#[test]
fn import_inspect_handles_circular() {
    let r = dispatch(
        m(r#"
        import { inspect } from "node:util";
        export function test() {
            const o = { a: 1 };
            o.self = o;
            return inspect(o);
        }
        "#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(r.json.contains("Circular"), "got: {}", r.json);
}

#[test]
fn import_inspect_array() {
    let r = dispatch(
        m(r#"
        import { inspect } from "node:util";
        export function test() {
            return inspect([1, 2, 3]);
        }
        "#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(r.json.contains("1, 2, 3"), "got: {}", r.json);
}

#[test]
fn import_inspect_colors_styles_present() {
    let r = dispatch(
        m(r#"
        import { inspect } from "node:util";
        export function test() {
            return {
                hasColors: typeof inspect.colors === "object",
                hasStyles: typeof inspect.styles === "object",
                hasCustom: typeof inspect.custom === "symbol",
            };
        }
        "#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(r.json.contains(r#""hasColors":true"#), "got: {}", r.json);
    assert!(r.json.contains(r#""hasStyles":true"#), "got: {}", r.json);
    assert!(r.json.contains(r#""hasCustom":true"#), "got: {}", r.json);
}

#[test]
fn import_promisify_works() {
    let r = dispatch(
        m(r#"
        import { promisify } from "node:util";
        export async function test() {
            const fn = promisify((cb) => cb(null, 42));
            const v = await fn();
            return { v };
        }
        "#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(r.json.contains(r#""v":42"#), "got: {}", r.json);
}

#[test]
fn import_promisify_rejects_on_error() {
    let r = dispatch(
        m(r#"
        import { promisify } from "node:util";
        export async function test() {
            const fn = promisify((cb) => cb(new Error("boom")));
            try { await fn(); return { ok: true }; }
            catch (e) { return { msg: e.message }; }
        }
        "#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(r.json.contains(r#""msg":"boom""#), "got: {}", r.json);
}

#[test]
fn import_callbackify_works() {
    let r = dispatch(
        m(r#"
        import { callbackify } from "node:util";
        export async function test() {
            const fn = callbackify(async (x) => x * 2);
            return await new Promise((resolve) => {
                fn(21, (err, val) => resolve({ err: err ? err.message : null, val }));
            });
        }
        "#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(r.json.contains(r#""err":null"#), "got: {}", r.json);
    assert!(r.json.contains(r#""val":42"#), "got: {}", r.json);
}

#[test]
fn import_deprecate_warns_once() {
    let r = dispatch(
        m(r#"
        import { deprecate } from "node:util";
        export function test() {
            const fn = deprecate(() => 1, "old");
            const a = fn();
            const b = fn();
            return { a, b };
        }
        "#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(r.json.contains(r#""a":1"#), "got: {}", r.json);
    assert!(r.json.contains(r#""b":1"#), "got: {}", r.json);
}

#[test]
fn import_types_predicates_work() {
    let r = dispatch(
        m(r#"
        import { types } from "node:util";
        export async function test() {
            return {
                date: types.isDate(new Date()),
                notDate: types.isDate({}),
                promise: types.isPromise(Promise.resolve()),
                notPromise: types.isPromise({ then: () => {} }),
                map: types.isMap(new Map()),
                set: types.isSet(new Set()),
                regex: types.isRegExp(/x/),
                ab: types.isArrayBuffer(new ArrayBuffer(4)),
                u8: types.isUint8Array(new Uint8Array(2)),
                asyncFn: types.isAsyncFunction(async () => {}),
                genFn: types.isGeneratorFunction(function* () {}),
                typed: types.isTypedArray(new Int32Array(1)),
            };
        }
        "#),
        "test",
        "[]",
    )
    .unwrap();
    for needle in [
        r#""date":true"#,
        r#""notDate":false"#,
        r#""promise":true"#,
        r#""notPromise":false"#,
        r#""map":true"#,
        r#""set":true"#,
        r#""regex":true"#,
        r#""ab":true"#,
        r#""u8":true"#,
        r#""asyncFn":true"#,
        r#""genFn":true"#,
        r#""typed":true"#,
    ] {
        assert!(r.json.contains(needle), "missing {needle}: {}", r.json);
    }
}

#[test]
fn import_is_deep_strict_equal_works() {
    let r = dispatch(
        m(r#"
        import { isDeepStrictEqual } from "node:util";
        export function test() {
            return {
                obj: isDeepStrictEqual({ a: 1, b: { c: 2 } }, { a: 1, b: { c: 2 } }),
                objNeq: isDeepStrictEqual({ a: 1 }, { a: 2 }),
                arr: isDeepStrictEqual([1, 2, 3], [1, 2, 3]),
                arrNeq: isDeepStrictEqual([1, 2], [1, 2, 3]),
                date: isDeepStrictEqual(new Date(0), new Date(0)),
                map: isDeepStrictEqual(new Map([["a", 1]]), new Map([["a", 1]])),
                set: isDeepStrictEqual(new Set([1, 2]), new Set([2, 1])),
                primitive: isDeepStrictEqual(1, 1),
                nan: isDeepStrictEqual(NaN, NaN),
            };
        }
        "#),
        "test",
        "[]",
    )
    .unwrap();
    for needle in [
        r#""obj":true"#,
        r#""objNeq":false"#,
        r#""arr":true"#,
        r#""arrNeq":false"#,
        r#""date":true"#,
        r#""map":true"#,
        r#""set":true"#,
        r#""primitive":true"#,
        r#""nan":true"#,
    ] {
        assert!(r.json.contains(needle), "missing {needle}: {}", r.json);
    }
}

#[test]
fn import_parse_args_long_form() {
    let r = dispatch(
        m(r#"
        import { parseArgs } from "node:util";
        export function test() {
            const r = parseArgs({
                args: ["--flag", "value", "pos1"],
                options: { flag: { type: "string" } },
            });
            return { values: r.values, positionals: r.positionals };
        }
        "#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(r.json.contains(r#""flag":"value""#), "got: {}", r.json);
    assert!(r.json.contains(r#""positionals":["pos1"]"#), "got: {}", r.json);
}

#[test]
fn import_parse_args_eq_form_and_boolean() {
    let r = dispatch(
        m(r#"
        import { parseArgs } from "node:util";
        export function test() {
            const r = parseArgs({
                args: ["--name=alice", "--verbose", "x", "y"],
                options: {
                    name: { type: "string" },
                    verbose: { type: "boolean" },
                },
            });
            return { v: r.values, p: r.positionals };
        }
        "#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(r.json.contains(r#""name":"alice""#), "got: {}", r.json);
    assert!(r.json.contains(r#""verbose":true"#), "got: {}", r.json);
    assert!(r.json.contains(r#""p":["x","y"]"#), "got: {}", r.json);
}

#[test]
fn import_text_encoder_decoder_identity() {
    let r = dispatch(
        m(r#"
        import { TextEncoder, TextDecoder } from "node:util";
        export function test() {
            return {
                encSame: TextEncoder === globalThis.TextEncoder,
                decSame: TextDecoder === globalThis.TextDecoder,
                roundtrip: new TextDecoder().decode(new TextEncoder().encode("zs")),
            };
        }
        "#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(r.json.contains(r#""encSame":true"#), "got: {}", r.json);
    assert!(r.json.contains(r#""decSame":true"#), "got: {}", r.json);
    assert!(r.json.contains(r#""roundtrip":"zs""#), "got: {}", r.json);
}

#[test]
fn default_import_returns_namespace_object() {
    let r = dispatch(
        m(r#"
        import util from "node:util";
        export function test() {
            return {
                hasFormat: typeof util.format === "function",
                hasInspect: typeof util.inspect === "function",
                hasPromisify: typeof util.promisify === "function",
                hasTypes: typeof util.types === "object",
            };
        }
        "#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(r.json.contains(r#""hasFormat":true"#), "got: {}", r.json);
    assert!(r.json.contains(r#""hasInspect":true"#), "got: {}", r.json);
    assert!(r.json.contains(r#""hasPromisify":true"#), "got: {}", r.json);
    assert!(r.json.contains(r#""hasTypes":true"#), "got: {}", r.json);
}
