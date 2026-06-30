//! Token-sweep cron — live PG. Skip when `AUTH_DB_URL` unset.
//!
//! Drives [`token_sweep::tick`] directly so the sweep is observable inside
//! a single test run (the real cron sleeps 1 h between ticks).

use compio_postgres::{connect, NoTls};
use std::sync::Mutex;
use uuid::Uuid;
use zeroship_auth::cron::token_sweep;
use zeroship_auth::store::{users};

static TOKEN_SWEEP_TEST_LOCK: Mutex<()> = Mutex::new(());

#[allow(clippy::future_not_send)]
async fn pg() -> Option<compio_postgres::Client> {
    let dsn = std::env::var("AUTH_DB_URL").ok()?;
    let (client, connection) = connect(&dsn, NoTls).await.expect("connect");
    compio::runtime::spawn(async move {
        if let Err(e) = connection.run().await {
            eprintln!("token_sweep test pg connection error: {e}");
        }
    })
    .detach();
    Some(client)
}

#[compio::test]
async fn token_sweep_deletes_expired_rows_after_grace_and_keeps_fresh_rows() {
    let Some(client) = pg().await else {
        eprintln!("skipping token_sweep_test (no AUTH_DB_URL)");
        return;
    };
    let _guard = TOKEN_SWEEP_TEST_LOCK.lock().expect("token sweep test lock");

    let tag = Uuid::new_v4().simple().to_string();
    let login_email = format!("token-sweep-login-{tag}@zeroship.test");
    let reset_email = format!("token-sweep-reset-{tag}@zeroship.test");
    let verify_email = format!("token-sweep-verify-{tag}@zeroship.test");
    let user = users::create(&client, &verify_email, "Token Sweep", None)
        .await
        .expect("seed user");

    let stale_login_hash = token_hash(&tag, "stale-login");
    let fresh_login_hash = token_hash(&tag, "fresh-login");
    let stale_reset_hash = token_hash(&tag, "stale-reset");
    let fresh_reset_hash = token_hash(&tag, "fresh-reset");
    let stale_verify_hash = token_hash(&tag, "stale-verify");
    let fresh_verify_hash = token_hash(&tag, "fresh-verify");
    let stale_completion_nonce = format!("token-sweep-stale-{tag}");
    let fresh_completion_nonce = format!("token-sweep-fresh-{tag}");

    client
        .execute(
            "INSERT INTO zeroship.magic_links \
                (token_hash, email, csrf_nonce, purpose, expires_at, consumed_at) \
             VALUES \
                ($1, $2::citext, $3, 'login', NOW() - INTERVAL '10 days', NOW() - INTERVAL '10 days'), \
                ($4, $2::citext, $5, 'login', NOW() + INTERVAL '1 hour', NULL), \
                ($6, $7::citext, $8, 'reset', NOW() - INTERVAL '10 days', NOW() - INTERVAL '10 days'), \
                ($9, $7::citext, $10, 'reset', NOW() + INTERVAL '1 hour', NULL)",
            &[
                &stale_login_hash.as_slice(),
                &login_email,
                &format!("login-stale-{tag}"),
                &fresh_login_hash.as_slice(),
                &format!("login-fresh-{tag}"),
                &stale_reset_hash.as_slice(),
                &reset_email,
                &format!("reset-stale-{tag}"),
                &fresh_reset_hash.as_slice(),
                &format!("reset-fresh-{tag}"),
            ],
        )
        .await
        .expect("seed magic links");

    client
        .execute(
            "INSERT INTO zeroship.magic_completions \
                (csrf_nonce, code, email, login_challenge, expires_at) \
             VALUES \
                ($1, '123456', $2::citext, $3, NOW() - INTERVAL '10 days'), \
                ($4, '654321', $2::citext, $5, NOW() + INTERVAL '1 hour')",
            &[
                &stale_completion_nonce,
                &login_email,
                &format!("challenge-stale-{tag}"),
                &fresh_completion_nonce,
                &format!("challenge-fresh-{tag}"),
            ],
        )
        .await
        .expect("seed magic completions");

    client
        .execute(
            "INSERT INTO zeroship.email_verifications \
                (token_hash, user_id, email, expires_at, consumed_at) \
             VALUES \
                ($1, $2, $3::citext, NOW() - INTERVAL '10 days', NOW() - INTERVAL '10 days'), \
                ($4, $2, $3::citext, NOW() + INTERVAL '1 hour', NULL)",
            &[
                &stale_verify_hash.as_slice(),
                &user.id,
                &verify_email,
                &fresh_verify_hash.as_slice(),
            ],
        )
        .await
        .expect("seed email verifications");

    let report = token_sweep::tick(&client).await.expect("tick");

    assert_eq!(report.magic_links_deleted, 1);
    assert_eq!(report.password_resets_deleted, 1);
    assert_eq!(report.magic_completions_deleted, 1);
    assert_eq!(report.email_verifications_deleted, 1);

    assert_eq!(
        count_magic_links(&client, &login_email).await,
        1,
        "fresh login magic link should remain"
    );
    assert_eq!(
        count_magic_links(&client, &reset_email).await,
        1,
        "fresh reset token should remain"
    );
    assert_eq!(
        count_magic_completions(&client, &tag).await,
        1,
        "fresh magic completion should remain"
    );
    assert_eq!(
        count_email_verifications(&client, user.id).await,
        1,
        "fresh email verification should remain"
    );

    cleanup(&client, &login_email, &reset_email, &tag, user.id).await;
}

fn token_hash(tag: &str, label: &str) -> Vec<u8> {
    format!("{tag}:{label}").into_bytes()
}

async fn count_magic_links(client: &compio_postgres::Client, email: &str) -> i64 {
    client
        .query_one(
            "SELECT COUNT(*)::bigint AS count FROM zeroship.magic_links WHERE email = $1::citext",
            &[&email],
        )
        .await
        .expect("count magic_links")
        .get("count")
}

async fn count_magic_completions(client: &compio_postgres::Client, tag: &str) -> i64 {
    let pattern = format!("token-sweep-%-{tag}");
    client
        .query_one(
            "SELECT COUNT(*)::bigint AS count \
             FROM zeroship.magic_completions \
             WHERE csrf_nonce LIKE $1",
            &[&pattern],
        )
        .await
        .expect("count magic_completions")
        .get("count")
}

async fn count_email_verifications(client: &compio_postgres::Client, user_id: uuid::Uuid) -> i64 {
    client
        .query_one(
            "SELECT COUNT(*)::bigint AS count \
             FROM zeroship.email_verifications \
             WHERE user_id = $1",
            &[&user_id],
        )
        .await
        .expect("count email_verifications")
        .get("count")
}

async fn cleanup(
    client: &compio_postgres::Client,
    login_email: &str,
    reset_email: &str,
    tag: &str,
    user_id: uuid::Uuid,
) {
    client
        .execute(
            "DELETE FROM zeroship.magic_links WHERE email = $1::citext OR email = $2::citext",
            &[&login_email, &reset_email],
        )
        .await
        .ok();
    client
        .execute(
            "DELETE FROM zeroship.magic_completions WHERE csrf_nonce LIKE $1",
            &[&format!("token-sweep-%-{tag}")],
        )
        .await
        .ok();
    client
        .execute(
            "DELETE FROM zeroship.email_verifications WHERE user_id = $1",
            &[&user_id],
        )
        .await
        .ok();
    client
        .execute("DELETE FROM zeroship.users WHERE id = $1", &[&user_id])
        .await
        .ok();
}

/// SEC-3: the token sweep also reaps idle `zeroship.rate_limits` rows (and the
/// relay dedup sentinels that share the table) so a forged-IP flood cannot
/// leave permanent rows. A bucket idle past the 24h grace window is deleted; a
/// freshly-touched one survives. Live PG — skip when `AUTH_DB_URL` unset.
#[compio::test]
async fn token_sweep_reaps_idle_rate_limit_buckets_and_keeps_fresh() {
    let Some(client) = pg().await else {
        eprintln!("skipping token_sweep rate_limits test (no AUTH_DB_URL)");
        return;
    };
    let _guard = TOKEN_SWEEP_TEST_LOCK.lock().expect("token sweep test lock");

    let tag = Uuid::new_v4().simple().to_string();
    let stale_key = format!("login:ip:sec3-stale-{tag}");
    let fresh_key = format!("login:ip:sec3-fresh-{tag}");

    // Seed one row idle 25h (reapable) and one just-touched (kept).
    client
        .execute(
            "INSERT INTO zeroship.rate_limits (bucket_key, tokens, updated_at) VALUES \
                ($1, 0::REAL, NOW() - INTERVAL '25 hours'), \
                ($2, 0::REAL, NOW())",
            &[&stale_key, &fresh_key],
        )
        .await
        .expect("seed rate_limits rows");

    token_sweep::tick(&client).await.expect("tick");

    let stale_remaining: i64 = client
        .query_one(
            "SELECT COUNT(*)::BIGINT AS count FROM zeroship.rate_limits WHERE bucket_key = $1",
            &[&stale_key],
        )
        .await
        .expect("count stale")
        .get("count");
    let fresh_remaining: i64 = client
        .query_one(
            "SELECT COUNT(*)::BIGINT AS count FROM zeroship.rate_limits WHERE bucket_key = $1",
            &[&fresh_key],
        )
        .await
        .expect("count fresh")
        .get("count");

    // Cleanup before asserting so a failure doesn't leak fixtures.
    client
        .execute(
            "DELETE FROM zeroship.rate_limits WHERE bucket_key = $1 OR bucket_key = $2",
            &[&stale_key, &fresh_key],
        )
        .await
        .ok();

    assert_eq!(stale_remaining, 0, "idle (>24h) rate-limit bucket must be reaped");
    assert_eq!(fresh_remaining, 1, "freshly-touched rate-limit bucket must survive");
}
