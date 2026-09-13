//! Confirmation and refusal of account linking through the production router.

mod fixtures;

use crate::common::{auth_server::AuthServer, database::Database};
use fixtures::*;
use zeroship_auth::store::users;
use zeroship_authn::rate_limit::Quota;

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn locked_account_cannot_link_until_the_lock_expires() {
    Database::run(async |database| {
        let server = AuthServer::start(database).await;
        let confirmation =
            Confirmation::new(&server, "locked@example.test", "locked-profile").await;
        for _ in 0..users::lockout::THRESHOLD {
            users::record_login_failure(&server.pg, &confirmation.user.id)
                .await
                .unwrap();
        }
        let locked: bool = server
            .pg
            .query_one(
                "SELECT locked_until > NOW() FROM zeroship.users WHERE id = $1",
                &[&confirmation.user.id.as_str()],
            )
            .await
            .unwrap()
            .get(0);
        assert!(locked, "the account starts with an active password lock");

        let response = confirmation.submit(&server, PASSWORD, "192.0.2.1").await;
        assert_refused(
            &server,
            &confirmation,
            response,
            401,
            "account temporarily locked",
        )
        .await;

        database.connect().await.execute(
            "UPDATE zeroship.users SET locked_until = NOW() - INTERVAL '1 second' WHERE id = $1",
            &[&confirmation.user.id.as_str()],
        ).await.unwrap();
        let response = confirmation.submit(&server, PASSWORD, "192.0.2.1").await;
        assert_linked(&server, &confirmation, &response).await;
    })
    .await;
}

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn disabled_account_cannot_link_until_reenabled() {
    Database::run(async |database| {
        let server = AuthServer::start(database).await;
        let confirmation =
            Confirmation::new(&server, "disabled@example.test", "disabled-profile").await;
        let admin = database.connect().await;
        admin
            .execute(
                "UPDATE zeroship.users SET disabled_at = NOW() WHERE id = $1",
                &[&confirmation.user.id.as_str()],
            )
            .await
            .unwrap();

        let response = confirmation.submit(&server, PASSWORD, "192.0.2.1").await;
        assert_refused(
            &server,
            &confirmation,
            response,
            401,
            "account temporarily locked",
        )
        .await;
        let disabled: bool = admin
            .query_one(
                "SELECT disabled_at IS NOT NULL FROM zeroship.users WHERE id = $1",
                &[&confirmation.user.id.as_str()],
            )
            .await
            .unwrap()
            .get(0);
        assert!(disabled, "confirmation must not clear an admin disable");

        admin
            .execute(
                "UPDATE zeroship.users SET disabled_at = NULL WHERE id = $1",
                &[&confirmation.user.id.as_str()],
            )
            .await
            .unwrap();
        let response = confirmation.submit(&server, PASSWORD, "192.0.2.1").await;
        assert_linked(&server, &confirmation, &response).await;
    })
    .await;
}

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn wrong_password_attempts_exhaust_only_their_bucket_and_refill_allows_confirmation() {
    Database::run(async |database| {
        let server = AuthServer::start(database).await;
        let confirmation =
            Confirmation::new(&server, "limited@example.test", "limited-profile").await;
        let other = Confirmation::new(&server, "other@example.test", "other-profile").await;
        let ip = "192.0.2.1";
        let response = confirmation.submit(&server, WRONG_PASSWORD, ip).await;
        assert_refused(&server, &confirmation, response, 401, "invalid password").await;
        let mut remaining = Quota::LINK_ATTEMPT.capacity - 1.0;
        confirmation.assert_tokens(&server, ip, remaining).await;
        for attempt in
            (2_u32..).take_while(|attempt| f64::from(*attempt) <= Quota::LINK_ATTEMPT.capacity)
        {
            confirmation.freeze_refill(database, ip).await;
            let response = confirmation.submit(&server, WRONG_PASSWORD, ip).await;
            assert_refused(&server, &confirmation, response, 401, "invalid password").await;
            remaining = Quota::LINK_ATTEMPT.capacity - f64::from(attempt);
            confirmation.assert_tokens(&server, ip, remaining).await;
        }
        confirmation.freeze_refill(database, ip).await;
        let response = confirmation.submit(&server, PASSWORD, ip).await;
        assert_refused(&server, &confirmation, response, 429, "too many attempts").await;
        confirmation.assert_tokens(&server, ip, remaining).await;

        let response = confirmation
            .submit(&server, WRONG_PASSWORD, "192.0.2.2")
            .await;
        assert_refused(&server, &confirmation, response, 401, "invalid password").await;
        let response = other.submit(&server, PASSWORD, ip).await;
        assert_linked(&server, &other, &response).await;
        let response = confirmation.submit(&server, PASSWORD, ip).await;
        assert_refused(&server, &confirmation, response, 429, "too many attempts").await;
        confirmation.assert_tokens(&server, ip, remaining).await;

        confirmation.elapse_refill(database, ip).await;
        let response = confirmation.submit(&server, PASSWORD, ip).await;
        assert_linked(&server, &confirmation, &response).await;
        confirmation
            .assert_tokens(&server, ip, Quota::LINK_ATTEMPT.capacity - 1.0)
            .await;
    })
    .await;
}

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn unusable_authorization_target_cannot_link_even_with_the_correct_password() {
    Database::run(async |database| {
        let server = AuthServer::start(database).await;
        let confirmation =
            Confirmation::new(&server, "target@example.test", "target-profile").await;
        let token = confirmation
            .token_for_target(&server, "/oauth2/authorize?client_id=link-test-client")
            .await;
        let response = submit_token(&server, &token, PASSWORD, "192.0.2.1").await;
        assert_refused(&server, &confirmation, response, 200, "invalid_request").await;

        let response = confirmation.submit(&server, PASSWORD, "192.0.2.1").await;
        assert_linked(&server, &confirmation, &response).await;
    })
    .await;
}
