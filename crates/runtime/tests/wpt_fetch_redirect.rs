//! WPT runner for `fetch/api/redirect/redirect-method.any.js`.
//!
//! WPT's redirect-method test requires a Python testserver that
//! accepts `?redirect_status=301&location=...` parameters. Rather
//! than vendoring the testserver, we re-implement the redirect
//! behaviour in a tiny in-process HTTP fixture and run the SAME
//! test cases the WPT file declares (every `redirectMethod(...)`
//! call), against the algorithm chain.
//!
//! This isn't a literal WPT runner (no testharness.js) but mirrors
//! the spec coverage WPT exercises: per-status method changes
//! (301/302/303 → POST→GET; 307/308 → preserved) for GET/POST/PUT.
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
