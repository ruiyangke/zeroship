use std::sync::Arc;

use uuid::Uuid;

use crate::metering::provider::{
    BillingPeriod, Capabilities, CorrectionCapability, InvoiceRef, LineItem, MeteringProvider,
    ProviderCtx, ProviderError, Rater, RatedInput, SecretHandle, SubjectRef, WebhookEvent,
    WebhookOutcome, WebhookSink,
};

use super::stripe_webhook::StripeWebhookState;

#[derive(Debug, serde::Deserialize)]
struct StripeInvoiceCfg {
    secret_key: SecretHandle,
    webhook_secret: SecretHandle,
}

pub struct StripeInvoiceProvider {
    store: Arc<dyn crate::metering::provider::LiteStore>,
    _secret_key: crate::SecretString,
    webhook: StripeWebhookState,
}

impl std::fmt::Debug for StripeInvoiceProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StripeInvoiceProvider")
            .field("store", &"<lite store>")
            .field("secret_key", &self._secret_key)
            .field("webhook", &self.webhook)
            .finish()
    }
}

pub fn factory(ctx: &ProviderCtx) -> Result<Arc<dyn MeteringProvider>, ProviderError> {
    let cfg: StripeInvoiceCfg = ctx.parse_adapter_config("stripe_invoice")?;
    let secret_key = ctx.secrets.resolve(&cfg.secret_key)?;
    let webhook_secret = ctx.secrets.resolve(&cfg.webhook_secret)?;
    if secret_key.expose_secret().trim().is_empty() {
        return Err(ProviderError::Config(
            "stripe_invoice: secret_key resolved empty".to_string(),
        ));
    }
    let store = ctx.store.clone().ok_or_else(|| {
        ProviderError::Config("stripe_invoice: LiteStore is required".to_string())
    })?;
    let webhook = StripeWebhookState::new(webhook_secret, ctx.clock)?;
    Ok(Arc::new(StripeInvoiceProvider {
        store,
        _secret_key: secret_key,
        webhook,
    }))
}

#[async_trait::async_trait(?Send)]
impl Rater for StripeInvoiceProvider {
    async fn rate(
        &self,
        _subject: &SubjectRef,
        _period: BillingPeriod,
        _input: &RatedInput,
    ) -> Result<Vec<LineItem>, ProviderError> {
        // The stream recompute witness will provide first-class lines here.
        // Until then, period close preserves the existing reconciler as source.
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

#[async_trait::async_trait(?Send)]
impl WebhookSink for StripeInvoiceProvider {
    fn verify(&self, payload: &[u8], sig: &str) -> Result<(), ProviderError> {
        self.webhook.verify(payload, sig)
    }

    async fn handle(&self, event: WebhookEvent) -> Result<WebhookOutcome, ProviderError> {
        self.webhook.handle(event)
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
