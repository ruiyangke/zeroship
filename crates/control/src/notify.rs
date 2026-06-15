//! Billing-notification seam — map a billing lifecycle/money event to a creator
//! email, with a provider-side idempotency key (billing-ops gap #26, PR-6; design
//! §"0051 notifications" + flow F + the template inventory).
//!
//! ## The [`BillingNotifier`] seam
//!
//! Mirrors [`crate::tax::TaxProvider`] / [`crate::refund::RefundProvider`] /
//! [`crate::metering::provider::MeteringProvider`]: an `#[async_trait(?Send)]`
//! object-safe trait, a default impl ([`MailerNotifier`]) over the relocated
//! `zeroship-mailer` `Mailer`, and a recording fake for tests. The notifier lives on
//! [`crate::AppState`] as `notifier`, so the notify cron reaches it through `&AppState`.
//!
//! The notify cron (`cron::billing_notify`) owns the durable two-phase claim ledger
//! (`billing_notifications`); the notifier is a thin "render template → `Mailer::send`"
//! step. The split keeps the multi-node-safety (claim-before-send + advisory lock) in
//! the cron and the I/O capability in the seam.
//!
//! ## MAJOR-A: at-least-once delivery, exactly-once claim, idempotent effect
//!
//! The claim INSERT — not the send — arbitrates the multi-node race (exactly-once
//! CLAIM). But a crash AFTER `Mailer::send` succeeds and BEFORE the `status='sent'`
//! flip commits leaves a `pending` row that re-drives past `NOTIFY_REDRIVE_HORIZON` →
//! a SECOND send (at-least-once DELIVERY). To make the delivery EFFECT idempotent, the
//! notifier passes a provider-side [`Email::idempotency_key`] =
//! `(creator_id, kind, transition_id)` — the SAME tuple as the claim PK. A provider
//! that honours it (Resend) drops the duplicate, so the recipient sees ONE email. The
//! stdout/SMTP dev drivers do not dedup (documented, not a launch blocker).

use std::sync::Arc;

use async_trait::async_trait;
use compio_postgres::Client;
use zeroship_mailer::{Address, Email, Mailer, MailerError};

/// Every distinct billing notification. Each maps to exactly one template
/// ([`render_template`]) and is the `kind` column of the `billing_notifications`
/// send-ledger (`billing_notification_kind` domain).
///
/// PR-6 wires the cron for the dunning- and invoice/refund-driven kinds; PR-8 adds
/// `disputed` (off `billing_disputes`). The `spend_*` kinds are still reserved in the
/// domain (their source tables exist) for a follow-up — see the cron's scan set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BillingNotificationKind {
    /// First failed charge — `creator_billing_status_history` (`*→past_due`, the
    /// reason carries the failure). v1 maps the active→past_due edge to `past_due`;
    /// `payment_failed` is reserved for an explicit first-failure signal.
    PaymentFailed,
    /// active→past_due transition (`creator_billing_status_history`).
    PastDue,
    /// Dunning exhausted → suspended (`creator_billing_status_history`).
    Suspended,
    /// past_due/suspended→active recovery (`creator_billing_status_history`).
    Recovered,
    /// A newly-finalized invoice (`invoices`, incl. $0 paid-by-credit).
    InvoiceFinalized,
    /// A newly-issued refund (`refunds`).
    Refunded,
    /// A newly-opened dispute / chargeback (`billing_disputes`; PR-8). transition_id =
    /// the `dsp_…` dispute id.
    Disputed,
    /// A payout to the creator's connected account FAILED (`payout_failures`; webhook
    /// follow-up). transition_id = the `pof_…` payout-failure id.
    PayoutFailed,
    /// An end-user's Connect checkout charge failed (`connect_checkout_failures`; webhook
    /// follow-up). transition_id = the `cof_…` checkout-failure id. Informational.
    CheckoutFailed,
}

impl BillingNotificationKind {
    /// The `billing_notification_kind` domain string (the `kind` column value).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PaymentFailed => "payment_failed",
            Self::PastDue => "past_due",
            Self::Suspended => "suspended",
            Self::Recovered => "recovered",
            Self::InvoiceFinalized => "invoice_finalized",
            Self::Refunded => "refunded",
            Self::Disputed => "disputed",
            Self::PayoutFailed => "payout_failed",
            Self::CheckoutFailed => "checkout_failed",
        }
    }

    /// Parse the domain string back to a kind (the cron reads it off the ledger).
    #[must_use]
    pub fn from_str(s: &str) -> Option<Self> {
        Some(match s {
            "payment_failed" => Self::PaymentFailed,
            "past_due" => Self::PastDue,
            "suspended" => Self::Suspended,
            "recovered" => Self::Recovered,
            "invoice_finalized" => Self::InvoiceFinalized,
            "refunded" => Self::Refunded,
            "disputed" => Self::Disputed,
            "payout_failed" => Self::PayoutFailed,
            "checkout_failed" => Self::CheckoutFailed,
            _ => return None,
        })
    }
}

/// One notification to send: the recipient + the kind + the formatted, frozen money
/// detail the template renders. Money amounts are formatted from the frozen
/// invoice/refund fields by the cron (never re-priced here).
#[derive(Debug, Clone)]
pub struct Notification {
    /// The creator's email (resolved from `users` by the cron).
    pub to_email: String,
    /// The creator's display name, if known (`users.name`).
    pub to_name: Option<String>,
    pub kind: BillingNotificationKind,
    /// Detail rendered into the template body. Pre-formatted strings (e.g. a money
    /// amount `"$12.34"`, a month `"June 2026"`) so the seam stays string-only.
    pub detail: NotificationDetail,
    /// `(creator_id, kind, transition_id)` — the claim PK, threaded into the
    /// provider-side `Idempotency-Key` so a re-driven send is idempotent at the
    /// provider (MAJOR-A).
    pub idempotency_key: String,
}

/// Per-kind detail the template body renders. All fields pre-formatted to strings so
/// the notifier never touches money math (the cron formats from frozen fields).
#[derive(Debug, Clone, Default)]
pub struct NotificationDetail {
    /// e.g. `"June 2026"` for an invoice-finalized email.
    pub period_label: Option<String>,
    /// e.g. `"$12.34"` — the frozen invoice total or refund amount.
    pub amount_label: Option<String>,
    /// For `refunded`: `"cash to your card"` / `"credit to your balance"`.
    pub refund_destination_label: Option<String>,
}

/// The `From:` address billing notifications are sent from. A constant so the seam is
/// self-contained; a real deployment overrides it via config when needed.
const BILLING_FROM_EMAIL: &str = "billing@zeroship.ai";
const BILLING_FROM_NAME: &str = "zeroship billing";

/// Render the (subject, text-body) pair for a notification. Transactional, plain-text,
/// locale `en` only (v1). Kept deliberately simple — the seam matters more than copy
/// (design: "Keep them simple"). The template KEY carries a locale segment so adding
/// locales later is a template-pack drop, not a code change.
#[must_use]
pub fn render_template(n: &Notification) -> (String, String) {
    let name = n.to_name.as_deref().unwrap_or("there");
    let amount = n.detail.amount_label.as_deref().unwrap_or("");
    let period = n.detail.period_label.as_deref().unwrap_or("");
    match n.kind {
        BillingNotificationKind::PaymentFailed => (
            "We couldn't charge your card — retrying".to_owned(),
            format!(
                "Hi {name},\n\nWe weren't able to charge your payment method for your \
                 zeroship usage. We'll retry automatically. To avoid any interruption, \
                 please make sure your card on file is up to date.\n\n— zeroship billing\n"
            ),
        ),
        BillingNotificationKind::PastDue => (
            "Your account is past due — update payment".to_owned(),
            format!(
                "Hi {name},\n\nYour zeroship account is past due. Your apps are still \
                 running, but please update your payment method to avoid suspension.\n\n\
                 — zeroship billing\n"
            ),
        ),
        BillingNotificationKind::Suspended => (
            "Your apps are suspended for non-payment".to_owned(),
            format!(
                "Hi {name},\n\nYour zeroship apps have been suspended because an invoice \
                 went unpaid past the grace period. Settle the outstanding balance to \
                 restore service — suspension is reversible the moment payment is \
                 received.\n\n— zeroship billing\n"
            ),
        ),
        BillingNotificationKind::Recovered => (
            "Payment received — your account is active".to_owned(),
            format!(
                "Hi {name},\n\nThanks — we received your payment and your zeroship account \
                 is active again. Any suspended apps are back online.\n\n— zeroship billing\n"
            ),
        ),
        BillingNotificationKind::InvoiceFinalized => (
            format!("Your {period} invoice: {amount}"),
            format!(
                "Hi {name},\n\nYour zeroship invoice for {period} is ready: {amount}.\n\n\
                 You can view the full breakdown in your dashboard.\n\n— zeroship billing\n"
            ),
        ),
        BillingNotificationKind::Refunded => {
            let dest = n
                .detail
                .refund_destination_label
                .as_deref()
                .unwrap_or("to your account");
            (
                format!("We refunded {amount}"),
                format!(
                    "Hi {name},\n\nWe've refunded {amount} {dest}. No action is needed.\n\n\
                     — zeroship billing\n"
                ),
            )
        }
        BillingNotificationKind::Disputed => (
            "A charge was disputed — what happens next".to_owned(),
            format!(
                "Hi {name},\n\nA payment of {amount} on one of your zeroship invoices was \
                 disputed by the cardholder. The funds are held by the card network while \
                 the dispute is reviewed. No action is needed from you right now — we'll \
                 update you when it resolves.\n\n— zeroship billing\n"
            ),
        ),
        BillingNotificationKind::PayoutFailed => (
            "Your payout couldn't be completed".to_owned(),
            format!(
                "Hi {name},\n\nA payout of {amount} to your connected bank account couldn't \
                 be completed — your bank rejected it (often a closed account or incorrect \
                 details). The funds are safe and will be re-attempted once you fix your \
                 payout details. Please review your bank account in your dashboard.\n\n\
                 — zeroship billing\n"
            ),
        ),
        BillingNotificationKind::CheckoutFailed => (
            "A customer's payment didn't go through".to_owned(),
            format!(
                "Hi {name},\n\nA customer's payment of {amount} on one of your apps didn't \
                 go through (their card was declined or the charge failed). No money moved \
                 and no action is needed from you — your customer can simply try again. \
                 We're letting you know for visibility.\n\n— zeroship billing\n"
            ),
        ),
    }
}

/// The notify seam: render + send one notification. The cron owns the claim ledger;
/// this is the I/O step.
///
/// `#[async_trait(?Send)]` for object-safety as `Arc<dyn BillingNotifier>`, matching the
/// tax/refund/metering seams (compio is per-thread; control's futures are `!Send`).
#[async_trait(?Send)]
pub trait BillingNotifier: Send + Sync + std::fmt::Debug {
    /// Render the template for `n.kind` and send the email, passing a provider-side
    /// idempotency key so a re-driven send is an effective no-op at the provider.
    ///
    /// # Errors
    /// Returns [`MailerError`] from the underlying transport. [`MailerError::Suppressed`]
    /// is the deliverability skip the cron treats as a non-fatal "recipient suppressed".
    async fn notify(&self, db: &Client, n: &Notification) -> Result<(), MailerError>;
}

/// The production notifier: wraps the relocated `zeroship-mailer` `Mailer`. Renders the
/// template, builds an `Email` carrying the `(creator_id, kind, transition_id)`
/// idempotency key, and delegates to `Mailer::send` (which enforces the suppression
/// contract first).
#[derive(Clone)]
pub struct MailerNotifier {
    mailer: Arc<dyn Mailer>,
}

impl std::fmt::Debug for MailerNotifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MailerNotifier").finish_non_exhaustive()
    }
}

impl MailerNotifier {
    #[must_use]
    pub fn new(mailer: Arc<dyn Mailer>) -> Self {
        Self { mailer }
    }
}

#[async_trait(?Send)]
impl BillingNotifier for MailerNotifier {
    async fn notify(&self, db: &Client, n: &Notification) -> Result<(), MailerError> {
        let (subject, text) = render_template(n);
        let msg = Email {
            to: Address { email: n.to_email.clone(), name: n.to_name.clone() },
            header_to: None,
            from: Address {
                email: BILLING_FROM_EMAIL.to_owned(),
                name: Some(BILLING_FROM_NAME.to_owned()),
            },
            reply_to: None,
            envelope_from: None,
            subject,
            text,
            html: None,
            headers: vec![],
            tags: vec!["billing".to_owned(), n.kind.as_str().to_owned()],
            // MAJOR-A: the provider-side dedup key = the claim PK tuple.
            idempotency_key: Some(n.idempotency_key.clone()),
        };
        self.mailer.send(db, msg).await.map(|_id| ())
    }
}

/// A recording, in-memory [`BillingNotifier`] for tests — captures every `(to_email,
/// kind, idempotency_key)` it is asked to send WITHOUT touching email or the
/// suppression DB. Honours its own per-key dedup so a test can prove the provider-side
/// `Idempotency-Key` makes a re-driven send an effective no-op (MAJOR-A): a second
/// `notify` with the SAME `idempotency_key` records the attempt but reports
/// `delivered = false` (deduped), so a test asserts ONE delivery per key across
/// re-drives.
///
/// Not `#[cfg(test)]` so integration tests (separate crates) can use it; it carries no
/// production wiring (the boot path always builds a [`MailerNotifier`]).
#[derive(Debug, Default, Clone)]
pub struct RecordingNotifier {
    inner: Arc<std::sync::Mutex<RecordingState>>,
    /// When set, every `notify` returns this error instead of recording a delivery —
    /// lets a test simulate a transport failure (the row stays `pending` → re-drive).
    fail: Arc<std::sync::Mutex<bool>>,
    /// Recipients to treat as suppressed (return `MailerError::Suppressed`).
    suppressed: Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
}

/// What a [`RecordingNotifier`] captured.
#[derive(Debug, Default)]
pub struct RecordingState {
    /// Every send ATTEMPT (in order): `(to_email, kind, idempotency_key, delivered)`.
    /// `delivered=false` ⇒ the key was already delivered (provider dedup) — the attempt
    /// was made but the recipient saw no new email.
    pub attempts: Vec<(String, BillingNotificationKind, String, bool)>,
    /// Idempotency keys already delivered (the provider-side dedup set).
    delivered_keys: std::collections::HashSet<String>,
}

impl RecordingNotifier {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Make the NEXT-and-subsequent sends fail with a transport error until cleared.
    pub fn set_failing(&self, failing: bool) {
        *self.fail.lock().unwrap() = failing;
    }

    /// Mark a recipient as suppressed (the notifier returns `Suppressed` for it).
    pub fn suppress(&self, email: &str) {
        self.suppressed.lock().unwrap().insert(email.to_owned());
    }

    /// Number of attempts that actually DELIVERED (deduped re-sends don't count).
    #[must_use]
    pub fn delivered_count(&self) -> usize {
        self.inner.lock().unwrap().attempts.iter().filter(|a| a.3).count()
    }

    /// Number of delivered emails for a given idempotency key (≤ 1 if dedup works).
    #[must_use]
    pub fn delivered_for_key(&self, key: &str) -> usize {
        self.inner
            .lock()
            .unwrap()
            .attempts
            .iter()
            .filter(|a| a.3 && a.2 == key)
            .count()
    }

    /// Total send ATTEMPTS (including deduped re-sends) for a key.
    #[must_use]
    pub fn attempts_for_key(&self, key: &str) -> usize {
        self.inner.lock().unwrap().attempts.iter().filter(|a| a.2 == key).count()
    }

    /// Delivered count for a key PREFIX (e.g. `"{creator_id}:{kind}:"`) — lets a test
    /// scope assertions to its OWN seeded creators when the cron sweeps a shared DB.
    #[must_use]
    pub fn delivered_for_key_prefix(&self, prefix: &str) -> usize {
        self.inner
            .lock()
            .unwrap()
            .attempts
            .iter()
            .filter(|a| a.3 && a.2.starts_with(prefix))
            .count()
    }

    /// Delivered count for a given kind.
    #[must_use]
    pub fn delivered_for_kind(&self, kind: BillingNotificationKind) -> usize {
        self.inner
            .lock()
            .unwrap()
            .attempts
            .iter()
            .filter(|a| a.3 && a.1 == kind)
            .count()
    }

    /// Snapshot of all attempts.
    #[must_use]
    pub fn attempts(&self) -> Vec<(String, BillingNotificationKind, String, bool)> {
        self.inner.lock().unwrap().attempts.clone()
    }
}

#[async_trait(?Send)]
impl BillingNotifier for RecordingNotifier {
    async fn notify(&self, _db: &Client, n: &Notification) -> Result<(), MailerError> {
        if *self.fail.lock().unwrap() {
            return Err(MailerError::Transport("recording notifier: forced failure".into()));
        }
        if self.suppressed.lock().unwrap().contains(&n.to_email) {
            // Record the suppressed attempt (not delivered) so a test can assert it.
            self.inner.lock().unwrap().attempts.push((
                n.to_email.clone(),
                n.kind,
                n.idempotency_key.clone(),
                false,
            ));
            return Err(MailerError::Suppressed(n.to_email.clone()));
        }
        let mut st = self.inner.lock().unwrap();
        // Provider-side idempotency: a re-driven send of the same key is a no-op
        // DELIVERY (the attempt is recorded with delivered=false).
        let delivered = st.delivered_keys.insert(n.idempotency_key.clone());
        st.attempts.push((n.to_email.clone(), n.kind, n.idempotency_key.clone(), delivered));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_str_roundtrips() {
        for k in [
            BillingNotificationKind::PaymentFailed,
            BillingNotificationKind::PastDue,
            BillingNotificationKind::Suspended,
            BillingNotificationKind::Recovered,
            BillingNotificationKind::InvoiceFinalized,
            BillingNotificationKind::Refunded,
            BillingNotificationKind::Disputed,
            BillingNotificationKind::PayoutFailed,
            BillingNotificationKind::CheckoutFailed,
        ] {
            assert_eq!(BillingNotificationKind::from_str(k.as_str()), Some(k));
        }
        assert_eq!(BillingNotificationKind::from_str("spend_warn"), None);
    }

    #[test]
    fn every_kind_renders_nonempty_subject_and_body() {
        for k in [
            BillingNotificationKind::PaymentFailed,
            BillingNotificationKind::PastDue,
            BillingNotificationKind::Suspended,
            BillingNotificationKind::Recovered,
            BillingNotificationKind::InvoiceFinalized,
            BillingNotificationKind::Refunded,
            BillingNotificationKind::Disputed,
            BillingNotificationKind::PayoutFailed,
            BillingNotificationKind::CheckoutFailed,
        ] {
            let n = Notification {
                to_email: "c@example.test".to_owned(),
                to_name: Some("Casey".to_owned()),
                kind: k,
                detail: NotificationDetail {
                    period_label: Some("June 2026".to_owned()),
                    amount_label: Some("$12.34".to_owned()),
                    refund_destination_label: Some("cash to your card".to_owned()),
                },
                idempotency_key: "c:k:t".to_owned(),
            };
            let (subject, body) = render_template(&n);
            assert!(!subject.is_empty(), "{k:?} subject empty");
            assert!(!body.is_empty(), "{k:?} body empty");
        }
    }
}
