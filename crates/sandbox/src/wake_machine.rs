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
                match self
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
                    Ok(rows_affected) if rows_affected == 0 => {
                        tracing::warn!(
                            target: "sandbox::wake::terminal_overwrite_blocked",
                            wake_id = %self.wake_id,
                            attempted_state = ?WakeJobState::Ok,
                            "update_wake_job_state no-op: row already terminal (R20-C1 guard tripped)"
                        );
                        crate::metrics::inc_wake_terminal_overwrite_blocked();
                    }
                    Ok(_) => {}
                    Err(e) => {
                        tracing::error!(
                            wake_id = %self.wake_id,
                            error = %e,
                            "wake_machine: failed to persist terminal=ok; poll will reflect last in-flight state until GC sweep"
                        );
                    }
                }
            }
            Phase::Failed { code, message } => {
                // R16-S2: sanitize the human-readable error before pg
                // write. The full unredacted message is logged at
                // tracing::warn! level (operator-only journald), but
                // the pg column is SELECT-able by other roles and
                // retained for T_KEEP (5 min) — strip RFC1918 / IPv6
                // link-local / agent URLs and truncate.
                let sanitized = sanitize_error_message(message);
                tracing::warn!(
                    wake_id = %self.wake_id,
                    sandbox_id = %self.sandbox_id,
                    error_code = code.as_str(),
                    error_message = %message,
                    "wake_machine: terminal failed"
                );
                match self
                    .database
                    .update_wake_job_state(
                        &self.wake_id,
                        WakeJobState::Failed,
                        Some(*code),
                        Some(sanitized.as_str()),
                        None,
                    )
                    .await
                {
                    Ok(rows_affected) if rows_affected == 0 => {
                        // R24-I1: thread error_code + error_message into
                        // the terminal-overwrite WARN so a single log
                        // line carries full context. Operator triage no
                        // longer requires grep-correlating this WARN
                        // with the prior "terminal failed" WARN by
                        // wake_id. Uses the unsanitized `message` to
                        // match the prior WARN — both events route to
                        // the same operator-only journald stream (the
                        // R16-S2 sanitization is required only for the
                        // pg column, which is SELECT-able by other roles).
                        tracing::warn!(
                            target: "sandbox::wake::terminal_overwrite_blocked",
                            wake_id = %self.wake_id,
                            attempted_state = ?WakeJobState::Failed,
                            error_code = code.as_str(),
                            error_message = %message,
                            "update_wake_job_state no-op: row already terminal (R20-C1 guard tripped)"
                        );
                        crate::metrics::inc_wake_terminal_overwrite_blocked();
                    }
                    Ok(_) => {}
                    Err(e) => {
                        tracing::error!(
                            wake_id = %self.wake_id,
                            error = %e,
                            "wake_machine: failed to persist terminal=failed; poll will reflect last in-flight state until GC sweep"
                        );
                    }
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
        // R26-I1 collapse (2026-05-25): shared with `do_restore_inner`
        // via `crate::restore_handler::read_snapshot_row`. The unified
        // `SnapshotRowMeta` carries an extra `artifact_path` field the
        // wake-path does not consume; one `String` per wake is a
        // negligible cost for the DRY win.
        let snap = match crate::restore_handler::read_snapshot_row(
            self.database.as_ref(),
            self.sandbox_id,
        )
        .await
        {
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
            .unwrap_or_else(|p| {
                Err(crate::restore_handler::SubmitRestoreError::Other(format!(
                    "spawn_blocking panic: {p:?}"
                )))
            })
        };
        // R23-API1 / R25-S1 / R25-I2: typed submit-error split.
        // `Preflight` → `RestoreHandlerError::StagingPreflight` (path-
        // free Display, typed-id form via the local helper) which
        // `classify_failure` maps to `WakeErrorCode::StagingPathMissing`
        // and the wire code `staging_image_missing`. Verbatim host
        // path + source error are tracing-logged here at the
        // controller boundary BEFORE the path-free typed map; they
        // never cross the wire to an RO admin bearer.
        match submit_result {
            Ok(()) => {}
            Err(e @ crate::restore_handler::SubmitRestoreError::Preflight { .. }) => {
                let sandbox_id_typed = sandbox_id_typed(self.sandbox_id);
                e.log_detail(&sandbox_id_typed);
                let crate::restore_handler::SubmitRestoreError::Preflight { which, .. } = e
                else {
                    unreachable!()
                };
                return self
                    .rollback_and_classify(
                        g1,
                        snap.vm_index,
                        RestoreHandlerError::StagingPreflight {
                            which,
                            sandbox_id_typed,
                        },
                    )
                    .await;
            }
            Err(crate::restore_handler::SubmitRestoreError::Other(s)) => {
                return self
                    .rollback_and_classify(g1, snap.vm_index, RestoreHandlerError::Backend(s))
                    .await;
            }
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

            // ─── T5: signed /version fingerprint check ─────────────
            //
            // After livez but BEFORE clock_resync. Same signing key as
            // clock_resync; reuses the agent's already-spun-up HTTP
            // server. Mismatch = partial-fleet rollout landed the
            // wake on a stale agent binary; roll back with a distinct
            // wire code so the SLO dashboard can route rollout-skew
            // failures away from `livez_timeout` /
            // `restore_backend_failed`. See
            // `verify_agent_version_post_restore` for the sentinel
            // semantics (legacy agent / "unknown" build SHA → skip
            // with WARN, not fail).
            match crate::restore_handler::verify_agent_version_post_restore(
                &agent_url,
                &sealed.signing_key_bytes,
                crate::restore_handler::CONTROLLER_GIT_COMMIT,
            )
            .await
            {
                crate::restore_handler::VersionCheckOutcome::Match => {
                    tracing::debug!(
                        wake_id = %self.wake_id,
                        sandbox_id = %self.sandbox_id,
                        "T5 /version probe matched — wake proceeds"
                    );
                }
                crate::restore_handler::VersionCheckOutcome::Skipped { reason } => {
                    tracing::warn!(
                        target: "sandbox::wake::version_check",
                        wake_id = %self.wake_id,
                        sandbox_id = %self.sandbox_id,
                        reason,
                        "T5 /version probe skipped — additive check disabled for this wake"
                    );
                }
                crate::restore_handler::VersionCheckOutcome::Mismatch { expected, got } => {
                    return self
                        .rollback_with(
                            g1,
                            snap.vm_index,
                            WakeErrorCode::AgentVersionMismatch,
                            format!(
                                "agent /version git_commit={got} does not match \
                                 controller build SHA {expected} \
                                 (partial fleet rollout — wait for completion and retry)"
                            ),
                        )
                        .await;
                }
            }

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
    ///
    /// R22-I1: if `rows_affected == 0` the R20-C1 guard fired — the row
    /// is already terminal and this in-flight transition silently no-oped.
    /// Emit a WARN + bump the counter so the guard-fire is visible.
    async fn set_state(&self, state: WakeJobState) {
        match self
            .database
            .update_wake_job_state(&self.wake_id, state, None, None, None)
            .await
        {
            Ok(rows_affected) if rows_affected == 0 => {
                tracing::warn!(
                    target: "sandbox::wake::terminal_overwrite_blocked",
                    wake_id = %self.wake_id,
                    attempted_state = ?state,
                    "update_wake_job_state no-op: row already terminal (R20-C1 guard tripped)"
                );
                crate::metrics::inc_wake_terminal_overwrite_blocked();
            }
            Ok(_) => {}
            Err(e) => {
                tracing::warn!(
                    wake_id = %self.wake_id,
                    state = state.as_str(),
                    error = %e,
                    "wake_machine: intermediate state write failed; continuing"
                );
            }
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
        // R23-API1 / R25-S1: controller-side staging preflight
        // rejection (workspace.img / user_home.img missing) routes
        // to its own typed wire code so the SLO dashboard and
        // operator triage path can distinguish it from the generic
        // alloc-level `restore_backend_failed` bucket. The
        // `error_message` recorded with this variant is path-free
        // (see `RestoreHandlerError::StagingPreflight`'s `Display`
        // impl) — the host path is logged controller-side via
        // tracing only.
        RestoreHandlerError::StagingPreflight { .. } => WakeErrorCode::StagingPathMissing,
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
// Snapshot row reader: removed in R26-I1 (2026-05-25).
//
// The local `WakeSnapshotMeta` + `read_snapshot_row` were a verbatim
// mirror of `restore_handler::read_snapshot_row` retained per the
// async-wake proposal's § 7 "phase 5 will fix this." After three
// rounds of carry through r22-A4 / r24-A4 / r25 / r26-I1, the bump-
// visibility-and-share path won: `restore_handler::SnapshotRowMeta`
// + `restore_handler::read_snapshot_row` are now `pub(crate)` and
// callers above use the shared reader directly. The unified type
// carries `artifact_path` (one extra `String` per wake read) — the
// wake path does not consume it; the cost is negligible.
// ────────────────────────────────────────────────────────────────────

// ────────────────────────────────────────────────────────────────────
// Error-message sanitizer (R16-S2)
// ────────────────────────────────────────────────────────────────────

/// Maximum length of a sanitized error message in bytes. The wake_jobs
/// `error_message` column is unbounded TEXT, but operator-facing poll
/// responses don't need more than a sentence-or-two of context. 256
/// bytes is generous — matches the standard "log line length" rule of
/// thumb and bounds the controller-side memory footprint of a wedged
/// fleet's wake-job rows.
const ERROR_MESSAGE_MAX_BYTES: usize = 256;

const REDACT_TOKEN: &str = "[redacted]";

/// Sanitize a human-readable error message before writing to the pg
/// `wake_jobs.error_message` column.
///
/// The column is SELECT-able by `sandbox_app` (and any future
/// read-only audit role) and retained for `T_KEEP` post-completion.
/// Wake-path errors today often carry cluster-internal IPs
/// (`10.x.y.z`), agent URLs (`http://10.x.y.z:7000/...`), ureq error
/// bodies, and filesystem paths. None of these should land in a
/// durably-stored, role-readable column.
///
/// Strategy (conservative starting set; **TODO**: expand as new leak
/// surfaces emerge during PR2 cluster smoke — kerberos tickets,
/// pubkey fingerprints, jwt suffixes, GCS signed-URL query strings):
/// 1. Strip agent-shape URLs (`http(s)://<rfc1918-host>:port/...`)
///    as a unit, so the redaction reads as one `[redacted]` rather
///    than `http://[redacted]:7000/[redacted]`.
/// 2. Strip bare RFC1918 IPv4 addresses (10/8, 172.16/12,
///    192.168/16), optionally followed by `:port`.
/// 3. Strip IPv6 link-local prefixes (`fe80::/10`).
/// 4. R22-S1 Mode A: strip filesystem-path absolute prefixes
///    (`/var/zeroship/...`, `/opt/nomad/...`, `/etc/zeroship/...`)
///    as a unit. These are the only path roots the controller +
///    driver write to that carry sandbox/tenant topology
///    (workspace.img, home.img, alloc dirs, key material). Non-secret
///    binary paths (`/usr/local/bin/...`, `/tmp/...`) are NOT redacted
///    so operator triage of binary-path errors still works.
/// 5. R22-S1 Mode A: strip typed-IDs (`[a-z]{3}_[A-Za-z0-9]{18,32}`)
///    — matches the canonical `^[a-z]{3}_[A-Za-z0-9]{22}$` shape in
///    `crates/core/src/typed_id.rs` (`sbx_<base62>`, `wak_<base62>`,
///    `usr_<base62>`, `app_<base62>`, `ses_<base62>`). Length range
///    18-32 is intentional drift-tolerance — if a future prefix
///    changes encoding width the redaction still fires.
/// 6. Truncate to [`ERROR_MESSAGE_MAX_BYTES`] (char-boundary safe).
///
/// Implementation uses a byte-scan rather than `regex` to keep the
/// crate-graph thin (the workspace deliberately avoids `regex` in
/// hot paths). The patterns are simple enough — RFC1918 prefixes
/// are short string constants, port suffixes are `:\d{1,5}` — that
/// a hand-rolled scanner is comparable in cost and far simpler to
/// audit.
pub(crate) fn sanitize_error_message(msg: &str) -> String {
    // Composition order: longer/more-structured matches first so
    // shorter matches don't pre-empt a multi-component pattern. URLs
    // (which embed an IP) before bare IPs. Filesystem paths before
    // typed-IDs (paths may embed typed-IDs as path segments — e.g.
    // `/var/zeroship/ch/<sbx-typed-id>/workspace.img`; redacting the
    // whole path is more informative than two adjacent `[redacted]`
    // tokens, and the typed-id pass would otherwise match the
    // embedded base62 segment).
    let pass1 = strip_agent_urls(msg);
    let pass2 = strip_rfc1918(&pass1);
    let pass3 = strip_ipv6_link_local(&pass2);
    let pass4 = strip_filesystem_paths(&pass3);
    let pass5 = strip_typed_ids(&pass4);

    let s: &str = &pass5;
    if s.len() <= ERROR_MESSAGE_MAX_BYTES {
        return s.to_string();
    }
    // Truncate at a char boundary ≤ ERROR_MESSAGE_MAX_BYTES.
    let mut end = ERROR_MESSAGE_MAX_BYTES;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

/// Scan for the start of a private/reserved IPv4 literal. Returns the
/// byte length of the match (including optional `:port`) at the given
/// offset, or 0 if no match starts there. Match shapes:
/// - `10.\d{1,3}.\d{1,3}.\d{1,3}`            (RFC 1918 10/8)
/// - `172.(1[6-9]|2\d|3[01]).\d{1,3}.\d{1,3}` (RFC 1918 172.16/12)
/// - `192.168.\d{1,3}.\d{1,3}`               (RFC 1918 192.168/16)
/// - `169.254.\d{1,3}.\d{1,3}`               (RFC 3927 link-local / IMDS)
/// - `100.(6[4-9]|[7-9]\d|1[01]\d|12[0-7]).\d{1,3}.\d{1,3}`
///   (RFC 6598 CGNAT 100.64/10)
/// followed by optional `:\d{1,5}`. The function deliberately
/// over-accepts (e.g. `10.999.0.0` would match) — the goal is
/// redaction, not validation. A non-RFC1918 IP that happens to share
/// the `10.` prefix is still cluster-topology; redacting it is the
/// correct conservative choice.
fn match_rfc1918_at(s: &[u8], i: usize) -> usize {
    let n = s.len();
    if i >= n {
        return 0;
    }
    // Encodes (bytes_consumed, octets_remaining) for each prefix.
    let prefix_match: (usize, usize) = if s[i..].starts_with(b"10.") {
        (3, 3)
    } else if s[i..].starts_with(b"192.168.") {
        (8, 2)
    } else if s[i..].starts_with(b"169.254.") {
        // RFC 3927 IPv4 link-local — includes 169.254.169.254 IMDS.
        (8, 2)
    } else if s[i..].starts_with(b"100.") {
        // RFC 6598 CGNAT: second octet must be 64-127.
        let mut j = i + 4;
        let start = j;
        while j < n && s[j].is_ascii_digit() {
            j += 1;
        }
        if j - start == 0 || j - start > 3 || j >= n || s[j] != b'.' {
            return 0;
        }
        let octet: u32 = std::str::from_utf8(&s[start..j])
            .ok()
            .and_then(|t| t.parse().ok())
            .unwrap_or(0);
        if !(64..=127).contains(&octet) {
            return 0;
        }
        (j + 1 - i, 2) // through the `.`, then 2 octets remain
    } else if s[i..].starts_with(b"172.") {
        // Second octet must be 16-31. Parse it.
        let mut j = i + 4;
        let start = j;
        while j < n && s[j].is_ascii_digit() {
            j += 1;
        }
        if j - start == 0 || j - start > 3 || j >= n || s[j] != b'.' {
            return 0;
        }
        // Parse the 1-3 digit number.
        let octet: u32 = std::str::from_utf8(&s[start..j])
            .ok()
            .and_then(|t| t.parse().ok())
            .unwrap_or(0);
        if !(16..=31).contains(&octet) {
            return 0;
        }
        (j + 1 - i, 2) // through the `.`, then 2 octets remain
    } else {
        return 0;
    };
    let (bytes_consumed, need_octets) = prefix_match;
    let mut j = i + bytes_consumed;
    for k in 0..need_octets {
        let start = j;
        while j < n && s[j].is_ascii_digit() {
            j += 1;
        }
        if j - start == 0 || j - start > 3 {
            return 0;
        }
        if k + 1 < need_octets {
            if j >= n || s[j] != b'.' {
                return 0;
            }
            j += 1;
        }
    }
    // Optional `:port`.
    if j < n && s[j] == b':' {
        let mut k = j + 1;
        let start = k;
        while k < n && s[k].is_ascii_digit() {
            k += 1;
        }
        if k - start >= 1 && k - start <= 5 {
            j = k;
        }
    }
    j - i
}

/// Byte length of the UTF-8 codepoint that begins at `bytes[i]`.
/// `bytes` must be the `as_bytes()` view of a valid `&str` and `i`
/// must point at a leading byte (the strip functions only call this
/// on the fallthrough branch where `i` was advanced by either a
/// matched ASCII prefix length or by the previous codepoint width,
/// so this precondition holds).
///
/// Used by the strip passes to advance the non-match branch by a
/// whole codepoint rather than one byte — replacing the pre-r27-M2
/// `out.push(bytes[i] as char)` Latin-1 cast that corrupted any byte
/// > 0x7F. Nomad bodies are UTF-8 JSON, so the latent bug never
/// triggered, but multi-byte glyphs in driver-quoted text (currency
/// symbols, non-Latin scripts) would round-trip as Latin-1 codepoints
/// instead of the intended character.
#[inline]
fn utf8_char_len_at(bytes: &[u8], i: usize) -> usize {
    let b = bytes[i];
    if b < 0x80 {
        1
    } else if b < 0xC0 {
        // Continuation byte — should not happen given the
        // precondition; fall back to 1 so we still advance and the
        // outer loop terminates.
        1
    } else if b < 0xE0 {
        2
    } else if b < 0xF0 {
        3
    } else {
        4
    }
}

/// Strip agent-shape URLs (`http(s)://<rfc1918>:<port>/<path>`).
/// Path consumes anything up to whitespace or quote terminator.
fn strip_agent_urls(msg: &str) -> String {
    let bytes = msg.as_bytes();
    let n = bytes.len();
    let mut out = String::with_capacity(n);
    let mut i = 0;
    while i < n {
        let rest = &bytes[i..];
        let scheme_len = if rest.starts_with(b"http://") {
            7
        } else if rest.starts_with(b"https://") {
            8
        } else {
            0
        };
        if scheme_len == 0 {
            let c_len = utf8_char_len_at(bytes, i);
            out.push_str(&msg[i..i + c_len]);
            i += c_len;
            continue;
        }
        let ip_len = match_rfc1918_at(bytes, i + scheme_len);
        if ip_len == 0 {
            // Scheme matched but no IP — the scheme prefix itself
            // is pure ASCII ("http://" / "https://"), so advancing
            // by one byte is correct here. Use the helper anyway
            // for symmetry / future-proofing if the matcher grows.
            let c_len = utf8_char_len_at(bytes, i);
            out.push_str(&msg[i..i + c_len]);
            i += c_len;
            continue;
        }
        // Consume the optional path: any non-whitespace / non-quote
        // character.
        let mut j = i + scheme_len + ip_len;
        if j < n && bytes[j] == b'/' {
            while j < n {
                let b = bytes[j];
                if b.is_ascii_whitespace() || b == b'"' || b == b'\'' || b == b'`' {
                    break;
                }
                j += 1;
            }
        }
        out.push_str(REDACT_TOKEN);
        i = j;
    }
    out
}

/// Strip bare RFC1918 IPv4 + optional port.
fn strip_rfc1918(msg: &str) -> String {
    let bytes = msg.as_bytes();
    let n = bytes.len();
    let mut out = String::with_capacity(n);
    let mut i = 0;
    while i < n {
        // Only attempt to match at a "word boundary" — start of
        // string OR previous char is non-digit/non-dot. This avoids
        // partial matches inside a larger numeric literal.
        let at_boundary = i == 0 || {
            let p = bytes[i - 1];
            !p.is_ascii_digit() && p != b'.'
        };
        if at_boundary {
            let len = match_rfc1918_at(bytes, i);
            if len > 0 {
                out.push_str(REDACT_TOKEN);
                i += len;
                continue;
            }
        }
        let c_len = utf8_char_len_at(bytes, i);
        out.push_str(&msg[i..i + c_len]);
        i += c_len;
    }
    out
}

/// Strip IPv6 link-local prefix (`fe80::/10`). Conservative: matches
/// `fe80::` followed by `[0-9a-fA-F:%.]` chars + optional `]:port`.
/// Case-insensitive on the `fe80::` prefix.
fn strip_ipv6_link_local(msg: &str) -> String {
    let bytes = msg.as_bytes();
    let n = bytes.len();
    let mut out = String::with_capacity(n);
    let mut i = 0;
    while i < n {
        let rest = &bytes[i..];
        let starts = rest
            .get(..6)
            .map(|p| p.eq_ignore_ascii_case(b"fe80::"))
            .unwrap_or(false);
        if starts {
            let mut j = i + 6;
            // Hex / colon body.
            while j < n {
                let b = bytes[j];
                if b.is_ascii_hexdigit() || matches!(b, b':' | b'.') {
                    j += 1;
                } else {
                    break;
                }
            }
            // Optional zone-id (`%<alnum>+`) — accept letters &
            // digits AFTER the `%` only, so `%eth0` consumes the
            // whole zone-id, not just the hex-prefix.
            if j < n && bytes[j] == b'%' {
                j += 1;
                while j < n && (bytes[j].is_ascii_alphanumeric()) {
                    j += 1;
                }
            }
            // Optional `]:port`.
            if j + 1 < n && bytes[j] == b']' && bytes[j + 1] == b':' {
                let mut k = j + 2;
                let start = k;
                while k < n && bytes[k].is_ascii_digit() {
                    k += 1;
                }
                if k - start >= 1 && k - start <= 5 {
                    j = k;
                }
            }
            if j > i + 6 {
                out.push_str(REDACT_TOKEN);
                i = j;
                continue;
            }
        }
        let c_len = utf8_char_len_at(bytes, i);
        out.push_str(&msg[i..i + c_len]);
        i += c_len;
    }
    out
}

/// R22-S1 Mode A: strip filesystem-path absolute prefixes that
/// expose sandbox/tenant topology. Whitelist of secret-bearing
/// roots ONLY — binary search paths (`/usr/local/bin/...`),
/// temp dirs (`/tmp/...`), and the rest of the FS hierarchy are
/// NOT touched so operator-debuggable error text (e.g. "ch-remote
/// at /usr/local/bin/ch-remote exited 1") still reads cleanly.
///
/// Match shape: `/(var/zeroship|var/lib/zeroship|opt/nomad|etc/zeroship|run/zeroship)`
/// followed by any non-whitespace / non-quote character. The path
/// body is consumed greedily so an embedded typed-id segment
/// doesn't get double-redacted by the later `strip_typed_ids` pass.
///
/// Path roots:
/// - `/var/zeroship/` — controller-owned state: `ch/<sbx>/workspace.img`,
///   `ch/users/<usr>/home.img`, snapshot artifacts.
/// - `/var/lib/zeroship/` — r27-M1: default `runtime_dir` per
///   `NomadCHConfig::runtime_dir` (vmlinuz / rootfs source / kernel
///   blobs); a driver-emitted "open /var/lib/zeroship/ch/vmlinuz"
///   error would have leaked the runtime layout pre-fix.
/// - `/opt/nomad/` — Nomad client alloc dirs:
///   `data/alloc/<alloc-id>/ch/local/rootfs.img`, task_dir, secrets.
/// - `/etc/zeroship/` — controller config: keys/, certs/, TLS material.
/// - `/run/zeroship/` — r27-M1: CH api-socket location (per-VM
///   `<sandbox-id>.sock` lives here on the host); a CH-side
///   "connect /run/zeroship/<sbx>.sock: ECONNREFUSED" error would
///   have leaked the socket path + typed-id pre-fix.
///
/// **Operator-overridden `host_state_dir`**: when `SANDBOX_NOMAD_CH_HOST_STATE_DIR`
/// is set to a non-default value (anything outside the whitelist
/// roots above), driver-emitted path strings still leak. This is
/// documented as a known limitation; closing it requires plumbing
/// the runtime config through the sanitizer, which is structurally
/// larger than the r27-M1 line-item. Operators on a non-default
/// `host_state_dir` should either (a) keep the override under one
/// of the whitelist roots or (b) accept the residual leak until
/// the operator-aware sanitizer ships.
fn strip_filesystem_paths(msg: &str) -> String {
    // r27-M1: order matters — `/var/lib/zeroship/` must come BEFORE
    // `/var/zeroship/` is even ATTEMPTED, otherwise a substring
    // match on `/var/zeroship/` would never run (it doesn't share a
    // prefix with `/var/lib/zeroship/`, so order is actually safe
    // either way here, but we keep the longest-prefix-first
    // discipline so a future operator-prefix addition that DOES
    // share a prefix lands correctly).
    const ROOTS: &[&[u8]] = &[
        b"/var/lib/zeroship/",
        b"/var/zeroship/",
        b"/opt/nomad/",
        b"/etc/zeroship/",
        b"/run/zeroship/",
    ];
    const REDACT_PATH: &str = "<redacted-path>";
    let bytes = msg.as_bytes();
    let n = bytes.len();
    let mut out = String::with_capacity(n);
    let mut i = 0;
    while i < n {
        let mut matched_root_len = 0usize;
        if bytes[i] == b'/' {
            for root in ROOTS {
                if bytes[i..].starts_with(root) {
                    matched_root_len = root.len();
                    break;
                }
            }
        }
        if matched_root_len == 0 {
            let c_len = utf8_char_len_at(bytes, i);
            out.push_str(&msg[i..i + c_len]);
            i += c_len;
            continue;
        }
        // Consume the path body: any non-whitespace / non-quote /
        // non-comma character. Stop on common terminators so a
        // sentence like "path /var/zeroship/x/y.img does not exist"
        // doesn't swallow the trailing prose.
        let mut j = i + matched_root_len;
        while j < n {
            let b = bytes[j];
            if b.is_ascii_whitespace()
                || matches!(b, b'"' | b'\'' | b'`' | b',' | b';' | b')' | b']')
            {
                break;
            }
            j += 1;
        }
        out.push_str(REDACT_PATH);
        i = j;
    }
    out
}

/// R22-S1 Mode A + r27-M1 extension: strip canonical typed-IDs AND
/// standard hyphenated UUIDs to prevent tenant / sandbox / wake
/// enumeration via repeat polling. Two patterns:
///
/// 1. **Typed-ID** — global `^[a-z]{3}_[A-Za-z0-9]{18,32}$` shape
///    from `crates/core/src/typed_id.rs`: three lowercase ASCII
///    letters, underscore, then 18-32 base62 chars. Canonical
///    length is 22 (pinned by `wake_prefix_is_three_chars` etc.);
///    18-32 is deliberate drift-tolerance.
/// 2. **Hyphenated UUID** (r27-M1) — RFC 4122 textual form
///    `xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx` (8-4-4-4-12 hex chars,
///    36 chars total). Surfaces in error messages that quote a
///    Nomad `alloc_id` verbatim (e.g.
///    `alloc 550e8400-e29b-41d4-a716-446655440000 failed`). Without
///    a strip pass, an RO admin reading `wake_jobs.error_message`
///    can enumerate alloc IDs across tenants. Both upper- and
///    lower-case hex accepted; the redaction token is shared with
///    typed-IDs (`<redacted-typed-id>`) because the privacy
///    rationale is identical.
///
/// Word-boundary semantics (both patterns): the match MUST start at
/// a non-alnum-and-non-`_` boundary (or string start) and end at a
/// non-alnum-and-non-`_` boundary (or string end), so embedded
/// substring matches inside larger identifiers are excluded.
fn strip_typed_ids(msg: &str) -> String {
    const REDACT_ID: &str = "<redacted-typed-id>";
    let bytes = msg.as_bytes();
    let n = bytes.len();
    let mut out = String::with_capacity(n);
    let mut i = 0;
    while i < n {
        let at_boundary = i == 0
            || {
                let p = bytes[i - 1];
                !p.is_ascii_alphanumeric() && p != b'_'
            };
        if at_boundary {
            // r27-M1: hyphenated UUID first (its alphabet is
            // [0-9a-fA-F-], strict subset of the typed-id
            // alphabet's hex-relevant slice, but the leading char
            // is a hex digit OR letter so prefix-ambiguity with
            // typed-IDs (which require `_` at position 3) is nil).
            if let Some(end) = match_hyphenated_uuid_at(bytes, i) {
                // Trailing boundary check — `01h00000-1234-...-xxxxxxxxxxxx`
                // followed by alnum/underscore would be a longer
                // identifier, not a UUID. Refuse to redact then.
                let trailing_boundary = end >= n
                    || {
                        let p = bytes[end];
                        !p.is_ascii_alphanumeric() && p != b'_'
                    };
                if trailing_boundary {
                    out.push_str(REDACT_ID);
                    i = end;
                    continue;
                }
            }
        }
        if at_boundary
            && i + 4 < n
            && bytes[i].is_ascii_lowercase()
            && bytes[i + 1].is_ascii_lowercase()
            && bytes[i + 2].is_ascii_lowercase()
            && bytes[i + 3] == b'_'
        {
            // Consume the trailing base62 run (`[A-Za-z0-9]`).
            let mut j = i + 4;
            while j < n && bytes[j].is_ascii_alphanumeric() {
                j += 1;
            }
            let id_chars = j - (i + 4);
            // Must be followed by a non-alnum / non-underscore
            // terminator (or EOS) so e.g. `sbx_ABCDEFGHIJKLMNOPQRSTUV_extra`
            // doesn't match — that's a longer identifier, not a typed-id.
            let trailing_boundary = j >= n
                || {
                    let p = bytes[j];
                    !p.is_ascii_alphanumeric() && p != b'_'
                };
            if (18..=32).contains(&id_chars) && trailing_boundary {
                out.push_str(REDACT_ID);
                i = j;
                continue;
            }
        }
        let c_len = utf8_char_len_at(bytes, i);
        out.push_str(&msg[i..i + c_len]);
        i += c_len;
    }
    out
}

/// r27-M1: match a hyphenated UUID (RFC 4122 textual form) starting
/// at byte offset `i`. Returns the past-end byte offset on a match,
/// `None` otherwise. Pattern: `[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-
/// [0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}` (36 chars total).
/// Caller is responsible for the leading-boundary check; this
/// function does only the literal pattern match. Trailing-boundary
/// check is the caller's job too (so a Nomad `alloc_id` followed
/// by a slash terminator validates, but a UUID followed by `_extra`
/// chars stays a longer identifier and doesn't redact).
fn match_hyphenated_uuid_at(s: &[u8], i: usize) -> Option<usize> {
    const UUID_LEN: usize = 36;
    if i + UUID_LEN > s.len() {
        return None;
    }
    // Hyphen positions per RFC 4122: indices 8, 13, 18, 23.
    const HYPHEN_POSITIONS: [usize; 4] = [8, 13, 18, 23];
    for off in 0..UUID_LEN {
        let b = s[i + off];
        let is_hyphen_slot = HYPHEN_POSITIONS.contains(&off);
        if is_hyphen_slot {
            if b != b'-' {
                return None;
            }
        } else if !b.is_ascii_hexdigit() {
            return None;
        }
    }
    Some(i + UUID_LEN)
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
    /// / `ConfigRewrite` → `RestoreFailed`; `Database` → `Internal`;
    /// `StagingPreflight` → `StagingPathMissing` (R23-API1 / R25-S1).
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
        // R23-API1 / R25-S1: staging-preflight routes to its own wire
        // bucket. The Display impl on this variant is path-free, so
        // the `error_message` recorded by `rollback_and_classify`
        // never carries a host path.
        assert_eq!(
            classify_failure(&RestoreHandlerError::StagingPreflight {
                which: "workspace.img",
                sandbox_id_typed: "sbx_abc".to_string(),
            }),
            WakeErrorCode::StagingPathMissing
        );
    }

    /// R25-S1 pin: the `Display` impl on `StagingPreflight` MUST be
    /// path-free so the `error_message` field that flows to
    /// `wake_jobs` (and thence to an RO admin via `GET
    /// /admin/sandboxes/{id}/wake/{wake_id}`) never carries a host
    /// path. The typed-id form goes on the wire; the host path stays
    /// in tracing logs only.
    #[test]
    fn staging_preflight_display_is_path_free() {
        let err = RestoreHandlerError::StagingPreflight {
            which: "workspace.img",
            sandbox_id_typed: "sbx_01h00000000000000000000000".to_string(),
        };
        let rendered = err.to_string();
        assert!(
            !rendered.contains('/'),
            "Display must not contain any path separator; got: {rendered}"
        );
        assert!(
            rendered.contains("workspace.img"),
            "Display must name the resource; got: {rendered}"
        );
        assert!(
            rendered.contains("sbx_01h00000000000000000000000"),
            "Display must include the typed-id form; got: {rendered}"
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

    // ─── R16-S2 sanitize_error_message ──────────────────────────────

    #[test]
    fn sanitize_strips_rfc1918_10_dot() {
        let s = sanitize_error_message("connect failed to 10.0.0.1");
        assert_eq!(s, "connect failed to [redacted]");
    }

    #[test]
    fn sanitize_strips_rfc1918_with_port() {
        let s = sanitize_error_message("agent /livez 503 from 10.128.0.7:7000");
        assert_eq!(s, "agent /livez 503 from [redacted]");
    }

    #[test]
    fn sanitize_strips_rfc1918_192_168() {
        let s = sanitize_error_message("peer 192.168.5.42:9090 timed out");
        assert_eq!(s, "peer [redacted] timed out");
    }

    #[test]
    fn sanitize_strips_rfc1918_172_16_through_31() {
        // 172.15 is NOT private; 172.16 + 172.31 are.
        let s = sanitize_error_message("hosts: 172.16.1.1, 172.31.255.1, 172.15.0.1");
        assert_eq!(
            s,
            "hosts: [redacted], [redacted], 172.15.0.1"
        );
    }

    #[test]
    fn sanitize_preserves_loopback_and_public_ips() {
        // 127.0.0.1 is loopback (not RFC1918); 8.8.8.8 is public.
        let s = sanitize_error_message("from 127.0.0.1 via 8.8.8.8");
        assert_eq!(s, "from 127.0.0.1 via 8.8.8.8");
    }

    #[test]
    fn sanitize_strips_agent_url_with_path() {
        let s = sanitize_error_message(
            "GET http://10.0.0.1:7000/livez timed out",
        );
        assert_eq!(s, "GET [redacted] timed out");
    }

    #[test]
    fn sanitize_strips_agent_url_https() {
        let s = sanitize_error_message(
            "TLS error against https://172.16.0.1:443/agent/v1",
        );
        assert_eq!(s, "TLS error against [redacted]");
    }

    #[test]
    fn sanitize_strips_ipv6_link_local() {
        let s = sanitize_error_message(
            "could not reach fe80::1234:5678:abcd:ef01%eth0",
        );
        assert_eq!(s, "could not reach [redacted]");
    }

    #[test]
    fn sanitize_truncates_to_256_bytes() {
        let long = "x".repeat(500);
        let s = sanitize_error_message(&long);
        assert_eq!(s.len(), 256);
        assert!(s.chars().all(|c| c == 'x'));
    }

    #[test]
    fn sanitize_handles_truncation_at_char_boundary() {
        // 254 bytes of "x" + 4-byte char "💩" + more chars: must
        // truncate at a char boundary <= 256, so the 4-byte char
        // either fits or is dropped wholesale.
        let mut s = String::new();
        for _ in 0..254 {
            s.push('x');
        }
        s.push('💩'); // 4 bytes; would land at 258
        s.push('a');
        let out = sanitize_error_message(&s);
        assert!(out.len() <= 256);
        // Must end at a valid UTF-8 boundary.
        assert!(out.is_char_boundary(out.len()));
    }

    #[test]
    fn sanitize_passes_through_clean_messages() {
        let s = sanitize_error_message("snapshot artifact corrupt");
        assert_eq!(s, "snapshot artifact corrupt");
    }

    #[test]
    fn sanitize_handles_empty() {
        assert_eq!(sanitize_error_message(""), "");
    }

    #[test]
    fn sanitize_multiple_ips_in_one_message() {
        let s = sanitize_error_message(
            "primary 10.0.0.1 / fallback 192.168.1.5 / link fe80::1",
        );
        assert_eq!(
            s,
            "primary [redacted] / fallback [redacted] / link [redacted]"
        );
    }

    // ─── R17-S1: 169.254/16 (link-local / IMDS) + 100.64/10 (CGNAT) ──

    /// 169.254.169.254 is the AWS/GCP/Azure IMDS address — must be
    /// redacted if it leaks into an error message.
    #[test]
    fn sanitize_strips_169_254_link_local_imds() {
        let s = sanitize_error_message(
            "IMDS probe failed: GET http://169.254.169.254/latest/meta-data timed out",
        );
        assert_eq!(s, "IMDS probe failed: GET [redacted] timed out");
    }

    /// Bare 169.254.x.y without a URL scheme (e.g. appears in a
    /// ureq error body like "connect to 169.254.0.1:80").
    #[test]
    fn sanitize_strips_169_254_bare_with_port() {
        let s = sanitize_error_message("connect to 169.254.0.1:80 refused");
        assert_eq!(s, "connect to [redacted] refused");
    }

    /// 100.64.0.0/10 CGNAT range — lower edge.
    #[test]
    fn sanitize_strips_cgnat_100_64_lower_edge() {
        let s = sanitize_error_message("peer 100.64.0.1:443 reset connection");
        assert_eq!(s, "peer [redacted] reset connection");
    }

    /// 100.127.255.254 — upper edge of CGNAT range (second octet 127).
    #[test]
    fn sanitize_strips_cgnat_100_127_upper_edge() {
        let s = sanitize_error_message("route via 100.127.255.254 unreachable");
        assert_eq!(s, "route via [redacted] unreachable");
    }

    /// 100.63.x.y is below the CGNAT range — must NOT be redacted.
    #[test]
    fn sanitize_preserves_100_63_below_cgnat() {
        let s = sanitize_error_message("host 100.63.0.1 is public");
        assert_eq!(s, "host 100.63.0.1 is public");
    }

    /// 100.128.x.y is above the CGNAT range — must NOT be redacted.
    #[test]
    fn sanitize_preserves_100_128_above_cgnat() {
        let s = sanitize_error_message("host 100.128.0.1 is public");
        assert_eq!(s, "host 100.128.0.1 is public");
    }

    // ─── R22-S1 Mode A: filesystem paths + typed-IDs ─────────────────

    /// Driver TaskEvent verbatim: `disk[1] /var/zeroship/ch/<sid>/
    /// workspace.img does not exist` — the elevated wire path
    /// R22-S1 identifies. Must redact the path; trailing prose must
    /// stay intact.
    #[test]
    fn sanitize_strips_var_zeroship_path() {
        let s = sanitize_error_message(
            "disk[1] /var/zeroship/ch/abc-123/workspace.img does not exist",
        );
        assert_eq!(s, "disk[1] <redacted-path> does not exist");
    }

    /// Nomad alloc-dir leak via driver-side path-rewriter logs.
    #[test]
    fn sanitize_strips_opt_nomad_alloc_dir() {
        let s = sanitize_error_message(
            "Disk(NotFound): /opt/nomad/data/alloc/8f3a-xyz/ch/local/rootfs.img",
        );
        assert_eq!(s, "Disk(NotFound): <redacted-path>");
    }

    /// Controller config / key material path leak.
    #[test]
    fn sanitize_strips_etc_zeroship_config() {
        let s = sanitize_error_message(
            "open /etc/zeroship/keys/ed25519.pub failed",
        );
        assert_eq!(s, "open <redacted-path> failed");
    }

    /// r27-M1: `/var/lib/zeroship/` is the default `runtime_dir`
    /// (vmlinuz + rootfs source + kernel blobs). Pre-fix, a driver
    /// error like "open /var/lib/zeroship/ch/vmlinuz failed" would
    /// have escaped the strip pass and leaked the runtime layout.
    /// (Path body consumption uses whitespace as the terminator
    /// per the existing strip semantics; trailing punctuation like
    /// `:` is consumed into the path span — that's pre-existing
    /// behavior and is documented by the unchanged path-strip
    /// tests above.)
    #[test]
    fn sanitize_strips_var_lib_zeroship_path() {
        let s = sanitize_error_message(
            "open /var/lib/zeroship/ch/vmlinuz failed",
        );
        assert_eq!(s, "open <redacted-path> failed");
    }

    /// r27-M1: `/run/zeroship/` is the per-VM CH api-socket location.
    /// A CH-side connect failure would leak the socket path + the
    /// embedded sandbox-id, doubly bad: enumerable + correlatable
    /// with a wake_job.
    #[test]
    fn sanitize_strips_run_zeroship_path() {
        let s = sanitize_error_message(
            "connect /run/zeroship/sock.s ECONNREFUSED",
        );
        assert_eq!(s, "connect <redacted-path> ECONNREFUSED");
    }

    /// r27-M1: hyphenated UUID (RFC 4122 textual form) surfaces in
    /// Nomad `alloc_id` strings quoted verbatim into error messages.
    /// Both lowercase and uppercase hex must redact; trailing prose
    /// must stay intact via word-boundary handling.
    #[test]
    fn sanitize_strips_hyphenated_uuid_id() {
        // Lowercase hex.
        let s = sanitize_error_message(
            "alloc 550e8400-e29b-41d4-a716-446655440000 failed",
        );
        assert_eq!(s, "alloc <redacted-typed-id> failed");
        // Uppercase hex.
        let s = sanitize_error_message(
            "alloc 550E8400-E29B-41D4-A716-446655440000 failed",
        );
        assert_eq!(s, "alloc <redacted-typed-id> failed");
        // Mixed case.
        let s = sanitize_error_message(
            "alloc 550e8400-E29B-41d4-A716-446655440000 failed",
        );
        assert_eq!(s, "alloc <redacted-typed-id> failed");
    }

    /// r27-M1: a hyphenated UUID followed by an alnum char (i.e. a
    /// longer identifier that happens to start with a UUID-shaped
    /// prefix) must NOT redact — the trailing-boundary rule applies
    /// equally to UUIDs and typed-IDs.
    #[test]
    fn sanitize_preserves_uuid_substring_inside_longer_identifier() {
        // No space or punctuation between UUID and `extra`; this is
        // a single 41-char identifier, not a UUID.
        let s = sanitize_error_message(
            "match 550e8400-e29b-41d4-a716-446655440000extra here",
        );
        // Whole thing left intact (no redaction).
        assert_eq!(
            s, "match 550e8400-e29b-41d4-a716-446655440000extra here",
        );
    }

    /// r27-M1: a UUID-shaped substring with the wrong character at
    /// a hyphen position (an extra hex digit) must NOT match — the
    /// pattern is strict on the 8-4-4-4-12 group widths.
    #[test]
    fn sanitize_preserves_uuid_with_wrong_group_widths() {
        // 9-4-4-4-12 instead of 8-4-4-4-12 — one extra digit in the
        // first group. The strict matcher must reject the whole
        // shape so this passes through verbatim.
        let s = sanitize_error_message(
            "look 550e84000-e29b-41d4-a716-446655440000 here",
        );
        assert_eq!(
            s, "look 550e84000-e29b-41d4-a716-446655440000 here",
        );
    }

    /// Typed-IDs across all five canonical prefixes
    /// (`sbx_`, `wak_`, `usr_`, `app_`, `ses_`). Each is 3 lowercase
    /// chars + `_` + 22 base62 chars per `crates/core/src/typed_id.rs`.
    #[test]
    fn sanitize_strips_typed_ids() {
        // 22 base62 chars each (canonical uuid_to_base62 output).
        let s = sanitize_error_message(
            "wake sbx_0Bk3Np4qR5sT7uV8wYz1A failed at wak_AbCdEfGhIjKlMnOpQrStUv",
        );
        assert_eq!(
            s,
            "wake <redacted-typed-id> failed at <redacted-typed-id>"
        );
        // Each of the well-known prefixes lands the same redaction.
        for prefix in &["sbx", "wak", "usr", "app", "ses"] {
            let input = format!("touched {prefix}_0123456789ABCDEFGHIJKL today");
            let out = sanitize_error_message(&input);
            assert_eq!(
                out, "touched <redacted-typed-id> today",
                "prefix {prefix} must redact (got {out})"
            );
        }
    }

    /// Whitelist-by-prefix: non-secret absolute paths must remain
    /// intact so operator triage of binary / temp-dir errors works.
    #[test]
    fn sanitize_preserves_non_secret_paths() {
        let s = sanitize_error_message(
            "exec /usr/local/bin/ch-remote at /tmp/work failed",
        );
        assert_eq!(
            s,
            "exec /usr/local/bin/ch-remote at /tmp/work failed"
        );
    }

    /// Compound case: one path + one typed-id + one RFC1918 IP in
    /// the same message — all three classes must redact in one
    /// pass without stepping on each other's tokens.
    #[test]
    fn sanitize_strips_combination() {
        let s = sanitize_error_message(
            "wak_AbCdEfGhIjKlMnOpQrStUv: /var/zeroship/ch/x.img on 10.0.0.7 missing",
        );
        assert_eq!(
            s,
            "<redacted-typed-id>: <redacted-path> on [redacted] missing"
        );
    }

    // ─── r27-M2: UTF-8 preservation in strip-pass fallthrough ────
    //
    // Pre-r27-M2 each strip pass advanced the non-match branch via
    // `out.push(bytes[i] as char); i += 1`, which for any byte
    // `> 0x7F` casts as the matching Latin-1 codepoint instead of
    // the actual UTF-8 character. Nomad bodies are JSON (UTF-8
    // passthrough) and driver msgs observed today are 100% ASCII so
    // the bug never triggers in production, but r27-M2 (Aug 2026)
    // closed the same hole on the truncation tail and the strip
    // passes need symmetric treatment. The `€` glyph (`E2 82 AC`,
    // 3 bytes all > 0x7F) is the canonical test — Latin-1 cast on
    // each of its bytes would produce three distinct 1-byte
    // codepoints (`â‚¬`) instead of the single `€`.

    /// Site 1 (`strip_filesystem_paths` fallthrough) — non-match
    /// trailing prose containing `€` must round-trip verbatim.
    #[test]
    fn sanitize_strips_path_preserves_unicode_suffix() {
        let s = sanitize_error_message(
            "open /var/zeroship/x.img failed for €",
        );
        assert_eq!(s, "open <redacted-path> failed for €");
    }

    /// Site 2 (`strip_typed_ids` fallthrough) — non-match suffix
    /// after a redacted typed-ID must preserve `€`.
    #[test]
    fn sanitize_strips_typed_id_preserves_unicode_suffix() {
        let s = sanitize_error_message(
            "sbx_AbCdEfGhIjKlMnOpQrStUv €",
        );
        assert_eq!(s, "<redacted-typed-id> €");
    }

    /// Site 2 (hyphenated-UUID branch in `strip_typed_ids`) — non-
    /// match suffix after a redacted UUID must preserve `€`.
    #[test]
    fn sanitize_strips_uuid_preserves_unicode() {
        let s = sanitize_error_message(
            "alloc 550e8400-e29b-41d4-a716-446655440000 €",
        );
        assert_eq!(s, "alloc <redacted-typed-id> €");
    }

    /// Site 3 (`strip_rfc1918` fallthrough) — non-match suffix
    /// after a redacted 10.x.x.x address must preserve `€`.
    #[test]
    fn sanitize_strips_rfc1918_preserves_unicode() {
        let s = sanitize_error_message("peer 10.0.0.7 €");
        assert_eq!(s, "peer [redacted] €");
    }

    /// Site 4 (`strip_ipv6_link_local` fallthrough) — non-match
    /// suffix after a redacted `fe80::` address must preserve `€`.
    #[test]
    fn sanitize_strips_ipv6_ll_preserves_unicode() {
        let s = sanitize_error_message("via fe80::1 €");
        assert_eq!(s, "via [redacted] €");
    }

    /// Sites 5+6 (`strip_agent_urls` two fallthrough branches +
    /// every pass) — full sanitize composition must be idempotent
    /// on a message containing `€`. Pre-r27-M2 the first pass
    /// would corrupt `€` bytes into Latin-1 codepoints, and a
    /// second pass over the corrupted output would either match
    /// nothing or match unstably depending on the surrounding
    /// pattern — either way `once != twice`. Post-fix every byte
    /// of `€` is preserved verbatim across all strip passes and
    /// the truncation tail, so the result is byte-identical on
    /// re-sanitize.
    #[test]
    fn sanitize_idempotent_with_unicode() {
        let inputs = [
            "open /var/zeroship/x.img failed for €",
            "peer 10.0.0.7:7000 € reset",
            "via fe80::1 €",
            "sbx_AbCdEfGhIjKlMnOpQrStUv €",
            "alloc 550e8400-e29b-41d4-a716-446655440000 €",
            "url http://10.0.0.7:7000/livez €",
            "plain €/yen ¥/pound £ — multi-byte glyphs",
        ];
        for inp in inputs {
            let once = sanitize_error_message(inp);
            let twice = sanitize_error_message(&once);
            assert_eq!(
                once, twice,
                "sanitize must be idempotent on unicode (input: {inp})"
            );
            // The `€` byte sequence `E2 82 AC` must survive.
            assert!(
                once.contains('€'),
                "€ must round-trip through sanitize (input: {inp})"
            );
        }
    }

    /// Idempotency: re-sanitizing already-redacted output must be a
    /// no-op. The redaction tokens themselves (`[redacted]`,
    /// `<redacted-path>`, `<redacted-typed-id>`) must not match any
    /// of the patterns and must not double-redact on repeat.
    #[test]
    fn sanitize_idempotent() {
        let inputs = [
            "wake sbx_0Bk3Np4qR5sT7uV8wYz1A on /var/zeroship/x.img via 10.0.0.7:7000",
            "disk[1] /var/zeroship/ch/abc-123/workspace.img does not exist",
            "primary 10.0.0.1 / fallback 192.168.1.5 / link fe80::1",
            "plain prose with no secrets",
        ];
        for inp in inputs {
            let once = sanitize_error_message(inp);
            let twice = sanitize_error_message(&once);
            assert_eq!(
                once, twice,
                "sanitize must be idempotent (input: {inp})"
            );
        }
    }
}
