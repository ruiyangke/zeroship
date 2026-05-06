//! `await import(specifier)` — V8 host callback that resolves dynamic
//! imports against the per-isolate module registry installed by
//! `load_modules`. This coverage is limited to bundle-resident
//! modules (no fetch, no compile-on-demand).
//!
//! Covers:
//!   - Static + dynamic imports of the same module yield the SAME
//!     namespace object (no duplicate evaluation, no split state).
//!   - A module reachable only through `await import()` works as long
//!     as it's been pulled into the registry by some static path.
//!   - Two consecutive dynamic imports of the same specifier share
//!     module state (counter exported from the module increments).
//!   - Unknown specifier → rejected with `TypeError("Cannot find
//!     module '<spec>'")`.
//!   - `node:async_hooks` and `node:crypto` resolve via the native
//!     synthetic path even when only dynamically imported.

mod common;
use common::{dispatch, m};
use zeroship_runtime::ModuleEntry;

fn me(specifier: &str, source: &str) -> ModuleEntry {
    ModuleEntry { specifier: specifier.into(), source: source.into() }
}

#[test]
fn static_and_dynamic_share_namespace() {
    // The user statically imports `./shared.js`, then dynamically
    // imports the same specifier. The two namespace objects must be
    // `===`: same module record, same exports, no double evaluation.
    let modules = vec![
        me(
            "index.js",
            r#"
            import * as staticNs from "./shared.js";
            export async function test() {
                const dynNs = await import("./shared.js");
                return {
                    sameNs: staticNs === dynNs,
                    sameValue: staticNs.value === dynNs.value,
                    value: dynNs.value,
                };
            }
            "#,
        ),
        me("shared.js", "export const value = 42;"),
    ];

    let r = dispatch(modules, "test", "[]").unwrap();
    assert!(r.json.contains(r#""sameNs":true"#), "got: {}", r.json);
    assert!(r.json.contains(r#""sameValue":true"#), "got: {}", r.json);
    assert!(r.json.contains(r#""value":42"#), "got: {}", r.json);
}

#[test]
fn dynamic_only_import_via_registry_path() {
    // User code never spells `import "./tools.js"` statically, but a
    // sibling module does — so the BFS in `load_modules` does compile
    // tools.js into the registry. The dynamic import then hits the
    // registry path.
    let modules = vec![
        me(
            "index.js",
            r#"
            import "./bridge.js"; // forces bridge.js → tools.js into registry
            export async function test() {
                const tools = await import("./tools.js");
                return { kind: typeof tools.add, sum: tools.add(2, 3) };
            }
            "#,
        ),
        me("bridge.js", r#"import "./tools.js";"#),
        me("tools.js", "export function add(a, b) { return a + b; }"),
    ];

    let r = dispatch(modules, "test", "[]").unwrap();
    assert!(r.json.contains(r#""kind":"function""#), "got: {}", r.json);
    assert!(r.json.contains(r#""sum":5"#), "got: {}", r.json);
}

#[test]
fn dynamic_import_caches_module_instance() {
    // Two `await import()` calls in the same RPC return the same
    // module record — the exported counter increments once because
    // the side-effecting initializer ran once.
    let modules = vec![
        me(
            "index.js",
            r#"
            import "./counter.js"; // pull into registry
            export async function test() {
                const a = await import("./counter.js");
                const b = await import("./counter.js");
                a.bump();
                b.bump();
                return { sameNs: a === b, count: a.count() };
            }
            "#,
        ),
        me(
            "counter.js",
            r#"
            let n = 0;
            export function bump() { n += 1; }
            export function count() { return n; }
            "#,
        ),
    ];

    let r = dispatch(modules, "test", "[]").unwrap();
    assert!(r.json.contains(r#""sameNs":true"#), "got: {}", r.json);
    assert!(r.json.contains(r#""count":2"#), "got: {}", r.json);
}

#[test]
fn unknown_specifier_rejects_with_typeerror() {
    // No `does-not-exist.js` in the bundle → the dynamic-import
    // callback rejects with TypeError naming the bad specifier.
    let r = dispatch(
        m(r#"
        export async function test() {
            try {
                await import("./does-not-exist.js");
                return { ok: true };
            } catch (e) {
                return { name: e?.constructor?.name, msg: e?.message };
            }
        }
        "#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(r.json.contains(r#""name":"TypeError""#), "got: {}", r.json);
    assert!(r.json.contains("does-not-exist"), "got: {}", r.json);
    assert!(r.json.contains("Cannot find module"), "got: {}", r.json);
}

#[test]
fn dynamic_node_async_hooks_resolves_via_native_path() {
    // No static import of `node:async_hooks` — the dynamic import
    // mints the synthetic module on first call and caches it.
    let r = dispatch(
        m(r#"
        export async function test() {
            const ah = await import("node:async_hooks");
            const als = new ah.AsyncLocalStorage();
            return als.run({ key: "v" }, () => ({
                hasAls: typeof ah.AsyncLocalStorage === "function",
                store: als.getStore(),
            }));
        }
        "#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(r.json.contains(r#""hasAls":true"#), "got: {}", r.json);
    assert!(r.json.contains(r#""store":{"key":"v"}"#), "got: {}", r.json);
}

#[test]
fn dynamic_node_crypto_resolves_via_native_path() {
    // `createHash` + `randomUUID` come back from a dynamic-only
    // import — verifies the second native specifier flows through
    // the same callback.
    let r = dispatch(
        m(r#"
        export async function test() {
            const c = await import("node:crypto");
            const h = c.createHash("sha256");
            h.update("abc");
            const id = c.randomUUID();
            return {
                digest: h.digest("hex"),
                idLen: id.length,
                dashes: (id.match(/-/g) || []).length,
            };
        }
        "#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(
        r.json.contains("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"),
        "got: {}", r.json,
    );
    assert!(r.json.contains(r#""idLen":36"#), "got: {}", r.json);
    assert!(r.json.contains(r#""dashes":4"#), "got: {}", r.json);
}

#[test]
fn dynamic_native_import_caches_for_subsequent_calls() {
    // After a dynamic import of `node:crypto`, a *second* dynamic
    // import returns the same module namespace. The cache_into_registry
    // step in the callback ensures we don't mint two different
    // SyntheticModules for one specifier.
    let r = dispatch(
        m(r#"
        export async function test() {
            const a = await import("node:crypto");
            const b = await import("node:crypto");
            return { same: a === b };
        }
        "#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(r.json.contains(r#""same":true"#), "got: {}", r.json);
}
