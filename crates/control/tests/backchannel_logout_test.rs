//! Live-PG smoke test for `console_sessions::revoke_all_for_user` —
//! the revoke-side of the OIDC Back-Channel Logout 1.0 handler for the
//! control plane (Phase 7 U2).
//!
//! Skipped silently when `AUTH_DB_URL` is unset (same convention as
//! `console_sessions_test.rs`).
//!
//! Coverage:
//!   - Seed two live console sessions for the same user_id and one
//!     session for an unrelated user.
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
use zeroship_control::console_sessions::{create, revoke_all_for_user, validate};
use zeroship_core::oidc_verify::TokenClaims;

fn claims_for(user_id: &str, email: &str) -> TokenClaims {
    TokenClaims {
        sub: user_id.to_string(),
        iss: "https://auth.zeroship.test/".to_string(),
        aud: serde_json::Value::String("console.zeroship.ai".to_string()),
        exp: 9_999_999_999,
        iat: 0,
        nbf: None,
        nonce: None,
        at_hash: None,
        c_hash: None,
        email: Some(email.to_string()),
        email_verified: Some(true),
        name: Some("Console User".to_string()),
        picture: None,
        acr: None,
        amr: None,
        other: Default::default(),
    }
}

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

    // Two sessions for the same user — the BCL handler revokes every
    // console session for the same sub. There's only one console
    // origin so unlike gateway_sessions there's no per-app dimension;
    // we still want at least two rows to verify the count.
    let target_user_id = insert_user(&client, "console-bcl-target").await;
    let target_user = target_user_id.to_string();
    let target_email = format!("alice-{}@zeroship.test", Uuid::new_v4().simple());

    let s_a = create(&client, &claims_for(&target_user, &target_email))
        .await
        .expect("create s_a");
    let s_b = create(&client, &claims_for(&target_user, &target_email))
        .await
        .expect("create s_b");

    // One session for an unrelated user — must NOT be touched.
    let other_user_id = insert_user(&client, "console-bcl-other").await;
    let other_user = other_user_id.to_string();
    let other_email = format!("bob-{}@zeroship.test", Uuid::new_v4().simple());
    let s_other = create(&client, &claims_for(&other_user, &other_email))
        .await
        .expect("create s_other");

    // Sanity: all three validate before we revoke.
    assert!(validate(&client, s_a.id)
        .await
        .expect("pre validate s_a")
        .is_some());
    assert!(validate(&client, s_b.id)
        .await
        .expect("pre validate s_b")
        .is_some());
    assert!(validate(&client, s_other.id)
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
        validate(&client, s_a.id)
            .await
            .expect("post validate s_a")
            .is_none(),
        "s_a must be revoked"
    );
    assert!(
        validate(&client, s_b.id)
            .await
            .expect("post validate s_b")
            .is_none(),
        "s_b must be revoked"
    );

    // The unrelated user's session must still validate.
    assert!(
        validate(&client, s_other.id)
            .await
            .expect("post validate s_other")
            .is_some(),
        "unrelated user's session must NOT be revoked"
    );

    // Idempotent — running revoke_all_for_user again touches no rows.
    let again = revoke_all_for_user(&client, &target_user)
        .await
        .expect("revoke_all_for_user idempotent");
    assert_eq!(
        again, 0,
        "second revoke must touch 0 rows (filter on revoked_at IS NULL)"
    );

    // Cleanup (best effort).
    for id in [s_a.id, s_b.id, s_other.id] {
        client
            .execute("DELETE FROM auth.console_sessions WHERE id = $1", &[&id])
            .await
            .ok();
    }
    for id in [target_user_id, other_user_id] {
        client
            .execute("DELETE FROM auth.users WHERE id = $1", &[&id])
            .await
            .ok();
    }
}

async fn insert_user(client: &compio_postgres::Client, label: &str) -> Uuid {
    let email = format!("{label}-{}@zeroship.test", Uuid::new_v4().simple());
    let rows = client
        .query(
            "INSERT INTO auth.users (email, name, email_verified_at)
             VALUES ($1, $2, NOW())
             RETURNING id",
            &[&email, &label],
        )
        .await
        .expect("insert user");
    rows[0].get("id")
}
