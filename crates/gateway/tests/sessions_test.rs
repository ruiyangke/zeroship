//! Live-PG smoke test for `gateway::sessions`.
//!
//! Skipped silently when `AUTH_DB_URL` is unset (same convention as
//! `crates/auth/tests/migrations_smoke.rs`).
//!
//! Runs the full CRUD round-trip: create → validate (positive) →
//! validate w/ wrong `app_id` (negative) → revoke → validate
//! (post-revoke negative).

use compio_postgres::{connect, NoTls};
use uuid::Uuid;
use zeroship_gateway::sessions::{create, revoke, validate, NewSession};

#[compio::test]
async fn create_validate_revoke_roundtrip() {
    let Ok(dsn) = std::env::var("AUTH_DB_URL") else {
        eprintln!("skipping (no AUTH_DB_URL)");
        return;
    };

    let (client, connection) = connect(&dsn, NoTls).await.expect("connect");
    compio::runtime::spawn(async move {
        if let Err(e) = connection.run().await {
            eprintln!("connection error: {e}");
        }
    })
    .detach();

    // Defensive: U4.1 will have created the table earlier in any sane
    // boot sequence, but running this test on a fresh DB should still
    // work standalone.
    zeroship_auth::store::migrations::migrate(&client)
        .await
        .expect("migrate");

    // Random ids — keeps the test repeatable on a shared DB.
    let app_id = format!("app-{}", Uuid::new_v4().simple());
    let user_id = format!("usr_{}", Uuid::new_v4().simple());

    let session = create(
        &client,
        &NewSession {
            user_id: &user_id,
            app_id: &app_id,
            email: Some("test@zeroship.test"),
            name: Some("Test User"),
            avatar_url: None,
            email_verified: true,
        },
    )
    .await
    .expect("create");

    assert_eq!(session.user_id, user_id);
    assert_eq!(session.app_id, app_id);
    assert_eq!(session.email.as_deref(), Some("test@zeroship.test"));
    assert_eq!(session.name.as_deref(), Some("Test User"));
    assert!(session.avatar_url.is_none());
    assert!(session.email_verified);

    // Sliding-window assertion: validate must return Some immediately
    // after creation, and the returned idle_expires_at should be
    // >= the value we got from create() (NOW() advanced between the
    // two statements, so the inequality is non-strict).
    let valid = validate(&client, session.id, &app_id)
        .await
        .expect("validate");
    let valid = valid.expect("session must validate immediately after creation");
    assert!(
        valid.idle_expires_at >= session.idle_expires_at,
        "validate must slide idle_expires_at forward, not backward"
    );

    // Wrong app_id → None (defends against confused-deputy across apps
    // sharing the gateway PG instance).
    let invalid = validate(&client, session.id, "wrong-app")
        .await
        .expect("validate wrong app");
    assert!(invalid.is_none(), "app mismatch must fail validation");

    // Wrong session id → None.
    let bogus = validate(&client, Uuid::new_v4(), &app_id)
        .await
        .expect("validate bogus id");
    assert!(bogus.is_none(), "unknown id must fail validation");

    // Revoke and confirm validate now returns None.
    revoke(&client, session.id).await.expect("revoke");
    let post_revoke = validate(&client, session.id, &app_id)
        .await
        .expect("validate post revoke");
    assert!(
        post_revoke.is_none(),
        "revoked session must not validate"
    );

    // Cleanup (best effort — failure here doesn't fail the test).
    client
        .execute(
            "DELETE FROM auth.gateway_sessions WHERE id = $1",
            &[&session.id],
        )
        .await
        .ok();
}
