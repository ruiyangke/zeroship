//! In-process stand-in for the control plane's erasure preflight.
//!
//! The auth service asks the control plane whether a human is the last owner of
//! a live organization before it opens a deletion window, and again before the
//! reaper erases (`zeroship_auth::control_client::erasure_preflight`). That is
//! an HTTP call by design: MEASURED, `zeroship_auth` holds no privilege on any
//! organization table, so the question cannot be answered by a query from this
//! process.
//!
//! This serves the SAME path and the SAME wire shape over a real loopback
//! socket, so the client's URL construction, bearer header, status handling and
//! JSON parse are all exercised. What it does NOT exercise is control's SQL -
//! that belongs to `crates/zeroship-control/tests/`, where the tables are.
//!
//! It also enforces the bearer token, so a test can prove the auth service
//! actually presents its control key rather than reaching an endpoint that
//! never looked.

// Test-only fixture: structural `future_not_send` is inherited from ntex.
#![allow(clippy::future_not_send, dead_code)]

use std::sync::{Arc, Mutex};

use ntex::web::{self, HttpRequest, HttpResponse};
use serde_json::json;

/// What the mock answers with.
#[derive(Debug, Clone)]
pub enum Answer {
    /// `200` with both blocker lists empty - erasure may proceed.
    Clear,
    /// `200` with one OWNERSHIP blocker naming this organization slug.
    SoleOwnerOf { slug: String },
    /// `200` with one MONEY blocker: an organization whose last owner seat is
    /// this human's and which still owes. Its organization is DISSOLVED, which
    /// is the shape the ownership rule cannot produce - so a test using this
    /// arm cannot pass by accident on the older rule.
    OwesBilling { slug: String, owed_cents: i64 },
    /// `500` - the control plane could not compute the answer. NOT the same as
    /// `Clear`, and the whole point of having this arm.
    Unavailable,
}

struct State {
    key: String,
    answer: Mutex<Answer>,
    /// Every principal id the mock was asked about, in order. A test asserting
    /// "the preflight ran" reads this rather than inferring it from an outcome
    /// that a skipped call would also produce.
    asked: Mutex<Vec<String>>,
}

pub struct MockControl {
    /// Loopback base URL - what `--control-url` / `ControlAccess.control_url`
    /// is pointed at.
    pub base: String,
    /// The bearer token the mock demands. A request without it gets `401`.
    pub key: String,
    state: Arc<State>,
    pub srv: ntex::web::test::TestServer,
}

impl MockControl {
    pub async fn start(answer: Answer) -> Self {
        let key = format!("mock-control-key-{}", uuid::Uuid::new_v4().simple());
        let state = Arc::new(State {
            key: key.clone(),
            answer: Mutex::new(answer),
            asked: Mutex::new(Vec::new()),
        });
        let factory_state = state.clone();
        let srv = ntex::web::test::server(move || {
            let state = factory_state.clone();
            async move {
                web::App::new().state(state).service(
                    web::resource("/internal/principals/{principal_id}/erasure-preflight")
                        .route(web::get().to(preflight)),
                )
            }
        })
        .await;
        let base = format!("http://{}", srv.addr());
        Self {
            base,
            key,
            state,
            srv,
        }
    }

    /// Change the answer between calls - the reaper asks again after the
    /// request did, and a blocker that appeared inside the grace window is
    /// exactly the case that needs proving.
    pub fn set(&self, answer: Answer) {
        *self.state.answer.lock().expect("answer lock") = answer;
    }

    pub fn asked(&self) -> Vec<String> {
        self.state.asked.lock().expect("asked lock").clone()
    }
}

async fn preflight(
    req: HttpRequest,
    state: web::types::State<Arc<State>>,
    principal_id: web::types::Path<String>,
) -> HttpResponse {
    let presented = req
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if presented != format!("Bearer {}", state.key) {
        return HttpResponse::Unauthorized().json(&json!({"error": "unauthorized"}));
    }
    state
        .asked
        .lock()
        .expect("asked lock")
        .push(principal_id.to_string());
    let answer = state.answer.lock().expect("answer lock").clone();
    match answer {
        Answer::Clear => HttpResponse::Ok().json(&json!({
            "principal_id": principal_id.to_string(),
            "blockers": [],
            "billing_blockers": [],
        })),
        Answer::SoleOwnerOf { slug } => HttpResponse::Ok().json(&json!({
            "principal_id": principal_id.to_string(),
            "blockers": [{
                "organization_id": format!("org_{slug}"),
                "organization_slug": slug,
                "organization_name": slug,
                "personal": true,
                "other_member_count": 0,
                "project_count": 0,
                "remedy": "dissolve",
            }],
            "billing_blockers": [],
        })),
        Answer::OwesBilling { slug, owed_cents } => HttpResponse::Ok().json(&json!({
            "principal_id": principal_id.to_string(),
            // EMPTY on purpose. The ownership rule only looks at LIVE
            // organizations, so a dissolved one cannot appear here - which is
            // what makes a test on this arm rule on the money rule alone.
            "blockers": [],
            "billing_blockers": [{
                "organization_id": format!("org_{slug}"),
                "organization_slug": slug,
                "organization_name": slug,
                "personal": false,
                "dissolved": true,
                "owed_cents": owed_cents,
                "currency": "usd",
                "unpaid_invoice_count": 1,
                "unbilled_period_count": 0,
                "remedy": "settle_invoices",
                // Control sends the per-invoice detail beside the summary and
                // the client deliberately does not deserialize it. It is sent
                // anyway, and kept consistent with the summary above, so this
                // body is one the real handler could have produced - a mock
                // that omitted it would stop exercising the client's tolerance
                // of the fields it ignores.
                "outstanding": {
                    "organization_id": format!("org_{slug}"),
                    "unpaid_invoices": [{
                        "invoice_id": format!("inv_{slug}"),
                        "period": "2026-08-01",
                        "currency": "usd",
                        "total_cents": owed_cents,
                        "cash_collected_cents": 0,
                        "owed_cents": owed_cents,
                    }],
                    "unbilled_periods": [],
                },
            }],
        })),
        Answer::Unavailable => {
            HttpResponse::InternalServerError().json(&json!({"error": "preflight unavailable"}))
        }
    }
}
