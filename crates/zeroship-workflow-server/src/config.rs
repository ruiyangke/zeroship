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
    /// Private signing key for the workflow service's Control requests.
    #[config(name = "workflow.service_key_file", default = PathBuf::new())]
    pub service_key_file: Operational<PathBuf>,
    /// Control origin used for deployment queue retention.
    #[config(name = "workflow.control_url", default = String::new())]
    pub control_url: Operational<String>,
    /// Migration-service origin used to install and upgrade app journals.
    ///
    /// Empty disables the journal endpoint, loudly: Control and workers are told
    /// the manager cannot provision rather than being answered as though it had.
    #[config(name = "workflow.migrate_url", default = String::new())]
    pub migrate_url: Operational<String>,
    /// HTTP worker threads.
    #[config(name = "workflow.http_threads", default = 2)]
    pub http_threads: Operational<usize>,
    /// Maximum connections per HTTP thread.
    #[config(name = "workflow.max_connections", default = 1024)]
    pub max_connections: Operational<usize>,
    /// Maximum cached app policy observations, shared by every HTTP thread.
    #[config(name = "workflow.policy_cache_entries", default = 1024)]
    pub policy_cache_entries: Operational<usize>,
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
    /// Delay between completed native manager passes.
    #[config(name = "workflow.driver_interval_ms", default = 1000)]
    pub driver_interval_ms: Operational<u64>,
    /// Deadline for each scheduling, recovery, retention or closing lane in a manager pass.
    #[config(name = "workflow.driver_lane_timeout_ms", default = 10000)]
    pub driver_lane_timeout_ms: Operational<u64>,
    /// Inactivity after which an app's recovery responsibility may close.
    #[config(name = "workflow.closing_idle_ms", default = 900_000)]
    pub closing_idle_ms: Operational<u64>,
    /// Bound on a closing attempt's delivery before responsibility reopens.
    #[config(name = "workflow.closing_timeout_ms", default = 300_000)]
    pub closing_timeout_ms: Operational<u64>,
    /// Delay before retrying a closing attempt that did not retire; it doubles per attempt.
    #[config(name = "workflow.closing_backoff_ms", default = 60000)]
    pub closing_backoff_ms: Operational<u64>,
    /// Ceiling of the doubling closing backoff.
    #[config(name = "workflow.closing_backoff_max_ms", default = 3_600_000)]
    pub closing_backoff_max_ms: Operational<u64>,
    /// Fewest placement slots an execution zone's capacity target may name.
    #[config(name = "workflow.capacity_min_slots", default = 0)]
    pub capacity_min_slots: Operational<i64>,
    /// Most placement slots an execution zone's capacity target may name.
    #[config(name = "workflow.capacity_max_slots", default = 1024)]
    pub capacity_max_slots: Operational<i64>,
    /// Idleness a zone's demand must stay below its target before it shrinks.
    #[config(name = "workflow.capacity_hold_down_ms", default = 300_000)]
    pub capacity_hold_down_ms: Operational<u64>,
    /// Deadline for one claimed capacity request before it is recorded unavailable.
    #[config(name = "workflow.capacity_request_timeout_ms", default = 10000)]
    pub capacity_request_timeout_ms: Operational<u64>,
    /// Pause after a capacity reply before the same target is requested again.
    #[config(name = "workflow.capacity_retry_interval_ms", default = 30000)]
    pub capacity_retry_interval_ms: Operational<u64>,
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
