//! ESM integration tests — ride on `call_fetch_handler` via the common
//! `dispatch` bootstrap helper. Proves import graphs resolve correctly
//! in the actual kernel, complementing the lib-side `modules::tests`
//! unit suite.

mod common;
use common::*;

use zeroship_runtime::ModuleEntry;

#[test]
fn esm_basic_rpc() {
    let modules = m(r#"
        export function ping() { return "pong"; }
        export function add(a, b) { return a + b; }
    "#);
    let r = dispatch(modules.clone(), "ping", "[]").unwrap();
    assert_eq!(r.json, "\"pong\"");

    let r2 = dispatch(modules, "add", "[3,4]").unwrap();
    assert_eq!(r2.json, "7");
}

#[test]
fn esm_multi_module_rpc() {
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
    let r = dispatch(modules, "compute", "[3,4]").unwrap();
    assert_eq!(r.json, "7");
}
