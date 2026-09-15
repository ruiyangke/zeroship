//! Worker services shared by the executable and their owning unit tests.

#![recursion_limit = "256"]

use std::sync::Arc;

use zeroship_bundle::{BlobStore, WorkflowBlobStore};
use zeroship_storage::StorageBackendConfig;

pub mod cache;
pub mod config;
pub mod db_posture;
pub mod executable;
pub mod handler;
pub mod health;
pub mod logs;
pub mod metrics;
pub mod policy;
pub mod sync;
pub mod workflow_creator;
pub mod workflow_runtime;

/// Resolved process resources supplied to the worker's services.
#[allow(missing_debug_implementations)]
pub struct WorkerConfig {
    /// Joined instance identity used to verify dispatch and authenticate
    /// control-plane reads. Startup joins with a trusted signer's join token
    /// before constructing this configuration, drawing the instance keypair
    /// in memory; no worker holds a `svc/worker` role key.
    pub service_auth: Arc<zeroship_core::service_peers::ServiceAuth>,
    pub control_url: String,
    pub control_key: String,
    pub db_url: Option<String>,
    /// Process-owned KV store. Absence leaves the app KV namespace unavailable.
    pub kv_store: Option<zeroship_kv::KvStore>,
    /// App object-storage backend. Absence leaves its namespace unavailable.
    pub storage_backend: Option<StorageBackendConfig>,
    pub max_isolates: usize,
    pub max_pinned_isolates_per_app: usize,
    pub poll_interval_secs: u64,
    /// Grace period passed to ntex for draining requests during shutdown.
    pub shutdown_timeout_secs: u64,
    /// Content-addressed deployment blobs shared with control and gateway.
    pub blob_store: Arc<dyn BlobStore>,
    pub workflow_blob_store: Arc<dyn WorkflowBlobStore>,
    pub max_step_blob_bytes: u64,
    /// Additional workflow replay ingress for local acceptance tests.
    /// Startup requires a loopback bind when this endpoint is enabled.
    pub workflow_advance_unsigned: bool,
}

#[cfg(test)]
mod identity_fixture;
#[cfg(test)]
mod test_database;

#[cfg(test)]
mod worker_fixture;
