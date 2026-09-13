//! Uniform duplicate responses and recoverable refusal without account effects.

use super::fixtures::*;
use crate::common::{CapturingMailer, auth_server::AuthServer, database::Database};
use std::sync::Arc;

#[ntex::test]
async fn duplicate_signup_preserves_the_account_and_matches_a_fresh_signup_response() {
    Database::run(async |database| {
        let mailer = Arc::new(CapturingMailer::default());
        let server = AuthServer::with_mailer(database, mailer.clone()).await;
        let form = Form::get(&server, "/signup").await;
        let initial = assert_redirect(
            form.submit(&server, "creator@example.test", NAME, IP).await,
            "/me",
        )
        .await;
        let original = created(&server, "creator@example.test").await;
        let form = Form::get(&server, "/signup").await;
        let duplicate = post(
            &server,
            "/signup",
            &form.cookies.header(),
            &[
                ("csrf", &form.csrf),
                ("return_to", &form.return_to),
                ("email", " CREATOR@EXAMPLE.TEST "),
                ("name", "Impostor"),
                ("password", "different signup password phrase"),
            ],
            IP,
        )
        .await;
        let duplicate = assert_redirect(duplicate, "/me").await;
        let form = Form::get(&server, "/signup").await;
        let fresh = assert_redirect(
            form.submit(&server, "fresh@example.test", NAME, IP).await,
            "/me",
        )
        .await;
        assert_eq!(duplicate, initial);
        assert_eq!(duplicate, fresh);
        let unchanged = created(&server, "creator@example.test").await;
        assert_eq!(unchanged.id, original.id);
        assert_eq!(unchanged.password_hash, original.password_hash);
        created(&server, "fresh@example.test").await;
        assert_counts(&server, 2, 2).await;
        assert_eq!(mailer.sent().len(), 2);
        verification_link(&server, &mailer, "creator@example.test");
        verification_link(&server, &mailer, "fresh@example.test");
    })
    .await;
}

#[ntex::test]
async fn a_database_refusal_creates_no_account_or_mail_and_the_same_submission_can_recover() {
    Database::run(async |database| {
        let mailer = Arc::new(CapturingMailer::default());
        let server = AuthServer::with_mailer(database, mailer.clone()).await;
        let admin = database.connect().await;
        admin
            .batch_execute(
                "ALTER TABLE zeroship.users ADD CONSTRAINT signup_refusal \
             CHECK (email <> 'creator@example.test'::citext)",
            )
            .await
            .unwrap();
        let form = Form::get(&server, "/signup").await;
        let response = form.submit(&server, "creator@example.test", NAME, IP).await;
        assert_eq!(response.status().as_u16(), 200);
        assert!(response.headers().get("location").is_none());
        assert!(response.text().await.unwrap().contains("contact support"));
        assert_counts(&server, 0, 0).await;
        assert!(mailer.sent().is_empty());
        let row = server
            .pg
            .query_one(
                "SELECT outcome, detail->>'reason', detail->>'db_code' \
             FROM zeroship.audit_events WHERE event_type = 'signup_failed'",
                &[],
            )
            .await
            .unwrap();
        assert_eq!(row.get::<_, String>(0), "failure");
        assert_eq!(row.get::<_, String>(1), "users_create_failed");
        assert_eq!(row.get::<_, String>(2), "23514");

        admin
            .batch_execute("ALTER TABLE zeroship.users DROP CONSTRAINT signup_refusal")
            .await
            .unwrap();
        assert_redirect(
            form.submit(&server, "creator@example.test", NAME, IP).await,
            "/me",
        )
        .await;
        created(&server, "creator@example.test").await;
        verification_link(&server, &mailer, "creator@example.test");
        assert_counts(&server, 1, 1).await;
    })
    .await;
}

#[ntex::test]
async fn signup_requires_a_matching_form_cookie_before_creating_an_account() {
    Database::run(async |database| {
        let mailer = Arc::new(CapturingMailer::default());
        let server = AuthServer::with_mailer(database, mailer.clone()).await;
        for cookies in ["", "__Host-zsidp_csrf=unrelated-token"] {
            let form = Form::get(&server, "/signup").await;
            let response = post(
                &server,
                "/signup",
                cookies,
                &[
                    ("csrf", &form.csrf),
                    ("return_to", &form.return_to),
                    ("email", "creator@example.test"),
                    ("name", NAME),
                    ("password", PASSWORD),
                ],
                IP,
            )
            .await;
            assert_eq!(response.status().as_u16(), 200);
            assert!(response.headers().get("location").is_none());
            assert!(response.text().await.unwrap().contains("invalid request"));
            assert_counts(&server, 0, 0).await;
            assert!(mailer.sent().is_empty());
        }
        let form = Form::get(&server, "/signup").await;
        assert_redirect(
            form.submit(&server, "creator@example.test", NAME, IP).await,
            "/me",
        )
        .await;
        created(&server, "creator@example.test").await;
        assert_counts(&server, 1, 1).await;
        verification_link(&server, &mailer, "creator@example.test");
    })
    .await;
}
