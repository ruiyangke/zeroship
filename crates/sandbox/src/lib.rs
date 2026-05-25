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
pub mod detach;
pub(crate) mod error_envelope;
pub mod files;
pub mod handlers;
pub mod metrics;
// Composite-r1 #2: sole production consumer is
// `admin_handlers::metrics_endpoint` (same crate); sole test consumer
// is the module's own unit tests. No integration test reaches in via
// `zeroship_sandbox::metrics_export::*`, so the surface stays internal.
pub(crate) mod metrics_export;
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
pub mod wake_machine;

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
use crate::snapshot_aead::{AeadSnapshotStore, RootKek, ROOT_KEK_ENV};
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
    /// Mint-side rate limiter. `Some` in production; `None`
    /// for tests that build `AppState` directly without
    /// `from_config`. Handlers that consume it `expect()` on `Some`.
    pub mint_rate_limiter: Option<MintRateLimiter>,
    /// Pg-backed non-secret state handle (`docs/proposals/sandbox-pg-state.md`
    /// §8.2). `None` is the disabled-by-absence shape:
    /// `SANDBOX_DATABASE_URL` is unset, so pg integration is off.
    /// The schema is brought to
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
    /// Admin bearer token, read once
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
    /// Wrapped in `zeroize::Zeroizing<String>` so
    /// the heap allocation is scrubbed on drop. A core dump or
    /// `/proc/<pid>/mem` read after process exit can't trivially
    /// recover the bearer. (Live-process reads are still a concern,
    /// but the post-mortem surface is closed.) Mirrors the
    /// `config::ApiToken` treatment of `SANDBOX_TOKEN`.
    ///
    /// A5 (api-surface-2026-05-24-r1): restricted to `pub(crate)` so
    /// no out-of-crate caller can clobber the field with an empty
    /// `Zeroizing<String>` (which would defeat the constant-time
    /// compare — see `admin_handlers::admin_check_required`). Tests
    /// and other in-crate constructors set the field via the safe
    /// [`AppState::with_admin_token`] builder, which rejects empty
    /// strings before they can reach the auth path.
    pub(crate) admin_token: Option<zeroize::Zeroizing<String>>,

    /// T1 (sandbox_admin_ro role, 2026-05-25): the read-only admin
    /// bearer. Mirrors [`admin_token`] in lifecycle (read ONCE at
    /// boot from `SANDBOX_ADMIN_RO_TOKEN_PATH`, mode 0o400, owner
    /// uid 0). `None` is the disabled-by-absence shape; the admin
    /// API's role-gate then accepts only the full bearer (when
    /// configured) or 503's the read endpoints (when both bearers
    /// are absent).
    ///
    /// Threat model: bearer leak via an unprivileged dashboard or
    /// on-call tooling. The RO bearer authorizes GETs only —
    /// destructive endpoints (snapshot / wake / GDPR / cold-boot)
    /// surface 403 `insufficient_role` against this token. A leak
    /// limits the attacker to fleet enumeration, not write actions.
    ///
    /// Field is `pub(crate)` for the same reason as `admin_token`:
    /// the empty-string footgun must not be plantable from outside
    /// the crate. The builder [`AppState::with_admin_ro_token`]
    /// rejects empty strings; the boot loader rejects equal-content
    /// full+ro token files at boot.
    pub(crate) admin_ro_token: Option<zeroize::Zeroizing<String>>,

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

    /// C-7-LT (PR1 scaffolding): wake-response contract mode. Read once
    /// at boot from `SANDBOX_WAKE_RESPONSE_MODE`; defaults to
    /// [`config::WakeResponseMode::Sync`] for back-compat with the
    /// existing wake contract. PR2 reads this flag in the wake handler
    /// to switch between the legacy 200 OK shape and the 202 Accepted +
    /// polling shape; PR1 only carries the flag (no handler branching
    /// yet — existing tests stay green).
    ///
    /// Field is `pub` (no security sensitivity — a per-request mode flag
    /// can't grant or weaken any privilege; it only selects the
    /// response shape).
    pub wake_response_mode: crate::config::WakeResponseMode,

    /// C-7-LT (PR2-FOLLOWUP, R16-S5): wake-job lifecycle configuration.
    /// Resolved at boot from env (`SANDBOX_WAKE_JOBS_GC_RETENTION_SECS`).
    /// Owns the GC retention window the wake_jobs GC sweep reads at
    /// every iteration. Static at runtime (env reload not supported).
    pub wake_lifecycle: crate::config::WakeLifecycleConfig,

    /// r3-A (T-8b-stress-r3 fix): the local Nomad agent's node ID,
    /// fetched once at boot via `GET /v1/agent/self`. When `Some`, the
    /// jobspec builders emit a `Constraints` block pinning every
    /// submitted alloc to THIS worker — closing the cross-node race
    /// where the controller stages `workspace.img` on its local fs
    /// but Nomad's scheduler picks a different worker (78%
    /// stress-r3 failure rate at WORKER_COUNT=3).
    ///
    /// `None` is the disabled-by-detection-failure shape: a boot-time
    /// /v1/agent/self HTTP failure, non-200, parse error, or missing
    /// `stats.client.node_id` field. Boot does NOT block on this —
    /// the controller boots without the constraint and falls back to
    /// the pre-r3-A random-placement behaviour (a controller restart
    /// shouldn't fail because Nomad agent restarted). The
    /// `sandbox_nomad_node_id_lookup_failures_total` counter (see
    /// [`crate::metrics::inc_nomad_node_id_lookup_failure`]) bumps so
    /// operators can alert on the degraded shape.
    ///
    /// Field is `pub` (no security sensitivity — a node-id is the
    /// local agent's self-reported identifier, not a credential).
    pub local_nomad_node_id: Option<String>,

    /// **r30-A1 (concurrency-r30 CRITICAL #A1)**: global semaphore
    /// capping concurrent Nomad `/shutdown` ladders across every
    /// teardown call path (`AppStateGcStopper`, snap-idle-evict,
    /// snap-idle-gc, admin snapshot teardown, transient-state takeover,
    /// registry GC, restore-failure rollback). Sized from
    /// `SANDBOX_NOMAD_STOP_CONCURRENCY` (default 16) in
    /// [`Self::from_config`]. The same `Arc` is installed on the inner
    /// `NomadCHBackend` via
    /// [`crate::backend::nomad_ch::NomadCHBackend::install_nomad_stop_permits`]
    /// so `stop_inner` can acquire a permit before the `/shutdown`
    /// ladder runs — structurally impossible to bypass, even from a
    /// future teardown call site that forgets the convention.
    ///
    /// **Why on `AppState` and not just on the backend**: the cap is a
    /// process-global resource budget, not a backend-implementation
    /// detail. Plumbing it through `AppState` makes its lifecycle
    /// observable (one place to read for `metrics_export`, one place
    /// to size at boot, one place for future ops/test fixtures to
    /// inject a smaller cap for chaos testing).
    ///
    /// Field is `pub(crate)` for the same reason as `database` /
    /// `persist` — a `state.nomad_stop_permits = attacker_perms` swap
    /// from out-of-crate code could plant a capacity-0 semaphore that
    /// silently deadlocks every teardown path (denying the cluster's
    /// ability to reap idle sandboxes). Out-of-crate callers are
    /// expected to go through `from_config`.
    pub(crate) nomad_stop_permits:
        Arc<crate::backend::nomad_ch::NomadStopPermits>,

    /// **R33-I1 (concurrency-r33 IMPORTANT)**: per-user fence around the
    /// `home.img` mkfs.ext4 step inside the cold-boot create path. The
    /// `home.img` file is **per-USER** (cf
    /// `nomad_ch.rs::user_home_image_path`), not per-sandbox — two
    /// concurrent cold-boot CREATEs by the same user would race the
    /// exists-then-mkfs sequence inside `create_ext4_image_if_missing`.
    /// R32-P1's `std::thread::scope` parallelisation widens the window
    /// by running both mkfs in parallel inside each CREATE, so under
    /// c≥2 same-user cold-boot stress N `mkfs.ext4 -q -F <same-path>`
    /// subprocesses can be in flight at once. Worst observable failure
    /// is silent corruption surfaced only at guest mount.
    ///
    /// Shape: a `HashMap<UserId, Arc<Mutex<()>>>` lazily populated as
    /// users hit their first cold-boot. The outer `Mutex` guards only
    /// the map's structure (insert / lookup); the inner `Mutex` is what
    /// the `home_h` thread holds across the mkfs subprocess. The
    /// workspace_h thread is NOT gated — `workspace.img` is per-sandbox
    /// (UUID-scoped path), so there's no cross-CREATE collision and the
    /// parallel-mkfs perf win from R32-P1 is preserved for the
    /// workspace half.
    ///
    /// Why `std::sync::Mutex` (not `tokio::sync::Mutex`): the lock is
    /// acquired inside the `compio::runtime::spawn_blocking` closure
    /// (sync context), and the critical section is a CPU/syscall mix
    /// (truncate + mkfs.ext4 + fsync_dir) that already blocks the
    /// spawn_blocking thread — async-await semantics buy nothing.
    ///
    /// Why `pub(crate)` (matching `nomad_stop_permits`): a planted
    /// always-locked map from out-of-crate code could deadlock every
    /// same-user cold-boot. Out-of-crate callers construct via
    /// `from_config` / `new_fixture`.
    ///
    /// `#[allow(dead_code)]`: production reads go through the
    /// `Arc::clone` installed on the inner `NomadCHBackend` (via
    /// `install_user_home_mkfs_locks`), not through this field
    /// directly. The field is kept on `AppState` for lifecycle
    /// observability (same posture as `nomad_stop_permits`) and so
    /// future metrics-export / chaos-test fixtures can read through
    /// `user_home_mkfs_locks()` without going via the backend.
    #[allow(dead_code)]
    pub(crate) user_home_mkfs_locks:
        Arc<std::sync::Mutex<std::collections::HashMap<String, Arc<std::sync::Mutex<()>>>>>,
}

impl AppState {
    /// Signal background tasks to
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

    /// r30-A1: read-only accessor for the global Nomad /shutdown
    /// semaphore. Returned `Arc` is cheap to clone; intended for tests
    /// + future call sites that want to inspect `permits_available()`
    /// or `capacity()` for monitoring / chaos-test injection. Production
    /// `stop_inner` does NOT read through this — it goes through the
    /// `OnceLock` installed on the `NomadCHBackend` directly so the
    /// hot teardown path doesn't take an extra `Arc::clone` per stop.
    pub fn nomad_stop_permits(
        &self,
    ) -> &Arc<crate::backend::nomad_ch::NomadStopPermits> {
        &self.nomad_stop_permits
    }

    /// R33-I1: read-only accessor for the per-user `home.img` mkfs
    /// fence map. `pub(crate)` (not `pub`) — this is an internal fence
    /// for the cold-boot path inside `NomadCHBackend::try_create`, not
    /// a public lifecycle hook. Returned `Arc` is cheap to clone; the
    /// inner `HashMap` is guarded by an `std::sync::Mutex` whose only
    /// invariant is "outer-lock-held for insert/lookup, inner-lock-held
    /// across the per-user mkfs". See the field doc on
    /// [`AppState::user_home_mkfs_locks`] for the race shape this
    /// fence closes.
    #[allow(dead_code)]
    pub(crate) fn user_home_mkfs_locks(
        &self,
    ) -> &Arc<std::sync::Mutex<std::collections::HashMap<String, Arc<std::sync::Mutex<()>>>>>
    {
        &self.user_home_mkfs_locks
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
    /// (`subtle::ConstantTimeEq` via
    /// `admin_handlers::admin_check_required`) rather than `==`
    /// against user-presented bytes. Mainly here so integration
    /// tests can assert wiring without poking the `pub(crate)` field.
    pub fn admin_token(&self) -> Option<&str> {
        self.admin_token.as_deref().map(|z| z.as_str())
    }

    /// T1: safe builder for the read-only admin bearer. Mirrors
    /// [`AppState::with_admin_token`] verbatim: the field is
    /// `pub(crate)` and the only legal out-of-crate write path is
    /// this builder, which rejects empty strings before the auth
    /// path can see them.
    ///
    /// Semantics:
    ///   - `token = None` → clears the field (RO bearer disabled).
    ///   - `token = Some("")` → `Err("admin_ro_token must not be empty")`.
    ///   - `token = Some(non-empty)` → wraps in `Zeroizing<String>`.
    pub fn with_admin_ro_token(
        mut self,
        token: Option<String>,
    ) -> Result<Self, String> {
        match token {
            None => {
                self.admin_ro_token = None;
                Ok(self)
            }
            Some(t) if t.is_empty() => {
                Err("admin_ro_token must not be empty".to_string())
            }
            Some(t) => {
                self.admin_ro_token = Some(zeroize::Zeroizing::new(t));
                Ok(self)
            }
        }
    }

    /// T1: read-only accessor for the read-only admin bearer. Mirrors
    /// [`AppState::admin_token`]; used by integration tests to assert
    /// wiring without poking the `pub(crate)` field.
    pub fn admin_ro_token(&self) -> Option<&str> {
        self.admin_ro_token.as_deref().map(|z| z.as_str())
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
    /// R3-Q2 (code-quality-r3): originally returned `Result<Self,
    /// String>` for "future-proof symmetry"; flagged across r3/r4/r5
    /// reviews as a signature smell because the body cannot fail and
    /// every in-crate caller had to `.expect("infallible operation")`.
    /// Now returns plain `Self`. Future invariants (e.g.
    /// dir-writability probes, AEAD-key liveness pings) can switch
    /// back to `Result` when they actually need it.
    ///
    /// Semantics:
    ///   - `with_persistence(p)` → `self` with the field set to
    ///     `Some(p)`. Replaces any prior value.
    pub fn with_persistence(mut self, persist: Arc<Persistence>) -> Self {
        self.persist = Some(persist);
        self
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
        // r30-A1: build a permits pool sized from config (default 16
        // unless the fixture caller pre-mutated `nomad_stop_concurrency`).
        // The fixture does NOT install the permits on the inner
        // NomadCHBackend — unit tests that need the install go through
        // `backend.nomad_ch_handle().install_nomad_stop_permits(...)`
        // explicitly. Holding the Arc on AppState is enough to satisfy
        // the field's `pub(crate)` invariant and gives test fixtures a
        // live handle if they want one.
        let nomad_stop_permits = crate::backend::nomad_ch::NomadStopPermits::new(
            config.nomad_ch.nomad_stop_concurrency.max(1),
        );
        Self {
            config,
            sandboxes: SandboxRegistry::new(),
            backend,
            mint_rate_limiter: Some(MintRateLimiter::new()),
            database: None,
            persist: None,
            shutdown: Arc::new(AtomicBool::new(false)),
            admin_token: None,
            admin_ro_token: None,
            snapshot_store: None,
            ch_remote: None,
            restore_backend: None,
            // C-7-LT (PR1): default to Sync in fixtures — tests that
            // exercise the (PR2) async path will set this via field
            // assignment on `let mut state = new_fixture(...)`.
            wake_response_mode: crate::config::WakeResponseMode::Sync,
            wake_lifecycle: crate::config::WakeLifecycleConfig::default(),
            // r3-A: fixtures get `None` — no boot-time /v1/agent/self
            // lookup happens for in-process tests, so the produced
            // jobspecs omit the Constraints block (matches pre-r3-A
            // behaviour). Tests that need to exercise the pinned-shape
            // can set the field via plain assignment on the returned
            // `Self`.
            local_nomad_node_id: None,
            nomad_stop_permits,
            // R33-I1: empty map; lazily populated on first cold-boot per
            // user. Fixtures get their own map so unit tests that
            // exercise the parallel-mkfs path have an isolated fence.
            user_home_mkfs_locks: Arc::new(std::sync::Mutex::new(
                std::collections::HashMap::new(),
            )),
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

        // R5-S1 (security-r5) / B21 cluster fix. Refuse to boot in the
        // fail-OPEN configuration (snapshot enabled + persist disabled).
        // See [`assert_persist_required_when_snapshot_enabled`] for the
        // full rationale; the function is factored out so tests can
        // exercise the assertion without spinning up a backend probe.
        //
        // R6-A1: the escape hatch env var is read here on the prod boot
        // path, so its NAME has to scream "test-only". The original
        // SANDBOX_PERSIST_NONE_OK was indistinguishable from real prod
        // env vars (SANDBOX_PERSIST_AUTH, SANDBOX_PERSIST_DIR), making
        // operator misconfiguration a silent fail-OPEN re-enable of the
        // exact bug R5-S1 closed. Renamed with an explicit
        // ZEROSHIP_SANDBOX_TEST_ prefix so it cannot be confused for
        // a production setting.
        let persist_test_override = matches!(
            std::env::var("ZEROSHIP_SANDBOX_TEST_DISABLE_PERSIST_ASSERTION")
                .as_deref(),
            Ok("1")
        );
        assert_persist_required_when_snapshot_enabled(
            config.snapshot_enabled,
            persist.is_some(),
            persist_test_override,
        )?;

        // A1-FOLLOWUP (arch-r9 fail-CLOSED gap). Refuse to boot in the
        // production-shaped configuration where snapshot/restore writes
        // to a non-local L2 (GCS) AND no AEAD root KEK is configured:
        // bare guest RAM would land in the remote object store in clear.
        // Local-only L1 stores still warn-and-continue (dev/test
        // ergonomics) — the warn arm lives in the snapshot-store
        // composition site below.
        //
        // Escape hatch (matching R6-A1's naming convention so operator
        // misuse is obvious from any unit-file env block):
        // ZEROSHIP_SANDBOX_TEST_ALLOW_UNENCRYPTED_REMOTE=1.
        let remote_unencrypted_test_override = matches!(
            std::env::var("ZEROSHIP_SANDBOX_TEST_ALLOW_UNENCRYPTED_REMOTE")
                .as_deref(),
            Ok("1")
        );
        assert_kek_required_for_remote_store(
            config.snapshot_enabled,
            config.snapshot_use_gcs,
            config.snapshot_root_kek_path.is_some(),
            remote_unencrypted_test_override,
        )?;

        // Phase-0 pg-backed state: build BEFORE the backend probe so
        // the schema reaches the right version before any backend op
        // could try to write.
        let database: Option<Arc<Database>> = match Database::from_env().await {
            Ok(opt) => opt.map(Arc::new),
            Err(e) => {
                // Dev escape hatch: `SANDBOX_PG_OPTIONAL=1` logs and
                // continues with pg disabled. Production refuses to
                // boot until the operator fixes the config.
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

        // r3-A (T-8b-stress-r3 fix): fetch THIS controller's local Nomad
        // node_id once at boot. The jobspec builders (cold-boot +
        // restore) emit a Nomad `Constraints` block pinning every alloc
        // to this node when the value is `Some`, closing the cross-node
        // placement race where `workspace.img` is staged on THIS
        // worker's local fs but Nomad's scheduler picks a different
        // worker → the driver's `assert_disk_image_present` ENOENTs.
        // T-8b-stress-r3 surfaced 78% cross-node failure at
        // WORKER_COUNT=3 from exactly this gap.
        //
        // Failure is NON-fatal: a transient /v1/agent/self HTTP failure
        // or unparseable response leaves the field as `None`, the
        // jobspec builders omit the Constraints block, and the
        // controller falls back to pre-r3-A random-placement behaviour.
        // Bumping `inc_nomad_node_id_lookup_failure` so operators can
        // alert on the degraded shape; a controller restart against a
        // transiently-unavailable Nomad agent shouldn't fail boot.
        //
        // Sourced from `config.nomad_ch.nomad_addr` even when the
        // active backend isn't nomad-ch — docker/k8s deploys won't
        // submit Nomad jobs so the field is harmlessly ignored, and we
        // don't want to branch on backend before backend construction
        // (chicken-and-egg with the probe).
        let local_nomad_node_id: Option<String> =
            match crate::backend::nomad_ch::fetch_local_nomad_node_id(
                &config.nomad_ch.nomad_addr,
            )
            .await
            {
                Ok(id) => {
                    tracing::info!(
                        nomad_addr = %config.nomad_ch.nomad_addr,
                        node_id = %id,
                        "sandbox/nomad-ch: cached local node_id for r3-A \
                         placement constraint"
                    );
                    Some(id)
                }
                Err(e) => {
                    crate::metrics::inc_nomad_node_id_lookup_failure();
                    tracing::warn!(
                        nomad_addr = %config.nomad_ch.nomad_addr,
                        error = %e,
                        "sandbox/nomad-ch: boot-time /v1/agent/self lookup \
                         failed (non-fatal — jobspecs will omit the r3-A \
                         Constraints block; cross-node placement race \
                         possible at WORKER_COUNT>1 until next controller \
                         restart against a reachable Nomad agent)"
                    );
                    None
                }
            };

        let backend = {
            let mut b = Backend::builder(&config);
            if let Some(p) = persist.clone() {
                b = b.with_persist(p);
            }
            if let Some(id) = local_nomad_node_id.clone() {
                b = b.with_local_nomad_node_id(id);
            }
            b.build()?
        };
        // r30-A1 (concurrency-r30 CRITICAL #A1): size + install the
        // global Nomad /shutdown semaphore. The cap is read once at
        // boot from `SANDBOX_NOMAD_STOP_CONCURRENCY` (default 16,
        // validated > 0 by `NomadCHConfig::validate`); the SAME
        // `Arc<NomadStopPermits>` is held on `AppState` AND installed
        // on the inner `NomadCHBackend` so that every `stop_inner`
        // invocation — regardless of which of the 7 teardown call
        // paths triggered it — competes for the same permit pool.
        //
        // Set BEFORE the backend's first `stop_inner` could fire (the
        // probe + cleanup_orphans_at_startup calls below DO touch
        // backend state but none invoke stop_inner: probe is a
        // health-check only; cleanup_orphans tears down Nomad jobs via
        // a different code path that doesn't go through stop_inner).
        // No race: the install happens before any HTTP handler is
        // registered.
        let nomad_stop_permits = crate::backend::nomad_ch::NomadStopPermits::new(
            config.nomad_ch.nomad_stop_concurrency,
        );
        crate::metrics::set_nomad_stop_permits_total(
            config.nomad_ch.nomad_stop_concurrency as u64,
        );
        if let Some(nch) = backend.nomad_ch_handle() {
            nch.install_nomad_stop_permits(Arc::clone(&nomad_stop_permits));
            tracing::info!(
                cap = config.nomad_ch.nomad_stop_concurrency,
                "sandbox/nomad-ch r30-A1: installed global Nomad /shutdown \
                 semaphore (SANDBOX_NOMAD_STOP_CONCURRENCY)"
            );
        }
        // R33-I1 (concurrency-r33 IMPORTANT): build + install the
        // per-user `home.img` mkfs fence map. Held on `AppState` so the
        // lifecycle is observable in one place (matches the
        // `nomad_stop_permits` precedent); installed on the inner
        // `NomadCHBackend` via `OnceLock` so the cold-boot path can
        // acquire the inner per-user Mutex from `&self` without taking
        // the whole-backend write-lock. Install happens BEFORE any
        // create() can fire (probe + cleanup_orphans_at_startup don't
        // invoke try_create).
        let user_home_mkfs_locks: Arc<
            std::sync::Mutex<std::collections::HashMap<String, Arc<std::sync::Mutex<()>>>>,
        > = Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
        if let Some(nch) = backend.nomad_ch_handle() {
            nch.install_user_home_mkfs_locks(Arc::clone(&user_home_mkfs_locks));
            tracing::info!(
                "sandbox/nomad-ch R33-I1: installed per-user home.img \
                 mkfs fence (DashMap-equivalent: Mutex<HashMap<UserId, \
                 Arc<Mutex<()>>>>)"
            );
        }
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

        // Sealed-record restore (preview-URL design).
        // Reuses the shared `persist` handle built above so we don't
        // re-open the AEAD key file or re-read the env. `None` is the
        // disabled/no-op shape — feature-flagged behind
        // `SANDBOX_PERSIST_AUTH=1`; default OFF.
        // Restart restore is pg-driven. We need both
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
        // Read the admin bearer once at
        // boot. Previously every `/admin/*` request stat()+read()'d
        // the file (slow-FS DoS amplification; fail-open on chmod
        // error). Boot-time read is fail-loud — a misconfigured
        // mode (anything other than 0o400) refuses to start the
        // process. "Disabled because env-unset" stays `None`; the
        // file existing-but-misconfigured is `Err`.
        //
        // Env resolution happens here, then the pure
        // `load_admin_token` reads and validates the path. Tests can
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

        // T1: same shape for the read-only admin bearer. Re-uses
        // `load_admin_token` so the mode 0o400 + uid 0 + non-empty
        // invariants apply identically.
        let admin_ro_token_path = std::env::var("SANDBOX_ADMIN_RO_TOKEN_PATH")
            .ok()
            .and_then(|v| {
                let t = v.trim();
                if t.is_empty() { None } else { Some(std::path::PathBuf::from(t)) }
            });
        let admin_ro_token = load_admin_token(admin_ro_token_path.as_deref())?
            .map(zeroize::Zeroizing::new);

        // T1 boot guard: refuse to boot when both bearers resolve to
        // the SAME secret. The whole point of the RO bearer is least-
        // privilege; if the operator pointed both env vars at the
        // same file (or two files with identical contents), the RO
        // distinction is illusory and any leak of the RO bearer
        // grants Full admin too. Fail loud at boot rather than
        // silently equalise the two roles.
        assert_distinct_admin_tokens(
            admin_token.as_deref().map(|z| z.as_str()),
            admin_ro_token.as_deref().map(|z| z.as_str()),
        )?;

        // C-7-LT (PR1): resolve wake-response mode from env at boot.
        // Resolved BEFORE the snapshot-wiring block so the value can
        // be threaded into `RealRestoreBackend::with_wake_response_mode`
        // — the C-7-LT-1 (smoke-r12) fix needs the mode at wake-retry-
        // policy construction time, not just on `AppState`.
        //
        // R16-S4 fail-CLOSED: any unrecognised env value aborts boot
        // instead of silently defaulting to sync. Misconfigured
        // feature flags are config bugs, not silent-fallback hazards.
        let wake_response_mode = crate::config::WakeResponseMode::from_env()
            .map_err(|e| format!("WakeResponseMode::from_env: {e}"))?;
        // R16-S5: wake lifecycle config (GC retention). Resolved at
        // boot; propagates to `sweep::run_wake_jobs_gc_once` via
        // `AppState::wake_lifecycle`.
        let wake_lifecycle = crate::config::WakeLifecycleConfig::from_env()
            .map_err(|e| format!("WakeLifecycleConfig::from_env: {e}"))?;
        tracing::info!(
            mode = wake_response_mode.as_str(),
            wake_jobs_gc_retention_secs = wake_lifecycle.wake_jobs_gc_retention_secs,
            "sandbox wake-response: contract mode + lifecycle resolved"
        );

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

            // A1 (audit-r1): load the root KEK BEFORE composing the
            // inner store so the wrap decision is visible in one place.
            // `RootKek::from_env` returns:
            //   - `Ok(Some(kek))` → `SANDBOX_SNAPSHOT_ROOT_KEK_PATH` set
            //     to a 32-byte, mode-0o400 file. AEAD is enabled.
            //   - `Ok(None)` → env unset. AEAD passthrough — guest RAM
            //     lands on L1 disk (and GCS, if tiered) in clear.
            //   - `Err(_)` → env set but the file is missing/wrong mode/
            //     wrong length. Fail boot — the operator's intent was
            //     to enable AEAD; silently falling back to passthrough
            //     would re-enable the exact CRITICAL fail-OPEN shape
            //     this commit is closing.
            let aead_root_kek: Option<RootKek> = RootKek::from_env()
                .map_err(|e| format!("RootKek::from_env: {e}"))?;
            let kek_present = aead_root_kek.is_some();

            // Compose the inner store (L1-only or tiered L1+GCS), then
            // wrap unconditionally in `AeadSnapshotStore`. When
            // `aead_root_kek = None` the wrapper is in passthrough mode
            // (verified by `AeadSnapshotStore::is_active() == false`);
            // the boot log makes that posture explicit so the operator
            // can't miss it in `journalctl`. Wrapping unconditionally
            // (rather than branching `Arc<dyn SnapshotStore>` at the
            // wrapper boundary) means the put/get paths run through the
            // same code in both shapes — no second-class disabled path.
            let store: Arc<dyn SnapshotStore> = if config.snapshot_use_gcs {
                let bucket = config
                    .snapshot_gcs_bucket
                    .clone()
                    .expect("SANDBOX_SNAPSHOT_GCS_BUCKET must be set when use_gcs=true (validated at config parse)");
                let l2 = GcsSnapshotStore::new(bucket.clone(), "default");
                let tiered = TieredSnapshotStore::new(l1, l2);
                if kek_present {
                    tracing::info!(
                        l1_root = %l1_root.display(),
                        gcs_bucket = %bucket,
                        kek_env = ROOT_KEK_ENV,
                        "snapshot_store: AEAD ENABLED (tiered L1+GCS)"
                    );
                } else {
                    // A1-FOLLOWUP: this branch is gated by
                    // `assert_kek_required_for_remote_store` at the top
                    // of `from_config`, which returns Err before we ever
                    // reach the snapshot-store composition. The only way
                    // to land here is via the named test override
                    // `ZEROSHIP_SANDBOX_TEST_ALLOW_UNENCRYPTED_REMOTE=1`
                    // — log loudly so a misconfigured test env can't
                    // pretend to be prod.
                    tracing::error!(
                        l1_root = %l1_root.display(),
                        gcs_bucket = %bucket,
                        kek_env = ROOT_KEK_ENV,
                        "snapshot_store: AEAD DISABLED via test override — \
                         guest RAM plaintext on disk + GCS \
                         (ZEROSHIP_SANDBOX_TEST_ALLOW_UNENCRYPTED_REMOTE=1). \
                         NOT for production."
                    );
                }
                Arc::new(AeadSnapshotStore::new(tiered, aead_root_kek))
            } else if kek_present {
                tracing::info!(
                    l1_root = %l1_root.display(),
                    kek_env = ROOT_KEK_ENV,
                    "snapshot_store: AEAD ENABLED (L1 disk-only)"
                );
                Arc::new(AeadSnapshotStore::new(l1, aead_root_kek))
            } else {
                tracing::warn!(
                    l1_root = %l1_root.display(),
                    kek_env = ROOT_KEK_ENV,
                    "snapshot_store: AEAD DISABLED — guest RAM \
                     plaintext on L1 disk (kek env unset). Set the \
                     kek env to a 32-byte mode-0o400 file."
                );
                Arc::new(AeadSnapshotStore::new(l1, aead_root_kek))
            };
            let ch: Arc<dyn ChRemoteClient> =
                Arc::new(RealChRemoteClient::new());
            // B18 fix: share the create-side allocator with the
            // restore-side reservations. Without this, a CREATE after
            // a successful WAKE hands the same tap/IP to a fresh
            // sandbox because the restored VM holds the slot in a
            // private `VmIndexReservations` map invisible to
            // `NomadCHBackend::vm_index_allocator`. Cluster smoke
            // 2026-05-24 r4: 11/16 c=4 cycles failed with a
            // stale-pubkey 401 once slots 1-6 had been used once.
            let shared_allocator = backend.vm_index_allocator();
            let nomad_handle = backend.nomad_ch_handle();
            let rb_inner = RealRestoreBackend::new(
                config.nomad_ch.clone(),
                config.memory_mb,
                config.cpus,
            )
            // C-7-LT-1 (smoke-r12): thread the wake response mode in
            // so `VmIndexRetryPolicy::from_host_fence_timeout` drops
            // the deadline cap under async (where the wake loop runs
            // on `detach_isolated` with no client-side cancellation).
            // Pre-fix the policy capped at 50 s under async too,
            // racing the 60.166 s source-teardown wall-time.
            .with_wake_response_mode(wake_response_mode)
            // r3-A (T-8b-stress-r3): pin restore alloc placement to
            // THIS worker so the staged snapshot bytes match the
            // node the driver runs on. Same shape as the cold-boot
            // path's `NomadCHBackend::with_local_nomad_node_id`.
            .with_local_nomad_node_id(local_nomad_node_id.clone());
            let rb_inner = match shared_allocator {
                Some(a) => {
                    tracing::info!(
                        "snapshot wiring: shared vm_index allocator with backend (B18)"
                    );
                    rb_inner.with_shared_allocator(a)
                }
                None => {
                    tracing::warn!(
                        backend = %backend.name(),
                        "snapshot wiring: backend has no vm_index allocator; \
                         RealRestoreBackend falls back to private reservations \
                         (B18 race possible if create + restore concurrent)"
                    );
                    rb_inner
                }
            };
            // B19 fix: install the shared NomadCHBackend handle so the
            // post-wake `register_restored` call lands the restored
            // sandbox in the backend's in-memory state map. Without
            // this, every post-wake exec/stop/delete returned "sandbox
            // not found" and the vm_index slot leaked across the
            // controller's uptime.
            let rb_inner = match nomad_handle {
                Some(h) => {
                    tracing::info!(
                        "snapshot wiring: shared NomadCHBackend handle for \
                         register_restored (B19)"
                    );
                    rb_inner.with_nomad_handle(h)
                }
                None => {
                    tracing::warn!(
                        backend = %backend.name(),
                        "snapshot wiring: backend has no NomadCHBackend \
                         handle; register_restored will surface as a 500 on \
                         every wake (B19 wiring missing)"
                    );
                    rb_inner
                }
            };
            let rb: Arc<dyn RestoreBackend> = Arc::new(rb_inner);
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
            admin_ro_token,
            snapshot_store,
            ch_remote,
            restore_backend,
            wake_response_mode,
            wake_lifecycle,
            local_nomad_node_id,
            nomad_stop_permits,
            // R33-I1: same `Arc` installed on the inner NomadCHBackend
            // above; AppState's handle is what production tests / future
            // metrics-export reads through.
            user_home_mkfs_locks,
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

        // Takeover task. Gated behind
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
        // C-7-LT-PR2: periodically GC terminal wake_jobs rows older
        // than T_KEEP (5 min). Without this loop a row stays in pg
        // forever after wake completes; polls keep returning the
        // terminal state and the table grows unbounded. The proposal
        // (§ 5) chose to wire GC at 60 s cadence — cheap (one
        // indexed DELETE) and responsive enough that clients hitting
        // the T_KEEP boundary observe the 404 cleanly.
        if state.database.is_some() {
            sweep::spawn_wake_jobs_gc(state.clone());
        }
        // R19-C1: periodic takeover sweep that flips non-terminal
        // wake_jobs rows whose `lessee_updated_at` has gone stale
        // (controller crashed mid-wake) to `failed`/
        // `wake_worker_aborted`. Without it, GATE-C2's UNIQUE INDEX
        // (migration 0011) wedges the sandbox permanently after any
        // mid-wake controller crash — every subsequent wake POST
        // returns a 202 pointing at the dead wake_id and the client
        // polls forever. Spawn is gated on `database.is_some()` for
        // the same reason as `spawn_wake_jobs_gc`: no pg → no rows
        // to sweep. Sibling concern of `spawn_transient_state_takeover`
        // but on the `wake_jobs` table instead of `sandboxes`.
        if state.database.is_some() {
            sweep::spawn_wake_jobs_takeover(state.clone());
        }
        // T-8b-stress-r2 controller v34: host_dir GC sweep. Reaps
        // `<host_state_dir>/<sandbox-id>/` directories whose sandbox is
        // in a terminal state (or absent from the DB) with no pending
        // wake_jobs row, after a 1-hour grace. THIS is the load-bearing
        // fix for Bug 1 (`workspace.img does not exist`) — the per-alloc
        // `rm -rf` paths in CreateGuard::drop / stop_inner now leak the
        // dir on purpose; this sweeper is the catchall. See
        // `crates/sandbox/src/backend/nomad_ch.rs` doc comment ("Cleanup
        // contract") and `docs/reviews/sandbox-snapshot-restore-cluster-
        // 2026-05-25-T8b-stress-r2.md` for the full diagnosis. Gated on
        // `database.is_some()` for the same reason as the other pg-only
        // sweeps; single-tenant deploys keep the operator-wipe story.
        if state.database.is_some() {
            sweep::spawn_host_dir_gc(state.clone());
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

/// R5-S1 (security-r5) / cluster bug #21. Refuse to boot the controller
/// in the silent fail-OPEN configuration where snapshot/restore is
/// enabled but no persistence layer is wired.
///
/// **Why this matters.** The wake path's `do_restore_inner` calls
/// `Persistence::unseal` after `wait_for_livez` to recover the
/// per-sandbox signing key, then hands the key bytes to
/// `Backend::register_restored` to install the restored VM into the
/// backend's in-memory state map. When `state.persist=None`, the wake
/// path falls into a `tracing::warn!` skip branch — the wake still
/// returns 200 but the state map gets no entry, so every subsequent
/// `exec`/`stop`/`delete` returns `sandbox_not_found`, the `vm_index`
/// allocator slot leaks across the controller's uptime, and the
/// per-host pool saturates after a few wakes (cluster smoke 2026-05-23
/// r5 Appendix D observed exactly this).
///
/// **What this checks.** When `snapshot_enabled=true`:
///   - `persist=true` → `Ok(())` (the production-correct shape).
///   - `persist=false` + `test_override=true` → `Ok(())` (the
///     `ZEROSHIP_SANDBOX_TEST_DISABLE_PERSIST_ASSERTION=1` escape hatch
///     for dev fixtures that drive `StubRestoreBackend` without
///     persistence).
///   - `persist=false` + `test_override=false` → `Err(...)` (fail-CLOSED).
///
/// When `snapshot_enabled=false`, the persistence layer is optional;
/// returns `Ok(())` regardless.
///
/// **Why the override env var has a `ZEROSHIP_SANDBOX_TEST_` prefix**
/// (R6-A1). The env var is read on the production boot path (no
/// `#[cfg(test)]` gate — the assertion itself runs in prod, so tests
/// must be able to set the override at runtime without
/// `unsafe { std::env::set_var(...) }`). The earlier name
/// `SANDBOX_PERSIST_NONE_OK` was visually indistinguishable from
/// real prod env vars (`SANDBOX_PERSIST_AUTH`, `SANDBOX_PERSIST_DIR`),
/// so an operator who set it would silently re-enable the R5-S1
/// fail-OPEN shape that was B21 in prod. The explicit
/// `TEST_DISABLE_PERSIST_ASSERTION` suffix makes operator misuse
/// obvious from one glance at the unit file's environment block.
pub(crate) fn assert_persist_required_when_snapshot_enabled(
    snapshot_enabled: bool,
    persist_present: bool,
    test_override: bool,
) -> Result<(), String> {
    if snapshot_enabled && !persist_present && !test_override {
        return Err(
            "FATAL: SANDBOX_SNAPSHOT_ENABLED=true but persistence is disabled \
             (SANDBOX_PERSIST_AUTH != 1 or SANDBOX_AEAD_KEY_PATH unset). \
             Restored sandboxes would silently fail to register in the \
             backend state map (R5-S1 / cluster bug #21). \
             Fix: set SANDBOX_PERSIST_AUTH=1 + SANDBOX_AEAD_KEY_PATH to a \
             32-byte mode-0o400 file. \
             Test-only override (NOT for production): \
             ZEROSHIP_SANDBOX_TEST_DISABLE_PERSIST_ASSERTION=1."
                .to_string(),
        );
    }
    Ok(())
}

/// A1-FOLLOWUP (arch-r9 fail-CLOSED gap). Refuse to boot in the silent
/// fail-OPEN configuration where snapshot/restore writes to a non-local
/// L2 (GCS) AND no AEAD root KEK is configured: bare guest RAM would
/// land in the remote object store in clear, while the audit trail in
/// pg (`snapshot_aead_dek_id="v1"`, stamped unconditionally by
/// `snapshot_handler.rs`) would still claim the snapshot is encrypted.
///
/// **Why this matters.** A1 (commit `18e2034b`) wrapped the inner
/// snapshot store in `AeadSnapshotStore` when `RootKek::from_env`
/// returned `Ok(Some(_))`. But when the operator FORGOT to set
/// `SANDBOX_SNAPSHOT_ROOT_KEK_PATH` in a production-shaped
/// `tiered+GCS` deploy, boot logged a warning and continued — the
/// AEAD wrapper composed in passthrough mode, plaintext guest RAM
/// landed in GCS. The boot warning is invisible compared to the pg
/// audit row's "encrypted=v1" claim; an operator reading the row
/// would believe the snapshot is encrypted at rest when it is not.
///
/// **What this checks.** When `snapshot_enabled=true && use_gcs=true`:
///   - `kek_present=true` → `Ok(())` (the production-correct shape).
///   - `kek_present=false` + `test_override=true` → `Ok(())` (the
///     `ZEROSHIP_SANDBOX_TEST_ALLOW_UNENCRYPTED_REMOTE=1` escape hatch
///     for non-prod envs that explicitly opt in to plaintext-to-GCS).
///   - `kek_present=false` + `test_override=false` → `Err(...)`
///     (fail-CLOSED).
///
/// When `snapshot_enabled=false` OR `use_gcs=false` (local-only L1
/// store), the KEK is optional; the local-only warn-and-continue path
/// lives in the snapshot-store composition site (dev/test ergonomics —
/// a single-node dev box doesn't need at-rest encryption to a
/// non-existent L2).
///
/// **Why the override env var has a `ZEROSHIP_SANDBOX_TEST_` prefix**
/// (matches R6-A1's reasoning for the persist assertion's override).
/// The env var is read on the production boot path; an operator who
/// set a confusingly-named `SANDBOX_*` variant would re-enable the
/// exact audit-trail-vs-reality gap A1-FOLLOWUP closes. The explicit
/// `TEST_ALLOW_UNENCRYPTED_REMOTE` suffix screams operator-misuse the
/// moment it appears in a unit file's environment block.
pub(crate) fn assert_kek_required_for_remote_store(
    snapshot_enabled: bool,
    snapshot_use_gcs: bool,
    kek_present: bool,
    test_override: bool,
) -> Result<(), String> {
    if snapshot_enabled
        && snapshot_use_gcs
        && !kek_present
        && !test_override
    {
        return Err(
            "FATAL: SANDBOX_SNAPSHOT_ENABLED=true + SANDBOX_SNAPSHOT_USE_GCS=true \
             but SANDBOX_SNAPSHOT_ROOT_KEK_PATH is unset. Tiered L1+GCS without \
             AEAD writes guest RAM in clear to the remote object store, while \
             the pg audit row stamps snapshot_aead_dek_id=\"v1\" \
             (audit-trail-vs-reality gap, arch-r9 fail-CLOSED). \
             Fix: set SANDBOX_SNAPSHOT_ROOT_KEK_PATH to a 32-byte mode-0o400 \
             file owned by uid 0. \
             Test-only override (NOT for production): \
             ZEROSHIP_SANDBOX_TEST_ALLOW_UNENCRYPTED_REMOTE=1."
                .to_string(),
        );
    }
    Ok(())
}

/// Round-3 / Phase-3 CRITICAL #3: read the admin bearer ONCE at
/// boot. Mirrors `Persistence::AeadKey::from_path`.
///
/// This is a pure function over an optional path. The production
/// caller in `AppState::from_config` resolves
/// `SANDBOX_ADMIN_TOKEN_PATH` first then passes the path here, so
/// tests can drive the loader with a `tempfile::NamedTempFile` path
/// without mutating process env.
///
/// Three outcomes:
///   - `path = None` → `Ok(None)` (admin API disabled by config)
///   - file readable + mode 0o400 + owner uid 0 + non-empty → `Ok(Some(token))`
///   - ANYTHING else → `Err(...)` (refuse to boot loudly)
///
/// Distinguishes "admin API disabled" (legitimate config) from
/// "admin token misconfigured" (operator error) — Round-2 leaked
/// the latter as a silent fail-open via `.ok()?` on metadata().
///
/// The file's owner uid is also checked: only uid 0 (root) is
/// accepted (R9-S4d) — mode 0o400 alone is insufficient because a
/// non-root attacker who pre-creates a chmod-400 file at
/// `SANDBOX_ADMIN_TOKEN_PATH` before systemd starts could inject
/// an attacker-known admin bearer; the controller would then
/// register that token as the admin credential on boot, granting
/// the attacker full admin-API access (sandbox create/delete/exec/
/// file-tree everywhere) on first request. Strict "uid == 0"
/// matches the R9-S4 (snapshot KEK), R9-S4b (sealed-records AEAD
/// key) and R9-S4c (pg-password file) sibling invariants and the
/// systemd-style secret-loading convention at `/etc/zeroship/`.
pub(crate) fn load_admin_token(
    path: Option<&std::path::Path>,
) -> Result<Option<String>, String> {
    let Some(path) = path else {
        return Ok(None);
    };

    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
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
        let uid = meta.uid();
        if uid != 0 {
            return Err(format!(
                "SANDBOX_ADMIN_TOKEN_PATH={path:?}: owner uid {uid} != 0 \
                 (chown root:root the file)"
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

/// T1: boot-time guard against the operator footgun where both
/// `SANDBOX_ADMIN_TOKEN_PATH` and `SANDBOX_ADMIN_RO_TOKEN_PATH` point
/// at files containing the same secret (whether by symlink, identical
/// generated content, or paste error). If both bearers resolve to
/// the same string, the role-gate's "full ⊋ read-only" distinction
/// collapses: the RO bearer matches the Full ct_eq compare, so an
/// attacker who leaks the RO bearer trivially escalates to Full
/// admin via any destructive endpoint.
///
/// The check runs in `AppState::from_config` AFTER both env-resolved
/// `load_admin_token` calls have succeeded. We compare in
/// constant-time so the boot log doesn't leak which prefix of the
/// secrets matched — irrelevant in practice (boot happens once and
/// the error is fatal), but it keeps the invariant local to the
/// auth path's overall posture.
///
/// Three outcomes:
///   - both `None` → `Ok(())` (no tokens configured; admin API is
///     disabled by either env var being absent).
///   - one `Some`, other `None` → `Ok(())` (the asymmetric, legal
///     "Full only" or "RO only" deployment shapes).
///   - both `Some` with EQUAL contents → `Err(...)` (the footgun).
///   - both `Some` with DISTINCT contents → `Ok(())`.
pub(crate) fn assert_distinct_admin_tokens(
    full: Option<&str>,
    ro: Option<&str>,
) -> Result<(), String> {
    let (Some(f), Some(r)) = (full, ro) else {
        return Ok(());
    };
    // Constant-time compare of two configured boot-time secrets. The
    // boot path runs once; this is purely defense-in-depth so the
    // error log doesn't telegraph a partial match.
    use subtle::ConstantTimeEq;
    if f.as_bytes().ct_eq(r.as_bytes()).into() {
        return Err(
            "FATAL: SANDBOX_ADMIN_TOKEN_PATH and SANDBOX_ADMIN_RO_TOKEN_PATH \
             contain identical secrets. The read-only role exists to \
             limit blast radius on bearer leak; pointing both env vars \
             at the same secret collapses the Full ⊋ ReadOnly \
             distinction. Fix: generate two distinct random tokens, one \
             per file (chmod 0o400, chown root:root each). To run with \
             only the full bearer, unset SANDBOX_ADMIN_RO_TOKEN_PATH. \
             To run with only the RO bearer, unset \
             SANDBOX_ADMIN_TOKEN_PATH."
                .to_string(),
        );
    }
    Ok(())
}

/// Periodic backend probe. `probe()` updates the `healthy` flag
/// that `/readyz` exposes; without this loop the flag is set once
/// at boot and stays stale forever (e.g. true after kubectl auth
/// has expired). Re-probes every 30s on a dedicated OS thread with
/// its own compio runtime (R16-I1: same C-6 wedge fingerprint as
/// admin_handlers::teardown_source_for_snapshot — `backend.probe()`
/// can issue a multi-second blocking HTTP call against an unhealthy
/// agent; on the shared ntex worker runtime it would starve sibling
/// per-request tasks).
///
/// We don't wrap async calls in `catch_unwind` — `probe` is
/// designed to return `Result`, not panic. If it does panic the
/// task dies and re-probes stop; that's a real bug worth crashing
/// loudly rather than papering over.
fn start_health_loop(state: Arc<AppState>) {
    // Thread name MUST be ≤ 15 bytes — Linux `pr_set_name` truncates
    // anything longer, so the OS-level name visible in `ps`/`top -H`
    // gets clipped. Earlier draft `snap-health-loop` (16 B) silently
    // became `snap-health-loo` (R17-I1). The
    // `detach::tests::all_known_thread_names_fit_kernel_limit`
    // regression test pins this contract.
    crate::detach::detach_isolated("snap-health", move || async move {
        loop {
            // Top-of-loop shutdown
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
    });
}

// ────────────────────────────────────────────────────────────────────
// Heartbeat And Takeover
// ────────────────────────────────────────────────────────────────────

/// `SANDBOX_HA_HEARTBEAT_SECS`. Cadence at which the heartbeat task
/// updates `sandbox.hosts.last_heartbeat`. Default 5 s; validated at
/// boot to be > 0 and `lease_ttl >= 4 × heartbeat`.
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
    // R16-I1: dedicated OS thread + private compio runtime. The pg
    // `heartbeat()` call is a single SQL UPDATE; the wedge risk here
    // is lower than for the takeover task (which runs probes), but
    // uniformity + decoupling from the ntex worker runtime means a
    // pg-side stall cannot back-pressure HTTP wake handlers.
    crate::detach::detach_isolated("snap-heartbeat", move || async move {
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
            // Shutdown check.
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
    });
}

/// For each newly-owned sandbox the takeover SQL produced, run the
/// boot-time probe-and-register pipeline against the local sealed
/// record. The new owner ends up with an in-memory registry entry
/// (or the row is marked recreating / unreachable / lost based on
/// the probe result, exactly the way `restore_at_startup`'s loop
/// classifies things — code path is shared via
/// `restore::probe_and_register_one`).
///
/// Current limitation: the new owner's persist dir might not have the
/// sealed record. When the seal is missing, the row is marked `lost`
/// and we
/// bump `sandbox_ha_takeover_orphan_total`.
///
/// **Does nothing** when:
///   - state.persist is None (controller booted with persistence
///     off — e.g. SANDBOX_PERSIST_AUTH != 1). Without sealed
///     records there's no way to recover the signing key, so the
///     row stays `running` in pg and the operator will see a
///     stale-row alert via the admin tooling. This is the
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
                    "sandbox HA: takeover rehydrate seal missing on this host (cross-host sync is still missing)"
                );
                metrics::inc_takeover_orphan();
            }
            // Every non-restored outcome also bumps a labeled counter
            // so operators can alert on takeover failures by class.
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
/// 1. Refresh the `sandbox_ha_heartbeat_lag_seconds` gauge and fire
///    `sandbox_ha_clock_rewind_total` if pg sees
///    `now() - last_heartbeat < 0`.
/// 2. Read `dead_hosts(lease_ttl)`.
/// 3. For each dead host (excluding self — defensive), issue the
///    CAS-guarded takeover UPDATE per § 11.2.
/// 4. Bump `sandbox_ha_takeover_total{reason="lease_expiration"}`
///    by the number of rows successfully reclaimed.
///
/// The task exits cleanly only on process shutdown; transient
/// errors are logged and the loop continues. The takeover write is
/// a single SQL statement plus a host-status flip in the same
/// transaction.
pub fn spawn_takeover_task(state: Arc<AppState>) {
    // R16-I1: dedicated OS thread + private compio runtime. This loop
    // is the most wedge-prone of the periodic tasks — `rehydrate_after_takeover`
    // issues per-sandbox signed `/version` probes via the backend, each of
    // which can sit on a multi-second `ureq` timeout against a half-dead
    // worker. On the shared ntex worker runtime that would starve every
    // sibling wake/heartbeat task for the duration of the probe (the
    // same C-6 fingerprint as admin_handlers::teardown_source_for_snapshot).
    crate::detach::detach_isolated("snap-takeover", move || async move {
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
            // Shutdown check.
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
                        // Pg-side clock rewind. Healthy fleet
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
                // Shutdown check
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
                            // Post-takeover rehydrate. For each taken
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
    });
}

// ────────────────────────────────────────────────────────────────────
// Shutdown-Flag Unit Tests
// ────────────────────────────────────────────────────────────────────
//
// The binary's main loop wires
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
// Boot-Loader Unit Tests
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

    /// Helper for R9-S4d tests whose 0o400 positive arm depends
    /// on whether the test-runner is root: non-root runners can't
    /// materialise a uid-0 file, so the loader correctly rejects
    /// with the owner-uid error; root runners exercise the happy
    /// path. Returns the runner's effective uid (always 0 on
    /// non-unix targets, where the uid check is compiled out).
    #[cfg(unix)]
    fn current_file_uid(path: &std::path::Path) -> u32 {
        use std::os::unix::fs::MetadataExt as _;
        std::fs::metadata(path).unwrap().uid()
    }
    #[cfg(not(unix))]
    fn current_file_uid(_path: &std::path::Path) -> u32 {
        0
    }

    #[test]
    fn loader_reads_token_when_mode_0o400() {
        let path = temp_path();
        let token = "boot-loader-token-0o400-aaaa";
        write_with_mode(&path, token, 0o400);
        // R9-S4d: the "0o400 must pass" arm only holds when the
        // file is root-owned. In CI/dev the test-runner uid is
        // non-zero, so the loader now correctly refuses the file.
        // Pin the positive case behind a uid guard; the
        // non-root-owned-rejection assertion is covered by
        // `load_admin_token_rejects_non_root_owned_file` below.
        let runner_uid = current_file_uid(&path);
        let got = load_admin_token(Some(&path));
        cleanup(&path);
        if runner_uid == 0 {
            assert!(matches!(&got, Ok(Some(s)) if s == token), "got {got:?}");
        } else {
            let err = got.expect_err("0o400 non-root-owned must be rejected (R9-S4d)");
            assert!(
                err.contains("owner uid") && err.contains("!= 0"),
                "error must mention owner uid != 0; got: {err}"
            );
        }
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
        // R9-S4d: the empty-file check fires AFTER the mode + uid
        // checks pass. In a non-root test runner the uid check
        // trips first; assert whichever error surfaces. Both
        // branches are correct refusals — this test pins "loader
        // rejects an empty 0o400 file" rather than the specific
        // message.
        let runner_uid = current_file_uid(&path);
        let got = load_admin_token(Some(&path));
        cleanup(&path);
        let err = got.expect_err("empty / non-root file must yield Err");
        if runner_uid == 0 {
            assert!(err.contains("empty"), "error must mention 'empty'; got {err}");
        } else {
            assert!(
                err.contains("owner uid") && err.contains("!= 0"),
                "error must mention owner uid != 0; got: {err}"
            );
        }
    }

    #[test]
    fn loader_trims_trailing_newline() {
        let path = temp_path();
        let token = "trim-newline-token-bbbb";
        write_with_mode(&path, &format!("{token}\n"), 0o400);
        // R9-S4d: same uid-guard logic as
        // `loader_reads_token_when_mode_0o400`.
        let runner_uid = current_file_uid(&path);
        let got = load_admin_token(Some(&path));
        cleanup(&path);
        if runner_uid == 0 {
            assert!(matches!(&got, Ok(Some(s)) if s == token), "got {got:?}");
        } else {
            let err = got.expect_err("0o400 non-root-owned must be rejected (R9-S4d)");
            assert!(
                err.contains("owner uid") && err.contains("!= 0"),
                "error must mention owner uid != 0; got: {err}"
            );
        }
    }

    /// R9-S4d: a 0o400 admin-token file owned by a non-root uid
    /// (i.e. the test-runner user, which is uid != 0 in CI/dev)
    /// MUST be refused. Without the owner check, a non-root
    /// attacker who pre-creates a chmod-400 file at
    /// `SANDBOX_ADMIN_TOKEN_PATH` before the controller starts can
    /// inject an attacker-known admin bearer; the controller
    /// registers it as the admin credential on boot, yielding full
    /// admin-API access (sandbox create/delete/exec/file-tree
    /// everywhere) on first request. Sibling of R9-S4 (snapshot
    /// KEK), R9-S4b (sealed-records AEAD key) and R9-S4c (pg
    /// password file).
    #[cfg(unix)]
    #[test]
    fn load_admin_token_rejects_non_root_owned_file() {
        let path = temp_path();
        write_with_mode(&path, "attacker-known-admin-bearer", 0o400);
        // The file is created by the test-runner process, so its
        // uid == effective uid of the runner. If that's 0 there's
        // no non-root-owned file to materialise — skip (the
        // positive arm is covered by
        // `load_admin_token_accepts_root_owned_file_when_running_as_root`).
        let runner_uid = current_file_uid(&path);
        if runner_uid == 0 {
            cleanup(&path);
            eprintln!(
                "skipping load_admin_token_rejects_non_root_owned_file: \
                 running as root, can't materialise a non-root-owned file"
            );
            return;
        }
        let got = load_admin_token(Some(&path));
        cleanup(&path);
        let err = got.expect_err(
            "non-root-owned admin-token file must be refused even at 0o400",
        );
        assert!(
            err.contains("owner uid") && err.contains("!= 0"),
            "error must mention owner uid != 0; got: {err}"
        );
    }

    // ─── T1: distinct-token boot guard ──────────────────────────
    //
    // Pure-function tests over `assert_distinct_admin_tokens`. The
    // production caller in `AppState::from_config` runs this AFTER
    // both `load_admin_token` calls succeed; we test the truth
    // table here without spinning up a backend probe.

    #[test]
    fn distinct_admin_tokens_both_none_is_ok() {
        // Default-disabled shape. The role-gate will 503 every
        // /admin/* endpoint; nothing to compare.
        assert_distinct_admin_tokens(None, None)
            .expect("both None → admin API disabled, no comparison needed");
    }

    #[test]
    fn distinct_admin_tokens_only_full_configured_is_ok() {
        // Legal asymmetric shape: operator wired the full bearer but
        // hasn't provisioned an RO yet. Read endpoints still work
        // via the full bearer; RO bearers don't exist on the wire.
        assert_distinct_admin_tokens(Some("full-bearer-aaaa"), None)
            .expect("full only → legal");
    }

    #[test]
    fn distinct_admin_tokens_only_ro_configured_is_ok() {
        // Legal asymmetric shape: operator wired only the RO
        // bearer (e.g. a dashboard-only deploy where there's no
        // operator with destructive privileges). Destructive
        // endpoints 503 `admin_api_disabled`; read endpoints work
        // via the RO bearer.
        assert_distinct_admin_tokens(None, Some("ro-bearer-aaaaa"))
            .expect("ro only → legal");
    }

    #[test]
    fn distinct_admin_tokens_with_distinct_contents_is_ok() {
        // The intended production shape: two distinct random tokens,
        // one per file.
        assert_distinct_admin_tokens(
            Some("full-bearer-aaaaaaaaaaaaaaaaaaaaaaaa"),
            Some("ro-bearer-bbbbbbbbbbbbbbbbbbbbbbbbbb"),
        )
        .expect("two distinct tokens → legal");
    }

    /// T1 boot guard: equal contents on both paths must refuse to
    /// boot. The footgun this closes is an operator who symlinks
    /// `SANDBOX_ADMIN_RO_TOKEN_PATH` at the full-bearer file, or
    /// generates the two files from the same source — silently
    /// collapsing the role distinction.
    #[test]
    fn distinct_admin_tokens_with_equal_contents_is_err() {
        let shared = "secret-pasted-into-both-files-aaaa";
        let err = assert_distinct_admin_tokens(Some(shared), Some(shared))
            .expect_err("equal contents must refuse to boot");
        assert!(
            err.contains("FATAL"),
            "error must announce FATAL severity; got: {err}"
        );
        assert!(
            err.contains("SANDBOX_ADMIN_TOKEN_PATH")
                && err.contains("SANDBOX_ADMIN_RO_TOKEN_PATH"),
            "error must name both env vars so the operator can locate \
             the misconfig; got: {err}"
        );
    }

    /// Negative test: tokens differing in only one byte are still
    /// distinct. Belt-and-suspenders against a sloppy substring
    /// compare regression.
    #[test]
    fn distinct_admin_tokens_with_one_byte_difference_is_ok() {
        assert_distinct_admin_tokens(
            Some("matching-prefix-aaaaaaaaaaaaaaa1"),
            Some("matching-prefix-aaaaaaaaaaaaaaa2"),
        )
        .expect("one-byte difference is still distinct");
    }

    /// R9-S4d positive arm: when the test runs as root, a 0o400
    /// admin-token file owned by root passes the check. Skipped
    /// when not running as root (the common case in CI/dev) — the
    /// negative arm above already pins the bug-fix assertion in
    /// non-root environments.
    #[cfg(unix)]
    #[test]
    fn load_admin_token_accepts_root_owned_file_when_running_as_root() {
        let path = temp_path();
        let token = "root-owned-admin-bearer-cccc";
        write_with_mode(&path, token, 0o400);
        let runner_uid = current_file_uid(&path);
        if runner_uid != 0 {
            cleanup(&path);
            eprintln!(
                "skipping load_admin_token_accepts_root_owned_file_when_running_as_root: \
                 not running as root, can't create a root-owned admin-token file"
            );
            return;
        }
        let got = load_admin_token(Some(&path));
        cleanup(&path);
        assert!(
            matches!(&got, Ok(Some(s)) if s == token),
            "root-owned 0o400 admin-token file must load; got {got:?}"
        );
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

    /// Minimal config that satisfies `Backend::builder(&cfg).build()` for the
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
                runtime_dir: std::path::PathBuf::from("/var/lib/zeroship/ch"),
                host_state_dir: std::path::PathBuf::from("/var/zeroship/ch"),
                user_home_dir_root: std::path::PathBuf::from(
                    "/var/zeroship/ch/users",
                ),
                vm_index_floor: 1,
                vm_index_ceil: 200,
                alloc_running_timeout_secs: 120,
                agent_livez_timeout_secs: 30,
                host_fence_timeout_secs: 30,
                startup_orphan_cleanup: false,
                subnet_second_octet: 99,
                vm_index_release_delay_secs: 0, // r24-A2-S3: test default 0
                nomad_stop_concurrency: 16,     // r30-A1: prod default
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
            driver_stages_disk_images: false,
        }
    }

    fn min_state() -> AppState {
        let cfg = min_cfg();
        let backend = Backend::builder(&cfg).build().expect("backend");
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

    // ─── T1: with_admin_ro_token mirror tests ─────────────────────

    #[test]
    fn admin_ro_token_setter_rejects_empty() {
        let state = min_state();
        match state.with_admin_ro_token(Some(String::new())) {
            Ok(_) => panic!("empty string must yield Err"),
            Err(e) => assert!(
                e.contains("empty"),
                "error message must mention 'empty'; got {e}"
            ),
        }
    }

    #[test]
    fn admin_ro_token_setter_accepts_non_empty() {
        let state = min_state();
        let state = match state
            .with_admin_ro_token(Some("ro-bearer-abcdef".into()))
        {
            Ok(s) => s,
            Err(e) => panic!("non-empty string must be accepted: {e}"),
        };
        assert_eq!(
            state.admin_ro_token(),
            Some("ro-bearer-abcdef"),
            "the reader must surface the wrapped RO token"
        );
        assert!(
            state.admin_token().is_none(),
            "with_admin_ro_token must not touch the full-bearer field"
        );
    }

    #[test]
    fn admin_ro_token_setter_none_clears_field() {
        let state = min_state();
        let state = match state
            .with_admin_ro_token(Some("ro-bearer-to-clear-aaaa".into()))
        {
            Ok(s) => s,
            Err(e) => panic!("non-empty accepted: {e}"),
        };
        assert!(state.admin_ro_token().is_some(), "precondition: set");
        let state = match state.with_admin_ro_token(None) {
            Ok(s) => s,
            Err(e) => panic!("None clears unconditionally: {e}"),
        };
        assert!(state.admin_ro_token().is_none(), "None must clear the field");
    }

    #[test]
    fn full_and_ro_setters_are_independent() {
        // Both bearers can be set without one clobbering the other.
        // The role-gate distinguishes them on the auth path; the
        // builders are pure field setters.
        let state = min_state();
        let state = state
            .with_admin_token(Some("full-bearer-aaaaaaaaaaaaaaaa".into()))
            .expect("non-empty full accepted");
        let state = state
            .with_admin_ro_token(Some("ro-bearer-bbbbbbbbbbbbbbbb".into()))
            .expect("non-empty ro accepted");
        assert_eq!(
            state.admin_token(),
            Some("full-bearer-aaaaaaaaaaaaaaaa")
        );
        assert_eq!(
            state.admin_ro_token(),
            Some("ro-bearer-bbbbbbbbbbbbbbbb")
        );
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

    /// Minimal config that satisfies `Backend::builder(&cfg).build()` for the
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
                runtime_dir: std::path::PathBuf::from("/var/lib/zeroship/ch"),
                host_state_dir: std::path::PathBuf::from("/var/zeroship/ch"),
                user_home_dir_root: std::path::PathBuf::from(
                    "/var/zeroship/ch/users",
                ),
                vm_index_floor: 1,
                vm_index_ceil: 200,
                alloc_running_timeout_secs: 120,
                agent_livez_timeout_secs: 30,
                host_fence_timeout_secs: 30,
                startup_orphan_cleanup: false,
                subnet_second_octet: 99,
                vm_index_release_delay_secs: 0, // r24-A2-S3: test default 0
                nomad_stop_concurrency: 16,     // r30-A1: prod default
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
            driver_stages_disk_images: false,
        }
    }

    fn min_state() -> AppState {
        let cfg = min_cfg();
        let backend = Backend::builder(&cfg).build().expect("backend");
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
        let state = state.with_persistence(p.clone());
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
        let state = state.with_persistence(first.clone());
        let state = state.with_persistence(second.clone());
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

    /// Minimal config that satisfies `Backend::builder(&cfg).build()` for the
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
                runtime_dir: std::path::PathBuf::from("/var/lib/zeroship/ch"),
                host_state_dir: std::path::PathBuf::from("/var/zeroship/ch"),
                user_home_dir_root: std::path::PathBuf::from(
                    "/var/zeroship/ch/users",
                ),
                vm_index_floor: 1,
                vm_index_ceil: 200,
                alloc_running_timeout_secs: 120,
                agent_livez_timeout_secs: 30,
                host_fence_timeout_secs: 30,
                startup_orphan_cleanup: false,
                subnet_second_octet: 99,
                vm_index_release_delay_secs: 0, // r24-A2-S3: test default 0
                nomad_stop_concurrency: 16,     // r30-A1: prod default
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
            driver_stages_disk_images: false,
        }
    }

    fn min_state() -> AppState {
        let cfg = min_cfg();
        let backend = Backend::builder(&cfg).build().expect("backend");
        AppState::new_fixture(cfg, backend)
    }

    #[test]
    fn with_config_replaces_existing() {
        // The setter is the only legal out-of-crate write path to
        // `config`; flip a discriminator (port number) to verify the
        // new config landed.
        let mut first = min_cfg();
        first.port = 11111;
        let backend = Backend::builder(&first).build().expect("backend");
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

// ────────────────────────────────────────────────────────────────────
// R5-S1 / cluster bug #21 — boot-time fail-CLOSED for the
// `snapshot_enabled=true && persist=None` configuration. The helper is
// a pure function over three booleans so we can pin every cell of the
// truth table without spinning up a backend probe.
// ────────────────────────────────────────────────────────────────────
#[cfg(test)]
mod persist_required_assertion_tests {
    use super::assert_persist_required_when_snapshot_enabled as check;

    #[test]
    fn snapshot_off_persist_off_is_ok() {
        // Phase-A feature-flagged off: persist is optional. The deferred
        // persist=None shape is the historical default.
        check(false, false, false).expect("snapshot disabled → persist optional");
    }

    #[test]
    fn snapshot_off_persist_on_is_ok() {
        // Snapshot off but operator wired persistence anyway (e.g. for
        // restart-restore of long-lived sandboxes). Allowed.
        check(false, true, false).expect("snapshot disabled + persist on → ok");
    }

    #[test]
    fn snapshot_on_persist_on_is_ok() {
        // The production-correct shape under Phase B. No assertion fires.
        check(true, true, false).expect("snapshot+persist both on → ok");
    }

    #[test]
    fn snapshot_on_persist_off_without_override_is_err() {
        // The fail-OPEN configuration cluster bug #21 surfaced. MUST
        // refuse to boot.
        let err = check(true, false, false)
            .expect_err("snapshot_enabled && persist=None must refuse to boot");
        assert!(
            err.contains("FATAL"),
            "error must announce FATAL severity; got: {err}"
        );
        assert!(
            err.contains("SANDBOX_PERSIST_AUTH"),
            "error must mention the env var the operator must set; got: {err}"
        );
        assert!(
            err.contains("R5-S1") || err.contains("#21"),
            "error must reference the deferred entry / bug; got: {err}"
        );
    }

    #[test]
    fn snapshot_on_persist_off_with_test_override_is_ok() {
        // Dev/test fixtures that drive `StubRestoreBackend` without a
        // persistence layer use the explicit
        // `ZEROSHIP_SANDBOX_TEST_DISABLE_PERSIST_ASSERTION=1` escape
        // hatch (R6-A1 renamed from the old `SANDBOX_PERSIST_NONE_OK`
        // so operator misuse is obvious from a glance at a unit file).
        // The override is intentional, named, and visible in the env
        // block of any production unit it appears in.
        check(true, false, true).expect(
            "ZEROSHIP_SANDBOX_TEST_DISABLE_PERSIST_ASSERTION=1 \
             overrides the assertion",
        );
    }
}

// ────────────────────────────────────────────────────────────────────
// A1-FOLLOWUP (arch-r9 fail-CLOSED gap) — boot-time fail-CLOSED for the
// `snapshot_enabled=true && snapshot_use_gcs=true && kek_path=None`
// configuration. Truth table is pinned per-cell so a future drift in
// the assertion (e.g. accidentally gating on `snapshot_enabled` alone,
// which would break L1-only dev fixtures) fails loudly.
// ────────────────────────────────────────────────────────────────────
#[cfg(test)]
mod kek_required_for_remote_store_tests {
    use super::assert_kek_required_for_remote_store as check;

    #[test]
    fn tiered_gcs_with_kek_is_ok() {
        // The production-correct shape: snapshot enabled, GCS tier
        // wired, KEK file path set. AEAD wraps the tiered store.
        check(true, true, true, false).expect(
            "snapshot_enabled + use_gcs + kek_present → ok (prod shape)",
        );
    }

    #[test]
    fn tiered_gcs_without_kek_is_err() {
        // The fail-OPEN shape A1-FOLLOWUP closes: prod-shaped deploy
        // with GCS enabled but the operator forgot the KEK env. MUST
        // refuse to boot — plaintext guest RAM in GCS while the pg
        // audit row claims "encrypted=v1" is the exact
        // audit-trail-vs-reality gap the assertion exists to prevent.
        let err = check(true, true, false, false).expect_err(
            "snapshot_enabled + use_gcs + kek_unset must refuse to boot",
        );
        assert!(
            err.contains("FATAL"),
            "error must announce FATAL severity; got: {err}"
        );
        assert!(
            err.contains("SANDBOX_SNAPSHOT_ROOT_KEK_PATH"),
            "error must name the env var the operator must set; got: {err}"
        );
        assert!(
            err.contains("SANDBOX_SNAPSHOT_USE_GCS"),
            "error must name the GCS flag so the operator sees which \
             mode triggered the assertion; got: {err}"
        );
    }

    #[test]
    fn local_only_with_kek_is_ok() {
        // Operator wired the KEK even though the store is L1-only —
        // perfectly fine; AEAD encrypts at rest on disk too.
        check(true, false, true, false).expect(
            "snapshot_enabled + L1-only + kek_present → ok",
        );
    }

    #[test]
    fn local_only_without_kek_is_ok() {
        // Dev/test shape: snapshot enabled but L1-only, no KEK. The
        // assertion does NOT fire — the warn-and-continue path lives in
        // the snapshot-store composition site for local-only stores
        // (dev-box ergonomics; no remote leak surface).
        check(true, false, false, false).expect(
            "snapshot_enabled + L1-only + kek_unset → ok (dev ergonomics)",
        );
    }

    #[test]
    fn snapshot_disabled_is_ok_regardless_of_kek() {
        // Feature flag off: KEK is irrelevant. Phase-A default shape.
        check(false, false, false, false).expect("snap off → ok");
        check(false, false, true, false).expect("snap off + kek set → ok");
        check(false, true, false, false).expect("snap off + use_gcs ignored → ok");
        check(false, true, true, false).expect("snap off + use_gcs + kek → ok");
    }

    #[test]
    fn tiered_gcs_without_kek_with_test_override_is_ok() {
        // Non-prod envs that need plaintext-to-GCS for debugging set
        // ZEROSHIP_SANDBOX_TEST_ALLOW_UNENCRYPTED_REMOTE=1. The
        // override is intentional, named, and visible in any unit
        // file's environment block.
        check(true, true, false, true).expect(
            "ZEROSHIP_SANDBOX_TEST_ALLOW_UNENCRYPTED_REMOTE=1 \
             overrides the assertion",
        );
    }
}


// ────────────────────────────────────────────────────────────────────
// A1 (audit-r1) — prod-shape AEAD wrap of the snapshot store.
//
// Verifies the wrap composition the production `AppState::from_config`
// path produces when `RootKek::from_env` returns `Some(kek)`: the
// resulting `Arc<dyn SnapshotStore>` round-trips put/get AND the bytes
// landing on the L1 root are ciphertext (not plaintext).
//
// We don't re-test the cipher itself (covered exhaustively in
// `snapshot_aead::tests`); we test that the WIRING produces the
// expected on-disk shape. Without this assertion the prior wiring's
// "AEAD never composed" footgun could regress silently — the
// snapshot/restore round-trips through plaintext L1 just as cleanly as
// through wrapped L1.
// ────────────────────────────────────────────────────────────────────
#[cfg(test)]
mod prod_aead_wrap_tests {
    use super::*;
    use crate::snapshot_aead::{AeadSnapshotStore, RootKek};
    use crate::snapshot_store::LocalDiskSnapshotStore;

    /// Build the production wrap shape (L1-only branch) explicitly and
    /// assert the on-L1 `memory-ranges` blob is ciphertext, not the
    /// plaintext that was handed to `put`. This mirrors the wrap in
    /// `from_config`'s `else` branch (kek_present, !use_gcs) so a
    /// regression that drops the AEAD wrap fails this test loudly.
    #[test]
    fn prod_l1_wrap_produces_ciphertext_on_disk() {
        let root = std::env::temp_dir().join(format!(
            "zsbx-a1-wrap-{}-{}",
            std::process::id(),
            uuid::Uuid::now_v7().simple()
        ));
        std::fs::create_dir_all(&root).expect("mkdir tmp root");
        let l1_root = root.join("l1");
        let src = root.join("src");
        std::fs::create_dir_all(&src).expect("mkdir src");

        // CH artifact triple — config.json and state.json are plaintext;
        // memory-ranges is the byte sequence we'll grep for post-put.
        let plaintext_marker: Vec<u8> = (0u32..(64 * 1024))
            .flat_map(|i| i.to_be_bytes().into_iter())
            .collect();
        std::fs::write(src.join("config.json"), b"{\"cfg\":1}").expect("write cfg");
        std::fs::write(src.join("state.json"), b"{\"st\":2}").expect("write st");
        std::fs::write(src.join("memory-ranges"), &plaintext_marker)
            .expect("write mem");

        let kek = RootKek::from_bytes([0xc3; 32]);
        // EXACTLY the L1-only kek-present branch from `from_config`.
        let inner = LocalDiskSnapshotStore::new(l1_root.clone());
        let store: Arc<dyn SnapshotStore> =
            Arc::new(AeadSnapshotStore::new(inner, Some(kek)));

        let meta = store
            .put("sbx_a1_wrap_l1", &src, "v51.1")
            .expect("put round-trip");

        // 1. ch_version is annotated by the AEAD layer when active —
        //    proves the wrap is on the put path (vs. a passthrough or
        //    bypassed wrap).
        assert!(
            meta.ch_version.contains("+aead-cc20p1305"),
            "wrap must annotate ch_version with AEAD tag; got {}",
            meta.ch_version
        );

        // 2. The L1 `memory-ranges` blob must NOT contain the plaintext
        //    marker — if AEAD encryption ran, the ciphertext + header
        //    bytes overwrite the raw pattern.
        let l1_mr =
            std::path::PathBuf::from(&meta.artifact_path).join("memory-ranges");
        let on_disk = std::fs::read(&l1_mr).expect("read l1 memory-ranges");
        assert!(
            on_disk.len() >= 1024,
            "on-disk blob shorter than expected: {}",
            on_disk.len()
        );
        // The AEAD file magic confirms it's a wrapped artifact.
        assert_eq!(
            &on_disk[0..8],
            b"ZSBXAEAD",
            "L1 blob must begin with ZSBXAEAD magic; got {:?}",
            &on_disk[0..8.min(on_disk.len())]
        );
        // Deep-window plaintext check: confirm a contiguous 256-byte
        // slice of the plaintext does NOT appear anywhere in `on_disk`.
        // The AEAD ciphertext is uniformly random; the chance of
        // accidental collision is 2^-2048.
        let needle = &plaintext_marker[1024..1024 + 256];
        let found_plain = on_disk.windows(needle.len()).any(|w| w == needle);
        assert!(
            !found_plain,
            "plaintext byte pattern must NOT appear in the L1 blob \
             (AEAD wrap is the only thing standing between guest RAM \
             and disk-resident plaintext)"
        );

        // 3. Round-trip: get must recover the plaintext byte-for-byte.
        let target = root.join("target");
        store
            .get("sbx_a1_wrap_l1", &target, &meta.sha256)
            .expect("get round-trip");
        let restored = std::fs::read(target.join("memory-ranges"))
            .expect("read restored memory-ranges");
        assert_eq!(
            restored, plaintext_marker,
            "AEAD wrap must round-trip plaintext byte-for-byte"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// The kek-absent branch — covers the dev-mode `Ok(None)` from
    /// `RootKek::from_env` where the wrapper is composed but in
    /// passthrough. Asserts the L1 blob is plaintext (proves the
    /// kek_present=false branch in `from_config` is the dev shape and
    /// nothing else).
    ///
    /// This is the "audit-trail-honest" half: when no key is wired the
    /// wrap is structurally identical (still composed) but observably
    /// transparent — so the boot log is the single source of truth on
    /// the actual posture.
    #[test]
    fn prod_l1_wrap_passthrough_when_no_kek() {
        let root = std::env::temp_dir().join(format!(
            "zsbx-a1-wrap-pt-{}-{}",
            std::process::id(),
            uuid::Uuid::now_v7().simple()
        ));
        std::fs::create_dir_all(&root).expect("mkdir tmp root");
        let l1_root = root.join("l1");
        let src = root.join("src");
        std::fs::create_dir_all(&src).expect("mkdir src");

        let plaintext_marker: Vec<u8> = (0u32..16_384)
            .flat_map(|i| i.to_be_bytes().into_iter())
            .collect();
        std::fs::write(src.join("config.json"), b"{\"cfg\":1}").expect("write cfg");
        std::fs::write(src.join("state.json"), b"{\"st\":2}").expect("write st");
        std::fs::write(src.join("memory-ranges"), &plaintext_marker)
            .expect("write mem");

        let inner = LocalDiskSnapshotStore::new(l1_root.clone());
        let store: Arc<dyn SnapshotStore> =
            Arc::new(AeadSnapshotStore::new(inner, None));

        let meta = store
            .put("sbx_a1_wrap_pt", &src, "v51.1")
            .expect("put passthrough");
        assert!(
            !meta.ch_version.contains("+aead"),
            "passthrough must NOT annotate ch_version with AEAD tag; got {}",
            meta.ch_version
        );

        let l1_mr =
            std::path::PathBuf::from(&meta.artifact_path).join("memory-ranges");
        let on_disk = std::fs::read(&l1_mr).expect("read l1 memory-ranges");
        // Passthrough → bytes on disk match what we put in.
        assert_eq!(
            on_disk, plaintext_marker,
            "passthrough must land plaintext on L1 — the kek_present=false \
             warn log is the only signal an operator gets"
        );

        let _ = std::fs::remove_dir_all(&root);
    }
}
