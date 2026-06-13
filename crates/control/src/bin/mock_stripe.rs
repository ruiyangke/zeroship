//! Standalone mock-Stripe server for the billing & metering multi-node E2E
//! (`tests/e2e_metering_billing.sh`).
//!
//! This is the SHELL-harness peer of the in-process mock-Stripe used by
//! `crates/control/tests/billing_reconcile_test.rs`. It reuses the SAME
//! Stripe-wire idiom — a real `compio::net::TcpListener` speaking HTTP/1.1
//! that parses Stripe's form/JSON create endpoints, RECORDS every request
//! (method, path, Idempotency-Key, Authorization, body) and replays the
//! original response on a repeated Idempotency-Key (faithful 24h dedup) — so
//! the REAL `cyper`-based `StripeClient` inside `zeroship-control` hits it over
//! the wire with no stubbing on the path under test.
//!
//! Beyond the test mock it adds an INTROSPECTION endpoint so a shell harness
//! can assert on recorded calls out-of-band:
//!
//!   GET  /__mock/requests   → JSON array of every recorded request
//!   POST /__mock/reset      → clear the recorded set (200)
//!
//! The `/__mock/*` paths are NOT part of Stripe's surface; the control plane
//! never calls them — only the harness does.
//!
//! Zero tokio: `compio::net` + `compio::runtime` throughout (matches the rest
//! of the stack). Usage:
//!
//!   zeroship-mock-stripe --port 9555
//!   # or: PORT=9555 zeroship-mock-stripe

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use compio::io::{AsyncRead, AsyncWriteExt};
use compio::net::{TcpListener, TcpStream};

/// One recorded inbound HTTP request.
#[derive(Clone, Default)]
struct RecordedRequest {
    method: String,
    path: String,
    idempotency_key: Option<String>,
    authorization: Option<String>,
    body: String,
    /// True when served from the Idempotency-Key replay cache (Stripe would
    /// NOT have created a new object).
    replayed: bool,
}

#[derive(Default)]
struct MockState {
    requests: Vec<RecordedRequest>,
    /// Idempotency-Key → the exact JSON previously returned for that key.
    /// Faithful Stripe dedup within the 24h window: a repeat key replays the
    /// ORIGINAL response rather than creating a second object.
    idempotency_replies: HashMap<String, String>,
}

fn main() -> std::io::Result<()> {
    let port = std::env::args()
        .collect::<Vec<_>>()
        .windows(2)
        .find_map(|w| (w[0] == "--port").then(|| w[1].clone()))
        .or_else(|| std::env::var("PORT").ok())
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(9555);

    let rt = compio::runtime::Runtime::new()?;
    rt.block_on(async move {
        let listener = TcpListener::bind(("127.0.0.1", port))
            .await
            .expect("bind mock-stripe");
        let addr = listener.local_addr().expect("local_addr");
        // Line the harness greps for to know the server is up.
        println!("mock-stripe listening on http://{addr}");
        let state = Arc::new(Mutex::new(MockState::default()));
        loop {
            let Ok((stream, _peer)) = listener.accept().await else {
                break;
            };
            let conn_state = Arc::clone(&state);
            compio::runtime::spawn(async move {
                serve_conn(stream, conn_state).await;
            })
            .detach();
        }
    });
    Ok(())
}

/// Serve one connection, honoring HTTP keep-alive so a reused `cyper`
/// connection's later requests are also answered + recorded.
async fn serve_conn(mut stream: TcpStream, state: Arc<Mutex<MockState>>) {
    let mut acc: Vec<u8> = Vec::new();
    loop {
        loop {
            let Some((req, consumed)) = try_parse_request(&acc) else {
                break;
            };
            acc.drain(0..consumed);
            let response = handle_request(&req, &state);
            if stream.write_all(response).await.0.is_err() {
                return;
            }
        }
        let buf = vec![0u8; 4096];
        let compio::BufResult(n, buf) = stream.read(buf).await;
        match n {
            Ok(0) | Err(_) => return,
            Ok(read) => acc.extend_from_slice(&buf[..read]),
        }
    }
}

/// Parse ONE complete HTTP request from `buf` (request line + headers + body
/// per Content-Length). Returns the parsed request + bytes consumed, or `None`
/// when `buf` is incomplete.
fn try_parse_request(buf: &[u8]) -> Option<(RecordedRequest, usize)> {
    let text = std::str::from_utf8(buf).ok()?;
    let header_end = text.find("\r\n\r\n")?;
    let head = &text[..header_end];
    let mut lines = head.split("\r\n");
    let request_line = lines.next()?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next()?.to_string();
    let path = parts.next()?.to_string();

    let mut content_length = 0usize;
    let mut idempotency_key = None;
    let mut authorization = None;
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            let key = k.trim().to_ascii_lowercase();
            let val = v.trim().to_string();
            match key.as_str() {
                "content-length" => content_length = val.parse().unwrap_or(0),
                "idempotency-key" => idempotency_key = Some(val),
                "authorization" => authorization = Some(val),
                _ => {}
            }
        }
    }

    let body_start = header_end + 4;
    if buf.len() < body_start + content_length {
        return None;
    }
    let body = String::from_utf8_lossy(&buf[body_start..body_start + content_length]).to_string();

    Some((
        RecordedRequest {
            method,
            path,
            idempotency_key,
            authorization,
            body,
            replayed: false,
        },
        body_start + content_length,
    ))
}

/// Produce a response for a recorded request, recording it. Stripe create
/// endpoints get Stripe-shaped JSON; `/__mock/*` are the harness introspection
/// surface.
fn handle_request(req: &RecordedRequest, state: &Arc<Mutex<MockState>>) -> Vec<u8> {
    // Introspection: dump the recorded requests as JSON.
    if req.path.starts_with("/__mock/requests") {
        let st = state.lock().unwrap();
        return http_200_json(&records_json(&st.requests));
    }
    if req.path.starts_with("/__mock/reset") {
        let mut st = state.lock().unwrap();
        st.requests.clear();
        st.idempotency_replies.clear();
        return http_200_json(r#"{"reset":true}"#);
    }

    // Faithful Idempotency-Key replay (Stripe's <24h dedup): a repeat key
    // returns the ORIGINAL response rather than creating a new object.
    {
        let mut st = state.lock().unwrap();
        if let Some(key) = req.idempotency_key.clone() {
            if let Some(prev) = st.idempotency_replies.get(&key).cloned() {
                let mut rec = req.clone();
                rec.replayed = true;
                st.requests.push(rec);
                return http_200_json(&prev);
            }
        }
    }

    let json: String = if req.path.starts_with("/v1/customers") {
        format!(r#"{{"id":"cus_mock_{}","object":"customer"}}"#, short())
    } else if req.path.starts_with("/v1/checkout/sessions") {
        format!(
            r#"{{"id":"cs_mock_{0}","object":"checkout.session","url":"https://checkout.stripe.test/cs_mock_{0}"}}"#,
            short()
        )
    } else if req.path.starts_with("/v1/invoiceitems") {
        format!(r#"{{"id":"ii_mock_{}","object":"invoiceitem"}}"#, short())
    } else if req.path.contains("/finalize") {
        format!(
            r#"{{"id":"in_mock_final_{}","object":"invoice","status":"open"}}"#,
            short()
        )
    } else if req.path.starts_with("/v1/invoices") {
        format!(r#"{{"id":"in_mock_{}","object":"invoice","status":"draft"}}"#, short())
    } else {
        r#"{"id":"obj_mock","object":"unknown"}"#.to_string()
    };

    {
        let mut st = state.lock().unwrap();
        st.requests.push(req.clone());
        if let Some(key) = req.idempotency_key.clone() {
            st.idempotency_replies.entry(key).or_insert_with(|| json.clone());
        }
    }

    http_200_json(&json)
}

/// Serialize the recorded requests to a JSON array (hand-rolled to avoid a
/// serde dep in this tiny bin; field values are escaped).
fn records_json(reqs: &[RecordedRequest]) -> String {
    let mut out = String::from("[");
    for (i, r) in reqs.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&format!(
            r#"{{"method":{},"path":{},"idempotency_key":{},"authorization":{},"body":{},"replayed":{}}}"#,
            json_str(&r.method),
            json_str(&r.path),
            r.idempotency_key.as_deref().map_or("null".to_string(), json_str),
            r.authorization.as_deref().map_or("null".to_string(), json_str),
            json_str(&r.body),
            r.replayed,
        ));
    }
    out.push(']');
    out
}

/// Minimal JSON string escaper (quotes, backslashes, control chars).
fn json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn http_200_json(json: &str) -> Vec<u8> {
    let body = json.as_bytes();
    let mut resp = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: keep-alive\r\n\r\n",
        body.len()
    )
    .into_bytes();
    resp.extend_from_slice(body);
    resp
}

fn short() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{n:x}")
}
