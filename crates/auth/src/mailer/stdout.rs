//! Stdout/stderr `Mailer` — the default in local dev.
//!
//! Writes a human-readable `=== MAIL ===` block to stderr so developers can
//! copy magic-link tokens out of the terminal, and emits a structured
//! `tracing::info!` for SIEM consumers / test assertions.

use async_trait::async_trait;
use compio_postgres::Client;

use crate::mailer::{check_suppression, Email, Mailer, MailerError, MessageId};

#[derive(Debug, Default)]
pub struct StdoutMailer;

#[async_trait]
impl Mailer for StdoutMailer {
    async fn send(&self, db: &Client, msg: Email) -> Result<MessageId, MailerError> {
        check_suppression(db, &msg.to.email).await?;
        tracing::info!(
            target: "auth.mailer.stdout",
            to = %msg.to.email,
            from = %msg.from.email,
            subject = %msg.subject,
            tags = ?msg.tags,
            "stdout mailer: send",
        );
        eprintln!(
            "\n=== MAIL ===\nTo: {} <{}>\nFrom: {} <{}>\nSubject: {}\n\n{}\n=== END ===\n",
            msg.to.name.as_deref().unwrap_or(""),
            msg.to.email,
            msg.from.name.as_deref().unwrap_or(""),
            msg.from.email,
            msg.subject,
            msg.text,
        );
        Ok(MessageId(format!(
            "stdout-{}",
            uuid::Uuid::new_v4().simple()
        )))
    }
}
