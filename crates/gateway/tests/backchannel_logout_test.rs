//! Live-PG smoke test for `gateway::sessions::revoke_all_for_user` —
//! the revoke-side of the OIDC Back-Channel Logout 1.0 handler (Phase
//! 7 U1.2).
//!
//! Skipped silently when `AUTH_DB_URL` is unset (same convention as
//! the rest of the gateway PG smoke tests, e.g. `sessions_test.rs`).
//!
//! Coverage:
//!   - Seed two live sessions for the same user_id (different app_ids)
//!     and one session for a different user_id.
//!   - Call `revoke_all_for_user`. Returned count must equal 2 (the
//!     two same-user rows).
//!   - Subsequent `validate(...)` on the revoked rows must return None.
//!   - The unrelated user's session must still validate.
//!   - Calling `revoke_all_for_user` again returns 0 (idempotent — the
//!     `revoked_at IS NULL` filter skips already-revoked rows).
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
    backchannel_logout,
    blob_cache::{BlobCache, DiskBlobCache},
    db::DbConfig,
    enforce, idempotency,
    oidc_rp::OidcRp,
    proxy::HashRing,
    sessions::{create, revoke_all_for_user, validate, NewSession},
    sync::RouteCache,
    GateConfig, GateState,
};

#[compio::test]
async fn revoke_all_for_user_revokes_only_the_target_user() {
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


    // Two sessions for the same user at two different apps — the BCL
    // handler revokes ACROSS apps for the same sub.
    let target_user_id = insert_user(&client, "gateway-bcl-target").await;
    let target_user = target_user_id.to_string();
    let app_a = format!("app-a-{}", Uuid::new_v4().simple());
    let app_b = format!("app-b-{}", Uuid::new_v4().simple());

    let s_a = create(
        &client,
        &NewSession {
            user_id: &target_user,
            app_id: &app_a,
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
        &client,
        &NewSession {
            user_id: &target_user,
            app_id: &app_b,
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

    // One session for an unrelated user — must NOT be touched.
    let other_user_id = insert_user(&client, "gateway-bcl-other").await;
    let other_user = other_user_id.to_string();
    let s_other = create(
        &client,
        &NewSession {
            user_id: &other_user,
            app_id: &app_a,
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
    assert!(validate(&client, s_a.id, &app_a)
        .await
        .expect("pre validate s_a")
        .is_some());
    assert!(validate(&client, s_b.id, &app_b)
        .await
        .expect("pre validate s_b")
        .is_some());
    assert!(validate(&client, s_other.id, &app_a)
        .await
        .expect("pre validate s_other")
        .is_some());

    // Revoke everything for the target user.
    let count = revoke_all_for_user(&client, &target_user)
        .await
        .expect("revoke_all_for_user");
    assert_eq!(count, 2, "expected 2 sessions revoked, got {count}");

    // Both target sessions must now fail validation.
    assert!(
        validate(&client, s_a.id, &app_a)
            .await
            .expect("post validate s_a")
            .is_none(),
        "s_a must be revoked"
    );
    assert!(
        validate(&client, s_b.id, &app_b)
            .await
            .expect("post validate s_b")
            .is_none(),
        "s_b must be revoked"
    );

    // The unrelated user's session must still validate.
    assert!(
        validate(&client, s_other.id, &app_a)
            .await
            .expect("post validate s_other")
            .is_some(),
        "unrelated user's session must NOT be revoked"
    );

    // Idempotent — running revoke_all_for_user again touches no rows.
    let again = revoke_all_for_user(&client, &target_user)
        .await
        .expect("revoke_all_for_user idempotent");
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
        dpop_jti_cache: Arc::new(zeroship_core::dpop::TieredJtiCache::default()),
        logout_jti_cache: Arc::new(zeroship_core::logout_token::LogoutJtiCache::default()),
        revocation_cache: Arc::new(zeroship_core::wrapper_revocation::RevocationCache::new()),
        signing_key: None,
        prev_signing_key: None,
        session_issuer: None::<Arc<zeroship_gateway::session_token::Issuer>>,
        session_verifier: None::<Arc<zeroship_gateway::session_token::Verifier>>,
        anchor_enc_key: [0u8; 32],
            pairwise_salt: [0u8; 32],
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
    // helpers (all `&Client`); `db_cfg` backs the handler's `GateState`,
    // which now holds a `DbConfig` (the handler builds its own per-thread
    // pool from it). Both point at the same rows.
    let db = Arc::new(client);
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
    let app_id = format!("app-bcl-{}", Uuid::new_v4().simple());
    let session = create(
        &db,
        &NewSession {
            user_id: &target_user_string,
            app_id: &app_id,
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
    let token = sign_logout_token(&key, &issuer, &target_user_string, &jti);
    let state = build_handler_state(db_cfg.clone(), &auth_base);
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
        validate(&db, session.id, &app_id)
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
    // Batch A M2: the shared-`gateway`-client BCL no longer writes any
    // wrapper-token denylist — its REAL effect is the session revocation
    // asserted above (the `validate(...) is_none()` check). Wrapper / raw-Hydra
    // access tokens are per-app `pws_`-keyed and were never owned by the shared
    // client, so there is nothing else to revoke here.

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
    name: &str,
    oauth_client_id: &str,
    sector: &str,
    pairwise_salt: [u8; 32],
) -> Arc<GateState> {
    let state = build_handler_state(db, auth_base);
    let mut map: std::collections::HashMap<Uuid, zeroship_core::types::RouteEntry> =
        std::collections::HashMap::new();
    map.insert(
        Uuid::new_v4(),
        zeroship_core::types::RouteEntry {
            name: name.to_string(),
            plan_id: "free".to_string(),
            api_key_hash: "h".to_string(),
            deploy_hash: None,
            manifest: zeroship_bundle::Manifest::passthrough(),
            oauth_client_id: Some(oauth_client_id.to_string()),
            sector_identifier: Some(sector.to_string()),
        },
    );
    state.routes.update(map);
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
    let db = Arc::new(client);
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
    let app_name = format!("perapp-{}", Uuid::new_v4().simple());
    let oauth_client_id = format!("oac_perappbcl_{}", Uuid::new_v4().simple());
    let sector = format!("https://{app_name}.zeroship.localhost");
    // A gateway session so the per-app `revoke_app_sessions_for_user` has a row.
    create(
        &db,
        &NewSession {
            user_id: &target_user_string,
            app_id: &app_name,
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
            db.as_ref(),
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
            db.as_ref(),
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
    let db = Arc::new(client);
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
    let app_name = format!("noncanon-{}", Uuid::new_v4().simple());
    let oauth_client_id = format!("oac_noncanonbcl_{}", Uuid::new_v4().simple());
    let sector = format!("https://{app_name}.zeroship.localhost");
    create(
        &db,
        &NewSession {
            user_id: &canonical_sub,
            app_id: &app_name,
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
            db.as_ref(),
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
            db.as_ref(),
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
