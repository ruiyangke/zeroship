use std::collections::HashMap;
use std::sync::Arc;

use serde::de::DeserializeOwned;

use super::{LiteStore, ProviderError};
use crate::SecretString;

thread_local! {
    static PROVIDER_HTTP_CLIENT: cyper::Client = cyper::Client::new();
}

/// Cheap handle that returns the calling thread's `cyper` client.
#[derive(Debug, Clone, Copy, Default)]
pub struct HttpClientFactory;

impl HttpClientFactory {
    #[must_use]
    pub fn client(&self) -> cyper::Client {
        PROVIDER_HTTP_CLIENT.with(Clone::clone)
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Clock;

impl Clock {
    #[must_use]
    pub fn now_unix(&self) -> i64 {
        chrono::Utc::now().timestamp()
    }
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(transparent)]
pub struct SecretHandle(pub String);

pub trait SecretResolver: Send + Sync {
    fn resolve(&self, handle: &SecretHandle) -> Result<SecretString, ProviderError>;
}

#[derive(Debug, Default)]
pub struct StaticSecretResolver {
    values: HashMap<String, String>,
}

impl StaticSecretResolver {
    #[must_use]
    pub fn new(values: HashMap<String, String>) -> Self {
        Self { values }
    }
}

impl SecretResolver for StaticSecretResolver {
    fn resolve(&self, handle: &SecretHandle) -> Result<SecretString, ProviderError> {
        let raw = handle.0.trim();
        if raw.is_empty() {
            return Err(ProviderError::Config("empty secret handle".to_string()));
        }
        if let Some(var) = raw.strip_prefix("env:") {
            let value = std::env::var(var).map_err(|_| {
                ProviderError::Config(format!("secret handle '{raw}' references an unset env var"))
            })?;
            return Ok(SecretString::new(value));
        }
        if let Some(value) = self.values.get(raw) {
            return Ok(SecretString::new(value.clone()));
        }
        Err(ProviderError::Config(format!("unknown secret handle '{raw}'")))
    }
}

pub struct ProviderCtx {
    pub http: HttpClientFactory,
    pub raw_config: serde_json::Value,
    pub secrets: Arc<dyn SecretResolver>,
    pub clock: Clock,
    pub store: Option<Arc<dyn LiteStore>>,
}

impl ProviderCtx {
    #[must_use]
    pub fn new(
        raw_config: serde_json::Value,
        secrets: Arc<dyn SecretResolver>,
        store: Option<Arc<dyn LiteStore>>,
    ) -> Self {
        Self {
            http: HttpClientFactory,
            raw_config,
            secrets,
            clock: Clock,
            store,
        }
    }

    pub fn parse_config<T: DeserializeOwned>(&self) -> Result<T, ProviderError> {
        serde_json::from_value(self.raw_config.clone()).map_err(|e| {
            ProviderError::Config(format!("provider config is invalid: {e}"))
        })
    }

    pub fn parse_adapter_config<T: DeserializeOwned>(&self, id: &str) -> Result<T, ProviderError> {
        let value = self
            .raw_config
            .get(id)
            .cloned()
            .unwrap_or_else(|| self.raw_config.clone());
        serde_json::from_value(value).map_err(|e| {
            ProviderError::Config(format!("{id}: provider config is invalid: {e}"))
        })
    }
}
