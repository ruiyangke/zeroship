//! Mailer wire types — provider-neutral `Email`/`Address`, opaque `MessageId`,
//! and the `MailerError` variants every `Mailer` impl returns.

use thiserror::Error;

/// A single outbound message. Provider-neutral — drivers translate to their
/// own representation (lettre `Message`, Resend JSON, stdout, ...).
#[derive(Debug, Clone)]
pub struct Email {
    pub to: Address,
    pub from: Address,
    pub subject: String,
    pub text: String,
    pub html: Option<String>,
    pub headers: Vec<(String, String)>,
    pub tags: Vec<String>,
}

/// RFC-5322 mailbox: addr-spec + optional display name.
#[derive(Debug, Clone)]
pub struct Address {
    pub email: String,
    pub name: Option<String>,
}

/// Provider-issued message id (Postmark id, lettre Message-ID, stdout sentinel).
/// Opaque to callers — only used for logging + correlating delivery webhooks.
#[derive(Debug, Clone)]
pub struct MessageId(pub String);

/// All failure modes a `Mailer::send` can surface.
///
/// `Suppressed` is intentionally distinct from `Transport` — callers (magic-link
/// issue, password-reset, verification) treat suppression as a soft failure
/// (drop the request silently to avoid revealing whether an address is a
/// bouncer / complainer), whereas `Transport`/`Config` failures are surfaced.
#[derive(Debug, Error)]
pub enum MailerError {
    #[error("suppressed recipient: {0}")]
    Suppressed(String),
    #[error("transport: {0}")]
    Transport(String),
    #[error("config: {0}")]
    Config(String),
}
