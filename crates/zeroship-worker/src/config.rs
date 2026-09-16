//! Worker's generated settings: bootstrap, command, observability, operational.
//!
//! In the library rather than `main.rs` so the compiled configuration checker
//! can link the declaration and invoke clap's `CommandFactory` against it.
//!
//! Every input the worker takes is declared here, credentials included. A
//! `Secret<T>` field generates only a `--<name>-file PATH` flag, so no worker
//! credential can reach argv; the value tiers are the canonical `ZEROSHIP_*`
//! environment name. The worker deliberately has no TOML overlay source.

use std::path::{Path, PathBuf};

use zeroship_core::config::{
    zeroship_config, CheckFormat, CommandControl, ObservabilityControls,
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

/// Resolve the worker's KV configuration, requiring shared storage when enabled.
pub fn open_kv_store(input: &str) -> Result<Option<zeroship_kv::KvStore>, zeroship_kv::KvError> {
    if input.is_empty() {
        return Ok(None);
    }
    let config = zeroship_kv::KvConfig::from_toml(input)?;
    if !matches!(config, zeroship_kv::KvConfig::Redis { .. }) {
        return Err(zeroship_kv::KvError::invalid_argument(
            "worker KV requires shared Redis-compatible storage",
        ));
    }
    zeroship_kv::KvStore::open(&config).map(Some)
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

    /// FILE holding the JOIN TOKEN this worker presents at boot.
    ///
    /// The one thing a worker carries, and it is not a signing key: it is a JWT
    /// a trusted signer minted, naming a zone, an expiry and a use budget. The
    /// worker cannot mint anything with it, and presenting it without the
    /// keypair this process draws in memory registers nothing - the join request
    /// is signed by that key. Every assertion after the join is minted under
    /// that instance key. No worker holds a `svc/worker` role key.
    /// `crates/zeroship-core/src/worker_join.rs` carries the token and proof
    /// formats, and `docs/runbooks/worker-join-signers.md` the operator
    /// procedure.
    ///
    /// The token is read FRESH AT EVERY BOOT, so a single-host deployment can
    /// have its control plane rotate the file and a container restarted days
    /// later still gets a token minted minutes ago.
    ///
    /// A PATH, not a `Secret<String>`: the loader refuses a group- or
    /// world-readable file, which is not possible once the material has become
    /// an in-memory `String`.
    ///
    /// Empty (the default) REFUSES THE BOOT. A worker that cannot join has no
    /// identity to verify dispatch or read an app's environment with, so it
    /// would bind its port, pass a liveness probe and turn away every request
    /// that reached it - a failure first visible to an end user.
    #[config(name = "worker.join_token_file", default = PathBuf::new())]
    pub join_token_file: Operational<PathBuf>,

    /// JWKS-shaped FILE holding the public key of every peer service.
    ///
    /// One document is handed to every service. `crates/zeroship-core/src/service_peers.rs`
    /// carries the shape, why the keys are configured rather than fetched from
    /// a peer, and why a shared document grants nothing beyond the ability to
    /// check a signature.
    #[config(name = "worker.service_peers_file", default = PathBuf::new())]
    pub service_peers_file: Operational<PathBuf>,

    /// `PostgreSQL` DSN for runtime env/db state. A DSN grammar admits userinfo,
    /// so it is secret-classed whether or not a given value carries a password.
    #[config(name = "worker.database_url")]
    pub database_url: Secret<String>,

    /// TOML KV deployment configuration, supplied as secret material because it
    /// can contain data-server and Sentinel credentials. Distributed workers
    /// require a shared Redis-compatible deployment; an absent configuration
    /// leaves env.kv unavailable.
    #[config(name = "worker.kv_config")]
    pub kv_config: Secret<String>,

    /// `EnvFilter` directive for the tracing subscriber.
    #[config(shared = OBSERVABILITY_LOG_FILTER, default = DEFAULT_LOG_FILTER.to_owned())]
    pub log_filter: Operational<String>,

    /// Tracing output format; `auto` picks pretty on a TTY and json otherwise.
    #[config(shared = OBSERVABILITY_LOG_FORMAT, default = LogFormat::Auto)]
    pub log_format: Operational<LogFormat>,

    /// TLS endpoint of the PostgreSQL CDC relay.
    #[config(name = "worker.cdc_relay_url", default = String::new())]
    pub cdc_relay_url: Operational<String>,

    /// Private certificate authority for the CDC relay; empty uses host trust.
    #[config(name = "worker.cdc_relay_ca_file", default = PathBuf::new())]
    pub cdc_relay_ca_file: Operational<PathBuf>,

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

    /// Origin of the workflow manager (`zeroship-workflow-server`) this
    /// worker registers with and consumes delivered jobs from.
    ///
    /// Empty (the default) runs no workflow host: every app's `env.workflows`
    /// call is refused as retryable, and nothing falls back to Control. When
    /// set, the worker also requires `worker.database_url` for creator
    /// journals and `worker.storage_url` for workflow payloads, and refuses
    /// to start without them. Remote managers must use HTTPS; plain HTTP is
    /// accepted only for literal loopback addresses.
    #[config(name = "worker.workflow_manager_url", default = String::new())]
    pub workflow_manager_url: Operational<String>,

    /// App placements this worker advertises to the workflow manager.
    #[config(name = "worker.workflow_capacity", default = 64)]
    pub workflow_capacity: Operational<usize>,

    /// Delivered workflow jobs this worker executes at once.
    #[config(name = "worker.workflow_slots", default = 4)]
    pub workflow_slots: Operational<usize>,

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
    fn kv_configuration_accepts_shared_topologies_and_rejects_local_storage() {
        assert!(super::open_kv_store("").unwrap().is_none());
        assert!(super::open_kv_store("backend = 'redb'\npath = 'unused.redb'").is_err());
        for topology in [
            "mode = 'standalone'\nendpoint = 'localhost:6379'",
            "mode = 'cluster'\nseeds = ['redis-a:6379', 'redis-b:6379']",
            "mode = 'sentinel'\nendpoints = ['sentinel:26379']\nservice_name = 'kv'",
        ] {
            let input = format!("backend = 'redis'\n[redis.topology]\n{topology}");
            assert!(super::open_kv_store(&input).unwrap().is_some());
        }
    }

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
        // The usage-stream settings must be settable as flags, not only through
        // ambient env vars. Asserted on the COMMAND rather than on the struct: a
        // field that resolves correctly but projects no flag is the state this
        // guards.
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
