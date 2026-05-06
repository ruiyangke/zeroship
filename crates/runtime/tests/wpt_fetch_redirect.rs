//! WPT runner for `fetch/api/redirect/`:
//!
//!   * `redirect-method.any.js`   — per-status method/body mutation.
//!   * `redirect-mode.any.js`     — RedirectMode follow/error/manual.
//!   * `redirect-origin.any.js`   — cross-origin Authorization stripping.
//!
//! WPT's redirect tests rely on a Python testserver
//! (`fetch/api/resources/redirect.py`) accepting
//! `?redirect_status=&location=` and an in-process multi-server
//! harness for cross-origin scenarios. Rather than vendor the
//! testserver, we drive the same scenarios via a flexible in-process
//! HTTP fixture (`Server`) that lets each test wire up its own
//! redirect-target and assertion, then invoke `main_fetch` directly.
//!
//! This isn't a literal WPT runner (no testharness.js) but mirrors
//! the spec coverage WPT exercises against the algorithm chain
//! (`crates/runtime/src/fetch_native/`).
//!
//! Pass criterion: ≥70% of the cases per the brief.

#![allow(unsafe_code)]

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use zeroship_runtime::fetch_body::body::BodySource;
use zeroship_runtime::fetch_native::algorithms::{
    main_fetch, CredentialsMode, FetchRequest, RedirectMode,
};

#[derive(Debug, Clone)]
struct ParsedReq {
    method: String,
    body: Vec<u8>,
}

struct Server {
    addr: SocketAddr,
    requests: Arc<Mutex<Vec<ParsedReq>>>,
    _listener: Arc<TcpListener>,
}

impl Server {
    fn url(&self) -> String {
        format!("http://{}/", self.addr)
    }
    fn last_method(&self) -> Option<String> {
        self.requests
            .lock()
            .unwrap()
            .last()
            .map(|r| r.method.clone())
    }
    fn last_body(&self) -> Option<Vec<u8>> {
        self.requests
            .lock()
            .unwrap()
            .last()
            .map(|r| r.body.clone())
    }
}

fn start_server(redirect_status: u16) -> Server {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let listener = Arc::new(listener);
    let requests: Arc<Mutex<Vec<ParsedReq>>> = Arc::new(Mutex::new(Vec::new()));
    let req_clone = requests.clone();
    let lst_clone = listener.clone();
    let hop_count = Arc::new(AtomicU16::new(0));
    thread::spawn(move || loop {
        let (mut stream, _) = match lst_clone.accept() {
            Ok(s) => s,
            Err(_) => break,
        };
        stream.set_read_timeout(Some(Duration::from_secs(2))).ok();
        stream.set_write_timeout(Some(Duration::from_secs(2))).ok();
        let r = match read_req(&mut stream) {
            Some(r) => r,
            None => continue,
        };
        req_clone.lock().unwrap().push(r.clone());

        let n = hop_count.fetch_add(1, Ordering::Relaxed);
        let response = if n == 0 {
            // First hop — redirect.
            format!(
                "HTTP/1.1 {redirect_status} Redirect\r\nLocation: /final\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            )
            .into_bytes()
        } else {
            // Final hop — echo the method + body.
            let body = format!("method={};body={}", r.method, String::from_utf8_lossy(&r.body));
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            resp.into_bytes()
        };
        let _ = stream.write_all(&response);
        let _ = stream.shutdown(std::net::Shutdown::Both);
    });
    Server {
        addr,
        requests,
        _listener: listener,
    }
}

fn read_req(stream: &mut TcpStream) -> Option<ParsedReq> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 1024];
    let header_end;
    loop {
        let n = stream.read(&mut tmp).ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            header_end = p + 4;
            break;
        }
        if buf.len() > 64 * 1024 {
            return None;
        }
    }
    let header_str = std::str::from_utf8(&buf[..header_end]).ok()?;
    let mut lines = header_str.split("\r\n");
    let req_line = lines.next()?;
    let method = req_line.split_whitespace().next()?.to_string();
    let mut headers: Vec<(String, String)> = Vec::new();
    for line in lines {
        if line.is_empty() {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            headers.push((k.trim().to_string(), v.trim().to_string()));
        }
    }
    let cl = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.parse::<usize>().ok())
        .unwrap_or(0);
    let mut body = buf[header_end..].to_vec();
    while body.len() < cl {
        let n = stream.read(&mut tmp).ok()?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&tmp[..n]);
    }
    Some(ParsedReq { method, body })
}

fn run<R>(fut: impl std::future::Future<Output = R>) -> R {
    unsafe { std::env::set_var("ZEROSHIP_DEV", "1"); }
    compio::runtime::Runtime::new().unwrap().block_on(fut)
}

#[derive(Debug)]
enum Outcome {
    Pass,
    Fail(String),
}

/// Mirrors WPT's `redirectMethod(desc, _url, _location, redirectStatus,
/// method, expectedMethod, opts)` shape.
fn redirect_method(
    desc: &str,
    redirect_status: u16,
    method: &str,
    expected_method: &str,
    body: Option<&[u8]>,
    expected_body: Option<&[u8]>,
) -> Outcome {
    let server = start_server(redirect_status);
    let mut r = FetchRequest {
        method: method.to_string(),
        url: server.url(),
        headers: Vec::new(),
        body: body.map(|b| b.to_vec()),
        body_source: body.map(|b| BodySource::Bytes(std::rc::Rc::new(b.to_vec()))),
        redirect_mode: RedirectMode::Follow,
        credentials_mode: CredentialsMode::SameOrigin,
        cancel: None,
        redirect_count: 0,
        origin_url: server.url(),
    };
    if let Some(_b) = body {
        r.headers
            .push(("Content-Type".to_string(), "text/plain".to_string()));
    }
    let resp = run(main_fetch(r));
    let resp = match resp {
        Ok(x) => x,
        Err(e) => return Outcome::Fail(format!("{desc}: fetch errored: {e}")),
    };
    if resp.status != 200 {
        return Outcome::Fail(format!("{desc}: status {} (want 200)", resp.status));
    }
    let last_m = server.last_method().unwrap_or_default();
    if last_m != expected_method {
        return Outcome::Fail(format!(
            "{desc}: server saw method {} (want {})",
            last_m, expected_method
        ));
    }
    let last_body = server.last_body().unwrap_or_default();
    let want = expected_body.unwrap_or(b"");
    if last_body != want {
        return Outcome::Fail(format!(
            "{desc}: server saw body {:?} (want {:?})",
            String::from_utf8_lossy(&last_body),
            String::from_utf8_lossy(want)
        ));
    }
    Outcome::Pass
}

#[test]
fn wpt_fetch_redirect_method() {
    let mut results: Vec<(String, Outcome)> = Vec::new();

    // 301 — GET stays GET; HEAD stays HEAD; POST → GET (body dropped).
    for &(method, expected_method) in &[("GET", "GET"), ("HEAD", "HEAD"), ("POST", "GET")] {
        results.push((
            format!("301 {method} -> {expected_method}"),
            redirect_method(
                &format!("301 {method}"),
                301,
                method,
                expected_method,
                if method == "POST" { Some(b"data") } else { None },
                Some(b""),
            ),
        ));
    }

    // 302 — same as 301: POST → GET, others preserved.
    for &(method, expected_method) in &[("GET", "GET"), ("HEAD", "HEAD"), ("POST", "GET")] {
        results.push((
            format!("302 {method} -> {expected_method}"),
            redirect_method(
                &format!("302 {method}"),
                302,
                method,
                expected_method,
                if method == "POST" { Some(b"x") } else { None },
                Some(b""),
            ),
        ));
    }

    // 303 — all non-{GET,HEAD} → GET, body dropped.
    for &(method, expected_method) in
        &[("GET", "GET"), ("HEAD", "HEAD"), ("POST", "GET"), ("PUT", "GET")]
    {
        results.push((
            format!("303 {method} -> {expected_method}"),
            redirect_method(
                &format!("303 {method}"),
                303,
                method,
                expected_method,
                if method == "POST" || method == "PUT" {
                    Some(b"data")
                } else {
                    None
                },
                Some(b""),
            ),
        ));
    }

    // 307 — method preserved; body preserved (rewindable).
    for &(method, body) in &[
        ("GET", None::<&[u8]>),
        ("HEAD", None),
        ("POST", Some(b"hello".as_slice())),
        ("PUT", Some(b"data".as_slice())),
    ] {
        results.push((
            format!("307 {method} preserved"),
            redirect_method(
                &format!("307 {method}"),
                307,
                method,
                method,
                body,
                Some(body.unwrap_or(b"")),
            ),
        ));
    }

    // 308 — same as 307.
    for &(method, body) in &[
        ("GET", None::<&[u8]>),
        ("POST", Some(b"hi".as_slice())),
        ("PUT", Some(b"data".as_slice())),
    ] {
        results.push((
            format!("308 {method} preserved"),
            redirect_method(
                &format!("308 {method}"),
                308,
                method,
                method,
                body,
                Some(body.unwrap_or(b"")),
            ),
        ));
    }

    let pass = results.iter().filter(|(_, o)| matches!(o, Outcome::Pass)).count();
    let fail = results.len() - pass;

    eprintln!("\n=== WPT fetch/api/redirect/redirect-method results ===");
    for (name, outcome) in &results {
        match outcome {
            Outcome::Pass => eprintln!("  PASS  {name}"),
            Outcome::Fail(why) => eprintln!("  FAIL  {name}: {why}"),
        }
    }
    let pct = (pass as f64 / results.len() as f64) * 100.0;
    eprintln!("  total pass={pass} fail={fail}  rate={pct:.1}%");

    assert!(
        pass * 100 >= results.len() * 70,
        "expected ≥70% pass rate; got {pct:.1}% ({pass}/{})",
        results.len()
    );
}

// ---------------------------------------------------------------------------
// Flexible server for redirect-mode + redirect-origin scenarios
// ---------------------------------------------------------------------------

/// A captured request seen by the flex server (method, headers, body).
#[derive(Debug, Clone)]
struct CapturedReq {
    method: String,
    headers: Vec<(String, String)>,
    #[allow(dead_code)]
    body: Vec<u8>,
}

/// Recorded request capture by hop index. Used to assert outbound
/// headers / methods on each hop independently.
struct FlexServer {
    addr: SocketAddr,
    requests: Arc<Mutex<Vec<CapturedReq>>>,
    _listener: Arc<TcpListener>,
}

impl FlexServer {
    fn url(&self) -> String {
        format!("http://{}/", self.addr)
    }
}

/// A pre-baked hop response: produces a redirect targeting `next_url` /
/// returns 200 with `final_body`.
#[derive(Clone, Debug)]
enum HopResponse {
    Redirect {
        status: u16,
        location: String,
    },
    /// 200 OK that echoes nothing (used as the "tail" hop).
    Final200,
}

fn write_hop(stream: &mut TcpStream, hop: &HopResponse) {
    let bytes = match hop {
        HopResponse::Redirect { status, location } => format!(
            "HTTP/1.1 {status} Redirect\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        )
        .into_bytes(),
        HopResponse::Final200 => b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 2\r\nConnection: close\r\n\r\nOK".to_vec(),
    };
    let _ = stream.write_all(&bytes);
    let _ = stream.shutdown(std::net::Shutdown::Both);
}

/// Start a server with a fixed sequence of hop responses. The first
/// request gets `hops[0]`, the second `hops[1]`, etc. After exhausting
/// the list, returns 404.
fn start_flex(hops: Vec<HopResponse>) -> FlexServer {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let listener = Arc::new(listener);
    let requests: Arc<Mutex<Vec<CapturedReq>>> = Arc::new(Mutex::new(Vec::new()));
    let req_clone = requests.clone();
    let lst_clone = listener.clone();
    let counter = Arc::new(AtomicU16::new(0));
    thread::spawn(move || loop {
        let (mut stream, _) = match lst_clone.accept() {
            Ok(s) => s,
            Err(_) => break,
        };
        stream.set_read_timeout(Some(Duration::from_secs(2))).ok();
        stream.set_write_timeout(Some(Duration::from_secs(2))).ok();
        let r = match read_full_req(&mut stream) {
            Some(r) => r,
            None => continue,
        };
        req_clone.lock().unwrap().push(r);
        let n = counter.fetch_add(1, Ordering::Relaxed) as usize;
        if let Some(hop) = hops.get(n) {
            write_hop(&mut stream, hop);
        } else {
            let _ = stream.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
            let _ = stream.shutdown(std::net::Shutdown::Both);
        }
    });
    FlexServer {
        addr,
        requests,
        _listener: listener,
    }
}

/// Like `read_req` but captures all headers (used for cross-origin
/// Authorization-strip assertion).
fn read_full_req(stream: &mut TcpStream) -> Option<CapturedReq> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 1024];
    let header_end;
    loop {
        let n = stream.read(&mut tmp).ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            header_end = p + 4;
            break;
        }
        if buf.len() > 64 * 1024 {
            return None;
        }
    }
    let header_str = std::str::from_utf8(&buf[..header_end]).ok()?;
    let mut lines = header_str.split("\r\n");
    let req_line = lines.next()?;
    let method = req_line.split_whitespace().next()?.to_string();
    let mut headers: Vec<(String, String)> = Vec::new();
    for line in lines {
        if line.is_empty() {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            headers.push((k.trim().to_string(), v.trim().to_string()));
        }
    }
    let cl = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.parse::<usize>().ok())
        .unwrap_or(0);
    let mut body = buf[header_end..].to_vec();
    while body.len() < cl {
        let n = stream.read(&mut tmp).ok()?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&tmp[..n]);
    }
    Some(CapturedReq {
        method,
        headers,
        body,
    })
}

fn make_request(url: String, redirect_mode: RedirectMode) -> FetchRequest {
    FetchRequest {
        method: "GET".to_string(),
        url: url.clone(),
        headers: Vec::new(),
        body: None,
        body_source: None,
        redirect_mode,
        credentials_mode: CredentialsMode::SameOrigin,
        cancel: None,
        redirect_count: 0,
        origin_url: url,
    }
}

// ---------------------------------------------------------------------------
// redirect-mode.any.js — RedirectMode follow / error / manual
// ---------------------------------------------------------------------------

#[test]
fn wpt_fetch_redirect_mode() {
    let mut results: Vec<(String, Outcome)> = Vec::new();

    // Follow: 301 + 200 should produce 200 OK.
    {
        let server = start_flex(vec![
            HopResponse::Redirect {
                status: 301,
                location: "/final".to_string(),
            },
            HopResponse::Final200,
        ]);
        let resp = run(main_fetch(make_request(server.url(), RedirectMode::Follow)));
        let outcome = match resp {
            Ok(r) if r.status == 200 && r.redirected => Outcome::Pass,
            Ok(r) => Outcome::Fail(format!(
                "follow: status={} redirected={} (want 200, redirected=true)",
                r.status, r.redirected
            )),
            Err(e) => Outcome::Fail(format!("follow: errored: {e}")),
        };
        results.push(("follow 301".to_string(), outcome));
    }

    // Error: 301 with redirect_mode=error should produce a network error.
    for status in [301u16, 302, 303, 307, 308] {
        let server = start_flex(vec![
            HopResponse::Redirect {
                status,
                location: "/final".to_string(),
            },
            HopResponse::Final200,
        ]);
        let resp = run(main_fetch(make_request(server.url(), RedirectMode::Error)));
        let outcome = match resp {
            Ok(_) => Outcome::Fail(format!("error mode {status}: expected network error, got Response")),
            Err(_) => Outcome::Pass,
        };
        results.push((format!("error mode {status}"), outcome));
    }

    // Manual: returns the redirect response unchanged. Per Fetch §5.6
    // step 4 + §5.5 (response handling), redirect_mode=manual returns
    // the raw redirect (still has a real status; the spec's "type"
    // mapping to opaqueredirect happens later in CORS land). Our
    // algorithm returns the redirect with its real status code, with
    // a Location header.
    for status in [301u16, 302, 303, 307, 308] {
        let server = start_flex(vec![
            HopResponse::Redirect {
                status,
                location: "/final".to_string(),
            },
        ]);
        let resp = run(main_fetch(make_request(server.url(), RedirectMode::Manual)));
        let outcome = match resp {
            Ok(r) if r.status == status && !r.redirected => {
                let has_loc = r
                    .headers
                    .iter()
                    .any(|(k, _)| k.eq_ignore_ascii_case("location"));
                if has_loc {
                    Outcome::Pass
                } else {
                    Outcome::Fail(format!("manual {status}: missing Location header"))
                }
            }
            Ok(r) => Outcome::Fail(format!(
                "manual {status}: status={} redirected={} (want {status}, redirected=false)",
                r.status, r.redirected
            )),
            Err(e) => Outcome::Fail(format!("manual {status}: errored: {e}")),
        };
        results.push((format!("manual mode {status}"), outcome));
    }

    let pass = results.iter().filter(|(_, o)| matches!(o, Outcome::Pass)).count();
    let fail = results.len() - pass;

    eprintln!("\n=== WPT fetch/api/redirect/redirect-mode results ===");
    for (name, outcome) in &results {
        match outcome {
            Outcome::Pass => eprintln!("  PASS  {name}"),
            Outcome::Fail(why) => eprintln!("  FAIL  {name}: {why}"),
        }
    }
    let pct = (pass as f64 / results.len() as f64) * 100.0;
    eprintln!("  total pass={pass} fail={fail}  rate={pct:.1}%");

    assert!(
        pass * 100 >= results.len() * 70,
        "expected ≥70% pass rate; got {pct:.1}% ({pass}/{})",
        results.len()
    );
}

// ---------------------------------------------------------------------------
// redirect-origin.any.js — cross-origin Authorization stripping
// ---------------------------------------------------------------------------
//
// On cross-origin redirect, only the Authorization
// header is stripped on cross-origin redirect (not Cookie/Host/
// Proxy-Authorization). Spec: Fetch §5.6 step 13.
//
// The full WPT redirect-origin test exercises 4 scenarios:
//   - same-origin → Authorization preserved
//   - cross-origin → Authorization stripped
//   - cross-origin then back same-origin → Authorization NOT restored
//   - cross-origin with new Authorization in subsequent hop → preserved

#[test]
fn wpt_fetch_redirect_origin() {
    let mut results: Vec<(String, Outcome)> = Vec::new();

    // 1. Same-origin redirect: Authorization preserved.
    {
        let server = start_flex(vec![
            HopResponse::Redirect {
                status: 302,
                location: "/final".to_string(),
            },
            HopResponse::Final200,
        ]);
        let mut req = make_request(server.url(), RedirectMode::Follow);
        req.headers.push(("Authorization".to_string(), "Basic dXNlcjpwYXNz".to_string()));
        let resp = run(main_fetch(req));
        let outcome = match resp {
            Ok(r) if r.status == 200 => {
                // Check the *second* hop saw Authorization.
                let reqs = server.requests.lock().unwrap();
                let saw_auth = reqs
                    .get(1)
                    .map(|r| {
                        r.headers
                            .iter()
                            .any(|(k, _)| k.eq_ignore_ascii_case("authorization"))
                    })
                    .unwrap_or(false);
                if saw_auth {
                    Outcome::Pass
                } else {
                    Outcome::Fail("same-origin: Authorization was stripped (should be preserved)".to_string())
                }
            }
            Ok(r) => Outcome::Fail(format!("same-origin: status {} (want 200)", r.status)),
            Err(e) => Outcome::Fail(format!("same-origin: errored: {e}")),
        };
        results.push(("same-origin Authorization preserved".to_string(), outcome));
    }

    // 2. Cross-origin redirect: Authorization stripped on next hop.
    //    Fixture: server A redirects to server B. Verify B doesn't see
    //    Authorization.
    {
        let final_server = start_flex(vec![HopResponse::Final200]);
        let final_url = final_server.url();
        let redirect_server = start_flex(vec![HopResponse::Redirect {
            status: 302,
            location: final_url.clone(),
        }]);
        let mut req = make_request(redirect_server.url(), RedirectMode::Follow);
        req.headers.push(("Authorization".to_string(), "Basic dXNlcjpwYXNz".to_string()));
        let resp = run(main_fetch(req));
        let outcome = match resp {
            Ok(r) if r.status == 200 => {
                // Check that the *destination* server did NOT see Authorization.
                let reqs = final_server.requests.lock().unwrap();
                let saw_auth = reqs
                    .first()
                    .map(|r| {
                        r.headers
                            .iter()
                            .any(|(k, _)| k.eq_ignore_ascii_case("authorization"))
                    })
                    .unwrap_or(false);
                if saw_auth {
                    Outcome::Fail("cross-origin: Authorization leaked to destination (should be stripped)".to_string())
                } else {
                    Outcome::Pass
                }
            }
            Ok(r) => Outcome::Fail(format!("cross-origin: status {} (want 200)", r.status)),
            Err(e) => Outcome::Fail(format!("cross-origin: errored: {e}")),
        };
        results.push(("cross-origin Authorization stripped".to_string(), outcome));
    }

    // 3. Cross-origin redirect: ONLY Authorization is stripped — verify
    //    Cookie / Proxy-Authorization etc. are preserved.
    //    Note: the rust algorithm doesn't have a Cookie jar, but a
    //    user-set Cookie header should pass through cross-origin.
    {
        let final_server = start_flex(vec![HopResponse::Final200]);
        let final_url = final_server.url();
        let redirect_server = start_flex(vec![HopResponse::Redirect {
            status: 302,
            location: final_url.clone(),
        }]);
        let mut req = make_request(redirect_server.url(), RedirectMode::Follow);
        req.headers.push(("Authorization".to_string(), "Basic dXNlcjpwYXNz".to_string()));
        req.headers.push(("X-Custom".to_string(), "kept".to_string()));
        let resp = run(main_fetch(req));
        let outcome = match resp {
            Ok(r) if r.status == 200 => {
                let reqs = final_server.requests.lock().unwrap();
                match reqs.first() {
                    Some(captured) => {
                        let auth = captured
                            .headers
                            .iter()
                            .any(|(k, _)| k.eq_ignore_ascii_case("authorization"));
                        let custom = captured
                            .headers
                            .iter()
                            .find(|(k, _)| k.eq_ignore_ascii_case("x-custom"))
                            .map(|(_, v)| v.as_str() == "kept")
                            .unwrap_or(false);
                        if auth {
                            Outcome::Fail("cross-origin: Authorization leaked".to_string())
                        } else if !custom {
                            Outcome::Fail("cross-origin: X-Custom header was incorrectly stripped".to_string())
                        } else {
                            Outcome::Pass
                        }
                    }
                    None => Outcome::Fail("cross-origin: destination got no request".to_string()),
                }
            }
            Ok(r) => Outcome::Fail(format!("cross-origin selective: status {} (want 200)", r.status)),
            Err(e) => Outcome::Fail(format!("cross-origin selective: errored: {e}")),
        };
        results.push(("cross-origin only Authorization stripped".to_string(), outcome));
    }

    let pass = results.iter().filter(|(_, o)| matches!(o, Outcome::Pass)).count();
    let fail = results.len() - pass;

    eprintln!("\n=== WPT fetch/api/redirect/redirect-origin results ===");
    for (name, outcome) in &results {
        match outcome {
            Outcome::Pass => eprintln!("  PASS  {name}"),
            Outcome::Fail(why) => eprintln!("  FAIL  {name}: {why}"),
        }
    }
    let pct = (pass as f64 / results.len() as f64) * 100.0;
    eprintln!("  total pass={pass} fail={fail}  rate={pct:.1}%");

    assert!(
        pass * 100 >= results.len() * 70,
        "expected ≥70% pass rate; got {pct:.1}% ({pass}/{})",
        results.len()
    );
}

// ---------------------------------------------------------------------------
// redirect-location.any.js — Location header parsing edge cases
// ---------------------------------------------------------------------------
//
// The full WPT version requires a Python testserver that varies the
// Location header. We exercise the meaningful cases in-process:
//   - relative Location (/path)
//   - absolute URL Location (http://host/path)
//   - Location with query
//   - missing Location → return as-is (per §5.6 step 4)

#[test]
fn wpt_fetch_redirect_location() {
    let mut results: Vec<(String, Outcome)> = Vec::new();

    // Relative Location.
    {
        let server = start_flex(vec![
            HopResponse::Redirect {
                status: 302,
                location: "/page2".to_string(),
            },
            HopResponse::Final200,
        ]);
        let resp = run(main_fetch(make_request(server.url(), RedirectMode::Follow)));
        let outcome = match resp {
            Ok(r) if r.status == 200 && r.url.ends_with("/page2") => Outcome::Pass,
            Ok(r) => Outcome::Fail(format!("relative: status={} url={}", r.status, r.url)),
            Err(e) => Outcome::Fail(format!("relative: errored: {e}")),
        };
        results.push(("relative Location".to_string(), outcome));
    }

    // Absolute Location: redirect from server A to server B.
    {
        let final_server = start_flex(vec![HopResponse::Final200]);
        let final_url = final_server.url();
        let redirect_server = start_flex(vec![HopResponse::Redirect {
            status: 302,
            location: final_url.clone(),
        }]);
        let resp = run(main_fetch(make_request(redirect_server.url(), RedirectMode::Follow)));
        let outcome = match resp {
            Ok(r) if r.status == 200 && r.url.starts_with(final_url.trim_end_matches('/')) => {
                Outcome::Pass
            }
            Ok(r) => Outcome::Fail(format!("absolute: status={} url={}", r.status, r.url)),
            Err(e) => Outcome::Fail(format!("absolute: errored: {e}")),
        };
        results.push(("absolute Location".to_string(), outcome));
    }

    // Location with query string.
    {
        let server = start_flex(vec![
            HopResponse::Redirect {
                status: 302,
                location: "/page?foo=bar".to_string(),
            },
            HopResponse::Final200,
        ]);
        let resp = run(main_fetch(make_request(server.url(), RedirectMode::Follow)));
        let outcome = match resp {
            Ok(r) if r.status == 200 && r.url.contains("?foo=bar") => Outcome::Pass,
            Ok(r) => Outcome::Fail(format!("query: status={} url={}", r.status, r.url)),
            Err(e) => Outcome::Fail(format!("query: errored: {e}")),
        };
        results.push(("Location with query".to_string(), outcome));
    }

    // Redirect to non-HTTP scheme → network error (Fetch §5.6 step 7).
    {
        let server = start_flex(vec![HopResponse::Redirect {
            status: 302,
            location: "ftp://example.com/file".to_string(),
        }]);
        let resp = run(main_fetch(make_request(server.url(), RedirectMode::Follow)));
        let outcome = match resp {
            Ok(_) => Outcome::Fail("ftp scheme: expected network error, got Response".to_string()),
            Err(_) => Outcome::Pass,
        };
        results.push(("Location to non-HTTP scheme".to_string(), outcome));
    }

    let pass = results.iter().filter(|(_, o)| matches!(o, Outcome::Pass)).count();
    let fail = results.len() - pass;

    eprintln!("\n=== WPT fetch/api/redirect/redirect-location results ===");
    for (name, outcome) in &results {
        match outcome {
            Outcome::Pass => eprintln!("  PASS  {name}"),
            Outcome::Fail(why) => eprintln!("  FAIL  {name}: {why}"),
        }
    }
    let pct = (pass as f64 / results.len() as f64) * 100.0;
    eprintln!("  total pass={pass} fail={fail}  rate={pct:.1}%");

    assert!(
        pass * 100 >= results.len() * 70,
        "expected ≥70% pass rate; got {pct:.1}% ({pass}/{})",
        results.len()
    );
}
