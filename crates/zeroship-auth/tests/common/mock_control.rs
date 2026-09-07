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
//! It also runs the REAL inbound guard - `verify_service_call` against a trust
//! bundle holding this fixture's auth public key, audienced to the control
//! plane's issuer and gated on `CONTROL_ERASURE_PREFLIGHT` - so a test can prove
//! the auth service presents an assertion naming itself, not merely that it
//! reached an endpoint which never looked. A credential this fixture refuses is
//! refused for the same reason control would refuse it.

// Test-only fixture: structural `future_not_send` is inherited from ntex.
#![allow(clippy::future_not_send, dead_code)]

use std::sync::{Arc, Mutex};

use ntex::web::{self, HttpRequest, HttpResponse};
use serde_json::json;
use zeroship_core::service_assertion::{
    InMemoryReplayStore, ServiceAssertionVerifier, ServiceIssuer, ServiceSigningKey,
    ServiceTrustBundle,
};
use zeroship_core::service_identity::{endpoints, verify_service_call};
use zeroship_core::service_peers::{
    service_issuer, ServiceKeyring, AUTH_SERVICE_NAME, CONTROL_SERVICE_NAME,
};

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
    verifier: ServiceAssertionVerifier,
    control_issuer: ServiceIssuer,
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
    /// The auth-side keyring whose public half this mock trusts. Handed to
    /// `ControlAccess`; a keyring built anywhere else is refused.
    keyring: Arc<ServiceKeyring>,
    state: Arc<State>,
    pub srv: ntex::web::test::TestServer,
}

/// A keyring for `svc/auth` that NO mock trusts.
///
/// The negative control: same shape, same code path, one variable changed. A
/// test using it proves the refusal comes from the credential rather than from
/// the fixture being unreachable.
#[must_use]
pub fn untrusted_auth_keyring() -> Arc<ServiceKeyring> {
    let issuer = service_issuer(AUTH_SERVICE_NAME).expect("auth issuer");
    Arc::new(
        ServiceKeyring::from_parts(issuer, ServiceSigningKey::generate(), ServiceTrustBundle::new())
            .expect("build an untrusted auth keyring"),
    )
}

impl MockControl {
    pub async fn start(answer: Answer) -> Self {
        let auth_issuer = service_issuer(AUTH_SERVICE_NAME).expect("auth issuer");
        let control_issuer = service_issuer(CONTROL_SERVICE_NAME).expect("control issuer");
        let signing = ServiceSigningKey::generate();
        let mut trusted = ServiceTrustBundle::new();
        trusted
            .trust_signing_key(&auth_issuer, signing.key_id(), &signing)
            .expect("trust the fixture's auth key");
        let keyring = Arc::new(
            ServiceKeyring::from_parts(auth_issuer, signing, ServiceTrustBundle::new())
                .expect("build the auth keyring"),
        );
        let state = Arc::new(State {
            verifier: ServiceAssertionVerifier::new(
                trusted,
                Arc::new(InMemoryReplayStore::new()),
            ),
            control_issuer,
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
            keyring,
            state,
            srv,
        }
    }

    /// The keyring whose assertions this mock accepts.
    #[must_use]
    pub fn keyring(&self) -> Arc<ServiceKeyring> {
        self.keyring.clone()
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
        .and_then(|v| v.to_str().ok());
    // The SAME guard control runs on this route: the assertion must verify under
    // a trusted issuer, be audienced to the control plane, and carry the
    // endpoint grant. Nothing here special-cases the fixture.
    if verify_service_call(
        &state.verifier,
        presented,
        state.control_issuer.as_str(),
        endpoints::CONTROL_ERASURE_PREFLIGHT,
    )
    .await
    .is_err()
    {
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
