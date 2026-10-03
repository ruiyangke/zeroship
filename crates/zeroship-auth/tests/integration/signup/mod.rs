//! Signup continuations, account effects and verification through the auth router.

#![allow(clippy::future_not_send)]

mod fixtures;
mod rate_limit;
mod refusal;

use crate::support::{self, CapturingMailer, auth_server::AuthServer, database::Database};
use fixtures::*;
use std::sync::Arc;
use zeroship_auth::store::users;

#[ntex::test]
async fn the_login_pages_signup_link_creates_an_account_with_a_usable_verification_email() {
    Database::run(async |database| {
        let mailer = Arc::new(CapturingMailer::default());
        let server = AuthServer::with_mailer(database, mailer.clone()).await;
        let ip = fixture_ip();
        let creator = email("login-link");
        let href = signup_href(&server, "/login").await;
        let form = Form::get(&server, &href).await;
        assert_eq!(form.return_to, "/me");
        assert_redirect(form.submit(&server, &creator, NAME, &ip).await, "/me").await;
        let user = created(&server, &creator).await;
        assert!(user.email_verified_at.is_none());
        assert_counts(&server, &[creator.as_str()], 1, 1).await;

        let link = verification_link(&server, &mailer, &creator);
        let response = server
            .http
            .get(link.as_str())
            .unwrap()
            .send()
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), 200);
        let csrf = support::read_set_cookie(&response, "__Host-zsidp_csrf").unwrap();
        let html = response.text().await.unwrap();
        let token = form_field(&html, "token");
        assert_eq!(
            Some(token.as_str()),
            link.query_pairs()
                .find(|(key, _)| key == "token")
                .map(|(_, v)| v.into_owned())
                .as_deref()
        );
        assert!(
            users::find_by_id(&server.orm, &user.id)
                .await
                .unwrap()
                .unwrap()
                .email_verified_at
                .is_none()
        );
        let response = post(
            &server,
            "/verify/redeem",
            &format!("__Host-zsidp_csrf={csrf}"),
            &[("csrf", &csrf), ("token", &token)],
            &ip,
        )
        .await;
        assert_eq!(response.status().as_u16(), 200);
        assert!(response.text().await.unwrap().contains("Email verified"));
        assert!(
            users::find_by_id(&server.orm, &user.id)
                .await
                .unwrap()
                .unwrap()
                .email_verified_at
                .is_some()
        );
        assert_counts(&server, &[creator.as_str()], 1, 1).await;
        sign_in(&server, &user, "/me", &ip).await;
    })
    .await;
}

#[ntex::test]
async fn signup_preserves_the_native_authorization_target_rendered_by_login() {
    Database::run(async |database| {
        let mailer = Arc::new(CapturingMailer::default());
        let server = AuthServer::with_mailer(database, mailer.clone()).await;
        let ip = fixture_ip();
        let creator = email("native-target");
        let target = AuthServer::fresh_challenge();
        let href = signup_href(&server, &query("/login", &target)).await;
        let form = Form::get(&server, &href).await;
        assert_eq!(form.return_to, target);
        assert_redirect(form.submit(&server, &creator, NAME, &ip).await, &target).await;
        let user = created(&server, &creator).await;
        verification_link(&server, &mailer, &creator);
        assert_counts(&server, &[creator.as_str()], 1, 1).await;
        sign_in(&server, &user, &target, &ip).await;
    })
    .await;
}

#[ntex::test]
async fn bare_signup_is_usable_and_off_origin_targets_cannot_escape_the_safe_continuation() {
    Database::run(async |database| {
        let mailer = Arc::new(CapturingMailer::default());
        let server = AuthServer::with_mailer(database, mailer.clone()).await;
        let ip = fixture_ip();
        let bare_email = email("bare");
        let mut emitted = vec![bare_email.clone()];
        let bare = Form::get(&server, "/signup").await;
        assert_eq!(bare.return_to, "/me");
        assert_counts(&server, &[bare_email.as_str()], 0, 0).await;
        assert!(mailer.sent().is_empty());
        assert_redirect(bare.submit(&server, &bare_email, NAME, &ip).await, "/me").await;
        let mut count = 1;
        for bad in [
            "//outside.example",
            "https://outside.example",
            "/\\outside.example",
        ] {
            let mut form = Form::get(&server, &query("/signup", bad)).await;
            assert_eq!(form.return_to, "/me");
            let seen: Vec<&str> = emitted.iter().map(String::as_str).collect();
            assert_counts(&server, &seen, count, count).await;
            form.return_to = bad.to_owned();
            let escape = email(&format!("escape-{count}"));
            assert_redirect(form.submit(&server, &escape, NAME, &ip).await, "/me").await;
            created(&server, &escape).await;
            verification_link(&server, &mailer, &escape);
            emitted.push(escape);
            count += 1;
        }
        let seen: Vec<&str> = emitted.iter().map(String::as_str).collect();
        assert_counts(&server, &seen, count, count).await;
    })
    .await;
}
