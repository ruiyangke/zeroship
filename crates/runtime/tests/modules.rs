mod common;
use common::*;

use zeroship_runtime::{init_v8, ModuleEntry};
use zeroship_runtime::runtime::Runtime;

#[test]
fn esm_basic_rpc() {
    init_v8();
    let modules = vec![ModuleEntry {
        specifier: "index.js".into(),
        source: r#"
            export function ping() { return "pong"; }
            export function add(a, b) { return a + b; }
        "#.into(),
    }];
    let mut runtime = Runtime::new_direct(modules.clone(), no_env(), None, None);
    let r = runtime.dispatch_rpc(&modules, r#"{"jsonrpc":"2.0","method":"ping","params":[],"id":1}"#).unwrap();
    assert!(r.json.contains("pong"), "got: {}", r.json);

    let r2 = runtime.dispatch_rpc(&modules, r#"{"jsonrpc":"2.0","method":"add","params":[3,4],"id":2}"#).unwrap();
    assert!(r2.json.contains("\"result\":7"), "got: {}", r2.json);
}

#[test]
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
    let mut runtime = Runtime::new_direct(modules.clone(), no_env(), None, None);
    let r = runtime.dispatch_rpc(&modules, r#"{"jsonrpc":"2.0","method":"compute","params":[3,4],"id":1}"#).unwrap();
    assert!(r.json.contains("7"), "got: {}", r.json);
}
