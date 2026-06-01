//! Faithful integration tests for the auth-sdk Slice 1b-browser HTTP
//! surface (`GET /__zeroship/auth/authorize`, `GET /__zeroship/auth/popup-callback`,
//! `POST /__zeroship/auth/signout`).
//!
//! These drive the REAL ntex handlers through `ntex::web::test`. The
//! unconditional tests (authorize URL shape, 503 when un-provisioned,
//! popup-callback page + CSP + no-reflection) need NO database. The signout revocation
//! test stands up a loopback MOCK Hydra (counting `/oauth2/revoke` hits)
//! and is DB-gated on `GATEWAY_ANCHORS_DB_URL` (the established skip
//! convention — no live PG in CI by default), but the same-origin guard +
//! cookie-clear parts run unconditionally.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use ed25519_dalek::SigningKey;
use ntex::web::{self, test};
use uuid::Uuid;

use zeroship_gateway::{
    anchors,
    blob_cache::{BlobCache, DiskBlobCache},
    browser_auth, enforce, idempotency,
    oidc_rp::OidcRp,
    proxy::HashRing,
    session_token,
    sync::RouteCache,
    GateConfig, GateState,
};

const APP_HOST: &str = "myapp.zeroship.ai";
const APP_NAME: &str = "myapp";
// The app's stable UUID — the CANONICAL key for gateway_sessions/anchors (NOT
// the subdomain slug). Anchors/sessions are keyed by this, matching /token,
// /session, /signout, and the live dispatch arm (RouteCtx.app_id).
const APP_UUID: &str = "0192b3c4-d5e6-7f80-9a1b-2c3d4e5f6071";
const CLIENT_ID: &str = "oac_myapp";
const GATEWAY_ISS: &str = "https://api.zeroship.ai";
const HYDRA_ISS: &str = "https://auth.zeroship.ai/";

// ─── BlobStore stub ──────────────────────────────────────────────────────

#[derive(Debug, Default)]
struct StubBlobStore;

#[async_trait::async_trait(?Send)]
impl zeroship_bundle::BlobStore for StubBlobStore {
    async fn get_blob(&self, _h: &str) -> Result<bytes::Bytes, zeroship_bundle::BlobError> {
        Err(zeroship_bundle::BlobError::NotFound("unused".into()))
    }
    fn local_path(&self, _h: &str) -> Option<std::path::PathBuf> {
        None
    }
    async fn put_blob(
        &self,
        _h: &str,
        _d: &[u8],
    ) -> Result<zeroship_bundle::PutOutcome, zeroship_bundle::BlobError> {
        Ok(zeroship_bundle::PutOutcome::Wrote)
    }
    async fn put_blob_stream(
        &self,
        _h: &str,
        _s: u64,
        _r: &mut dyn std::io::Read,
    ) -> Result<zeroship_bundle::PutOutcome, zeroship_bundle::BlobError> {
        Ok(zeroship_bundle::PutOutcome::Wrote)
    }
    async fn has_blob(&self, _h: &str) -> Result<bool, zeroship_bundle::BlobError> {
        Ok(false)
    }
    async fn put_manifest(
        &self,
        _a: &Uuid,
        _d: &str,
        _j: &[u8],
    ) -> Result<(), zeroship_bundle::BlobError> {
        Ok(())
    }
    async fn get_manifest(
        &self,
        _a: &Uuid,
        _d: &str,
    ) -> Result<bytes::Bytes, zeroship_bundle::BlobError> {
        Err(zeroship_bundle::BlobError::NotFound("unused".into()))
    }
}

// ─── State builders ──────────────────────────────────────────────────────

struct StateOpts {
    /// Provision the route with `Some(client_id)` + sector, or leave it
    /// un-provisioned (`None`/`None`) to exercise the 503 path.
    provisioned: bool,
    /// Hydra dial URL (loopback mock) for `OidcRp`.
    hydra_base: String,
    db: Option<zeroship_gateway::db::DbConfig>,
    /// Optional previous signing key (session-cookie rotation overlap).
    prev_signing: Option<SigningKey>,
}

impl Default for StateOpts {
    fn default() -> Self {
        Self {
            provisioned: true,
            hydra_base: "http://127.0.0.1:1".into(),
            db: None,
            prev_signing: None,
        }
    }
}

/// The fixed gateway signing key every fixture uses (so session cookies
/// minted in a test verify against the fixture's own verifier).
fn gateway_signing() -> SigningKey {
    SigningKey::from_bytes(&[7u8; 32])
}

fn build_state(opts: StateOpts) -> Arc<GateState> {
    let mut tmp = std::env::temp_dir();
    tmp.push(format!("zsgate-browser-{}", Uuid::new_v4().simple()));
    let disk = DiskBlobCache::new(tmp, 1024 * 1024).expect("disk cache");

    let signing_key = gateway_signing();
    let session_issuer =
        session_token::Issuer::new(&signing_key, GATEWAY_ISS.into()).expect("session issuer");
    let session_verifier = match opts.prev_signing.as_ref() {
        Some(prev) => session_token::Verifier::with_previous(
            &signing_key.verifying_key(),
            &prev.verifying_key(),
            GATEWAY_ISS.into(),
        ),
        None => session_token::Verifier::new(&signing_key.verifying_key(), GATEWAY_ISS.into()),
    };

    // OidcRp dials the loopback mock for /oauth2/{auth,token,revoke}.
    let oidc_rp = OidcRp::new(&opts.hydra_base, "gateway", "test-secret", b"k".repeat(32))
        .with_issuer(HYDRA_ISS);

    let routes = RouteCache::new();
    routes.update(build_route_map(opts.provisioned));

    Arc::new(GateState {
        config: GateConfig {
            control_url: String::new(),
            control_key: String::new(),
            worker_urls: vec![],
            poll_interval_secs: 5,
            worker_key: "worker-key".into(),
            hydra_public_url: opts.hydra_base.clone(),
            auth_ui_url: opts.hydra_base.clone(),
            // insecure_dev=false → prod __Host- / Strict / Secure cookies +
            // https Origin compare are exercised.
            insecure_dev: false,
            trust_proxy: false,
            public_url: GATEWAY_ISS.into(),
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
        db: opts.db,
        dpop_jti_cache: Arc::new(zeroship_core::dpop::TieredJtiCache::default()),
        logout_jti_cache: Arc::new(zeroship_core::logout_token::LogoutJtiCache::default()),
        revocation_cache: Arc::new(zeroship_core::wrapper_revocation::RevocationCache::new()),
        signing_key: Some(Arc::new(signing_key)),
        prev_signing_key: opts.prev_signing.map(Arc::new),
        session_issuer: Some(Arc::new(session_issuer)),
        session_verifier: Some(Arc::new(session_verifier)),
        anchor_enc_key: zeroship_core::crypto::derive_key("anchor-test-key"),
        pairwise_salt: zeroship_core::crypto::derive_key("pairwise-test-salt"),
        trusted_oauth_clients: zeroship_core::auth::default_trusted_oauth_clients(),
    })
}

fn build_route_map(provisioned: bool) -> zeroship_core::types::RouteMap {
    use zeroship_core::types::RouteEntry;
    let mut m = std::collections::HashMap::new();
    m.insert(
        Uuid::parse_str(APP_UUID).expect("valid APP_UUID"),
        RouteEntry {
            name: APP_NAME.into(),
            plan_id: "free".into(),
            api_key_hash: String::new(),
            deploy_hash: None,
            manifest: zeroship_bundle::Manifest::passthrough(),
            oauth_client_id: provisioned.then(|| CLIENT_ID.to_string()),
            sector_identifier: provisioned.then(|| format!("https://{APP_HOST}")),
        },
    );
    m
}

macro_rules! browser_app {
    ($state:expr) => {{
        web::App::new()
            .state($state.clone())
            .service(
                web::resource("/__zeroship/auth/authorize")
                    .route(web::get().to(browser_auth::authorize)),
            )
            .service(
                web::resource("/__zeroship/auth/popup-callback")
                    .route(web::get().to(browser_auth::popup_callback)),
            )
            .service(
                web::resource("/__zeroship/auth/signout")
                    .route(web::post().to(browser_auth::signout)),
            )
    }};
}

fn header_str(resp: &ntex::web::WebResponse, name: &str) -> Option<String> {
    resp.headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
}

fn set_cookie_with_prefix(resp: &ntex::web::WebResponse, prefix: &str) -> Option<String> {
    for hv in resp.headers().get_all(http::header::SET_COOKIE) {
        let Ok(s) = hv.to_str() else { continue };
        if s.starts_with(prefix) {
            return Some(s.to_string());
        }
    }
    None
}

async fn read_text(resp: ntex::web::WebResponse) -> String {
    let bytes = test::read_body(resp).await;
    String::from_utf8(bytes.to_vec()).expect("utf8 body")
}

// ─── GET /__zeroship/auth/authorize ────────────────────────────────────────────

#[ntex::test]
async fn authorize_redirects_to_hydra_with_browser_pkce() {
    let state = build_state(StateOpts::default());
    let app = test::init_service(browser_app!(state)).await;

    let req = test::TestRequest::get()
        .uri("/__zeroship/auth/authorize?code_challenge=CH_browser&code_challenge_method=S256&state=ST_x&nonce=NO_y&scope=openid+profile+read%3Abilling")
        .header(http::header::HOST, APP_HOST)
        .to_request();
    let resp = test::call_service(&app, req).await;

    assert_eq!(resp.status().as_u16(), 302, "authorize must 302 to Hydra");
    let loc = header_str(&resp, "location").expect("Location header");
    // Cross-site hop to Hydra's /oauth2/auth.
    assert!(loc.starts_with("http://127.0.0.1:1/oauth2/auth?"), "{loc}");
    // PER-APP public client_id (never the gateway confidential client).
    assert!(loc.contains("client_id=oac_myapp"), "{loc}");
    assert!(!loc.contains("client_id=gateway"), "{loc}");
    assert!(loc.contains("response_type=code"), "{loc}");
    // The BROWSER's PKCE challenge + state + nonce ride through.
    assert!(loc.contains("code_challenge=CH_browser"), "{loc}");
    assert!(loc.contains("code_challenge_method=S256"), "{loc}");
    assert!(loc.contains("state=ST_x"), "{loc}");
    assert!(loc.contains("nonce=NO_y"), "{loc}");
    assert!(loc.contains("scope=openid+profile+read%3Abilling"), "{loc}");
    // redirect_uri defaults to THIS app's own popup-callback.
    assert!(
        loc.contains("redirect_uri=https%3A%2F%2Fmyapp.zeroship.ai%2F__zeroship%2Fauth%2Fpopup-callback"),
        "{loc}"
    );
    // No prompt in the common case (so Hydra SSO skip fires).
    assert!(!loc.contains("prompt="), "default omits prompt: {loc}");
    // no-store on the redirect.
    assert_eq!(header_str(&resp, "cache-control").as_deref(), Some("no-store"));
}

#[ntex::test]
async fn authorize_passes_prompt_through() {
    let state = build_state(StateOpts::default());
    let app = test::init_service(browser_app!(state)).await;

    let req = test::TestRequest::get()
        .uri("/__zeroship/auth/authorize?code_challenge=c&state=s&nonce=n&scope=openid&prompt=consent")
        .header(http::header::HOST, APP_HOST)
        .to_request();
    let resp = test::call_service(&app, req).await;
    let loc = header_str(&resp, "location").expect("Location");
    assert!(loc.contains("prompt=consent"), "{loc}");
}

#[ntex::test]
async fn authorize_503_when_client_not_provisioned() {
    // Un-provisioned route (oauth_client_id == None) ⇒ 503 client_not_provisioned.
    let state = build_state(StateOpts { provisioned: false, ..Default::default() });
    let app = test::init_service(browser_app!(state)).await;

    let req = test::TestRequest::get()
        .uri("/__zeroship/auth/authorize?code_challenge=c&state=s&nonce=n")
        .header(http::header::HOST, APP_HOST)
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status().as_u16(), 503, "un-provisioned app must 503");
    let body = read_text(resp).await;
    assert!(body.contains("client_not_provisioned"), "{body}");
}

#[ntex::test]
async fn authorize_400_when_missing_pkce_or_state_or_nonce() {
    let state = build_state(StateOpts::default());
    let app = test::init_service(browser_app!(state)).await;

    for (uri, why) in [
        ("/__zeroship/auth/authorize?state=s&nonce=n", "no code_challenge"),
        ("/__zeroship/auth/authorize?code_challenge=c&nonce=n", "no state"),
        ("/__zeroship/auth/authorize?code_challenge=c&state=s", "no nonce"),
        (
            "/__zeroship/auth/authorize?code_challenge=c&state=s&nonce=n&code_challenge_method=plain",
            "plain method rejected",
        ),
    ] {
        let req = test::TestRequest::get()
            .uri(uri)
            .header(http::header::HOST, APP_HOST)
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status().as_u16(), 400, "{why}: {uri}");
    }
}

#[ntex::test]
async fn authorize_rejects_foreign_redirect_uri() {
    // An open-redirect guard: a redirect_uri NOT on this app's origin is 400.
    let state = build_state(StateOpts::default());
    let app = test::init_service(browser_app!(state)).await;
    let req = test::TestRequest::get()
        .uri("/__zeroship/auth/authorize?code_challenge=c&state=s&nonce=n&redirect_uri=https%3A%2F%2Fevil.example%2Fcb")
        .header(http::header::HOST, APP_HOST)
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status().as_u16(), 400, "foreign redirect_uri must 400");
}

// ─── GET /__zeroship/auth/popup-callback ───────────────────────────────────────

#[ntex::test]
async fn popup_callback_relays_to_own_origin_and_never_reflects_query() {
    let state = build_state(StateOpts::default());
    let app = test::init_service(browser_app!(state)).await;

    // Inject an XSS payload into the query — it MUST NOT appear in the body.
    let xss = "<script>alert(1)</script>";
    let uri = format!(
        "/__zeroship/auth/popup-callback?code=abc&state=st&error_description={}",
        urlencoding(xss)
    );
    let req = test::TestRequest::get()
        .uri(&uri)
        .header(http::header::HOST, APP_HOST)
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status().as_u16(), 200);

    // Strict security headers.
    let csp = header_str(&resp, "content-security-policy").expect("CSP");
    assert!(csp.contains("default-src 'none'"), "{csp}");
    assert!(csp.contains("script-src 'nonce-"), "{csp}");
    assert!(csp.contains("frame-ancestors 'self'"), "{csp}");
    assert!(!csp.contains("unsafe-inline"), "no unsafe-inline: {csp}");
    assert_eq!(header_str(&resp, "referrer-policy").as_deref(), Some("no-referrer"));
    assert_eq!(
        header_str(&resp, "cross-origin-opener-policy").as_deref(),
        Some("same-origin")
    );

    // The CSP nonce in the header MUST equal the <script nonce> in the body.
    let nonce = csp
        .split("script-src 'nonce-")
        .nth(1)
        .and_then(|s| s.split('\'').next())
        .expect("nonce in CSP")
        .to_string();

    let body = read_text(resp).await;
    assert!(
        body.contains(&format!("<script nonce=\"{nonce}\">")),
        "script nonce must match CSP nonce: body={body}"
    );
    // postMessage targets the OWN origin, never '*'.
    assert!(
        body.contains("window.opener.postMessage(msg, location.origin)"),
        "{body}"
    );
    assert!(!body.contains(", '*')"), "never postMessage to '*': {body}");
    // The XSS payload from the query is NEVER reflected into the DOM.
    assert!(!body.contains(xss), "raw query must NOT be reflected: {body}");
    assert!(!body.contains("alert(1)"), "no reflected script: {body}");
    assert!(!body.contains("code=abc"), "no reflected code: {body}");
    // COOP fallbacks present.
    assert!(body.contains("new BroadcastChannel('zs:auth')"), "{body}");
    assert!(body.contains("@@zsauth@@::relay::"), "{body}");
    // The exact message type the SDK matches.
    assert!(body.contains("zs:authorization_response"), "{body}");
}

#[ntex::test]
async fn popup_callback_nonce_is_per_response() {
    // Two requests must get DISTINCT nonces (fresh per response).
    let state = build_state(StateOpts::default());
    let app = test::init_service(browser_app!(state)).await;

    let nonce_of = |csp: String| {
        csp.split("script-src 'nonce-")
            .nth(1)
            .and_then(|s| s.split('\'').next())
            .map(str::to_string)
    };
    let r1 = test::call_service(
        &app,
        test::TestRequest::get()
            .uri("/__zeroship/auth/popup-callback")
            .header(http::header::HOST, APP_HOST)
            .to_request(),
    )
    .await;
    let r2 = test::call_service(
        &app,
        test::TestRequest::get()
            .uri("/__zeroship/auth/popup-callback")
            .header(http::header::HOST, APP_HOST)
            .to_request(),
    )
    .await;
    let n1 = nonce_of(header_str(&r1, "content-security-policy").unwrap());
    let n2 = nonce_of(header_str(&r2, "content-security-policy").unwrap());
    assert!(n1.is_some() && n2.is_some());
    assert_ne!(n1, n2, "CSP nonce must be fresh per response");
}

// ─── POST /__zeroship/auth/signout ─────────────────────────────────────────────

#[ntex::test]
async fn signout_rejects_missing_custom_header_and_foreign_origin() {
    // The same-origin guard runs unconditionally (no DB needed): a POST
    // missing X-ZS-Auth is 400; a foreign Origin is 403.
    let state = build_state(StateOpts::default());
    let app = test::init_service(browser_app!(state)).await;

    // Missing X-ZS-Auth → 400.
    let req = test::TestRequest::post()
        .uri("/__zeroship/auth/signout")
        .header(http::header::HOST, APP_HOST)
        .header(http::header::ORIGIN, format!("https://{APP_HOST}"))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status().as_u16(), 400, "missing X-ZS-Auth must 400");

    // Foreign Origin → 403.
    let req = test::TestRequest::post()
        .uri("/__zeroship/auth/signout")
        .header(http::header::HOST, APP_HOST)
        .header("x-zs-auth", "1")
        .header(http::header::ORIGIN, "https://evil.example")
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status().as_u16(), 403, "foreign Origin must 403");
}

#[ntex::test]
async fn signout_with_no_anchor_is_204_and_clears_cookies() {
    // No DB / no anchor cookie: signout is an idempotent no-op that still
    // clears the cookies and returns 204.
    let state = build_state(StateOpts::default());
    let app = test::init_service(browser_app!(state)).await;

    let req = test::TestRequest::post()
        .uri("/__zeroship/auth/signout")
        .header(http::header::HOST, APP_HOST)
        .header("x-zs-auth", "1")
        .header(http::header::ORIGIN, format!("https://{APP_HOST}"))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status().as_u16(), 204, "signout must 204");
    // Both cookies cleared (Max-Age=0).
    let anchor_clear =
        set_cookie_with_prefix(&resp, "__Host-zeroship_app_anchor=").expect("anchor clear cookie");
    assert!(anchor_clear.contains("Max-Age=0"), "{anchor_clear}");
    let crumb_clear = set_cookie_with_prefix(&resp, &format!("zs.{APP_HOST}.is.authenticated="))
        .expect("breadcrumb clear cookie");
    assert!(crumb_clear.contains("Max-Age=0"), "{crumb_clear}");
    assert_eq!(header_str(&resp, "cache-control").as_deref(), Some("no-store"));
}

/// DB-gated: seed an anchor, sign out, and assert (a) the per-app family
/// marker is set, (b) the anchor row is deleted, (c) the cookies are
/// cleared, (d) Hydra `/oauth2/revoke` was hit exactly once.
#[ntex::test]
async fn signout_local_revokes_family_marker_deletes_anchor_and_hits_hydra_revoke() {
    let Some(dsn) = std::env::var("GATEWAY_ANCHORS_DB_URL").ok() else {
        eprintln!("skipping (no GATEWAY_ANCHORS_DB_URL)");
        return;
    };
    let db = zeroship_gateway::db::DbConfig::new(dsn.clone(), 4);

    // Loopback mock Hydra that counts /oauth2/revoke calls.
    let revoke_calls = Arc::new(AtomicU32::new(0));
    let mock = start_mock_revoke(revoke_calls.clone()).await;
    let base = mock.url("").trim_end_matches('/').to_string();

    let state = build_state(StateOpts {
        provisioned: true,
        hydra_base: base,
        db: Some(db.clone()),
        prev_signing: None,
    });
    let app = test::init_service(browser_app!(state.clone())).await;

    // Seed the GLOBAL user first — app_session_anchors.global_user_id has a
    // FK to zeroship.users(id) (ON DELETE CASCADE).
    let global_user_id = Uuid::new_v4();
    seed_user(&dsn, global_user_id).await;

    // Seed an anchor row with an encrypted refresh family (encrypted with
    // the SAME AAD the gateway uses, so signout can decrypt + Hydra-revoke).
    let refresh_plain = "rt_seeded_family_secret";
    let aad = format!("zs-anchor-refresh:{CLIENT_ID}:{global_user_id}").into_bytes();
    let refresh_enc =
        zeroship_core::crypto::encrypt(&state.anchor_enc_key, &aad, refresh_plain.as_bytes())
            .expect("encrypt");

    let anchor_id = {
        let pool = zeroship_gateway::db::checkout(&db).await.expect("pool");
        let mut conn = pool.get().await.expect("conn");
        let a = anchors::create(
            &mut conn,
            &anchors::NewAnchor {
                app_id: Uuid::parse_str(APP_UUID).expect("valid APP_UUID"),
                client_id: CLIENT_ID,
                global_user_id,
                refresh_token_enc: &refresh_enc,
                refresh_family_id: "rfam_test",
                granted_scopes: &["openid".to_string()],
            },
        )
        .await
        .expect("anchor create");
        a.id
    };

    // The pws_ subject the family marker should be keyed on.
    let pws_sub = zeroship_core::auth::derive_pairwise(
        &state.pairwise_salt,
        &global_user_id.to_string(),
        &format!("https://{APP_HOST}"),
    );

    // Sign out (local) carrying the anchor cookie + same-origin guard.
    let req = test::TestRequest::post()
        .uri("/__zeroship/auth/signout")
        .header(http::header::HOST, APP_HOST)
        .header("x-zs-auth", "1")
        .header(http::header::ORIGIN, format!("https://{APP_HOST}"))
        .header(http::header::CONTENT_TYPE, "application/json")
        .header(
            http::header::COOKIE,
            format!("__Host-zeroship_app_anchor={anchor_id}"),
        )
        .set_payload(r#"{"scope":"local"}"#)
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status().as_u16(), 204, "signout must 204");

    // (d) cookies cleared.
    assert!(set_cookie_with_prefix(&resp, "__Host-zeroship_app_anchor=")
        .is_some_and(|c| c.contains("Max-Age=0")));
    assert!(
        set_cookie_with_prefix(&resp, &format!("zs.{APP_HOST}.is.authenticated="))
            .is_some_and(|c| c.contains("Max-Age=0"))
    );

    // (b) anchor row deleted (read_live now returns None).
    {
        let pool = zeroship_gateway::db::checkout(&db).await.expect("pool");
        let mut conn = pool.get().await.expect("conn");
        let still =
            anchors::read_live(&mut conn, Uuid::parse_str(APP_UUID).expect("valid APP_UUID"), anchor_id)
                .await
                .expect("read");
        assert!(still.is_none(), "anchor row must be deleted after signout");

        // (a) family marker set for (client_id, pws_sub) — assert a token
        // with iat BEFORE now is revoked.
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let revoked = zeroship_core::wrapper_revocation::is_family_revoked_since(
            &conn,
            CLIENT_ID,
            &pws_sub,
            now - 60,
        )
        .await
        .expect("family check");
        assert!(revoked, "family marker (client_id, pws_) must be set by signout");

        // cleanup the marker.
        conn.execute(
            "DELETE FROM zeroship.token_revocations WHERE client_id = $1 AND sub = $2",
            &[&CLIENT_ID, &pws_sub],
        )
        .await
        .ok();
    }

    // (c) Hydra /oauth2/revoke hit exactly once (best-effort revoke fired).
    assert_eq!(
        revoke_calls.load(Ordering::SeqCst),
        1,
        "signout must best-effort revoke the family at Hydra exactly once"
    );
    drop(mock);
    cleanup_user(&dsn, global_user_id).await;
}

// ─── helpers ─────────────────────────────────────────────────────────────

/// Seed the GLOBAL user row the anchor FK requires (`zeroship.users(id)`).
async fn seed_user(dsn: &str, user_id: Uuid) {
    let (client, conn) = compio_postgres::connect(dsn, compio_postgres::NoTls)
        .await
        .expect("connect");
    compio::runtime::spawn(async move {
        let _ = conn.run().await;
    })
    .detach();
    let email = format!("signout-{}@zeroship.test", user_id.simple());
    client
        .execute(
            "INSERT INTO zeroship.users (id, email, name, email_verified_at) \
             VALUES ($1, $2::citext, $3, NOW()) ON CONFLICT (id) DO NOTHING",
            &[&user_id, &email, &"Signout Test"],
        )
        .await
        .expect("seed user");
}

/// Cascade-delete the seeded user (anchors cascade via the FK).
async fn cleanup_user(dsn: &str, user_id: Uuid) {
    let (client, conn) = compio_postgres::connect(dsn, compio_postgres::NoTls)
        .await
        .expect("connect");
    compio::runtime::spawn(async move {
        let _ = conn.run().await;
    })
    .detach();
    let _ = client
        .execute(
            "DELETE FROM zeroship.app_session_anchors WHERE global_user_id = $1",
            &[&user_id],
        )
        .await;
    let _ = client
        .execute("DELETE FROM zeroship.users WHERE id = $1", &[&user_id])
        .await;
}

/// Minimal URL-encode for the test query payload (avoids a dep).
fn urlencoding(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Start a loopback mock Hydra that answers `POST /oauth2/revoke` with 200
/// and increments `counter`. Faithful to RFC 7009 (always 200).
async fn start_mock_revoke(counter: Arc<AtomicU32>) -> test::TestServer {
    test::server(move || {
        let counter = counter.clone();
        async move {
            web::App::new().state(counter).service(
                web::resource("/oauth2/revoke").route(web::post().to(
                    |c: web::types::State<Arc<AtomicU32>>| async move {
                        c.fetch_add(1, Ordering::SeqCst);
                        web::HttpResponse::Ok().finish()
                    },
                )),
            )
        }
    })
    .await
}
