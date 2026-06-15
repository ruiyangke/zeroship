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
//! ## Providers
//!
//! [`MeteringProviderKind::Native`] is the default: a ZERO-behaviour-change
//! relocation of the existing billing pipeline behind this trait (the hardened
//! C1/C2 + MAJOR crash-window reconciler logic, REUSED verbatim from
//! `cron::billing_reconcile`). The export kinds are
//! [`MeteringProviderKind::Stripe`] (Stripe Billing Meters — CU → `meter_events`,
//! Stripe self-invoices) and [`MeteringProviderKind::OpenMeter`] (CU →
//! CloudEvents, export-only — OpenMeter aggregates, invoicing stays Native). An
//! export backend selected with no creds refuses to build (a silent revenue
//! black hole otherwise).
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
pub mod openmeter;
pub mod stripe_meters;
pub mod types;

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

    /// Forward this period's CU for one CUSTOMER (the per-creator export grain).
    /// `compute_units` is the integer CU from `pricing::total_units` summed across
    /// the creator's apps; `idempotency_key` is the deterministic
    /// per-(creator,period) key. The meter aggregates per customer, so the push is
    /// customer-scoped — there is no single app to attribute (an earlier per-app
    /// signature mismatched the per-customer aggregate and silently under-billed
    /// every app after the first). `now` is the sweep's wall-clock unix seconds —
    /// the CONSUMPTION instant the event is stamped at (Stripe rejects a timestamp
    /// more than 5min in the future or older than 35d, and aggregates the event
    /// into whatever period its timestamp falls in, so "now during the current
    /// period" is correct — NEVER `period.end`, which is a future timestamp).
    /// Native: NO-OP (usage is already local).
    async fn report_usage(
        &self,
        state: &AppState,
        customer: &CustomerRef,
        period: BillingPeriod,
        compute_units: u64,
        idempotency_key: &str,
        now: i64,
    ) -> Result<(), ProviderError>;

    /// Read the provider's EXTERNALLY-aggregated CU total for one
    /// `(customer, period)` window — the SUM the external meter has actually
    /// accepted. The export cron pushes `current_local − reported_total`, so a
    /// crash-then-re-drive past the external dedup window NEVER double-counts
    /// (the guarantee does not depend on the local high-water being fresh, nor on
    /// any time-bounded idempotency window). Native: returns 0 (the export cron
    /// is never spawned for Native; nothing is forwarded). Stripe: reads the
    /// meter's `event_summaries` aggregate.
    async fn reported_total(
        &self,
        state: &AppState,
        customer: &CustomerRef,
        period: BillingPeriod,
    ) -> Result<u64, ProviderError>;

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
    /// The Stripe **Meter id** (`mtr_…`) the operator provisioned. REQUIRED for
    /// the C2 re-drive reconcile: the export cron reads the meter's *aggregated*
    /// value for `(customer, period)` via `GET /v1/billing/meters/{id}/
    /// event_summaries` and pushes `current − aggregate`, so a re-drive past
    /// Stripe's ~24h `identifier` dedup window can NEVER double-count. The
    /// `event_name` aggregates events; the `meter_id` reads the aggregate back.
    pub meter_id: String,
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
            meter_id: self.meter_id.clone(),
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
            .field("meter_id", &self.meter_id)
            .field("secret_key", &"<redacted>")
            .field("base_url", &self.base_url)
            .finish()
    }
}

/// OpenMeter configuration (M-OpenMeter / §M4/§M6). The operator provisions an
/// OpenMeter **Meter** (with an `eventType` + a `slug`) ONCE; the export cron
/// pushes CU as **CloudEvents** under that `eventType` and reads the per-subject
/// aggregate back via the meter `slug`. OpenMeter aggregates — it does NOT
/// invoice (billing stays on the Native/Stripe rail).
pub struct OpenMeterConfig {
    /// The OpenMeter API base URL (`https://openmeter.cloud` or a self-hosted
    /// deployment; overridable so tests point at a localhost mock).
    pub base_url: String,
    /// The OpenMeter API token (Bearer). REQUIRED — never logged.
    pub token: crate::SecretString,
    /// The CloudEvent `type` = the operator-provisioned meter's `eventType`
    /// (e.g. `compute_units`). The `report_usage` push carries this as `type`.
    pub event_type: String,
    /// The OpenMeter meter **slug** the aggregate is queried under (the C2
    /// reconcile reads `GET /api/v1/meters/{slug}/query`). REQUIRED for the
    /// re-drive reconcile to work.
    pub meter_slug: String,
}

// `SecretString` is intentionally NOT `Clone`; re-wrap via the accessor so the
// config can be cloned at boot (same pattern as `StripeMeterConfig`).
impl Clone for OpenMeterConfig {
    fn clone(&self) -> Self {
        Self {
            base_url: self.base_url.clone(),
            token: crate::SecretString::new(self.token.expose_secret().to_string()),
            event_type: self.event_type.clone(),
            meter_slug: self.meter_slug.clone(),
        }
    }
}

impl std::fmt::Debug for OpenMeterConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never leak the API token in Debug output.
        f.debug_struct("OpenMeterConfig")
            .field("base_url", &self.base_url)
            .field("token", &"<redacted>")
            .field("event_type", &self.event_type)
            .field("meter_slug", &self.meter_slug)
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
    /// OpenMeter creds — required (and present) iff `kind == OpenMeter`.
    pub openmeter: Option<OpenMeterConfig>,
}

impl MeteringProviderConfig {
    /// Native is the default backend (no export creds).
    #[must_use]
    pub fn native() -> Self {
        Self {
            kind: MeteringProviderKind::Native,
            stripe_meter: None,
            openmeter: None,
        }
    }

    /// Stripe Billing Meters backend with its operator-provisioned meter creds.
    #[must_use]
    pub fn stripe(meter: StripeMeterConfig) -> Self {
        Self {
            kind: MeteringProviderKind::Stripe,
            stripe_meter: Some(meter),
            openmeter: None,
        }
    }

    /// OpenMeter backend with its operator-provisioned config (base URL + token +
    /// event type + meter slug).
    #[must_use]
    pub fn openmeter(config: OpenMeterConfig) -> Self {
        Self {
            kind: MeteringProviderKind::OpenMeter,
            stripe_meter: None,
            openmeter: Some(config),
        }
    }
}

/// Build the configured metering provider once at boot (M6).
///
/// `Native`, `Stripe` (Stripe Billing Meters), and `OpenMeter` are all
/// functional. An export backend selected with no creds is rejected so a
/// deployment that asks for it fails to boot with a clear message rather than
/// silently doing nothing (a silent revenue black hole — blueprint §M9 risk 3):
/// a `Stripe` selection with no `stripe_meter`, or an `OpenMeter` selection with
/// no `openmeter` config / empty url+token / empty meter slug, is rejected.
///
/// # Errors
/// Returns [`ProviderError::Config`] for an export backend selected without its
/// required creds.
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
            if meter.meter_id.trim().is_empty() {
                return Err(ProviderError::Config(
                    "metering-provider 'stripe' requires --stripe-meter-id (the \
                     operator-provisioned Stripe Meter's `mtr_…` id) — it is needed to read the \
                     meter's aggregated value back for the >24h re-drive reconcile (C2); \
                     refusing to boot without it (a silent over-bill window)"
                        .to_string(),
                ));
            }
            Ok(std::sync::Arc::new(stripe_meters::StripeProvider::new(meter)))
        }
        MeteringProviderKind::OpenMeter => {
            let cfg = cfg.openmeter.clone().ok_or_else(|| {
                ProviderError::Config(
                    "metering-provider 'openmeter' requires --openmeter-url + --openmeter-token \
                     (the operator-provisioned OpenMeter base URL + API token) — refusing to boot \
                     an OpenMeter deployment with no endpoint (a silent revenue black hole)"
                        .to_string(),
                )
            })?;
            if cfg.base_url.trim().is_empty() || cfg.token.expose_secret().trim().is_empty() {
                return Err(ProviderError::Config(
                    "metering-provider 'openmeter' requires a non-empty --openmeter-url and \
                     --openmeter-token; refusing to boot without them (a silent revenue black hole)"
                        .to_string(),
                ));
            }
            if cfg.meter_slug.trim().is_empty() {
                return Err(ProviderError::Config(
                    "metering-provider 'openmeter' requires --openmeter-meter-slug (the \
                     operator-provisioned meter's slug) — it is needed to read the aggregate back \
                     for the >24h re-drive reconcile (C2); refusing to boot without it"
                        .to_string(),
                ));
            }
            Ok(std::sync::Arc::new(openmeter::OpenMeterProvider::new(cfg)))
        }
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
        let provider = build_provider(&MeteringProviderConfig { kind, stripe_meter: None, openmeter: None })
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
            meter_id: "mtr_test_x".to_string(),
            secret_key: crate::SecretString::new("sk_test_x".to_string()),
            base_url: "http://localhost:0".to_string(),
        });
        let provider = build_provider(&cfg).expect("stripe provider must build with creds");
        assert_eq!(provider.kind(), MeteringProviderKind::Stripe);
    }

    #[test]
    fn build_stripe_provider_without_meter_id_is_rejected_at_boot() {
        // The C2 re-drive reconcile reads the meter's aggregate via the meter id;
        // a stripe deployment with no meter id MUST fail to boot (else it would
        // silently fall back to trusting Stripe's 24h identifier window — the
        // over-bill window the fix closes).
        let cfg = MeteringProviderConfig::stripe(StripeMeterConfig {
            event_name: "compute_units".to_string(),
            meter_id: "  ".to_string(),
            secret_key: crate::SecretString::new("sk_test_x".to_string()),
            base_url: "http://localhost:0".to_string(),
        });
        let Err(err) = build_provider(&cfg) else {
            panic!("stripe must be rejected without a meter id");
        };
        assert!(matches!(err, ProviderError::Config(_)), "got {err:?}");
        assert!(err.to_string().contains("stripe-meter-id"), "msg: {err}");
    }

    #[test]
    fn build_stripe_provider_without_meter_creds_is_rejected_at_boot() {
        // A Stripe-Meters deployment with no meter is a silent revenue black hole
        // (blueprint §M9 risk 3) — it MUST fail to boot with a clear error.
        let kind = MeteringProviderKind::parse("stripe").expect("stripe parses to a kind");
        let Err(err) = build_provider(&MeteringProviderConfig { kind, stripe_meter: None, openmeter: None }) else {
            panic!("stripe must be rejected without meter creds");
        };
        assert!(matches!(err, ProviderError::Config(_)), "got {err:?}");
        assert!(err.to_string().contains("stripe-meter-event-name"), "msg: {err}");
    }

    #[test]
    fn build_openmeter_provider_with_creds_succeeds_and_reports_openmeter_kind() {
        // M-OpenMeter: an openmeter deployment WITH its base URL + token + slug
        // builds and reports the OpenMeter kind (the value `spawn_all` keys the
        // export-cron spawn on).
        let cfg = MeteringProviderConfig::openmeter(OpenMeterConfig {
            base_url: "http://localhost:0".to_string(),
            token: crate::SecretString::new("om_test_x".to_string()),
            event_type: "compute_units".to_string(),
            meter_slug: "compute_units".to_string(),
        });
        let provider = build_provider(&cfg).expect("openmeter provider must build with creds");
        assert_eq!(provider.kind(), MeteringProviderKind::OpenMeter);
    }

    #[test]
    fn build_openmeter_provider_without_creds_is_rejected_at_boot() {
        // An OpenMeter deployment with no endpoint is a silent revenue black hole
        // (blueprint §M9 risk 3) — it MUST fail to boot with a clear error.
        let kind = MeteringProviderKind::parse("openmeter").expect("openmeter parses to a kind");
        let Err(err) = build_provider(&MeteringProviderConfig { kind, stripe_meter: None, openmeter: None }) else {
            panic!("openmeter must be rejected without creds");
        };
        assert!(matches!(err, ProviderError::Config(_)), "got {err:?}");
        assert!(err.to_string().contains("openmeter-url"), "msg: {err}");
    }

    #[test]
    fn build_openmeter_provider_without_meter_slug_is_rejected_at_boot() {
        // The C2 re-drive reconcile reads the meter's aggregate via the slug; an
        // openmeter deployment with url+token but no slug MUST fail to boot.
        let cfg = MeteringProviderConfig::openmeter(OpenMeterConfig {
            base_url: "http://localhost:0".to_string(),
            token: crate::SecretString::new("om_test_x".to_string()),
            event_type: "compute_units".to_string(),
            meter_slug: "  ".to_string(),
        });
        let Err(err) = build_provider(&cfg) else {
            panic!("openmeter must be rejected without a meter slug");
        };
        assert!(matches!(err, ProviderError::Config(_)), "got {err:?}");
        assert!(err.to_string().contains("openmeter-meter-slug"), "msg: {err}");
    }

    #[test]
    fn build_openmeter_provider_with_empty_token_is_rejected_at_boot() {
        // url present but token blank → reject (no silent $0 export).
        let cfg = MeteringProviderConfig::openmeter(OpenMeterConfig {
            base_url: "http://localhost:0".to_string(),
            token: crate::SecretString::new("  ".to_string()),
            event_type: "compute_units".to_string(),
            meter_slug: "compute_units".to_string(),
        });
        let Err(err) = build_provider(&cfg) else {
            panic!("openmeter must be rejected with a blank token");
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
