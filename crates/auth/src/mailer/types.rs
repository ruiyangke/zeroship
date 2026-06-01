//! Mailer wire types — provider-neutral `Email`/`Address`, opaque `MessageId`,
//! and the `MailerError` variants every `Mailer` impl returns.

use thiserror::Error;

/// A single outbound message. Provider-neutral — drivers translate to their
/// own representation (lettre `Message`, Resend JSON, stdout, ...).
#[derive(Debug, Clone)]
pub struct Email {
    /// The envelope recipient (SMTP `RCPT TO`). For relay forwards this is the
    /// user's REAL inbox — and it appears ONLY here, never in a rendered header
    /// (the `To:` header is taken from [`Email::header_to`] when set). For
    /// transactional mail it is also the rendered `To:` (`header_to` is `None`).
    pub to: Address,
    /// The rendered `To:` header, when it must differ from the envelope
    /// recipient. `None` ⇒ the driver renders `To:` from [`Email::to`] (the
    /// transactional case — byte-for-byte unchanged). Relay forwards set this to
    /// the relay alias so the real inbox (`to.email`) is NEVER emitted in any
    /// header line (sub-spec §5.3: the real inbox appears only in the `RCPT TO`).
    pub header_to: Option<Address>,
    pub from: Address,
    /// Reply-To mailbox. `None` ⇒ no `Reply-To` header is emitted. Relay
    /// forwards set this to the relay alias so user replies route back to the
    /// relay (which bounces in v1), never to the real inbox or the third party.
    pub reply_to: Option<Address>,
    /// SMTP envelope-from (MAIL FROM / `Return-Path`). `None` ⇒ the driver uses
    /// `from.email` as today (transactional mail is byte-for-byte unchanged).
    /// Relay forwards pin this to the relay bounce mailbox so forwarded-mail
    /// bounces NEVER route to the real inbox or the original sender.
    pub envelope_from: Option<String>,
    pub subject: String,
    pub text: String,
    pub html: Option<String>,
    /// Arbitrary `(name, value)` headers. Now actually emitted by the SMTP and
    /// Resend drivers (the loop-protection `X-ZS-Relay` marker rides here).
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
