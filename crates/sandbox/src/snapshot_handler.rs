//! `SnapshotHandler` — orchestrates `running → snapshotting → snapshotted`
//! per § 3 / § 8.2 / § 11 of the snapshot-restore proposal.
//!
//! Source-of-truth: docs/proposals/sandbox-snapshot-restore.md
//!
//! ## Flow
//!
//! 1. Validate the sandbox is currently `Running` in pg.
//! 2. CAS `running → snapshotting` (existing
//!    `Database::update_sandbox_status_with_host`, generation-fenced).
//! 3. Locate the alloc's CH `--api-socket` path.
//! 4. `ch-remote pause` then `ch-remote snapshot file://<temp>` via
//!    [`ChRemoteClient`].
//! 5. `LocalDiskSnapshotStore::put` (PR 3a) lands the artifact at the
//!    canonical L1 path + computes the SHA-256 of the plaintext.
//! 6. CAS `snapshotting → snapshotted` AND record the artifact
//!    descriptor in pg via `Database::update_snapshot_metadata`
//!    (PR 3h).
//! 7. Kill the source CH + virtiofsd + Nomad alloc; release vm_index.
//!    (Snapshot is **destructive** per § 1 lifecycle.)
//! 8. On any failure mid-flight: CAS `snapshotting → running`
//!    rollback, leave the source VM alive (§ 8 "snapshot mid-write
//!    fails / killed").
//!
//! ## Why a separate module
//!
//! The snapshot/restore handlers want a different testability shape
//! than `nomad_ch.rs::create/stop`: they need a mockable
//! `ch-remote` client (PR 3b ships only the mock — real subprocess
//! invocation lands in v2) and they touch the `SnapshotStore` trait.
//! Keeping them in a sibling module avoids ballooning the already-
//! 3700-line `nomad_ch.rs`.
//!
//! ## Mock-first ch-remote
//!
//! [`ChRemoteClient`] is the trait wrapping the `ch-remote pause` /
//! `snapshot file://` subprocess. PR 3b ships only [`MockChRemoteClient`]
//! which fabricates the three artifact files with placeholder bytes.
//! This lets the handler land + be unit-testable without a real CH
//! process. The real `RealChRemoteClient` (subprocess invoking
//! `ch-remote` from PATH) is a v2 follow-on; the trait + impl
//! boundary is documented inline at the bottom of this file.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use uuid::Uuid;

use crate::db::{Database, DatabaseError, SandboxStatus};
use crate::snapshot_store::{SnapshotMetadata, SnapshotStore, ARTIFACT_FILES};

/// Outcome shape returned by [`snapshot_sandbox`].
#[derive(Debug)]
pub struct SnapshotOutcome {
    pub sandbox_id: Uuid,
    pub metadata: SnapshotMetadata,
    pub vm_index: i16,
    /// Final pg generation post-CAS to `snapshotted`. Caller may
    /// audit-log this; not consumed by anything else in v1.
    pub generation: i64,
}

/// Why the snapshot couldn't proceed. Maps to § 10.0 wire envelope:
///
/// - `StateMismatch`           → 409 `state_mismatch`
/// - `FeatureDisabled`         → 501 `feature_disabled`
/// - `ChRemote` / `Store` /…   → 500 (controller-internal failures)
/// - `CasLost`                 → 409 `state_mismatch` (peer raced us)
/// - `NotFound`                → 404
#[derive(Debug, thiserror::Error)]
pub enum SnapshotHandlerError {
    #[error("snapshot feature disabled (SANDBOX_SNAPSHOT_ENABLED=false)")]
    FeatureDisabled,

    #[error("state mismatch: expected `running`, current `{current}`")]
    StateMismatch { current: &'static str },

    #[error("sandbox not found: {0}")]
    NotFound(String),

    #[error("ch-remote: {0}")]
    ChRemote(String),

    #[error("snapshot store: {0}")]
    Store(#[from] crate::snapshot_store::SnapshotError),

    #[error("database: {0}")]
    Database(#[from] DatabaseError),

    #[error("internal: {0}")]
    Internal(String),
}

/// Trait wrapping the `ch-remote` subprocess. PR 3b ships only the
/// [`MockChRemoteClient`]; the real `RealChRemoteClient` (subprocess
/// invoking `ch-remote` from PATH) is a v2 follow-on.
///
/// Methods are blocking — callers that need async should hop through
/// `compio::runtime::spawn_blocking`.
pub trait ChRemoteClient: Send + Sync {
    /// Send `pause` to the CH API socket. Idempotent — pausing an
    /// already-paused VM should succeed.
    fn pause(&self, api_socket: &Path) -> Result<(), String>;

    /// Send `snapshot file://<dest_dir>` and wait for completion.
    /// `dest_dir` must exist; CH writes the three artifact files
    /// (config.json, state.json, memory-ranges) into it.
    fn snapshot(
        &self,
        api_socket: &Path,
        dest_dir: &Path,
    ) -> Result<(), String>;

    /// CH version string ("v51.1") for the snapshot artifact's
    /// `snapshot_ch_version` column. Used by restore to refuse
    /// cross-major restores (§ 8 "CH version mismatch"). Real
    /// implementation calls `ch-remote --version` once at startup
    /// and caches the result.
    fn version(&self) -> &str;
}

/// Test/mock implementation: fabricates the three artifact files
/// with deterministic placeholder bytes. Used by handler unit tests
/// + dev-mode dry runs.
#[derive(Debug)]
pub struct MockChRemoteClient {
    pub paused: std::sync::Mutex<Vec<PathBuf>>,
    pub fail_pause: bool,
    pub fail_snapshot: bool,
    pub version: String,
}

impl Default for MockChRemoteClient {
    fn default() -> Self {
        Self {
            paused: std::sync::Mutex::new(Vec::new()),
            fail_pause: false,
            fail_snapshot: false,
            version: "v51.1".into(),
        }
    }
}

impl ChRemoteClient for MockChRemoteClient {
    fn pause(&self, api_socket: &Path) -> Result<(), String> {
        if self.fail_pause {
            return Err("mock: pause failed".into());
        }
        self.paused
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(api_socket.to_path_buf());
        Ok(())
    }

    fn snapshot(
        &self,
        _api_socket: &Path,
        dest_dir: &Path,
    ) -> Result<(), String> {
        if self.fail_snapshot {
            return Err("mock: snapshot failed".into());
        }
        std::fs::create_dir_all(dest_dir).map_err(|e| e.to_string())?;
        for &name in ARTIFACT_FILES {
            std::fs::write(
                dest_dir.join(name),
                format!("mock-{name}-placeholder").as_bytes(),
            )
            .map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    fn version(&self) -> &str {
        &self.version
    }
}

/// Hooks the snapshot handler calls into to (a) locate the alloc
/// dir's API socket and (b) tear down the source VM (kill CH +
/// virtiofsd, purge Nomad job, release vm_index). The real
/// implementation in `nomad_ch.rs` does this via the existing
/// registry + `stop_nomad_job` path; tests inject a stub.
///
/// The trait abstracts the pieces of `NomadCHBackend` the handler
/// touches without dragging the entire backend into the snapshot
/// module's surface.
pub trait SourceVmOps: Send + Sync {
    /// Look up the alloc's CH API socket path for `sandbox_id`.
    /// Returns `None` if the sandbox isn't in the in-memory registry
    /// (lease-takeover case — caller decides what to do).
    fn locate_api_socket(&self, sandbox_id: Uuid) -> Option<PathBuf>;

    /// Read the alloc's currently-bound `vm_index`. Used to record
    /// `snapshot_vm_index` for the restore-side identity check.
    fn locate_vm_index(&self, sandbox_id: Uuid) -> Option<i16>;

    /// Kill CH + virtiofsd + Nomad alloc; release vm_index. Called
    /// after the snapshot artifact lands in pg per § 1's destructive
    /// lifecycle. Best-effort — failures are logged but don't roll
    /// back the snapshot (the artifact is already authoritative).
    fn teardown_source(&self, sandbox_id: Uuid) -> Result<(), String>;
}

/// v1 placeholder default backing-versions JSON. The real hash
/// mechanism (walking the `keys/`, `userhome/`, `rootfs-overlay/`
/// dirs and computing per-file content hashes) is a v2 follow-up.
/// For now we store sentinels so the column is populated and the
/// CHECK constraint is satisfied.
pub const DEFAULT_BACKING_VERSIONS: &str =
    r#"{"keys":"unknown","userhome":"unknown","rootfs_overlay":"unknown"}"#;

/// Snapshot a running sandbox. See module doc for the full flow.
///
/// `temp_dir` is a per-controller scratch dir where CH writes the
/// snapshot before [`SnapshotStore::put`] moves it to the canonical
/// L1 path. In production this is `/var/zeroship/ch/snap-stage/<sid>/`;
/// tests use a process-unique temp dir.
pub async fn snapshot_sandbox(
    db: &Database,
    store: &dyn SnapshotStore,
    ch: &dyn ChRemoteClient,
    vm_ops: &dyn SourceVmOps,
    sandbox_id: Uuid,
    temp_dir: PathBuf,
    snapshot_enabled: bool,
) -> Result<SnapshotOutcome, SnapshotHandlerError> {
    if !snapshot_enabled {
        return Err(SnapshotHandlerError::FeatureDisabled);
    }

    // 1. Read the row + check the source state. Refuse anything but
    //    `running` (§ 10.1 default `if_state="running"`).
    let row = db
        .get_sandbox_row(sandbox_id)
        .await?
        .ok_or_else(|| {
            SnapshotHandlerError::NotFound(format!(
                "sbx_{}",
                zeroship_core::typed_id::uuid_to_base62(&sandbox_id)
            ))
        })?;
    if row.status != SandboxStatus::Running {
        return Err(SnapshotHandlerError::StateMismatch {
            current: row.status.as_str(),
        });
    }
    let g0 = row.generation;

    // 2. Resolve the source VM identity BEFORE the destructive CAS:
    //    if the registry doesn't know about this sandbox, we can't
    //    reach `ch-remote` and the snapshot is impossible.
    let api_socket = vm_ops
        .locate_api_socket(sandbox_id)
        .ok_or_else(|| SnapshotHandlerError::Internal(
            "registry: api_socket not found (lease-takeover orphan?)".into()
        ))?;
    let vm_index = vm_ops
        .locate_vm_index(sandbox_id)
        .ok_or_else(|| SnapshotHandlerError::Internal(
            "registry: vm_index not found".into()
        ))?;

    // 3. CAS to snapshotting. From here on, any error path must
    //    rollback the row to `running` (or escalate to `suspect`,
    //    but v1 keeps the simple model: ANY in-handler failure
    //    rolls back; lease-takeover handles crashes).
    let g1 = db
        .update_sandbox_status(sandbox_id, SandboxStatus::Snapshotting, g0, None)
        .await?;

    // 4–7. From here on use a closure so we can run rollback once
    //      on any error.
    let result =
        do_snapshot_inner(db, store, ch, vm_ops, sandbox_id, &api_socket, vm_index, &temp_dir, g1)
            .await;

    match result {
        Ok((meta, g2)) => Ok(SnapshotOutcome {
            sandbox_id,
            metadata: meta,
            vm_index,
            generation: g2,
        }),
        Err(e) => {
            // Best-effort rollback. The CAS may itself fail (e.g., a
            // peer took over via lease-takeover and CASed us to
            // snapshotting_aborted) — log + return the original
            // error so the operator sees the root cause.
            //
            // Cleanup the temp_dir (if it exists) so subsequent
            // attempts don't trip on stale files.
            let _ = std::fs::remove_dir_all(&temp_dir);
            if let Err(rb_err) = db
                .update_sandbox_status(
                    sandbox_id,
                    SandboxStatus::Running,
                    g1,
                    None,
                )
                .await
            {
                tracing::error!(
                    sandbox_id = %sandbox_id,
                    rollback_error = %rb_err,
                    original_error = %e,
                    "snapshot rollback failed; row may be wedged in `snapshotting` until lease-takeover sweep"
                );
            }
            Err(e)
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn do_snapshot_inner(
    db: &Database,
    store: &dyn SnapshotStore,
    ch: &dyn ChRemoteClient,
    vm_ops: &dyn SourceVmOps,
    sandbox_id: Uuid,
    api_socket: &Path,
    vm_index: i16,
    temp_dir: &Path,
    expected_generation: i64,
) -> Result<(SnapshotMetadata, i64), SnapshotHandlerError> {
    // 4. ch-remote pause + snapshot.
    std::fs::create_dir_all(temp_dir).map_err(|e| {
        SnapshotHandlerError::Internal(format!(
            "create snap-stage dir {}: {e}",
            temp_dir.display()
        ))
    })?;
    ch.pause(api_socket)
        .map_err(SnapshotHandlerError::ChRemote)?;
    ch.snapshot(api_socket, temp_dir)
        .map_err(SnapshotHandlerError::ChRemote)?;

    // 5. Move into the snapshot store + compute SHA-256.
    let sandbox_id_typed = format!(
        "sbx_{}",
        zeroship_core::typed_id::uuid_to_base62(&sandbox_id)
    );
    let meta = store
        .put(&sandbox_id_typed, temp_dir, ch.version())
        .map_err(SnapshotHandlerError::Store)?;

    // 6. CAS to snapshotted + record metadata.
    let g2 = db
        .update_snapshot_metadata(
            sandbox_id,
            expected_generation,
            &meta,
            vm_index,
            DEFAULT_BACKING_VERSIONS,
            Some("v1"),
        )
        .await?;

    // 7. Tear down the source. Best-effort — even if this fails the
    //    snapshot is durable + recoverable; the operator gets the
    //    error logged and an orphan-prune sweep mops up at next boot.
    if let Err(e) = vm_ops.teardown_source(sandbox_id) {
        tracing::warn!(
            sandbox_id = %sandbox_id,
            error = %e,
            "snapshot: source teardown failed (non-fatal); orphan-prune will reclaim"
        );
    }

    Ok((meta, g2))
}

// ────────────────────────────────────────────────────────────────────
// TODO (v2): RealChRemoteClient subprocess wrapper.
//
// Shape (sketch):
//
//   pub struct RealChRemoteClient {
//       /// Absolute path to `ch-remote` on $PATH; resolved at
//       /// controller boot. Cached so the snapshot critical path
//       /// doesn't `which`.
//       binary: PathBuf,
//       version: String, // populated by spawning `ch-remote --version`
//   }
//
//   impl ChRemoteClient for RealChRemoteClient {
//       fn pause(&self, api_socket: &Path) -> Result<(), String> {
//           run_blocking(&self.binary, &["--api-socket", api_socket, "pause"])
//       }
//       fn snapshot(&self, api_socket: &Path, dest_dir: &Path) -> Result<(), String> {
//           let url = format!("file://{}", dest_dir.display());
//           run_blocking(&self.binary, &["--api-socket", api_socket, "snapshot", &url])
//       }
//       fn version(&self) -> &str { &self.version }
//   }
//
// `run_blocking` invokes std::process::Command and surfaces stderr
// on non-zero exit. The handler wraps these calls in
// `compio::runtime::spawn_blocking` to keep the ntex worker
// responsive during the ~2.1 s pause/snapshot wall (§ 2 measurement).
//
// The boundary stays here: a v2 PR adds `RealChRemoteClient`
// alongside `MockChRemoteClient`; the handler picks one at construct
// time. The trait + signature are stable (this is the v1 commit
// point).
// ────────────────────────────────────────────────────────────────────

/// Stub `SourceVmOps` for unit tests. Stores fabricated values for
/// `api_socket` and `vm_index`; teardown is a no-op (or fail
/// configurable). Real impl lives in `nomad_ch.rs` alongside the
/// existing alloc registry — wired in PR 3b's caller (admin handler)
/// once the controller end-to-end ships.
#[doc(hidden)]
#[derive(Debug)]
pub struct StubSourceVmOps {
    pub api_socket: PathBuf,
    pub vm_index: i16,
    pub teardown_err: std::sync::Mutex<Option<String>>,
    pub teardown_called: std::sync::atomic::AtomicBool,
}

impl StubSourceVmOps {
    pub fn new(api_socket: PathBuf, vm_index: i16) -> Self {
        Self {
            api_socket,
            vm_index,
            teardown_err: std::sync::Mutex::new(None),
            teardown_called: std::sync::atomic::AtomicBool::new(false),
        }
    }
}

impl SourceVmOps for StubSourceVmOps {
    fn locate_api_socket(&self, _sandbox_id: Uuid) -> Option<PathBuf> {
        Some(self.api_socket.clone())
    }

    fn locate_vm_index(&self, _sandbox_id: Uuid) -> Option<i16> {
        Some(self.vm_index)
    }

    fn teardown_source(&self, _sandbox_id: Uuid) -> Result<(), String> {
        self.teardown_called
            .store(true, std::sync::atomic::Ordering::SeqCst);
        match self.teardown_err.lock().unwrap_or_else(|p| p.into_inner()).clone() {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }
}

// ────────────────────────────────────────────────────────────────────
// Re-export so callers don't have to import from the parent module
// just to construct an Arc.
// ────────────────────────────────────────────────────────────────────

/// Convenience: build the canonical L1 stage dir for a sandbox.
/// `<host_state_dir>/snap-stage/<sandbox-id>/`.
pub fn snap_stage_dir(host_state_dir: &Path, sandbox_id: Uuid) -> PathBuf {
    let typed = format!(
        "sbx_{}",
        zeroship_core::typed_id::uuid_to_base62(&sandbox_id)
    );
    host_state_dir.join("snap-stage").join(typed)
}

// Silence the unused-import warning when no test-only Arc usage lands.
#[allow(dead_code)]
fn _arc_anchor() -> Option<Arc<()>> {
    None
}
