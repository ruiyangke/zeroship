//! WPT (Web Platform Tests) runner for the raw V8 runtime.
//!
//! Uses testharness.js in ShellTestEnvironment mode (no document/DOM).
//! Scripts are loaded separately via eval to avoid V8 string size limits.

use appbase_runtime::{init_v8, Isolate, ModuleEntry};
use std::path::Path;

const WPT_ROOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../refs/wpt");
const REPORT_JS: &str = include_str!("wpt/testharnessreport.js");
const SERVER_SHIM: &str = include_str!("wpt/server_shim.js");

/// Shims loaded BEFORE testharness.js (no `document` → ShellTestEnvironment).
const SHIMS_PRE: &str = r#"
var self = globalThis;
var window = globalThis;
var navigator = { userAgent: "appbase-v8" };
var location = { href:"http://localhost/test", protocol:"http:", host:"localhost",
    pathname:"/test", search:"", hash:"", origin:"http://localhost" };

(function() {
    var _l = {};
    globalThis.addEventListener = function(t,f){ if(!_l[t]) _l[t]=[]; _l[t].push(f); };
    globalThis.removeEventListener = function(t,f){ if(_l[t]) _l[t]=_l[t].filter(function(x){return x!==f}); };
    globalThis.dispatchEvent = function(e){ var ls=_l[e.type||e]||[]; for(var i=0;i<ls.length;i++) ls[i](e); };
})();

if (typeof Event === "undefined") {
    function Event(t,o){ this.type=t; this.bubbles=!!(o&&o.bubbles); }
    globalThis.Event = Event;
}
"#;

fn run_wpt_test(test_path: &str) -> (usize, usize, Vec<(String, bool, Option<String>)>) {
    let wpt_root = Path::new(WPT_ROOT);
    let harness_js = std::fs::read_to_string(wpt_root.join("resources/testharness.js"))
        .expect("testharness.js not found");
    let test_js = std::fs::read_to_string(wpt_root.join(test_path))
        .unwrap_or_else(|e| panic!("WPT test not found: {test_path}: {e}"));

    // Combine pre-shims + server_shim into a single ES module.
    // The shims set globalThis properties (works from module scope).
    let module_source = format!("{SHIMS_PRE}\n{SERVER_SHIM}");
    let modules = vec![ModuleEntry {
        specifier: "index.js".into(),
        source: module_source,
    }];

    let result = std::panic::catch_unwind(|| {
        let mut iso = Isolate::new(modules, std::collections::HashMap::new());

        // Load testharness.js via eval (200KB, too large for single server_js)
        let body = serde_json::json!({"jsonrpc":"2.0","method":"__load_harness","params":[harness_js],"id":1}).to_string();
        let r = iso.execute_request(&body).map_err(|e| format!("harness: {e}"))?;
        if let Some(s) = extract_result(&r.json) { if s.starts_with("error") { return Err(format!("harness: {s}")); } }

        // Load report.js
        let body = serde_json::json!({"jsonrpc":"2.0","method":"__load_report","params":[REPORT_JS],"id":2}).to_string();
        iso.execute_request(&body).map_err(|e| format!("report: {e}"))?;

        // Run test file
        let body = serde_json::json!({"jsonrpc":"2.0","method":"__run_test","params":[test_js],"id":3}).to_string();
        let r = iso.execute_request(&body).map_err(|e| format!("test: {e}"))?;
        if let Some(s) = extract_result(&r.json) {
            eprintln!("[wpt] {test_path}: after eval: {s}");
        }

        // Call done() to finalize test collection
        let r = iso.execute_request(r#"{"jsonrpc":"2.0","method":"__done","params":[],"id":4}"#)
            .map_err(|e| format!("done: {e}"))?;
        if let Some(s) = extract_result(&r.json) {
            eprintln!("[wpt] {test_path}: after done: {s}");
        }

        // Poll for async tests
        for _ in 0..200 {
            match iso.execute_request(r#"{"jsonrpc":"2.0","method":"__wpt_done","params":[],"id":0}"#) {
                Ok(r) if r.json.contains("true") => break,
                Ok(_) => std::thread::sleep(std::time::Duration::from_millis(50)),
                Err(_) => break,
            }
        }

        iso.execute_request(r#"{"jsonrpc":"2.0","method":"__wpt_run","params":[],"id":99}"#)
    });

    match result {
        Ok(Ok(r)) => parse_results(&r.json),
        Ok(Err(e)) => { eprintln!("[wpt] {test_path}: {e}"); (0, 0, vec![]) }
        Err(e) => {
            let msg = e.downcast_ref::<String>().map(|s| s.as_str())
                .or_else(|| e.downcast_ref::<&str>().copied())
                .unwrap_or("unknown");
            eprintln!("[wpt] {test_path}: panicked: {msg}");
            (0, 0, vec![])
        }
    }
}

fn extract_result(json: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    v.get("result").and_then(|r| r.as_str()).map(|s| s.to_string())
}

fn parse_results(json: &str) -> (usize, usize, Vec<(String, bool, Option<String>)>) {
    let rpc: serde_json::Value = serde_json::from_str(json).unwrap_or_default();
    let results_str = rpc.get("result").and_then(|v| v.as_str()).unwrap_or("[]");
    let results: Vec<serde_json::Value> = serde_json::from_str(results_str).unwrap_or_default();

    let mut passed = 0;
    let mut failed = 0;
    let mut details = Vec::new();

    for entry in &results {
        let name = entry.get("name").and_then(|v| v.as_str()).unwrap_or("<unnamed>").to_string();
        let status = entry.get("status").and_then(|v| v.as_u64()).unwrap_or(1);
        let message = entry.get("message").and_then(|v| v.as_str()).map(|s| s.to_string());
        let is_pass = status == 0;
        if is_pass { passed += 1; } else { failed += 1; }
        details.push((name, is_pass, message));
    }

    (passed, failed, details)
}

fn run_and_report(test_path: &str) {
    let (p, f, d) = run_wpt_test(test_path);
    let label = test_path.rsplit('/').next().unwrap_or(test_path);
    println!("\n=== WPT: {label} ===");
    println!("  PASS: {p}  FAIL: {f}  TOTAL: {}", p + f);
    for (name, is_pass, message) in &d {
        let tag = if *is_pass { "PASS" } else { "FAIL" };
        if *is_pass {
            println!("  [{tag}] {name}");
        } else {
            let msg = message.as_deref().unwrap_or("");
            let short = if msg.len() > 120 { &msg[..120] } else { msg };
            println!("  [{tag}] {name} -- {short}");
        }
    }
    if p + f == 0 {
        println!("  WARNING: 0 tests ran");
    }
}

// ---------------------------------------------------------------------------
// Smoke test
// ---------------------------------------------------------------------------

#[test]
fn wpt_harness_smoke() {
    init_v8();
    let wpt_root = Path::new(WPT_ROOT);
    let harness_js = std::fs::read_to_string(wpt_root.join("resources/testharness.js")).unwrap();
    let module_source = format!("{SHIMS_PRE}\n{SERVER_SHIM}");
    let modules = vec![ModuleEntry {
        specifier: "index.js".into(),
        source: module_source,
    }];

    let mut iso = Isolate::new(modules, std::collections::HashMap::new());
    let body = serde_json::json!({"jsonrpc":"2.0","method":"__load_harness","params":[harness_js],"id":1}).to_string();
    let r = iso.execute_request(&body).unwrap();
    eprintln!("[smoke] harness load: {}", r.json);

    let body = serde_json::json!({"jsonrpc":"2.0","method":"__load_report","params":[REPORT_JS],"id":2}).to_string();
    iso.execute_request(&body).unwrap();

    let test_code = r#"
test(function() { assert_equals(1 + 1, 2); }, "basic arithmetic");
test(function() { assert_true(true); }, "true is true");
test(function() { var h = new Headers(); assert_true(h instanceof Headers); }, "Headers constructor");
done();
"#;
    let body = serde_json::json!({"jsonrpc":"2.0","method":"__run_test","params":[test_code],"id":3}).to_string();
    let r = iso.execute_request(&body).unwrap();
    eprintln!("[smoke] test run: {}", r.json);

    let r = iso.execute_request(r#"{"jsonrpc":"2.0","method":"__wpt_run","params":[],"id":99}"#).unwrap();
    let (p, f, d) = parse_results(&r.json);
    println!("\n=== SMOKE ===  PASS: {p}  FAIL: {f}");
    for (name, ok, msg) in &d {
        println!("  [{}] {name} {}", if *ok {"PASS"} else {"FAIL"}, msg.as_deref().unwrap_or(""));
    }
    assert!(p >= 2, "Expected ≥2 passing, got {p}");
}

// ---------------------------------------------------------------------------
// Headers
// ---------------------------------------------------------------------------
#[test] fn wpt_headers_basic()     { init_v8(); run_and_report("fetch/api/headers/headers-basic.any.js"); }
#[test] fn wpt_headers_combine()   { init_v8(); run_and_report("fetch/api/headers/headers-combine.any.js"); }
#[test] fn wpt_headers_errors()    { init_v8(); run_and_report("fetch/api/headers/headers-errors.any.js"); }
#[test] fn wpt_headers_normalize() { init_v8(); run_and_report("fetch/api/headers/headers-normalize.any.js"); }
#[test] fn wpt_headers_record()    { init_v8(); run_and_report("fetch/api/headers/headers-record.any.js"); }
#[test] fn wpt_headers_casing()    { init_v8(); run_and_report("fetch/api/headers/headers-casing.any.js"); }
#[test] fn wpt_headers_structure() { init_v8(); run_and_report("fetch/api/headers/headers-structure.any.js"); }

// ---------------------------------------------------------------------------
// Response
// ---------------------------------------------------------------------------
#[test] fn wpt_response_json()          { init_v8(); run_and_report("fetch/api/response/json.any.js"); }
#[test] fn wpt_response_error()         { init_v8(); run_and_report("fetch/api/response/response-error.any.js"); }
#[test] fn wpt_response_consume_empty() { init_v8(); run_and_report("fetch/api/response/response-consume-empty.any.js"); }
#[test] fn wpt_response_clone()         { init_v8(); run_and_report("fetch/api/response/response-clone.any.js"); }

// ---------------------------------------------------------------------------
// AbortController
// ---------------------------------------------------------------------------
#[test] fn wpt_abort_event()  { init_v8(); run_and_report("dom/abort/event.any.js"); }
#[test] fn wpt_abort_signal() { init_v8(); run_and_report("dom/abort/AbortSignal.any.js"); }
