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
        // NO `env:<NAME>` arm. It was the same env-to-env indirection as the
        // deleted `urn:zeroship:env:` secret reference, in a second place: an
        // operator-supplied provider-config STRING naming an environment
        // variable, so the variable read had no declared identity and the
        // billing provider could be pointed at any variable in the process. A
        // handle now names a value the control plane already resolved.
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

impl std::fmt::Debug for ProviderCtx {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderCtx")
            .field("http", &self.http)
            .field("raw_config", &self.raw_config)
            .field("secrets", &"<secret resolver>")
            .field("clock", &self.clock)
            .field("has_store", &self.store.is_some())
            .finish()
    }
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
