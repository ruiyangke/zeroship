//! Live-PG roundtrip for `auth::identity::magic_link`.
//!
//! Skipped unless `AUTH_DB_URL` is set. Each test scopes itself with a
//! random email so concurrent runs don't collide; the cleanup at the end
//! removes every row that test inserted.

use compio_postgres::{connect, NoTls};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use zeroship_auth::identity::magic_link;
use zeroship_auth::store::migrations;
use zeroship_auth::ui::magic::completions_store::{self, ConsumeError};

// `compio_postgres::Client` is `!Send` — the futures inherit that
// structurally. The lint is informational, not actionable here.
#[allow(clippy::future_not_send)]
async fn pg() -> Option<compio_postgres::Client> {
    let dsn = std::env::var("AUTH_DB_URL").ok()?;
    let (client, connection) = connect(&dsn, NoTls).await.expect("connect");
    compio::runtime::spawn(async move {
        if let Err(e) = connection.run().await {
            eprintln!("magic_link test pg connection error: {e}");
        }
    })
    .detach();
    migrations::migrate(&client).await.expect("migrate");
    Some(client)
}

fn sha256(s: &str) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(s.as_bytes());
    h.finalize().into()
}

#[compio::test]
async fn issue_then_redeem_happy_path() {
    let Some(client) = pg().await else {
        eprintln!("skipping magic_link_test (no AUTH_DB_URL)");
        return;
    };

    let email = format!("magic-happy-{}@example.test", Uuid::new_v4().simple());
    let issued = magic_link::issue(&client, &email, "login")
        .await
        .expect("issue");
    assert!(!issued.raw.is_empty(), "raw token must be non-empty");
    assert!(
        !issued.csrf_nonce.is_empty(),
        "csrf nonce must be non-empty"
    );

    let redeemed = magic_link::redeem(&client, &issued.raw)
        .await
        .expect("redeem")
        .expect("redeem should return Some on first call");
    assert_eq!(redeemed.email, email);
    assert_eq!(redeemed.csrf_nonce, issued.csrf_nonce);
    assert_eq!(redeemed.purpose, "login");

    // Cleanup.
    client
        .execute(
            "DELETE FROM auth.magic_links WHERE email = $1::citext",
            &[&email],
        )
        .await
        .ok();
}

#[compio::test]
async fn second_redeem_returns_none() {
    let Some(client) = pg().await else {
        eprintln!("skipping magic_link_test (no AUTH_DB_URL)");
        return;
    };

    let email = format!("magic-once-{}@example.test", Uuid::new_v4().simple());
    let issued = magic_link::issue(&client, &email, "login")
        .await
        .expect("issue");

    let first = magic_link::redeem(&client, &issued.raw)
        .await
        .expect("redeem 1");
    assert!(first.is_some(), "first redeem must succeed");

    let second = magic_link::redeem(&client, &issued.raw)
        .await
        .expect("redeem 2");
    assert!(
        second.is_none(),
        "second redeem must return None (single-use)"
    );

    client
        .execute(
            "DELETE FROM auth.magic_links WHERE email = $1::citext",
            &[&email],
        )
        .await
        .ok();
}

#[compio::test]
async fn redeem_rejects_reset_purpose_row_without_consuming_it() {
    let Some(client) = pg().await else {
        eprintln!("skipping magic_link_test (no AUTH_DB_URL)");
        return;
    };

    let email = format!("magic-reset-purpose-{}@example.test", Uuid::new_v4().simple());
    let raw_token = format!("reset-token-{}", Uuid::new_v4().simple());
    let token_hash = sha256(&raw_token);
    let csrf_nonce = "reset-no-csrf-nonce";
    let purpose = "reset";

    client
        .execute(
            "INSERT INTO auth.magic_links \
                (token_hash, email, csrf_nonce, purpose, expires_at) \
             VALUES ($1, $2::citext, $3, $4, NOW() + INTERVAL '60 minutes')",
            &[&token_hash.as_slice(), &email, &csrf_nonce, &purpose],
        )
        .await
        .expect("insert reset-purpose row");

    let attempt = magic_link::redeem(&client, &raw_token)
        .await
        .expect("redeem reset-purpose row via magic login");
    assert!(
        attempt.is_none(),
        "reset-purpose row must not redeem through magic-link login"
    );

    let rows = client
        .query(
            "SELECT consumed_at IS NULL AS still_unconsumed \
             FROM auth.magic_links \
             WHERE token_hash = $1 AND email = $2::citext AND purpose = $3",
            &[&token_hash.as_slice(), &email, &purpose],
        )
        .await
        .expect("load reset-purpose row after rejected redeem");
    assert_eq!(rows.len(), 1, "test row must still exist");
    let still_unconsumed: bool = rows[0].get("still_unconsumed");
    assert!(
        still_unconsumed,
        "rejected reset-purpose row must remain unconsumed"
    );

    client
        .execute(
            "DELETE FROM auth.magic_links WHERE email = $1::citext",
            &[&email],
        )
        .await
        .ok();
}

#[compio::test]
async fn expired_token_returns_none() {
    let Some(client) = pg().await else {
        eprintln!("skipping magic_link_test (no AUTH_DB_URL)");
        return;
    };

    let email = format!("magic-expired-{}@example.test", Uuid::new_v4().simple());
    let issued = magic_link::issue(&client, &email, "login")
        .await
        .expect("issue");

    // Force the row's expiry into the past.
    client
        .execute(
            "UPDATE auth.magic_links SET expires_at = NOW() - INTERVAL '1 minute' \
             WHERE email = $1::citext AND consumed_at IS NULL",
            &[&email],
        )
        .await
        .expect("force expiry");

    let attempt = magic_link::redeem(&client, &issued.raw)
        .await
        .expect("redeem");
    assert!(attempt.is_none(), "expired token must NOT redeem");

    client
        .execute(
            "DELETE FROM auth.magic_links WHERE email = $1::citext",
            &[&email],
        )
        .await
        .ok();
}

#[compio::test]
async fn new_issue_supersedes_previous_unconsumed() {
    let Some(client) = pg().await else {
        eprintln!("skipping magic_link_test (no AUTH_DB_URL)");
        return;
    };

    let email = format!("magic-super-{}@example.test", Uuid::new_v4().simple());
    let first = magic_link::issue(&client, &email, "login")
        .await
        .expect("issue 1");
    let _second = magic_link::issue(&client, &email, "login")
        .await
        .expect("issue 2");

    // The first raw token must no longer be redeemable.
    let attempt = magic_link::redeem(&client, &first.raw)
        .await
        .expect("redeem old");
    assert!(
        attempt.is_none(),
        "previous unconsumed token must be invalidated by a fresh issue"
    );

    client
        .execute(
            "DELETE FROM auth.magic_links WHERE email = $1::citext",
            &[&email],
        )
        .await
        .ok();
}

#[compio::test]
async fn magic_completion_invalidates_after_five_wrong_codes() {
    let Some(client) = pg().await else {
        eprintln!("skipping magic_link_test (no AUTH_DB_URL)");
        return;
    };

    let csrf_nonce = format!("completion-attempts-{}", Uuid::new_v4().simple());
    let email = format!("magic-complete-{}@example.test", Uuid::new_v4().simple());
    let login_challenge = format!("lc-{}", Uuid::new_v4().simple());
    let code = "123456";

    client
        .execute(
            "INSERT INTO auth.magic_completions \
                (csrf_nonce, code, email, login_challenge, expires_at) \
             VALUES ($1, $2, $3::citext, $4, NOW() + INTERVAL '5 minutes')",
            &[&csrf_nonce, &code, &email, &login_challenge],
        )
        .await
        .expect("insert completion row");

    for i in 1..=4 {
        let err = completions_store::consume(&client, &csrf_nonce, "000000")
            .await
            .expect_err("wrong code must fail before invalidation");
        assert!(
            matches!(err, ConsumeError::WrongCode),
            "wrong attempt {i} should return WrongCode, got {err:?}"
        );
    }

    let err = completions_store::consume(&client, &csrf_nonce, "000000")
        .await
        .expect_err("fifth wrong code must fail and invalidate");
    assert!(
        matches!(err, ConsumeError::WrongCode),
        "fifth wrong attempt should return WrongCode, got {err:?}"
    );

    let rows = client
        .query(
            "SELECT consumed_at IS NOT NULL AS consumed \
             FROM auth.magic_completions \
             WHERE csrf_nonce = $1",
            &[&csrf_nonce],
        )
        .await
        .expect("load completion row");
    assert_eq!(rows.len(), 1, "completion row should still exist");
    let consumed: bool = rows[0].get("consumed");
    assert!(
        consumed,
        "fifth wrong completion attempt must invalidate the row"
    );

    let err = completions_store::consume(&client, &csrf_nonce, code)
        .await
        .expect_err("correct code must not redeem after invalidation");
    assert!(
        matches!(err, ConsumeError::WrongCode),
        "correct code after invalidation should return WrongCode, got {err:?}"
    );

    client
        .execute(
            "DELETE FROM auth.magic_completions WHERE csrf_nonce = $1",
            &[&csrf_nonce],
        )
        .await
        .ok();
}
