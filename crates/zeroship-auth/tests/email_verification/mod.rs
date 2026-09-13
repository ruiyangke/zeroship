//! Email verification through the production router in an owned database.

mod fixtures;

use crate::common::{self, auth_server::AuthServer, database::Database};
use fixtures::{assert_failure_audit, assert_state, assert_success, issue, landing, redeem};

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn landing_preserves_the_token_and_post_verifies_only_its_user_once() {
    Database::run(async |database| {
        let server = AuthServer::start(database).await;
        let (user_id, token) = issue(&server.pg, "verify@example.test").await;
        let (other_id, other_token) = issue(&server.pg, "other@example.test").await;
        let csrf = landing(&server, &token).await;
        let cookie = format!("__Host-zsidp_csrf={csrf}");
        assert_state(&server.pg, &user_id, false).await;
        assert_state(&server.pg, &other_id, false).await;

        assert_success(redeem(&server, &token, &csrf, Some(&cookie), "verify-success").await).await;
        assert_state(&server.pg, &user_id, true).await;
        assert_state(&server.pg, &other_id, false).await;
        let audit = server
            .pg
            .query_one(
                "SELECT actor_user_id, outcome FROM zeroship.audit_events \
             WHERE event_type = 'verification_redeemed' AND request_id = 'verify-success'",
                &[],
            )
            .await
            .unwrap();
        assert_eq!(
            zeroship_core::UserId::parse(audit.get::<_, &str>("actor_user_id")).unwrap(),
            user_id
        );
        assert_eq!(audit.get::<_, String>("outcome"), "success");

        let replay = redeem(&server, &token, &csrf, Some(&cookie), "verify-replay").await;
        assert_eq!(replay.status().as_u16(), 200);
        assert!(common::read_set_cookie(&replay, "__Host-zsidp_session").is_none());
        assert!(replay.text().await.unwrap().contains("session expired"));
        assert_failure_audit(&server.pg, "verify-replay").await;
        assert_state(&server.pg, &user_id, true).await;
        assert_state(&server.pg, &other_id, false).await;

        assert_success(redeem(&server, &other_token, &csrf, Some(&cookie), "other-success").await)
            .await;
        assert_state(&server.pg, &other_id, true).await;
    })
    .await;
}

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn missing_or_mismatched_csrf_preserves_the_token_for_a_valid_submission() {
    Database::run(async |database| {
        let server = AuthServer::start(database).await;
        let (user_id, token) = issue(&server.pg, "csrf@example.test").await;
        let csrf = landing(&server, &token).await;
        for cookie in [None, Some("__Host-zsidp_csrf=wrong")] {
            let response = redeem(&server, &token, &csrf, cookie, "bad-csrf").await;
            assert_eq!(response.status().as_u16(), 403);
            assert!(common::read_set_cookie(&response, "__Host-zsidp_session").is_none());
            assert_state(&server.pg, &user_id, false).await;
        }
        let cookie = format!("__Host-zsidp_csrf={csrf}");
        assert_success(redeem(&server, &token, &csrf, Some(&cookie), "good-csrf").await).await;
        assert_state(&server.pg, &user_id, true).await;
    })
    .await;
}

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn an_unknown_token_is_audited_without_consuming_an_existing_verification() {
    Database::run(async |database| {
        let server = AuthServer::start(database).await;
        let (user_id, token) = issue(&server.pg, "unknown-token@example.test").await;
        let csrf = landing(&server, "never-issued").await;
        let cookie = format!("__Host-zsidp_csrf={csrf}");
        let response = redeem(
            &server,
            "never-issued",
            &csrf,
            Some(&cookie),
            "unknown-token",
        )
        .await;
        assert_eq!(response.status().as_u16(), 200);
        assert!(common::read_set_cookie(&response, "__Host-zsidp_session").is_none());
        assert!(response.text().await.unwrap().contains("session expired"));
        assert_failure_audit(&server.pg, "unknown-token").await;
        assert_state(&server.pg, &user_id, false).await;
        assert_success(redeem(&server, &token, &csrf, Some(&cookie), "valid-token").await).await;
        assert_state(&server.pg, &user_id, true).await;
    })
    .await;
}
