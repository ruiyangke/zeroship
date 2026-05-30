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
            reply_to = msg.reply_to.as_ref().map(|a| a.email.as_str()),
            envelope_from = msg.envelope_from.as_deref(),
            subject = %msg.subject,
            headers = ?msg.headers,
            tags = ?msg.tags,
            "stdout mailer: send",
        );
        // The dev default deliberately surfaces `reply_to`, `envelope_from`, and
        // the header set so a developer can eyeball the relay header surgery
        // (rewritten From/Reply-To, pinned envelope-from, the X-ZS-Relay marker)
        // straight out of the terminal. Headers are printed, not otherwise acted on.
        eprintln!(
            "\n=== MAIL ===\nTo: {} <{}>\nFrom: {} <{}>\nReply-To: {}\nReturn-Path: {}\nSubject: {}\nHeaders: {:?}\n\n{}\n=== END ===\n",
            msg.to.name.as_deref().unwrap_or(""),
            msg.to.email,
            msg.from.name.as_deref().unwrap_or(""),
            msg.from.email,
            msg.reply_to
                .as_ref()
                .map_or("", |a| a.email.as_str()),
            msg.envelope_from.as_deref().unwrap_or(""),
            msg.subject,
            msg.headers,
            msg.text,
        );
        Ok(MessageId(format!(
            "stdout-{}",
            uuid::Uuid::new_v4().simple()
        )))
    }
}
