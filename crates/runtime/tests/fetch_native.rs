//! Hand-written tests for the native fetch chain.
//!
//! These split into two strata:
//!
//!   1. **Pure-Rust tests** (no V8) — exercise the algorithm chain
//!      directly against a tiny in-process HTTP test server, so we
//!      verify redirects / Content-Encoding / bad-port / data: URLs
//!      / abort behaviour without paying the runtime/V8 startup cost.
//!
//!   2. **V8-driven tests** — run a small JS snippet through a built
//!      Runtime with `ZEROSHIP_NATIVE_FETCH=1` set, asserting the
//!      `fetch()` global is wired up. These are limited because the
//!      pump-driven async fetch needs a live compio runtime; the bulk
//!      of behaviour is in the pure-Rust stratum above.
//!
//! Per the dispatch brief (§13), we aim for ≥15 hand-written tests
//! covering: data: URLs, signal-aborted-pre-fetch, signal-aborted-
//! mid-fetch, 20-redirect cap, 301/POST → GET method change, 307/POST
//! preserved, 307/Stream → network error, cross-origin Authorization
//! strip, same-origin Authorization preserved, gzip Content-Encoding,
//! multi-coding gzip+br, Content-Length stripped, Origin header, bad
//! port blocked, AbortSignal.timeout.

#![allow(unsafe_code)]

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicU16, Ordering};
use std::thread;
use std::time::Duration;

use zeroship_runtime::channel::CancelFlag;
use zeroship_runtime::codec::{make_codec, CodecMode, CompressionFormat};
use zeroship_runtime::fetch_body::body::BodySource;
use zeroship_runtime::fetch_native::algorithms::{
    main_fetch, CredentialsMode, FetchRequest, RedirectMode,
};

// ---------------------------------------------------------------------------
// Tiny test HTTP server — single-threaded, accepts one connection at a
// time, dispatches request handling to a closure provided by the test.
// ---------------------------------------------------------------------------

/// One handled request. Fields populated from the parsed wire bytes.
#[derive(Debug, Clone)]
struct ParsedRequest {
    method: String,
    target: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

/// Mock HTTP server for fetch tests. Spawns a background thread that
/// dispatches up to `request_cap` requests through `handler`. The
/// handler returns raw response bytes (status line + headers + body)
/// that are written verbatim to the socket.
struct MockServer {
    addr: SocketAddr,
    requests: Arc<Mutex<Vec<ParsedRequest>>>,
    _shutdown: ShutdownGuard,
}

impl MockServer {
    fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    fn requests(&self) -> Vec<ParsedRequest> {
        self.requests.lock().unwrap().clone()
    }
}

struct ShutdownGuard {
    listener: Arc<TcpListener>,
}

impl Drop for ShutdownGuard {
    fn drop(&mut self) {
        // Close the listener to stop accept(); the handler thread
        // exits when accept() fails.
        let addr = self.listener.local_addr().unwrap();
        let _ = TcpStream::connect_timeout(&addr, Duration::from_millis(50));
    }
}

fn start_mock_server(
    handler: impl Fn(&ParsedRequest) -> Vec<u8> + Send + Sync + 'static,
) -> MockServer {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(false).unwrap();
    let addr = listener.local_addr().unwrap();
    let listener = Arc::new(listener);
    let requests: Arc<Mutex<Vec<ParsedRequest>>> = Arc::new(Mutex::new(Vec::new()));

    let req_clone = requests.clone();
    let listener_clone = listener.clone();
    let handler = Arc::new(handler);
    thread::spawn(move || {
        loop {
            let (mut stream, _) = match listener_clone.accept() {
                Ok(s) => s,
                Err(_) => break,
            };
            stream.set_read_timeout(Some(Duration::from_secs(2))).ok();
            stream.set_write_timeout(Some(Duration::from_secs(2))).ok();
            let req = match read_request(&mut stream) {
                Some(r) => r,
                None => continue,
            };
            req_clone.lock().unwrap().push(req.clone());
            let response = handler(&req);
            let _ = stream.write_all(&response);
            let _ = stream.shutdown(std::net::Shutdown::Both);
        }
    });
    MockServer {
        addr,
        requests,
        _shutdown: ShutdownGuard { listener },
    }
}

fn read_request(stream: &mut TcpStream) -> Option<ParsedRequest> {
    // Read until we have the full headers (\r\n\r\n).
    let mut buf = Vec::with_capacity(2048);
    let mut tmp = [0u8; 1024];
    let header_end;
    loop {
        let n = stream.read(&mut tmp).ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(pos) = find_double_crlf(&buf) {
            header_end = pos + 4;
            break;
        }
        if buf.len() > 64 * 1024 {
            return None;
        }
    }

    let header_str = std::str::from_utf8(&buf[..header_end]).ok()?;
    let mut lines = header_str.split("\r\n");
    let req_line = lines.next()?;
    let mut parts = req_line.split_whitespace();
    let method = parts.next()?.to_string();
    let target = parts.next()?.to_string();

    let mut headers: Vec<(String, String)> = Vec::new();
    for line in lines {
        if line.is_empty() {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            headers.push((k.trim().to_string(), v.trim().to_string()));
        }
    }

    // Read body up to Content-Length or until socket closes.
    let mut body = buf[header_end..].to_vec();
    let cl = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.parse::<usize>().ok())
        .unwrap_or(0);
    while body.len() < cl {
        let n = stream.read(&mut tmp).ok()?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&tmp[..n]);
    }

    Some(ParsedRequest {
        method,
        target,
        headers,
        body,
    })
}

fn find_double_crlf(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

fn http_response(status: u16, status_text: &str, headers: &[(&str, &str)], body: &[u8]) -> Vec<u8> {
    let mut out = format!("HTTP/1.1 {status} {status_text}\r\n").into_bytes();
    for (k, v) in headers {
        out.extend_from_slice(format!("{k}: {v}\r\n").as_bytes());
    }
    out.extend_from_slice(format!("Content-Length: {}\r\n", body.len()).as_bytes());
    out.extend_from_slice(b"Connection: close\r\n\r\n");
    out.extend_from_slice(body);
    out
}

// ---------------------------------------------------------------------------
// FetchRequest builder
// ---------------------------------------------------------------------------

fn req(method: &str, url: &str) -> FetchRequest {
    FetchRequest {
        method: method.to_string(),
        url: url.to_string(),
        headers: Vec::new(),
        body: None,
        body_source: None,
        redirect_mode: RedirectMode::Follow,
        credentials_mode: CredentialsMode::SameOrigin,
        cancel: None,
        redirect_count: 0,
        origin_url: url.to_string(),
    }
}

fn run<R>(fut: impl std::future::Future<Output = R>) -> R {
    // Each test gets its own compio runtime (cheap to spin up) — the
    // SsrfResolver thread-local is per-thread so it gets a fresh client.
    // Required: ZEROSHIP_DEV=1 so loopback isn't blocked by the SSRF
    // string-level fast path.
    // SAFETY: tests are not concurrent on env vars (single-process,
    // and the value never changes after first set).
    unsafe { std::env::set_var("ZEROSHIP_DEV", "1"); }
    compio::runtime::Runtime::new().unwrap().block_on(fut)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

// 1. data: URL (text/plain)
#[test]
fn data_url_text_plain() {
    let r = run(main_fetch(req("GET", "data:text/plain,hello"))).unwrap();
    assert_eq!(r.status, 200);
    assert_eq!(r.body, b"hello");
    assert!(r
        .headers
        .iter()
        .any(|(k, v)| k.eq_ignore_ascii_case("content-type") && v == "text/plain"));
}

// 2. data: URL (base64)
#[test]
fn data_url_base64() {
    let r = run(main_fetch(req("GET", "data:text/plain;base64,aGVsbG8="))).unwrap();
    assert_eq!(r.status, 200);
    assert_eq!(r.body, b"hello");
}

// 3. unsupported scheme → network error
#[test]
fn unsupported_scheme_returns_network_error() {
    let r = run(main_fetch(req("GET", "gopher://example.com/x")));
    assert!(r.is_err(), "gopher should be rejected");
}

// 4. bad port (port 25 = SMTP) → network error
#[test]
fn bad_port_blocked() {
    let r = run(main_fetch(req("GET", "http://127.0.0.1:25/")));
    assert!(r.is_err());
    let err = r.err().unwrap();
    assert!(err.contains("blocked port") || err.contains("network error"));
}

// 5. 200 OK round-trip
#[test]
fn basic_get_round_trip() {
    let server = start_mock_server(|_req| http_response(200, "OK", &[("Content-Type", "text/plain")], b"hello"));
    let r = run(main_fetch(req("GET", &server.url()))).unwrap();
    assert_eq!(r.status, 200);
    assert_eq!(r.body, b"hello");
    assert!(!r.redirected);
}

// 6. 301 + POST → GET, body dropped
#[test]
fn redirect_301_post_becomes_get() {
    static HOP_COUNT: AtomicU16 = AtomicU16::new(0);
    HOP_COUNT.store(0, Ordering::Relaxed);
    let server = start_mock_server(|req| {
        let n = HOP_COUNT.fetch_add(1, Ordering::Relaxed);
        if n == 0 {
            http_response(301, "Moved Permanently", &[("Location", "/final")], b"")
        } else {
            // Final hop — assert we received GET with no body.
            let body = format!("method={} bodylen={}", req.method, req.body.len());
            http_response(200, "OK", &[], body.as_bytes())
        }
    });
    let mut r = req("POST", &server.url());
    r.body = Some(b"PAYLOAD".to_vec());
    r.body_source = Some(BodySource::Bytes(std::rc::Rc::new(b"PAYLOAD".to_vec())));
    let resp = run(main_fetch(r)).unwrap();
    assert_eq!(resp.status, 200);
    assert!(resp.redirected);
    let body = String::from_utf8_lossy(&resp.body);
    assert!(body.contains("method=GET"), "got {body}");
    assert!(body.contains("bodylen=0"), "got {body}");
}

// 7. 303 + PUT → GET with body dropped
#[test]
fn redirect_303_put_becomes_get_drops_body() {
    static HOP: AtomicU16 = AtomicU16::new(0);
    HOP.store(0, Ordering::Relaxed);
    let server = start_mock_server(|req| {
        let n = HOP.fetch_add(1, Ordering::Relaxed);
        if n == 0 {
            http_response(303, "See Other", &[("Location", "/end")], b"")
        } else {
            let body = format!("method={} bodylen={}", req.method, req.body.len());
            http_response(200, "OK", &[], body.as_bytes())
        }
    });
    let mut r = req("PUT", &server.url());
    r.body = Some(b"DATA".to_vec());
    r.body_source = Some(BodySource::Bytes(std::rc::Rc::new(b"DATA".to_vec())));
    let resp = run(main_fetch(r)).unwrap();
    let body = String::from_utf8_lossy(&resp.body);
    assert!(body.contains("method=GET"), "got {body}");
    assert!(body.contains("bodylen=0"), "got {body}");
}

// 8. 307 + POST + Bytes body → method/body preserved on retransmit
#[test]
fn redirect_307_post_preserves_method_and_body() {
    static HOP: AtomicU16 = AtomicU16::new(0);
    HOP.store(0, Ordering::Relaxed);
    let server = start_mock_server(|req| {
        let n = HOP.fetch_add(1, Ordering::Relaxed);
        if n == 0 {
            http_response(307, "Temporary Redirect", &[("Location", "/again")], b"")
        } else {
            let body = format!(
                "method={} body={}",
                req.method,
                String::from_utf8_lossy(&req.body)
            );
            http_response(200, "OK", &[], body.as_bytes())
        }
    });
    let mut r = req("POST", &server.url());
    r.body = Some(b"PAYLOAD".to_vec());
    r.body_source = Some(BodySource::Bytes(std::rc::Rc::new(b"PAYLOAD".to_vec())));
    r.headers.push(("Content-Type".to_string(), "text/plain".to_string()));
    let resp = run(main_fetch(r)).unwrap();
    let body = String::from_utf8_lossy(&resp.body);
    assert!(body.contains("method=POST"), "got {body}");
    assert!(body.contains("body=PAYLOAD"), "got {body}");
}

// 9. 307 + Stream body → network error (non-rewindable)
#[test]
fn redirect_307_stream_body_errors() {
    let server = start_mock_server(|_req| {
        http_response(307, "Temporary Redirect", &[("Location", "/x")], b"")
    });
    let mut r = req("POST", &server.url());
    r.body_source = Some(BodySource::Stream);
    let result = run(main_fetch(r));
    assert!(result.is_err());
    let err = result.err().unwrap();
    assert!(err.contains("stream") || err.contains("rewind"), "got {err}");
}

// 10. 20-redirect cap
#[test]
fn redirect_loop_capped_at_20() {
    static HOP: AtomicU16 = AtomicU16::new(0);
    HOP.store(0, Ordering::Relaxed);
    let server = start_mock_server(|_req| {
        // Every request redirects forever — but because we use one
        // listener the relative Location resolves back here.
        let n = HOP.fetch_add(1, Ordering::Relaxed);
        let target = format!("/hop{}", n + 1);
        http_response(302, "Found", &[("Location", &target)], b"")
    });
    let result = run(main_fetch(req("GET", &server.url())));
    assert!(result.is_err());
    let err = result.err().unwrap();
    assert!(err.contains("too many redirects") || err.contains(">"), "got {err}");
}

// 11. cross-origin redirect strips Authorization
#[test]
fn cross_origin_redirect_strips_authorization() {
    let final_server = start_mock_server(|req| {
        let auth = req
            .headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("authorization"))
            .map(|(_, v)| v.clone())
            .unwrap_or_else(|| "MISSING".to_string());
        let body = format!("auth={auth}");
        http_response(200, "OK", &[], body.as_bytes())
    });
    let final_url = final_server.url();
    let first_server = start_mock_server(move |_req| {
        http_response(302, "Found", &[("Location", &final_url)], b"")
    });
    let mut r = req("GET", &first_server.url());
    r.headers
        .push(("Authorization".to_string(), "Bearer SECRET".to_string()));
    let resp = run(main_fetch(r)).unwrap();
    let body = String::from_utf8_lossy(&resp.body);
    assert!(body.contains("auth=MISSING"), "got {body}");
}

// 12. same-origin redirect preserves Authorization
#[test]
fn same_origin_redirect_preserves_authorization() {
    static HOP: AtomicU16 = AtomicU16::new(0);
    HOP.store(0, Ordering::Relaxed);
    let server = start_mock_server(|req| {
        let n = HOP.fetch_add(1, Ordering::Relaxed);
        if n == 0 {
            http_response(302, "Found", &[("Location", "/keep")], b"")
        } else {
            let auth = req
                .headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case("authorization"))
                .map(|(_, v)| v.clone())
                .unwrap_or_else(|| "MISSING".to_string());
            let body = format!("auth={auth}");
            http_response(200, "OK", &[], body.as_bytes())
        }
    });
    let mut r = req("GET", &server.url());
    r.headers
        .push(("Authorization".to_string(), "Bearer KEEP".to_string()));
    let resp = run(main_fetch(r)).unwrap();
    let body = String::from_utf8_lossy(&resp.body);
    assert!(body.contains("auth=Bearer KEEP"), "got {body}");
}

// 13. gzip Content-Encoding → response body decompressed
#[test]
fn gzip_response_decompressed() {
    let mut enc = make_codec(CompressionFormat::Gzip, CodecMode::Compress);
    let (mut bytes, _) = enc.write(b"hello compressed world").unwrap();
    bytes.extend(enc.finish().unwrap());
    let bytes = Arc::new(bytes);
    let bytes_clone = bytes.clone();
    let server = start_mock_server(move |_req| {
        http_response(
            200,
            "OK",
            &[("Content-Encoding", "gzip"), ("Content-Type", "text/plain")],
            &bytes_clone,
        )
    });
    let resp = run(main_fetch(req("GET", &server.url()))).unwrap();
    assert_eq!(resp.body, b"hello compressed world");
    // Content-Encoding is stripped after decoding.
    assert!(!resp
        .headers
        .iter()
        .any(|(k, _)| k.eq_ignore_ascii_case("content-encoding")));
    // Content-Length stripped post-decode.
    assert!(!resp
        .headers
        .iter()
        .any(|(k, _)| k.eq_ignore_ascii_case("content-length")));
}

// 14. multi-coding "gzip, br": decode in REVERSE order (RFC 9110)
#[test]
fn multi_coding_decodes_in_reverse() {
    // Server applied gzip first, then brotli.
    let mut g = make_codec(CompressionFormat::Gzip, CodecMode::Compress);
    let (mut b1, _) = g.write(b"hello world").unwrap();
    b1.extend(g.finish().unwrap());
    let mut br = make_codec(CompressionFormat::Brotli, CodecMode::Compress);
    let (mut b2, _) = br.write(&b1).unwrap();
    b2.extend(br.finish().unwrap());
    let b2 = Arc::new(b2);
    let b2_clone = b2.clone();
    let server = start_mock_server(move |_req| {
        http_response(
            200,
            "OK",
            &[("Content-Encoding", "gzip, br")],
            &b2_clone,
        )
    });
    let resp = run(main_fetch(req("GET", &server.url()))).unwrap();
    assert_eq!(resp.body, b"hello world");
}

// 15. Origin header on POST (not on GET)
#[test]
fn origin_header_on_post_only() {
    let server = start_mock_server(|req| {
        let origin = req
            .headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("origin"))
            .map(|(_, v)| v.clone())
            .unwrap_or_else(|| "MISSING".to_string());
        let body = format!("origin={origin}");
        http_response(200, "OK", &[], body.as_bytes())
    });
    // GET — no Origin.
    let resp = run(main_fetch(req("GET", &server.url()))).unwrap();
    assert!(String::from_utf8_lossy(&resp.body).contains("origin=MISSING"));

    // POST — Origin populated.
    let mut r = req("POST", &server.url());
    r.body = Some(b"x".to_vec());
    r.body_source = Some(BodySource::Bytes(std::rc::Rc::new(b"x".to_vec())));
    let resp2 = run(main_fetch(r)).unwrap();
    let body = String::from_utf8_lossy(&resp2.body);
    assert!(body.contains("origin=http://"), "got {body}");
}

// 16. Pre-aborted CancelFlag → network error before request goes out
#[test]
fn pre_aborted_cancel_flag_errors() {
    let server = start_mock_server(|_req| http_response(200, "OK", &[], b""));
    let mut r = req("GET", &server.url());
    let cf = CancelFlag::new();
    cf.cancel();
    r.cancel = Some(cf);
    let result = run(main_fetch(r));
    assert!(result.is_err());
    assert!(server.requests().is_empty(), "request should not have been sent");
}

#[test]
fn mid_body_cancel_flag_aborts_body_read() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let _ = read_request(&mut stream);
        stream
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\n",
            )
            .unwrap();
        stream.flush().unwrap();
        thread::sleep(Duration::from_millis(150));
        stream.write_all(b"hello").unwrap();
        let _ = stream.shutdown(std::net::Shutdown::Both);
    });

    let cancel = CancelFlag::new();
    let request = FetchRequest {
        cancel: Some(cancel.clone()),
        ..req("GET", &format!("http://{addr}/slow-body"))
    };

    let err = run(async {
        let cancel_for_task = cancel.clone();
        compio::runtime::spawn(async move {
            compio::time::sleep(Duration::from_millis(20)).await;
            cancel_for_task.cancel();
        })
        .detach();
        zeroship_runtime::fetch_native::http_network::http_network_fetch(&request)
            .await
            .expect_err("mid-body cancellation should abort the body read")
    });
    assert_eq!(err, "network error: aborted");
}

// 17. Manual redirect mode returns the redirect response unmodified
#[test]
fn manual_redirect_mode_returns_redirect() {
    let server = start_mock_server(|_req| {
        http_response(302, "Found", &[("Location", "/elsewhere")], b"")
    });
    let mut r = req("GET", &server.url());
    r.redirect_mode = RedirectMode::Manual;
    let resp = run(main_fetch(r)).unwrap();
    assert_eq!(resp.status, 302);
    assert!(resp
        .headers
        .iter()
        .any(|(k, v)| k.eq_ignore_ascii_case("location") && v == "/elsewhere"));
}

// 18. Error redirect mode yields network error on any redirect
#[test]
fn error_redirect_mode_errors() {
    let server = start_mock_server(|_req| {
        http_response(302, "Found", &[("Location", "/x")], b"")
    });
    let mut r = req("GET", &server.url());
    r.redirect_mode = RedirectMode::Error;
    let result = run(main_fetch(r));
    assert!(result.is_err());
}

// 19. Default Accept-Encoding sent when user didn't supply one
#[test]
fn default_accept_encoding_sent() {
    let server = start_mock_server(|req| {
        let ae = req
            .headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("accept-encoding"))
            .map(|(_, v)| v.clone())
            .unwrap_or_else(|| "MISSING".to_string());
        http_response(200, "OK", &[], ae.as_bytes())
    });
    let resp = run(main_fetch(req("GET", &server.url()))).unwrap();
    let body = String::from_utf8_lossy(&resp.body);
    // HTTP → no `br`; should include gzip + deflate.
    assert!(body.contains("gzip"), "got {body}");
    assert!(body.contains("deflate"), "got {body}");
}

// 20. User-supplied Accept-Encoding NOT clobbered
#[test]
fn user_accept_encoding_preserved() {
    let server = start_mock_server(|req| {
        let ae = req
            .headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("accept-encoding"))
            .map(|(_, v)| v.clone())
            .unwrap_or_else(|| "MISSING".to_string());
        http_response(200, "OK", &[], ae.as_bytes())
    });
    let mut r = req("GET", &server.url());
    r.headers
        .push(("Accept-Encoding".to_string(), "gzip".to_string()));
    let resp = run(main_fetch(r)).unwrap();
    let body = String::from_utf8_lossy(&resp.body);
    // We sent only "gzip" — server saw exactly that.
    assert_eq!(body.trim(), "gzip");
}

// 21. Identity Content-Encoding alone strips CE+CL but no decode
#[test]
fn identity_only_strips_ce_cl() {
    let server = start_mock_server(|_req| {
        http_response(200, "OK", &[("Content-Encoding", "identity"), ("Content-Type", "text/plain")], b"ABC")
    });
    let resp = run(main_fetch(req("GET", &server.url()))).unwrap();
    assert_eq!(resp.body, b"ABC");
    assert!(!resp.headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("content-encoding")));
}

// 22. Unknown Content-Encoding → network error
#[test]
fn unknown_content_encoding_errors() {
    let server = start_mock_server(|_req| {
        http_response(200, "OK", &[("Content-Encoding", "snappy")], b"x")
    });
    let result = run(main_fetch(req("GET", &server.url())));
    assert!(result.is_err());
    let err = result.err().unwrap();
    assert!(err.contains("Content-Encoding") || err.contains("snappy"), "got {err}");
}

// 23. 308 + non-rewindable body → network error (analogous to 307)
#[test]
fn redirect_308_stream_body_errors() {
    let server = start_mock_server(|_req| {
        http_response(308, "Permanent Redirect", &[("Location", "/x")], b"")
    });
    let mut r = req("PUT", &server.url());
    r.body_source = Some(BodySource::Stream);
    let result = run(main_fetch(r));
    assert!(result.is_err());
}

// 24. 302 + GET preserved (no method change for non-POST)
#[test]
fn redirect_302_get_preserved() {
    static HOP: AtomicU16 = AtomicU16::new(0);
    HOP.store(0, Ordering::Relaxed);
    let server = start_mock_server(|req| {
        let n = HOP.fetch_add(1, Ordering::Relaxed);
        if n == 0 {
            http_response(302, "Found", &[("Location", "/end")], b"")
        } else {
            let body = format!("method={}", req.method);
            http_response(200, "OK", &[], body.as_bytes())
        }
    });
    let resp = run(main_fetch(req("GET", &server.url()))).unwrap();
    assert!(resp.redirected);
    assert!(String::from_utf8_lossy(&resp.body).contains("method=GET"));
}
