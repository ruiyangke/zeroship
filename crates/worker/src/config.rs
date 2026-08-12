//! Worker's generated settings: bootstrap, command, observability, operational.
//!
//! In the library rather than `main.rs` so the compiled configuration checker
//! can link the declaration and invoke clap's `CommandFactory` against it.
//!
//! What is NOT here is every credential-bearing field - `control_key`,
//! `worker_key`, `db`, `kv_url`. Those stay hand-spelled on `WorkerCli` in
//! `main.rs` until the `Secret<T>` conversion, which changes their clap carrier
//! to a `-file` path and their resolution order at the same time.

use std::path::{Path, PathBuf};

use zeroship_core::config::{
    zeroship_config, BootstrapControl, CheckFormat, CommandControl, ObservabilityControls,
    Operational, OverlaySelector,
};
use zeroship_core::observability::LogFormat;

/// Tracing directive applied when nothing supplies `observability.log_filter`.
pub const DEFAULT_LOG_FILTER: &str = "info,zeroship_worker=debug";

/// ntex worker threads when nothing supplies `worker.threads`.
///
/// A function, not a constant: the compiled default is "one per core", which is
/// only knowable at run time. The generated resolver calls this exactly when no
/// flag, environment value or overlay entry supplied one.
#[must_use]
pub fn default_worker_threads() -> usize {
    std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get)
}

/// Every value a worker launch resolves before it starts serving.
#[zeroship_config(binary = "zeroship-worker", scope = "worker")]
#[derive(Debug)]
pub struct WorkerSettings {
    /// Optional shared config overlay path.
    #[config(shared = CONFIG)]
    pub config: BootstrapControl<Option<PathBuf>>,

    /// Disable auto-discovery of the well-known overlay path; use compiled
    /// defaults even if `/etc/zeroship/zeroship.toml` exists (O5).
    #[config(shared = NO_CONFIG)]
    pub no_config: BootstrapControl<bool>,

    /// Validate config (CLI + overlay + guards) and print the resolved non-secret
    /// config, then exit without starting the server.
    #[config(shared = CHECK_CONFIG)]
    pub check_config: CommandControl<bool>,

    /// Output format for `--check-config`.
    #[config(shared = CHECK_CONFIG_FORMAT, default = CheckFormat::Text)]
    pub check_config_format: CommandControl<CheckFormat>,

    /// `EnvFilter` directive for the tracing subscriber.
    #[config(shared = OBSERVABILITY_LOG_FILTER, default = DEFAULT_LOG_FILTER.to_owned())]
    pub log_filter: Operational<String>,

    /// Tracing output format; `auto` picks pretty on a TTY and json otherwise.
    #[config(shared = OBSERVABILITY_LOG_FORMAT, default = LogFormat::Auto)]
    pub log_format: Operational<LogFormat>,

    /// Enable the unsigned durable-workflow replay ingress, which performs NO
    /// signature or nonce verification. Hidden because signed advance is the
    /// production transport; this exercises the real replay path.
    ///
    /// The handler itself always ships - this control is what refuses it at
    /// runtime, so the default here IS the production protection. `env = false`
    /// is load-bearing: a stray environment variable must not be able to turn
    /// signature verification off, and a bootstrap control also has no overlay
    /// tier that could persist it.
    #[arg(hide = true)]
    #[config(name = "worker.workflow_advance_unsigned", env = false)]
    pub workflow_advance_unsigned: BootstrapControl<bool>,

    /// HTTP listen port.
    #[config(name = "worker.port", default = 8080)]
    pub port: Operational<u16>,

    /// Address to bind. Defaults to loopback; pass 0.0.0.0 to expose across a network.
    #[config(name = "worker.bind", default = "127.0.0.1".to_owned())]
    pub bind: Operational<String>,

    /// Optional Unix domain socket path. Empty means TCP only.
    #[config(name = "worker.socket", default = String::new())]
    pub socket: Operational<String>,

    /// Number of ntex worker threads. Defaults to one per available core.
    #[config(name = "worker.threads", default = default_worker_threads())]
    pub threads: Operational<usize>,

    /// Control-plane API base URL.
    #[config(shared = CONTROL_URL, default = "http://localhost:9090".to_owned())]
    pub control_url: Operational<String>,

    /// Control-plane polling interval in seconds.
    #[config(shared = POLL_INTERVAL, default = 5)]
    pub poll_interval: Operational<u64>,

    /// Root directory or `s3://` URL for content-addressed deploy blobs.
    #[config(shared = BLOB_STORE, default = "./bundles".to_owned())]
    pub blob_store: Operational<String>,

    /// Maximum number of cached app isolates.
    #[config(name = "worker.max_isolates", default = 200)]
    pub max_isolates: Operational<usize>,

    /// Maximum deploy-pinned workflow replay isolates kept per app.
    #[config(name = "worker.max_pinned_isolates_per_app", default = 4)]
    pub max_pinned_isolates_per_app: Operational<usize>,

    /// Shutdown drain timeout in seconds.
    ///
    /// Zero does NOT mean wait forever. ntex takes its ungraceful branch when the
    /// timeout is zero and stops workers immediately, dropping in-flight requests,
    /// so zero is the harshest setting rather than the most patient one. To wait a
    /// long time, pass a long time.
    #[config(name = "worker.shutdown_timeout", default = 30)]
    pub shutdown_timeout: Operational<u64>,

    /// Object-store location for the app `env.storage` namespace.
    ///
    /// A bare path or `file://...` selects the `LocalFs` backend; `s3://...`
    /// selects the S3 backend (S3/R2/MinIO/Spaces/B2), parsed through the
    /// same grammar as the blob store. Multi-node storage MUST be shared so
    /// an object `put` on one worker node is readable on another: a `LocalFs`
    /// path is a shared volume mounted identically on every replica (the
    /// deploy-blob-store pattern); S3/R2 is inherently shared. S3 credentials
    /// resolve from the AWS env vars. When empty the `env.storage` namespace
    /// is absent.
    #[config(name = "worker.storage_url", default = String::new())]
    pub storage_url: Operational<String>,

    /// Maximum persisted bytes for one workflow step output blob.
    #[config(name = "worker.max_step_blob_bytes", default = 67_108_864)]
    pub max_step_blob_bytes: Operational<u64>,
}

impl OverlaySelector for WorkerSettingsSources {
    fn overlay_path(&self) -> Option<&Path> {
        self.config.as_deref()
    }

    fn allow_discovery(&self) -> bool {
        !self.no_config
    }
}

impl ObservabilityControls for WorkerSettings {
    fn log_filter(&self) -> &str {
        self.log_filter.get()
    }

    fn log_format(&self) -> LogFormat {
        *self.log_format.get()
    }
}

#[cfg(test)]
mod tests {
    use clap::{CommandFactory, Parser};
    use zeroship_core::config::GeneratedConfig;

    use super::{default_worker_threads, WorkerSettings, WorkerSettingsSources};

    #[test]
    fn unsigned_workflow_advance_has_no_environment_or_overlay_source() {
        // The pre-conversion comment on this flag says a stray environment
        // variable must not be able to turn signature verification off. The
        // conversion keeps that by DECLARATION (`env = false`) rather than by
        // remembering to omit an attribute, and a bootstrap control has no TOML
        // tier at all, so a persisted overlay cannot supply it either.
        let arg = WorkerSettingsSources::command()
            .get_arguments()
            .find(|arg| arg.get_id() == "workflow_advance_unsigned")
            .cloned()
            .expect("workflow_advance_unsigned argument");
        assert_eq!(arg.get_long(), Some("workflow-advance-unsigned"));
        assert_eq!(arg.get_env(), None);

        let overlay: toml::Value =
            toml::from_str("[worker]\nworkflow_advance_unsigned = true\n").expect("overlay");
        let resolved = WorkerSettings::resolve_config(
            WorkerSettingsSources::try_parse_from(["zeroship-worker"]).expect("bare parse"),
            Some(&overlay),
        )
        .expect("settings resolve");
        assert!(
            !*resolved.workflow_advance_unsigned.get(),
            "an overlay entry must not enable the unsigned replay ingress"
        );

        // The one-variable control: the same parse WITH the flag.
        let flagged = WorkerSettings::resolve_config(
            WorkerSettingsSources::try_parse_from([
                "zeroship-worker",
                "--workflow-advance-unsigned",
            ])
            .expect("flag parse"),
            None,
        )
        .expect("settings resolve");
        assert!(*flagged.workflow_advance_unsigned.get());

        // Does not cover: whether the handler honours the resolved value. That
        // is the worker's dispatch path, asserted in its own tests.
    }

    #[test]
    fn every_operational_worker_value_reads_flag_then_overlay_then_default() {
        // One assertion per tier on the SAME setting, because a test that only
        // checks the flag cannot tell a working overlay walk from a resolver
        // that ignores the overlay entirely.
        let overlay: toml::Value = toml::from_str(
            "[worker]\nport = 9999\nmax_isolates = 12\n[observability]\nlog_filter = \"warn\"\n",
        )
        .expect("overlay");

        let defaults = WorkerSettings::resolve_config(
            WorkerSettingsSources::try_parse_from(["zeroship-worker"]).expect("bare parse"),
            None,
        )
        .expect("settings resolve");
        assert_eq!(*defaults.port.get(), 8080);
        assert_eq!(*defaults.max_isolates.get(), 200);
        assert_eq!(*defaults.threads.get(), default_worker_threads());

        let overlaid = WorkerSettings::resolve_config(
            WorkerSettingsSources::try_parse_from(["zeroship-worker"]).expect("bare parse"),
            Some(&overlay),
        )
        .expect("settings resolve");
        assert_eq!(*overlaid.port.get(), 9999);
        assert_eq!(*overlaid.max_isolates.get(), 12);
        assert_eq!(overlaid.log_filter.get(), "warn");

        let flagged = WorkerSettings::resolve_config(
            WorkerSettingsSources::try_parse_from(["zeroship-worker", "--port", "7000"])
                .expect("flag parse"),
            Some(&overlay),
        )
        .expect("settings resolve");
        assert_eq!(*flagged.port.get(), 7000, "the flag must win over the overlay");
        assert_eq!(*flagged.max_isolates.get(), 12);
    }

    #[test]
    fn the_platform_global_settings_project_without_a_worker_prefix() {
        // `blob_store`, `control_url` and `poll_interval` are declared by more
        // than one binary, so their canonical names carry NO component prefix
        // and the environment spelling has no `WORKER_` in it. Getting this
        // wrong is invisible at run time: each binary would simply read a
        // different variable and silently keep its default.
        let envs = WorkerSettingsSources::command()
            .get_arguments()
            .filter_map(|arg| {
                arg.get_env()
                    .map(|env| (arg.get_id().to_string(), env.to_string_lossy().into_owned()))
            })
            .collect::<Vec<_>>();
        for (id, env) in &envs {
            assert!(env.starts_with("ZEROSHIP_"), "{id} projects {env}");
        }
        let find = |id: &str| {
            envs.iter()
                .find(|(name, _)| name == id)
                .map(|(_, env)| env.clone())
        };
        assert_eq!(find("blob_store"), Some("ZEROSHIP_BLOB_STORE".to_owned()));
        assert_eq!(find("control_url"), Some("ZEROSHIP_CONTROL_URL".to_owned()));
        assert_eq!(find("poll_interval"), Some("ZEROSHIP_POLL_INTERVAL".to_owned()));
        // The one-variable control: a worker-scoped identity DOES carry the
        // prefix, so this is not merely asserting that everything is unprefixed.
        assert_eq!(find("max_isolates"), Some("ZEROSHIP_WORKER_MAX_ISOLATES".to_owned()));
    }
}
