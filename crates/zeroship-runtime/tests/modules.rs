//! ESM integration tests — ride on `call_fetch_handler` via the common
//! `dispatch` helper. Proves import graphs resolve correctly
//! in the actual kernel, complementing the lib-side `modules::tests`
//! unit suite.

use crate::common;
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
            "#
            .into(),
        },
        ModuleEntry {
            specifier: "math.js".into(),
            source: "export function add(a, b) { return a + b; }".into(),
        },
    ];
    let r = dispatch(modules, "compute", "[3,4]").unwrap();
    assert_eq!(r.json, "7");
}

#[test]
fn nested_imports_preserve_entry_identity_and_referrer_paths() {
    let modules = vec![
        ModuleEntry {
            specifier: "app/entry.js".into(),
            source: "import { read } from './deep/read.js'; export const marker = 'entry'; export function test() { return read(); }".into(),
        },
        ModuleEntry {
            specifier: "app/deep/read.js".into(),
            source: "import { marker } from '../entry.js'; import value from '../value.js'; export function read() { return marker + ':' + value; }".into(),
        },
        ModuleEntry { specifier: "app/value.js".into(), source: "export default 'nested';".into() },
        ModuleEntry { specifier: "value.js".into(), source: "throw new Error('root module must stay unused');".into() },
    ];
    assert_eq!(
        dispatch(modules, "test", "[]").unwrap().json,
        r#""entry:nested""#
    );
}

#[test]
fn creator_entry_names_are_preserved_and_safely_quoted() {
    for entry in ["__user__.js", "app/\"quoted.js"] {
        let modules = vec![ModuleEntry {
            specifier: entry.into(),
            source: "export function test() { return 'ok'; }".into(),
        }];
        assert_eq!(dispatch(modules, "test", "[]").unwrap().json, r#""ok""#);
    }
}

#[test]
fn relative_imports_do_not_fall_back_to_root_modules() {
    let modules = vec![
        ModuleEntry {
            specifier: "app/entry.js".into(),
            source: "import value from './value.js'; export function test() { return value; }"
                .into(),
        },
        ModuleEntry {
            specifier: "value.js".into(),
            source: "export default 'wrong';".into(),
        },
    ];
    assert!(dispatch(modules, "test", "[]").is_err());
}
