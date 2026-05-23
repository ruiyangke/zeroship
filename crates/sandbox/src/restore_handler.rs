//! `RestoreHandler` — orchestrates `snapshotted → restoring → running`
//! per § 5 / § 6.1 / § 8 of the snapshot-restore proposal.
//!
//! Source-of-truth: docs/proposals/sandbox-snapshot-restore.md
//!
//! ## Flow
//!
//! 1. Read row; refuse anything but `Snapshotted` / `SnapshottedSuspect`.
//! 2. CAS `snapshotted → restoring` (generation-fenced).
//! 3. Allocate `vm_index`: try the row's recorded `snapshot_vm_index`
//!    first per § 5.0/§ 5.1 (sticky); fall back to a fresh `alloc()`.
//!    Cluster-exhaustion → 503 `vm_index_unavailable`.
//! 4. `SnapshotStore::get(sandbox_id, alloc_dir, expected_sha256)`.
//!    `ChecksumMismatch` → CAS `restoring → snapshotted_suspect`,
//!    return 500 `snapshot_corrupt`.
//! 5. Rewrite `config.json` per § 5.1: substitute `payload.cmdline`
//!    IP arg, `disks[].path`, `net[].tap`, `net[].mac`, `fs[].socket`,
//!    `serial.file`. Disks + IP are documented no-ops in v1 but the
//!    code paths exist for the v2 cross-cluster work.
//! 6. Submit a Nomad job (or equivalent backend op) for the new alloc
//!    with `ZSBX_RESTORE_FROM=<alloc_dir>` env so the wrapper script's
//!    PR 3f branch knows to invoke `cloud-hypervisor --restore`.
//! 7. Wait for `/livez` to 200.
//! 8. CAS `restoring → running`; clear snapshot metadata.
//! 9. On any mid-flight failure: CAS `restoring → snapshotted`
//!    (rollback; the artifact is preserved).
//!
//! ## Why a separate module
//!
//! Same rationale as `snapshot_handler.rs`: keep the snapshot/restore
//! orchestration out of `nomad_ch.rs` (already 3.7k lines) and pin
//! the ch-spawn + livez integration behind a trait so the v1 unit
//! tests can drive the flow with a mock backend.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use uuid::Uuid;

use crate::config::NomadCHConfig;
use crate::db::{Database, DatabaseError, SandboxStatus};
use crate::snapshot_store::SnapshotStore;

/// Outcome shape returned by [`restore_sandbox`].
#[derive(Debug)]
pub struct RestoreOutcome {
    pub sandbox_id: Uuid,
    pub vm_index: i16,
    /// Final pg generation post-CAS to `running`.
    pub generation: i64,
}

/// Why the restore couldn't proceed. Maps to § 10.0 wire envelope:
///
/// - `StateMismatch`        → 409 `state_mismatch`
/// - `FeatureDisabled`      → 501 `feature_disabled`
/// - `VmIndexUnavailable`   → 503 `vm_index_unavailable` (Retry-After)
/// - `SnapshotCorrupt`      → 500 `snapshot_corrupt` (CAS to suspect)
/// - `Backend` / `Database` → 500 (controller-internal)
/// - `NotFound`             → 404
#[derive(Debug, thiserror::Error)]
pub enum RestoreHandlerError {
    #[error("snapshot feature disabled (SANDBOX_SNAPSHOT_ENABLED=false)")]
    FeatureDisabled,

    #[error("state mismatch: expected `snapshotted` or `snapshotted_suspect`, current `{current}`")]
    StateMismatch { current: &'static str },

    #[error("sandbox not found: {0}")]
    NotFound(String),

    #[error("vm_index unavailable (cluster exhausted at vm_index={requested})")]
    VmIndexUnavailable { requested: i16 },

    #[error("snapshot corrupt (sha256 mismatch); row marked snapshotted_suspect")]
    SnapshotCorrupt,

    #[error("snapshot store: {0}")]
    Store(#[from] crate::snapshot_store::SnapshotError),

    #[error("backend: {0}")]
    Backend(String),

    #[error("config.json rewrite: {0}")]
    ConfigRewrite(String),

    #[error("database: {0}")]
    Database(#[from] DatabaseError),

    #[error("internal: {0}")]
    Internal(String),
}

/// Pieces of the production backend the restore handler touches.
/// Kept behind a trait so the v1 unit tests can inject a stub
/// without dragging the entire `NomadCHBackend` into scope. The
/// real implementation in `nomad_ch.rs` (or its sibling) wires
/// `submit_nomad_job` + `wait_for_agent_livez`.
pub trait RestoreBackend: Send + Sync {
    /// Reserve `vm_index` (the source slot) on this worker. Returns
    /// `Ok(())` on success; `Err(_)` if the slot is already held
    /// (cluster-fallback path; v1 doesn't try sibling workers and
    /// surfaces this as 503 immediately).
    fn reserve_vm_index(&self, vm_index: i16) -> Result<(), String>;

    /// Release a previously-reserved vm_index. Idempotent.
    fn release_vm_index(&self, vm_index: i16);

    /// Worker-local alloc-dir path for a sandbox restore. Stable
    /// across restore attempts (so `ZSBX_RESTORE_FROM` is
    /// deterministic). `<host_state_dir>/<sandbox-id>/restore/`.
    fn restore_alloc_dir(&self, sandbox_id: Uuid) -> PathBuf;

    /// Submit a Nomad (or equivalent) job for the restored alloc.
    /// `ZSBX_RESTORE_FROM=<alloc_dir>` must be set in the spawned
    /// task's env so the wrapper's restore branch fires (PR 3f).
    /// Synchronous return on success means the job was *enqueued*;
    /// readiness is signalled by [`Self::wait_for_livez`].
    fn submit_restore_job(
        &self,
        sandbox_id: Uuid,
        vm_index: i16,
        alloc_dir: &Path,
        user_id: &str,
    ) -> Result<(), String>;

    /// Block until the restored VM's agent serves `/livez 200`. Best
    /// implemented over `compio::time::sleep` polling; v1 ships the
    /// existing `wait_for_agent_livez` helper from `nomad_ch.rs`.
    fn wait_for_livez(
        &self,
        sandbox_id: Uuid,
        vm_index: i16,
    ) -> Result<(), String>;

    /// Best-effort teardown after restore failure: kill the alloc,
    /// release vm_index. Mirrors `SourceVmOps::teardown_source`.
    fn teardown_restore(&self, sandbox_id: Uuid, vm_index: i16);

    /// B19 fix (cluster smoke 2026-05-23 r4): install the restored
    /// sandbox into the backend's in-memory state map after
    /// `wait_for_livez` succeeds, so subsequent `exec` / `stop` /
    /// `delete` calls find it. Pre-B19 this step was missing, so
    /// post-wake `exec` returned 500 "sandbox not found in nomad-ch
    /// backend", `stop`/`delete` returned 404, and the vm_index slot
    /// leaked across the controller's uptime.
    ///
    /// `signing_key_bytes` is the 32-byte per-sandbox signing seed
    /// the handler has already unsealed from `Persistence` before
    /// calling here. The agent_url is re-derived inside the impl
    /// (it's a deterministic function of `vm_index` and the
    /// backend's `subnet_second_octet` config — same shape as
    /// `wait_for_livez`).
    ///
    /// **Default impl is a no-op `Ok(())`** so the in-crate
    /// `StubRestoreBackend` (test scaffolding) doesn't need to
    /// implement state-map registration just to keep the existing
    /// pg-gated tests compiling. The real impl on
    /// `RealRestoreBackend` performs the insert through a shared
    /// `Arc<NomadCHBackend>` handle.
    fn register_restored(
        &self,
        _sandbox_id: Uuid,
        _vm_index: i16,
        _signing_key_bytes: [u8; 32],
        _user_id: &str,
    ) -> Result<(), String> {
        Ok(())
    }

    /// Bug #22 fix: re-derive the in-VM agent URL for a given
    /// `vm_index`. Same formula `RealRestoreBackend::wait_for_livez`
    /// + `NomadCHBackend::derive_agent_url` use —
    /// `http://10.<subnet_second_octet>.<100+idx>.2:7777`. Exposed on
    /// the trait so `restore_sandbox` can issue the post-livez
    /// `/_clock_resync` handshake without reaching into backend
    /// internals.
    ///
    /// **No default impl.** R7-S2 fix: B22's initial sketch had a
    /// `"http://127.0.0.1:0"` sentinel default — a bogus URL that
    /// resolves but answers nothing, so any backend that forgot to
    /// override would surface as transport error ("agent down") rather
    /// than the real misconfiguration ("trait method not implemented").
    /// Silent fail-OPEN. Removing the default forces every impl
    /// (including test stubs) to provide a real URL at compile time.
    fn derive_agent_url(&self, vm_index: i16) -> String;
}

/// Restore a snapshotted sandbox. See module doc for the full flow.
///
/// `persist` is the sealed-record handle the controller built at
/// startup; the wake path calls `persist.unseal(sandbox_id)` after
/// `wait_for_livez` returns Ok to recover the per-sandbox signing
/// key for the state-map insert (B19 fix). `None` makes the
/// post-wake registration a no-op — appropriate for unit tests that
/// drive `restore_sandbox` with a `StubRestoreBackend` (whose
/// default `register_restored` impl is also a no-op).
pub async fn restore_sandbox(
    db: &Database,
    store: Arc<dyn SnapshotStore>,
    backend: Arc<dyn RestoreBackend>,
    persist: Option<&crate::persist::Persistence>,
    sandbox_id: Uuid,
    snapshot_enabled: bool,
) -> Result<RestoreOutcome, RestoreHandlerError> {
    if !snapshot_enabled {
        return Err(RestoreHandlerError::FeatureDisabled);
    }

    // 1. Read row + check state.
    let row = db
        .get_sandbox_row(sandbox_id)
        .await?
        .ok_or_else(|| {
            RestoreHandlerError::NotFound(format!(
                "sbx_{}",
                zeroship_core::typed_id::uuid_to_base62(&sandbox_id)
            ))
        })?;
    if !matches!(
        row.status,
        SandboxStatus::Snapshotted | SandboxStatus::SnapshottedSuspect
    ) {
        return Err(RestoreHandlerError::StateMismatch {
            current: row.status.as_str(),
        });
    }
    // SnapshottedSuspect is restorable too — operators may choose to
    // attempt a restore from a suspect artifact (e.g. after manual
    // verification). The artifact's SHA-256 is still verified by
    // `SnapshotStore::get`, so a corrupt suspect artifact cannot be
    // silently accepted. § 8.2 lifecycle table.
    let g0 = row.generation;

    // 2. Look up snapshot metadata (sha256 + vm_index) from pg. The
    //    `SandboxRow` doesn't carry the snapshot_* columns today —
    //    we read them via a focused query against the row at
    //    `snapshotted` state. (Folding them into `SandboxRow` would
    //    ripple through every existing reader; v1 keeps the read
    //    local to this handler.)
    let snap = read_snapshot_row(db, sandbox_id).await?;

    // 3. CAS to restoring.
    let g1 = db
        .update_sandbox_status(sandbox_id, SandboxStatus::Restoring, g0, None)
        .await?;

    // From here on use a closure + rollback semantics.
    let result = do_restore_inner(
        db,
        Arc::clone(&store),
        Arc::clone(&backend),
        persist,
        sandbox_id,
        &snap,
        g1,
    )
    .await;

    match result {
        Ok((vm_index, g2)) => Ok(RestoreOutcome { sandbox_id, vm_index, generation: g2 }),
        Err(e) => {
            // Rollback path. ChecksumMismatch / SnapshotCorrupt is
            // distinct: row goes to `snapshotted_suspect` instead
            // of bouncing back to `snapshotted` (§ 8.2).
            let target = match &e {
                RestoreHandlerError::SnapshotCorrupt => {
                    SandboxStatus::SnapshottedSuspect
                }
                _ => SandboxStatus::Snapshotted,
            };
            // Best-effort teardown of the partially-spawned alloc.
            backend.teardown_restore(sandbox_id, snap.vm_index);
            if let Err(rb_err) = db
                .update_sandbox_status(sandbox_id, target, g1, None)
                .await
            {
                tracing::error!(
                    sandbox_id = %sandbox_id,
                    rollback_target = target.as_str(),
                    rollback_error = %rb_err,
                    original_error = %e,
                    "restore rollback failed; row may be wedged in `restoring` until lease-takeover sweep"
                );
            }
            Err(e)
        }
    }
}

#[derive(Debug, Clone)]
struct SnapshotRowMeta {
    artifact_path: String,
    sha256: [u8; 32],
    vm_index: i16,
    /// Source sandbox's `user_id` (typed-id form `usr_<base62>`).
    /// Needed by `submit_restore_job` to derive `ZSBX_USER_HOME_IMG`
    /// per the cold-boot env contract (Phase B fix #6).
    user_id: String,
}

/// Focused pg read for the snapshot_* columns. Returns
/// `RestoreHandlerError::Internal` if any required column is NULL —
/// the 0007 CHECK should make that impossible for a `snapshotted` /
/// `snapshotted_suspect` row, but we surface a clear error rather
/// than panic.
async fn read_snapshot_row(
    db: &Database,
    sandbox_id: Uuid,
) -> Result<SnapshotRowMeta, RestoreHandlerError> {
    let pool = db.pool_app().await.map_err(|e| {
        RestoreHandlerError::Internal(format!("pool_app: {e}"))
    })?;
    let client = pool.get().await.map_err(|e| {
        RestoreHandlerError::Internal(format!("pool_app.get: {e}"))
    })?;
    let sandbox_id_typed = format!(
        "sbx_{}",
        zeroship_core::typed_id::uuid_to_base62(&sandbox_id)
    );
    let row = client
        .query_opt(
            "SELECT snapshot_artifact_path, snapshot_sha256, snapshot_vm_index, user_id \
               FROM sandbox.sandboxes \
              WHERE sandbox_id = $1::TEXT AND deleted_at IS NULL",
            &[&sandbox_id_typed],
        )
        .await
        .map_err(|e| RestoreHandlerError::Internal(format!("snap row read: {e}")))?
        .ok_or_else(|| RestoreHandlerError::NotFound(sandbox_id_typed.clone()))?;
    let artifact_path: Option<String> = row.try_get(0).ok();
    let sha_bytes: Option<Vec<u8>> = row.try_get(1).ok();
    let vm_index: Option<i16> = row.try_get(2).ok();
    let user_id: Option<String> = row.try_get(3).ok();
    let (Some(p), Some(s), Some(v), Some(u)) = (artifact_path, sha_bytes, vm_index, user_id) else {
        return Err(RestoreHandlerError::Internal(format!(
            "snapshot row missing required columns for {sandbox_id_typed} \
             (artifact / sha / vm_index / user_id)"
        )));
    };
    if s.len() != 32 {
        return Err(RestoreHandlerError::Internal(format!(
            "snapshot_sha256 length={} expected 32", s.len()
        )));
    }
    let mut sha = [0u8; 32];
    sha.copy_from_slice(&s);
    Ok(SnapshotRowMeta { artifact_path: p, sha256: sha, vm_index: v, user_id: u })
}

async fn do_restore_inner(
    db: &Database,
    store: Arc<dyn SnapshotStore>,
    backend: Arc<dyn RestoreBackend>,
    persist: Option<&crate::persist::Persistence>,
    sandbox_id: Uuid,
    snap: &SnapshotRowMeta,
    expected_generation: i64,
) -> Result<(i16, i64), RestoreHandlerError> {
    // 3 (cont). Reserve vm_index. v1: source slot only. § 5.1
    // "v1 forces vm_index = source vm_index"; cross-worker fallback
    // is documented but not implemented in v1.
    backend
        .reserve_vm_index(snap.vm_index)
        .map_err(|_| RestoreHandlerError::VmIndexUnavailable { requested: snap.vm_index })?;

    let alloc_dir = backend.restore_alloc_dir(sandbox_id);
    if alloc_dir.exists() {
        std::fs::remove_dir_all(&alloc_dir).map_err(|e| {
            RestoreHandlerError::Internal(format!(
                "remove stale alloc_dir {}: {e}", alloc_dir.display()
            ))
        })?;
    }
    std::fs::create_dir_all(&alloc_dir).map_err(|e| {
        RestoreHandlerError::Internal(format!(
            "create alloc_dir {}: {e}", alloc_dir.display()
        ))
    })?;

    // 4. Fetch artifact. ChecksumMismatch / InvalidArtifact → corrupt.
    let sandbox_id_typed = format!(
        "sbx_{}",
        zeroship_core::typed_id::uuid_to_base62(&sandbox_id)
    );
    let _ = &snap.artifact_path; // pg-recorded path; the store may
    // ignore it (the LocalDiskSnapshotStore re-derives from
    // `<root>/<sandbox-id>/`). Surfaced here for tracing/logs.
    // R5-P1b: store.get is sync (sha256 + AEAD decrypt + GCS read on
    // a ~1 GB blob) — run it on the blocking pool so the ntex worker
    // thread can serve other restores while this one's I/O+crypto
    // chews. Pattern mirrors `persist::Persistence::unseal` at
    // `crates/sandbox/src/persist.rs:677-687`. Owned clones of the
    // by-ref args are needed because spawn_blocking requires
    // `'static + Send + FnOnce`.
    let get_result = {
        let store_clone = Arc::clone(&store);
        let sid_clone = sandbox_id_typed.clone();
        let alloc_dir_clone = alloc_dir.clone();
        let sha_clone = snap.sha256;
        compio::runtime::spawn_blocking(move || {
            store_clone.get(&sid_clone, &alloc_dir_clone, &sha_clone)
        })
        .await
        .unwrap_or_else(|p| {
            Err(crate::snapshot_store::SnapshotError::Io(
                std::io::Error::other(format!("spawn_blocking panic: {p:?}")),
            ))
        })
    };
    // Bug-#14a diagnostic: surface what's on disk immediately after
    // store.get returns. Prior cluster smokes (2026-05-22) reported
    // the wake-time staging dir was empty despite a successful Ok
    // from store.get. Logging file presence + sizes here gives the
    // next cycle hard evidence whether the controller wrote files
    // that subsequently disappeared, or store.get is mis-claiming
    // success.
    {
        let mut sizes = Vec::with_capacity(3);
        for name in ["config.json", "memory-ranges", "state.json"] {
            let p = alloc_dir.join(name);
            sizes.push(match std::fs::metadata(&p) {
                Ok(m) => format!("{name}={} bytes", m.len()),
                Err(e) => format!("{name}=MISSING ({e})"),
            });
        }
        tracing::info!(
            sandbox_id = %sandbox_id,
            alloc_dir = %alloc_dir.display(),
            stat = %sizes.join(", "),
            "restore: post-store.get staged files"
        );
    }
    if let Err(e) = get_result {
        return Err(match e {
            crate::snapshot_store::SnapshotError::ChecksumMismatch { .. } => {
                tracing::error!(
                    sandbox_id = %sandbox_id,
                    "restore: checksum mismatch — marking snapshotted_suspect"
                );
                RestoreHandlerError::SnapshotCorrupt
            }
            crate::snapshot_store::SnapshotError::InvalidArtifact(s) => {
                // AEAD-auth failure (PR 3c) or magic mismatch is also
                // a "don't trust this artifact" signal. Treat the same
                // way as ChecksumMismatch — suspect + 500.
                tracing::error!(
                    sandbox_id = %sandbox_id,
                    error = %s,
                    "restore: artifact invalid (AEAD auth / magic mismatch)"
                );
                RestoreHandlerError::SnapshotCorrupt
            }
            other => RestoreHandlerError::Store(other),
        });
    }

    // 5. Rewrite config.json per § 5 — controller-side rewrites
    //    ONLY the vm_index-dependent fields (net[].tap, net[].mac).
    //    The path-bearing fields (disks[].path, fs[].socket,
    //    serial.file) are NOT touched here; the wrapper rewrites
    //    them at exec time because only the wrapper knows the
    //    actual NOMAD_TASK_DIR (Nomad assigns the alloc UUID after
    //    job submission). See bug-#8 diagnostic 2026-05-22.
    let config_path = alloc_dir.join("config.json");
    rewrite_config_json(&config_path, snap.vm_index)
        .map_err(RestoreHandlerError::ConfigRewrite)?;

    // 6. Submit the restore job.
    //
    // R8-A3-5 (perf-r8): `submit_restore_job` on `RealRestoreBackend`
    // is internally sync — it POSTs to Nomad over a blocking ureq
    // client, then `std::thread::sleep(250ms)`-polls allocations
    // until the alloc reaches `running` (typically 2-4 s, deadline
    // up to `restore_job_timeout`). Running it on the async caller
    // parked the ntex worker for the entire duration. Per perf-r8
    // this was the single largest remaining wake-path target (wake
    // p50 ~9.2 s, ~5-8 s of which was this call). Hop through
    // `compio::runtime::spawn_blocking` so the worker can serve
    // other RPCs while Nomad churns. Pattern mirrors R7-P1
    // (79428d53) on snapshot's ch.pause + ch.snapshot and R5-P1b
    // (cdd2e677) on store.get.
    {
        let backend_clone = Arc::clone(&backend);
        let alloc_dir_owned = alloc_dir.clone();
        let user_id_owned = snap.user_id.clone();
        let vm_index = snap.vm_index;
        compio::runtime::spawn_blocking(move || {
            backend_clone.submit_restore_job(
                sandbox_id,
                vm_index,
                &alloc_dir_owned,
                &user_id_owned,
            )
        })
        .await
        .unwrap_or_else(|p| Err(format!("spawn_blocking panic: {p:?}")))
        .map_err(RestoreHandlerError::Backend)?;
    }

    // 7. Wait for /livez.
    //
    // R8-A3-5 (perf-r8): `wait_for_livez` is also sync — it polls
    // the in-VM agent's `/livez` with `std::thread::sleep(250ms)`
    // between attempts until the agent answers 200 or the deadline
    // hits (typically 1-3 s post-`running`). Same blocking-poll
    // shape as `submit_restore_job`; same spawn_blocking treatment.
    {
        let backend_clone = Arc::clone(&backend);
        let vm_index = snap.vm_index;
        compio::runtime::spawn_blocking(move || {
            backend_clone.wait_for_livez(sandbox_id, vm_index)
        })
        .await
        .unwrap_or_else(|p| Err(format!("spawn_blocking panic: {p:?}")))
        .map_err(RestoreHandlerError::Backend)?;
    }

    // 7b (B19 fix, cluster smoke 2026-05-23 r4). Install the restored
    //    VM into the backend's in-memory state map. Without this step
    //    every post-wake `exec`/`stop`/`delete` returned "sandbox not
    //    found" and the vm_index slot leaked across the controller's
    //    uptime (stop_inner's idempotent-Ok branch fired without
    //    releasing the allocator). Unseal the per-sandbox signing key
    //    from the persistence handle the controller built at startup,
    //    then hand the bytes to the backend's `register_restored`
    //    through the trait. The trait default impl is a no-op so
    //    `StubRestoreBackend`-driven unit tests don't need to
    //    implement state-map registration. A `None` persist or a
    //    `NotFound` sealed record both surface as an Internal error
    //    here — a live restored VM whose signing key is unreachable
    //    cannot be safely registered (subsequent signed-RPC traffic
    //    would fail on every call); fail loudly so the operator sees
    //    the rollback rather than a silent wedge.
    //
    // **Bug #22 fix (cluster smoke 2026-05-23 r6+B22-fixer).** After
    //    unseal, BEFORE register_restored, issue a one-shot signed
    //    `/_clock_resync` to the restored agent. CH `--restore`
    //    brings the VM back with `CLOCK_REALTIME` frozen at the
    //    snapshot-time value, so without this handshake every
    //    subsequent signed RPC fails the agent's strict 5-second
    //    skew check (surface: 401 unauthorized on every post-wake
    //    `/exec`). The resync uses the same Ed25519 signing key the
    //    agent already trusts — same `signing_key_bytes` we are
    //    about to install into the state map — and the agent's
    //    `verify_kind_skew_bypass` path accepts the call without
    //    applying the skew window. After it returns Ok the agent's
    //    wall clock matches the controller's; the state-map insert
    //    + subsequent `/exec` traffic runs under the normal strict
    //    path. We sequence resync BEFORE register_restored so an
    //    `/exec` racing with this code path either (a) precedes the
    //    state-map insert and gets the existing "sandbox not found"
    //    surface, or (b) follows both and runs on a healthy clock.
    //    There is no window where the state map says "ready" but
    //    the clock is still broken.
    if let Some(p) = persist {
        let sealed = p.unseal(sandbox_id).await.map_err(|e| {
            RestoreHandlerError::Internal(format!(
                "post-wake unseal sandbox {sandbox_id}: {e}"
            ))
        })?;
        let agent_url = backend.derive_agent_url(snap.vm_index);
        clock_resync_post_restore(
            &agent_url,
            sandbox_id,
            &sealed.signing_key_bytes,
        )
        .await
        .map_err(|e| {
            RestoreHandlerError::Backend(format!(
                "post-wake clock_resync to {agent_url}: {e}"
            ))
        })?;
        backend
            .register_restored(
                sandbox_id,
                snap.vm_index,
                sealed.signing_key_bytes,
                &snap.user_id,
            )
            .map_err(RestoreHandlerError::Backend)?;
    } else {
        // Test path — `restore_sandbox` was called with persist=None
        // (StubRestoreBackend driven). The trait default impl is a
        // no-op; log so prod misconfiguration doesn't slip past.
        tracing::warn!(
            sandbox_id = %sandbox_id,
            "restore: register_restored skipped — persist=None \
             (expected only in tests; production wiring at \
             AppState::from_config plumbs Some)"
        );
    }

    // 8. CAS restoring → running. We also clear the snapshot
    //    metadata: the artifact is no longer the canonical state
    //    once the VM is live again. (Per § 1: snapshot is
    //    destructive of the source; restore is destructive of the
    //    snapshot. Idle-eviction will produce a fresh snapshot.)
    let g2 = db
        .update_sandbox_status(sandbox_id, SandboxStatus::Running, expected_generation, None)
        .await?;
    if let Err(e) = db.clear_snapshot_metadata(sandbox_id, g2).await {
        // Non-fatal: the row is `running` and serves traffic; the
        // operator's stale snapshot_* columns become a tidy-up
        // chore for the next idle-snapshot cycle. Log loudly.
        tracing::warn!(
            sandbox_id = %sandbox_id,
            error = %e,
            "restore: clear_snapshot_metadata after restoring→running failed (non-fatal)"
        );
    }

    Ok((snap.vm_index, g2))
}

// ────────────────────────────────────────────────────────────────────
// config.json rewrite (§ 5.1)
// ────────────────────────────────────────────────────────────────────

/// MAC derivation rule from `crates/sandbox/scripts/nomad-vm-wrapper.sh`:
/// `printf '12:34:56:78:9b:%02x' "$VM_INDEX"`. v1 vm_index is i16
/// (max 256/worker per § 5.0); we mask to one byte for the format.
pub(crate) fn derive_mac(vm_index: i16) -> String {
    format!("12:34:56:78:9b:{:02x}", (vm_index as u16) & 0xff)
}

/// Tap derivation rule: `zsbx-nm-<vm_index>`. Source-of-truth is
/// the wrapper script; this Rust copy must match.
pub(crate) fn derive_tap(vm_index: i16) -> String {
    format!("zsbx-nm-{vm_index}")
}

/// Rewrite the snapshot's `config.json` in place per § 5.1. v1 only
/// touches `net[].tap` (worker-local) and the alloc-uuid embedded
/// in `fs[].socket` and `serial.file`. IP / cmdline / disks / MAC
/// are all no-op rewrites in v1 (documented in the proposal).
///
/// Any structural surprise (missing key, wrong type) returns an
/// `Err` — the caller maps to `RestoreHandlerError::ConfigRewrite`
/// which lands as `snapshot_corrupt` so the row is marked suspect.
pub(crate) fn rewrite_config_json(
    config_path: &Path,
    vm_index: i16,
) -> Result<(), String> {
    let raw = std::fs::read_to_string(config_path)
        .map_err(|e| format!("read {}: {e}", config_path.display()))?;
    let mut v: serde_json::Value = serde_json::from_str(&raw)
        .map_err(|e| format!("parse {}: {e}", config_path.display()))?;

    // 5.1 net[].tap + net[].mac: vm_index-dependent. Source and dest
    // may have different vm_indices (cluster-fallback in v2). We
    // recompute from the destination vm_index. This is the ONLY
    // path the controller rewrites — path-bearing fields (disks[].path,
    // fs[].socket, serial.file) require the runtime NOMAD_TASK_DIR
    // which the controller cannot know at job-submit time. The
    // wrapper handles path rewrites at exec time. See bug-#8
    // diagnostic 2026-05-22 / proposal § 5.
    let tap = derive_tap(vm_index);
    let mac = derive_mac(vm_index);
    if let Some(nets) = v.get_mut("net").and_then(|n| n.as_array_mut()) {
        for net in nets.iter_mut() {
            if let Some(obj) = net.as_object_mut() {
                obj.insert("tap".into(), serde_json::Value::String(tap.clone()));
                obj.insert("mac".into(), serde_json::Value::String(mac.clone()));
            }
        }
    }

    // 5.1 payload.cmdline IP rewrite — no-op in v1 (the in-VM kernel
    // doesn't re-DHCP), but include the substitution so a v2
    // refactor lands cleanly. Comment-only for now.
    let _ = (); // placeholder for v2 cmdline rewrite

    let new = serde_json::to_string_pretty(&v)
        .map_err(|e| format!("re-serialize: {e}"))?;
    std::fs::write(config_path, new)
        .map_err(|e| format!("write {}: {e}", config_path.display()))?;
    Ok(())
}

// `rewrite_alloc_path` removed 2026-05-22 (bug #8 fix). The controller
// can't fabricate a meaningful alloc UUID at job-submit time; the
// wrapper rewrites path-bearing fields at exec time using the real
// `NOMAD_TASK_DIR` env var. See `crates/sandbox/scripts/nomad-vm-wrapper.sh`
// restore branch and the diagnostic at the top of this commit.

// ────────────────────────────────────────────────────────────────────
// Test stub `RestoreBackend`.
// ────────────────────────────────────────────────────────────────────

/// Minimal stub for unit tests. Returns programmable success/error
/// from each operation; records calls so tests can assert on order.
#[doc(hidden)]
#[derive(Debug)]
pub struct StubRestoreBackend {
    pub root: PathBuf,
    pub reserved: std::sync::Mutex<Vec<i16>>,
    pub released: std::sync::Mutex<Vec<i16>>,
    pub submit_called: std::sync::atomic::AtomicBool,
    pub livez_called: std::sync::atomic::AtomicBool,
    pub teardown_called: std::sync::atomic::AtomicBool,
    pub fail_reserve: bool,
    pub fail_submit: bool,
    pub fail_livez: bool,
}

impl StubRestoreBackend {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            reserved: std::sync::Mutex::new(Vec::new()),
            released: std::sync::Mutex::new(Vec::new()),
            submit_called: std::sync::atomic::AtomicBool::new(false),
            livez_called: std::sync::atomic::AtomicBool::new(false),
            teardown_called: std::sync::atomic::AtomicBool::new(false),
            fail_reserve: false,
            fail_submit: false,
            fail_livez: false,
        }
    }
}

impl RestoreBackend for StubRestoreBackend {
    fn reserve_vm_index(&self, vm_index: i16) -> Result<(), String> {
        if self.fail_reserve {
            return Err(format!("stub: reserve_vm_index({vm_index}) cluster-exhausted"));
        }
        self.reserved
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(vm_index);
        Ok(())
    }
    fn release_vm_index(&self, vm_index: i16) {
        self.released
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(vm_index);
    }
    fn restore_alloc_dir(&self, sandbox_id: Uuid) -> PathBuf {
        self.root.join(sandbox_id.simple().to_string())
    }
    fn submit_restore_job(
        &self,
        _sandbox_id: Uuid,
        _vm_index: i16,
        _alloc_dir: &Path,
        _user_id: &str,
    ) -> Result<(), String> {
        self.submit_called
            .store(true, std::sync::atomic::Ordering::SeqCst);
        if self.fail_submit {
            return Err("stub: submit_restore_job failed".into());
        }
        Ok(())
    }
    fn wait_for_livez(&self, _sandbox_id: Uuid, _vm_index: i16) -> Result<(), String> {
        self.livez_called
            .store(true, std::sync::atomic::Ordering::SeqCst);
        if self.fail_livez {
            return Err("stub: wait_for_livez failed".into());
        }
        Ok(())
    }
    fn teardown_restore(&self, _sandbox_id: Uuid, vm_index: i16) {
        self.teardown_called
            .store(true, std::sync::atomic::Ordering::SeqCst);
        self.release_vm_index(vm_index);
    }
    /// R7-S2: deterministic test-friendly URL on loopback. Encodes
    /// `vm_index` into the port so test assertions can pin the shape
    /// without resolving the host. NOT the old `127.0.0.1:0` sentinel
    /// — that was a fail-OPEN trap (resolves but nothing answers,
    /// indistinguishable from a real backend whose agent is down).
    /// `StubRestoreBackend` is used only on the `persist=None` test
    /// path where `clock_resync_post_restore` is skipped, so this URL
    /// is never dialled in current tests — but providing a real one
    /// keeps the contract honest and gives future stub-driven tests
    /// something they can actually bind to.
    fn derive_agent_url(&self, vm_index: i16) -> String {
        // Port in the IANA ephemeral-style range; offset by vm_index so
        // each stub URL is unique. Bound to 127.0.0.1 so any accidental
        // dial fails loudly with "connection refused" rather than
        // hitting an unrelated host.
        format!("http://127.0.0.1:{}", 17777u16 + (vm_index as u16))
    }
}

#[cfg(test)]
mod unit_tests {
    use super::*;

    #[test]
    fn derive_mac_matches_wrapper_pattern() {
        assert_eq!(derive_mac(3), "12:34:56:78:9b:03");
        assert_eq!(derive_mac(16), "12:34:56:78:9b:10");
        assert_eq!(derive_mac(255), "12:34:56:78:9b:ff");
    }

    #[test]
    fn derive_tap_matches_wrapper_pattern() {
        assert_eq!(derive_tap(0), "zsbx-nm-0");
        assert_eq!(derive_tap(42), "zsbx-nm-42");
    }

    /// Bug #8 fix (2026-05-22 cluster diagnostic): the controller
    /// rewrites ONLY net[].tap + net[].mac. Path-bearing fields
    /// (disks[].path, fs[].socket, serial.file) are left untouched
    /// because the controller can't know the runtime NOMAD_TASK_DIR
    /// at job-submit time. The wrapper does that rewrite at exec
    /// time with a `sed` against the real env-resolved task dir.
    #[test]
    fn rewrite_config_json_rewrites_only_net_fields() {
        let dir = std::env::temp_dir().join(format!(
            "zsbx-restore-test-{}-{}",
            std::process::id(),
            uuid::Uuid::now_v7().simple()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let cfg = dir.join("config.json");
        // Source-shaped config with paths that must NOT be touched.
        let source_disk = "/opt/nomad/data/alloc/oldalloc/ch/local/rootfs.img";
        let source_sock = "/opt/nomad/data/alloc/oldalloc/ch/local/vfs-keys.sock";
        let source_serial = "/opt/nomad/data/alloc/oldalloc/ch/local/serial.log";
        std::fs::write(
            &cfg,
            format!(
                r#"{{
                    "net":[{{"tap":"zsbx-nm-9","mac":"12:34:56:78:9b:09"}}],
                    "fs":[{{"tag":"keys","socket":"{source_sock}"}}],
                    "disks":[{{"path":"{source_disk}"}}],
                    "serial":{{"mode":"File","file":"{source_serial}"}}
                }}"#
            ),
        )
        .unwrap();
        rewrite_config_json(&cfg, 7).unwrap();
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        // net.tap + net.mac rewritten from vm_index=7
        assert_eq!(v["net"][0]["tap"], "zsbx-nm-7");
        assert_eq!(v["net"][0]["mac"], "12:34:56:78:9b:07");
        // Paths LEFT UNTOUCHED — the wrapper handles them at exec.
        assert_eq!(v["fs"][0]["socket"], source_sock);
        assert_eq!(v["disks"][0]["path"], source_disk);
        assert_eq!(v["serial"]["file"], source_serial);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

// ────────────────────────────────────────────────────────────────────
// `RealRestoreBackend` — production impl mirroring nomad_ch's
// create-side path (submit job, poll alloc-running, poll livez)
// with two key differences:
//
//   1. ZSBX_RESTORE_FROM=<alloc_dir> set in the spawned task's env
//      so the wrapper's PR 3f branch invokes
//      `cloud-hypervisor --restore source_url=file://<alloc_dir>`.
//   2. The vm_index is forced to the source slot (from the snapshot
//      row); cluster-fallback is documented but not implemented in
//      v1. A reserve() collision surfaces as 503
//      `vm_index_unavailable`.
//
// The trait's methods are sync. Each method uses the blocking ureq
// client directly; the handler invokes them from an async task and
// will block its compio worker for the duration. This is acceptable
// because (a) restore is a one-shot, infrequent op (not on the hot
// path) and (b) the wall time is dominated by CH boot + agent
// readiness — same shape as the existing nomad_ch create flow that
// already runs blocking-ish in spawn_blocking.
// ────────────────────────────────────────────────────────────────────

use std::collections::BTreeSet;

/// Minimal vm_index tracker used by the restore backend. Independent
/// of `NomadCHBackend::vm_index_allocator` because the restore path
/// runs ahead of any controller-managed registry — the in-memory
/// state for the restored alloc lives only in the restore-flow
/// scratch (alloc dir + Nomad job).  Production wiring shares the
/// allocator with `NomadCHBackend` via `Arc<Mutex<_>>` so a v2
/// cross-backend create cannot collide with an in-flight restore.
#[derive(Debug, Default)]
pub struct VmIndexReservations {
    reserved: BTreeSet<i16>,
}

impl VmIndexReservations {
    pub fn new() -> Self {
        Self {
            reserved: BTreeSet::new(),
        }
    }

    /// Returns Err if `vm_index` is already reserved.
    pub fn reserve(&mut self, vm_index: i16) -> Result<(), String> {
        if !self.reserved.insert(vm_index) {
            return Err(format!("vm_index {vm_index} already reserved"));
        }
        Ok(())
    }

    pub fn release(&mut self, vm_index: i16) {
        self.reserved.remove(&vm_index);
    }
}

/// Production restore backend. Submits a Nomad job that mirrors the
/// shape of `NomadCHBackend::create`'s, with `ZSBX_RESTORE_FROM` set.
pub struct RealRestoreBackend {
    cfg: NomadCHConfig,
    /// Controller-wide memory_mb default — written into the restore
    /// alloc's `ZSBX_VM_MEMORY_MB` env (Phase B fix #6). The wrapper
    /// passes this through to CH's `--memory size=${N}M,shared=on`
    /// flag; for restore it must match the snapshot's memory size
    /// (CH refuses to restore against a size mismatch).
    memory_mb: u32,
    /// Controller-wide cpu count — for `ZSBX_VM_CPUS_BOOT`. Same
    /// match-the-snapshot constraint applies.
    cpus: f32,
    /// Wall-time budget for the spawned alloc to reach
    /// `ClientStatus="running"` (mirrors `cfg.alloc_running_timeout_secs`).
    alloc_running_timeout: Duration,
    /// Wall-time budget for `/livez` to return 200 once the alloc is
    /// running. Mirrors `cfg.agent_livez_timeout_secs`. v1 polls
    /// unsigned /livez only — the signed /version fingerprint check
    /// requires plumbing the per-sandbox signing key out of the
    /// sealed record, which is a follow-up PR.
    agent_livez_timeout: Duration,
    /// **B18 fix (cluster smoke 2026-05-24 r4)**: shared
    /// `vm_index` allocator with `NomadCHBackend`. When present,
    /// `reserve_vm_index` and `release_vm_index` go through this
    /// allocator instead of the local `VmIndexReservations` below —
    /// so create-side `alloc()` cannot hand the same slot to a fresh
    /// sandbox while the restored VM is still alive on that
    /// tap/IP. Wired by `crate::AppState::from_config` from
    /// `Backend::vm_index_allocator()`. `None` only for unit tests
    /// that don't construct a `NomadCHBackend`.
    shared_allocator: Option<Arc<Mutex<crate::backend::nomad_ch::VmIndexAllocator>>>,
    /// Process-local reservation map for the source vm_index. Used
    /// only when `shared_allocator` is `None` (unit tests). In
    /// production this is bypassed entirely — `RealRestoreBackend::
    /// with_shared_allocator` wires through to the create-side pool.
    reservations: Arc<Mutex<VmIndexReservations>>,
    /// **B19 fix (cluster smoke 2026-05-23 r4 post-fix)**: shared
    /// `Arc<NomadCHBackend>` handle. When present, the trait's
    /// `register_restored` call after `wait_for_livez` Ok delegates
    /// to `NomadCHBackend::register_restored(...)`, which inserts
    /// the per-sandbox record into the backend's state map. Without
    /// the share, the restored VM stays invisible to the registry
    /// and every post-wake `exec`/`stop`/`delete` hits the "sandbox
    /// not found" branch + leaks the vm_index slot across controller
    /// uptime. Wired by `crate::AppState::from_config` from
    /// `Backend::nomad_ch_handle()`. `None` only for unit tests
    /// that don't construct a `NomadCHBackend` (the default trait
    /// impl returns `Ok(())` for those).
    nomad_handle: Option<Arc<crate::backend::nomad_ch::NomadCHBackend>>,
}

impl std::fmt::Debug for RealRestoreBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RealRestoreBackend")
            .field("nomad_addr", &self.cfg.nomad_addr)
            .field("datacenter", &self.cfg.datacenter)
            .field("alloc_running_timeout", &self.alloc_running_timeout)
            .field("agent_livez_timeout", &self.agent_livez_timeout)
            .finish_non_exhaustive()
    }
}

impl RealRestoreBackend {
    pub fn new(cfg: NomadCHConfig, memory_mb: u32, cpus: f32) -> Self {
        let alloc_running_timeout =
            Duration::from_secs(cfg.alloc_running_timeout_secs);
        let agent_livez_timeout =
            Duration::from_secs(cfg.agent_livez_timeout_secs);
        Self {
            cfg,
            memory_mb,
            cpus,
            alloc_running_timeout,
            agent_livez_timeout,
            shared_allocator: None,
            reservations: Arc::new(Mutex::new(VmIndexReservations::new())),
            nomad_handle: None,
        }
    }

    /// **B18 fix**: install a shared `vm_index` allocator. The
    /// production wiring (`AppState::from_config`) extracts the
    /// allocator from `Backend::vm_index_allocator()` and passes it
    /// here so the restore path's `reserve(source_slot)` and the
    /// create path's `alloc()` share state. Without this share, a
    /// CREATE after WAKE can hand the same tap/IP to a fresh sandbox,
    /// surfacing as a stale-pubkey 401 on /version (cluster smoke
    /// 2026-05-24 r4; 11/16 c=4 cycles failed once slots 1-6 had
    /// been used once).
    pub fn with_shared_allocator(
        mut self,
        allocator: Arc<Mutex<crate::backend::nomad_ch::VmIndexAllocator>>,
    ) -> Self {
        self.shared_allocator = Some(allocator);
        self
    }

    /// **B19 fix**: install a shared `Arc<NomadCHBackend>` handle so
    /// `register_restored(...)` can route the post-wake state-map
    /// insert back into the backend. Without this, the restored VM
    /// stays invisible to the registry — every post-wake
    /// `exec`/`stop`/`delete` returns "sandbox not found" and the
    /// vm_index slot leaks (cluster smoke 2026-05-23: ~10 wakes
    /// per controller boot exhausted the allocator). Wired by
    /// `crate::AppState::from_config` from
    /// `Backend::nomad_ch_handle()`.
    pub fn with_nomad_handle(
        mut self,
        handle: Arc<crate::backend::nomad_ch::NomadCHBackend>,
    ) -> Self {
        self.nomad_handle = Some(handle);
        self
    }
}

impl RestoreBackend for RealRestoreBackend {
    fn reserve_vm_index(&self, vm_index: i16) -> Result<(), String> {
        if let Some(shared) = self.shared_allocator.as_ref() {
            // B18 fix: route through the create-side allocator so a
            // concurrent CREATE on the same worker cannot hand the
            // same tap/IP to a fresh sandbox while a restored VM is
            // still alive on this slot. i16 → u16 cast: vm_index is
            // validated [1..=255] on the producer side (proposal §
            // 5.0; allocator's reserve() rejects out-of-range with
            // a clear error).
            let i = u16::try_from(vm_index).map_err(|_| {
                format!("reserve_vm_index: vm_index {vm_index} out of u16 range")
            })?;
            return shared
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .reserve(i);
        }
        self.reservations
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .reserve(vm_index)
    }

    fn release_vm_index(&self, vm_index: i16) {
        if let Some(shared) = self.shared_allocator.as_ref() {
            // B18 fix: release back into the shared pool so the
            // create-side `alloc()` can immediately reuse the slot
            // once a restored VM has been torn down (or the wake
            // path rolled back via `teardown_restore`).
            if let Ok(i) = u16::try_from(vm_index) {
                shared
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .release(i);
            }
            return;
        }
        self.reservations
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .release(vm_index);
    }

    fn restore_alloc_dir(&self, sandbox_id: Uuid) -> PathBuf {
        // Stable across attempts so ZSBX_RESTORE_FROM is deterministic.
        // <host_state_dir>/<sandbox-id>/restore/
        self.cfg
            .host_state_dir
            .join(sandbox_id.simple().to_string())
            .join("restore")
    }

    fn submit_restore_job(
        &self,
        sandbox_id: Uuid,
        vm_index: i16,
        alloc_dir: &Path,
        user_id: &str,
    ) -> Result<(), String> {
        let job_id = format!("zsbx-restore-{}", sandbox_id.simple());
        let job_json = build_restore_nomad_job_json(
            &job_id,
            &self.cfg,
            vm_index as u16,
            alloc_dir,
            sandbox_id,
            user_id,
            self.memory_mb,
            self.cpus,
        );
        let body = serde_json::to_vec(&job_json)
            .map_err(|e| format!("serialize Nomad job JSON: {e}"))?;
        let url = format!("{}/v1/jobs", self.cfg.nomad_addr);
        let resp = nomad_post_blocking(&url, &body, Duration::from_secs(15))?;
        if resp.status != 200 {
            return Err(format!(
                "POST {url} → status {}: {}",
                resp.status,
                resp.body.trim()
            ));
        }
        // Now poll until the alloc reaches running (or terminal).
        wait_for_alloc_running_blocking(
            &self.cfg.nomad_addr,
            &job_id,
            self.alloc_running_timeout,
        )
    }

    fn wait_for_livez(
        &self,
        _sandbox_id: Uuid,
        vm_index: i16,
    ) -> Result<(), String> {
        let agent_url = self.derive_agent_url(vm_index);
        wait_for_livez_blocking(&agent_url, self.agent_livez_timeout)
    }

    fn derive_agent_url(&self, vm_index: i16) -> String {
        // Same formula `NomadCHBackend::derive_agent_url` uses
        // (lines ~1521 of nomad_ch.rs); centralised here so the
        // RealRestoreBackend doesn't need to thread an Arc<NomadCH>
        // just to compute the URL.
        format!(
            "http://10.{}.{}.2:7777",
            self.cfg.subnet_second_octet,
            100u16 + (vm_index as u16)
        )
    }

    fn teardown_restore(&self, sandbox_id: Uuid, vm_index: i16) {
        let job_id = format!("zsbx-restore-{}", sandbox_id.simple());
        // Best-effort DELETE; ignore errors. The orphan-prune sweep
        // will mop up if Nomad is unreachable right now.
        let url = format!("{}/v1/job/{}?purge=true", self.cfg.nomad_addr, job_id);
        if let Err(e) = nomad_delete_blocking(&url, Duration::from_secs(10)) {
            tracing::warn!(
                sandbox_id = %sandbox_id,
                vm_index,
                error = %e,
                "restore teardown: nomad DELETE failed (non-fatal)"
            );
        }
        // Always release the vm_index regardless of teardown outcome.
        self.release_vm_index(vm_index);
    }

    /// **B19 fix**: install the restored sandbox into
    /// `NomadCHBackend::state` via the shared handle. The handler
    /// has already unsealed the signing key from `Persistence` and
    /// passes it in. The agent_url is re-derived here from
    /// `vm_index` + the backend's `subnet_second_octet` — same
    /// shape as `wait_for_livez`. Returns Err if the handle is
    /// `None` (production wiring should always set it; if it's
    /// missing we surface the misconfiguration loudly rather than
    /// silently no-op'ing — a silent no-op would re-introduce the
    /// pre-B19 "sandbox not found" + slot-leak symptoms).
    fn register_restored(
        &self,
        sandbox_id: Uuid,
        vm_index: i16,
        signing_key_bytes: [u8; 32],
        user_id: &str,
    ) -> Result<(), String> {
        let handle = self.nomad_handle.as_ref().ok_or_else(|| {
            "register_restored: RealRestoreBackend has no shared NomadCHBackend \
             handle (B19 wiring missing — check AppState::from_config)".to_string()
        })?;
        let vm_index_u16 = u16::try_from(vm_index).map_err(|_| {
            format!("register_restored: vm_index {vm_index} out of u16 range")
        })?;
        // Same agent_url formula as `wait_for_livez` above. Keeps the
        // shape that `wait_for_agent_livez` and `derive_agent_url`
        // use — `http://10.<subnet_second_octet>.<100+idx>.2:7777`.
        let agent_url = format!(
            "http://10.{}.{}.2:7777",
            self.cfg.subnet_second_octet,
            100u16 + vm_index_u16
        );
        handle.register_restored(
            sandbox_id,
            vm_index_u16,
            signing_key_bytes,
            agent_url,
            user_id.to_string(),
        )
    }
}

/// Build the Nomad job JSON for a restore alloc. Same shape as
/// `build_nomad_job_json` in nomad_ch but with `ZSBX_RESTORE_FROM`
/// set. We don't share the helper because the restore path doesn't
/// have a `user_id`/`project_id` to plumb through Meta — those are
/// already recorded on the source sandbox row in pg, the wrapper
/// doesn't need them.
#[allow(clippy::too_many_arguments)]
fn build_restore_nomad_job_json(
    job_id: &str,
    cfg: &NomadCHConfig,
    vm_index: u16,
    alloc_dir: &Path,
    sandbox_id: Uuid,
    user_id: &str,
    memory_mb: u32,
    cpus: f32,
) -> serde_json::Value {
    // Phase B fix #6 (later: virtio-blk pivot, bug #11): the wrapper
    // up-front validates a 5-env block (VM_INDEX + ARTIFACT_DIR +
    // RUNTIME + MEMORY_MB + CPUS_BOOT) on both branches, and on
    // cold-boot it additionally validates WORKSPACE_IMG +
    // USER_HOME_IMG + PUBKEY_HEX. Restore doesn't *need* the latter
    // three (CH `--restore` reads the disk paths + cmdline from the
    // snapshot's saved config.json), but we still emit the image
    // paths because the controller derives them deterministically
    // and the wrapper's defensive `[ -f $PATH ]` check on the
    // images catches a hand-edited restore jobspec with a typo'd
    // path before CH's "block device file" error.
    //
    // PUBKEY_HEX is left empty on restore: it's hex-only validated
    // only when set, and we have no separate persisted hex form on
    // hand here (the signing key lives in the persist layer as raw
    // 32 bytes; the restore path doesn't need to recompute the hex
    // because CH never reads the cmdline on restore).
    let host_dir = cfg
        .host_state_dir
        .join(sandbox_id.simple().to_string());
    let workspace_img = host_dir.join("workspace.img");
    let user_home_img = cfg
        .user_home_dir_root
        .join(user_id)
        .join("home.img");
    // cpus_boot: ceil(cpus) with a min of 1; mirrors nomad_ch::cpus_boot.
    let cpus_boot = if !cpus.is_finite() {
        1u32
    } else {
        let n = cpus.ceil() as i64;
        if n < 1 { 1 } else { n as u32 }
    };

    serde_json::json!({
        "Job": {
            "ID": job_id,
            "Name": job_id,
            "Type": "service",
            "Datacenters": [cfg.datacenter],
            "Meta": {
                "zeroship.sandbox": sandbox_id.to_string(),
                "zeroship.user": user_id,
                "zeroship.vm_index": vm_index.to_string(),
                "zeroship.kind": "restore",
            },
            "TaskGroups": [{
                "Name": "vm",
                "Count": 1,
                "RestartPolicy": {
                    "Attempts": 0,
                    "Mode": "fail",
                    "Interval": 30_000_000_000u64,
                    "Delay":     5_000_000_000u64,
                },
                "ReschedulePolicy": {
                    "Attempts": 0,
                    "Unlimited": false,
                },
                "Tasks": [{
                    "Name": "ch",
                    "Driver": "raw_exec",
                    "Config": {
                        "command": cfg.wrapper_path.display().to_string(),
                    },
                    "Env": {
                        "ZSBX_VM_INDEX": vm_index.to_string(),
                        "ZSBX_ARTIFACT_DIR": cfg.runtime_dir.display().to_string(),
                        "ZSBX_RUNTIME": "${NOMAD_TASK_DIR}",
                        // virtio-blk pivot (bug #11): the three
                        // virtio-fs share dirs are gone from the
                        // cold-boot env contract; mirror that here
                        // by emitting the two image paths. The
                        // controller derives both paths the same
                        // way cold-boot does (host_state_dir +
                        // user_home_dir_root) so the wrapper's
                        // existence checks pass. PUBKEY_HEX is left
                        // unset on restore: CH ignores --cmdline on
                        // --restore, and the wrapper's hex
                        // validation now skips when
                        // ZSBX_RESTORE_FROM is set.
                        "ZSBX_WORKSPACE_IMG": workspace_img.display().to_string(),
                        "ZSBX_USER_HOME_IMG": user_home_img.display().to_string(),
                        // Must match the snapshot's saved config —
                        // CH refuses to restore against a memory
                        // size mismatch. Pulled from the controller's
                        // SandboxConfig at backend construction.
                        "ZSBX_VM_MEMORY_MB": memory_mb.to_string(),
                        "ZSBX_VM_CPUS_BOOT": cpus_boot.to_string(),
                        // The wrapper's PR 3f restore branch reads
                        // this and switches to `cloud-hypervisor
                        // --restore source_url=file://<dir>`.
                        "ZSBX_RESTORE_FROM": alloc_dir.display().to_string(),
                        "ZSBX_SUBNET_BASE_OCTET":
                            cfg.subnet_second_octet.to_string(),
                    },
                    "Resources": {
                        // Match nomad_ch.rs: CPU MHz advisory under
                        // raw_exec + CH; memory comes from the
                        // snapshot's saved config.
                        //
                        // MemoryMaxMB = 2 × MemoryMB (bug-#9 fix,
                        // 2026-05-22). CH v51.1 mmap-faults full
                        // guest RAM during restore which gets
                        // memcg-accounted; without 2× slack the
                        // cgroup OOM-kills CH at ~t=30s before
                        // /livez is reachable. Mirrors cold-boot's
                        // jobspec in nomad_ch.rs.
                        "CPU": 500,
                        "MemoryMB": memory_mb,
                        "MemoryMaxMB": memory_mb * 2,
                    },
                    "KillTimeout": 10_000_000_000u64,
                }],
            }],
        }
    })
}

/// Sync HTTP response shape — mirrors `AgentResponse` in nomad_ch.
struct BlockingResponse {
    status: u16,
    body: String,
}

fn nomad_post_blocking(
    url: &str,
    body: &[u8],
    timeout: Duration,
) -> Result<BlockingResponse, String> {
    let req = ureq::post(url)
        .timeout(timeout)
        .set("content-type", "application/json");
    send_ureq_blocking(req, body)
}

fn nomad_get_blocking(
    url: &str,
    timeout: Duration,
) -> Result<BlockingResponse, String> {
    let req = ureq::get(url).timeout(timeout);
    send_ureq_blocking(req, &[])
}

fn nomad_delete_blocking(
    url: &str,
    timeout: Duration,
) -> Result<BlockingResponse, String> {
    let req = ureq::delete(url).timeout(timeout);
    send_ureq_blocking(req, &[])
}

fn send_ureq_blocking(
    req: ureq::Request,
    body: &[u8],
) -> Result<BlockingResponse, String> {
    use std::io::Read;
    let send = if body.is_empty() {
        req.call()
    } else {
        req.send_bytes(body)
    };
    match send {
        Ok(resp) => {
            let status = resp.status();
            let mut bytes = Vec::new();
            let _ = resp.into_reader().take(8 * 1024 * 1024).read_to_end(&mut bytes);
            Ok(BlockingResponse {
                status,
                body: String::from_utf8_lossy(&bytes).into_owned(),
            })
        }
        Err(ureq::Error::Status(code, resp)) => {
            let mut bytes = Vec::new();
            let _ = resp.into_reader().take(8 * 1024).read_to_end(&mut bytes);
            Ok(BlockingResponse {
                status: code,
                body: String::from_utf8_lossy(&bytes).into_owned(),
            })
        }
        Err(e) => Err(format!("{e}")),
    }
}

/// Sync version of `wait_for_alloc_running` (nomad_ch.rs) — polls
/// the job's allocations every 250 ms until at least one reaches
/// `ClientStatus="running"`, or terminal (failed/lost) → Err, or the
/// deadline expires.
fn wait_for_alloc_running_blocking(
    nomad_addr: &str,
    job_id: &str,
    timeout: Duration,
) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    let url = format!("{nomad_addr}/v1/job/{job_id}/allocations");
    let mut last_status: Option<String> = None;
    let mut last_err: Option<String> = None;
    while Instant::now() < deadline {
        match nomad_get_blocking(&url, Duration::from_secs(5)) {
            Ok(r) if r.status == 200 => {
                let allocs: Result<serde_json::Value, _> =
                    serde_json::from_str(&r.body);
                match allocs {
                    Ok(allocs) => {
                        for a in allocs.as_array().into_iter().flatten() {
                            let cs = a["ClientStatus"]
                                .as_str()
                                .unwrap_or("")
                                .to_string();
                            if cs == "running" {
                                return Ok(());
                            }
                            if cs == "failed" || cs == "lost" {
                                let desc = a["ClientDescription"]
                                    .as_str()
                                    .unwrap_or("")
                                    .to_string();
                                return Err(format!(
                                    "nomad alloc terminal status={cs}: {desc}"
                                ));
                            }
                            last_status = Some(cs);
                        }
                    }
                    Err(e) => {
                        last_err = Some(format!("parse allocs: {e}"));
                    }
                }
            }
            Ok(r) => {
                last_err = Some(format!("status {} body={}", r.status, r.body.trim()));
            }
            Err(e) => {
                last_err = Some(e);
            }
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    let mut msg = format!(
        "restore: nomad alloc never reached running for {job_id} (last status={:?})",
        last_status.unwrap_or_else(|| "<no allocs>".to_string())
    );
    if let Some(e) = last_err {
        msg.push_str(&format!(" last_err={e}"));
    }
    Err(msg)
}

/// Sync version of `wait_for_agent_livez` — polls just the unsigned
/// `/livez` until 200, or the deadline. v1 skips the signed /version
/// fingerprint check; that requires plumbing the per-sandbox signing
/// key out of the sealed record (follow-up PR). For v1 the absence
/// of the fp check is acceptable because the restore path forces the
/// source vm_index — there's no "stale tenant" because the prior
/// alloc already terminated as part of the snapshot's destructive
/// teardown.
fn wait_for_livez_blocking(
    base_url: &str,
    timeout: Duration,
) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    let url = format!("{base_url}/livez");
    let mut last: Option<String> = None;
    while Instant::now() < deadline {
        match nomad_get_blocking(&url, Duration::from_millis(500)) {
            Ok(r) if r.status == 200 => return Ok(()),
            Ok(r) => last = Some(format!("status {}", r.status)),
            Err(e) => last = Some(e),
        }
        std::thread::sleep(Duration::from_millis(150));
    }
    Err(format!(
        "restore: agent at {base_url} never returned 200 on /livez (last={})",
        last.unwrap_or_else(|| "<no responses>".into())
    ))
}

/// Bug #22 fix (cluster smoke 2026-05-23 r6+B22-fixer). After CH
/// `--restore` brings the VM back, the guest's `CLOCK_REALTIME` is
/// frozen at the snapshot-time value, so the agent's strict 5-second
/// skew check rejects every subsequent signed RPC with 401
/// unauthorized. This helper issues a single signed `POST
/// /_clock_resync` with the current controller timestamp; the agent
/// verifies the signature WITHOUT applying the skew gate
/// (`verify_kind_skew_bypass`) and `settimeofday(2)`s
/// `CLOCK_REALTIME` to the request's signed ts. After it returns Ok
/// the agent's wall clock matches the controller's and subsequent
/// `/exec`, `/files`, etc. work under the normal strict-skew path.
///
/// **Signing:** uses [`zeroship_sandbox_agent::sig::sign`] with the
/// per-sandbox Ed25519 signing key the caller has just unsealed from
/// [`crate::persist::Persistence`]. Same canonical/header format every
/// other controller→agent RPC uses; the only difference is the agent
/// dispatches `/_clock_resync` through the skew-bypass verifier.
///
/// **R7-S1 body shape (wire change).** The signed body is now
/// `{"sandbox_id": <uuid>, "ts": <unix_secs>, "challenge": <hex64>}`.
/// The `sandbox_id` field lets the agent reject a captured resync
/// from sandbox A replayed against sandbox B; the `challenge` field
/// (32 random bytes hex-encoded) lets the agent reject a captured
/// resync from cycle-N replayed against cycle-N+1 of the same sandbox.
/// Pre-R7-S1 controllers signed just `{"ts": ...}` — those messages
/// no longer pass the new agent's body validation. Rolling upgrade:
/// rootfs v5 + controller v18 land together; the wire change is
/// atomic in deployment.
///
/// **Security:** the controller's private key is the trust anchor —
/// an in-VM attacker can't forge this call. The agent's nonce LRU
/// prevents a captured resync from being replayed within the agent's
/// process lifetime; the new **challenge LRU** prevents the post-
/// restore replay-DoS (a fresh outer nonce on the same captured body
/// would otherwise slip past). The endpoint is the ONLY one that
/// bypasses the skew window; every other agent endpoint stays on
/// strict 5-second skew.
///
/// **Failure mode:** any non-200 response (including the agent's
/// genuine 401 if signing-key bytes are wrong) surfaces as `Err`. The
/// caller (`do_restore_inner`) maps this to a `Backend(...)` error so
/// the wake path rolls back to `Snapshotted` rather than wedging the
/// row at `Restoring` with a broken VM.
async fn clock_resync_post_restore(
    agent_url: &str,
    sandbox_id: Uuid,
    signing_key_bytes: &[u8; 32],
) -> Result<(), String> {
    use ed25519_dalek::SigningKey;
    use zeroship_sandbox_agent::sig;

    let signing_key = SigningKey::from_bytes(signing_key_bytes);
    let url = format!("{agent_url}/_clock_resync");
    let path = "/_clock_resync".to_string();
    let url_for_blocking = url.clone();
    // R7-S1: bind the sandbox UUID into the signed body so the agent's
    // handler can assert it matches its own boot-time-known id. We
    // capture by value (`to_string`) because spawn_blocking takes
    // `'static` closures.
    let sandbox_id_str = sandbox_id.to_string();
    compio::runtime::spawn_blocking(move || {
        // Use std::time directly here (mirror of `unix_now` in
        // nomad_ch.rs) — clock_resync targets `CLOCK_REALTIME` so
        // we want CLOCK_REALTIME bytes too, matching whatever the
        // controller's wall clock thinks "now" is.
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        // R7-S1: mint a fresh per-restore challenge (32 random bytes
        // hex-encoded → 64 chars to match the agent's
        // `RESYNC_CHALLENGE_HEX_LEN`). The agent's challenge LRU
        // rejects any value it has seen recently — without this bind,
        // a captured resync could be replayed POST-cycle to set a
        // stale ts and 401 every strict-skew RPC.
        let challenge = match clock_resync_random_hex(32) {
            Ok(c) => c,
            Err(e) => return Err(format!("challenge gen: {e}")),
        };
        // Bind sandbox_id + ts + challenge into the body so the
        // agent's body-hash check covers all three and an in-VM
        // attacker can't substitute a different value for any of them.
        // R7-S1 wire shape (pre-R7-S1 was just `{"ts": ts}`).
        let body = serde_json::json!({
            "sandbox_id": sandbox_id_str,
            "ts": ts,
            "challenge": challenge,
        })
        .to_string();
        let body_bytes = body.as_bytes();
        let nonce = match clock_resync_nonce() {
            Ok(n) => n,
            Err(e) => return Err(format!("nonce gen: {e}")),
        };
        let signature = sig::sign(&signing_key, "POST", &path, body_bytes, ts, &nonce);
        let resp = ureq::post(&url_for_blocking)
            .timeout(Duration::from_secs(10))
            .set("content-type", "application/json")
            .set("x-sbx-timestamp", &ts.to_string())
            .set("x-sbx-nonce", &nonce)
            .set("x-sbx-signature", &signature)
            .send_bytes(body_bytes);
        match resp {
            Ok(r) => {
                let status = r.status();
                if status == 200 {
                    return Ok(());
                }
                let body_excerpt = r.into_string().unwrap_or_default();
                Err(format!(
                    "/_clock_resync status {status}: {}",
                    body_excerpt.chars().take(256).collect::<String>()
                ))
            }
            Err(ureq::Error::Status(code, r)) => {
                let body_excerpt = r.into_string().unwrap_or_default();
                Err(format!(
                    "/_clock_resync status {code}: {}",
                    body_excerpt.chars().take(256).collect::<String>()
                ))
            }
            Err(e) => Err(format!("/_clock_resync transport: {e}")),
        }
    })
    .await
    .unwrap_or_else(|p| Err(format!("clock_resync spawn_blocking panic: {p:?}")))
}

/// Random hex nonce for `/_clock_resync`. Matches the agent's
/// `MAX_NONCE_LEN=64` ceiling with plenty of margin (32 hex chars
/// over 128 bits of entropy). Same `/dev/urandom`-backed shape as
/// `random_hex` in `crates/sandbox/src/restore.rs`; kept local so
/// the restore module doesn't reach into backend internals — bug
/// #22's surgical scope is fully contained to (a) restore_handler,
/// (b) the agent crate.
fn clock_resync_nonce() -> Result<String, String> {
    clock_resync_random_hex(16)
}

/// R7-S1 helper: fetch `n_bytes` from `/dev/urandom` and hex-encode.
/// Used for both the per-restore `challenge` field (n_bytes=32 → 64
/// hex chars, matching the agent's `RESYNC_CHALLENGE_HEX_LEN`) and
/// the resync nonce (n_bytes=16 → 32 hex chars). Same source as
/// `random_hex` in `crates/sandbox/src/restore.rs`; kept local so the
/// restore module doesn't reach into backend internals.
fn clock_resync_random_hex(n_bytes: usize) -> Result<String, String> {
    use std::io::Read as _;
    let mut buf = vec![0u8; n_bytes];
    std::fs::File::open("/dev/urandom")
        .map_err(|e| format!("open /dev/urandom: {e}"))?
        .read_exact(&mut buf)
        .map_err(|e| format!("read /dev/urandom: {e}"))?;
    Ok(buf.iter().map(|b| format!("{b:02x}")).collect())
}

#[cfg(test)]
mod real_backend_tests {
    use super::*;
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicU32, Ordering as AOrdering};
    use std::thread;

    fn fresh_dir() -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "zsbx-restore-real-{}-{}",
            std::process::id(),
            uuid::Uuid::now_v7().simple()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn base_cfg(nomad_addr: String, host_state: PathBuf) -> NomadCHConfig {
        NomadCHConfig {
            nomad_addr,
            datacenter: "dc1".into(),
            wrapper_path: PathBuf::from("/etc/zeroship/nomad-vm-wrapper.sh"),
            runtime_dir: PathBuf::from("/var/lib/zeroship/ch"),
            host_state_dir: host_state,
            user_home_dir_root: PathBuf::from("/var/zeroship/ch/users"),
            vm_index_floor: 1,
            vm_index_ceil: 155,
            alloc_running_timeout_secs: 1,
            agent_livez_timeout_secs: 1,
            host_fence_timeout_secs: 30,
            startup_orphan_cleanup: false,
            subnet_second_octet: 99,
        }
    }

    /// Tiny synchronous HTTP server thread for tests. Accepts one
    /// connection at a time; serves a fixed `(status, body)` pair.
    /// The handler is a closure that returns `(status, body, kind)`
    /// per request so we can vary responses across calls.
    fn spawn_fake_nomad<F>(handler: F) -> (String, Arc<AtomicU32>)
    where
        F: Fn(u32) -> (u16, String) + Send + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let counter = Arc::new(AtomicU32::new(0));
        let c2 = counter.clone();
        thread::spawn(move || {
            for stream in listener.incoming() {
                let mut s = match stream {
                    Ok(s) => s,
                    Err(_) => return,
                };
                let n = c2.fetch_add(1, AOrdering::SeqCst);
                let (status, body) = handler(n);
                use std::io::{Read, Write};
                // Drain request — at least one chunk; ureq sends
                // headers + maybe body. We don't actually parse.
                let mut buf = [0u8; 8192];
                let _ = s.set_read_timeout(Some(Duration::from_millis(200)));
                let _ = s.read(&mut buf);
                let response = format!(
                    "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = s.write_all(response.as_bytes());
                let _ = s.flush();
            }
        });
        (format!("http://{addr}"), counter)
    }

    /// Submit + alloc-running succeeds when Nomad returns 200 then a
    /// running alloc.
    #[test]
    fn submit_restore_job_succeeds_when_nomad_returns_running() {
        let host_state = fresh_dir();
        let (nomad_addr, calls) = spawn_fake_nomad(move |n| match n {
            0 => (200, "{}".to_string()),
            _ => (
                200,
                r#"[{"ClientStatus":"running","ClientDescription":"ok"}]"#.to_string(),
            ),
        });
        let cfg = base_cfg(nomad_addr, host_state.clone());
        let backend = RealRestoreBackend::new(cfg, 1024, 2.0);
        let sid = Uuid::now_v7();
        let alloc_dir = backend.restore_alloc_dir(sid);
        std::fs::create_dir_all(&alloc_dir).unwrap();

        backend
            .submit_restore_job(sid, 7, &alloc_dir, "usr_test")
            .expect("submit_restore_job must succeed");
        assert!(
            calls.load(AOrdering::SeqCst) >= 2,
            "expected at least submit + 1 poll; got {}",
            calls.load(AOrdering::SeqCst)
        );
        let _ = std::fs::remove_dir_all(&host_state);
    }

    /// 500 on POST surfaces as a recoverable error (the handler will
    /// CAS restoring → snapshotted).
    #[test]
    fn submit_restore_job_errors_when_nomad_500s() {
        let host_state = fresh_dir();
        let (nomad_addr, _) = spawn_fake_nomad(|_| {
            (500, r#"{"error":"nomad: backend down"}"#.to_string())
        });
        let cfg = base_cfg(nomad_addr, host_state.clone());
        let backend = RealRestoreBackend::new(cfg, 1024, 2.0);
        let sid = Uuid::now_v7();
        let alloc_dir = backend.restore_alloc_dir(sid);
        std::fs::create_dir_all(&alloc_dir).unwrap();

        let err = backend
            .submit_restore_job(sid, 8, &alloc_dir, "usr_test")
            .expect_err("500 must error");
        assert!(err.contains("status 500"), "{err}");
        let _ = std::fs::remove_dir_all(&host_state);
    }

    /// reserve_vm_index returns Err on collision (simulates v1's
    /// "no cluster fallback" surface).
    #[test]
    fn reserve_vm_index_collides() {
        let cfg = base_cfg("http://127.0.0.1:1".into(), fresh_dir());
        let backend = RealRestoreBackend::new(cfg, 1024, 2.0);
        backend.reserve_vm_index(42).expect("first must succeed");
        let err = backend.reserve_vm_index(42).expect_err("collision");
        assert!(err.contains("already reserved"), "{err}");
        backend.release_vm_index(42);
        backend.reserve_vm_index(42).expect("post-release must succeed");
    }

    /// **B18 regression** (cluster smoke 2026-05-24 r4): the wake
    /// path's `reserve_vm_index` must block a concurrent create-side
    /// `alloc()` from handing out the same slot. Pre-fix, two
    /// separate allocators (NomadCHBackend's `vm_index_allocator` and
    /// RealRestoreBackend's private `VmIndexReservations`) let a
    /// CREATE-after-WAKE silently reuse a slot held by a live
    /// restored VM, surfacing as a stale-pubkey 401 on /version.
    /// Post-fix, `with_shared_allocator` routes reserve/release
    /// through the create-side pool so this is impossible.
    #[test]
    fn b18_shared_allocator_blocks_create_side_reuse() {
        use crate::backend::nomad_ch::VmIndexAllocator;
        let cfg = base_cfg("http://127.0.0.1:1".into(), fresh_dir());
        // Create-side allocator. Models `NomadCHBackend::vm_index_allocator`.
        let shared = Arc::new(Mutex::new(VmIndexAllocator::new(1, 5)));
        let backend = RealRestoreBackend::new(cfg, 1024, 2.0)
            .with_shared_allocator(shared.clone());
        // Simulate the prior cycle: create-side allocs slot 1, then
        // releases it as part of `stop_preserving_state` (snapshot
        // teardown). Slot 1 is now in `freed`.
        let slot = shared.lock().unwrap().alloc().unwrap();
        assert_eq!(slot, 1);
        shared.lock().unwrap().release(slot);
        // Wake reserves the slot via RealRestoreBackend.
        backend.reserve_vm_index(slot as i16).expect("wake reserve");
        // Now a concurrent create-side `alloc()` MUST NOT hand back
        // slot 1 (the restored VM is still alive on that tap/IP).
        // Pre-B18-fix it did — the freed set still contained slot 1
        // because reserve() ran against a separate allocator.
        let next = shared.lock().unwrap().alloc().unwrap();
        assert_ne!(
            next, slot,
            "B18 regression: create-side allocator handed out slot {slot} \
             while restored VM still owned it via wake reserve()"
        );
        // Release on wake teardown frees the slot for the next create.
        backend.release_vm_index(slot as i16);
        let reclaimed = shared.lock().unwrap().alloc().unwrap();
        assert_eq!(
            reclaimed, slot,
            "post-release, the slot must be reusable by create-side alloc"
        );
    }

    /// **B18 regression**: with shared allocator, calling
    /// `reserve_vm_index` twice for the same slot must fail (the
    /// allocator's new in-flight collision detection refuses
    /// double-reserve of a live slot, even when the prior reserver is
    /// the create-side `alloc()` rather than another wake).
    #[test]
    fn b18_shared_allocator_rejects_double_reserve_of_live_slot() {
        use crate::backend::nomad_ch::VmIndexAllocator;
        let cfg = base_cfg("http://127.0.0.1:1".into(), fresh_dir());
        let shared = Arc::new(Mutex::new(VmIndexAllocator::new(1, 5)));
        let backend = RealRestoreBackend::new(cfg, 1024, 2.0)
            .with_shared_allocator(shared.clone());
        // Create-side hands out slot 1; the slot is now live in the
        // shared pool (next=2, freed={}).
        let live = shared.lock().unwrap().alloc().unwrap();
        assert_eq!(live, 1);
        // Wake tries to reserve the same live slot — must error.
        // Pre-B18-fix the allocator's `reserve()` would silently
        // succeed (no in-flight detection), and the wake handler
        // would proceed to submit a restore alloc that collides with
        // the live create-side sandbox.
        let err = backend
            .reserve_vm_index(live as i16)
            .expect_err("must reject reserve of in-flight slot");
        assert!(
            err.contains("already reserved"),
            "expected 'already reserved' error, got: {err}"
        );
    }

    /// Alloc-never-running times out and the error mentions the job
    /// id + last status. Uses a 1-second cfg budget (set in
    /// `base_cfg`).
    #[test]
    fn submit_restore_job_times_out_when_alloc_never_running() {
        let host_state = fresh_dir();
        let (nomad_addr, _) = spawn_fake_nomad(|n| match n {
            0 => (200, "{}".to_string()), // POST OK
            _ => (
                200,
                r#"[{"ClientStatus":"pending","ClientDescription":"queued"}]"#.to_string(),
            ),
        });
        let cfg = base_cfg(nomad_addr, host_state.clone());
        let backend = RealRestoreBackend::new(cfg, 1024, 2.0);
        let sid = Uuid::now_v7();
        let alloc_dir = backend.restore_alloc_dir(sid);
        std::fs::create_dir_all(&alloc_dir).unwrap();

        let err = backend
            .submit_restore_job(sid, 9, &alloc_dir, "usr_test")
            .expect_err("never-running must time out");
        assert!(
            err.contains("never reached running"),
            "{err}"
        );
        let _ = std::fs::remove_dir_all(&host_state);
    }

    // ─── Bug #22 fix: /_clock_resync handshake ─────────────────────
    //
    // The wake path's `clock_resync_post_restore` issues a single
    // signed POST /_clock_resync to repair the guest's frozen-at-
    // snapshot CLOCK_REALTIME (CH `--restore` preserves the wall
    // clock; the agent's strict 5-second skew would otherwise 401
    // every subsequent /exec). These tests pin:
    //   - happy path: 200 from the agent → `Ok(())`,
    //   - 401 from the agent → `Err(... status 401 ...)`,
    //   - transport failure (closed listener) → `Err(... transport ...)`.

    /// Tiny fake-agent that serves a configurable HTTP response shape.
    /// Mirrors `spawn_fake_nomad` above (we keep it separate so a
    /// future fake-nomad change can't accidentally break the
    /// clock-resync tests).
    fn spawn_fake_agent<F>(handler: F) -> (String, Arc<AtomicU32>)
    where
        F: Fn(u32, &[u8]) -> (u16, String) + Send + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let counter = Arc::new(AtomicU32::new(0));
        let c2 = counter.clone();
        thread::spawn(move || {
            for stream in listener.incoming() {
                let mut s = match stream {
                    Ok(s) => s,
                    Err(_) => return,
                };
                let n = c2.fetch_add(1, AOrdering::SeqCst);
                use std::io::{Read, Write};
                let mut buf = vec![0u8; 8192];
                let _ = s.set_read_timeout(Some(Duration::from_millis(200)));
                let n_read = s.read(&mut buf).unwrap_or(0);
                buf.truncate(n_read);
                let (status, body) = handler(n, &buf);
                let response = format!(
                    "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = s.write_all(response.as_bytes());
                let _ = s.flush();
            }
        });
        (format!("http://{addr}"), counter)
    }

    /// Happy path: agent returns 200 → resync succeeds.
    #[ntex::test]
    async fn clock_resync_post_restore_happy_path() {
        let (agent_url, calls) = spawn_fake_agent(|_, _req| {
            (200, r#"{"resynced":true,"ts":1700000000}"#.to_string())
        });
        let signing_key_bytes = [0xabu8; 32];
        let sandbox_id = Uuid::now_v7();
        let r = clock_resync_post_restore(&agent_url, sandbox_id, &signing_key_bytes).await;
        assert!(r.is_ok(), "happy path must Ok; got {r:?}");
        assert_eq!(
            calls.load(AOrdering::SeqCst),
            1,
            "expected exactly one POST to /_clock_resync"
        );
    }

    /// Agent 401 (e.g., wrong signing-key bytes) → Err carries the
    /// status code so the wake path can roll back with diagnostic text.
    #[ntex::test]
    async fn clock_resync_post_restore_surfaces_agent_401() {
        let (agent_url, _calls) = spawn_fake_agent(|_, _req| {
            (401, r#"{"error":"unauthorized"}"#.to_string())
        });
        let signing_key_bytes = [0xcdu8; 32];
        let sandbox_id = Uuid::now_v7();
        let err = clock_resync_post_restore(&agent_url, sandbox_id, &signing_key_bytes)
            .await
            .expect_err("401 must error");
        assert!(
            err.contains("/_clock_resync status 401"),
            "error text must surface the 401: {err}"
        );
    }

    /// Transport failure (port not listening) → Err. Uses a port we
    /// know nothing will be listening on.
    #[ntex::test]
    async fn clock_resync_post_restore_transport_error() {
        // Bind + immediately drop so the port is closed.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let agent_url = format!("http://{addr}");
        let sandbox_id = Uuid::now_v7();
        let r =
            clock_resync_post_restore(&agent_url, sandbox_id, &[0u8; 32]).await;
        let err = r.expect_err("closed port must error");
        // ureq surfaces this as a transport error; the wrapper text
        // distinguishes from the status-code variant so an operator
        // can grep the right shape out of the controller log.
        assert!(
            err.contains("transport"),
            "expected transport error wrapper text; got: {err}"
        );
    }

    /// **R7-S1**: pin the wire-format change. The body the controller
    /// sends to `/_clock_resync` MUST carry `sandbox_id` + `ts` +
    /// `challenge`. A future contributor who removes any of these
    /// re-opens the post-restore replay-DoS surface; this test reads
    /// the bytes the fake agent received and asserts the JSON keys
    /// + the 64-char challenge width.
    #[ntex::test]
    async fn clock_resync_post_restore_binds_sandbox_id_and_challenge() {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        // Hand-rolled fake agent that reads UNTIL the body content-
        // length is satisfied. The default `spawn_fake_agent` reads
        // one 8KB chunk and may miss the body on slow TCP packets;
        // this variant explicitly drains both halves of the request.
        let captured: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let captured_for_handler = captured.clone();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        thread::spawn(move || {
            let mut stream = match listener.accept() {
                Ok((s, _)) => s,
                Err(_) => return,
            };
            let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
            // Drain the request in a loop until we've seen CRLF CRLF
            // (header terminator) AND the body bytes implied by
            // Content-Length, OR until the read times out.
            let mut all = Vec::with_capacity(4096);
            let mut buf = [0u8; 4096];
            let mut header_end: Option<usize> = None;
            let mut content_length: Option<usize> = None;
            loop {
                match stream.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        all.extend_from_slice(&buf[..n]);
                        if header_end.is_none() {
                            if let Some(idx) =
                                all.windows(4).position(|w| w == b"\r\n\r\n")
                            {
                                header_end = Some(idx + 4);
                                // Parse Content-Length from headers.
                                let hdrs = &all[..idx];
                                for line in hdrs.split(|b| *b == b'\n') {
                                    let line = String::from_utf8_lossy(line);
                                    let line = line.trim();
                                    if line.to_ascii_lowercase()
                                        .starts_with("content-length:")
                                    {
                                        if let Some((_, v)) = line.split_once(':') {
                                            if let Ok(n) = v.trim().parse::<usize>() {
                                                content_length = Some(n);
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        if let (Some(end), Some(cl)) = (header_end, content_length) {
                            if all.len() >= end + cl {
                                break;
                            }
                        }
                    }
                    Err(_) => break, // read timeout — we have what we have
                }
            }
            // Hand the captured body to the assertion code via the
            // shared Arc<Mutex<>>. Empty body falls through to the
            // assertion's panic message.
            if let (Some(end), Some(cl)) = (header_end, content_length) {
                let slice_end = (end + cl).min(all.len());
                let mut g = captured_for_handler.lock().unwrap();
                *g = all[end..slice_end].to_vec();
            } else if let Some(end) = header_end {
                let mut g = captured_for_handler.lock().unwrap();
                *g = all[end..].to_vec();
            }
            let body = r#"{"resynced":true,"ts":1700000000}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        });
        let agent_url = format!("http://{addr}");

        let signing_key_bytes = [0xefu8; 32];
        let sandbox_id = Uuid::now_v7();
        let r =
            clock_resync_post_restore(&agent_url, sandbox_id, &signing_key_bytes)
                .await;
        assert!(r.is_ok(), "happy path must Ok; got {r:?}");
        // Body has all three fields and the challenge is exactly the
        // 64-char hex width the agent demands.
        let body_bytes = captured.lock().unwrap().clone();
        let body_str = String::from_utf8_lossy(&body_bytes).to_string();
        let parsed: serde_json::Value = serde_json::from_str(&body_str)
            .unwrap_or_else(|e| panic!("body not valid JSON: {e}: body={body_str:?}"));
        // R7-S1: sandbox_id field carries the UUID the caller passed.
        assert_eq!(
            parsed["sandbox_id"].as_str().unwrap_or(""),
            sandbox_id.to_string(),
            "R7-S1: body must bind the caller-supplied sandbox_id"
        );
        // R7-S1: challenge is a 64-char lowercase hex string (32 random bytes).
        let challenge = parsed["challenge"].as_str().unwrap_or("");
        assert_eq!(
            challenge.len(),
            64,
            "R7-S1: challenge MUST be exactly 64 hex chars (32 random bytes); got len={}",
            challenge.len()
        );
        assert!(
            challenge.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
            "R7-S1: challenge MUST be lowercase hex; got {challenge:?}"
        );
        // R7-S1: ts is present + numeric.
        assert!(
            parsed["ts"].is_u64(),
            "R7-S1: body must carry a numeric ts; got {parsed:?}"
        );
    }
}

// Silence unused-Arc warning if no caller imports the alias.
#[allow(dead_code)]
fn _arc_anchor() -> Option<Arc<()>> {
    None
}
