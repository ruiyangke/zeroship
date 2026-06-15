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

/// The pinned Stripe API version every call must carry (C1). Sourced from the
/// production constant so the mock's expectation can never drift from the client.
const PINNED_STRIPE_VERSION: &str = zeroship_control::stripe_client::STRIPE_API_VERSION;

/// One recorded inbound HTTP request.
#[derive(Clone, Default)]
struct RecordedRequest {
    method: String,
    path: String,
    idempotency_key: Option<String>,
    authorization: Option<String>,
    /// The pinned `Stripe-Version` header (C1) — recorded so a harness can assert
    /// every Stripe call pinned the API version.
    stripe_version: Option<String>,
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
    /// Pending invoice items per customer: (customer, amount). A faithful sweep
    /// (D1) totals these onto a draft ONLY when the create sends
    /// `pending_invoice_items_behavior=include` (real Stripe defaults to exclude).
    invoice_items: Vec<(String, i64)>,
    /// Created invoices keyed by the `in_…` id → (customer, swept_total). The swept
    /// total is 0 unless the create requested `include` (D1).
    invoices: HashMap<String, (String, i64)>,
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
    let mut stripe_version = None;
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            let key = k.trim().to_ascii_lowercase();
            let val = v.trim().to_string();
            match key.as_str() {
                "content-length" => content_length = val.parse().unwrap_or(0),
                "idempotency-key" => idempotency_key = Some(val),
                "authorization" => authorization = Some(val),
                "stripe-version" => stripe_version = Some(val),
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
            stripe_version,
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

    // C1: a faithful Stripe REQUIRES the pinned `Stripe-Version` header on every
    // API call. Reject (record then 400) when it is ABSENT so a missing pin is
    // a loud, mechanical failure rather than a silent render against the account
    // default. The pinned value mirrors `STRIPE_API_VERSION` in stripe_client.rs.
    if req.stripe_version.as_deref() != Some(PINNED_STRIPE_VERSION) {
        state.lock().unwrap().requests.push(req.clone());
        let err = r#"{"error":{"type":"invalid_request_error","code":"version_unpinned","message":"missing or wrong Stripe-Version"}}"#;
        return http_json(400, err);
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

    // POST /v1/refunds (D2 refund leg): faithful to real Stripe, `currency` is NOT an
    // accepted parameter — a body that sends it gets a 400 `parameter_unknown`.
    if req.method == "POST" && req.path.starts_with("/v1/refunds") {
        state.lock().unwrap().requests.push(req.clone());
        if form_param(&req.body, "currency").is_some() {
            let err = r#"{"error":{"type":"invalid_request_error","code":"parameter_unknown","message":"Received unknown parameter: currency","param":"currency"}}"#;
            return http_json(400, err);
        }
        return http_200_json(&format!(
            r#"{{"id":"re_mock_{}","object":"refund","status":"succeeded"}}"#,
            short()
        ));
    }

    // GET /v1/invoices/{in_…}[?expand[]=…] (D2): surface the settling pi_/ch_ ONLY
    // when the caller EXPANDS payments.data.payment.payment_intent — mirroring real
    // Stripe (Basil removed the top-level fields; the webhook/bare invoice omit them).
    if req.method == "GET" && req.path.starts_with("/v1/invoices/") {
        let id = req
            .path
            .trim_start_matches("/v1/invoices/")
            .split('?')
            .next()
            .unwrap_or("")
            .to_string();
        let expands_pi = req.path.contains("payments.data.payment.payment_intent");
        let body = if expands_pi {
            // A deterministic settlement pair derived from the invoice id.
            format!(
                r#"{{"id":"{id}","object":"invoice","status":"paid","payments":{{"object":"list","data":[{{"object":"invoice_payment","payment":{{"type":"payment_intent","payment_intent":{{"id":"pi_mock_{id}","object":"payment_intent","latest_charge":"ch_mock_{id}"}}}}}}]}}}}"#
            )
        } else {
            // No expand → Basil invoice WITHOUT the settlement ids (the masking shape).
            format!(r#"{{"id":"{id}","object":"invoice","status":"paid"}}"#)
        };
        state.lock().unwrap().requests.push(req.clone());
        return http_200_json(&body);
    }

    let new_invoice_id = format!("in_mock_{}", short());
    let is_invoice_create =
        req.path.starts_with("/v1/invoices") && !req.path.contains("/finalize");

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
    } else if is_invoice_create {
        format!(r#"{{"id":"{new_invoice_id}","object":"invoice","status":"draft"}}"#)
    } else if req.path.starts_with("/v1/invoices") {
        format!(r#"{{"id":"in_mock_{}","object":"invoice","status":"draft"}}"#, short())
    } else {
        r#"{"id":"obj_mock","object":"unknown"}"#.to_string()
    };

    {
        let mut st = state.lock().unwrap();
        // Track pending invoice items so a D1-faithful sweep can total them.
        if req.path.starts_with("/v1/invoiceitems") {
            let customer = form_param(&req.body, "customer").unwrap_or_default();
            let amount = form_param(&req.body, "amount")
                .and_then(|a| a.parse::<i64>().ok())
                .unwrap_or(0);
            st.invoice_items.push((customer, amount));
        }
        // D1: sweep the customer's pending items onto a draft create ONLY when
        // `pending_invoice_items_behavior=include` was sent (else a $0 draft).
        if is_invoice_create {
            let customer = form_param(&req.body, "customer").unwrap_or_default();
            let include = form_param(&req.body, "pending_invoice_items_behavior").as_deref()
                == Some("include");
            let swept = if include {
                let total: i64 = st
                    .invoice_items
                    .iter()
                    .filter(|(c, _)| *c == customer)
                    .map(|(_, a)| *a)
                    .sum();
                st.invoice_items.retain(|(c, _)| *c != customer);
                total
            } else {
                0
            };
            st.invoices.insert(new_invoice_id.clone(), (customer, swept));
        }
        st.requests.push(req.clone());
        if let Some(key) = req.idempotency_key.clone() {
            st.idempotency_replies.entry(key).or_insert_with(|| json.clone());
        }
    }

    http_200_json(&json)
}

/// Extract a form field from an `application/x-www-form-urlencoded` body. Keys are
/// matched after percent-decoding so `pending_invoice_items_behavior` /
/// `metadata[...]` match the wire's escaping.
fn form_param(body: &str, name: &str) -> Option<String> {
    for pair in body.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            if percent_decode(k) == name {
                return Some(percent_decode(v));
            }
        }
    }
    None
}

/// Minimal `application/x-www-form-urlencoded` decode: `+` → space, `%XX` → byte.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hi = (bytes[i + 1] as char).to_digit(16);
                let lo = (bytes[i + 2] as char).to_digit(16);
                if let (Some(h), Some(l)) = (hi, lo) {
                    out.push((h * 16 + l) as u8);
                    i += 3;
                } else {
                    out.push(bytes[i]);
                    i += 1;
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
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
            r#"{{"method":{},"path":{},"idempotency_key":{},"authorization":{},"stripe_version":{},"body":{},"replayed":{}}}"#,
            json_str(&r.method),
            json_str(&r.path),
            r.idempotency_key.as_deref().map_or("null".to_string(), json_str),
            r.authorization.as_deref().map_or("null".to_string(), json_str),
            r.stripe_version.as_deref().map_or("null".to_string(), json_str),
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
    http_json(200, json)
}

fn http_json(status: u16, json: &str) -> Vec<u8> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        _ => "Error",
    };
    let body = json.as_bytes();
    let mut resp = format!(
        "HTTP/1.1 {status} {reason}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: keep-alive\r\n\r\n",
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
