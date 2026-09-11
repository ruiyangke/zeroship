//! Generated-declaration configuration for the workflow authority.

use std::path::{Path, PathBuf};
use zeroship_core::config::{
    zeroship_config, BootstrapControl, CheckFormat, CommandControl, ObservabilityControls,
    Operational, OverlaySelector, Secret,
};
use zeroship_core::observability::LogFormat;
use zeroship_workflow::service::{AppPolicy, PlatformPolicy};

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
    /// Private workflow signing key file.
    #[config(name = "workflow.service_key_file", default = PathBuf::new())]
    pub service_key_file: Operational<PathBuf>,
    /// Issuer-bound peer verification keys.
    #[config(name = "workflow.service_peers_file", default = PathBuf::new())]
    pub service_peers_file: Operational<PathBuf>,
    /// Workflow payload storage location.
    #[config(name = "workflow.payload_url", default = String::new())]
    pub payload_url: Operational<String>,
    /// HTTP worker threads.
    #[config(name = "workflow.http_threads", default = 2)]
    pub http_threads: Operational<usize>,
    /// Maximum connections per HTTP thread.
    #[config(name = "workflow.max_connections", default = 1024)]
    pub max_connections: Operational<usize>,
    /// Maximum JSON request size; payload uploads stream separately.
    #[config(name = "workflow.max_request_bytes", default = 64 * 1024 * 1024)]
    pub max_request_bytes: Operational<usize>,
    /// Interval between durable maintenance sweeps.
    #[config(name = "workflow.tick_interval_ms", default = 1000)]
    pub tick_interval_ms: Operational<u64>,
    /// Maximum items in each deployment or payload maintenance sweep.
    #[config(name = "workflow.maintenance_batch", default = 128)]
    pub maintenance_batch: Operational<usize>,
    /// Maximum live runs per app.
    #[config(name = "workflow.max_live_runs", default = AppPolicy::default().max_live_runs)]
    pub max_live_runs: Operational<i64>,
    /// Maximum child workflow depth.
    #[config(name = "workflow.max_child_depth", default = AppPolicy::default().max_child_depth)]
    pub max_child_depth: Operational<i64>,
    /// Maximum concurrent tasks per app.
    #[config(name = "workflow.max_running", default = AppPolicy::default().max_running)]
    pub max_running: Operational<i64>,
    /// Maximum inline input or signal size.
    #[config(name = "workflow.max_input_bytes", default = AppPolicy::default().max_input_bytes)]
    pub max_input_bytes: Operational<usize>,
    /// Maximum operations accepted in a task completion.
    #[config(name = "workflow.max_frontier", default = AppPolicy::default().max_frontier)]
    pub max_frontier: Operational<usize>,
    /// Maximum retained journal size per generation.
    #[config(name = "workflow.max_journal_bytes", default = AppPolicy::default().max_journal_bytes)]
    pub max_journal_bytes: Operational<usize>,
    /// Maximum size of a payload object.
    #[config(name = "workflow.max_payload_bytes", default = AppPolicy::default().max_payload_bytes)]
    pub max_payload_bytes: Operational<i64>,
    /// Maximum payload objects per app.
    #[config(name = "workflow.max_payload_objects", default = AppPolicy::default().max_payload_objects)]
    pub max_payload_objects: Operational<i64>,
    /// Maximum retained payload storage per app.
    #[config(name = "workflow.max_payload_storage_bytes", default = AppPolicy::default().max_payload_storage_bytes)]
    pub max_payload_storage_bytes: Operational<i64>,
    /// Retention window for unreferenced uploads.
    #[config(name = "workflow.payload_staging_retention_ms", default = AppPolicy::default().payload_staging_retention_ms)]
    pub payload_staging_retention_ms: Operational<i64>,
    /// Maximum attempts for a compensator.
    #[config(name = "workflow.max_compensation_attempts", default = AppPolicy::default().max_compensation_attempts)]
    pub max_compensation_attempts: Operational<i32>,
    /// Delay between compensation attempts.
    #[config(name = "workflow.compensation_retry_ms", default = AppPolicy::default().compensation_retry_ms)]
    pub compensation_retry_ms: Operational<i64>,
    /// Maximum active schedules per app.
    #[config(name = "workflow.max_schedules", default = AppPolicy::default().max_schedules)]
    pub max_schedules: Operational<usize>,
    /// Maximum occurrences replayed in a schedule sweep.
    #[config(name = "workflow.max_schedule_backfill", default = AppPolicy::default().max_schedule_backfill)]
    pub max_schedule_backfill: Operational<usize>,
    /// Minimum fixed schedule interval.
    #[config(name = "workflow.min_schedule_interval_ms", default = AppPolicy::default().min_schedule_interval_ms)]
    pub min_schedule_interval_ms: Operational<i64>,
    /// Maximum public signal capability lifetime.
    #[config(name = "workflow.max_signal_token_lifetime_seconds", default = AppPolicy::default().max_signal_token_lifetime_seconds)]
    pub max_signal_token_lifetime_seconds: Operational<i64>,
    /// Task lease duration.
    #[config(name = "workflow.lease_ms", default = AppPolicy::default().lease_ms)]
    pub lease_ms: Operational<i64>,
    /// Retention window for mutation receipts.
    #[config(name = "workflow.request_retention_ms", default = AppPolicy::default().request_retention_ms)]
    pub request_retention_ms: Operational<i64>,
    /// Workflow database login; the service receives DML and narrow policy reads.
    #[config(name = "workflow.database_url")]
    pub database_url: Secret<String>,
}

impl WorkflowSettings {
    pub fn policy(&self) -> Result<PlatformPolicy, zeroship_workflow::WorkflowServiceError> {
        PlatformPolicy::new(AppPolicy {
            max_live_runs: *self.max_live_runs.get(),
            max_child_depth: *self.max_child_depth.get(),
            max_running: *self.max_running.get(),
            max_input_bytes: *self.max_input_bytes.get(),
            max_frontier: *self.max_frontier.get(),
            max_journal_bytes: *self.max_journal_bytes.get(),
            max_payload_bytes: *self.max_payload_bytes.get(),
            max_payload_objects: *self.max_payload_objects.get(),
            max_payload_storage_bytes: *self.max_payload_storage_bytes.get(),
            payload_staging_retention_ms: *self.payload_staging_retention_ms.get(),
            max_compensation_attempts: *self.max_compensation_attempts.get(),
            compensation_retry_ms: *self.compensation_retry_ms.get(),
            max_schedules: *self.max_schedules.get(),
            max_schedule_backfill: *self.max_schedule_backfill.get(),
            min_schedule_interval_ms: *self.min_schedule_interval_ms.get(),
            max_signal_token_lifetime_seconds: *self.max_signal_token_lifetime_seconds.get(),
            lease_ms: *self.lease_ms.get(),
            request_retention_ms: *self.request_retention_ms.get(),
            ..AppPolicy::default()
        })
    }
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
