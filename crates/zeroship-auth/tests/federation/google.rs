#![allow(clippy::future_not_send)]

use super::{
    fixtures::{Fixture, confirm_password},
    provider::{Provider, Request, TokenIssue, User},
};
use crate::common::database::Database;
use zeroship_auth::{identity::password, store::users};

#[ntex::test]
async fn existing_password_account_requires_confirmation_before_identity_and_session() {
    Database::run(async |database| {
        let fixture = Fixture::new(
            database,
            Provider::Google,
            User::google("creator@gmail.com"),
        )
        .await;
        let password = "federation confirmation password phrase";
        let hash = password::hash(password).unwrap();
        let account = users::create(
            &fixture.server.orm,
            &fixture.provider.user().email,
            "Local account",
            Some(&hash),
        )
        .await
        .unwrap();
        let attempt = fixture.begin().await;
        let response = attempt.complete(&fixture).await;
        fixture.assert_counts(1, 0, 0).await;
        let confirmed = confirm_password(&fixture, &response, password).await;
        assert_eq!(
            fixture
                .assert_session(&confirmed, &attempt, &["oauth", "pwd"])
                .await,
            account.id
        );
        fixture.assert_counts(1, 1, 1).await;
    })
    .await;
}

#[ntex::test]
async fn verified_gmail_creates_a_session_and_redeemed_code_cannot_replay() {
    Database::run(async |database| {
        let fixture = Fixture::new(
            database,
            Provider::Google,
            User::google("creator@gmail.com"),
        )
        .await;
        let attempt = fixture.begin().await;
        let response = attempt.complete(&fixture).await;
        fixture
            .assert_session(&response, &attempt, &["oauth"])
            .await;
        fixture.assert_stash_cleared(&response);
        fixture.assert_created_profile().await;
        assert_eq!(
            fixture.provider.requests(),
            [Request::Authorize, Request::Token, Request::Jwks]
        );

        let replay = attempt.complete(&fixture).await;
        fixture.assert_refused(&replay, &["verify_failed"]).await;
        fixture.assert_counts(1, 1, 1).await;
    })
    .await;
}

#[ntex::test]
async fn external_email_requires_verified_workspace_authority() {
    Database::run(async |database| {
        let fixture = Fixture::new(
            database,
            Provider::Google,
            User::google("creator@example.test"),
        )
        .await;
        let attempt = fixture.begin().await;
        fixture
            .assert_refused(&attempt.complete(&fixture).await, &["linker_failed"])
            .await;
        fixture.assert_counts(0, 0, 0).await;

        let mut user = fixture.provider.user();
        user.hosted_domain = Some("example.test".into());
        user.verified = false;
        fixture.provider.set_user(user.clone());
        let attempt = fixture.begin().await;
        fixture
            .assert_refused(
                &attempt.complete(&fixture).await,
                &["linker_failed", "linker_failed"],
            )
            .await;
        fixture.assert_counts(0, 0, 0).await;

        user.verified = true;
        fixture.provider.set_user(user);
        let attempt = fixture.begin().await;
        fixture
            .assert_session(&attempt.complete(&fixture).await, &attempt, &["oauth"])
            .await;
        fixture.assert_created_profile().await;
    })
    .await;
}

async fn invalid_token_cannot_persist_a_login(issue: TokenIssue) {
    Database::run(async |database| {
        let fixture = Fixture::new(
            database,
            Provider::Google,
            User::google("creator@gmail.com"),
        )
        .await;
        fixture.provider.set_token_issue(issue);
        let attempt = fixture.begin().await;
        fixture
            .assert_refused(&attempt.complete(&fixture).await, &["verify_failed"])
            .await;
        fixture.assert_counts(0, 0, 0).await;

        fixture.provider.set_token_issue(TokenIssue::Valid);
        let attempt = fixture.begin().await;
        fixture
            .assert_session(&attempt.complete(&fixture).await, &attempt, &["oauth"])
            .await;
        fixture.assert_created_profile().await;
    })
    .await;
}

#[ntex::test]
async fn signed_token_with_another_nonce_is_refused() {
    invalid_token_cannot_persist_a_login(TokenIssue::WrongNonce).await;
}

#[ntex::test]
async fn token_signed_by_an_untrusted_key_is_refused() {
    invalid_token_cannot_persist_a_login(TokenIssue::UntrustedSignature).await;
}
