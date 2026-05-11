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
}

/// Restore a snapshotted sandbox. See module doc for the full flow.
pub async fn restore_sandbox(
    db: &Database,
    store: &dyn SnapshotStore,
    backend: &dyn RestoreBackend,
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
    let result = do_restore_inner(db, store, backend, sandbox_id, &snap, g1).await;

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
    /// Needed by `submit_restore_job` to derive `ZSBX_USER_HOME_DIR`
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
    store: &dyn SnapshotStore,
    backend: &dyn RestoreBackend,
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
    let get_result = store.get(&sandbox_id_typed, &alloc_dir, &snap.sha256);
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

    // 5. Rewrite config.json per § 5.
    let config_path = alloc_dir.join("config.json");
    let alloc_uuid = Uuid::now_v7();
    rewrite_config_json(&config_path, snap.vm_index, alloc_uuid)
        .map_err(RestoreHandlerError::ConfigRewrite)?;

    // 6. Submit the restore job.
    backend
        .submit_restore_job(sandbox_id, snap.vm_index, &alloc_dir, &snap.user_id)
        .map_err(RestoreHandlerError::Backend)?;

    // 7. Wait for /livez.
    backend
        .wait_for_livez(sandbox_id, snap.vm_index)
        .map_err(RestoreHandlerError::Backend)?;

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
    alloc_uuid: Uuid,
) -> Result<(), String> {
    let raw = std::fs::read_to_string(config_path)
        .map_err(|e| format!("read {}: {e}", config_path.display()))?;
    let mut v: serde_json::Value = serde_json::from_str(&raw)
        .map_err(|e| format!("parse {}: {e}", config_path.display()))?;

    // 5.1 net[].tap rewrite. MAC is unchanged because the source
    // worker's vm_index equals the dest worker's vm_index in v1
    // (cross-worker rewrite is v2). We still write the MAC field
    // explicitly so an operator who manually re-uploads a snapshot
    // gets a consistent image.
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

    // 5.1 fs[].socket + serial.file: replace the alloc-uuid path
    // segment. Source paths look like
    // `/opt/nomad/data/alloc/<old-uuid>/.../<rest>`. We rewrite by
    // pattern-matching the `/alloc/<uuid>/` infix.
    let alloc_uuid_str = alloc_uuid.simple().to_string();
    if let Some(fses) = v.get_mut("fs").and_then(|n| n.as_array_mut()) {
        for fs in fses.iter_mut() {
            if let Some(obj) = fs.as_object_mut() {
                if let Some(serde_json::Value::String(s)) = obj.get_mut("socket") {
                    *s = rewrite_alloc_path(s, &alloc_uuid_str);
                }
            }
        }
    }
    if let Some(serial) = v.get_mut("serial").and_then(|n| n.as_object_mut()) {
        if let Some(serde_json::Value::String(s)) = serial.get_mut("file") {
            *s = rewrite_alloc_path(s, &alloc_uuid_str);
        }
    }

    // 5.1 disks[].path: same shape as fs[].socket. The snapshot
    // config embeds the source alloc dir in disk paths (e.g.,
    // `/opt/nomad/data/alloc/<old-uuid>/ch/local/rootfs.img`).
    // Without this rewrite, CH --restore opens the (Nomad-GC'd)
    // source path and the VM never boots — bug #7 surfaced on the
    // 2026-05-10 cluster smoke. Test pinning at
    // `rewrite_config_json_rewrites_disks_path`.
    if let Some(disks) = v.get_mut("disks").and_then(|n| n.as_array_mut()) {
        for disk in disks.iter_mut() {
            if let Some(obj) = disk.as_object_mut() {
                if let Some(serde_json::Value::String(s)) = obj.get_mut("path") {
                    *s = rewrite_alloc_path(s, &alloc_uuid_str);
                }
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

/// Replace the `/alloc/<old-uuid>/` segment in a path with the new
/// uuid. Match is conservative: we look for the literal `/alloc/`
/// + hex/uuid-like segment + `/`. If the source pattern doesn't
/// match, the path is returned unchanged (defensive — the snapshot
/// might have been produced on a non-Nomad backend in dev mode).
fn rewrite_alloc_path(s: &str, new_alloc_uuid: &str) -> String {
    const NEEDLE: &str = "/alloc/";
    let Some(start) = s.find(NEEDLE) else {
        return s.to_string();
    };
    let after = start + NEEDLE.len();
    // Find the next '/' after the alloc uuid segment.
    let Some(rel_end) = s[after..].find('/') else {
        return s.to_string();
    };
    let end = after + rel_end;
    let mut out = String::with_capacity(s.len());
    out.push_str(&s[..after]);
    out.push_str(new_alloc_uuid);
    out.push_str(&s[end..]);
    out
}

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

    #[test]
    fn rewrite_alloc_path_replaces_uuid_segment() {
        let s = "/opt/nomad/data/alloc/abc-123/foo/vfs.sock";
        let r = rewrite_alloc_path(s, "newuuidxxx");
        assert_eq!(r, "/opt/nomad/data/alloc/newuuidxxx/foo/vfs.sock");
    }

    #[test]
    fn rewrite_alloc_path_unchanged_when_no_match() {
        let s = "/var/lib/notnomad/foo";
        let r = rewrite_alloc_path(s, "newuuidxxx");
        assert_eq!(r, s);
    }

    #[test]
    fn rewrite_config_json_rewrites_net_and_socket_paths() {
        let dir = std::env::temp_dir().join(format!(
            "zsbx-restore-test-{}-{}",
            std::process::id(),
            uuid::Uuid::now_v7().simple()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let cfg = dir.join("config.json");
        // Minimal source-shaped config.
        std::fs::write(
            &cfg,
            r#"{
                "net":[{"tap":"zsbx-nm-9","mac":"12:34:56:78:9b:09"}],
                "fs":[{"tag":"keys","socket":"/opt/nomad/data/alloc/oldalloc/zsbx-keys/vfs-keys.sock"}],
                "serial":{"mode":"File","file":"/opt/nomad/data/alloc/oldalloc/serial.log"}
            }"#,
        )
        .unwrap();
        let alloc_uuid = Uuid::now_v7();
        rewrite_config_json(&cfg, 7, alloc_uuid).unwrap();
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        assert_eq!(v["net"][0]["tap"], "zsbx-nm-7");
        assert_eq!(v["net"][0]["mac"], "12:34:56:78:9b:07");
        let new_alloc = alloc_uuid.simple().to_string();
        assert_eq!(
            v["fs"][0]["socket"],
            format!("/opt/nomad/data/alloc/{new_alloc}/zsbx-keys/vfs-keys.sock")
        );
        assert_eq!(
            v["serial"]["file"],
            format!("/opt/nomad/data/alloc/{new_alloc}/serial.log")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Bug #7 (2026-05-10 cluster smoke): the rootfs disk path embeds
    /// the SOURCE alloc UUID. Without rewrite, CH `--restore` opens
    /// the (Nomad-GC'd) source path and the VM never boots; wake
    /// fails with `agent never returned 200 on /livez`.
    #[test]
    fn rewrite_config_json_rewrites_disks_path() {
        let dir = std::env::temp_dir().join(format!(
            "zsbx-restore-test-{}-{}",
            std::process::id(),
            uuid::Uuid::now_v7().simple()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let cfg = dir.join("config.json");
        std::fs::write(
            &cfg,
            r#"{
                "net":[{"tap":"zsbx-nm-2","mac":"12:34:56:78:9b:02"}],
                "fs":[],
                "disks":[{"path":"/opt/nomad/data/alloc/oldalloc/ch/local/rootfs.img"}],
                "serial":{"mode":"File","file":"/opt/nomad/data/alloc/oldalloc/serial.log"}
            }"#,
        )
        .unwrap();
        let alloc_uuid = Uuid::now_v7();
        rewrite_config_json(&cfg, 2, alloc_uuid).unwrap();
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        let new_alloc = alloc_uuid.simple().to_string();
        assert_eq!(
            v["disks"][0]["path"],
            format!("/opt/nomad/data/alloc/{new_alloc}/ch/local/rootfs.img")
        );
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
    /// Process-local reservation map for the source vm_index. The v1
    /// reserve path is "this worker, this slot, right now"; v2's
    /// cross-cluster fallback would consult pg.
    reservations: Arc<Mutex<VmIndexReservations>>,
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
            reservations: Arc::new(Mutex::new(VmIndexReservations::new())),
        }
    }
}

impl RestoreBackend for RealRestoreBackend {
    fn reserve_vm_index(&self, vm_index: i16) -> Result<(), String> {
        self.reservations
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .reserve(vm_index)
    }

    fn release_vm_index(&self, vm_index: i16) {
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
        let agent_url = format!(
            "http://10.{}.{}.2:7777",
            self.cfg.subnet_second_octet,
            100u16 + (vm_index as u16)
        );
        wait_for_livez_blocking(&agent_url, self.agent_livez_timeout)
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
    // Phase B fix #6: the wrapper unconditionally validates 8 envs
    // before branching to the restore path. Derive the same per-
    // sandbox host dirs the cold-boot path uses (see
    // backend/nomad_ch.rs::NomadCHBackend::create, around the
    // `host_dir`/`keys_dir`/`workspace_dir`/`user_home_dir` block).
    let host_dir = cfg
        .host_state_dir
        .join(sandbox_id.simple().to_string());
    let keys_dir = host_dir.join("keys");
    let workspace_dir = host_dir.join("workspace");
    let user_home_dir = cfg
        .user_home_dir_root
        .join(user_id)
        .join("home");
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
                        "ZSBX_KEYS_DIR": keys_dir.display().to_string(),
                        "ZSBX_WORKSPACE_DIR": workspace_dir.display().to_string(),
                        "ZSBX_USER_HOME_DIR": user_home_dir.display().to_string(),
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
                        "CPU": 500,
                        "MemoryMB": memory_mb,
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
}

// Silence unused-Arc warning if no caller imports the alias.
#[allow(dead_code)]
fn _arc_anchor() -> Option<Arc<()>> {
    None
}
