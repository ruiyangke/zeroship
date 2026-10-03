//! Password refusal, account lockout and recovery through the production router.

mod fixtures;
mod enumeration;

use crate::support::{self, auth_server::AuthServer, database::Database};
use fixtures::*;
use zeroship_auth::{identity::password_reset, store::users};

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn failed_logins_lock_the_account_and_success_after_expiry_clears_it() {
    Database::run(async |database| {
        let server = AuthServer::start(database).await;
        let account = user(&server, &email("lockout")).await;
        lock_through_login(&server, &account).await;
        assert_eq!(session_count(&server, &[&account.id]).await, 0);

        database.connect().await.execute(
            "UPDATE zeroship.users SET locked_until = NOW() - INTERVAL '1 second' WHERE id = $1",
            &[&account.id.as_str()],
        ).await.unwrap();
        assert_state(
            &server,
            &account.id,
            users::lockout::THRESHOLD,
            LockState::Expired,
        )
        .await;

        let response = login(&server, &account.email, PASSWORD, &fixture_ip()).await;
        assert_session(&server, &response, &account.id).await;
        assert_state(&server, &account.id, 0, LockState::Clear).await;
        assert_eq!(session_count(&server, &[&account.id]).await, 1);
    })
    .await;
}

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn reset_recovers_a_locked_account_before_the_next_password_login() {
    Database::run(async |database| {
        let server = AuthServer::start(database).await;
        let account = user(&server, &email("recover")).await;
        let other = user(&server, &email("other")).await;
        lock_through_login(&server, &account).await;
        lock_through_login(&server, &other).await;
        let token = password_reset::issue(&server.pg, &account.email)
            .await
            .unwrap();
        let new_password = "recovered password fixture phrase";

        let response = reset(&server, &token.raw, new_password, &fixture_ip()).await;
        assert_eq!(response.status().as_u16(), 302);
        assert_eq!(support::location(&response), "/login");
        assert!(support::read_set_cookie(&response, "__Host-zsidp_session").is_none());
        assert_eq!(session_count(&server, &[&account.id]).await, 0);
        assert!(
            !password_reset::is_live(&server.pg, &token.raw)
                .await
                .unwrap()
        );
        // Observe recovery before a successful password login can clear it.
        assert_state(&server, &account.id, 0, LockState::Clear).await;
        assert_state(
            &server,
            &other.id,
            users::lockout::THRESHOLD,
            LockState::Active,
        )
        .await;

        let replay = reset(&server, &token.raw, WRONG_PASSWORD, &fixture_ip()).await;
        assert_eq!(replay.status().as_u16(), 200);
        assert!(
            replay
                .text()
                .await
                .unwrap()
                .contains("reset link invalid or expired")
        );
        assert_rejected(login(&server, &account.email, PASSWORD, &fixture_ip()).await).await;
        let response = login(&server, &account.email, new_password, &fixture_ip()).await;
        assert_session(&server, &response, &account.id).await;
        assert_state(&server, &account.id, 0, LockState::Clear).await;
    })
    .await;
}

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn password_failures_share_the_public_refusal_and_cannot_mint_sessions() {
    Database::run(async |database| {
        let server = AuthServer::start(database).await;
        let locked = user(&server, &email("locked")).await;
        let wrong = user(&server, &email("wrong")).await;
        let disabled = user(&server, &email("disabled")).await;
        let oauth_only = users::create(&server.orm, &email("oauth"), "OAuth only", None)
            .await
            .unwrap();
        let absent = email("absent");
        lock_through_login(&server, &locked).await;
        database
            .connect()
            .await
            .execute(
                "UPDATE zeroship.users SET disabled_at = NOW() WHERE id = $1",
                &[&disabled.id.as_str()],
            )
            .await
            .unwrap();

        let case_ids = [&locked.id, &wrong.id, &disabled.id, &oauth_only.id];
        for (email, password) in [
            (locked.email.as_str(), PASSWORD),
            (absent.as_str(), PASSWORD),
            (oauth_only.email.as_str(), PASSWORD),
            (wrong.email.as_str(), WRONG_PASSWORD),
        ] {
            assert_rejected(login(&server, email, password, &fixture_ip()).await).await;
            assert_eq!(
                session_count(&server, &case_ids).await,
                0,
                "refusal for {email}"
            );
        }
        let response = login(&server, &disabled.email, PASSWORD, &fixture_ip()).await;
        assert_eq!(response.status().as_u16(), 403);
        assert!(support::read_set_cookie(&response, "__Host-zsidp_session").is_none());
        assert_eq!(session_count(&server, &case_ids).await, 0);
        assert_state(
            &server,
            &locked.id,
            users::lockout::THRESHOLD,
            LockState::Active,
        )
        .await;
        assert_state(&server, &wrong.id, 1, LockState::Clear).await;
        assert_state(&server, &disabled.id, 0, LockState::Clear).await;
        assert_state(&server, &oauth_only.id, 0, LockState::Clear).await;

        let response = login(&server, &wrong.email, PASSWORD, &fixture_ip()).await;
        assert_session(&server, &response, &wrong.id).await;
        assert_state(&server, &wrong.id, 0, LockState::Clear).await;
        assert_eq!(session_count(&server, &case_ids).await, 1);
    })
    .await;
}
