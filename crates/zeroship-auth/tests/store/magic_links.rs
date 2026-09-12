//! Magic-link issuance, redemption and reservation ownership in owned databases.

use crate::common::database::Database;
use zeroship_auth::identity::{magic_link, password_reset};

const EMAIL: &str = "magic@example.test";

#[compio::test]
async fn issue_then_redeem_happy_path() {
    Database::run(async |database| {
        let client = database.connect_as_auth().await;

        let email = EMAIL;
        let issued = magic_link::issue(&client, email, "login")
            .await
            .expect("issue");
        assert!(!issued.raw.is_empty(), "raw token must be non-empty");
        assert!(
            !issued.csrf_nonce.is_empty(),
            "csrf nonce must be non-empty"
        );

        let redeemed = magic_link::redeem_pending(&client, &issued.raw)
            .await
            .expect("redeem")
            .expect("redeem should return Some on first call");
        assert_eq!(redeemed.email, email);
        assert_eq!(redeemed.csrf_nonce, issued.csrf_nonce);
        assert_eq!(redeemed.purpose, "login");
        assert!(
            magic_link::finalize_consume(&client, &redeemed.token_hash, &redeemed.reserved_at)
                .await
                .expect("finalize consume"),
            "finalize should update the pending row"
        );

        // Cleanup.
    })
    .await;
}

#[compio::test]
async fn second_redeem_returns_none() {
    Database::run(async |database| {
        let client = database.connect_as_auth().await;

        let email = EMAIL;
        let issued = magic_link::issue(&client, email, "login")
            .await
            .expect("issue");

        let first = magic_link::redeem_pending(&client, &issued.raw)
            .await
            .expect("redeem 1");
        let first = first.expect("first redeem must succeed");
        assert!(
            magic_link::finalize_consume(&client, &first.token_hash, &first.reserved_at)
                .await
                .expect("finalize consume"),
            "finalize should update the pending row"
        );

        let second = magic_link::redeem_pending(&client, &issued.raw)
            .await
            .expect("redeem 2");
        assert!(
            second.is_none(),
            "second redeem must return None (single-use)"
        );
    })
    .await;
}

#[compio::test]
async fn pending_consume_can_be_cleared_and_retried_before_finalize() {
    Database::run(async |database| {
        let client = database.connect_as_auth().await;

        let email = EMAIL;
        let issued = magic_link::issue(&client, email, "login")
            .await
            .expect("issue");

        let first = magic_link::redeem_pending(&client, &issued.raw)
            .await
            .expect("redeem pending 1")
            .expect("first redeem should reserve token");
        assert_eq!(first.email, email);

        let rows = client
            .query(
                "SELECT consumed_pending_at IS NOT NULL AS pending, \
                        consumed_at IS NOT NULL AS consumed \
                 FROM zeroship.magic_links \
                 WHERE token_hash = $1",
                &[&first.token_hash.as_slice()],
            )
            .await
            .expect("load pending row");
        assert_eq!(rows.len(), 1, "magic link row should exist");
        let pending: bool = rows[0].get("pending");
        let consumed: bool = rows[0].get("consumed");
        assert!(pending, "redeem_pending should set consumed_pending_at");
        assert!(!consumed, "redeem_pending must not finalize consumed_at");

        assert!(
            magic_link::clear_consume_pending(&client, &first.token_hash, Some(&first.reserved_at))
                .await
                .expect("clear consume pending"),
            "clear should update the pending row"
        );

        let second = magic_link::redeem_pending(&client, &issued.raw)
            .await
            .expect("redeem pending 2")
            .expect("same token should be retriable after clear");
        assert_eq!(second.email, email);

        assert!(
            magic_link::finalize_consume(&client, &second.token_hash, &second.reserved_at)
                .await
                .expect("finalize consume"),
            "finalize should set consumed_at"
        );

        let third = magic_link::redeem_pending(&client, &issued.raw)
            .await
            .expect("redeem after finalize");
        assert!(third.is_none(), "finalized token should not redeem again");
    })
    .await;
}

#[compio::test]
async fn stale_reservation_cannot_finalize_or_clear_a_retried_link() {
    Database::run(async |database| {
        let client = database.connect_as_auth().await;
        let issued = magic_link::issue(&client, EMAIL, "login").await.unwrap();
        let first = magic_link::redeem_pending(&client, &issued.raw)
            .await
            .unwrap()
            .expect("first reservation");
        assert!(magic_link::clear_consume_pending(
            &client,
            &first.token_hash,
            Some(&first.reserved_at)
        )
        .await
        .unwrap());
        let current = magic_link::redeem_pending(&client, &issued.raw)
            .await
            .unwrap()
            .expect("retry after releasing the first reservation");
        assert_ne!(first.reserved_at, current.reserved_at);
        assert!(
            !magic_link::finalize_consume(&client, &first.token_hash, &first.reserved_at)
                .await
                .unwrap(),
            "stale owner must not finalize a retried link"
        );
        assert!(
            !magic_link::clear_consume_pending(
                &client,
                &first.token_hash,
                Some(&first.reserved_at)
            )
            .await
            .unwrap(),
            "stale owner must not clear a retried link"
        );
        assert!(
            magic_link::finalize_consume(&client, &current.token_hash, &current.reserved_at)
                .await
                .unwrap(),
            "the current reservation still finalizes"
        );
        assert!(magic_link::redeem_pending(&client, &issued.raw)
            .await
            .unwrap()
            .is_none());
    })
    .await;
}

#[compio::test]
async fn second_redeem_while_pending_returns_in_flight() {
    Database::run(async |database| {
        let client = database.connect_as_auth().await;

        let email = EMAIL;
        let issued = magic_link::issue(&client, email, "login")
            .await
            .expect("issue");

        let first = magic_link::redeem_pending(&client, &issued.raw)
            .await
            .expect("redeem pending 1")
            .expect("first redeem should reserve token");

        let err = magic_link::redeem_pending(&client, &issued.raw)
            .await
            .expect_err("second redeem during pending window should be in-flight");
        assert!(
            matches!(err, magic_link::RedeemError::InFlight),
            "second pending redeem should return InFlight, got {err:?}"
        );

        magic_link::clear_consume_pending(&client, &first.token_hash, Some(&first.reserved_at))
            .await
            .expect("clear consume pending");
    })
    .await;
}

#[compio::test]
async fn stale_pending_redeem_burns_link_as_consumed() {
    Database::run(async |database| {
        let client = database.connect_as_auth().await;

        let email = EMAIL;
        let issued = magic_link::issue(&client, email, "login")
            .await
            .expect("issue");

        let first = magic_link::redeem_pending(&client, &issued.raw)
            .await
            .expect("redeem pending 1")
            .expect("first redeem should reserve token");

        client
            .execute(
                "UPDATE zeroship.magic_links \
                 SET consumed_pending_at = NOW() - INTERVAL '6 seconds' \
                 WHERE token_hash = $1",
                &[&first.token_hash.as_slice()],
            )
            .await
            .expect("age pending reservation");

        let err = magic_link::redeem_pending(&client, &issued.raw)
            .await
            .expect_err("stale pending redeem should burn link");
        assert!(
            matches!(err, magic_link::RedeemError::AlreadyConsumed),
            "stale pending redeem should return AlreadyConsumed, got {err:?}"
        );

        let rows = client
            .query(
                "SELECT consumed_at IS NOT NULL AS consumed \
                 FROM zeroship.magic_links \
                 WHERE token_hash = $1",
                &[&first.token_hash.as_slice()],
            )
            .await
            .expect("load burned row");
        assert_eq!(rows.len(), 1, "magic link row should exist");
        let consumed: bool = rows[0].get("consumed");
        assert!(consumed, "stale pending redeem should mark consumed_at");
    })
    .await;
}

#[compio::test]
async fn redeem_rejects_reset_purpose_without_consuming_its_token() {
    Database::run(async |database| {
        let client = database.connect_as_auth().await;
        zeroship_auth::store::users::create(&client, EMAIL, "Magic", None)
            .await
            .unwrap();
        let reset = password_reset::issue(&client, EMAIL).await.unwrap();
        assert!(magic_link::redeem_pending(&client, &reset.raw)
            .await
            .unwrap()
            .is_none());
        assert!(password_reset::is_live(&client, &reset.raw).await.unwrap());
        let redeemed = password_reset::redeem(&client, &reset.raw)
            .await
            .unwrap()
            .expect("the rejected login attempt leaves reset usable");
        assert_eq!(redeemed.email, EMAIL);
    })
    .await;
}

#[compio::test]
async fn expired_token_returns_none() {
    Database::run(async |database| {
        let client = database.connect_as_auth().await;

        let email = EMAIL;
        let issued = magic_link::issue(&client, email, "login")
            .await
            .expect("issue");

        // Force the row's expiry into the past.
        client
            .execute(
                "UPDATE zeroship.magic_links SET expires_at = NOW() - INTERVAL '1 minute' \
                 WHERE email = $1::citext AND consumed_at IS NULL",
                &[&email],
            )
            .await
            .expect("force expiry");

        let attempt = magic_link::redeem_pending(&client, &issued.raw)
            .await
            .expect("redeem");
        assert!(attempt.is_none(), "expired token must NOT redeem");
    })
    .await;
}

#[compio::test]
async fn new_issue_supersedes_previous_unconsumed() {
    Database::run(async |database| {
        let client = database.connect_as_auth().await;

        let email = EMAIL;
        let first = magic_link::issue(&client, email, "login")
            .await
            .expect("issue 1");
        let second = magic_link::issue(&client, email, "login")
            .await
            .expect("issue 2");

        // The first raw token must no longer be redeemable.
        let attempt = magic_link::redeem_pending(&client, &first.raw)
            .await
            .expect("redeem old");
        assert!(
            attempt.is_none(),
            "previous unconsumed token must be invalidated by a fresh issue"
        );
        let current = magic_link::redeem_pending(&client, &second.raw)
            .await
            .unwrap()
            .expect("fresh token remains redeemable");
        assert_eq!(current.email, email);
    })
    .await;
}

#[compio::test]
async fn login_issue_does_not_supersede_reset_token() {
    Database::run(async |database| {
        let client = database.connect_as_auth().await;

        let email = EMAIL;
        // A reset token binds to the issuing user's immutable id (security finding
        // L4), so `issue` only writes a row when the email maps to a real user —
        // mirroring the production `/forgot` caller, which guards on `find_by_email`.
        zeroship_auth::store::users::create(&client, email, "Test", None)
            .await
            .expect("seed user");
        let reset = password_reset::issue(&client, email)
            .await
            .expect("issue reset token");
        let login = magic_link::issue(&client, email, "login")
            .await
            .expect("issue login token");

        let redeemed_reset = password_reset::redeem(&client, &reset.raw)
            .await
            .expect("redeem reset token after login issue");
        assert!(
            redeemed_reset.is_some(),
            "login-purpose magic issue must not consume reset-purpose tokens"
        );
        assert!(magic_link::redeem_pending(&client, &login.raw)
            .await
            .unwrap()
            .is_some());
    })
    .await;
}

#[compio::test]
async fn concurrent_issue_leaves_one_active_token() {
    Database::run(async |database| {
        let client = database.connect_as_auth().await;
        let original = magic_link::issue(&client, EMAIL, "login").await.unwrap();
        let mut locker = database.connect().await;
        let transaction = locker.transaction().await.unwrap();
        transaction
            .query_one(
                "SELECT token_hash FROM zeroship.magic_links WHERE email = $1::citext \
             AND purpose = 'login' AND consumed_at IS NULL FOR UPDATE",
                &[&EMAIL],
            )
            .await
            .expect("hold the token being superseded");
        let client_a = database.connect_as_auth().await;
        let client_b = database.connect_as_auth().await;
        let pid_a = client_a
            .query_one("SELECT pg_backend_pid()", &[])
            .await
            .unwrap()
            .get(0);
        let pid_b = client_b
            .query_one("SELECT pg_backend_pid()", &[])
            .await
            .unwrap()
            .get(0);
        let first =
            compio::runtime::spawn(
                async move { magic_link::issue(&client_a, EMAIL, "login").await },
            );
        let first_waiting = database.wait_until_blocked(&[pid_a]).await;
        let second = compio::runtime::spawn(async move {
            magic_link::issue(&client_b, &EMAIL.to_ascii_uppercase(), "login").await
        });
        let both_waiting = database.wait_until_blocked(&[pid_a, pid_b]).await;
        transaction.commit().await.unwrap();
        let (first, second) = futures::join!(first, second);
        let first = first.unwrap().unwrap();
        let second = second.unwrap().unwrap();
        let active: i64 = client
            .query_one(
                "SELECT COUNT(*) FROM zeroship.magic_links WHERE email = $1::citext \
             AND purpose = 'login' AND consumed_at IS NULL",
                &[&EMAIL],
            )
            .await
            .unwrap()
            .get(0);
        assert_eq!(
            active, 1,
            "competing issuers must leave a single active magic link"
        );
        for token in [original, first] {
            assert!(magic_link::redeem_pending(&client, &token.raw)
                .await
                .unwrap()
                .is_none());
        }
        let current = magic_link::redeem_pending(&client, &second.raw)
            .await
            .unwrap()
            .expect("last issued token redeems");
        assert_eq!(current.email.to_ascii_lowercase(), EMAIL);
        assert_eq!(current.csrf_nonce, second.csrf_nonce);
        assert!(
            magic_link::finalize_consume(&client, &current.token_hash, &current.reserved_at)
                .await
                .unwrap()
        );
        assert!(
            first_waiting && both_waiting,
            "issuers must overlap on the existing token"
        );
    })
    .await;
}
