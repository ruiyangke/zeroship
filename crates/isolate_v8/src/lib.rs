//! Raw V8 isolate runtime for appbase.
//!
//! Two execution models:
//! - **Per-request** (`Isolate`, `IsolatePool`): blocking, one request at a time per isolate.
//!   Best for multi-threaded CPU-heavy workloads.
//! - **Concurrent** (`ConcurrentIsolate`): serial JS + concurrent I/O on a single thread.
//!   Best for I/O-heavy workloads with clean per-request kill.
//!
//! # Module structure
//! - `runtime` — V8 init, CPU time, shared constants
//! - `timers` — min-heap timer system (setTimeout/setInterval)
//! - `globals` — V8 global bindings (console, timers)
//! - `event_loop` — event loop state and drivers
//! - `isolate` — per-request `Isolate` and `IsolatePool`
//! - `concurrent` — `ConcurrentIsolate` with serial JS + concurrent I/O

#![allow(unsafe_code)]

pub mod concurrent;
#[cfg(target_os = "linux")]
pub mod cpu_timer;
mod crypto;
mod env;
mod event_loop;
mod fetch;
mod globals;
mod isolate;
mod kv;
pub mod modules;
pub mod ops;
pub mod runtime;
pub mod storage;
mod timers;
mod url;

// Re-export public API
pub use isolate::{Isolate, IsolatePool};
pub use modules::ModuleEntry;
pub use storage::AppStorage;
pub use runtime::{init_v8, RequestResult};

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    /// Helper to create a single-module entry for tests.
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

    #[test]
    fn basic_rpc() {
        init_v8();
        let mut isolate = Isolate::new(m(r#"export function ping() { return "pong"; }"#), no_env());
        let r = isolate
            .execute_request(r#"{"jsonrpc":"2.0","method":"ping","params":[],"id":1}"#)
            .unwrap();
        assert!(r.json.contains("pong"));
    }

    #[test]
    fn persistent_context() {
        init_v8();
        let mut isolate = Isolate::new(m(r#"
            let n = 0;
            export function count() { return ++n; }
        "#), no_env());

        let r1 = isolate
            .execute_request(r#"{"jsonrpc":"2.0","method":"count","params":[],"id":1}"#)
            .unwrap();
        let r2 = isolate
            .execute_request(r#"{"jsonrpc":"2.0","method":"count","params":[],"id":2}"#)
            .unwrap();
        let r3 = isolate
            .execute_request(r#"{"jsonrpc":"2.0","method":"count","params":[],"id":3}"#)
            .unwrap();

        assert!(r1.json.contains("\"result\":1"));
        assert!(r2.json.contains("\"result\":2"));
        assert!(r3.json.contains("\"result\":3"));
    }

    #[test]
    fn per_request_cpu() {
        init_v8();
        let mut isolate = Isolate::new(m(r#"
            export function fib(n) {
                function f(n) { return n <= 1 ? n : f(n-1) + f(n-2); }
                return f(n);
            }
        "#), no_env());
        let r1 = isolate
            .execute_request(r#"{"jsonrpc":"2.0","method":"fib","params":[20],"id":1}"#)
            .unwrap();
        let r2 = isolate
            .execute_request(r#"{"jsonrpc":"2.0","method":"fib","params":[35],"id":2}"#)
            .unwrap();
        assert!(r2.cpu_time > r1.cpu_time * 5);
    }

    #[test]
    fn pool_reuse() {
        init_v8();
        let pool = IsolatePool::new(m(r#"export function ping() { return "pong"; }"#), no_env(), 4);
        for i in 0..10 {
            let r = pool
                .execute(&format!(
                    r#"{{"jsonrpc":"2.0","method":"ping","params":[],"id":{i}}}"#
                ))
                .unwrap();
            assert!(r.json.contains("pong"));
        }
    }

    #[test]
    fn async_timeout() {
        init_v8();
        let mut isolate = Isolate::new(m(r#"
            export function delayed() {
                return new Promise(function(resolve) {
                    setTimeout(function() { resolve("done after delay"); }, 10);
                });
            }
        "#), no_env());
        let r = isolate
            .execute_request(r#"{"jsonrpc":"2.0","method":"delayed","params":[],"id":1}"#)
            .unwrap();
        assert!(r.json.contains("done after delay"));
    }

    #[test]
    fn async_await_syntax() {
        init_v8();
        let mut isolate = Isolate::new(m(r#"
            export async function greeting(name) {
                const msg = await new Promise(function(resolve) {
                    setTimeout(function() { resolve("Hello, " + name + "!"); }, 5);
                });
                return msg;
            }
        "#), no_env());
        let r = isolate
            .execute_request(
                r#"{"jsonrpc":"2.0","method":"greeting","params":["world"],"id":1}"#,
            )
            .unwrap();
        assert!(r.json.contains("Hello, world!"));
    }

    #[test]
    fn clear_timeout_works() {
        init_v8();
        let mut isolate = Isolate::new(m(r#"
            export function test_clear() {
                return new Promise(function(resolve) {
                    var id = setTimeout(function() { resolve("should not fire"); }, 5000);
                    clearTimeout(id);
                    setTimeout(function() { resolve("cleared ok"); }, 5);
                });
            }
        "#), no_env());
        let r = isolate
            .execute_request(r#"{"jsonrpc":"2.0","method":"test_clear","params":[],"id":1}"#)
            .unwrap();
        assert!(r.json.contains("cleared ok"));
    }

    #[test]
    fn promise_chain() {
        init_v8();
        let mut isolate = Isolate::new(m(r#"
            export function chain() {
                return new Promise(function(resolve) {
                    setTimeout(function() { resolve(1); }, 5);
                }).then(function(v) {
                    return v + 10;
                }).then(function(v) {
                    return v * 2;
                });
            }
        "#), no_env());
        let r = isolate
            .execute_request(r#"{"jsonrpc":"2.0","method":"chain","params":[],"id":1}"#)
            .unwrap();
        assert!(r.json.contains("\"result\":22"));
    }

    #[test]
    fn set_timeout_zero_delay() {
        init_v8();
        let mut isolate = Isolate::new(m(r#"
            export function immediate() {
                return new Promise(function(resolve) {
                    setTimeout(function() { resolve("immediate"); }, 0);
                });
            }
        "#), no_env());
        let r = isolate
            .execute_request(r#"{"jsonrpc":"2.0","method":"immediate","params":[],"id":1}"#)
            .unwrap();
        assert!(r.json.contains("immediate"));
    }

    #[test]
    fn multiple_timeouts_ordered() {
        init_v8();
        let mut isolate = Isolate::new(m(r#"
            export function ordered() {
                var results = [];
                return new Promise(function(resolve) {
                    setTimeout(function() { results.push("c"); resolve(results.join(",")); }, 30);
                    setTimeout(function() { results.push("a"); }, 5);
                    setTimeout(function() { results.push("b"); }, 15);
                });
            }
        "#), no_env());
        let r = isolate
            .execute_request(r#"{"jsonrpc":"2.0","method":"ordered","params":[],"id":1}"#)
            .unwrap();
        assert!(r.json.contains("a,b,c"));
    }

    #[test]
    fn sync_still_works_with_event_loop() {
        init_v8();
        let mut isolate = Isolate::new(m(r#"export function add(a, b) { return a + b; }"#), no_env());
        let r = isolate
            .execute_request(r#"{"jsonrpc":"2.0","method":"add","params":[3,4],"id":1}"#)
            .unwrap();
        assert!(r.json.contains("\"result\":7"));
    }

    #[test]
    fn console_log_works() {
        init_v8();
        let mut isolate = Isolate::new(m(r#"
            export function greet() {
                console.log("Hello from JS!");
                return "logged";
            }
        "#), no_env());
        let r = isolate
            .execute_request(r#"{"jsonrpc":"2.0","method":"greet","params":[],"id":1}"#)
            .unwrap();
        assert!(r.json.contains("logged"));
    }

    // -----------------------------------------------------------------------
    // Fetch API integration tests
    // -----------------------------------------------------------------------

    #[test]
    fn fetch_get_json() {
        init_v8();
        let mut isolate = Isolate::new(m(r#"
            export async function test() {
                var resp = await fetch("https://httpbin.org/get");
                if (!resp.ok) return "status: " + resp.status;
                var data = await resp.json();
                return data.url;
            }
        "#), no_env());
        let r = isolate
            .execute_request(r#"{"jsonrpc":"2.0","method":"test","params":[],"id":1}"#)
            .unwrap();
        assert!(r.json.contains("httpbin.org/get"), "got: {}", r.json);
    }

    #[test]
    fn fetch_post_body() {
        init_v8();
        let mut isolate = Isolate::new(m(r#"
            export async function test() {
                var resp = await fetch("https://httpbin.org/post", {
                    method: "POST",
                    headers: { "Content-Type": "application/json" },
                    body: JSON.stringify({ hello: "world" }),
                });
                var data = await resp.json();
                return JSON.parse(data.data).hello;
            }
        "#), no_env());
        let r = isolate
            .execute_request(r#"{"jsonrpc":"2.0","method":"test","params":[],"id":1}"#)
            .unwrap();
        assert!(r.json.contains("world"), "got: {}", r.json);
    }

    #[test]
    fn fetch_response_headers() {
        init_v8();
        let mut isolate = Isolate::new(m(r#"
            export async function test() {
                var resp = await fetch("https://httpbin.org/get");
                return resp.headers.get("content-type");
            }
        "#), no_env());
        let r = isolate
            .execute_request(r#"{"jsonrpc":"2.0","method":"test","params":[],"id":1}"#)
            .unwrap();
        assert!(r.json.contains("application/json"), "got: {}", r.json);
    }

    #[test]
    fn fetch_404_not_ok() {
        init_v8();
        let mut isolate = Isolate::new(m(r#"
            export async function test() {
                var resp = await fetch("https://httpbin.org/status/404");
                return { ok: resp.ok, status: resp.status };
            }
        "#), no_env());
        let r = isolate
            .execute_request(r#"{"jsonrpc":"2.0","method":"test","params":[],"id":1}"#)
            .unwrap();
        assert!(r.json.contains("false"), "ok should be false, got: {}", r.json);
        assert!(r.json.contains("404"), "status should be 404, got: {}", r.json);
    }

    #[test]
    fn fetch_invalid_url_rejects() {
        init_v8();
        let mut isolate = Isolate::new(m(r#"
            export async function test() {
                try {
                    await fetch("not-a-url://invalid");
                    return "should not reach";
                } catch (e) {
                    return "caught: " + e.message;
                }
            }
        "#), no_env());
        let r = isolate
            .execute_request(r#"{"jsonrpc":"2.0","method":"test","params":[],"id":1}"#)
            .unwrap();
        assert!(r.json.contains("caught:"), "should catch error, got: {}", r.json);
    }

    #[test]
    fn fetch_response_text() {
        init_v8();
        let mut isolate = Isolate::new(m(r#"
            export async function test() {
                var resp = await fetch("https://httpbin.org/robots.txt");
                var text = await resp.text();
                return text.length > 0 ? "has body" : "empty";
            }
        "#), no_env());
        let r = isolate
            .execute_request(r#"{"jsonrpc":"2.0","method":"test","params":[],"id":1}"#)
            .unwrap();
        assert!(r.json.contains("has body"), "got: {}", r.json);
    }

    #[test]
    fn headers_class_works() {
        init_v8();
        let mut isolate = Isolate::new(m(r#"
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
        "#), no_env());
        let r = isolate
            .execute_request(r#"{"jsonrpc":"2.0","method":"test","params":[],"id":1}"#)
            .unwrap();
        assert!(r.json.contains("text/plain"), "got: {}", r.json);
        assert!(r.json.contains("hello, world"), "got: {}", r.json);
    }

    #[test]
    fn request_class_works() {
        init_v8();
        let mut isolate = Isolate::new(m(r#"
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
        "#), no_env());
        let r = isolate
            .execute_request(r#"{"jsonrpc":"2.0","method":"test","params":[],"id":1}"#)
            .unwrap();
        assert!(r.json.contains("example.com"), "got: {}", r.json);
        assert!(r.json.contains("POST"), "got: {}", r.json);
    }

    #[test]
    fn response_static_json() {
        init_v8();
        let mut isolate = Isolate::new(m(r#"
            export async function test() {
                var resp = Response.json({ hello: "world" });
                var data = await resp.json();
                return { status: resp.status, hello: data.hello, ct: resp.headers.get("content-type") };
            }
        "#), no_env());
        let r = isolate
            .execute_request(r#"{"jsonrpc":"2.0","method":"test","params":[],"id":1}"#)
            .unwrap();
        assert!(r.json.contains("world"), "got: {}", r.json);
        assert!(r.json.contains("application/json"), "got: {}", r.json);
    }

    #[test]
    fn fetch_concurrent_model() {
        use crate::concurrent::{ConcurrentIsolate, Event};
        init_v8();
        let modules = vec![ModuleEntry {
            specifier: "index.js".into(),
            source: r#"
                export async function test() {
                    var resp = await fetch("https://httpbin.org/get");
                    var data = await resp.json();
                    return data.url;
                }
            "#.into(),
        }];
        let (event_tx, event_rx) = std::sync::mpsc::channel();
        let event_tx_clone = event_tx.clone();
        let handle = std::thread::spawn(move || {
            let mut isolate = ConcurrentIsolate::new(modules, event_rx, event_tx_clone, None, None, HashMap::new());
            isolate.run_until_idle();
        });
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        event_tx
            .send(Event::NewRequest {
                id: 1,
                body: r#"{"jsonrpc":"2.0","method":"test","params":[],"id":1}"#.to_string(),
                reply: reply_tx,
            })
            .unwrap();
        drop(event_tx);
        let result = reply_rx.blocking_recv().unwrap().unwrap();
        assert!(result.json.contains("httpbin.org/get"), "got: {}", result.json);
        handle.join().unwrap();
    }

    #[test]
    fn kv_store_works() {
        init_v8();
        let mut isolate = Isolate::new(m(r#"
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
        "#), no_env());
        let r = isolate
            .execute_request(r#"{"jsonrpc":"2.0","method":"test","params":[],"id":1}"#)
            .unwrap();
        assert!(r.json.contains("Alice"), "got: {}", r.json);
        assert!(r.json.contains("null"), "got: {}", r.json);
    }

    #[test]
    fn env_get_works() {
        init_v8();
        let env = HashMap::from([("test_key".to_string(), "test_value".to_string())]);
        let mut isolate = Isolate::new(m(r#"export function test() { return env.get("test_key"); }"#), env);
        let r = isolate
            .execute_request(r#"{"jsonrpc":"2.0","method":"test","params":[],"id":1}"#)
            .unwrap();
        assert!(r.json.contains("test_value"), "got: {}", r.json);
    }

    #[test]
    fn env_get_missing_returns_null() {
        init_v8();
        let mut isolate = Isolate::new(m(
            r#"export function test() { return env.get("nonexistent_key_xyz") === null ? "is_null" : "not_null"; }"#,
        ), no_env());
        let r = isolate
            .execute_request(r#"{"jsonrpc":"2.0","method":"test","params":[],"id":1}"#)
            .unwrap();
        assert!(r.json.contains("is_null"), "got: {}", r.json);
    }

    #[test]
    fn kv_persists_across_requests() {
        init_v8();
        let mut isolate = Isolate::new(m(r#"
            export function set(k, v) { kv.set(k, v); return "ok"; }
            export function get(k) { return kv.get(k); }
        "#), no_env());
        isolate
            .execute_request(
                r#"{"jsonrpc":"2.0","method":"set","params":["key1","value1"],"id":1}"#,
            )
            .unwrap();
        let r = isolate
            .execute_request(r#"{"jsonrpc":"2.0","method":"get","params":["key1"],"id":2}"#)
            .unwrap();
        assert!(r.json.contains("value1"), "got: {}", r.json);
    }

    #[test]
    fn text_encoder_decoder() {
        init_v8();
        let mut isolate = Isolate::new(m(r#"export function test() {
            var enc = new TextEncoder();
            var buf = enc.encode("Hello");
            var dec = new TextDecoder();
            return { encoded: Array.from(buf), decoded: dec.decode(buf) };
        }"#), no_env());
        let r = isolate
            .execute_request(r#"{"jsonrpc":"2.0","method":"test","params":[],"id":1}"#)
            .unwrap();
        assert!(r.json.contains("Hello"), "got: {}", r.json);
        assert!(r.json.contains("[72,101,108,108,111]"), "got: {}", r.json);
    }

    #[test]
    fn crypto_random_uuid() {
        init_v8();
        let mut isolate = Isolate::new(m(r#"export function test() {
            var id1 = crypto.randomUUID();
            var id2 = crypto.randomUUID();
            return { id1, id2, different: id1 !== id2, format: /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/.test(id1) };
        }"#), no_env());
        let r = isolate
            .execute_request(r#"{"jsonrpc":"2.0","method":"test","params":[],"id":1}"#)
            .unwrap();
        assert!(r.json.contains("\"different\":true"), "got: {}", r.json);
        assert!(r.json.contains("\"format\":true"), "got: {}", r.json);
    }

    #[test]
    fn structured_clone() {
        init_v8();
        let mut isolate = Isolate::new(m(r#"export function test() {
            var obj = { a: 1, b: [2, 3], c: { d: "hello" } };
            var clone = structuredClone(obj);
            clone.a = 99;
            clone.b.push(4);
            return { original: obj.a, cloned: clone.a, origLen: obj.b.length, cloneLen: clone.b.length };
        }"#), no_env());
        let r = isolate
            .execute_request(r#"{"jsonrpc":"2.0","method":"test","params":[],"id":1}"#)
            .unwrap();
        assert!(r.json.contains("\"original\":1"), "got: {}", r.json);
        assert!(r.json.contains("\"cloned\":99"), "got: {}", r.json);
        assert!(r.json.contains("\"origLen\":2"), "got: {}", r.json);
        assert!(r.json.contains("\"cloneLen\":3"), "got: {}", r.json);
    }

    #[test]
    fn btoa_atob() {
        init_v8();
        let mut isolate = Isolate::new(m(r#"export function test() {
            var encoded = btoa("Hello, World!");
            var decoded = atob(encoded);
            return { encoded, decoded };
        }"#), no_env());
        let r = isolate
            .execute_request(r#"{"jsonrpc":"2.0","method":"test","params":[],"id":1}"#)
            .unwrap();
        assert!(r.json.contains("SGVsbG8sIFdvcmxkIQ=="), "got: {}", r.json);
        assert!(r.json.contains("Hello, World!"), "got: {}", r.json);
    }

    // -----------------------------------------------------------------------
    // ESM module integration tests
    // -----------------------------------------------------------------------

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
        let mut isolate = Isolate::new(modules, no_env());
        let r = isolate
            .execute_request(r#"{"jsonrpc":"2.0","method":"ping","params":[],"id":1}"#)
            .unwrap();
        assert!(r.json.contains("pong"), "got: {}", r.json);

        let r2 = isolate
            .execute_request(r#"{"jsonrpc":"2.0","method":"add","params":[3,4],"id":2}"#)
            .unwrap();
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
        let mut isolate = Isolate::new(modules, no_env());
        let r = isolate
            .execute_request(r#"{"jsonrpc":"2.0","method":"compute","params":[3,4],"id":1}"#)
            .unwrap();
        assert!(r.json.contains("7"), "got: {}", r.json);
    }

    #[test]
    fn esm_async_with_fetch() {
        init_v8();
        let modules = vec![ModuleEntry {
            specifier: "index.js".into(),
            source: r#"
                export async function fetchTest() {
                    const resp = await fetch("https://httpbin.org/get");
                    return resp.status;
                }
            "#.into(),
        }];
        let mut isolate = Isolate::new(modules, no_env());
        let r = isolate
            .execute_request(r#"{"jsonrpc":"2.0","method":"fetchTest","params":[],"id":1}"#)
            .unwrap();
        assert!(r.json.contains("200"), "got: {}", r.json);
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
        let mut isolate = Isolate::new(modules, no_env());
        isolate
            .execute_request(r#"{"jsonrpc":"2.0","method":"set","params":["x","1"],"id":1}"#)
            .unwrap();
        let r = isolate
            .execute_request(r#"{"jsonrpc":"2.0","method":"get","params":["x"],"id":2}"#)
            .unwrap();
        assert!(r.json.contains("1"), "got: {}", r.json);
    }

    #[test]
    fn esm_async_timeout() {
        init_v8();
        let modules = vec![ModuleEntry {
            specifier: "index.js".into(),
            source: r#"
                export function delayed() {
                    return new Promise(function(resolve) {
                        setTimeout(function() { resolve("done after delay"); }, 10);
                    });
                }
            "#.into(),
        }];
        let mut isolate = Isolate::new(modules, no_env());
        let r = isolate
            .execute_request(r#"{"jsonrpc":"2.0","method":"delayed","params":[],"id":1}"#)
            .unwrap();
        assert!(r.json.contains("done after delay"), "got: {}", r.json);
    }

    #[test]
    fn esm_concurrent_sync() {
        use crate::concurrent::{ConcurrentIsolate, Event};
        init_v8();
        let modules = vec![ModuleEntry {
            specifier: "index.js".into(),
            source: r#"
                export function ping() { return "pong"; }
                export function add(a, b) { return a + b; }
            "#.into(),
        }];
        let (event_tx, event_rx) = std::sync::mpsc::channel();
        let event_tx_clone = event_tx.clone();
        let handle = std::thread::spawn(move || {
            let mut isolate = ConcurrentIsolate::new(modules, event_rx, event_tx_clone, None, None, HashMap::new());
            isolate.run_until_idle();
        });
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        event_tx
            .send(Event::NewRequest {
                id: 1,
                body: r#"{"jsonrpc":"2.0","method":"ping","params":[],"id":1}"#.to_string(),
                reply: reply_tx,
            })
            .unwrap();
        drop(event_tx);
        let result = reply_rx.blocking_recv().unwrap().unwrap();
        assert!(result.json.contains("pong"), "got: {}", result.json);
        handle.join().unwrap();
    }

    #[test]
    fn esm_concurrent_async() {
        use crate::concurrent::{ConcurrentIsolate, Event};
        init_v8();
        let modules = vec![ModuleEntry {
            specifier: "index.js".into(),
            source: r#"
                export function delayed(ms) {
                    return new Promise(function(resolve) {
                        setTimeout(function() { resolve("done_" + ms); }, ms || 10);
                    });
                }
            "#.into(),
        }];
        let (event_tx, event_rx) = std::sync::mpsc::channel();
        let event_tx_clone = event_tx.clone();
        let handle = std::thread::spawn(move || {
            let mut isolate = ConcurrentIsolate::new(modules, event_rx, event_tx_clone, None, None, HashMap::new());
            isolate.run_until_idle();
        });
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        event_tx
            .send(Event::NewRequest {
                id: 1,
                body: r#"{"jsonrpc":"2.0","method":"delayed","params":[10],"id":1}"#.to_string(),
                reply: reply_tx,
            })
            .unwrap();
        drop(event_tx);
        let result = reply_rx.blocking_recv().unwrap().unwrap();
        assert!(result.json.contains("done_10"), "got: {}", result.json);
        handle.join().unwrap();
    }

    // -----------------------------------------------------------------------
    // onRequest HTTP handler tests
    // -----------------------------------------------------------------------

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
        let mut isolate = Isolate::new(modules, no_env());
        assert!(isolate.has_http_handler());

        let result = isolate.execute_http("GET", "http://localhost/hello", "[]", "").unwrap().unwrap();
        assert_eq!(result.status, 200);
        assert!(result.body.contains("Hello from GET"), "got: {}", result.body);
    }

    #[test]
    fn on_request_with_rpc() {
        // Both RPC methods and onRequest in the same app
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
        let mut isolate = Isolate::new(modules, no_env());

        // RPC still works
        let rpc = isolate.execute_request(r#"{"jsonrpc":"2.0","method":"add","params":[3,4],"id":1}"#).unwrap();
        assert!(rpc.json.contains("7"));

        // HTTP also works
        let http = isolate.execute_http("GET", "http://localhost/", "[]", "").unwrap().unwrap();
        assert_eq!(http.status, 200);
        assert!(http.body.contains("HTTP handler"));
    }

    #[test]
    fn no_on_request_returns_none() {
        init_v8();
        let modules = vec![ModuleEntry {
            specifier: "index.js".into(),
            source: "export function ping() { return 'pong'; }".into(),
        }];
        let mut isolate = Isolate::new(modules, no_env());
        assert!(!isolate.has_http_handler());
        assert!(isolate.execute_http("GET", "http://localhost/", "[]", "").is_none());
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
        let mut isolate = Isolate::new(modules, no_env());
        let result = isolate.execute_http("GET", "http://localhost/", "[]", "").unwrap().unwrap();
        assert_eq!(result.status, 201);
        assert!(result.body.contains("async response"));
    }

    // -----------------------------------------------------------------------
    // URL / URLSearchParams tests
    // -----------------------------------------------------------------------

    #[test]
    fn url_parse_basic() {
        init_v8();
        let modules = vec![ModuleEntry {
            specifier: "index.js".into(),
            source: r#"
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
            "#.into(),
        }];
        let mut isolate = Isolate::new(modules, no_env());
        let r = isolate.execute_request(r#"{"jsonrpc":"2.0","method":"test","params":[],"id":1}"#).unwrap();
        assert!(r.json.contains("\"protocol\":\"https:\""), "got: {}", r.json);
        assert!(r.json.contains("\"hostname\":\"example.com\""), "got: {}", r.json);
        assert!(r.json.contains("\"port\":\"8080\""), "got: {}", r.json);
        assert!(r.json.contains("\"pathname\":\"/path\""), "got: {}", r.json);
        assert!(r.json.contains("\"hash\":\"#frag\""), "got: {}", r.json);
    }

    #[test]
    fn url_with_base() {
        init_v8();
        let modules = vec![ModuleEntry {
            specifier: "index.js".into(),
            source: r#"
                export function test() {
                    const url = new URL("/api/users", "https://example.com");
                    return url.href;
                }
            "#.into(),
        }];
        let mut isolate = Isolate::new(modules, no_env());
        let r = isolate.execute_request(r#"{"jsonrpc":"2.0","method":"test","params":[],"id":1}"#).unwrap();
        assert!(r.json.contains("https://example.com/api/users"), "got: {}", r.json);
    }

    #[test]
    fn url_invalid_throws() {
        init_v8();
        let modules = vec![ModuleEntry {
            specifier: "index.js".into(),
            source: r#"
                export function test() {
                    try { new URL("not a url"); return "should have thrown"; }
                    catch (e) { return "caught: " + e.message; }
                }
            "#.into(),
        }];
        let mut isolate = Isolate::new(modules, no_env());
        let r = isolate.execute_request(r#"{"jsonrpc":"2.0","method":"test","params":[],"id":1}"#).unwrap();
        assert!(r.json.contains("caught:"), "got: {}", r.json);
    }

    #[test]
    fn url_can_parse() {
        init_v8();
        let modules = vec![ModuleEntry {
            specifier: "index.js".into(),
            source: r#"
                export function test() {
                    return {
                        valid: URL.canParse("https://example.com"),
                        invalid: URL.canParse("not a url"),
                        relative: URL.canParse("/path", "https://example.com"),
                    };
                }
            "#.into(),
        }];
        let mut isolate = Isolate::new(modules, no_env());
        let r = isolate.execute_request(r#"{"jsonrpc":"2.0","method":"test","params":[],"id":1}"#).unwrap();
        assert!(r.json.contains("\"valid\":true"), "got: {}", r.json);
        assert!(r.json.contains("\"invalid\":false"), "got: {}", r.json);
        assert!(r.json.contains("\"relative\":true"), "got: {}", r.json);
    }

    #[test]
    fn url_search_params() {
        init_v8();
        let modules = vec![ModuleEntry {
            specifier: "index.js".into(),
            source: r#"
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
            "#.into(),
        }];
        let mut isolate = Isolate::new(modules, no_env());
        let r = isolate.execute_request(r#"{"jsonrpc":"2.0","method":"test","params":[],"id":1}"#).unwrap();
        assert!(r.json.contains("\"q\":\"hello\""), "got: {}", r.json);
        assert!(r.json.contains("\"lang\":\"en\""), "got: {}", r.json);
        assert!(r.json.contains("\"missing\":null"), "got: {}", r.json);
        assert!(r.json.contains("\"has_q\":true"), "got: {}", r.json);
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
        let mut isolate = Isolate::new(modules, no_env());
        let r = isolate.execute_http("GET", "http://localhost/hello?name=world", "[]", "").unwrap().unwrap();
        assert_eq!(r.status, 200);
        assert!(r.body.contains("\"path\":\"/hello\""), "got: {}", r.body);
        assert!(r.body.contains("\"query\":\"world\""), "got: {}", r.body);
    }

    // -----------------------------------------------------------------------
    // Web Crypto: getRandomValues + subtle.digest
    // -----------------------------------------------------------------------

    #[test]
    fn crypto_get_random_values() {
        init_v8();
        let mut isolate = Isolate::new(m(r#"
            export function test() {
                var buf = new Uint8Array(16);
                crypto.getRandomValues(buf);
                var nonzero = 0;
                for (var i = 0; i < buf.length; i++) { if (buf[i] !== 0) nonzero++; }
                return nonzero > 0 ? "ok" : "all_zeros";
            }
        "#), no_env());
        let r = isolate.execute_request(r#"{"jsonrpc":"2.0","method":"test","params":[],"id":1}"#).unwrap();
        assert!(r.json.contains("ok"), "got: {}", r.json);
    }

    #[test]
    fn crypto_subtle_digest_sha256() {
        init_v8();
        let mut isolate = Isolate::new(m(r#"
            export async function test() {
                var data = new TextEncoder().encode("hello");
                var hash = await crypto.subtle.digest("SHA-256", data);
                var bytes = new Uint8Array(hash);
                var hex = "";
                for (var i = 0; i < bytes.length; i++) hex += ("0" + bytes[i].toString(16)).slice(-2);
                return hex;
            }
        "#), no_env());
        let r = isolate.execute_request(r#"{"jsonrpc":"2.0","method":"test","params":[],"id":1}"#).unwrap();
        // SHA-256("hello") = 2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824
        assert!(r.json.contains("2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"), "got: {}", r.json);
    }

    #[test]
    fn crypto_subtle_digest_sha512() {
        init_v8();
        let mut isolate = Isolate::new(m(r#"
            export async function test() {
                var data = new TextEncoder().encode("hello");
                var hash = await crypto.subtle.digest("SHA-512", data);
                return new Uint8Array(hash).length;
            }
        "#), no_env());
        let r = isolate.execute_request(r#"{"jsonrpc":"2.0","method":"test","params":[],"id":1}"#).unwrap();
        assert!(r.json.contains("64"), "SHA-512 should produce 64 bytes, got: {}", r.json);
    }

    // -----------------------------------------------------------------------
    // Web Crypto: importKey + exportKey + generateKey
    // -----------------------------------------------------------------------

    #[test]
    fn crypto_import_export_hmac_raw() {
        init_v8();
        let mut isolate = Isolate::new(m(r#"
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
        "#), no_env());
        let r = isolate.execute_request(r#"{"jsonrpc":"2.0","method":"test","params":[],"id":1}"#).unwrap();
        assert!(r.json.contains("ok"), "got: {}", r.json);
    }

    #[test]
    fn crypto_generate_hmac_key() {
        init_v8();
        let mut isolate = Isolate::new(m(r#"
            export async function test() {
                var key = await crypto.subtle.generateKey({name: "HMAC", hash: "SHA-256"}, true, ["sign", "verify"]);
                if (key.type !== "secret") return "wrong type";
                var exported = await crypto.subtle.exportKey("raw", key);
                if (new Uint8Array(exported).length !== 32) return "wrong length";
                return "ok";
            }
        "#), no_env());
        let r = isolate.execute_request(r#"{"jsonrpc":"2.0","method":"test","params":[],"id":1}"#).unwrap();
        assert!(r.json.contains("ok"), "got: {}", r.json);
    }

    #[test]
    fn crypto_generate_ecdsa_keypair() {
        init_v8();
        let mut isolate = Isolate::new(m(r#"
            export async function test() {
                var kp = await crypto.subtle.generateKey({name: "ECDSA", namedCurve: "P-256"}, true, ["sign", "verify"]);
                if (!kp.publicKey || !kp.privateKey) return "no keypair";
                if (kp.publicKey.type !== "public") return "wrong pub type";
                if (kp.privateKey.type !== "private") return "wrong priv type";
                return "ok";
            }
        "#), no_env());
        let r = isolate.execute_request(r#"{"jsonrpc":"2.0","method":"test","params":[],"id":1}"#).unwrap();
        assert!(r.json.contains("ok"), "got: {}", r.json);
    }

    #[test]
    fn crypto_generate_aes_key() {
        init_v8();
        let mut isolate = Isolate::new(m(r#"
            export async function test() {
                var key = await crypto.subtle.generateKey({name: "AES-GCM", length: 256}, true, ["encrypt", "decrypt"]);
                if (key.type !== "secret") return "wrong type";
                var raw = await crypto.subtle.exportKey("raw", key);
                if (new Uint8Array(raw).length !== 32) return "wrong length";
                return "ok";
            }
        "#), no_env());
        let r = isolate.execute_request(r#"{"jsonrpc":"2.0","method":"test","params":[],"id":1}"#).unwrap();
        assert!(r.json.contains("ok"), "got: {}", r.json);
    }

    // -----------------------------------------------------------------------
    // Web Crypto: sign + verify
    // -----------------------------------------------------------------------

    #[test]
    fn crypto_hmac_sign_verify() {
        init_v8();
        let mut isolate = Isolate::new(m(r#"
            export async function test() {
                var raw = new TextEncoder().encode("my-secret-key-for-hmac-test!!");
                var key = await crypto.subtle.importKey("raw", raw, {name: "HMAC", hash: "SHA-256"}, false, ["sign", "verify"]);
                var data = new TextEncoder().encode("hello world");
                var sig = await crypto.subtle.sign("HMAC", key, data);
                var valid = await crypto.subtle.verify("HMAC", key, sig, data);
                if (!valid) return "verify failed";
                // Tamper with data
                var bad = await crypto.subtle.verify("HMAC", key, sig, new TextEncoder().encode("tampered"));
                if (bad) return "tampered verify should fail";
                return "ok";
            }
        "#), no_env());
        let r = isolate.execute_request(r#"{"jsonrpc":"2.0","method":"test","params":[],"id":1}"#).unwrap();
        assert!(r.json.contains("ok"), "got: {}", r.json);
    }

    #[test]
    fn crypto_ecdsa_sign_verify() {
        init_v8();
        let mut isolate = Isolate::new(m(r#"
            export async function test() {
                var kp = await crypto.subtle.generateKey({name: "ECDSA", namedCurve: "P-256"}, false, ["sign", "verify"]);
                var data = new TextEncoder().encode("test message");
                var sig = await crypto.subtle.sign({name: "ECDSA", hash: "SHA-256"}, kp.privateKey, data);
                var valid = await crypto.subtle.verify({name: "ECDSA", hash: "SHA-256"}, kp.publicKey, sig, data);
                if (!valid) return "verify failed";
                return "ok";
            }
        "#), no_env());
        let r = isolate.execute_request(r#"{"jsonrpc":"2.0","method":"test","params":[],"id":1}"#).unwrap();
        assert!(r.json.contains("ok"), "got: {}", r.json);
    }

    #[test]
    fn crypto_ed25519_sign_verify() {
        init_v8();
        let mut isolate = Isolate::new(m(r#"
            export async function test() {
                var kp = await crypto.subtle.generateKey({name: "Ed25519"}, false, ["sign", "verify"]);
                var data = new TextEncoder().encode("ed25519 test");
                var sig = await crypto.subtle.sign("Ed25519", kp.privateKey, data);
                var valid = await crypto.subtle.verify("Ed25519", kp.publicKey, sig, data);
                if (!valid) return "verify failed";
                return "ok";
            }
        "#), no_env());
        let r = isolate.execute_request(r#"{"jsonrpc":"2.0","method":"test","params":[],"id":1}"#).unwrap();
        assert!(r.json.contains("ok"), "got: {}", r.json);
    }
}
