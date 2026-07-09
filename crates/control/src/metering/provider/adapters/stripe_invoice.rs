use std::sync::Arc;

use uuid::Uuid;

use crate::metering::provider::{
    BillingPeriod, Capabilities, CorrectionCapability, InvoiceRef, MeteringProvider, ProviderCtx,
    ProviderError, SecretHandle, SubjectRef,
};

#[derive(Debug, serde::Deserialize)]
struct StripeInvoiceCfg {
    secret_key: SecretHandle,
}

pub struct StripeInvoiceProvider {
    store: Arc<dyn crate::metering::provider::LiteStore>,
    _secret_key: crate::SecretString,
}

impl std::fmt::Debug for StripeInvoiceProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StripeInvoiceProvider")
            .field("store", &"<lite store>")
            .field("secret_key", &self._secret_key)
            .finish()
    }
}

pub fn factory(ctx: &ProviderCtx) -> Result<Arc<dyn MeteringProvider>, ProviderError> {
    let cfg: StripeInvoiceCfg = ctx.parse_adapter_config("stripe_invoice")?;
    let secret_key = ctx.secrets.resolve(&cfg.secret_key)?;
    if secret_key.expose_secret().trim().is_empty() {
        return Err(ProviderError::Config(
            "stripe_invoice: secret_key resolved empty".to_string(),
        ));
    }
    let store = ctx.store.clone().ok_or_else(|| {
        ProviderError::Config("stripe_invoice: LiteStore is required".to_string())
    })?;
    Ok(Arc::new(StripeInvoiceProvider {
        store,
        _secret_key: secret_key,
    }))
}

#[async_trait::async_trait(?Send)]
impl crate::metering::provider::Invoicer for StripeInvoiceProvider {
    async fn close_period(
        &self,
        subject: &SubjectRef,
        period: BillingPeriod,
    ) -> Result<InvoiceRef, ProviderError> {
        let creator = Uuid::parse_str(subject.as_str()).map_err(|e| {
            ProviderError::Config(format!("stripe_invoice: subject is not a creator UUID: {e}"))
        })?;
        self.store.close_period_invoice(&creator, period).await
    }

    async fn adjustment_note(
        &self,
        subject: &SubjectRef,
        note: &crate::metering::provider::AdjustmentNote,
    ) -> Result<InvoiceRef, ProviderError> {
        let creator = Uuid::parse_str(subject.as_str()).map_err(|e| {
            ProviderError::Config(format!("stripe_invoice: subject is not a creator UUID: {e}"))
        })?;
        self.store.adjustment_note_invoice(&creator, note).await
    }
}

impl MeteringProvider for StripeInvoiceProvider {
    fn id(&self) -> &str {
        "stripe_invoice"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::INVOICE
    }

    fn as_invoicer(&self) -> Option<&dyn crate::metering::provider::Invoicer> {
        Some(self)
    }

    fn owns_local_invoice(&self) -> bool {
        true
    }

    fn correction(&self) -> CorrectionCapability {
        CorrectionCapability::InvoiceCredit
    }
}
