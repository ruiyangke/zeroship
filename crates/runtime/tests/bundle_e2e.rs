//! End-to-end test: real npm app → .appbundle → V8 execution

mod common;

use zeroship_runtime::bundle::{AppBundle, ModuleType};
use zeroship_runtime::init_v8;
use zeroship_runtime::runtime::Runtime;
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
    let mut runtime = Runtime::new_direct(entries.clone(), HashMap::new(), None, None);
    
    // Test ping
    let r = runtime.dispatch_rpc(&entries, r#"{"jsonrpc":"2.0","method":"ping","params":[],"id":1}"#).unwrap();
    assert!(r.json.contains("pong"), "ping failed: {}", r.json);
    eprintln!("  ping: OK");
    
    // Test createUser with zod validation
    let r = runtime.dispatch_rpc(&entries,
        r#"{"jsonrpc":"2.0","method":"createUser","params":["Alice","alice@example.com",30],"id":2}"#
    ).unwrap();
    eprintln!("  createUser: {}", &r.json[..80.min(r.json.len())]);
    assert!(r.json.contains("Alice"), "createUser failed: {}", r.json);
    assert!(r.json.contains("id"), "createUser should return id: {}", r.json);
    
    // Test validateUser (zod validation — invalid email)
    let r = runtime.dispatch_rpc(&entries,
        r#"{"jsonrpc":"2.0","method":"validateUser","params":[{"name":"Bob","email":"not-email","age":25}],"id":3}"#
    ).unwrap();
    eprintln!("  validateUser (invalid): {}", &r.json[..80.min(r.json.len())]);
    assert!(r.json.contains("false") || r.json.contains("error"), "validation should fail: {}", r.json);
    
    // Test listUsers (lodash sortBy)
    let r = runtime.dispatch_rpc(&entries,
        r#"{"jsonrpc":"2.0","method":"listUsers","params":[],"id":4}"#
    ).unwrap();
    eprintln!("  listUsers: {}", &r.json[..80.min(r.json.len())]);
    assert!(r.json.contains("Alice"), "listUsers should contain Alice: {}", r.json);
    
    eprintln!("\n  All real-world npm functions working from .appbundle!");
}

#[test]
fn truncated_bundle() {
    // Simulate interrupted download — file cut at various points
    let bundle = AppBundle::new("index.js", vec![
        ("index.js".into(), ModuleType::EsModule, BUNDLED_JS.into()),
    ]);
    let bytes = bundle.to_bytes();
    
    // Cut at header
    assert!(AppBundle::from_bytes(&bytes[..20]).is_err());
    // Cut at hash
    assert!(AppBundle::from_bytes(&bytes[..40]).is_err());
    // Cut mid-index
    assert!(AppBundle::from_bytes(&bytes[..50]).is_err());
    // Cut mid-data (hash will mismatch)
    assert!(AppBundle::from_bytes(&bytes[..bytes.len() / 2]).is_err());
    // Missing last byte
    assert!(AppBundle::from_bytes(&bytes[..bytes.len() - 1]).is_err());
    eprintln!("  truncation: all 5 cases rejected");
}

#[test]
fn empty_input() {
    assert!(AppBundle::from_bytes(&[]).is_err());
    assert!(AppBundle::from_bytes(&[0u8; 4]).is_err());
    assert!(AppBundle::from_bytes(b"APPB").is_err());
    eprintln!("  empty input: all rejected");
}

#[test]
fn wrong_magic() {
    let bundle = AppBundle::new("x.js", vec![
        ("x.js".into(), ModuleType::EsModule, "export default 1;".into()),
    ]);
    let mut bytes = bundle.to_bytes();
    bytes[0..4].copy_from_slice(b"NOPE");
    assert!(AppBundle::from_bytes(&bytes).is_err());
    eprintln!("  wrong magic: rejected");
}

#[test]
fn future_version() {
    let bundle = AppBundle::new("x.js", vec![
        ("x.js".into(), ModuleType::EsModule, "export default 1;".into()),
    ]);
    let mut bytes = bundle.to_bytes();
    // Set version to 99
    bytes[4..8].copy_from_slice(&99u32.to_le_bytes());
    // Rehash so integrity passes
    let hash: [u8; 32] = sha2::Sha256::digest(&bytes[40..]).into();
    bytes[8..40].copy_from_slice(&hash);
    assert!(AppBundle::from_bytes(&bytes).is_err());
    eprintln!("  future version: rejected");
}

#[test]
fn unicode_specifiers() {
    // Module paths with unicode (some npm packages have this)
    let bundle = AppBundle::new("índex.js", vec![
        ("índex.js".into(), ModuleType::EsModule, "export default '¡hola!';".into()),
        ("日本語.js".into(), ModuleType::EsModule, "export default 'こんにちは';".into()),
        ("émoji🎉.js".into(), ModuleType::EsModule, "export default '🎊';".into()),
    ]);
    let bytes = bundle.to_bytes();
    let mut loaded = AppBundle::from_bytes(&bytes).unwrap();
    
    assert_eq!(loaded.entry(), "índex.js");
    assert_eq!(loaded.get_source("índex.js").unwrap(), "export default '¡hola!';");
    assert_eq!(loaded.get_source("日本語.js").unwrap(), "export default 'こんにちは';");
    assert_eq!(loaded.get_source("émoji🎉.js").unwrap(), "export default '🎊';");
    eprintln!("  unicode specifiers: all round-trip correctly");
}

#[test]
fn large_module_count() {
    // 500 modules — stress the index parsing
    let mut modules = Vec::new();
    let entry_src = (0..500).map(|i| format!("import './m{i}.js';")).collect::<Vec<_>>().join("\n")
        + "\nexport function ping() { return 'pong'; }";
    modules.push(("index.js".into(), ModuleType::EsModule, entry_src));
    
    for i in 0..500 {
        modules.push((
            format!("m{i}.js"),
            ModuleType::EsModule,
            format!("export const val_{i} = {};", i),
        ));
    }
    
    let bundle = AppBundle::new("index.js", modules);
    let bytes = bundle.to_bytes();
    
    let start = std::time::Instant::now();
    let mut loaded = AppBundle::from_bytes(&bytes).unwrap();
    let parse_time = start.elapsed();
    
    assert_eq!(loaded.entry(), "index.js");
    // Spot check some modules
    assert!(loaded.get_source("m0.js").unwrap().contains("val_0"));
    assert!(loaded.get_source("m250.js").unwrap().contains("val_250"));
    assert!(loaded.get_source("m499.js").unwrap().contains("val_499"));
    
    eprintln!("  501 modules: parse {:?}, bundle {} bytes", parse_time, bytes.len());
}

#[test]
fn execute_multi_module_from_bundle() {
    // Multi-module app: entry imports helpers
    let entry = r#"
        import { double } from './math.js';
        import { greet } from './strings.js';
        export function compute(n) { return double(n); }
        export function hello(name) { return greet(name); }
        export function ping() { return "pong"; }
    "#;
    let math = "export function double(n) { return n * 2; }";
    let strings = "export function greet(name) { return 'Hello, ' + name + '!'; }";
    let unused = "export function unused() { THIS_WOULD_FAIL_IF_COMPILED; }";
    
    let bundle = AppBundle::new("index.js", vec![
        ("index.js".into(), ModuleType::EsModule, entry.into()),
        ("math.js".into(), ModuleType::EsModule, math.into()),
        ("strings.js".into(), ModuleType::EsModule, strings.into()),
        ("unused.js".into(), ModuleType::EsModule, unused.into()),
    ]);
    let bytes = bundle.to_bytes();
    
    // Load from bundle
    let mut loaded = AppBundle::from_bytes(&bytes).unwrap();
    let entries = loaded.to_module_entries();
    
    // Execute in V8
    init_v8();
    let mut runtime = Runtime::new_direct(entries.clone(), HashMap::new(), None, None);
    
    let r = runtime.dispatch_rpc(&entries, r#"{"jsonrpc":"2.0","method":"ping","params":[],"id":1}"#).unwrap();
    assert!(r.json.contains("pong"));
    
    let r = runtime.dispatch_rpc(&entries, r#"{"jsonrpc":"2.0","method":"compute","params":[21],"id":2}"#).unwrap();
    assert!(r.json.contains("42"), "compute(21) should be 42: {}", r.json);
    
    let r = runtime.dispatch_rpc(&entries, r#"{"jsonrpc":"2.0","method":"hello","params":["World"],"id":3}"#).unwrap();
    assert!(r.json.contains("Hello, World!"), "hello should greet: {}", r.json);
    
    eprintln!("  multi-module from bundle: all functions working");
}

use sha2::Digest;
