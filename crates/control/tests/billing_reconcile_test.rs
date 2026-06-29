//! Integration tests for the billing-reconcile cron + Stripe client (billing
//! PR6, ISS-31, Stream-1).
//!
//! FAITHFUL by construction: the tests drive the REAL `cyper`-based
//! [`StripeClient`] against a localhost **mock-Stripe HTTP server** (a small
//! HTTP/1.1 server stood up in-test on `compio::net::TcpListener` that speaks
//! Stripe's form/JSON protocol and RECORDS every request). There is NO stubbed
//! client object on the wire path: the reconciler builds a real `StripeClient`
//! pointed at the mock via `AppState.stripe_base_url`, so the form encoding,
//! the `Authorization: Bearer` + `Idempotency-Key` headers, the HTTP round-trip
//! and the JSON parse are all exercised end to end.
//!
//! Real Postgres via `CONTROL_TEST_DB`; silent skip otherwise. The DB must have
//! changeset 0040 applied.

mod common;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use ntex::http::StatusCode;
use ntex::web::{self, test};

use compio::io::{AsyncRead, AsyncWriteExt};
use compio::net::{TcpListener, TcpStream};
use uuid::Uuid;

use zeroship_bundle::{BlobStore, LocalDiskBlobStore};
use zeroship_control::cron::billing_reconcile;
use zeroship_control::metering::Metering;
use zeroship_control::pricing::{charge_cents, MetricWeight, MetricWeights, PlanPrice};
use zeroship_control::stripe_client::{Period, StripeApi, StripeClient, STRIPE_API_VERSION};
use zeroship_control::{
    AppState, EnvStore, Quota, RateLimiter, Registry, SecretString, StripeStore,
};
use zeroship_core::types::{AppUsage, UsageReport};

fn db_url() -> Option<String> {
    std::env::var("CONTROL_TEST_DB").ok()
}

/// `billing_reconcile::tick_with`/`sweep` single-flights fleet-wide via
/// `pg_try_advisory_lock` (production multi-instance safety) — a NON-blocking
/// `try` lock, so two sweeps racing would have one LOSE the lock and return
/// `Ok(0)` (skip). Under the default multi-threaded test runner two
/// reconcile-driving tests would race and the loser's `billed == 1` assertion
/// would fail. `missing_default_fx` additionally mutates the fleet-wide
/// `pricing_config` singleton. Serialize the reconcile-driving tests with a
/// process-wide lock to mirror the production single-flight (a poisoned lock from
/// a prior panic is recovered).
static RECONCILE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

const TEST_MASTER_KEY: &str = "test-master-key-deadbeefcafebabe";

/// The Stripe API version the client MUST pin (C1). Sourced from the production
/// constant so a drift between the code's pin and the mock's expectation fails
/// the build, not silently at runtime.
const PINNED_STRIPE_VERSION: &str = STRIPE_API_VERSION;

fn tmpdir(label: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("zs-bill-{label}-{}", Uuid::new_v4().simple()));
    std::fs::create_dir_all(&p).expect("mk tmpdir");
    p
}

// ===========================================================================
// Mock-Stripe HTTP server — a real localhost server the REAL cyper client hits.
// ===========================================================================

/// One recorded inbound HTTP request to the mock-Stripe server.
#[derive(Debug, Clone)]
struct RecordedRequest {
    method: String,
    path: String,
    idempotency_key: Option<String>,
    authorization: Option<String>,
    /// The `Stripe-Version` header the client pinned (C1). A faithful mock
    /// REQUIRES it on every Stripe call (a 400 otherwise) and serves the wire
    /// shape of THAT version — so a regression test proves we send the pin.
    stripe_version: Option<String>,
    body: String,
    /// True if the mock served this request from its Idempotency-Key replay
    /// cache (i.e. Stripe would NOT have created a new object).
    replayed: bool,
}

#[derive(Default)]
struct MockState {
    requests: Vec<RecordedRequest>,
    /// Idempotency-Key → the exact JSON response previously returned for that
    /// key. Real Stripe replays the ORIGINAL response on a repeated key (within
    /// its 24h window); a faithful mock must do the same so the idempotency
    /// tests actually exercise layer-2 (the deterministic Stripe key). See
    /// `dedupe_by_key`.
    idempotency_replies: HashMap<String, String>,
    /// When false, the mock does NOT replay by Idempotency-Key — it treats every
    /// request as fresh. This simulates Stripe's key window having EXPIRED
    /// (>24h), proving the per-app LEDGER (not Stripe's key) is what guarantees
    /// at-most-once posting (CRIT-1).
    dedupe_by_key: bool,
    /// Pending invoice items, in creation order: (id, customer, zs_item_key, amount).
    /// A faithful Stripe lists these on `GET /v1/invoiceitems?...&pending=true`
    /// so the reconciler's `find_invoice_item_by_key` (C1) can adopt an
    /// already-posted item on a >24h re-drive instead of double-posting. An item
    /// swept onto a finalized invoice would drop off `pending=true`, but our
    /// finalize never sweeps a SECOND copy, so leaving them is faithful enough
    /// for the >24h adopt path under test. The `amount` lets the mock compute a
    /// SWEPT invoice total — faithful to D1 (sweep only on
    /// `pending_invoice_items_behavior=include`).
    invoice_items: Vec<MockInvoiceItem>,
    /// Created invoices, keyed by the `in_…` id the mock minted: the swept total
    /// (D1) + the settlement ids the EXPANDED `GET /v1/invoices` returns (D2).
    invoices: HashMap<String, MockInvoice>,
}

/// A pending invoice item recorded by the mock.
#[derive(Clone)]
struct MockInvoiceItem {
    id: String,
    customer: String,
    zs_item_key: Option<String>,
    amount: i64,
}

/// A created invoice recorded by the mock — enough to faithfully model D1 (the
/// swept total) and D2 (the settlement ids surfaced only under the right expand).
#[derive(Clone, Default)]
struct MockInvoice {
    customer: String,
    /// Sum of the pending items swept onto this invoice at create time. ZERO
    /// unless the create carried `pending_invoice_items_behavior=include` (D1 —
    /// real Stripe defaults to `exclude`).
    swept_total: i64,
    /// The settling pi_/ch_ (set when the harness "pays" the invoice). Real
    /// Stripe surfaces these ONLY via expand[]=payments.data.payment.payment_intent
    /// — the mock mirrors that (D2): they appear in GET only when expand is asked.
    payment_intent: Option<String>,
    charge: Option<String>,
}

#[derive(Clone)]
struct MockStripe {
    state: Arc<Mutex<MockState>>,
    base_url: String,
}

impl MockStripe {
    fn requests(&self) -> Vec<RecordedRequest> {
        self.state.lock().unwrap().requests.clone()
    }

    fn count_path(&self, method: &str, path_prefix: &str) -> usize {
        self.requests()
            .iter()
            .filter(|r| r.method == method && r.path.starts_with(path_prefix))
            .count()
    }

    /// Count distinct POSTs to `path_prefix` that ACTUALLY created a new object
    /// (i.e. were not replayed from a prior identical Idempotency-Key). This is
    /// the count that matters for "double-bill": even if a request was sent
    /// twice, a deduped reply means Stripe created the object once.
    fn count_created(&self, method: &str, path_prefix: &str) -> usize {
        let st = self.state.lock().unwrap();
        st.requests
            .iter()
            .filter(|r| r.method == method && r.path.starts_with(path_prefix) && !r.replayed)
            .count()
    }

    /// Count POSTs to the EXACT path that actually created a new object (not
    /// replayed). Distinct from `count_created`, which matches by prefix — needed
    /// to separate `POST /v1/invoices` (draft create) from
    /// `POST /v1/invoices/{id}/finalize` (which shares the `/v1/invoices` prefix).
    fn count_created_exact(&self, method: &str, path: &str) -> usize {
        let st = self.state.lock().unwrap();
        st.requests
            .iter()
            .filter(|r| r.method == method && r.path == path && !r.replayed)
            .count()
    }

    /// Turn OFF Idempotency-Key replay to simulate Stripe's >24h key expiry.
    fn disable_dedupe(&self) {
        self.state.lock().unwrap().dedupe_by_key = false;
    }

    /// The swept total the mock attached to a created invoice (D1). ZERO when the
    /// create did NOT send `pending_invoice_items_behavior=include` (real Stripe's
    /// default — the masking bug). `None` when no such invoice was created.
    fn invoice_swept_total(&self, invoice_id: &str) -> Option<i64> {
        self.state
            .lock()
            .unwrap()
            .invoices
            .get(invoice_id)
            .map(|inv| inv.swept_total)
    }

    /// Simulate the harness PAYING an invoice: stamp the settling pi_/ch_ so a
    /// later EXPANDED `GET /v1/invoices/{id}?expand[]=payments.data.payment.payment_intent`
    /// returns them (D2). On real Stripe these are surfaced ONLY under that expand.
    /// `register_paid_invoice` lets a test stand up a paid invoice the handler can
    /// then resolve through (for the invoice.paid linkage leg).
    fn register_paid_invoice(&self, invoice_id: &str, customer: &str, pi: &str, ch: Option<&str>) {
        let mut st = self.state.lock().unwrap();
        let inv = st.invoices.entry(invoice_id.to_string()).or_default();
        inv.customer = customer.to_string();
        inv.payment_intent = Some(pi.to_string());
        inv.charge = ch.map(str::to_string);
    }
}

/// Stand up a localhost HTTP/1.1 server that answers Stripe's create endpoints
/// with minimal Stripe-shaped JSON, recording every request. Each accepted
/// connection is served in its own detached task with a keep-alive loop (cyper
/// reuses connections). Returns a handle exposing the base URL + recorded calls.
async fn start_mock_stripe() -> MockStripe {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind mock");
    let addr = listener.local_addr().expect("local_addr");
    let base_url = format!("http://{addr}");
    let state = Arc::new(Mutex::new(MockState {
        dedupe_by_key: true, // faithful default: replay by Idempotency-Key like real Stripe
        ..MockState::default()
    }));
    let accept_state = Arc::clone(&state);

    compio::runtime::spawn(async move {
        loop {
            let Ok((stream, _peer)) = listener.accept().await else { break };
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

/// Serve a single connection: read request(s), record them, respond. Loops to
/// honor HTTP keep-alive so a reused cyper connection's later requests are also
/// answered and recorded.
async fn serve_conn(mut stream: TcpStream, state: Arc<Mutex<MockState>>) {
    let mut acc: Vec<u8> = Vec::new();
    loop {
        // Parse as many complete requests as `acc` holds, draining each.
        loop {
            let Some((req, consumed)) = try_parse_request(&acc) else { break };
            acc.drain(0..consumed);
            let response = handle_mock_request(&req, &state);
            if stream.write_all(response).await.0.is_err() {
                return;
            }
        }
        // Need more bytes.
        let buf = vec![0u8; 4096];
        let compio::BufResult(n, buf) = stream.read(buf).await;
        match n {
            Ok(0) | Err(_) => return, // EOF or error → close
            Ok(read) => acc.extend_from_slice(&buf[..read]),
        }
    }
}

/// Attempt to parse ONE complete HTTP request from `buf`. Returns the parsed
/// request + the number of bytes consumed, or `None` if `buf` is incomplete.
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
        return None; // body not fully arrived
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

/// Produce a Stripe-shaped JSON 200 response for a recorded request and record
/// the request. The `id` returned is derived from the path so each endpoint
/// yields a plausible object id.
fn handle_mock_request(req: &RecordedRequest, state: &Arc<Mutex<MockState>>) -> Vec<u8> {
    // C1: a faithful Stripe REQUIRES the pinned `Stripe-Version` header on every
    // API call. Reject (record then 400) when it is ABSENT — proving the client
    // sends the pin on EVERY method. (Real Stripe would simply render against the
    // account default; we make the absence loud so the regression is mechanical.)
    {
        let pinned = req.stripe_version.as_deref() == Some(PINNED_STRIPE_VERSION);
        if !pinned {
            state.lock().unwrap().requests.push(req.clone());
            let err = r#"{"error":{"type":"invalid_request_error","code":"version_unpinned","message":"missing or wrong Stripe-Version"}}"#;
            return http_json(400, err);
        }
    }

    // Faithful Stripe idempotency: if dedupe is on and we've seen this
    // Idempotency-Key before, replay the EXACT original response (Stripe does
    // not create a second object). Mark the recorded request `replayed` so the
    // test can distinguish "sent twice but deduped" from "created twice".
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

    // GET /v1/invoiceitems?...&pending=true — list the pending items for a
    // customer (C1's `find_invoice_item_by_key`). Faithful Stripe list shape:
    // `{ "object":"list", "data":[ {id, metadata:{zs_item_key}}, … ] }`.
    if req.method == "GET" && req.path.starts_with("/v1/invoiceitems") {
        let customer = query_param(&req.path, "customer");
        let st = state.lock().unwrap();
        let data: Vec<String> = st
            .invoice_items
            .iter()
            .filter(|it| customer.as_deref() == Some(it.customer.as_str()))
            .map(|it| match &it.zs_item_key {
                Some(k) => format!(
                    r#"{{"id":"{}","object":"invoiceitem","metadata":{{"zs_item_key":"{k}"}}}}"#,
                    it.id
                ),
                None => format!(r#"{{"id":"{}","object":"invoiceitem","metadata":{{}}}}"#, it.id),
            })
            .collect();
        drop(st);
        let body = format!(r#"{{"object":"list","data":[{}]}}"#, data.join(","));
        state.lock().unwrap().requests.push(req.clone());
        return http_200_json(&body);
    }

    // GET /v1/invoices/{in_…}[?expand[]=…] — retrieve an invoice. Faithful to D2:
    // the settling pi_/ch_ are surfaced via `payments.data[].payment` ONLY when the
    // caller EXPANDS `payments.data.payment.payment_intent`. Without the expand, the
    // Basil invoice carries NEITHER a top-level payment_intent/charge NOR an inline
    // payments list (exactly what masked the bug). The pi_ comes back as the EXPANDED
    // PaymentIntent object (its `id`=pi_, `latest_charge`=ch_).
    if req.method == "GET" && req.path.starts_with("/v1/invoices/") {
        let id = req
            .path
            .trim_start_matches("/v1/invoices/")
            .split('?')
            .next()
            .unwrap_or("")
            .to_string();
        let expands_pi = req.path.contains("payments.data.payment.payment_intent");
        let st = state.lock().unwrap();
        let inv = st.invoices.get(&id).cloned().unwrap_or_default();
        drop(st);
        // Basil: NO top-level payment_intent/charge (they were removed). The payments
        // list is present only when we expand; absent otherwise.
        let body = if expands_pi {
            match (&inv.payment_intent, &inv.charge) {
                (Some(pi), ch) => {
                    let lc = ch
                        .as_deref()
                        .map_or("null".to_string(), |c| format!("\"{c}\""));
                    format!(
                        r#"{{"id":"{id}","object":"invoice","status":"paid","payments":{{"object":"list","data":[{{"object":"invoice_payment","payment":{{"type":"payment_intent","payment_intent":{{"id":"{pi}","object":"payment_intent","latest_charge":{lc}}}}}}}]}}}}"#
                    )
                }
                // Paid with no recorded settlement (a $0/credit invoice): empty list.
                _ => format!(
                    r#"{{"id":"{id}","object":"invoice","status":"paid","payments":{{"object":"list","data":[]}}}}"#
                ),
            }
        } else {
            // No expand → Basil invoice WITHOUT the settlement ids (the masking shape).
            format!(r#"{{"id":"{id}","object":"invoice","status":"paid"}}"#)
        };
        state.lock().unwrap().requests.push(req.clone());
        return http_200_json(&body);
    }

    // POST /v1/refunds (D2 refund leg). Faithful to real Stripe: `currency` is NOT an
    // accepted parameter — a body that sends it gets a 400 `parameter_unknown`. The
    // body MUST carry exactly one money target (`payment_intent` OR `charge`).
    if req.method == "POST" && req.path.starts_with("/v1/refunds") {
        state.lock().unwrap().requests.push(req.clone());
        if form_param(&req.body, "currency").is_some() {
            let err = r#"{"error":{"type":"invalid_request_error","code":"parameter_unknown","message":"Received unknown parameter: currency","param":"currency"}}"#;
            return http_json(400, err);
        }
        let target = form_param(&req.body, "payment_intent")
            .or_else(|| form_param(&req.body, "charge"));
        if target.is_none() {
            let err = r#"{"error":{"type":"invalid_request_error","code":"parameter_missing","message":"Missing payment_intent or charge"}}"#;
            return http_json(400, err);
        }
        return http_200_json(&format!(r#"{{"id":"re_mock_{}","object":"refund","status":"succeeded"}}"#, short()));
    }

    let new_item_id = format!("ii_mock_{}", short());
    // For a draft-invoice CREATE we mint the id up front so we can register the
    // swept MockInvoice (D1). `is_invoice_create` = POST /v1/invoices that is NOT a
    // /finalize sub-resource.
    let is_invoice_create = req.method == "POST"
        && req.path.starts_with("/v1/invoices")
        && !req.path.contains("/finalize");
    let new_invoice_id = format!("in_mock_{}", short());

    let json: String = if req.path.starts_with("/v1/customers") {
        format!(r#"{{"id":"cus_mock_{}","object":"customer"}}"#, short())
    } else if req.path.starts_with("/v1/checkout/sessions") {
        format!(
            r#"{{"id":"cs_mock_{0}","object":"checkout.session","url":"https://checkout.stripe.test/cs_mock_{0}"}}"#,
            short()
        )
    } else if req.path.starts_with("/v1/invoiceitems") {
        format!(r#"{{"id":"{new_item_id}","object":"invoiceitem"}}"#)
    } else if req.path.contains("/finalize") {
        format!(r#"{{"id":"in_mock_final_{}","object":"invoice","status":"open"}}"#, short())
    } else if is_invoice_create {
        format!(r#"{{"id":"{new_invoice_id}","object":"invoice","status":"draft"}}"#)
    } else if req.path.starts_with("/v1/invoices") {
        format!(r#"{{"id":"in_mock_{}","object":"invoice","status":"draft"}}"#, short())
    } else {
        r#"{"id":"obj_mock","object":"unknown"}"#.to_string()
    };

    {
        let mut st = state.lock().unwrap();
        // Record a created invoice item so the GET-list (adopt) path can find it.
        if req.method == "POST" && req.path.starts_with("/v1/invoiceitems") {
            let customer = form_param(&req.body, "customer").unwrap_or_default();
            let key = form_param(&req.body, "metadata[zs_item_key]");
            let amount = form_param(&req.body, "amount")
                .and_then(|a| a.parse::<i64>().ok())
                .unwrap_or(0);
            st.invoice_items.push(MockInvoiceItem {
                id: new_item_id.clone(),
                customer,
                zs_item_key: key,
                amount,
            });
        }
        // D1: on a draft create, SWEEP the customer's pending items onto the invoice
        // ONLY when `pending_invoice_items_behavior=include` was sent (real Stripe
        // defaults to `exclude` → an empty $0 draft). We model the sweep by totalling
        // the customer's pending item amounts and clearing them off the pending list.
        if is_invoice_create {
            let customer = form_param(&req.body, "customer").unwrap_or_default();
            let include = form_param(&req.body, "pending_invoice_items_behavior")
                .as_deref()
                == Some("include");
            let swept_total = if include {
                let total: i64 = st
                    .invoice_items
                    .iter()
                    .filter(|it| it.customer == customer)
                    .map(|it| it.amount)
                    .sum();
                // Swept items leave the pending list (Stripe attaches them to the invoice).
                st.invoice_items.retain(|it| it.customer != customer);
                total
            } else {
                0
            };
            st.invoices.insert(
                new_invoice_id.clone(),
                MockInvoice {
                    customer,
                    swept_total,
                    payment_intent: None,
                    charge: None,
                },
            );
        }
        st.requests.push(req.clone());
        if let Some(key) = req.idempotency_key.clone() {
            st.idempotency_replies.entry(key).or_insert_with(|| json.clone());
        }
    }

    http_200_json(&json)
}

/// Extract a query-string parameter from a request path (e.g. `customer`).
fn query_param(path: &str, name: &str) -> Option<String> {
    let q = path.split_once('?').map(|(_, q)| q)?;
    for pair in q.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            if k == name {
                return Some(percent_decode(v));
            }
        }
    }
    None
}

/// Extract a form field from an `application/x-www-form-urlencoded` body. Keys
/// are matched after percent-decoding so `metadata[zs_item_key]` matches the
/// wire's `metadata%5Bzs_item_key%5D`.
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

/// Minimal `application/x-www-form-urlencoded` / query decode: `+` → space,
/// `%XX` → byte. Sufficient for the mock's keys/values under test.
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

/// Build a `200 OK` HTTP/1.1 response with a JSON body.
fn http_200_json(json: &str) -> Vec<u8> {
    http_json(200, json)
}

/// Build an HTTP/1.1 response with the given status + JSON body.
fn http_json(status: u16, json: &str) -> Vec<u8> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        _ => "Error",
    };
    let body = json.to_string().into_bytes();
    let mut resp = format!(
        "HTTP/1.1 {status} {reason}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: keep-alive\r\n\r\n",
        body.len()
    )
    .into_bytes();
    resp.extend_from_slice(&body);
    resp
}

fn short() -> String {
    Uuid::new_v4().simple().to_string()[..8].to_string()
}

// ===========================================================================
// Fixture (real PG + the mock-Stripe base URL wired into AppState).
// ===========================================================================

struct Fixture {
    state: Arc<AppState>,
    mock: MockStripe,
    blob_root: PathBuf,
    deploy_tmp_dir: PathBuf,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.blob_root);
        let _ = std::fs::remove_dir_all(&self.deploy_tmp_dir);
    }
}

async fn build_fixture(db_url: &str, label: &str) -> Fixture {
    let mock = start_mock_stripe().await;
    let blob_root = tmpdir(&format!("blob-{label}"));
    let deploy_tmp_dir = tmpdir(&format!("dtmp-{label}"));
    let registry = Registry::new(db_url).await.expect("registry");
    zeroship_control::bootstrap_console::seed_plans(&registry)
        .await
        .expect("seed built-in plans");
    let env_store = EnvStore::new(registry.clone(), TEST_MASTER_KEY, false).expect("env store");
    let stripe_store = StripeStore::new(registry.clone());
    let blob_store: Arc<dyn BlobStore> =
        Arc::new(LocalDiskBlobStore::new(blob_root.clone()).expect("blob store"));

    let (control_pg_client, control_pg_conn) =
        compio_postgres::connect(db_url, compio_postgres::NoTls)
            .await
            .expect("control-pg connect");
    compio::runtime::spawn(async move {
        let _ = control_pg_conn.run().await;
    })
    .detach();
    let control_pg = Arc::new(control_pg_client);

    let state = Arc::new(AppState {
        registry,
        env_store,
        stripe_store,
        blob_store,
        control_key: SecretString::new("test-control-key".to_string()),
        master_key: SecretString::new(TEST_MASTER_KEY.to_string()),
        stripe_webhook_secret: SecretString::new(String::new()),
        stripe_secret_key: SecretString::new("sk_test_mock".to_string()),
        stripe_base_url: mock.base_url.clone(),
        worker_urls: Vec::new(),
        worker_key: SecretString::new(String::new()),
        admin_limiter: Arc::new(RateLimiter::new(Quota::per_minute(10_000, 100))),
        webhook_limiter: Arc::new(RateLimiter::new(Quota::per_minute(10_000, 100))),
        insecure_dev: false,
        trust_proxy: false,
        deploy_tmp_dir: deploy_tmp_dir.clone(),
        control_pg,
        hydra_admin_url: "http://127.0.0.1:9".to_string(),
        app_base_domain: "zeroship.localhost".to_string(),
        trusted_oauth_clients: zeroship_control::default_trusted_oauth_clients(),
        expected_oauth_audience: "control.zeroship.ai".to_string(),
        static_policies: zeroship_authz::load_platform_policies()
            .expect("bundled authz policies parse"),
        pat_issuer: Arc::new(zeroship_control::token_handlers::PatIssuer::dev_insecure()),
        auth_provider: zeroship_control::hydra_auth_provider("http://127.0.0.1:9"),
        logout_jti_cache: Arc::new(zeroship_core::logout_token::LogoutJtiCache::default()),
        metering_provider: zeroship_control::metering::provider::build_provider(
            &zeroship_control::metering::provider::MeteringProviderConfig::native(),
        )
        .expect("native provider builds"),
        tax_provider: zeroship_control::tax::build_tax_provider(
            &zeroship_control::tax::TaxProviderConfig::native(),
        )
        .expect("native tax provider builds"),
        notifier: std::sync::Arc::new(zeroship_control::notify::RecordingNotifier::new()),
        pairwise_salt: [0u8; 32],
        projected_charge_cache: std::sync::Arc::new(
            zeroship_control::billing_read::ProjectedChargeCache::default(),
        ),
    });

    Fixture {
        state,
        mock,
        blob_root,
        deploy_tmp_dir,
    }
}

// ---------------------------------------------------------------------------
// DB seeding helpers.
// ---------------------------------------------------------------------------

/// Insert a user (the creator). Returns its id.
async fn make_user(state: &AppState, label: &str) -> Uuid {
    let email = format!("{label}-{}@example.test", Uuid::new_v4().simple());
    let rows = state
        .control_pg
        .query(
            "INSERT INTO zeroship.users (email, name) VALUES ($1, $2) RETURNING id",
            &[&email, &"Test Creator".to_string()],
        )
        .await
        .expect("insert user");
    rows[0].get("id")
}

/// Seed a plan that charges 1 cent/request with no included CU. CU pricing:
/// global weight `requests` = 1 CU/op × fx 10^12 pico-cents/CU (= 1 cent/CU).
async fn make_plan(state: &AppState) -> String {
    state
        .control_pg
        .execute(
            "INSERT INTO zeroship.metric_weights (metric, units_per_op, per_units) \
             VALUES ('requests', 1, 1) \
             ON CONFLICT (metric) DO UPDATE SET units_per_op = 1, per_units = 1",
            &[],
        )
        .await
        .expect("upsert requests weight");
    let plan_id = format!("pln_bill_{}", Uuid::new_v4().simple());
    let fx_one_cent: i64 = 1_000_000_000_000;
    state
        .control_pg
        .execute(
            "INSERT INTO zeroship.plans \
               (id, name, base_fee_cents, included_units, fx_pico_cents_per_unit, \
                runtime_limits_json, spend_limit_default_cents) \
             VALUES ($1, 'bill-test', 0, 0, $2, \
                     '{\"cpu_limit_ms\":50,\"wall_timeout_ms\":5000,\"heap_limit_mb\":64}', 100000)",
            &[&plan_id, &fx_one_cent],
        )
        .await
        .expect("seed priced plan");
    plan_id
}

/// Create an app on `plan_id` owned by `owner`. Returns the app id.
async fn make_owned_app(state: &AppState, plan_id: &str, owner: Uuid) -> Uuid {
    let name = format!("bill-{}", Uuid::new_v4());
    let rows = state
        .control_pg
        .query(
            "INSERT INTO zeroship.apps (name, plan_id, api_key, api_key_hash) \
             VALUES ($1, $2, $3, '') RETURNING id",
            &[&name, &plan_id, &Uuid::new_v4().to_string()],
        )
        .await
        .expect("insert app");
    let app_id: Uuid = rows[0].get("id");
    state
        .control_pg
        .execute(
            "INSERT INTO zeroship.app_members (app_id, user_id, role) VALUES ($1, $2, 'owner')",
            &[&app_id, &owner],
        )
        .await
        .expect("insert owner membership");
    app_id
}

fn report(worker: &str, seq: u64, app: Uuid, requests: u64) -> UsageReport {
    let mut counters = HashMap::new();
    counters.insert(app, AppUsage { requests, ..Default::default() });
    UsageReport {
        worker_id: worker.to_string(),
        report_id: Uuid::now_v7(),
        sequence: seq,
        counters,
    }
}

/// Ingest usage directly at a given period_start (the CLOSED period the
/// reconciler bills). Mirrors `Metering::ingest_at`.
async fn ingest_at(state: &AppState, app: Uuid, requests: u64, period_start: i64, seq: u64) {
    let metering = Metering::new(state.registry.clone());
    let worker = format!("w-{}", Uuid::new_v4());
    metering
        .ingest_at(&report(&worker, seq, app, requests), period_start)
        .await
        .expect("ingest usage");
}

/// Ingest a set of CUSTOM metrics (name → raw) for `app` at `period_start`, after
/// seeding a `1 CU / op` weight for each so they price through the real CU
/// pipeline. Used by the description-cap / metadata-cap regression tests to drive
/// a MANY-metric app through the REAL reconcile (not a stub).
async fn ingest_custom_metrics(
    state: &AppState,
    app: Uuid,
    metrics: &[(String, u64)],
    period_start: i64,
    seq: u64,
) {
    // Ingest FIRST: the real metering path auto-registers each `custom` metric in
    // `billing_metrics` (the catalog `metric_weights.metric` FKs to) and writes its
    // `usage_aggregates` delta. Seeding a weight before the catalog row exists would
    // violate `metric_weights_metric_fkey`.
    let mut custom = HashMap::new();
    for (name, raw) in metrics {
        custom.insert(name.clone(), *raw);
    }
    let mut counters = HashMap::new();
    counters.insert(app, AppUsage { custom, ..Default::default() });
    let report = UsageReport {
        worker_id: format!("w-{}", Uuid::new_v4()),
        report_id: Uuid::now_v7(),
        sequence: seq,
        counters,
    };
    Metering::new(state.registry.clone())
        .ingest_at(&report, period_start)
        .await
        .expect("ingest custom metrics");

    // Now that each metric is cataloged, seed a 1 CU/op weight so it prices through
    // the real CU pipeline at reconcile time.
    for (name, _) in metrics {
        state
            .control_pg
            .execute(
                "INSERT INTO zeroship.metric_weights (metric, units_per_op, per_units) \
                 VALUES ($1, 1, 1) ON CONFLICT (metric) DO UPDATE SET units_per_op = 1, per_units = 1",
                &[name],
            )
            .await
            .expect("seed custom weight");
    }
}

/// `now` placed mid-current-month so the CLOSED period is the previous month.
fn now_for_closed_period() -> i64 {
    chrono::Utc::now().timestamp()
}

fn prev_period(now: i64) -> i64 {
    billing_reconcile::previous_period_start_unix(now)
}

/// The first-of-month `billing_period` DATE for a unix-seconds period start —
/// the key the redesigned `invoices`/`invoice_lines` tables use. Mirrors
/// `metering::period_date` (re-derived here so the test owns its key shape).
fn period_d(period_start: i64) -> chrono::NaiveDate {
    use chrono::{Datelike, TimeZone};
    let dt = chrono::Utc.timestamp_opt(period_start, 0).single().unwrap();
    chrono::NaiveDate::from_ymd_opt(dt.year(), dt.month(), 1).unwrap()
}

/// Read the `(status, total_cents)` of the invoice for `(creator, period)`, or
/// `None` if no invoice row exists. Replaces the old `billing_runs` read.
async fn read_invoice(
    state: &AppState,
    creator: Uuid,
    period_start: i64,
) -> Option<(String, i64)> {
    state
        .control_pg
        .query(
            "SELECT status, total_cents FROM zeroship.invoices \
             WHERE creator_id = $1 AND period = $2::date",
            &[&creator, &period_d(period_start)],
        )
        .await
        .expect("read invoices")
        .first()
        .map(|r| (r.get::<_, String>("status"), r.get::<_, i64>("total_cents")))
}

/// The finalized provider invoice id (`in_…`) for `(creator, period)` via
/// `invoices ⋈ billing_provider_refs(provider='stripe', ref_kind='invoice')`, or
/// `None`. Replaces the old `billing_runs.stripe_invoice_id` read.
async fn finalized_invoice_id(
    state: &AppState,
    creator: Uuid,
    period_start: i64,
) -> Option<String> {
    state
        .control_pg
        .query(
            "SELECT r.external_id FROM zeroship.invoices i \
             JOIN zeroship.billing_provider_refs r ON r.invoice_id = i.id \
             WHERE i.creator_id = $1 AND i.period = $2::date \
               AND i.status = 'finalized' AND r.provider = 'stripe' AND r.ref_kind = 'invoice'",
            &[&creator, &period_d(period_start)],
        )
        .await
        .expect("read finalized invoice id")
        .first()
        .map(|r| r.get::<_, String>("external_id"))
}

/// The persisted draft provider id (`in_…`) for `(creator, period)` via
/// `billing_provider_refs(ref_kind='draft_invoice')`, or `None`. Replaces the old
/// `billing_runs.draft_invoice_id` read.
async fn draft_invoice_id(
    state: &AppState,
    creator: Uuid,
    period_start: i64,
) -> Option<String> {
    state
        .control_pg
        .query(
            "SELECT r.external_id FROM zeroship.invoices i \
             JOIN zeroship.billing_provider_refs r ON r.invoice_id = i.id \
             WHERE i.creator_id = $1 AND i.period = $2::date \
               AND r.provider = 'stripe' AND r.ref_kind = 'draft_invoice'",
            &[&creator, &period_d(period_start)],
        )
        .await
        .expect("read draft invoice id")
        .first()
        .map(|r| r.get::<_, String>("external_id"))
}

/// Count the invoice LINES for a creator (across all their invoices). Replaces
/// the old `billing_run_items` row count.
async fn lines_count(state: &AppState, creator: Uuid) -> i64 {
    state
        .control_pg
        .query(
            "SELECT COUNT(*)::bigint AS n FROM zeroship.invoice_lines l \
             JOIN zeroship.invoices i ON i.id = l.invoice_id WHERE i.creator_id = $1",
            &[&creator],
        )
        .await
        .expect("count lines")[0]
        .get::<_, i64>("n")
}

/// Count CONFIRMED line provider-refs (== the old non-NULL `stripe_item_id`
/// count) for a creator. A line WITH a `billing_line_provider_refs` row is a
/// confirmed post; a line without one is intent-only.
async fn confirmed_lines_count(state: &AppState, creator: Uuid) -> i64 {
    state
        .control_pg
        .query(
            "SELECT COUNT(*)::bigint AS n FROM zeroship.billing_line_provider_refs r \
             JOIN zeroship.invoices i ON i.id = r.invoice_id \
             WHERE i.creator_id = $1 AND r.provider = 'stripe' AND r.ref_kind = 'invoice_item'",
            &[&creator],
        )
        .await
        .expect("count confirmed lines")[0]
        .get::<_, i64>("n")
}

/// Read back ONE finalized invoice line's persisted snapshot columns for
/// `(creator, app)` — exactly the bytes `bill_creator` wrote. Returns
/// `(included_units, fx_pico_cents_per_unit, base_fee_cents, amount_cents,
/// usage_snapshot, weights_snapshot)`.
#[allow(clippy::type_complexity)]
async fn read_line_snapshot(
    state: &AppState,
    creator: Uuid,
    app: Uuid,
) -> (i64, i64, i64, i64, serde_json::Value, serde_json::Value) {
    let row = state
        .control_pg
        .query(
            "SELECT l.included_units, l.fx_pico_cents_per_unit, l.base_fee_cents, \
                    l.amount_cents, l.usage_snapshot, l.weights_snapshot \
             FROM zeroship.invoice_lines l \
             JOIN zeroship.invoices i ON i.id = l.invoice_id \
             WHERE i.creator_id = $1 AND l.app_id = $2",
            &[&creator, &app],
        )
        .await
        .expect("read line snapshot")
        .into_iter()
        .next()
        .expect("one line for the (creator, app)");
    (
        row.get("included_units"),
        row.get("fx_pico_cents_per_unit"),
        row.get("base_fee_cents"),
        row.get("amount_cents"),
        row.get("usage_snapshot"),
        row.get("weights_snapshot"),
    )
}

// ===========================================================================
// Tests.
// ===========================================================================

/// The reconciler builds Stripe invoice items per owned app from REAL
/// aggregates, then creates + finalizes an invoice — all through the real cyper
/// client hitting the mock server. Records on `billing_runs`.
#[compio::test]
async fn reconcile_creates_invoice_items_per_app_from_real_aggregates() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = build_fixture(&url, "items").await;
    let _recon = RECONCILE_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let now = now_for_closed_period();
    let period = prev_period(now);

    let creator = make_user(&fx.state, "items").await;
    let plan = make_plan(&fx.state).await;
    let app1 = make_owned_app(&fx.state, &plan, creator).await;
    let app2 = make_owned_app(&fx.state, &plan, creator).await;
    // Customer must exist (set lazily by billing/setup in prod; here directly).
    fx.state.stripe_store.set_customer(creator, &format!("cus_test_items_{}", Uuid::new_v4().simple())).await.unwrap();

    ingest_at(&fx.state, app1, 500, period, 1).await; // 500c
    ingest_at(&fx.state, app2, 250, period, 2).await; // 250c

    let billed = billing_reconcile::tick_with(&fx.state, &dummy_passthrough(&fx), now)
        .await
        .expect("tick");
    assert_eq!(billed, 1, "one creator billed");

    // Two invoice-item creates (one per app) + one invoice create + one finalize.
    assert_eq!(fx.mock.count_path("POST", "/v1/invoiceitems"), 2, "one item per app");
    assert_eq!(fx.mock.count_path("POST", "/v1/invoices"), 2, "create + finalize (both POST /v1/invoices…)");

    // invoices records the finalized invoice + the summed total (750c).
    let inv = read_invoice(&fx.state, creator, period).await;
    assert_eq!(
        inv,
        Some(("finalized".to_string(), 750)),
        "one finalized invoice totalling the summed charge across both apps",
    );
    assert!(
        finalized_invoice_id(&fx.state, creator, period).await.is_some(),
        "provider invoice id recorded after finalize",
    );
}

/// billing-metering (a): a single-segment invoice → the `create_invoice_item`
/// description CONTAINS the CU count, the metadata carries the FULL derivation
/// (`compute_units`/`billable_units`/`included_units`/`fx`/per-metric `usage`),
/// and the authoritative `amount` is UNCHANGED (== the frozen `amount_cents`).
///
/// `make_plan` = 1 CU/request, FX 1c/CU, no included CU. 750 requests ⇒ 750 CU,
/// 750 billable, amount 750c.
///
/// RED pre-change (description-only): the item POST carried `description="Infra
/// usage — app … — YYYY-MM"` with NO CU suffix and NO `compute_units`/`usage`
/// metadata — every CU/metadata assertion below fails (the amount assertion held
/// before and after: the money is provably unchanged).
#[compio::test]
async fn single_segment_item_carries_cu_and_full_metadata_amount_unchanged() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = build_fixture(&url, "cu1seg").await;
    let _recon = RECONCILE_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let now = now_for_closed_period();
    let period = prev_period(now);

    let creator = make_user(&fx.state, "cu1seg").await;
    let plan = make_plan(&fx.state).await;
    let app = make_owned_app(&fx.state, &plan, creator).await;
    fx.state
        .stripe_store
        .set_customer(creator, &format!("cus_cu1_{}", Uuid::new_v4().simple()))
        .await
        .unwrap();

    ingest_at(&fx.state, app, 750, period, 1).await;

    let billed = billing_reconcile::tick_with(&fx.state, &dummy_passthrough(&fx), now)
        .await
        .expect("tick");
    assert_eq!(billed, 1, "one creator billed");

    let reqs = fx.mock.requests();
    let item = reqs
        .iter()
        .find(|r| r.method == "POST" && r.path.starts_with("/v1/invoiceitems"))
        .expect("invoice-item POST recorded");

    // (1) the description shows the CU prominently.
    let desc = form_param(&item.body, "description").unwrap_or_default();
    assert!(
        desc.contains("750 compute units"),
        "description must show the CU count; got: {desc}",
    );
    assert!(desc.starts_with("Infra usage — app"), "keeps the existing prefix; got: {desc}");

    // (2) the metadata carries the FULL derivation.
    assert_eq!(form_param(&item.body, "metadata[compute_units]").as_deref(), Some("750"));
    assert_eq!(form_param(&item.body, "metadata[billable_units]").as_deref(), Some("750"));
    assert_eq!(form_param(&item.body, "metadata[included_units]").as_deref(), Some("0"));
    assert_eq!(
        form_param(&item.body, "metadata[fx_pico_cents_per_unit]").as_deref(),
        Some("1000000000000"),
        "FX frozen on the item metadata (1 cent/CU = 10^12 pico-cents)",
    );
    assert_eq!(form_param(&item.body, "metadata[base_fee_cents]").as_deref(), Some("0"));
    assert_eq!(form_param(&item.body, "metadata[segment]").as_deref(), Some("full"));
    assert_eq!(
        form_param(&item.body, "metadata[usage]").as_deref(),
        Some("requests=750:750"),
        "per-metric raw:cu blob present",
    );
    // The zs_item_key adopt-path metadata is still there (not clobbered).
    assert!(form_param(&item.body, "metadata[zs_item_key]").is_some(), "adopt-path key preserved");

    // (3) the AUTHORITATIVE amount is UNCHANGED (== the frozen line amount_cents).
    let amount: i64 = form_param(&item.body, "amount").and_then(|a| a.parse().ok()).expect("amount");
    let line_amount: i64 = fx
        .state
        .control_pg
        .query(
            "SELECT l.amount_cents FROM zeroship.invoice_lines l \
             JOIN zeroship.invoices i ON i.id = l.invoice_id \
             WHERE i.creator_id = $1 AND l.app_id = $2",
            &[&creator, &app],
        )
        .await
        .expect("read line amount")[0]
        .get::<_, i64>("amount_cents");
    assert_eq!(amount, line_amount, "Stripe amount == frozen line amount_cents");
    assert_eq!(amount, 750, "amount is the authoritative ChargeBreakdown.total_cents, untouched");
}

/// billing-metering (c)+(d): a MANY-metric app drives the REAL reconcile, and the
/// emitted Stripe item respects BOTH Stripe limits — the description ≤ the
/// line-item cap (Stripe caps line-item descriptions at 500 chars; we cap at 350),
/// and EVERY metadata value ≤ 500 chars — packing the per-metric `usage` across
/// `usage`/`usage_2`/… and setting `usage_truncated=true` when the set overflows
/// the key budget.
///
/// 80 custom metrics with long names (each 1 CU/op) → the packed usage blob far
/// exceeds one 500-char metadata value, forcing the split + the truncation flag.
///
/// RED pre-change: no enriched description / no usage metadata at all — the
/// description-length + per-value-cap + truncation-flag assertions have nothing to
/// check (the keys are absent), so the test fails on the first metadata lookup.
#[compio::test]
async fn many_metric_item_respects_description_and_metadata_length_caps() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = build_fixture(&url, "cucap").await;
    let _recon = RECONCILE_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let now = now_for_closed_period();
    let period = prev_period(now);

    let creator = make_user(&fx.state, "cucap").await;
    let plan = make_plan(&fx.state).await;
    let app = make_owned_app(&fx.state, &plan, creator).await;
    fx.state
        .stripe_store
        .set_customer(creator, &format!("cus_cucap_{}", Uuid::new_v4().simple()))
        .await
        .unwrap();

    // 80 long-named custom metrics, each a distinct non-trivial raw → distinct CU.
    let metrics: Vec<(String, u64)> = (0..80)
        .map(|i| (format!("a_reasonably_long_custom_metric_name_index_{i:04}"), 1_000 + i as u64))
        .collect();
    ingest_custom_metrics(&fx.state, app, &metrics, period, 1).await;

    let billed = billing_reconcile::tick_with(&fx.state, &dummy_passthrough(&fx), now)
        .await
        .expect("tick");
    assert_eq!(billed, 1, "one creator billed");

    let reqs = fx.mock.requests();
    let item = reqs
        .iter()
        .find(|r| r.method == "POST" && r.path.starts_with("/v1/invoiceitems"))
        .expect("invoice-item POST recorded");

    // (c) the description respects the cap (and still shows the gross CU).
    let desc = form_param(&item.body, "description").unwrap_or_default();
    assert!(
        desc.chars().count() <= zeroship_control::pricing::INVOICE_ITEM_DESC_MAX,
        "description ({} chars) exceeds the {}-char cap",
        desc.chars().count(),
        zeroship_control::pricing::INVOICE_ITEM_DESC_MAX,
    );
    assert!(desc.chars().count() < 500, "well under Stripe's 500-char line-item limit");
    assert!(desc.contains("compute units"), "description still surfaces the CU; got: {desc}");

    // (d) EVERY metadata value is ≤ 500 chars (Stripe's per-value cap).
    let mut saw_usage = false;
    for pair in item.body.split('&') {
        if let Some((k, _)) = pair.split_once('=') {
            let key = percent_decode(k);
            if key.starts_with("metadata[usage") {
                saw_usage = true;
            }
            if key.starts_with("metadata[") {
                let val = form_param(&item.body, &key).unwrap_or_default();
                assert!(
                    val.chars().count() <= zeroship_control::pricing::METADATA_VALUE_MAX,
                    "metadata value for {key} is {} chars (> 500 cap)",
                    val.chars().count(),
                );
            }
        }
    }
    assert!(saw_usage, "at least one packed usage blob present");
    // 80 long metrics overflow the key budget → the truncation flag is set.
    assert_eq!(
        form_param(&item.body, "metadata[usage_truncated]").as_deref(),
        Some("true"),
        "a many-metric set that overflows the key budget sets usage_truncated=true",
    );
    // The gross CU metadata is still exact (sum of all 80 metric deltas).
    let expected_cu: u64 = metrics.iter().map(|(_, r)| *r).sum();
    assert_eq!(
        form_param(&item.body, "metadata[compute_units]").and_then(|v| v.parse::<u64>().ok()),
        Some(expected_cu),
        "gross compute_units is exact even when the per-metric blob is truncated",
    );
}

/// C1: EVERY outbound Stripe call — POST, GET, DELETE — carries the pinned
/// `Stripe-Version` header. The mock REQUIRES the pinned version on every API call
/// (400 `version_unpinned` otherwise), so each REAL `StripeClient` method below only
/// SUCCEEDS when the client sent the pin; we then assert the recorded header on
/// every request.
///
/// Drives the client DIRECTLY (no reconcile tick) so it is light + deterministic
/// under parallel load. Exercises a POST (`create_customer`), a GET
/// (`find_invoice_item_by_key`), and a DELETE (`delete_invoice_item`) so all three
/// request builders are covered.
///
/// RED pre-fix: with no `Stripe-Version` header sent, the mock 400s every call →
/// each client method errors, and the per-request assertion (`stripe_version` is
/// `None`) fails.
#[compio::test]
async fn every_stripe_call_pins_the_api_version() {
    let mock = start_mock_stripe().await;
    let client = StripeClient::new(SecretString::new("sk_test_mock".to_string()))
        .with_base_url(mock.base_url.clone());

    // POST: succeeds only if the pinned version was sent.
    let cus = client.create_customer("pin@test.invalid", "creator-pin").await;
    assert!(cus.is_ok(), "create_customer (POST) must succeed with the pinned version: {cus:?}");

    // GET: list pending items by key (returns None against the empty mock).
    let got = client.find_invoice_item_by_key(&cus.unwrap(), "zs_key_pin").await;
    assert!(got.is_ok(), "find_invoice_item_by_key (GET) must succeed with the pinned version: {got:?}");

    // DELETE: a missing item converges to Ok (the mock returns a Stripe-shaped obj;
    // the client treats resource_missing as success — here it's a 200 from the mock).
    let del = client.delete_invoice_item("ii_pin_missing").await;
    assert!(del.is_ok(), "delete_invoice_item (DELETE) must succeed with the pinned version: {del:?}");

    // Every recorded request pinned the EXACT version on every method.
    let reqs = mock.requests();
    assert!(reqs.len() >= 3, "POST + GET + DELETE recorded, got {}", reqs.len());
    let mut saw = (false, false, false);
    for r in &reqs {
        assert_eq!(
            r.stripe_version.as_deref(),
            Some(PINNED_STRIPE_VERSION),
            "{} {} must pin Stripe-Version={PINNED_STRIPE_VERSION}, got {:?}",
            r.method,
            r.path,
            r.stripe_version,
        );
        match r.method.as_str() {
            "POST" => saw.0 = true,
            "GET" => saw.1 = true,
            "DELETE" => saw.2 = true,
            _ => {}
        }
    }
    assert_eq!(saw, (true, true, true), "all three HTTP methods exercised + pinned");
}

/// THE no-double-bill guarantee. Run the tick TWICE for the same (creator,
/// period). The second run is a pure no-op via the `billing_runs` PK conflict —
/// the mock server sees the invoice-item creates EXACTLY ONCE.
///
/// RED→GREEN: remove the `billing_runs` ON CONFLICT claim and the second run
/// re-bills (the mock sees 4 invoice-item creates, not 2). The blueprint's
/// idempotency guard is what makes this GREEN.
#[compio::test]
async fn reconcile_is_idempotent_per_period() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = build_fixture(&url, "idem").await;
    let _recon = RECONCILE_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let now = now_for_closed_period();
    let period = prev_period(now);

    let creator = make_user(&fx.state, "idem").await;
    let plan = make_plan(&fx.state).await;
    let app = make_owned_app(&fx.state, &plan, creator).await;
    fx.state.stripe_store.set_customer(creator, &format!("cus_test_idem_{}", Uuid::new_v4().simple())).await.unwrap();
    ingest_at(&fx.state, app, 300, period, 1).await; // 300c

    let billed1 = billing_reconcile::tick_with(&fx.state, &dummy_passthrough(&fx), now)
        .await
        .expect("tick 1");
    assert_eq!(billed1, 1, "first run bills the creator");

    let items_after_first = fx.mock.count_path("POST", "/v1/invoiceitems");
    assert_eq!(items_after_first, 1, "one invoice item on the first run");

    // Second run for the SAME period — must be a no-op (already billed).
    let billed2 = billing_reconcile::tick_with(&fx.state, &dummy_passthrough(&fx), now)
        .await
        .expect("tick 2");
    assert_eq!(billed2, 0, "second run is a no-op (per-period idempotency)");

    // The mock server saw the invoice-item create EXACTLY ONCE total.
    assert_eq!(
        fx.mock.count_path("POST", "/v1/invoiceitems"),
        1,
        "no double-bill: the invoice item was created exactly once across two runs",
    );

    // Still exactly one invoice row.
    let rows = fx
        .state
        .control_pg
        .query(
            "SELECT COUNT(*)::bigint AS n FROM zeroship.invoices WHERE creator_id = $1",
            &[&creator],
        )
        .await
        .expect("count invoices");
    assert_eq!(rows[0].get::<_, i64>("n"), 1, "exactly one invoice row");
}

/// The REAL cyper client sends a non-empty `Idempotency-Key` + the
/// `Authorization: Bearer` header on every mutating call. Asserted on the mock
/// server's recorded requests — proving the wire path, not a stubbed client.
#[compio::test]
async fn stripe_client_uses_cyper_and_sends_idempotency_key() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = build_fixture(&url, "hdr").await;

    // Drive the REAL client directly against the mock.
    let client = StripeClient::new(SecretString::new("sk_test_mock".to_string()))
        .with_base_url(fx.mock.base_url.clone());
    let period = Period { start: 1_700_000_000, end: 1_702_000_000 };
    let item = client
        .create_invoice_item("cus_x", 1234, "usd", "infra", period, "billitem:k1", "billitem:k1", &[])
        .await
        .expect("create invoice item");
    assert!(item.starts_with("ii_mock_"), "parsed the ii_ id from the mock JSON");
    let draft = client
        .create_invoice("cus_x", &Uuid::new_v4().to_string(), "billrun:k2")
        .await
        .expect("create draft");
    assert!(draft.starts_with("in_mock_"), "parsed the draft invoice id");
    let invoice = client.finalize_invoice(&draft).await.expect("finalize");
    assert!(invoice.starts_with("in_mock_final_"), "parsed the finalized invoice id");

    let reqs = fx.mock.requests();
    let item_req = reqs.iter().find(|r| r.method == "POST" && r.path.starts_with("/v1/invoiceitems")).expect("item req");
    assert_eq!(item_req.idempotency_key.as_deref(), Some("billitem:k1"), "item idempotency key sent");
    assert_eq!(item_req.authorization.as_deref(), Some("Bearer sk_test_mock"), "bearer auth sent");
    // Form body carries the bracketed period params (proves form encoding).
    assert!(item_req.body.contains("period%5Bstart%5D=1700000000"), "period[start] form-encoded; body={}", item_req.body);
    assert!(item_req.body.contains("amount=1234"), "amount in form body");
    // The deterministic lookup key is stamped into metadata for the >24h adopt path (C1).
    assert!(item_req.body.contains("metadata%5Bzs_item_key%5D=billitem%3Ak1"), "zs_item_key metadata sent; body={}", item_req.body);

    let invoice_create = reqs.iter().find(|r| r.path == "/v1/invoices").expect("invoice create");
    assert_eq!(invoice_create.idempotency_key.as_deref(), Some("billrun:k2"), "invoice idempotency key sent");
    let finalize = reqs.iter().find(|r| r.path.contains("/finalize")).expect("finalize");
    assert_eq!(finalize.idempotency_key.as_deref(), Some(format!("finalize:{draft}").as_str()), "finalize idempotency key keyed on draft id");
}

/// D1 (real-Stripe regression): `create_invoice` MUST send
/// `pending_invoice_items_behavior=include` so the period's pending invoice items
/// are SWEPT onto the draft. On API 2025-09-30.clover the param defaults to
/// `exclude`, so omitting it finalizes a $0 invoice and bills NO infra usage. The
/// mock now models the real default: it sweeps the customer's pending items onto a
/// draft create ONLY when `include` is sent. We assert (a) the wire body carries
/// the param AND (b) the swept invoice total equals the items' sum (not $0).
///
/// RED pre-fix: `create_invoice` omitted the param → the mock swept nothing →
/// `invoice_swept_total` is 0 (≠ 1234+766) and the body lacks the param.
#[compio::test]
async fn create_invoice_sweeps_pending_items_via_include_behavior() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = build_fixture(&url, "d1-sweep").await;
    let client = StripeClient::new(SecretString::new("sk_test_mock".to_string()))
        .with_base_url(fx.mock.base_url.clone());
    let cus = format!("cus_d1_{}", Uuid::new_v4().simple());
    let period = Period { start: 1_700_000_000, end: 1_702_000_000 };

    // Two pending items on the customer (totalling 2000c).
    client
        .create_invoice_item(&cus, 1234, "usd", "infra a", period, "d1:k1", "d1:k1", &[])
        .await
        .expect("item 1");
    client
        .create_invoice_item(&cus, 766, "usd", "infra b", period, "d1:k2", "d1:k2", &[])
        .await
        .expect("item 2");

    // Create the draft (the call under test).
    let draft = client
        .create_invoice(&cus, &Uuid::new_v4().to_string(), "d1:run")
        .await
        .expect("create draft");

    // (a) the WIRE body carried the include behavior.
    let reqs = fx.mock.requests();
    let create = reqs
        .iter()
        .find(|r| r.method == "POST" && r.path == "/v1/invoices")
        .expect("invoice create recorded");
    assert!(
        create.body.contains("pending_invoice_items_behavior=include"),
        "create_invoice must send pending_invoice_items_behavior=include; body={}",
        create.body
    );

    // (b) the mock swept the pending items onto the invoice → total = 2000c, NOT 0.
    assert_eq!(
        fx.mock.invoice_swept_total(&draft),
        Some(2000),
        "the pending items must be swept onto the draft (D1); a $0 sweep means the creator is not billed",
    );
}

/// D2 (real-Stripe regression): a paid infra invoice's settling pi_/ch_ live ONLY
/// under `expand[]=payments.data.payment.payment_intent` on API 2025-09-30.clover
/// (Basil removed the top-level fields). `invoice_settlement_ids` must do the
/// EXPANDED fetch and read them from the expanded PaymentIntent object
/// (`id`=pi_, `latest_charge`=ch_). The mock surfaces them ONLY under that expand.
///
/// RED pre-fix: the handler read the ids off the (un-expandable) webhook payload,
/// so against a Basil invoice it captured NOTHING. Here the un-expanded GET also
/// returns nothing — proving the expand is load-bearing.
#[compio::test]
async fn invoice_settlement_ids_requires_expand_and_reads_pi_ch() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = build_fixture(&url, "d2-expand").await;
    let client = StripeClient::new(SecretString::new("sk_test_mock".to_string()))
        .with_base_url(fx.mock.base_url.clone());

    // A paid invoice whose settlement objects the mock surfaces ONLY under expand.
    let inv = format!("in_d2_{}", Uuid::new_v4().simple());
    fx.mock
        .register_paid_invoice(&inv, "cus_d2", "pi_d2real", Some("ch_d2real"));

    // The real client does the EXPANDED fetch and reads both ids.
    let (pi, ch) = client
        .invoice_settlement_ids(&inv)
        .await
        .expect("settlement ids");
    assert_eq!(pi.as_deref(), Some("pi_d2real"), "pi_ resolved from expanded payment_intent.id");
    assert_eq!(ch.as_deref(), Some("ch_d2real"), "ch_ resolved from payment_intent.latest_charge");

    // Prove the expand is load-bearing: the client's GET carried the expand path.
    // (The mock surfaces the ids ONLY under this expand — exactly mirroring real
    // Stripe, whose bare invoice / webhook payload omits them, which is what
    // masked the bug.)
    let reqs = fx.mock.requests();
    let get = reqs
        .iter()
        .find(|r| r.method == "GET" && r.path.starts_with(&format!("/v1/invoices/{inv}")))
        .expect("expanded invoice GET recorded");
    assert!(
        get.path.contains("expand%5B%5D=payments.data.payment.payment_intent")
            || get.path.contains("expand[]=payments.data.payment.payment_intent"),
        "the settlement fetch must EXPAND payments.data.payment.payment_intent; path={}",
        get.path
    );
}

/// D2 (real-Stripe regression, refund leg): `POST /v1/refunds` does NOT accept a
/// `currency` parameter — sending it is a 400 `parameter_unknown`. `create_refund`
/// must NOT send `currency`. It also refunds a `pi_…`/`ch_…` DIRECTLY and resolves an
/// `in_…` via the expanded fetch. The mock 400s a refund body that carries `currency`.
///
/// RED pre-fix: `create_refund` always sent `currency` → the mock 400s → the cash
/// refund fails (the exact real-Stripe 400 the e2e hit).
#[compio::test]
async fn create_refund_omits_currency_and_targets_pi_directly() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = build_fixture(&url, "d2-refund").await;
    let client = StripeClient::new(SecretString::new("sk_test_mock".to_string()))
        .with_base_url(fx.mock.base_url.clone());

    // (a) Refund a pi_ directly → succeeds (no `currency` sent), one POST /v1/refunds.
    let re = client
        .create_refund("pi_real123", 200, "usd", "idem-refund-pi")
        .await
        .expect("refund a pi_ directly");
    assert!(re.starts_with("re_mock_"), "got a re_ id: {re}");

    let refund_reqs: Vec<_> = fx
        .mock
        .requests()
        .into_iter()
        .filter(|r| r.method == "POST" && r.path.starts_with("/v1/refunds"))
        .collect();
    assert_eq!(refund_reqs.len(), 1, "exactly one refund POST");
    let body = &refund_reqs[0].body;
    assert!(
        !body.contains("currency="),
        "create_refund must NOT send `currency` (400 parameter_unknown); body={body}",
    );
    assert!(body.contains("payment_intent=pi_real123"), "refunds the pi_ directly; body={body}");
    assert!(!body.contains("expand"), "no expand fetch needed when given a pi_ directly");

    // (b) Refund an in_ → the expanded fetch resolves its settling pi_, then refunds.
    let inv = format!("in_refund_{}", Uuid::new_v4().simple());
    fx.mock.register_paid_invoice(&inv, "cus_r", "pi_frominvoice", Some("ch_frominvoice"));
    let re2 = client
        .create_refund(&inv, 100, "usd", "idem-refund-in")
        .await
        .expect("refund via in_ → expanded fetch");
    assert!(re2.starts_with("re_mock_"));
    let last = fx
        .mock
        .requests()
        .into_iter()
        .filter(|r| r.method == "POST" && r.path.starts_with("/v1/refunds"))
        .last()
        .expect("second refund POST");
    assert!(
        last.body.contains("payment_intent=pi_frominvoice"),
        "an in_ resolves to its settling pi_ via the expanded fetch; body={}",
        last.body
    );
    assert!(!last.body.contains("currency="), "still no currency param");
}

/// `billing/setup` ensures a Customer exists, and a SECOND setup reuses the same
/// `cus_…` (only one `POST /v1/customers` ever fires). Drives the real client
/// through the store + mock; asserts the store persisted one customer id.
#[compio::test]
async fn setup_session_creates_customer_once() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = build_fixture(&url, "setup").await;
    let creator = make_user(&fx.state, "setup").await;

    // First setup: no customer yet → create one + a setup session.
    let client = StripeClient::new(SecretString::new("sk_test_mock".to_string()))
        .with_base_url(fx.mock.base_url.clone());
    // Mirror the handler's ensure-then-session flow twice.
    for _ in 0..2 {
        let existing = fx.state.stripe_store.get_customer(creator).await.unwrap();
        let customer = match existing {
            Some(c) => c,
            None => {
                let cus = client.create_customer("c@example.test", &creator.to_string()).await.unwrap();
                fx.state.stripe_store.set_customer(creator, &cus).await.unwrap();
                cus
            }
        };
        let _url = client
            .create_checkout_setup_session(&customer, "https://ok", "https://cancel")
            .await
            .unwrap();
    }

    assert_eq!(
        fx.mock.count_path("POST", "/v1/customers"),
        1,
        "the customer is created exactly once across two setups (reuse on the second)",
    );
    assert_eq!(fx.mock.count_path("POST", "/v1/checkout/sessions"), 2, "a session per setup");
    let stored = fx.state.stripe_store.get_customer(creator).await.unwrap();
    assert!(stored.is_some(), "customer id persisted to billing_customer_refs");
}

/// Two apps owned by the SAME user_id roll into ONE creator invoice spanning
/// both apps (proves the owner-join grouping). Apps with no owner row are
/// skipped (an unowned app gets no invoice).
#[compio::test]
async fn reconcile_groups_apps_by_owner_via_app_members() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = build_fixture(&url, "owner").await;
    let _recon = RECONCILE_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let now = now_for_closed_period();
    let period = prev_period(now);

    let creator = make_user(&fx.state, "owner").await;
    let plan = make_plan(&fx.state).await;
    let owned_a = make_owned_app(&fx.state, &plan, creator).await;
    let owned_b = make_owned_app(&fx.state, &plan, creator).await;
    fx.state.stripe_store.set_customer(creator, &format!("cus_test_owner_{}", Uuid::new_v4().simple())).await.unwrap();

    // An app with NO owner row — must be skipped (no billable creator).
    let unowned = {
        let name = format!("unowned-{}", Uuid::new_v4());
        let rows = fx
            .state
            .control_pg
            .query(
                "INSERT INTO zeroship.apps (name, plan_id, api_key, api_key_hash) \
                 VALUES ($1, $2, $3, '') RETURNING id",
                &[&name, &plan, &Uuid::new_v4().to_string()],
            )
            .await
            .expect("insert unowned app");
        rows[0].get::<_, Uuid>("id")
    };

    ingest_at(&fx.state, owned_a, 100, period, 1).await; // 100c
    ingest_at(&fx.state, owned_b, 200, period, 2).await; // 200c
    ingest_at(&fx.state, unowned, 999, period, 3).await; // would be 999c — must NOT bill

    let billed = billing_reconcile::tick_with(&fx.state, &dummy_passthrough(&fx), now)
        .await
        .expect("tick");
    assert_eq!(billed, 1, "one creator billed (the unowned app is skipped)");

    // One invoice item per OWNED app (2), not 3.
    assert_eq!(fx.mock.count_path("POST", "/v1/invoiceitems"), 2, "two owned apps → two items");

    // The creator's invoice total spans both owned apps (300c), excluding unowned.
    let inv = read_invoice(&fx.state, creator, period).await;
    assert_eq!(
        inv,
        Some(("finalized".to_string(), 300)),
        "owned apps summed; unowned excluded",
    );
}

/// Commit-then-crash recovery: a `billing_runs` row pre-exists with
/// `stripe_invoice_id IS NULL` (the run was claimed but the process died before
/// Stripe responded). The next tick RE-DRIVES it — the Stripe call fires with
/// the SAME deterministic idempotency key and the row is completed.
#[compio::test]
async fn crashed_run_with_null_invoice_id_is_redriven() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = build_fixture(&url, "crash").await;
    let _recon = RECONCILE_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let now = now_for_closed_period();
    let period = prev_period(now);

    let creator = make_user(&fx.state, "crash").await;
    let plan = make_plan(&fx.state).await;
    let app = make_owned_app(&fx.state, &plan, creator).await;
    fx.state.stripe_store.set_customer(creator, &format!("cus_test_crash_{}", Uuid::new_v4().simple())).await.unwrap();
    ingest_at(&fx.state, app, 400, period, 1).await; // 400c

    // Simulate the crash window: the invoice row exists (claimed, draft) but is
    // not yet finalized.
    fx.state
        .control_pg
        .execute(
            "INSERT INTO zeroship.invoices (id, creator_id, period, status) \
             VALUES ($1, $2, $3::date, 'draft')",
            &[
                &zeroship_core::typed_id::new_invoice_id(),
                &creator,
                &period_d(period),
            ],
        )
        .await
        .expect("pre-insert draft invoice (crash-window claim)");

    let billed = billing_reconcile::tick_with(&fx.state, &dummy_passthrough(&fx), now)
        .await
        .expect("tick");
    assert_eq!(billed, 1, "the NULL-invoice run is re-driven and completed");

    // The Stripe finalize used the deterministic invoice idempotency key.
    let invoice_create = fx
        .mock
        .requests()
        .into_iter()
        .find(|r| r.path == "/v1/invoices")
        .expect("invoice create fired");
    let expected_key = billing_reconcile::invoice_idempotency_key(&creator, period);
    assert_eq!(
        invoice_create.idempotency_key.as_deref(),
        Some(expected_key.as_str()),
        "re-drive replays the SAME deterministic invoice idempotency key",
    );

    // The invoice is now finalized + carries the provider invoice id.
    assert_eq!(
        read_invoice(&fx.state, creator, period).await.map(|(s, _)| s).as_deref(),
        Some("finalized"),
        "the draft invoice is finalized after re-drive",
    );
    assert!(
        finalized_invoice_id(&fx.state, creator, period).await.is_some(),
        "provider invoice id filled in",
    );
}

/// MAJOR-2 (fail-closed) REGRESSION: weights present, a plan that INHERITS the
/// global FX (`fx_pico_cents_per_unit = NULL`), and the global `pricing_config`
/// default row REMOVED ⇒ the platform cannot price ⇒ the sweep must ABORT
/// (error out) and produce NO invoice and NO `billing_runs` row — never a silent
/// base-only $0 invoice (the revenue leak the critic flagged).
///
/// RED→GREEN: under the old `charge_cents` (fx None ⇒ 0 ⇒ base-only), this
/// creator with 600 requests would bill $0 silently and `tick_with` would return
/// `Ok`; here it returns `Err` and writes nothing.
#[compio::test]
async fn missing_default_fx_aborts_sweep_and_bills_no_one() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = build_fixture(&url, "nofx").await;
    let _recon = RECONCILE_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let now = now_for_closed_period();
    let period = prev_period(now);

    let creator = make_user(&fx.state, "nofx").await;

    // Weights present (so usage WOULD accrue CU), but the plan inherits the FX
    // (NULL) and we delete the global default — leaving the FX unresolvable.
    fx.state
        .control_pg
        .execute(
            "INSERT INTO zeroship.metric_weights (metric, units_per_op, per_units) \
             VALUES ('requests', 1, 1) \
             ON CONFLICT (metric) DO UPDATE SET units_per_op = 1, per_units = 1",
            &[],
        )
        .await
        .expect("seed weight");
    let plan_id = format!("pln_nofx_{}", Uuid::new_v4().simple());
    fx.state
        .control_pg
        .execute(
            "INSERT INTO zeroship.plans \
               (id, name, base_fee_cents, included_units, fx_pico_cents_per_unit, \
                runtime_limits_json, spend_limit_default_cents) \
             VALUES ($1, 'nofx', 0, 0, NULL, \
                     '{\"cpu_limit_ms\":50,\"wall_timeout_ms\":5000,\"heap_limit_mb\":64}', 100000)",
            &[&plan_id],
        )
        .await
        .expect("seed inheriting plan");
    let app = make_owned_app(&fx.state, &plan_id, creator).await;
    fx.state.stripe_store.set_customer(creator, &format!("cus_test_nofx_{}", Uuid::new_v4().simple())).await.unwrap();
    ingest_at(&fx.state, app, 600, period, 1).await; // would be 600c IF priceable

    // Capture the shared singleton so we can RESTORE it before any assertion —
    // the pricing_config row is fleet-wide shared state across test binaries, so
    // a mid-test panic must NOT leak the deletion into a sibling test's pricing.
    let saved_fx: Option<i64> = fx
        .state
        .control_pg
        .query("SELECT fx_pico_cents_per_unit FROM zeroship.pricing_config WHERE id = 'global'", &[])
        .await
        .expect("read saved fx")
        .first()
        .map(|r| r.get("fx_pico_cents_per_unit"));

    // Remove the global default FX so the inheriting plan cannot resolve it.
    fx.state
        .control_pg
        .execute("DELETE FROM zeroship.pricing_config WHERE id = 'global'", &[])
        .await
        .expect("delete global pricing_config");

    // The sweep must FAIL CLOSED — abort with an error, not bill $0. Capture the
    // observations FIRST, then restore the singleton, then assert (so a failing
    // assert can never leak the deletion).
    let res = billing_reconcile::tick_with(&fx.state, &dummy_passthrough(&fx), now).await;
    let items = fx.mock.count_path("POST", "/v1/invoiceitems");
    let invoices = fx.mock.count_path("POST", "/v1/invoices");
    let runs = fx
        .state
        .control_pg
        .query(
            "SELECT total_cents FROM zeroship.invoices \
             WHERE creator_id = $1 AND period = $2::date",
            &[&creator, &period_d(period)],
        )
        .await
        .expect("read invoices");

    // Restore the shared singleton BEFORE asserting.
    let restore_fx = saved_fx.unwrap_or(30_000_000);
    fx.state
        .control_pg
        .execute(
            "INSERT INTO zeroship.pricing_config (id, fx_pico_cents_per_unit) \
             VALUES ('global', $1) \
             ON CONFLICT (id) DO UPDATE SET fx_pico_cents_per_unit = EXCLUDED.fx_pico_cents_per_unit",
            &[&restore_fx],
        )
        .await
        .expect("restore global pricing_config");

    assert!(
        res.is_err(),
        "missing global default FX must abort the sweep (fail closed), not bill base-only $0"
    );
    assert_eq!(items, 0, "no item posted");
    assert_eq!(invoices, 0, "no invoice created");
    assert!(runs.is_empty(), "no invoice row — bill no one when the platform can't price");
}

/// Build the real `StripeClient` pointed at the fixture's mock — used by the
/// reconcile tests so the sweep drives the REAL cyper client (NOT a stub).
fn dummy_passthrough(fx: &Fixture) -> StripeClient {
    StripeClient::new(SecretString::new(
        fx.state.stripe_secret_key.expose_secret().to_string(),
    ))
    .with_base_url(fx.state.stripe_base_url.clone())
}

/// A StripeApi decorator that forwards to the REAL `StripeClient` (so requests
/// still hit the mock + get ledgered) but FAILS after the first invoice-item
/// create — simulating a crash/timeout partway through posting a creator's
/// items. The first item posts (and is ledgered by `bill_creator`); the second
/// returns an error, aborting the drive before the invoice is finalized.
struct FailAfterFirstItem {
    inner: StripeClient,
    items_seen: std::cell::Cell<usize>,
}

impl StripeApi for FailAfterFirstItem {
    async fn create_customer(&self, email: &str, creator_id: &str) -> Result<String, zeroship_control::stripe_store::StripeError> {
        self.inner.create_customer(email, creator_id).await
    }
    async fn create_checkout_setup_session(&self, c: &str, ok: &str, cancel: &str) -> Result<String, zeroship_control::stripe_store::StripeError> {
        self.inner.create_checkout_setup_session(c, ok, cancel).await
    }
    async fn create_invoice_item(
        &self,
        customer: &str,
        amount_cents: u64,
        currency: &str,
        description: &str,
        period: Period,
        idempotency_key: &str,
        lookup_key: &str,
        metadata: &[(String, String)],
    ) -> Result<String, zeroship_control::stripe_store::StripeError> {
        let n = self.items_seen.get();
        self.items_seen.set(n + 1);
        if n >= 1 {
            // Second (and later) item: simulate the crash/timeout window.
            return Err(zeroship_control::stripe_store::StripeError::Db(
                "simulated crash after first item".to_string(),
            ));
        }
        self.inner
            .create_invoice_item(customer, amount_cents, currency, description, period, idempotency_key, lookup_key, metadata)
            .await
    }
    async fn delete_invoice_item(&self, item_id: &str) -> Result<(), zeroship_control::stripe_store::StripeError> {
        self.inner.delete_invoice_item(item_id).await
    }
    async fn find_invoice_item_by_key(&self, customer: &str, lookup_key: &str) -> Result<Option<String>, zeroship_control::stripe_store::StripeError> {
        self.inner.find_invoice_item_by_key(customer, lookup_key).await
    }
    async fn create_invoice(&self, customer: &str, creator_id: &str, idempotency_key: &str) -> Result<String, zeroship_control::stripe_store::StripeError> {
        self.inner.create_invoice(customer, creator_id, idempotency_key).await
    }
    async fn finalize_invoice(&self, invoice_id: &str) -> Result<String, zeroship_control::stripe_store::StripeError> {
        self.inner.finalize_invoice(invoice_id).await
    }
    async fn create_meter_event(&self, event_name: &str, customer: &str, value: u64, identifier: &str, timestamp: i64) -> Result<(), zeroship_control::stripe_store::StripeError> {
        self.inner.create_meter_event(event_name, customer, value, identifier, timestamp).await
    }
    async fn meter_event_summary(&self, meter_id: &str, customer: &str, start_time: i64, end_time: i64) -> Result<u64, zeroship_control::stripe_store::StripeError> {
        self.inner.meter_event_summary(meter_id, customer, start_time, end_time).await
    }
    async fn create_connect_account(&self, email: &str, creator_id: &str, country: &str) -> Result<String, zeroship_control::stripe_store::StripeError> {
        self.inner.create_connect_account(email, creator_id, country).await
    }
    async fn create_account_link(&self, account_id: &str, refresh_url: &str, return_url: &str) -> Result<String, zeroship_control::stripe_store::StripeError> {
        self.inner.create_account_link(account_id, refresh_url, return_url).await
    }
    async fn retrieve_account(&self, account_id: &str) -> Result<zeroship_control::stripe_client::ConnectAccount, zeroship_control::stripe_store::StripeError> {
        self.inner.retrieve_account(account_id).await
    }
    async fn create_connect_payment_intent(&self, connected_account: &str, amount_cents: u64, currency: &str, application_fee_cents: u64, description: &str, idempotency_key: &str) -> Result<zeroship_control::stripe_client::ConnectPaymentIntent, zeroship_control::stripe_store::StripeError> {
        self.inner.create_connect_payment_intent(connected_account, amount_cents, currency, application_fee_cents, description, idempotency_key).await
    }
    async fn create_refund(&self, provider_invoice_id: &str, amount_cents: u64, currency: &str, idempotency_key: &str) -> Result<String, zeroship_control::stripe_store::StripeError> {
        self.inner.create_refund(provider_invoice_id, amount_cents, currency, idempotency_key).await
    }
    async fn invoice_settlement_ids(&self, provider_invoice_id: &str) -> Result<(Option<String>, Option<String>), zeroship_control::stripe_store::StripeError> {
        self.inner.invoice_settlement_ids(provider_invoice_id).await
    }
}

/// CRIT-1: a partial post then crash, followed by a re-drive AFTER Stripe's
/// Idempotency-Key window has expired (dedupe OFF). The per-app LEDGER — not
/// Stripe's 24h key — must guarantee app A's invoice item is created EXACTLY
/// ONCE; the re-drive posts ONLY app B.
///
/// RED→GREEN: without the `billing_run_items` ledger (or if the re-drive does
/// not skip ledgered apps), the second drive re-posts app A and the mock — with
/// dedupe OFF — creates a SECOND item for A (double-bill). The ledger makes it
/// GREEN: count_created == 2 total (A once + B once), never 3.
#[compio::test]
async fn partial_post_then_crash_does_not_double_bill_app_a() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = build_fixture(&url, "partial").await;
    let _recon = RECONCILE_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let now = now_for_closed_period();
    let period = prev_period(now);

    let creator = make_user(&fx.state, "partial").await;
    let plan = make_plan(&fx.state).await;
    let app_a = make_owned_app(&fx.state, &plan, creator).await;
    let app_b = make_owned_app(&fx.state, &plan, creator).await;
    fx.state.stripe_store.set_customer(creator, &format!("cus_test_partial_{}", Uuid::new_v4().simple())).await.unwrap();
    ingest_at(&fx.state, app_a, 100, period, 1).await; // 100c
    ingest_at(&fx.state, app_b, 200, period, 2).await; // 200c

    // First drive: crashes after the first invoice item posts.
    let failing = FailAfterFirstItem {
        inner: dummy_passthrough(&fx),
        items_seen: std::cell::Cell::new(0),
    };
    let res = billing_reconcile::tick_with(&fx.state, &failing, now).await;
    // The sweep swallows per-creator errors → Ok(0) (nobody fully billed), but
    // exactly ONE item must have posted + been ledgered.
    assert_eq!(res.expect("tick swallows the per-creator error"), 0, "no creator fully billed on the crashed drive");

    let created_after_crash = fx.mock.count_created("POST", "/v1/invoiceitems");
    assert_eq!(created_after_crash, 1, "exactly one item posted before the crash");
    // Claim-then-call (C1): the line (snapshot intent) is written BEFORE each
    // Stripe POST, so after the crash app A is CONFIRMED (has a
    // billing_line_provider_refs row) and app B is INTENT-only (a line with NO
    // provider-ref — its POST failed). Exactly ONE confirmed post.
    assert_eq!(
        confirmed_lines_count(&fx.state, creator).await,
        1,
        "exactly one app CONFIRMED-posted after the crash (claim-then-call)",
    );

    // Simulate >24h: Stripe's Idempotency-Key no longer dedupes.
    fx.mock.disable_dedupe();

    // Re-drive with a healthy client — the ledger must skip the already-posted
    // app and post ONLY the remaining one.
    let billed = billing_reconcile::tick_with(&fx.state, &dummy_passthrough(&fx), now)
        .await
        .expect("re-drive tick");
    assert_eq!(billed, 1, "the creator is now fully billed on the re-drive");

    // THE guarantee: total CREATED items == 2 (A once + B once), NOT 3 — even
    // though Stripe's key window expired. The ledger, not Stripe, enforced this.
    assert_eq!(
        fx.mock.count_created("POST", "/v1/invoiceitems"),
        2,
        "no double-bill: app A's item was created exactly once across both drives (ledger guard)",
    );

    // Both apps now have lines, and the invoice is finalized.
    assert_eq!(lines_count(&fx.state, creator).await, 2, "both apps lined after the re-drive");
    assert!(
        finalized_invoice_id(&fx.state, creator, period).await.is_some(),
        "invoice finalized",
    );
}

/// C1 decorator: `create_invoice_item` POSTS to the real Stripe (mock) — so the
/// item EXISTS on Stripe — but then returns an Err, simulating the process
/// crashing AFTER the Stripe POST returns yet BEFORE the ledger row is confirmed
/// (its `stripe_item_id` UPDATE commits). This is the EXACT crash window the old
/// "ledger-after-call" ordering could not survive: Stripe has the item, the
/// ledger does not.
struct PostThenCrash {
    inner: StripeClient,
}

impl StripeApi for PostThenCrash {
    async fn create_customer(&self, email: &str, creator_id: &str) -> Result<String, zeroship_control::stripe_store::StripeError> {
        self.inner.create_customer(email, creator_id).await
    }
    async fn create_checkout_setup_session(&self, c: &str, ok: &str, cancel: &str) -> Result<String, zeroship_control::stripe_store::StripeError> {
        self.inner.create_checkout_setup_session(c, ok, cancel).await
    }
    async fn create_invoice_item(
        &self,
        customer: &str,
        amount_cents: u64,
        currency: &str,
        description: &str,
        period: Period,
        idempotency_key: &str,
        lookup_key: &str,
        metadata: &[(String, String)],
    ) -> Result<String, zeroship_control::stripe_store::StripeError> {
        // Post for real (the item lands on Stripe)…
        let _id = self
            .inner
            .create_invoice_item(customer, amount_cents, currency, description, period, idempotency_key, lookup_key, metadata)
            .await?;
        // …then "crash" before bill_creator can confirm it in the ledger.
        Err(zeroship_control::stripe_store::StripeError::Db(
            "simulated crash after the Stripe POST returned, before ledger confirm".to_string(),
        ))
    }
    async fn delete_invoice_item(&self, item_id: &str) -> Result<(), zeroship_control::stripe_store::StripeError> {
        self.inner.delete_invoice_item(item_id).await
    }
    async fn find_invoice_item_by_key(&self, customer: &str, lookup_key: &str) -> Result<Option<String>, zeroship_control::stripe_store::StripeError> {
        self.inner.find_invoice_item_by_key(customer, lookup_key).await
    }
    async fn create_invoice(&self, customer: &str, creator_id: &str, idempotency_key: &str) -> Result<String, zeroship_control::stripe_store::StripeError> {
        self.inner.create_invoice(customer, creator_id, idempotency_key).await
    }
    async fn finalize_invoice(&self, invoice_id: &str) -> Result<String, zeroship_control::stripe_store::StripeError> {
        self.inner.finalize_invoice(invoice_id).await
    }
    async fn create_meter_event(&self, event_name: &str, customer: &str, value: u64, identifier: &str, timestamp: i64) -> Result<(), zeroship_control::stripe_store::StripeError> {
        self.inner.create_meter_event(event_name, customer, value, identifier, timestamp).await
    }
    async fn meter_event_summary(&self, meter_id: &str, customer: &str, start_time: i64, end_time: i64) -> Result<u64, zeroship_control::stripe_store::StripeError> {
        self.inner.meter_event_summary(meter_id, customer, start_time, end_time).await
    }
    async fn create_connect_account(&self, email: &str, creator_id: &str, country: &str) -> Result<String, zeroship_control::stripe_store::StripeError> {
        self.inner.create_connect_account(email, creator_id, country).await
    }
    async fn create_account_link(&self, account_id: &str, refresh_url: &str, return_url: &str) -> Result<String, zeroship_control::stripe_store::StripeError> {
        self.inner.create_account_link(account_id, refresh_url, return_url).await
    }
    async fn retrieve_account(&self, account_id: &str) -> Result<zeroship_control::stripe_client::ConnectAccount, zeroship_control::stripe_store::StripeError> {
        self.inner.retrieve_account(account_id).await
    }
    async fn create_connect_payment_intent(&self, connected_account: &str, amount_cents: u64, currency: &str, application_fee_cents: u64, description: &str, idempotency_key: &str) -> Result<zeroship_control::stripe_client::ConnectPaymentIntent, zeroship_control::stripe_store::StripeError> {
        self.inner.create_connect_payment_intent(connected_account, amount_cents, currency, application_fee_cents, description, idempotency_key).await
    }
    async fn create_refund(&self, provider_invoice_id: &str, amount_cents: u64, currency: &str, idempotency_key: &str) -> Result<String, zeroship_control::stripe_store::StripeError> {
        self.inner.create_refund(provider_invoice_id, amount_cents, currency, idempotency_key).await
    }
    async fn invoice_settlement_ids(&self, provider_invoice_id: &str) -> Result<(Option<String>, Option<String>), zeroship_control::stripe_store::StripeError> {
        self.inner.invoice_settlement_ids(provider_invoice_id).await
    }
}

/// C1 (the tighter crash window): the item POSTS to Stripe, then the process
/// crashes BEFORE the ledger records the post. A re-drive AFTER Stripe's 24h
/// Idempotency-Key window has expired (dedupe OFF) must NOT post a second item —
/// the claim-then-call intent row + the deterministic metadata LOOKUP adopt the
/// already-posted item. The app's invoice item is created EXACTLY ONCE.
///
/// RED→GREEN: under the OLD ordering (ledger written AFTER the Stripe call, no
/// intent row, no zs_item_key metadata), the crashed drive leaves NO ledger row,
/// so the >24h re-drive (key expired) re-POSTs the SAME app → count_created == 2
/// (double-bill). The claim-then-call fix makes it count_created == 1.
#[compio::test]
async fn post_then_crash_before_ledger_does_not_double_bill_after_24h() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = build_fixture(&url, "c1crash").await;
    let _recon = RECONCILE_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let now = now_for_closed_period();
    let period = prev_period(now);

    let creator = make_user(&fx.state, "c1crash").await;
    let plan = make_plan(&fx.state).await;
    let app = make_owned_app(&fx.state, &plan, creator).await;
    fx.state.stripe_store.set_customer(creator, &format!("cus_test_c1crash_{}", Uuid::new_v4().simple())).await.unwrap();
    ingest_at(&fx.state, app, 500, period, 1).await; // 500c

    // First drive: the item posts to Stripe, then we crash before the ledger
    // confirms it.
    let crashing = PostThenCrash { inner: dummy_passthrough(&fx) };
    let res = billing_reconcile::tick_with(&fx.state, &crashing, now).await;
    assert_eq!(res.expect("sweep swallows the per-creator error"), 0, "no creator fully billed on the crashed drive");

    // The item DID post to Stripe exactly once on the crashed drive.
    assert_eq!(fx.mock.count_created("POST", "/v1/invoiceitems"), 1, "item posted once before the crash");
    // The line (snapshot intent) exists (claim-then-call) but has NO provider-ref
    // yet (the post was unconfirmed at crash time).
    assert_eq!(lines_count(&fx.state, creator).await, 1, "claim-then-call wrote one line before the POST");
    assert_eq!(
        confirmed_lines_count(&fx.state, creator).await,
        0,
        "the line has no billing_line_provider_refs row (post unconfirmed at crash time)",
    );

    // Simulate >24h: Stripe's Idempotency-Key no longer dedupes.
    fx.mock.disable_dedupe();

    // Re-drive with a healthy client. The NULL intent row + the metadata lookup
    // must ADOPT the already-posted item rather than POST a duplicate.
    let billed = billing_reconcile::tick_with(&fx.state, &dummy_passthrough(&fx), now)
        .await
        .expect("re-drive tick");
    assert_eq!(billed, 1, "the creator is fully billed on the re-drive");

    // THE guarantee: the app's invoice item was CREATED exactly once across both
    // drives — even though Stripe's key window expired. The ledger + metadata
    // lookup, not Stripe's key, enforced this.
    assert_eq!(
        fx.mock.count_created("POST", "/v1/invoiceitems"),
        1,
        "no double-bill: the item was created exactly once (claim-then-call + metadata adopt)",
    );

    // The line is now confirmed and the invoice completed with the right total.
    assert_eq!(
        read_invoice(&fx.state, creator, period).await,
        Some(("finalized".to_string(), 500)),
        "invoice finalized with the real amount, not $0",
    );
    assert!(
        finalized_invoice_id(&fx.state, creator, period).await.is_some(),
        "provider invoice id recorded",
    );
    assert_eq!(lines_count(&fx.state, creator).await, 1, "still exactly one line (no duplicate)");
    assert_eq!(
        confirmed_lines_count(&fx.state, creator).await,
        1,
        "the post is now confirmed (one line provider-ref)",
    );
}

/// A normal (≤24h) re-drive of the same crash window is STILL idempotent: with
/// dedupe ON, the re-driven POST replays Stripe's original object (no new item),
/// so the deterministic Idempotency-Key path also yields exactly one created item.
#[compio::test]
async fn post_then_crash_redrive_within_24h_is_idempotent() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = build_fixture(&url, "c1within").await;
    let _recon = RECONCILE_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let now = now_for_closed_period();
    let period = prev_period(now);

    let creator = make_user(&fx.state, "c1within").await;
    let plan = make_plan(&fx.state).await;
    let app = make_owned_app(&fx.state, &plan, creator).await;
    fx.state.stripe_store.set_customer(creator, &format!("cus_test_c1within_{}", Uuid::new_v4().simple())).await.unwrap();
    ingest_at(&fx.state, app, 320, period, 1).await; // 320c

    let crashing = PostThenCrash { inner: dummy_passthrough(&fx) };
    let _ = billing_reconcile::tick_with(&fx.state, &crashing, now).await;
    assert_eq!(fx.mock.count_created("POST", "/v1/invoiceitems"), 1, "item posted once before crash");

    // Re-drive WITHIN 24h: dedupe stays ON. Stripe replays the original item.
    let billed = billing_reconcile::tick_with(&fx.state, &dummy_passthrough(&fx), now)
        .await
        .expect("re-drive within 24h");
    assert_eq!(billed, 1, "creator billed on the re-drive");
    assert_eq!(
        fx.mock.count_created("POST", "/v1/invoiceitems"),
        1,
        "no double-bill within 24h: the deterministic key replayed the original item",
    );
    assert_eq!(
        read_invoice(&fx.state, creator, period).await,
        Some(("finalized".to_string(), 320)),
        "billed the real amount; invoice finalized",
    );
    assert!(
        finalized_invoice_id(&fx.state, creator, period).await.is_some(),
        "provider invoice id recorded",
    );
}

/// C2 decorator: `create_invoice` creates the draft for real (it lands on Stripe
/// carrying the swept line items) but `finalize_invoice` returns an Err — the
/// process crashes AFTER the draft is created/persisted but BEFORE finalize.
struct CrashOnFinalize {
    inner: StripeClient,
}

impl StripeApi for CrashOnFinalize {
    async fn create_customer(&self, email: &str, creator_id: &str) -> Result<String, zeroship_control::stripe_store::StripeError> {
        self.inner.create_customer(email, creator_id).await
    }
    async fn create_checkout_setup_session(&self, c: &str, ok: &str, cancel: &str) -> Result<String, zeroship_control::stripe_store::StripeError> {
        self.inner.create_checkout_setup_session(c, ok, cancel).await
    }
    async fn create_invoice_item(
        &self,
        customer: &str,
        amount_cents: u64,
        currency: &str,
        description: &str,
        period: Period,
        idempotency_key: &str,
        lookup_key: &str,
        metadata: &[(String, String)],
    ) -> Result<String, zeroship_control::stripe_store::StripeError> {
        self.inner
            .create_invoice_item(customer, amount_cents, currency, description, period, idempotency_key, lookup_key, metadata)
            .await
    }
    async fn delete_invoice_item(&self, item_id: &str) -> Result<(), zeroship_control::stripe_store::StripeError> {
        self.inner.delete_invoice_item(item_id).await
    }
    async fn find_invoice_item_by_key(&self, customer: &str, lookup_key: &str) -> Result<Option<String>, zeroship_control::stripe_store::StripeError> {
        self.inner.find_invoice_item_by_key(customer, lookup_key).await
    }
    async fn create_invoice(&self, customer: &str, creator_id: &str, idempotency_key: &str) -> Result<String, zeroship_control::stripe_store::StripeError> {
        // Create the draft for real (it sweeps the pending items)…
        self.inner.create_invoice(customer, creator_id, idempotency_key).await
    }
    async fn finalize_invoice(&self, _invoice_id: &str) -> Result<String, zeroship_control::stripe_store::StripeError> {
        // …then crash before finalize.
        Err(zeroship_control::stripe_store::StripeError::Db(
            "simulated crash after draft create, before finalize".to_string(),
        ))
    }
    async fn create_meter_event(&self, event_name: &str, customer: &str, value: u64, identifier: &str, timestamp: i64) -> Result<(), zeroship_control::stripe_store::StripeError> {
        self.inner.create_meter_event(event_name, customer, value, identifier, timestamp).await
    }
    async fn meter_event_summary(&self, meter_id: &str, customer: &str, start_time: i64, end_time: i64) -> Result<u64, zeroship_control::stripe_store::StripeError> {
        self.inner.meter_event_summary(meter_id, customer, start_time, end_time).await
    }
    async fn create_connect_account(&self, email: &str, creator_id: &str, country: &str) -> Result<String, zeroship_control::stripe_store::StripeError> {
        self.inner.create_connect_account(email, creator_id, country).await
    }
    async fn create_account_link(&self, account_id: &str, refresh_url: &str, return_url: &str) -> Result<String, zeroship_control::stripe_store::StripeError> {
        self.inner.create_account_link(account_id, refresh_url, return_url).await
    }
    async fn retrieve_account(&self, account_id: &str) -> Result<zeroship_control::stripe_client::ConnectAccount, zeroship_control::stripe_store::StripeError> {
        self.inner.retrieve_account(account_id).await
    }
    async fn create_connect_payment_intent(&self, connected_account: &str, amount_cents: u64, currency: &str, application_fee_cents: u64, description: &str, idempotency_key: &str) -> Result<zeroship_control::stripe_client::ConnectPaymentIntent, zeroship_control::stripe_store::StripeError> {
        self.inner.create_connect_payment_intent(connected_account, amount_cents, currency, application_fee_cents, description, idempotency_key).await
    }
    async fn create_refund(&self, provider_invoice_id: &str, amount_cents: u64, currency: &str, idempotency_key: &str) -> Result<String, zeroship_control::stripe_store::StripeError> {
        self.inner.create_refund(provider_invoice_id, amount_cents, currency, idempotency_key).await
    }
    async fn invoice_settlement_ids(&self, provider_invoice_id: &str) -> Result<(Option<String>, Option<String>), zeroship_control::stripe_store::StripeError> {
        self.inner.invoice_settlement_ids(provider_invoice_id).await
    }
}

/// C2 (under-bill): create draft (sweeping the real items) → crash before
/// finalize. A re-drive AFTER Stripe's 24h create-key window has expired (dedupe
/// OFF) must FINALIZE the ORIGINAL draft (which carries the items) — NOT create a
/// fresh empty draft and finalize a $0 invoice.
///
/// RED→GREEN: under the OLD non-atomic create+finalize (no persisted draft id),
/// the >24h re-drive's `create` key is expired ⇒ a NEW draft is created ⇒ it
/// sweeps NO pending items (they're on the orphaned first draft) ⇒ finalizes a $0
/// invoice (under-bill). Persisting the draft id before finalize + re-finalizing
/// THAT draft on re-drive makes the finalized invoice carry the real amount.
#[compio::test]
async fn crash_before_finalize_finalizes_original_draft_after_24h() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = build_fixture(&url, "c2crash").await;
    let _recon = RECONCILE_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let now = now_for_closed_period();
    let period = prev_period(now);

    let creator = make_user(&fx.state, "c2crash").await;
    let plan = make_plan(&fx.state).await;
    let app = make_owned_app(&fx.state, &plan, creator).await;
    fx.state.stripe_store.set_customer(creator, &format!("cus_test_c2crash_{}", Uuid::new_v4().simple())).await.unwrap();
    ingest_at(&fx.state, app, 700, period, 1).await; // 700c

    // First drive: items post, draft is created + persisted, then finalize crashes.
    let crashing = CrashOnFinalize { inner: dummy_passthrough(&fx) };
    let res = billing_reconcile::tick_with(&fx.state, &crashing, now).await;
    assert_eq!(res.expect("sweep swallows the per-creator error"), 0, "not fully billed (finalize crashed)");

    // The item posted, the draft was created exactly once and PERSISTED.
    assert_eq!(fx.mock.count_created("POST", "/v1/invoiceitems"), 1, "item posted once");
    assert_eq!(fx.mock.count_created_exact("POST", "/v1/invoices"), 1, "exactly one draft created (no finalize yet)");
    let persisted_draft = draft_invoice_id(&fx.state, creator, period).await;
    assert!(persisted_draft.is_some(), "the draft invoice id was persisted BEFORE finalize (C2)");
    assert_eq!(
        read_invoice(&fx.state, creator, period).await.map(|(s, _)| s).as_deref(),
        Some("draft"),
        "invoice still draft (not finalized yet)",
    );
    assert!(
        finalized_invoice_id(&fx.state, creator, period).await.is_none(),
        "no finalized provider ref yet",
    );

    // Simulate >24h: Stripe's create Idempotency-Key window has expired.
    fx.mock.disable_dedupe();

    // Re-drive with a healthy client: it must FINALIZE the EXISTING draft, NOT
    // create a new empty one.
    let billed = billing_reconcile::tick_with(&fx.state, &dummy_passthrough(&fx), now)
        .await
        .expect("re-drive tick");
    assert_eq!(billed, 1, "the creator is fully billed on the re-drive");

    // THE guarantee: still exactly ONE draft created across both drives (no new
    // empty draft), and the finalize targeted the ORIGINAL draft id.
    assert_eq!(
        fx.mock.count_created_exact("POST", "/v1/invoices"),
        1,
        "no new draft on re-drive: the ORIGINAL draft (with the real items) was finalized",
    );
    let finalize_req = fx
        .mock
        .requests()
        .into_iter()
        .find(|r| r.path.contains("/finalize"))
        .expect("finalize fired on re-drive");
    let draft_id = persisted_draft.unwrap();
    assert!(
        finalize_req.path.contains(&draft_id),
        "finalize targeted the persisted ORIGINAL draft id ({draft_id}); path={}",
        finalize_req.path,
    );

    // The invoice is finalized with the REAL total (not $0).
    assert_eq!(
        read_invoice(&fx.state, creator, period).await,
        Some(("finalized".to_string(), 700)),
        "finalized the real amount, NOT a $0 empty invoice",
    );
    assert!(
        finalized_invoice_id(&fx.state, creator, period).await.is_some(),
        "completed with the finalized provider invoice id",
    );
}

// ===========================================================================
// CRIT-10: billing/setup self-service authz (own-id ok; cross-creator 403).
// ===========================================================================

/// Wire just the `billing/setup` route onto a test App (same path the prod
/// router registers).
fn billing_setup_route(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::resource("/api/creators/{id}/billing/setup")
            .route(web::post().to(zeroship_control::stripe_handlers::billing_setup)),
    );
}

/// CRIT-10: a creator may set up their OWN card (principal == :id), but a
/// creator calling billing/setup for ANOTHER creator's id is denied (the `:id`
/// is bound to the principal). A non-billing-operator principal acting on a
/// foreign id falls through to the Cedar BillingWrite gate and is FORBIDDEN.
///
/// RED→GREEN: before the fix, billing/setup gated ONLY on Cedar
/// BillingWrite/Resource::Any with `:id` unbound — so (a) self-service was
/// impossible for a normal creator (403 on their OWN id) and (b) nothing tied
/// `:id` to the principal. The fix makes own-id OK and keeps foreign-id 403.
#[compio::test]
async fn billing_setup_is_self_service_and_blocks_cross_creator() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = build_fixture(&url, "authz").await;

    // A normal (non-operator) creator principal. Its PAT user_id IS the creator.
    let creator_a = common::authz_fixture::non_admin_pat(&fx.state).await;
    // A second creator (a different user id) — the cross-creator target.
    let creator_b = common::authz_fixture::non_admin_pat(&fx.state).await;

    let app = test::init_service(
        web::App::new().state(fx.state.clone()).configure(billing_setup_route),
    )
    .await;

    // (1) Self-service: creator A acts on creator A's OWN id → OK (200).
    let req = test::TestRequest::post()
        .uri(&format!("/api/creators/{}/billing/setup", creator_a.user_id))
        .header("authorization", creator_a.bearer())
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::OK, "creator may set up their OWN card");

    // (2) Cross-creator: creator A acts on creator B's id → FORBIDDEN (403),
    //     and no Stripe customer is created for B.
    let req = test::TestRequest::post()
        .uri(&format!("/api/creators/{}/billing/setup", creator_b.user_id))
        .header("authorization", creator_a.bearer())
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "a creator must NOT set up billing for a DIFFERENT creator",
    );
    let stored_b = fx.state.stripe_store.get_customer(creator_b.user_id).await.unwrap();
    assert!(stored_b.is_none(), "no customer created for the cross-creator victim");

    creator_a.cleanup(&fx.state).await;
    creator_b.cleanup(&fx.state).await;
}

/// CRIT-10 (operator path): a platform billing operator may set up ANY
/// creator's billing (foreign id) — the admin PAT carries BillingWrite.
#[compio::test]
async fn billing_setup_allows_platform_operator_for_any_creator() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = build_fixture(&url, "authz-op").await;

    let operator = common::authz_fixture::admin_pat(&fx.state).await;
    let creator = make_user(&fx.state, "op-target").await;

    let app = test::init_service(
        web::App::new().state(fx.state.clone()).configure(billing_setup_route),
    )
    .await;

    let req = test::TestRequest::post()
        .uri(&format!("/api/creators/{creator}/billing/setup"))
        .header("authorization", operator.bearer())
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::OK, "a billing operator may set up any creator");

    operator.cleanup(&fx.state).await;
}

// ===========================================================================
// On-demand reconcile endpoint (POST /internal/billing/reconcile) — the
// operator-gated trigger the billing & metering E2E (tests/e2e_metering_billing.sh)
// drives so it can reconcile a chosen CLOSED period without waiting a month.
// ===========================================================================

/// Wire the `/internal/billing/reconcile` route onto a test App (same path +
/// handler the prod router registers in `main.rs`).
fn force_reconcile_route(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::resource("/internal/billing/reconcile")
            .route(web::post().to(zeroship_control::internal::force_reconcile)),
    );
}

/// The endpoint is GATED by the SAME `/internal/*` `check_auth` as every other
/// internal route (control-key bearer or `--dev-insecure`) — it is NOT an
/// unauthenticated bypass. With the fixture's `insecure_dev: false` + a
/// non-empty `control_key`:
///   * no bearer ⇒ 401 (the gate, RED if the handler skipped check_auth), and
///   * the correct control-key bearer ⇒ it drives the REAL reconcile for the
///     caller-chosen period, hitting the mock-Stripe over the wire EXACTLY once.
///
/// RED→GREEN: drop the `check_auth` call from `force_reconcile` and the
/// no-bearer request would 200 + bill — a privilege bypass. Keeping the gate
/// makes the no-bearer case 401 while the keyed case still reconciles.
#[compio::test]
async fn force_reconcile_endpoint_is_operator_gated_and_drives_a_chosen_period() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = build_fixture(&url, "force").await;
    let _recon = RECONCILE_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let now = now_for_closed_period();
    let period = prev_period(now);

    let creator = make_user(&fx.state, "force").await;
    let plan = make_plan(&fx.state).await;
    let app = make_owned_app(&fx.state, &plan, creator).await;
    fx.state.stripe_store.set_customer(creator, &format!("cus_test_force_{}", Uuid::new_v4().simple())).await.unwrap();
    ingest_at(&fx.state, app, 600, period, 1).await; // 600c in the CLOSED period

    let svc = test::init_service(
        web::App::new().state(fx.state.clone()).configure(force_reconcile_route),
    )
    .await;

    // (1) No bearer → 401. The gate, not the reconcile, answers.
    let req = test::TestRequest::post()
        .uri(&format!("/internal/billing/reconcile?period={now}"))
        .to_request();
    let resp = test::call_service(&svc, req).await;
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "force-reconcile without the control-key bearer must be rejected (gated, not a bypass)",
    );
    // Nothing was billed by the rejected call.
    assert_eq!(
        fx.mock.count_path("POST", "/v1/invoiceitems"),
        0,
        "the 401'd request must NOT have driven any Stripe call",
    );

    // (2) Correct control-key bearer → drives the real reconcile for `period`.
    let req = test::TestRequest::post()
        .uri(&format!("/internal/billing/reconcile?period={now}"))
        .header("authorization", "Bearer test-control-key")
        .to_request();
    let resp = test::call_service(&svc, req).await;
    assert_eq!(resp.status(), StatusCode::OK, "the keyed request reconciles");

    // The mock saw exactly one invoice-item create (the owned app) + the
    // invoice create/finalize — the REAL cyper wire path, billing the chosen
    // closed period.
    assert_eq!(
        fx.mock.count_path("POST", "/v1/invoiceitems"),
        1,
        "the keyed reconcile created exactly one invoice item for the closed period",
    );

    // And it recorded a finalized invoice for THAT period.
    assert_eq!(
        read_invoice(&fx.state, creator, period).await.map(|(s, _)| s).as_deref(),
        Some("finalized"),
        "one finalized invoice for the reconciled period",
    );
    assert!(
        finalized_invoice_id(&fx.state, creator, period).await.is_some(),
        "the reconciled invoice carries a finalized provider invoice id",
    );
}

// ===========================================================================
// C1 (replay is FAITHFUL): drive bill_creator end-to-end, then read the
// PERSISTED invoice_lines row BACK from the DB and assert charge_cents over the
// FROZEN snapshot reproduces the stored amount_cents bit-for-bit. The test reads
// what bill_creator WROTE, not what the test built. The snapshot must freeze the
// FULL weights map that was actually passed to charge_cents (a superset), so a
// weight for a metric the app did NOT use is still frozen — proving the snapshot
// equals the real charge INPUT and does not silently depend on pricing.rs's loop
// iterating usage keys.
// ===========================================================================

#[compio::test]
async fn finalized_line_replays_persisted_amount_bit_for_bit_via_bill_creator() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = build_fixture(&url, "c1replay").await;
    let _recon = RECONCILE_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let now = now_for_closed_period();
    let period = prev_period(now);

    let creator = make_user(&fx.state, "c1replay").await;
    let plan = make_plan(&fx.state).await; // seeds `requests` = 1 CU/op, fx 1c/CU
    // Seed a SECOND global weight for a metric the app will NOT use, so the frozen
    // weights_snapshot is a strict SUPERSET of the app's usage keys. Pre-fix (the
    // snapshot filtered to usage.keys()) this metric would be ABSENT from the
    // frozen map; the C1 fix freezes the full map. Either way the replay must equal
    // amount_cents (an unused weight contributes 0), but freezing the full map is
    // what makes the snapshot equal to the real charge INPUT.
    fx.state
        .control_pg
        .execute(
            "INSERT INTO zeroship.metric_weights (metric, units_per_op, per_units) \
             VALUES ('cpu_us', 1, 1000) \
             ON CONFLICT (metric) DO UPDATE SET units_per_op = 1, per_units = 1000",
            &[],
        )
        .await
        .expect("seed second weight");
    let app = make_owned_app(&fx.state, &plan, creator).await;
    fx.state
        .stripe_store
        .set_customer(creator, &format!("cus_c1replay_{}", Uuid::new_v4().simple()))
        .await
        .unwrap();
    ingest_at(&fx.state, app, 640, period, 1).await; // 640 requests → 640 CU → 640c

    // Drive the REAL bill_creator path (via the sweep) against live PG + mock.
    let billed = billing_reconcile::tick_with(&fx.state, &dummy_passthrough(&fx), now)
        .await
        .expect("tick");
    assert_eq!(billed, 1, "the creator is billed");
    assert_eq!(
        read_invoice(&fx.state, creator, period).await,
        Some(("finalized".to_string(), 640)),
        "the invoice is finalized at the real amount",
    );

    // Read the PERSISTED line snapshot back (what bill_creator wrote).
    let (included, fx_pico, base, amount, usage_json, weights_json) =
        read_line_snapshot(&fx.state, creator, app).await;

    // The frozen weights snapshot is the FULL global map — it includes the unused
    // `cpu_us` weight (C1: superset), not just the applied `requests`.
    let frozen_weights: MetricWeights =
        serde_json::from_value(weights_json).expect("parse frozen weights");
    assert!(
        frozen_weights.contains_key("requests") && frozen_weights.contains_key("cpu_us"),
        "the snapshot freezes the FULL weights map passed to charge_cents (a superset), \
         including the metric the app never used; got {:?}",
        frozen_weights.keys().collect::<Vec<_>>(),
    );

    // REPLAY: re-run charge_cents over the frozen snapshot read back from the DB.
    let replay_usage: HashMap<String, i64> =
        serde_json::from_value(usage_json).expect("parse frozen usage");
    let replay_price = PlanPrice {
        base_fee_cents: u64::try_from(base).unwrap(),
        included_units: u64::try_from(included).unwrap(),
        fx_pico_cents_per_unit: Some(u64::try_from(fx_pico).unwrap()),
        spend_limit_default_cents: 0,
    };
    let replayed = charge_cents(&replay_price, &replay_usage, &frozen_weights).expect("replay");
    assert_eq!(
        i64::try_from(replayed.total_cents).unwrap(),
        amount,
        "re-running charge_cents over the PERSISTED frozen snapshot reproduces the stored \
         amount_cents bit-for-bit (the snapshot equals the real charge input)",
    );
}

/// M2 decorator: `finalize_invoice` returns Stripe's `invoice_already_finalized`
/// API error (a 4xx), simulating a re-drive of the crash window where Stripe
/// finalized but our local UPDATE never committed. Everything else forwards to the
/// real client (so items + draft land on the mock for real).
struct FinalizeAlreadyFinalized {
    inner: StripeClient,
}

impl StripeApi for FinalizeAlreadyFinalized {
    async fn create_customer(&self, email: &str, creator_id: &str) -> Result<String, zeroship_control::stripe_store::StripeError> {
        self.inner.create_customer(email, creator_id).await
    }
    async fn create_checkout_setup_session(&self, c: &str, ok: &str, cancel: &str) -> Result<String, zeroship_control::stripe_store::StripeError> {
        self.inner.create_checkout_setup_session(c, ok, cancel).await
    }
    async fn create_invoice_item(
        &self,
        customer: &str,
        amount_cents: u64,
        currency: &str,
        description: &str,
        period: Period,
        idempotency_key: &str,
        lookup_key: &str,
        metadata: &[(String, String)],
    ) -> Result<String, zeroship_control::stripe_store::StripeError> {
        self.inner
            .create_invoice_item(customer, amount_cents, currency, description, period, idempotency_key, lookup_key, metadata)
            .await
    }
    async fn delete_invoice_item(&self, item_id: &str) -> Result<(), zeroship_control::stripe_store::StripeError> {
        self.inner.delete_invoice_item(item_id).await
    }
    async fn find_invoice_item_by_key(&self, customer: &str, lookup_key: &str) -> Result<Option<String>, zeroship_control::stripe_store::StripeError> {
        self.inner.find_invoice_item_by_key(customer, lookup_key).await
    }
    async fn create_invoice(&self, customer: &str, creator_id: &str, idempotency_key: &str) -> Result<String, zeroship_control::stripe_store::StripeError> {
        self.inner.create_invoice(customer, creator_id, idempotency_key).await
    }
    async fn finalize_invoice(&self, _invoice_id: &str) -> Result<String, zeroship_control::stripe_store::StripeError> {
        // Stripe rejects finalizing an already-finalized invoice with a 4xx whose
        // machine-readable code is `invoice_already_finalized`.
        Err(zeroship_control::stripe_store::StripeError::Api {
            status: 400,
            code: Some("invoice_already_finalized".to_string()),
        })
    }
    async fn create_meter_event(&self, event_name: &str, customer: &str, value: u64, identifier: &str, timestamp: i64) -> Result<(), zeroship_control::stripe_store::StripeError> {
        self.inner.create_meter_event(event_name, customer, value, identifier, timestamp).await
    }
    async fn meter_event_summary(&self, meter_id: &str, customer: &str, start_time: i64, end_time: i64) -> Result<u64, zeroship_control::stripe_store::StripeError> {
        self.inner.meter_event_summary(meter_id, customer, start_time, end_time).await
    }
    async fn create_connect_account(&self, email: &str, creator_id: &str, country: &str) -> Result<String, zeroship_control::stripe_store::StripeError> {
        self.inner.create_connect_account(email, creator_id, country).await
    }
    async fn create_account_link(&self, account_id: &str, refresh_url: &str, return_url: &str) -> Result<String, zeroship_control::stripe_store::StripeError> {
        self.inner.create_account_link(account_id, refresh_url, return_url).await
    }
    async fn retrieve_account(&self, account_id: &str) -> Result<zeroship_control::stripe_client::ConnectAccount, zeroship_control::stripe_store::StripeError> {
        self.inner.retrieve_account(account_id).await
    }
    async fn create_connect_payment_intent(&self, connected_account: &str, amount_cents: u64, currency: &str, application_fee_cents: u64, description: &str, idempotency_key: &str) -> Result<zeroship_control::stripe_client::ConnectPaymentIntent, zeroship_control::stripe_store::StripeError> {
        self.inner.create_connect_payment_intent(connected_account, amount_cents, currency, application_fee_cents, description, idempotency_key).await
    }
    async fn create_refund(&self, provider_invoice_id: &str, amount_cents: u64, currency: &str, idempotency_key: &str) -> Result<String, zeroship_control::stripe_store::StripeError> {
        self.inner.create_refund(provider_invoice_id, amount_cents, currency, idempotency_key).await
    }
    async fn invoice_settlement_ids(&self, provider_invoice_id: &str) -> Result<(Option<String>, Option<String>), zeroship_control::stripe_store::StripeError> {
        self.inner.invoice_settlement_ids(provider_invoice_id).await
    }
}

// ===========================================================================
// M2 (re-finalize converges): a re-drive of the crash window where Stripe
// finalized but our DB stayed draft. Stripe's `finalize_invoice` now returns
// `invoice_already_finalized`; bill_creator must treat that as SUCCESS, read back
// the finalized id (== the draft id), and converge the LOCAL finalize — never
// error-loop forever leaving the DB stranded at 'draft'.
//
// RED→GREEN: pre-fix, finalize_invoice's Api error propagates as
// RegistryError::Database, the sweep swallows it (Ok(0)) and the DB stays 'draft'
// FOREVER on every re-drive (each re-drive re-finalizes → same error). The M2 fix
// makes the re-drive converge to 'finalized' with the invoice ref recorded.
// ===========================================================================

#[compio::test]
async fn refinalize_already_finalized_converges_locally() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = build_fixture(&url, "m2converge").await;
    let _recon = RECONCILE_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let now = now_for_closed_period();
    let period = prev_period(now);

    let creator = make_user(&fx.state, "m2converge").await;
    let plan = make_plan(&fx.state).await;
    let app = make_owned_app(&fx.state, &plan, creator).await;
    fx.state
        .stripe_store
        .set_customer(creator, &format!("cus_m2converge_{}", Uuid::new_v4().simple()))
        .await
        .unwrap();
    ingest_at(&fx.state, app, 450, period, 1).await; // 450c

    // First drive: items + draft post for real, but finalize reports the invoice
    // is ALREADY finalized on Stripe (the crash-after-finalize window). The drive
    // must CONVERGE the local finalize, not error-loop.
    let decorated = FinalizeAlreadyFinalized { inner: dummy_passthrough(&fx) };
    // `billed` is a FLEET-wide count (the sweep bills every un-finalized creator
    // with usage in `period`), so other tests' leftovers can inflate it; assert
    // on THIS creator's converged outcome below rather than the exact count. The
    // key M2 guarantee is that the drive did NOT error-loop (it returned Ok and
    // this creator converged), which a pre-fix run could not do.
    let billed = billing_reconcile::tick_with(&fx.state, &decorated, now)
        .await
        .expect("tick converges on already-finalized (no error loop)");
    assert!(billed >= 1, "the re-finalize converges (at least this creator billed)");

    // The DB is now finalized at the real amount — NOT stranded at 'draft'.
    assert_eq!(
        read_invoice(&fx.state, creator, period).await,
        Some(("finalized".to_string(), 450)),
        "local finalize converged to 'finalized' with the real amount",
    );
    // The invoice provider-ref was recorded (M1's atomic pair), so lookup is
    // auditable. The recorded id is the draft id (finalize does not change the id).
    let persisted_draft = draft_invoice_id(&fx.state, creator, period).await.expect("draft id persisted");
    let finalized = finalized_invoice_id(&fx.state, creator, period).await.expect("finalized ref recorded");
    assert_eq!(
        finalized, persisted_draft,
        "the recorded finalized id is the draft id (Stripe finalize does not change the id)",
    );
}

/// M1 decorator: `finalize_invoice` returns a FIXED provider invoice id, so the
/// test can pre-seed a colliding `billing_provider_refs(provider,ref_kind,external_id)`
/// row and force the finalize-ref INSERT to violate the UNIQUE constraint —
/// exercising the failure window BETWEEN the finalize UPDATE and the ref INSERT.
struct FinalizeReturnsFixedId {
    inner: StripeClient,
    fixed_id: String,
}

impl StripeApi for FinalizeReturnsFixedId {
    async fn create_customer(&self, email: &str, creator_id: &str) -> Result<String, zeroship_control::stripe_store::StripeError> {
        self.inner.create_customer(email, creator_id).await
    }
    async fn create_checkout_setup_session(&self, c: &str, ok: &str, cancel: &str) -> Result<String, zeroship_control::stripe_store::StripeError> {
        self.inner.create_checkout_setup_session(c, ok, cancel).await
    }
    async fn create_invoice_item(
        &self,
        customer: &str,
        amount_cents: u64,
        currency: &str,
        description: &str,
        period: Period,
        idempotency_key: &str,
        lookup_key: &str,
        metadata: &[(String, String)],
    ) -> Result<String, zeroship_control::stripe_store::StripeError> {
        self.inner
            .create_invoice_item(customer, amount_cents, currency, description, period, idempotency_key, lookup_key, metadata)
            .await
    }
    async fn delete_invoice_item(&self, item_id: &str) -> Result<(), zeroship_control::stripe_store::StripeError> {
        self.inner.delete_invoice_item(item_id).await
    }
    async fn find_invoice_item_by_key(&self, customer: &str, lookup_key: &str) -> Result<Option<String>, zeroship_control::stripe_store::StripeError> {
        self.inner.find_invoice_item_by_key(customer, lookup_key).await
    }
    async fn create_invoice(&self, customer: &str, creator_id: &str, idempotency_key: &str) -> Result<String, zeroship_control::stripe_store::StripeError> {
        self.inner.create_invoice(customer, creator_id, idempotency_key).await
    }
    async fn finalize_invoice(&self, _invoice_id: &str) -> Result<String, zeroship_control::stripe_store::StripeError> {
        Ok(self.fixed_id.clone())
    }
    async fn create_meter_event(&self, event_name: &str, customer: &str, value: u64, identifier: &str, timestamp: i64) -> Result<(), zeroship_control::stripe_store::StripeError> {
        self.inner.create_meter_event(event_name, customer, value, identifier, timestamp).await
    }
    async fn meter_event_summary(&self, meter_id: &str, customer: &str, start_time: i64, end_time: i64) -> Result<u64, zeroship_control::stripe_store::StripeError> {
        self.inner.meter_event_summary(meter_id, customer, start_time, end_time).await
    }
    async fn create_connect_account(&self, email: &str, creator_id: &str, country: &str) -> Result<String, zeroship_control::stripe_store::StripeError> {
        self.inner.create_connect_account(email, creator_id, country).await
    }
    async fn create_account_link(&self, account_id: &str, refresh_url: &str, return_url: &str) -> Result<String, zeroship_control::stripe_store::StripeError> {
        self.inner.create_account_link(account_id, refresh_url, return_url).await
    }
    async fn retrieve_account(&self, account_id: &str) -> Result<zeroship_control::stripe_client::ConnectAccount, zeroship_control::stripe_store::StripeError> {
        self.inner.retrieve_account(account_id).await
    }
    async fn create_connect_payment_intent(&self, connected_account: &str, amount_cents: u64, currency: &str, application_fee_cents: u64, description: &str, idempotency_key: &str) -> Result<zeroship_control::stripe_client::ConnectPaymentIntent, zeroship_control::stripe_store::StripeError> {
        self.inner.create_connect_payment_intent(connected_account, amount_cents, currency, application_fee_cents, description, idempotency_key).await
    }
    async fn create_refund(&self, provider_invoice_id: &str, amount_cents: u64, currency: &str, idempotency_key: &str) -> Result<String, zeroship_control::stripe_store::StripeError> {
        self.inner.create_refund(provider_invoice_id, amount_cents, currency, idempotency_key).await
    }
    async fn invoice_settlement_ids(&self, provider_invoice_id: &str) -> Result<(Option<String>, Option<String>), zeroship_control::stripe_store::StripeError> {
        self.inner.invoice_settlement_ids(provider_invoice_id).await
    }
}

// ===========================================================================
// M1 (atomic finalize→provider-ref): the finalize UPDATE and the invoice
// provider-ref INSERT commit in ONE transaction, so a failure of the ref INSERT
// rolls BACK the finalize — the invoice can never be left 'finalized' with NO
// 'invoice' ref (the un-auditable partial state where lookup_invoice_id returns
// None forever).
//
// We force the ref INSERT to fail by pre-seeding a DIFFERENT invoice whose
// `billing_provider_refs(provider='stripe', ref_kind='invoice', external_id=<fixed>)`
// collides on the UNIQUE(provider, ref_kind, external_id) constraint with the id
// the decorated finalize returns. The ref INSERT then errors INSIDE the txn.
//
// RED→GREEN: pre-fix (two separate autocommits) the finalize UPDATE commits FIRST,
// then the ref INSERT errors — leaving 'finalized' + NO invoice ref. Post-fix the
// transaction rolls back the finalize too, so the invoice stays 'draft' (a clean
// retry) and is NEVER finalized-without-ref.
// ===========================================================================

#[compio::test]
async fn finalize_and_invoice_ref_commit_atomically() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = build_fixture(&url, "m1atomic").await;
    let _recon = RECONCILE_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    // Use a DISTINCT closed period (~5 months back) so this test's PERMANENT
    // draft-invoice leftover (the finalize is intentionally never allowed to
    // commit) can never be swept by a sibling reconcile test billing the
    // current-prev month — which would inflate that sibling's fleet-wide `billed`.
    let now = now_for_closed_period() - 150 * 86_400;
    let period = prev_period(now);

    let creator = make_user(&fx.state, "m1atomic").await;
    let plan = make_plan(&fx.state).await;
    let app = make_owned_app(&fx.state, &plan, creator).await;
    fx.state
        .stripe_store
        .set_customer(creator, &format!("cus_m1atomic_{}", Uuid::new_v4().simple()))
        .await
        .unwrap();
    ingest_at(&fx.state, app, 350, period, 1).await; // 350c

    // Pre-seed a SEPARATE finalized invoice that already owns the fixed external_id
    // under (provider='stripe', ref_kind='invoice') — so the decorated finalize's
    // ref INSERT will violate UNIQUE(provider, ref_kind, external_id).
    let fixed_id = format!("in_collide_{}", Uuid::new_v4().simple());
    let other_creator = make_user(&fx.state, "m1other").await;
    fx.state
        .stripe_store
        .set_customer(other_creator, &format!("cus_m1other_{}", Uuid::new_v4().simple()))
        .await
        .unwrap();
    let other_inv = zeroship_core::typed_id::new_invoice_id();
    // A finalized invoice for a DIFFERENT (creator, period) holding the fixed id.
    let other_period = period_d(billing_reconcile::previous_period_start_unix(
        billing_reconcile::previous_period_start_unix(now),
    ));
    fx.state
        .control_pg
        .execute(
            "INSERT INTO zeroship.invoices (id, creator_id, period, subtotal_cents, total_cents, \
               status, finalized_at) VALUES ($1, $2, $3::date, 1, 1, 'finalized', NOW())",
            &[&other_inv, &other_creator, &other_period],
        )
        .await
        .expect("seed other finalized invoice");
    fx.state
        .control_pg
        .execute(
            "INSERT INTO zeroship.billing_provider_refs (invoice_id, provider, ref_kind, external_id) \
             VALUES ($1, 'stripe', 'invoice', $2)",
            &[&other_inv, &fixed_id],
        )
        .await
        .expect("seed colliding invoice ref");

    // Drive the reconcile: finalize returns the fixed id → the ref INSERT collides
    // → the txn must roll back the finalize.
    let decorated = FinalizeReturnsFixedId { inner: dummy_passthrough(&fx), fixed_id: fixed_id.clone() };
    // The colliding finalize is a per-creator error the sweep swallows + continues
    // past (so the tick still returns Ok). We assert on THIS creator's state below
    // rather than the fleet-wide count (other tests' creators may also be swept).
    let _ = billing_reconcile::tick_with(&fx.state, &decorated, now)
        .await
        .expect("sweep swallows the per-creator error and returns Ok");

    // THE guarantee: the invoice is NOT left 'finalized' (the txn rolled the
    // finalize UPDATE back when the ref INSERT failed). It stays 'draft' — a clean
    // retry — and crucially is NEVER finalized-without-an-invoice-ref.
    let inv = read_invoice(&fx.state, creator, period).await;
    assert_eq!(
        inv.map(|(s, _)| s).as_deref(),
        Some("draft"),
        "finalize+ref are atomic: a failed ref INSERT rolls back the finalize (no half-commit)",
    );
    assert!(
        finalized_invoice_id(&fx.state, creator, period).await.is_none(),
        "no finalized invoice ref — and since the invoice is not finalized, the partial \
         'finalized-without-ref' state never occurs (lookup_invoice_id can't strand at None)",
    );
}
