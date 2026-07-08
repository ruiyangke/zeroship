use std::sync::Arc;

use uuid::Uuid;

use crate::metering::provider::{
    BillingPeriod, Capabilities, CorrectionCapability, InvoiceRef, LineItem, MeteringProvider,
    ProviderCtx, ProviderError, Rater, RatedInput, SecretHandle, SubjectRef, WebhookEvent,
    WebhookOutcome, WebhookSink,
};

#[derive(Debug, serde::Deserialize)]
struct StripeInvoiceCfg {
    secret_key: SecretHandle,
    #[serde(default)]
    webhook_secret: Option<SecretHandle>,
}

pub struct StripeInvoiceProvider {
    store: Option<Arc<dyn crate::metering::provider::LiteStore>>,
    _secret_key: crate::SecretString,
    _webhook_secret: Option<crate::SecretString>,
}

pub fn factory(ctx: &ProviderCtx) -> Result<Arc<dyn MeteringProvider>, ProviderError> {
    let cfg: StripeInvoiceCfg = ctx.parse_adapter_config("stripe_invoice")?;
    let secret_key = ctx.secrets.resolve(&cfg.secret_key)?;
    if secret_key.expose_secret().trim().is_empty() {
        return Err(ProviderError::Config(
            "stripe_invoice: secret_key resolved empty".to_string(),
        ));
    }
    let webhook_secret = cfg
        .webhook_secret
        .as_ref()
        .map(|h| ctx.secrets.resolve(h))
        .transpose()?;
    Ok(Arc::new(StripeInvoiceProvider {
        store: ctx.store.clone(),
        _secret_key: secret_key,
        _webhook_secret: webhook_secret,
    }))
}

impl StripeInvoiceProvider {
    fn store(&self) -> Result<&dyn crate::metering::provider::LiteStore, ProviderError> {
        self.store
            .as_deref()
            .ok_or_else(|| ProviderError::Config("stripe_invoice: LiteStore is required".to_string()))
    }
}

#[async_trait::async_trait(?Send)]
impl Rater for StripeInvoiceProvider {
    async fn rate(
        &self,
        _subject: &SubjectRef,
        _period: BillingPeriod,
        _input: &RatedInput,
    ) -> Result<Vec<LineItem>, ProviderError> {
        // TODO(S4/S6): rate from the stream recompute witness. S1 intentionally
        // preserves the current usage_aggregates/pricing reconciler as the source.
        Ok(Vec::new())
    }
}

#[async_trait::async_trait(?Send)]
impl crate::metering::provider::Invoicer for StripeInvoiceProvider {
    async fn close_period(
        &self,
        subject: &SubjectRef,
        period: BillingPeriod,
        _lines: &[LineItem],
    ) -> Result<InvoiceRef, ProviderError> {
        let creator = Uuid::parse_str(subject.as_str()).map_err(|e| {
            ProviderError::Config(format!("stripe_invoice: subject is not a creator UUID: {e}"))
        })?;
        self.store()?.close_period_invoice(&creator, period).await
    }

    async fn adjustment_note(
        &self,
        _subject: &SubjectRef,
        _note: &crate::metering::provider::AdjustmentNote,
    ) -> Result<InvoiceRef, ProviderError> {
        Err(ProviderError::Config(
            "stripe_invoice: adjustment notes are not implemented in S1".to_string(),
        ))
    }
}

#[async_trait::async_trait(?Send)]
impl WebhookSink for StripeInvoiceProvider {
    fn verify(&self, _payload: &[u8], _sig: &str) -> Result<(), ProviderError> {
        Ok(())
    }

    async fn handle(&self, _event: WebhookEvent) -> Result<WebhookOutcome, ProviderError> {
        Ok(WebhookOutcome::Ignored)
    }
}

impl MeteringProvider for StripeInvoiceProvider {
    fn id(&self) -> &str {
        "stripe_invoice"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::RATE | Capabilities::INVOICE | Capabilities::WEBHOOK
    }

    fn as_rater(&self) -> Option<&dyn Rater> {
        Some(self)
    }

    fn as_invoicer(&self) -> Option<&dyn crate::metering::provider::Invoicer> {
        Some(self)
    }

    fn as_webhook(&self) -> Option<&dyn WebhookSink> {
        Some(self)
    }

    fn correction(&self) -> CorrectionCapability {
        CorrectionCapability::InvoiceCredit
    }
}
