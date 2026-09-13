//! Token expiry and rate-limit retention against an owned platform database.

use crate::common::database::Database;
use uuid::Uuid;
use zeroship_auth::cron::token_sweep;
use zeroship_auth::store::users;

#[compio::test]
async fn token_sweep_deletes_expired_rows_after_grace_and_keeps_fresh_rows() {
    Database::run(async |database| {
        let client = database.connect().await;
        let db_url = database.url().to_owned();

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
                    &user.id.as_str(),
                    &verify_email,
                    &fresh_verify_hash.as_slice(),
                ],
            )
            .await
            .expect("seed email verifications");

        let refresh_pool = zeroship_auth::oidc::refresh::RefreshSessionPool::new(db_url.clone(), 4);
        let report = token_sweep::tick(&client, &refresh_pool).await.expect("tick");

        assert_eq!(report.magic_links_deleted, 1, "{report:?}");
        assert_eq!(report.password_resets_deleted, 1, "{report:?}");
        assert_eq!(report.magic_completions_deleted, 1, "{report:?}");
        assert_eq!(report.email_verifications_deleted, 1, "{report:?}");

        // The surviving identities must be the tokens still inside their window.
        assert_eq!(
            retained_magic_tokens(&client, &login_email).await,
            vec![fresh_login_hash],
            "the stale login magic link should go and the fresh one remain"
        );
        assert_eq!(
            retained_magic_tokens(&client, &reset_email).await,
            vec![fresh_reset_hash],
            "the stale reset token should go and the fresh one remain"
        );
        assert_eq!(
            retained_completion_nonces(&client, &tag).await,
            vec![fresh_completion_nonce],
            "the stale magic completion should go and the fresh one remain"
        );
        assert_eq!(
            retained_verification_tokens(&client, user.id).await,
            vec![fresh_verify_hash],
            "the stale email verification should go and the fresh one remain"
        );

    }).await;
}

fn token_hash(tag: &str, label: &str) -> Vec<u8> {
    format!("{tag}:{label}").into_bytes()
}

async fn retained_magic_tokens(client: &compio_postgres::Client, email: &str) -> Vec<Vec<u8>> {
    client
        .query(
            "SELECT token_hash FROM zeroship.magic_links WHERE email = $1::citext",
            &[&email],
        )
        .await
        .expect("read retained magic links")
        .iter()
        .map(|row| row.get(0))
        .collect()
}

async fn retained_completion_nonces(client: &compio_postgres::Client, tag: &str) -> Vec<String> {
    client
        .query(
            "SELECT csrf_nonce FROM zeroship.magic_completions WHERE csrf_nonce LIKE $1",
            &[&format!("token-sweep-%-{tag}")],
        )
        .await
        .expect("read retained completions")
        .iter()
        .map(|row| row.get(0))
        .collect()
}

async fn retained_verification_tokens(
    client: &compio_postgres::Client,
    user_id: zeroship_core::UserId,
) -> Vec<Vec<u8>> {
    client
        .query(
            "SELECT token_hash FROM zeroship.email_verifications WHERE user_id = $1",
            &[&user_id.as_str()],
        )
        .await
        .expect("read retained verifications")
        .iter()
        .map(|row| row.get(0))
        .collect()
}

/// Idle rate-limit buckets expire while recently touched buckets survive.
#[compio::test]
async fn token_sweep_reaps_idle_rate_limit_buckets_and_keeps_fresh() {
    Database::run(async |database| {
        let client = database.connect().await;
        let db_url = database.url().to_owned();

        let tag = Uuid::new_v4().simple().to_string();
        let stale_key = format!("login:ip:sec3-stale-{tag}");
        let fresh_key = format!("login:ip:sec3-fresh-{tag}");

        // Seed an expired bucket and a recently touched bucket.
        client
            .execute(
                "INSERT INTO zeroship.rate_limits (bucket_key, tokens, updated_at) VALUES \
                ($1, 0::REAL, NOW() - INTERVAL '25 hours'), \
                ($2, 0::REAL, NOW())",
                &[&stale_key, &fresh_key],
            )
            .await
            .expect("seed rate_limits rows");

        let refresh_pool = zeroship_auth::oidc::refresh::RefreshSessionPool::new(db_url, 4);
        token_sweep::tick(&client, &refresh_pool)
            .await
            .expect("tick");

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

        assert_eq!(
            stale_remaining, 0,
            "idle (>24h) rate-limit bucket must be reaped"
        );
        assert_eq!(
            fresh_remaining, 1,
            "freshly-touched rate-limit bucket must survive"
        );
    })
    .await;
}
