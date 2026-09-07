//! Live-PG smoke test for `gateway::sessions`.
//!
//! Skipped silently when there is no test database (set `PG_TEST_URL` or run
//! `tests/provision_test_backends.sh`; the same convention every live-PG
//! test under `crates/auth/tests/` uses).
//!
//! Runs the full CRUD round-trip: create → validate (positive) →
//! validate w/ wrong `app_id` (negative) → revoke (per-app) → validate
//! (post-revoke negative).
//!
//! RLS (changeset 0025): the store fns now take `&mut Client` and set the
//! `zeroship.tenant_app` GUC in their own transaction. This test connects as
//! the migration/superuser role (which is BYPASSRLS), so the policy does not
//! filter; the per-`app_id` WHERE clauses still enforce the same behaviour.

mod common;

use compio_postgres::{connect, NoTls};
use uuid::Uuid;
use zeroship_gateway::sessions::{create, revoke_app_sessions_for_user, NewSession};

/// The test's OWN oracle for "is this audit row still live". It replaces the
/// crate's deleted `sessions::validate`, which nothing in the request path ever
/// called: revocation is enforced by the per-app family marker the hot path
/// reads, not by reading this table. Keeping the read here keeps the assertion
/// that a revoke really wrote `revoked_at` without keeping a production-facing
/// function that reads like the gate and is not one.
///
/// Unlike the deleted function this is a pure SELECT: nothing slides
/// `idle_expires_at` any more, so nothing here should either.
async fn live_session(
    client: &compio_postgres::Client,
    id: Uuid,
    app_id: Uuid,
) -> Option<compio_postgres::Row> {
    client
        .query(
            "SELECT id, user_id, app_id, email::text AS email, name, avatar_url, \
                    email_verified, granted_scopes, auth_time, amr, sid, \
                    idle_expires_at, abs_expires_at \
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

#[compio::test]
async fn create_validate_revoke_roundtrip() {
    let Some(dsn) = common::platform_db_or_skip() else {
        zeroship_test_support::skip("skipping (no test database; set PG_TEST_URL)");
        return;
    };

    let (mut client, connection) = connect(&dsn, NoTls).await.expect("connect");
    compio::runtime::spawn(async move {
        if let Err(e) = connection.run().await {
            eprintln!("connection error: {e}");
        }
    })
    .detach();

    // Migrations normally create the table before the gateway starts, but this
    // test should still work standalone against a freshly migrated database.

    // Random ids — keeps the test repeatable on a shared DB. `app_id` is the
    // app's stable UUID (the column is UUID, bound natively).
    let app_id = Uuid::new_v4();
    insert_app(&client, app_id).await;
    let user_id = insert_user(&client, "gateway-session").await;
    let user_id_text = user_id.to_string();

    let session = create(
        &mut client,
        &NewSession {
            user_id: &user_id_text,
            sid: None,
            app_id,
            email: Some("test@zeroship.test"),
            name: Some("Test User"),
            avatar_url: None,
            email_verified: true,
            granted_scopes: &[],
            // BFF redesign §2.2 step 5b: auth_time + amr are carried on the
            // cookie session for the SPA projection + step-up freshness.
            auth_time: Some(1_700_000_000),
            amr: &["pwd".to_string()],
        },
    )
    .await
    .expect("create");

    assert_eq!(session.user_id, user_id_text);
    assert_eq!(session.app_id, app_id);
    assert_eq!(session.email.as_deref(), Some("test@zeroship.test"));
    assert_eq!(session.name.as_deref(), Some("Test User"));
    assert!(session.avatar_url.is_none());
    assert!(session.email_verified);
    // auth_time/amr round-trip through create (§2.2 step 5b).
    assert_eq!(
        session.auth_time.map(|t| t.timestamp()),
        Some(1_700_000_000),
        "auth_time must round-trip from the id_token claim"
    );
    assert_eq!(session.amr, vec!["pwd".to_string()], "amr must round-trip");

    // The row is live immediately after creation, and its idle window is the
    // one `create` stamped. NOTHING SLIDES IT: the read-and-slide function this
    // test used to call is deleted, so equality here is the assertion, not the
    // `>=` a sliding window would need.
    let live = live_session(&client, session.id, app_id)
        .await
        .expect("session row must be live immediately after creation");
    let live_idle: chrono::DateTime<chrono::Utc> = live.get("idle_expires_at");
    assert_eq!(
        live_idle, session.idle_expires_at,
        "the idle window is stamped once at create and never bumped"
    );
    // auth_time/amr sit on the same row the per-request projection reads
    // (BFF redesign §2.2 step 5b).
    let live_auth_time: Option<chrono::DateTime<chrono::Utc>> = live.try_get("auth_time").ok();
    assert_eq!(live_auth_time.map(|t| t.timestamp()), Some(1_700_000_000));
    let live_amr: Vec<String> = live.try_get("amr").unwrap_or_default();
    assert_eq!(live_amr, vec!["pwd".to_string()]);

    // Wrong app_id → no row (defends against confused-deputy across apps
    // sharing the gateway PG instance).
    assert!(
        live_session(&client, session.id, Uuid::new_v4())
            .await
            .is_none(),
        "app mismatch must not resolve a row"
    );

    // Wrong session id → no row.
    assert!(
        live_session(&client, Uuid::new_v4(), app_id).await.is_none(),
        "unknown id must not resolve a row"
    );

    // Revoke (per-app, the only revoke path under RLS) and confirm the row
    // stops resolving, which is what says `revoked_at` was written.
    let revoked = revoke_app_sessions_for_user(&mut client, app_id, &user_id_text)
        .await
        .expect("revoke");
    assert_eq!(revoked, 1, "exactly the one session for (app_id, user) is revoked");
    assert!(
        live_session(&client, session.id, app_id).await.is_none(),
        "a revoked session must not resolve as live"
    );

    // Cleanup (best effort — failure here doesn't fail the test).
    client
        .execute(
            "DELETE FROM zeroship.gateway_sessions WHERE id = $1",
            &[&session.id],
        )
        .await
        .ok();
    client
        .execute("DELETE FROM zeroship.users WHERE id = $1", &[&user_id])
        .await
        .ok();
    client
        .execute("DELETE FROM zeroship.apps WHERE id = $1", &[&app_id])
        .await
        .ok();
}

async fn insert_app(client: &compio_postgres::Client, app_id: Uuid) {
    client
        .execute(
            "INSERT INTO zeroship.plans \
                (id, name, runtime_limits_json, assignable_by_creator) \
             VALUES ('free', 'Free', '{}'::jsonb, TRUE) \
             ON CONFLICT (id) DO NOTHING",
            &[],
        )
        .await
        .expect("insert free plan");
    let project_id = common::unowned_project(client).await;
    client
        .execute(
            "INSERT INTO zeroship.apps (id, name, project_id, organization_id) \
             SELECT $1, $2, p.id, p.organization_id \
               FROM zeroship.projects p WHERE p.id = $3",
            &[
                &app_id,
                &format!("gateway-session-app-{}", app_id.simple()),
                &project_id
            ],
        )
        .await
        .expect("insert app");
}

async fn insert_user(client: &compio_postgres::Client, label: &str) -> Uuid {
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
