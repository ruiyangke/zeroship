mod common;
use common::*;

#[test]
fn kv_store_works() {
    let r = dispatch(m(r#"
        export function test() {
            kv.set("name", "Alice");
            kv.set("age", "30");
            var name = kv.get("name");
            var missing = kv.get("nonexistent");
            var keys = kv.list();
            kv.delete("age");
            var afterDelete = kv.list();
            return { name, missing, keys, afterDelete };
        }
    "#), r#"{"jsonrpc":"2.0","method":"test","params":[],"id":1}"#).unwrap();
    assert!(r.json.contains("Alice"), "got: {}", r.json);
    assert!(r.json.contains("null"), "got: {}", r.json);
}

#[test]
fn kv_persists_across_requests() {
    let results = dispatch_multi(
        m(r#"
            export function set(k, v) { kv.set(k, v); return "ok"; }
            export function get(k) { return kv.get(k); }
        "#),
        &[
            r#"{"jsonrpc":"2.0","method":"set","params":["key1","value1"],"id":1}"#,
            r#"{"jsonrpc":"2.0","method":"get","params":["key1"],"id":2}"#,
        ],
    );
    assert!(results[1].as_ref().unwrap().json.contains("value1"), "got: {}", results[1].as_ref().unwrap().json);
}
