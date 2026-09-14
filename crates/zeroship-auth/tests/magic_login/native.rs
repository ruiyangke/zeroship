//! Email issuance, browser handoff and session persistence through native routes.

use super::fixtures::*;
use crate::common::{self, CapturingMailer, auth_server::AuthServer, database::Database};
use std::sync::Arc;
use zeroship_auth::store::users;

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn same_device_login_requires_csrf_and_recovers_a_soft_locked_account() {
    Database::run(async |database| {
        let mailer = Arc::new(CapturingMailer::default());
        let server = AuthServer::with_mailer(database, mailer.clone()).await;
        let email = "same-device@example.test";
        let user = users::create(&server.orm, email, "Magic recovery", None)
            .await
            .unwrap();
        soft_lock(&server, &user.id).await;
        let login = RequestedLogin::start(&server, &mailer, email).await;
        let cookie = format!("__Host-zsidp_magic_csrf={}", login.nonce);

        let response = server
            .http
            .get(&login.landing_url)
            .unwrap()
            .header("cookie", &cookie)
            .unwrap()
            .send()
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), 200);
        assert!(
            response
                .text()
                .await
                .unwrap()
                .contains("/magic/verify/redeem")
        );
        assert_link_state(&server, &login.nonce, false).await;
        assert_eq!(session_count(&server).await, 0);

        let rejected = login.redeem(&server, &login.nonce, None).await;
        assert_eq!(rejected.status().as_u16(), 403);
        assert!(common::read_set_cookie(&rejected, "__Host-zsidp_session").is_none());
        assert_link_state(&server, &login.nonce, false).await;
        assert_eq!(session_count(&server).await, 0);

        let response = login.redeem(&server, &login.nonce, Some(&cookie)).await;
        assert_login_session(&server, &response, email, &login.return_to).await;
        assert_lock_cleared(&server, &user.id).await;
        assert_eq!(
            common::read_set_cookie(&response, "__Host-zsidp_magic_csrf").as_deref(),
            Some("")
        );
        assert_link_state(&server, &login.nonce, true).await;
        let completions: i64 = server
            .pg
            .query_one("SELECT COUNT(*) FROM zeroship.magic_completions", &[])
            .await
            .unwrap()
            .get(0);
        assert_eq!(completions, 0, "same-device login needs no completion code");

        assert_login_rejected(login.redeem(&server, &login.nonce, Some(&cookie)).await).await;
        assert_eq!(
            session_count(&server).await,
            1,
            "replay cannot mint another session"
        );
    })
    .await;
}

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn cross_device_login_binds_the_completion_to_its_target_and_consumes_it() {
    Database::run(async |database| {
        let mailer = Arc::new(CapturingMailer::default());
        let server = AuthServer::with_mailer(database, mailer.clone()).await;
        let email = "cross-device@example.test";
        let login = RequestedLogin::start(&server, &mailer, email).await;

        let response = server.http.get(&login.landing_url).unwrap().send().await.unwrap();
        assert_eq!(response.status().as_u16(), 200);
        let other_csrf = common::read_set_cookie(&response, "__Host-zsidp_magic_csrf").unwrap();
        assert_ne!(other_csrf, login.nonce);
        assert!(response.text().await.unwrap().contains("/magic/verify/redeem"));
        assert_link_state(&server, &login.nonce, false).await;
        let other_cookie = format!("__Host-zsidp_magic_csrf={other_csrf}");
        let response = login.redeem(&server, &other_csrf, Some(&other_cookie)).await;
        assert_eq!(response.status().as_u16(), 200);
        assert!(common::read_set_cookie(&response, "__Host-zsidp_session").is_none());
        let html = response.text().await.unwrap();
        let code: String = server.pg.query_one(
            "SELECT code FROM zeroship.magic_completions WHERE csrf_nonce = $1 AND email = $2::citext",
            &[&login.nonce, &email],
        ).await.unwrap().get(0);
        assert!(html.contains(&code), "redeeming browser receives the persisted code");
        assert_link_state(&server, &login.nonce, true).await;
        assert_completion_state(&server, &login.nonce, false, 0).await;
        assert_eq!(session_count(&server).await, 0, "the redeeming browser is not signed in");

        let user = users::find_by_email(&server.orm, email).await.unwrap().unwrap();
        soft_lock(&server, &user.id).await;

        let other_target = common::native_authorize_return_to(
            "another-client", "http://127.0.0.1:9999/another-cb",
        );
        assert_login_rejected(login.complete(&server, &code, &other_target).await).await;
        assert_completion_state(&server, &login.nonce, false, 1).await;
        assert_eq!(session_count(&server).await, 0, "a swapped target cannot mint a session");

        let response = login.complete(&server, &code, &login.return_to).await;
        assert_login_session(&server, &response, email, &login.return_to).await;
        assert_lock_cleared(&server, &user.id).await;
        assert_completion_state(&server, &login.nonce, true, 2).await;
        assert_login_rejected(login.complete(&server, &code, &login.return_to).await).await;
        assert_completion_state(&server, &login.nonce, true, 2).await;
        assert_eq!(session_count(&server).await, 1, "completion replay cannot mint another session");
    }).await;
}

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn invalid_return_targets_issue_no_state_or_mail_while_native_authorize_works() {
    Database::run(async |database| {
        let mailer = Arc::new(CapturingMailer::default());
        let server = AuthServer::with_mailer(database, mailer.clone()).await;
        let email = "target-validation@example.test";
        for invalid in [
            "//evil.example",
            "https://evil.example",
            "/me",
            "/oauth2/authorize?client_id=oac_123\r\nLocation: https://evil.example",
        ] {
            let response = start_request(&server, email, invalid).await;
            assert_eq!(response.status().as_u16(), 200, "target {invalid:?}");
            assert!(common::read_set_cookie(&response, "__Host-zsidp_magic_csrf").is_none());
            assert!(
                response.text().await.unwrap().contains("invalid_request"),
                "target {invalid:?}"
            );
            let row = server
                .pg
                .query_one(
                    "SELECT (SELECT COUNT(*) FROM zeroship.magic_links), \
                 (SELECT COUNT(*) FROM zeroship.magic_completions), \
                 (SELECT COUNT(*) FROM zeroship.users)",
                    &[],
                )
                .await
                .unwrap();
            assert_eq!(
                row.get::<_, i64>(0),
                0,
                "invalid target must not issue a link"
            );
            assert_eq!(
                row.get::<_, i64>(1),
                0,
                "invalid target must not create a completion"
            );
            assert_eq!(
                row.get::<_, i64>(2),
                0,
                "invalid target must not create a user"
            );
            assert!(
                mailer.sent().is_empty(),
                "invalid target must not send email"
            );
        }
        let valid = RequestedLogin::start(&server, &mailer, email).await;
        assert_link_state(&server, &valid.nonce, false).await;
    })
    .await;
}
