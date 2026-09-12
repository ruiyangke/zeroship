//! Gateway OIDC RP end-to-end test against the platform OP.
//!
//! This is the P5a-3b-ii shape: no OP admin/client registration. The test
//! seeds a per-app brokered `oac_` client directly in `zeroship.oauth_clients`,
//! boots `crates/auth` in-process with a broker master secret, then drives the
//! native OP `/oauth2/authorize` + `/login` flow. `OidcRp::finish_callback`
//! exchanges the code as that per-app client by deriving the broker secret from the same
//! master and verifies the OP id_token (`iss` with the fixed `/oauth2` prefix,
//! `aud = oac_...`).

mod common;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use compio_postgres::{Client, NoTls};
use ed25519_dalek::{pkcs8::EncodePrivateKey, SigningKey};
use ntex::web::{self, test};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use zeroship_auth::config::AuthConfig;
use zeroship_auth::headers::SecurityHeaders;
use zeroship_auth::identity::password;
use zeroship_auth::oidc::{BrokerSecrets, Issuer};
use zeroship_auth::server;
use zeroship_bundle::{AssetEntry, Manifest, RequiredPrincipal, ResourceEntry, StaticAction};
use zeroship_gateway::blob_cache::{BlobCache, DiskBlobCache};
use zeroship_gateway::enforce;
use zeroship_gateway::idempotency;
use zeroship_gateway::oidc_rp::{BrokerSecret, BrowserAuthorizeParams, OidcRp, TokenSet};
use zeroship_gateway::proxy::HashRing;
use zeroship_gateway::sessions::{create, revoke_app_sessions_for_user, NewSession};
use zeroship_gateway::sync::RouteCache;
use zeroship_gateway::{session_token, GateConfig, GateState};

/// The test's own oracle for "is this audit row still live", replacing the
/// crate's deleted `sessions::validate`. That function had no production
/// caller: revocation is enforced by the per-app family marker the request path
/// reads, never by reading this table. A pure SELECT, because nothing slides
/// `idle_expires_at` any more.
async fn live_session(
    client: &compio_postgres::Client,
    id: Uuid,
    app_id: Uuid,
) -> Option<compio_postgres::Row> {
    client
        .query(
            "SELECT id, user_id, app_id, granted_scopes \
             FROM zeroship.gateway_sessions \
             WHERE id = $1 \
               AND app_id = $2 \
               AND revoked_at IS NULL \
               AND idle_expires_at > NOW() \
               AND abs_expires_at > NOW()",
            &[&id, &app_id],
        )
        .await
        .expect("read gateway_sessions row")
        .into_iter()
        .next()
}

const ISSUER: &str = "https://auth.zeroship.ai/oauth2";
const REDIRECT_URI: &str = "http://127.0.0.1:9999/__zeroship/auth/callback";
const SECTOR: &str = "https://gateway-e2e.zeroship.test";
const APP_HOST: &str = "gateway-e2e.zeroship.test";
const APP_NAME: &str = "gateway-e2e";
const PASSWORD: &str = "gateway-test-password-with-enough-bytes-1234";
const BROKER_MASTER: &[u8] = b"gateway-oidc-rp-e2e-broker-master-32-bytes";
const GATEWAY_ISS: &str = "https://api.zeroship.ai";

/// ONE gateway identity for this whole test binary.
///
/// The fake worker below must verify the identity envelope under the PUBLIC
/// half of the key the gateway signs with, which is the production
/// relationship. A per-call `test_gateway_service_auth()` would give the state
/// and the worker different keys and the worker would refuse every request -
/// so the fixture holds one and hands out both halves.
fn gateway_identity() -> &'static std::sync::Arc<zeroship_core::service_peers::ServiceAuth> {
    static IDENTITY: std::sync::OnceLock<
        std::sync::Arc<zeroship_core::service_peers::ServiceAuth>,
    > = std::sync::OnceLock::new();
    IDENTITY.get_or_init(|| std::sync::Arc::new(zeroship_gateway::test_gateway_service_auth()))
}

fn db_url() -> String {
    common::require_platform_db()
}

fn location(resp: &cyper::Response) -> String {
    resp.headers()
        .get(http::header::LOCATION)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("")
        .to_string()
}

fn read_set_cookie(resp: &cyper::Response, name: &str) -> Option<String> {
    for hv in resp.headers().get_all(http::header::SET_COOKIE) {
        let Ok(s) = hv.to_str() else { continue };
        let first = s.split(';').next().unwrap_or("");
        if let Some((n, v)) = first.split_once('=') {
            if n.trim() == name {
                return Some(v.trim().to_string());
            }
        }
    }
    None
}

fn query_param(raw_url: &str, key: &str) -> Option<String> {
    let parsed = url::Url::parse(raw_url).ok()?;
    parsed
        .query_pairs()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.into_owned())
}

fn relative_query_param(path: &str, key: &str) -> Option<String> {
    let url = url::Url::parse(&format!("http://auth.test{path}")).ok()?;
    url.query_pairs()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.into_owned())
}

fn form(pairs: &[(&str, &str)]) -> String {
    let mut ser = url::form_urlencoded::Serializer::new(String::new());
    for (k, v) in pairs {
        ser.append_pair(k, v);
    }
    ser.finish()
}

fn write_secret_file(path: &std::path::Path, bytes: &[u8]) {
    std::fs::write(path, bytes).expect("write secret file");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .expect("secret file permissions");
    }
}

#[derive(Debug, Clone)]
struct MemoryBlobStore {
    hash: String,
    body: bytes::Bytes,
}

#[async_trait::async_trait(?Send)]
impl zeroship_bundle::BlobStore for MemoryBlobStore {
    async fn get_blob(&self, h: &str) -> Result<bytes::Bytes, zeroship_bundle::BlobError> {
        if h == self.hash {
            Ok(self.body.clone())
        } else {
            Err(zeroship_bundle::BlobError::NotFound(h.to_string()))
        }
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

    async fn has_blob(&self, h: &str) -> Result<bool, zeroship_bundle::BlobError> {
        Ok(h == self.hash)
    }

    async fn probe(&self) -> Result<(), zeroship_bundle::BlobError> {
        Ok(())
    }

    async fn get_blob_to_file(
        &self,
        h: &str,
        _out: &compio::fs::File,
        _expected_size: Option<u64>,
        _max_bytes: u64,
    ) -> Result<u64, zeroship_bundle::BlobError> {
        Err(zeroship_bundle::BlobError::NotFound(h.to_string()))
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

    async fn delete_manifest(
        &self,
        _a: &Uuid,
        _d: &str,
    ) -> Result<bool, zeroship_bundle::BlobError> {
        Ok(false)
    }

    async fn delete_app_manifests(&self, _a: &Uuid) -> Result<(), zeroship_bundle::BlobError> {
        Ok(())
    }
}

fn gateway_signing() -> SigningKey {
    SigningKey::from_bytes(&[7u8; 32])
}

/// The OP signing key this test PROCESS publishes, and no other.
///
/// `zeroship.signing_keys` allows one `active` OP key per DATABASE:
/// `publish_active_key` retires every other active row and refuses to
/// reactivate a retired one. The fixed `[42u8; 32]` this replaces gave every
/// run the same kid, so two runs sharing a suite database retired each
/// other. MEASURED 2026-08-20, two auth suites on one database: 3 failures in
/// each run, all `publish active OP key: ... has non-activatable status
/// "retiring"` naming one kid present in both logs.
///
/// `gateway_signing` above is deliberately left fixed: it is the RP-side key
/// and never reaches `zeroship.signing_keys`, so it is not a singleton and
/// two runs holding it collide over nothing.
/// Publish the OP key of THIS PROCESS once, however many tests ask.
///
/// A per-process kid is not enough on its own. Each test here publishes, and a
/// peer run retires our row between calls, so the second REPUBLISH of our own
/// kid fails with `is terminally retired and cannot be reactivated`. MEASURED
/// 2026-08-20 with per-process keys but per-test publishes, two runs against
/// one database: 3 passed in one and 1 passed / 2 failed in the other.
///
/// Publishing once removes the only operation that can fail. The row stays
/// usable after a peer retires it, because the JWKS keeps `retiring` keys
/// (`crates/zeroship-auth/src/oidc/metadata.rs:71-84`) and every assertion here looks
/// its key up by kid rather than counting them.
/// LOAD-THEN-STORE, NOT `swap`: the flag records that a publish SUCCEEDED, not
/// that one was attempted. With `swap` the flag is already set when the
/// `expect` below panics, so the first test reports the real failure and every
/// later test in the process skips the publish and fails downstream on a key
/// that was never registered. The race `swap` bought is not worth having -
/// republishing our own still-ACTIVE kid succeeds, while skipping the publish
/// cannot be recovered from.
async fn publish_op_key_once(issuer: &Issuer, db: &compio_postgres::Client) {
    use std::sync::atomic::{AtomicBool, Ordering};
    static PUBLISHED: AtomicBool = AtomicBool::new(false);
    if PUBLISHED.load(Ordering::SeqCst) {
        return;
    }
    issuer
        .publish_active_key(db)
        .await
        .expect("publish active OP key");
    PUBLISHED.store(true, Ordering::SeqCst);
}

fn op_signing() -> SigningKey {
    static SEED: std::sync::OnceLock<[u8; 32]> = std::sync::OnceLock::new();
    let seed = SEED.get_or_init(|| {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        zeroship_core::crypto::derive_key(&format!("gateway-op-{}-{nanos}", std::process::id()))
    });
    SigningKey::from_bytes(seed)
}

fn protected_static_manifest(body: bytes::Bytes) -> (Manifest, MemoryBlobStore) {
    let hash = hex::encode(Sha256::digest(&body));
    let mut assets = HashMap::new();
    assets.insert(
        "/private.txt".to_string(),
        AssetEntry {
            hash: hash.clone(),
            content_type: "text/plain; charset=utf-8".to_string(),
            size: body.len() as u64,
            cache: None,
            updated_at: 0,
            variants: HashMap::new(),
        },
    );

    let mut resources = HashMap::new();
    resources.insert(
        "/private".to_string(),
        ResourceEntry {
            auth: Some(RequiredPrincipal::User),
            r#static: Some(StaticAction {
                r#try: vec!["/private.txt".to_string()],
            }),
            ..Default::default()
        },
    );

    (
        Manifest {
            version: 1,
            assets,
            resources,
            ..Manifest::default()
        },
        MemoryBlobStore { hash, body },
    )
}

fn protected_worker_manifest() -> Manifest {
    let mut manifest = Manifest::passthrough();
    let root = manifest.resources.get_mut("*").expect("passthrough root");
    root.auth = Some(RequiredPrincipal::User);
    root.publicly_accessible = None;
    manifest
        .resources
        .insert("/private".to_string(), ResourceEntry::default());
    manifest
}

async fn echo_verified_user(req: web::HttpRequest) -> web::HttpResponse {
    // The transport credential is an ed25519 service assertion now, not a shared
    // string. This stand-in worker checks only that one was PRESENTED - the real
    // worker's verification of it is bound in `zeroship-worker`'s own suite -
    // because what this test is for is the IDENTITY envelope below, which is a
    // separate credential under a separate key.
    let presented = req
        .headers()
        .get(http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .is_some_and(|assertion| !assertion.is_empty());
    if !presented {
        return web::HttpResponse::Unauthorized().finish();
    }
    let Some(user_header) = req
        .headers()
        .get("zeroship-user")
        .and_then(|value| value.to_str().ok())
    else {
        return web::HttpResponse::Unauthorized().finish();
    };
    let Some(request_id) = req
        .headers()
        .get("x-request-id")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| Uuid::parse_str(value).ok())
    else {
        return web::HttpResponse::Unauthorized().finish();
    };
    // Verified under the GATEWAY's public half, which is the whole point: this
    // worker cannot produce this envelope, only check it.
    let Some(user_json) = gateway_identity()
        .user_envelope_signer()
        .expect("the test gateway signs")
        .own_verifier()
        .verify_for_request(user_header, request_id)
    else {
        return web::HttpResponse::Unauthorized().finish();
    };
    web::HttpResponse::Ok()
        .header(http::header::CONTENT_TYPE, "application/json")
        .body(user_json)
}

fn build_gateway_state(
    auth_base: &str,
    app_id: Uuid,
    client_id: &str,
    worker_url: Option<&str>,
    db: Option<zeroship_gateway::db::DbConfig>,
) -> Arc<GateState> {
    let body = bytes::Bytes::from_static(b"ok");
    let (static_manifest, blob_store) = protected_static_manifest(body);
    let manifest = if worker_url.is_some() {
        protected_worker_manifest()
    } else {
        static_manifest
    };
    let worker_urls = vec![worker_url.unwrap_or("http://0.0.0.0:0").to_string()];
    let mut tmp = std::env::temp_dir();
    tmp.push(format!("zsgate-oidc-e2e-{}", Uuid::new_v4().simple()));
    let disk_cache = DiskBlobCache::new(tmp, 1024 * 1024).expect("disk cache");

    let signing_key = gateway_signing();
    let session_issuer =
        session_token::Issuer::new(&signing_key, GATEWAY_ISS.to_string()).expect("session issuer");
    let session_verifier =
        session_token::Verifier::new(&signing_key.verifying_key(), GATEWAY_ISS.to_string());
    let oidc_rp = OidcRp::new(
        auth_base.to_string(),
        BrokerSecret::from_bytes(BROKER_MASTER.to_vec()).expect("gateway broker secret"),
        b"gateway-e2e-stash-signing-key-32-bytes!".to_vec(),
    )
    .with_issuer(ISSUER);

    let state = Arc::new(GateState {
        service_auth: std::sync::Arc::clone(gateway_identity()),
        config: GateConfig {
            control_url: String::new(),
            control_key: String::new(),
            worker_urls: worker_urls.clone(),
            poll_interval_secs: 5,
            auth_ui_url: auth_base.to_string(),
            origin_scheme: zeroship_core::config::OriginScheme::Https,
            trusted_origins: vec![],
            trust_proxy: false,
            public_url: GATEWAY_ISS.to_string(),
        },
        routes: RouteCache::new(),
        hash_ring: HashRing::new(worker_urls, 1),
        rate_limiters: enforce::RateLimitRegistry::new(100, 100),
        per_rule_rate_limits: enforce::PerRuleRateLimitRegistry::new(),
        concurrency: enforce::ConcurrencyRegistry::new(100),
        blob_store: Arc::new(blob_store),
        blob_cache: BlobCache::new(1024 * 1024),
        disk_cache,
        idempotency_store: Arc::new(idempotency::InMemoryIdempotencyStore::new()),
        oidc_rp: Arc::new(oidc_rp),
        db,
        logout_jti_cache: Arc::new(zeroship_core::logout_token::LogoutJtiCache::default()),
        revocation_cache: Arc::new(zeroship_authz::wrapper_revocation::RevocationCache::new()),
        signing_key: Some(Arc::new(signing_key)),
        prev_signing_key: None,
        session_issuer: Some(Arc::new(session_issuer)),
        session_verifier: Some(Arc::new(session_verifier)),
        anchor_enc_key: zeroship_core::crypto::derive_key("gateway-e2e-anchor-key"),
        // THE SAME SALT THE OP RUNS WITH (`Issuer::from_signing_key(.., [9u8; 32], ..)`).
        // It is one platform-wide secret in production (`AUTH_PAIRWISE_SALT_FILE`),
        // and the two sides must agree because both write `pws_` into
        // `zeroship.app_user_identities`: the OP at the code exchange, the gateway
        // when it signs the session cookie. Its upsert refuses to change an
        // existing binding, so a fixture with two salts fails the cookie mint with
        // `app_user_identities pairwise binding changed` (500). Measured here,
        // and it is real misconfiguration rather than a test artefact.
        pairwise_salt: [9u8; 32],
        meter: Arc::new(zeroship_metering::Meter::new()),
    });

    let mut routes = HashMap::new();
    routes.insert(
        app_id,
        zeroship_core::types::RouteEntry {
            name: APP_NAME.to_string(),
            plan_id: "free".to_string(),
            deploy_hash: None,
            manifest,
            oauth_client_id: Some(client_id.to_string()),
            sector_identifier: Some(SECTOR.to_string()),
            spend_state: zeroship_core::types::SpendState::Allow,
            account_state: zeroship_core::types::AccountState::Active,
        },
    );
    state.routes.update_snapshot(
        zeroship_core::types::GatewaySnapshot {
            routes,
            principal_lifecycle: Vec::new(),
            family_revocations: Vec::new(),
        },
        &state.rate_limiters,
        &state.concurrency,
    );

    state
}

async fn start_platform_op(
    db_url: &str,
    pg_client: Arc<Client>,
    issuer: Arc<Issuer>,
) -> test::TestServer {
    // The session keyring comes from `test_auth_config`, which is the ONE
    // place in this file that decides what a booted OP is configured with.
    let (cfg, _auth_secret_files) = test_auth_config(db_url);
    let cfg = Arc::new(cfg);
    let refresh_pool = zeroship_auth::oidc::refresh::RefreshSessionPool::new(db_url.to_string(), 4);

    test::server(move || {
        let cfg_state = cfg.clone();
        let db_state = pg_client.clone();
        let issuer_state = issuer.clone();
        let refresh_pool_state = refresh_pool.clone();
        async move {
            web::App::new()
                .state(cfg_state)
                .state(db_state)
                .state(issuer_state)
                .state(refresh_pool_state)
                .middleware(SecurityHeaders::default())
                .configure(server::configure(false, false))
        }
    })
    .await
}

async fn connect_test_db(db_url: &str) -> Arc<Client> {
    let (pg_client, pg_connection) = compio_postgres::connect(db_url, NoTls)
        .await
        .expect("connect pg");
    compio::runtime::spawn(async move {
        if let Err(e) = pg_connection.run().await {
            eprintln!("[oidc_rp_e2e] pg connection driver: {e}");
        }
    })
    .detach();
    Arc::new(pg_client)
}

/// What a driven login yields: the authorization code AND the `IdP` session
/// cookie value. The cookie is what authenticates the caller to `/me/sessions`,
/// so a test that both signs in to an app and then manages that sign-in from
/// the OP needs both halves of one login.
struct DrivenLogin {
    code: String,
    idp_session: String,
}

async fn drive_login_to_code(
    auth_base: &str,
    auth_url: &str,
    email: &str,
    redirect_uri: &str,
) -> DrivenLogin {
    let http = cyper::Client::new();

    let first = http
        .request(http::Method::GET, auth_url)
        .expect("build GET /authorize")
        .send()
        .await
        .expect("send GET /authorize");
    assert_eq!(first.status().as_u16(), 303);
    let login_loc = location(&first);
    assert!(
        login_loc.starts_with("/login?return_to="),
        "login redirect: {login_loc}"
    );
    let return_to = relative_query_param(&login_loc, "return_to").expect("return_to");

    let login_get = http
        .request(http::Method::GET, format!("{auth_base}{login_loc}"))
        .expect("build GET /login")
        .send()
        .await
        .expect("send GET /login");
    assert_eq!(login_get.status().as_u16(), 200);
    let csrf = read_set_cookie(&login_get, "__Host-zsidp_csrf").expect("login csrf");
    let login_body = form(&[
        ("csrf", csrf.as_str()),
        ("email", email),
        ("password", PASSWORD),
        ("return_to", return_to.as_str()),
    ]);
    let login_post = http
        .request(http::Method::POST, format!("{auth_base}/login"))
        .expect("build POST /login")
        .header("content-type", "application/x-www-form-urlencoded")
        .expect("content-type")
        .header("cookie", format!("__Host-zsidp_csrf={csrf}"))
        .expect("cookie")
        .body(login_body)
        .send()
        .await
        .expect("send POST /login");
    assert_eq!(login_post.status().as_u16(), 303);
    assert_eq!(location(&login_post), return_to);
    let session = read_set_cookie(&login_post, "__Host-zsidp_session").expect("session cookie");

    let final_authorize = http
        .request(http::Method::GET, format!("{auth_base}{return_to}"))
        .expect("build GET /authorize with session")
        .header("cookie", format!("__Host-zsidp_session={session}"))
        .expect("cookie")
        .send()
        .await
        .expect("send GET /authorize with session");
    assert_eq!(final_authorize.status().as_u16(), 303);
    let cb_url = location(&final_authorize);
    assert!(cb_url.starts_with(redirect_uri), "callback: {cb_url}");
    assert_eq!(query_param(&cb_url, "iss").as_deref(), Some(ISSUER));
    DrivenLogin {
        code: query_param(&cb_url, "code").expect("code param"),
        idp_session: session,
    }
}

async fn browser_pkce_tokens(
    rp: &OidcRp,
    auth_base: &str,
    client_id: &str,
    email: &str,
) -> TokenSet {
    let verifier = zeroship_core::pkce::generate_verifier();
    let challenge = zeroship_core::pkce::s256_challenge(&verifier);
    let state = format!("st-{}", Uuid::new_v4().simple());
    let nonce = format!("nonce-{}", Uuid::new_v4().simple());
    let params = BrowserAuthorizeParams {
        code_challenge: &challenge,
        state: &state,
        nonce: &nonce,
        scope: "openid offline_access email profile",
        redirect_uri: REDIRECT_URI,
        prompt: None,
        idp_hint: None,
    };
    let auth_url = rp.build_browser_authorize_url(client_id, &params);
    let login = drive_login_to_code(auth_base, &auth_url, email, REDIRECT_URI).await;
    rp.exchange_code_public(client_id, &login.code, &verifier, REDIRECT_URI)
        .await
        .expect("exchange code for token set")
}

/// Build an `AuthConfig` the way a real boot does.
///
/// The three secrets go through `-file` PATH flags because that is the only
/// shape auth accepts: `--db-url`, `--stash-signing-key` and `--totp-enc-key`
/// were value flags, and `crates/zeroship-auth/src/config.rs` now asserts clap REJECTS
/// all three. `parse_from` panics on an unknown argument, so this helper was a
/// hard failure waiting for the first run with a database - it is skipped
/// today only because `db_url()` returns `None` without a test database
/// (see `PG_TEST_URL`).
///
/// The tempdir is returned, not dropped here: deleting it before the caller is
/// done would be harmless for the already-resolved config but makes the
/// lifetime obvious rather than accidental.
///
/// The session-secret keyring is set HERE rather than by each caller. Both
/// callers used to write their own `refresh-hmac` / `refresh-idem` pair into
/// their own temp directory, which is the duplication that let the control
/// plane's fixture ship without one at all and answer every token exchange
/// with `refresh hash key is not configured`. `session_key_files` is the one
/// definition of that operation for the whole workspace, so a third fixture
/// added here inherits the keyring instead of having to remember it.
///
/// It is NOT written into the tempdir above: that directory is dropped with
/// the caller, while `session_key_files` is memoised for the process, and
/// `SessionSecretKeys::from_files` re-reads the paths on every exchange.
fn test_auth_config(db_url: &str) -> (AuthConfig, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("temp dir for auth secrets");
    // `write_secret_file`, not a bare `std::fs::write`: all three of these are
    // Secret<String> settings and resolve through
    // `zeroship_core::config::read_secret_file`, which refuses anything a
    // second local account could read. The default 022 umask leaves 0644.
    let write = |name: &str, contents: &str| -> String {
        let path = dir.path().join(name);
        write_secret_file(&path, contents.as_bytes());
        path.to_str().expect("utf8 temp path").to_owned()
    };
    let db_file = write("database-url", db_url);
    let stash_file = write("stash-signing-key", "test-stash-key-not-for-prod-32bytes!");
    let totp_file = write(
        "totp-enc-key",
        "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f",
    );

    let mut config = AuthConfig::parse_from([
        "zeroship-auth",
        "--addr",
        "127.0.0.1:0",
        "--database-url-file",
        &db_file,
        "--stash-signing-key-file",
        &stash_file,
        "--totp-enc-key-file",
        &totp_file,
        "--mail-from-email",
        "test@zeroship.test",
        "--mail-from-name",
        "Test",
        "--public-url",
        "http://localhost:0",
    ]);
    let (hash_file, idem_file) = session_keys::session_key_files();
    config.settings.refresh_hash_key_file = zeroship_core::config::Operational::new(hash_file);
    config.settings.refresh_idem_key_file = zeroship_core::config::Operational::new(idem_file);
    (config, dir)
}

/// Seed a user plus the app's brokered `oac_` client.
///
/// `redirect_uri` and `backchannel_logout_uri` are explicit because the two
/// flows this file drives register different ones: a raw token exchange lands
/// on the loopback callback and needs no logout receiver, while a real gateway
/// BFF session lands on the app-origin popup callback and MUST name the
/// gateway's back-channel-logout endpoint. Without that registration the OP
/// has nowhere to send a revocation and the gateway never hears about one.
async fn seed_user_client(
    db: &Client,
    user_id: Uuid,
    app_id: Uuid,
    client_id: &str,
    email: &str,
    redirect_uri: &str,
    backchannel_logout_uri: Option<&str>,
) {
    let phc = password::hash(PASSWORD).expect("password hash");
    db.execute(
        "INSERT INTO zeroship.users (id, email, email_verified_at, name, password_hash) \
         VALUES ($1, $2::citext, NOW(), 'Gateway E2E User', $3)",
        &[&user_id, &email, &phc],
    )
    .await
    .expect("seed user");
    db.execute(
        "INSERT INTO zeroship.plans \
            (id, name, runtime_limits_json, assignable_by_creator) \
         VALUES ('free', 'Free', '{}'::jsonb, TRUE) \
         ON CONFLICT (id) DO NOTHING",
        &[],
    )
    .await
    .expect("seed free plan");
    let project_id = common::unowned_project(db).await;
    db.execute(
        "INSERT INTO zeroship.apps (id, name, project_id, organization_id) \
         SELECT $1, $2, p.id, p.organization_id FROM zeroship.projects p WHERE p.id = $3",
        &[
            &app_id,
            &format!("gateway-e2e-app-{}", app_id.simple()),
            &project_id,
        ],
    )
    .await
    .expect("seed app");
    let scopes = vec![
        "openid".to_string(),
        "offline_access".to_string(),
        "email".to_string(),
        "profile".to_string(),
    ];
    db.execute(
        "INSERT INTO zeroship.oauth_clients \
            (client_id, client_name, redirect_uris, scopes, skip_consent, \
             token_endpoint_auth_method, brokered, refresh_allowed, \
             backchannel_logout_uri) \
         VALUES ($1, 'Gateway OP e2e', $2, $3, TRUE, 'client_secret_basic', TRUE, TRUE, $4)",
        &[
            &client_id,
            &vec![redirect_uri.to_string()],
            &scopes,
            &backchannel_logout_uri,
        ],
    )
    .await
    .expect("seed brokered oauth client");
    db.execute(
        "INSERT INTO zeroship.app_oauth_clients (app_id, client_id, sector_identifier) \
         VALUES ($1, $2, $3)",
        &[&app_id, &client_id, &SECTOR],
    )
    .await
    .expect("seed app oauth client");
    db.execute(
        "INSERT INTO zeroship.oauth_grants \
             (user_id, client_id, granted_scopes, granted_at, updated_at) \
         VALUES ($1, $2, $3, NOW(), NOW())",
        &[&user_id, &client_id, &scopes],
    )
    .await
    .expect("seed oauth grant");
}

async fn cleanup(db: &Client, user_id: Uuid, app_id: Uuid, client_id: &str) {
    let _ = db
        .execute(
            "DELETE FROM zeroship.oauth_grants WHERE client_id = $1",
            &[&client_id],
        )
        .await;
    let _ = db
        .execute(
            "DELETE FROM zeroship.oauth_clients WHERE client_id = $1",
            &[&client_id],
        )
        .await;
    let _ = db
        .execute(
            "DELETE FROM zeroship.idp_sessions WHERE user_id = $1",
            &[&user_id],
        )
        .await;
    let _ = db
        .execute("DELETE FROM zeroship.users WHERE id = $1", &[&user_id])
        .await;
    let _ = db
        .execute("DELETE FROM zeroship.apps WHERE id = $1", &[&app_id])
        .await;
}

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn gateway_bearer_rejects_real_op_id_token_but_accepts_access_token() {
    let db_url = db_url();

    let pg_client = connect_test_db(&db_url).await;
    let signing = op_signing();
    let broker = BrokerSecrets::new(BROKER_MASTER.to_vec(), None).expect("auth broker secrets");
    let issuer = Arc::new(
        Issuer::from_signing_key(&signing, [9u8; 32], ISSUER.to_string())
            .expect("issuer")
            .with_broker_secrets(broker),
    );
    publish_op_key_once(&issuer, &pg_client).await;

    let user_id = Uuid::new_v4();
    let app_id = Uuid::new_v4();
    let client_id = format!("oac_{}", zeroship_core::typed_id::uuid_to_base62(&app_id));
    let email = format!("gw-op-h2-{}@zeroship.test", Uuid::new_v4().simple());
    seed_user_client(
        &pg_client,
        user_id,
        app_id,
        &client_id,
        &email,
        REDIRECT_URI,
        None,
    )
    .await;

    let srv = start_platform_op(&db_url, pg_client.clone(), issuer).await;
    let auth_base = srv.url("").trim_end_matches('/').to_string();
    let rp = OidcRp::new(
        auth_base.clone(),
        BrokerSecret::from_bytes(BROKER_MASTER.to_vec()).expect("gateway broker secret"),
        b"gateway-e2e-stash-signing-key-32-bytes!".to_vec(),
    )
    .with_issuer(ISSUER);
    let tokens = browser_pkce_tokens(&rp, &auth_base, &client_id, &email).await;
    let id_token = tokens
        .id_token
        .as_deref()
        .expect("openid flow returns id_token");

    let worker = test::server(|| async {
        web::App::new()
            .service(web::resource("/dispatch/{app_id}").route(web::post().to(echo_verified_user)))
    })
    .await;
    let worker_base = worker.url("").trim_end_matches('/').to_string();
    let state = build_gateway_state(&auth_base, app_id, &client_id, Some(&worker_base), None);
    let app = test::init_service(
        web::App::new().state(state).service(
            web::resource("/{tail}*")
                .route(web::route().to(zeroship_gateway::router::handle_subdomain)),
        ),
    )
    .await;

    let access_req = test::TestRequest::get()
        .uri("/private")
        .header(http::header::HOST, APP_HOST)
        .header(
            http::header::AUTHORIZATION,
            format!("Bearer {}", tokens.access_token),
        )
        .to_request();
    let access_resp = test::call_service(&app, access_req).await;
    assert_eq!(
        access_resp.status().as_u16(),
        200,
        "real OP at+jwt access token must authenticate the user route"
    );
    let access_body = test::read_body(access_resp).await;
    let projected_user: serde_json::Value =
        serde_json::from_slice(&access_body).expect("worker returned projected user JSON");
    let expected_pws =
        zeroship_core::auth::derive_pairwise(&[9u8; 32], &user_id.to_string(), SECTOR);
    assert_eq!(
        projected_user["id"],
        serde_json::json!(expected_pws),
        "worker must receive the OP's once-projected pairwise subject"
    );

    let id_req = test::TestRequest::get()
        .uri("/private")
        .header(http::header::HOST, APP_HOST)
        .header(http::header::AUTHORIZATION, format!("Bearer {id_token}"))
        .to_request();
    let id_resp = test::call_service(&app, id_req).await;
    assert_eq!(
        id_resp.status().as_u16(),
        401,
        "OP id_token must not authenticate as a Bearer access token"
    );

    cleanup(&pg_client, user_id, app_id, &client_id).await;
    compio::time::sleep(Duration::from_millis(50)).await;
    drop(worker);
    drop(srv);
}

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn gateway_bearer_rejects_access_token_for_different_resource_audience() {
    let db_url = db_url();

    let pg_client = connect_test_db(&db_url).await;
    let signing = op_signing();
    let issuer = Arc::new(
        Issuer::from_signing_key(&signing, [9u8; 32], ISSUER.to_string()).expect("issuer"),
    );
    publish_op_key_once(&issuer, &pg_client).await;

    let app_id = Uuid::new_v4();
    let client_id = format!("oac_{}", zeroship_core::typed_id::uuid_to_base62(&app_id));
    let srv = start_platform_op(&db_url, pg_client, issuer.clone()).await;
    let auth_base = srv.url("").trim_end_matches('/').to_string();
    let state = build_gateway_state(&auth_base, app_id, &client_id, None, None);
    let app = test::init_service(
        web::App::new().state(state).service(
            web::resource("/{tail}*")
                .route(web::route().to(zeroship_gateway::router::handle_subdomain)),
        ),
    )
    .await;

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock")
        .as_secs();
    let mut claims = serde_json::json!({
        "iss": issuer.issuer(),
        "sub": issuer.pairwise_subject(&Uuid::new_v4().to_string(), SECTOR),
        "aud": format!("app:{app_id}"),
        "client_id": client_id,
        "scope": "openid email",
        "iat": now,
        "exp": now + 300,
        "jti": Uuid::new_v4().to_string(),
    });
    let key = signing.to_pkcs8_der().expect("fixture signing key");
    let key = jsonwebtoken::EncodingKey::from_ed_der(key.as_bytes());
    let mut header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::EdDSA);
    header.typ = Some(zeroship_auth::oidc::ACCESS_TOKEN_TYP.to_owned());
    header.kid = Some(issuer.kid().to_owned());
    let accepted = jsonwebtoken::encode(&header, &claims, &key).expect("sign accepted claims");
    let control = test::TestRequest::get()
        .uri("/private")
        .header(http::header::HOST, APP_HOST)
        .header(http::header::AUTHORIZATION, format!("Bearer {accepted}"))
        .to_request();
    let response = test::call_service(&app, control).await;
    assert_eq!(
        response.status().as_u16(),
        200,
        "matching audience must authenticate"
    );
    assert_eq!(test::read_body(response).await.as_ref(), b"ok");

    claims["aud"] = serde_json::json!(format!("app:{}", Uuid::new_v4()));
    let wrong_aud_token = jsonwebtoken::encode(&header, &claims, &key)
        .expect("sign otherwise-identical claims for another audience");

    let req = test::TestRequest::get()
        .uri("/private")
        .header(http::header::HOST, APP_HOST)
        .header(
            http::header::AUTHORIZATION,
            format!("Bearer {wrong_aud_token}"),
        )
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(
        resp.status().as_u16(),
        401,
        "access token with the route client_id but a different resource aud must reject"
    );

    compio::time::sleep(Duration::from_millis(50)).await;
    drop(srv);
}

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn gateway_oidc_rp_full_dance_against_platform_op() {
    let db_url = db_url();

    let (pg_client, pg_connection) = compio_postgres::connect(&db_url, NoTls)
        .await
        .expect("connect pg");
    compio::runtime::spawn(async move {
        if let Err(e) = pg_connection.run().await {
            eprintln!("[oidc_rp_e2e] pg connection driver: {e}");
        }
    })
    .detach();
    let pg_client = Arc::new(pg_client);

    let signing = op_signing();
    let broker = BrokerSecrets::new(BROKER_MASTER.to_vec(), None).expect("auth broker secrets");
    let issuer = Arc::new(
        Issuer::from_signing_key(&signing, [9u8; 32], ISSUER.to_string())
            .expect("issuer")
            .with_broker_secrets(broker),
    );
    publish_op_key_once(&issuer, &pg_client).await;

    let user_id = Uuid::new_v4();
    let app_id = Uuid::new_v4();
    let client_id = format!("oac_{}", zeroship_core::typed_id::uuid_to_base62(&app_id));
    let email = format!("gw-op-{}@zeroship.test", Uuid::new_v4().simple());
    seed_user_client(
        &pg_client,
        user_id,
        app_id,
        &client_id,
        &email,
        REDIRECT_URI,
        None,
    )
    .await;

    // Keyring via `test_auth_config`, as `start_platform_op` does.
    let (cfg, _auth_secret_files) = test_auth_config(&db_url);
    let cfg = Arc::new(cfg);
    let refresh_pool = zeroship_auth::oidc::refresh::RefreshSessionPool::new(db_url.clone(), 4);
    let srv = {
        let cfg_state = cfg.clone();
        let db_state = pg_client.clone();
        let issuer_state = issuer.clone();
        let refresh_pool_state = refresh_pool.clone();
        web::test::server(move || {
            let cfg_state = cfg_state.clone();
            let db_state = db_state.clone();
            let issuer_state = issuer_state.clone();
            let refresh_pool_state = refresh_pool_state.clone();
            async move {
                web::App::new()
                    .state(cfg_state)
                    .state(db_state)
                    .state(issuer_state)
                    .state(refresh_pool_state)
                    .middleware(SecurityHeaders::default())
                    .configure(server::configure(false, false))
            }
        })
        .await
    };
    let auth_base = srv.url("").trim_end_matches('/').to_string();

    let rp = OidcRp::new(
        auth_base.clone(),
        BrokerSecret::from_bytes(BROKER_MASTER.to_vec()).expect("gateway broker secret"),
        b"gateway-e2e-stash-signing-key-32-bytes!".to_vec(),
    )
    .with_issuer(ISSUER);

    let (auth_url, stash) = rp.build_authorize_redirect(&client_id, "/some/path", REDIRECT_URI);
    assert!(
        auth_url.starts_with(&format!("{auth_base}/oauth2/authorize?")),
        "{auth_url}"
    );
    assert!(
        auth_url.contains(&format!("client_id={client_id}")),
        "{auth_url}"
    );
    assert!(
        auth_url.contains("scope=openid+offline_access+email+profile"),
        "{auth_url}"
    );

    let http = cyper::Client::new();

    let first = http
        .request(http::Method::GET, &auth_url)
        .expect("build GET /authorize")
        .send()
        .await
        .expect("send GET /authorize");
    assert_eq!(first.status().as_u16(), 303);
    let login_loc = location(&first);
    assert!(
        login_loc.starts_with("/login?return_to="),
        "login redirect: {login_loc}"
    );
    let return_to = relative_query_param(&login_loc, "return_to").expect("return_to");

    let login_get = http
        .request(http::Method::GET, format!("{auth_base}{login_loc}"))
        .expect("build GET /login")
        .send()
        .await
        .expect("send GET /login");
    assert_eq!(login_get.status().as_u16(), 200);
    let csrf = read_set_cookie(&login_get, "__Host-zsidp_csrf").expect("login csrf");
    let login_body = form(&[
        ("csrf", csrf.as_str()),
        ("email", email.as_str()),
        ("password", PASSWORD),
        ("return_to", return_to.as_str()),
    ]);
    let login_post = http
        .request(http::Method::POST, format!("{auth_base}/login"))
        .expect("build POST /login")
        .header("content-type", "application/x-www-form-urlencoded")
        .expect("content-type")
        .header("cookie", format!("__Host-zsidp_csrf={csrf}"))
        .expect("cookie")
        .body(login_body)
        .send()
        .await
        .expect("send POST /login");
    assert_eq!(login_post.status().as_u16(), 303);
    assert_eq!(location(&login_post), return_to);
    let session = read_set_cookie(&login_post, "__Host-zsidp_session").expect("session cookie");

    let final_authorize = http
        .request(http::Method::GET, format!("{auth_base}{return_to}"))
        .expect("build GET /authorize with session")
        .header("cookie", format!("__Host-zsidp_session={session}"))
        .expect("cookie")
        .send()
        .await
        .expect("send GET /authorize with session");
    assert_eq!(final_authorize.status().as_u16(), 303);
    let cb_url = location(&final_authorize);
    assert!(cb_url.starts_with(REDIRECT_URI), "callback: {cb_url}");
    assert_eq!(
        query_param(&cb_url, "state").as_deref(),
        query_param(&auth_url, "state").as_deref()
    );
    assert_eq!(query_param(&cb_url, "iss").as_deref(), Some(ISSUER));
    let code = query_param(&cb_url, "code").expect("code param");
    let state = query_param(&cb_url, "state").expect("state param");

    // MAJOR-1 regression: redeeming this stash under a DIFFERENT route client_id
    // must fail closed (per-app isolation is an enforced invariant, not merely an
    // emergent property of __Host- cookie origin-isolation). The client check is
    // before the code exchange, so the one-time code is untouched for the real
    // call below.
    let mismatch = rp
        .finish_callback(&code, &state, &stash, "oac_someotherapp000000000000")
        .await;
    assert!(
        matches!(
            mismatch,
            Err(zeroship_gateway::oidc_rp::OidcRpError::ClientMismatch)
        ),
        "stash redeemed under a mismatched route client_id must fail ClientMismatch, got {mismatch:?}"
    );

    let (claims, original_path, granted_scopes) = rp
        .finish_callback(&code, &state, &stash, &client_id)
        .await
        .expect("finish_callback");
    assert_eq!(original_path, "/some/path");
    assert_eq!(claims.sub, user_id.to_string());
    assert!(granted_scopes.contains(&"openid".to_string()));
    assert!(granted_scopes.contains(&"offline_access".to_string()));

    let (mut sess_client, sess_connection) = compio_postgres::connect(&db_url, NoTls)
        .await
        .expect("connect pg (session store)");
    compio::runtime::spawn(async move {
        if let Err(e) = sess_connection.run().await {
            eprintln!("[oidc_rp_e2e] session-store pg driver: {e}");
        }
    })
    .detach();
    let session = create(
        &mut sess_client,
        &NewSession {
            user_id: &claims.sub,
            sid: claims.sid.as_deref(),
            app_id,
            email: claims.email.as_deref(),
            name: claims.name.as_deref(),
            avatar_url: claims.picture.as_deref(),
            email_verified: claims.email_verified.unwrap_or(false),
            granted_scopes: &granted_scopes,
            auth_time: claims.auth_time,
            amr: claims.amr.as_deref().unwrap_or(&[]),
        },
    )
    .await
    .expect("session create");

    let live = live_session(&sess_client, session.id, app_id)
        .await
        .expect("session row must be live after create");
    let live_user: Uuid = live.get("user_id");
    assert_eq!(live_user.to_string(), claims.sub);
    let live_scopes: Vec<String> = live.try_get("granted_scopes").unwrap_or_default();
    assert_eq!(live_scopes, granted_scopes);

    revoke_app_sessions_for_user(&mut sess_client, app_id, &claims.sub)
        .await
        .expect("revoke");
    assert!(
        live_session(&sess_client, session.id, app_id)
            .await
            .is_none(),
        "a revoked session must not resolve as live"
    );

    cleanup(&pg_client, user_id, app_id, &client_id).await;
    compio::time::sleep(Duration::from_millis(50)).await;
    drop(srv);
}

/// THE APP-SESSION REVOKE SECURITY PROPERTY, end to end across both services.
///
/// A user signs in to a hosted app, then revokes THAT app session from the OP
/// (`POST /me/sessions/{id}/revoke`, `kind=app`). The request that the session
/// authenticated a moment ago must stop being authenticated, and the 30-day
/// reload anchor must stop re-minting.
///
/// WHY THIS IS ASSERTED HERE AND NOT IN THE AUTH CRATE. The revoke handler runs
/// in `crates/auth`, but nothing it can observe proves the property: the row it
/// deletes (`zeroship.gateway_sessions`) is an audit record, and the gateway
/// authenticates a request from a locally-verified `zeroship-sess+jwt` cookie
/// plus the `(client_id, sub)` family marker instead. Asserting "the row is
/// gone" or "the API returned 200" is exactly the shape of assertion that let a
/// revoke which revoked NOTHING pass as working. So both services run for real
/// here: the OP emits its back-channel logout over a real HTTP hop to the
/// gateway's real receiver, and the assertions are made on gateway REQUESTS.
///
/// The control is the same request, same cookie, one variable: taken BEFORE the
/// revoke it must be accepted, and after it must not.
#[ntex::test]
#[allow(clippy::future_not_send, clippy::too_many_lines)]
async fn app_session_revoke_at_the_op_ends_the_gateway_session() {
    let db_url = db_url();

    let pg_client = connect_test_db(&db_url).await;
    let signing = op_signing();
    let broker = BrokerSecrets::new(BROKER_MASTER.to_vec(), None).expect("auth broker secrets");
    let issuer = Arc::new(
        Issuer::from_signing_key(&signing, [9u8; 32], ISSUER.to_string())
            .expect("issuer")
            .with_broker_secrets(broker),
    );
    publish_op_key_once(&issuer, &pg_client).await;

    let user_id = Uuid::new_v4();
    let app_id = Uuid::new_v4();
    let client_id = format!("oac_{}", zeroship_core::typed_id::uuid_to_base62(&app_id));
    let email = format!("gw-revoke-{}@zeroship.test", Uuid::new_v4().simple());
    // The gateway's BFF default: the SDK lands the code on the app origin.
    let popup_callback = format!("{SECTOR}/__zeroship/auth/popup-callback");
    seed_user_client(
        &pg_client,
        user_id,
        app_id,
        &client_id,
        &email,
        &popup_callback,
        None,
    )
    .await;

    let op_srv = start_platform_op(&db_url, pg_client.clone(), issuer).await;
    let auth_base = op_srv.url("").trim_end_matches('/').to_string();

    let db_cfg = zeroship_gateway::db::DbConfig::new(db_url.clone(), 8);
    let state = build_gateway_state(&auth_base, app_id, &client_id, None, Some(db_cfg.clone()));

    // The gateway's back-channel-logout receiver on a REAL socket, sharing the
    // SAME `GateState` as the in-process app below, so the teardown it runs
    // (family marker + same-node revocation-cache bust + anchor delete) is seen
    // by the very request path the assertions exercise.
    let bcl_state = state.clone();
    let bcl_srv = test::server(move || {
        let bcl_state = bcl_state.clone();
        async move {
            web::App::new().state(bcl_state).service(
                web::resource("/oidc/backchannel-logout")
                    .route(web::post().to(zeroship_gateway::backchannel_logout::handle)),
            )
        }
    })
    .await;
    let bcl_uri = format!(
        "{}/oidc/backchannel-logout",
        bcl_srv.url("").trim_end_matches('/')
    );
    // Register it now that the port is known. Without this the OP has no
    // receiver and the revoke below is unreachable by construction.
    pg_client
        .execute(
            "UPDATE zeroship.oauth_clients SET backchannel_logout_uri = $2 WHERE client_id = $1",
            &[&client_id, &bcl_uri],
        )
        .await
        .expect("register backchannel_logout_uri");

    let app = test::init_service(
        web::App::new()
            .state(state.clone())
            .service(
                web::resource("/__zeroship/auth/session")
                    .route(web::post().to(zeroship_gateway::auth_token::session_post))
                    .route(web::get().to(zeroship_gateway::auth_token::session)),
            )
            .service(
                web::resource("/{tail}*")
                    .route(web::route().to(zeroship_gateway::router::handle_subdomain)),
            ),
    )
    .await;

    // 1. Real OP login, real authorization code, real PKCE.
    let verifier = zeroship_core::pkce::generate_verifier();
    let challenge = zeroship_core::pkce::s256_challenge(&verifier);
    let rp = OidcRp::new(
        auth_base.clone(),
        BrokerSecret::from_bytes(BROKER_MASTER.to_vec()).expect("gateway broker secret"),
        b"gateway-e2e-stash-signing-key-32-bytes!".to_vec(),
    )
    .with_issuer(ISSUER);
    let auth_url = rp.build_browser_authorize_url(
        &client_id,
        &BrowserAuthorizeParams {
            code_challenge: &challenge,
            state: &format!("st-{}", Uuid::new_v4().simple()),
            nonce: &format!("nonce-{}", Uuid::new_v4().simple()),
            scope: "openid offline_access email profile",
            redirect_uri: &popup_callback,
            prompt: None,
            idp_hint: None,
        },
    );
    let login = drive_login_to_code(&auth_base, &auth_url, &email, &popup_callback).await;

    // 2. Real gateway BFF session: signed session cookie + reload anchor.
    let body = form(&[
        ("grant_type", "authorization_code"),
        ("code", login.code.as_str()),
        ("code_verifier", verifier.as_str()),
        ("redirect_uri", popup_callback.as_str()),
    ]);
    let req = test::TestRequest::post()
        .uri("/__zeroship/auth/session")
        .header(http::header::HOST, APP_HOST)
        .header("origin", SECTOR)
        .header("x-zs-auth", "1")
        .header(
            http::header::CONTENT_TYPE,
            "application/x-www-form-urlencoded",
        )
        .set_payload(body)
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(
        resp.status().as_u16(),
        200,
        "gateway BFF session mint must succeed against the real OP"
    );
    let session_cookie =
        set_cookie_pair(&resp, "__Host-zeroship_app_session=").expect("session cookie");
    let anchor_cookie =
        set_cookie_pair(&resp, "__Host-zeroship_app_anchor=").expect("anchor cookie");

    // 3. CONTROL, before the revoke: this exact request is ACCEPTED.
    let before = test::call_service(&app, protected_request(&session_cookie)).await;
    assert_eq!(
        before.status().as_u16(),
        200,
        "the freshly minted session must authenticate the protected route"
    );

    // 4. The OP's own view of that session, and the revoke a user clicks.
    let gw_session_id: Uuid = pg_client
        .query_one(
            "SELECT id FROM zeroship.gateway_sessions \
             WHERE user_id = $1 AND app_id = $2 AND revoked_at IS NULL \
             ORDER BY issued_at DESC LIMIT 1",
            &[&user_id, &app_id],
        )
        .await
        .expect("the OP lists the app session it is about to revoke")
        .get("id");

    let csrf = "revoke-csrf-token-1234";
    let http = cyper::Client::new();
    let revoke = http
        .request(
            http::Method::POST,
            format!("{auth_base}/me/sessions/{gw_session_id}/revoke"),
        )
        .expect("build POST revoke")
        .header("content-type", "application/x-www-form-urlencoded")
        .expect("content-type")
        .header(
            "cookie",
            format!(
                "__Host-zsidp_session={}; __Host-zsidp_csrf={csrf}",
                login.idp_session
            ),
        )
        .expect("cookie")
        .body(form(&[("csrf", csrf), ("kind", "app")]))
        .send()
        .await
        .expect("send POST revoke");
    assert_eq!(revoke.status().as_u16(), 200, "revoke must be accepted");
    let revoke_body: serde_json::Value =
        serde_json::from_slice(&revoke.bytes().await.expect("revoke body")).expect("revoke json");
    assert_eq!(
        revoke_body["revoked"],
        serde_json::json!(true),
        "the OP must report it revoked the caller's own app session"
    );

    // 5. THE PROPERTY: the same request, the same cookie, now REJECTED.
    let after = test::call_service(&app, protected_request(&session_cookie)).await;
    assert_ne!(
        after.status().as_u16(),
        200,
        "a revoked app session must not authenticate the protected route"
    );

    // 6. And the DURABLE half: the 30-day anchor must not re-mint a fresh
    //    cookie. Without this the revoke is merely delayed by the cookie's
    //    15-minute lifetime and then undone for the next 30 days.
    let mint = test::call_service(
        &app,
        test::TestRequest::get()
            .uri("/__zeroship/auth/session?mint=1")
            .header(http::header::HOST, APP_HOST)
            .header("origin", SECTOR)
            .header("x-zs-auth", "1")
            .header(http::header::COOKIE, anchor_cookie.as_str())
            .to_request(),
    )
    .await;
    assert_eq!(
        mint.status().as_u16(),
        401,
        "the reload anchor of a revoked app session must require a fresh login"
    );
    assert!(
        set_cookie_pair(&mint, "__Host-zeroship_app_session=").is_none(),
        "a revoked anchor must not yield a fresh session cookie"
    );

    let pws =
        zeroship_core::auth::derive_pairwise(&state.pairwise_salt, &user_id.to_string(), SECTOR);
    let _ = pg_client
        .execute(
            "DELETE FROM zeroship.token_revocations WHERE client_id = $1 AND sub = $2",
            &[&client_id, &pws],
        )
        .await;
    let _ = pg_client
        .execute(
            "DELETE FROM zeroship.audit_events WHERE client_id = $1",
            &[&client_id],
        )
        .await;
    let _ = pg_client
        .execute(
            "DELETE FROM zeroship.app_session_anchors WHERE app_id = $1",
            &[&app_id],
        )
        .await;
    let _ = pg_client
        .execute(
            "DELETE FROM zeroship.gateway_sessions WHERE app_id = $1",
            &[&app_id],
        )
        .await;
    let _ = pg_client
        .execute(
            "DELETE FROM zeroship.app_user_identities WHERE app_id = $1",
            &[&app_id],
        )
        .await;
    let _ = pg_client
        .execute(
            "DELETE FROM zeroship.oidc_session_clients WHERE user_id = $1",
            &[&user_id],
        )
        .await;
    let _ = pg_client
        .execute(
            "DELETE FROM zeroship.sessions WHERE client_id = $1",
            &[&client_id],
        )
        .await;
    let _ = pg_client
        .execute(
            "DELETE FROM zeroship.app_oauth_clients WHERE app_id = $1",
            &[&app_id],
        )
        .await;
    cleanup(&pg_client, user_id, app_id, &client_id).await;
    compio::time::sleep(Duration::from_millis(50)).await;
    drop(bcl_srv);
    drop(op_srv);
}

/// The one request both the control and the property are made on: a GET of the
/// manifest's `auth: user` resource, carrying only the session cookie.
fn protected_request(session_cookie: &str) -> ntex::http::Request {
    test::TestRequest::get()
        .uri("/private")
        .header(http::header::HOST, APP_HOST)
        .header(http::header::COOKIE, session_cookie)
        .to_request()
}

/// Extract a `Set-Cookie` value as a `name=value` pair ready to re-send.
fn set_cookie_pair(resp: &ntex::web::WebResponse, prefix: &str) -> Option<String> {
    for hv in resp.headers().get_all(http::header::SET_COOKIE) {
        let Ok(s) = hv.to_str() else { continue };
        if s.starts_with(prefix) {
            let pair = s.split(';').next()?.trim();
            // A clear (`Max-Age=0`) carries an empty value; that is not a cookie
            // the browser would send back, so it must not read as one here.
            if pair.ends_with('=') {
                return None;
            }
            return Some(pair.to_string());
        }
    }
    None
}

#[path = "../../../tests/fixtures/session_keys.rs"]
mod session_keys;
