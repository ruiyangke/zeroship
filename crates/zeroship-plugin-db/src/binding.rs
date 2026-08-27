//! Immutable identity of one `env.db` binding.
//!
//! A worker thread can keep multiple V8 isolates for the same app alive at
//! different deploys. The app id alone therefore does not identify the schema
//! metadata a CRUD receiver was minted to use. `DbBinding` captures both parts
//! from the active isolate once and travels with every `Db` / `Collection`
//! wrapper and asynchronous CRUD continuation.

/// Deploy token used when a host does not inject `ZEROSHIP_DEPLOY_ID` (local
/// dev, raw-JS deploys, and narrow test harnesses).
pub(crate) const COLD_START_DEPLOY_TOKEN: &str = "cold_start";

/// Identity of one app-at-deploy database binding.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct DbBinding {
    app_id: String,
    deploy_token: String,
}

impl DbBinding {
    pub(crate) fn new(app_id: impl Into<String>, deploy_token: impl Into<String>) -> Self {
        Self {
            app_id: app_id.into(),
            deploy_token: deploy_token.into(),
        }
    }

    pub(crate) fn cold_start(app_id: impl Into<String>) -> Self {
        Self::new(app_id, COLD_START_DEPLOY_TOKEN)
    }

    pub(crate) fn app_id(&self) -> &str {
        &self.app_id
    }

    pub(crate) fn deploy_token(&self) -> &str {
        &self.deploy_token
    }
}
