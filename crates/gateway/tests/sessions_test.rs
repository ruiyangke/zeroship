//! Live-PG smoke test for `gateway::sessions`.
//!
//! Skipped silently when `AUTH_DB_URL` is unset (same convention as
//! `crates/auth/tests/migrations_smoke.rs`).
//!
//! Runs the full CRUD round-trip: create → validate (positive) →
//! validate w/ wrong `app_id` (negative) → revoke (per-app) → validate
//! (post-revoke negative).
//!
//! RLS (changeset 0025): the store fns now take `&mut Client` and set the
//! `zeroship.tenant_app` GUC in their own transaction. This test connects as
//! the migration/superuser role (which is BYPASSRLS), so the policy does not
//! filter; the per-`app_id` WHERE clauses still enforce the same behaviour.

use compio_postgres::{connect, NoTls};
use uuid::Uuid;
use zeroship_gateway::sessions::{create, revoke_app_sessions_for_user, validate, NewSession};

#[compio::test]
async fn create_validate_revoke_roundtrip() {
    let Ok(dsn) = std::env::var("AUTH_DB_URL") else {
        eprintln!("skipping (no AUTH_DB_URL)");
        return;
    };

    let (mut client, connection) = connect(&dsn, NoTls).await.expect("connect");
    compio::runtime::spawn(async move {
        if let Err(e) = connection.run().await {
            eprintln!("connection error: {e}");
        }
    })
    .detach();

    // Defensive: U4.1 will have created the table earlier in any sane
    // boot sequence, but running this test on a fresh DB should still
    // work standalone.

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

    // Sliding-window assertion: validate must return Some immediately
    // after creation, and the returned idle_expires_at should be
    // >= the value we got from create() (NOW() advanced between the
    // two statements, so the inequality is non-strict).
    let valid = validate(&mut client, session.id, app_id)
        .await
        .expect("validate");
    let valid = valid.expect("session must validate immediately after creation");
    assert!(
        valid.idle_expires_at >= session.idle_expires_at,
        "validate must slide idle_expires_at forward, not backward"
    );
    // validate() returns auth_time/amr off the same row the per-request
    // projection reads (BFF redesign §2.2 step 5b).
    assert_eq!(valid.auth_time.map(|t| t.timestamp()), Some(1_700_000_000));
    assert_eq!(valid.amr, vec!["pwd".to_string()]);

    // Wrong app_id → None (defends against confused-deputy across apps
    // sharing the gateway PG instance).
    let invalid = validate(&mut client, session.id, Uuid::new_v4())
        .await
        .expect("validate wrong app");
    assert!(invalid.is_none(), "app mismatch must fail validation");

    // Wrong session id → None.
    let bogus = validate(&mut client, Uuid::new_v4(), app_id)
        .await
        .expect("validate bogus id");
    assert!(bogus.is_none(), "unknown id must fail validation");

    // Revoke (per-app, the only revoke path under RLS) and confirm validate
    // now returns None.
    let revoked = revoke_app_sessions_for_user(&mut client, app_id, &user_id_text)
        .await
        .expect("revoke");
    assert_eq!(revoked, 1, "exactly the one session for (app_id, user) is revoked");
    let post_revoke = validate(&mut client, session.id, app_id)
        .await
        .expect("validate post revoke");
    assert!(
        post_revoke.is_none(),
        "revoked session must not validate"
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
    client
        .execute(
            "INSERT INTO zeroship.apps (id, name, api_key, api_key_hash) \
             VALUES ($1, $2, $3, $4)",
            &[
                &app_id,
                &format!("gateway-session-app-{}", app_id.simple()),
                &format!("api-{app_id}"),
                &format!("hash-{app_id}"),
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
