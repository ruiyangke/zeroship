#![allow(dead_code)]

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use compio::io::{AsyncRead, AsyncWriteExt};
use compio::net::{TcpListener, TcpStream};
use uuid::Uuid;

#[derive(Clone, Debug)]
#[allow(dead_code)]
pub struct RecordedRequest {
    pub method: String,
    pub path: String,
    pub idempotency_key: Option<String>,
    pub body: String,
    pub replayed: bool,
}

#[derive(Default)]
struct MockState {
    requests: Vec<RecordedRequest>,
    idempotency_replies: HashMap<String, String>,
    dedupe_by_key: bool,
    invoice_items: Vec<MockInvoiceItem>,
    invoices: HashMap<String, MockInvoice>,
    deleted_items: HashSet<String>,
}

#[derive(Clone, Default)]
struct MockInvoiceItem {
    id: String,
    customer: String,
    zs_item_key: Option<String>,
    amount: i64,
}

#[derive(Clone, Default)]
struct MockInvoice {
    customer: String,
    swept_total: i64,
}

#[derive(Clone)]
#[allow(dead_code)]
pub struct MockStripe {
    state: Arc<Mutex<MockState>>,
    pub base_url: String,
}

#[allow(dead_code)]
impl MockStripe {
    pub fn requests(&self) -> Vec<RecordedRequest> {
        self.state.lock().unwrap().requests.clone()
    }

    pub fn count_created(&self, method: &str, path_prefix: &str) -> usize {
        let st = self.state.lock().unwrap();
        st.requests
            .iter()
            .filter(|r| r.method == method && r.path.starts_with(path_prefix) && !r.replayed)
            .count()
    }

    pub fn disable_dedupe(&self) {
        self.state.lock().unwrap().dedupe_by_key = false;
    }

    pub fn preload_invoice_item(&self, customer: &str, item_key: &str) -> String {
        let id = format!("ii_orphan_{}", short());
        self.state
            .lock()
            .unwrap()
            .invoice_items
            .push(MockInvoiceItem {
                id: id.clone(),
                customer: customer.to_string(),
                zs_item_key: Some(item_key.to_string()),
                amount: 0,
            });
        id
    }

    pub fn was_item_deleted(&self, item_id: &str) -> bool {
        self.state.lock().unwrap().deleted_items.contains(item_id)
    }

    pub fn invoice_swept_total(&self, invoice_id: &str) -> Option<i64> {
        self.state
            .lock()
            .unwrap()
            .invoices
            .get(invoice_id)
            .map(|invoice| invoice.swept_total)
    }
}

pub async fn start_mock_stripe() -> MockStripe {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock stripe");
    let addr = listener.local_addr().expect("mock stripe local addr");
    let base_url = format!("http://{addr}");
    let state = Arc::new(Mutex::new(MockState {
        dedupe_by_key: true,
        ..MockState::default()
    }));
    let accept_state = Arc::clone(&state);

    compio::runtime::spawn(async move {
        loop {
            let Ok((stream, _peer)) = listener.accept().await else {
                break;
            };
            let conn_state = Arc::clone(&accept_state);
            compio::runtime::spawn(async move {
                serve_conn(stream, conn_state).await;
            })
            .detach();
        }
    })
    .detach();

    MockStripe { state, base_url }
}

async fn serve_conn(mut stream: TcpStream, state: Arc<Mutex<MockState>>) {
    let mut acc: Vec<u8> = Vec::new();
    loop {
        while let Some((req, consumed)) = try_parse_request(&acc) {
            acc.drain(0..consumed);
            let response = handle_mock_request(&req, &state);
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
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            match k.trim().to_ascii_lowercase().as_str() {
                "content-length" => content_length = v.trim().parse().unwrap_or(0),
                "idempotency-key" => idempotency_key = Some(v.trim().to_string()),
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
            body,
            replayed: false,
        },
        body_start + content_length,
    ))
}

fn handle_mock_request(req: &RecordedRequest, state: &Arc<Mutex<MockState>>) -> Vec<u8> {
    {
        let mut st = state.lock().unwrap();
        if st.dedupe_by_key {
            if let Some(key) = req.idempotency_key.clone() {
                if let Some(prev) = st.idempotency_replies.get(&key).cloned() {
                    let mut rec = req.clone();
                    rec.replayed = true;
                    st.requests.push(rec);
                    return http_200_json(&prev);
                }
            }
        }
    }

    if req.method == "DELETE" && req.path.starts_with("/v1/invoiceitems/") {
        let id = req
            .path
            .trim_start_matches("/v1/invoiceitems/")
            .split('?')
            .next()
            .unwrap_or("")
            .to_string();
        {
            let mut st = state.lock().unwrap();
            st.deleted_items.insert(id.clone());
            st.invoice_items.retain(|item| item.id != id);
            st.requests.push(req.clone());
        }
        return http_200_json(&format!(
            r#"{{"id":"{id}","object":"invoiceitem","deleted":true}}"#
        ));
    }

    if req.method == "GET" && req.path.starts_with("/v1/invoiceitems") {
        let customer = query_param(&req.path, "customer");
        let st = state.lock().unwrap();
        let data: Vec<String> = st
            .invoice_items
            .iter()
            .filter(|item| customer.as_deref() == Some(item.customer.as_str()))
            .map(|item| match &item.zs_item_key {
                Some(k) => format!(
                    r#"{{"id":"{}","object":"invoiceitem","metadata":{{"zs_item_key":"{k}"}}}}"#,
                    item.id
                ),
                None => format!(
                    r#"{{"id":"{}","object":"invoiceitem","metadata":{{}}}}"#,
                    item.id
                ),
            })
            .collect();
        drop(st);
        let body = format!(r#"{{"object":"list","data":[{}]}}"#, data.join(","));
        state.lock().unwrap().requests.push(req.clone());
        return http_200_json(&body);
    }

    let new_item_id = format!("ii_mock_{}", short());
    if req.method == "POST"
        && req.path.starts_with("/v1/invoices/")
        && req.path.contains("/finalize")
    {
        let invoice_id = req
            .path
            .trim_start_matches("/v1/invoices/")
            .split('/')
            .next()
            .unwrap_or("")
            .to_string();
        state.lock().unwrap().requests.push(req.clone());
        return http_200_json(&format!(
            r#"{{"id":"{invoice_id}","object":"invoice","status":"open"}}"#
        ));
    }
    let is_invoice_create = req.method == "POST"
        && req.path.starts_with("/v1/invoices")
        && !req.path.contains("/finalize");
    let new_invoice_id = format!("in_mock_{}", short());

    let json = if req.path.starts_with("/v1/invoiceitems") {
        format!(r#"{{"id":"{new_item_id}","object":"invoiceitem"}}"#)
    } else if req.path.contains("/finalize") {
        format!(r#"{{"id":"{new_invoice_id}","object":"invoice","status":"open"}}"#)
    } else if is_invoice_create {
        format!(r#"{{"id":"{new_invoice_id}","object":"invoice","status":"draft"}}"#)
    } else if req.path.starts_with("/v1/invoices") {
        format!(
            r#"{{"id":"in_mock_{}","object":"invoice","status":"draft"}}"#,
            short()
        )
    } else {
        r#"{"id":"obj_mock","object":"unknown"}"#.to_string()
    };

    {
        let mut st = state.lock().unwrap();
        if req.method == "POST" && req.path.starts_with("/v1/invoiceitems") {
            let customer = form_param(&req.body, "customer").unwrap_or_default();
            let key = form_param(&req.body, "metadata[zs_item_key]");
            let amount = form_param(&req.body, "amount")
                .and_then(|amount| amount.parse::<i64>().ok())
                .unwrap_or(0);
            st.invoice_items.push(MockInvoiceItem {
                id: new_item_id.clone(),
                customer,
                zs_item_key: key,
                amount,
            });
        }
        if is_invoice_create {
            let customer = form_param(&req.body, "customer").unwrap_or_default();
            let include = form_param(&req.body, "pending_invoice_items_behavior").as_deref()
                == Some("include");
            let swept_total = if include {
                let total: i64 = st
                    .invoice_items
                    .iter()
                    .filter(|item| item.customer == customer)
                    .map(|item| item.amount)
                    .sum();
                st.invoice_items.retain(|item| item.customer != customer);
                total
            } else {
                0
            };
            st.invoices.insert(
                new_invoice_id.clone(),
                MockInvoice {
                    customer,
                    swept_total,
                },
            );
        }
        st.requests.push(req.clone());
        if let Some(key) = req.idempotency_key.clone() {
            st.idempotency_replies
                .entry(key)
                .or_insert_with(|| json.clone());
        }
    }

    http_200_json(&json)
}

fn query_param(path: &str, name: &str) -> Option<String> {
    let query = path.split_once('?').map(|(_, query)| query)?;
    for pair in query.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            if k == name {
                return Some(percent_decode(v));
            }
        }
    }
    None
}

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

fn http_200_json(json: &str) -> Vec<u8> {
    let body = json.to_string().into_bytes();
    let mut resp = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: keep-alive\r\n\r\n",
        body.len()
    )
    .into_bytes();
    resp.extend_from_slice(&body);
    resp
}

fn short() -> String {
    Uuid::new_v4().simple().to_string()[..8].to_string()
}
