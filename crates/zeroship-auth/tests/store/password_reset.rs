//! Password-reset storage behavior under the auth role in owned databases.

use crate::common::database::Database;
use zeroship_auth::identity::{password, password_reset};
use zeroship_auth::store::users;

#[compio::test]
async fn concurrent_issue_leaves_one_active_reset_token() {
    Database::run(async |database| {
        let orm = database.orm().await;
        let client = database.connect_as_auth().await;
        let email = "reset@example.test";
        let user = users::create(&orm, email, "Reset", None).await.unwrap();
        let original = password_reset::issue(&client, email).await.unwrap();
        let mut locker = database.connect().await;
        let transaction = locker.transaction().await.expect("begin token lock");
        transaction
            .query_one(
                "SELECT token_hash FROM zeroship.magic_links \
                 WHERE user_id = $1 AND purpose = 'reset' AND consumed_at IS NULL FOR UPDATE",
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
        let issue_a =
            compio::runtime::spawn(async move { password_reset::issue(&client_a, email).await });
        let first_waiting = database.wait_until_blocked(&[pid_a]).await;
        let issue_b = compio::runtime::spawn(async move {
            password_reset::issue(&client_b, &email.to_ascii_uppercase()).await
        });
        let both_waiting = database.wait_until_blocked(&[pid_a, pid_b]).await;
        transaction.commit().await.expect("release waiting issuers");
        let (first, second) = futures::join!(issue_a, issue_b);
        let first = first.expect("join issue A").expect("issue A");
        let second = second.expect("join issue B").expect("issue B");

        let active: i64 = client
            .query_one(
                "SELECT COUNT(*) FROM zeroship.magic_links \
                 WHERE user_id = $1 AND purpose = 'reset' AND consumed_at IS NULL",
                &[&user.id.as_str()],
            )
            .await
            .unwrap()
            .get(0);
        assert_eq!(
            active, 1,
            "competing issuers must leave a single active token"
        );
        for superseded in [original, first] {
            assert!(password_reset::redeem(&client, &superseded.raw)
                .await
                .unwrap()
                .is_none());
        }
        let redeemed = password_reset::redeem(&client, &second.raw)
            .await
            .unwrap()
            .expect("the last issued token remains redeemable");
        assert_eq!(redeemed.email.to_ascii_lowercase(), email);
        assert!(
            first_waiting && both_waiting,
            "issuers must overlap on the existing token"
        );
    })
    .await;
}

#[compio::test]
async fn issue_then_redeem_roundtrip() {
    Database::run(async |database| {
        let orm = database.orm().await;
        let client = database.connect_as_auth().await;
        let email = "reset@example.test";
        users::create(&orm, email, "Reset", None).await.unwrap();
        let issued = password_reset::issue(&client, email).await.unwrap();
        assert!(!issued.raw.is_empty());
        let redeemed = password_reset::redeem(&client, &issued.raw)
            .await
            .unwrap()
            .expect("first redemption succeeds");
        assert_eq!(redeemed.email, email);
        assert!(password_reset::redeem(&client, &issued.raw)
            .await
            .unwrap()
            .is_none());
    })
    .await;
}

#[compio::test]
async fn completion_obeys_transaction_rollback_and_commit() {
    Database::run(async |database| {
        let orm = database.orm().await;
        let mut client = database.connect_as_auth().await;
        let old_hash = password::hash("old reset password phrase").unwrap();
        let new_hash = password::hash("new reset password phrase").unwrap();
        let user = users::create(&orm, "reset@example.test", "Reset", Some(&old_hash))
            .await
            .unwrap();
        let issued = password_reset::issue(&client, &user.email).await.unwrap();
        let epoch: i64 = client
            .query_one(
                "SELECT credential_version FROM zeroship.users WHERE id = $1",
                &[&user.id.as_str()],
            )
            .await
            .unwrap()
            .get(0);

        let transaction = client.transaction().await.unwrap();
        let completed = password_reset::complete(&transaction, &issued.raw, &new_hash)
            .await
            .unwrap()
            .expect("complete reset in transaction");
        assert_eq!(completed.user_id, user.id);
        transaction.rollback().await.unwrap();

        let row = client
            .query_one(
                "SELECT password_hash, credential_version FROM zeroship.users WHERE id = $1",
                &[&user.id.as_str()],
            )
            .await
            .unwrap();
        assert_eq!(row.get::<_, String>("password_hash"), old_hash);
        assert_eq!(row.get::<_, i64>("credential_version"), epoch);
        assert!(password_reset::is_live(&client, &issued.raw).await.unwrap());

        let transaction = client.transaction().await.unwrap();
        password_reset::complete(&transaction, &issued.raw, &new_hash)
            .await
            .unwrap()
            .expect("same token can complete after rollback");
        transaction.commit().await.unwrap();
        let row = client
            .query_one(
                "SELECT password_hash, credential_version FROM zeroship.users WHERE id = $1",
                &[&user.id.as_str()],
            )
            .await
            .unwrap();
        assert_eq!(row.get::<_, String>("password_hash"), new_hash);
        assert_eq!(row.get::<_, i64>("credential_version"), epoch + 1);
        assert!(!password_reset::is_live(&client, &issued.raw).await.unwrap());
        assert!(password_reset::complete(&client, &issued.raw, &old_hash)
            .await
            .unwrap()
            .is_none());
        let stored: String = client
            .query_one(
                "SELECT password_hash FROM zeroship.users WHERE id = $1",
                &[&user.id.as_str()],
            )
            .await
            .unwrap()
            .get(0);
        assert_eq!(
            stored, new_hash,
            "replaying the token cannot restore the old password"
        );
    })
    .await;
}

#[compio::test]
async fn completion_binds_the_user_at_issue_time_after_email_reassignment() {
    Database::run(async |database| {
        let orm = database.orm().await;
        let client = database.connect_as_auth().await;
        let old_hash = password::hash("old reset password phrase").unwrap();
        let new_hash = password::hash("new reset password phrase").unwrap();
        let original = users::create(
            &orm,
            "original@example.test",
            "Original",
            Some(&old_hash),
        )
        .await
        .unwrap();
        let other = users::create(&orm, "other@example.test", "Other", Some(&old_hash))
            .await
            .unwrap();
        let issued = password_reset::issue(&client, &original.email)
            .await
            .unwrap();

        let admin = database.connect().await;
        admin
            .execute(
                "UPDATE zeroship.users SET email = 'moved@example.test'::citext WHERE id = $1",
                &[&original.id.as_str()],
            )
            .await
            .unwrap();
        admin
            .execute(
                "UPDATE zeroship.users SET email = $1::citext WHERE id = $2",
                &[&original.email, &other.id.as_str()],
            )
            .await
            .unwrap();

        let completed = password_reset::complete(&client, &issued.raw, &new_hash)
            .await
            .unwrap()
            .expect("the issued token still resets its original user");
        assert_eq!(completed.user_id, original.id);
        assert_eq!(completed.email, "moved@example.test");
        let original_hash: String = client
            .query_one(
                "SELECT password_hash FROM zeroship.users WHERE id = $1",
                &[&original.id.as_str()],
            )
            .await
            .unwrap()
            .get(0);
        let other_hash: String = client
            .query_one(
                "SELECT password_hash FROM zeroship.users WHERE id = $1",
                &[&other.id.as_str()],
            )
            .await
            .unwrap()
            .get(0);
        assert_eq!(original_hash, new_hash);
        assert_eq!(
            other_hash, old_hash,
            "reset must not target the new email owner"
        );
        assert!(!password_reset::is_live(&client, &issued.raw).await.unwrap());
    })
    .await;
}

#[compio::test]
async fn new_issue_supersedes_previous_reset_token() {
    Database::run(async |database| {
        let orm = database.orm().await;
        let client = database.connect_as_auth().await;
        let email = "reset@example.test";
        users::create(&orm, email, "Reset", None).await.unwrap();
        let first = password_reset::issue(&client, email).await.unwrap();
        let second = password_reset::issue(&client, email).await.unwrap();
        assert!(password_reset::redeem(&client, &first.raw)
            .await
            .unwrap()
            .is_none());
        let redeemed = password_reset::redeem(&client, &second.raw)
            .await
            .unwrap()
            .expect("fresh token redeems");
        assert_eq!(redeemed.email, email);
    })
    .await;
}
