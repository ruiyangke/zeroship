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
    store: Arc<dyn SnapshotStore>,
    ch: Arc<dyn ChRemoteClient>,
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
        do_snapshot_inner(
            db,
            Arc::clone(&store),
            Arc::clone(&ch),
            vm_ops,
            sandbox_id,
            &api_socket,
            vm_index,
            &temp_dir,
            g1,
        )
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
    store: Arc<dyn SnapshotStore>,
    ch: Arc<dyn ChRemoteClient>,
    vm_ops: &dyn SourceVmOps,
    sandbox_id: Uuid,
    api_socket: &Path,
    vm_index: i16,
    temp_dir: &Path,
    expected_generation: i64,
) -> Result<(SnapshotMetadata, i64), SnapshotHandlerError> {
    // 4. ch-remote pause + snapshot.
    //
    // R7-P1 (perf-r7): both calls are synchronous — `pause` is a fast
    // HTTP-on-unix-socket round-trip but `snapshot` is the ~2 GB
    // memory dump (~40–50 MB/s per SSD stream). Running them on the
    // async caller pegged a ntex worker for the duration; at c=4 four
    // sync dumps contended on the SSD and snapshot p50 was ~50 s.
    // Hop through `compio::runtime::spawn_blocking` so the worker can
    // serve other RPCs while the dump runs. Pattern mirrors
    // `persist::Persistence::unseal` at
    // `crates/sandbox/src/persist.rs:677-687` and the R5-P1b wrap on
    // `store.get` (commit cdd2e677).
    //
    // SEQUENCING: `ch.pause` MUST complete before `ch.snapshot`
    // otherwise CH may capture a mid-write state. We `.await` the
    // pause future before submitting the snapshot future; the two
    // spawn_blocking calls are strictly sequential, not concurrent.
    std::fs::create_dir_all(temp_dir).map_err(|e| {
        SnapshotHandlerError::Internal(format!(
            "create snap-stage dir {}: {e}",
            temp_dir.display()
        ))
    })?;
    {
        let ch_clone = Arc::clone(&ch);
        let api_socket_owned = api_socket.to_path_buf();
        compio::runtime::spawn_blocking(move || ch_clone.pause(&api_socket_owned))
            .await
            .unwrap_or_else(|p| Err(format!("spawn_blocking panic: {p:?}")))
            .map_err(SnapshotHandlerError::ChRemote)?;
    }
    {
        let ch_clone = Arc::clone(&ch);
        let api_socket_owned = api_socket.to_path_buf();
        let temp_dir_owned = temp_dir.to_path_buf();
        compio::runtime::spawn_blocking(move || {
            ch_clone.snapshot(&api_socket_owned, &temp_dir_owned)
        })
        .await
        .unwrap_or_else(|p| Err(format!("spawn_blocking panic: {p:?}")))
        .map_err(SnapshotHandlerError::ChRemote)?;
    }

    // 5. Move into the snapshot store + compute SHA-256.
    //
    // R7-P1 (perf-r7): `SnapshotStore::put` reads the staged ~2 GB
    // artifact, SHA-256s it, and (for AeadSnapshotStore) seals it.
    // Per the trait doc at `snapshot_store.rs:99`, callers MUST hop
    // through `spawn_blocking`. Same pattern as `store.get` on the
    // restore path (R5-P1b, commit cdd2e677).
    let sandbox_id_typed = format!(
        "sbx_{}",
        zeroship_core::typed_id::uuid_to_base62(&sandbox_id)
    );
    let meta = {
        let store_clone = Arc::clone(&store);
        let sid_clone = sandbox_id_typed.clone();
        let temp_dir_owned = temp_dir.to_path_buf();
        let ch_version = ch.version().to_string();
        compio::runtime::spawn_blocking(move || {
            store_clone.put(&sid_clone, &temp_dir_owned, &ch_version)
        })
        .await
        .unwrap_or_else(|p| {
            Err(crate::snapshot_store::SnapshotError::Io(
                std::io::Error::other(format!("spawn_blocking panic: {p:?}")),
            ))
        })
        .map_err(SnapshotHandlerError::Store)?
    };

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
// Real `ch-remote` subprocess wrapper. Synchronous `std::process::
// Command`; callers in the async path hop through
// `compio::runtime::spawn_blocking` (existing pattern in
// `crates/sandbox/src/persist.rs`).
//
// Hard wall-time budget per call: 30 s. CH v51.1 measured
// pause+snapshot ≈ 2.1 s on n2-standard-32 (§ 2 measurement); 30 s is
// a generous safety net so a wedged `ch-remote` doesn't park a
// blocking worker forever. On budget overrun we kill(SIGKILL) the
// child + return an error.
// ────────────────────────────────────────────────────────────────────

/// Wall-time budget for any single `ch-remote` invocation. CH v51.1
/// snapshot wall is ≈ 2.1 s; 30 s leaves ~14× headroom for a slow
/// host without parking the controller forever.
const CH_REMOTE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Production `ChRemoteClient` — wraps the `ch-remote` binary on
/// `$PATH`. The binary path + version string are resolved once at
/// construct time so the snapshot critical path doesn't `which`.
#[derive(Debug, Clone)]
pub struct RealChRemoteClient {
    binary: PathBuf,
    version: String,
}

impl RealChRemoteClient {
    /// Default constructor: resolves `ch-remote` on `$PATH`.
    /// Falls back to `/usr/local/bin/ch-remote` if `which`-style
    /// resolution fails (production hosts have it there). Caches
    /// `ch-remote --version` on success; on failure caches the
    /// sentinel "unknown" so the handler can still run (the
    /// version string is recorded into pg for forensics, not
    /// gating).
    pub fn new() -> Self {
        let binary = resolve_ch_remote_binary();
        let version = read_ch_remote_version(&binary)
            .unwrap_or_else(|e| {
                tracing::warn!(
                    binary = %binary.display(),
                    error = %e,
                    "ch-remote: --version probe failed; recording 'unknown' for snapshot_ch_version"
                );
                "unknown".to_string()
            });
        Self { binary, version }
    }

    /// Test-friendly constructor — accepts an explicit binary path
    /// + version. Used by unit tests with a fake `ch-remote` shell
    /// script.
    pub fn with_binary(binary: PathBuf, version: String) -> Self {
        Self { binary, version }
    }
}

impl Default for RealChRemoteClient {
    fn default() -> Self {
        Self::new()
    }
}

impl ChRemoteClient for RealChRemoteClient {
    fn pause(&self, api_socket: &Path) -> Result<(), String> {
        run_with_timeout(
            &self.binary,
            &[
                std::ffi::OsStr::new("--api-socket"),
                api_socket.as_os_str(),
                std::ffi::OsStr::new("pause"),
            ],
            CH_REMOTE_TIMEOUT,
        )
    }

    fn snapshot(
        &self,
        api_socket: &Path,
        dest_dir: &Path,
    ) -> Result<(), String> {
        let url = format!("file://{}", dest_dir.display());
        let outcome = run_with_timeout(
            &self.binary,
            &[
                std::ffi::OsStr::new("--api-socket"),
                api_socket.as_os_str(),
                std::ffi::OsStr::new("snapshot"),
                std::ffi::OsStr::new(&url),
            ],
            CH_REMOTE_TIMEOUT,
        );

        // CH v51.1 quirk: `ch-remote snapshot` writes the three
        // artifact files, then CH itself crashes / closes the API
        // socket as part of the snapshot teardown. ch-remote's NEXT
        // request on the same connection (some versions issue a
        // post-snapshot status query) hits the dead socket and
        // returns `Fatal error: HttpApiClient(MissingProtocol)` /
        // `ConnectionReset` with non-zero exit. The snapshot itself
        // SUCCEEDED — the files are on disk. We discovered this in
        // the May-5 manual stress run (round-2 report calls it out
        // verbatim under "Failure modes #5: CH v50.2 snapshot crashes
        // the VMM after writing").
        //
        // Treat the snapshot as authoritative on artifact-on-disk:
        // if `memory-ranges` exists (largest file, written last by
        // CH), the snapshot succeeded regardless of ch-remote's exit
        // code. If we still get a non-zero exit AND no artifact, the
        // original error is real — propagate.
        match outcome {
            Ok(()) => Ok(()),
            Err(e) => {
                // Last-written artifact: present + non-empty → snapshot
                // is on disk; ch-remote's complaint is the post-write
                // VMM-down quirk and not an error we should bubble.
                let memory_ranges = dest_dir.join("memory-ranges");
                match std::fs::metadata(&memory_ranges) {
                    Ok(m) if m.len() > 0 => {
                        tracing::info!(
                            error = %e,
                            memory_ranges_bytes = m.len(),
                            "ch-remote snapshot exited non-zero but artifact is on disk; \
                             treating as success (CH v51.1 post-snapshot VMM-down quirk)"
                        );
                        Ok(())
                    }
                    _ => Err(e),
                }
            }
        }
    }

    fn version(&self) -> &str {
        &self.version
    }
}

/// Best-effort `which ch-remote`. We don't take a dep on the `which`
/// crate just for this — splitting `$PATH` ourselves is ~10 lines.
fn resolve_ch_remote_binary() -> PathBuf {
    if let Ok(path) = std::env::var("PATH") {
        for dir in path.split(':') {
            if dir.is_empty() {
                continue;
            }
            let candidate = std::path::Path::new(dir).join("ch-remote");
            if candidate.is_file() {
                return candidate;
            }
        }
    }
    // Fall back to the production install path. If it's not there
    // either, the first invocation will fail loudly.
    PathBuf::from("/usr/local/bin/ch-remote")
}

/// `ch-remote --version` → "v51.1\n" (or similar). Trims trailing
/// whitespace. Errors on non-zero exit / process spawn failure.
fn read_ch_remote_version(binary: &Path) -> Result<String, String> {
    let out = std::process::Command::new(binary)
        .arg("--version")
        .output()
        .map_err(|e| format!("spawn {} --version: {e}", binary.display()))?;
    if !out.status.success() {
        return Err(format!(
            "{} --version: exit={:?}, stderr={}",
            binary.display(),
            out.status.code(),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if stdout.is_empty() {
        // Some `ch-remote` builds emit the version on stderr.
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        if !stderr.is_empty() {
            return Ok(stderr);
        }
    }
    Ok(stdout)
}

/// Spawn a child, wait up to `timeout`, surface stderr on non-zero
/// exit, kill on timeout. Uses a short polling loop on `try_wait`
/// rather than threads — the controller is single-host, ch-remote
/// finishes in ~2 s, busy-poll at 50 ms is fine.
fn run_with_timeout(
    binary: &Path,
    args: &[&std::ffi::OsStr],
    timeout: std::time::Duration,
) -> Result<(), String> {
    let mut cmd = std::process::Command::new(binary);
    cmd.args(args);
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("spawn {} {:?}: {e}", binary.display(), args))?;

    let deadline = std::time::Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if status.success() {
                    return Ok(());
                }
                let mut stderr = Vec::new();
                if let Some(mut s) = child.stderr.take() {
                    use std::io::Read;
                    let _ = s.read_to_end(&mut stderr);
                }
                return Err(format!(
                    "{} {:?}: exit={:?}, stderr={}",
                    binary.display(),
                    args,
                    status.code(),
                    String::from_utf8_lossy(&stderr).trim()
                ));
            }
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    // Timeout — kill the child. Best-effort; ignore
                    // kill errors (the process may have just exited).
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!(
                        "{} {:?}: timed out after {:?}",
                        binary.display(),
                        args,
                        timeout
                    ));
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "{} {:?}: try_wait: {e}",
                    binary.display(),
                    args
                ));
            }
        }
    }
}

/// Stub `SourceVmOps` for unit tests. Stores fabricated values for
/// `api_socket` and `vm_index`; teardown is a no-op (or fail
/// configurable). Real impl lives in `nomad_ch.rs` alongside the
/// existing alloc registry — wired in PR 3b's caller (admin handler)
/// once the controller end-to-end ships.
///
/// Gated under `cfg(any(test, feature = "test-support"))` so the
/// scaffolding is stripped from production rlibs (R28-API2 sweep,
/// mirrors the R27-API2 `_test_inject_sandbox` precedent).
#[doc(hidden)]
#[cfg(any(test, feature = "test-support"))]
#[derive(Debug)]
pub struct StubSourceVmOps {
    pub api_socket: PathBuf,
    pub vm_index: i16,
    pub teardown_err: std::sync::Mutex<Option<String>>,
    pub teardown_called: std::sync::atomic::AtomicBool,
}

#[cfg(any(test, feature = "test-support"))]
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

#[cfg(any(test, feature = "test-support"))]
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

// ────────────────────────────────────────────────────────────────────
// `RealChRemoteClient` unit tests — drive the subprocess wrapper with
// a fake `ch-remote` shell script. The script is written into a
// temp dir + chmod 0o755'd so the child shells it out exactly like
// a real install.
// ────────────────────────────────────────────────────────────────────

#[cfg(unix)]
#[cfg(test)]
mod real_ch_remote_tests {
    use super::*;
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;

    fn write_script(dir: &Path, name: &str, body: &str) -> PathBuf {
        let p = dir.join(name);
        {
            // Scope the file handle so it's closed (drop) BEFORE exec.
            // ETXTBSY can otherwise fire if the kernel still has the
            // file open for writing when we try to execve it.
            let mut f = std::fs::File::create(&p).unwrap();
            f.write_all(body.as_bytes()).unwrap();
            f.sync_all().unwrap();
        }
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p
    }

    fn fresh_dir() -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "zsbx-real-chrem-{}-{}",
            std::process::id(),
            uuid::Uuid::now_v7().simple()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    /// pause + snapshot succeed when the fake script exits 0 and
    /// writes the three artifact files. Snapshot path is
    /// `file://<dest_dir>` per CH's CLI.
    #[test]
    fn pause_and_snapshot_succeed_with_zero_exit() {
        let dir = fresh_dir();
        // Fake ch-remote: parses `pause` (exit 0) and `snapshot
        // file://<dest>` (extracts the dir, writes the three files).
        let body = r#"#!/usr/bin/env bash
set -eu
sub=""
dest=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --api-socket) shift; shift ;;
    pause) sub="pause"; shift ;;
    snapshot) sub="snapshot"; shift; dest="${1#file://}"; shift ;;
    --version) echo "v51.1"; exit 0 ;;
    *) shift ;;
  esac
done
case "$sub" in
  pause) exit 0 ;;
  snapshot)
    mkdir -p "$dest"
    echo "fake-config" > "$dest/config.json"
    echo "fake-mem"    > "$dest/memory-ranges"
    echo "fake-state"  > "$dest/state.json"
    exit 0 ;;
  *) echo "unknown sub" >&2; exit 2 ;;
esac
"#;
        let bin = write_script(&dir, "ch-remote", body);
        let client = RealChRemoteClient::with_binary(bin, "v51.1".to_string());
        let api_sock = dir.join("ch.sock");
        // pause
        client.pause(&api_sock).expect("pause must succeed");
        // snapshot — dir is created by the fake script
        let dest = dir.join("snap-out");
        client.snapshot(&api_sock, &dest).expect("snapshot must succeed");
        for &name in crate::snapshot_store::ARTIFACT_FILES {
            assert!(dest.join(name).is_file(), "{name} must exist after snapshot");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Non-zero exit surfaces stderr in the error string. Operators
    /// trying to triage a failed snapshot need to see what
    /// `ch-remote` wrote to stderr.
    #[test]
    fn nonzero_exit_surfaces_stderr() {
        let dir = fresh_dir();
        let body = r#"#!/usr/bin/env bash
echo "ch-remote: api-socket connect refused" >&2
exit 1
"#;
        let bin = write_script(&dir, "ch-remote-fail", body);
        let client = RealChRemoteClient::with_binary(bin, "v51.1".to_string());
        let err = client
            .pause(&dir.join("ch.sock"))
            .expect_err("non-zero exit must error");
        assert!(
            err.contains("connect refused"),
            "stderr must surface in error; got {err}"
        );
        assert!(err.contains("exit"), "error must mention exit code; got {err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// CH v51.1 quirk: `ch-remote snapshot` writes the artifact then
    /// CH crashes / closes the API socket, which makes ch-remote's
    /// post-write status query fail with `MissingProtocol` and
    /// non-zero exit. The artifact files ARE on disk; the snapshot
    /// IS authoritative. Verify the on-disk fallback path treats
    /// this as success rather than propagating ch-remote's exit
    /// code as an error. (Discovered May 6 cluster stress;
    /// regression-pinned.)
    #[test]
    fn snapshot_succeeds_when_ch_remote_exits_nonzero_but_artifact_on_disk() {
        let dir = fresh_dir();
        let body = r#"#!/usr/bin/env bash
set -eu
sub=""
dest=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --api-socket) shift; shift ;;
    snapshot) sub="snapshot"; shift; dest="${1#file://}"; shift ;;
    *) shift ;;
  esac
done
if [[ "$sub" == "snapshot" ]]; then
  mkdir -p "$dest"
  echo "fake-config" > "$dest/config.json"
  printf 'fake-memory-ranges-non-empty' > "$dest/memory-ranges"
  echo "fake-state"  > "$dest/state.json"
  # Now mimic v51.1: VMM crashed, post-write ping fails.
  echo "Fatal error: HttpApiClient(MissingProtocol)" >&2
  exit 1
fi
exit 2
"#;
        let bin = write_script(&dir, "ch-remote-quirk", body);
        let client = RealChRemoteClient::with_binary(bin, "v51.1".to_string());
        let dest = dir.join("snap-out");
        client
            .snapshot(&dir.join("ch.sock"), &dest)
            .expect("artifact-on-disk path must treat non-zero exit as success");
        for &name in crate::snapshot_store::ARTIFACT_FILES {
            assert!(dest.join(name).is_file(), "{name} must exist post-fallback");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Sister to the above: non-zero exit AND no artifact on disk
    /// must still propagate the error. The on-disk check is the
    /// trust signal; we don't mask genuine failures.
    #[test]
    fn snapshot_propagates_error_when_no_artifact_on_disk() {
        let dir = fresh_dir();
        let body = r#"#!/usr/bin/env bash
echo "Fatal error: HttpApiClient(MissingProtocol)" >&2
exit 1
"#;
        let bin = write_script(&dir, "ch-remote-real-fail", body);
        let client = RealChRemoteClient::with_binary(bin, "v51.1".to_string());
        let dest = dir.join("snap-out");
        let err = client
            .snapshot(&dir.join("ch.sock"), &dest)
            .expect_err("genuine snapshot failure must error");
        assert!(
            err.contains("MissingProtocol") || err.contains("exit"),
            "error must surface ch-remote stderr / exit; got {err}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Hung child gets killed past the timeout; error mentions
    /// "timed out". We use a 1-second budget here (production budget
    /// is 30 s) so the test runs quickly.
    #[test]
    fn timeout_kills_hung_child() {
        let dir = fresh_dir();
        // Sleeps forever; the timeout path must SIGKILL it.
        let body = r#"#!/usr/bin/env bash
sleep 60
"#;
        let bin = write_script(&dir, "ch-remote-hang", body);
        let api_sock = dir.join("ch.sock");
        let started = std::time::Instant::now();
        let err = run_with_timeout(
            &bin,
            &[
                std::ffi::OsStr::new("--api-socket"),
                api_sock.as_os_str(),
                std::ffi::OsStr::new("pause"),
            ],
            std::time::Duration::from_secs(1),
        )
        .expect_err("hung child must time out");
        let elapsed = started.elapsed();
        assert!(
            elapsed < std::time::Duration::from_secs(5),
            "timeout must fire near the budget, took {elapsed:?}"
        );
        assert!(err.contains("timed out"), "error must mention timeout; got {err}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

// Silence the unused-import warning when no test-only Arc usage lands.
#[allow(dead_code)]
fn _arc_anchor() -> Option<Arc<()>> {
    None
}
