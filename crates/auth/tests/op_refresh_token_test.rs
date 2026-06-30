//! P5b refresh-token family tests for the platform OP.

mod common;

use std::path::PathBuf;
use std::sync::Arc;

use base64::{engine::general_purpose::STANDARD, Engine as _};
use compio_postgres::{connect, Client, NoTls};
use ed25519_dalek::SigningKey;
use ntex::web;
use serde::Deserialize;
use serde_json::Value;
use uuid::Uuid;
use zeroship_auth::headers::SecurityHeaders;
use zeroship_auth::hydra_client::HydraAdmin;
use zeroship_auth::op::Issuer;
use zeroship_auth::server;
use zeroship_auth::sessions::login as session_cookie;
use zeroship_auth::store::sessions as session_store;
use zeroship_core::auth::hash_api_key;

use common::{location, pkce_challenge_s256, pkce_verifier, test_auth_config};

const ISSUER: &str = "https://auth.zeroship.test";
const REDIRECT_URI: &str = "http://127.0.0.1:9998/cb";
const SECTOR: &str = "https://app-refresh.zeroship.test";
const REFRESH_CLIENT_SECRET: &str = "refresh-client-secret-32-bytes-minimum";
const FULL_SCOPE: &str = "openid profile email offline_access";
const NARROW_SCOPE: &str = "openid profile";

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    id_token: Option<String>,
    token_type: String,
    expires_in: u64,
    scope: String,
    #[serde(default)]
    refresh_token: Option<String>,
}

struct Fixture {
    srv: ntex::web::test::TestServer,
    auth_base: String,
    db: Arc<Client>,
    client_id: String,
    app_id: Uuid,
    user_id: Uuid,
    session_cookie: String,
    key_dir: PathBuf,
}

impl Fixture {
    #[allow(clippy::future_not_send)]
    async fn boot(scopes: &[&str]) -> Option<Self> {
        let Some(db_url) = db_url() else {
            eprintln!("[op_refresh_token_test] skip (AUTH_DB_URL or CONTROL_TEST_DB unset)");
            return None;
        };
        let (pg_client, pg_connection) = connect(&db_url, NoTls).await.expect("connect pg");
        compio::runtime::spawn(async move {
            if let Err(err) = pg_connection.run().await {
                eprintln!("[op_refresh_token_test] pg connection error: {err}");
            }
        })
        .detach();
        let db = Arc::new(pg_client);

        let issuer = Arc::new(test_issuer());
        issuer
            .publish_active_key(&db)
            .await
            .expect("publish active OP key");

        let user_id = Uuid::new_v4();
        let app_id = Uuid::new_v4();
        let client_id = format!("oac_p5b_{}", Uuid::new_v4().simple());
        let app_name = format!("p5b-refresh-{}", Uuid::new_v4().simple());
        seed_user_client(&db, user_id, app_id, &app_name, &client_id, scopes).await;
        let session = session_store::create(
            &db,
            &session_store::CreateSession {
                user_id,
                auth_method: "pwd",
                amr: vec!["pwd".to_string()],
                acr: None,
                expected_credential_version: None,
                idle_minutes: zeroship_auth::sessions::login::IDLE_MINUTES,
                absolute_hours: zeroship_auth::sessions::login::ABSOLUTE_HOURS,
            },
        )
        .await
        .expect("create idp session");
        let session_cookie = session_cookie::set_cookie(&session.id, true)
            .split(';')
            .next()
            .expect("cookie pair")
            .to_string();

        let key_dir = make_key_dir();
        let hash_key_file = key_dir.join("refresh-hmac.keys");
        let idem_key_file = key_dir.join("refresh-idem.key");
        write_secret_file(&hash_key_file, b"1:refresh-hmac-key-material-32-bytes");
        write_secret_file(&idem_key_file, b"refresh-idem-key-material-32-bytes");

        let mut cfg = test_auth_config(
            &db_url,
            "http://127.0.0.1:4445",
            "http://127.0.0.1:4444",
        );
        cfg.refresh_hash_key_file = Some(hash_key_file);
        cfg.refresh_idem_key_file = Some(idem_key_file);
        let cfg = Arc::new(cfg);
        let admin = HydraAdmin::new("http://127.0.0.1:4445");
        let admin_state = admin.clone();
        let cfg_state = cfg.clone();
        let db_state = db.clone();
        let issuer_state = issuer.clone();
        let srv = web::test::server(move || {
            let admin_state = admin_state.clone();
            let cfg_state = cfg_state.clone();
            let db_state = db_state.clone();
            let issuer_state = issuer_state.clone();
            async move {
                web::App::new()
                    .state(admin_state)
                    .state(cfg_state)
                    .state(db_state)
                    .state(issuer_state)
                    .middleware(SecurityHeaders::default())
                    .configure(server::configure(false, false))
            }
        })
        .await;

        Some(Self {
            auth_base: srv.url("").trim_end_matches('/').to_string(),
            srv,
            db,
            client_id,
            app_id,
            user_id,
            session_cookie,
            key_dir,
        })
    }

    async fn cleanup(self) {
        cleanup_seeded_rows(&self.db, self.user_id, self.app_id, &self.client_id).await;
        let _ = std::fs::remove_dir_all(&self.key_dir);
        drop(self.srv);
    }
}

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn offline_access_authorization_code_returns_refresh_token_bound_to_family() {
    let Some(fx) = Fixture::boot(&["openid", "profile", "email", "offline_access"]).await else {
        return;
    };
    let token = issue_refresh(&fx, FULL_SCOPE).await;
    assert_eq!(token.token_type, "Bearer");
    assert!(token.expires_in > 0);
    assert!(token.refresh_token.as_deref().is_some_and(|t| t.starts_with("zrt_")));
    assert!(token.id_token.is_some(), "auth-code response still includes id_token");

    let row = fx
        .db
        .query_one(
            "SELECT refresh_family_id, sub, family_granted_scopes, octet_length(token_hash) AS hash_len \
             FROM zeroship.oauth_refresh_tokens \
             WHERE client_id = $1 AND user_id = $2",
            &[&fx.client_id, &fx.user_id],
        )
        .await
        .expect("refresh family row");
    let family_id: String = row.get("refresh_family_id");
    let sub: String = row.get("sub");
    let family_scopes: Vec<String> = row.get("family_granted_scopes");
    let hash_len: i32 = row.get("hash_len");
    assert!(family_id.starts_with("rfam_"));
    assert_eq!(
        sub,
        test_issuer().pairwise_subject(&fx.user_id.to_string(), SECTOR)
    );
    assert!(family_scopes.iter().any(|s| s == "offline_access"));
    assert_eq!(hash_len, 32, "refresh token hash is HMAC-SHA256 bytes only");

    fx.cleanup().await;
}

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn stale_credential_version_recheck_rejects_refresh_issuance() {
    let Some(fx) = Fixture::boot(&["openid", "profile", "email", "offline_access"]).await else {
        return;
    };
    let verifier = pkce_verifier();
    let authorize = send_authorize(&fx, FULL_SCOPE, &verifier)
        .await
        .expect("authorize");
    let code = query_param(&location(&authorize), "code").expect("code");
    fx.db
        .execute(
            "UPDATE zeroship.users SET credential_version = credential_version + 1 WHERE id = $1",
            &[&fx.user_id],
        )
        .await
        .expect("bump credential_version");

    let resp = token_request(&fx, &code, REDIRECT_URI, &verifier)
        .await
        .expect("token response");
    assert_eq!(resp.status().as_u16(), 400);
    assert_error(resp, "invalid_grant").await;

    fx.cleanup().await;
}

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn refresh_rotation_returns_new_refresh_narrows_scope_and_no_id_token() {
    let Some(fx) = Fixture::boot(&["openid", "profile", "email", "offline_access"]).await else {
        return;
    };
    let root = issue_refresh(&fx, FULL_SCOPE).await;
    let root_refresh = root.refresh_token.expect("root refresh token");

    let rotated = refresh_request(&fx, &root_refresh, Some(NARROW_SCOPE))
        .await
        .expect("refresh response");
    assert_eq!(rotated.status().as_u16(), 200);
    let rotated = rotated.json::<TokenResponse>().await.expect("refresh json");
    assert_eq!(rotated.token_type, "Bearer");
    assert!(rotated.access_token.contains('.'));
    assert!(rotated.refresh_token.as_deref().is_some_and(|t| t.starts_with("zrt_")));
    assert_ne!(rotated.refresh_token.as_ref(), Some(&root_refresh));
    assert_eq!(rotated.id_token, None, "refresh grant must not mint id_token");
    assert_eq!(rotated.scope, NARROW_SCOPE);

    let row = fx
        .db
        .query_one(
            "SELECT COUNT(*) FILTER (WHERE rotated_at IS NOT NULL AND consumed_at IS NOT NULL)::BIGINT AS rotated, \
                    COUNT(*) FILTER (WHERE rotated_at IS NULL AND revoked_at IS NULL)::BIGINT AS live \
             FROM zeroship.oauth_refresh_tokens WHERE client_id = $1 AND user_id = $2",
            &[&fx.client_id, &fx.user_id],
        )
        .await
        .expect("refresh row counts");
    assert_eq!(row.get::<_, i64>("rotated"), 1);
    assert_eq!(row.get::<_, i64>("live"), 1);

    fx.cleanup().await;
}

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn refresh_scope_cannot_widen_past_family_granted_scopes() {
    let Some(fx) = Fixture::boot(&["openid", "profile", "email", "offline_access"]).await else {
        return;
    };
    let root = issue_refresh(&fx, "openid profile offline_access").await;
    let root_refresh = root.refresh_token.expect("root refresh token");

    let widened = refresh_request(&fx, &root_refresh, Some("openid profile email"))
        .await
        .expect("refresh response");
    assert_eq!(widened.status().as_u16(), 400);
    assert_error(widened, "invalid_scope").await;

    let row = fx
        .db
        .query_one(
            "SELECT COUNT(*) FILTER (WHERE consumed_at IS NOT NULL)::BIGINT AS consumed \
             FROM zeroship.oauth_refresh_tokens WHERE client_id = $1 AND user_id = $2",
            &[&fx.client_id, &fx.user_id],
        )
        .await
        .expect("refresh consumed count");
    assert_eq!(row.get::<_, i64>("consumed"), 0);

    fx.cleanup().await;
}

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn replay_after_legitimate_rotation_kills_family() {
    replay_after_rotation_kills_family("legitimate").await;
}

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn replay_after_attacker_rotation_kills_family() {
    replay_after_rotation_kills_family("attacker").await;
}

#[allow(clippy::future_not_send)]
async fn replay_after_rotation_kills_family(label: &str) {
    let Some(fx) = Fixture::boot(&["openid", "profile", "email", "offline_access"]).await else {
        return;
    };
    let root = issue_refresh(&fx, FULL_SCOPE).await;
    let root_refresh = root.refresh_token.expect("root refresh token");
    let first_rotation = refresh_request(&fx, &root_refresh, Some(NARROW_SCOPE))
        .await
        .expect("first rotation");
    assert_eq!(first_rotation.status().as_u16(), 200, "{label} first rotation");
    let family_id = refresh_family_id(&fx).await;
    expire_idempotency_window(&fx, &family_id).await;

    let replay = refresh_request(&fx, &root_refresh, Some(NARROW_SCOPE))
        .await
        .expect("replay response");
    assert_eq!(replay.status().as_u16(), 400, "{label} replay");
    assert_error(replay, "invalid_grant").await;
    assert_family_revoked(&fx, &family_id).await;

    fx.cleanup().await;
}

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn legit_lost_response_retry_recovers_without_family_kill() {
    let Some(fx) = Fixture::boot(&["openid", "profile", "email", "offline_access"]).await else {
        return;
    };
    let root = issue_refresh(&fx, FULL_SCOPE).await;
    let root_refresh = root.refresh_token.expect("root refresh token");
    let first = refresh_request(&fx, &root_refresh, Some(NARROW_SCOPE))
        .await
        .expect("first refresh");
    assert_eq!(first.status().as_u16(), 200);
    let first = first.json::<TokenResponse>().await.expect("first json");
    let child_refresh = first.refresh_token.clone().expect("child refresh");

    let retry = refresh_request(&fx, &root_refresh, Some(NARROW_SCOPE))
        .await
        .expect("lost response retry");
    assert_eq!(retry.status().as_u16(), 200);
    let retry = retry.json::<TokenResponse>().await.expect("retry json");
    assert_eq!(retry.refresh_token.as_deref(), Some(child_refresh.as_str()));
    assert_ne!(
        retry.access_token, first.access_token,
        "idempotent replay returns cached refresh token with a fresh access token"
    );
    assert_family_not_revoked(&fx, &refresh_family_id(&fx).await).await;

    fx.cleanup().await;
}

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn revoke_refresh_token_kills_family_and_is_uniform() {
    let Some(fx) = Fixture::boot(&["openid", "profile", "email", "offline_access"]).await else {
        return;
    };
    let root = issue_refresh(&fx, FULL_SCOPE).await;
    let root_refresh = root.refresh_token.expect("root refresh token");
    let family_id = refresh_family_id(&fx).await;

    let revoke = revoke_request(&fx, &root_refresh).await.expect("revoke response");
    assert_eq!(revoke.status().as_u16(), 200);
    assert_family_revoked(&fx, &family_id).await;

    let replay_revoke = revoke_request(&fx, &root_refresh)
        .await
        .expect("second revoke response");
    assert_eq!(replay_revoke.status().as_u16(), 200);
    let unknown = revoke_request(&fx, "zrt_unknown-token-material")
        .await
        .expect("unknown revoke response");
    assert_eq!(unknown.status().as_u16(), 200);

    fx.cleanup().await;
}

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn bulk_credential_bump_revoke_does_not_deadlock_concurrent_rotation() {
    let Some(fx) = Fixture::boot(&["openid", "profile", "email", "offline_access"]).await else {
        return;
    };
    let root = issue_refresh(&fx, FULL_SCOPE).await;
    let root_refresh = root.refresh_token.expect("root refresh token");
    let Some(db_url) = db_url() else {
        fx.cleanup().await;
        return;
    };
    let (revoke_db, revoke_conn) = connect(&db_url, NoTls).await.expect("revoke pg connect");
    compio::runtime::spawn(async move {
        let _ = revoke_conn.run().await;
    })
    .detach();

    let rotate = refresh_request(&fx, &root_refresh, Some(NARROW_SCOPE));
    let revoke = zeroship_auth::op::refresh::revoke_user_refresh_families(
        &revoke_db,
        fx.user_id,
        "credential_bump_test",
    );
    let (rotate, revoke) = futures::join!(rotate, revoke);
    revoke.expect("credential-bump family revoke completes");
    let rotate = rotate.expect("rotation response completes");
    assert!(
        matches!(rotate.status().as_u16(), 200 | 400),
        "rotation races with credential revoke but must complete, got {}",
        rotate.status()
    );
    assert_family_revoked(&fx, &refresh_family_id(&fx).await).await;

    fx.cleanup().await;
}

fn db_url() -> Option<String> {
    std::env::var("AUTH_DB_URL")
        .or_else(|_| std::env::var("CONTROL_TEST_DB"))
        .ok()
}

fn test_issuer() -> Issuer {
    let signing = SigningKey::from_bytes(&[43u8; 32]);
    Issuer::from_signing_key(&signing, [11u8; 32], ISSUER.to_string()).expect("issuer")
}

fn make_key_dir() -> PathBuf {
    let path = std::env::temp_dir().join(format!("zs-refresh-keys-{}", Uuid::new_v4().simple()));
    std::fs::create_dir_all(&path).expect("create key dir");
    path
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

async fn seed_user_client(
    db: &Client,
    user_id: Uuid,
    app_id: Uuid,
    app_name: &str,
    client_id: &str,
    scopes: &[&str],
) {
    let email = format!("p5b-{}@zeroship.test", Uuid::new_v4().simple());
    db.execute(
        "INSERT INTO zeroship.users (id, email, email_verified_at, name) \
         VALUES ($1, $2::citext, NOW(), 'P5b User')",
        &[&user_id, &email],
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
    db.execute(
        "INSERT INTO zeroship.apps (id, name, api_key, api_key_hash) \
         VALUES ($1, $2, $3, $4)",
        &[
            &app_id,
            &app_name,
            &format!("api-{app_id}"),
            &format!("hash-{app_id}"),
        ],
    )
    .await
    .expect("seed app");
    let scope_vec = scopes.iter().map(|scope| (*scope).to_string()).collect::<Vec<_>>();
    let secret_hash = hash_api_key(REFRESH_CLIENT_SECRET);
    db.execute(
        "INSERT INTO zeroship.oauth_clients \
            (client_id, client_name, redirect_uris, scopes, skip_consent, hydra_client_id, \
             client_secret_hash, refresh_allowed, token_endpoint_auth_method) \
         VALUES ($1, 'P5b OP refresh test', $2, $3, FALSE, $1, $4, TRUE, 'client_secret_basic')",
        &[&client_id, &vec![REDIRECT_URI.to_string()], &scope_vec, &secret_hash],
    )
    .await
    .expect("seed oauth client");
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
        &[&user_id, &client_id, &scope_vec],
    )
    .await
    .expect("seed oauth grant");
}

async fn cleanup_seeded_rows(db: &Client, user_id: Uuid, app_id: Uuid, client_id: &str) {
    let _ = db
        .execute("DELETE FROM zeroship.token_revocations WHERE client_id = $1", &[&client_id])
        .await;
    let _ = db
        .execute(
            "DELETE FROM zeroship.oauth_refresh_tokens WHERE client_id = $1",
            &[&client_id],
        )
        .await;
    let _ = db
        .execute(
            "DELETE FROM zeroship.oauth_authorization_codes WHERE client_id = $1",
            &[&client_id],
        )
        .await;
    let _ = db
        .execute("DELETE FROM zeroship.oauth_grants WHERE client_id = $1", &[&client_id])
        .await;
    let _ = db
        .execute(
            "DELETE FROM zeroship.idp_sessions WHERE user_id = $1",
            &[&user_id],
        )
        .await;
    let _ = db
        .execute(
            "DELETE FROM zeroship.app_oauth_clients WHERE client_id = $1",
            &[&client_id],
        )
        .await;
    let _ = db
        .execute("DELETE FROM zeroship.oauth_clients WHERE client_id = $1", &[&client_id])
        .await;
    let _ = db.execute("DELETE FROM zeroship.apps WHERE id = $1", &[&app_id]).await;
    let _ = db.execute("DELETE FROM zeroship.users WHERE id = $1", &[&user_id]).await;
}

#[allow(clippy::future_not_send)]
async fn issue_refresh(fx: &Fixture, scope: &str) -> TokenResponse {
    let verifier = pkce_verifier();
    let authorize = send_authorize(fx, scope, &verifier)
        .await
        .expect("authorize response");
    assert_eq!(authorize.status().as_u16(), 303);
    let code = query_param(&location(&authorize), "code").expect("code in redirect");
    let token = token_request(fx, &code, REDIRECT_URI, &verifier)
        .await
        .expect("token response");
    assert_eq!(token.status().as_u16(), 200, "token response status");
    token.json::<TokenResponse>().await.expect("token json")
}

#[allow(clippy::future_not_send)]
async fn send_authorize(
    fx: &Fixture,
    scope: &str,
    verifier: &str,
) -> Result<cyper::Response, cyper::Error> {
    let url = format!(
        "{}/authorize?{}",
        fx.auth_base,
        url::form_urlencoded::Serializer::new(String::new())
            .append_pair("client_id", &fx.client_id)
            .append_pair("response_type", "code")
            .append_pair("scope", scope)
            .append_pair("redirect_uri", REDIRECT_URI)
            .append_pair("state", "state-123")
            .append_pair("nonce", "nonce-123")
            .append_pair("code_challenge", &pkce_challenge_s256(verifier))
            .append_pair("code_challenge_method", "S256")
            .finish()
    );
    cyper::Client::new()
        .request(http::Method::GET, url)
        .expect("build GET /authorize")
        .header("cookie", fx.session_cookie.clone())
        .expect("cookie")
        .send()
        .await
}

#[allow(clippy::future_not_send)]
async fn token_request(
    fx: &Fixture,
    code: &str,
    redirect_uri: &str,
    verifier: &str,
) -> Result<cyper::Response, cyper::Error> {
    let body = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("grant_type", "authorization_code")
        .append_pair("client_id", &fx.client_id)
        .append_pair("code", code)
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("code_verifier", verifier)
        .finish();
    cyper::Client::new()
        .request(http::Method::POST, format!("{}/token", fx.auth_base))
        .expect("build POST /token")
        .header("content-type", "application/x-www-form-urlencoded")
        .expect("content-type")
        .body(body)
        .send()
        .await
}

#[allow(clippy::future_not_send)]
async fn refresh_request(
    fx: &Fixture,
    refresh_token: &str,
    scope: Option<&str>,
) -> Result<cyper::Response, cyper::Error> {
    let mut form = url::form_urlencoded::Serializer::new(String::new());
    form.append_pair("grant_type", "refresh_token")
        .append_pair("refresh_token", refresh_token);
    if let Some(scope) = scope {
        form.append_pair("scope", scope);
    }
    cyper::Client::new()
        .request(http::Method::POST, format!("{}/token", fx.auth_base))
        .expect("build POST /token")
        .header("content-type", "application/x-www-form-urlencoded")
        .expect("content-type")
        .header("authorization", basic_auth(&fx.client_id))
        .expect("authorization")
        .body(form.finish())
        .send()
        .await
}

#[allow(clippy::future_not_send)]
async fn revoke_request(fx: &Fixture, refresh_token: &str) -> Result<cyper::Response, cyper::Error> {
    let body = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("token", refresh_token)
        .append_pair("token_type_hint", "refresh_token")
        .finish();
    cyper::Client::new()
        .request(http::Method::POST, format!("{}/revoke", fx.auth_base))
        .expect("build POST /revoke")
        .header("content-type", "application/x-www-form-urlencoded")
        .expect("content-type")
        .header("authorization", basic_auth(&fx.client_id))
        .expect("authorization")
        .body(body)
        .send()
        .await
}

fn basic_auth(client_id: &str) -> String {
    format!(
        "Basic {}",
        STANDARD.encode(format!("{client_id}:{REFRESH_CLIENT_SECRET}"))
    )
}

async fn refresh_family_id(fx: &Fixture) -> String {
    fx.db
        .query_one(
            "SELECT refresh_family_id \
             FROM zeroship.oauth_refresh_tokens \
             WHERE client_id = $1 AND user_id = $2 \
             LIMIT 1",
            &[&fx.client_id, &fx.user_id],
        )
        .await
        .expect("refresh family id")
        .get("refresh_family_id")
}

async fn expire_idempotency_window(fx: &Fixture, family_id: &str) {
    fx.db
        .execute(
            "UPDATE zeroship.oauth_refresh_tokens \
             SET idem_expires_at = NOW() - INTERVAL '1 second' \
             WHERE refresh_family_id = $1 AND idem_response_enc IS NOT NULL",
            &[&family_id],
        )
        .await
        .expect("expire idempotency cache");
}

async fn assert_family_revoked(fx: &Fixture, family_id: &str) {
    let row = fx
        .db
        .query_one(
            "SELECT COUNT(*) FILTER (WHERE revoked_at IS NOT NULL)::BIGINT AS revoked, \
                    COUNT(*)::BIGINT AS total \
             FROM zeroship.oauth_refresh_tokens WHERE refresh_family_id = $1",
            &[&family_id],
        )
        .await
        .expect("family revoke count");
    let revoked: i64 = row.get("revoked");
    let total: i64 = row.get("total");
    assert!(total > 0);
    assert_eq!(revoked, total, "family must be fully revoked");
}

async fn assert_family_not_revoked(fx: &Fixture, family_id: &str) {
    let row = fx
        .db
        .query_one(
            "SELECT COUNT(*) FILTER (WHERE revoked_at IS NOT NULL)::BIGINT AS revoked \
             FROM zeroship.oauth_refresh_tokens WHERE refresh_family_id = $1",
            &[&family_id],
        )
        .await
        .expect("family revoke count");
    assert_eq!(row.get::<_, i64>("revoked"), 0);
}

fn query_param(raw_url: &str, name: &str) -> Option<String> {
    url::Url::parse(raw_url).ok()?.query_pairs().find_map(|(key, value)| {
        if key == name {
            Some(value.into_owned())
        } else {
            None
        }
    })
}

async fn assert_error(resp: cyper::Response, expected: &str) {
    let body = resp
        .json::<Value>()
        .await
        .expect("oauth error json");
    assert_eq!(body["error"], expected);
}
