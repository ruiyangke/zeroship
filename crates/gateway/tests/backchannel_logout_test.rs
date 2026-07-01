//! Live-PG smoke test for `gateway::sessions::revoke_app_sessions_for_user` —
//! the revoke-side of the OIDC Back-Channel Logout 1.0 handler (Phase 7 U1.2),
//! per-app under RLS (changeset 0025).
//!
//! Skipped silently when `AUTH_DB_URL` is unset (same convention as
//! the rest of the gateway PG smoke tests, e.g. `sessions_test.rs`).
//!
//! Coverage:
//!   - Seed two live sessions for the same user_id (different app_ids)
//!     and one session for a different user_id at the FIRST app.
//!   - Call `revoke_app_sessions_for_user(app_a, user)`. The returned count
//!     must equal 1 (only the target user's session AT app_a).
//!   - The same user's session at app_b must STILL validate (per-app scope —
//!     a per-app BCL never logs the user out of OTHER apps; the former
//!     cross-tenant `revoke_all_for_user` was removed because the non-bypass
//!     `zeroship_gateway` role cannot span tenants under RLS).
//!   - The unrelated user's session at app_a must still validate.
//!   - Calling it again returns 0 (idempotent — the `revoked_at IS NULL`
//!     filter skips the already-revoked row).
//!
//! The handler-level path (verify + revoke) is exercised by the
//! Phase 7 follow-up e2e against a real hydra; for the verifier-only
//! check see `crates/core/src/logout_token.rs::tests`.

use std::sync::Arc;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use compio_postgres::{connect, Client, NoTls};
use ed25519_dalek::pkcs8::EncodePrivateKey;
use ed25519_dalek::SigningKey;
use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
use ntex::http::StatusCode;
use ntex::web::{self, test, HttpResponse};
use serde::Serialize;
use serde_json::json;
use uuid::Uuid;
use zeroship_gateway::{
    anchors, backchannel_logout,
    blob_cache::{BlobCache, DiskBlobCache},
    db::{self, DbConfig},
    enforce, idempotency,
    oidc_rp::OidcRp,
    proxy::HashRing,
    sessions::{create, revoke_app_sessions_for_user, validate, NewSession},
    sync::RouteCache,
    GateConfig, GateState,
};

#[compio::test]
async fn revoke_app_sessions_for_user_revokes_only_the_target_app_and_user() {
    let Ok(dsn) = std::env::var("AUTH_DB_URL") else {
        eprintln!("skipping (no AUTH_DB_URL)");
        return;
    };

    let (mut client, connection) = connect(&dsn, NoTls).await.expect("connect");
    compio::runtime::spawn(async move {
        if let Err(e) = connection.run().await {
            eprintln!("connection error: {e}");
        }
    })
    .detach();

    // Two sessions for the same user at two different apps. The per-app BCL
    // revokes ONLY the target app's session — never the other app's (RLS
    // tenant scope; the former cross-tenant `revoke_all_for_user` was removed).
    let target_user_id = insert_user(&client, "gateway-bcl-target").await;
    let target_user = target_user_id.to_string();
    let app_a = Uuid::new_v4();
    let app_b = Uuid::new_v4();

    let s_a = create(
        &mut client,
        &NewSession {
            user_id: &target_user,
            app_id: app_a,
            email: Some("alice@zeroship.test"),
            name: Some("Alice"),
            avatar_url: None,
            email_verified: true,
            granted_scopes: &[],
            auth_time: None,
            amr: &[],
        },
    )
    .await
    .expect("create s_a");

    let s_b = create(
        &mut client,
        &NewSession {
            user_id: &target_user,
            app_id: app_b,
            email: Some("alice@zeroship.test"),
            name: Some("Alice"),
            avatar_url: None,
            email_verified: true,
            granted_scopes: &[],
            auth_time: None,
            amr: &[],
        },
    )
    .await
    .expect("create s_b");

    // One session for an unrelated user at app_a — must NOT be touched.
    let other_user_id = insert_user(&client, "gateway-bcl-other").await;
    let other_user = other_user_id.to_string();
    let s_other = create(
        &mut client,
        &NewSession {
            user_id: &other_user,
            app_id: app_a,
            email: Some("bob@zeroship.test"),
            name: Some("Bob"),
            avatar_url: None,
            email_verified: true,
            granted_scopes: &[],
            auth_time: None,
            amr: &[],
        },
    )
    .await
    .expect("create s_other");

    // Sanity: all three validate before we revoke.
    assert!(validate(&mut client, s_a.id, app_a)
        .await
        .expect("pre validate s_a")
        .is_some());
    assert!(validate(&mut client, s_b.id, app_b)
        .await
        .expect("pre validate s_b")
        .is_some());
    assert!(validate(&mut client, s_other.id, app_a)
        .await
        .expect("pre validate s_other")
        .is_some());

    // Revoke the target user's sessions AT app_a only.
    let count = revoke_app_sessions_for_user(&mut client, app_a, &target_user)
        .await
        .expect("revoke_app_sessions_for_user");
    assert_eq!(count, 1, "expected exactly 1 session revoked (target user @ app_a), got {count}");

    // The target session at app_a must now fail validation.
    assert!(
        validate(&mut client, s_a.id, app_a)
            .await
            .expect("post validate s_a")
            .is_none(),
        "s_a (target user @ app_a) must be revoked"
    );

    // The SAME user's session at app_b must STILL validate (per-app scope).
    assert!(
        validate(&mut client, s_b.id, app_b)
            .await
            .expect("post validate s_b")
            .is_some(),
        "s_b (same user @ app_b) must NOT be revoked by a per-app BCL at app_a"
    );

    // The unrelated user's session at app_a must still validate.
    assert!(
        validate(&mut client, s_other.id, app_a)
            .await
            .expect("post validate s_other")
            .is_some(),
        "unrelated user's session must NOT be revoked"
    );

    // Idempotent — running it again touches no rows.
    let again = revoke_app_sessions_for_user(&mut client, app_a, &target_user)
        .await
        .expect("revoke_app_sessions_for_user idempotent");
    assert_eq!(again, 0, "second revoke must touch 0 rows (filter on revoked_at IS NULL)");

    // Cleanup (best effort).
    for id in [s_a.id, s_b.id, s_other.id] {
        client
            .execute("DELETE FROM zeroship.gateway_sessions WHERE id = $1", &[&id])
            .await
            .ok();
    }
    for id in [target_user_id, other_user_id] {
        client
            .execute("DELETE FROM zeroship.users WHERE id = $1", &[&id])
            .await
            .ok();
    }
}

async fn insert_user(client: &Client, label: &str) -> Uuid {
    let email = format!("{label}-{}@zeroship.test", Uuid::new_v4().simple());
    let rows = client
        .query(
            "INSERT INTO zeroship.users (email, name, email_verified_at)
             VALUES ($1, $2, NOW())
             RETURNING id",
            &[&email, &label],
        )
        .await
        .expect("insert user");
    rows[0].get("id")
}

/// Seed a real `zeroship.apps` row with the given stable UUID so the
/// `app_session_anchors` / `gateway_sessions` FKs to `apps(id)` are satisfied.
async fn seed_app(client: &Client, app_id: Uuid, name: &str) {
    client
        .execute(
            "INSERT INTO zeroship.apps (id, name, plan_id, api_key, api_key_hash) \
             VALUES ($1, $2, 'free', $3, $4)",
            &[
                &app_id,
                &name,
                &format!("key-{}", Uuid::new_v4().simple()),
                &format!("hash-{}", Uuid::new_v4().simple()),
            ],
        )
        .await
        .expect("seed app");
}

/// Seed a real `zeroship.oauth_clients` row so the `app_session_anchors`
/// FK to `oauth_clients(client_id)` is satisfied.
async fn seed_oauth_client(client: &Client, client_id: &str) {
    let empty: Vec<String> = vec![];
    client
        .execute(
            "INSERT INTO zeroship.oauth_clients \
                (client_id, client_name, redirect_uris, scopes, hydra_client_id) \
             VALUES ($1, $2, $3, $4, $1) \
             ON CONFLICT (client_id) DO NOTHING",
            &[&client_id, &"BCL test client", &empty, &empty],
        )
        .await
        .expect("seed oauth_client");
}

/// Seed one `zeroship.app_session_anchors` row directly (bypassing the enc
/// machinery — the ciphertext is opaque here; the Hydra revoke fan-out the BCL
/// performs is best-effort and may fail harmlessly in the test). Returns the
/// new anchor id.
async fn seed_anchor(client: &Client, app_id: Uuid, client_id: &str, global_user_id: Uuid) -> Uuid {
    let scopes: Vec<String> = vec!["openid".into()];
    let refresh_enc: Vec<u8> = vec![1, 2, 3, 4];
    let rows = client
        .query(
            "INSERT INTO zeroship.app_session_anchors \
                (app_id, client_id, global_user_id, refresh_token_enc, refresh_family_id, \
                 granted_scopes, abs_expires_at) \
             VALUES ($1, $2, $3, $4, $5, $6, NOW() + interval '30 days') \
             RETURNING id",
            &[
                &app_id,
                &client_id,
                &global_user_id,
                &refresh_enc,
                &format!("fam-{}", Uuid::new_v4().simple()),
                &scopes,
            ],
        )
        .await
        .expect("seed anchor");
    rows[0].get("id")
}

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

    async fn get_blob_to_file(
        &self,
        _hash: &str,
        _out: &compio::fs::File,
        _expected_size: Option<u64>,
        _max_bytes: u64,
    ) -> Result<u64, zeroship_bundle::BlobError> {
        Err(zeroship_bundle::BlobError::NotFound("unused".into()))
    }

    async fn put_manifest(
        &self,
        _app_id: &Uuid,
        _deploy_hash: &str,
        _json: &[u8],
    ) -> Result<(), zeroship_bundle::BlobError> {
        Ok(())
    }

    async fn get_manifest(
        &self,
        _app_id: &Uuid,
        _deploy_hash: &str,
    ) -> Result<bytes::Bytes, zeroship_bundle::BlobError> {
        Err(zeroship_bundle::BlobError::NotFound("unused".into()))
    }

    async fn delete_app_manifests(
        &self,
        _app_id: &Uuid,
    ) -> Result<(), zeroship_bundle::BlobError> {
        Ok(())
    }
}

#[derive(Clone)]
struct TestKey {
    encoding: EncodingKey,
    kid: String,
    public_key_b64: String,
}

#[derive(Serialize)]
struct Jwk<'a> {
    kty: &'a str,
    alg: &'a str,
    #[serde(rename = "use")]
    use_: &'a str,
    crv: &'a str,
    kid: &'a str,
    x: &'a str,
}

#[derive(Serialize)]
struct Jwks<'a> {
    keys: Vec<Jwk<'a>>,
}

async fn jwks(state: web::types::State<Arc<TestKey>>) -> HttpResponse {
    HttpResponse::Ok().json(&Jwks {
        keys: vec![Jwk {
            kty: "OKP",
            alg: "EdDSA",
            use_: "sig",
            crv: "Ed25519",
            kid: &state.kid,
            x: &state.public_key_b64,
        }],
    })
}

fn make_key() -> TestKey {
    let sk = SigningKey::from_bytes(&[9u8; 32]);
    let pkcs8 = sk.to_pkcs8_der().expect("encode pkcs8");
    let encoding = EncodingKey::from_ed_der(pkcs8.as_bytes());
    let public_key_b64 = URL_SAFE_NO_PAD.encode(sk.verifying_key().to_bytes());
    TestKey {
        encoding,
        kid: format!("bcl-{}", Uuid::new_v4().simple()),
        public_key_b64,
    }
}

fn sign_logout_token(key: &TestKey, issuer: &str, sub: &str, jti: &str) -> String {
    let now = i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs(),
    )
    .unwrap();
    let mut header = Header::new(Algorithm::EdDSA);
    header.kid = Some(key.kid.clone());
    let claims = json!({
        "iss": issuer,
        "aud": "gateway",
        "iat": now,
        "jti": jti,
        "events": { zeroship_core::logout_token::BCL_EVENT: {} },
        "sub": sub,
        "sid": format!("sid-{jti}"),
    });
    encode(&header, &claims, &key.encoding).expect("encode logout_token")
}

fn build_handler_state(db: DbConfig, auth_base: &str) -> Arc<GateState> {
    let mut tmp = std::env::temp_dir();
    tmp.push(format!("zsgate-bcl-{}", Uuid::new_v4().simple()));
    let disk = DiskBlobCache::new(tmp, 1024 * 1024).expect("disk cache");
    Arc::new(GateState {
        config: GateConfig {
            control_url: String::new(),
            control_key: String::new(),
            worker_urls: vec![],
            poll_interval_secs: 5,
            worker_key: String::new(),
            hydra_public_url: String::new(),
            auth_ui_url: auth_base.to_string(),
            insecure_dev: true,
            trust_proxy: false,
            public_url: "https://api.zeroship.ai".into(),
        },
        routes: RouteCache::new(),
        hash_ring: HashRing::new(vec!["http://0.0.0.0:0".into()], 1),
        rate_limiters: enforce::RateLimitRegistry::new(1, 1),
        per_rule_rate_limits: enforce::PerRuleRateLimitRegistry::new(),
        concurrency: enforce::ConcurrencyRegistry::new(1),
        blob_store: Arc::new(StubBlobStore),
        blob_cache: BlobCache::new(8 * 1024 * 1024),
        disk_cache: disk,
        idempotency_store: Arc::new(idempotency::InMemoryIdempotencyStore::new()),
        oidc_rp: Arc::new(OidcRp::new(
            auth_base,
            "gateway",
            "test-secret",
            b"test-stash-key-32-bytes-long----".to_vec(),
        )),
        db: Some(db),
        logout_jti_cache: Arc::new(zeroship_core::logout_token::LogoutJtiCache::default()),
        revocation_cache: Arc::new(zeroship_core::wrapper_revocation::RevocationCache::new()),
        signing_key: None,
        prev_signing_key: None,
        session_issuer: None::<Arc<zeroship_gateway::session_token::Issuer>>,
        session_verifier: None::<Arc<zeroship_gateway::session_token::Verifier>>,
        anchor_enc_key: [0u8; 32],
            pairwise_salt: [0u8; 32],
        meter: Arc::new(zeroship_metering::Meter::new()),
    })
}

async fn audit_count(client: &Client, jti: &str) -> i64 {
    let row = client
        .query_one(
            "SELECT COUNT(*)::BIGINT AS count \
             FROM zeroship.audit_events \
             WHERE event_type = $1 AND detail->>'jti' = $2",
            &[&"backchannel_logout_revoke", &jti],
        )
        .await
        .expect("audit count");
    row.get("count")
}

#[ntex::test]
async fn handler_accepts_replay_idempotently_without_duplicate_revocation_audit() {
    let Ok(dsn) = std::env::var("AUTH_DB_URL") else {
        eprintln!("skipping (no AUTH_DB_URL)");
        return;
    };

    let (client, connection) = connect(&dsn, NoTls).await.expect("connect");
    compio::runtime::spawn(async move {
        if let Err(e) = connection.run().await {
            eprintln!("connection error: {e}");
        }
    })
    .detach();
    // `db` (single client) drives the test's direct seed/assert/cleanup
    // helpers; the RLS-scoped store fns (`create`/`validate`) need `&mut Client`,
    // so it is bound `mut`. `db_cfg` backs the handler's `GateState`, which now
    // holds a `DbConfig` (the handler builds its own per-thread pool from it).
    // Both point at the same rows.
    let mut db = client;
    let db_cfg = DbConfig::new(dsn.clone(), 4);

    let key = make_key();
    let jwks_key = Arc::new(key.clone());
    let jwks_server = test::server(move || {
        let jwks_key = jwks_key.clone();
        async move {
            web::App::new().state(jwks_key).service(
                web::resource("/.well-known/jwks.json").route(web::get().to(jwks)),
            )
        }
    })
    .await;
    let auth_base = jwks_server.url("").trim_end_matches('/').to_string();
    let issuer = format!("{auth_base}/");

    let target_user = insert_user(&db, "gateway-bcl-handler-target").await;
    let target_user_string = target_user.to_string();
    let app_id = Uuid::new_v4();
    // Per-app BCL (the only revoking path under RLS): register a route whose
    // `oauth_client_id` matches the logout_token `aud`, so the handler resolves
    // the per-app revoke scope and revokes THIS app's session for the subject.
    let app_name = format!("bcl-replay-{}", Uuid::new_v4().simple());
    let oauth_client_id = format!("oac_bclreplay_{}", Uuid::new_v4().simple());
    let sector = format!("https://{app_name}.zeroship.localhost");
    let session = create(
        &mut db,
        &NewSession {
            user_id: &target_user_string,
            app_id,
            email: Some("alice@zeroship.test"),
            name: Some("Alice"),
            avatar_url: None,
            email_verified: true,
            granted_scopes: &[],
            auth_time: None,
            amr: &[],
        },
    )
    .await
    .expect("create session");

    let jti = format!("jti-{}", Uuid::new_v4().simple());
    let token =
        sign_logout_token_with_aud(&key, &issuer, &oauth_client_id, &target_user_string, &jti);
    let state = build_handler_state_with_route(
        db_cfg.clone(),
        &auth_base,
        app_id,
        &app_name,
        &oauth_client_id,
        &sector,
        zeroship_core::auth::derive_pairwise_salt(b"bcl-replay-stash"),
    );
    let app = test::init_service(
        web::App::new()
            .state(state.clone())
            .service(
                web::resource("/oidc/backchannel-logout")
                    .route(web::post().to(backchannel_logout::handle)),
            ),
    )
    .await;

    let first = test::TestRequest::post()
        .uri("/oidc/backchannel-logout")
        .header("content-type", "application/x-www-form-urlencoded")
        .set_payload(format!("logout_token={token}"))
        .to_request();
    let first_resp = test::call_service(&app, first).await;
    assert_eq!(first_resp.status(), StatusCode::OK);
    assert!(
        validate(&mut db, session.id, app_id)
            .await
            .expect("validate after first logout")
            .is_none(),
        "first logout_token must revoke the session"
    );
    assert_eq!(
        audit_count(&db, &jti).await,
        1,
        "first logout_token must emit one revocation audit row"
    );
    // The per-app BCL's REAL effects are the session revocation asserted above
    // (the `validate(...) is_none()` check) plus the `(client_id, pws_)`
    // token-family marker (covered by `per_app_bcl_writes_token_family_marker`).
    // Here we focus on replay-idempotency of the audit row.

    let replay = test::TestRequest::post()
        .uri("/oidc/backchannel-logout")
        .header("content-type", "application/x-www-form-urlencoded")
        .set_payload(format!("logout_token={token}"))
        .to_request();
    let replay_resp = test::call_service(&app, replay).await;
    assert_eq!(
        replay_resp.status(),
        StatusCode::OK,
        "replayed logout_token remains idempotent at the HTTP layer"
    );
    assert_eq!(
        audit_count(&db, &jti).await,
        1,
        "replayed logout_token must not emit duplicate revocation audit"
    );
    assert_eq!(state.logout_jti_cache.len(), 1);

    db.execute(
        "DELETE FROM zeroship.audit_events WHERE detail->>'jti' = $1",
        &[&jti],
    )
    .await
    .ok();
    db.execute("DELETE FROM zeroship.gateway_sessions WHERE id = $1", &[&session.id])
        .await
        .ok();
    db.execute("DELETE FROM zeroship.users WHERE id = $1", &[&target_user])
        .await
        .ok();
}

// ─── Batch A fix 4: per-app BCL writes the token-family marker ──────────────

/// Like [`sign_logout_token`] but with a caller-chosen `aud` (the per-app
/// `oac_…` client) so the handler's per-app BCL branch fires.
fn sign_logout_token_with_aud(
    key: &TestKey,
    issuer: &str,
    aud: &str,
    sub: &str,
    jti: &str,
) -> String {
    let now = i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs(),
    )
    .unwrap();
    let mut header = Header::new(Algorithm::EdDSA);
    header.kid = Some(key.kid.clone());
    let claims = json!({
        "iss": issuer,
        "aud": aud,
        "iat": now,
        "jti": jti,
        "events": { zeroship_core::logout_token::BCL_EVENT: {} },
        "sub": sub,
        "sid": format!("sid-{jti}"),
    });
    encode(&header, &claims, &key.encoding).expect("encode logout_token")
}

/// Build a handler `GateState` whose RouteCache is provisioned with ONE
/// per-app route `(name, oauth_client_id, sector)` + a non-zero pairwise salt,
/// so a logout_token with `aud == oauth_client_id` routes to the per-app BCL
/// branch and derives the same `pws_` the test asserts on.
fn build_handler_state_with_route(
    db: DbConfig,
    auth_base: &str,
    app_id: Uuid,
    name: &str,
    oauth_client_id: &str,
    sector: &str,
    pairwise_salt: [u8; 32],
) -> Arc<GateState> {
    let state = build_handler_state(db, auth_base);
    let mut map: std::collections::HashMap<Uuid, zeroship_core::types::RouteEntry> =
        std::collections::HashMap::new();
    // Register the route under the SAME stable app UUID the seeded
    // gateway_sessions row uses — the per-app BCL handler resolves the revoke
    // scope to this id (via lookup_by_oauth_client_id), so a fresh random id
    // here would never match the seeded session.
    map.insert(
        app_id,
        zeroship_core::types::RouteEntry {
            name: name.to_string(),
            plan_id: "free".to_string(),
            api_key_hash: "h".to_string(),
            deploy_hash: None,
            manifest: zeroship_bundle::Manifest::passthrough(),
            oauth_client_id: Some(oauth_client_id.to_string()),
            sector_identifier: Some(sector.to_string()),
            spend_state: zeroship_core::types::SpendState::Allow,
            account_state: zeroship_core::types::AccountState::Active,
        },
    );
    state.routes.update(map, &state.rate_limiters, &state.concurrency);
    // `build_handler_state` hard-codes an all-zero salt; rebuild with ours.
    // GateState is in an Arc with refcount 1 here, so get_mut succeeds.
    let mut state = state;
    Arc::get_mut(&mut state)
        .expect("unique state Arc")
        .pairwise_salt = pairwise_salt;
    state
}

/// A per-app back-channel logout (logout_token `aud` = a per-app `oac_…`
/// client) must write the PER-APP token-family marker keyed on `(client_id,
/// pws_)` (Batch A fix 4), so the user's live wrapper / raw-Hydra access token
/// for THAT app is rejected on the next request — not just their gateway
/// sessions. We POST a real signed logout_token through the handler and assert
/// the REAL gateway reader (`is_family_revoked_since`) — keyed exactly as the
/// auth arms key it — reports a still-live token as revoked. PG-gated.
#[ntex::test]
async fn per_app_bcl_writes_token_family_marker() {
    let Ok(dsn) = std::env::var("AUTH_DB_URL") else {
        eprintln!("skipping (no AUTH_DB_URL)");
        return;
    };
    let (client, connection) = connect(&dsn, NoTls).await.expect("connect");
    compio::runtime::spawn(async move {
        let _ = connection.run().await;
    })
    .detach();
    let mut db = client;
    let db_cfg = DbConfig::new(dsn.clone(), 4);

    let key = make_key();
    let jwks_key = Arc::new(key.clone());
    let jwks_server = test::server(move || {
        let jwks_key = jwks_key.clone();
        async move {
            web::App::new()
                .state(jwks_key)
                .service(web::resource("/.well-known/jwks.json").route(web::get().to(jwks)))
        }
    })
    .await;
    let auth_base = jwks_server.url("").trim_end_matches('/').to_string();
    let issuer = format!("{auth_base}/");

    let target_user = insert_user(&db, "gateway-bcl-perapp").await;
    let target_user_string = target_user.to_string();
    let app_id = Uuid::new_v4();
    let app_name = format!("perapp-{}", Uuid::new_v4().simple());
    let oauth_client_id = format!("oac_perappbcl_{}", Uuid::new_v4().simple());
    let sector = format!("https://{app_name}.zeroship.localhost");
    // A gateway session so the per-app `revoke_app_sessions_for_user` has a row.
    // Keyed by the app's stable UUID — the SAME id the route is registered
    // under below, so the handler's UUID-keyed per-app revoke matches it.
    create(
        &mut db,
        &NewSession {
            user_id: &target_user_string,
            app_id,
            email: Some("alice@zeroship.test"),
            name: Some("Alice"),
            avatar_url: None,
            email_verified: true,
            granted_scopes: &[],
            auth_time: None,
            amr: &[],
        },
    )
    .await
    .expect("create session");

    let pairwise_salt = zeroship_core::auth::derive_pairwise_salt(b"bcl-perapp-stash");
    let pws = zeroship_core::auth::derive_pairwise(&pairwise_salt, &target_user_string, &sector);
    let live_token_iat = i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs(),
    )
    .unwrap()
        - 60;

    // Pre: no marker ⇒ the live token is NOT family-revoked.
    assert!(
        !zeroship_core::wrapper_revocation::is_family_revoked_since(
            &db,
            &oauth_client_id,
            &pws,
            live_token_iat,
        )
        .await
        .expect("pre-BCL family check"),
        "before BCL the live token must NOT be family-revoked"
    );

    let jti = format!("jti-{}", Uuid::new_v4().simple());
    let token =
        sign_logout_token_with_aud(&key, &issuer, &oauth_client_id, &target_user_string, &jti);
    let state = build_handler_state_with_route(
        db_cfg,
        &auth_base,
        app_id,
        &app_name,
        &oauth_client_id,
        &sector,
        pairwise_salt,
    );
    let app = test::init_service(web::App::new().state(state).service(
        web::resource("/oidc/backchannel-logout").route(web::post().to(backchannel_logout::handle)),
    ))
    .await;

    let req = test::TestRequest::post()
        .uri("/oidc/backchannel-logout")
        .header("content-type", "application/x-www-form-urlencoded")
        .set_payload(format!("logout_token={token}"))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::OK);

    // The per-app branch wrote the family marker → the live token is rejected
    // by the EXACT reader the auth arms run.
    assert!(
        zeroship_core::wrapper_revocation::is_family_revoked_since(
            &db,
            &oauth_client_id,
            &pws,
            live_token_iat,
        )
        .await
        .expect("post-BCL family check"),
        "per-app BCL must write the (client_id, pws_) marker so the live token is rejected"
    );

    // Cleanup.
    db.execute(
        "DELETE FROM zeroship.token_revocations WHERE client_id = $1",
        &[&oauth_client_id],
    )
    .await
    .ok();
    db.execute(
        "DELETE FROM zeroship.audit_events WHERE detail->>'jti' = $1",
        &[&jti],
    )
    .await
    .ok();
    db.execute(
        "DELETE FROM zeroship.gateway_sessions WHERE user_id = $1",
        &[&target_user_string],
    )
    .await
    .ok();
    db.execute("DELETE FROM zeroship.users WHERE id = $1", &[&target_user])
        .await
        .ok();
}

/// Batch A M1 regression — cross-arm `pws_` derivation must be invariant to the
/// inbound Hydra `sub` SPELLING. The per-app BCL writer derives the `pws_` from
/// the logout_token's `sub` (which Hydra controls), while a reader / the
/// `/signout`-style writer derive it from the canonical `Uuid::to_string()`. If
/// Hydra ever emits a NON-canonical sub (here: UPPERCASE), the writer's `pws_`
/// must STILL equal the canonical reader's `pws_`, or the BCL silently fails to
/// revoke the live token. We sign the logout_token with the user's uppercase
/// UUID, run the REAL handler (real writer), and assert the reader keyed on the
/// CANONICAL `pws_` reports the live token revoked. PG-gated.
#[ntex::test]
async fn per_app_bcl_marker_is_invariant_to_non_canonical_sub_spelling() {
    let Ok(dsn) = std::env::var("AUTH_DB_URL") else {
        eprintln!("skipping (no AUTH_DB_URL)");
        return;
    };
    let (client, connection) = connect(&dsn, NoTls).await.expect("connect");
    compio::runtime::spawn(async move {
        let _ = connection.run().await;
    })
    .detach();
    let mut db = client;
    let db_cfg = DbConfig::new(dsn.clone(), 4);

    let key = make_key();
    let jwks_key = Arc::new(key.clone());
    let jwks_server = test::server(move || {
        let jwks_key = jwks_key.clone();
        async move {
            web::App::new()
                .state(jwks_key)
                .service(web::resource("/.well-known/jwks.json").route(web::get().to(jwks)))
        }
    })
    .await;
    let auth_base = jwks_server.url("").trim_end_matches('/').to_string();
    let issuer = format!("{auth_base}/");

    let target_user = insert_user(&db, "gateway-bcl-noncanon").await;
    let canonical_sub = target_user.to_string(); // hyphenated lowercase
    // The NON-canonical spelling Hydra could put in the logout_token `sub`.
    let uppercase_sub = canonical_sub.to_uppercase();
    assert_ne!(
        canonical_sub, uppercase_sub,
        "fixture must actually exercise a spelling difference"
    );
    let app_id = Uuid::new_v4();
    let app_name = format!("noncanon-{}", Uuid::new_v4().simple());
    let oauth_client_id = format!("oac_noncanonbcl_{}", Uuid::new_v4().simple());
    let sector = format!("https://{app_name}.zeroship.localhost");
    create(
        &mut db,
        &NewSession {
            user_id: &canonical_sub,
            app_id,
            email: Some("alice@zeroship.test"),
            name: Some("Alice"),
            avatar_url: None,
            email_verified: true,
            granted_scopes: &[],
            auth_time: None,
            amr: &[],
        },
    )
    .await
    .expect("create session");

    let pairwise_salt = zeroship_core::auth::derive_pairwise_salt(b"bcl-noncanon-stash");
    // The reader / canonical writer derive the `pws_` from the CANONICAL UUID.
    let pws_canonical =
        zeroship_core::auth::derive_pairwise(&pairwise_salt, &canonical_sub, &sector);
    let live_token_iat = i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs(),
    )
    .unwrap()
        - 60;

    assert!(
        !zeroship_core::wrapper_revocation::is_family_revoked_since(
            &db,
            &oauth_client_id,
            &pws_canonical,
            live_token_iat,
        )
        .await
        .expect("pre-BCL family check"),
        "before BCL the live token must NOT be family-revoked"
    );

    // Sign the logout_token with the UPPERCASE (non-canonical) sub.
    let jti = format!("jti-{}", Uuid::new_v4().simple());
    let token = sign_logout_token_with_aud(&key, &issuer, &oauth_client_id, &uppercase_sub, &jti);
    let state = build_handler_state_with_route(
        db_cfg,
        &auth_base,
        app_id,
        &app_name,
        &oauth_client_id,
        &sector,
        pairwise_salt,
    );
    let app = test::init_service(web::App::new().state(state).service(
        web::resource("/oidc/backchannel-logout").route(web::post().to(backchannel_logout::handle)),
    ))
    .await;
    let req = test::TestRequest::post()
        .uri("/oidc/backchannel-logout")
        .header("content-type", "application/x-www-form-urlencoded")
        .set_payload(format!("logout_token={token}"))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::OK);

    // The writer derived from the UPPERCASE sub; the reader keys on the
    // CANONICAL `pws_`. With M1 canonicalization the two are byte-identical, so
    // the live token is rejected. Pre-fix this assertion would FAIL.
    assert!(
        zeroship_core::wrapper_revocation::is_family_revoked_since(
            &db,
            &oauth_client_id,
            &pws_canonical,
            live_token_iat,
        )
        .await
        .expect("post-BCL family check"),
        "BCL marker derived from a non-canonical sub must still match the \
         canonical reader's pws_ (M1)"
    );

    db.execute(
        "DELETE FROM zeroship.token_revocations WHERE client_id = $1",
        &[&oauth_client_id],
    )
    .await
    .ok();
    db.execute(
        "DELETE FROM zeroship.audit_events WHERE detail->>'jti' = $1",
        &[&jti],
    )
    .await
    .ok();
    db.execute(
        "DELETE FROM zeroship.gateway_sessions WHERE user_id = $1",
        &[&canonical_sub],
    )
    .await
    .ok();
    db.execute("DELETE FROM zeroship.users WHERE id = $1", &[&target_user])
        .await
        .ok();
}

// ─── M1 regression: per-app BCL must DELETE the reload-recovery anchor ───────

/// M1 (MEDIUM) regression — a per-app back-channel logout ("sign out
/// everywhere" for THIS app) must durably terminate the 30-day SDK
/// reload-recovery anchor, not just the gateway session + the family marker.
///
/// Before the fix the per-app BCL branch wrote the `(client_id, pws_)` family
/// marker and deleted `gateway_sessions`, but NEVER deleted the
/// `app_session_anchors` row. The family marker rejects only tokens whose
/// `iat < revoked_after`, so a `GET /__zeroship/auth/session?mint=1` could read
/// the surviving anchor (`anchors::read_live` ignores the marker), run a Hydra
/// refresh, and re-mint a fresh cookie whose `iat` post-dates the marker —
/// silently resurrecting the session the user believed they killed.
///
/// We seed a live anchor for the subject + app, POST a real signed per-app
/// logout_token through the REAL handler, and assert the anchor is no longer
/// live (`read_live` → None) — i.e. the `?mint=1` resurrection path has nothing
/// to read. Pre-fix this assertion FAILS (the anchor survives). PG-gated.
#[ntex::test]
async fn per_app_bcl_deletes_reload_recovery_anchor() {
    let Ok(dsn) = std::env::var("AUTH_DB_URL") else {
        eprintln!("skipping (no AUTH_DB_URL)");
        return;
    };
    let (client, connection) = connect(&dsn, NoTls).await.expect("connect");
    compio::runtime::spawn(async move {
        let _ = connection.run().await;
    })
    .detach();
    let mut db = client;
    let db_cfg = DbConfig::new(dsn.clone(), 4);

    let key = make_key();
    let jwks_key = Arc::new(key.clone());
    let jwks_server = test::server(move || {
        let jwks_key = jwks_key.clone();
        async move {
            web::App::new()
                .state(jwks_key)
                .service(web::resource("/.well-known/jwks.json").route(web::get().to(jwks)))
        }
    })
    .await;
    let auth_base = jwks_server.url("").trim_end_matches('/').to_string();
    let issuer = format!("{auth_base}/");

    let target_user = insert_user(&db, "gateway-bcl-anchor").await;
    let target_user_string = target_user.to_string();
    let app_id = Uuid::new_v4();
    let app_name = format!("anchorbcl-{}", Uuid::new_v4().simple());
    let oauth_client_id = format!("oac_anchorbcl_{}", Uuid::new_v4().simple());
    let sector = format!("https://{app_name}.zeroship.localhost");

    // The anchor + gateway_session rows FK to apps(id)/oauth_clients(client_id):
    // seed both real rows so the inserts succeed under the live schema.
    seed_app(&db, app_id, &app_name).await;
    seed_oauth_client(&db, &oauth_client_id).await;

    create(
        &mut db,
        &NewSession {
            user_id: &target_user_string,
            app_id,
            email: Some("alice@zeroship.test"),
            name: Some("Alice"),
            avatar_url: None,
            email_verified: true,
            granted_scopes: &[],
            auth_time: None,
            amr: &[],
        },
    )
    .await
    .expect("create session");

    // The 30-day reload-recovery anchor that `?mint=1` would resurrect from.
    let anchor_id = seed_anchor(&db, app_id, &oauth_client_id, target_user).await;

    // Pre: the anchor reads LIVE (this is exactly what `?mint=1` reads).
    assert!(
        anchors::read_live(&mut db, app_id, anchor_id)
            .await
            .expect("pre-BCL anchor read")
            .is_some(),
        "the seeded anchor must be live before the BCL"
    );

    let pairwise_salt = zeroship_core::auth::derive_pairwise_salt(b"bcl-anchor-stash");
    let jti = format!("jti-{}", Uuid::new_v4().simple());
    let token =
        sign_logout_token_with_aud(&key, &issuer, &oauth_client_id, &target_user_string, &jti);
    let state = build_handler_state_with_route(
        db_cfg.clone(),
        &auth_base,
        app_id,
        &app_name,
        &oauth_client_id,
        &sector,
        pairwise_salt,
    );
    let app = test::init_service(web::App::new().state(state).service(
        web::resource("/oidc/backchannel-logout").route(web::post().to(backchannel_logout::handle)),
    ))
    .await;

    let req = test::TestRequest::post()
        .uri("/oidc/backchannel-logout")
        .header("content-type", "application/x-www-form-urlencoded")
        .set_payload(format!("logout_token={token}"))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::OK);

    // THE M1 ASSERTION: the per-app BCL must have deleted the anchor, so the
    // `?mint=1` resurrection path (`read_live`) now finds NOTHING. Pre-fix the
    // anchor survives and this fails — "sign out everywhere" stays resurrectable.
    {
        let pool = db::checkout(&db_cfg).await.expect("checkout");
        let mut conn = pool.get().await.expect("conn");
        assert!(
            anchors::read_live(&mut conn, app_id, anchor_id)
                .await
                .expect("post-BCL anchor read")
                .is_none(),
            "per-app BCL must delete the reload-recovery anchor so ?mint=1 cannot \
             resurrect the session (M1)"
        );
    }

    // Cleanup.
    db.execute(
        "DELETE FROM zeroship.token_revocations WHERE client_id = $1",
        &[&oauth_client_id],
    )
    .await
    .ok();
    db.execute(
        "DELETE FROM zeroship.audit_events WHERE detail->>'jti' = $1",
        &[&jti],
    )
    .await
    .ok();
    db.execute(
        "DELETE FROM zeroship.app_session_anchors WHERE app_id = $1",
        &[&app_id],
    )
    .await
    .ok();
    db.execute(
        "DELETE FROM zeroship.gateway_sessions WHERE user_id = $1",
        &[&target_user_string],
    )
    .await
    .ok();
    db.execute("DELETE FROM zeroship.users WHERE id = $1", &[&target_user])
        .await
        .ok();
    db.execute(
        "DELETE FROM zeroship.oauth_clients WHERE client_id = $1",
        &[&oauth_client_id],
    )
    .await
    .ok();
    db.execute("DELETE FROM zeroship.apps WHERE id = $1", &[&app_id])
        .await
        .ok();
}
