//! Exercise the embedded DB adapter through the real module loader, including
//! top-level evaluation and reuse of the module namespace.

use crate::common;
use common::{dispatch, m};

#[test]
fn install_schema_module_resolves_and_is_callable() {
    // Faithful mirror of `runtime-entry.js`'s
    // `await import("@zeroship/db/internal")`. Before the
    // ISS-63 fix this rejected with "Cannot find module"; after the fix
    // the runtime provides the module and `installSchema` is a function.
    let r = dispatch(
        m(r#"
        export async function test() {
            const sdk = await import("@zeroship/db/internal");
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
fn install_schema_entry_includes_db_helpers() {
    // The installer and its dependencies share the DB internal entry.
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
    // `await import("@zeroship/db/internal")` at the
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
        const _sdk = await import("@zeroship/db/internal");
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
            const a = await import("@zeroship/db/internal");
            const b = await import("@zeroship/db/internal");
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
