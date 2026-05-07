//! `node:async_hooks` / `node:crypto` / `node:zlib` / `node:os` /
//! `node:util` registered as native V8 SyntheticModules — the runtime
//! resolves them itself rather than handing the bare specifier to a
//! vite-plugin shim that re-exports `globalThis.__zsAsyncHooks` /
//! `globalThis.__zeroship_node_crypto`.
//!
//! Covers:
//!   - `import { AsyncLocalStorage }` returns a usable class.
//!   - `import { createHash }` produces working digests.
//!   - `import { gzipSync } from "node:zlib"` resolves.
//!   - `import { platform } from "node:os"` resolves.
//!   - `import { format } from "node:util"` resolves.
//!   - The legacy globals are gone.
//!   - Unknown `node:*` specifiers surface a clear error.

mod common;
use common::{dispatch, m};

#[test]
fn import_async_local_storage_works() {
    let r = dispatch(
        m(r#"
        import { AsyncLocalStorage } from "node:async_hooks";
        export function test() {
            const als = new AsyncLocalStorage();
            return als.run({ ctx: "v" }, () => {
                return { kind: typeof als, store: als.getStore() };
            });
        }
        "#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(r.json.contains(r#""store":{"ctx":"v"}"#), "got: {}", r.json);
}

#[test]
fn import_create_hash_works() {
    let r = dispatch(
        m(r#"
        import { createHash } from "node:crypto";
        export function test() {
            const h = createHash("sha256");
            h.update("abc");
            return h.digest("hex");
        }
        "#),
        "test",
        "[]",
    )
    .unwrap();
    // SHA-256("abc")
    assert!(
        r.json.contains("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"),
        "got: {}", r.json
    );
}

#[test]
fn import_random_uuid_works() {
    let r = dispatch(
        m(r#"
        import { randomUUID } from "node:crypto";
        export function test() {
            const id = randomUUID();
            return { len: id.length, dashes: (id.match(/-/g) || []).length };
        }
        "#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(r.json.contains(r#""len":36"#), "got: {}", r.json);
    assert!(r.json.contains(r#""dashes":4"#), "got: {}", r.json);
}

#[test]
fn legacy_async_hooks_global_is_gone() {
    let r = dispatch(
        m(r#"
        export function test() {
            return { has: typeof globalThis.__zsAsyncHooks };
        }
        "#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(r.json.contains(r#""has":"undefined""#), "got: {}", r.json);
}

#[test]
fn legacy_crypto_global_is_gone() {
    let r = dispatch(
        m(r#"
        export function test() {
            return { has: typeof globalThis.__zeroship_node_crypto };
        }
        "#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(r.json.contains(r#""has":"undefined""#), "got: {}", r.json);
}

#[test]
fn unknown_node_specifier_surfaces_clear_error() {
    let err = dispatch(
        m(r#"
        import { something } from "node:doesnotexist";
        export function test() { return something; }
        "#),
        "test",
        "[]",
    )
    .unwrap_err();
    // The module loader's "Cannot resolve" message bubbles up via
    // the dispatch error path. We don't pin the exact string — just
    // that the unknown specifier appears.
    assert!(
        err.contains("doesnotexist") || err.contains("resolve") || err.contains("Cannot find"),
        "expected resolution error, got: {err}"
    );
}

#[test]
fn default_import_returns_namespace_object() {
    let r = dispatch(
        m(r#"
        import nh from "node:async_hooks";
        export function test() {
            return { hasAls: typeof nh.AsyncLocalStorage === "function" };
        }
        "#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(r.json.contains(r#""hasAls":true"#), "got: {}", r.json);
}

#[test]
fn crypto_default_import_returns_namespace_object() {
    let r = dispatch(
        m(r#"
        import crypto from "node:crypto";
        export function test() {
            return {
                hasCreateHash: typeof crypto.createHash === "function",
                hasRandomUUID: typeof crypto.randomUUID === "function",
            };
        }
        "#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(r.json.contains(r#""hasCreateHash":true"#), "got: {}", r.json);
    assert!(r.json.contains(r#""hasRandomUUID":true"#), "got: {}", r.json);
}

#[test]
fn import_gzip_sync_works() {
    let r = dispatch(
        m(r#"
        import { gzipSync } from "node:zlib";
        export function test() {
            return { kind: typeof gzipSync, len: gzipSync("hi").length > 0 };
        }
        "#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(r.json.contains(r#""kind":"function""#), "got: {}", r.json);
    assert!(r.json.contains(r#""len":true"#), "got: {}", r.json);
}

#[test]
fn import_os_platform_works() {
    let r = dispatch(
        m(r#"
        import { platform } from "node:os";
        export function test() {
            return { p: platform() };
        }
        "#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(r.json.contains(r#""p":"linux""#), "got: {}", r.json);
}

#[test]
fn import_util_format_works() {
    let r = dispatch(
        m(r#"
        import { format, types } from "node:util";
        export function test() {
            return {
                fmt: format("hi %s %d", "x", 7),
                isMap: types.isMap(new Map()),
            };
        }
        "#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(r.json.contains(r#""fmt":"hi x 7""#), "got: {}", r.json);
    assert!(r.json.contains(r#""isMap":true"#), "got: {}", r.json);
}
