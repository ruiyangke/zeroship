//! ISS-63 — `@zeroship/bootstrap/install-schema` must resolve on the
//! production/worker runtime.
//!
//! The runtime's embedded `runtime-entry.js` (spliced into the bootstrap
//! `index.js` via `include_str!`) does, during module evaluation:
//!
//!     const sdk = await import("@zeroship/bootstrap/install-schema");
//!
//! whenever the app declares a `default.schema` and `env.db` is present.
//! On the production worker the user's `.zship` bundle is a single
//! self-contained `index.js` that does NOT contain that module (vite
//! tree-shakes the framework-internal installSchema out), and it is not a
//! `node:*` native module — so the dynamic-import host callback used to
//! reject with `TypeError: Cannot find module
//! '@zeroship/bootstrap/install-schema'`, which aborted module evaluation
//! and 500'd every env.db app.
//!
//! These tests drive the REAL runtime module loader + the REAL
//! dynamic-import host callback (no shim) via the `dispatch` harness,
//! which builds a `Runtime` exactly as the worker does (BOOTSTRAP_JS
//! injection included). The user source below mirrors the runtime-entry's
//! import so the assertion exercises the same resolution path.

mod common;
use common::{dispatch, m};

#[test]
fn install_schema_module_resolves_and_is_callable() {
    // Faithful mirror of `runtime-entry.js`'s
    // `await import("@zeroship/bootstrap/install-schema")`. Before the
    // ISS-63 fix this rejected with "Cannot find module"; after the fix
    // the runtime provides the module and `installSchema` is a function.
    let r = dispatch(
        m(r#"
        export async function test() {
            const sdk = await import("@zeroship/bootstrap/install-schema");
            return {
                hasInstallSchema: typeof sdk.installSchema === "function",
            };
        }
        "#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(
        r.json.contains(r#""hasInstallSchema":true"#),
        "install-schema did not resolve to a module exporting installSchema; got: {}",
        r.json,
    );
}

#[test]
fn install_schema_transitive_db_internal_resolves() {
    // installSchema statically imports `@zeroship/db/internal`. The
    // runtime must resolve that transitive dependency too (it in turn
    // imports the runtime-provided `zeroship` facade). A direct import of
    // `@zeroship/db/internal` exercises that leg — runtime-entry.js also
    // imports it directly for `_flushPendingMaskPolicy`.
    let r = dispatch(
        m(r#"
        export async function test() {
            const internal = await import("@zeroship/db/internal");
            return {
                hasCollection: typeof internal.Collection === "function",
                hasNaming: typeof internal.naming !== "undefined",
                hasFlush: typeof internal._flushPendingMaskPolicy === "function",
            };
        }
        "#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(
        r.json.contains(r#""hasCollection":true"#),
        "@zeroship/db/internal did not resolve with Collection; got: {}",
        r.json,
    );
    assert!(
        r.json.contains(r#""hasFlush":true"#),
        "@zeroship/db/internal missing _flushPendingMaskPolicy; got: {}",
        r.json,
    );
}

#[test]
fn install_schema_resolves_during_module_evaluation() {
    // FAITHFUL to the production timing: `runtime-entry.js` issues
    // `await import("@zeroship/bootstrap/install-schema")` at the
    // bootstrap module's TOP LEVEL — during `load_modules`' `evaluate` +
    // microtask checkpoint, NOT from a later request handler. The module
    // registry's `RefCell` is borrowed by `load_modules` across that
    // evaluate; a dynamic-import resolution that re-borrows it mutably
    // would `RefCell::already_borrowed`-panic (a non-unwinding abort on
    // the worker). This test reproduces that timing with a top-level
    // await so the resolution must succeed WITHOUT a borrow conflict.
    let modules = m(r#"
        // Top-level await — runs during module evaluation, exactly like
        // the runtime-injected runtime-entry's install-schema import.
        const _sdk = await import("@zeroship/bootstrap/install-schema");
        globalThis.__zsEvalTimeInstallSchema = typeof _sdk.installSchema;
        export async function test() {
            return { evalTime: globalThis.__zsEvalTimeInstallSchema };
        }
    "#);
    let r = dispatch(modules, "test", "[]").unwrap();
    assert!(
        r.json.contains(r#""evalTime":"function""#),
        "eval-time install-schema import did not resolve to a function; got: {}",
        r.json,
    );
}

#[test]
fn install_schema_shares_instance_across_imports() {
    // Two dynamic imports of the same specifier return the SAME module
    // namespace — the runtime caches the compiled+evaluated module into
    // the registry on first resolution (no double evaluation, no split
    // state). Mirrors the `node:*` native caching guarantee.
    let r = dispatch(
        m(r#"
        export async function test() {
            const a = await import("@zeroship/bootstrap/install-schema");
            const b = await import("@zeroship/bootstrap/install-schema");
            return { same: a === b };
        }
        "#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(
        r.json.contains(r#""same":true"#),
        "repeated install-schema imports did not share a module instance; got: {}",
        r.json,
    );
}
