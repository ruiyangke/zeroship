//! Email verification issuance, redemption and transaction behavior in owned databases.

use crate::common::database::Database;
use uuid::Uuid;
use zeroship_auth::identity::verification;
use zeroship_auth::store::users;

#[compio::test]
async fn concurrent_issue_leaves_one_active_verification_token() {
    Database::run(async |database| {
        let orm = database.orm().await;
        let client = database.connect_as_auth().await;

        let email = format!(
            "verify-concurrent-{}@zeroship.test",
            Uuid::new_v4().simple()
        );
        let user = users::create(&orm, &email, "Test", None)
            .await
            .expect("seed user");
        let original = verification::issue(&client, &user.id, &email)
            .await
            .unwrap();
        let mut locker = database.connect().await;
        let transaction = locker.transaction().await.expect("begin token lock");
        transaction
            .query_one(
                "SELECT token_hash FROM zeroship.email_verifications \
             WHERE user_id = $1 AND consumed_at IS NULL FOR UPDATE",
                &[&user.id.as_str()],
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
        let email_a = email.clone();
        let email_b = email.to_ascii_uppercase();
        let user_id_a = user.id.clone();
        let user_id_b = user.id.clone();
        let issue_a = compio::runtime::spawn(async move {
            verification::issue(&client_a, &user_id_a, &email_a).await
        });
        let first_waiting = database.wait_until_blocked(&[pid_a]).await;
        let issue_b = compio::runtime::spawn(async move {
            verification::issue(&client_b, &user_id_b, &email_b).await
        });
        let both_waiting = database.wait_until_blocked(&[pid_a, pid_b]).await;
        transaction.commit().await.expect("release waiting issuers");

        let (first, second) = futures::join!(issue_a, issue_b);
        let first = first.expect("join issue A").expect("issue A");
        let second = second.expect("join issue B").expect("issue B");

        let active_count: i64 = client
            .query_one(
                "SELECT COUNT(*) FROM zeroship.email_verifications \
                 WHERE user_id = $1 AND consumed_at IS NULL",
                &[&user.id.as_str()],
            )
            .await
            .expect("count active verification links")
            .get(0);
        assert_eq!(
            active_count, 1,
            "concurrent issue must leave exactly one active verification token"
        );
        assert!(verification::redeem(&client, &original.raw)
            .await
            .unwrap()
            .is_none());
        assert!(verification::redeem(&client, &first.raw)
            .await
            .unwrap()
            .is_none());
        let redeemed = verification::redeem(&client, &second.raw)
            .await
            .unwrap()
            .expect("the last issued token remains redeemable");
        assert_eq!(redeemed.user_id, user.id);
        assert_eq!(redeemed.email.to_ascii_lowercase(), email);
        assert!(
            first_waiting && both_waiting,
            "issuers must overlap while superseding the existing token"
        );
    })
    .await;
}

#[compio::test]
async fn issue_then_redeem_roundtrip() {
    Database::run(async |database| {
        let orm = database.orm().await;
        let client = database.connect_as_auth().await;

        let email = format!("verify-{}@zeroship.test", Uuid::new_v4().simple());
        let user = users::create(&orm, &email, "Test", None)
            .await
            .expect("seed user");

        let issued = verification::issue(&client, &user.id, &email)
            .await
            .expect("issue");
        assert!(!issued.raw.is_empty(), "raw token must be non-empty");

        let redeemed = verification::redeem(&client, &issued.raw)
            .await
            .expect("redeem")
            .expect("redeem should return Some on first call");
        assert_eq!(redeemed.user_id, user.id);
        assert_eq!(
            redeemed.email.to_ascii_lowercase(),
            email.to_ascii_lowercase()
        );

        // Single-use: second redeem returns None.
        let second = verification::redeem(&client, &issued.raw)
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
async fn redeem_and_mark_verified_rolls_back_token_consume_with_transaction() {
    Database::run(async |database| {
        let orm = database.orm().await;
        let mut client = database.connect_as_auth().await;

        let email = format!("verify-rollback-{}@zeroship.test", Uuid::new_v4().simple());
        let user = users::create(&orm, &email, "Test", None)
            .await
            .expect("seed user");
        let issued = verification::issue(&client, &user.id, &email)
            .await
            .expect("issue");

        let transaction = client.transaction().await.expect("begin");
        let redeemed = verification::redeem_and_mark_verified(&transaction, &issued.raw)
            .await
            .expect("redeem and mark verified")
            .expect("token should redeem inside transaction");
        assert_eq!(redeemed.user_id, user.id);
        transaction.rollback().await.expect("rollback");

        let row = client
            .query_one(
                "SELECT ev.consumed_at IS NULL AS token_unconsumed, \
                        u.email_verified_at IS NULL AS user_unverified \
                 FROM zeroship.email_verifications ev \
                 JOIN zeroship.users u ON u.id = ev.user_id \
                 WHERE ev.user_id = $1",
                &[&user.id.as_str()],
            )
            .await
            .expect("load verification state");
        let token_unconsumed: bool = row.get("token_unconsumed");
        let user_unverified: bool = row.get("user_unverified");
        assert!(
            token_unconsumed,
            "rolled-back verification must leave token unconsumed"
        );
        assert!(
            user_unverified,
            "rolled-back verification must leave user unverified"
        );

        // The same token can still complete verification after the rollback.
        let transaction = client
            .transaction()
            .await
            .expect("begin committed verification");
        verification::redeem_and_mark_verified(&transaction, &issued.raw)
            .await
            .unwrap()
            .expect("token still usable after rollback");
        transaction.commit().await.expect("commit verification");
        let verified: bool = client
            .query_one(
                "SELECT email_verified_at IS NOT NULL FROM zeroship.users WHERE id = $1",
                &[&user.id.as_str()],
            )
            .await
            .unwrap()
            .get(0);
        assert!(verified, "committed redemption must mark the user verified");
        assert!(verification::redeem(&client, &issued.raw)
            .await
            .unwrap()
            .is_none());
    })
    .await;
}

#[compio::test]
async fn new_issue_supersedes_previous() {
    Database::run(async |database| {
        let orm = database.orm().await;
        let client = database.connect_as_auth().await;

        let email = format!("verify-supersede-{}@zeroship.test", Uuid::new_v4().simple());
        let user = users::create(&orm, &email, "Test", None)
            .await
            .expect("seed user");

        let first = verification::issue(&client, &user.id, &email)
            .await
            .expect("issue 1");
        let second = verification::issue(&client, &user.id, &email)
            .await
            .expect("issue 2");

        // First token is now invalid (superseded by the second issue).
        let r1 = verification::redeem(&client, &first.raw)
            .await
            .expect("redeem first");
        assert!(
            r1.is_none(),
            "first token must be invalidated by a fresh issue"
        );

        // Second token still works.
        let r2 = verification::redeem(&client, &second.raw)
            .await
            .expect("redeem second");
        assert!(r2.is_some(), "second token must still redeem");
    })
    .await;
}
