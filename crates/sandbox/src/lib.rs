//! `zeroship-sandbox` — pluggable sandbox-session controller.
//!
//! The crate is split between a thin binary (`src/main.rs`) and this
//! library, so integration tests and the lifecycle e2e example can
//! reuse `AppState`, the [`backend::Backend`] enum, and the session
//! registry without re-launching the HTTP server in-process.
//!
//! See [`backend`] for the backend contract and the docker / k8s
//! implementations.

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

use std::sync::Arc;
use std::time::Duration;

use crate::backend::Backend;
use crate::config::SandboxConfig;
use crate::db::{Database, LATEST_MIGRATION_VERSION};
use crate::persist::Persistence;
use crate::preview_share_handlers::MintRateLimiter;
use crate::registry::SandboxRegistry;

/// Shared application state passed to every handler.
#[allow(missing_debug_implementations)]
pub struct AppState {
    pub config: SandboxConfig,
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
    pub database: Option<Arc<Database>>,
    /// Round-1 fixer / CRITICAL #4: shared sealed-record persistence
    /// handle, plumbed for the Phase-2.5 takeover-rehydrate path.
    /// `None` mirrors the pre-fix disabled shape — the handle exists
    /// only when `SANDBOX_PERSIST_AUTH=1` was set and a key file is
    /// readable.
    pub persist: Option<Arc<Persistence>>,
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
        let state = Arc::new(Self {
            config,
            sandboxes: registry,
            backend,
            mint_rate_limiter: Some(MintRateLimiter::new()),
            database,
            persist,
        });
        // Background re-probe so /readyz reflects current backend
        // state. Without this, the `is_healthy()` flag is set once
        // at boot and stays true even if kubectl auth expires or
        // the cluster goes unreachable.
        start_health_loop(state.clone());

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
        Ok(state)
    }
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
            compio::time::sleep(Duration::from_secs(30)).await;
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
            compio::time::sleep(interval).await;
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
            other => {
                tracing::info!(
                    sandbox_id = %ts.sandbox_id,
                    outcome = ?other,
                    "sandbox HA: takeover rehydrate non-restored outcome"
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
            compio::time::sleep(interval).await;

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
