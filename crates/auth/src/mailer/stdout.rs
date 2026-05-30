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
        // The rendered `To:` header is `header_to` when set (relay forwards put
        // the alias there so the real inbox is never in a header); the envelope
        // recipient (`RCPT TO`) is always `msg.to`.
        let header_to = msg.header_to.as_ref().unwrap_or(&msg.to);
        tracing::info!(
            target: "auth.mailer.stdout",
            rcpt_to = %msg.to.email,
            header_to = %header_to.email,
            from = %msg.from.email,
            reply_to = msg.reply_to.as_ref().map(|a| a.email.as_str()),
            envelope_from = msg.envelope_from.as_deref(),
            subject = %msg.subject,
            headers = ?msg.headers,
            tags = ?msg.tags,
            "stdout mailer: send",
        );
        // The dev default deliberately surfaces the rendered `To:` (header_to),
        // the envelope `RCPT TO`, `reply_to`, `envelope_from`, and the header set
        // so a developer can eyeball the relay header surgery (rewritten
        // From/Reply-To, pinned envelope-from, the X-ZS-Relay marker, and that
        // the real inbox is ONLY the RCPT TO) straight out of the terminal.
        eprintln!(
            "\n=== MAIL ===\nTo: {} <{}>\nRCPT TO (envelope): {}\nFrom: {} <{}>\nReply-To: {}\nReturn-Path: {}\nSubject: {}\nHeaders: {:?}\n\n{}\n=== END ===\n",
            header_to.name.as_deref().unwrap_or(""),
            header_to.email,
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
