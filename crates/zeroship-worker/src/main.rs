// The connection future of `compio-postgres` carries both transports' split
// halves (`MaybeTlsReadHalf`), and computing the layout of the version-poll
// async block walks that whole type. It lands ~130 deep, just past rustc's
// default of 128. A compile-time budget only - nothing here recurses at run
// time.
#![recursion_limit = "256"]

mod enrol;
mod handler;
#[cfg(test)]
mod test_database;
#[cfg(test)]
mod identity_fixture;
mod health;
mod sync;
mod cache;
mod metrics;
mod logs;
mod policy;

use std::sync::{Arc, RwLock};
use clap::Parser;
use ntex::web;
use zeroship_worker::config::{WorkerSettings, WorkerSettingsSources, WorkerSettingsConsumer};
use zeroship_worker::executable;
use zeroship_core::config::{
    audit_credentials, bootstrap_or_exit, mark_dev_escape_active, require_nonempty,
    BuildProfile, CheckConfigReport, CheckValue, CredentialPosture,
    CredentialVerdict, SubsystemCredential,
};
use zeroship_bundle::{
    build_blob_store, build_workflow_blob_store, BlobStore, StoreUrl, WorkflowBlobStore,
};
use zeroship_storage::StorageBackendConfig;
use zeroship_runtime::init::init_v8;

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
/// worker with nothing to do. The dispatch credential is NOT here and is not an
/// omission - it is an ed25519 key file loaded by
/// [`load_role_material`], which refuses a file it cannot read or that other
/// local users can, checks this audit cannot express and a strength floor on a
/// shared string cannot replace.
/// [`zeroship_core::config::audit_credentials`] handles the three material
/// cases, so a `--check-config` run still never judges a secret it deliberately
/// did not read.
fn worker_credentials(
    settings: &zeroship_worker::config::WorkerSettings,
) -> Vec<SubsystemCredential<'_>> {
    vec![
        SubsystemCredential {
            subsystem: "control-version-poll",
            enabled: true,
            label: CONTROL_KEY_LABEL,
            secret: &settings.control_key,
            validate: require_nonempty,
        },
    ]
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

/// Whether `bind_host` reaches only this machine.
///
/// String comparison, not a parse, because this compares against the value the
/// operator supplied rather than a resolved socket address - `--bind localhost` is
/// loopback in intent and does not parse as an `IpAddr` at all. It is therefore
/// deliberately conservative: an unusual spelling of loopback (`127.1`,
/// `::ffff:127.0.0.1`) reads as routable and is refused, which fails in the safe
/// direction for both callers.
///
/// Extracted so the two guards that need it cannot drift apart. The set used to be
/// inlined at the credential guard only; a second copy at the unsigned-advance
/// guard would have been one edit away from disagreeing about what counts as local.
fn is_loopback_bind(bind_host: &str) -> bool {
    bind_host == "127.0.0.1" || bind_host == "::1" || bind_host == "localhost"
}

/// Whether the worker may bind `bind_host` given the unsigned-workflow-advance flag.
///
/// With the flag off - the default, and what every deployment under `deploy/` uses -
/// any bind is fine, because the endpoint answers 403. With it on, the endpoint
/// replays workflow state with no signature or nonce check, so it must not be
/// reachable from off-box.
fn unsigned_advance_bind_allowed(bind_host: &str, unsigned_advance: bool) -> bool {
    !unsigned_advance || is_loopback_bind(bind_host)
}

// The usage-stream producer wiring (`build_usage_outbox`) is shared with the
// gateway producer and lives in `zeroship_metering`. What it is fed comes from
// this binary's own `metering.*` declarations - see
// `zeroship_worker::config::usage_stream_settings`.

#[allow(missing_debug_implementations)]
pub struct WorkerConfig {
    /// This process's service identity: the peer bundle it verifies inbound
    /// dispatch with, and its own ed25519 key for the control-plane reads it
    /// makes.
    ///
    /// THE INSTANCE IDENTITY, ALWAYS, AND NEVER THE ROLE. It mints under
    /// `svc/worker/<wkr_id>` on a key drawn at boot in memory, and is addressed
    /// as `svc/worker`. The operator's shared role key does not reach this
    /// field and cannot: `crate::enrol::enrol` consumes it and returns this,
    /// and this struct is not constructed until it has. That is the whole of
    /// "the role key authenticates the enrolment call and nothing else".
    ///
    /// INBOUND takes the transport-only profile - the dispatch hop is the app
    /// data path, so no `jti` is claimed and no shared store is consulted.
    /// OUTBOUND to control takes the FULL profile, because those reads fire
    /// once per app load.
    ///
    /// `ServiceAuth::unconfigured()` refuses every inbound call and mints
    /// nothing outbound. Absence is a CLOSED DOOR: it refuses, never disables.
    pub service_auth: Arc<zeroship_core::service_peers::ServiceAuth>,
    pub control_url: String,
    pub control_key: String,
    pub db_url: Option<String>,
    /// Process-owned KV store selected from runtime configuration.
    /// `None` leaves the app KV namespace absent.
    pub kv_store: Option<zeroship_kv::KvStore>,
    /// Object-store backend for the app `env.storage` namespace. `None` ⇒
    /// namespace absent. `LocalFs` (a shared volume across nodes) or `S3`
    /// (inherently shared) - see `WorkerSettings::storage_url`.
    pub storage_backend: Option<StorageBackendConfig>,
    pub max_isolates: usize,
    pub max_pinned_isolates_per_app: usize,
    pub poll_interval_secs: u64,
    /// Seconds ntex will wait after SIGTERM for in-flight requests to
    /// finish. Requests still running after the deadline are dropped and
    /// the worker exits. `0` means "wait forever" — useful locally but
    /// fatal for Kubernetes preemption (which will SIGKILL after its own
    /// `terminationGracePeriodSeconds`).
    pub shutdown_timeout_secs: u64,
    /// Content-addressed blob store. The worker fetches bundle bytes
    /// here directly instead of round-tripping through the control
    /// plane. In dev and single-host production the gateway, control,
    /// and worker all point at the same path; in multi-host production
    /// each crate keeps its own `Arc` over a shared remote backend
    /// (for example S3 with an on-disk LRU).
    pub blob_store: Arc<dyn BlobStore>,
    pub workflow_blob_store: Arc<dyn WorkflowBlobStore>,
    pub max_step_blob_bytes: u64,
    /// Test-only unsigned durable-workflow replay ingress. Production boot
    /// never exposes a CLI/env switch for this; signed control-plane advance
    /// replaces it in a later durable-workflows task.
    pub workflow_advance_unsigned: bool,
}

/// Load the operator's `svc/worker` key material, or refuse to start.
///
/// It is NOT this process's serving identity, and that is the change this
/// function's name now carries. The role key is shared by every worker replica,
/// so an assertion minted under it names a fleet; what serves is the INSTANCE
/// identity `crate::enrol::enrol` exchanges this material for, once, after the
/// port is bound. See that module for why there are two keyrings and why the
/// split is forced rather than chosen.
///
/// The verifier is the TRANSPORT-ONLY one. The worker is a callee on exactly
/// one edge - the gateway's dispatch hop - and that hop carries every end-user
/// request, so it claims no `jti` and consults no shared store. The worker
/// therefore needs NO database reachability for inbound authentication at all,
/// which is the property the tiering buys and the reason it is stated here
/// rather than left implicit in a missing argument.
///
/// EVERY OUTCOME BUT ONE IS AN EXIT, and that is fence F4 of
/// `docs/proposals/2026-09-05-auth-foundation-redesign.md` in full: "absent a
/// configured gateway public key the worker refuses to start". Unconfigured,
/// unreadable, unparseable and missing-the-gateway-key are one fate, because
/// from the outside they produce one behaviour - a worker that binds its port,
/// passes a liveness probe and turns away every request that reaches it. Step 3
/// landed the request-time half of this and left the startup half owing; this
/// is the half that is loud where an operator is looking.
///
/// The unconfigured case is refused by `ServiceKeyring::load` rather than by a
/// branch here, so no future edit of this function can restore the escape by
/// giving the empty path its own arm.
///
/// The same peer document also supplies the GATEWAY's public key for the
/// `ZeroShip-User` identity envelope, and a document that omits it is a HARD
/// STOP rather than a warning. The predecessor's failure mode was exactly the
/// opposite - an empty `worker_key` turned the envelope check off and the
/// bearer check with it - so the missing-key branch here is the point of the
/// change, not an edge case of it.
fn load_role_material(
    key_file: &std::path::Path,
    peers_file: &std::path::Path,
) -> crate::enrol::RoleMaterial {
    use zeroship_core::service_peers::{service_issuer, ServiceKeyring};
    use zeroship_core::user_envelope::UserEnvelopeVerifier;

    let issuer = match service_issuer(zeroship_core::service_peers::WORKER_SERVICE_NAME) {
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
    let mut keyring = match ServiceKeyring::load(issuer.clone(), key_file, peers_file) {
        Ok(keyring) => keyring,
        Err(error) => {
            tracing::error!(
                %error,
                "worker: refusing to start - service key material rejected; set \
                 worker.service_key_file and worker.service_peers_file"
            );
            std::process::exit(1);
        }
    };
    let Some(bundle) = keyring.take_bundle() else {
        tracing::error!("worker: refusing to start - peer bundle already taken");
        std::process::exit(1);
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
    crate::enrol::RoleMaterial::new(keyring, bundle, user_envelope, issuer)
}

fn main() -> std::io::Result<()> {
    let (settings, boot) = bootstrap_or_exit::<WorkerSettings>(
        WorkerSettingsSources::parse(),
        "info,zeroship_worker=debug,zeroship_runtime=info",
        "worker",
    );
    let check_config = *settings.check_config.get();
    let workflow_advance_unsigned = *settings.workflow_advance_unsigned.get();

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
    let max_pinned_isolates_per_app = *settings.max_pinned_isolates_per_app.get();
    let poll_interval = match crate::sync::validate_poll_interval_secs(*settings.poll_interval.get()) {
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

    // THE BOOT GATE, before the bind guard below and before the
    // `--check-config` report, so a dry run over a placeholder credential exits
    // non-zero. The credential strength check used to sit AFTER the bind guard;
    // the two are independent refusals and only the order in which a
    // doubly-misconfigured launch reports changes.
    let credentials =
        enforce_worker_credentials(&settings, &boot.overlay.source, check_config);

    // `--workflow-advance-unsigned` makes POST /internal/workflow/advance-unsigned
    // live. That route is registered unconditionally (handler.rs) and performs NO
    // signature or nonce verification - its own doc records that DW-05 deferred
    // that - so the flag is the only thing standing between an unauthenticated
    // caller and workflow state replay.
    //
    // The unsigned route has its own deliberately narrow loopback-only guard. It is
    // unrelated to the deleted process-wide security-relaxation mode.
    if !unsigned_advance_bind_allowed(&bind_host, workflow_advance_unsigned) {
        tracing::error!(
            bind = %bind_host,
            "refusing to bind non-loopback with --workflow-advance-unsigned — would expose \
             unauthenticated workflow replay"
        );
        std::process::exit(1);
    }

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
        report.field("worker_threads", CheckValue::Count(workers_count));
        report.field("max_isolates", CheckValue::Count(max_isolates));
        report.field(
            "max_pinned_isolates_per_app",
            CheckValue::Count(max_pinned_isolates_per_app),
        );
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
            "max_step_blob_bytes",
            CheckValue::Count(usize::try_from(*settings.max_step_blob_bytes.get()).unwrap_or(usize::MAX)),
        );
        report.field("socket_configured", CheckValue::Flag(!socket_path.is_empty()));
        // Both are reported by PRESENCE, which is all a resolved `Secret<T>`
        // will answer. `!value.is_empty()` used to stand in for that and could
        // not: under `--check-config` a file-sourced secret has no material, so
        // the old test read "unset" for a correctly configured deployment.
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
            CheckValue::Flag(storage_backend.as_ref().is_some_and(StorageBackendConfig::is_remote)),
        );
        // Both from the RESOLVED settings, which is the same expression the
        // producer boots from. They used to be two independent readings of
        // `REDPANDA_BROKERS` / `USAGE_EVENTS_TOPIC` straight out of the
        // environment, so the report could only agree with the producer while
        // the environment was the sole channel; with a flag it would have
        // reported `usage_stream_configured=false` on a worker whose outbox was
        // running. This field is the surface a harness asserts the producer on
        // BEFORE it launches anything, so it disagreeing is worse than useless.
        let usage_stream = zeroship_worker::config::usage_stream_settings(&settings);
        report.field(
            "usage_stream_configured",
            CheckValue::Flag(usage_stream.producer_enabled()),
        );
        report.field(
            "usage_events_topic",
            CheckValue::Plain(usage_stream.effective_topic().to_string()),
        );
        // The credential posture, plus how much of it was measured. The field
        // above is the cautionary example: `usage_stream_configured=false` was
        // reported truthfully for 43 days while nothing read it, which is why
        // the posture rides the EXIT CODE as well as this report.
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
    // conditional on an unrelated subsystem being reachable. It used to be built
    // inside `WorkerConfig` below, which is after
    // `db_posture::validate_database_url` CONNECTS: a worker with no peer
    // document and no database reported the database, so the operator fixed
    // Postgres and only then learned about the key material. Worse, it made the
    // one fence the worker cannot serve a request without dependent on the one
    // subsystem the tiering was chosen to keep it independent of - the dispatch
    // hop claims no `jti` precisely so inbound authentication needs no database
    // at all. Reading two files needs no async runtime, so nothing is lost by
    // doing it first.
    let role_material = load_role_material(
        settings.service_key_file.get(),
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
    let workflow_blob_store: Arc<dyn WorkflowBlobStore> =
        build_workflow_blob_store(&store_url, s3_runtime.as_ref())
            .expect("failed to initialise workflow blob store");
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
    // observed by every ntex worker thread's reconcile loop. Previously every
    // thread made its own HTTP poll — this multiplied control-plane traffic
    // by `workers_count` with no benefit.
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
    // worker deliberately has no TOML overlay source (9b205f6ed, a credential
    // boundary), so its tiers are flag then `ZEROSHIP_METERING_*` then the
    // compiled default - and the flag is what nine e2e harnesses had to fake
    // with ambient variables on the command prefix until 2026-08-20, because
    // `UsageStreamSettings::from_env` was the only channel that existed.
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
        // The old behaviour degraded to `spawn_disabled_drain_task`, which
        // drains the meter and DROPS every event for the life of the process -
        // permanent total loss standing in for an intermittent partial one.
        // Refusing to boot is the recoverable failure; silent free hosting is
        // not.
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
    // of letting `HttpServer::bind` create it. The order is the point:
    // enrolment ADVERTISES this port to control, control writes a row carrying
    // it, and NOTHING REAPS THAT ROW. Enrolling before the socket exists would
    // therefore let a bind failure leave a live-looking registry entry pointing
    // at a port nothing listens on. `bind` is reachable only through the server
    // builder, and the server cannot be built until the identity enrolment
    // returns is in hand - so the bind moves out here rather than the enrolment
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
    // worker that logged this and carried on would serve traffic under the
    // SHARED role key while control's registry either knows nothing about it or
    // holds a row for a process that never finished starting - and from the
    // outside it would look exactly like a worker that enrolled, which is the
    // failure shape this platform keeps re-learning.
    //
    // `enrol` CONSUMES the role material, so the operator's shared key is
    // spent on this one call and is unreachable afterwards. What comes back
    // mints under `svc/worker/<wkr_id>` and is addressed as `svc/worker`;
    // everything below - the version poller, every reconcile, every dispatch -
    // is handed that and only that.
    let service_auth = match enrol::enrol(role_material, &control_url, port).await {
        Ok(auth) => Arc::new(auth),
        Err(error) => {
            tracing::error!(
                control_url = %control_url,
                port,
                %error,
                "worker: refusing to start - this process could not enrol an instance identity"
            );
            std::process::exit(1);
        }
    };

    let db_service = match db_url_opt.as_deref() {
        Some(url) => Some(
            zeroship_data_v8::service::DbService::new(
                zeroship_data_v8::service::DbServiceConfig {
                    project_keys: Default::default(),
                    connection: zeroship_data_orm::connection::ConnectionFactory::for_url(url)
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


    let config = Arc::new(WorkerConfig {
        service_auth,
        control_url,
        control_key,
        db_url: db_url_opt,
        kv_store,
        storage_backend,
        max_isolates,
        max_pinned_isolates_per_app,
        poll_interval_secs: poll_interval,
        shutdown_timeout_secs: shutdown_timeout,
        blob_store,
        workflow_blob_store,
        max_step_blob_bytes: *settings.max_step_blob_bytes.get(),
        workflow_advance_unsigned,
    });

    // The single process-wide version poller. Started after the `env.db`
    // service exists because a deleted app's CDC teardown runs through that
    // service's operator lifecycle handle, and still before `web::server` below
    // spawns any worker thread, so the shared version map is already filling
    // when they come up. Poller also GCs SharedEnvs against the current
    // known-app set, so env entries for deleted apps don't leak forever.
    //
    // It is also the LAST thing before the server that talks to control, and it
    // is now downstream of enrolment - so no outbound call this process makes
    // can be minted under the role key. Nothing before this point mints at all:
    // the poller's own credential is the shared control key
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

    tracing::info!(
        bind = %bind_addr,
        threads = workers_count,
        max_isolates = config.max_isolates,
        max_pinned_isolates_per_app = config.max_pinned_isolates_per_app,
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
            config.max_pinned_isolates_per_app,
            cache::KernelConfig {
                control_url: config.control_url.clone(),
                control_key: config.control_key.clone(),
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
    let run_result = server.run().await;
    tracing::info!("worker shutdown complete");
    run_result
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeroship_core::config::{GeneratedConfig, SourceKind, SERVICE_CREDENTIAL_SENTINEL};

    /// A temp file that removes itself even when an assertion panics.
    struct SecretFile(std::path::PathBuf);

    impl SecretFile {
        fn new(tag: &str, contents: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "zeroship_worker_secret_{tag}_{}_{:?}.txt",
                std::process::id(),
                std::thread::current().id()
            ));
            std::fs::write(&path, contents).expect("write fixture secret");
            // The resolver refuses a secret file any second local account could
            // read, and `std::fs::write` leaves 0644 under the usual umask.
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
                    .expect("owner-only fixture secret");
            }
            Self(path)
        }

        fn arg(&self) -> &str {
            self.0.to_str().expect("utf8 fixture path")
        }
    }

    impl Drop for SecretFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    fn resolve(args: &[&str]) -> WorkerSettings {
        let mut argv = vec!["zeroship-worker"];
        argv.extend_from_slice(args);
        WorkerSettings::resolve_config(
            WorkerSettingsSources::try_parse_from(argv).expect("worker sources parse"),
            None,
        )
        .expect("worker settings resolve")
    }

    #[test]
    fn worker_cli_has_no_security_relaxation_flag() {
        let error = WorkerSettingsSources::try_parse_from(["zeroship-worker", "--dev-insecure"])
            .expect_err("deleted --dev-insecure flag must be rejected");
        assert_eq!(error.kind(), clap::error::ErrorKind::UnknownArgument);
    }

    #[test]
    fn worker_env_has_no_security_relaxation_binding() {
        assert!(WorkerSettingsSources::try_parse_from(["zeroship-worker"]).is_ok());
    }

    /// `--workflow-advance-unsigned` must not be combined with a routable bind.
    ///
    /// The workflow acceptance fixtures enable the replay endpoint on loopback.
    /// It still requires the gateway's service assertion. The bind guard keeps
    /// this extra dispatch path local even when a caller supplies the flag;
    /// Compose never enables it.
    #[test]
    fn unsigned_advance_refused_on_a_routable_bind() {
        // The dangerous combination, in the three spellings a routable bind takes.
        assert!(!unsigned_advance_bind_allowed("0.0.0.0", true));
        assert!(!unsigned_advance_bind_allowed("::", true));
        assert!(!unsigned_advance_bind_allowed("10.0.0.7", true));
    }

    #[test]
    fn unsigned_advance_allowed_on_loopback() {
        for host in ["127.0.0.1", "::1", "localhost"] {
            assert!(
                unsigned_advance_bind_allowed(host, true),
                "{host} is loopback and must stay allowed"
            );
        }
    }

    /// POSITIVE CONTROL. Both assertions above are satisfied by a predicate that
    /// refuses every bind, which would stop the worker booting anywhere. With the
    /// flag OFF - the default, and what every deployment uses - any bind is fine.
    #[test]
    fn a_routable_bind_is_fine_without_the_flag() {
        assert!(unsigned_advance_bind_allowed("0.0.0.0", false));
        assert!(unsigned_advance_bind_allowed("10.0.0.7", false));
        assert!(unsigned_advance_bind_allowed("127.0.0.1", false));
    }

    #[test]
    fn worker_threads_default_resolves_to_positive_count() {
        // The Option-taking helper is gone: absence is now the generated
        // resolver's job, and the compiled default is the "one per core" call.
        assert!(zeroship_worker::config::default_worker_threads() > 0);

        let flagged = WorkerSettingsSources::try_parse_from(["zeroship-worker", "--threads", "3"])
            .expect("--threads parses");
        assert_eq!(flagged.threads, Some(3));
    }

    #[test]
    fn worker_rejects_sqlite_database_url() {
        // File-backed SQLite and ephemeral selectors are both refused.
        assert!(worker_rejects_db_url("sqlite:.zeroship/dev.sqlite"));
        assert!(worker_rejects_db_url("sqlite://./data/app.sqlite"));
        assert!(worker_rejects_db_url("file:./local.db"));
        assert!(worker_rejects_db_url(":memory:"));
        assert!(worker_rejects_db_url("/var/lib/zeroship/dev.sqlite"));
    }

    #[test]
    fn worker_rejects_invalid_and_unsupported_database_selectors() {
        for selector in [
            "sqlite:",
            "sqlite::memory:",
            "file:db?mode=memory",
            "mysql://localhost/db",
        ] {
            assert!(worker_rejects_db_url(selector), "{selector}");
        }
    }

    #[test]
    fn worker_accepts_postgres_database_url() {
        // The only valid prod backend.
        assert!(!worker_rejects_db_url("postgres://localhost/dev"));
        assert!(!worker_rejects_db_url("postgresql://u:p@host:5432/db"));
        // An empty / unread DSN is NOT rejected here (db_configured is a
        // separate, softer concern — the worker can boot with auth-only).
        assert!(!worker_rejects_db_url(""));
    }

    #[test]
    fn worker_thread_flag_uses_unambiguous_name() {
        let sources = WorkerSettingsSources::try_parse_from([
            "zeroship-worker",
            "--threads",
            "3",
            "--max-isolates",
            "200",
            "--poll-interval",
            "5",
            "--shutdown-timeout",
            "30",
        ])
        .expect("--threads should parse");
        assert_eq!(sources.threads, Some(3));

        // The point of the original assertion survives the rename: the worker's
        // thread count must never share a flag with the control plane's list of
        // worker URLs. `--workers` belongs to control and gateway.
        let old_flag = WorkerSettingsSources::try_parse_from(["zeroship-worker", "--workers", "3"]);
        assert!(old_flag.is_err(), "--workers must not parse for worker threads");
        let renamed =
            WorkerSettingsSources::try_parse_from(["zeroship-worker", "--worker-threads", "3"]);
        assert!(
            renamed.is_err(),
            "the pre-conversion flag must be gone, not aliased"
        );
    }

    #[test]
    fn worker_numeric_fields_reject_bad_input() {
        let err = WorkerSettingsSources::try_parse_from(["zeroship-worker", "--max-isolates", "abc"])
            .expect_err("bad max-isolates should be a clap error");
        assert_eq!(err.kind(), clap::error::ErrorKind::ValueValidation);
    }

    // --- Secret<T> conversion: the guards still run on the resolved material ---

    /// The boot gate, driven end to end through the REAL declaration: a
    /// `--control-key-file` path is resolved by the generated resolver and the
    /// audit hands the material it produced to `require_nonempty`.
    ///
    /// It drives `control_key` because that is now the worker's ONLY audited
    /// credential. Its predecessor drove the worker key, whose length floor is
    /// gone with the secret itself: the dispatch hop is authenticated by an
    /// ed25519 service assertion, and the file it loads is refused for being
    /// unreadable or wrongly permissioned rather than for being short.
    ///
    /// Three cases plus a one-variable partner, because "the guard rejects
    /// everything" and "the guard rejects nothing" both satisfy a single case.
    #[test]
    fn a_missing_or_placeholder_control_key_still_fails_the_boot_guard() {
        let good = SecretFile::new("control_ok", "control-key-material");

        // 1. ABSENT: nothing supplies the control key. The audit runs the
        // validator on "", which is how it produces its own "is required".
        let settings = resolve(&[]);
        assert!(!settings.control_key.is_configured());
        let absent = audit_credentials(&worker_credentials(&settings));
        assert_eq!(absent.weak().len(), 1, "{absent:?}");
        assert_eq!(absent.weak()[0].subsystem, "control-version-poll");
        assert!(absent.weak()[0].unset);
        assert!(absent.weak()[0].message.contains("required"), "{absent:?}");
        assert!(
            absent.weak()[0].message.contains("ZEROSHIP_CONTROL_KEY"),
            "{absent:?}"
        );

        // 2. THE SENTINEL on a credential with NO length floor. `control_key`
        // is `SecretStrength::Unrestricted`, so nothing but the sentinel branch
        // can refuse a placeholder here - which is the case the gate exists for,
        // and it must produce the SAME message as absent.
        let sentinel = SecretFile::new("control_sentinel", SERVICE_CREDENTIAL_SENTINEL);
        let settings = resolve(&["--control-key-file", sentinel.arg()]);
        assert_eq!(settings.control_key.source(), Some(SourceKind::CliFile));
        let placeholder = audit_credentials(&worker_credentials(&settings));
        assert_eq!(placeholder.weak().len(), 1, "{placeholder:?}");
        assert_eq!(placeholder.weak()[0].message, absent.weak()[0].message);

        // 3. The one-variable partner: real material at the same path passes,
        // so the two refusals above are about the material and not about the
        // guard refusing everything it is handed.
        let ok = audit_credentials(&worker_credentials(&resolve(&[
            "--control-key-file",
            good.arg(),
        ])));
        assert!(ok.is_ok(), "a real control key passes every guard: {ok:?}");
        assert_eq!(ok.checked(), 1, "the worker audits exactly one credential");

        // Does NOT cover the environment or TOML tiers of this secret: the env
        // tier is process-global and would race sibling tests, and the overlay
        // tier is `resolve_secret_sources`'s, asserted in core. It also does not
        // prove `main` calls `worker_credentials` - only that the function main
        // calls behaves this way. That link is covered by
        // `tests/service_credential_boot_gate.sh`, against the real binary.
    }

    /// The `--check-config` half: a dry run must not open the file, and an
    /// unread secret must not be judged. Paired with the boot run over the SAME
    /// missing path, which does fail - so "no error" above is a property of the
    /// mode, not of the guard having been removed.
    #[test]
    fn a_check_config_run_neither_reads_nor_judges_a_secret_file() {
        let missing = std::env::temp_dir().join("zeroship_worker_absent_control_key");
        let _ = std::fs::remove_file(&missing);
        assert!(!missing.exists(), "the fixture path must really be absent");
        let missing = missing.to_str().expect("utf8 path").to_owned();

        let dry = resolve(&["--check-config", "--control-key-file", &missing]);
        assert!(dry.control_key.is_configured());
        assert_eq!(
            dry.control_key.expose_secret(),
            None,
            "--check-config must not have read the file"
        );
        let posture = audit_credentials(&worker_credentials(&dry));
        assert!(
            posture
                .weak()
                .iter()
                .all(|weak| weak.subsystem != "control-version-poll"),
            "an unread secret must not be judged: {posture:?}"
        );
        assert_eq!(posture.unread(), 1, "and it must be COUNTED as unread");

        // The one-variable partner: only `--check-config` differs, and the boot
        // run does try to open the same path.
        let boot = WorkerSettingsSources::try_parse_from([
            "zeroship-worker",
            "--control-key-file",
            &missing,
        ])
        .expect("worker sources parse");
        assert!(WorkerSettings::resolve_config(boot, None).is_err());

        // Does NOT cover whether `main` routes `--check-config` to the report
        // rather than to the server; that is `tests/config_check_e2e.sh`.
    }

    /// A secret is reported by PRESENCE. The resolved declaration derives
    /// `Debug`, which is what replaced the hand-written `impl Debug for
    /// WorkerCli` redaction list: that list named four fields and had to be
    /// edited whenever a fifth arrived, and nothing failed if it was not.
    #[test]
    fn a_resolved_secret_never_formats_its_material_or_its_length() {
        // Deliberately unlike any other text in the struct, so a prefix match
        // below can only be the secret leaking and never an unrelated field.
        const SENTINEL: &str = "k9x2m7q4v8b3n6z1p5t0w4y7r2j8h5d3";
        let file = SecretFile::new("debug", SENTINEL);
        let settings = resolve(&["--kv-config-file", file.arg()]);

        assert!(settings.kv_config.is_configured());
        assert_eq!(
            settings.kv_config.expose_str(),
            SENTINEL,
            "the boot path must still get the real material"
        );

        // The secret's OWN formatter: no value, no prefix of it, no length.
        let field = format!("{:?}", settings.kv_config);
        for length in 4..=SENTINEL.len() {
            assert!(
                !field.contains(&SENTINEL[..length]),
                "Debug leaked a {length}-char prefix of the secret: {field}"
            );
        }
        assert!(
            !field.contains(&SENTINEL.len().to_string()),
            "Debug leaked the secret's length: {field}"
        );
        // The one-variable control: the tier IS published, so this is not
        // passing because Debug prints nothing at all.
        assert_eq!(field, "Secret(configured from CliFile)");

        // And the whole resolved declaration, which is what a `{:?}` on the
        // settings reaches.
        let rendered = format!("{settings:?}");
        assert!(!rendered.contains(SENTINEL), "{rendered}");
        assert!(rendered.contains("Secret(configured from CliFile)"), "{rendered}");

        // Does NOT cover a caller that calls `expose_str` and prints the result
        // itself. No type can stop deliberate disclosure; what it removes is the
        // accidental `{:?}` - and the maintenance of a hand-written redaction
        // list that could silently fall behind the field set.
    }

    /// A secret's clap carrier is a PATH flag, and there is no value flag for
    /// any of them - a secret must never travel through argv, where it is
    /// visible in `ps` to every user on the host.
    #[test]
    fn no_worker_secret_has_a_value_flag() {
        use clap::CommandFactory;

        let command = WorkerSettingsSources::command();
        let longs = command
            .get_arguments()
            .filter_map(|arg| arg.get_long().map(str::to_owned))
            .collect::<Vec<_>>();
        for secret in ["control-key", "database-url", "kv-config"] {
            assert!(
                longs.iter().any(|long| long == &format!("{secret}-file")),
                "{secret} must offer a -file path flag: {longs:?}"
            );
            assert!(
                !longs.iter().any(|long| long == secret),
                "{secret} must NOT offer a value flag: {longs:?}"
            );
        }
        // The deleted pre-conversion spelling of the DSN, which put it in argv.
        assert!(!longs.iter().any(|long| long == "db"), "{longs:?}");
        // The one-variable control: an operational setting still has a value
        // flag, so this is not asserting that no value flag exists at all.
        assert!(longs.iter().any(|long| long == "storage-url"), "{longs:?}");

        // Does NOT cover clap's env bindings; a secret carries none by
        // construction (the macro emits no `env = ...` for a Secret field) and
        // its canonical environment name is read by the resolver instead.
    }
}
