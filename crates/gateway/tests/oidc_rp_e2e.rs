//! Gateway OIDC RP end-to-end test against the platform OP.
//!
//! This is the P5a-3b-ii shape: no OP admin/client registration. The test
//! seeds a per-app brokered `oac_` client directly in `zeroship.oauth_clients`,
//! boots `crates/auth` in-process with a broker master secret, then drives the
//! native OP `/oauth2/authorize` + `/login` flow. `OidcRp::finish_callback`
//! exchanges the code as that per-app client by deriving the broker secret from the same
//! master and verifies the OP id_token (`iss` with the fixed `/oauth2` prefix,
//! `aud = oac_...`).

use std::sync::Arc;
use std::time::Duration;

use clap::Parser as _;
use compio_postgres::{Client, NoTls};
use ed25519_dalek::SigningKey;
use ntex::web;
use uuid::Uuid;

use zeroship_auth::config::AuthConfig;
use zeroship_auth::headers::SecurityHeaders;
use zeroship_auth::identity::password;
use zeroship_auth::oidc::{BrokerSecrets, Issuer};
use zeroship_auth::server;
use zeroship_gateway::oidc_rp::{BrokerSecret, OidcRp};
use zeroship_gateway::sessions::{create, revoke_app_sessions_for_user, validate, NewSession};

const ISSUER: &str = "https://auth.zeroship.ai/oauth2";
const REDIRECT_URI: &str = "http://127.0.0.1:9999/__zeroship/auth/callback";
const SECTOR: &str = "https://gateway-e2e.zeroship.test";
const PASSWORD: &str = "gateway-test-password-with-enough-bytes-1234";
const BROKER_MASTER: &[u8] = b"gateway-oidc-rp-e2e-broker-master-32-bytes";

fn db_url() -> Option<String> {
    std::env::var("AUTH_DB_URL")
        .or_else(|_| std::env::var("CONTROL_TEST_DB"))
        .ok()
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

fn test_auth_config(db_url: &str) -> AuthConfig {
    let mut cfg = AuthConfig::parse_from([
        "zeroship-auth",
        "--addr",
        "127.0.0.1:0",
        "--db-url",
        db_url,
        "--dev-insecure",
        "--stash-signing-key",
        "test-stash-key-not-for-prod-32bytes!",
        "--mail-from-email",
        "test@zeroship.test",
        "--mail-from-name",
        "Test",
        "--public-url",
        "http://localhost:0",
    ]);
    cfg.resolve(zeroship_core::config::AuthSection::default());
    cfg
}

async fn seed_user_client(
    db: &Client,
    user_id: Uuid,
    app_id: Uuid,
    client_id: &str,
    email: &str,
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
    db.execute(
        "INSERT INTO zeroship.apps (id, name, api_key, api_key_hash) \
         VALUES ($1, $2, $3, $4)",
        &[
            &app_id,
            &format!("gateway-e2e-app-{}", app_id.simple()),
            &format!("api-{app_id}"),
            &format!("hash-{app_id}"),
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
             token_endpoint_auth_method, brokered, refresh_allowed) \
         VALUES ($1, 'Gateway OP e2e', $2, $3, TRUE, 'client_secret_basic', TRUE, TRUE)",
        &[&client_id, &vec![REDIRECT_URI.to_string()], &scopes],
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
        .execute("DELETE FROM zeroship.oauth_grants WHERE client_id = $1", &[&client_id])
        .await;
    let _ = db
        .execute("DELETE FROM zeroship.oauth_clients WHERE client_id = $1", &[&client_id])
        .await;
    let _ = db
        .execute("DELETE FROM zeroship.idp_sessions WHERE user_id = $1", &[&user_id])
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
async fn gateway_oidc_rp_full_dance_against_platform_op() {
    let Some(db_url) = db_url() else {
        eprintln!("[oidc_rp_e2e] skip (AUTH_DB_URL or CONTROL_TEST_DB unset)");
        return;
    };

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

    let signing = SigningKey::from_bytes(&[42u8; 32]);
    let broker =
        BrokerSecrets::new(BROKER_MASTER.to_vec(), None).expect("auth broker secrets");
    let issuer = Arc::new(
        Issuer::from_signing_key(&signing, [9u8; 32], ISSUER.to_string())
            .expect("issuer")
            .with_broker_secrets(broker),
    );
    issuer
        .publish_active_key(&pg_client)
        .await
        .expect("publish active OP key");

    let user_id = Uuid::new_v4();
    let app_id = Uuid::new_v4();
    let client_id = format!("oac_{}", zeroship_core::typed_id::uuid_to_base62(&app_id));
    let email = format!("gw-op-{}@zeroship.test", Uuid::new_v4().simple());
    seed_user_client(&pg_client, user_id, app_id, &client_id, &email).await;

    let mut cfg = test_auth_config(&db_url);
    let key_dir = std::env::temp_dir().join(format!("gateway-op-refresh-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&key_dir).expect("refresh key dir");
    let hash_key_file = key_dir.join("refresh-hmac.keys");
    let idem_key_file = key_dir.join("refresh-idem.key");
    write_secret_file(
        &hash_key_file,
        b"1:000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f",
    );
    write_secret_file(&idem_key_file, b"refresh-idem-key-material-32-bytes");
    cfg.refresh_hash_key_file = Some(hash_key_file);
    cfg.refresh_idem_key_file = Some(idem_key_file);
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

    let (auth_url, stash) =
        rp.build_authorize_redirect(&client_id, "/some/path", REDIRECT_URI);
    assert!(auth_url.starts_with(&format!("{auth_base}/oauth2/authorize?")), "{auth_url}");
    assert!(auth_url.contains(&format!("client_id={client_id}")), "{auth_url}");
    assert!(auth_url.contains("scope=openid+offline_access+email+profile"), "{auth_url}");

    let http = cyper::Client::new();

    let first = http
        .request(http::Method::GET, &auth_url)
        .expect("build GET /authorize")
        .send()
        .await
        .expect("send GET /authorize");
    assert_eq!(first.status().as_u16(), 303);
    let login_loc = location(&first);
    assert!(login_loc.starts_with("/login?return_to="), "login redirect: {login_loc}");
    let return_to = relative_query_param(&login_loc, "return_to").expect("return_to");

    let login_get = http
        .request(http::Method::GET, format!("{auth_base}{login_loc}"))
        .expect("build GET /login")
        .send()
        .await
        .expect("send GET /login");
    assert_eq!(login_get.status().as_u16(), 200);
    let csrf = read_set_cookie(&login_get, "zsidp_csrf").expect("login csrf");
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
        .header("cookie", format!("zsidp_csrf={csrf}"))
        .expect("cookie")
        .body(login_body)
        .send()
        .await
        .expect("send POST /login");
    assert_eq!(login_post.status().as_u16(), 303);
    assert_eq!(location(&login_post), return_to);
    let session = read_set_cookie(&login_post, "zsidp_session").expect("session cookie");

    let final_authorize = http
        .request(http::Method::GET, format!("{auth_base}{return_to}"))
        .expect("build GET /authorize with session")
        .header("cookie", format!("zsidp_session={session}"))
        .expect("cookie")
        .send()
        .await
        .expect("send GET /authorize with session");
    assert_eq!(final_authorize.status().as_u16(), 303);
    let cb_url = location(&final_authorize);
    assert!(cb_url.starts_with(REDIRECT_URI), "callback: {cb_url}");
    assert_eq!(query_param(&cb_url, "state").as_deref(), query_param(&auth_url, "state").as_deref());
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
        matches!(mismatch, Err(zeroship_gateway::oidc_rp::OidcRpError::ClientMismatch)),
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

    let (mut sess_client, sess_connection) =
        compio_postgres::connect(&db_url, NoTls)
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

    let validated = validate(&mut sess_client, session.id, app_id)
        .await
        .expect("validate")
        .expect("session validates");
    assert_eq!(validated.user_id, claims.sub);
    assert_eq!(validated.granted_scopes, granted_scopes);

    revoke_app_sessions_for_user(&mut sess_client, app_id, &claims.sub)
        .await
        .expect("revoke");
    let after_revoke = validate(&mut sess_client, session.id, app_id)
        .await
        .expect("validate post-revoke");
    assert!(after_revoke.is_none(), "session must not validate after revoke");

    cleanup(&pg_client, user_id, app_id, &client_id).await;
    compio::time::sleep(Duration::from_millis(50)).await;
    drop(srv);
}
