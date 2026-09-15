#![allow(clippy::future_not_send)]

use super::{
    fixtures::Fixture,
    provider::{Provider, Request, User},
};
use crate::common::database::Database;
use zeroship_auth::store::users;

fn user(provider: Provider) -> User {
    match provider {
        Provider::Google => User::google("creator@gmail.com"),
        Provider::GitHub => User::github("creator@example.test"),
    }
}

async fn rejected_state_preserves_the_original_flow(provider: Provider) {
    Database::run(async |database| {
        let fixture = Fixture::new(database, provider, user(provider)).await;
        let attempt = fixture.begin().await;
        let missing = attempt.send(&fixture, &attempt.callback, None).await;
        fixture
            .assert_refused(&missing, &["stash_cookie_missing"])
            .await;
        fixture.assert_counts(0, 0, 0).await;
        assert_eq!(fixture.provider.requests(), [Request::Authorize]);

        let mut wrong = attempt.callback.clone();
        let pairs: Vec<_> = wrong
            .query_pairs()
            .filter(|(key, _)| key != "state")
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect();
        wrong.set_query(None);
        wrong
            .query_pairs_mut()
            .extend_pairs(pairs)
            .append_pair("state", "unrelated-state");
        let response = attempt.send(&fixture, &wrong, Some(&attempt.stash)).await;
        fixture
            .assert_refused(&response, &["stash_cookie_missing", "state_mismatch"])
            .await;
        fixture.assert_counts(0, 0, 0).await;
        assert_eq!(fixture.provider.requests(), [Request::Authorize]);

        fixture
            .assert_session(&attempt.complete(&fixture).await, &attempt, &["oauth"])
            .await;
        fixture.assert_created_profile().await;
    })
    .await;
}

async fn callback_recovers_soft_lock(provider: Provider) {
    Database::run(async |database| {
        let fixture = Fixture::new(database, provider, user(provider)).await;
        let initial = fixture.begin().await;
        let id = fixture
            .assert_session(&initial.complete(&fixture).await, &initial, &["oauth"])
            .await;
        for _ in 0..users::lockout::THRESHOLD {
            users::record_login_failure(&fixture.server.orm, &id)
                .await
                .unwrap();
        }
        let row = fixture
            .server
            .pg
            .query_one(
                "SELECT failed_login_count, locked_until > NOW() FROM zeroship.users WHERE id = $1",
                &[&id.as_str()],
            )
            .await
            .unwrap();
        assert_eq!(row.get::<_, i32>(0), users::lockout::THRESHOLD);
        assert!(row.get::<_, bool>(1));

        let attempt = fixture.begin().await;
        assert_eq!(
            fixture
                .assert_session(&attempt.complete(&fixture).await, &attempt, &["oauth"])
                .await,
            id
        );
        let row = fixture
            .server
            .pg
            .query_one(
                "SELECT failed_login_count, locked_until IS NULL FROM zeroship.users WHERE id = $1",
                &[&id.as_str()],
            )
            .await
            .unwrap();
        assert_eq!(row.get::<_, i32>(0), 0);
        assert!(row.get::<_, bool>(1));
        fixture.assert_counts(1, 1, 2).await;
    })
    .await;
}

async fn callback_cannot_reenable_disabled_account(provider: Provider) {
    Database::run(async |database| {
        let fixture = Fixture::new(database, provider, user(provider)).await;
        let initial = fixture.begin().await;
        let id = fixture
            .assert_session(&initial.complete(&fixture).await, &initial, &["oauth"])
            .await;
        let admin = database.connect().await;
        assert_eq!(
            admin
                .execute(
                    "UPDATE zeroship.users SET disabled_at = NOW() WHERE id = $1",
                    &[&id.as_str()]
                )
                .await
                .unwrap(),
            1
        );
        let attempt = fixture.begin().await;
        fixture
            .assert_refused(&attempt.complete(&fixture).await, &["account_ineligible"])
            .await;
        assert!(
            users::find_by_id(&fixture.server.orm, &id)
                .await
                .unwrap()
                .unwrap()
                .disabled_at
                .is_some()
        );
        fixture.assert_counts(1, 1, 1).await;

        assert_eq!(
            admin
                .execute(
                    "UPDATE zeroship.users SET disabled_at = NULL WHERE id = $1",
                    &[&id.as_str()]
                )
                .await
                .unwrap(),
            1
        );
        let attempt = fixture.begin().await;
        assert_eq!(
            fixture
                .assert_session(&attempt.complete(&fixture).await, &attempt, &["oauth"])
                .await,
            id
        );
        fixture.assert_counts(1, 1, 2).await;
    })
    .await;
}

#[ntex::test]
async fn google_rejects_missing_stash_and_mismatched_state() {
    rejected_state_preserves_the_original_flow(Provider::Google).await;
}

#[ntex::test]
async fn github_rejects_missing_stash_and_mismatched_state() {
    rejected_state_preserves_the_original_flow(Provider::GitHub).await;
}

#[ntex::test]
async fn google_recovers_a_soft_locked_account() {
    callback_recovers_soft_lock(Provider::Google).await;
}

#[ntex::test]
async fn github_recovers_a_soft_locked_account() {
    callback_recovers_soft_lock(Provider::GitHub).await;
}

#[ntex::test]
async fn google_refuses_a_disabled_account_until_reenabled() {
    callback_cannot_reenable_disabled_account(Provider::Google).await;
}

#[ntex::test]
async fn github_refuses_a_disabled_account_until_reenabled() {
    callback_cannot_reenable_disabled_account(Provider::GitHub).await;
}
