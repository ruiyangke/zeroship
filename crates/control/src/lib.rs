//! Control plane for appbase — app registry, deploy, plan management.
//!
//! Defines the `AppRegistry` trait (the boundary between control and data planes)
//! and provides `SqlxRegistry` as the default implementation backed by SQLite or Postgres.

pub mod sqlx_registry;

use async_trait::async_trait;
use serde::Serialize;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// Full app data including JS bundle.
#[derive(Debug, Clone)]
pub struct AppData {
    pub id: String,
    pub plan_id: String,
    pub server_js: String,
    pub client_html: Option<Vec<u8>>,
    pub version: i64,
}

/// App metadata (without JS bundle).
#[derive(Debug, Clone, Serialize)]
pub struct AppRecord {
    pub id: String,
    pub plan_id: String,
    pub version: i64,
    pub api_key: String,
    pub created_at: String,
    pub updated_at: String,
}

/// Control plane errors.
#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    #[error("App not found: {0}")]
    NotFound(String),
    #[error("App already exists: {0}")]
    AlreadyExists(String),
    #[error("Database error: {0}")]
    Database(String),
    #[error("Invalid input: {0}")]
    InvalidInput(String),
}

// ---------------------------------------------------------------------------
// AppRegistry trait
// ---------------------------------------------------------------------------

/// The boundary between control plane and data plane.
///
/// Day 1: `SqlxRegistry` (SQLite or Postgres via AnyPool).
/// Day N: `RemoteRegistry` (HTTP client to central control plane API).
#[async_trait]
pub trait AppRegistry: Send + Sync {
    /// Load app code + metadata. Called on data plane cache miss.
    async fn get_app(&self, app_id: &str) -> Result<Option<AppData>, RegistryError>;

    /// Get current version. Called by poll loop for hot-reload detection.
    async fn get_version(&self, app_id: &str) -> Result<Option<i64>, RegistryError>;

    /// Get app's plan ID.
    async fn get_plan(&self, app_id: &str) -> Result<String, RegistryError>;

    /// Create a new app. Returns the app record with generated API key.
    async fn create_app(&self, app_id: &str, plan_id: &str) -> Result<AppRecord, RegistryError>;

    /// Deploy new JS to an app. Returns the new version number.
    async fn deploy(
        &self,
        app_id: &str,
        server_js: &str,
        client_html: Option<&[u8]>,
    ) -> Result<i64, RegistryError>;

    /// Delete an app. Returns true if it existed.
    async fn delete_app(&self, app_id: &str) -> Result<bool, RegistryError>;

    /// List all apps (metadata only).
    async fn list_apps(&self) -> Result<Vec<AppRecord>, RegistryError>;

    /// Update an app's plan. Returns true if the app existed.
    async fn set_plan(&self, app_id: &str, plan_id: &str) -> Result<bool, RegistryError>;

    /// Validate an API key for deploy operations.
    async fn validate_key(&self, app_id: &str, key: &str) -> Result<bool, RegistryError>;
}
