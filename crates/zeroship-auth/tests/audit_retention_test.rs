//! Audit retention and transaction-scoped tamper protection on owned databases.

use crate::common::database::Database;
use compio_postgres::{error::SqlState, Client};
use zeroship_auth::cron::audit_retention;

async fn seed_event(client: &Client, event_type: &str, age_days: i32) -> i64 {
    client
        .query_one(
            "INSERT INTO zeroship.audit_events (event_type, outcome, occurred_at) \
             VALUES ($1, 'success', NOW() - make_interval(days => $2)) RETURNING id",
            &[&event_type, &age_days],
        )
        .await
        .expect("seed audit event")
        .get(0)
}

async fn retained_events(client: &Client) -> Vec<i64> {
    client
        .query("SELECT id FROM zeroship.audit_events ORDER BY id", &[])
        .await
        .expect("read retained audit events")
        .iter()
        .map(|row| row.get(0))
        .collect()
}

async fn assert_tamper_protection(client: &Client, event_id: i64) {
    let setting: String = client
        .query_one(
            "SELECT current_setting('zeroship.audit_retention', true)",
            &[],
        )
        .await
        .expect("read retention privilege after transaction")
        .get(0);
    assert_eq!(
        setting, "",
        "retention privilege must reset after the transaction"
    );

    let error = client
        .execute(
            "DELETE FROM zeroship.audit_events WHERE id = $1",
            &[&event_id],
        )
        .await
        .expect_err("ordinary deletion must hit the append-only trigger");
    let database_error = error.as_db_error().expect("PostgreSQL tamper refusal");
    assert_eq!(database_error.code(), &SqlState::INSUFFICIENT_PRIVILEGE);
    assert_eq!(
        database_error.message(),
        "zeroship.audit_events is append-only"
    );
    assert!(retained_events(client).await.contains(&event_id));
}

#[compio::test]
async fn retention_applies_each_event_class_window() {
    Database::run(async |database| {
        let client = database.connect_as_auth().await;
        let mut expected = Vec::new();
        for (event_type, retention_days) in [
            ("login_success", 365),
            ("password_changed", 365),
            ("signup", 90),
            ("magic_issued", 90),
            ("mailer_send", 30),
        ] {
            seed_event(&client, event_type, retention_days + 1).await;
            expected.push(seed_event(&client, event_type, retention_days - 1).await);
        }

        // Exercise the production entry point and its dedicated connection.
        audit_retention::tick(database.url())
            .await
            .expect("retention tick");
        assert_eq!(retained_events(&client).await, expected);
        audit_retention::tick(database.url())
            .await
            .expect("repeat retention tick");
        assert_eq!(retained_events(&client).await, expected);
    })
    .await;
}

#[compio::test]
async fn retention_preserves_theft_alerts_and_unclassified_events() {
    Database::run(async |database| {
        let mut client = database.connect_as_auth().await;
        let theft_alert = seed_event(&client, "refresh_reuse_detected", 2_000).await;
        let unclassified = seed_event(&client, "future_security_event", 2_000).await;
        seed_event(&client, "login_success", 2_000).await;

        let report = audit_retention::sweep_once(&mut client)
            .await
            .expect("retention sweep");
        assert_eq!(report, (1, 0, 0));
        assert_eq!(retained_events(&client).await, [theft_alert, unclassified]);
    })
    .await;
}

#[compio::test]
async fn sweep_restores_tamper_protection_after_commit() {
    Database::run(async |database| {
        let mut client = database.connect_as_auth().await;
        seed_event(&client, "login_success", 366).await;
        let retained = seed_event(&client, "login_success", 1).await;

        let report = audit_retention::sweep_once(&mut client)
            .await
            .expect("retention sweep");
        assert_eq!(report, (1, 0, 0));
        assert_eq!(retained_events(&client).await, [retained]);
        assert_tamper_protection(&client, retained).await;
    })
    .await;
}

#[compio::test]
async fn failed_sweep_rolls_back_deletes_and_restores_tamper_protection() {
    Database::run(async |database| {
        let admin = database.connect().await;
        let mut client = database.connect_as_auth().await;
        let security = seed_event(&client, "login_success", 366).await;
        let pii = seed_event(&client, "signup", 91).await;
        let debug = seed_event(&client, "mailer_send", 31).await;
        let retained = seed_event(&client, "login_success", 1).await;
        admin.batch_execute(
            "CREATE FUNCTION zeroship.fail_pii_retention() RETURNS trigger \
             LANGUAGE plpgsql AS $$ BEGIN \
               RAISE EXCEPTION 'audit retention failure injection' USING ERRCODE = 'check_violation'; \
             END $$; \
             CREATE TRIGGER fail_pii_retention BEFORE DELETE ON zeroship.audit_events \
             FOR EACH ROW WHEN (OLD.event_type = 'signup') \
             EXECUTE FUNCTION zeroship.fail_pii_retention()"
        ).await.expect("inject a failure after the security sweep");

        let error = audit_retention::sweep_once(&mut client).await.expect_err("injected failure");
        assert!(error.to_string().contains("audit retention failure injection"), "{error}");
        assert_eq!(retained_events(&client).await, [security, pii, debug, retained]);
        assert_tamper_protection(&client, retained).await;

        admin.batch_execute("DROP TRIGGER fail_pii_retention ON zeroship.audit_events")
            .await.expect("allow a retry on the same sweep connection");
        let report = audit_retention::sweep_once(&mut client).await.expect("retry retention sweep");
        assert_eq!(report, (1, 1, 1));
        assert_eq!(retained_events(&client).await, [retained]);
        assert_tamper_protection(&client, retained).await;
    }).await;
}
