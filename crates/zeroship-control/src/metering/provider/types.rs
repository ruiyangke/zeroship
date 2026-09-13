//! Provider-neutral metering/billing value types.

use std::time::Duration;

pub use zeroship_core::usage_event::{UsageEvent, UsageSubject};
use zeroship_core::AppId;

use crate::registry::RegistryError;
use crate::stripe_store::StripeError;

/// Provider-side subject/customer handle.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SubjectRef(pub String);

impl SubjectRef {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The result of a billing close.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct InvoiceRef(pub Option<String>);

/// A billing period `[start, end)` in unix seconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BillingPeriod {
    pub start: i64,
    pub end: i64,
}

impl From<BillingPeriod> for crate::stripe_client::Period {
    fn from(p: BillingPeriod) -> Self {
        Self {
            start: p.start,
            end: p.end,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct IngestAck {
    pub accepted: usize,
    /// Number of provider-deduped events when the provider reports it.
    ///
    /// `None` means the adapter only knows the accepted/requested count. Remote
    /// provider APIs such as Stripe meter events, OpenMeter event ingest, and Lago
    /// event ingest do not return an accepted-vs-deduped split for the calls used
    /// here, so reporting `Some(0)` would be a false metric.
    pub deduped: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AggregateQuery {
    pub subject: SubjectRef,
    pub meter: String,
    pub period: BillingPeriod,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdjustmentNote {
    pub period: BillingPeriod,
    pub app_id: Option<AppId>,
    pub meter: String,
    pub quantity_delta: i64,
    pub correction_seq: u32,
    pub amount_cents: i64,
    pub reason: String,
    pub idempotency_key: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DedupContract {
    pub key: DedupKey,
    pub ttl: DedupTtl,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DedupKey {
    SourceAndId,
    Identifier,
    TransactionId,
    NotApplicable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DedupTtl {
    Bounded(Duration),
    Unbounded,
    NotApplicable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClosedPeriodPolicy {
    OpenPeriodOnly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CorrectionCapability {
    Backfill {
        window: Duration,
        closed: ClosedPeriodPolicy,
    },
    InvoiceCredit,
    None,
}

/// Errors a provider verb can return.
#[derive(Debug)]
pub enum ProviderError {
    Stripe(StripeError),
    Registry(RegistryError),
    Transport(String),
    Store(String),
    Config(String),
    PermanentReject { status: u16, message: String },
}

impl ProviderError {
    #[must_use]
    pub fn permanent_reject(status: u16, message: impl Into<String>) -> Self {
        Self::PermanentReject {
            status,
            message: message.into(),
        }
    }

    #[must_use]
    pub fn is_permanent_reject(&self) -> bool {
        matches!(
            self,
            Self::PermanentReject {
                status: 400..=499,
                ..
            }
        )
    }

    #[must_use]
    pub fn reject_reason(&self) -> String {
        match self {
            Self::PermanentReject { status, message } => {
                format!("provider permanent reject {status}: {message}")
            }
            other => other.to_string(),
        }
    }
}

impl std::fmt::Display for ProviderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Stripe(e) => write!(f, "stripe: {e}"),
            Self::Registry(e) => write!(f, "{e}"),
            Self::Transport(m) => write!(f, "transport: {m}"),
            Self::Store(m) => write!(f, "store: {m}"),
            Self::Config(m) => write!(f, "config: {m}"),
            Self::PermanentReject { status, message } => {
                write!(f, "provider permanent reject {status}: {message}")
            }
        }
    }
}

impl std::error::Error for ProviderError {}

impl From<StripeError> for ProviderError {
    fn from(e: StripeError) -> Self {
        Self::Stripe(e)
    }
}

impl From<RegistryError> for ProviderError {
    fn from(e: RegistryError) -> Self {
        Self::Registry(e)
    }
}

impl From<compio_postgres::Error> for ProviderError {
    fn from(e: compio_postgres::Error) -> Self {
        Self::Store(e.to_string())
    }
}

impl From<ProviderError> for RegistryError {
    fn from(e: ProviderError) -> Self {
        match e {
            ProviderError::Registry(r) => r,
            ProviderError::Stripe(s) => Self::Database(format!("stripe: {s}")),
            ProviderError::Transport(m) => Self::Database(format!("transport: {m}")),
            ProviderError::Store(m) => Self::Database(format!("provider store: {m}")),
            ProviderError::Config(m) => Self::Database(format!("provider config: {m}")),
            ProviderError::PermanentReject { status, message } => {
                Self::Database(format!("provider permanent reject {status}: {message}"))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn billing_period_converts_to_stripe_period() {
        let p = BillingPeriod {
            start: 100,
            end: 200,
        };
        let sp: crate::stripe_client::Period = p.into();
        assert_eq!(sp.start, 100);
        assert_eq!(sp.end, 200);
    }

    #[test]
    fn provider_error_maps_fx_unresolved_through_to_registry() {
        let pe = ProviderError::Registry(RegistryError::FxUnresolved);
        let re: RegistryError = pe.into();
        assert!(matches!(re, RegistryError::FxUnresolved));
    }
}
