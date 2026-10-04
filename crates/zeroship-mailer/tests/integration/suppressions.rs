use crate::support::{database::Database, message, recipient};
use zeroship_mailer::{suppressions, Mailer, MailerError, StdoutMailer};

#[compio::test]
async fn stdout_sends_an_unsuppressed_message() {
    Database::run(async |database| {
        let client = database.connect_as("zeroship_auth").await;
        let sent = StdoutMailer
            .send(&client, message(&recipient("stdout-unsuppressed")))
            .await
            .unwrap();
        assert!(!sent.0.is_empty());
    })
    .await;
}

#[compio::test]
async fn suppression_refreshes_case_insensitively_and_blocks_stdout() {
    Database::run(async |database| {
        let client = database.connect_as("zeroship_auth").await;
        let to = recipient("suppression-refresh");
        let email = message(&to);
        assert!(!suppressions::is_suppressed(&client, &to).await.unwrap());
        suppressions::add(&client, &to, "hard_bounce", None)
            .await
            .unwrap();
        suppressions::add(
            &client,
            &to.to_uppercase(),
            "complaint",
            Some("spam-report"),
        )
        .await
        .unwrap();
        let rows = client
            .query(
                "SELECT reason, provider_msg FROM zeroship.email_suppressions \
                 WHERE email = $1::citext",
                &[&to],
            )
            .await
            .unwrap();
        assert_eq!(
            rows.len(),
            1,
            "case variants must update the same suppression"
        );
        assert_eq!(rows[0].get::<_, String>(0), "complaint");
        assert_eq!(
            rows[0].get::<_, Option<String>>(1).as_deref(),
            Some("spam-report")
        );
        assert!(suppressions::is_suppressed(&client, &to.to_uppercase())
            .await
            .unwrap());
        assert!(matches!(StdoutMailer.send(&client, email).await,
            Err(MailerError::Suppressed(address)) if address == to));
    })
    .await;
}
