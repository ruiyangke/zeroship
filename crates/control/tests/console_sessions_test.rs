//! Live-PG smoke test for `console_sessions`.
//!
//! Skipped silently when `AUTH_DB_URL` is unset (same convention as
//! `crates/auth/tests/migrations_smoke.rs` and
//! `crates/gateway/tests/sessions_test.rs`).
//!
//! Runs the full CRUD round-trip: create → validate (positive) →
//! validate w/ bogus id (negative) → revoke → validate (post-revoke
//! negative).

use compio_postgres::{connect, NoTls};
use uuid::Uuid;
use zeroship_control::console_sessions::{create, revoke, validate};
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

    // Defensive: U7.1 will have created the table earlier in any sane
    // boot sequence, but running this test standalone on a fresh DB
    // should still work.
    zeroship_auth::store::migrations::migrate(&client)
        .await
        .expect("migrate");

    let user_id = format!("usr_{}", Uuid::new_v4().simple());
    let email = format!("u-{}@zeroship.test", Uuid::new_v4().simple());
    let claims = claims_for(&user_id, &email);

    let session = create(&client, &claims).await.expect("create");

    assert_eq!(session.user_id, user_id);
    assert_eq!(session.email.as_deref(), Some(email.as_str()));
    assert_eq!(session.name.as_deref(), Some("Console User"));
    assert!(session.avatar_url.is_none());
    assert!(session.email_verified);

    // validate must return Some immediately after creation; the
    // returned idle_expires_at should be >= the value we got from
    // create() (NOW() advanced between the two statements, so the
    // inequality is non-strict).
    let valid = validate(&client, session.id).await.expect("validate");
    let valid = valid.expect("session must validate immediately after creation");
    assert!(
        valid.idle_expires_at >= session.idle_expires_at,
        "validate must slide idle_expires_at forward, not backward"
    );

    // Bogus id → None.
    let bogus = validate(&client, Uuid::new_v4())
        .await
        .expect("validate bogus id");
    assert!(bogus.is_none(), "unknown id must fail validation");

    // Revoke and confirm validate now returns None.
    revoke(&client, session.id).await.expect("revoke");
    let post_revoke = validate(&client, session.id)
        .await
        .expect("validate post revoke");
    assert!(
        post_revoke.is_none(),
        "revoked session must not validate"
    );

    // Cleanup (best-effort).
    client
        .execute(
            "DELETE FROM auth.console_sessions WHERE id = $1",
            &[&session.id],
        )
        .await
        .ok();
}
