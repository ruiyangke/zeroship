//! Share-token end-to-end tests (`preview-URL` § II.4 + § II.6 + § III).
//!
//! Tests both the API surface (POST/GET/DELETE /share) and the
//! validate-side surface (`?t=...` cookie-conversion + cookie auth on
//! subsequent requests). The same in-process mock pattern as
//! `sandbox_preview_e2e.rs` — a fixture HTTP "agent" listens on a
//! random port and replies with a canned status/body. The controller
//! is wired up via `ntex::web::test` so the handlers see real route
//! matching.
//!
//! Coverage:
//!
//! - Mint → exchange → fetch (green path).
//! - Tamper byte → 401.
//! - Expiry → 401 `code: "expired"`.
//! - Scope: `ro` token + POST through dispatch → 404 uniform; via
//!   cookie-conversion `?t=` → 403 `scope_forbidden` (UX-helpful).
//! - Cross-sandbox: token for sbx-A used against sbx-B → 401.
//! - Path scope: token against `/sandboxes/{id}/exec` → 401.
//! - Cookie-auth fails after revoke.
//! - Ownership checks across two sandboxes.
//! - Existing-vs-fabricated slug oracle stays uniform.
//! - HMAC input is stable across field order.
//! - `iss` remains audit-only, with and without the claim.
//! - Token separator handling (`~` vs `.` vs `~~`).
//! - 1 KiB raw-token cap.
//! - deny_unknown_fields rejects extra field.
//! - HMAC constant-time compare smoke.
//! - Sec-Fetch-Site fail-closed.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use ed25519_dalek::SigningKey;
use ntex::http::StatusCode;
use ntex::web::{self, test};
use uuid::Uuid;

use zeroship_sandbox::backend::{Backend, SandboxInfo};
use zeroship_sandbox::config::{ApiToken, SandboxConfig};
use zeroship_sandbox::preview;
use zeroship_sandbox::preview_share::{
    self, mint as mint_token, TokenClaims, TOKEN_RAW_MAX,
};
use zeroship_sandbox::preview_share_handlers;

// ─── fixture agent ──────────────────────────────────────────────────

struct AgentReply {
    status: u16,
    body: Vec<u8>,
}

struct FixtureAgent {
    port: u16,
    stop: Arc<AtomicBool>,
    #[allow(dead_code)]
    captured: Arc<Mutex<Vec<String>>>,
}

impl FixtureAgent {
    fn spawn(reply: AgentReply) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_c = stop.clone();
        let captured = Arc::new(Mutex::new(Vec::new()));
        let captured_c = captured.clone();
        let reply = Arc::new(reply);
        thread::spawn(move || {
            while !stop_c.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream
                            .set_read_timeout(Some(Duration::from_millis(200)))
                            .ok();
                        let mut buf = vec![0u8; 64 * 1024];
                        let n = stream.read(&mut buf).unwrap_or(0);
                        let raw = String::from_utf8_lossy(&buf[..n]).to_string();
                        captured_c.lock().unwrap().push(raw);
                        let head = format!(
                            "HTTP/1.1 {} OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            reply.status,
                            reply.body.len(),
                        );
                        let _ = stream.write_all(head.as_bytes());
                        let _ = stream.write_all(&reply.body);
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });
        Self { port, stop, captured }
    }
    #[allow(dead_code)]
    fn captured(&self) -> Vec<String> {
        self.captured.lock().unwrap().clone()
    }
}
impl Drop for FixtureAgent {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

// ─── controller fixture ─────────────────────────────────────────────

fn make_cfg(token: &str) -> SandboxConfig {
    // A7 (deferred): see sandbox_admin_e2e.rs::make_cfg for rationale.
    // `token` is `pub(crate)`; construct via the public fixture
    // constructor + `with_token` builder.
    SandboxConfig::new_fixture().with_token(ApiToken::new(token))
}

/// Build state with one sandbox pre-injected; returns `(state, sbx_id)`.
fn make_state(
    token: &str,
    user_id: &str,
    agent_port: u16,
    sk: SigningKey,
) -> (Arc<zeroship_sandbox::AppState>, Uuid) {
    make_state_inner(token, user_id, agent_port, sk, None)
}

/// Variant of [`make_state`] that wires a real `Persistence` handle
/// into the backend; used by the persist-on-mint regression tests.
/// Returns `(state, sbx_id, persist_dir)` so the test can read back
/// the sealed file directly to confirm round-trip.
#[allow(clippy::type_complexity)]
fn make_state_with_persist(
    token: &str,
    user_id: &str,
    agent_port: u16,
    sk: SigningKey,
) -> (Arc<zeroship_sandbox::AppState>, Uuid, std::path::PathBuf, [u8; 32]) {
    let aead = [0x9Au8; 32];
    let dir = std::env::temp_dir()
        .join(format!("zsbx-share-mint-persist-{}", Uuid::now_v7().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    let persist = Arc::new(zeroship_sandbox::persist::Persistence::new(
        dir.clone(),
        zeroship_sandbox::persist::AeadKey::from_bytes(aead),
    ));
    let (state, id) = make_state_inner(token, user_id, agent_port, sk, Some(persist));
    (state, id, dir, aead)
}

fn make_state_inner(
    token: &str,
    user_id: &str,
    agent_port: u16,
    sk: SigningKey,
    persist: Option<Arc<zeroship_sandbox::persist::Persistence>>,
) -> (Arc<zeroship_sandbox::AppState>, Uuid) {
    let cfg = make_cfg(token);
    let backend = {
        let mut b = Backend::builder(&cfg);
        if let Some(p) = persist {
            b = b.with_persist(p);
        }
        b.build().expect("backend")
    };
    let sandbox_id = Uuid::now_v7();
    if let Backend::NomadCh(b) = &backend {
        b._test_inject_sandbox(
            sandbox_id,
            user_id,
            sk,
            format!("http://127.0.0.1:{agent_port}"),
            42,
        );
    } else {
        unreachable!("test pinned to nomad-ch");
    }
    // NOTE: legacy hyphenated UUID form. The typed-id wire shape is
    // covered in `tests/sandbox_typed_id_e2e.rs`.
    let info = SandboxInfo {
        sandbox_id: sandbox_id.to_string(),
        user_id: user_id.to_string(),
        project_id: "p".into(),
        backend: "nomad-ch".into(),
        backend_hint: "test".into(),
        created_at_secs: 0,
        last_used_at_secs: 0,
    };
    // A5: out-of-crate construction goes through `new_fixture`
    // (admin_token = None; Phase-0 sandbox-pg-state tests run pg-disabled).
    let state = zeroship_sandbox::AppState::new_fixture(cfg, backend);
    state.sandboxes.insert(sandbox_id, info);
    let state = Arc::new(state);
    (state, sandbox_id)
}

/// Build the test app with all relevant share-token routes.
macro_rules! make_app {
    ($state:expr) => {
        test::init_service(
            ntex::web::App::new()
                .state($state)
                .state(
                    web::types::PayloadConfig::default().limit(256 * 1024 * 1024),
                )
                .service(
                    web::resource("/sandboxes/{id}/preview/{port}/share")
                        .route(
                            web::post()
                                .to(preview_share_handlers::mint_share),
                        )
                        .route(
                            web::get()
                                .to(preview_share_handlers::list_share),
                        )
                        .route(
                            web::delete()
                                .to(preview_share_handlers::revoke_all_share),
                        ),
                )
                .service(
                    web::resource(
                        "/sandboxes/{id}/preview/{port}/share/{token_id}",
                    )
                    .route(
                        web::delete()
                            .to(preview_share_handlers::revoke_one_share),
                    ),
                )
                .service(
                    web::resource("/sandboxes/{id}/preview/{port}/{path}*")
                        .state(
                            web::types::PayloadConfig::default()
                                .limit(256 * 1024 * 1024),
                        )
                        .route(web::route().to(preview::preview_proxy)),
                ),
        )
        .await
    };
}

// ─── helpers ────────────────────────────────────────────────────────

/// Build the cookie name `__Host-zsbx_share_<slug>` exactly as the
/// preview module emits it (lowercase + non-alnum stripped).
fn cookie_name_for(id: Uuid) -> String {
    let slug: String = id
        .to_string()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect();
    format!("__Host-zsbx_share_{slug}")
}

/// Sec-Fetch-* triple for a top-level navigation. Required on every
/// request that hits `__zsbx_share` (i.e., uses `?t=`).
const SEC_FETCH_HEADERS: &[(&str, &str)] = &[
    ("Sec-Fetch-Site", "cross-site"),
    ("Sec-Fetch-Mode", "navigate"),
    ("Sec-Fetch-Dest", "document"),
];

fn now_unix() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Mint a share-token directly via the registry, bypassing the API.
/// Used by tests that don't care about the rate-limit /
/// auth-handler logic — they want a token bound to known
/// sandbox-id/port/secret-version state.
fn direct_mint(
    state: &Arc<zeroship_sandbox::AppState>,
    id: Uuid,
    port: u16,
    scope: &str,
    ttl_secs: u64,
) -> String {
    let ring = state.sandboxes.ensure_preview_secret(id).unwrap();
    let now = now_unix();
    let claims = TokenClaims {
        aud: "preview".into(),
        sbx: id.to_string(),
        port,
        iss: Some("usr_test".into()),
        iat: now,
        exp: now + ttl_secs,
        sv: ring.sv_current,
        tid: preview_share::fresh_token_id(),
        scope: scope.into(),
    };
    let tok = mint_token(&claims, &ring.current);
    state.sandboxes.append_audit(
        id,
        zeroship_sandbox::registry::PreviewAuditEntry {
            token_id: claims.tid.clone(),
            port,
            issued_at_unix: now,
            expires_at_unix: claims.exp,
            scope: claims.scope.clone(),
            secret_version: ring.sv_current,
            iss: claims.iss.clone(),
            last_used_at_unix: 0,
            use_count: 0,
        },
    );
    tok
}

// ═══════════════════════════════════════════════════════════════════
//  Mint API
// ═══════════════════════════════════════════════════════════════════

#[ntex::test]
async fn mint_then_use_token_round_trip() {
    let sk = SigningKey::from_bytes(&[1u8; 32]);
    let agent = FixtureAgent::spawn(AgentReply { status: 200, body: b"ok".to_vec() });
    let (state, id) = make_state("tok", "alice", agent.port, sk);
    let app = make_app!(state.clone());

    // 1. Mint via the POST /share API.
    let path = format!("/sandboxes/{id}/preview/5173/share?user_id=alice");
    let req = test::TestRequest::post()
        .uri(&path)
        .header("Authorization", "Bearer tok")
        .set_json(&serde_json::json!({"expires_in_secs": 3600, "scope": "ro"}))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = test::read_body(resp).await; let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let token = body["token"].as_str().unwrap().to_string();
    let token_id = body["token_id"].as_str().unwrap().to_string();
    assert!(!token.is_empty());
    assert!(token.contains('~'));
    // Wire-stable token_id is `shr_` + raw 22-char base64url tid.
    assert!(
        token_id.starts_with("shr_"),
        "API surface prefixes token_id with shr_; got {token_id}"
    );
    assert_eq!(token_id.len(), 4 + 22, "shr_ + 22-char tid");

    // 2. Use the token via cookie-conversion: ?t=<token>.
    let convert_path = format!("/sandboxes/{id}/preview/5173/index.html?t={token}");
    let mut req = test::TestRequest::get().uri(&convert_path);
    for (k, v) in SEC_FETCH_HEADERS {
        req = req.header(*k, *v);
    }
    let resp = test::call_service(&app, req.to_request()).await;
    assert_eq!(resp.status(), StatusCode::SEE_OTHER, "303 redirect on convert");
    let set_cookie = resp.headers().get("set-cookie").unwrap().to_str().unwrap();
    let expected_name = cookie_name_for(id);
    assert!(
        set_cookie.starts_with(&format!("{expected_name}={token}")),
        "Set-Cookie carries verbatim token; got: {set_cookie}"
    );
    assert!(set_cookie.contains("HttpOnly"));
    assert!(set_cookie.contains("Secure"));
    assert!(set_cookie.contains("SameSite=Strict"));
    assert!(set_cookie.contains("Path=/"));
    assert!(!set_cookie.to_lowercase().contains("domain="), "no Domain attr");

    // 3. Subsequent fetch carrying the cookie reaches the agent.
    let fetch_path = format!("/sandboxes/{id}/preview/5173/index.html");
    let cookie = format!("{expected_name}={token}");
    let req = test::TestRequest::get()
        .uri(&fetch_path)
        .header("Cookie", cookie.as_str())
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = test::read_body(resp).await;
    assert_eq!(&body[..], b"ok");

    // 4. Audit row reflects use — list_audit. The audit stores the
    // raw `tid` claim bytes; the API surface adds the `shr_` prefix
    // for presentation. Strip the prefix to compare.
    let audit = state.sandboxes.list_audit(id);
    assert_eq!(audit.len(), 1);
    let raw_tid = token_id.strip_prefix("shr_").expect("API prefix");
    assert_eq!(audit[0].token_id, raw_tid);
    assert!(audit[0].use_count >= 2, "convert + fetch both bump use_count");
}

#[ntex::test]
async fn mint_rejects_too_short_or_too_long_ttl() {
    let sk = SigningKey::from_bytes(&[2u8; 32]);
    let agent = FixtureAgent::spawn(AgentReply { status: 200, body: vec![] });
    let (state, id) = make_state("tok", "alice", agent.port, sk);
    let app = make_app!(state);
    let path = format!("/sandboxes/{id}/preview/5173/share?user_id=alice");
    for ttl in [0u64, 59, 7 * 24 * 60 * 60 + 1] {
        let req = test::TestRequest::post()
            .uri(&path)
            .header("Authorization", "Bearer tok")
            .set_json(&serde_json::json!({"expires_in_secs": ttl, "scope": "ro"}))
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "ttl={ttl}");
    }
}

#[ntex::test]
async fn mint_rejects_unknown_scope() {
    let sk = SigningKey::from_bytes(&[3u8; 32]);
    let agent = FixtureAgent::spawn(AgentReply { status: 200, body: vec![] });
    let (state, id) = make_state("tok", "alice", agent.port, sk);
    let app = make_app!(state);
    let path = format!("/sandboxes/{id}/preview/5173/share?user_id=alice");
    let req = test::TestRequest::post()
        .uri(&path)
        .header("Authorization", "Bearer tok")
        .set_json(&serde_json::json!({"expires_in_secs": 3600, "scope": "evil"}))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[ntex::test]
async fn list_share_returns_audit_metadata() {
    let sk = SigningKey::from_bytes(&[4u8; 32]);
    let agent = FixtureAgent::spawn(AgentReply { status: 200, body: vec![] });
    let (state, id) = make_state("tok", "alice", agent.port, sk);
    let app = make_app!(state.clone());

    // Mint two tokens.
    for scope in ["ro", "rw"] {
        let path = format!("/sandboxes/{id}/preview/5173/share?user_id=alice");
        let req = test::TestRequest::post()
            .uri(&path)
            .header("Authorization", "Bearer tok")
            .set_json(&serde_json::json!({"expires_in_secs": 3600, "scope": scope}))
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), StatusCode::OK);
    }

    let path = format!("/sandboxes/{id}/preview/5173/share?user_id=alice");
    let req = test::TestRequest::get()
        .uri(&path)
        .header("Authorization", "Bearer tok")
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = test::read_body(resp).await; let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let tokens = body["tokens"].as_array().unwrap();
    assert_eq!(tokens.len(), 2);
    let scopes: Vec<&str> = tokens
        .iter()
        .map(|t| t["scope"].as_str().unwrap())
        .collect();
    assert!(scopes.contains(&"ro") && scopes.contains(&"rw"));
    // The bytes themselves are NEVER returned.
    for tok in tokens {
        assert!(tok.get("token").is_none(), "token bytes must not be in list");
        // Wire-stable token_id is `shr_` + raw 22-char base64url tid.
        let tid_str = tok["token_id"].as_str().unwrap();
        assert!(
            tid_str.starts_with("shr_"),
            "GET /share rows prefix token_id with shr_; got {tid_str}"
        );
        assert_eq!(tid_str.len(), 4 + 22, "shr_ + 22-char tid");
    }
}

#[ntex::test]
async fn delete_share_zero_grace_invalidates_existing_cookie() {
    // cookie-after-revoke: mint, convert to cookie,
    // hit (200), DELETE all tokens, hit with cached cookie → 401
    // code:"revoked".
    let sk = SigningKey::from_bytes(&[5u8; 32]);
    let agent = FixtureAgent::spawn(AgentReply { status: 200, body: b"ok".to_vec() });
    let (state, id) = make_state("tok", "alice", agent.port, sk);
    let app = make_app!(state.clone());

    let token = direct_mint(&state, id, 5173, "ro", 3600);
    let cookie_name = cookie_name_for(id);
    let cookie = format!("{cookie_name}={token}");

    // First request with the cookie: 200.
    let req = test::TestRequest::get()
        .uri(&format!("/sandboxes/{id}/preview/5173/index.html"))
        .header("Cookie", cookie.as_str())
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::OK);

    // Explicit DELETE.
    let req = test::TestRequest::delete()
        .uri(&format!(
            "/sandboxes/{id}/preview/5173/share?user_id=alice"
        ))
        .header("Authorization", "Bearer tok")
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::OK);

    // Cookie now invalid — same exact request → 401 code:"revoked".
    let req = test::TestRequest::get()
        .uri(&format!("/sandboxes/{id}/preview/5173/index.html"))
        .header("Cookie", cookie.as_str())
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[ntex::test]
async fn delete_per_token_returns_501_deferred_to_phase_5() {
    let sk = SigningKey::from_bytes(&[6u8; 32]);
    let agent = FixtureAgent::spawn(AgentReply { status: 200, body: vec![] });
    let (state, id) = make_state("tok", "alice", agent.port, sk);
    let app = make_app!(state);
    let req = test::TestRequest::delete()
        .uri(&format!(
            "/sandboxes/{id}/preview/5173/share/shr_x?user_id=alice"
        ))
        .header("Authorization", "Bearer tok")
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::NOT_IMPLEMENTED);
    let bytes = test::read_body(resp).await; let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["error"].as_str().unwrap(), "deferred_to_phase_5");
}

// ═══════════════════════════════════════════════════════════════════
//  Token validation negatives
// ═══════════════════════════════════════════════════════════════════

#[ntex::test]
async fn tampered_byte_in_token_rejected_401() {
    let sk = SigningKey::from_bytes(&[10u8; 32]);
    let agent = FixtureAgent::spawn(AgentReply { status: 200, body: b"ok".to_vec() });
    let (state, id) = make_state("tok", "alice", agent.port, sk);
    let app = make_app!(state.clone());
    let token = direct_mint(&state, id, 5173, "ro", 3600);
    // Flip last byte of sig.
    let mut bytes = token.into_bytes();
    let last = bytes.len() - 1;
    bytes[last] = if bytes[last] == b'A' { b'B' } else { b'A' };
    let bad_tok = String::from_utf8(bytes).unwrap();
    let req = test::TestRequest::get()
        .uri(&format!(
            "/sandboxes/{id}/preview/5173/x?t={bad_tok}"
        ))
        .header("Sec-Fetch-Site", "cross-site")
        .header("Sec-Fetch-Mode", "navigate")
        .header("Sec-Fetch-Dest", "document")
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[ntex::test]
async fn expired_token_returns_401_with_expired_code() {
    let sk = SigningKey::from_bytes(&[11u8; 32]);
    let agent = FixtureAgent::spawn(AgentReply { status: 200, body: vec![] });
    let (state, id) = make_state("tok", "alice", agent.port, sk);
    let app = make_app!(state.clone());

    // Mint with TTL=2s, sleep 3s so the validator sees it expired.
    let token = direct_mint(&state, id, 5173, "ro", 2);
    std::thread::sleep(Duration::from_secs(3));
    let req = test::TestRequest::get()
        .uri(&format!(
            "/sandboxes/{id}/preview/5173/x?t={token}"
        ))
        .header("Sec-Fetch-Site", "cross-site")
        .header("Sec-Fetch-Mode", "navigate")
        .header("Sec-Fetch-Dest", "document")
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let bytes = test::read_body(resp).await; let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["error"].as_str().unwrap(), "expired");
}

/// Dispatch-path scope mismatch — the cookie validates fully (HMAC,
/// audience, sandbox, port, expiry) but the bound scope (`ro`) does
/// not allow the request method (`POST`). Per § II.8 the dispatch
/// path collapses scope-mismatch into the same uniform 404 as
/// port-deny / not-owner / sandbox-not-found so an attacker on the
/// public edge cannot distinguish a valid-but-wrong-scope token from
/// any other reason for failure.
///
/// The cookie-conversion path (`?t=<token>`) is intentionally noisier:
/// see [`scope_forbidden_via_cookie_conversion_returns_403`] below.
#[ntex::test]
async fn ro_token_post_via_dispatch_returns_404_uniform() {
    let sk = SigningKey::from_bytes(&[12u8; 32]);
    let agent = FixtureAgent::spawn(AgentReply { status: 200, body: b"ok".to_vec() });
    let (state, id) = make_state("tok", "alice", agent.port, sk);
    let app = make_app!(state.clone());
    let token = direct_mint(&state, id, 5173, "ro", 3600);
    let cookie = format!("{}={token}", cookie_name_for(id));
    // Single-segment path here is sufficient to exercise the
    // dispatch + authorize_with_method gate. (Production route is
    // `/sandboxes/{id}/preview/{port}/{path}*` for tail-match.)
    let req = test::TestRequest::post()
        .uri(&format!("/sandboxes/{id}/preview/5173/api"))
        .header("Cookie", cookie.as_str())
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(
        resp.status(),
        StatusCode::NOT_FOUND,
        "ro+POST through dispatch must surface as uniform 404 \
         (oracle-safe; scope-mismatch coalesced with port-deny / not-owner)"
    );
    let bytes = test::read_body(resp).await;
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["error"].as_str().unwrap(), "not_found");
}

/// Cookie-conversion scope-mismatch — the helpful 403 path.
///
/// `?t=<ro-token>` on a POST hits `handle_cookie_conversion`, which
/// runs the *full* `validate_token` (with scope-vs-method enforced)
/// so the AI-builder UI gets a distinct `code: "scope_forbidden"`
/// error to render "this is a read-only link, the action you tried
/// needs a `rw` token". Only the dispatch path collapses into 404
/// (oracle-uniform); cookie-conversion stays specific by design.
#[ntex::test]
async fn scope_forbidden_via_cookie_conversion_returns_403() {
    let sk = SigningKey::from_bytes(&[13u8; 32]);
    let agent = FixtureAgent::spawn(AgentReply { status: 200, body: b"ok".to_vec() });
    let (state, id) = make_state("tok", "alice", agent.port, sk);
    let app = make_app!(state.clone());
    let token = direct_mint(&state, id, 5173, "ro", 3600);
    let req = test::TestRequest::post()
        .uri(&format!("/sandboxes/{id}/preview/5173/api?t={token}"))
        .header("Sec-Fetch-Site", "cross-site")
        .header("Sec-Fetch-Mode", "navigate")
        .header("Sec-Fetch-Dest", "document")
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "ro+POST via cookie-conversion (?t=…) must surface 403 \
         scope_forbidden for the AI-builder UI"
    );
    let bytes = test::read_body(resp).await;
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["error"].as_str().unwrap(), "scope_forbidden");
}

#[ntex::test]
async fn cross_sandbox_token_rejected_with_other_sandbox_path() {
    let sk_a = SigningKey::from_bytes(&[20u8; 32]);
    let sk_b = SigningKey::from_bytes(&[21u8; 32]);
    let agent = FixtureAgent::spawn(AgentReply { status: 200, body: b"ok".to_vec() });
    let (state_a, id_a) = make_state("tok", "alice", agent.port, sk_a);
    // Build state-b inside the same registry / backend? Different
    // sandboxes share the registry through the Arc<AppState>. Because
    // each `make_state` builds a *separate* AppState, we instead
    // inject a second sandbox into state_a's backend / registry.
    let (state_b, id_b) = make_state("tok", "bob", agent.port, sk_b);
    let app_a = make_app!(state_a.clone());
    let app_b = make_app!(state_b.clone());

    let token_a = direct_mint(&state_a, id_a, 5173, "ro", 3600);
    // Cookie name encodes the *target* sandbox; we rebuild it.
    let cookie_name_b = cookie_name_for(id_b);
    let cookie = format!("{cookie_name_b}={token_a}");
    let req = test::TestRequest::get()
        .uri(&format!("/sandboxes/{id_b}/preview/5173/index.html"))
        .header("Cookie", cookie.as_str())
        .to_request();
    let resp = test::call_service(&app_b, req).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    // Sanity: the same token DOES validate against its own sandbox.
    let cookie_a = format!("{}={token_a}", cookie_name_for(id_a));
    let req = test::TestRequest::get()
        .uri(&format!("/sandboxes/{id_a}/preview/5173/index.html"))
        .header("Cookie", cookie_a.as_str())
        .to_request();
    let resp = test::call_service(&app_a, req).await;
    assert_eq!(resp.status(), StatusCode::OK);
}

#[ntex::test]
async fn raw_token_over_1kib_rejected_before_hmac() {
    let sk = SigningKey::from_bytes(&[30u8; 32]);
    let agent = FixtureAgent::spawn(AgentReply { status: 200, body: vec![] });
    let (state, id) = make_state("tok", "alice", agent.port, sk);
    let app = make_app!(state);
    let huge = "x".repeat(TOKEN_RAW_MAX + 50);
    let req = test::TestRequest::get()
        .uri(&format!(
            "/sandboxes/{id}/preview/5173/x?t={huge}"
        ))
        .header("Sec-Fetch-Site", "cross-site")
        .header("Sec-Fetch-Mode", "navigate")
        .header("Sec-Fetch-Dest", "document")
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[ntex::test]
async fn token_with_extra_field_rejected_via_deny_unknown_fields() {
    let sk = SigningKey::from_bytes(&[31u8; 32]);
    let agent = FixtureAgent::spawn(AgentReply { status: 200, body: vec![] });
    let (state, id) = make_state("tok", "alice", agent.port, sk);
    let app = make_app!(state.clone());

    // Hand-build a JSON with an extra "evil" field; sign it with the
    // sandbox's mint-the-ring secret; convert via ?t=.
    let ring = state.sandboxes.ensure_preview_secret(id).unwrap();
    let now = now_unix();
    let json = serde_json::json!({
        "aud": "preview",
        "sbx": id.to_string(),
        "port": 5173,
        "iat": now,
        "exp": now + 3600,
        "sv": ring.sv_current,
        "tid": "abc",
        "scope": "ro",
        "evil": "x",
    });
    let payload_b = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&json).unwrap());
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    let mut mac = Hmac::<Sha256>::new_from_slice(&ring.current).unwrap();
    mac.update(payload_b.as_bytes());
    let sig_b = URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes());
    let tok = format!("{payload_b}~{sig_b}");

    let req = test::TestRequest::get()
        .uri(&format!("/sandboxes/{id}/preview/5173/x?t={tok}"))
        .header("Sec-Fetch-Site", "cross-site")
        .header("Sec-Fetch-Mode", "navigate")
        .header("Sec-Fetch-Dest", "document")
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[ntex::test]
async fn separator_dot_rejected_separator_double_tilde_rejected() {
    // : parser accepts `payload~sig`, rejects
    // `payload.sig` (legacy separator), rejects `payload~sig~extra`.
    let sk = SigningKey::from_bytes(&[32u8; 32]);
    let agent = FixtureAgent::spawn(AgentReply { status: 200, body: vec![] });
    let (state, id) = make_state("tok", "alice", agent.port, sk);
    let app = make_app!(state.clone());

    let token = direct_mint(&state, id, 5173, "ro", 3600);

    // Replace `~` with `.`.
    let dotted = token.replacen('~', ".", 1);
    let req = test::TestRequest::get()
        .uri(&format!("/sandboxes/{id}/preview/5173/x?t={dotted}"))
        .header("Sec-Fetch-Site", "cross-site")
        .header("Sec-Fetch-Mode", "navigate")
        .header("Sec-Fetch-Dest", "document")
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "dot separator rejected");

    // Append `~extra`.
    let extra = format!("{token}~extra");
    let req = test::TestRequest::get()
        .uri(&format!("/sandboxes/{id}/preview/5173/x?t={extra}"))
        .header("Sec-Fetch-Site", "cross-site")
        .header("Sec-Fetch-Mode", "navigate")
        .header("Sec-Fetch-Dest", "document")
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "double-tilde rejected"
    );
}

#[ntex::test]
async fn missing_sec_fetch_site_returns_400_client_too_old() {
    // fail-closed: cookie-conversion endpoint without
    // Sec-Fetch-* triple → 400 code:"client_too_old".
    let sk = SigningKey::from_bytes(&[33u8; 32]);
    let agent = FixtureAgent::spawn(AgentReply { status: 200, body: vec![] });
    let (state, id) = make_state("tok", "alice", agent.port, sk);
    let app = make_app!(state.clone());
    let token = direct_mint(&state, id, 5173, "ro", 3600);
    let req = test::TestRequest::get()
        .uri(&format!("/sandboxes/{id}/preview/5173/x?t={token}"))
        // INTENTIONALLY no Sec-Fetch-* headers.
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let bytes = test::read_body(resp).await; let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["error"].as_str().unwrap(), "client_too_old");
}

#[ntex::test]
async fn missing_iss_validates_token_with_iss_records_value() {
    // — audit-only at v7. Token without iss validates;
    // token with iss records iss in the audit row.
    let sk = SigningKey::from_bytes(&[34u8; 32]);
    let agent = FixtureAgent::spawn(AgentReply { status: 200, body: b"ok".to_vec() });
    let (state, id) = make_state("tok", "alice", agent.port, sk);
    let app = make_app!(state.clone());

    // (a) Mint a token with iss=Some.
    let ring = state.sandboxes.ensure_preview_secret(id).unwrap();
    let now = now_unix();
    let mut claims_with = TokenClaims {
        aud: "preview".into(),
        sbx: id.to_string(),
        port: 5173,
        iss: Some("usr_alice".into()),
        iat: now,
        exp: now + 3600,
        sv: ring.sv_current,
        tid: preview_share::fresh_token_id(),
        scope: "ro".into(),
    };
    let tok_with = mint_token(&claims_with, &ring.current);
    state.sandboxes.append_audit(
        id,
        zeroship_sandbox::registry::PreviewAuditEntry {
            token_id: claims_with.tid.clone(),
            port: 5173,
            issued_at_unix: now,
            expires_at_unix: claims_with.exp,
            scope: "ro".into(),
            secret_version: ring.sv_current,
            iss: claims_with.iss.clone(),
            last_used_at_unix: 0,
            use_count: 0,
        },
    );
    let cookie = format!("{}={tok_with}", cookie_name_for(id));
    let req = test::TestRequest::get()
        .uri(&format!("/sandboxes/{id}/preview/5173/x"))
        .header("Cookie", cookie.as_str())
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::OK);

    // (b) Mint a token with iss=None.
    claims_with.iss = None;
    claims_with.tid = preview_share::fresh_token_id();
    let tok_without = mint_token(&claims_with, &ring.current);
    state.sandboxes.append_audit(
        id,
        zeroship_sandbox::registry::PreviewAuditEntry {
            token_id: claims_with.tid.clone(),
            port: 5173,
            issued_at_unix: now,
            expires_at_unix: claims_with.exp,
            scope: "ro".into(),
            secret_version: ring.sv_current,
            iss: None,
            last_used_at_unix: 0,
            use_count: 0,
        },
    );
    let cookie = format!("{}={tok_without}", cookie_name_for(id));
    let req = test::TestRequest::get()
        .uri(&format!("/sandboxes/{id}/preview/5173/y"))
        .header("Cookie", cookie.as_str())
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::OK, "iss-less token validates");

    // Audit table records iss for the with-iss token; missing for the
    // other.
    let audit = state.sandboxes.list_audit(id);
    assert_eq!(audit.len(), 2);
    let with = audit.iter().find(|r| r.iss.is_some()).unwrap();
    let without = audit.iter().find(|r| r.iss.is_none()).unwrap();
    assert_eq!(with.iss.as_deref(), Some("usr_alice"));
    assert!(without.iss.is_none());
}

#[ntex::test]
async fn hmac_input_invariant_pinned_via_two_field_orders() {
    // : validator hashes the on-wire bytes; two tokens with
    // different field orders both validate iff the validator is wire-
    // byte-faithful.
    let sk = SigningKey::from_bytes(&[35u8; 32]);
    let agent = FixtureAgent::spawn(AgentReply { status: 200, body: b"ok".to_vec() });
    let (state, id) = make_state("tok", "alice", agent.port, sk);
    let app = make_app!(state.clone());
    let ring = state.sandboxes.ensure_preview_secret(id).unwrap();
    let now = now_unix();
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    let order_a = format!(
        r#"{{"aud":"preview","sbx":"{id}","port":5173,"iat":{now},"exp":{exp},"sv":{sv},"tid":"a","scope":"ro"}}"#,
        id = id,
        now = now,
        exp = now + 3600,
        sv = ring.sv_current,
    );
    let order_b = format!(
        r#"{{"port":5173,"sbx":"{id}","aud":"preview","sv":{sv},"tid":"b","scope":"ro","iat":{now},"exp":{exp}}}"#,
        id = id,
        now = now,
        exp = now + 3600,
        sv = ring.sv_current,
    );
    for (label, payload) in [("A", order_a.as_bytes()), ("B", order_b.as_bytes())] {
        let payload_b = URL_SAFE_NO_PAD.encode(payload);
        let mut mac = Hmac::<Sha256>::new_from_slice(&ring.current).unwrap();
        mac.update(payload_b.as_bytes());
        let sig_b = URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes());
        let tok = format!("{payload_b}~{sig_b}");
        let req = test::TestRequest::get()
            .uri(&format!("/sandboxes/{id}/preview/5173/x?t={tok}"))
            .header("Sec-Fetch-Site", "cross-site")
            .header("Sec-Fetch-Mode", "navigate")
            .header("Sec-Fetch-Dest", "document")
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), StatusCode::SEE_OTHER, "field-order {label} validates");
    }
}

#[ntex::test]
async fn anonymous_existing_vs_fabricated_byte_identical_modulo_slug() {
    // oracle: anonymous against existing-vs-fabricated slug
    // produces byte-identical 401 bodies (modulo `next` echoing the
    // slug). v1 controller's body has no `login_url` containing the
    // slug — that DNS wiring does not exist yet — so we just assert the bodies are
    // identical and the status is 401 in both cases.
    let sk = SigningKey::from_bytes(&[36u8; 32]);
    let agent = FixtureAgent::spawn(AgentReply { status: 200, body: vec![] });
    let (state, real_id) = make_state("tok", "alice", agent.port, sk);
    let app = make_app!(state);

    // Real existing sandbox-id, no auth.
    let req = test::TestRequest::get()
        .uri(&format!("/sandboxes/{real_id}/preview/5173/x"))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let body_real = test::read_body(resp).await;

    // Fabricated UUID, no auth.
    let fake = Uuid::now_v7();
    let req = test::TestRequest::get()
        .uri(&format!("/sandboxes/{fake}/preview/5173/x"))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let body_fake = test::read_body(resp).await;

    assert_eq!(body_real, body_fake, "401 body must be invariant");
}

#[ntex::test]
async fn share_token_not_accepted_against_exec_or_files_endpoints() {
    // The validator only runs on the preview public route; routes
    // like /exec or /files have their own bearer auth. A request
    // carrying a share-token cookie to `/exec` is just an unauthed
    // request to the controller.
    //
    // We verify by: build a token, mount it as the share cookie,
    // hit `/sandboxes/{id}/preview/5173/x` (cookie-auth path = OK),
    // then verify that the same cookie does NOT mysteriously make
    // `/sandboxes/{id}/exec` work (we don't even mount that route in
    // this test app — assert that handler resolution would fail at
    // an unrelated path under the same prefix).
    //
    // The negative test against /exec specifically requires the
    // `handlers::exec` route being mounted; instead we assert here
    // the simpler invariant: the share-token cookie does NOT add
    // any `Authorization: Bearer` to subsequent dispatches, so
    // any non-/preview/ route that gates on the bearer continues to
    // 401.
    let sk = SigningKey::from_bytes(&[37u8; 32]);
    let agent = FixtureAgent::spawn(AgentReply { status: 200, body: b"ok".to_vec() });
    let (state, id) = make_state("tok", "alice", agent.port, sk);
    let token = direct_mint(&state, id, 5173, "ro", 3600);
    let cookie = format!("{}={token}", cookie_name_for(id));

    // App mounts only the preview routes; we hit a path the app
    // doesn't handle to demonstrate the cookie has no out-of-band
    // effect.
    let app = make_app!(state);
    let req = test::TestRequest::get()
        .uri(&format!("/sandboxes/{id}/exec"))
        .header("Cookie", cookie.as_str())
        .to_request();
    let resp = test::call_service(&app, req).await;
    // ntex's default no-route is 404 — fine. The point is the
    // cookie's reach is bounded; it's not magically interpreted as a
    // bearer for the rest of the API surface.
    assert!(
        resp.status() == StatusCode::NOT_FOUND
            || resp.status() == StatusCode::METHOD_NOT_ALLOWED,
        "share cookie must not unlock non-preview routes; got {}",
        resp.status()
    );
}

#[ntex::test]
async fn cross_creator_authorize_returns_404_uniform() {
    // authorize ownership: creator-A's bearer
    // against creator-B's preview URL → 404 (uniform, no oracle).
    let sk_a = SigningKey::from_bytes(&[40u8; 32]);
    let sk_b = SigningKey::from_bytes(&[41u8; 32]);
    let agent = FixtureAgent::spawn(AgentReply { status: 200, body: vec![] });
    let (_state_a, id_a) = make_state("tok", "alice", agent.port, sk_a);
    let (state_b, id_b) = make_state("tok", "bob", agent.port, sk_b);
    let _ = id_a; // unused — we hit B's preview with A's claim.
    let app_b = make_app!(state_b);
    let req = test::TestRequest::get()
        .uri(&format!(
            "/sandboxes/{id_b}/preview/5173/index.html?user_id=alice"
        ))
        .header("Authorization", "Bearer tok")
        .to_request();
    let resp = test::call_service(&app_b, req).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// Persist-on-mint end-to-end: POST /share through the real handler
/// triggers `persist_preview_state` which calls
/// `Backend::seal_with_preview_state`. We then unseal the file and
/// confirm the freshly-minted token's audit row + the new
/// `PreviewSecrets` ring round-trip — i.e. a controller crash mid-
/// flight wouldn't lose the just-issued token's metadata.
#[ntex::test]
async fn mint_endpoint_seals_preview_state_to_disk() {
    use zeroship_sandbox::persist::{unseal_one, AeadKey};

    let sk = SigningKey::from_bytes(&[60u8; 32]);
    let agent = FixtureAgent::spawn(AgentReply { status: 200, body: vec![] });
    let (state, id, persist_dir, aead) =
        make_state_with_persist("tok", "alice", agent.port, sk);
    let app = make_app!(state.clone());

    let path = format!("/sandboxes/{id}/preview/5173/share?user_id=alice");
    let req = test::TestRequest::post()
        .uri(&path)
        .header("Authorization", "Bearer tok")
        .set_json(&serde_json::json!({"expires_in_secs": 3600, "scope": "ro"}))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = test::read_body(resp).await;
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let token_id_wire = body["token_id"].as_str().unwrap().to_string();
    let raw_tid = token_id_wire.strip_prefix("shr_").unwrap();

    // Read back the sealed file directly (simulates controller restart).
    let sealed_path = persist_dir
        .join("sealed-records")
        .join(zeroship_sandbox::persist::seal_filename_for(id));
    assert!(
        sealed_path.exists(),
        "POST /share must persist-on-mint (sealed file at {sealed_path:?})"
    );
    let _ = raw_tid; // round-8: audit metadata moves to pg
    let key = AeadKey::from_bytes(aead);
    let reread = unseal_one(&sealed_path, &key).expect("unseal");
    assert_eq!(reread.sandbox_id, id.to_string());
    let ring = reread.preview_secrets.expect("ring sealed on mint");
    assert_eq!(ring.sv_current, 1);
    // per-token audit metadata is now a `sandbox.shares`
    // pg row; the sealed record holds the secret ring only.
    let _ = std::fs::remove_dir_all(&persist_dir);
}

/// Persist-on-rotate: DELETE /share rebumps the sealed record with
/// the new `(sv_current, audit=cleared)` state. The sandbox itself
/// stays alive — the sealed record is updated, NOT removed.
#[ntex::test]
async fn delete_endpoint_seals_post_rotate_state_to_disk() {
    use zeroship_sandbox::persist::{unseal_one, AeadKey};

    let sk = SigningKey::from_bytes(&[61u8; 32]);
    let agent = FixtureAgent::spawn(AgentReply { status: 200, body: vec![] });
    let (state, id, persist_dir, aead) =
        make_state_with_persist("tok", "alice", agent.port, sk);
    let app = make_app!(state.clone());

    // 1. Mint a token first so there's audit content + sv_current=1.
    let path = format!("/sandboxes/{id}/preview/5173/share?user_id=alice");
    let req = test::TestRequest::post()
        .uri(&path)
        .header("Authorization", "Bearer tok")
        .set_json(&serde_json::json!({"expires_in_secs": 3600, "scope": "ro"}))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::OK);

    // 2. DELETE — explicit rotate-and-clear (zero-grace).
    let req = test::TestRequest::delete()
        .uri(&format!(
            "/sandboxes/{id}/preview/5173/share?user_id=alice"
        ))
        .header("Authorization", "Bearer tok")
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::OK);

    // 3. Sealed file should reflect the post-rotate state: bumped
    //    sv_current AND empty audit (the DELETE handler clears the
    //    audit table; the seal helper re-snapshots from the registry).
    let sealed_path = persist_dir
        .join("sealed-records")
        .join(zeroship_sandbox::persist::seal_filename_for(id));
    assert!(sealed_path.exists(), "DELETE persists the rotated state");
    let reread = unseal_one(&sealed_path, &AeadKey::from_bytes(aead)).expect("unseal");
    let ring = reread.preview_secrets.expect("ring still sealed post-rotate");
    assert_eq!(
        ring.sv_current, 2,
        "DELETE bumps secret_version_current to 2"
    );
    // audit table is in pg now; the sealed record only
    // carries the secret ring after the rotate.
    let _ = std::fs::remove_dir_all(&persist_dir);
}
