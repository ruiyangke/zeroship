mod common;
use common::*;

#[test]
fn console_log_works() {
    let r = dispatch(m(r#"
        export function greet() {
            console.log("Hello from JS!");
            return "logged";
        }
    "#), r#"{"jsonrpc":"2.0","method":"greet","params":[],"id":1}"#).unwrap();
    assert!(r.json.contains("logged"));
}
