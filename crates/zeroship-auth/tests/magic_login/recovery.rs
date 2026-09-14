//! Magic recovery clears a password lock without bypassing an admin disable.

use super::fixtures::*;
use crate::common::{self, CapturingMailer, auth_server::AuthServer, database::Database};
use std::sync::Arc;
use zeroship_auth::{csrf, store::users};
use zeroship_core::UserId;

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn disabled_account_cannot_redeem_but_the_link_survives_for_an_eligible_retry() {
    Database::run(async |database| {
        let mailer = Arc::new(CapturingMailer::default());
        let server = AuthServer::with_mailer(database, mailer.clone()).await;
        let user = users::create(&server.orm, "disabled@example.test", "Disabled", None)
            .await
            .unwrap();
        let login = RequestedLogin::start(&server, &mailer, &user.email).await;
        let cookie = format!("__Host-zsidp_magic_csrf={}", login.nonce);
        set_disabled(database, &user.id, true).await;

        assert_disabled_refusal(
            &server,
            &user.id,
            login.redeem(&server, &login.nonce, Some(&cookie)).await,
        )
        .await;
        assert_link_state(&server, &login.nonce, false).await;
        let completions: i64 = server
            .pg
            .query_one("SELECT COUNT(*) FROM zeroship.magic_completions", &[])
            .await
            .unwrap()
            .get(0);
        assert_eq!(completions, 0);

        set_disabled(database, &user.id, false).await;
        let response = login.redeem(&server, &login.nonce, Some(&cookie)).await;
        assert_login_session(&server, &response, &user.email, &login.return_to).await;
        assert_link_state(&server, &login.nonce, true).await;
    })
    .await;
}

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn disabled_account_cannot_complete_but_the_code_survives_for_an_eligible_retry() {
    Database::run(async |database| {
        let mailer = Arc::new(CapturingMailer::default());
        let server = AuthServer::with_mailer(database, mailer.clone()).await;
        let user = users::create(&server.orm, "disabled@example.test", "Disabled", None)
            .await
            .unwrap();
        let login = RequestedLogin::start(&server, &mailer, &user.email).await;
        let other_csrf = csrf::generate_token();
        let cookie = format!("__Host-zsidp_magic_csrf={other_csrf}");
        let response = login.redeem(&server, &other_csrf, Some(&cookie)).await;
        assert_eq!(response.status().as_u16(), 200);
        let code: String = server
            .pg
            .query_one(
                "SELECT code FROM zeroship.magic_completions WHERE csrf_nonce = $1",
                &[&login.nonce],
            )
            .await
            .unwrap()
            .get(0);
        assert!(response.text().await.unwrap().contains(&code));
        assert_link_state(&server, &login.nonce, true).await;
        assert_completion_state(&server, &login.nonce, false, 0).await;
        set_disabled(database, &user.id, true).await;

        assert_disabled_refusal(
            &server,
            &user.id,
            login.complete(&server, &code, &login.return_to).await,
        )
        .await;
        assert_completion_state(&server, &login.nonce, false, 1).await;

        set_disabled(database, &user.id, false).await;
        let response = login.complete(&server, &code, &login.return_to).await;
        assert_login_session(&server, &response, &user.email, &login.return_to).await;
        assert_completion_state(&server, &login.nonce, true, 2).await;
    })
    .await;
}

#[allow(clippy::future_not_send)]
async fn set_disabled(database: &Database, id: &UserId, disabled: bool) {
    database.connect().await.execute(
        "UPDATE zeroship.users SET disabled_at = CASE WHEN $2 THEN NOW() ELSE NULL END WHERE id = $1",
        &[&id.as_str(), &disabled],
    ).await.unwrap();
}

#[allow(clippy::future_not_send)]
async fn assert_disabled_refusal(server: &AuthServer, id: &UserId, response: cyper::Response) {
    assert_eq!(response.status().as_u16(), 200);
    assert!(common::read_set_cookie(&response, "__Host-zsidp_session").is_none());
    assert!(
        response
            .text()
            .await
            .unwrap()
            .contains("account temporarily locked")
    );
    assert_eq!(session_count(server).await, 0);
    let disabled: bool = server
        .pg
        .query_one(
            "SELECT disabled_at IS NOT NULL FROM zeroship.users WHERE id = $1",
            &[&id.as_str()],
        )
        .await
        .unwrap()
        .get(0);
    assert!(disabled, "magic login must not clear an admin disable");
}
