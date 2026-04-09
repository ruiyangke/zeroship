#![allow(unsafe_code)]

pub mod channel;
pub mod runtime;
pub mod fetch;

// Re-export v8-core for convenience
pub use appbase_v8_core as v8_core;
pub use appbase_v8_core::state;
pub use appbase_v8_core::init;
pub use appbase_v8_core::modules;

pub use appbase_v8_core::{init_v8, RequestResult, ModuleEntry, OpResult, FetchRequest};
pub use runtime::{Runtime, AsyncWork, AsyncEvent, DispatchOutcome, HttpDispatchResult};

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    /// Helper to create a module entry for tests (single-module shorthand).
    fn m(source: &str) -> Vec<ModuleEntry> {
        vec![ModuleEntry {
            specifier: "index.js".into(),
            source: source.into(),
        }]
    }

    /// Shorthand: empty env vars for tests.
    fn no_env() -> HashMap<String, String> {
        HashMap::new()
    }

    /// Helper: create a compio Runtime and dispatch a single RPC request.
    fn dispatch(modules: Vec<ModuleEntry>, json: &str) -> Result<RequestResult, String> {
        init_v8();
        let mut runtime = Runtime::new_direct(modules, no_env(), None, None);
        runtime.dispatch_rpc(json)
    }

    /// Helper: create a Runtime, dispatch multiple requests sequentially.
    fn dispatch_multi(modules: Vec<ModuleEntry>, requests: &[&str]) -> Vec<Result<RequestResult, String>> {
        init_v8();
        let mut runtime = Runtime::new_direct(modules, no_env(), None, None);
        requests.iter().map(|json| runtime.dispatch_rpc(json)).collect()
    }

    /// Helper: create a Runtime with env vars and dispatch a single RPC request.
    fn dispatch_with_env(modules: Vec<ModuleEntry>, env: HashMap<String, String>, json: &str) -> Result<RequestResult, String> {
        init_v8();
        let mut runtime = Runtime::new_direct(modules, env, None, None);
        runtime.dispatch_rpc(json)
    }

    /// Helper: dispatch an HTTP request and extract the body from a sync HttpComplete outcome.
    fn dispatch_http_sync(
        modules: Vec<ModuleEntry>,
        method: &str,
        url: &str,
        headers_json: &str,
        body: &str,
    ) -> Option<(u16, Vec<(String, String)>, String)> {
        init_v8();
        let mut runtime = Runtime::new_direct(modules, no_env(), None, None);
        match runtime.dispatch_http(method, url, headers_json, body) {
            DispatchOutcome::HttpComplete { status, headers, body, .. } => {
                Some((status, headers, body))
            }
            DispatchOutcome::Complete(Err(e)) if e.contains("No onRequest") => None,
            _ => panic!("unexpected dispatch_http outcome"),
        }
    }

    // =======================================================================
    // Core RPC tests
    // =======================================================================

    #[test]
    fn basic_rpc() {
        let r = dispatch(m(r#"export function ping() { return "pong"; }"#),
            r#"{"jsonrpc":"2.0","method":"ping","params":[],"id":1}"#).unwrap();
        assert!(r.json.contains("pong"));
    }

    #[test]
    fn persistent_context() {
        let results = dispatch_multi(
            m(r#"
                let n = 0;
                export function count() { return ++n; }
            "#),
            &[
                r#"{"jsonrpc":"2.0","method":"count","params":[],"id":1}"#,
                r#"{"jsonrpc":"2.0","method":"count","params":[],"id":2}"#,
                r#"{"jsonrpc":"2.0","method":"count","params":[],"id":3}"#,
            ],
        );
        assert!(results[0].as_ref().unwrap().json.contains("\"result\":1"));
        assert!(results[1].as_ref().unwrap().json.contains("\"result\":2"));
        assert!(results[2].as_ref().unwrap().json.contains("\"result\":3"));
    }

    #[test]
    fn per_request_cpu() {
        init_v8();
        let mut runtime = Runtime::new_direct(m(r#"
            export function fib(n) {
                function f(n) { return n <= 1 ? n : f(n-1) + f(n-2); }
                return f(n);
            }
        "#), no_env(), None, None);
        let r1 = runtime.dispatch_rpc(r#"{"jsonrpc":"2.0","method":"fib","params":[20],"id":1}"#).unwrap();
        let r2 = runtime.dispatch_rpc(r#"{"jsonrpc":"2.0","method":"fib","params":[35],"id":2}"#).unwrap();
        assert!(r2.cpu_time > r1.cpu_time * 5);
    }

    #[test]
    fn sync_still_works_with_event_loop() {
        let r = dispatch(m(r#"export function add(a, b) { return a + b; }"#),
            r#"{"jsonrpc":"2.0","method":"add","params":[3,4],"id":1}"#).unwrap();
        assert!(r.json.contains("\"result\":7"));
    }

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

    // =======================================================================
    // Async tests (setTimeout with 0 delay — settles inline via fire_ready_timers)
    // =======================================================================

    #[test]
    fn set_timeout_zero_delay() {
        let r = dispatch(m(r#"
            export function immediate() {
                return new Promise(function(resolve) {
                    setTimeout(function() { resolve("immediate"); }, 0);
                });
            }
        "#), r#"{"jsonrpc":"2.0","method":"immediate","params":[],"id":1}"#).unwrap();
        assert!(r.json.contains("immediate"));
    }

    #[test]
    fn promise_resolve_sync() {
        let r = dispatch(m(r#"
            export async function test() {
                return "sync-async";
            }
        "#), r#"{"jsonrpc":"2.0","method":"test","params":[],"id":1}"#).unwrap();
        assert!(r.json.contains("sync-async"));
    }

    #[test]
    fn promise_then_chain_sync() {
        let r = dispatch(m(r#"
            export function test() {
                return Promise.resolve(1).then(v => v + 10).then(v => v * 2);
            }
        "#), r#"{"jsonrpc":"2.0","method":"test","params":[],"id":1}"#).unwrap();
        assert!(r.json.contains("\"result\":22"));
    }

    // =======================================================================
    // KV store tests
    // =======================================================================

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

    // =======================================================================
    // Env tests
    // =======================================================================

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

    // =======================================================================
    // TextEncoder / TextDecoder
    // =======================================================================

    #[test]
    fn text_encoder_decoder() {
        let r = dispatch(m(r#"export function test() {
            var enc = new TextEncoder();
            var buf = enc.encode("Hello");
            var dec = new TextDecoder();
            return { encoded: Array.from(buf), decoded: dec.decode(buf) };
        }"#), r#"{"jsonrpc":"2.0","method":"test","params":[],"id":1}"#).unwrap();
        assert!(r.json.contains("Hello"), "got: {}", r.json);
        assert!(r.json.contains("[72,101,108,108,111]"), "got: {}", r.json);
    }

    // =======================================================================
    // structuredClone, btoa/atob
    // =======================================================================

    #[test]
    fn structured_clone() {
        let r = dispatch(m(r#"export function test() {
            var obj = { a: 1, b: [2, 3], c: { d: "hello" } };
            var clone = structuredClone(obj);
            clone.a = 99;
            clone.b.push(4);
            return { original: obj.a, cloned: clone.a, origLen: obj.b.length, cloneLen: clone.b.length };
        }"#), r#"{"jsonrpc":"2.0","method":"test","params":[],"id":1}"#).unwrap();
        assert!(r.json.contains("\"original\":1"), "got: {}", r.json);
        assert!(r.json.contains("\"cloned\":99"), "got: {}", r.json);
        assert!(r.json.contains("\"origLen\":2"), "got: {}", r.json);
        assert!(r.json.contains("\"cloneLen\":3"), "got: {}", r.json);
    }

    #[test]
    fn btoa_atob() {
        let r = dispatch(m(r#"export function test() {
            var encoded = btoa("Hello, World!");
            var decoded = atob(encoded);
            return { encoded, decoded };
        }"#), r#"{"jsonrpc":"2.0","method":"test","params":[],"id":1}"#).unwrap();
        assert!(r.json.contains("SGVsbG8sIFdvcmxkIQ=="), "got: {}", r.json);
        assert!(r.json.contains("Hello, World!"), "got: {}", r.json);
    }

    // =======================================================================
    // Headers, Request, Response classes
    // =======================================================================

    #[test]
    fn headers_class_works() {
        let r = dispatch(m(r#"
            export function test() {
                var h = new Headers({ "Content-Type": "text/plain", "X-Custom": "hello" });
                h.append("X-Custom", "world");
                return {
                    ct: h.get("content-type"),
                    custom: h.get("x-custom"),
                    has_ct: h.has("content-type"),
                    missing: h.has("nonexistent"),
                };
            }
        "#), r#"{"jsonrpc":"2.0","method":"test","params":[],"id":1}"#).unwrap();
        assert!(r.json.contains("text/plain"), "got: {}", r.json);
        assert!(r.json.contains("hello, world"), "got: {}", r.json);
    }

    #[test]
    fn request_class_works() {
        let r = dispatch(m(r#"
            export function test() {
                var req = new Request("https://example.com", {
                    method: "POST",
                    headers: { "X-Test": "1" },
                    body: "hello",
                });
                return {
                    url: req.url,
                    method: req.method,
                    header: req.headers.get("x-test"),
                };
            }
        "#), r#"{"jsonrpc":"2.0","method":"test","params":[],"id":1}"#).unwrap();
        assert!(r.json.contains("example.com"), "got: {}", r.json);
        assert!(r.json.contains("POST"), "got: {}", r.json);
    }

    #[test]
    fn response_static_json() {
        // Response.json() creates a sync Response — no async runtime needed
        let r = dispatch(m(r#"
            export async function test() {
                var resp = Response.json({ hello: "world" });
                var data = await resp.json();
                return { status: resp.status, hello: data.hello, ct: resp.headers.get("content-type") };
            }
        "#), r#"{"jsonrpc":"2.0","method":"test","params":[],"id":1}"#).unwrap();
        assert!(r.json.contains("world"), "got: {}", r.json);
        assert!(r.json.contains("application/json"), "got: {}", r.json);
    }

    // =======================================================================
    // ESM module tests
    // =======================================================================

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
        let mut runtime = Runtime::new_direct(modules, no_env(), None, None);
        let r = runtime.dispatch_rpc(r#"{"jsonrpc":"2.0","method":"ping","params":[],"id":1}"#).unwrap();
        assert!(r.json.contains("pong"), "got: {}", r.json);

        let r2 = runtime.dispatch_rpc(r#"{"jsonrpc":"2.0","method":"add","params":[3,4],"id":2}"#).unwrap();
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
        let mut runtime = Runtime::new_direct(modules, no_env(), None, None);
        let r = runtime.dispatch_rpc(r#"{"jsonrpc":"2.0","method":"compute","params":[3,4],"id":1}"#).unwrap();
        assert!(r.json.contains("7"), "got: {}", r.json);
    }

    #[test]
    fn esm_with_kv() {
        init_v8();
        let modules = vec![ModuleEntry {
            specifier: "index.js".into(),
            source: r#"
                export function set(k, v) { kv.set(k, v); return "ok"; }
                export function get(k) { return kv.get(k); }
            "#.into(),
        }];
        let mut runtime = Runtime::new_direct(modules, no_env(), None, None);
        runtime.dispatch_rpc(r#"{"jsonrpc":"2.0","method":"set","params":["x","1"],"id":1}"#).unwrap();
        let r = runtime.dispatch_rpc(r#"{"jsonrpc":"2.0","method":"get","params":["x"],"id":2}"#).unwrap();
        assert!(r.json.contains("1"), "got: {}", r.json);
    }

    // =======================================================================
    // URL / URLSearchParams tests
    // =======================================================================

    #[test]
    fn url_parse_basic() {
        let r = dispatch(m(r#"
            export function test() {
                const url = new URL("https://example.com:8080/path?q=1#frag");
                return {
                    protocol: url.protocol,
                    hostname: url.hostname,
                    port: url.port,
                    pathname: url.pathname,
                    search: url.search,
                    hash: url.hash,
                    origin: url.origin,
                    host: url.host,
                };
            }
        "#), r#"{"jsonrpc":"2.0","method":"test","params":[],"id":1}"#).unwrap();
        assert!(r.json.contains("\"protocol\":\"https:\""), "got: {}", r.json);
        assert!(r.json.contains("\"hostname\":\"example.com\""), "got: {}", r.json);
        assert!(r.json.contains("\"port\":\"8080\""), "got: {}", r.json);
        assert!(r.json.contains("\"pathname\":\"/path\""), "got: {}", r.json);
        assert!(r.json.contains("\"hash\":\"#frag\""), "got: {}", r.json);
    }

    #[test]
    fn url_with_base() {
        let r = dispatch(m(r#"
            export function test() {
                const url = new URL("/api/users", "https://example.com");
                return url.href;
            }
        "#), r#"{"jsonrpc":"2.0","method":"test","params":[],"id":1}"#).unwrap();
        assert!(r.json.contains("https://example.com/api/users"), "got: {}", r.json);
    }

    #[test]
    fn url_invalid_throws() {
        let r = dispatch(m(r#"
            export function test() {
                try { new URL("not a url"); return "should have thrown"; }
                catch (e) { return "caught: " + e.message; }
            }
        "#), r#"{"jsonrpc":"2.0","method":"test","params":[],"id":1}"#).unwrap();
        assert!(r.json.contains("caught:"), "got: {}", r.json);
    }

    #[test]
    fn url_can_parse() {
        let r = dispatch(m(r#"
            export function test() {
                return {
                    valid: URL.canParse("https://example.com"),
                    invalid: URL.canParse("not a url"),
                    relative: URL.canParse("/path", "https://example.com"),
                };
            }
        "#), r#"{"jsonrpc":"2.0","method":"test","params":[],"id":1}"#).unwrap();
        assert!(r.json.contains("\"valid\":true"), "got: {}", r.json);
        assert!(r.json.contains("\"invalid\":false"), "got: {}", r.json);
        assert!(r.json.contains("\"relative\":true"), "got: {}", r.json);
    }

    #[test]
    fn url_search_params() {
        let r = dispatch(m(r#"
            export function test() {
                const url = new URL("https://example.com/search?q=hello&lang=en");
                const p = url.searchParams;
                return {
                    q: p.get("q"),
                    lang: p.get("lang"),
                    missing: p.get("x"),
                    has_q: p.has("q"),
                };
            }
        "#), r#"{"jsonrpc":"2.0","method":"test","params":[],"id":1}"#).unwrap();
        assert!(r.json.contains("\"q\":\"hello\""), "got: {}", r.json);
        assert!(r.json.contains("\"lang\":\"en\""), "got: {}", r.json);
        assert!(r.json.contains("\"missing\":null"), "got: {}", r.json);
        assert!(r.json.contains("\"has_q\":true"), "got: {}", r.json);
    }

    // =======================================================================
    // Web Crypto: randomUUID, getRandomValues, subtle.digest
    // =======================================================================

    #[test]
    fn crypto_random_uuid() {
        let r = dispatch(m(r#"export function test() {
            var id1 = crypto.randomUUID();
            var id2 = crypto.randomUUID();
            return { id1, id2, different: id1 !== id2, format: /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/.test(id1) };
        }"#), r#"{"jsonrpc":"2.0","method":"test","params":[],"id":1}"#).unwrap();
        assert!(r.json.contains("\"different\":true"), "got: {}", r.json);
        assert!(r.json.contains("\"format\":true"), "got: {}", r.json);
    }

    #[test]
    fn crypto_get_random_values() {
        let r = dispatch(m(r#"
            export function test() {
                var buf = new Uint8Array(16);
                crypto.getRandomValues(buf);
                var nonzero = 0;
                for (var i = 0; i < buf.length; i++) { if (buf[i] !== 0) nonzero++; }
                return nonzero > 0 ? "ok" : "all_zeros";
            }
        "#), r#"{"jsonrpc":"2.0","method":"test","params":[],"id":1}"#).unwrap();
        assert!(r.json.contains("ok"), "got: {}", r.json);
    }

    #[test]
    fn crypto_subtle_digest_sha256() {
        let r = dispatch(m(r#"
            export async function test() {
                var data = new TextEncoder().encode("hello");
                var hash = await crypto.subtle.digest("SHA-256", data);
                var bytes = new Uint8Array(hash);
                var hex = "";
                for (var i = 0; i < bytes.length; i++) hex += ("0" + bytes[i].toString(16)).slice(-2);
                return hex;
            }
        "#), r#"{"jsonrpc":"2.0","method":"test","params":[],"id":1}"#).unwrap();
        assert!(r.json.contains("2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"), "got: {}", r.json);
    }

    #[test]
    fn crypto_subtle_digest_sha512() {
        let r = dispatch(m(r#"
            export async function test() {
                var data = new TextEncoder().encode("hello");
                var hash = await crypto.subtle.digest("SHA-512", data);
                return new Uint8Array(hash).length;
            }
        "#), r#"{"jsonrpc":"2.0","method":"test","params":[],"id":1}"#).unwrap();
        assert!(r.json.contains("64"), "SHA-512 should produce 64 bytes, got: {}", r.json);
    }

    // =======================================================================
    // Web Crypto: importKey, exportKey, generateKey
    // =======================================================================

    #[test]
    fn crypto_import_export_hmac_raw() {
        let r = dispatch(m(r#"
            export async function test() {
                var raw = new Uint8Array([1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16]);
                var key = await crypto.subtle.importKey("raw", raw, {name: "HMAC", hash: "SHA-256"}, true, ["sign", "verify"]);
                if (key.type !== "secret") return "wrong type: " + key.type;
                if (!key.extractable) return "not extractable";
                var exported = await crypto.subtle.exportKey("raw", key);
                var arr = new Uint8Array(exported);
                if (arr.length !== 16) return "wrong length: " + arr.length;
                for (var i = 0; i < 16; i++) { if (arr[i] !== i+1) return "mismatch at " + i; }
                return "ok";
            }
        "#), r#"{"jsonrpc":"2.0","method":"test","params":[],"id":1}"#).unwrap();
        assert!(r.json.contains("ok"), "got: {}", r.json);
    }

    #[test]
    fn crypto_generate_hmac_key() {
        let r = dispatch(m(r#"
            export async function test() {
                var key = await crypto.subtle.generateKey({name: "HMAC", hash: "SHA-256"}, true, ["sign", "verify"]);
                if (key.type !== "secret") return "wrong type";
                var exported = await crypto.subtle.exportKey("raw", key);
                if (new Uint8Array(exported).length !== 32) return "wrong length";
                return "ok";
            }
        "#), r#"{"jsonrpc":"2.0","method":"test","params":[],"id":1}"#).unwrap();
        assert!(r.json.contains("ok"), "got: {}", r.json);
    }

    #[test]
    fn crypto_generate_ecdsa_keypair() {
        let r = dispatch(m(r#"
            export async function test() {
                var kp = await crypto.subtle.generateKey({name: "ECDSA", namedCurve: "P-256"}, true, ["sign", "verify"]);
                if (!kp.publicKey || !kp.privateKey) return "no keypair";
                if (kp.publicKey.type !== "public") return "wrong pub type";
                if (kp.privateKey.type !== "private") return "wrong priv type";
                return "ok";
            }
        "#), r#"{"jsonrpc":"2.0","method":"test","params":[],"id":1}"#).unwrap();
        assert!(r.json.contains("ok"), "got: {}", r.json);
    }

    #[test]
    fn crypto_generate_aes_key() {
        let r = dispatch(m(r#"
            export async function test() {
                var key = await crypto.subtle.generateKey({name: "AES-GCM", length: 256}, true, ["encrypt", "decrypt"]);
                if (key.type !== "secret") return "wrong type";
                var raw = await crypto.subtle.exportKey("raw", key);
                if (new Uint8Array(raw).length !== 32) return "wrong length";
                return "ok";
            }
        "#), r#"{"jsonrpc":"2.0","method":"test","params":[],"id":1}"#).unwrap();
        assert!(r.json.contains("ok"), "got: {}", r.json);
    }

    // =======================================================================
    // Web Crypto: sign + verify
    // =======================================================================

    #[test]
    fn crypto_hmac_sign_verify() {
        let r = dispatch(m(r#"
            export async function test() {
                var raw = new TextEncoder().encode("my-secret-key-for-hmac-test!!");
                var key = await crypto.subtle.importKey("raw", raw, {name: "HMAC", hash: "SHA-256"}, false, ["sign", "verify"]);
                var data = new TextEncoder().encode("hello world");
                var sig = await crypto.subtle.sign("HMAC", key, data);
                var valid = await crypto.subtle.verify("HMAC", key, sig, data);
                if (!valid) return "verify failed";
                var bad = await crypto.subtle.verify("HMAC", key, sig, new TextEncoder().encode("tampered"));
                if (bad) return "tampered verify should fail";
                return "ok";
            }
        "#), r#"{"jsonrpc":"2.0","method":"test","params":[],"id":1}"#).unwrap();
        assert!(r.json.contains("ok"), "got: {}", r.json);
    }

    #[test]
    fn crypto_ecdsa_sign_verify() {
        let r = dispatch(m(r#"
            export async function test() {
                var kp = await crypto.subtle.generateKey({name: "ECDSA", namedCurve: "P-256"}, false, ["sign", "verify"]);
                var data = new TextEncoder().encode("test message");
                var sig = await crypto.subtle.sign({name: "ECDSA", hash: "SHA-256"}, kp.privateKey, data);
                var valid = await crypto.subtle.verify({name: "ECDSA", hash: "SHA-256"}, kp.publicKey, sig, data);
                if (!valid) return "verify failed";
                return "ok";
            }
        "#), r#"{"jsonrpc":"2.0","method":"test","params":[],"id":1}"#).unwrap();
        assert!(r.json.contains("ok"), "got: {}", r.json);
    }

    #[test]
    fn crypto_ed25519_sign_verify() {
        let r = dispatch(m(r#"
            export async function test() {
                var kp = await crypto.subtle.generateKey({name: "Ed25519"}, false, ["sign", "verify"]);
                var data = new TextEncoder().encode("ed25519 test");
                var sig = await crypto.subtle.sign("Ed25519", kp.privateKey, data);
                var valid = await crypto.subtle.verify("Ed25519", kp.publicKey, sig, data);
                if (!valid) return "verify failed";
                return "ok";
            }
        "#), r#"{"jsonrpc":"2.0","method":"test","params":[],"id":1}"#).unwrap();
        assert!(r.json.contains("ok"), "got: {}", r.json);
    }

    // =======================================================================
    // Web Crypto: encrypt + decrypt
    // =======================================================================

    #[test]
    fn crypto_aes_gcm_encrypt_decrypt() {
        let r = dispatch(m(r#"
            export async function test() {
                var key = await crypto.subtle.generateKey({name: "AES-GCM", length: 256}, true, ["encrypt", "decrypt"]);
                var iv = new Uint8Array(12);
                crypto.getRandomValues(iv);
                var data = new TextEncoder().encode("secret message");
                var encrypted = await crypto.subtle.encrypt({name: "AES-GCM", iv: iv}, key, data);
                var decrypted = await crypto.subtle.decrypt({name: "AES-GCM", iv: iv}, key, encrypted);
                var text = new TextDecoder().decode(new Uint8Array(decrypted));
                return text === "secret message" ? "ok" : "mismatch: " + text;
            }
        "#), r#"{"jsonrpc":"2.0","method":"test","params":[],"id":1}"#).unwrap();
        assert!(r.json.contains("ok"), "got: {}", r.json);
    }

    #[test]
    fn crypto_aes_cbc_encrypt_decrypt() {
        let r = dispatch(m(r#"
            export async function test() {
                var key = await crypto.subtle.generateKey({name: "AES-CBC", length: 256}, true, ["encrypt", "decrypt"]);
                var iv = new Uint8Array(16);
                crypto.getRandomValues(iv);
                var data = new TextEncoder().encode("hello cbc");
                var encrypted = await crypto.subtle.encrypt({name: "AES-CBC", iv: iv}, key, data);
                var decrypted = await crypto.subtle.decrypt({name: "AES-CBC", iv: iv}, key, encrypted);
                var text = new TextDecoder().decode(new Uint8Array(decrypted));
                return text === "hello cbc" ? "ok" : "mismatch: " + text;
            }
        "#), r#"{"jsonrpc":"2.0","method":"test","params":[],"id":1}"#).unwrap();
        assert!(r.json.contains("ok"), "got: {}", r.json);
    }

    // =======================================================================
    // Web Crypto: deriveBits + deriveKey
    // =======================================================================

    #[test]
    fn crypto_hkdf_derive_bits() {
        let r = dispatch(m(r#"
            export async function test() {
                var keyMaterial = new TextEncoder().encode("input-key-material");
                var key = await crypto.subtle.importKey("raw", keyMaterial, "HKDF", false, ["deriveBits"]);
                var salt = new TextEncoder().encode("salt-value");
                var info = new TextEncoder().encode("info-value");
                var bits = await crypto.subtle.deriveBits(
                    {name: "HKDF", hash: "SHA-256", salt: salt, info: info},
                    key, 256
                );
                if (new Uint8Array(bits).length !== 32) return "wrong length";
                return "ok";
            }
        "#), r#"{"jsonrpc":"2.0","method":"test","params":[],"id":1}"#).unwrap();
        assert!(r.json.contains("ok"), "got: {}", r.json);
    }

    #[test]
    fn crypto_pbkdf2_derive_bits() {
        let r = dispatch(m(r#"
            export async function test() {
                var password = new TextEncoder().encode("password");
                var key = await crypto.subtle.importKey("raw", password, "PBKDF2", false, ["deriveBits"]);
                var salt = new TextEncoder().encode("salt");
                var bits = await crypto.subtle.deriveBits(
                    {name: "PBKDF2", hash: "SHA-256", salt: salt, iterations: 100000},
                    key, 256
                );
                if (new Uint8Array(bits).length !== 32) return "wrong length";
                return "ok";
            }
        "#), r#"{"jsonrpc":"2.0","method":"test","params":[],"id":1}"#).unwrap();
        assert!(r.json.contains("ok"), "got: {}", r.json);
    }

    #[test]
    fn crypto_derive_key_hkdf_to_aes() {
        let r = dispatch(m(r#"
            export async function test() {
                var keyMaterial = new TextEncoder().encode("master-secret");
                var baseKey = await crypto.subtle.importKey("raw", keyMaterial, "HKDF", false, ["deriveKey"]);
                var aesKey = await crypto.subtle.deriveKey(
                    {name: "HKDF", hash: "SHA-256", salt: new Uint8Array(16), info: new TextEncoder().encode("aes-key")},
                    baseKey,
                    {name: "AES-GCM", length: 256},
                    true, ["encrypt", "decrypt"]
                );
                if (aesKey.type !== "secret") return "wrong type";
                var raw = await crypto.subtle.exportKey("raw", aesKey);
                if (new Uint8Array(raw).length !== 32) return "wrong length";
                return "ok";
            }
        "#), r#"{"jsonrpc":"2.0","method":"test","params":[],"id":1}"#).unwrap();
        assert!(r.json.contains("ok"), "got: {}", r.json);
    }

    // =======================================================================
    // ReadableStream tests (sync enqueue — no timers needed)
    // =======================================================================

    #[test]
    fn readable_stream_sync_enqueue() {
        let r = dispatch(m(r#"
            export async function test() {
                var stream = new ReadableStream({
                    start(controller) {
                        controller.enqueue("hello ");
                        controller.enqueue("world");
                        controller.close();
                    }
                });
                var reader = stream.getReader();
                var text = "";
                while (true) {
                    var r = await reader.read();
                    if (r.done) break;
                    text += new TextDecoder().decode(r.value);
                }
                return text;
            }
        "#), r#"{"jsonrpc":"2.0","method":"test","params":[],"id":1}"#).unwrap();
        assert!(r.json.contains("hello world"), "got: {}", r.json);
    }

    #[test]
    fn readable_stream_response_text_method() {
        let r = dispatch(m(r#"
            export async function test() {
                var stream = new ReadableStream({
                    start(controller) {
                        controller.enqueue("abc");
                        controller.enqueue("def");
                        controller.close();
                    }
                });
                var resp = new Response(stream);
                var text = await resp.text();
                return text;
            }
        "#), r#"{"jsonrpc":"2.0","method":"test","params":[],"id":1}"#).unwrap();
        assert!(r.json.contains("abcdef"), "got: {}", r.json);
    }

    // =======================================================================
    // onRequest HTTP handler tests (sync handlers only — no pump needed)
    // =======================================================================

    #[test]
    fn on_request_basic() {
        init_v8();
        let modules = vec![ModuleEntry {
            specifier: "index.js".into(),
            source: r#"
                export function onRequest(request) {
                    return new Response("Hello from " + request.method + " " + request.url, {
                        status: 200,
                        headers: { "X-Custom": "test" },
                    });
                }
            "#.into(),
        }];
        let mut runtime = Runtime::new_direct(modules, no_env(), None, None);

        // dispatch_http triggers ensure_initialized, which detects onRequest
        let (status, _headers, body) = match runtime.dispatch_http("GET", "http://localhost/hello", "[]", "") {
            DispatchOutcome::HttpComplete { status, headers, body, .. } => (status, headers, body),
            _ => panic!("expected HttpComplete"),
        };
        assert_eq!(status, 200);
        assert!(body.contains("Hello from GET"), "got: {}", body);
    }

    #[test]
    fn on_request_with_rpc() {
        init_v8();
        let modules = vec![ModuleEntry {
            specifier: "index.js".into(),
            source: r#"
                export function add(a, b) { return a + b; }
                export function onRequest(request) {
                    return new Response("HTTP handler", { status: 200 });
                }
            "#.into(),
        }];
        let mut runtime = Runtime::new_direct(modules, no_env(), None, None);

        // RPC still works
        let rpc = runtime.dispatch_rpc(r#"{"jsonrpc":"2.0","method":"add","params":[3,4],"id":1}"#).unwrap();
        assert!(rpc.json.contains("7"));

        // HTTP also works
        match runtime.dispatch_http("GET", "http://localhost/", "[]", "") {
            DispatchOutcome::HttpComplete { status, body, .. } => {
                assert_eq!(status, 200);
                assert!(body.contains("HTTP handler"));
            }
            _ => panic!("expected HttpComplete"),
        }
    }

    #[test]
    fn no_on_request_returns_error() {
        init_v8();
        let modules = vec![ModuleEntry {
            specifier: "index.js".into(),
            source: "export function ping() { return 'pong'; }".into(),
        }];
        let mut runtime = Runtime::new_direct(modules, no_env(), None, None);
        // dispatch_http triggers init — should return error since no onRequest
        match runtime.dispatch_http("GET", "http://localhost/", "[]", "") {
            DispatchOutcome::Complete(Err(e)) => assert!(e.contains("No onRequest")),
            _ => panic!("expected error"),
        }
    }

    #[test]
    fn on_request_async() {
        init_v8();
        let modules = vec![ModuleEntry {
            specifier: "index.js".into(),
            source: r#"
                export async function onRequest(request) {
                    return new Response("async response", { status: 201 });
                }
            "#.into(),
        }];
        let mut runtime = Runtime::new_direct(modules, no_env(), None, None);
        // async function that returns immediately (no pending ops) should settle inline
        match runtime.dispatch_http("GET", "http://localhost/", "[]", "") {
            DispatchOutcome::HttpComplete { status, body, .. } => {
                assert_eq!(status, 201);
                assert!(body.contains("async response"));
            }
            // If the runtime returns HttpPending for trivially-async handlers, that's also valid
            _ => panic!("expected HttpComplete for trivially-async handler"),
        }
    }

    #[test]
    fn url_in_http_handler() {
        init_v8();
        let modules = vec![ModuleEntry {
            specifier: "index.js".into(),
            source: r#"
                export function onRequest(request) {
                    const url = new URL(request.url);
                    return new Response(JSON.stringify({
                        path: url.pathname,
                        query: url.searchParams.get("name"),
                    }), { headers: { "Content-Type": "application/json" } });
                }
            "#.into(),
        }];
        let mut runtime = Runtime::new_direct(modules, no_env(), None, None);
        match runtime.dispatch_http("GET", "http://localhost/hello?name=world", "[]", "") {
            DispatchOutcome::HttpComplete { status, body, .. } => {
                assert_eq!(status, 200);
                assert!(body.contains("\"path\":\"/hello\""), "got: {}", body);
                assert!(body.contains("\"query\":\"world\""), "got: {}", body);
            }
            _ => panic!("expected HttpComplete"),
        }
    }

    // =======================================================================
    // Streaming HTTP response (sync enqueue — no timers)
    // =======================================================================

    #[test]
    fn streaming_http_response_sync() {
        init_v8();
        let modules = vec![ModuleEntry {
            specifier: "index.js".into(),
            source: r#"
                export function onRequest(request) {
                    var stream = new ReadableStream({
                        start(controller) {
                            controller.enqueue("data: event 0\n\n");
                            controller.enqueue("data: event 1\n\n");
                            controller.enqueue("data: event 2\n\n");
                            controller.close();
                        }
                    });
                    return new Response(stream, {
                        headers: { "Content-Type": "text/event-stream" }
                    });
                }
            "#.into(),
        }];
        let mut runtime = Runtime::new_direct(modules, no_env(), None, None);
        match runtime.dispatch_http("GET", "http://localhost/events", "[]", "") {
            DispatchOutcome::HttpComplete { status, body, .. } => {
                assert_eq!(status, 200);
                assert!(body.contains("data: event 0"), "got: {}", body);
                assert!(body.contains("data: event 2"), "got: {}", body);
            }
            DispatchOutcome::HttpStream { status, headers, body: body_reader, .. } => {
                assert_eq!(status, 200);
                let ct = headers.iter().find(|(k, _)| k == "content-type");
                assert!(ct.is_some());
                // Drain the shared buffer
                let mut body = String::new();
                for chunk in body_reader.drain() {
                    body.push_str(&String::from_utf8_lossy(&chunk));
                }
                assert!(body.contains("data: event 0"), "got: {}", body);
            }
            _ => panic!("expected HttpComplete or HttpStream"),
        }
    }
}
