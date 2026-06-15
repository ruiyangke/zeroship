//! Shared value types for the pluggable [`MeteringProvider`](super::MeteringProvider)
//! layer (M-Native, blueprint §M1).
//!
//! These are the neutral shapes that cross the metering↔billing seam. The
//! local ledger (`usage_aggregates`) stays raw-metric-keyed and is NEVER
//! touched by a provider — the only quantity that crosses to a provider is
//! **compute units (CU)** (Native invoices in cents via the existing reconciler;
//! the export providers push CU). See the blueprint §M0/§M1.

use uuid::Uuid;

use crate::registry::RegistryError;
use crate::stripe_store::StripeError;

/// A provider-side customer handle. Native/Stripe: a Stripe `cus_…`.
/// OpenMeter: the subject id (the app/creator id) — OpenMeter has no customer
/// object. Newtype so it can't be confused with an arbitrary string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CustomerRef(pub String);

impl CustomerRef {
    /// Borrow the underlying id (the `cus_…` on the Native/Stripe rail).
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The result of a billing close. `Some("in_…")` for the Native rail (a
/// finalized Stripe invoice id); `None` when the provider self-invoices
/// (Stripe-Meters) or never invoices (OpenMeter export-only).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct InvoiceRef(pub Option<String>);

/// A billing period `[start, end)` in unix seconds. Wire-identical to
/// [`crate::stripe_client::Period`]; kept distinct so the provider surface does
/// not leak the low-level Stripe type into its signature.
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

/// What [`ensure_customer`](super::MeteringProvider::ensure_customer) needs and
/// what the per-creator [`invoice`](super::MeteringProvider::invoice) verb keys
/// on: the creator's identity plus any already-saved customer handle.
#[derive(Debug, Clone)]
pub struct CreatorBilling {
    pub creator_id: Uuid,
    pub email: String,
    /// An already-saved `cus_…` if one exists (so `ensure_customer` is a no-op).
    pub customer: Option<CustomerRef>,
}

/// Errors a [`MeteringProvider`](super::MeteringProvider) verb can return. Wraps
/// the low-level Stripe error plus a transport variant and a config variant, and
/// maps into [`RegistryError`] at the cron boundary exactly as [`StripeError`]
/// does today (so the reconciler's existing error posture is unchanged).
#[derive(Debug)]
pub enum ProviderError {
    /// An underlying Stripe REST error (the existing low-level surface).
    Stripe(StripeError),
    /// A registry / database error surfaced from the local ledger or the
    /// billing-run bookkeeping.
    Registry(RegistryError),
    /// A transport-level failure talking to an external metering backend
    /// (OpenMeter / Stripe-Meters). Held as a String to stay backend-neutral.
    Transport(String),
    /// The provider is misconfigured (e.g. a required meter id / token is
    /// absent). Surfaced at boot or first use.
    Config(String),
}

impl std::fmt::Display for ProviderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Stripe(e) => write!(f, "stripe: {e}"),
            Self::Registry(e) => write!(f, "{e}"),
            Self::Transport(m) => write!(f, "transport: {m}"),
            Self::Config(m) => write!(f, "config: {m}"),
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

/// At the cron boundary a [`ProviderError`] maps back into [`RegistryError`]
/// (the type the reconcile sweep already returns), preserving the EXACT
/// fail-closed behaviour: a propagated `FxUnresolved` still aborts the whole
/// sweep (see `billing_reconcile::sweep`).
impl From<ProviderError> for RegistryError {
    fn from(e: ProviderError) -> Self {
        match e {
            ProviderError::Registry(r) => r,
            ProviderError::Stripe(s) => Self::Database(format!("stripe: {s}")),
            ProviderError::Transport(m) => Self::Database(format!("transport: {m}")),
            ProviderError::Config(m) => Self::Database(format!("provider config: {m}")),
        }
    }
}

/// The metering-provider backend selected per deployment (M6). `Native` is the
/// default and the ONLY functional backend in the M-Native phase; the export
/// backends are guarded stubs that fail to boot (a clear "not yet implemented")
/// until their phases land.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MeteringProviderKind {
    /// Control-side aggregation → CU×FX → the existing Stripe reconciler.
    Native,
    /// Stripe Billing Meters (CU → `meter_events`; Stripe self-invoices).
    Stripe,
    /// OpenMeter (CU → CloudEvents; export-only).
    OpenMeter,
}

impl MeteringProviderKind {
    /// Parse the `--metering-provider` value. Unknown values are rejected so a
    /// typo fails fast at boot rather than silently defaulting.
    ///
    /// # Errors
    /// Returns the offending string when it is not one of the known kinds.
    pub fn parse(s: &str) -> Result<Self, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "native" => Ok(Self::Native),
            "stripe" => Ok(Self::Stripe),
            "openmeter" => Ok(Self::OpenMeter),
            other => Err(other.to_string()),
        }
    }

    /// The canonical lowercase name (for logs).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Native => "native",
            Self::Stripe => "stripe",
            Self::OpenMeter => "openmeter",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_parses_known_values_case_insensitively() {
        assert_eq!(MeteringProviderKind::parse("native").unwrap(), MeteringProviderKind::Native);
        assert_eq!(MeteringProviderKind::parse(" Native ").unwrap(), MeteringProviderKind::Native);
        assert_eq!(MeteringProviderKind::parse("STRIPE").unwrap(), MeteringProviderKind::Stripe);
        assert_eq!(
            MeteringProviderKind::parse("openmeter").unwrap(),
            MeteringProviderKind::OpenMeter
        );
    }

    #[test]
    fn kind_rejects_unknown_value() {
        let err = MeteringProviderKind::parse("bogus").unwrap_err();
        assert_eq!(err, "bogus");
    }

    #[test]
    fn billing_period_converts_to_stripe_period() {
        let p = BillingPeriod { start: 100, end: 200 };
        let sp: crate::stripe_client::Period = p.into();
        assert_eq!(sp.start, 100);
        assert_eq!(sp.end, 200);
    }

    #[test]
    fn provider_error_maps_fx_unresolved_through_to_registry() {
        // The fail-closed invariant: a FxUnresolved that bubbles up as a
        // ProviderError must map back to RegistryError::FxUnresolved so the
        // sweep still aborts (never bills a base-only $0 invoice).
        let pe = ProviderError::Registry(RegistryError::FxUnresolved);
        let re: RegistryError = pe.into();
        assert!(matches!(re, RegistryError::FxUnresolved));
    }
}
