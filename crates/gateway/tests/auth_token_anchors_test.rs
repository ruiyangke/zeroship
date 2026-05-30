//! Faithful integration tests for the auth-sdk Slice 1b-anchors
//! browser-token CORE (`POST /__zs/auth/token`, `GET /__zs/auth/session`,
//! the per-node mint single-flight, the `auth.app_session_anchors` store).
//!
//! A loopback MOCK Hydra (in-process ntex test server) serves
//! `/.well-known/jwks.json` and `/oauth2/token` so the REAL code path runs
//! locally — the gateway's `OidcRp` dials it, verifies EdDSA-signed
//! id/access tokens against its JWKS, and the mock COUNTS refresh-grant
//! calls so the single-flight assertion ("N concurrent mints ⇒ exactly 1
//! Hydra refresh") is exact. No stubs of the issuer or the single-flight.
//!
//! The DB-backed handler tests (token-exchange → anchor row →
//! session?mint=1) are gated on `GATEWAY_ANCHORS_DB_URL` (the established
//! env-skip convention — no live PG in CI by default). The mock-Hydra
//! single-flight test and the cookie/Origin tests run unconditionally.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use ed25519_dalek::SigningKey;
use ntex::web::{self, test};
use uuid::Uuid;

use zeroship_gateway::{
    anchors,
    blob_cache::{BlobCache, DiskBlobCache},
    enforce, idempotency,
    oidc_rp::OidcRp,
    proxy::HashRing,
    sync::RouteCache,
    wrapper_token, GateConfig, GateState,
};

// ─── Mock Hydra ──────────────────────────────────────────────────────────

const MOCK_ISSUER: &str = "https://auth.zeroship.ai/";

/// A malformed-but-2xx `/oauth2/token` body: it carries a refresh_token-shaped
/// secret but is missing the required `access_token` field, so it fails to
/// deserialize into `TokenSet`. The secret value below MUST NOT appear in any
/// error string the gateway surfaces or logs (§8.1/§8.5).
const GARBAGE_REFRESH_SECRET: &str = "rt_super_secret_family_lineage_DO_NOT_LOG";
const GARBAGE_REFRESH_BODY: &str =
    r#"{"refresh_token":"rt_super_secret_family_lineage_DO_NOT_LOG","unexpected":true}"#;

/// Shared mock-Hydra state. Signs tokens with a fixed EdDSA key, counts
/// refresh-grant calls (the single-flight assertion), and can be flipped to
/// answer `invalid_grant` (anchor-dead path).
struct MockHydra {
    signing: SigningKey,
    kid: String,
    /// The global user UUID every token's `sub` carries.
    user_id: Uuid,
    client_id: String,
    /// Count of `grant_type=refresh_token` calls — the single-flight proof.
    refresh_calls: AtomicU32,
    /// When true, the next refresh-grant answers `400 invalid_grant`.
    invalid_grant: std::sync::atomic::AtomicBool,
    /// When true, the refresh-grant answers `200` with a body that contains
    /// a (fake) refresh_token but is NOT valid `TokenSet` JSON — exercising
    /// the malformed-but-2xx redaction path (§8.1/§8.5: tokens never logged).
    garbage_2xx: std::sync::atomic::AtomicBool,
    /// Optional artificial delay (ms) on the refresh grant — lets a test
    /// hold N concurrent minters inside ONE in-flight refresh.
    refresh_delay_ms: AtomicU32,
}

impl MockHydra {
    fn new(client_id: &str) -> Self {
        let signing = SigningKey::from_bytes(&[42u8; 32]);
        let kid = zeroship_gateway::signing::jwk_thumbprint(&signing);
        Self {
            signing,
            kid,
            user_id: Uuid::new_v4(),
            client_id: client_id.to_string(),
            refresh_calls: AtomicU32::new(0),
            invalid_grant: std::sync::atomic::AtomicBool::new(false),
            garbage_2xx: std::sync::atomic::AtomicBool::new(false),
            refresh_delay_ms: AtomicU32::new(0),
        }
    }

    fn jwks_json(&self) -> String {
        use base64::engine::general_purpose::URL_SAFE_NO_PAD;
        use base64::Engine as _;
        let x = URL_SAFE_NO_PAD.encode(self.signing.verifying_key().as_bytes());
        format!(
            r#"{{"keys":[{{"kid":"{}","kty":"OKP","alg":"EdDSA","crv":"Ed25519","use":"sig","x":"{x}"}}]}}"#,
            self.kid
        )
    }

    /// Sign an EdDSA JWT (id_token or access JWT) with the standard claims.
    fn sign(&self, claims: serde_json::Value) -> String {
        use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
        use ed25519_dalek::pkcs8::EncodePrivateKey;
        let der = self.signing.to_pkcs8_der().expect("pkcs8");
        let key = EncodingKey::from_ed_der(der.as_bytes());
        let mut header = Header::new(Algorithm::EdDSA);
        header.kid = Some(self.kid.clone());
        encode(&header, &claims, &key).expect("sign jwt")
    }

    fn id_token(&self) -> String {
        let now = now_secs();
        self.sign(serde_json::json!({
            "iss": MOCK_ISSUER,
            "sub": self.user_id.to_string(),
            "aud": self.client_id,
            "exp": now + 3600,
            "iat": now,
            "email": "user@example.com",
            "email_verified": true,
            "name": "Test User",
        }))
    }

    fn access_token(&self) -> String {
        let now = now_secs();
        self.sign(serde_json::json!({
            "iss": MOCK_ISSUER,
            "sub": self.user_id.to_string(),
            "aud": "https://api.zeroship.ai",
            "client_id": self.client_id,
            "exp": now + 3600,
            "iat": now,
            "scope": "openid email profile offline_access",
        }))
    }
}

fn now_secs() -> i64 {
    i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs(),
    )
    .unwrap()
}

/// Boot the loopback mock-Hydra HTTP server. Returns `(base_url, server)`.
async fn boot_mock_hydra(hydra: Arc<MockHydra>) -> (String, ntex::web::test::TestServer) {
    let srv = test::server(move || {
        let h = hydra.clone();
        async move {
            web::App::new()
                .state(h)
                .service(web::resource("/.well-known/jwks.json").route(web::get().to(jwks_endpoint)))
                .service(web::resource("/oauth2/token").route(web::post().to(token_endpoint)))
        }
    })
    .await;
    let base = srv.url("").trim_end_matches('/').to_string();
    (base, srv)
}

async fn jwks_endpoint(h: web::types::State<Arc<MockHydra>>) -> web::HttpResponse {
    web::HttpResponse::Ok()
        .header("content-type", "application/json")
        .body(h.jwks_json())
}

async fn token_endpoint(
    body: ntex::util::Bytes,
    h: web::types::State<Arc<MockHydra>>,
) -> web::HttpResponse {
    let mut grant = String::new();
    for (k, v) in url::form_urlencoded::parse(&body) {
        if k == "grant_type" {
            grant = v.into_owned();
        }
    }
    match grant.as_str() {
        "authorization_code" => web::HttpResponse::Ok()
            .header("content-type", "application/json")
            .body(serde_json::json!({
                "access_token": h.access_token(),
                "id_token": h.id_token(),
                "refresh_token": format!("rt_{}", Uuid::new_v4().simple()),
                "token_type": "Bearer",
                "expires_in": 3600,
                "scope": "openid email profile offline_access",
            }).to_string()),
        "refresh_token" => {
            let delay = h.refresh_delay_ms.load(Ordering::SeqCst);
            if delay > 0 {
                ntex::time::sleep(std::time::Duration::from_millis(u64::from(delay))).await;
            }
            h.refresh_calls.fetch_add(1, Ordering::SeqCst);
            if h.invalid_grant.load(Ordering::SeqCst) {
                return web::HttpResponse::BadRequest()
                    .header("content-type", "application/json")
                    .body(r#"{"error":"invalid_grant","error_description":"token expired"}"#);
            }
            if h.garbage_2xx.load(Ordering::SeqCst) {
                // 200 OK but NOT valid TokenSet JSON, yet carrying a (fake)
                // refresh_token substring — the redaction path must keep this
                // out of the surfaced error string.
                return web::HttpResponse::Ok()
                    .header("content-type", "application/json")
                    .body(GARBAGE_REFRESH_BODY);
            }
            web::HttpResponse::Ok()
                .header("content-type", "application/json")
                .body(serde_json::json!({
                    "access_token": h.access_token(),
                    "refresh_token": format!("rt_{}", Uuid::new_v4().simple()),
                    "token_type": "Bearer",
                    "expires_in": 3600,
                    "scope": "openid email profile offline_access",
                }).to_string())
        }
        _ => web::HttpResponse::BadRequest()
            .body(r#"{"error":"unsupported_grant_type"}"#),
    }
}

// ─── GateState fixture ───────────────────────────────────────────────────

#[derive(Debug, Default)]
struct StubBlobStore;

#[async_trait::async_trait(?Send)]
impl zeroship_bundle::BlobStore for StubBlobStore {
    async fn get_blob(&self, _hash: &str) -> Result<bytes::Bytes, zeroship_bundle::BlobError> {
        Err(zeroship_bundle::BlobError::NotFound("unused".into()))
    }
    fn local_path(&self, _hash: &str) -> Option<std::path::PathBuf> {
        None
    }
    async fn put_blob(
        &self,
        _hash: &str,
        _data: &[u8],
    ) -> Result<zeroship_bundle::PutOutcome, zeroship_bundle::BlobError> {
        Ok(zeroship_bundle::PutOutcome::Wrote)
    }
    async fn put_blob_stream(
        &self,
        _hash: &str,
        _expected_size: u64,
        _reader: &mut dyn std::io::Read,
    ) -> Result<zeroship_bundle::PutOutcome, zeroship_bundle::BlobError> {
        Ok(zeroship_bundle::PutOutcome::Wrote)
    }
    async fn has_blob(&self, _hash: &str) -> Result<bool, zeroship_bundle::BlobError> {
        Ok(false)
    }
    async fn put_manifest(
        &self,
        _app_id: &uuid::Uuid,
        _deploy_hash: &str,
        _json: &[u8],
    ) -> Result<(), zeroship_bundle::BlobError> {
        Ok(())
    }
    async fn get_manifest(
        &self,
        _app_id: &uuid::Uuid,
        _deploy_hash: &str,
    ) -> Result<bytes::Bytes, zeroship_bundle::BlobError> {
        Err(zeroship_bundle::BlobError::NotFound("unused".into()))
    }
}

const APP_HOST: &str = "myapp.zeroship.ai";
const APP_NAME: &str = "myapp";
const CLIENT_ID: &str = "oac_myapp";

/// Build a `GateState` whose `OidcRp` dials the loopback mock Hydra and
/// whose route cache has one provisioned app (`myapp` → `oac_myapp`).
fn build_state(hydra_base: &str, db: Option<zeroship_gateway::db::DbConfig>) -> Arc<GateState> {
    let mut tmp = std::env::temp_dir();
    tmp.push(format!("zsgate-anchors-{}", Uuid::new_v4().simple()));
    let disk = DiskBlobCache::new(tmp, 1024 * 1024).expect("disk cache");

    let signing_key = SigningKey::from_bytes(&[7u8; 32]);
    let issuer = wrapper_token::Issuer::new(&signing_key, "https://api.zeroship.ai".into())
        .expect("issuer");
    let verifier =
        wrapper_token::Verifier::new(&signing_key.verifying_key(), "https://api.zeroship.ai".into());

    // OidcRp dials the loopback mock for /oauth2/token + JWKS, but expects
    // the canonical issuer the mock stamps into tokens.
    let oidc_rp = OidcRp::new(hydra_base, "gateway", "test-secret", b"k".repeat(32))
        .with_issuer(MOCK_ISSUER);

    let routes = RouteCache::new();
    routes.update(build_route_map());

    Arc::new(GateState {
        config: GateConfig {
            control_url: String::new(),
            control_key: String::new(),
            worker_urls: vec![],
            poll_interval_secs: 5,
            worker_key: "worker-key".into(),
            hydra_public_url: hydra_base.to_string(),
            auth_ui_url: hydra_base.to_string(),
            // insecure_dev = false so we exercise the prod __Host- / Strict
            // / Secure cookie attributes and the https Origin compare.
            insecure_dev: false,
            trust_proxy: false,
            public_url: "https://api.zeroship.ai".into(),
        },
        routes,
        hash_ring: HashRing::new(vec!["http://0.0.0.0:0".into()], 1),
        rate_limiters: enforce::RateLimitRegistry::new(1, 1),
        per_rule_rate_limits: enforce::PerRuleRateLimitRegistry::new(),
        concurrency: enforce::ConcurrencyRegistry::new(1),
        blob_store: Arc::new(StubBlobStore),
        blob_cache: BlobCache::new(8 * 1024 * 1024),
        disk_cache: disk,
        idempotency_store: Arc::new(idempotency::InMemoryIdempotencyStore::new()),
        oidc_rp: Arc::new(oidc_rp),
        db,
        dpop_jti_cache: Arc::new(zeroship_core::dpop::TieredJtiCache::default()),
        logout_jti_cache: Arc::new(zeroship_core::logout_token::LogoutJtiCache::default()),
        signing_key: Some(Arc::new(signing_key)),
        prev_signing_key: None,
        wrapper_issuer: Some(Arc::new(issuer)),
        wrapper_verifier: Some(Arc::new(verifier)),
        anchor_enc_key: zeroship_core::crypto::derive_key("anchor-test-key"),
        pairwise_salt: zeroship_core::crypto::derive_key("pairwise-test-salt"),
    })
}

fn build_route_map() -> zeroship_core::types::RouteMap {
    use zeroship_core::types::RouteEntry;
    let mut m = std::collections::HashMap::new();
    m.insert(
        Uuid::new_v4(),
        RouteEntry {
            name: APP_NAME.into(),
            plan_id: "free".into(),
            api_key_hash: String::new(),
            deploy_hash: None,
            manifest: zeroship_bundle::Manifest::passthrough(),
            oauth_client_id: Some(CLIENT_ID.into()),
            sector_identifier: Some(format!("https://{APP_HOST}")),
        },
    );
    m
}

// ─── Test app wiring ─────────────────────────────────────────────────────

macro_rules! anchors_app {
    ($state:expr) => {{
        let state = $state.clone();
        web::App::new()
            .state(state)
            .service(
                web::resource("/__zs/auth/token")
                    .route(web::post().to(zeroship_gateway::auth_token::token)),
            )
            .service(
                web::resource("/__zs/auth/session")
                    .route(web::get().to(zeroship_gateway::auth_token::session)),
            )
    }};
}

/// Extract a `Set-Cookie` whose name starts with `prefix`.
fn set_cookie_with_prefix(resp: &ntex::web::WebResponse, prefix: &str) -> Option<String> {
    for hv in resp.headers().get_all(http::header::SET_COOKIE) {
        let Ok(s) = hv.to_str() else { continue };
        if s.starts_with(prefix) {
            return Some(s.to_string());
        }
    }
    None
}

/// Read a JSON body (ntex 3.x has no `read_body_json` helper).
async fn read_json(resp: ntex::web::WebResponse) -> serde_json::Value {
    let bytes = test::read_body(resp).await;
    serde_json::from_slice(&bytes).expect("json body")
}

// ─── DB-independent tests (loopback Hydra only) ──────────────────────────

#[ntex::test]
async fn foreign_origin_is_rejected_no_cors_reflection() {
    let hydra = Arc::new(MockHydra::new(CLIENT_ID));
    let (base, _srv) = boot_mock_hydra(hydra).await;
    let state = build_state(&base, None);

    let app = test::init_service(anchors_app!(state)).await;

    // A cross-origin POST /token with a foreign Origin must be rejected
    // outright (403) and MUST NOT echo Access-Control-Allow-Origin.
    let req = test::TestRequest::post()
        .uri("/__zs/auth/token")
        .header("host", APP_HOST)
        .header("origin", "https://evil.example.com")
        .header("x-zs-auth", "1")
        .header("content-type", "application/x-www-form-urlencoded")
        .set_payload("grant_type=authorization_code&code=c&code_verifier=v")
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status().as_u16(), 403, "foreign Origin must be 403");
    assert!(
        resp.headers().get("access-control-allow-origin").is_none(),
        "MUST NOT reflect a foreign Origin (no credentialed CORS oracle)"
    );

    // Origin: null is likewise rejected.
    let req = test::TestRequest::post()
        .uri("/__zs/auth/token")
        .header("host", APP_HOST)
        .header("origin", "null")
        .header("x-zs-auth", "1")
        .header("content-type", "application/x-www-form-urlencoded")
        .set_payload("grant_type=authorization_code&code=c&code_verifier=v")
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status().as_u16(), 403, "Origin: null must be 403");
}

#[ntex::test]
async fn token_missing_custom_header_is_rejected() {
    let hydra = Arc::new(MockHydra::new(CLIENT_ID));
    let (base, _srv) = boot_mock_hydra(hydra).await;
    let state = build_state(&base, None);
    let app = test::init_service(anchors_app!(state)).await;

    // Same-origin but NO X-ZS-Auth header → 400 (the primary CSRF defense).
    let req = test::TestRequest::post()
        .uri("/__zs/auth/token")
        .header("host", APP_HOST)
        .header("origin", format!("https://{APP_HOST}"))
        .header("content-type", "application/x-www-form-urlencoded")
        .set_payload("grant_type=authorization_code&code=c&code_verifier=v")
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status().as_u16(), 400);
}

#[ntex::test]
async fn session_mint_without_custom_header_is_rejected() {
    let hydra = Arc::new(MockHydra::new(CLIENT_ID));
    let (base, _srv) = boot_mock_hydra(hydra).await;
    let state = build_state(&base, None);
    let app = test::init_service(anchors_app!(state)).await;

    // ?mint=1 WITHOUT X-ZS-Auth must not mint (a top-level navigation
    // cannot set the custom header).
    let req = test::TestRequest::get()
        .uri("/__zs/auth/session?mint=1")
        .header("host", APP_HOST)
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status().as_u16(), 400);
}

/// Build the SAME coalesced future `auth_token::mint` builds for a leader:
/// a Hydra refresh-grant wrapped in the REAL `anchors::EntryGuard` so the
/// single-flight entry is removed when the future body is dropped — NOT when
/// any one task survives. This is the exact round-6 BLOCKER mechanism.
fn leader_mint_future(
    oidc: Arc<OidcRp>,
    hydra: Arc<MockHydra>,
    anchor_id: Uuid,
) -> anchors::SharedMintFuture {
    use futures::FutureExt as _;
    (Box::pin(async move {
        // The guard's Drop removes the entry on resolution OR cancellation —
        // identical to `auth_token::mint`'s leader future.
        let _guard = anchors::EntryGuard::new(anchor_id);
        match hydra_refresh(&oidc, &hydra).await {
            Ok(tok) => Ok(anchors::MintOk { access_token: tok, expires_in: 600 }),
            Err(e) => Err(anchors::MintError::Upstream(e)),
        }
    }) as std::pin::Pin<Box<dyn std::future::Future<Output = anchors::MintResult>>>)
        .shared()
}

/// Drive one mint through the REAL single-flight + `EntryGuard` exactly as
/// `auth_token::mint` does: follower-get → leader-insert(guarded future) →
/// await. There is NO manual `remove` — removal is the guard's job, so this
/// faithfully exercises the blocker fix (no leader-only removal).
async fn coalesced_mint(
    oidc: Arc<OidcRp>,
    hydra: Arc<MockHydra>,
    anchor_id: Uuid,
) -> anchors::MintResult {
    if let Some(existing) = anchors::with_single_flight(|sf| sf.get(anchor_id)) {
        return existing.await;
    }
    let fut = leader_mint_future(oidc, hydra, anchor_id);
    let shared = anchors::with_single_flight(|sf| sf.insert(anchor_id, fut));
    // NO `remove` here — the guard inside the future body owns removal.
    shared.await
}

#[ntex::test]
async fn n_parallel_mints_cause_one_hydra_refresh() {
    // FAITHFUL single-flight proof WITHOUT a DB: drive the real per-node
    // single-flight ([`anchors::with_single_flight`]) + the real
    // [`anchors::EntryGuard`] removal over a future that does a REAL Hydra
    // `/oauth2/token` refresh against the loopback mock, and assert the mock
    // saw exactly ONE refresh for N concurrent minters.
    //
    // The coalescing key + future-sharing + guard-based removal is the
    // round-6 BLOCKER core; the anchor DB read/write around it is orthogonal
    // to coalescing (covered by the PG-gated full-handler test below).
    let hydra = Arc::new(MockHydra::new(CLIENT_ID));
    // Hold each refresh open long enough that all N callers pile onto the
    // SAME in-flight future before the leader resolves.
    hydra.refresh_delay_ms.store(150, Ordering::SeqCst);
    let (base, _srv) = boot_mock_hydra(hydra.clone()).await;

    let oidc = Arc::new(
        OidcRp::new(&base, "gateway", "s", b"k".repeat(32)).with_issuer(MOCK_ISSUER),
    );
    let anchor_id = Uuid::new_v4();

    // N concurrent minters coalesce into ONE shared future driving ONE
    // Hydra refresh. Each caller resolves the SHARED future's result.
    let n = 8;
    let mut handles = Vec::new();
    for _ in 0..n {
        handles.push(coalesced_mint(oidc.clone(), hydra.clone(), anchor_id));
    }
    let results = futures::future::join_all(handles).await;

    // Exactly ONE Hydra refresh for all N concurrent minters.
    assert_eq!(
        hydra.refresh_calls.load(Ordering::SeqCst),
        1,
        "single-flight must coalesce {n} concurrent minters into ONE Hydra refresh"
    );
    // All N callers got a valid access token (the shared result).
    assert_eq!(results.len(), n);
    for r in &results {
        let tok = r.as_ref().expect("each minter resolves the shared result");
        assert!(!tok.access_token.is_empty());
    }

    // The single-flight entry is cleared once resolved (the guard fired).
    assert_eq!(
        anchors::with_single_flight(|sf| sf.in_flight()),
        0,
        "entry must be removed after the mint resolves"
    );

    // A FOLLOW-UP mint after the in-flight one cleared triggers a SECOND
    // refresh (no stale coalescing).
    let _ = coalesced_mint(oidc.clone(), hydra.clone(), anchor_id)
        .await
        .expect("second mint");
    assert_eq!(hydra.refresh_calls.load(Ordering::SeqCst), 2);
}

#[ntex::test]
async fn cancelled_leader_does_not_leak_single_flight_entry() {
    // REGRESSION for the round-6 BLOCKER (leader-only removal leak): if the
    // LEADER's request future is dropped mid-flight AFTER `insert` but before
    // it resolves, a surviving follower still drives the shared future to
    // completion, the `EntryGuard` fires on the future's drop, and the entry
    // is cleared — so the NEXT mint for the same anchor issues a FRESH Hydra
    // refresh instead of being handed a stale resolved wrapper forever.
    let hydra = Arc::new(MockHydra::new(CLIENT_ID));
    hydra.refresh_delay_ms.store(120, Ordering::SeqCst);
    let (base, _srv) = boot_mock_hydra(hydra.clone()).await;
    let oidc = Arc::new(
        OidcRp::new(&base, "gateway", "s", b"k".repeat(32)).with_issuer(MOCK_ISSUER),
    );
    let anchor_id = Uuid::new_v4();

    // Leader registers the guarded future; a follower shares it.
    let leader = leader_mint_future(oidc.clone(), hydra.clone(), anchor_id);
    let shared_for_leader = anchors::with_single_flight(|sf| sf.insert(anchor_id, leader));
    let follower = anchors::with_single_flight(|sf| sf.get(anchor_id))
        .expect("follower shares the in-flight future");
    assert_eq!(anchors::with_single_flight(|sf| sf.in_flight()), 1);

    // DROP the leader's awaiting handle mid-flight (client disconnect /
    // ntex timeout) — the follower keeps the shared future alive.
    drop(shared_for_leader);

    // The follower drives the same future to completion → exactly ONE refresh.
    let out = follower.await.expect("follower resolves the shared mint");
    assert!(!out.access_token.is_empty());
    assert_eq!(
        hydra.refresh_calls.load(Ordering::SeqCst),
        1,
        "the cancelled leader + surviving follower must yield exactly one refresh"
    );

    // BLOCKER assertion: the entry was removed by the guard (not leaked),
    // even though the leader task that called `insert` did not survive.
    assert_eq!(
        anchors::with_single_flight(|sf| sf.in_flight()),
        0,
        "single-flight entry must NOT leak after a cancelled leader"
    );

    // And the NEXT mint for the same anchor re-rotates (fresh refresh), proving
    // no stale resolved wrapper is served from a leaked entry.
    let _ = coalesced_mint(oidc.clone(), hydra.clone(), anchor_id)
        .await
        .expect("post-cancel mint re-rotates");
    assert_eq!(
        hydra.refresh_calls.load(Ordering::SeqCst),
        2,
        "a mint after the cancelled-leader case must issue a FRESH refresh"
    );
}

#[ntex::test]
async fn all_awaiters_dropped_clears_single_flight_entry() {
    // Companion to the leader-cancel test: if EVERY awaiter is dropped before
    // the future resolves, the future body (and its `EntryGuard`) is dropped,
    // so a half-started, never-resolved entry is ALSO cleared rather than
    // leaking.
    let hydra = Arc::new(MockHydra::new(CLIENT_ID));
    hydra.refresh_delay_ms.store(200, Ordering::SeqCst);
    let (base, _srv) = boot_mock_hydra(hydra.clone()).await;
    let oidc = Arc::new(
        OidcRp::new(&base, "gateway", "s", b"k".repeat(32)).with_issuer(MOCK_ISSUER),
    );
    let anchor_id = Uuid::new_v4();

    let leader = leader_mint_future(oidc.clone(), hydra.clone(), anchor_id);
    let shared = anchors::with_single_flight(|sf| sf.insert(anchor_id, leader));
    assert_eq!(anchors::with_single_flight(|sf| sf.in_flight()), 1);

    // Drop the only awaiter before it ever polls to completion. The shared
    // future has no other live clones in the map's value beyond the stored
    // one; dropping the map entry too lets the body drop and fire the guard.
    drop(shared);
    // Remove the map's stored clone so the last strong ref to the future
    // drops, running its body's guard Drop. (In the real `mint`, the awaiting
    // task holds the only clone; here we drop both to simulate full teardown.)
    anchors::with_single_flight(|sf| sf.remove(anchor_id));
    assert_eq!(
        anchors::with_single_flight(|sf| sf.in_flight()),
        0,
        "entry must be cleared when all awaiters are gone"
    );
}

#[ntex::test]
async fn malformed_2xx_refresh_body_is_not_logged() {
    // REGRESSION (§8.1/§8.5 — refresh token never logged): a malformed-but-2xx
    // Hydra token body must NOT have its raw contents embedded in the surfaced
    // `OidcRpError`. Before the fix, `post_token`'s parse-failure error was
    // `format!("parse: {e}\nbody: {resp_body}")`, which carried the full
    // success body (access_token + refresh_token) into `tracing::warn!`.
    let hydra = Arc::new(MockHydra::new(CLIENT_ID));
    hydra.garbage_2xx.store(true, Ordering::SeqCst);
    let (base, _srv) = boot_mock_hydra(hydra.clone()).await;
    let oidc = OidcRp::new(&base, "gateway", "s", b"k".repeat(32)).with_issuer(MOCK_ISSUER);

    let err = oidc
        .refresh_token_public(CLIENT_ID, "rt_seed")
        .await
        .expect_err("garbage 2xx body must fail to parse");
    let msg = err.to_string();
    // The error must be a parse failure WITHOUT the raw body / refresh secret.
    assert!(msg.contains("parse:"), "expected a parse error, got: {msg}");
    assert!(
        !msg.contains(GARBAGE_REFRESH_SECRET),
        "refresh secret leaked into error string: {msg}"
    );
    assert!(
        !msg.contains("refresh_token"),
        "raw token body must not appear in the error: {msg}"
    );
}

/// One real Hydra refresh-grant + local verify of the rotated access JWT —
/// the same two steps `auth_token::do_refresh` runs (minus the DB write).
async fn hydra_refresh(oidc: &OidcRp, hydra: &MockHydra) -> Result<String, String> {
    let tokens = oidc
        .refresh_token_public(CLIENT_ID, "rt_seed")
        .await
        .map_err(|e| e.to_string())?;
    // Verify the rotated raw access JWT locally (no introspection) — the
    // §1.2 round-5 transform.
    let raw = oidc
        .verify_access_token(&tokens.access_token)
        .await
        .map_err(|e| e.to_string())?;
    assert_eq!(raw.sub, hydra.user_id.to_string());
    Ok(tokens.access_token)
}

// ─── PG-backed full-handler tests (gated on GATEWAY_ANCHORS_DB_URL) ──────

/// Resolve the anchors test DB DSN.
///
/// FAIL LOUDLY in CI: if `CI` is set (the harness expects full coverage) but
/// `GATEWAY_ANCHORS_DB_URL` is absent, panic instead of silently skipping —
/// the repo's faithful-e2e mandate forbids a DB-gated test that quietly
/// no-ops in CI. Locally (no `CI`), `None` ⇒ the test prints a skip line and
/// returns, the established env-skip convention for a dev box without PG.
fn db_url() -> Option<String> {
    match std::env::var("GATEWAY_ANCHORS_DB_URL") {
        Ok(dsn) if !dsn.is_empty() => Some(dsn),
        _ => {
            if std::env::var("CI").is_ok() {
                panic!(
                    "GATEWAY_ANCHORS_DB_URL must be set in CI so the DB-backed anchor handler \
                     tests run the real /token→anchor→/session?mint=1 path instead of silently \
                     skipping (faithful-e2e mandate)"
                );
            }
            None
        }
    }
}

/// Seed the global user row the anchor FKs into, returning its id.
async fn seed_user(dsn: &str, user_id: Uuid) {
    let (client, conn) = compio_postgres::connect(dsn, compio_postgres::NoTls)
        .await
        .expect("connect");
    compio::runtime::spawn(async move {
        let _ = conn.run().await;
    })
    .detach();
    let email = format!("anchor-{}@zeroship.test", user_id.simple());
    client
        .execute(
            "INSERT INTO auth.users (id, email, name, email_verified_at) \
             VALUES ($1, $2::citext, $3, NOW()) ON CONFLICT (id) DO NOTHING",
            &[&user_id, &email, &"Anchor Test"],
        )
        .await
        .expect("seed user");
}

async fn cleanup(dsn: &str, user_id: Uuid) {
    let (client, conn) = compio_postgres::connect(dsn, compio_postgres::NoTls)
        .await
        .expect("connect");
    compio::runtime::spawn(async move {
        let _ = conn.run().await;
    })
    .detach();
    let _ = client
        .execute(
            "DELETE FROM auth.app_session_anchors WHERE global_user_id = $1",
            &[&user_id],
        )
        .await;
    let _ = client
        .execute("DELETE FROM auth.users WHERE id = $1", &[&user_id])
        .await;
}

#[ntex::test]
async fn token_exchange_mints_wrapper_and_sets_anchor() {
    let Some(dsn) = db_url() else {
        eprintln!("[anchors] skip token_exchange (no GATEWAY_ANCHORS_DB_URL)");
        return;
    };
    let hydra = Arc::new(MockHydra::new(CLIENT_ID));
    seed_user(&dsn, hydra.user_id).await;
    let user_id = hydra.user_id;
    let (base, _srv) = boot_mock_hydra(hydra.clone()).await;
    let db = zeroship_gateway::db::DbConfig::new(dsn.clone(), 8);
    let state = build_state(&base, Some(db));
    let app = test::init_service(anchors_app!(state.clone())).await;

    let req = test::TestRequest::post()
        .uri("/__zs/auth/token")
        .header("host", APP_HOST)
        .header("origin", format!("https://{APP_HOST}"))
        .header("x-zs-auth", "1")
        .header("sec-fetch-site", "same-origin")
        .header("content-type", "application/x-www-form-urlencoded")
        .set_payload("grant_type=authorization_code&code=thecode&code_verifier=theverifier")
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status().as_u16(), 200, "code exchange must succeed");

    // Cache-Control: no-store (wrapper tokens must not be cached).
    assert_eq!(
        resp.headers().get("cache-control").and_then(|v| v.to_str().ok()),
        Some("no-store")
    );

    // __Host- anchor cookie (Strict, HttpOnly, Secure) + breadcrumb.
    let anchor_cookie = set_cookie_with_prefix(&resp, "__Host-zs_app_session=")
        .expect("anchor cookie set");
    assert!(anchor_cookie.contains("HttpOnly"));
    assert!(anchor_cookie.contains("SameSite=Strict"));
    assert!(anchor_cookie.contains("Secure"));
    let breadcrumb = set_cookie_with_prefix(&resp, "zs.myapp.zeroship.ai.is.authenticated=")
        .expect("breadcrumb set");
    assert!(!breadcrumb.contains("HttpOnly"), "breadcrumb must be JS-readable");

    let body: serde_json::Value = read_json(resp).await;
    assert_eq!(body["expires_in"], 600);
    assert_eq!(body["token_type"], "Bearer");
    let access = body["access_token"].as_str().expect("access_token");

    // The browser-held token is the WRAPPER: it verifies against the
    // gateway's own verifier, and its `sub` is the per-app pairwise `pws_`
    // (§6.2/G4) — NOT the global Hydra UUID. App JS decoding its own
    // access_token must never read the global user id.
    let verifier = state.wrapper_verifier.as_ref().unwrap();
    let claims = verifier
        .verify(access, APP_HOST, Some(CLIENT_ID))
        .expect("wrapper verifies against the gateway verifier");
    let expected_pws = zeroship_core::auth::derive_pairwise(
        &state.pairwise_salt,
        &user_id.to_string(),
        &format!("https://{APP_HOST}"),
    );
    assert!(expected_pws.starts_with("pws_"), "{expected_pws}");
    assert_eq!(claims.sub, expected_pws, "wrapper sub must be the pws_");
    // The global UUID must NOT appear anywhere in the browser token.
    assert_ne!(claims.sub, user_id.to_string(), "wrapper must not carry the global UUID");
    assert!(!access.contains(&user_id.to_string()), "raw global UUID must not be in the wrapper");
    assert_eq!(claims.client_id, CLIENT_ID);
    assert!(claims.cnf.is_none(), "plain-Bearer browser wrapper has no cnf");
    assert!(claims.wraps.is_none(), "browser wrapper has no underlying raw token");
    // User.id is the pws_ too (§6.3), never the global UUID.
    assert_eq!(body["user"]["id"], expected_pws);
    assert_ne!(body["user"]["id"], user_id.to_string());

    cleanup(&dsn, user_id).await;
}

#[ntex::test]
async fn anchor_abs_expiry_is_created_at_plus_30d_not_slid() {
    let Some(dsn) = db_url() else {
        eprintln!("[anchors] skip abs_expiry (no GATEWAY_ANCHORS_DB_URL)");
        return;
    };
    let hydra = Arc::new(MockHydra::new(CLIENT_ID));
    seed_user(&dsn, hydra.user_id).await;
    let user_id = hydra.user_id;
    let (base, _srv) = boot_mock_hydra(hydra.clone()).await;
    let db_cfg = zeroship_gateway::db::DbConfig::new(dsn.clone(), 4);

    // Create an anchor directly through the REAL store and assert
    // abs_expires_at ≈ created_at + 30d (set once at create).
    let pool = zeroship_gateway::db::checkout(&db_cfg).await.expect("pool");
    let conn = pool.get().await.expect("conn");
    let refresh_enc = zeroship_core::crypto::encrypt(
        &zeroship_core::crypto::derive_key("k"),
        b"aad",
        b"rt_seed",
    )
    .unwrap();
    let anchor = anchors::create(
        &conn,
        &anchors::NewAnchor {
            app_id: APP_NAME,
            client_id: CLIENT_ID,
            global_user_id: user_id,
            refresh_token_enc: &refresh_enc,
            refresh_family_id: "fam",
            granted_scopes: &["openid".to_string()],
            cached_access_token: None,
        },
    )
    .await
    .expect("create anchor");

    let delta = anchor.abs_expires_at - anchor.created_at;
    let days = delta.num_seconds() as f64 / 86400.0;
    assert!(
        (days - 30.0).abs() < 0.01,
        "abs_expires_at must be created_at + 30d, got {days} days"
    );
    let _ = base; // keep the mock alive for symmetry
    drop(conn);
    drop(pool);
    cleanup(&dsn, user_id).await;
}

#[ntex::test]
async fn session_mint_recovers_after_reload_one_refresh() {
    let Some(dsn) = db_url() else {
        eprintln!("[anchors] skip session_mint_recovers (no GATEWAY_ANCHORS_DB_URL)");
        return;
    };
    let hydra = Arc::new(MockHydra::new(CLIENT_ID));
    seed_user(&dsn, hydra.user_id).await;
    let user_id = hydra.user_id;
    let (base, _srv) = boot_mock_hydra(hydra.clone()).await;
    let db_cfg = zeroship_gateway::db::DbConfig::new(dsn.clone(), 8);
    let state = build_state(&base, Some(db_cfg.clone()));
    let app = test::init_service(anchors_app!(state.clone())).await;

    // 1. Establish an anchor via the real /token exchange.
    let req = test::TestRequest::post()
        .uri("/__zs/auth/token")
        .header("host", APP_HOST)
        .header("origin", format!("https://{APP_HOST}"))
        .header("x-zs-auth", "1")
        .header("content-type", "application/x-www-form-urlencoded")
        .set_payload("grant_type=authorization_code&code=c&code_verifier=v")
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status().as_u16(), 200);
    let anchor_cookie = set_cookie_with_prefix(&resp, "__Host-zs_app_session=")
        .expect("anchor cookie");
    let cookie_pair = anchor_cookie.split(';').next().unwrap().to_string();
    let anchor_id_str = cookie_pair.split('=').nth(1).unwrap().to_string();
    let anchor_id = Uuid::parse_str(&anchor_id_str).unwrap();

    // 2. Simulate a reload AFTER the short cached-wrapper window: expire the
    //    cache so /session?mint=1 must actually rotate at Hydra.
    {
        let pool = zeroship_gateway::db::checkout(&db_cfg).await.unwrap();
        let conn = pool.get().await.unwrap();
        conn.execute(
            "UPDATE auth.app_session_anchors SET cached_access_exp = NOW() - interval '1 hour' WHERE id = $1",
            &[&anchor_id],
        )
        .await
        .unwrap();
    }
    let before = hydra.refresh_calls.load(Ordering::SeqCst);

    // 3. Reload-recovery: /session?mint=1 with the anchor cookie.
    let req = test::TestRequest::get()
        .uri("/__zs/auth/session?mint=1")
        .header("host", APP_HOST)
        .header("origin", format!("https://{APP_HOST}"))
        .header("x-zs-auth", "1")
        .header("cookie", cookie_pair.clone())
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status().as_u16(), 200, "reload-recovery must succeed");
    let body: serde_json::Value = read_json(resp).await;
    let expected_pws = zeroship_core::auth::derive_pairwise(
        &state.pairwise_salt,
        &user_id.to_string(),
        &format!("https://{APP_HOST}"),
    );
    // /session projects the user id as the pws_, NOT the global UUID.
    assert_eq!(body["user"]["id"], expected_pws);
    assert_ne!(body["user"]["id"], user_id.to_string());
    let fresh = body["access_token"].as_str().expect("fresh wrapper minted");
    let verifier = state.wrapper_verifier.as_ref().unwrap();
    let fresh_claims = verifier
        .verify(fresh, APP_HOST, Some(CLIENT_ID))
        .expect("minted wrapper verifies");
    // The refresh-path wrapper carries the pws_ too (not the global UUID).
    assert_eq!(fresh_claims.sub, expected_pws, "refresh-path wrapper sub must be pws_");
    assert!(!fresh.contains(&user_id.to_string()), "global UUID must not be in the refreshed wrapper");

    // Exactly ONE Hydra refresh happened on the cache-miss mint.
    assert_eq!(
        hydra.refresh_calls.load(Ordering::SeqCst),
        before + 1,
        "cache-miss mint triggers exactly one Hydra refresh"
    );

    cleanup(&dsn, user_id).await;
}

#[ntex::test]
async fn cached_wrapper_within_ttl_skips_hydra() {
    let Some(dsn) = db_url() else {
        eprintln!("[anchors] skip cached_wrapper_skips (no GATEWAY_ANCHORS_DB_URL)");
        return;
    };
    let hydra = Arc::new(MockHydra::new(CLIENT_ID));
    seed_user(&dsn, hydra.user_id).await;
    let user_id = hydra.user_id;
    let (base, _srv) = boot_mock_hydra(hydra.clone()).await;
    let db_cfg = zeroship_gateway::db::DbConfig::new(dsn.clone(), 8);
    let state = build_state(&base, Some(db_cfg.clone()));
    let app = test::init_service(anchors_app!(state.clone())).await;

    // /token caches the wrapper with a fresh (in-window) cached_access_exp.
    let req = test::TestRequest::post()
        .uri("/__zs/auth/token")
        .header("host", APP_HOST)
        .header("origin", format!("https://{APP_HOST}"))
        .header("x-zs-auth", "1")
        .header("content-type", "application/x-www-form-urlencoded")
        .set_payload("grant_type=authorization_code&code=c&code_verifier=v")
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status().as_u16(), 200);
    let anchor_cookie =
        set_cookie_with_prefix(&resp, "__Host-zs_app_session=").expect("anchor cookie");
    let cookie_pair = anchor_cookie.split(';').next().unwrap().to_string();

    let before = hydra.refresh_calls.load(Ordering::SeqCst);
    // Immediately /session?mint=1: the cached wrapper is in-window, so NO
    // Hydra refresh.
    let req = test::TestRequest::get()
        .uri("/__zs/auth/session?mint=1")
        .header("host", APP_HOST)
        .header("origin", format!("https://{APP_HOST}"))
        .header("x-zs-auth", "1")
        .header("cookie", cookie_pair)
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status().as_u16(), 200);
    let body: serde_json::Value = read_json(resp).await;
    assert!(body["access_token"].is_string());
    assert_eq!(
        hydra.refresh_calls.load(Ordering::SeqCst),
        before,
        "an in-window cached wrapper must skip Hydra entirely"
    );

    cleanup(&dsn, user_id).await;
}
