use crate::common::{database::Database, message};
use zeroship_mailer::{suppressions, Mailer, MailerError, StdoutMailer};

#[compio::test]
async fn stdout_sends_an_unsuppressed_message() {
    Database::run(async |database| {
        let client = database.connect_as("zeroship_auth").await;
        let sent = StdoutMailer.send(&client, message()).await.unwrap();
        assert!(!sent.0.is_empty());
    })
    .await;
}

#[compio::test]
async fn suppression_refreshes_case_insensitively_and_blocks_stdout() {
    Database::run(async |database| {
        let client = database.connect_as("zeroship_auth").await;
        let message = message();
        assert!(!suppressions::is_suppressed(&client, &message.to.email)
            .await
            .unwrap());
        suppressions::add(&client, &message.to.email, "hard_bounce", None)
            .await
            .unwrap();
        suppressions::add(
            &client,
            &message.to.email.to_uppercase(),
            "complaint",
            Some("spam-report"),
        )
        .await
        .unwrap();
        let rows = client
            .query(
                "SELECT reason, provider_msg FROM zeroship.email_suppressions",
                &[],
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
        assert!(suppressions::is_suppressed(&client, "READER@PERSONAL.TEST")
            .await
            .unwrap());
        assert!(matches!(StdoutMailer.send(&client, message).await,
            Err(MailerError::Suppressed(address)) if address == "reader@personal.test"));
    })
    .await;
}
