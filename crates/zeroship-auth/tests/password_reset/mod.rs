//! HTTP password-reset effects in databases owned by each case.

#![allow(
    clippy::future_not_send,
    reason = "HTTP and database fixtures stay on their owning runtime"
)]

mod fixtures;
mod landing;

use crate::common::database::Database;
use ntex::web::test;
use std::sync::Arc;
use zeroship_auth::identity::{credentials, magic_link, password_reset};
use zeroship_auth::store::sessions;

#[ntex::test]
async fn reset_revokes_the_users_sessions_and_audits_the_effects() {
    Database::run(async |database| {
        let pg = Arc::new(database.connect_as_auth().await);
        let user = fixtures::user(&pg, "reset@example.test").await;
        let other = fixtures::user(&pg, "other@example.test").await;
        let app = fixtures::app(database).await;
        fixtures::idp_session(&pg, &user).await;
        fixtures::gateway_session(database, &user, &app).await;
        let other_idp = fixtures::idp_session(&pg, &other).await;
        let other_gateway = fixtures::gateway_session(database, &other, &app).await;
        let reset = password_reset::issue(&pg, &user.email).await.unwrap();

        fixtures::submit(pg.clone(), &reset.raw).await;

        let remaining = pg
            .query_one(
                "SELECT \
             (SELECT COUNT(*) FROM zeroship.idp_sessions WHERE user_id = $1) AS idp, \
             (SELECT COUNT(*) FROM zeroship.gateway_sessions WHERE user_id = $1) AS gateway",
                &[&user.id],
            )
            .await
            .unwrap();
        assert_eq!(remaining.get::<_, i64>("idp"), 0);
        assert_eq!(remaining.get::<_, i64>("gateway"), 0);
        let mut preserved: Vec<_> = sessions::list_by_user(&pg, other.id)
            .await
            .unwrap()
            .iter()
            .map(|session| session.id)
            .collect();
        preserved.sort();
        let mut expected = vec![other_idp, other_gateway];
        expected.sort();
        assert_eq!(preserved, expected, "another user's sessions remain listed as active");

        let changes: i64 = pg
            .query_one(
                "SELECT COUNT(*) FROM zeroship.audit_events WHERE actor_user_id = $1 \
             AND event_type = 'password_changed' AND outcome = 'success'",
                &[&user.id],
            )
            .await
            .unwrap()
            .get(0);
        assert_eq!(changes, 1);
        let detail: serde_json::Value = pg
            .query_one(
                "SELECT detail FROM zeroship.audit_events WHERE actor_user_id = $1 \
             AND event_type = 'sessions_revoked_after_password_reset' AND outcome = 'success'",
                &[&user.id],
            )
            .await
            .unwrap()
            .get(0);
        assert_eq!(
            detail,
            serde_json::json!({
                "idp_sessions": 1, "gateway_sessions": 1, "magic_tokens": 0, "magic_completions": 0,
            })
        );
    })
    .await;
}

#[ntex::test]
async fn reset_consumes_only_the_users_pending_magic_login_state() {
    Database::run(async |database| {
        let pg = Arc::new(database.connect_as_auth().await);
        let user = fixtures::user(&pg, "reset@example.test").await;
        let other = fixtures::user(&pg, "other@example.test").await;
        let magic = magic_link::issue(&pg, &user.email, "login").await.unwrap();
        let other_magic = magic_link::issue(&pg, &other.email, "login").await.unwrap();
        let admin = database.connect().await;
        for person in [&user, &other] {
            admin.execute(
                "INSERT INTO zeroship.magic_completions \
                 (csrf_nonce, code, email, login_challenge, expires_at) \
                 VALUES ($1, '123456', $2::citext, 'reset-login-challenge', NOW() + INTERVAL '5 minutes')",
                &[&format!("completion-{}", person.id), &person.email],
            ).await.unwrap();
        }
        let reset = password_reset::issue(&pg, &user.email).await.unwrap();

        fixtures::submit(pg.clone(), &reset.raw).await;

        assert!(magic_link::redeem_pending(&pg, &magic.raw).await.unwrap().is_none());
        assert!(magic_link::redeem_pending(&pg, &other_magic.raw).await.unwrap().is_some());
        let remaining = admin.query("SELECT email::text FROM zeroship.magic_completions", &[])
            .await.unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].get::<_, String>(0), other.email);
        let detail: serde_json::Value = pg.query_one(
            "SELECT detail FROM zeroship.audit_events WHERE actor_user_id = $1 \
             AND event_type = 'sessions_revoked_after_password_reset' AND outcome = 'success'",
            &[&user.id],
        ).await.unwrap().get(0);
        assert_eq!(detail["magic_tokens"], 1);
        assert_eq!(detail["magic_completions"], 1);
    }).await;
}

#[ntex::test]
async fn reset_revokes_app_anchors_and_marks_each_pairwise_family() {
    Database::run(async |database| {
        let pg = Arc::new(database.connect_as_auth().await);
        let user = fixtures::user(&pg, "reset@example.test").await;
        let other = fixtures::user(&pg, "other@example.test").await;
        let app = fixtures::app(database).await;
        let second_app = fixtures::app(database).await;
        let subject = fixtures::identity(database, &user, &app).await;
        let second_subject = fixtures::identity(database, &user, &second_app).await;
        let other_subject = fixtures::identity(database, &other, &app).await;
        let anchor = fixtures::anchor(database, &user, &app).await;
        let second_anchor = fixtures::anchor(database, &user, &second_app).await;
        let other_anchor = fixtures::anchor(database, &other, &app).await;
        let reset = password_reset::issue(&pg, &user.email).await.unwrap();

        fixtures::submit(pg.clone(), &reset.raw).await;

        let admin = database.connect().await;
        for id in [anchor, second_anchor] {
            let revoked: bool = admin
                .query_one(
                    "SELECT revoked_at IS NOT NULL FROM zeroship.app_session_anchors WHERE id = $1",
                    &[&id],
                )
                .await
                .unwrap()
                .get(0);
            assert!(revoked, "reset must revoke anchors across the user's apps");
        }
        let other_live: bool = admin
            .query_one(
                "SELECT revoked_at IS NULL FROM zeroship.app_session_anchors WHERE id = $1",
                &[&other_anchor],
            )
            .await
            .unwrap()
            .get(0);
        assert!(other_live, "another user's anchor survives");

        let mut markers: Vec<(String, String)> = admin
            .query("SELECT client_id, sub FROM zeroship.token_revocations", &[])
            .await
            .unwrap()
            .iter()
            .map(|row| (row.get(0), row.get(1)))
            .collect();
        markers.sort();
        let mut expected = vec![
            (app.client_id.clone(), subject),
            (second_app.client_id, second_subject),
        ];
        expected.sort();
        assert_eq!(
            markers, expected,
            "mark every app family without marking another user's subject"
        );
        assert!(!markers.contains(&(app.client_id, other_subject)));

        let epoch: i64 = pg
            .query_one(
                "SELECT credential_version FROM zeroship.users WHERE id = $1",
                &[&user.id],
            )
            .await
            .unwrap()
            .get(0);
        assert_eq!(epoch, user.credential_version + 1);
    })
    .await;
}

/// A gateway identity and an app refresh grant can name the same family.
/// Reset must complete when both sources contribute that family marker.
#[ntex::test]
async fn reset_completes_with_an_identity_and_refresh_grant_for_the_same_family() {
    Database::run(async |database| {
        let pg = Arc::new(database.connect_as_auth().await);
        let user = fixtures::user(&pg, "reset@example.test").await;
        let other = fixtures::user(&pg, "other@example.test").await;
        let app = fixtures::app(database).await;
        let subject = fixtures::identity(database, &user, &app).await;
        let other_subject = fixtures::identity(database, &other, &app).await;
        let session = fixtures::refresh_session(&pg, &user, &app, &subject).await;
        let other_session = fixtures::refresh_session(&pg, &other, &app, &other_subject).await;

        let ip = "192.0.2.2";
        let req = test::TestRequest::default()
            .header("x-forwarded-for", ip)
            .to_http_request();
        let before = credentials::verify_password_credentials(
            &pg,
            &req,
            &app.client_id,
            ip,
            &user.email,
            fixtures::OLD_PASSWORD,
        )
        .await
        .expect("the old password works before reset");
        assert_eq!(before.id, user.id);
        let reset = password_reset::issue(&pg, &user.email).await.unwrap();

        fixtures::submit(pg.clone(), &reset.raw).await;

        let old = credentials::verify_password_credentials(
            &pg,
            &req,
            &app.client_id,
            ip,
            &user.email,
            fixtures::OLD_PASSWORD,
        )
        .await;
        assert!(
            matches!(old, Err(credentials::CredentialError::InvalidCredentials)),
            "the old password must be rejected as invalid credentials: {old:?}"
        );
        let new = credentials::verify_password_credentials(
            &pg,
            &req,
            &app.client_id,
            ip,
            &user.email,
            fixtures::NEW_PASSWORD,
        )
        .await
        .expect("the new password authenticates after reset");
        assert_eq!(new.id, user.id);
        assert_eq!(new.credential_version, before.credential_version + 1);
        assert!(!password_reset::is_live(&pg, &reset.raw).await.unwrap());

        let revoked: bool = pg
            .query_one(
                "SELECT revoked_at IS NOT NULL FROM zeroship.sessions WHERE id = $1",
                &[&session],
            )
            .await
            .unwrap()
            .get(0);
        let other_live: bool = pg
            .query_one(
                "SELECT revoked_at IS NULL FROM zeroship.sessions WHERE id = $1",
                &[&other_session],
            )
            .await
            .unwrap()
            .get(0);
        assert!(revoked);
        assert!(other_live, "another user's refresh session survives");
        let markers = pg
            .query("SELECT client_id, sub FROM zeroship.token_revocations", &[])
            .await
            .unwrap();
        assert_eq!(markers.len(), 1);
        assert_eq!(markers[0].get::<_, String>(0), app.client_id);
        assert_eq!(markers[0].get::<_, String>(1), subject);
    })
    .await;
}
