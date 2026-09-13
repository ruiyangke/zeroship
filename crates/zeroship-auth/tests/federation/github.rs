#![allow(clippy::future_not_send)]

use super::{
    fixtures::{Fixture, confirm_password},
    provider::{GitHubEmail, Provider, Request, User},
};
use crate::common::database::Database;
use zeroship_auth::{identity::password, store::users};

#[ntex::test]
async fn verified_primary_email_creates_a_session_and_redeemed_code_cannot_replay() {
    Database::run(async |database| {
        let fixture = Fixture::new(
            database,
            Provider::GitHub,
            User::github("creator@example.test"),
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
            [
                Request::Authorize,
                Request::Token,
                Request::User,
                Request::Emails
            ]
        );

        fixture
            .assert_refused(&attempt.complete(&fixture).await, &["upstream_error"])
            .await;
        fixture.assert_counts(1, 1, 1).await;
    })
    .await;
}

#[ntex::test]
async fn existing_password_account_requires_confirmation_before_identity_and_session() {
    Database::run(async |database| {
        let fixture = Fixture::new(
            database,
            Provider::GitHub,
            User::github("creator@example.test"),
        )
        .await;
        let password = "federation confirmation password phrase";
        let hash = password::hash(password).unwrap();
        let account = users::create(
            &fixture.server.pg,
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
async fn noreply_primary_and_ineligible_alternatives_cannot_create_an_account() {
    Database::run(async |database| {
        let mut user = User::github("creator@users.noreply.github.com");
        user.additional_emails = vec![
            GitHubEmail {
                email: "secondary@example.test".into(),
                primary: false,
                verified: true,
            },
            GitHubEmail {
                email: "unverified@example.test".into(),
                primary: true,
                verified: false,
            },
        ];
        let fixture = Fixture::new(database, Provider::GitHub, user).await;
        let attempt = fixture.begin().await;
        fixture
            .assert_refused(&attempt.complete(&fixture).await, &["email_picker_failed"])
            .await;
        assert_eq!(
            fixture.provider.requests(),
            [
                Request::Authorize,
                Request::Token,
                Request::User,
                Request::Emails
            ]
        );
        fixture.assert_counts(0, 0, 0).await;

        let mut user = fixture.provider.user();
        user.email = "creator@example.test".into();
        fixture.provider.set_user(user);
        let attempt = fixture.begin().await;
        fixture
            .assert_session(&attempt.complete(&fixture).await, &attempt, &["oauth"])
            .await;
        fixture.assert_created_profile().await;
    })
    .await;
}

#[ntex::test]
async fn unverified_primary_email_cannot_create_an_account() {
    Database::run(async |database| {
        let mut user = User::github("creator@example.test");
        user.verified = false;
        let fixture = Fixture::new(database, Provider::GitHub, user).await;
        let attempt = fixture.begin().await;
        fixture
            .assert_refused(&attempt.complete(&fixture).await, &["email_picker_failed"])
            .await;
        fixture.assert_counts(0, 0, 0).await;

        let mut user = fixture.provider.user();
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
