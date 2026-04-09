mod common;
use common::*;

use std::collections::HashMap;

#[test]
fn env_get_works() {
    let env = HashMap::from([("test_key".to_string(), "test_value".to_string())]);
    let r = dispatch_with_env(
        m(r#"export function test() { return env.get("test_key"); }"#),
        env,
        r#"{"jsonrpc":"2.0","method":"test","params":[],"id":1}"#,
    ).unwrap();
    assert!(r.json.contains("test_value"), "got: {}", r.json);
}

#[test]
fn env_get_missing_returns_null() {
    let r = dispatch(
        m(r#"export function test() { return env.get("nonexistent_key_xyz") === null ? "is_null" : "not_null"; }"#),
        r#"{"jsonrpc":"2.0","method":"test","params":[],"id":1}"#,
    ).unwrap();
    assert!(r.json.contains("is_null"), "got: {}", r.json);
}
