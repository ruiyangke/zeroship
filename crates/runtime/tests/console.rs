mod common;
use common::*;

#[test]
fn console_log_works() {
    let r = dispatch(m(r#"
        export function greet() {
            console.log("Hello from JS!");
            return "logged";
        }
    "#), "greet", "[]").unwrap();
    assert!(r.json.contains("logged"));
}
