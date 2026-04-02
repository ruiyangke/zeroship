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
mod event_loop;
mod fetch;
mod globals;
mod isolate;
mod kv;
pub mod runtime;
mod timers;

// Re-export public API
pub use isolate::{Isolate, IsolatePool};
pub use runtime::{init_v8, RequestResult};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_rpc() {
        init_v8();
        let js = r#"var __rpc = { ping: function() { return "pong"; } };"#;
        let mut isolate = Isolate::new(js);
        let r = isolate
            .execute_request(r#"{"jsonrpc":"2.0","method":"ping","params":[],"id":1}"#)
            .unwrap();
        assert!(r.json.contains("pong"));
    }

    #[test]
    fn persistent_context() {
        init_v8();
        let js = r#"var __rpc = { count: (function() { var n=0; return function() { return ++n; }; })() };"#;
        let mut isolate = Isolate::new(js);

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
        let js = r#"
            var __rpc = {
                fib: function(n) {
                    function f(n) { return n <= 1 ? n : f(n-1) + f(n-2); }
                    return f(n);
                }
            };
        "#;
        let mut isolate = Isolate::new(js);
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
        let js = r#"var __rpc = { ping: function() { return "pong"; } };"#;
        let pool = IsolatePool::new(js, 4);
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
        let js = r#"
            var __rpc = {
                delayed: function() {
                    return new Promise(function(resolve) {
                        setTimeout(function() { resolve("done after delay"); }, 10);
                    });
                }
            };
        "#;
        let mut isolate = Isolate::new(js);
        let r = isolate
            .execute_request(r#"{"jsonrpc":"2.0","method":"delayed","params":[],"id":1}"#)
            .unwrap();
        assert!(r.json.contains("done after delay"));
    }

    #[test]
    fn async_await_syntax() {
        init_v8();
        let js = r#"
            var __rpc = {
                greeting: async function(name) {
                    var msg = await new Promise(function(resolve) {
                        setTimeout(function() { resolve("Hello, " + name + "!"); }, 5);
                    });
                    return msg;
                }
            };
        "#;
        let mut isolate = Isolate::new(js);
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
        let js = r#"
            var __rpc = {
                test_clear: function() {
                    return new Promise(function(resolve) {
                        var id = setTimeout(function() { resolve("should not fire"); }, 5000);
                        clearTimeout(id);
                        setTimeout(function() { resolve("cleared ok"); }, 5);
                    });
                }
            };
        "#;
        let mut isolate = Isolate::new(js);
        let r = isolate
            .execute_request(r#"{"jsonrpc":"2.0","method":"test_clear","params":[],"id":1}"#)
            .unwrap();
        assert!(r.json.contains("cleared ok"));
    }

    #[test]
    fn promise_chain() {
        init_v8();
        let js = r#"
            var __rpc = {
                chain: function() {
                    return new Promise(function(resolve) {
                        setTimeout(function() { resolve(1); }, 5);
                    }).then(function(v) {
                        return v + 10;
                    }).then(function(v) {
                        return v * 2;
                    });
                }
            };
        "#;
        let mut isolate = Isolate::new(js);
        let r = isolate
            .execute_request(r#"{"jsonrpc":"2.0","method":"chain","params":[],"id":1}"#)
            .unwrap();
        assert!(r.json.contains("\"result\":22"));
    }

    #[test]
    fn set_timeout_zero_delay() {
        init_v8();
        let js = r#"
            var __rpc = {
                immediate: function() {
                    return new Promise(function(resolve) {
                        setTimeout(function() { resolve("immediate"); }, 0);
                    });
                }
            };
        "#;
        let mut isolate = Isolate::new(js);
        let r = isolate
            .execute_request(r#"{"jsonrpc":"2.0","method":"immediate","params":[],"id":1}"#)
            .unwrap();
        assert!(r.json.contains("immediate"));
    }

    #[test]
    fn multiple_timeouts_ordered() {
        init_v8();
        let js = r#"
            var __rpc = {
                ordered: function() {
                    var results = [];
                    return new Promise(function(resolve) {
                        setTimeout(function() { results.push("c"); resolve(results.join(",")); }, 30);
                        setTimeout(function() { results.push("a"); }, 5);
                        setTimeout(function() { results.push("b"); }, 15);
                    });
                }
            };
        "#;
        let mut isolate = Isolate::new(js);
        let r = isolate
            .execute_request(r#"{"jsonrpc":"2.0","method":"ordered","params":[],"id":1}"#)
            .unwrap();
        assert!(r.json.contains("a,b,c"));
    }

    #[test]
    fn sync_still_works_with_event_loop() {
        init_v8();
        let js = r#"var __rpc = { add: function(a, b) { return a + b; } };"#;
        let mut isolate = Isolate::new(js);
        let r = isolate
            .execute_request(r#"{"jsonrpc":"2.0","method":"add","params":[3,4],"id":1}"#)
            .unwrap();
        assert!(r.json.contains("\"result\":7"));
    }

    #[test]
    fn console_log_works() {
        init_v8();
        let js = r#"
            var __rpc = {
                greet: function() {
                    console.log("Hello from JS!");
                    return "logged";
                }
            };
        "#;
        let mut isolate = Isolate::new(js);
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
        let js = r#"
            var __rpc = {
                test: async function() {
                    var resp = await fetch("https://httpbin.org/get");
                    if (!resp.ok) return "status: " + resp.status;
                    var data = await resp.json();
                    return data.url;
                }
            };
        "#;
        let mut isolate = Isolate::new(js);
        let r = isolate
            .execute_request(r#"{"jsonrpc":"2.0","method":"test","params":[],"id":1}"#)
            .unwrap();
        assert!(r.json.contains("httpbin.org/get"), "got: {}", r.json);
    }

    #[test]
    fn fetch_post_body() {
        init_v8();
        let js = r#"
            var __rpc = {
                test: async function() {
                    var resp = await fetch("https://httpbin.org/post", {
                        method: "POST",
                        headers: { "Content-Type": "application/json" },
                        body: JSON.stringify({ hello: "world" }),
                    });
                    var data = await resp.json();
                    return JSON.parse(data.data).hello;
                }
            };
        "#;
        let mut isolate = Isolate::new(js);
        let r = isolate
            .execute_request(r#"{"jsonrpc":"2.0","method":"test","params":[],"id":1}"#)
            .unwrap();
        assert!(r.json.contains("world"), "got: {}", r.json);
    }

    #[test]
    fn fetch_response_headers() {
        init_v8();
        let js = r#"
            var __rpc = {
                test: async function() {
                    var resp = await fetch("https://httpbin.org/get");
                    return resp.headers.get("content-type");
                }
            };
        "#;
        let mut isolate = Isolate::new(js);
        let r = isolate
            .execute_request(r#"{"jsonrpc":"2.0","method":"test","params":[],"id":1}"#)
            .unwrap();
        assert!(r.json.contains("application/json"), "got: {}", r.json);
    }

    #[test]
    fn fetch_404_not_ok() {
        init_v8();
        let js = r#"
            var __rpc = {
                test: async function() {
                    var resp = await fetch("https://httpbin.org/status/404");
                    return { ok: resp.ok, status: resp.status };
                }
            };
        "#;
        let mut isolate = Isolate::new(js);
        let r = isolate
            .execute_request(r#"{"jsonrpc":"2.0","method":"test","params":[],"id":1}"#)
            .unwrap();
        assert!(r.json.contains("false"), "ok should be false, got: {}", r.json);
        assert!(r.json.contains("404"), "status should be 404, got: {}", r.json);
    }

    #[test]
    fn fetch_invalid_url_rejects() {
        init_v8();
        let js = r#"
            var __rpc = {
                test: async function() {
                    try {
                        await fetch("not-a-url://invalid");
                        return "should not reach";
                    } catch (e) {
                        return "caught: " + e.message;
                    }
                }
            };
        "#;
        let mut isolate = Isolate::new(js);
        let r = isolate
            .execute_request(r#"{"jsonrpc":"2.0","method":"test","params":[],"id":1}"#)
            .unwrap();
        assert!(r.json.contains("caught:"), "should catch error, got: {}", r.json);
    }

    #[test]
    fn fetch_response_text() {
        init_v8();
        let js = r#"
            var __rpc = {
                test: async function() {
                    var resp = await fetch("https://httpbin.org/robots.txt");
                    var text = await resp.text();
                    return text.length > 0 ? "has body" : "empty";
                }
            };
        "#;
        let mut isolate = Isolate::new(js);
        let r = isolate
            .execute_request(r#"{"jsonrpc":"2.0","method":"test","params":[],"id":1}"#)
            .unwrap();
        assert!(r.json.contains("has body"), "got: {}", r.json);
    }

    #[test]
    fn headers_class_works() {
        init_v8();
        let js = r#"
            var __rpc = {
                test: function() {
                    var h = new Headers({ "Content-Type": "text/plain", "X-Custom": "hello" });
                    h.append("X-Custom", "world");
                    return {
                        ct: h.get("content-type"),
                        custom: h.get("x-custom"),
                        has_ct: h.has("content-type"),
                        missing: h.has("nonexistent"),
                    };
                }
            };
        "#;
        let mut isolate = Isolate::new(js);
        let r = isolate
            .execute_request(r#"{"jsonrpc":"2.0","method":"test","params":[],"id":1}"#)
            .unwrap();
        assert!(r.json.contains("text/plain"), "got: {}", r.json);
        assert!(r.json.contains("hello, world"), "got: {}", r.json);
    }

    #[test]
    fn request_class_works() {
        init_v8();
        let js = r#"
            var __rpc = {
                test: function() {
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
            };
        "#;
        let mut isolate = Isolate::new(js);
        let r = isolate
            .execute_request(r#"{"jsonrpc":"2.0","method":"test","params":[],"id":1}"#)
            .unwrap();
        assert!(r.json.contains("example.com"), "got: {}", r.json);
        assert!(r.json.contains("POST"), "got: {}", r.json);
    }

    #[test]
    fn response_static_json() {
        init_v8();
        let js = r#"
            var __rpc = {
                test: async function() {
                    var resp = Response.json({ hello: "world" });
                    var data = await resp.json();
                    return { status: resp.status, hello: data.hello, ct: resp.headers.get("content-type") };
                }
            };
        "#;
        let mut isolate = Isolate::new(js);
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
        let js = r#"
            var __rpc = {
                test: async function() {
                    var resp = await fetch("https://httpbin.org/get");
                    var data = await resp.json();
                    return data.url;
                }
            };
        "#;
        let (event_tx, event_rx) = std::sync::mpsc::channel();
        let event_tx_clone = event_tx.clone();
        let js_owned = js.to_string();
        let handle = std::thread::spawn(move || {
            let mut isolate = ConcurrentIsolate::new(&js_owned, event_rx, event_tx_clone, None, None);
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
        let js = r#"
            var __rpc = {
                test: function() {
                    kv.set("name", "Alice");
                    kv.set("age", "30");
                    var name = kv.get("name");
                    var missing = kv.get("nonexistent");
                    var keys = kv.list();
                    kv.delete("age");
                    var afterDelete = kv.list();
                    return { name, missing, keys, afterDelete };
                }
            };
        "#;
        let mut isolate = Isolate::new(js);
        let r = isolate
            .execute_request(r#"{"jsonrpc":"2.0","method":"test","params":[],"id":1}"#)
            .unwrap();
        assert!(r.json.contains("Alice"), "got: {}", r.json);
        assert!(r.json.contains("null"), "got: {}", r.json);
    }

    #[test]
    fn env_get_works() {
        // SAFETY: test is single-threaded with respect to this env var.
        unsafe { std::env::set_var("APPBASE_APP_TEST_KEY", "test_value") };
        init_v8();
        let js = r#"var __rpc = { test: function() { return env.get("test_key"); } };"#;
        let mut isolate = Isolate::new(js);
        let r = isolate
            .execute_request(r#"{"jsonrpc":"2.0","method":"test","params":[],"id":1}"#)
            .unwrap();
        assert!(r.json.contains("test_value"), "got: {}", r.json);
        // SAFETY: test is single-threaded with respect to this env var.
        unsafe { std::env::remove_var("APPBASE_APP_TEST_KEY") };
    }

    #[test]
    fn env_get_missing_returns_null() {
        init_v8();
        let js =
            r#"var __rpc = { test: function() { return env.get("nonexistent_key_xyz") === null ? "is_null" : "not_null"; } };"#;
        let mut isolate = Isolate::new(js);
        let r = isolate
            .execute_request(r#"{"jsonrpc":"2.0","method":"test","params":[],"id":1}"#)
            .unwrap();
        assert!(r.json.contains("is_null"), "got: {}", r.json);
    }

    #[test]
    fn kv_persists_across_requests() {
        init_v8();
        let js = r#"
            var __rpc = {
                set: function(k, v) { kv.set(k, v); return "ok"; },
                get: function(k) { return kv.get(k); }
            };
        "#;
        let mut isolate = Isolate::new(js);
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
        let js = r#"var __rpc = { test: function() {
            var enc = new TextEncoder();
            var buf = enc.encode("Hello");
            var dec = new TextDecoder();
            return { encoded: Array.from(buf), decoded: dec.decode(buf) };
        }};"#;
        let mut isolate = Isolate::new(js);
        let r = isolate
            .execute_request(r#"{"jsonrpc":"2.0","method":"test","params":[],"id":1}"#)
            .unwrap();
        assert!(r.json.contains("Hello"), "got: {}", r.json);
        assert!(r.json.contains("[72,101,108,108,111]"), "got: {}", r.json);
    }

    #[test]
    fn btoa_atob() {
        init_v8();
        let js = r#"var __rpc = { test: function() {
            var encoded = btoa("Hello, World!");
            var decoded = atob(encoded);
            return { encoded, decoded };
        }};"#;
        let mut isolate = Isolate::new(js);
        let r = isolate
            .execute_request(r#"{"jsonrpc":"2.0","method":"test","params":[],"id":1}"#)
            .unwrap();
        assert!(r.json.contains("SGVsbG8sIFdvcmxkIQ=="), "got: {}", r.json);
        assert!(r.json.contains("Hello, World!"), "got: {}", r.json);
    }
}
