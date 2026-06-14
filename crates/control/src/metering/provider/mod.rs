//! Pluggable metering providers (M-Native, blueprint §M1–§M2/§M6).
//!
//! A [`MeteringProvider`] is the WRITE/EXPORT + INVOICE contract — "what the
//! billing backend is". It sits ABOVE the low-level [`crate::stripe_client::StripeApi`]
//! ("how to talk to Stripe"): the [`native::NativeProvider`] calls into the
//! existing Stripe pipeline; the future export providers differ only in WHERE
//! compute units go.
//!
//! The local ledger (`usage_aggregates`) + spend enforcement (`spend.rs` /
//! `enforce.rs`) are NEVER wired through a provider — enforcement is always the
//! local fact. A provider is export/invoice only.
//!
//! ## M-Native phase
//!
//! Only [`MeteringProviderKind::Native`] is functional. It is a ZERO-behaviour
//! change relocation of the existing billing pipeline behind this trait: the
//! hardened C1/C2 + MAJOR crash-window reconciler logic is REUSED verbatim from
//! `cron::billing_reconcile` (it is relocated behind the trait, not rewritten).
//! The export kinds (`Stripe`, `OpenMeter`) are guarded stubs that fail to build
//! with a clear "not yet implemented" error until their phases land.
//!
//! ## Why the verbs take `&AppState`
//!
//! The Native pipeline is fundamentally `AppState`-driven (it reads
//! `state.registry` + `state.stripe_store` + the Stripe config). Threading
//! `&AppState` through the verbs (rather than having the provider own clones of
//! those deps) lets `NativeProvider::invoice` REUSE `billing_reconcile::bill_creator`
//! byte-for-byte — the zero-drift relocation the blueprint mandates — and avoids
//! an `AppState`→provider→`AppState` ownership cycle (the provider stays a ZST).

pub mod native;
pub mod stripe_meters;
pub mod types;

use uuid::Uuid;

pub use types::{
    BillingPeriod, CreatorBilling, CustomerRef, InvoiceRef, MeteringProviderKind, ProviderError,
};

use crate::AppState;

/// The pluggable metering/billing backend. Four verbs; a provider decides where
/// compute units *also* go (export) and how a period is closed (invoice). It
/// never owns the local ledger.
///
/// `#[async_trait(?Send)]` for object-safety as an `Arc<dyn MeteringProvider>`,
/// matching the codebase's `BlobStore` pattern (compio is per-thread; the
/// reconciler's futures are intentionally `!Send`).
#[async_trait::async_trait(?Send)]
pub trait MeteringProvider: Send + Sync {
    /// The backend kind (drives provider-aware cron spawning in `spawn_all`).
    fn kind(&self) -> MeteringProviderKind;

    /// Ensure the provider knows this creator (Native/Stripe: a `cus_…`;
    /// OpenMeter: a no-op, the subject is the app/creator id). Idempotent.
    async fn ensure_customer(
        &self,
        state: &AppState,
        creator: &CreatorBilling,
    ) -> Result<CustomerRef, ProviderError>;

    /// Forward this period's CU for one app. `compute_units` is the integer CU
    /// from `pricing::total_units`; `idempotency_key` is the deterministic
    /// per-(app,period) key. Native: NO-OP (usage is already local).
    async fn report_usage(
        &self,
        state: &AppState,
        customer: &CustomerRef,
        app_id: Uuid,
        period: BillingPeriod,
        compute_units: u64,
        idempotency_key: &str,
    ) -> Result<(), ProviderError>;

    /// Close + bill the period for one creator. Native: the WHOLE current
    /// reconciler body (price → invoice items → create+finalize, with C1/C2),
    /// reused verbatim. Stripe/OpenMeter: NO-OP.
    async fn invoice(
        &self,
        state: &AppState,
        creator: &CreatorBilling,
        period: BillingPeriod,
    ) -> Result<InvoiceRef, ProviderError>;

    /// Inbound webhook (signature-verify + mutate). Native/Stripe: the existing
    /// Stripe webhook path (handled at the HTTP handler today). OpenMeter: a
    /// no-op. M-Native keeps webhook handling in `stripe_handlers::webhook`; the
    /// provider verb is the seam an export backend will use.
    async fn handle_webhook(
        &self,
        state: &AppState,
        payload: &[u8],
        sig: &str,
    ) -> Result<(), ProviderError>;
}

/// Stripe Billing Meters configuration (M-Stripe / §M6). The operator
/// provisions a Stripe **Meter** (with `event_name`) + a metered **Price** +
/// **Subscription** ONCE; the export cron pushes CU as `meter_events` against
/// that meter and Stripe self-invoices. The provider assumes the Price /
/// Subscription already exist — it does NOT create them.
pub struct StripeMeterConfig {
    /// The Stripe Meter's configured `event_name` (e.g. `compute_units`).
    pub event_name: String,
    /// The platform Stripe secret key (same account the Native rail uses).
    pub secret_key: crate::SecretString,
    /// Stripe API base URL (overridable so tests point at a localhost mock).
    pub base_url: String,
}

// `SecretString` is intentionally NOT `Clone` (leak-resistant); re-wrap it by
// re-exposing through the sanctioned accessor so the config can be cloned at
// boot (built once, threaded into the provider) without a derive.
impl Clone for StripeMeterConfig {
    fn clone(&self) -> Self {
        Self {
            event_name: self.event_name.clone(),
            secret_key: crate::SecretString::new(self.secret_key.expose_secret().to_string()),
            base_url: self.base_url.clone(),
        }
    }
}

impl std::fmt::Debug for StripeMeterConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never leak the secret key in Debug output.
        f.debug_struct("StripeMeterConfig")
            .field("event_name", &self.event_name)
            .field("secret_key", &"<redacted>")
            .field("base_url", &self.base_url)
            .finish()
    }
}

/// Per-deployment provider configuration (M6). Parsed from CLI/env in `main.rs`.
/// The `kind` selects the backend; the export-backend creds are carried in the
/// matching `Option` field (present iff that kind is selected).
#[derive(Debug, Clone)]
pub struct MeteringProviderConfig {
    pub kind: MeteringProviderKind,
    /// Stripe Billing Meters creds — required (and present) iff `kind == Stripe`.
    pub stripe_meter: Option<StripeMeterConfig>,
}

impl MeteringProviderConfig {
    /// Native is the default backend (no export creds).
    #[must_use]
    pub fn native() -> Self {
        Self {
            kind: MeteringProviderKind::Native,
            stripe_meter: None,
        }
    }

    /// Stripe Billing Meters backend with its operator-provisioned meter creds.
    #[must_use]
    pub fn stripe(meter: StripeMeterConfig) -> Self {
        Self {
            kind: MeteringProviderKind::Stripe,
            stripe_meter: Some(meter),
        }
    }
}

/// Build the configured metering provider once at boot (M6).
///
/// `Native` and `Stripe` (Stripe Billing Meters) are functional. `OpenMeter`
/// is not yet implemented and returns a guarded `Config` error so a deployment
/// that asks for it fails to boot with a clear message rather than silently
/// doing nothing (a silent revenue black hole — blueprint §M9 risk 3). A
/// `Stripe` selection with no `stripe_meter` creds is likewise rejected.
///
/// # Errors
/// Returns [`ProviderError::Config`] for a not-yet-implemented backend or a
/// `Stripe` selection missing its meter creds.
pub fn build_provider(
    cfg: &MeteringProviderConfig,
) -> Result<std::sync::Arc<dyn MeteringProvider>, ProviderError> {
    match cfg.kind {
        MeteringProviderKind::Native => Ok(std::sync::Arc::new(native::NativeProvider::new())),
        MeteringProviderKind::Stripe => {
            let meter = cfg.stripe_meter.clone().ok_or_else(|| {
                ProviderError::Config(
                    "metering-provider 'stripe' requires --stripe-meter-event-name (the \
                     operator-provisioned Stripe Meter's event name) — refusing to boot a \
                     Stripe-Meters deployment with no meter (a silent revenue black hole)"
                        .to_string(),
                )
            })?;
            Ok(std::sync::Arc::new(stripe_meters::StripeProvider::new(meter)))
        }
        MeteringProviderKind::OpenMeter => Err(ProviderError::Config(
            "metering-provider 'openmeter' is not yet implemented — use 'native' (the default)"
                .to_string(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_default_is_native() {
        // The boot default (`--metering-provider` defaults to `native`).
        assert_eq!(MeteringProviderConfig::native().kind, MeteringProviderKind::Native);
    }

    #[test]
    fn build_native_provider_succeeds_and_reports_native_kind() {
        // AppState gets an Arc<dyn MeteringProvider> built from the native
        // config; it must construct and report the Native kind (the value
        // `spawn_all` keys the reconcile-cron spawn on).
        let provider = build_provider(&MeteringProviderConfig::native())
            .expect("native provider must build");
        assert_eq!(provider.kind(), MeteringProviderKind::Native);
    }

    #[test]
    fn build_native_via_parsed_native_flag_succeeds() {
        // `--metering-provider native` parses to the Native kind and builds.
        let kind = MeteringProviderKind::parse("native").expect("native parses");
        let provider = build_provider(&MeteringProviderConfig { kind, stripe_meter: None })
            .expect("native provider must build");
        assert_eq!(provider.kind(), MeteringProviderKind::Native);
    }

    #[test]
    fn build_stripe_provider_with_creds_succeeds_and_reports_stripe_kind() {
        // M-Stripe: a stripe deployment WITH its operator-provisioned meter creds
        // builds and reports the Stripe kind (the value `spawn_all` keys the
        // export-cron spawn on).
        let cfg = MeteringProviderConfig::stripe(StripeMeterConfig {
            event_name: "compute_units".to_string(),
            secret_key: crate::SecretString::new("sk_test_x".to_string()),
            base_url: "http://localhost:0".to_string(),
        });
        let provider = build_provider(&cfg).expect("stripe provider must build with creds");
        assert_eq!(provider.kind(), MeteringProviderKind::Stripe);
    }

    #[test]
    fn build_stripe_provider_without_meter_creds_is_rejected_at_boot() {
        // A Stripe-Meters deployment with no meter is a silent revenue black hole
        // (blueprint §M9 risk 3) — it MUST fail to boot with a clear error.
        let kind = MeteringProviderKind::parse("stripe").expect("stripe parses to a kind");
        let Err(err) = build_provider(&MeteringProviderConfig { kind, stripe_meter: None }) else {
            panic!("stripe must be rejected without meter creds");
        };
        assert!(matches!(err, ProviderError::Config(_)), "got {err:?}");
        assert!(err.to_string().contains("stripe-meter-event-name"), "msg: {err}");
    }

    #[test]
    fn build_openmeter_provider_is_rejected_at_boot() {
        let kind = MeteringProviderKind::parse("openmeter").expect("openmeter parses to a kind");
        let Err(err) = build_provider(&MeteringProviderConfig { kind, stripe_meter: None }) else {
            panic!("openmeter must be rejected until implemented");
        };
        assert!(matches!(err, ProviderError::Config(_)), "got {err:?}");
    }

    #[test]
    fn unknown_provider_value_is_rejected() {
        // An unknown `--metering-provider` value fails fast (main.rs exits 1).
        assert_eq!(MeteringProviderKind::parse("bogus").unwrap_err(), "bogus");
        assert_eq!(MeteringProviderKind::parse("").unwrap_err(), "");
    }
}
