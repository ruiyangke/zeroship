//! Real-HTTP authorization checks for the internal platform-token mint.

mod common;

use std::sync::Arc;

use ed25519_dalek::SigningKey;
use ntex::web;
use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

use common::{test_auth_config, test_secret};
use zeroship_auth::headers::SecurityHeaders;
use zeroship_auth::oidc::{Issuer, ACCESS_TOKEN_TTL_SECS};
use zeroship_auth::server;
use zeroship_auth::store::users;
use zeroship_core::device_grant::{PLATFORM_CLI_CLIENT_ID, PLATFORM_TOKEN_MAX_TTL_SECS};

const TEST_CONTROL_KEY: &str = "platform-mint-http-test-control-key";
const TEST_PLATFORM_MINT_KEY: &str = "platform-mint-http-test-dedicated-key";

#[derive(Debug, Deserialize)]
struct MintResponse {
    access_token: String,
    expires_in: u64,
    scope: String,
    client_id: String,
}

struct Fixture {
    srv: ntex::web::test::TestServer,
    base_url: String,
    db_url: String,
    admin_pg: Arc<compio_postgres::Client>,
    issuer: Arc<Issuer>,
    http: cyper::Client,
}

impl Fixture {
    #[allow(clippy::future_not_send)]
    async fn boot() -> Option<Self> {
        let db_url = zeroship_core::declared_env!(
            external,
            "AUTH_DB_URL",
            zeroship_core::config::TestHarness
        )
        .or_else(|| zeroship_core::test_env!("CONTROL_TEST_DB"))?;

        let (admin_client, admin_connection) =
            compio_postgres::connect(&db_url, compio_postgres::NoTls)
                .await
                .expect("connect admin pg");
        compio::runtime::spawn(async move {
            if let Err(err) = admin_connection.run().await {
                eprintln!("[platform_token_mint] admin pg driver: {err}");
            }
        })
        .detach();
        let admin_pg = Arc::new(admin_client);

        let (auth_client, auth_connection) =
            compio_postgres::connect(&db_url, compio_postgres::NoTls)
                .await
                .expect("connect auth pg");
        compio::runtime::spawn(async move {
            if let Err(err) = auth_connection.run().await {
                eprintln!("[platform_token_mint] auth pg driver: {err}");
            }
        })
        .detach();
        auth_client
            .execute("SET ROLE zeroship_auth", &[])
            .await
            .expect("assume the production auth database role");
        let auth_pg = Arc::new(auth_client);

        let mut cfg = test_auth_config(&db_url);
        cfg.settings.platform_mint_key = test_secret(TEST_PLATFORM_MINT_KEY);
        let cfg = Arc::new(cfg);
        let issuer = Arc::new(test_issuer());
        issuer
            .publish_active_key(&admin_pg)
            .await
            .expect("publish active OP key");

        let cfg_state = cfg.clone();
        let pg_state = auth_pg.clone();
        let issuer_state = issuer.clone();
        let refresh_pool_state =
            zeroship_auth::oidc::refresh::RefreshSessionPool::new(db_url.clone(), 4);
        let srv = web::test::server(move || {
            let cfg_state = cfg_state.clone();
            let pg_state = pg_state.clone();
            let issuer_state = issuer_state.clone();
            let refresh_pool_state = refresh_pool_state.clone();
            async move {
                web::App::new()
                    .state(cfg_state)
                    .state(pg_state)
                    .state(issuer_state)
                    .state(refresh_pool_state)
                    .middleware(SecurityHeaders::default())
                    .configure(server::configure(false, false))
            }
        })
        .await;
        let base_url = srv.url("").trim_end_matches('/').to_string();

        Some(Self {
            srv,
            base_url,
            db_url,
            admin_pg,
            issuer,
            http: cyper::Client::new(),
        })
    }

    async fn create_principal(&self, grants: &[&str]) -> Uuid {
        let email = format!("platform-mint-{}@zeroship.test", Uuid::new_v4().simple());
        let user = users::create(&self.admin_pg, &email, "Platform Mint Test", None)
            .await
            .expect("create platform principal");
        for grant in grants {
            self.admin_pg
                .execute(
                    "INSERT INTO zeroship.principal_grants (principal_id, grant_name) VALUES ($1, $2)",
                    &[&user.id, grant],
                )
                .await
                .expect("grant platform scope");
        }
        user.id
    }

    async fn delete_principal(&self, principal_id: Uuid) {
        self.admin_pg
            .execute(
                "DELETE FROM zeroship.principal_grants WHERE principal_id = $1",
                &[&principal_id],
            )
            .await
            .expect("delete principal grants");
        self.admin_pg
            .execute("DELETE FROM zeroship.users WHERE id = $1", &[&principal_id])
            .await
            .expect("delete platform principal");
    }

    async fn disable_principal(&self, principal_id: Uuid) {
        self.admin_pg
            .execute(
                "UPDATE zeroship.users SET disabled_at = NOW() WHERE id = $1",
                &[&principal_id],
            )
            .await
            .expect("disable platform principal");
    }

    async fn lock_principal(&self, principal_id: Uuid) {
        self.admin_pg
            .execute(
                "UPDATE zeroship.users SET locked_until = NOW() + INTERVAL '1 hour' WHERE id = $1",
                &[&principal_id],
            )
            .await
            .expect("lock platform principal");
    }

    async fn mint(&self, bearer: &str, body: serde_json::Value) -> (u16, String) {
        let response = self
            .http
            .request(
                http::Method::POST,
                format!("{}/internal/platform-token", self.base_url),
            )
            .expect("build platform mint request")
            .header("content-type", "application/json")
            .expect("content type")
            .header("authorization", format!("Bearer {bearer}"))
            .expect("authorization")
            .body(serde_json::to_vec(&body).expect("encode mint request"))
            .send()
            .await
            .expect("send platform mint request");
        let status = response.status().as_u16();
        let body = response.text().await.expect("read platform mint response");
        (status, body)
    }
}

fn test_issuer() -> Issuer {
    let signing = SigningKey::from_bytes(&[91_u8; 32]);
    Issuer::from_signing_key(
        &signing,
        [37_u8; 32],
        "https://auth.zeroship.test/oauth2".into(),
    )
    .expect("issuer")
}

fn mint_body(principal_id: Uuid, scopes: &[&str], ttl_secs: i64) -> serde_json::Value {
    json!({
        "principal_id": principal_id,
        "scopes": scopes,
        "ttl_secs": ttl_secs,
    })
}

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn platform_mint_caps_scopes_to_the_principals_stored_grants_over_http() {
    let Some(fx) = Fixture::boot().await else {
        zeroship_test_support::skip(
            "[platform_token_mint] skip (need AUTH_DB_URL or CONTROL_TEST_DB)",
        );
        return;
    };
    let principal_id = fx.create_principal(&["apps:read"]).await;

    let (status, raw_body) = fx
        .mint(
            TEST_PLATFORM_MINT_KEY,
            mint_body(
                principal_id,
                &["apps:read", "apps:deploy"],
                ACCESS_TOKEN_TTL_SECS,
            ),
        )
        .await;
    fx.delete_principal(principal_id).await;

    assert_eq!(status, 200, "platform mint response: {raw_body}");
    let minted: MintResponse = serde_json::from_str(&raw_body).expect("decode mint response");
    assert_eq!(minted.expires_in, ACCESS_TOKEN_TTL_SECS as u64);
    assert_eq!(minted.scope, "apps:read");
    assert_eq!(minted.client_id, PLATFORM_CLI_CLIENT_ID);
    let claims = fx
        .issuer
        .verify_access_token(&minted.access_token)
        .expect("verify minted platform token");
    assert_eq!(claims.sub, principal_id.to_string());
    assert_eq!(claims.aud, "control.zeroship.ai");
    assert_eq!(claims.client_id, PLATFORM_CLI_CLIENT_ID);
    assert_eq!(claims.scope, "apps:read");
    drop(fx.srv);
}

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn platform_mint_rejects_an_unknown_principal_over_http() {
    let Some(fx) = Fixture::boot().await else {
        zeroship_test_support::skip(
            "[platform_token_mint] skip (need AUTH_DB_URL or CONTROL_TEST_DB)",
        );
        return;
    };

    let (status, raw_body) = fx
        .mint(
            TEST_PLATFORM_MINT_KEY,
            mint_body(Uuid::new_v4(), &[], ACCESS_TOKEN_TTL_SECS),
        )
        .await;

    assert_eq!(status, 400, "platform mint response: {raw_body}");
    drop(fx.srv);
}

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn platform_mint_rejects_a_disabled_principal_over_http() {
    let Some(fx) = Fixture::boot().await else {
        zeroship_test_support::skip(
            "[platform_token_mint] skip (need AUTH_DB_URL or CONTROL_TEST_DB)",
        );
        return;
    };
    let principal_id = fx.create_principal(&["apps:read"]).await;
    fx.disable_principal(principal_id).await;

    let (status, raw_body) = fx
        .mint(
            TEST_PLATFORM_MINT_KEY,
            mint_body(principal_id, &["apps:read"], ACCESS_TOKEN_TTL_SECS),
        )
        .await;
    fx.delete_principal(principal_id).await;

    assert_eq!(status, 400, "platform mint response: {raw_body}");
    drop(fx.srv);
}

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn platform_mint_rejects_a_locked_principal_over_http() {
    let Some(fx) = Fixture::boot().await else {
        zeroship_test_support::skip(
            "[platform_token_mint] skip (need AUTH_DB_URL or CONTROL_TEST_DB)",
        );
        return;
    };
    let principal_id = fx.create_principal(&["apps:read"]).await;
    fx.lock_principal(principal_id).await;

    let (status, raw_body) = fx
        .mint(
            TEST_PLATFORM_MINT_KEY,
            mint_body(principal_id, &["apps:read"], ACCESS_TOKEN_TTL_SECS),
        )
        .await;
    fx.delete_principal(principal_id).await;

    assert_eq!(status, 400, "platform mint response: {raw_body}");
    drop(fx.srv);
}

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn platform_mint_rejects_a_ttl_above_the_ceiling_over_http() {
    let Some(fx) = Fixture::boot().await else {
        zeroship_test_support::skip(
            "[platform_token_mint] skip (need AUTH_DB_URL or CONTROL_TEST_DB)",
        );
        return;
    };
    let principal_id = fx.create_principal(&["apps:read"]).await;

    let (status, raw_body) = fx
        .mint(
            TEST_PLATFORM_MINT_KEY,
            mint_body(
                principal_id,
                &["apps:read"],
                PLATFORM_TOKEN_MAX_TTL_SECS + 1,
            ),
        )
        .await;

    assert_eq!(status, 400, "platform mint response: {raw_body}");

    let (status, raw_body) = fx
        .mint(
            TEST_PLATFORM_MINT_KEY,
            mint_body(principal_id, &["apps:read"], PLATFORM_TOKEN_MAX_TTL_SECS),
        )
        .await;
    fx.delete_principal(principal_id).await;

    assert_eq!(status, 200, "platform mint response: {raw_body}");
    let minted: MintResponse = serde_json::from_str(&raw_body).expect("decode mint response");
    let claims = fx
        .issuer
        .verify_access_token(&minted.access_token)
        .expect("verify maximum-lifetime platform token");
    assert_eq!(claims.exp - claims.iat, PLATFORM_TOKEN_MAX_TTL_SECS);
    drop(fx.srv);
}

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn platform_mint_rejects_the_shared_control_key_over_http() {
    let Some(fx) = Fixture::boot().await else {
        zeroship_test_support::skip(
            "[platform_token_mint] skip (need AUTH_DB_URL or CONTROL_TEST_DB)",
        );
        return;
    };

    let (status, raw_body) = fx
        .mint(
            TEST_CONTROL_KEY,
            mint_body(Uuid::new_v4(), &[], ACCESS_TOKEN_TTL_SECS),
        )
        .await;

    assert_eq!(status, 401, "platform mint response: {raw_body}");
    drop(fx.srv);
}

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn platform_mint_holds_the_user_lock_until_the_token_is_signed() {
    let Some(fx) = Fixture::boot().await else {
        zeroship_test_support::skip(
            "[platform_token_mint] skip (need AUTH_DB_URL or CONTROL_TEST_DB)",
        );
        return;
    };
    let principal_id = fx.create_principal(&["apps:read"]).await;
    let (mut blocker, blocker_connection) =
        compio_postgres::connect(&fx.db_url, compio_postgres::NoTls)
            .await
            .expect("connect lock blocker");
    compio::runtime::spawn(async move {
        let _ = blocker_connection.run().await;
    })
    .detach();
    let blocker_tx = blocker.transaction().await.expect("begin lock blocker");
    blocker_tx
        .execute(
            "SELECT pg_advisory_xact_lock(2052390913::INT4, hashtext($1::text))",
            &[&principal_id.to_string()],
        )
        .await
        .expect("hold user lock");

    let url = format!("{}/internal/platform-token", fx.base_url);
    let request_body = serde_json::to_vec(&mint_body(
        principal_id,
        &["apps:read"],
        ACCESS_TOKEN_TTL_SECS,
    ))
    .expect("encode mint request");
    let mint_task = compio::runtime::spawn(async move {
        let response = cyper::Client::new()
            .request(http::Method::POST, url)
            .expect("build platform mint request")
            .header("content-type", "application/json")
            .expect("content type")
            .header("authorization", format!("Bearer {TEST_PLATFORM_MINT_KEY}"))
            .expect("authorization")
            .body(request_body)
            .send()
            .await
            .expect("send platform mint request");
        response.status().as_u16()
    });

    let mut observed_wait = false;
    for _ in 0..200 {
        let waiting: bool = fx
            .admin_pg
            .query_one(
                "SELECT EXISTS ( \
                   SELECT 1 FROM pg_stat_activity \
                   WHERE datname = current_database() \
                     AND wait_event_type = 'Lock' \
                     AND wait_event = 'advisory' \
                     AND query LIKE 'SELECT pg_advisory_xact_lock%')",
                &[],
            )
            .await
            .expect("inspect platform mint wait")
            .get(0);
        if waiting {
            observed_wait = true;
            break;
        }
        compio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert!(
        observed_wait,
        "platform mint signed without waiting for the user lifecycle lock"
    );
    blocker_tx.rollback().await.expect("release user lock");
    assert_eq!(mint_task.await.expect("join platform mint"), 200);

    fx.delete_principal(principal_id).await;
    drop(fx.srv);
}
