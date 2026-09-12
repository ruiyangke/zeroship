//! Generated configuration for the workflow metadata coordinator.

use std::path::{Path, PathBuf};
use zeroship_core::config::{
    zeroship_config, BootstrapControl, CheckFormat, CommandControl, ObservabilityControls,
    Operational, OverlaySelector, Secret,
};
use zeroship_core::observability::LogFormat;

pub const DEFAULT_LOG_FILTER: &str = "info,zeroship_workflow_server=debug";

#[zeroship_config(binary = "zeroship-workflow-server", scope = "workflow")]
#[derive(Debug)]
pub struct WorkflowSettings {
    /// Optional shared TOML overlay.
    #[config(shared = CONFIG)]
    pub config: BootstrapControl<Option<PathBuf>>,
    /// Disable automatic overlay discovery.
    #[config(shared = NO_CONFIG)]
    pub no_config: BootstrapControl<bool>,
    /// Validate and report configuration without opening databases or listeners.
    #[config(shared = CHECK_CONFIG)]
    pub check_config: CommandControl<bool>,
    /// Configuration report format.
    #[config(shared = CHECK_CONFIG_FORMAT, default = CheckFormat::Text)]
    pub check_config_format: CommandControl<CheckFormat>,
    /// Tracing filter.
    #[config(shared = OBSERVABILITY_LOG_FILTER, default = DEFAULT_LOG_FILTER.to_owned())]
    pub log_filter: Operational<String>,
    /// Tracing format.
    #[config(shared = OBSERVABILITY_LOG_FORMAT, default = LogFormat::Auto)]
    pub log_format: Operational<LogFormat>,
    /// HTTP listener address.
    #[config(name = "workflow.listen", default = "127.0.0.1:9093".to_owned())]
    pub listen: Operational<String>,
    /// Issuer-bound peer verification keys.
    #[config(name = "workflow.service_peers_file", default = PathBuf::new())]
    pub service_peers_file: Operational<PathBuf>,
    /// HTTP worker threads.
    #[config(name = "workflow.http_threads", default = 2)]
    pub http_threads: Operational<usize>,
    /// Maximum connections per HTTP thread.
    #[config(name = "workflow.max_connections", default = 1024)]
    pub max_connections: Operational<usize>,
    /// Maximum metadata JSON request size.
    #[config(name = "workflow.max_request_bytes", default = crate::api::DEFAULT_MAX_REQUEST_BYTES)]
    pub max_request_bytes: Operational<usize>,
    /// Metadata database connections per HTTP thread.
    #[config(name = "workflow.database_connections", default = 8)]
    pub database_connections: Operational<usize>,
    /// Maximum wait to acquire a metadata connection.
    #[config(name = "workflow.database_acquire_timeout_ms", default = 5000)]
    pub database_acquire_timeout_ms: Operational<u64>,
    /// Deadline for a complete metadata transaction.
    #[config(name = "workflow.database_command_timeout_ms", default = 10000)]
    pub database_command_timeout_ms: Operational<u64>,
    /// Worker registration lifetime between heartbeats.
    #[config(name = "workflow.worker_ttl_ms", default = 30000)]
    pub worker_ttl_ms: Operational<u64>,
    /// Placement lifetime between authorized renewals.
    #[config(name = "workflow.assignment_ttl_ms", default = 30000)]
    pub assignment_ttl_ms: Operational<u64>,
    /// Maximum records in a metadata response page.
    #[config(name = "workflow.batch_limit", default = 128)]
    pub batch_limit: Operational<usize>,
    /// Maximum pending lifecycle commands per app.
    #[config(name = "workflow.max_pending_management", default = 1024)]
    pub max_pending_management: Operational<usize>,
    /// Interval between expired service-assertion cleanup sweeps.
    #[config(name = "workflow.replay_sweep_ms", default = 30000)]
    pub replay_sweep_ms: Operational<u64>,
    /// Platform coordination metadata login; no customer database credentials.
    #[config(name = "workflow.database_url")]
    pub database_url: Secret<String>,
}

impl OverlaySelector for WorkflowSettingsSources {
    fn overlay_path(&self) -> Option<&Path> {
        self.config.as_deref()
    }
    fn allow_discovery(&self) -> bool {
        !self.no_config
    }
}
impl ObservabilityControls for WorkflowSettings {
    fn log_filter(&self) -> &str {
        self.log_filter.get()
    }
    fn log_format(&self) -> LogFormat {
        *self.log_format.get()
    }
}
