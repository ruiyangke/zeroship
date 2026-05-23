//! `zeroship-sandbox` — pluggable sandbox-session controller.
//!
//! The crate is split between a thin binary (`src/main.rs`) and this
//! library, so integration tests and the lifecycle e2e example can
//! reuse `AppState`, the [`backend::Backend`] enum, and the session
//! registry without re-launching the HTTP server in-process.
//!
//! See [`backend`] for the backend contract and the docker / k8s
//! implementations.

pub mod admin_handlers;
pub mod auth;
pub mod backend;
pub mod config;
pub mod db;
pub mod files;
pub mod handlers;
pub mod metrics;
pub mod persist;
pub mod preview;
pub mod preview_share;
pub mod preview_share_handlers;
pub mod preview_ws;
pub mod registry;
pub mod restore;
pub mod restore_handler;
pub mod snapshot_aead;
pub mod snapshot_handler;
pub mod snapshot_store;
pub mod snapshot_store_gcs;
pub mod sweep;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::backend::Backend;
use crate::config::SandboxConfig;
use crate::db::{Database, LATEST_MIGRATION_VERSION};
use crate::persist::Persistence;
use crate::preview_share_handlers::MintRateLimiter;
use crate::registry::SandboxRegistry;
use crate::restore_handler::{RealRestoreBackend, RestoreBackend};
use crate::snapshot_handler::{ChRemoteClient, RealChRemoteClient};
use crate::snapshot_store::{LocalDiskSnapshotStore, SnapshotStore};
use crate::snapshot_store_gcs::{GcsSnapshotStore, TieredSnapshotStore};

/// Shared application state passed to every handler.
#[allow(missing_debug_implementations)]
pub struct AppState {
    /// A6b (deferred): restricted to `pub(crate)` because
    /// `SandboxConfig.token: ApiToken` is the creator-side bearer that
    /// authenticates every `/sandbox/*` request (see
    /// `crates/sandbox/src/config.rs`). A `state.config = attacker_cfg`
    /// swap could plant a known token or repoint
    /// `nomad_addr`/`workspace_root` to an attacker-controlled host.
    /// Out-of-crate callers use [`AppState::with_config`] to set the
    /// field. Note: `SandboxConfig.token` itself is still a `pub` field
    /// inside `config.rs` — narrowing that requires touching the
    /// config-parse surface and is tracked separately (A7).
    pub(crate) config: SandboxConfig,
    pub sandboxes: SandboxRegistry,
    pub backend: Backend,
    /// Phase-3 mint-side rate limiter. `Some` in production; `None`
    /// for tests that build `AppState` directly without
    /// `from_config`. Handlers that consume it `expect()` on `Some`.
    pub mint_rate_limiter: Option<MintRateLimiter>,
    /// Phase-0 pg-backed non-secret state handle (sandbox-pg-state
    /// design § 8.2). `None` is the disabled-by-absence shape:
    /// `SANDBOX_DATABASE_URL` is unset, so pg integration is off.
    /// In Phase 0, the handle exists but no live call sites consume
    /// it — Phase 1 wires `insert_sandbox` / `record_event` into
    /// the backends. The schema is brought to
    /// [`LATEST_MIGRATION_VERSION`] before this state ships.
    ///
    /// A6b (deferred): restricted to `pub(crate)` because the DSN held
    /// inside `Database` carries an embedded pg password. A
    /// `state.database = attacker_db` swap could redirect every
    /// sandbox INSERT / heartbeat / takeover UPDATE to an
    /// attacker-controlled postgres (silently exfiltrating sandbox
    /// metadata and host topology). Out-of-crate callers use
    /// [`AppState::with_database`] to set the field.
    pub(crate) database: Option<Arc<Database>>,
    /// Round-1 fixer / CRITICAL #4: shared sealed-record persistence
    /// handle, plumbed for the Phase-2.5 takeover-rehydrate path.
    /// `None` mirrors the pre-fix disabled shape — the handle exists
    /// only when `SANDBOX_PERSIST_AUTH=1` was set and a key file is
    /// readable.
    ///
    /// A6 (api-surface-2026-05-24-r1): restricted to `pub(crate)` so
    /// no out-of-crate caller can swap the handle for an attacker-
    /// controlled `Persistence` carrying a different AEAD key. The
    /// concrete attack: a hostile in-process caller plants a
    /// `Persistence` whose `aead_key` is one the attacker knows, then
    /// waits for the controller to seal a sandbox's signing key under
    /// the planted key — disk read of the sealed file then yields the
    /// signing key in clear. Tests and other in-crate constructors
    /// set the field via the safe [`AppState::with_persistence`]
    /// builder. The builder is a thin wrapper (no runtime check —
    /// `Persistence` has no in-memory "is valid" beyond construction,
    /// which `AeadKey::from_path` already enforces); the point is to
    /// be the single legal write path, so the `state.persist =
    /// attacker_handle` swap stops compiling out-of-crate.
    pub(crate) persist: Option<Arc<Persistence>>,
    /// Round-1 fixer / IMPORTANT #8: graceful-shutdown flag observed
    /// by the periodic background tasks (heartbeat, takeover-scan,
    /// health-probe). [`AppState::trigger_shutdown`] flips this to
    /// `true` and atomically marks the host row `'draining'` in pg
    /// so peers see the intent before any takeover would fire.
    /// Each task checks the flag at the top of every iteration and
    /// exits cleanly when set; the next deploy can then drop the
    /// process without leaving phantom heartbeat traffic.
    pub shutdown: Arc<AtomicBool>,
    /// Round-3 / Phase-3 CRITICAL #3: admin bearer token, read ONCE
    /// at boot from `SANDBOX_ADMIN_TOKEN_PATH`. `None` is the
    /// disabled-by-absence shape: the env is unset, so every
    /// `/admin/*` endpoint 503s with `"admin api disabled"`. `Some`
    /// is the opt-in shape; the boot-time read enforces mode 0o400.
    ///
    /// Per-request disk I/O on the auth path is gone — was a slow-FS
    /// DoS amplification + fail-open on chmod-error. `admin_check`
    /// reads this field in O(1) and constant-time-compares against
    /// the request bearer.
    ///
    /// Round-4 / MINOR #5: wrapped in `zeroize::Zeroizing<String>` so
    /// the heap allocation is scrubbed on drop. A core dump or
    /// `/proc/<pid>/mem` read after process exit can't trivially
    /// recover the bearer. (Live-process reads are still a concern,
    /// but the post-mortem surface is closed.) Mirrors the
    /// `config::ApiToken` treatment of `SANDBOX_TOKEN`.
    ///
    /// A5 (api-surface-2026-05-24-r1): restricted to `pub(crate)` so
    /// no out-of-crate caller can clobber the field with an empty
    /// `Zeroizing<String>` (which would defeat the constant-time
    /// compare — see `admin_handlers::admin_check`). Tests and other
    /// in-crate constructors set the field via the safe
    /// [`AppState::with_admin_token`] builder, which rejects empty
    /// strings before they can reach the auth path.
    pub(crate) admin_token: Option<zeroize::Zeroizing<String>>,

    /// Phase-A snapshot/restore wiring: present (`Some`) only when
    /// `config.snapshot_enabled = true`. The trio of stores +
    /// clients construct together; either all three are populated
    /// or none. Tests building `AppState` directly leave them
    /// `None` — admin handlers fall back to 501 `feature_disabled`
    /// in that case.
    ///
    /// A6b (deferred): restricted to `pub(crate)` because each is an
    /// `Arc<dyn …>` trait object whose impl can carry arbitrary
    /// credentials (GCS SA keys in `GcsSnapshotStore`, the `ch-remote`
    /// binary path in `RealChRemoteClient`, Nomad creds in
    /// `RealRestoreBackend`). A `state.snapshot_store = attacker_impl`
    /// swap could exfiltrate every subsequent snapshot blob to an
    /// attacker bucket. Out-of-crate callers use the
    /// `with_snapshot_store` / `with_ch_remote` / `with_restore_backend`
    /// builders to set these.
    pub(crate) snapshot_store: Option<Arc<dyn SnapshotStore>>,
    pub(crate) ch_remote: Option<Arc<dyn ChRemoteClient>>,
    pub(crate) restore_backend: Option<Arc<dyn RestoreBackend>>,
}

impl AppState {
    /// Round-1 fixer / IMPORTANT #8: signal background tasks to
    /// exit cleanly. Best-effort UPDATEs `sandbox.hosts.status =
    /// 'draining'` for THIS host so peers know we're going away
    /// before our heartbeat goes silent (without that hint, peers
    /// would wait the full lease_ttl before noticing). The pg
    /// write is fire-and-forget; we don't fail shutdown when pg
    /// is unavailable.
    ///
    /// Tasks observe the flag at the top of each iteration via
    /// [`AppState::shutdown_requested`]. After this call returns,
    /// the next iteration of each loop will exit; in the worst
    /// case that is bounded by the longest sleep interval
    /// (heartbeat=5s, takeover-scan=30s, health-probe=30s).
    pub async fn trigger_shutdown(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
        if let Some(db) = self.database.as_ref() {
            // Best-effort. A hung pg call here would block the
            // shutdown caller; we already told the tasks to exit
            // so the process can proceed even if this never
            // returns success.
            if let Err(e) = db.set_host_draining().await {
                tracing::warn!(
                    error = %e,
                    "sandbox HA: set_host_draining on shutdown failed (non-fatal)"
                );
            }
        }
    }

    /// Returns `true` once [`AppState::trigger_shutdown`] has been
    /// called. Background loops poll this at the top of each
    /// iteration to decide whether to break out.
    pub fn shutdown_requested(&self) -> bool {
        self.shutdown.load(Ordering::SeqCst)
    }

    /// A5 (api-surface-2026-05-24-r1): safe builder for
    /// `admin_token`. The field itself is `pub(crate)` so external
    /// callers (notably integration tests in `crates/sandbox/tests/`)
    /// cannot construct it directly — they go through this builder,
    /// which rejects empty strings BEFORE they can reach
    /// `admin_handlers::admin_check`.
    ///
    /// `admin_check` has a defense-in-depth empty check on the
    /// expected token, but pushing the rejection to the only
    /// out-of-crate entry point turns the footgun (the post-Round-4
    /// review's `pub admin_token: Some(Zeroizing::new(String::new()))`
    /// silent-auth-bypass shape) into a `Err(...)` at the call site.
    ///
    /// Semantics:
    ///   - `token = None`  → clears the field (admin API disabled).
    ///   - `token = Some("")` → `Err("admin_token must not be empty")`.
    ///   - `token = Some(non-empty)` → wraps in `Zeroizing<String>`.
    pub fn with_admin_token(
        mut self,
        token: Option<String>,
    ) -> Result<Self, String> {
        match token {
            None => {
                self.admin_token = None;
                Ok(self)
            }
            Some(t) if t.is_empty() => {
                Err("admin_token must not be empty".to_string())
            }
            Some(t) => {
                self.admin_token = Some(zeroize::Zeroizing::new(t));
                Ok(self)
            }
        }
    }

    /// Read-only accessor for the admin bearer. Returns the raw
    /// string slice; callers MUST use constant-time comparison
    /// (`subtle::ConstantTimeEq` via `admin_handlers::admin_check`)
    /// rather than `==` against user-presented bytes. Mainly here
    /// so integration tests can assert wiring without poking the
    /// `pub(crate)` field.
    pub fn admin_token(&self) -> Option<&str> {
        self.admin_token.as_deref().map(|z| z.as_str())
    }

    /// A6 (api-surface-2026-05-24-r1): safe builder for `persist`.
    /// The field is `pub(crate)` (see the `AppState::persist` doc
    /// comment for the threat model — a planted `Persistence` with
    /// an attacker-known AEAD key lets a later disk read recover
    /// per-sandbox signing keys in clear). External callers must go
    /// through this builder.
    ///
    /// The validation surface is intentionally thin: `Persistence`
    /// has no in-memory "is valid" beyond construction. The
    /// `AeadKey` length and key-file mode are already enforced
    /// inside `AeadKey::from_path` / `AeadKey::from_bytes`; the
    /// sealed-records dir is created on first seal. So unlike
    /// [`AppState::with_admin_token`] (which has a real
    /// empty-string footgun to reject), this builder is a thin
    /// wrapper that exists for parity — it's the only legal way
    /// for out-of-crate code to set `persist`, which closes the
    /// `state.persist = …` swap path without requiring any new
    /// runtime check.
    ///
    /// Returning `Result<_, String>` rather than `Self` is
    /// deliberate symmetry: future invariants (e.g. dir-writability
    /// probes, AEAD-key liveness pings) can be added without a
    /// signature break.
    ///
    /// Semantics:
    ///   - `with_persistence(p)` → `Ok(self)` with the field set
    ///     to `Some(p)`. Replaces any prior value.
    pub fn with_persistence(
        mut self,
        persist: Arc<Persistence>,
    ) -> Result<Self, String> {
        self.persist = Some(persist);
        Ok(self)
    }

    /// Read-only accessor for the sealed-record persistence handle.
    /// Mainly here so integration tests can assert wiring without
    /// poking the `pub(crate)` field. The returned `&Arc` is a
    /// borrow; callers wanting an owned clone go through `.clone()`
    /// on the `Arc`, NOT through field access.
    ///
    /// `#[allow(dead_code)]`: today the in-crate uses access
    /// `self.persist` directly (they were written before this
    /// helper landed). The accessor exists for the in-crate
    /// `persist_setter_tests` and as the canonical read path for
    /// future code; widening to `pub` would let out-of-crate tests
    /// assert wiring the same way `admin_token()` does, but the A6
    /// task spec pinned this to `pub(crate)`.
    #[allow(dead_code)]
    pub(crate) fn persist(&self) -> Option<&Arc<Persistence>> {
        self.persist.as_ref()
    }

    // ────────────────────────────────────────────────────────────────
    // A6b (deferred backlog) — pub(crate)-restrict-and-builder pattern
    // applied to the 5 remaining credential-carrying AppState fields:
    // `config`, `database`, `snapshot_store`, `ch_remote`,
    // `restore_backend`. Each builder consumes `self` and returns
    // `Self` (or `Arc<Self>` via the caller's own wrap) — mirrors the
    // A5 `with_admin_token` / A6 `with_persistence` shape. None of
    // these can be "empty" in the same way the admin-token string can,
    // so the builders are thin wrappers that exist purely to be the
    // single legal out-of-crate write path. Tests in
    // `field_setter_tests` below exercise the replace-existing
    // semantics for each.
    // ────────────────────────────────────────────────────────────────

    /// A6b: safe builder for `config`. See the field doc for the
    /// threat model (creator-side bearer + `nomad_addr` pivot via
    /// `SandboxConfig.token`). Replaces any prior value.
    pub fn with_config(mut self, config: SandboxConfig) -> Self {
        self.config = config;
        self
    }

    /// A6b: safe builder for `database`. See the field doc for the
    /// threat model (DSN-embedded pg password). Replaces any prior
    /// value.
    pub fn with_database(mut self, database: Arc<Database>) -> Self {
        self.database = Some(database);
        self
    }

    /// A6b: read-only accessor for the pg-backed `Database` handle.
    /// `pub` (not `pub(crate)`) because out-of-crate integration tests
    /// in `crates/sandbox/tests/sandbox_pg_e2e.rs` need to read sandbox
    /// rows back through the same handle they wired in. Mirrors the
    /// `pub fn admin_token()` accessor in shape; the borrow can be
    /// cloned by callers via `Arc::clone` if they need ownership.
    pub fn database(&self) -> Option<&Arc<Database>> {
        self.database.as_ref()
    }

    /// A6b: read-only accessor for the active `SandboxConfig`. `pub`
    /// for the same reason as [`AppState::database`] — out-of-crate
    /// integration tests sometimes need to read back `config.port`,
    /// `config.snapshot_enabled`, etc. when asserting on test fixtures.
    pub fn config(&self) -> &SandboxConfig {
        &self.config
    }

    /// A6b: safe builder for `snapshot_store`. See the field doc for
    /// the threat model (trait-object impl can carry GCS SA creds).
    /// Replaces any prior value.
    pub fn with_snapshot_store(
        mut self,
        store: Arc<dyn SnapshotStore>,
    ) -> Self {
        self.snapshot_store = Some(store);
        self
    }

    /// A6b: safe builder for `ch_remote`. See the field doc for the
    /// threat model (trait-object impl resolves `ch-remote` on PATH).
    /// Replaces any prior value.
    pub fn with_ch_remote(
        mut self,
        ch_remote: Arc<dyn ChRemoteClient>,
    ) -> Self {
        self.ch_remote = Some(ch_remote);
        self
    }

    /// A6b: safe builder for `restore_backend`. See the field doc for
    /// the threat model (trait-object impl holds Nomad creds).
    /// Replaces any prior value.
    pub fn with_restore_backend(
        mut self,
        restore_backend: Arc<dyn RestoreBackend>,
    ) -> Self {
        self.restore_backend = Some(restore_backend);
        self
    }

    /// A5 (api-surface-2026-05-24-r1): public fixture constructor
    /// for out-of-crate integration tests. Returns an `AppState`
    /// with `admin_token = None` and the other "wiring" fields at
    /// their disabled defaults (`database = None`, `persist = None`,
    /// snapshot trio all `None`, fresh registry, fresh shutdown
    /// flag, fresh `MintRateLimiter`). Tests chain the `with_*`
    /// builders ([`AppState::with_admin_token`],
    /// [`AppState::with_database`], [`AppState::with_snapshot_store`]
    /// etc.) to populate the credential-carrying fields — direct
    /// field assignment is blocked by the A5/A6/A6b `pub(crate)`
    /// restrictions.
    ///
    /// Production code uses [`AppState::from_config`], not this.
    pub fn new_fixture(config: SandboxConfig, backend: Backend) -> Self {
        Self {
            config,
            sandboxes: SandboxRegistry::new(),
            backend,
            mint_rate_limiter: Some(MintRateLimiter::new()),
            database: None,
            persist: None,
            shutdown: Arc::new(AtomicBool::new(false)),
            admin_token: None,
            snapshot_store: None,
            ch_remote: None,
            restore_backend: None,
        }
    }
}

impl AppState {
    /// Build a fresh state from config. Probes the chosen backend at
    /// construction time so callers fail fast on misconfiguration,
    /// then cleans up any in-cluster runtime objects orphaned by a
    /// previous controller process (the per-sandbox signing keys
    /// only live in process memory; orphan Pods would 401 every
    /// signed request from the new controller forever).
    pub async fn from_config(config: SandboxConfig) -> Result<Arc<Self>, String> {
        // Build the shared persistence handle FIRST so the same `Arc`
        // can be cloned into both the backend (for seal-on-create /
        // delete-on-stop) and the boot-restore loop below (for the
        // initial directory walk). `from_env` returns `None` when
        // `SANDBOX_PERSIST_AUTH` is not `1` — the disabled shape both
        // for the backend and the restore call. Failures here are
        // fail-fast: an operator who set the flag with a missing /
        // wrong-mode key file wants to see that at boot, not silently
        // run with persistence off.
        let persist: Option<Arc<Persistence>> =
            Persistence::from_env()?.map(Arc::new);

        // Phase-0 pg-backed state: build BEFORE the backend probe so
        // the schema reaches the right version before any backend op
        // could try to write. Pg-required features stay dormant in
        // Phase 0 — the handle is plumbed-but-unused; Phase 1 wires
        // call sites. (`docs/proposals/sandbox-pg-state.md` § 15
        // Phase 0.)
        let database: Option<Arc<Database>> = match Database::from_env().await {
            Ok(opt) => opt.map(Arc::new),
            Err(e) => {
                // Dev escape hatch: SANDBOX_PG_OPTIONAL=1 (D-11)
                // logs and continues with pg disabled. Production
                // refuses to boot; the operator must fix the
                // config.
                if matches!(std::env::var("SANDBOX_PG_OPTIONAL").as_deref(), Ok("1")) {
                    tracing::warn!(
                        error = %e,
                        "SANDBOX_PG_OPTIONAL=1: Database::from_env failed; pg integration disabled"
                    );
                    None
                } else {
                    return Err(format!("Database::from_env: {e}"));
                }
            }
        };

        if let Some(db) = &database {
            // Block boot until the schema is at the version this
            // binary was built against. A designated migrator
            // applies pending migrations; everyone else polls.
            // Failure aborts startup.
            if let Err(e) = db
                .ensure_schema_at_version(LATEST_MIGRATION_VERSION)
                .await
            {
                if matches!(std::env::var("SANDBOX_PG_OPTIONAL").as_deref(), Ok("1")) {
                    tracing::warn!(
                        error = %e,
                        target_version = LATEST_MIGRATION_VERSION,
                        "SANDBOX_PG_OPTIONAL=1: ensure_schema_at_version failed; \
                         pg integration disabled"
                    );
                } else {
                    return Err(format!(
                        "ensure_schema_at_version({LATEST_MIGRATION_VERSION}): {e}"
                    ));
                }
            } else {
                tracing::info!(
                    target_version = LATEST_MIGRATION_VERSION,
                    host_id = %db.host_id(),
                    "sandbox pg: schema ready"
                );
            }
        }

        let backend = Backend::from_config_with_persist(&config, persist.clone())?;
        backend.probe().await?;
        // Clean up orphan Pods + ConfigMaps from a previous run.
        // Errors here are non-fatal — operators may want to keep
        // orphans around for forensics; we log and continue.
        match backend.cleanup_orphans_at_startup().await {
            Ok(0) => {}
            Ok(n) => tracing::info!(orphans = n, "sandbox startup cleanup: removed orphans"),
            Err(e) => tracing::warn!(error = %e, "sandbox startup cleanup failed (non-fatal)"),
        }
        let registry = SandboxRegistry::new();

        // Sealed-record restore (preview-URL § II.0 §4 + § II.5).
        // Reuses the shared `persist` handle built above so we don't
        // re-open the AEAD key file or re-read the env. `None` is the
        // disabled/no-op shape — feature-flagged behind
        // `SANDBOX_PERSIST_AUTH=1`; default OFF.
        // Round-8 Phase-1: restart-restore is pg-driven. We need both
        // a Database handle (for the host-scoped query) and a
        // Persistence handle (for the sealed-record secret material).
        // When either is missing, restore is skipped — the controller
        // boots empty.
        if let (Some(db), Some(p)) = (&database, &persist) {
            let dir = p.persist_dir();
            tracing::info!(
                persist_dir = ?dir,
                host_id = %db.host_id(),
                "sandbox persist: pg + sealed restore starting"
            );
            match restore::restore_at_startup(
                db.as_ref(),
                &dir,
                p.aead_key(),
                &backend,
                &registry,
                restore::DEFAULT_PROBE_TIMEOUT,
            )
            .await
            {
                Ok(s) => tracing::info!(
                    seen = s.records_seen,
                    restored = s.restored,
                    mismatched = s.mismatched,
                    unreachable = s.unreachable,
                    corrupt = s.corrupt,
                    seal_missing = s.seal_missing,
                    unsupported = s.unsupported,
                    orphans_unlinked = s.orphans_unlinked,
                    "sandbox persist: restore done"
                ),
                Err(e) => tracing::warn!(
                    error = %e,
                    "sandbox persist: restore_at_startup IO failure (non-fatal)"
                ),
            }
        }
        // Round-3 / Phase-3 CRITICAL #3: read admin bearer ONCE at
        // boot. Previously every `/admin/*` request stat()+read()'d
        // the file (slow-FS DoS amplification; fail-open on chmod
        // error). Boot-time read is fail-loud — a misconfigured
        // mode (anything other than 0o400) refuses to start the
        // process. "Disabled because env-unset" stays `None`; the
        // file existing-but-misconfigured is `Err`.
        //
        // Round-4 / MINOR #4: env resolution happens here, then the
        // pure `load_admin_token` reads/validates the path. Tests can
        // call `load_admin_token(Some(&path))` directly without
        // mutating process env (the env-mutation tests stay only on
        // the production wiring at `from_env`-shaped boundaries).
        let admin_token_path = std::env::var("SANDBOX_ADMIN_TOKEN_PATH")
            .ok()
            .and_then(|v| {
                let t = v.trim();
                if t.is_empty() { None } else { Some(std::path::PathBuf::from(t)) }
            });
        let admin_token = load_admin_token(admin_token_path.as_deref())?
            .map(zeroize::Zeroizing::new);

        // Phase-A snapshot/restore wiring. When `snapshot_enabled =
        // true`, construct the production trio:
        //   - LocalDiskSnapshotStore at config.snapshot_l1_root
        //     (optionally tier-wrapped with GcsSnapshotStore when
        //     config.snapshot_use_gcs = true)
        //   - RealChRemoteClient (resolves `ch-remote` on PATH at
        //     construct time)
        //   - RealRestoreBackend (Nomad submit + poll)
        // When `snapshot_enabled = false` we leave them `None`;
        // admin handlers see `None` and return 501 feature_disabled.
        let (snapshot_store, ch_remote, restore_backend): (
            Option<Arc<dyn SnapshotStore>>,
            Option<Arc<dyn ChRemoteClient>>,
            Option<Arc<dyn RestoreBackend>>,
        ) = if config.snapshot_enabled {
            let l1_root = config.snapshot_l1_root.clone();
            // Idempotent — directory may already exist.
            if let Err(e) = std::fs::create_dir_all(&l1_root) {
                tracing::warn!(
                    path = %l1_root.display(),
                    error = %e,
                    "snapshot wiring: L1 root mkdir failed (will surface on first put)"
                );
            }
            let l1 = LocalDiskSnapshotStore::new(l1_root.clone());
            let store: Arc<dyn SnapshotStore> = if config.snapshot_use_gcs {
                let bucket = config
                    .snapshot_gcs_bucket
                    .clone()
                    .expect("SANDBOX_SNAPSHOT_GCS_BUCKET must be set when use_gcs=true (validated at config parse)");
                let l2 = GcsSnapshotStore::new(bucket.clone(), "default");
                tracing::info!(
                    l1_root = %l1_root.display(),
                    gcs_bucket = %bucket,
                    "snapshot wiring: tiered L1+GCS"
                );
                Arc::new(TieredSnapshotStore::new(l1, l2))
            } else {
                tracing::info!(
                    l1_root = %l1_root.display(),
                    "snapshot wiring: L1 disk-only"
                );
                Arc::new(l1)
            };
            let ch: Arc<dyn ChRemoteClient> =
                Arc::new(RealChRemoteClient::new());
            let rb: Arc<dyn RestoreBackend> =
                Arc::new(RealRestoreBackend::new(
                    config.nomad_ch.clone(),
                    config.memory_mb,
                    config.cpus,
                ));
            tracing::info!(
                ch_version = ch.version(),
                kek_path = ?config.snapshot_root_kek_path,
                "snapshot/restore wiring: enabled"
            );
            (Some(store), Some(ch), Some(rb))
        } else {
            (None, None, None)
        };

        let state = Arc::new(Self {
            config,
            sandboxes: registry,
            backend,
            mint_rate_limiter: Some(MintRateLimiter::new()),
            database,
            persist,
            shutdown: Arc::new(AtomicBool::new(false)),
            admin_token,
            snapshot_store,
            ch_remote,
            restore_backend,
        });
        // Background re-probe so /readyz reflects current backend
        // state. Without this, the `is_healthy()` flag is set once
        // at boot and stays true even if kubectl auth expires or
        // the cluster goes unreachable.
        start_health_loop(state.clone());

        // Phase-2 HA: register THIS controller's host row before
        // spawning the heartbeat task. Without this, the FK on
        // `sandbox.sandboxes.host_id REFERENCES sandbox.hosts(host_id)`
        // rejects every sandbox INSERT and the heartbeat UPDATE
        // affects 0 rows forever (peers eventually see `last_heartbeat`
        // way in the past and would treat us as dead — except there's
        // no row, so `dead_hosts()` skips us too: silent corruption).
        // `upsert_host` is INSERT … ON CONFLICT DO UPDATE so a clean
        // restart with the same persisted host_id refreshes the row
        // rather than failing. Failure aborts boot — without a host
        // row, no sandbox op can succeed; better to fail loud at boot.
        // Hostname source: `SANDBOX_HOSTNAME` env (operator override)
        // → `/proc/sys/kernel/hostname` (Linux) → "unknown". Region is
        // read by `upsert_host` itself from `SANDBOX_REGION`. The
        // backend label comes from the live `Backend` instance, so it
        // matches the CHECK constraint by construction.
        if let Some(db) = state.database.as_ref() {
            let hostname = read_hostname();
            let backend_name = state.backend.name();
            if let Err(e) = db.upsert_host(&hostname, backend_name).await {
                return Err(format!("upsert_host({hostname}, {backend_name}): {e}"));
            }
            tracing::info!(
                hostname = %hostname,
                backend = %backend_name,
                host_id = %db.host_id(),
                "sandbox HA: host row registered"
            );
        }

        // Phase-2 HA: periodic heartbeat task — UPDATEs
        // `sandbox.hosts.last_heartbeat` so peers can tell whether
        // we're alive. The task self-runs forever; failure is
        // logged-and-continued (next tick retries).
        if state.database.is_some() {
            spawn_heartbeat_task(state.clone());
        }

        // Phase-2 HA: takeover task — gated behind
        // `SANDBOX_HA_AUTO_TAKEOVER=1` (default off; v2-of-v2 per
        // § 11.1: operators opt in once heartbeats are reliable).
        if state.database.is_some()
            && matches!(std::env::var("SANDBOX_HA_AUTO_TAKEOVER").as_deref(), Ok("1"))
        {
            spawn_takeover_task(state.clone());
        }

        // PR 3g: snapshot/restore sweep tasks (transient-state
        // takeover + idle eviction). Both observe
        // `state.shutdown_requested()` between iterations. The
        // transient sweep runs unconditionally so a feature-flipped-
        // on-then-off deploy still recovers in-flight transients;
        // the idle-eviction sweep self-disables when
        // `snapshot_enabled = false` or the threshold env is 0.
        if state.database.is_some() {
            sweep::spawn_transient_state_takeover(state.clone());
        }
        // T6: auto-spawn idle eviction sweep. Production now has all
        // deps wired (snapshot_store + ch_remote + restore_backend
        // populated above, Backend::lookup_source_vm_ops async lookup
        // is in tree). `ControllerIdleSnapshotter` bridges the loop's
        // `IdleSnapshotter` trait to `snapshot_handler::snapshot_sandbox`
        // + post-snapshot teardown — same chain the admin endpoint
        // drives. Gate: requires database + snapshot_enabled +
        // SANDBOX_IDLE_SNAPSHOT_SECS > 0; `spawn_idle_eviction_sweep`
        // re-checks all three internally and bails out cleanly.
        if state.database.is_some() && state.config.snapshot_enabled {
            let snapshotter: Arc<dyn sweep::IdleSnapshotter> = Arc::new(
                sweep::ControllerIdleSnapshotter::new(state.clone()),
            );
            sweep::spawn_idle_eviction_sweep(state.clone(), snapshotter);
        }
        Ok(state)
    }
}

/// Round-3 / Phase-3 CRITICAL #3: read the admin bearer ONCE at
/// boot. Mirrors `Persistence::AeadKey::from_path`.
///
/// Round-4 / MINOR #4: this is now a pure function over an optional
/// path. The production caller in `AppState::from_config` resolves
/// `SANDBOX_ADMIN_TOKEN_PATH` first then passes the path here, so
/// tests can drive the loader with a `tempfile::NamedTempFile` path
/// without mutating process env.
///
/// Three outcomes:
///   - `path = None` → `Ok(None)` (admin API disabled by config)
///   - file readable + mode 0o400 + non-empty → `Ok(Some(token))`
///   - ANYTHING else → `Err(...)` (refuse to boot loudly)
///
/// Distinguishes "admin API disabled" (legitimate config) from
/// "admin token misconfigured" (operator error) — Round-2 leaked
/// the latter as a silent fail-open via `.ok()?` on metadata().
pub(crate) fn load_admin_token(
    path: Option<&std::path::Path>,
) -> Result<Option<String>, String> {
    let Some(path) = path else {
        return Ok(None);
    };

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let meta = std::fs::metadata(path).map_err(|e| {
            format!("SANDBOX_ADMIN_TOKEN_PATH={path:?}: stat: {e}")
        })?;
        let mode = meta.permissions().mode() & 0o777;
        if mode != 0o400 {
            return Err(format!(
                "SANDBOX_ADMIN_TOKEN_PATH={path:?}: mode={mode:o} must be 0o400"
            ));
        }
    }

    let raw = std::fs::read_to_string(path).map_err(|e| {
        format!("SANDBOX_ADMIN_TOKEN_PATH={path:?}: read: {e}")
    })?;
    let trimmed = raw
        .trim_end_matches(|c: char| c == '\n' || c == '\r')
        .to_string();
    if trimmed.is_empty() {
        return Err(format!(
            "SANDBOX_ADMIN_TOKEN_PATH={path:?}: file is empty"
        ));
    }
    Ok(Some(trimmed))
}

/// Periodic backend probe. `probe()` updates the `healthy` flag
/// that `/readyz` exposes; without this loop the flag is set once
/// at boot and stays stale forever (e.g. true after kubectl auth
/// has expired). Re-probes every 30s on a detached compio task.
///
/// We don't wrap async calls in `catch_unwind` — `probe` is
/// designed to return `Result`, not panic. If it does panic the
/// task dies and re-probes stop; that's a real bug worth crashing
/// loudly rather than papering over.
fn start_health_loop(state: Arc<AppState>) {
    compio::runtime::spawn(async move {
        loop {
            // Round-1 fixer / IMPORTANT #8: top-of-loop shutdown
            // check. The previous iteration's sleep will have
            // returned by now; if shutdown was signalled during it,
            // exit before the next probe.
            if state.shutdown_requested() {
                tracing::info!("sandbox health loop: shutdown requested; exiting");
                break;
            }
            compio::time::sleep(Duration::from_secs(30)).await;
            if state.shutdown_requested() {
                break;
            }
            if let Err(e) = state.backend.probe().await {
                tracing::warn!(error = %e, "sandbox health re-probe failed");
            }
        }
    })
    .detach();
}

// ────────────────────────────────────────────────────────────────────
// Phase 2 — periodic heartbeat + lease-based takeover
// ────────────────────────────────────────────────────────────────────

/// `SANDBOX_HA_HEARTBEAT_SECS`. Cadence at which the heartbeat task
/// UPDATEs `sandbox.hosts.last_heartbeat`. Default 5 s; validated at
/// boot to be > 0 and `lease_ttl >= 4 × heartbeat` (R-NN, § 11.3).
const DEFAULT_HEARTBEAT_SECS: u64 = 5;

/// `SANDBOX_HA_TAKEOVER_POLL_SECS`. Cadence at which the takeover
/// task scans for dead peers. Default 30 s per § 11.3.
const DEFAULT_TAKEOVER_POLL_SECS: u64 = 30;

/// `SANDBOX_HA_LEASE_TTL_SECS`. The grace window per § 11.1:
/// `now() - last_heartbeat > lease_ttl` means the peer is considered
/// dead. Default 60 s. Validated at boot.
const DEFAULT_LEASE_TTL_SECS: u64 = 60;

/// Best-effort hostname for `sandbox.hosts.hostname`. Operators can
/// override via `SANDBOX_HOSTNAME` (useful in containers where the
/// kernel hostname is the random container ID). Falls back to
/// `/proc/sys/kernel/hostname` on Linux, then `"unknown"`. The column
/// has no CHECK constraint — only NOT NULL — so any non-empty value
/// is acceptable.
fn read_hostname() -> String {
    if let Ok(v) = std::env::var("SANDBOX_HOSTNAME") {
        let t = v.trim();
        if !t.is_empty() {
            return t.to_string();
        }
    }
    if let Ok(s) = std::fs::read_to_string("/proc/sys/kernel/hostname") {
        let t = s.trim();
        if !t.is_empty() {
            return t.to_string();
        }
    }
    "unknown".to_string()
}

fn read_u64_env(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(default)
}

/// Periodic heartbeat task (§ 11.3). Calls
/// [`crate::db::Database::heartbeat`] every `SANDBOX_HA_HEARTBEAT_SECS`
/// seconds. On `Err`, logs `warn!` and continues — a transient pg
/// blip should not crash the controller; the next tick will retry.
///
/// Detached on the compio runtime; the task lives as long as the
/// process. The `Arc<AppState>` keeps the database handle alive.
///
/// **Safety against drift:** the loop sleeps `interval` AFTER each
/// heartbeat, so a slow pg call delays the *next* heartbeat by the
/// query duration but does not double-up. If the pg call itself
/// hangs longer than `lease_ttl`, peers will fairly mark this host
/// dead — which is the lease semantics by design.
pub fn spawn_heartbeat_task(state: Arc<AppState>) {
    compio::runtime::spawn(async move {
        let secs = read_u64_env("SANDBOX_HA_HEARTBEAT_SECS", DEFAULT_HEARTBEAT_SECS).max(1);
        let interval = Duration::from_secs(secs);
        let Some(db) = state.database.clone() else {
            return;
        };
        tracing::info!(
            interval_secs = secs,
            host_id = %db.host_id(),
            "sandbox HA: heartbeat task started"
        );
        loop {
            // Round-1 fixer / IMPORTANT #8: shutdown check.
            if state.shutdown_requested() {
                tracing::info!(
                    host_id = %db.host_id(),
                    "sandbox HA: heartbeat task shutting down cleanly"
                );
                break;
            }
            compio::time::sleep(interval).await;
            if state.shutdown_requested() {
                break;
            }
            match db.heartbeat().await {
                Ok(()) => {}
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        host_id = %db.host_id(),
                        "sandbox HA: heartbeat write failed; will retry next tick"
                    );
                }
            }
        }
    })
    .detach();
}

/// Round-1 fixer / CRITICAL #4: post-takeover registry rehydrate.
///
/// For each newly-owned sandbox the takeover SQL produced, run the
/// boot-time probe-and-register pipeline against the local sealed
/// record. The new owner ends up with an in-memory registry entry
/// (or the row is marked recreating / unreachable / lost based on
/// the probe result, exactly the way `restore_at_startup`'s loop
/// classifies things — code path is shared via
/// `restore::probe_and_register_one`).
///
/// Phase-2 v1 limitation: the new owner's persist dir might not
/// have the sealed record (cross-host sealed sync ships in Phase
/// 3+). When the seal is missing, the row is marked `lost` and we
/// bump `sandbox_ha_takeover_orphan_total`.
///
/// **Does nothing** when:
///   - state.persist is None (controller booted with persistence
///     off — e.g. SANDBOX_PERSIST_AUTH != 1). Without sealed
///     records there's no way to recover the signing key, so the
///     row stays `running` in pg and the operator will see a
///     stale-row alert via the Phase-3 admin tooling. This is the
///     same behaviour as the pre-fix code, just with the no-op
///     made explicit.
///   - state.database is None (covered upstream by the
///     `state.database.is_some()` gate that spawned the task).
async fn rehydrate_after_takeover(
    state: &Arc<AppState>,
    taken: &[crate::db::TakenSandbox],
) {
    let Some(db) = state.database.as_ref() else {
        return;
    };
    let Some(persist) = state.persist.as_ref() else {
        tracing::warn!(
            taken_count = taken.len(),
            "sandbox HA: takeover rehydrate skipped (persist disabled); rows owned but not in-memory"
        );
        // Each taken sandbox is effectively orphan-owned until the
        // operator restarts with persistence enabled. Mark them as
        // such so the metric reflects reality.
        for _ in taken {
            metrics::inc_takeover_orphan();
        }
        return;
    };
    let persist_dir = persist.persist_dir();
    let aead_key = persist.aead_key();

    for ts in taken {
        // Parse the typed-id back to its embedded UUID for the
        // get_sandbox_row lookup.
        let sandbox_uuid = match zeroship_core::typed_id::parse_with_prefix(
            &ts.sandbox_id,
            "sbx",
        ) {
            Ok(u) => u,
            Err(e) => {
                tracing::warn!(
                    sandbox_id = %ts.sandbox_id,
                    error = %e,
                    "sandbox HA: takeover rehydrate skipped (bad typed-id)"
                );
                continue;
            }
        };
        // Fetch the full row.
        let row = match db.get_sandbox_row(sandbox_uuid).await {
            Ok(Some(r)) => r,
            Ok(None) => {
                // Tombstoned mid-takeover (or the row got DELETEd by
                // someone else). Nothing to rehydrate.
                tracing::info!(
                    sandbox_id = %ts.sandbox_id,
                    "sandbox HA: takeover rehydrate skipped (row gone)"
                );
                continue;
            }
            Err(e) => {
                tracing::warn!(
                    sandbox_id = %ts.sandbox_id,
                    error = %e,
                    "sandbox HA: takeover rehydrate row-fetch failed; will retry next scan"
                );
                continue;
            }
        };

        let outcome = restore::probe_and_register_one(
            db,
            &persist_dir,
            aead_key,
            &state.backend,
            &state.sandboxes,
            &row,
            restore::DEFAULT_PROBE_TIMEOUT,
        )
        .await;
        match outcome {
            restore::RestoreOutcome::Restored => {
                tracing::info!(
                    sandbox_id = %ts.sandbox_id,
                    generation = ts.generation,
                    "sandbox HA: takeover rehydrate restored to in-memory registry"
                );
            }
            restore::RestoreOutcome::SealMissing => {
                tracing::warn!(
                    sandbox_id = %ts.sandbox_id,
                    "sandbox HA: takeover rehydrate seal missing on this host (Phase 3+ adds cross-host sync)"
                );
                metrics::inc_takeover_orphan();
            }
            // Round-2 fixer / MINOR #3: pre-fix every non-Restored,
            // non-SealMissing outcome was logged at info but no
            // metric fired. Operators couldn't rate(...) over
            // takeover-but-then-mismatched / unreachable / corrupt
            // events. Today each outcome bumps a labelled counter.
            restore::RestoreOutcome::Mismatched => {
                tracing::warn!(
                    sandbox_id = %ts.sandbox_id,
                    "sandbox HA: takeover rehydrate fp mismatch / 401"
                );
                metrics::inc_takeover_mismatched();
            }
            restore::RestoreOutcome::Unreachable => {
                tracing::warn!(
                    sandbox_id = %ts.sandbox_id,
                    "sandbox HA: takeover rehydrate agent unreachable"
                );
                metrics::inc_takeover_unreachable();
            }
            restore::RestoreOutcome::Corrupt => {
                tracing::warn!(
                    sandbox_id = %ts.sandbox_id,
                    "sandbox HA: takeover rehydrate corrupt seal / typed-id / backend restore failed"
                );
                metrics::inc_takeover_corrupt();
            }
            restore::RestoreOutcome::BackendUnsupported => {
                tracing::info!(
                    sandbox_id = %ts.sandbox_id,
                    "sandbox HA: takeover rehydrate skipped (backend doesn't support restore)"
                );
            }
        }
    }
}

/// Periodic peer-scan task (§ 11.3). Every
/// `SANDBOX_HA_TAKEOVER_POLL_SECS` seconds:
///
/// 1. Refresh the `sandbox_ha_heartbeat_lag_seconds` gauge (and
///    fire `sandbox_ha_clock_rewind_total` if pg sees `now() -
///    last_heartbeat < 0` — § 12 R-MM).
/// 2. Read `dead_hosts(lease_ttl)`.
/// 3. For each dead host (excluding self — defensive), issue the
///    CAS-guarded takeover UPDATE per § 11.2.
/// 4. Bump `sandbox_ha_takeover_total{reason="lease_expiration"}`
///    by the number of rows successfully reclaimed.
///
/// The task exits cleanly only on process shutdown; transient
/// errors are logged and the loop continues. The takeover write is
/// a single SQL statement plus a host-status flip in the same
/// transaction (§ 11.2).
///
/// Per § 15 Phase 2: the in-memory map for newly-owned sandboxes
/// is not yet populated by this scaffold — the controller would
/// need to probe `/version` with the persisted signing_key before
/// registering. That probe pipeline reuses `crate::restore` and is
/// the next deliverable to flesh out (out-of-scope for this commit
/// to keep the takeover write atomic and tested).
pub fn spawn_takeover_task(state: Arc<AppState>) {
    compio::runtime::spawn(async move {
        let poll_secs = read_u64_env(
            "SANDBOX_HA_TAKEOVER_POLL_SECS",
            DEFAULT_TAKEOVER_POLL_SECS,
        )
        .max(1);
        let lease_ttl_secs = read_u64_env("SANDBOX_HA_LEASE_TTL_SECS", DEFAULT_LEASE_TTL_SECS);
        let interval = Duration::from_secs(poll_secs);
        let Some(db) = state.database.clone() else {
            return;
        };
        let my_host = db.host_id();
        let my_host_typed = format!(
            "hst_{}",
            zeroship_core::typed_id::uuid_to_base62(&my_host)
        );
        tracing::info!(
            poll_secs,
            lease_ttl_secs,
            host_id = %my_host,
            "sandbox HA: takeover task started"
        );

        loop {
            // Round-1 fixer / IMPORTANT #8: shutdown check.
            if state.shutdown_requested() {
                tracing::info!(
                    host_id = %my_host,
                    "sandbox HA: takeover task shutting down cleanly"
                );
                break;
            }
            compio::time::sleep(interval).await;
            if state.shutdown_requested() {
                break;
            }

            // Refresh heartbeat-lag gauge + clock-rewind detector.
            match db.heartbeat_lag_seconds().await {
                Ok(Some(lag)) => {
                    metrics::set_heartbeat_lag(lag);
                    if lag < 0.0 {
                        // R-MM: pg-side clock rewind. Healthy fleet
                        // never sees this. Loud-log + counter so an
                        // alert fires.
                        tracing::error!(
                            host_id = %my_host,
                            lag_secs = lag,
                            "sandbox HA: clock-rewind detected (now() - last_heartbeat < 0)"
                        );
                        metrics::inc_clock_rewind();
                    }
                }
                Ok(None) => {
                    tracing::warn!(
                        host_id = %my_host,
                        "sandbox HA: own host row missing; takeover skipped this round"
                    );
                    continue;
                }
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        host_id = %my_host,
                        "sandbox HA: heartbeat_lag read failed; takeover skipped this round"
                    );
                    continue;
                }
            }

            // Find dead peers.
            let dead = match db.dead_hosts(lease_ttl_secs).await {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        host_id = %my_host,
                        "sandbox HA: dead_hosts scan failed; will retry"
                    );
                    continue;
                }
            };
            if dead.is_empty() {
                continue;
            }
            // Filter self out as a defensive measure — `dead_hosts`
            // can include our own row only if the heartbeat task is
            // wedged for > lease_ttl, in which case taking over our
            // own sandboxes would still self-fence on the next CAS
            // miss. Cleaner to skip than to take and unwind.
            let real_dead: Vec<&String> =
                dead.iter().filter(|h| h.as_str() != my_host_typed.as_str()).collect();
            metrics::add_dead_hosts_observed(real_dead.len() as u64);

            for dead_host in real_dead {
                // Round-1 fixer / IMPORTANT #8: shutdown check
                // inside the inner loop so a slow takeover scan
                // doesn't ignore the flag while iterating dead
                // peers.
                if state.shutdown_requested() {
                    break;
                }
                match db
                    .takeover_sandboxes_from_host(dead_host, my_host, lease_ttl_secs)
                    .await
                {
                    Ok(taken) => {
                        if !taken.is_empty() {
                            metrics::add_takeover_lease_expiration(taken.len() as u64);
                            tracing::info!(
                                dead_host = %dead_host,
                                taken_count = taken.len(),
                                host_id = %my_host,
                                "sandbox HA: takeover succeeded"
                            );
                            // Round-1 fixer / CRITICAL #4: Phase 2.5
                            // post-takeover rehydrate. For each taken
                            // sandbox, fetch the full pg row, run the
                            // probe-and-classify pipeline against the
                            // new owner's local sealed record, and
                            // populate the in-memory registry. Without
                            // this, every HTTP request to a taken
                            // sandbox 404s until the next controller
                            // boot reads `restore_at_startup`.
                            rehydrate_after_takeover(&state, &taken).await;
                        }
                    }
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            dead_host = %dead_host,
                            host_id = %my_host,
                            "sandbox HA: takeover UPDATE failed; will retry"
                        );
                    }
                }
            }
        }
    })
    .detach();
}

// ────────────────────────────────────────────────────────────────────
// Round-1 fixer / IMPORTANT #8 — shutdown-flag unit tests.
// ────────────────────────────────────────────────────────────────────
//
// Round-2 fixer / CRITICAL #3: the binary's main loop now wires
// SIGINT/SIGTERM → ntex `run()` returns → `trigger_shutdown()` →
// `SANDBOX_HA_DRAIN_GRACE_SECS` (default 30s) of grace for the
// background tasks to observe the flag and exit. This is a
// post-drain shutdown: peers see the `'draining'` host status only
// AFTER ntex has finished draining HTTP. A pre-drain notification
// would require a signal handler that runs BEFORE ntex's, which
// compio doesn't yet expose; deferred follow-up.
//
// What we test here at the lib level:
//
//   1. A fixture task that polls `shutdown_requested()` exits within
//      one iteration of the flag flipping. The fixture mirrors the
//      heartbeat-task shape: top-of-loop check + sleep + post-sleep
//      check.
//
// The pg-side `set_host_draining` UPDATE is exercised by the
// pg-gated integration test in tests/sandbox_pg_e2e.rs.
// ────────────────────────────────────────────────────────────────────
// Round-4 / MINOR #4 — boot-loader unit tests (pure function, no env
// mutation; we just feed a path to the loader).
// ────────────────────────────────────────────────────────────────────
#[cfg(test)]
mod boot_loader_tests {
    use super::*;
    use std::path::PathBuf;
    use uuid::Uuid;

    fn temp_path() -> PathBuf {
        std::env::temp_dir().join(format!(
            "zsbx-admin-token-{}-{}",
            std::process::id(),
            Uuid::now_v7().simple()
        ))
    }

    fn write_with_mode(path: &std::path::Path, body: &str, mode: u32) {
        std::fs::write(path, body).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
        }
        // On non-unix the mode arg is ignored.
        let _ = mode;
    }

    fn cleanup(path: &std::path::Path) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
        }
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn loader_returns_none_when_path_is_none() {
        let got = load_admin_token(None);
        assert!(matches!(got, Ok(None)), "None path must yield Ok(None); got {got:?}");
    }

    #[test]
    fn loader_reads_token_when_mode_0o400() {
        let path = temp_path();
        let token = "boot-loader-token-0o400-aaaa";
        write_with_mode(&path, token, 0o400);
        let got = load_admin_token(Some(&path));
        cleanup(&path);
        assert!(matches!(&got, Ok(Some(s)) if s == token), "got {got:?}");
    }

    #[cfg(unix)]
    #[test]
    fn loader_refuses_bad_mode() {
        let path = temp_path();
        write_with_mode(&path, "boot-loader-token-bad-mode-aa", 0o644);
        let got = load_admin_token(Some(&path));
        cleanup(&path);
        let err = got.expect_err("mode 0o644 must yield Err");
        assert!(err.contains("0o400"), "error must mention required mode; got {err}");
    }

    #[test]
    fn loader_refuses_empty_file() {
        let path = temp_path();
        write_with_mode(&path, "", 0o400);
        let got = load_admin_token(Some(&path));
        cleanup(&path);
        let err = got.expect_err("empty file must yield Err");
        assert!(err.contains("empty"), "error must mention 'empty'; got {err}");
    }

    #[test]
    fn loader_trims_trailing_newline() {
        let path = temp_path();
        let token = "trim-newline-token-bbbb";
        write_with_mode(&path, &format!("{token}\n"), 0o400);
        let got = load_admin_token(Some(&path));
        cleanup(&path);
        assert!(matches!(&got, Ok(Some(s)) if s == token), "got {got:?}");
    }
}

// ────────────────────────────────────────────────────────────────────
// A5 (api-surface-2026-05-24-r1) — `AppState::with_admin_token`
// builder semantics. Constructs a minimal in-crate state (where
// `admin_token` is visible) and asserts the empty-string rejection
// that turns the post-Round-4 footgun into a `Result::Err`.
// ────────────────────────────────────────────────────────────────────
#[cfg(test)]
mod admin_token_setter_tests {
    use super::*;
    use crate::backend::Backend;
    use crate::config::{ApiToken, K8sConfig, NomadCHConfig, SandboxConfig};

    /// Minimal config that satisfies `Backend::from_config` for the
    /// nomad-ch backend WITHOUT touching the network — the builder
    /// only needs `SandboxConfig` to populate the field; no probe
    /// runs here.
    fn min_cfg() -> SandboxConfig {
        SandboxConfig {
            port: 9091,
            token: ApiToken::new("ignored-creator-token"),
            backend: "nomad-ch".into(),
            image: "img".into(),
            workspace_root: std::path::PathBuf::from("/var/zeroship/projects"),
            network: "n".into(),
            memory_mb: 1024,
            cpus: 2.0,
            idle_timeout_secs: 1800,
            max_lifetime_secs: 28800,
            auto_pull: false,
            k8s: K8sConfig {
                namespace: "default".into(),
                image: "i".into(),
                runtime_class: "kvm-sandbox".into(),
                ready_timeout_secs: 120,
                use_port_forward: false,
                port_forward_start: 18000,
                user_home_size: "5Gi".into(),
                user_home_storage_class: None,
                startup_orphan_cleanup: false,
            },
            nomad_ch: NomadCHConfig {
                nomad_addr: "http://127.0.0.1:4646".into(),
                datacenter: "dc1".into(),
                wrapper_path: std::path::PathBuf::from(
                    "/etc/zeroship/nomad-vm-wrapper.sh",
                ),
                runtime_dir: std::path::PathBuf::from("/var/lib/zeroship/ch"),
                host_state_dir: std::path::PathBuf::from("/var/zeroship/ch"),
                user_home_dir_root: std::path::PathBuf::from(
                    "/var/zeroship/ch/users",
                ),
                vm_index_floor: 1,
                vm_index_ceil: 200,
                alloc_running_timeout_secs: 60,
                agent_livez_timeout_secs: 30,
                host_fence_timeout_secs: 30,
                startup_orphan_cleanup: false,
                subnet_second_octet: 99,
            },
            create_retry_max: 2,
            create_retry_total_timeout_secs: 90,
            snapshot_enabled: false,
            snapshot_l1_root: std::path::PathBuf::from(
                "/var/zeroship/ch/snapshots",
            ),
            snapshot_use_gcs: false,
            snapshot_gcs_bucket: None,
            snapshot_root_kek_path: None,
            workspace_image_size_gb: 20,
        }
    }

    fn min_state() -> AppState {
        let cfg = min_cfg();
        let backend = Backend::from_config(&cfg).expect("backend");
        AppState::new_fixture(cfg, backend)
    }

    #[test]
    fn admin_token_setter_rejects_empty() {
        let state = min_state();
        match state.with_admin_token(Some(String::new())) {
            Ok(_) => panic!("empty string must yield Err"),
            Err(e) => assert!(
                e.contains("empty"),
                "error message must mention 'empty'; got {e}"
            ),
        }
    }

    #[test]
    fn admin_token_setter_accepts_non_empty() {
        let state = min_state();
        let state = match state
            .with_admin_token(Some("operator-bearer-abcdef".into()))
        {
            Ok(s) => s,
            Err(e) => panic!("non-empty string must be accepted: {e}"),
        };
        assert_eq!(
            state.admin_token(),
            Some("operator-bearer-abcdef"),
            "the reader must surface the wrapped token"
        );
    }

    #[test]
    fn admin_token_setter_none_clears_field() {
        // Set then clear — exercises the two-call flow that test
        // fixtures use when toggling the bearer between cases.
        let state = min_state();
        let state = match state
            .with_admin_token(Some("bearer-to-clear-aaaa".into()))
        {
            Ok(s) => s,
            Err(e) => panic!("non-empty accepted: {e}"),
        };
        assert!(state.admin_token().is_some(), "precondition: set");
        let state = match state.with_admin_token(None) {
            Ok(s) => s,
            Err(e) => panic!("None clears unconditionally: {e}"),
        };
        assert!(state.admin_token().is_none(), "None must clear the field");
    }
}

// ────────────────────────────────────────────────────────────────────
// A6 (api-surface-2026-05-24-r1) — `AppState::with_persistence`
// builder semantics. Mirrors the A5 admin-token tests above. The
// field is `pub(crate)` so out-of-crate callers must route the
// `Arc<Persistence>` through the builder; these tests assert that
// the builder accepts the handle and that a subsequent call
// replaces the previous one (so a re-wired test fixture doesn't
// silently keep a stale persist).
// ────────────────────────────────────────────────────────────────────
#[cfg(test)]
mod persist_setter_tests {
    use super::*;
    use crate::backend::Backend;
    use crate::config::{ApiToken, K8sConfig, NomadCHConfig, SandboxConfig};
    use crate::persist::{AeadKey, Persistence};

    /// Minimal config that satisfies `Backend::from_config` for the
    /// nomad-ch backend WITHOUT touching the network. Mirrors the
    /// fixture in `admin_token_setter_tests` — duplicated rather
    /// than shared so each test module's helpers stay self-contained
    /// (the alternative was widening `min_cfg` to `pub(super)`,
    /// which leaks a test-only contract into the parent mod).
    fn min_cfg() -> SandboxConfig {
        SandboxConfig {
            port: 9091,
            token: ApiToken::new("ignored-creator-token"),
            backend: "nomad-ch".into(),
            image: "img".into(),
            workspace_root: std::path::PathBuf::from("/var/zeroship/projects"),
            network: "n".into(),
            memory_mb: 1024,
            cpus: 2.0,
            idle_timeout_secs: 1800,
            max_lifetime_secs: 28800,
            auto_pull: false,
            k8s: K8sConfig {
                namespace: "default".into(),
                image: "i".into(),
                runtime_class: "kvm-sandbox".into(),
                ready_timeout_secs: 120,
                use_port_forward: false,
                port_forward_start: 18000,
                user_home_size: "5Gi".into(),
                user_home_storage_class: None,
                startup_orphan_cleanup: false,
            },
            nomad_ch: NomadCHConfig {
                nomad_addr: "http://127.0.0.1:4646".into(),
                datacenter: "dc1".into(),
                wrapper_path: std::path::PathBuf::from(
                    "/etc/zeroship/nomad-vm-wrapper.sh",
                ),
                runtime_dir: std::path::PathBuf::from("/var/lib/zeroship/ch"),
                host_state_dir: std::path::PathBuf::from("/var/zeroship/ch"),
                user_home_dir_root: std::path::PathBuf::from(
                    "/var/zeroship/ch/users",
                ),
                vm_index_floor: 1,
                vm_index_ceil: 200,
                alloc_running_timeout_secs: 60,
                agent_livez_timeout_secs: 30,
                host_fence_timeout_secs: 30,
                startup_orphan_cleanup: false,
                subnet_second_octet: 99,
            },
            create_retry_max: 2,
            create_retry_total_timeout_secs: 90,
            snapshot_enabled: false,
            snapshot_l1_root: std::path::PathBuf::from(
                "/var/zeroship/ch/snapshots",
            ),
            snapshot_use_gcs: false,
            snapshot_gcs_bucket: None,
            snapshot_root_kek_path: None,
            workspace_image_size_gb: 20,
        }
    }

    fn min_state() -> AppState {
        let cfg = min_cfg();
        let backend = Backend::from_config(&cfg).expect("backend");
        AppState::new_fixture(cfg, backend)
    }

    /// Build a fresh `Arc<Persistence>` against a unique temp dir so
    /// the two tests don't share filesystem state.
    fn fresh_persist(label: &str) -> Arc<Persistence> {
        let dir = std::env::temp_dir().join(format!(
            "zsbx-persist-setter-{label}-{}",
            uuid::Uuid::now_v7().simple()
        ));
        std::fs::create_dir_all(&dir).expect("mkdir tmpdir");
        // 32 bytes of constant noise — `AeadKey::from_bytes` requires
        // exactly AEAD_KEY_LEN; the cipher doesn't care about the
        // distribution for a unit test that never decrypts.
        let aead = AeadKey::from_bytes([0x5Au8; 32]);
        Arc::new(Persistence::new(dir, aead))
    }

    #[test]
    fn with_persistence_accepts_arc() {
        let state = min_state();
        assert!(
            state.persist().is_none(),
            "fixture must start with persist = None"
        );
        let p = fresh_persist("accepts");
        let state = state
            .with_persistence(p.clone())
            .expect("builder accepts Arc<Persistence>");
        let stored = state.persist().expect("field populated");
        assert!(
            Arc::ptr_eq(stored, &p),
            "stored handle must be the same Arc the builder received"
        );
    }

    #[test]
    fn with_persistence_replaces_existing() {
        // Two distinct handles → second call wins. Without this,
        // a test that re-wires a fixture could silently retain the
        // first handle and seal under the wrong key.
        let state = min_state();
        let first = fresh_persist("replaces-first");
        let second = fresh_persist("replaces-second");
        let state = state
            .with_persistence(first.clone())
            .expect("first call");
        let state = state
            .with_persistence(second.clone())
            .expect("second call");
        let stored = state.persist().expect("field populated");
        assert!(
            !Arc::ptr_eq(stored, &first),
            "second call must overwrite the first handle"
        );
        assert!(
            Arc::ptr_eq(stored, &second),
            "stored handle must be the second Arc"
        );
    }
}

// ────────────────────────────────────────────────────────────────────
// A6b (deferred backlog) — builder semantics for the 5 remaining
// credential-carrying fields. Each test follows the same shape as the
// A5/A6 setter tests: build a fresh `min_state`, populate the field
// twice with distinct values, assert the second wins. For the trait-
// object fields (`snapshot_store`, `ch_remote`, `restore_backend`)
// the test relies on `Arc::ptr_eq` against an upcast `Arc<dyn T>` —
// the borrowed reference inside the field must point at the second
// `Arc`. There is no empty-rejection test: `Arc<dyn T>` has no
// "empty" shape, and `SandboxConfig` / `Arc<Database>` carry their
// own construction invariants checked earlier in the boot chain.
// ────────────────────────────────────────────────────────────────────
#[cfg(test)]
mod field_setter_tests {
    use super::*;
    use crate::backend::Backend;
    use crate::config::{ApiToken, K8sConfig, NomadCHConfig, SandboxConfig};
    use crate::db::Database;
    use crate::restore_handler::StubRestoreBackend;
    use crate::snapshot_handler::MockChRemoteClient;
    use crate::snapshot_store::LocalDiskSnapshotStore;

    /// Minimal config that satisfies `Backend::from_config` for the
    /// nomad-ch backend WITHOUT touching the network. Duplicated from
    /// the sibling test modules for self-containment (same rationale
    /// as `persist_setter_tests::min_cfg`).
    fn min_cfg() -> SandboxConfig {
        SandboxConfig {
            port: 9091,
            token: ApiToken::new("ignored-creator-token"),
            backend: "nomad-ch".into(),
            image: "img".into(),
            workspace_root: std::path::PathBuf::from("/var/zeroship/projects"),
            network: "n".into(),
            memory_mb: 1024,
            cpus: 2.0,
            idle_timeout_secs: 1800,
            max_lifetime_secs: 28800,
            auto_pull: false,
            k8s: K8sConfig {
                namespace: "default".into(),
                image: "i".into(),
                runtime_class: "kvm-sandbox".into(),
                ready_timeout_secs: 120,
                use_port_forward: false,
                port_forward_start: 18000,
                user_home_size: "5Gi".into(),
                user_home_storage_class: None,
                startup_orphan_cleanup: false,
            },
            nomad_ch: NomadCHConfig {
                nomad_addr: "http://127.0.0.1:4646".into(),
                datacenter: "dc1".into(),
                wrapper_path: std::path::PathBuf::from(
                    "/etc/zeroship/nomad-vm-wrapper.sh",
                ),
                runtime_dir: std::path::PathBuf::from("/var/lib/zeroship/ch"),
                host_state_dir: std::path::PathBuf::from("/var/zeroship/ch"),
                user_home_dir_root: std::path::PathBuf::from(
                    "/var/zeroship/ch/users",
                ),
                vm_index_floor: 1,
                vm_index_ceil: 200,
                alloc_running_timeout_secs: 60,
                agent_livez_timeout_secs: 30,
                host_fence_timeout_secs: 30,
                startup_orphan_cleanup: false,
                subnet_second_octet: 99,
            },
            create_retry_max: 2,
            create_retry_total_timeout_secs: 90,
            snapshot_enabled: false,
            snapshot_l1_root: std::path::PathBuf::from(
                "/var/zeroship/ch/snapshots",
            ),
            snapshot_use_gcs: false,
            snapshot_gcs_bucket: None,
            snapshot_root_kek_path: None,
            workspace_image_size_gb: 20,
        }
    }

    fn min_state() -> AppState {
        let cfg = min_cfg();
        let backend = Backend::from_config(&cfg).expect("backend");
        AppState::new_fixture(cfg, backend)
    }

    #[test]
    fn with_config_replaces_existing() {
        // The setter is the only legal out-of-crate write path to
        // `config`; flip a discriminator (port number) to verify the
        // new config landed.
        let mut first = min_cfg();
        first.port = 11111;
        let backend = Backend::from_config(&first).expect("backend");
        let state = AppState::new_fixture(first, backend);
        assert_eq!(state.config.port, 11111, "fixture starts with first cfg");

        let mut second = min_cfg();
        second.port = 22222;
        let state = state.with_config(second);
        assert_eq!(
            state.config.port, 22222,
            "with_config must replace the prior SandboxConfig"
        );
    }

    #[test]
    fn with_database_replaces_existing() {
        // `Arc::ptr_eq` proves the stored handle is the second Arc —
        // not just an equal-by-value clone. `for_setter_test_only`
        // is a sync, in-crate constructor that skips the pg pool
        // (no live postgres required for this test).
        let state = min_state();
        assert!(
            state.database.is_none(),
            "fixture must start with database = None"
        );
        let first = Arc::new(Database::for_setter_test_only(
            "postgres://first:nopass@localhost/sbx_a".into(),
        ));
        let second = Arc::new(Database::for_setter_test_only(
            "postgres://second:nopass@localhost/sbx_b".into(),
        ));
        let state = state.with_database(first.clone());
        let state = state.with_database(second.clone());
        let stored = state.database.as_ref().expect("field populated");
        assert!(
            !Arc::ptr_eq(stored, &first),
            "second call must overwrite the first handle"
        );
        assert!(
            Arc::ptr_eq(stored, &second),
            "stored handle must be the second Arc"
        );
    }

    #[test]
    fn with_snapshot_store_replaces_existing() {
        let state = min_state();
        assert!(state.snapshot_store.is_none(), "fixture starts None");
        let first: Arc<dyn SnapshotStore> = Arc::new(
            LocalDiskSnapshotStore::new(std::path::PathBuf::from(
                "/tmp/zsbx-snap-setter-first",
            )),
        );
        let second: Arc<dyn SnapshotStore> = Arc::new(
            LocalDiskSnapshotStore::new(std::path::PathBuf::from(
                "/tmp/zsbx-snap-setter-second",
            )),
        );
        let state = state.with_snapshot_store(first.clone());
        let state = state.with_snapshot_store(second.clone());
        let stored = state.snapshot_store.as_ref().expect("field populated");
        assert!(
            !Arc::ptr_eq(stored, &first),
            "second call must overwrite the first handle"
        );
        assert!(
            Arc::ptr_eq(stored, &second),
            "stored handle must be the second Arc"
        );
    }

    #[test]
    fn with_ch_remote_replaces_existing() {
        let state = min_state();
        assert!(state.ch_remote.is_none(), "fixture starts None");
        let first: Arc<dyn ChRemoteClient> =
            Arc::new(MockChRemoteClient::default());
        let second: Arc<dyn ChRemoteClient> =
            Arc::new(MockChRemoteClient::default());
        let state = state.with_ch_remote(first.clone());
        let state = state.with_ch_remote(second.clone());
        let stored = state.ch_remote.as_ref().expect("field populated");
        assert!(
            !Arc::ptr_eq(stored, &first),
            "second call must overwrite the first handle"
        );
        assert!(
            Arc::ptr_eq(stored, &second),
            "stored handle must be the second Arc"
        );
    }

    #[test]
    fn with_restore_backend_replaces_existing() {
        let state = min_state();
        assert!(state.restore_backend.is_none(), "fixture starts None");
        let first: Arc<dyn RestoreBackend> =
            Arc::new(StubRestoreBackend::new(std::path::PathBuf::from(
                "/tmp/zsbx-rb-setter-first",
            )));
        let second: Arc<dyn RestoreBackend> =
            Arc::new(StubRestoreBackend::new(std::path::PathBuf::from(
                "/tmp/zsbx-rb-setter-second",
            )));
        let state = state.with_restore_backend(first.clone());
        let state = state.with_restore_backend(second.clone());
        let stored = state.restore_backend.as_ref().expect("field populated");
        assert!(
            !Arc::ptr_eq(stored, &first),
            "second call must overwrite the first handle"
        );
        assert!(
            Arc::ptr_eq(stored, &second),
            "stored handle must be the second Arc"
        );
    }
}

#[cfg(test)]
mod shutdown_tests {
    use super::*;
    use std::sync::atomic::AtomicU32;

    #[compio::test]
    async fn fixture_loop_exits_within_one_iteration_of_flag_flip() {
        let shutdown = Arc::new(AtomicBool::new(false));
        let iterations = Arc::new(AtomicU32::new(0));
        let shutdown_task = shutdown.clone();
        let iter_task = iterations.clone();

        let task = compio::runtime::spawn(async move {
            loop {
                if shutdown_task.load(Ordering::SeqCst) {
                    return iter_task.load(Ordering::SeqCst);
                }
                compio::time::sleep(Duration::from_millis(20)).await;
                if shutdown_task.load(Ordering::SeqCst) {
                    return iter_task.load(Ordering::SeqCst);
                }
                iter_task.fetch_add(1, Ordering::SeqCst);
            }
        });

        // Let the loop run a few iterations.
        compio::time::sleep(Duration::from_millis(80)).await;
        let pre_flip = iterations.load(Ordering::SeqCst);
        assert!(
            pre_flip >= 1,
            "fixture must iterate at least once before flip; got {pre_flip}"
        );

        // Flip the flag; the next post-sleep check terminates the loop.
        shutdown.store(true, Ordering::SeqCst);

        let final_iters = task.await.expect("task completes");
        assert!(
            final_iters <= pre_flip + 1,
            "loop must exit within one iteration of flag flip; pre={pre_flip}, post={final_iters}"
        );
    }
}
