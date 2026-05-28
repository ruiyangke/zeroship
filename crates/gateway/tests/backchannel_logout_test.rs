//! Live-PG smoke test for `gateway::sessions::revoke_all_for_user` —
//! the revoke-side of the OIDC Back-Channel Logout 1.0 handler (Phase
//! 7 U1.2).
//!
//! Skipped silently when `AUTH_DB_URL` is unset (same convention as
//! the rest of the gateway PG smoke tests, e.g. `sessions_test.rs`).
//!
//! Coverage:
//!   - Seed two live sessions for the same user_id (different app_ids)
//!     and one session for a different user_id.
//!   - Call `revoke_all_for_user`. Returned count must equal 2 (the
//!     two same-user rows).
//!   - Subsequent `validate(...)` on the revoked rows must return None.
//!   - The unrelated user's session must still validate.
//!   - Calling `revoke_all_for_user` again returns 0 (idempotent — the
//!     `revoked_at IS NULL` filter skips already-revoked rows).
//!
//! The handler-level path (verify + revoke) is exercised by the
//! Phase 7 follow-up e2e against a real hydra; for the verifier-only
//! check see `crates/core/src/logout_token.rs::tests`.

use compio_postgres::{connect, NoTls};
use uuid::Uuid;
use zeroship_gateway::sessions::{create, revoke_all_for_user, validate, NewSession};

#[compio::test]
async fn revoke_all_for_user_revokes_only_the_target_user() {
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

    // Defensive: ensure the schema is in place. Migrations are
    // idempotent.
    zeroship_auth::store::migrations::migrate(&client)
        .await
        .expect("migrate");

    // Two sessions for the same user at two different apps — the BCL
    // handler revokes ACROSS apps for the same sub.
    let target_user = format!("usr_{}", Uuid::new_v4().simple());
    let app_a = format!("app-a-{}", Uuid::new_v4().simple());
    let app_b = format!("app-b-{}", Uuid::new_v4().simple());

    let s_a = create(
        &client,
        &NewSession {
            user_id: &target_user,
            app_id: &app_a,
            email: Some("alice@zeroship.test"),
            name: Some("Alice"),
            avatar_url: None,
            email_verified: true,
        },
    )
    .await
    .expect("create s_a");

    let s_b = create(
        &client,
        &NewSession {
            user_id: &target_user,
            app_id: &app_b,
            email: Some("alice@zeroship.test"),
            name: Some("Alice"),
            avatar_url: None,
            email_verified: true,
        },
    )
    .await
    .expect("create s_b");

    // One session for an unrelated user — must NOT be touched.
    let other_user = format!("usr_{}", Uuid::new_v4().simple());
    let s_other = create(
        &client,
        &NewSession {
            user_id: &other_user,
            app_id: &app_a,
            email: Some("bob@zeroship.test"),
            name: Some("Bob"),
            avatar_url: None,
            email_verified: true,
        },
    )
    .await
    .expect("create s_other");

    // Sanity: all three validate before we revoke.
    assert!(validate(&client, s_a.id, &app_a)
        .await
        .expect("pre validate s_a")
        .is_some());
    assert!(validate(&client, s_b.id, &app_b)
        .await
        .expect("pre validate s_b")
        .is_some());
    assert!(validate(&client, s_other.id, &app_a)
        .await
        .expect("pre validate s_other")
        .is_some());

    // Revoke everything for the target user.
    let count = revoke_all_for_user(&client, &target_user)
        .await
        .expect("revoke_all_for_user");
    assert_eq!(count, 2, "expected 2 sessions revoked, got {count}");

    // Both target sessions must now fail validation.
    assert!(
        validate(&client, s_a.id, &app_a)
            .await
            .expect("post validate s_a")
            .is_none(),
        "s_a must be revoked"
    );
    assert!(
        validate(&client, s_b.id, &app_b)
            .await
            .expect("post validate s_b")
            .is_none(),
        "s_b must be revoked"
    );

    // The unrelated user's session must still validate.
    assert!(
        validate(&client, s_other.id, &app_a)
            .await
            .expect("post validate s_other")
            .is_some(),
        "unrelated user's session must NOT be revoked"
    );

    // Idempotent — running revoke_all_for_user again touches no rows.
    let again = revoke_all_for_user(&client, &target_user)
        .await
        .expect("revoke_all_for_user idempotent");
    assert_eq!(again, 0, "second revoke must touch 0 rows (filter on revoked_at IS NULL)");

    // Cleanup (best effort).
    for id in [s_a.id, s_b.id, s_other.id] {
        client
            .execute("DELETE FROM auth.gateway_sessions WHERE id = $1", &[&id])
            .await
            .ok();
    }
}
