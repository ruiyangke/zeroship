use std::collections::HashMap;
use std::sync::Arc;

use serde::de::DeserializeOwned;

use crate::{StreamError, StreamTransport};

pub type StreamFactory = fn(&StreamConfig) -> Result<Arc<dyn StreamTransport>, StreamError>;

#[derive(Debug, Clone)]
pub struct StreamConfig {
    raw: serde_json::Value,
}

impl StreamConfig {
    pub fn new(raw: serde_json::Value) -> Self {
        Self { raw }
    }

    pub fn empty() -> Self {
        Self::new(serde_json::Value::Object(Default::default()))
    }

    pub fn parse<T: DeserializeOwned>(&self) -> Result<T, StreamError> {
        serde_json::from_value(self.raw.clone()).map_err(StreamError::from)
    }

    /// Clone this config and mutate its JSON object form.
    pub fn map_object(
        &self,
        f: impl FnOnce(&mut serde_json::Map<String, serde_json::Value>),
    ) -> Result<Self, StreamError> {
        let mut raw = self.raw.clone();
        let Some(obj) = raw.as_object_mut() else {
            return Err(StreamError::Config(
                "stream config must be a JSON object".to_string(),
            ));
        };
        f(obj);
        Ok(Self::new(raw))
    }
}

impl Default for StreamConfig {
    fn default() -> Self {
        Self::empty()
    }
}

impl From<serde_json::Value> for StreamConfig {
    fn from(value: serde_json::Value) -> Self {
        Self::new(value)
    }
}

#[derive(Default)]
pub struct StreamRegistry {
    factories: HashMap<&'static str, StreamFactory>,
}

impl StreamRegistry {
    pub fn register(&mut self, id: &'static str, factory: StreamFactory) {
        if self.factories.insert(id, factory).is_some() {
            panic!("{}", StreamError::DuplicateTransport(id));
        }
    }

    pub fn build(
        &self,
        id: &str,
        config: &StreamConfig,
    ) -> Result<Arc<dyn StreamTransport>, StreamError> {
        let factory = self.factories.get(id).ok_or_else(|| StreamError::UnknownTransport {
            id: id.to_string(),
            known: self.known().join(", "),
        })?;
        factory(config)
    }

    pub fn known(&self) -> Vec<&'static str> {
        let mut known: Vec<_> = self.factories.keys().copied().collect();
        known.sort_unstable();
        known
    }
}

impl std::fmt::Debug for StreamRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamRegistry")
            .field("known", &self.known())
            .finish()
    }
}
