//! An in-memory `Mailer` that keeps what it was asked to send.
//!
//! It exists so a test can assert on the MESSAGE - that the token reached the
//! body, that the subject names the organization, that the recipient is the
//! invited address - without a transport, and so a test can force a transport
//! failure and watch the caller record it.
//!
//! It is NOT `#[cfg(test)]`, for the same reason
//! `zeroship_control::notify::RecordingNotifier` is not: the tests that need it
//! are integration tests in other crates, which cannot see a `#[cfg(test)]`
//! item. It carries no production wiring - the control plane's boot path builds
//! stdout, SMTP or Resend and nothing else.
//!
//! # It honours the suppression contract, and that is the point
//!
//! The module contract says every `Mailer::send` calls [`check_suppression`]
//! before transport. A recorder that skipped it would make every suppression
//! test a test of the recorder rather than of the platform, so this one runs the
//! real query against the real `zeroship.email_suppressions` table. A test
//! proves the suppressed path by inserting a row, not by configuring a fake.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use compio_postgres::Client;

use crate::{check_suppression, Email, Mailer, MailerError, MessageId};

/// A `Mailer` that records instead of transmitting.
#[derive(Clone, Default)]
pub struct RecordingMailer {
    sent: Arc<Mutex<Vec<Email>>>,
    /// When set, `send` returns `MailerError::Transport` with this text INSTEAD
    /// of recording - after the suppression check, so a suppressed recipient is
    /// still reported as suppressed rather than as a transport failure. That
    /// ordering matters: it is the ordering a real driver has.
    transport_failure: Arc<Mutex<Option<String>>>,
}

impl std::fmt::Debug for RecordingMailer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RecordingMailer").finish_non_exhaustive()
    }
}

impl RecordingMailer {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Make every subsequent `send` fail at the transport.
    ///
    /// # Panics
    ///
    /// If a previous holder of the lock panicked. A poisoned recorder means a
    /// test already failed inside it, so propagating is the honest outcome.
    pub fn fail_transport(&self, reason: &str) {
        *self
            .transport_failure
            .lock()
            .expect("recording mailer poisoned") = Some(reason.to_owned());
    }

    /// Every message this mailer accepted, in order.
    ///
    /// # Panics
    ///
    /// If a previous holder of the lock panicked.
    #[must_use]
    pub fn sent(&self) -> Vec<Email> {
        self.sent
            .lock()
            .expect("recording mailer poisoned")
            .clone()
    }

    /// The messages addressed to one recipient.
    ///
    /// # Panics
    ///
    /// If a previous holder of the lock panicked.
    #[must_use]
    pub fn sent_to(&self, email: &str) -> Vec<Email> {
        self.sent()
            .into_iter()
            .filter(|msg| msg.to.email.eq_ignore_ascii_case(email))
            .collect()
    }
}

#[async_trait]
impl Mailer for RecordingMailer {
    async fn send(&self, db: &Client, msg: Email) -> Result<MessageId, MailerError> {
        check_suppression(db, &msg.to.email).await?;
        // Bound to a local so the guard is dropped before the `if let` body,
        // rather than living to the end of the expression.
        let failure = self
            .transport_failure
            .lock()
            .expect("recording mailer poisoned")
            .clone();
        if let Some(reason) = failure {
            return Err(MailerError::Transport(reason));
        }
        let id = MessageId(format!("recorded-{}", uuid::Uuid::new_v4().simple()));
        self.sent
            .lock()
            .expect("recording mailer poisoned")
            .push(msg);
        Ok(id)
    }
}
