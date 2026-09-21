// The connection future of `compio-postgres` carries both transports' split
// halves (`MaybeTlsReadHalf`), and computing the layout of the version-poll
// async block walks that whole type. It lands ~130 deep, just past rustc's
// default of 128. A compile-time budget only - nothing here recurses at run
// time.
#![recursion_limit = "256"]

mod join;

use zeroship_worker::{cache, handler, health, logs, metrics, sync, WorkerConfig};

use clap::Parser;
use ntex::web;
use std::sync::{Arc, RwLock};
use zeroship_bundle::{
    build_blob_store, BlobStore, StoreUrl,
};
use zeroship_core::config::{
    audit_credentials, bootstrap_or_exit, mark_dev_escape_active, require_nonempty, BuildProfile,
    CheckConfigReport, CheckValue, CredentialPosture, CredentialVerdict, SubsystemCredential,
};
use zeroship_runtime::init::init_v8;
use zeroship_storage::StorageBackendConfig;
use zeroship_worker::config::{WorkerSettings, WorkerSettingsConsumer, WorkerSettingsSources};

use crate::sync::{SharedEnvs, SharedVersions};

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// Operator-facing spelling of the control key, for a diagnostic that has to
/// name something the operator can actually set.
const CONTROL_KEY_LABEL: &str = "ZEROSHIP_CONTROL_KEY / --control-key-file";

/// The listen backlog for this worker's HTTP socket.
///
/// Declared here because the listener is created BEFORE the server is - see the
/// bind site in `main`, and the reason it has to be. `HttpServer::backlog` is
/// what would otherwise carry it; this binary never called it, so this is the
/// same declaration moved to the one place that now needs it.
const WORKER_LISTEN_BACKLOG: i32 = 1024;

/// Every credential the worker needs, tagged by the subsystem that needs it.
///
/// ONE, and unconditional: a worker that cannot poll control for versions is a
/// worker with nothing to do. The JOIN TOKEN is NOT here and is not an
/// omission - it is the file loaded by [`load_join_material`], which refuses a
/// file it cannot read or that other local users can, checks this audit cannot
/// express and a strength floor on a shared string cannot replace.
/// [`zeroship_core::config::audit_credentials`] handles the three material
/// cases, so a `--check-config` run still never judges a secret it deliberately
/// did not read.
fn worker_credentials(
    settings: &zeroship_worker::config::WorkerSettings,
) -> Vec<SubsystemCredential<'_>> {
    vec![SubsystemCredential {
        subsystem: "control-version-poll",
        enabled: true,
        label: CONTROL_KEY_LABEL,
        secret: &settings.control_key,
        validate: require_nonempty,
    }]
}

/// Apply the boot gate, or exit. See `crates/zeroship-gateway/src/main.rs` for the shape;
/// it is deliberately identical across services so an operator reads one banner.
fn enforce_worker_credentials(
    settings: &zeroship_worker::config::WorkerSettings,
    overlay: &zeroship_core::config::ConfigSource,
    check_config: bool,
) -> CredentialPosture {
    let posture = audit_credentials(&worker_credentials(settings));
    let verdict = posture.verdict(BuildProfile::current(), check_config);
    if let Some(banner) = posture.banner("zeroship-worker", overlay, verdict) {
        eprint!("{banner}");
        tracing::error!(
            subsystems = %posture
                .weak()
                .iter()
                .map(|weak| weak.subsystem)
                .collect::<Vec<_>>()
                .join(","),
            "worker: unconfigured service credential"
        );
    }
    match verdict {
        CredentialVerdict::Proceed => posture,
        CredentialVerdict::Refuse => std::process::exit(1),
        CredentialVerdict::DevEscape => {
            mark_dev_escape_active();
            posture
        }
    }
}

/// Workers accept PostgreSQL only. Absence is handled separately so an
/// auth-only worker and a configuration dry run need no database connection.
fn worker_rejects_db_url(db_url: &str) -> bool {
    let normalized = db_url.trim().to_ascii_lowercase();
    !normalized.is_empty()
        && !normalized.starts_with("postgres://")
        && !normalized.starts_with("postgresql://")
}

/// A workflow host prepares creator journals in the worker's own database and
/// stages payloads in its own object store, so it cannot run without either.
/// Refusing the boot beats a host that registers capacity it can never use.
fn workflow_host_prerequisites(database: bool, storage: bool) -> Result<(), &'static str> {
    if !database {
        return Err("worker.workflow_manager_url requires worker.database_url: creator \
                    workflow journals live in the app database");
    }
    if !storage {
        return Err("worker.workflow_manager_url requires worker.storage_url: workflow \
                    payloads live in the app object store");
    }
    Ok(())
}

/// Load the JOIN TOKEN and peer document this worker boots with, or refuse to
/// start.
///
/// NEITHER IS A SIGNING KEY. The token is a JWT a trusted signer minted; this
/// process cannot mint anything with it and cannot even verify it. What serves
/// is the INSTANCE identity `crate::join::join` exchanges it for, once, after
/// the port is bound - and the keypair behind that identity is drawn in memory
/// at that moment, so a worker holds no private half on disk at all. No
/// `svc/worker` role key is loaded, because none exists.
///
/// The verifier is the TRANSPORT-ONLY one. The worker is a callee on exactly
/// one edge - the gateway's dispatch hop - and that hop carries every end-user
/// request, so it claims no `jti` and consults no shared store. The worker
/// therefore needs NO database reachability for inbound authentication at all,
/// which is the property the tiering buys and the reason it is stated here
/// rather than left implicit in a missing argument.
///
/// EVERY OUTCOME BUT ONE IS AN EXIT, and that is fence F4 in full: "absent a
/// configured gateway public key the worker refuses to start". An unconfigured,
/// unreadable, insecurely permissioned or unparseable join token or peer
/// document, and a peer document missing the gateway key, are one fate,
/// because from the outside they produce one behaviour - a worker that binds
/// its port, passes a liveness probe and turns away every request that reaches
/// it.
///
/// The unconfigured token file is refused by `crate::join::read_join_token`
/// rather than by a branch here, so no future edit of this function can restore
/// the escape by giving the empty path its own arm.
///
/// The same peer document also supplies the GATEWAY's public key for the
/// `ZeroShip-User` identity envelope, and a document that omits it is a HARD
/// STOP rather than a warning: an empty key would turn the envelope check off
/// and the bearer check with it, so the missing-key branch here is the point,
/// not an edge case.
fn load_join_material(
    join_token_file: &std::path::Path,
    peers_file: &std::path::Path,
) -> crate::join::JoinMaterial {
    use zeroship_core::service_peers::{load_peer_bundle, service_issuer};
    use zeroship_core::user_envelope::UserEnvelopeVerifier;

    let role = match service_issuer(zeroship_core::service_peers::WORKER_SERVICE_NAME) {
        Ok(issuer) => issuer,
        Err(error) => {
            tracing::error!(%error, "worker: refusing to start - worker service issuer is malformed");
            std::process::exit(1);
        }
    };
    let gateway = match service_issuer(zeroship_core::service_peers::GATEWAY_SERVICE_NAME) {
        Ok(issuer) => issuer,
        Err(error) => {
            tracing::error!(%error, "worker: refusing to start - gateway service issuer is malformed");
            std::process::exit(1);
        }
    };
    let token = match crate::join::read_join_token(join_token_file) {
        Ok(token) => token,
        Err(error) => {
            tracing::error!(
                %error,
                "worker: refusing to start - join token rejected; set worker.join_token_file"
            );
            std::process::exit(1);
        }
    };
    let bundle = match load_peer_bundle(peers_file) {
        Ok(bundle) => bundle,
        Err(error) => {
            tracing::error!(
                %error,
                "worker: refusing to start - peer document rejected; set \
                 worker.service_peers_file"
            );
            std::process::exit(1);
        }
    };
    // Built HERE, while the material is being read, and fatal if it cannot be.
    // A worker that came up without it would verify the dispatch hop and then
    // have no way to check who the request is FOR - and the only shapes
    // available then are "trust the header" or "drop every user to anonymous",
    // one unsafe and one silently wrong.
    //
    // It is deliberately NOT deferred to the point where the instance identity
    // is assembled. Deferring it would make F4's gateway-key refusal conditional
    // on the control plane being reachable, which is the same defect the comment
    // at this function's call site records about the database.
    let user_envelope = match UserEnvelopeVerifier::for_issuer(&bundle, &gateway) {
        Ok(verifier) => verifier,
        Err(error) => {
            tracing::error!(
                %error,
                "worker: refusing to start - the peer document must publish the gateway's \
                 public key, which is what identity envelopes are verified under"
            );
            std::process::exit(1);
        }
    };
    crate::join::JoinMaterial::new(token, bundle, user_envelope, role)
}

fn main() -> std::io::Result<()> {
    let (settings, boot) = bootstrap_or_exit::<WorkerSettings>(
        WorkerSettingsSources::parse(),
        "info,zeroship_worker=debug,zeroship_runtime=info",
        "worker",
    );
    let check_config = *settings.check_config.get();

    let port = *settings.port.get();
    let workers_count = *settings.threads.get();
    let control_url = settings.control_url.get().clone();
    // Secret material, already resolved by the generated resolver from the
    // `-file` flag, the canonical environment name, or the canonical TOML path -
    // in that order. Under `--check-config` a source needing I/O yields no
    // material at all, and these locals are then empty by construction; the
    // report below asks `is_configured()` and the boot path is not reached.
    let control_key = settings.control_key.expose_str().to_owned();
    let max_isolates = *settings.max_isolates.get();
    let poll_interval =
        match crate::sync::validate_poll_interval_secs(*settings.poll_interval.get()) {
            Ok(secs) => secs,
            Err(message) => {
                tracing::error!("worker: {message}");
                std::process::exit(2);
            }
        };
    let db_url = settings.database_url.expose_str().to_owned();
    let shutdown_timeout = *settings.shutdown_timeout.get();
    let blob_store_root = settings.blob_store.get().clone();
    // `s3://…` → remote S3 store, bare path → local disk (dev default).
    // Validated now so a bad `s3://` URL fails fast.
    let store_url = match StoreUrl::parse(&blob_store_root) {
        Ok(u) => u,
        Err(e) => {
            eprintln!("worker: invalid --blob-store: {e}");
            std::process::exit(2);
        }
    };
    let blob_store_is_remote = store_url.is_remote();
    // KV configuration may contain credentials, so it is secret-classed like
    // the DSNs and reaches the process the same way.
    let kv_config = settings.kv_config.expose_str().to_owned();
    // `env.storage` backend. Empty ⇒ namespace absent. A bare path/`file://`
    // is `LocalFs`; `s3://…` is the S3 backend. Validated now (parse only —
    // S3 credentials are resolved when the plugin is built per worker thread)
    // so a malformed `s3://` URL fails fast.
    let storage_raw = settings.storage_url.get().clone();
    let storage_backend = if storage_raw.is_empty() {
        None
    } else {
        match StorageBackendConfig::parse(&storage_raw) {
            Ok(c) => Some(c),
            Err(e) => {
                eprintln!("worker: invalid --storage-url: {e}");
                std::process::exit(2);
            }
        }
    };
    let bind_host = settings.bind.get().clone();
    let socket_path = settings.socket.get().clone();
    // The workflow host is optional: without a manager origin no host runs
    // and `env.workflows` refuses every app. A configured one must be usable.
    let workflow_manager_url = settings.workflow_manager_url.get().clone();
    let workflow_host_config = (!workflow_manager_url.is_empty()).then(|| {
        zeroship_worker::workflow_host::WorkflowHostConfig {
            manager_url: workflow_manager_url.clone(),
            capacity: *settings.workflow_capacity.get(),
            slots: *settings.workflow_slots.get(),
        }
    });
    if let Some(config) = &workflow_host_config {
        if let Err(message) = config.validate() {
            tracing::error!("worker: {message}");
            std::process::exit(2);
        }
        if let Err(message) = workflow_host_prerequisites(
            settings.database_url.is_configured(),
            storage_backend.is_some(),
        ) {
            tracing::error!("worker: {message}");
            std::process::exit(1);
        }
    }

    // THE BOOT GATE, before the bind guard below and before the
    // `--check-config` report, so a dry run over a placeholder credential exits
    // non-zero. The credential strength check runs here, before the bind guard:
    // the two are independent refusals, and only the order in which a
    // doubly-misconfigured launch reports changes.
    let credentials = enforce_worker_credentials(&settings, &boot.overlay.source, check_config);

    // SQLite belongs to local `zeroship serve`. Require a PostgreSQL selector
    // here rather than treating an invalid SQLite selector as another backend.
    // A configuration dry run deliberately leaves secret files unread.
    if worker_rejects_db_url(&db_url) {
        tracing::error!(
            "worker: ZEROSHIP_WORKER_DATABASE_URL must select PostgreSQL; \
             file-backed SQLite belongs to local `zeroship serve`"
        );
        std::process::exit(1);
    }

    if check_config {
        let log_format = boot.log_format.to_string();
        let mut report = CheckConfigReport::new();
        report.field("bind", CheckValue::Plain(bind_host.clone()));
        report.field("port", CheckValue::Count(usize::from(port)));
        report.field(
            "config_source",
            CheckValue::Plain(boot.overlay.source.to_string()),
        );
        report.field("control_url", CheckValue::Plain(control_url.clone()));
        // Presence only: the token is a bearer credential, and a dry run does
        // not read secret material.
        report.field(
            "join_token_file_configured",
            CheckValue::Flag(!settings.join_token_file.get().as_os_str().is_empty()),
        );
        report.field("worker_threads", CheckValue::Count(workers_count));
        report.field("max_isolates", CheckValue::Count(max_isolates));
        report.field(
            "poll_interval_secs",
            CheckValue::Count(usize::try_from(poll_interval).unwrap_or(usize::MAX)),
        );
        report.field(
            "shutdown_timeout_secs",
            CheckValue::Count(usize::try_from(shutdown_timeout).unwrap_or(usize::MAX)),
        );
        report.field("log_filter", CheckValue::Plain(boot.log_filter.clone()));
        report.field("log_format", CheckValue::Plain(log_format));
        report.field("blob_store", CheckValue::Plain(blob_store_root.clone()));
        report.field("blob_store_remote", CheckValue::Flag(blob_store_is_remote));
        report.field(
            "socket_configured",
            CheckValue::Flag(!socket_path.is_empty()),
        );
        report.field(
            "workflow_host_configured",
            CheckValue::Flag(workflow_host_config.is_some()),
        );
        report.field(
            "workflow_manager_url",
            CheckValue::Plain(workflow_manager_url.clone()),
        );
        report.field(
            "workflow_capacity",
            CheckValue::Count(*settings.workflow_capacity.get()),
        );
        report.field(
            "workflow_slots",
            CheckValue::Count(*settings.workflow_slots.get()),
        );
        // Both are reported by PRESENCE, which is all a resolved `Secret<T>`
        // will answer: under `--check-config` a file-sourced secret has no
        // material, so an emptiness test would read "unset" for a correctly
        // configured deployment.
        report.field(
            "db_configured",
            CheckValue::Secret(settings.database_url.is_configured()),
        );
        // Surface the kernel-namespace wiring without leaking the KV URL
        // (it may carry credentials) - presence only, like `db_configured`.
        report.field(
            "kv_configured",
            CheckValue::Secret(settings.kv_config.is_configured()),
        );
        report.field(
            "storage_configured",
            CheckValue::Flag(storage_backend.is_some()),
        );
        report.field(
            "storage_kind",
            CheckValue::Plain(
                storage_backend
                    .as_ref()
                    .map_or("absent", StorageBackendConfig::kind)
                    .to_string(),
            ),
        );
        report.field(
            "storage_remote",
            CheckValue::Flag(
                storage_backend
                    .as_ref()
                    .is_some_and(StorageBackendConfig::is_remote),
            ),
        );
        // Both from the RESOLVED settings, which is the same expression the
        // producer boots from, so the report cannot disagree with the producer.
        // This field is the surface a harness asserts the producer on BEFORE it
        // launches anything, so it disagreeing is worse than useless.
        let usage_stream = zeroship_worker::config::usage_stream_settings(&settings);
        report.field(
            "usage_stream_configured",
            CheckValue::Flag(usage_stream.producer_enabled()),
        );
        report.field(
            "usage_events_topic",
            CheckValue::Plain(usage_stream.effective_topic().to_string()),
        );
        // The credential posture, plus how much of it was measured. A reported
        // field can be truthful while nothing reads it, which is why the
        // posture rides the EXIT CODE as well as this report.
        report.field(
            "service_credentials",
            CheckValue::Plain(credentials.summary().to_string()),
        );
        report.field(
            "service_credentials_checked",
            CheckValue::Count(credentials.checked()),
        );
        report.field(
            "service_credentials_skipped",
            CheckValue::Count(credentials.skipped()),
        );
        report.field(
            "service_credentials_unread",
            CheckValue::Count(credentials.unread()),
        );

        report.emit(*settings.check_config_format.get());
        return Ok(());
    }

    // THE KEY-MATERIAL REFUSAL, and it runs HERE - before the runtime starts and
    // before the database posture check - because fence F4 must not be
    // conditional on an unrelated subsystem being reachable. Building it after
    // `db_posture::validate_database_url` CONNECTS would report the database
    // first, so the operator would fix Postgres and only then learn about the
    // key material; worse, it would make the one fence the worker cannot serve a
    // request without dependent on the one subsystem the tiering was chosen to
    // keep it independent of - the dispatch hop claims no `jti` precisely so
    // inbound authentication needs no database at all. Reading two files needs
    // no async runtime, so nothing is lost by doing it first.
    let join_material = load_join_material(
        settings.join_token_file.get(),
        settings.service_peers_file.get(),
    );

    ntex::rt::System::build()
        .name("zeroship-worker")
        .build(ntex::rt::DefaultRuntime)
        .block_on(async move {
    if let Err(error) = zeroship_worker::db_posture::validate_database_url(&db_url).await {
        tracing::error!(%error, "worker: refusing unsafe database authority");
        std::process::exit(1);
    }
    init_v8();

    // Read here rather than inside `zeroship-bundle`, so the record names the
    // worker as the reader. Only a remote store needs credentials.
    let s3_runtime = store_url.is_remote().then(|| {
        zeroship_core::resolve_s3_runtime!(zeroship_worker::config::WorkerSettingsConsumer)
            .expect("failed to resolve S3 credentials for the blob store")
    });
    let blob_store: Arc<dyn BlobStore> = build_blob_store(&store_url, s3_runtime.as_ref())
        .expect("failed to initialise blob store");
    tracing::info!(
        blob_store_root = %blob_store_root,
        blob_store_remote = blob_store_is_remote,
        "worker blob store configured"
    );

    let kv_store = zeroship_worker::config::open_kv_store(&kv_config).unwrap_or_else(|error| {
        eprintln!("worker: KV backend init failed: {error}");
        std::process::exit(1);
    });
    // Resolve S3 credentials NOW (fail fast) for a remote storage backend, so
    // a misconfigured worker refuses to start rather than degrading the
    // namespace silently per thread.
    if let Some(cfg) = &storage_backend {
        if cfg.is_remote() {
            if let Err(e) = zeroship_storage::StorageStore::open(cfg) {
                eprintln!("worker: --storage-url s3 backend init failed: {e}");
                std::process::exit(1);
            }
        }
    }
    // Announce the resolved app-kernel namespace surface so a deployment
    // that forgot to wire kv/storage is visible in the worker's boot log
    // (rather than only surfacing as a runtime "env.kv is undefined" in a
    // creator app). `auth` is always on; `db`/`kv`/`storage` track config.
    tracing::info!(
        db = !db_url.is_empty(),
        kv = kv_store.is_some(),
        storage = storage_backend.is_some(),
        storage_kind = storage_backend.as_ref().map_or("absent", StorageBackendConfig::kind),
        auth = true,
        "worker app-kernel namespaces"
    );

    let db_url_opt = if db_url.is_empty() { None } else { Some(db_url) };
    let bind_addr = format!("{bind_host}:{port}");

    // Shared version snapshot populated by a SINGLE process-wide poller and
    // observed by every ntex worker thread's reconcile loop, so control-plane
    // traffic does not multiply by `workers_count`.
    let shared_versions: SharedVersions = Arc::new(RwLock::new(None));
    // Process-wide env cache (single source of truth across all ntex
    // worker threads). Reconcile loops read+write through it, the
    // dispatch handler reads under a brief read lock + Arc clone.
    let shared_envs: SharedEnvs = Arc::new(RwLock::new(std::collections::HashMap::new()));
    let shared_logs = logs::new_store();

    if !socket_path.is_empty() {
        tracing::info!(socket = %socket_path, "worker also bound to unix socket");
        // Remove stale socket file
        let _ = std::fs::remove_file(&socket_path);
    }

    // ONE readiness state for the whole process: the version poller stamps it
    // on every successful control poll, and every ntex worker thread's
    // `/readyz` reads that same stamp plus the same blob-store gate.
    let readiness = Arc::new(health::WorkerReadiness::new());

    // ── Metering infrastructure ──────────────────────────────────────────
    // ONE process-wide meter, shared with every ntex worker thread's
    // `create_plugins` (via KernelConfig) AND the single stream outbox task
    // spawned here. Metering is infrastructure: there is NO `env.meter`
    // creator API. The worker emits the five platform counters
    // (`record_request`) and the db/kv/storage primitives emit raw usage
    // metrics at their op boundary. The outbox drains the meter every ~10s
    // into `UsageEvent`s and publishes them to the durable stream keyed by app.
    // Class `external`: `HOSTNAME` is the container runtime's / shell's, and
    // in a pod it is the pod name. Nothing zeroship sets it.
    let worker_base = zeroship_core::declared_env!(external, "HOSTNAME", WorkerSettingsConsumer)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| bind_addr.to_string());
    let meter_source = format!("{worker_base}-{}", uuid::Uuid::new_v4());
    let meter = Arc::new(zeroship_metering::Meter::with_source(meter_source.clone()));

    // ── The `env.db` service ─────────────────────────────────────────────
    // ONE `DbService` for the whole process, constructed HERE - before ntex
    // spawns a worker thread and therefore before any V8 isolate exists. It
    // owns the validated configuration, the plugin prototype every runtime
    // clones, the stable thread-resource key, the process-wide live-metadata
    // cache, and the operator-lifecycle handle the version poller deprovisions
    // through. Every worker thread is handed this same `Arc` via `KernelConfig`.
    //
    // The URL is parsed exactly once, right here. A URL naming no supported
    // backend fails the boot rather than surfacing inside the first `env.db`
    // call an app happens to make. That is not a new failure for this binary:
    // the slot reaper below already connects at boot and fails the process when
    // the database is unusable.

    // The producer's four `metering.*` declarations, already resolved. The
    // worker deliberately has no TOML overlay source (a credential boundary),
    // so its tiers are flag then `ZEROSHIP_METERING_*` then the compiled
    // default.
    let stream_settings = zeroship_worker::config::usage_stream_settings(&settings);
    // Keyed on the host, NOT on `meter_source`: the source carries a per-boot
    // uuid so two live producers never share a client id, and naming the WAL
    // after it meant every restart opened a new empty file and orphaned
    // whatever the previous boot had not published.
    let wal = zeroship_metering::wal_identity("worker", &worker_base);
    match zeroship_metering::build_usage_outbox(&meter_source, &wal, &stream_settings) {
        Ok(Some((outbox, outbox_config))) => {
            let topic = outbox.topic().to_string();
            let stream = format!("{outbox:?}");
            zeroship_metering::spawn_outbox_task(
                Arc::clone(&meter),
                outbox,
                outbox_config,
            );
            tracing::info!(
                meter_source = %meter_source,
                topic = %topic,
                stream = %stream,
                "metering usage-event outbox started"
            );
        }
        // NOT a refusal, and the choice is load-bearing. A worker with no
        // brokers is a supported deployment - `zeroship dev` and every
        // non-billing e2e harness run one - so exiting here would break far
        // more launches than it protects. What it must not be is INVISIBLE:
        // this arm drains the meter and DROPS every event for the life of the
        // process, which on a billing rail is indistinguishable from an app
        // that served no traffic. So the disabled state is announced with a
        // fixed phrase a harness can assert on, and `--check-config` reports
        // `usage_stream_configured=false` before a byte is served.
        Ok(None) => {
            zeroship_metering::spawn_disabled_drain_task(
                Arc::clone(&meter),
                zeroship_metering::DEFAULT_OUTBOX_INTERVAL,
                "no --metering-brokers / ZEROSHIP_METERING_BROKERS".to_string(),
            );
        }
        // FATAL, not a warning. Brokers are configured, so the operator
        // intends this worker to bill; the common cause on a stable WAL path
        // is a co-located second producer holding the single-writer redb lock.
        // Degrading to a disabled drain task would drain the meter and DROP
        // every event for the life of the process - permanent total loss
        // standing in for an intermittent partial one. Refusing to boot is the
        // recoverable failure; silent free hosting is not.
        Err(error) => {
            tracing::error!(
                meter_source = %meter_source,
                wal = %wal.as_str(),
                error = %error,
                "usage outbox could not be built; refusing to boot rather than \
                 dropping billable usage"
            );
            return Err(std::io::Error::other(format!(
                "usage outbox could not be built (wal={}): {error}",
                wal.as_str()
            )));
        }
    }

    // ── THE PORT, THEN THE IDENTITY ──────────────────────────────────────
    //
    // The listener is created HERE, eagerly, and handed to ntex below instead
    // of letting `HttpServer::bind` create it. The order is the point: the join
    // ADVERTISES this port to control and control writes a row carrying it.
    // Joining before the socket exists would let a bind failure leave a
    // live-looking registry entry pointing at a port nothing listens on, for as
    // long as the instance lease runs. `bind` is reachable only through the
    // server builder, and the server cannot be built until the identity the
    // join returns is in hand - so the bind moves out here rather than the join
    // moving earlier. Socket options match what `HttpServer::bind` would have
    // applied: `ntex::server::bind_addr` is the same function it calls.
    let listeners = match ntex::server::bind_addr(&bind_addr, WORKER_LISTEN_BACKLOG) {
        Ok(listeners) => listeners,
        Err(error) => {
            tracing::error!(bind = %bind_addr, %error, "worker: refusing to start - cannot bind");
            return Err(error);
        }
    };

    // EVERY FAILURE HERE REFUSES THE BOOT, and that is the whole of it. A
    // worker that logged this and carried on would have no identity to verify
    // dispatch or read an app with, while control's registry either knows
    // nothing about it or holds a row for a process that never finished
    // starting - and from the outside it would look exactly like a worker that
    // joined, which is the failure shape this platform keeps re-learning.
    //
    // `join` CONSUMES the join material, so the token is spent on this one call
    // and is unreachable afterwards. What comes back mints under
    // `svc/worker/<wkr_id>` and is addressed as `svc/worker`; everything below
    // - every reconcile, every dispatch, the CDC relay, the lease renewals and
    // the retirement at exit - is handed that and only that.
    let service_auth = match join::join(join_material, &control_url, port).await {
        Ok(auth) => Arc::new(auth),
        Err(error) => {
            tracing::error!(
                control_url = %control_url,
                port,
                %error,
                "worker: refusing to start - this process could not join an instance identity"
            );
            std::process::exit(1);
        }
    };

    // THE IDENTITY EXPIRES, so this process renews it for as long as it runs.
    // Started immediately after the join rather than with the other background
    // tasks: the lease is already ticking, and a renewal loop that only starts
    // once the server is up would leave a worker whose boot stalls holding a
    // credential nothing extends.
    //
    // The loop RETURNS when control refuses - the identity lapsed or was
    // retired, and neither can be revived - rather than exiting the process.
    // Killing the process there would drop requests in flight for a credential
    // that is already dead; the refusals those requests then get at control are
    // the honest outcome, and the orchestrator restarts a worker that rejoins
    // with a token this process no longer holds.
    compio::runtime::spawn(join::renew_forever(
        Arc::clone(&service_auth),
        control_url.clone(),
    ))
    .detach();

    let db_service = match db_url_opt.as_deref() {
        Some(url) => Some(
            zeroship_data_v8::service::DbService::new(
                zeroship_data_v8::service::DbServiceConfig {
                    project_keys: Default::default(),
                    app_bindings: Default::default(),
                    connection: zeroship_data_orm::connection::ConnectionFactory::for_app_url(url)
                        .map_err(|error| std::io::Error::other(error.to_string()))?,
                    cdc_relay: Some({
                        let relay = zeroship_data_orm::cdc::relay::RelayConfig::new(
                            settings.cdc_relay_url.get().clone(),
                            Arc::clone(&service_auth),
                        )
                        .map_err(|error| std::io::Error::other(error.to_string()))?;
                        let ca = settings.cdc_relay_ca_file.get();
                        if ca.as_os_str().is_empty() {
                            relay
                        } else {
                            relay.with_ca_file(ca)
                                .map_err(|error| std::io::Error::other(error.to_string()))?
                        }
                    }),
                    meter: Some(Arc::clone(&meter)),
                },
            )
            .map_err(|error| {
                std::io::Error::other(format!("worker database URL is unusable: {error}"))
            })?,
        ),
        None => None,
    };


    // Kept back for the retirement after the server drains: the config itself
    // moves into the server factory below.
    let retirement = (Arc::clone(&service_auth), control_url.clone());

    let config = Arc::new(WorkerConfig {
        service_auth,
        control_url,
        control_key,
        kv_store,
        storage_backend,
        max_isolates,
        poll_interval_secs: poll_interval,
        shutdown_timeout_secs: shutdown_timeout,
        blob_store,
    });

    // The single process-wide version poller. Started after the `env.db`
    // service exists because a deleted app's CDC teardown runs through that
    // service's operator lifecycle handle, and still before `web::server` below
    // spawns any worker thread, so the shared version map is already filling
    // when they come up. Poller also GCs SharedEnvs against the current
    // known-app set, so env entries for deleted apps don't leak forever.
    //
    // It is also the LAST thing before the server that talks to control, and it
    // is downstream of the join - so every service assertion this process mints
    // after joining is the instance's, and the join token is already gone. The
    // poller's own credential is the shared control key
    // (`sync::version_poll_authorization`), and the two service-assertion
    // callers - `fetch_app_version` and `fetch_app_env` - are reachable only
    // from the per-thread reconcile loop, the dispatch handler and the log
    // reader, all of which start when `server.run()` spawns worker threads.
    sync::start_version_poller(
        config.clone(),
        shared_versions.clone(),
        shared_envs.clone(),
        readiness.clone(),
        db_service.clone(),
    );

    // ── THE WORKFLOW HOST ────────────────────────────────────────────────
    //
    // ONE host for the whole process, on its own thread, holding this
    // instance's identity towards the manager. HTTP threads only ever see the
    // `ReadyApps` it publishes into: an app is reachable through
    // `env.workflows` once its assignment's preparation passed its final
    // checks, and not before or after. With no manager configured the
    // registry simply stays empty.
    let workflows = zeroship_workflow_runner::ready::ReadyApps::default();
    let workflow_host = match workflow_host_config {
        Some(host_config) => {
            let resources = zeroship_worker::workflow_host::HostResources {
                service_auth: Arc::clone(&config.service_auth),
                control_url: config.control_url.clone(),
                db_service: db_service
                    .clone()
                    .expect("the boot refused a workflow host without a database"),
                // The relocation moves this to the service's own schema at its
                // cutover step; until then each app journals beside its tables.
                journal: zeroship_worker::workflow_host::JournalLocation::CreatorSchema,
                storage: config
                    .storage_backend
                    .clone()
                    .expect("the boot refused a workflow host without storage"),
                kv_store: config.kv_store.clone(),
                blob_store: Arc::clone(&config.blob_store),
                meter: Arc::clone(&meter),
                versions: shared_versions.clone(),
                envs: shared_envs.clone(),
            };
            match zeroship_worker::workflow_host::WorkflowHost::start(
                host_config,
                resources,
                workflows.clone(),
            ) {
                Ok(host) => Some(host),
                Err(error) => {
                    tracing::error!(%error, "worker: refusing to start - the workflow host did not start");
                    std::process::exit(1);
                }
            }
        }
        None => {
            tracing::info!("worker: no workflow manager configured; env.workflows refuses every app");
            None
        }
    };

    tracing::info!(
        bind = %bind_addr,
        threads = workers_count,
        max_isolates = config.max_isolates,
        shutdown_timeout_secs = config.shutdown_timeout_secs,
        "worker listening"
    );

    // ntex installs SIGINT/SIGTERM handlers by default; `shutdown_timeout`
    // bounds how long worker threads have to drain in-flight requests
    // before they're force-dropped. Wire our flag through.
    let shutdown_timeout_secs: u16 =
        u16::try_from(config.shutdown_timeout_secs).unwrap_or(u16::MAX);

    let mut server = web::server(async move || {
        let config = config.clone();
        let shared = shared_versions.clone();
        let envs = shared_envs.clone();
        let logs = shared_logs.clone();
        cache::init_cache(
            config.max_isolates,
            cache::KernelConfig {
                workflows: workflows.clone(),
                db_service: db_service.clone(),
                kv_store: config.kv_store.clone(),
                storage_backend: config.storage_backend.clone(),
                // The ONE process-wide meter the usage-event outbox drains.
                meter: Arc::clone(&meter),
            },
        );
        // Per-thread reconcile loop — reads from the shared version map,
        // writes env into the process-wide env cache.
        sync::start_sync(config.clone(), shared, envs.clone());

        web::App::new()
            .state(config)
            .state(envs)
            .state(logs)
            .state(readiness.clone())
            .configure(handler::configure)
            .configure(health::configure)
            .service(web::resource("/logs/{app_id}").route(web::get().to(logs::get_logs)))
            // Prometheus-text metrics. No auth — same policy as `/healthz`,
            // intended for intra-cluster scrapers. Expose behind a side-car
            // or ingress filter if the worker port is ever reachable from
            // outside the cluster.
            .service(web::resource("/metrics").route(web::get().to(|| async {
                web::HttpResponse::Ok()
                    .content_type("text/plain; version=0.0.4; charset=utf-8")
                    .body(metrics::render())
            })))
    })
    .workers(workers_count)
    .shutdown_timeout(ntex::time::Seconds(shutdown_timeout_secs));

    // One listener per resolved address, exactly as `HttpServer::bind` does:
    // `localhost` resolves to both loopback families and both must be served.
    for listener in listeners {
        server = server.listen(listener)?;
    }

    // Also listen on Unix domain socket if configured
    if !socket_path.is_empty() {
        server = server.bind_uds(&socket_path)?;
    }

    // `server.run()` blocks until SIGINT/SIGTERM arrives; ntex then stops
    // accepting, waits up to `shutdown_timeout` for workers to finish
    // serving their current requests, and returns. Detached tasks
    // (fetch body readers, stream drainers) whose futures the pump is
    // polling get one last chance to run during the drain window.
    let server = server.run();

    // A request server must not keep accepting durable work after its
    // workflow host died: stop serving and exit non-zero so the orchestrator
    // replaces this process, which enrols as a new instance.
    let host_failure = std::rc::Rc::new(std::cell::RefCell::new(None::<String>));
    if let Some(host) = &workflow_host {
        let failure = host.failure();
        let failed = host_failure.clone();
        let handle = server.clone();
        ntex::rt::spawn(async move {
            let reason = failure.await;
            tracing::error!(%reason, "worker: the workflow host stopped unexpectedly; stopping");
            *failed.borrow_mut() = Some(reason);
            handle.stop(true).await;
        });
    }

    // `server` resolves once SIGINT/SIGTERM (or a failed host) stopped it and
    // the HTTP drain finished.
    let run_result = server.await;

    // HTTP HAS DRAINED; NOW THE WORKFLOW HOST. No request can resolve an app
    // backend any more, so the host closes its assignment bindings - which
    // withdraws every published backend and revokes its policy generation -
    // reports draining to the manager while delivered executions join, and
    // its thread is joined. Only then does the instance retire, because the
    // host's final manager exchange is signed with the instance key.
    if let Some(host) = workflow_host {
        match host.shutdown().await {
            Ok(()) => tracing::info!("worker: workflow host drained"),
            Err(error) => tracing::warn!(%error, "worker: workflow host drain failed"),
        }
    }

    // THE INSTANCE RETIRES ITSELF, and only here: after the drain, so no
    // request still in flight loses its identity mid-read, and only on the
    // graceful path, because a process that crashed says nothing at all. The
    // server factory does not stop this runtime when it stops, so the call
    // runs on the same thread that joined. A retirement that fails is
    // logged and the exit carries on - the row then stays `active` with no
    // process behind it, which is what a crash leaves.
    let (retirement_auth, retirement_control) = retirement;
    match join::retire(&retirement_auth, &retirement_control).await {
        Ok(()) => tracing::info!("worker: instance retired at control"),
        Err(error) => tracing::warn!(
            %error,
            "worker: could not retire this instance at control; it stays active until its \
             lease runs out"
        ),
    }
    tracing::info!("worker shutdown complete");
    if let Some(reason) = host_failure.borrow_mut().take() {
        return Err(std::io::Error::other(format!("workflow host failed: {reason}")));
    }
    run_result
        })
}

#[cfg(test)]
mod boot_tests;
