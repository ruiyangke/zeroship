//! Rotated OIDC signing-key retention tests against live PostgreSQL.

use compio_postgres::{connect, Client, NoTls};
use ed25519_dalek::SigningKey;
use jsonwebtoken::decode_header;
use serde_json::{json, Value};
use std::sync::Mutex;
use uuid::Uuid;
use zeroship_auth::cron::signing_key_retention;
use zeroship_auth::oidc::metadata::jwks_document;
use zeroship_auth::oidc::{AccessTokenMint, Issuer};

static RETENTION_TEST_LOCK: Mutex<()> = Mutex::new(());

async fn pg() -> Option<Client> {
    let dsn = zeroship_core::declared_env!(
        external,
        "AUTH_DB_URL",
        zeroship_core::config::TestHarness
    )
    .or_else(|| zeroship_core::test_env!("PG_TEST_URL"))?;
    let (client, connection) = connect(&dsn, NoTls).await.expect("connect");
    compio::runtime::spawn(async move {
        if let Err(err) = connection.run().await {
            eprintln!("signing_key_retention test pg connection error: {err}");
        }
    })
    .detach();
    Some(client)
}

fn test_jwk(kid: &str) -> Value {
    json!({
        "alg": "EdDSA",
        "crv": "Ed25519",
        "kid": kid,
        "kty": "OKP",
        "use": "sig",
        "x": "11qYAYLef1GoDEOdOP02JZ6F11QTaDbSdF83au1irXg"
    })
}

async fn seed_key(
    db: &Client,
    kid: &str,
    public_jwk: &Value,
    status: &str,
    expiry_age_secs: i64,
) {
    db.execute(
        "INSERT INTO zeroship.signing_keys \
            (kid, alg, public_jwk, status, activated_at, retiring_at, \
             max_issued_expires_at) \
         VALUES ($1, 'EdDSA', $2, $3, NOW() - INTERVAL '2 days', \
                 NOW() - INTERVAL '1 day', \
                 NOW() - ($4::BIGINT * INTERVAL '1 second'))",
        &[&kid, &public_jwk, &status, &expiry_age_secs],
    )
    .await
    .unwrap_or_else(|err| panic!("seed signing key {kid}: {err}"));
}

async fn cleanup_key(db: &Client, kid: &str) {
    db.execute("DELETE FROM zeroship.signing_keys WHERE kid = $1", &[&kid])
        .await
        .unwrap_or_else(|err| panic!("clean up signing key {kid}: {err}"));
}

async fn key_status(db: &Client, kid: &str) -> (String, Option<String>) {
    let row = db
        .query_one(
            "SELECT status, retired_at::text AS retired_at \
             FROM zeroship.signing_keys WHERE kid = $1",
            &[&kid],
        )
        .await
        .unwrap_or_else(|err| panic!("load signing key {kid}: {err}"));
    (row.get("status"), row.get("retired_at"))
}

async fn jwks_contains(db: &Client, kid: &str) -> bool {
    jwks_document(db)
        .await
        .expect("JWKS document")
        .get("keys")
        .and_then(Value::as_array)
        .expect("JWKS keys array")
        .iter()
        .any(|key| key["kid"] == kid)
}

#[allow(clippy::await_holding_lock, clippy::future_not_send)]
#[compio::test]
async fn retiring_key_inside_horizon_remains_published() {
    let Some(db) = pg().await else {
        zeroship_test_support::skip("skipping signing_key_retention_test (no AUTH_DB_URL)");
        return;
    };
    let _guard = RETENTION_TEST_LOCK.lock().expect("retention test lock");

    let kid = format!("retiring-fresh-{}", Uuid::new_v4());
    let inside_horizon_secs = signing_key_retention::RETENTION_AFTER_EXPIRY_SECS - 1;
    seed_key(&db, &kid, &test_jwk(&kid), "retiring", inside_horizon_secs).await;

    signing_key_retention::tick(&db).await.expect("retention tick");

    assert!(
        jwks_contains(&db, &kid).await,
        "a retiring key inside the retention horizon must remain published"
    );

    cleanup_key(&db, &kid).await;
}

#[allow(clippy::await_holding_lock, clippy::future_not_send)]
#[compio::test]
async fn key_past_horizon_leaves_jwks_with_reason_and_idempotently_keeps_audit_row() {
    let Some(db) = pg().await else {
        zeroship_test_support::skip("skipping signing_key_retention_test (no AUTH_DB_URL)");
        return;
    };
    let _guard = RETENTION_TEST_LOCK.lock().expect("retention test lock");

    let kid = format!("retiring-stale-{}", Uuid::new_v4());
    let past_horizon_secs = signing_key_retention::RETENTION_AFTER_EXPIRY_SECS + 1;
    seed_key(&db, &kid, &test_jwk(&kid), "retiring", past_horizon_secs).await;

    let first = signing_key_retention::tick(&db).await.expect("first retention tick");
    let retired = first
        .retired
        .iter()
        .find(|retired| retired.kid == kid)
        .expect("stale key retired on first tick");
    assert_eq!(retired.reason, signing_key_retention::RETIREMENT_REASON);
    assert!(!jwks_contains(&db, &kid).await, "retired key must leave JWKS");
    let (status, retired_at) = key_status(&db, &kid).await;
    assert_eq!(status, "retired");
    let retired_at = retired_at.expect("retired_at audit timestamp");

    let second = signing_key_retention::tick(&db).await.expect("second retention tick");
    assert!(
        second.retired.iter().all(|retired| retired.kid != kid),
        "a terminal retired row must not be pruned twice"
    );
    assert_eq!(
        key_status(&db, &kid).await,
        ("retired".to_string(), Some(retired_at)),
        "the audit row and original retirement timestamp must survive"
    );

    cleanup_key(&db, &kid).await;
}

#[allow(clippy::await_holding_lock, clippy::future_not_send)]
#[compio::test]
async fn active_and_next_keys_are_never_pruned_regardless_of_age() {
    let Some(db) = pg().await else {
        zeroship_test_support::skip("skipping signing_key_retention_test (no AUTH_DB_URL)");
        return;
    };
    let _guard = RETENTION_TEST_LOCK.lock().expect("retention test lock");

    let active_kid = format!("active-old-{}", Uuid::new_v4());
    let next_kid = format!("next-old-{}", Uuid::new_v4());
    let very_old_secs = 30 * 24 * 60 * 60;
    seed_key(&db, &active_kid, &test_jwk(&active_kid), "active", very_old_secs).await;
    seed_key(&db, &next_kid, &test_jwk(&next_kid), "next", very_old_secs).await;

    let report = signing_key_retention::tick(&db).await.expect("retention tick");
    assert!(report.retired.iter().all(|retired| {
        retired.kid != active_kid && retired.kid != next_kid
    }));
    assert_eq!(key_status(&db, &active_kid).await.0, "active");
    assert_eq!(key_status(&db, &next_kid).await.0, "next");
    assert!(jwks_contains(&db, &active_kid).await);
    assert!(jwks_contains(&db, &next_kid).await);

    cleanup_key(&db, &active_kid).await;
    cleanup_key(&db, &next_kid).await;
}

#[allow(clippy::await_holding_lock, clippy::future_not_send)]
#[compio::test]
async fn issuance_and_prune_never_return_a_token_without_its_published_key() {
    let Some(db) = pg().await else {
        zeroship_test_support::skip("skipping signing_key_retention_test (no AUTH_DB_URL)");
        return;
    };
    let Some(issue_db) = pg().await else {
        unreachable!("the same AUTH_DB_URL disappeared")
    };
    let Some(prune_db) = pg().await else {
        unreachable!("the same AUTH_DB_URL disappeared")
    };
    let _guard = RETENTION_TEST_LOCK.lock().expect("retention test lock");

    let random = *Uuid::new_v4().as_bytes();
    let mut secret = [0u8; 32];
    secret[..16].copy_from_slice(&random);
    secret[16..].copy_from_slice(&random);
    let signing = SigningKey::from_bytes(&secret);
    let issuer = Issuer::from_signing_key(
        &signing,
        [19u8; 32],
        "https://auth.zeroship.test/oauth2".to_string(),
    )
    .expect("issuer");
    let kid = issuer.kid().to_string();
    let past_horizon_secs = signing_key_retention::RETENTION_AFTER_EXPIRY_SECS + 1;
    seed_key(
        &db,
        &kid,
        issuer.public_jwk(),
        "retiring",
        past_horizon_secs,
    )
    .await;

    let user_id = Uuid::new_v4().to_string();
    let scopes = vec!["openid".to_string()];
    let mint = AccessTokenMint {
        user_id: &user_id,
        sector: "https://app.zeroship.test",
        audience: "app:00000000-0000-0000-0000-000000000001",
        client_id: "oac_retention_race",
        scopes: &scopes,
        ttl_secs: Some(60),
    };
    let (issued, report) = futures::join!(
        issuer.issue_access_token(&issue_db, &mint),
        signing_key_retention::tick(&prune_db)
    );
    let report = report.expect("concurrent retention tick");
    let published = jwks_contains(&db, &kid).await;
    let (status, _) = key_status(&db, &kid).await;

    match issued {
        Ok(token) => {
            assert_eq!(
                decode_header(&token).expect("token header").kid.as_deref(),
                Some(kid.as_str())
            );
            assert_eq!(status, "retiring");
            assert!(published, "a returned token's key must still be published");
            assert!(report.retired.iter().all(|retired| retired.kid != kid));
        }
        Err(err) => {
            assert_eq!(status, "retired", "unexpected issuance error: {err}");
            assert!(!published);
            assert!(report.retired.iter().any(|retired| retired.kid == kid));
        }
    }

    cleanup_key(&db, &kid).await;
}
