//! Worker's generated settings: bootstrap, command, observability, operational.
//!
//! In the library rather than `main.rs` so the compiled configuration checker
//! can link the declaration and invoke clap's `CommandFactory` against it.
//!
//! Every input the worker takes is declared here, credentials included. A
//! `Secret<T>` field generates only a `--<name>-file PATH` flag, so no worker
//! credential can reach argv; the value tiers are the canonical `ZEROSHIP_*`
//! environment name. The worker deliberately has no TOML overlay source.

use std::path::Path;

use zeroship_core::config::{
    zeroship_config, BootstrapControl, CheckFormat, CommandControl, ObservabilityControls,
    Operational, OverlaySelector, Secret,
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
    /// Validate config (CLI + overlay + guards) and print the resolved non-secret
    /// config, then exit without starting the server.
    #[config(shared = CHECK_CONFIG)]
    pub check_config: CommandControl<bool>,

    /// Output format for `--check-config`.
    #[config(shared = CHECK_CONFIG_FORMAT, default = CheckFormat::Text)]
    pub check_config_format: CommandControl<CheckFormat>,

    /// Admin/control API shared secret.
    #[config(shared = CONTROL_KEY)]
    pub control_key: Secret<String>,

    /// Shared secret for the gateway dispatch endpoints.
    ///
    /// It authenticates the dispatch bearer AND keys the per-request
    /// `ZeroShip-User` HMAC, so it carries a 32-byte strength floor rather than
    /// a presence check.
    #[config(shared = WORKER_KEY)]
    pub worker_key: Secret<String>,

    /// `PostgreSQL` DSN for runtime env/db state. A DSN grammar admits userinfo,
    /// so it is secret-classed whether or not a given value carries a password.
    #[config(name = "worker.database_url")]
    pub database_url: Secret<String>,

    /// Redis connection URL for the app `env.kv` namespace.
    ///
    /// Multi-node KV MUST be a SHARED store so a `set` on one worker node is
    /// visible on another - Redis is that store (the bespoke compio-redis
    /// driver; zero tokio). The URL selects single-node
    /// (`redis://host:port`) or cluster (`redis://seed/?cluster=true&seeds=...`)
    /// mode. When unset the `env.kv` namespace is absent (apps using
    /// `@zeroship/kv` then fail loudly rather than silently diverging on a
    /// per-process embedded store). The single-tenant CLI's per-process `redb`
    /// backend is deliberately NOT used here - it cannot stay consistent across
    /// a worker fleet.
    ///
    /// It may embed `redis://user:pass@host`, which is why it is secret-classed
    /// rather than operational.
    #[config(name = "worker.kv_url")]
    pub kv_url: Secret<String>,

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

    /// Kafka-wire brokers for the usage-event stream, e.g. `redpanda:9092`.
    ///
    /// EMPTY DISABLES THE PRODUCER, and that is a supported deployment: every
    /// non-billing e2e harness and `zeroship dev` run a worker that meters
    /// nothing. What the empty value must never be is INVISIBLE - the boot log
    /// says so in as many words and `--check-config` reports
    /// `usage_stream_configured=false`, because a worker that drains and drops
    /// billable usage looks exactly like a worker with no traffic.
    ///
    /// Operational, not `Secret`: the value is a host:port list an operator
    /// reads out of their broker's own dashboard. Kafka credentials, if this
    /// ever grows SASL, would be a separate secret-classed declaration with a
    /// `--<name>-file` flag - not a password smuggled into this string.
    #[config(shared = METERING_BROKERS, default = String::new())]
    pub metering_brokers: Operational<String>,

    /// Topic the usage-event producer publishes to.
    #[config(shared = METERING_EVENTS_TOPIC,
             default = zeroship_metering::DEFAULT_USAGE_EVENTS_TOPIC.to_owned())]
    pub metering_events_topic: Operational<String>,

    /// Consumer-group id override for the producer. Empty derives one from the
    /// per-boot producer source.
    #[config(shared = METERING_PRODUCER_GROUP_ID, default = String::new())]
    pub metering_producer_group_id: Operational<String>,

    /// redb write-ahead-log path for the usage outbox. Empty derives one from
    /// the stable per-host WAL identity.
    ///
    /// PER PROCESS, never shared: redb is single-writer, so a worker and a
    /// gateway on one host that name the same file leave the second producer
    /// unable to build an outbox at all - which the worker treats as fatal.
    #[config(shared = METERING_OUTBOX_WAL_PATH, default = String::new())]
    pub metering_outbox_wal_path: Operational<String>,
}

/// The worker's usage-stream producer settings, resolved.
///
/// A free function on [`WorkerSettings`] rather than four reads at the boot
/// site, so `--check-config` and the producer answer from ONE expression. They
/// did not before: the report re-read `REDPANDA_BROKERS` from the environment
/// while the producer went through `UsageStreamSettings::from_env`, and the two
/// agreed only for as long as the environment was the sole channel.
#[must_use]
pub fn usage_stream_settings(settings: &WorkerSettings) -> zeroship_metering::UsageStreamSettings {
    zeroship_metering::UsageStreamSettings::from_resolved(
        settings.metering_brokers.get(),
        settings.metering_events_topic.get(),
        settings.metering_producer_group_id.get(),
        settings.metering_outbox_wal_path.get(),
    )
}

impl OverlaySelector for WorkerSettingsSources {
    fn overlay_path(&self) -> Option<&Path> {
        None
    }

    fn allow_discovery(&self) -> bool {
        false
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
    use zeroship_core::config::{GeneratedConfig, OverlaySelector};

    use super::{default_worker_threads, WorkerSettings, WorkerSettingsSources};

    #[test]
    fn worker_cannot_select_or_discover_a_shared_overlay() {
        let command = WorkerSettingsSources::command();
        let longs = command
            .get_arguments()
            .filter_map(clap::Arg::get_long)
            .collect::<Vec<_>>();
        assert!(!longs.contains(&"config"), "{longs:?}");
        assert!(!longs.contains(&"no-config"), "{longs:?}");

        let sources =
            WorkerSettingsSources::try_parse_from(["zeroship-worker"]).expect("bare parse");
        assert!(sources.overlay_path().is_none());
        assert!(!sources.allow_discovery());
    }

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

    #[test]
    fn the_usage_stream_has_flags_and_is_not_environment_only() {
        // REGRESSION. Until 2026-08-20 the worker's usage-stream settings had
        // NO flag: `UsageStreamSettings::from_env` was the single channel for
        // `REDPANDA_BROKERS` and `USAGE_EVENTS_TOPIC`, and 9b205f6ed had
        // already removed the `--config` overlay that was the other one. Nine
        // e2e harnesses were left setting ambient variables on the worker's
        // command prefix because the product offered nothing else. Asserted on
        // the COMMAND rather than on the struct: a field that resolves
        // correctly but projects no flag is exactly the state being fixed.
        let command = WorkerSettingsSources::command();
        let longs = command
            .get_arguments()
            .filter_map(clap::Arg::get_long)
            .collect::<Vec<_>>();
        for expected in [
            "metering-brokers",
            "metering-events-topic",
            "metering-producer-group-id",
            "metering-outbox-wal-path",
        ] {
            assert!(longs.contains(&expected), "no --{expected} in {longs:?}");
        }

        // Each carries the ZEROSHIP_* twin every other flag in this tree has,
        // and the names are UNPREFIXED: one stream serves the whole deployment,
        // so `ZEROSHIP_WORKER_METERING_BROKERS` would be the wrong shape.
        let env_of = |id: &str| {
            WorkerSettingsSources::command()
                .get_arguments()
                .find(|arg| arg.get_id() == id)
                .and_then(clap::Arg::get_env)
                .map(|env| env.to_string_lossy().into_owned())
        };
        assert_eq!(
            env_of("metering_brokers"),
            Some("ZEROSHIP_METERING_BROKERS".to_owned())
        );
        assert_eq!(
            env_of("metering_events_topic"),
            Some("ZEROSHIP_METERING_EVENTS_TOPIC".to_owned())
        );
    }

    #[test]
    fn the_flag_and_not_only_the_default_reaches_the_producer_settings() {
        // The one-variable control pair: identical parses but for the flag, so
        // this cannot pass on a resolver that ignores the flag and returns the
        // compiled default, nor on one that always reports a producer.
        let bare = WorkerSettings::resolve_config(
            WorkerSettingsSources::try_parse_from(["zeroship-worker"]).expect("bare parse"),
            None,
        )
        .expect("settings resolve");
        let disabled = super::usage_stream_settings(&bare);
        assert!(
            !disabled.producer_enabled(),
            "a worker told nothing about brokers must report a disabled producer"
        );
        assert_eq!(disabled.effective_topic(), "usage-events");

        let flagged = WorkerSettings::resolve_config(
            WorkerSettingsSources::try_parse_from([
                "zeroship-worker",
                "--metering-brokers",
                "127.0.0.1:19092",
                "--metering-events-topic",
                "usage-e2e",
            ])
            .expect("flag parse"),
            None,
        )
        .expect("settings resolve");
        let enabled = super::usage_stream_settings(&flagged);
        assert!(enabled.producer_enabled());
        assert_eq!(enabled.effective_brokers(), Some("127.0.0.1:19092"));
        assert_eq!(enabled.effective_topic(), "usage-e2e");

        // Does NOT cover whether `main` spawns the outbox from this value; the
        // boot arm is asserted end to end by the metering harnesses, which now
        // fail if the worker log reports a disabled outbox.
    }
}
