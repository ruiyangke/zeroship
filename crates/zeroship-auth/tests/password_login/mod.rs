//! Password refusal, account lockout and recovery through the production router.

mod fixtures;
mod enumeration;

use crate::common::{self, auth_server::AuthServer, database::Database};
use fixtures::*;
use zeroship_auth::{identity::password_reset, store::users};

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn failed_logins_lock_the_account_and_success_after_expiry_clears_it() {
    Database::run(async |database| {
        let server = AuthServer::start(database).await;
        let account = user(&server, "lockout@example.test").await;
        lock_through_login(&server, &account).await;
        assert_eq!(session_count(&server).await, 0);

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

        let response = login(&server, &account.email, PASSWORD, "192.0.2.1").await;
        assert_session(&server, &response, &account.id).await;
        assert_state(&server, &account.id, 0, LockState::Clear).await;
        assert_eq!(session_count(&server).await, 1);
    })
    .await;
}

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn reset_recovers_a_locked_account_before_the_next_password_login() {
    Database::run(async |database| {
        let server = AuthServer::start(database).await;
        let account = user(&server, "recover@example.test").await;
        let other = user(&server, "other@example.test").await;
        lock_through_login(&server, &account).await;
        lock_through_login(&server, &other).await;
        let token = password_reset::issue(&server.pg, &account.email)
            .await
            .unwrap();
        let new_password = "recovered password fixture phrase";

        let response = reset(&server, &token.raw, new_password).await;
        assert_eq!(response.status().as_u16(), 302);
        assert_eq!(common::location(&response), "/login");
        assert!(common::read_set_cookie(&response, "__Host-zsidp_session").is_none());
        assert_eq!(session_count(&server).await, 0);
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

        let replay = reset(&server, &token.raw, WRONG_PASSWORD).await;
        assert_eq!(replay.status().as_u16(), 200);
        assert!(
            replay
                .text()
                .await
                .unwrap()
                .contains("reset link invalid or expired")
        );
        assert_rejected(login(&server, &account.email, PASSWORD, "192.0.2.2").await).await;
        let response = login(&server, &account.email, new_password, "192.0.2.3").await;
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
        let locked = user(&server, "locked@example.test").await;
        let wrong = user(&server, "wrong@example.test").await;
        let disabled = user(&server, "disabled@example.test").await;
        let oauth_only = users::create(&server.orm, "oauth@example.test", "OAuth only", None)
            .await
            .unwrap();
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

        for (email, password) in [
            (locked.email.as_str(), PASSWORD),
            ("absent@example.test", PASSWORD),
            (oauth_only.email.as_str(), PASSWORD),
            (wrong.email.as_str(), WRONG_PASSWORD),
        ] {
            assert_rejected(login(&server, email, password, "203.0.113.1").await).await;
            assert_eq!(session_count(&server).await, 0, "refusal for {email}");
        }
        let response = login(&server, &disabled.email, PASSWORD, "203.0.113.2").await;
        assert_eq!(response.status().as_u16(), 403);
        assert!(common::read_set_cookie(&response, "__Host-zsidp_session").is_none());
        assert_eq!(session_count(&server).await, 0);
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

        let response = login(&server, &wrong.email, PASSWORD, "203.0.113.3").await;
        assert_session(&server, &response, &wrong.id).await;
        assert_state(&server, &wrong.id, 0, LockState::Clear).await;
        assert_eq!(session_count(&server).await, 1);
    })
    .await;
}
