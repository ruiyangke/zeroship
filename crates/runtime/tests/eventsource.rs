//! End-to-end tests for native `EventSource` per HTML §9.2.
//!
//! Uses an in-process std::net TCP server that speaks just enough HTTP/1.1
//! + `text/event-stream` to drive the SSE parser through realistic wire
//! shapes. The native fetch client connects to it; promise reactions
//! drive the read loop.

mod common;

use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use common::{dispatch, m};

// ---------------------------------------------------------------------------
// In-process SSE server
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct ServerCfg {
    /// Lines emitted per connection. Each entry is a complete SSE
    /// "event" body (terminated by a blank line internally).
    events: Vec<String>,
    /// If `Some(ms)`, the server sends `retry: <ms>` first so reconnects
    /// fire faster.
    retry_ms: Option<u32>,
    /// If true, the server returns 500 instead of 200 on the first
    /// connection.
    fail_first: Arc<AtomicBool>,
    /// True when at least one connection has been seen with the given
    /// `Last-Event-ID` request header.
    saw_last_event_id: Arc<Mutex<Option<String>>>,
    stop: Arc<AtomicBool>,
}

struct Server {
    addr: SocketAddr,
    cfg: ServerCfg,
}

impl Server {
    fn url(&self) -> String {
        format!("http://{}/", self.addr)
    }
    fn stop(&self) {
        self.cfg.stop.store(true, Ordering::Relaxed);
        let _ = TcpStream::connect(self.addr);
    }
}

fn start_server(events: Vec<String>) -> Server {
    let cfg = ServerCfg {
        events,
        retry_ms: Some(50),
        fail_first: Arc::new(AtomicBool::new(false)),
        saw_last_event_id: Arc::new(Mutex::new(None)),
        stop: Arc::new(AtomicBool::new(false)),
    };
    start_server_with_cfg(cfg)
}

fn start_server_with_cfg(cfg: ServerCfg) -> Server {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let cfg_thread = cfg.clone();
    thread::spawn(move || {
        for stream in listener.incoming() {
            if cfg_thread.stop.load(Ordering::Relaxed) {
                break;
            }
            let Ok(stream) = stream else { continue };
            let cfg_clone = cfg_thread.clone();
            thread::spawn(move || {
                handle_connection(stream, cfg_clone);
            });
        }
    });
    Server { addr, cfg }
}

fn handle_connection(mut stream: TcpStream, cfg: ServerCfg) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));

    // Read request line + headers.
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut request_lines = Vec::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).is_err() {
            return;
        }
        if line == "\r\n" || line.is_empty() {
            break;
        }
        request_lines.push(line);
    }

    // Capture Last-Event-ID if present.
    for line in &request_lines {
        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("last-event-id") {
                *cfg.saw_last_event_id.lock().unwrap() = Some(value.trim().to_string());
            }
        }
    }

    if cfg.fail_first.swap(false, Ordering::Relaxed) {
        let _ = stream.write_all(
            b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\n\r\n",
        );
        return;
    }

    // 200 + text/event-stream headers.
    let _ = stream.write_all(
        b"HTTP/1.1 200 OK\r\n\
            Content-Type: text/event-stream\r\n\
            Cache-Control: no-cache\r\n\
            Connection: close\r\n\r\n",
    );

    // Optional retry: directive first.
    if let Some(ms) = cfg.retry_ms {
        let _ = stream.write_all(format!("retry: {ms}\n\n").as_bytes());
    }

    // Body — emit each event with a small delay so the parser can run
    // between them.
    for ev in &cfg.events {
        let _ = stream.write_all(ev.as_bytes());
        let _ = stream.flush();
        thread::sleep(Duration::from_millis(10));
    }

    // Idle for a bit — the EventSource may still be reading.
    thread::sleep(Duration::from_millis(100));
    let _ = stream.shutdown(std::net::Shutdown::Both);
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn run_js_with_url(url: &str, body: &str) -> Result<String, String> {
    // Enable dev mode so the SSRF guard lets us hit 127.0.0.1.
    set_dev_env();
    let src = format!(
        r#"
export async function run() {{
    const URL = "{url}";
{body}
}}
"#
    );
    let out = dispatch(m(&src), "run", "[]")?;
    Ok(out.json)
}

fn set_dev_env() {
    // SAFETY: env::set_var is `unsafe` to remind users that race
    // conditions exist on multi-threaded reads, but the test process
    // is single-threaded at this entry point and the SSRF guard is the
    // only consumer.
    #[allow(unsafe_code)]
    unsafe {
        std::env::set_var("ZEROSHIP_DEV", "1");
    }
}

// ---------------------------------------------------------------------------
// Smoke tests — constructor + shape (no network)
// ---------------------------------------------------------------------------

#[test]
fn eventsource_class_shape() {
    let r = run_js_with_url(
        "http://127.0.0.1:1/sse",
        r#"
    const es = new EventSource(URL);
    if (typeof es.readyState !== "number") throw new Error("readyState");
    if (es.readyState !== EventSource.CONNECTING)
        throw new Error("expected CONNECTING got " + es.readyState);
    if (typeof es.url !== "string") throw new Error("url");
    if (typeof es.close !== "function") throw new Error("close");
    if (typeof es.addEventListener !== "function")
        throw new Error("inherits EventTarget");
    if (es.withCredentials !== false) throw new Error("withCredentials");
    if (EventSource.CONNECTING !== 0) throw new Error("CONNECTING const");
    if (EventSource.OPEN !== 1) throw new Error("OPEN const");
    if (EventSource.CLOSED !== 2) throw new Error("CLOSED const");
    es.close();
    if (es.readyState !== EventSource.CLOSED)
        throw new Error("close should set CLOSED");
    return "ok";
"#,
    )
    .expect("class shape");
    assert_eq!(r, "\"ok\"");
}

#[test]
fn eventsource_invalid_url_throws() {
    let r = run_js_with_url(
        "http://127.0.0.1:1/sse",
        r#"
    let threw = false;
    try {
        new EventSource("not a valid url");
    } catch (e) {
        threw = e instanceof TypeError || e instanceof Error;
    }
    if (!threw) throw new Error("expected throw on invalid URL");
    return "ok";
"#,
    )
    .expect("invalid url");
    assert_eq!(r, "\"ok\"");
}

// ---------------------------------------------------------------------------
// E2E — receives `message` events
// ---------------------------------------------------------------------------

#[test]
fn eventsource_receives_message_events() {
    let server = start_server(vec![
        "data: hello\n\n".to_string(),
        "data: world\n\n".to_string(),
    ]);

    let r = run_js_with_url(
        &server.url(),
        r#"
    const es = new EventSource(URL);
    const messages = [];
    return await new Promise((resolve, reject) => {
        const t = setTimeout(() => {
            es.close();
            resolve("timeout: got " + messages.length + " messages");
        }, 2000);
        es.onmessage = (ev) => {
            messages.push(ev.data);
            if (messages.length === 2) {
                clearTimeout(t);
                es.close();
                resolve(messages.join("|"));
            }
        };
        es.onerror = () => {
            // Errors during read are normal (server closes); just
            // wait for messages to arrive first.
        };
    });
"#,
    );

    server.stop();
    let r = r.expect("messages");
    assert_eq!(r, "\"hello|world\"");
}

#[test]
fn eventsource_custom_event_types() {
    let server = start_server(vec![
        "event: ping\ndata: pong\n\n".to_string(),
        "event: tick\ndata: 1\n\n".to_string(),
    ]);

    let r = run_js_with_url(
        &server.url(),
        r#"
    const es = new EventSource(URL);
    const got = {};
    return await new Promise((resolve) => {
        const t = setTimeout(() => {
            es.close();
            resolve("timeout: " + JSON.stringify(got));
        }, 2000);
        es.addEventListener("ping", (ev) => { got.ping = ev.data; check(); });
        es.addEventListener("tick", (ev) => { got.tick = ev.data; check(); });
        function check() {
            if (got.ping && got.tick) {
                clearTimeout(t);
                es.close();
                resolve(got.ping + ":" + got.tick);
            }
        }
    });
"#,
    );

    server.stop();
    let r = r.expect("custom events");
    assert_eq!(r, "\"pong:1\"");
}

#[test]
fn eventsource_multiline_data_concatenates_with_newline() {
    let server = start_server(vec!["data: line1\ndata: line2\ndata: line3\n\n".to_string()]);

    let r = run_js_with_url(
        &server.url(),
        r#"
    const es = new EventSource(URL);
    return await new Promise((resolve) => {
        const t = setTimeout(() => { es.close(); resolve("timeout"); }, 2000);
        es.onmessage = (ev) => {
            clearTimeout(t);
            es.close();
            resolve(ev.data);
        };
    });
"#,
    );

    server.stop();
    let r = r.expect("multiline data");
    // Three data: lines concatenate with \n separator. The dispatch
    // helper hands us the raw JSON-stringified result; literal newlines
    // within JSON strings are NOT escaped in the value the helper
    // surfaces (which goes through serde_json::to_string).
    assert_eq!(r, "\"line1\nline2\nline3\"");
}

#[test]
fn eventsource_id_updates_last_event_id() {
    let server = start_server(vec![
        "id: 42\ndata: first\n\n".to_string(),
        "data: second\n\n".to_string(),
    ]);

    let r = run_js_with_url(
        &server.url(),
        r#"
    const es = new EventSource(URL);
    let got = [];
    return await new Promise((resolve) => {
        const t = setTimeout(() => { es.close(); resolve("timeout"); }, 2000);
        es.onmessage = (ev) => {
            got.push(ev.lastEventId);
            if (got.length === 2) {
                clearTimeout(t);
                es.close();
                resolve(got.join(","));
            }
        };
    });
"#,
    );

    server.stop();
    let r = r.expect("id updates");
    // First event has id="42"; second carries forward (since the last
    // seen id is 42 and no `id:` line resets it for the second event).
    assert_eq!(r, "\"42,42\"");
}

#[test]
fn eventsource_close_sets_closed_state() {
    let server = start_server(vec!["data: x\n\n".to_string()]);

    let r = run_js_with_url(
        &server.url(),
        r#"
    const es = new EventSource(URL);
    return await new Promise((resolve) => {
        const t = setTimeout(() => { es.close(); resolve("timeout"); }, 2000);
        es.onmessage = () => {
            es.close();
            const after = es.readyState;
            clearTimeout(t);
            resolve(String(after));
        };
    });
"#,
    );

    server.stop();
    let r = r.expect("close state");
    assert_eq!(r, "\"2\"");
}

// ---------------------------------------------------------------------------
// retry: directive
// ---------------------------------------------------------------------------

#[test]
fn eventsource_retry_directive_observed() {
    // The server emits one `retry: 25` directive then disconnects — we
    // observe the EventSource state transitions to CONNECTING (i.e. the
    // reconnect schedule fired).
    let server = start_server(vec!["retry: 25\ndata: hello\n\n".to_string()]);

    let r = run_js_with_url(
        &server.url(),
        r#"
    const es = new EventSource(URL);
    const transitions = [];
    return await new Promise((resolve) => {
        const t = setTimeout(() => {
            es.close();
            resolve(transitions.join(","));
        }, 1000);
        es.addEventListener("error", () => {
            transitions.push(es.readyState);
            if (transitions.length >= 2) {
                clearTimeout(t);
                es.close();
                resolve(transitions.join(","));
            }
        });
    });
"#,
    );

    server.stop();
    let r = r.expect("retry");
    // Each `error` event fires while readyState is CONNECTING (= 0), so
    // we expect "0,0" — at least two reconnect attempts within 1s.
    assert!(
        r.contains("0,0"),
        "expected at least two reconnects; got {r}",
    );
}
