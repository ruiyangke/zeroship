#![allow(clippy::future_not_send)]

use super::fixtures::*;
use crate::common::{self, auth_server::AuthServer, database::Database};
use zeroship_auth::{identity::password, store::users};

#[ntex::test]
async fn login_requires_a_second_factor_only_after_enrollment_is_confirmed() {
    Database::run(async |database| {
        let server = AuthServer::start(database).await;
        let user = account(&server, "creator@example.test").await;
        let return_to = AuthServer::fresh_challenge();
        let response = password_login(&server, &user, PASSWORD, &return_to).await;
        assert_session(&server, &response, &user.id, &return_to, "pwd", &["pwd"]).await;
        let mut device = Authenticator::pending(&server, &user.id).await;
        let response = password_login(&server, &user, PASSWORD, &return_to).await;
        assert_session(&server, &response, &user.id, &return_to, "pwd", &["pwd"]).await;
        device.confirm(&server, &user.id).await;
        let mut challenge = Challenge::password(&server, &user, PASSWORD).await;
        assert_counts(&server, 2, 0).await;
        let response = challenge.submit(&server, &device.code()).await;
        assert_session(
            &server,
            &response,
            &user.id,
            &challenge.return_to,
            "pwd",
            &["pwd", "otp"],
        )
        .await;
        assert_counts(&server, 3, 0).await;
    })
    .await;
}

#[ntex::test]
async fn wrong_totp_preserves_the_challenge_for_a_valid_retry() {
    Database::run(async |database| {
        let server = AuthServer::start(database).await;
        let user = account(&server, "creator@example.test").await;
        let device = Authenticator::confirmed(&server, &user.id).await;
        let mut challenge = Challenge::password(&server, &user, PASSWORD).await;
        assert_counts(&server, 0, 0).await;
        challenge.reject_wrong_code(&server, &device).await;
        assert_counts(&server, 0, 0).await;
        assert_eq!(
            unused_backups(&server, &user.id).await,
            device.backups.len()
        );
        let response = challenge.submit(&server, &device.code()).await;
        assert_session(
            &server,
            &response,
            &user.id,
            &challenge.return_to,
            "pwd",
            &["pwd", "otp"],
        )
        .await;
        assert_counts(&server, 1, 0).await;
    })
    .await;
}

#[ntex::test]
async fn backup_code_is_consumed_by_the_route_and_a_replay_can_retry_with_an_unused_code() {
    Database::run(async |database| {
        let server = AuthServer::start(database).await;
        let user = account(&server, "creator@example.test").await;
        let device = Authenticator::confirmed(&server, &user.id).await;
        let mut first = Challenge::password(&server, &user, PASSWORD).await;
        let response = first.submit(&server, &device.backups[0]).await;
        assert_session(
            &server,
            &response,
            &user.id,
            &first.return_to,
            "pwd",
            &["pwd", "otp"],
        )
        .await;
        assert_eq!(
            unused_backups(&server, &user.id).await,
            device.backups.len() - 1
        );
        let mut replay = Challenge::password(&server, &user, PASSWORD).await;
        assert_refused(
            replay.submit(&server, &device.backups[0]).await,
            "invalid code",
        )
        .await;
        assert_counts(&server, 1, 0).await;
        assert_eq!(
            unused_backups(&server, &user.id).await,
            device.backups.len() - 1
        );
        let response = replay.submit(&server, &device.backups[1]).await;
        assert_session(
            &server,
            &response,
            &user.id,
            &replay.return_to,
            "pwd",
            &["pwd", "otp"],
        )
        .await;
        assert_counts(&server, 2, 0).await;
    })
    .await;
}

#[ntex::test]
async fn concurrent_challenges_cannot_spend_the_same_backup_code_twice() {
    Database::run(async |database| {
        let server = AuthServer::start(database).await;
        let other_server = AuthServer::start(database).await;
        let user = account(&server, "creator@example.test").await;
        let device = Authenticator::confirmed(&server, &user.id).await;
        let mut first = Challenge::password(&server, &user, PASSWORD).await;
        let mut second = Challenge::password(&other_server, &user, PASSWORD).await;
        let pids = [
            server
                .pg
                .query_one("SELECT pg_backend_pid()", &[])
                .await
                .unwrap()
                .get::<_, i32>(0),
            other_server
                .pg
                .query_one("SELECT pg_backend_pid()", &[])
                .await
                .unwrap()
                .get::<_, i32>(0),
        ];
        assert_ne!(
            pids[0], pids[1],
            "the race needs independent database sessions"
        );
        let mut admin = database.connect().await;
        let transaction = admin.transaction().await.unwrap();
        let rows = transaction
            .query(
                "SELECT id FROM zeroship.totp_backup_codes WHERE user_id = $1 FOR UPDATE",
                &[&user.id.as_str()],
            )
            .await
            .unwrap();
        assert_eq!(rows.len(), device.backups.len());
        // Both servers can read the unused codes, but neither can consume one
        // until their independent database sessions are observed waiting here.
        let (a, b, blocked) = futures::join!(
            first.submit(&server, &device.backups[0]),
            second.submit(&other_server, &device.backups[0]),
            async {
                let blocked = database.wait_until_blocked(&pids).await;
                transaction.commit().await.unwrap();
                blocked
            },
        );
        assert!(
            blocked,
            "both challenge requests must reach the consumption race"
        );
        let ((winner, target), loser) = if a.status().as_u16() == 303 {
            ((a, first.return_to), b)
        } else {
            ((b, second.return_to), a)
        };
        assert_session(&server, &winner, &user.id, &target, "pwd", &["pwd", "otp"]).await;
        assert_refused(loser, "invalid code").await;
        assert_counts(&server, 1, 0).await;
        assert_eq!(
            unused_backups(&server, &user.id).await,
            device.backups.len() - 1
        );
    })
    .await;
}

#[ntex::test]
async fn changed_password_invalidates_the_challenge_without_spending_its_backup_code() {
    Database::run(async |database| {
        let server = AuthServer::start(database).await;
        let user = account(&server, "creator@example.test").await;
        let device = Authenticator::confirmed(&server, &user.id).await;
        let mut stale = Challenge::password(&server, &user, PASSWORD).await;
        let new_password = "new second factor fixture password phrase";
        users::update_password_hash(
            server.pg.as_ref(),
            &user.id,
            &password::hash(new_password).unwrap(),
        )
        .await
        .unwrap();
        assert_refused(
            stale.submit(&server, &device.backups[0]).await,
            "session expired, sign in again",
        )
        .await;
        assert_counts(&server, 0, 0).await;
        assert_eq!(
            unused_backups(&server, &user.id).await,
            device.backups.len()
        );
        let mut fresh = Challenge::password(&server, &user, new_password).await;
        let response = fresh.submit(&server, &device.backups[0]).await;
        assert_session(
            &server,
            &response,
            &user.id,
            &fresh.return_to,
            "pwd",
            &["pwd", "otp"],
        )
        .await;
        assert_counts(&server, 1, 0).await;
    })
    .await;
}

#[ntex::test]
async fn challenge_requires_its_signed_cookie_csrf_and_return_target() {
    Database::run(async |database| {
        let server = AuthServer::start(database).await;
        let user = account(&server, "creator@example.test").await;
        let device = Authenticator::confirmed(&server, &user.id).await;
        let mut challenge = Challenge::password(&server, &user, PASSWORD).await;
        let valid_cookies = challenge.cookies.header();
        let without_challenge = format!("__Host-zsidp_csrf={}", challenge.csrf);
        let forged_challenge = format!("{without_challenge}; __Host-zsidp_2fa=forged.signature");
        let other_target = AuthServer::fresh_challenge();
        for (cookies, csrf, target) in [
            (
                without_challenge.as_str(),
                challenge.csrf.as_str(),
                challenge.return_to.as_str(),
            ),
            (
                forged_challenge.as_str(),
                challenge.csrf.as_str(),
                challenge.return_to.as_str(),
            ),
            (
                valid_cookies.as_str(),
                "unrelated-csrf",
                challenge.return_to.as_str(),
            ),
            (
                valid_cookies.as_str(),
                challenge.csrf.as_str(),
                other_target.as_str(),
            ),
        ] {
            let response = post(
                &server,
                "/login/2fa",
                cookies,
                &[
                    ("csrf", csrf),
                    ("return_to", target),
                    ("code", &device.backups[0]),
                ],
            )
            .await;
            assert_eq!(response.status().as_u16(), 303);
            let location = url::Url::parse(&server.auth_base)
                .unwrap()
                .join(&common::location(&response))
                .unwrap();
            assert_eq!(location.path(), "/login");
            assert!(common::read_set_cookie(&response, "__Host-zsidp_session").is_none());
            assert_counts(&server, 0, 0).await;
            assert_eq!(
                unused_backups(&server, &user.id).await,
                device.backups.len()
            );
        }
        let response = challenge.submit(&server, &device.backups[0]).await;
        assert_session(
            &server,
            &response,
            &user.id,
            &challenge.return_to,
            "pwd",
            &["pwd", "otp"],
        )
        .await;
        assert_counts(&server, 1, 0).await;
    })
    .await;
}
