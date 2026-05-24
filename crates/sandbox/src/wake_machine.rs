//! C-7-LT-PR2 wake-job state machine driver.
//!
//! Runs on a [`crate::detach::detach_isolated`] OS thread + private
//! compio runtime — `ntex` worker disconnect cannot cancel it
//! (closes C-3/C-6/C-7 silent-cancel and -starvation). No client
//! deadline pressure: the machine can wait as long as source-teardown
//! actually takes (~150 s worst case), emitting `wake_jobs` row
//! transitions between phases so polling clients see live progress.
//!
//! ## Phase ladder (mirrors `do_restore_inner` in `restore_handler.rs`)
//!
//! ```text
//! pending
//!   → reserving_slot   (reserve_vm_index_with_retry on snap.vm_index)
//!   → restoring        (alloc_dir + store.get + config rewrite + submit_restore_job)
//!   → livez_polling    (wait_for_livez)
//!   → clock_resyncing  (unseal + clock_resync_post_restore)
//!   → registering      (backend.register_restored + CAS to Running + clear_snapshot_metadata)
//!   → ok | failed      (terminal)
//! ```
//!
//! Each phase boundary issues `update_wake_job_state(state, …)` against pg
//! BEFORE running the phase work, so polling clients always see the most
//! recent attempted phase. Failures map through [`classify_failure`] to
//! a structured [`WakeErrorCode`] + opaque `error_message`. The wire
//! shape of the poll body is rendered by `admin_handlers::poll_wake`;
//! this module only owns the durable pg state.
//!
//! ## Why duplicate the orchestration (rather than call `do_restore_inner`)
//!
//! The proposal in `docs/proposals/c7-lt-async-wake.md` § 7 keeps the
//! sync path running through phase 4 of the migration; the async machine
//! ships in parallel behind `WakeResponseMode::Async`. The sync path is
//! deleted in phase 5. Refactoring `do_restore_inner` mid-migration to
//! share phase functions with the async machine would couple the two
//! lifecycles: a bug in the refactor would land on both paths
//! simultaneously and the sync path stops being a safe fallback.
//! Duplicating ~150 LOC for one minor of carry-over is a deliberate
//! design choice — the sync path keeps its proven shape verbatim, the
//! async path stands alone.
//!
//! See also: api-surface-r16 spec gates (R16-API1..3 + M2 + the `?sync=1`
//! deprecation telemetry counter wired in `crate::metrics`).

use std::sync::Arc;

use uuid::Uuid;

use crate::db::{
    Database, SandboxStatus, WakeErrorCode, WakeJobState,
};
use crate::persist::Persistence;
use crate::restore_handler::{
    rewrite_config_json, RestoreBackend, RestoreHandlerError,
};
use crate::snapshot_store::SnapshotStore;

/// Driver for a single wake job. Constructed by `admin_handlers::wake_sandbox`
/// on the `POST /admin/sandboxes/{id}/wake` async path; consumed by the
/// detach-isolated worker thread via [`WakeMachine::drive`].
///
/// The struct is `Send + 'static` so it can be moved into the detached
/// thread. The constituent `Arc<dyn …>` types are all `Send + Sync`
/// (`RestoreBackend: Send + Sync`, `SnapshotStore: Send + Sync`, and
/// `Persistence` is `Send + Sync` by composition).
#[allow(missing_debug_implementations)]
pub struct WakeMachine {
    pub database: Arc<Database>,
    pub backend: Arc<dyn RestoreBackend>,
    pub snapshot_store: Arc<dyn SnapshotStore>,
    /// Persistence handle for the `unseal()` step. `None` is a test-only
    /// fixture — production wiring (`AppState::from_config`) fails the
    /// boot when `snapshot_enabled && persist=None` (R5-S1 / A1-FOLLOWUP).
    pub persist: Option<Arc<Persistence>>,
    pub sandbox_id: Uuid,
    pub wake_id: String,
    /// Free-form lessee tag — host_id in production, anything that
    /// identifies the owning controller well enough for the takeover
    /// sweep to evict the row on crash.
    pub lessee: String,
}

/// Outcome of a single wake-machine run. Used internally to feed
/// `drive`'s final pg write; the public surface is the polling
/// endpoint (`GET /wake/{wake_id}`), not this enum.
#[derive(Debug)]
enum Phase {
    /// Successful terminal — `vm_index` is back in service and the
    /// `sandboxes` row is `Running`.
    Ok { vm_index: i16, agent_url: String },
    /// Terminal failure — classification + opaque message for the
    /// poll body. The state machine has already rolled the
    /// `sandboxes` row back (`restoring → snapshotted` /
    /// `→ snapshotted_suspect`) by the time this is returned.
    Failed { code: WakeErrorCode, message: String },
}

impl WakeMachine {
    /// Drive the wake to terminal. Writes pg state transitions on
    /// every phase boundary. On any error, classifies the failure
    /// and rolls back the `sandboxes` row before recording the
    /// terminal `failed` state on the wake-job row.
    ///
    /// This is the fire-and-forget body the controller spawns into
    /// [`crate::detach::detach_isolated`] — the return type is `()`
    /// because the caller has already returned 202 to the client and
    /// the only sink for the outcome is the `wake_jobs` row pg writes.
    pub async fn drive(self) {
        tracing::info!(
            wake_id = %self.wake_id,
            sandbox_id = %self.sandbox_id,
            "wake_machine: drive started"
        );
        let result = self.run().await;
        // Always persist the terminal state, even if rollback failed
        // mid-execution — a `wake_jobs` row stuck on a non-terminal
        // state is a UX trap for polling clients (they'd loop forever
        // until the GC sweep evicts the row).
        match &result {
            Phase::Ok { vm_index, agent_url } => {
                tracing::info!(
                    wake_id = %self.wake_id,
                    sandbox_id = %self.sandbox_id,
                    vm_index = *vm_index,
                    agent_url = %agent_url,
                    "wake_machine: terminal ok"
                );
                if let Err(e) = self
                    .database
                    .update_wake_job_state(
                        &self.wake_id,
                        WakeJobState::Ok,
                        None,
                        None,
                        Some(agent_url.as_str()),
                    )
                    .await
                {
                    tracing::error!(
                        wake_id = %self.wake_id,
                        error = %e,
                        "wake_machine: failed to persist terminal=ok; poll will reflect last in-flight state until GC sweep"
                    );
                }
            }
            Phase::Failed { code, message } => {
                tracing::warn!(
                    wake_id = %self.wake_id,
                    sandbox_id = %self.sandbox_id,
                    error_code = code.as_str(),
                    error_message = %message,
                    "wake_machine: terminal failed"
                );
                if let Err(e) = self
                    .database
                    .update_wake_job_state(
                        &self.wake_id,
                        WakeJobState::Failed,
                        Some(*code),
                        Some(message.as_str()),
                        None,
                    )
                    .await
                {
                    tracing::error!(
                        wake_id = %self.wake_id,
                        error = %e,
                        "wake_machine: failed to persist terminal=failed; poll will reflect last in-flight state until GC sweep"
                    );
                }
            }
        }
    }

    /// Internal phase-by-phase driver. Each phase emits a
    /// `update_wake_job_state` for the entering state, then runs the
    /// work. Failure short-circuits to rollback + classified
    /// [`Phase::Failed`].
    async fn run(&self) -> Phase {
        // ─── Phase: reserving_slot ──────────────────────────────────
        // Pre-flight: read the sandboxes row, verify state, CAS to
        // `restoring`. These three steps are pre-machine work in the
        // sync path (`restore_sandbox`'s outer body); we replicate them
        // here so the state machine owns the full lifecycle.
        let row = match self.database.get_sandbox_row(self.sandbox_id).await {
            Ok(Some(row)) => row,
            Ok(None) => {
                return Phase::Failed {
                    code: WakeErrorCode::Internal,
                    message: format!(
                        "sandbox not found: {}",
                        sandbox_id_typed(self.sandbox_id)
                    ),
                };
            }
            Err(e) => {
                return Phase::Failed {
                    code: WakeErrorCode::Internal,
                    message: format!("get_sandbox_row: {e}"),
                };
            }
        };
        if !matches!(
            row.status,
            SandboxStatus::Snapshotted | SandboxStatus::SnapshottedSuspect
        ) {
            // State mismatch is normally a pre-flight 409 (the sync
            // wake path returns it from the handler before spawning).
            // The async path can also race: between `find_pending_wake`
            // and `insert_wake_job`, the row could have moved. Surface
            // it as `Internal` (the machine should never observe this
            // after a successful pre-flight; if it does, it's a bug).
            return Phase::Failed {
                code: WakeErrorCode::Internal,
                message: format!(
                    "state mismatch: expected snapshotted, current {}",
                    row.status.as_str()
                ),
            };
        }
        let g0 = row.generation;

        // Look up snapshot metadata (sha + vm_index + user_id).
        let snap = match read_snapshot_row(self.database.as_ref(), self.sandbox_id).await {
            Ok(s) => s,
            Err(e) => {
                return Phase::Failed {
                    code: WakeErrorCode::Internal,
                    message: format!("read_snapshot_row: {e}"),
                };
            }
        };

        // Mark `reserving_slot` BEFORE issuing the CAS to `restoring`,
        // so a polling client sees "we're trying to grab the slot"
        // throughout the bounded retry loop.
        self.set_state(WakeJobState::ReservingSlot).await;

        // CAS sandboxes → restoring.
        let g1 = match self
            .database
            .update_sandbox_status(self.sandbox_id, SandboxStatus::Restoring, g0, None)
            .await
        {
            Ok(g) => g,
            Err(e) => {
                return Phase::Failed {
                    code: WakeErrorCode::Internal,
                    message: format!("CAS snapshotted→restoring: {e}"),
                };
            }
        };

        // ─── Phase work: reserve vm_index (bounded retry) ──────────
        if let Err(e) = crate::restore_handler::reserve_vm_index_with_retry(
            self.backend.as_ref(),
            self.sandbox_id,
            snap.vm_index,
        )
        .await
        {
            return self.rollback_and_classify(g1, snap.vm_index, e).await;
        }

        // ─── Phase: restoring (alloc_dir + store.get + config + submit) ───
        self.set_state(WakeJobState::Restoring).await;

        let alloc_dir = self.backend.restore_alloc_dir(self.sandbox_id);
        if alloc_dir.exists() {
            if let Err(e) = std::fs::remove_dir_all(&alloc_dir) {
                return self
                    .rollback_and_classify(
                        g1,
                        snap.vm_index,
                        RestoreHandlerError::Internal(format!(
                            "remove stale alloc_dir {}: {e}",
                            alloc_dir.display()
                        )),
                    )
                    .await;
            }
        }
        if let Err(e) = std::fs::create_dir_all(&alloc_dir) {
            return self
                .rollback_and_classify(
                    g1,
                    snap.vm_index,
                    RestoreHandlerError::Internal(format!(
                        "create alloc_dir {}: {e}",
                        alloc_dir.display()
                    )),
                )
                .await;
        }

        // store.get is sync (sha256 + AEAD decrypt + GCS read on a
        // ~1 GB blob); hop to the blocking pool so the wake machine's
        // (private) compio runtime stays free for the rest of the
        // ladder. Same shape as `do_restore_inner`.
        let get_result = {
            let store_clone = Arc::clone(&self.snapshot_store);
            let sid_typed = sandbox_id_typed(self.sandbox_id);
            let alloc_dir_clone = alloc_dir.clone();
            let sha_clone = snap.sha256;
            compio::runtime::spawn_blocking(move || {
                store_clone.get(&sid_typed, &alloc_dir_clone, &sha_clone)
            })
            .await
            .unwrap_or_else(|p| {
                Err(crate::snapshot_store::SnapshotError::Io(
                    std::io::Error::other(format!("spawn_blocking panic: {p:?}")),
                ))
            })
        };
        if let Err(e) = get_result {
            let mapped = match e {
                crate::snapshot_store::SnapshotError::ChecksumMismatch { .. }
                | crate::snapshot_store::SnapshotError::InvalidArtifact(_) => {
                    RestoreHandlerError::SnapshotCorrupt
                }
                other => RestoreHandlerError::Store(other),
            };
            return self.rollback_and_classify(g1, snap.vm_index, mapped).await;
        }

        let config_path = alloc_dir.join("config.json");
        if let Err(e) = rewrite_config_json(&config_path, snap.vm_index) {
            return self
                .rollback_and_classify(
                    g1,
                    snap.vm_index,
                    RestoreHandlerError::ConfigRewrite(e),
                )
                .await;
        }

        // Submit the restore job (sync; blocking-pool hop).
        let submit_result = {
            let backend_clone = Arc::clone(&self.backend);
            let alloc_dir_owned = alloc_dir.clone();
            let user_id_owned = snap.user_id.clone();
            let sandbox_id = self.sandbox_id;
            let vm_index = snap.vm_index;
            compio::runtime::spawn_blocking(move || {
                backend_clone
                    .submit_restore_job(sandbox_id, vm_index, &alloc_dir_owned, &user_id_owned)
            })
            .await
            .unwrap_or_else(|p| Err(format!("spawn_blocking panic: {p:?}")))
        };
        if let Err(e) = submit_result {
            return self
                .rollback_and_classify(g1, snap.vm_index, RestoreHandlerError::Backend(e))
                .await;
        }

        // ─── Phase: livez_polling ──────────────────────────────────
        self.set_state(WakeJobState::LivezPolling).await;

        let livez_result = {
            let backend_clone = Arc::clone(&self.backend);
            let sandbox_id = self.sandbox_id;
            let vm_index = snap.vm_index;
            compio::runtime::spawn_blocking(move || {
                backend_clone.wait_for_livez(sandbox_id, vm_index)
            })
            .await
            .unwrap_or_else(|p| Err(format!("spawn_blocking panic: {p:?}")))
        };
        if let Err(e) = livez_result {
            // livez_timeout is a distinct wire kind (R16-API1 #4 +
            // the existing landed convention at the agent
            // `wait_for_agent_livez`). Surface accordingly so the SLO
            // dashboard can break it out from `restore_backend_failed`.
            return self
                .rollback_with(g1, snap.vm_index, WakeErrorCode::LivezTimeout, e)
                .await;
        }

        // ─── Phase: clock_resyncing ────────────────────────────────
        self.set_state(WakeJobState::ClockResyncing).await;

        if let Some(p) = self.persist.as_ref() {
            let sealed = match p.unseal(self.sandbox_id).await {
                Ok(s) => s,
                Err(e) => {
                    return self
                        .rollback_and_classify(
                            g1,
                            snap.vm_index,
                            RestoreHandlerError::Internal(format!(
                                "post-wake unseal sandbox {}: {e}",
                                self.sandbox_id
                            )),
                        )
                        .await;
                }
            };
            let agent_url = self.backend.derive_agent_url(snap.vm_index);
            if let Err(e) = crate::restore_handler::clock_resync_post_restore(
                &agent_url,
                self.sandbox_id,
                &sealed.signing_key_bytes,
            )
            .await
            {
                return self
                    .rollback_with(g1, snap.vm_index, WakeErrorCode::ClockResyncFailed, e)
                    .await;
            }

            // ─── Phase: registering ────────────────────────────────
            self.set_state(WakeJobState::Registering).await;

            if let Err(e) = self.backend.register_restored(
                self.sandbox_id,
                snap.vm_index,
                sealed.signing_key_bytes,
                &snap.user_id,
            ) {
                return self
                    .rollback_with(g1, snap.vm_index, WakeErrorCode::RegisterFailed, e)
                    .await;
            }
        } else {
            // Test-fixture path (`persist=None`): skip the
            // unseal/resync/register triad. The sync path mirrors this
            // skip; production refuses to boot in this configuration
            // (R5-S1). We still mark `registering` so the state machine
            // visibly transitions through every phase.
            self.set_state(WakeJobState::Registering).await;
            tracing::warn!(
                wake_id = %self.wake_id,
                sandbox_id = %self.sandbox_id,
                "wake_machine: persist=None — skipped unseal/resync/register \
                 (expected only in tests)"
            );
        }

        // CAS sandboxes → running, clear snapshot metadata.
        if let Err(e) = self
            .database
            .update_sandbox_status(self.sandbox_id, SandboxStatus::Running, g1, None)
            .await
        {
            return self
                .rollback_and_classify(
                    g1,
                    snap.vm_index,
                    RestoreHandlerError::Database(e),
                )
                .await;
        }
        if let Err(e) = self
            .database
            .clear_snapshot_metadata(self.sandbox_id, g1 + 1)
            .await
        {
            // Non-fatal: the row is `running` already. Log loudly.
            tracing::warn!(
                wake_id = %self.wake_id,
                sandbox_id = %self.sandbox_id,
                error = %e,
                "wake_machine: clear_snapshot_metadata after running CAS failed (non-fatal)"
            );
        }

        let agent_url = self.backend.derive_agent_url(snap.vm_index);
        Phase::Ok {
            vm_index: snap.vm_index,
            agent_url,
        }
    }

    /// Best-effort `update_wake_job_state` for an in-flight transition.
    /// Errors are logged and swallowed: a failure to record an
    /// intermediate transition is not fatal (the next phase or the
    /// terminal write will eventually rewrite the row), and aborting
    /// the wake mid-flight on a pg blip would be worse than racing on.
    async fn set_state(&self, state: WakeJobState) {
        if let Err(e) = self
            .database
            .update_wake_job_state(&self.wake_id, state, None, None, None)
            .await
        {
            tracing::warn!(
                wake_id = %self.wake_id,
                state = state.as_str(),
                error = %e,
                "wake_machine: intermediate state write failed; continuing"
            );
        }
    }

    /// Rollback the `sandboxes` row to `snapshotted` (or
    /// `snapshotted_suspect` for `SnapshotCorrupt`), best-effort
    /// teardown of the partial restore alloc, and return a typed
    /// failure for the wake-job row write.
    async fn rollback_and_classify(
        &self,
        g1: i64,
        vm_index: i16,
        err: RestoreHandlerError,
    ) -> Phase {
        let target = match &err {
            RestoreHandlerError::SnapshotCorrupt => SandboxStatus::SnapshottedSuspect,
            _ => SandboxStatus::Snapshotted,
        };
        // Best-effort backend teardown on a separate blocking task —
        // mirrors `do_restore_inner`'s R10-C2 fix.
        let backend_for_teardown = Arc::clone(&self.backend);
        let sandbox_id_for_teardown = self.sandbox_id;
        let _ = compio::runtime::spawn_blocking(move || {
            backend_for_teardown.teardown_restore(sandbox_id_for_teardown, vm_index);
        })
        .await;
        if let Err(rb_err) = self
            .database
            .update_sandbox_status(self.sandbox_id, target, g1, None)
            .await
        {
            tracing::error!(
                wake_id = %self.wake_id,
                sandbox_id = %self.sandbox_id,
                rollback_target = target.as_str(),
                rollback_error = %rb_err,
                original_error = %err,
                "wake_machine: rollback CAS failed; row may be wedged in restoring until transient-takeover sweep"
            );
        }
        Phase::Failed {
            code: classify_failure(&err),
            message: err.to_string(),
        }
    }

    /// Like [`Self::rollback_and_classify`] but takes a pre-classified
    /// `WakeErrorCode`. Used by the livez / clock-resync / register
    /// phases where the underlying string error is opaque but the
    /// wire kind is known from the call site.
    async fn rollback_with(
        &self,
        g1: i64,
        vm_index: i16,
        code: WakeErrorCode,
        message: String,
    ) -> Phase {
        // Rollback target is always `snapshotted` for the
        // post-reserve phases — only `SnapshotCorrupt` (which is
        // pre-livez) routes to `snapshotted_suspect`.
        let target = SandboxStatus::Snapshotted;
        let backend_for_teardown = Arc::clone(&self.backend);
        let sandbox_id_for_teardown = self.sandbox_id;
        let _ = compio::runtime::spawn_blocking(move || {
            backend_for_teardown.teardown_restore(sandbox_id_for_teardown, vm_index);
        })
        .await;
        if let Err(rb_err) = self
            .database
            .update_sandbox_status(self.sandbox_id, target, g1, None)
            .await
        {
            tracing::error!(
                wake_id = %self.wake_id,
                sandbox_id = %self.sandbox_id,
                rollback_target = target.as_str(),
                rollback_error = %rb_err,
                "wake_machine: rollback CAS failed (post-reserve phase)"
            );
        }
        Phase::Failed { code, message }
    }
}

/// Map a `RestoreHandlerError` to a structured `WakeErrorCode`. Used
/// by the pre-livez phases (reserve, store, submit, config rewrite)
/// where the failure shape varies; the post-livez phases use
/// [`WakeMachine::rollback_with`] with a known code.
///
/// Wire codes are determined at the poll-handler render time via
/// `WakeErrorCode::wire_code` — see R16-API1 spec gate #3.
fn classify_failure(err: &RestoreHandlerError) -> WakeErrorCode {
    match err {
        RestoreHandlerError::VmIndexUnavailable { .. } => WakeErrorCode::SlotUnavailable,
        RestoreHandlerError::SnapshotCorrupt => WakeErrorCode::RestoreFailed,
        RestoreHandlerError::Backend(_) => WakeErrorCode::RestoreFailed,
        RestoreHandlerError::Store(_) => WakeErrorCode::RestoreFailed,
        RestoreHandlerError::ConfigRewrite(_) => WakeErrorCode::RestoreFailed,
        RestoreHandlerError::Database(_) => WakeErrorCode::Internal,
        // FeatureDisabled / StateMismatch / NotFound are pre-flight
        // and the async handler refuses to spawn the machine in those
        // shapes — but defense-in-depth: if they leak here, surface as
        // Internal so the wire shape is well-formed.
        _ => WakeErrorCode::Internal,
    }
}

/// Local helper to typed-id-stringify a `Uuid`. Sandbox crate doesn't
/// expose a `sbx_<base62>` formatter as a free function — every
/// existing site re-inlines the `format!("sbx_{}", uuid_to_base62)`
/// call. We do the same here to avoid a one-shot cross-module
/// dependency.
fn sandbox_id_typed(sandbox_id: Uuid) -> String {
    format!(
        "sbx_{}",
        zeroship_core::typed_id::uuid_to_base62(&sandbox_id)
    )
}

// ────────────────────────────────────────────────────────────────────
// Snapshot row helper (mirror of restore_handler::read_snapshot_row).
//
// `restore_handler::read_snapshot_row` is `async fn read_snapshot_row(
// db: &Database, sandbox_id: Uuid) -> Result<SnapshotRowMeta,
// RestoreHandlerError>` but the `SnapshotRowMeta` struct is `pub(super)`
// (not exported), so we can't reuse the function directly without a
// visibility bump that ripples through the sync path.
//
// We re-implement the same SELECT here against the same columns. If
// the snapshot-row schema evolves both paths need to update; the
// proposal § 7 phase 5 deletes the sync path entirely, after which
// this becomes the sole reader.
// ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct WakeSnapshotMeta {
    sha256: [u8; 32],
    vm_index: i16,
    user_id: String,
}

async fn read_snapshot_row(
    db: &Database,
    sandbox_id: Uuid,
) -> Result<WakeSnapshotMeta, String> {
    let pool = db.pool_app().await.map_err(|e| format!("pool_app: {e}"))?;
    let client = pool.get().await.map_err(|e| format!("pool_app.get: {e}"))?;
    let sandbox_id_str = sandbox_id_typed(sandbox_id);
    let row = client
        .query_opt(
            "SELECT snapshot_sha256, snapshot_vm_index, user_id \
               FROM sandbox.sandboxes \
              WHERE sandbox_id = $1::TEXT AND deleted_at IS NULL",
            &[&sandbox_id_str],
        )
        .await
        .map_err(|e| format!("query: {e}"))?
        .ok_or_else(|| format!("sandbox not found: {sandbox_id_str}"))?;
    let sha_bytes: Option<Vec<u8>> = row.try_get(0).ok();
    let vm_index: Option<i16> = row.try_get(1).ok();
    let user_id: Option<String> = row.try_get(2).ok();
    let (Some(s), Some(v), Some(u)) = (sha_bytes, vm_index, user_id) else {
        return Err(format!(
            "snapshot row missing sha/vm_index/user_id for {sandbox_id_str}"
        ));
    };
    if s.len() != 32 {
        return Err(format!("snapshot_sha256 length={} expected 32", s.len()));
    }
    let mut sha = [0u8; 32];
    sha.copy_from_slice(&s);
    Ok(WakeSnapshotMeta {
        sha256: sha,
        vm_index: v,
        user_id: u,
    })
}

// ────────────────────────────────────────────────────────────────────
// Tests
// ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::WakeErrorCode;

    /// classify_failure pins the internal-error mapping for variants
    /// the machine should never see post-pre-flight (FeatureDisabled,
    /// StateMismatch, NotFound). If the async handler ever does spawn
    /// the machine for one of these (a bug), the poll body still
    /// surfaces a clean `internal_error` rather than panicking or
    /// emitting an unknown code.
    #[test]
    fn classify_failure_maps_pre_flight_variants_to_internal() {
        for err in [
            RestoreHandlerError::FeatureDisabled,
            RestoreHandlerError::StateMismatch { current: "running" },
            RestoreHandlerError::NotFound("sbx_x".to_string()),
        ] {
            assert_eq!(classify_failure(&err), WakeErrorCode::Internal);
        }
    }

    /// classify_failure pins the structural mapping: `VmIndexUnavailable`
    /// → `SlotUnavailable` (renders as `vm_index_unavailable` on the
    /// wire via `wire_code`); `SnapshotCorrupt` / `Backend` / `Store`
    /// / `ConfigRewrite` → `RestoreFailed`; `Database` → `Internal`.
    #[test]
    fn classify_failure_maps_restore_variants_to_wake_codes() {
        assert_eq!(
            classify_failure(&RestoreHandlerError::VmIndexUnavailable { requested: 1 }),
            WakeErrorCode::SlotUnavailable
        );
        assert_eq!(
            classify_failure(&RestoreHandlerError::SnapshotCorrupt),
            WakeErrorCode::RestoreFailed
        );
        assert_eq!(
            classify_failure(&RestoreHandlerError::Backend("any".into())),
            WakeErrorCode::RestoreFailed
        );
        assert_eq!(
            classify_failure(&RestoreHandlerError::ConfigRewrite("any".into())),
            WakeErrorCode::RestoreFailed
        );
    }

    /// Sandbox-id typed-string helper mirrors the existing
    /// `format!("sbx_{base62}", ...)` shape every other site uses.
    #[test]
    fn sandbox_id_typed_uses_sbx_prefix_and_base62() {
        let u = Uuid::now_v7();
        let s = sandbox_id_typed(u);
        assert!(s.starts_with("sbx_"), "got {s}");
        assert_eq!(s.len(), 4 + 22, "sbx_ + 22 base62 chars");
    }
}
