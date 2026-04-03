//! Integration tests that run real example apps against the runtime.
//! Each test loads an example from examples/ and exercises its API.

use appbase_isolate_v8::{init_v8, Isolate, ModuleEntry};

fn load_example(name: &str) -> Isolate {
    let path = format!(
        "{}/../../examples/{name}",
        env!("CARGO_MANIFEST_DIR")
    );
    let source = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("Failed to read {path}: {e}"));
    let modules = vec![ModuleEntry {
        specifier: "index.js".to_string(),
        source,
    }];
    Isolate::new(modules, std::collections::HashMap::new())
}

fn rpc(isolate: &mut Isolate, method: &str, params: &str) -> serde_json::Value {
    let body = format!(
        r#"{{"jsonrpc":"2.0","method":"{method}","params":{params},"id":1}}"#
    );
    let result = isolate.execute_request(&body).unwrap();
    serde_json::from_str(&result.json).unwrap()
}

// =========================================================================
// Todo API
// =========================================================================

#[test]
fn todo_api_crud() {
    init_v8();
    let mut iso = load_example("todo-api.js");

    // Add todos
    let r = rpc(&mut iso, "add", r#"["Buy groceries"]"#);
    assert_eq!(r["result"]["title"], "Buy groceries");
    assert_eq!(r["result"]["done"], false);
    let id1 = r["result"]["id"].as_i64().unwrap();

    let r = rpc(&mut iso, "add", r#"["Walk the dog"]"#);
    let id2 = r["result"]["id"].as_i64().unwrap();
    assert_ne!(id1, id2);

    // List
    let r = rpc(&mut iso, "list", "[]");
    assert_eq!(r["result"].as_array().unwrap().len(), 2);

    // Get
    let r = rpc(&mut iso, "get", &format!("[{id1}]"));
    assert_eq!(r["result"]["title"], "Buy groceries");

    // Toggle
    let r = rpc(&mut iso, "toggle", &format!("[{id1}]"));
    assert_eq!(r["result"]["done"], true);

    // Remove
    let r = rpc(&mut iso, "remove", &format!("[{id1}]"));
    assert_eq!(r["result"], true);

    let r = rpc(&mut iso, "list", "[]");
    assert_eq!(r["result"].as_array().unwrap().len(), 1);

    // Clear
    let r = rpc(&mut iso, "clear", "[]");
    assert!(r["result"]["cleared"].as_i64().unwrap() >= 1);

    let r = rpc(&mut iso, "list", "[]");
    assert_eq!(r["result"].as_array().unwrap().len(), 0);
}

#[test]
fn todo_api_persistence_across_requests() {
    init_v8();
    let mut iso = load_example("todo-api.js");

    rpc(&mut iso, "add", r#"["Item 1"]"#);
    rpc(&mut iso, "add", r#"["Item 2"]"#);

    // Items persist across separate RPC calls
    let r = rpc(&mut iso, "list", "[]");
    assert_eq!(r["result"].as_array().unwrap().len(), 2);

    // IDs auto-increment
    let r = rpc(&mut iso, "add", r#"["Item 3"]"#);
    assert_eq!(r["result"]["id"], 3);
}

// =========================================================================
// URL Shortener
// =========================================================================

#[test]
fn url_shortener_create_and_resolve() {
    init_v8();
    let mut iso = load_example("url-shortener.js");

    // Shorten
    let r = rpc(&mut iso, "shorten", r#"["https://example.com"]"#);
    assert!(r["result"]["code"].is_string());
    let code = r["result"]["code"].as_str().unwrap();
    assert_eq!(code.len(), 8);

    // Resolve
    let r = rpc(&mut iso, "resolve", &format!(r#"["{code}"]"#));
    assert_eq!(r["result"]["url"], "https://example.com");
    assert_eq!(r["result"]["clicks"], 1);

    // Resolve again — clicks increment
    let r = rpc(&mut iso, "resolve", &format!(r#"["{code}"]"#));
    assert_eq!(r["result"]["clicks"], 2);

    // Stats
    let r = rpc(&mut iso, "stats", &format!(r#"["{code}"]"#));
    assert_eq!(r["result"]["clicks"], 2);

    // List
    let r = rpc(&mut iso, "list", "[]");
    assert_eq!(r["result"].as_array().unwrap().len(), 1);
}

#[test]
fn url_shortener_invalid_url() {
    init_v8();
    let mut iso = load_example("url-shortener.js");

    let r = rpc(&mut iso, "shorten", r#"["not-a-url"]"#);
    assert!(r["error"].is_object());
}

#[test]
fn url_shortener_missing_code() {
    init_v8();
    let mut iso = load_example("url-shortener.js");

    let r = rpc(&mut iso, "resolve", r#"["nonexistent"]"#);
    assert!(r["result"].is_null());
}

// =========================================================================
// Weather Proxy (requires network)
// =========================================================================

#[test]
fn weather_proxy_current() {
    init_v8();
    let mut iso = load_example("weather-proxy.js");

    let r = rpc(&mut iso, "current", r#"["London"]"#);
    // Should have weather data
    assert!(r["result"]["city"].as_str().is_some());
    assert!(r["result"]["temp_c"].is_string());
}

#[test]
fn weather_proxy_missing_city() {
    init_v8();
    let mut iso = load_example("weather-proxy.js");

    let r = rpc(&mut iso, "current", "[]");
    assert!(r["error"].is_object());
    assert!(r["error"]["message"].as_str().unwrap().contains("required"));
}

// =========================================================================
// HTTP Handler
// =========================================================================

#[test]
fn http_handler_routes() {
    init_v8();
    let mut iso = load_example("http-handler.js");

    // Root
    let r = iso.execute_http("GET", "http://localhost/", "[]", "").unwrap().unwrap();
    assert_eq!(r.status, 200);
    assert!(r.body.contains("Welcome"));

    // Health
    let r = iso.execute_http("GET", "http://localhost/health", "[]", "").unwrap().unwrap();
    assert_eq!(r.status, 200);
    assert_eq!(r.body, "OK");

    // Echo
    let r = iso.execute_http("GET", "http://localhost/echo/hello%20world", "[]", "").unwrap().unwrap();
    assert_eq!(r.status, 200);
    assert_eq!(r.body, "hello world");

    // 404
    let r = iso.execute_http("GET", "http://localhost/nonexistent", "[]", "").unwrap().unwrap();
    assert_eq!(r.status, 404);
}

#[test]
fn http_handler_coexists_with_rpc() {
    init_v8();
    let mut iso = load_example("http-handler.js");

    // RPC works
    let r = rpc(&mut iso, "ping", "[]");
    assert_eq!(r["result"], "pong");

    let r = rpc(&mut iso, "add", "[10, 20]");
    assert_eq!(r["result"], 30);

    // HTTP also works
    let r = iso.execute_http("GET", "http://localhost/health", "[]", "").unwrap().unwrap();
    assert_eq!(r.status, 200);
}

// =========================================================================
// Multi-module (User CRUD)
// =========================================================================

#[test]
fn multi_module_user_crud() {
    init_v8();
    let mut iso = load_example("multi-module.js");

    // Create users
    let r = rpc(&mut iso, "createUser", r#"["Alice", "alice@example.com"]"#);
    assert_eq!(r["result"]["name"], "Alice");
    let id = r["result"]["id"].as_i64().unwrap();

    let r = rpc(&mut iso, "createUser", r#"["Bob", "bob@example.com"]"#);
    assert_ne!(r["result"]["id"].as_i64().unwrap(), id);

    // Get
    let r = rpc(&mut iso, "getUser", &format!("[{id}]"));
    assert_eq!(r["result"]["name"], "Alice");

    // List
    let r = rpc(&mut iso, "listUsers", "[]");
    assert_eq!(r["result"].as_array().unwrap().len(), 2);

    // Count
    let r = rpc(&mut iso, "userCount", "[]");
    assert_eq!(r["result"], 2);

    // Delete
    let r = rpc(&mut iso, "deleteUser", &format!("[{id}]"));
    assert_eq!(r["result"], true);

    let r = rpc(&mut iso, "userCount", "[]");
    assert_eq!(r["result"], 1);
}

#[test]
fn multi_module_validation() {
    init_v8();
    let mut iso = load_example("multi-module.js");

    // Invalid name
    let r = rpc(&mut iso, "createUser", r#"["X", "x@x.com"]"#);
    assert!(r["error"].is_object());

    // Invalid email
    let r = rpc(&mut iso, "createUser", r#"["Alice", "not-an-email"]"#);
    assert!(r["error"].is_object());
}

// =========================================================================
// JWT Validator (Web Crypto)
// =========================================================================

#[test]
fn jwt_sign_and_verify() {
    init_v8();
    let mut isolate = load_example("jwt-validator.js");

    // Sign a JWT
    let result = rpc(&mut isolate, "sign", r#"["{\"sub\":\"1234\",\"name\":\"Alice\"}", "my-secret-key"]"#);
    let token = result["result"].as_str().expect("should return token string");
    assert!(token.contains('.'), "JWT should have dots: {token}");
    let parts: Vec<&str> = token.split('.').collect();
    assert_eq!(parts.len(), 3, "JWT should have 3 parts");

    // Verify the JWT
    let result = rpc(&mut isolate, "verify", &format!(r#"["{token}", "my-secret-key"]"#));
    let payload = &result["result"];
    assert_eq!(payload["sub"].as_str().unwrap(), "1234");
    assert_eq!(payload["name"].as_str().unwrap(), "Alice");
}

#[test]
fn jwt_verify_wrong_key_fails() {
    init_v8();
    let mut isolate = load_example("jwt-validator.js");

    let result = rpc(&mut isolate, "sign", r#"["{\"data\":\"test\"}", "correct-key"]"#);
    let token = result["result"].as_str().unwrap();

    // Verify with wrong key should fail (returns error)
    let body = format!(
        r#"{{"jsonrpc":"2.0","method":"verify","params":["{token}", "wrong-key"],"id":1}}"#
    );
    let r = isolate.execute_request(&body).unwrap();
    assert!(r.json.contains("error") || r.json.contains("Invalid signature"), "wrong key should fail: {}", r.json);
}

#[test]
fn jwt_content_hash() {
    init_v8();
    let mut isolate = load_example("jwt-validator.js");

    let result = rpc(&mut isolate, "hash", r#"["hello"]"#);
    let hex = result["result"].as_str().unwrap();
    // SHA-256("hello") = 2cf24dba...
    assert!(hex.starts_with("2cf24dba"), "SHA-256 mismatch: {hex}");
}
