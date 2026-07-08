use std::collections::HashSet;
use std::sync::Mutex;

use crate::metering::provider::{Clock, ProviderError, WebhookEvent, WebhookOutcome};
use crate::SecretString;

pub(crate) struct StripeWebhookState {
    secret: SecretString,
    clock: Clock,
    seen: Mutex<HashSet<String>>,
}

impl StripeWebhookState {
    pub(crate) fn new(secret: SecretString, clock: Clock) -> Result<Self, ProviderError> {
        if secret.expose_secret().trim().is_empty() {
            return Err(ProviderError::Config(
                "stripe webhook signing secret resolved empty".to_string(),
            ));
        }
        Ok(Self {
            secret,
            clock,
            seen: Mutex::new(HashSet::new()),
        })
    }

    pub(crate) fn verify(&self, payload: &[u8], sig: &str) -> Result<(), ProviderError> {
        crate::stripe_handlers::verify_stripe_signature(
            payload,
            sig,
            self.secret.expose_secret(),
            self.clock.now_unix(),
            300,
        )
        .map(|_| ())
        .map_err(|e| ProviderError::PermanentReject {
            status: 400,
            message: format!("stripe webhook signature rejected: {e}"),
        })
    }

    pub(crate) fn handle(&self, event: WebhookEvent) -> Result<WebhookOutcome, ProviderError> {
        let event_id = event
            .payload
            .get("id")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
            .filter(|id| !id.trim().is_empty())
            .ok_or_else(|| {
                ProviderError::PermanentReject {
                    status: 400,
                    message: "stripe webhook event missing id".to_string(),
                }
            })?;
        let mut seen = self
            .seen
            .lock()
            .map_err(|_| {
                ProviderError::Store("stripe webhook idempotency lock poisoned".to_string())
            })?;
        if seen.insert(event_id) {
            Ok(WebhookOutcome::Processed)
        } else {
            Ok(WebhookOutcome::Ignored)
        }
    }
}
