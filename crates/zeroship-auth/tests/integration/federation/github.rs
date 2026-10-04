#![allow(clippy::future_not_send)]

use super::{
    fixtures::{Fixture, confirm_password, email},
    provider::{GitHubEmail, GitHubTokenReply, Provider, Request, User},
};
use crate::support::database::Database;
use zeroship_auth::{identity::password, store::users};

#[ntex::test]
async fn verified_primary_email_creates_a_session_and_redeemed_code_cannot_replay() {
    Database::run(async |database| {
        let fixture = Fixture::new(
            database,
            Provider::GitHub,
            User::github(&email("creator", "example.test")),
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
            User::github(&email("creator", "example.test")),
        )
        .await;
        let password = "federation confirmation password phrase";
        let hash = password::hash(password).await.unwrap();
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
async fn noreply_primary_and_ineligible_alternatives_cannot_create_an_account() {
    Database::run(async |database| {
        let mut user = User::github(&email("creator", "users.noreply.github.com"));
        user.additional_emails = vec![
            GitHubEmail {
                email: email("secondary", "example.test"),
                primary: false,
                verified: true,
            },
            GitHubEmail {
                email: email("unverified", "example.test"),
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
        user.email = email("creator", "example.test");
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
        let mut user = User::github(&email("creator", "example.test"));
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

/// A GitHub token response that names a credential type the exchange does not
/// understand must be refused before the access token is used.
#[ntex::test]
async fn unknown_token_type_is_refused_before_the_token_is_used() {
    Database::run(async |database| {
        let fixture = Fixture::new(
            database,
            Provider::GitHub,
            User::github(&email("creator", "example.test")),
        )
        .await;
        fixture.provider.set_github_token(GitHubTokenReply::Fields {
            scope: "read:user user:email".into(),
            token_type: Some("mac".into()),
        });
        let attempt = fixture.begin().await;
        fixture
            .assert_refused(&attempt.complete(&fixture).await, &["upstream_error"])
            .await;
        assert_eq!(
            fixture.provider.requests(),
            [Request::Authorize, Request::Token]
        );
        fixture.assert_counts(0, 0, 0).await;
    })
    .await;
}

/// A GitHub token response that omits `token_type` is missing a required
/// field and must be refused rather than defaulted to a usable credential.
#[ntex::test]
async fn missing_token_type_is_refused() {
    Database::run(async |database| {
        let fixture = Fixture::new(
            database,
            Provider::GitHub,
            User::github(&email("creator", "example.test")),
        )
        .await;
        fixture.provider.set_github_token(GitHubTokenReply::Fields {
            scope: "read:user user:email".into(),
            token_type: None,
        });
        let attempt = fixture.begin().await;
        fixture
            .assert_refused(&attempt.complete(&fixture).await, &["upstream_error"])
            .await;
        // A missing required field is a decode failure, not a credential of
        // an empty type.
        let error = fixture.last_callback_error().await;
        assert!(
            error.contains("github token parse"),
            "missing token_type was not treated as a parse failure: {error}"
        );
        assert_eq!(
            fixture.provider.requests(),
            [Request::Authorize, Request::Token]
        );
        fixture.assert_counts(0, 0, 0).await;
    })
    .await;
}

/// `Bearer` is compared case-insensitively, so the canonical spelling GitHub
/// may return is accepted.
#[ntex::test]
async fn uppercase_bearer_token_type_is_accepted() {
    Database::run(async |database| {
        let fixture = Fixture::new(
            database,
            Provider::GitHub,
            User::github(&email("creator", "example.test")),
        )
        .await;
        fixture.provider.set_github_token(GitHubTokenReply::Fields {
            scope: "read:user user:email".into(),
            token_type: Some("Bearer".into()),
        });
        let attempt = fixture.begin().await;
        fixture
            .assert_session(&attempt.complete(&fixture).await, &attempt, &["oauth"])
            .await;
        fixture.assert_created_profile().await;
    })
    .await;
}

/// A scope entry that merely contains `user:email` as a substring is not the
/// `user:email` grant.
#[ntex::test]
async fn scope_substring_is_not_a_user_email_grant() {
    Database::run(async |database| {
        let fixture = Fixture::new(
            database,
            Provider::GitHub,
            User::github(&email("creator", "example.test")),
        )
        .await;
        fixture.provider.set_github_token(GitHubTokenReply::Fields {
            scope: "user:emails".into(),
            token_type: Some("bearer".into()),
        });
        let attempt = fixture.begin().await;
        fixture
            .assert_refused(&attempt.complete(&fixture).await, &["upstream_error"])
            .await;
        assert_eq!(
            fixture.provider.requests(),
            [Request::Authorize, Request::Token]
        );
        fixture.assert_counts(0, 0, 0).await;
    })
    .await;
}

/// A comma-separated scope list grants `user:email` when one whole entry is
/// exactly that scope.
#[ntex::test]
async fn comma_separated_scope_list_grants_user_email() {
    Database::run(async |database| {
        let fixture = Fixture::new(
            database,
            Provider::GitHub,
            User::github(&email("creator", "example.test")),
        )
        .await;
        fixture.provider.set_github_token(GitHubTokenReply::Fields {
            scope: "read:user,user:email".into(),
            token_type: Some("bearer".into()),
        });
        let attempt = fixture.begin().await;
        fixture
            .assert_session(&attempt.complete(&fixture).await, &attempt, &["oauth"])
            .await;
        fixture.assert_created_profile().await;
    })
    .await;
}

/// A decode failure must keep the parse error kind without carrying the raw
/// response body, which can hold the access token, into errors and logs.
#[ntex::test]
async fn parse_failure_does_not_leak_the_response_body() {
    const MARKER: &str = "ghs_body_must_not_reach_errors";
    Database::run(async |database| {
        let fixture = Fixture::new(
            database,
            Provider::GitHub,
            User::github(&email("creator", "example.test")),
        )
        .await;
        fixture
            .provider
            .set_github_token(GitHubTokenReply::Body(format!(
                "{{\"access_token\":\"{MARKER}\",\"scope\":\"read:user user:email\"}} trailing"
            )));
        let attempt = fixture.begin().await;
        fixture
            .assert_refused(&attempt.complete(&fixture).await, &["upstream_error"])
            .await;
        let error = fixture.last_callback_error().await;
        assert!(
            !error.contains(MARKER),
            "parse error leaked the response body: {error}"
        );
        assert!(
            error.contains("github token parse"),
            "parse error kind changed: {error}"
        );
        fixture.assert_counts(0, 0, 0).await;
    })
    .await;
}
