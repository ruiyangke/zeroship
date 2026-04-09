//! End-to-end test: real npm app → .appbundle → V8 execution

mod common;
use common::*;

use appbase_runtime::bundle::{AppBundle, ModuleType};
use appbase_runtime::{init_v8, ModuleEntry};
use appbase_runtime::runtime::Runtime;
use std::collections::HashMap;

/// Load the esbuild-bundled JS at compile time
const BUNDLED_JS: &str = include_str!("/tmp/test-app/dist/bundled.js");

#[test]
fn bundle_round_trip_real_app() {
    // Create .appbundle from the real bundled JS
    let bundle = AppBundle::new("index.js", vec![
        ("index.js".into(), ModuleType::EsModule, BUNDLED_JS.into()),
    ]);
    
    let bytes = bundle.to_bytes();
    let original_size = BUNDLED_JS.len();
    let bundle_size = bytes.len();
    let ratio = bundle_size as f64 / original_size as f64 * 100.0;
    
    eprintln!("Real npm app (uuid + lodash-es + zod):");
    eprintln!("  Source:  {} bytes ({:.1} KB)", original_size, original_size as f64 / 1024.0);
    eprintln!("  Bundle:  {} bytes ({:.1} KB)", bundle_size, bundle_size as f64 / 1024.0);
    eprintln!("  Ratio:   {:.1}%", ratio);
    
    // Parse back
    let start = std::time::Instant::now();
    let mut loaded = AppBundle::from_bytes(&bytes).unwrap();
    let parse_time = start.elapsed();
    eprintln!("  Parse:   {:?}", parse_time);
    
    // Decompress
    let start = std::time::Instant::now();
    let source = loaded.get_source("index.js").unwrap();
    let decompress_time = start.elapsed();
    eprintln!("  Decompress: {:?}", decompress_time);
    
    assert_eq!(source, BUNDLED_JS);
    assert!(ratio < 50.0, "Should compress to under 50%");
}

#[test]
fn execute_real_app_from_bundle() {
    // Create .appbundle
    let bundle = AppBundle::new("index.js", vec![
        ("index.js".into(), ModuleType::EsModule, BUNDLED_JS.into()),
    ]);
    let bytes = bundle.to_bytes();
    
    // Parse bundle
    let mut loaded = AppBundle::from_bytes(&bytes).unwrap();
    let entries = loaded.to_module_entries();
    
    // Load into V8 and execute
    init_v8();
    let mut runtime = Runtime::new_direct(entries, HashMap::new(), None, None);
    
    // Test ping
    let r = runtime.dispatch_rpc(r#"{"jsonrpc":"2.0","method":"ping","params":[],"id":1}"#).unwrap();
    assert!(r.json.contains("pong"), "ping failed: {}", r.json);
    eprintln!("  ping: OK");
    
    // Test createUser with zod validation
    let r = runtime.dispatch_rpc(
        r#"{"jsonrpc":"2.0","method":"createUser","params":["Alice","alice@example.com",30],"id":2}"#
    ).unwrap();
    eprintln!("  createUser: {}", &r.json[..80.min(r.json.len())]);
    assert!(r.json.contains("Alice"), "createUser failed: {}", r.json);
    assert!(r.json.contains("id"), "createUser should return id: {}", r.json);
    
    // Test validateUser (zod validation — invalid email)
    let r = runtime.dispatch_rpc(
        r#"{"jsonrpc":"2.0","method":"validateUser","params":[{"name":"Bob","email":"not-email","age":25}],"id":3}"#
    ).unwrap();
    eprintln!("  validateUser (invalid): {}", &r.json[..80.min(r.json.len())]);
    assert!(r.json.contains("false") || r.json.contains("error"), "validation should fail: {}", r.json);
    
    // Test listUsers (lodash sortBy)
    let r = runtime.dispatch_rpc(
        r#"{"jsonrpc":"2.0","method":"listUsers","params":[],"id":4}"#
    ).unwrap();
    eprintln!("  listUsers: {}", &r.json[..80.min(r.json.len())]);
    assert!(r.json.contains("Alice"), "listUsers should contain Alice: {}", r.json);
    
    eprintln!("\n  All real-world npm functions working from .appbundle!");
}
