//! Mailer abstraction — provider-neutral `Mailer` trait + per-driver impls.
//!
//! ## Contract
//!
//! Every `Mailer::send` MUST call [`check_suppression`] before transport and
//! return [`MailerError::Suppressed`] if the recipient is in
//! `auth.email_suppressions`. This is enforced by every driver in this module
//! (stdout/smtp/resend) — never call the underlying transport directly.
//!
//! The `db: &Client` argument on `send` is what makes that contract
//! mechanically enforceable: the suppression check is a SQL query, so the
//! trait demands a connection handle alongside the message.

pub mod resend;
pub mod smtp;
pub mod stdout;
pub mod types;

use async_trait::async_trait;
use compio_postgres::Client;

use crate::store::suppressions;
pub use resend::{ResendConfig, ResendMailer};
pub use smtp::{SmtpConfig, SmtpMailer};
pub use stdout::StdoutMailer;
pub use types::{Address, Email, MailerError, MessageId};

/// Outbound email transport. Implementations MUST check
/// `auth.email_suppressions` before transport (via [`check_suppression`]) and
/// return [`MailerError::Suppressed`] for suppressed recipients.
#[async_trait]
pub trait Mailer: Send + Sync + std::fmt::Debug {
    async fn send(&self, db: &Client, msg: Email) -> Result<MessageId, MailerError>;
}

/// Suppression-list check that every `Mailer` impl calls first thing.
///
/// # Errors
///
/// - [`MailerError::Suppressed`] if `email` is in `auth.email_suppressions`.
/// - [`MailerError::Transport`] if the suppression query itself fails (DB
///   error). We wrap as `Transport` because suppression-check failure is a
///   transport-layer fault from the caller's point of view — the message
///   could not be delivered.
pub async fn check_suppression(db: &Client, email: &str) -> Result<(), MailerError> {
    if suppressions::is_suppressed(db, email)
        .await
        .map_err(|e| MailerError::Transport(format!("suppression check: {e}")))?
    {
        return Err(MailerError::Suppressed(email.to_string()));
    }
    Ok(())
}
