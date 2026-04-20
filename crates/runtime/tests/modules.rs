//! ESM integration tests via the deleted `dispatch_rpc` path.
//!
//! The lib-side `modules::tests` suite already covers module loading /
//! import-graph resolution using `load_modules` directly; these tests are
//! end-to-end smoke tests through `dispatch_rpc`, which D2 removes. Kept
//! here for history — D2 deletes them.
//!
//! TODO(PR 1 Task D2): delete this file once `dispatch_rpc` is removed.

use zeroship_runtime::{init_v8, ModuleEntry};
use zeroship_runtime::runtime::Runtime;

#[test]
#[ignore = "PR 1 Task D2: dispatch_rpc is removed — lib modules::tests covers module loading"]
fn esm_basic_rpc() {
    init_v8();
    let modules = vec![ModuleEntry {
        specifier: "index.js".into(),
        source: r#"
            export function ping() { return "pong"; }
            export function add(a, b) { return a + b; }
        "#.into(),
    }];
    let runtime = Runtime::builder().modules(modules).build();
    let r = runtime.dispatch_rpc("ping", "[]").unwrap();
    assert_eq!(r.json, "\"pong\"");

    let r2 = runtime.dispatch_rpc("add", "[3,4]").unwrap();
    assert_eq!(r2.json, "7");
}

#[test]
#[ignore = "PR 1 Task D2: dispatch_rpc is removed — lib modules::tests covers module loading"]
fn esm_multi_module_rpc() {
    init_v8();
    let modules = vec![
        ModuleEntry {
            specifier: "index.js".into(),
            source: r#"
                import { add } from './math.js';
                export function compute(a, b) { return add(a, b); }
            "#.into(),
        },
        ModuleEntry {
            specifier: "math.js".into(),
            source: "export function add(a, b) { return a + b; }".into(),
        },
    ];
    let runtime = Runtime::builder().modules(modules).build();
    let r = runtime.dispatch_rpc("compute", "[3,4]").unwrap();
    assert_eq!(r.json, "7");
}
