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

/// Per-deployment provider configuration (M6). Parsed from CLI/env in `main.rs`.
/// For the M-Native phase only the kind is load-bearing; the export-backend
/// creds will be carried here in a later phase without a config-shape change.
#[derive(Debug, Clone)]
pub struct MeteringProviderConfig {
    pub kind: MeteringProviderKind,
}

impl MeteringProviderConfig {
    /// Native is the default backend.
    #[must_use]
    pub fn native() -> Self {
        Self {
            kind: MeteringProviderKind::Native,
        }
    }
}

/// Build the configured metering provider once at boot (M6).
///
/// In the M-Native phase only `Native` is functional. `Stripe` / `OpenMeter`
/// return a guarded `Config` error so a deployment that asks for an
/// unimplemented backend fails to boot with a clear message rather than
/// silently doing nothing (a silent revenue black hole — blueprint §M9 risk 3).
///
/// # Errors
/// Returns [`ProviderError::Config`] for a not-yet-implemented backend.
pub fn build_provider(
    cfg: &MeteringProviderConfig,
) -> Result<std::sync::Arc<dyn MeteringProvider>, ProviderError> {
    match cfg.kind {
        MeteringProviderKind::Native => Ok(std::sync::Arc::new(native::NativeProvider::new())),
        MeteringProviderKind::Stripe => Err(ProviderError::Config(
            "metering-provider 'stripe' (Stripe Billing Meters) is not yet implemented — \
             use 'native' (the default)"
                .to_string(),
        )),
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
        let provider = build_provider(&MeteringProviderConfig { kind })
            .expect("native provider must build");
        assert_eq!(provider.kind(), MeteringProviderKind::Native);
    }

    #[test]
    fn build_stripe_provider_is_rejected_at_boot() {
        // M-Native: the export backends are guarded stubs — a deployment that
        // asks for one must FAIL to boot (clear error), never silently no-op.
        let kind = MeteringProviderKind::parse("stripe").expect("stripe parses to a kind");
        // `dyn MeteringProvider` is not Debug, so match instead of `expect_err`.
        let Err(err) = build_provider(&MeteringProviderConfig { kind }) else {
            panic!("stripe must be rejected until implemented");
        };
        assert!(matches!(err, ProviderError::Config(_)), "got {err:?}");
        assert!(err.to_string().contains("not yet implemented"), "msg: {err}");
    }

    #[test]
    fn build_openmeter_provider_is_rejected_at_boot() {
        let kind = MeteringProviderKind::parse("openmeter").expect("openmeter parses to a kind");
        let Err(err) = build_provider(&MeteringProviderConfig { kind }) else {
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
