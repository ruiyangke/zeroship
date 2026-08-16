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

const TEST_CONTROL_KEY: &str = "platform-mint-http-test-control-key";
const PLATFORM_TOKEN_MAX_TTL_SECS: i64 = 12 * 60 * 60;

#[derive(Debug, Deserialize)]
struct MintResponse {
    access_token: String,
    expires_in: u64,
    scope: String,
}

struct Fixture {
    srv: ntex::web::test::TestServer,
    base_url: String,
    admin_pg: Arc<compio_postgres::Client>,
    issuer: Arc<Issuer>,
    http: cyper::Client,
}

impl Fixture {
    #[allow(clippy::future_not_send)]
    async fn boot(configured_control_key: &str) -> Option<Self> {
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
        cfg.settings.control_key = test_secret(configured_control_key);
        let cfg = Arc::new(cfg);
        let issuer = Arc::new(test_issuer());
        issuer
            .publish_active_key(&admin_pg)
            .await
            .expect("publish active OP key");

        let cfg_state = cfg.clone();
        let pg_state = auth_pg.clone();
        let issuer_state = issuer.clone();
        let refresh_pool_state = zeroship_auth::oidc::refresh::RefreshSessionPool::new(db_url, 4);
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
        "audience": "control.zeroship.ai",
        "client_id": "zeroship-cli",
        "scopes": scopes,
        "ttl_secs": ttl_secs,
    })
}

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn platform_mint_caps_scopes_to_the_principals_stored_grants_over_http() {
    let Some(fx) = Fixture::boot(TEST_CONTROL_KEY).await else {
        zeroship_test_support::skip(
            "[platform_token_mint] skip (need AUTH_DB_URL or CONTROL_TEST_DB)",
        );
        return;
    };
    let principal_id = fx.create_principal(&["apps:read"]).await;

    let (status, raw_body) = fx
        .mint(
            TEST_CONTROL_KEY,
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
    let claims = fx
        .issuer
        .verify_access_token(&minted.access_token)
        .expect("verify minted platform token");
    assert_eq!(claims.sub, principal_id.to_string());
    assert_eq!(claims.scope, "apps:read");
    drop(fx.srv);
}

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn platform_mint_rejects_an_unknown_principal_over_http() {
    let Some(fx) = Fixture::boot(TEST_CONTROL_KEY).await else {
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

    assert_eq!(status, 400, "platform mint response: {raw_body}");
    drop(fx.srv);
}

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn platform_mint_rejects_a_ttl_above_the_ceiling_over_http() {
    let Some(fx) = Fixture::boot(TEST_CONTROL_KEY).await else {
        zeroship_test_support::skip(
            "[platform_token_mint] skip (need AUTH_DB_URL or CONTROL_TEST_DB)",
        );
        return;
    };
    let principal_id = fx.create_principal(&["apps:read"]).await;

    let (status, raw_body) = fx
        .mint(
            TEST_CONTROL_KEY,
            mint_body(
                principal_id,
                &["apps:read"],
                PLATFORM_TOKEN_MAX_TTL_SECS + 1,
            ),
        )
        .await;
    fx.delete_principal(principal_id).await;

    assert_eq!(status, 400, "platform mint response: {raw_body}");
    drop(fx.srv);
}

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn platform_mint_rejects_the_shared_control_key_over_http() {
    let Some(fx) = Fixture::boot(TEST_CONTROL_KEY).await else {
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
