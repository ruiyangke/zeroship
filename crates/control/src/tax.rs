//! Tax seam — compute tax at finalize, frozen onto the invoice (billing-ops gap #26,
//! PR-5; design §"PR-5 tax (seam only)" + flow E "Tax at finalize").
//!
//! **Code-only, no changeset.** `invoices.tax_cents` already exists (the redesign froze
//! the balance CHECK `total = subtotal − credit + tax`); this PR makes that column *live*
//! by computing it through a pluggable seam at finalize instead of hard-wiring `0`.
//!
//! ## The [`TaxProvider`] seam
//!
//! Mirroring [`crate::metering::provider::MeteringProvider`] and
//! [`crate::refund::RefundProvider`]: an `#[async_trait(?Send)]` object-safe trait, a
//! `Native` default that computes **0**, and an export/integration impl behind it. The
//! provider is selected once at boot ([`build_tax_provider`], `--tax-provider native`,
//! defaulting to native) and lives on [`crate::AppState`] exactly as `metering_provider`
//! does, so the reconciler reaches it through `&AppState`.
//!
//! ### Why Native computes 0 (the USD-launch default)
//!
//! A USD launch owes no tax. But hard-wiring `tax_cents = 0` into the reconciler forever
//! means enabling tax later is reconciler + (possibly) schema surgery. The seam costs one
//! trait + a no-op impl now; [`NativeTaxProvider::compute_tax`] returns `0`, and that `0`
//! is frozen into `tax_cents` in the SAME one-statement finalize UPDATE the reconciler
//! already runs (per segment, in the usage-segment design — tax is computed once per
//! invoice over the post-credit subtotal). `tax_cents` already exists, so enabling tax is
//! schema-free.
//!
//! ### Where `StripeTaxProvider` would slot in
//!
//! When the platform crosses a tax nexus, a `StripeTaxProvider` (a new `TaxProviderKind`
//! + a `build_tax_provider` arm) would implement [`TaxProvider::compute_tax`] by calling
//! Stripe's `automatic_tax` (`POST /v1/invoices` with `automatic_tax[enabled]=true`, or
//! the Tax `calculations` API) using the `TaxContext`'s creator/period/address to resolve
//! the jurisdiction, and return the computed `tax_cents`. **No schema change** — the
//! result lands in the existing `tax_cents` column through the same finalize UPDATE; only
//! this seam's `build_tax_provider` and the new provider impl change. The refund path's
//! proportional tax-on-refund split (`refunds.subtotal_cents`/`tax_cents`) already exists
//! (PR-3), so a taxed invoice refunds tax correctly the day a real provider is enabled.

use crate::metering::provider::ProviderError;

/// The frozen tax DATE the reconciler bills under (the closed period's
/// first-of-month). Threaded into [`TaxContext`] so a real provider can resolve the
/// jurisdiction-at-supply for the period being billed.
pub type BillingPeriodDate = chrono::NaiveDate;

/// The inputs a [`TaxProvider`] computes tax over — the post-credit subtotal plus the
/// creator/period context a real provider needs to resolve a jurisdiction.
///
/// `taxable_base_cents` is the **post-credit subtotal** (credit is applied BEFORE tax,
/// matching the balance CHECK's `total = subtotal − credit + tax` ordering — see flow E /
/// design principle 6): tax is computed on the amount the creator actually owes after
/// credit, not the gross subtotal. For the usage-segment design the base is the sum of the
/// segment lines' subtotal minus applied credit — computed once per invoice.
#[derive(Debug, Clone)]
pub struct TaxContext<'a> {
    /// The creator the invoice bills (the customer whose tax jurisdiction applies).
    pub creator_id: uuid::Uuid,
    /// The post-credit subtotal tax is computed over (`subtotal − applied_credit`,
    /// floored at 0). For Native this is unused (tax is always 0); a real provider
    /// taxes this base.
    pub taxable_base_cents: i64,
    /// The ISO currency the invoice is billed in (`usd` at launch).
    pub currency: &'a str,
    /// The closed period being billed (first-of-month). A real provider resolves the
    /// jurisdiction-at-supply for this period.
    pub period: BillingPeriodDate,
}

/// The tax a [`TaxProvider`] computed for one invoice — the cents frozen into
/// `invoices.tax_cents`. A struct (not a bare `i64`) so the seam can carry breakdown
/// (rate, jurisdiction, line splits) the day a real provider is enabled, without
/// changing the trait signature.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TaxAmount {
    /// The total tax in cents, frozen onto the invoice (`tax_cents`). `0` for Native.
    pub tax_cents: i64,
}

impl TaxAmount {
    /// The zero-tax amount (the USD-launch default Native returns).
    #[must_use]
    pub const fn zero() -> Self {
        Self { tax_cents: 0 }
    }
}

/// The pluggable tax backend. ONE verb (`compute_tax`); the provider decides how a
/// period's tax is computed. It never mutates an invoice — the reconciler freezes the
/// returned `tax_cents` into the one-statement finalize UPDATE.
///
/// `#[async_trait(?Send)]` for object-safety as an `Arc<dyn TaxProvider>`, matching the
/// [`crate::metering::provider::MeteringProvider`] / [`crate::refund::RefundProvider`]
/// pattern (compio is per-thread; the reconciler's futures are intentionally `!Send`).
#[async_trait::async_trait(?Send)]
pub trait TaxProvider: Send + Sync {
    /// The backend kind (drives observability + boot logging).
    fn kind(&self) -> TaxProviderKind;

    /// Compute the tax for one invoice at finalize, over the post-credit base in
    /// `ctx.taxable_base_cents`. Native returns `TaxAmount::zero()`; a real provider
    /// (e.g. `StripeTaxProvider`) would call `automatic_tax` and return the computed
    /// `tax_cents`. Called INSIDE the reconciler's finalize txn, just before the
    /// one-statement UPDATE; the result is frozen into `tax_cents` so the balance CHECK
    /// `total = subtotal − credit + tax` holds in one statement.
    ///
    /// # Errors
    /// Returns [`ProviderError`] if a real provider's tax call fails (Native never
    /// errors). A failure here aborts the per-creator finalize (fail-closed: never
    /// finalize an invoice with an unknown tax).
    async fn compute_tax(&self, ctx: &TaxContext<'_>) -> Result<TaxAmount, ProviderError>;
}

/// Tax backend kind. `native` (default) computes `0` (USD launch). A future
/// `StripeTaxProvider` would add a `Stripe` variant here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaxProviderKind {
    /// The USD-launch default: tax is always `0`.
    Native,
}

impl TaxProviderKind {
    /// The wire/flag string form (`--tax-provider <kind>`).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Native => "native",
        }
    }

    /// Parse the `--tax-provider` flag value (case-insensitive). Returns the raw value
    /// in `Err` for an unknown kind so the caller can fail to boot with a clear message
    /// (mirroring [`crate::metering::provider::MeteringProviderKind::parse`]).
    ///
    /// # Errors
    /// Returns the unrecognized string for any value outside `{native}`.
    pub fn parse(s: &str) -> Result<Self, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "native" => Ok(Self::Native),
            other => Err(other.to_string()),
        }
    }
}

/// Per-deployment tax-provider configuration (parsed from CLI/env in `main.rs`). The
/// `kind` selects the backend; future export-backend creds would carry in matching
/// `Option` fields (none needed for Native). Mirrors
/// [`crate::metering::provider::MeteringProviderConfig`].
#[derive(Debug, Clone)]
pub struct TaxProviderConfig {
    pub kind: TaxProviderKind,
}

impl TaxProviderConfig {
    /// Native is the default backend (no creds; tax = 0).
    #[must_use]
    pub fn native() -> Self {
        Self { kind: TaxProviderKind::Native }
    }
}

impl Default for TaxProviderConfig {
    fn default() -> Self {
        Self::native()
    }
}

/// The Native tax provider: returns `0` for EVERY invoice (the USD-launch default).
/// A ZST, exactly like [`crate::refund::NativeRefundProvider`].
#[derive(Debug, Default, Clone, Copy)]
pub struct NativeTaxProvider;

#[async_trait::async_trait(?Send)]
impl TaxProvider for NativeTaxProvider {
    fn kind(&self) -> TaxProviderKind {
        TaxProviderKind::Native
    }

    async fn compute_tax(&self, _ctx: &TaxContext<'_>) -> Result<TaxAmount, ProviderError> {
        // USD launch owes no tax. The seam exists so enabling Stripe Tax later is a
        // provider swap (a new TaxProviderKind + a build_tax_provider arm calling
        // `automatic_tax`), not a reconciler/schema change — `tax_cents` already exists.
        Ok(TaxAmount::zero())
    }
}

/// Build the configured tax provider once at boot. `Native` is the default (and the
/// only kind today). A future `StripeTaxProvider` arm would build here behind a creds
/// check, mirroring [`crate::metering::provider::build_provider`].
///
/// # Errors
/// Returns [`ProviderError::Config`] for a future export backend selected without its
/// required creds. Native never errors.
pub fn build_tax_provider(
    cfg: &TaxProviderConfig,
) -> Result<std::sync::Arc<dyn TaxProvider>, ProviderError> {
    match cfg.kind {
        TaxProviderKind::Native => Ok(std::sync::Arc::new(NativeTaxProvider)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_default_is_native() {
        assert_eq!(TaxProviderConfig::native().kind, TaxProviderKind::Native);
        assert_eq!(TaxProviderConfig::default().kind, TaxProviderKind::Native);
    }

    #[test]
    fn native_parses_and_builds_and_reports_native_kind() {
        let kind = TaxProviderKind::parse("native").expect("native parses");
        assert_eq!(kind, TaxProviderKind::Native);
        let provider = build_tax_provider(&TaxProviderConfig { kind })
            .expect("native tax provider must build");
        assert_eq!(provider.kind(), TaxProviderKind::Native);
    }

    #[test]
    fn native_parse_is_case_insensitive_and_trims() {
        assert_eq!(TaxProviderKind::parse("  Native  ").unwrap(), TaxProviderKind::Native);
    }

    #[test]
    fn unknown_tax_provider_value_is_rejected() {
        assert_eq!(TaxProviderKind::parse("stripe").unwrap_err(), "stripe");
        assert_eq!(TaxProviderKind::parse("").unwrap_err(), "");
    }

    #[compio::test]
    async fn native_compute_tax_returns_zero_for_any_base() {
        let provider = NativeTaxProvider;
        for base in [0_i64, 1, 100, 999_999] {
            let ctx = TaxContext {
                creator_id: uuid::Uuid::nil(),
                taxable_base_cents: base,
                currency: "usd",
                period: chrono::NaiveDate::from_ymd_opt(2026, 6, 1).unwrap(),
            };
            let tax = provider.compute_tax(&ctx).await.expect("native never errors");
            assert_eq!(tax, TaxAmount::zero(), "native tax must be 0 for base {base}");
            assert_eq!(tax.tax_cents, 0);
        }
    }
}
