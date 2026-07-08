use std::collections::HashMap;
use std::sync::Arc;

use super::{assert_capability_consistency, MeteringProvider, ProviderCtx, ProviderError};

pub type ProviderFactory = fn(&ProviderCtx) -> Result<Arc<dyn MeteringProvider>, ProviderError>;

#[derive(Debug, Default)]
pub struct ProviderRegistry {
    factories: HashMap<&'static str, ProviderFactory>,
}

impl ProviderRegistry {
    pub fn register(&mut self, id: &'static str, factory: ProviderFactory) {
        assert!(
            self.factories.insert(id, factory).is_none(),
            "duplicate provider id {id}"
        );
    }

    pub fn build(
        &self,
        id: &str,
        ctx: &ProviderCtx,
    ) -> Result<Arc<dyn MeteringProvider>, ProviderError> {
        let f = self.factories.get(id).ok_or_else(|| {
            ProviderError::Config(format!(
                "unknown metering provider '{id}' — known: {}",
                self.known().join(", ")
            ))
        })?;
        let provider = f(ctx)?;
        assert_capability_consistency(&*provider)?;
        Ok(provider)
    }

    #[must_use]
    pub fn known(&self) -> Vec<&'static str> {
        let mut v: Vec<_> = self.factories.keys().copied().collect();
        v.sort_unstable();
        v
    }
}
