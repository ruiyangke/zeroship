//! Rotated OIDC signing-key retention tests against live PostgreSQL.
//!
//! Almost every assertion here names a kid this run minted, which is what makes
//! them safe on a database every run on this migration set shares. The one
//! thing that is NOT per-run is the sweep: `signing_key_retention::tick`
//! retires every past-horizon key in the database, so a peer run's tick can
//! retire this run's key before this run's tick sees it, and then
//! `report.retired` names it nowhere. That is not a weaker claim to make - the
//! whole point of `concurrent_retirement_cannot_be_undone_by_signer_startup` is
//! that the retirement happened while ITS publisher was paused - so the fix is
//! to take turns on the sweep rather than to stop asserting.
//!
//! MEASURED 2026-08-20, two copies of this file and its three sweep-driving
//! siblings against one database, three pairs: 2 of 3 red, and the first
//! failure of each was
//!     assertion failed: report.retired.iter().any(|retired| retired.kid == kid)
//! The same two runs against a database EACH were green, which is the control
//! that says the database and not the fixture is what they were sharing.
//!
//! A `Mutex` used to guard this and excluded the other THREADS of one process;
//! the lease is a session advisory lock and excludes peer runs too. See
//! [`common::lease_sweep`].

use chrono::{DateTime, Utc};
use compio_postgres::{connect, Client, NoTls};
use ed25519_dalek::SigningKey;
use jsonwebtoken::decode_header;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tracing::field::{Field, Visit};
use tracing::{Event, Subscriber};
use tracing_subscriber::layer::{Context, Layer, SubscriberExt};
use tracing_subscriber::registry::LookupSpan;
use uuid::Uuid;
use zeroship_auth::cron::signing_key_retention;
use zeroship_auth::oidc::metadata::jwks_document;
use zeroship_auth::oidc::{
    AccessTokenMint, IdTokenMint, Issuer, LogoutTokenMint, PrincipalAccessTokenMint,
    PrincipalIdTokenMint,
};

use crate::common;

#[derive(Default)]
struct LogVisitor {
    fields: HashMap<String, String>,
}

impl Visit for LogVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.fields.insert(field.name().to_string(), value.to_string());
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.fields
            .insert(field.name().to_string(), format!("{value:?}"));
    }
}

struct LogCapture {
    events: Arc<Mutex<Vec<HashMap<String, String>>>>,
}

impl<S> Layer<S> for LogCapture
where
    S: Subscriber + for<'lookup> LookupSpan<'lookup>,
{
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let mut visitor = LogVisitor::default();
        event.record(&mut visitor);
        self.events
            .lock()
            .expect("retention log buffer mutex")
            .push(visitor.fields);
    }
}

async fn pg() -> Option<Client> {
    let dsn = zeroship_core::config::test_database_url_opt()?;
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

async fn seed_key_without_watermark(
    db: &Client,
    kid: &str,
    public_jwk: &Value,
    retiring_age_secs: i64,
) {
    db.execute(
        "INSERT INTO zeroship.signing_keys \
            (kid, alg, public_jwk, status, activated_at, retiring_at, \
             max_issued_expires_at) \
         VALUES ($1, 'EdDSA', $2, 'retiring', NOW() - INTERVAL '2 days', \
                 NOW() - ($3::BIGINT * INTERVAL '1 second'), NULL)",
        &[&kid, &public_jwk, &retiring_age_secs],
    )
    .await
    .unwrap_or_else(|err| panic!("seed signing key without watermark {kid}: {err}"));
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

async fn watermark_exists(db: &Client, kid: &str) -> bool {
    db.query_one(
        "SELECT max_issued_expires_at IS NOT NULL AS present \
         FROM zeroship.signing_keys WHERE kid = $1",
        &[&kid],
    )
    .await
    .unwrap_or_else(|err| panic!("load signing key watermark {kid}: {err}"))
    .get("present")
}

async fn watermark(db: &Client, kid: &str) -> DateTime<Utc> {
    db.query_one(
        "SELECT max_issued_expires_at FROM zeroship.signing_keys WHERE kid = $1",
        &[&kid],
    )
    .await
    .unwrap_or_else(|err| panic!("load signing key watermark {kid}: {err}"))
    .get("max_issued_expires_at")
}

async fn clear_watermark(db: &Client, kid: &str) {
    db.execute(
        "UPDATE zeroship.signing_keys SET max_issued_expires_at = NULL WHERE kid = $1",
        &[&kid],
    )
    .await
    .unwrap_or_else(|err| panic!("clear signing key watermark {kid}: {err}"));
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

#[allow(clippy::future_not_send)]
#[compio::test]
async fn retiring_key_inside_horizon_remains_published() {
    let Some(db) = pg().await else {
        zeroship_test_support::skip("skipping signing_key_retention_test (no test database (set PG_TEST_URL or run tests/provision_test_backends.sh))");
        return;
    };
    let _guard = common::lease_sweep(common::sweep_lock::SIGNING_KEY_RETENTION).await;

    let kid = format!("retiring-fresh-{}", Uuid::new_v4());
    let inside_horizon_secs = signing_key_retention::RETENTION_AFTER_EXPIRY_SECS - 60;
    seed_key(&db, &kid, &test_jwk(&kid), "retiring", inside_horizon_secs).await;

    signing_key_retention::tick(&db).await.expect("retention tick");

    assert!(
        jwks_contains(&db, &kid).await,
        "a retiring key inside the retention horizon must remain published"
    );

    cleanup_key(&db, &kid).await;
}

#[allow(clippy::future_not_send)]
#[compio::test]
async fn key_past_horizon_leaves_jwks_with_reason_and_idempotently_keeps_audit_row() {
    let Some(db) = pg().await else {
        zeroship_test_support::skip("skipping signing_key_retention_test (no test database (set PG_TEST_URL or run tests/provision_test_backends.sh))");
        return;
    };
    let _guard = common::lease_sweep(common::sweep_lock::SIGNING_KEY_RETENTION).await;

    let kid = format!("retiring-stale-{}", Uuid::new_v4());
    let past_horizon_secs = signing_key_retention::RETENTION_AFTER_EXPIRY_SECS + 60;
    seed_key(&db, &kid, &test_jwk(&kid), "retiring", past_horizon_secs).await;

    let events = Arc::new(Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::registry().with(LogCapture {
        events: Arc::clone(&events),
    });
    let trace_guard = tracing::subscriber::set_default(subscriber);
    let first = signing_key_retention::tick(&db).await.expect("first retention tick");
    drop(trace_guard);
    let retired = first
        .retired
        .iter()
        .find(|retired| retired.kid == kid)
        .expect("stale key retired on first tick");
    assert_eq!(retired.reason, signing_key_retention::RETIREMENT_REASON);
    assert!(
        events
            .lock()
            .expect("retention log buffer mutex")
            .iter()
            .any(|event| {
                event.get("kid") == Some(&kid)
                    && event.get("reason").map(String::as_str)
                        == Some(signing_key_retention::RETIREMENT_REASON)
            }),
        "retirement log must identify the key and structured reason"
    );
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

#[allow(clippy::future_not_send)]
#[compio::test]
async fn active_and_next_keys_are_never_pruned_regardless_of_age() {
    let Some(db) = pg().await else {
        zeroship_test_support::skip("skipping signing_key_retention_test (no test database (set PG_TEST_URL or run tests/provision_test_backends.sh))");
        return;
    };
    let _guard = common::lease_sweep(common::sweep_lock::SIGNING_KEY_RETENTION).await;

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

#[allow(clippy::future_not_send)]
#[compio::test]
async fn missing_watermark_uses_full_horizon_from_retiring_at() {
    let Some(db) = pg().await else {
        zeroship_test_support::skip("skipping signing_key_retention_test (no test database (set PG_TEST_URL or run tests/provision_test_backends.sh))");
        return;
    };
    let _guard = common::lease_sweep(common::sweep_lock::SIGNING_KEY_RETENTION).await;

    let inside_kid = format!("retiring-null-fresh-{}", Uuid::new_v4());
    let past_kid = format!("retiring-null-stale-{}", Uuid::new_v4());
    seed_key_without_watermark(
        &db,
        &inside_kid,
        &test_jwk(&inside_kid),
        signing_key_retention::RETENTION_HORIZON_SECS - 60,
    )
    .await;
    seed_key_without_watermark(
        &db,
        &past_kid,
        &test_jwk(&past_kid),
        signing_key_retention::RETENTION_HORIZON_SECS + 60,
    )
    .await;

    let report = signing_key_retention::tick(&db).await.expect("retention tick");

    assert!(jwks_contains(&db, &inside_kid).await);
    assert_eq!(key_status(&db, &inside_kid).await.0, "retiring");
    assert!(report.retired.iter().all(|retired| retired.kid != inside_kid));
    assert!(!jwks_contains(&db, &past_kid).await);
    assert_eq!(key_status(&db, &past_kid).await.0, "retired");
    assert!(report.retired.iter().any(|retired| retired.kid == past_kid));

    cleanup_key(&db, &inside_kid).await;
    cleanup_key(&db, &past_kid).await;
}

#[allow(clippy::future_not_send)]
#[compio::test]
async fn issuance_and_prune_never_return_a_token_without_its_published_key() {
    let Some(db) = pg().await else {
        zeroship_test_support::skip("skipping signing_key_retention_test (no test database (set PG_TEST_URL or run tests/provision_test_backends.sh))");
        return;
    };
    let Some(issue_db) = pg().await else {
        unreachable!("the same test database URL disappeared")
    };
    let Some(prune_db) = pg().await else {
        unreachable!("the same test database URL disappeared")
    };
    let _guard = common::lease_sweep(common::sweep_lock::SIGNING_KEY_RETENTION).await;

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
    let past_horizon_secs = signing_key_retention::RETENTION_AFTER_EXPIRY_SECS + 60;
    seed_key(
        &db,
        &kid,
        issuer.public_jwk(),
        "retiring",
        past_horizon_secs,
    )
    .await;

    let (proof, person_id) = common::validated_session(&db, "retention-race").await;
    let user_id = person_id;
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
        issuer.issue_access_token(&issue_db, &mint, &proof),
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

#[allow(clippy::future_not_send)]
#[compio::test]
async fn every_production_token_kind_advances_the_key_watermark() {
    let Some(db) = pg().await else {
        zeroship_test_support::skip("skipping signing_key_retention_test (no test database (set PG_TEST_URL or run tests/provision_test_backends.sh))");
        return;
    };
    let _guard = common::lease_sweep(common::sweep_lock::SIGNING_KEY_RETENTION).await;

    let random = *Uuid::new_v4().as_bytes();
    let mut secret = [0u8; 32];
    secret[..16].copy_from_slice(&random);
    secret[16..].copy_from_slice(&random);
    let signing = SigningKey::from_bytes(&secret);
    let issuer = Issuer::from_signing_key(
        &signing,
        [29u8; 32],
        "https://auth.zeroship.test/oauth2".to_string(),
    )
    .expect("issuer");
    let kid = issuer.kid().to_string();
    seed_key(&db, &kid, issuer.public_jwk(), "active", 1).await;
    clear_watermark(&db, &kid).await;

    let (proof, person_id) = common::validated_session(&db, "retention-kinds").await;
    let user_id = person_id;
    let scopes = vec!["openid".to_string()];
    let access_token = issuer
        .issue_access_token(
            &db,
            &AccessTokenMint {
                user_id: &user_id,
                sector: "https://app.zeroship.test",
                audience: "app:00000000-0000-0000-0000-000000000001",
                client_id: "oac_retention_kinds",
                scopes: &scopes,
                ttl_secs: Some(60),
            },
            &proof,
        )
        .await
        .expect("issue access token");
    assert!(watermark_exists(&db, &kid).await);

    clear_watermark(&db, &kid).await;
    issuer
        .issue_principal_access_token(
            &db,
            &PrincipalAccessTokenMint {
                principal_id: &user_id,
                audience: "control.zeroship.test",
                client_id: "zeroship-cli",
                scopes: &scopes,
                ttl_secs: Some(60),
            },
            &proof,
        )
        .await
        .expect("issue principal access token");
    assert!(watermark_exists(&db, &kid).await);

    clear_watermark(&db, &kid).await;
    issuer
        .issue_id_token(
            &db,
            &IdTokenMint {
                user_id: &user_id,
                sector: "https://app.zeroship.test",
                client_id: "oac_retention_kinds",
                sid: "sid-retention-kinds",
                nonce: "nonce-retention-kinds",
                access_token: &access_token,
                auth_time: None,
                amr: None,
                acr: None,
                email: None,
                email_verified: None,
                name: None,
                picture: None,
                ttl_secs: Some(60),
            },
            &proof,
        )
        .await
        .expect("issue ID token");
    assert!(watermark_exists(&db, &kid).await);

    clear_watermark(&db, &kid).await;
    issuer
        .issue_principal_id_token(
            &db,
            &PrincipalIdTokenMint {
                principal_id: &user_id,
                client_id: "oac_retention_kinds",
                sid: "sid-retention-kinds",
                nonce: "nonce-retention-kinds",
                access_token: &access_token,
                auth_time: None,
                amr: None,
                acr: None,
                email: None,
                email_verified: None,
                name: None,
                picture: None,
                ttl_secs: Some(60),
            },
            &proof,
        )
        .await
        .expect("issue principal ID token");
    assert!(watermark_exists(&db, &kid).await);

    clear_watermark(&db, &kid).await;
    issuer
        .issue_logout_token(
            &db,
            &LogoutTokenMint {
                client_id: "oac_retention_kinds",
                sub: Some(user_id.as_str()),
                sid: None,
                ttl_secs: Some(60),
            },
        )
        .await
        .expect("issue logout token");
    assert!(watermark_exists(&db, &kid).await);

    clear_watermark(&db, &kid).await;
    issuer
        .issue_principal_access_token(
            &db,
            &PrincipalAccessTokenMint {
                principal_id: &user_id,
                audience: "control.zeroship.test",
                client_id: "zeroship-cli",
                scopes: &scopes,
                ttl_secs: Some(3_600),
            },
            &proof,
        )
        .await
        .expect("issue long-lived token");
    let long_watermark = watermark(&db, &kid).await;
    issuer
        .issue_logout_token(
            &db,
            &LogoutTokenMint {
                client_id: "oac_retention_kinds",
                sub: Some(user_id.as_str()),
                sid: None,
                ttl_secs: Some(60),
            },
        )
        .await
        .expect("issue later short-lived token");
    assert_eq!(
        watermark(&db, &kid).await,
        long_watermark,
        "a later short-lived token must not move the watermark backward"
    );

    db.execute(
        "UPDATE zeroship.signing_keys SET status = 'retired' WHERE kid = $1",
        &[&kid],
    )
    .await
    .expect("retire signing key");
    let refused = issuer
        .issue_access_token(
            &db,
            &AccessTokenMint {
                user_id: &user_id,
                sector: "https://app.zeroship.test",
                audience: "app:00000000-0000-0000-0000-000000000001",
                client_id: "oac_retention_kinds",
                scopes: &scopes,
                ttl_secs: Some(60),
            },
            &proof,
        )
        .await;
    assert!(refused.is_err(), "terminal retired key issued a token");

    cleanup_key(&db, &kid).await;
}

#[allow(clippy::future_not_send)]
#[compio::test]
async fn concurrent_retirement_cannot_be_undone_by_signer_startup() {
    let Some(db) = pg().await else {
        zeroship_test_support::skip("skipping signing_key_retention_test (no test database (set PG_TEST_URL or run tests/provision_test_backends.sh))");
        return;
    };
    let Some(publish_db) = pg().await else {
        unreachable!("the same test database URL disappeared")
    };
    let Some(prune_db) = pg().await else {
        unreachable!("the same test database URL disappeared")
    };
    let _guard = common::lease_sweep(common::sweep_lock::SIGNING_KEY_RETENTION).await;

    let random = *Uuid::new_v4().as_bytes();
    let mut secret = [0u8; 32];
    secret[..16].copy_from_slice(&random);
    secret[16..].copy_from_slice(&random);
    let signing = SigningKey::from_bytes(&secret);
    let issuer = Issuer::from_signing_key(
        &signing,
        [23u8; 32],
        "https://auth.zeroship.test/oauth2".to_string(),
    )
    .expect("issuer");
    let kid = issuer.kid().to_string();
    let past_horizon_secs = signing_key_retention::RETENTION_AFTER_EXPIRY_SECS + 60;
    seed_key(
        &db,
        &kid,
        issuer.public_jwk(),
        "retiring",
        past_horizon_secs,
    )
    .await;

    let suffix = Uuid::new_v4().simple().to_string();
    let function_name = format!("test_pause_signing_key_activation_{suffix}");
    let trigger_name = format!("test_pause_signing_key_activation_{suffix}");
    let application_name = format!("retention-publisher-{suffix}");
    // A PER-RUN key, and the opposite call to the lease above. That lock is a
    // mutex and has to be a fixed number; this one is a private rendezvous
    // between this run's own two sessions - the trigger parks the publisher on
    // it until the retirement has happened - and a number every run shares
    // makes two runs block on each other for no reason. Worse than "no reason":
    // the key was `8_264_731` for every run, and the wait lands squarely
    // between the peer's seed and the peer's tick, so the peer's past-horizon
    // key was already retired by THIS run's sweep when it finally got the lock.
    // That is how the fixed key turned an occasional race into the near-certain
    // `report.retired ... any(kid)` failure in the module header.
    let lock_id = i64::from_le_bytes(
        Uuid::new_v4().as_bytes()[..8]
            .try_into()
            .expect("eight bytes of a uuid"),
    );
    db.query_one("SELECT pg_advisory_lock($1)", &[&lock_id])
        .await
        .expect("hold test activation lock");
    db.execute(
        &format!(
            "CREATE FUNCTION zeroship.{function_name}() RETURNS trigger \
             LANGUAGE plpgsql AS $$ \
             BEGIN \
               IF current_setting('application_name') = '{application_name}' THEN \
                 PERFORM pg_advisory_xact_lock({lock_id}); \
               END IF; \
               RETURN NULL; \
             END \
             $$"
        ),
        &[],
    )
    .await
    .expect("create activation pause function");
    db.execute(
        &format!(
            "CREATE TRIGGER {trigger_name} BEFORE UPDATE ON zeroship.signing_keys \
             FOR EACH STATEMENT EXECUTE FUNCTION zeroship.{function_name}()"
        ),
        &[],
    )
    .await
    .expect("create activation pause trigger");
    publish_db
        .execute(
            "SELECT set_config('application_name', $1, false)",
            &[&application_name],
        )
        .await
        .expect("name publisher connection");

    let publish = issuer.publish_active_key(&publish_db);
    let retire_then_release = async {
        let mut publisher_blocked = false;
        for _ in 0..500 {
            let row = db
                .query_one(
                    "SELECT EXISTS ( \
                         SELECT 1 FROM pg_stat_activity \
                         WHERE application_name = $1 \
                           AND wait_event_type = 'Lock' \
                           AND wait_event = 'advisory' \
                           AND query LIKE 'UPDATE zeroship.signing_keys%' \
                     ) AS blocked",
                    &[&application_name],
                )
                .await
                .expect("observe blocked publisher");
            publisher_blocked = row.get("blocked");
            if publisher_blocked {
                break;
            }
            compio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(publisher_blocked, "publisher did not reach the forced race window");
        let report = signing_key_retention::tick(&prune_db)
            .await
            .expect("retire while publisher is paused");
        let unlocked: bool = db
            .query_one("SELECT pg_advisory_unlock($1) AS unlocked", &[&lock_id])
            .await
            .expect("release test activation lock")
            .get("unlocked");
        assert!(unlocked, "test activation lock was not held");
        report
    };
    let (publish_result, report) = futures::join!(publish, retire_then_release);

    db.execute(
        &format!("DROP TRIGGER {trigger_name} ON zeroship.signing_keys"),
        &[],
    )
    .await
    .expect("drop activation pause trigger");
    db.execute(
        &format!("DROP FUNCTION zeroship.{function_name}()"),
        &[],
    )
    .await
    .expect("drop activation pause function");

    assert!(publish_result.is_err(), "retired key was reactivated");
    assert!(report.retired.iter().any(|retired| retired.kid == kid));
    assert_eq!(key_status(&db, &kid).await.0, "retired");
    assert!(!jwks_contains(&db, &kid).await);

    cleanup_key(&db, &kid).await;
}
