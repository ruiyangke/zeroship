//! Logout through the production router with case-owned `PostgreSQL` and browsers.

#![allow(clippy::future_not_send)]

mod backchannel;
mod fixtures;

use crate::common::{self, auth_server::AuthServer, database::Database};
use fixtures::{BrowserSession, account, assert_logged_out, issuer, post};

#[ntex::test]
async fn confirmation_preserves_sessions_and_logout_revokes_only_the_submitting_browser() {
    Database::run(async |database| {
        let server = AuthServer::with_issuer(database, issuer()).await;
        let creator = account(&server, "creator@example.test").await;
        let other = account(&server, "other@example.test").await;
        let mut browser = BrowserSession::login(&server, &creator).await;
        let same_user = BrowserSession::login(&server, &creator).await;
        let other_user = BrowserSession::login(&server, &other).await;
        assert_ne!(browser.id, same_user.id);
        assert_ne!(browser.id, other_user.id);

        let csrf = browser.confirmation(&server).await;
        for session in [&browser, &same_user, &other_user] {
            session.assert_active(&server).await;
        }
        let response = browser.logout(&server, &csrf).await;
        assert_logged_out(&response);
        browser.assert_revoked(&server).await;
        same_user.assert_active(&server).await;
        other_user.assert_active(&server).await;

        // A retry with the original cookie and a request after cookie deletion
        // both remain safe for the other browsers.
        assert_logged_out(&browser.logout(&server, &csrf).await);
        browser.cookies.absorb(&response);
        assert_logged_out(&browser.logout(&server, &csrf).await);
        same_user.assert_active(&server).await;
        other_user.assert_active(&server).await;
    })
    .await;
}

#[ntex::test]
async fn missing_or_mismatched_csrf_cannot_revoke_a_live_session() {
    Database::run(async |database| {
        let server = AuthServer::with_issuer(database, issuer()).await;
        let user = account(&server, "creator@example.test").await;
        let mut browser = BrowserSession::login(&server, &user).await;
        let csrf = browser.confirmation(&server).await;
        let cookies = browser.cookies.header();
        let no_csrf_cookie = format!("__Host-zsidp_session={}", browser.id);
        for (cookies, fields) in [
            (cookies.as_str(), vec![]),
            (cookies.as_str(), vec![("csrf", "mismatched-token")]),
            (no_csrf_cookie.as_str(), vec![("csrf", csrf.as_str())]),
        ] {
            let response = post(&server, "/logout", cookies, &fields).await;
            assert_eq!(response.status().as_u16(), 400);
            assert!(common::read_set_cookie(&response, "__Host-zsidp_session").is_none());
            browser.assert_active(&server).await;
        }
        assert_logged_out(&browser.logout(&server, &csrf).await);
        browser.assert_revoked(&server).await;
    })
    .await;
}
