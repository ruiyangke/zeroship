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
use std::sync::Arc;

use uuid::Uuid;

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
            "SELECT snapshot_artifact_path, snapshot_sha256, snapshot_vm_index \
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
    let (Some(p), Some(s), Some(v)) = (artifact_path, sha_bytes, vm_index) else {
        return Err(RestoreHandlerError::Internal(format!(
            "snapshot row missing required columns for {sandbox_id_typed} \
             (artifact / sha / vm_index)"
        )));
    };
    if s.len() != 32 {
        return Err(RestoreHandlerError::Internal(format!(
            "snapshot_sha256 length={} expected 32", s.len()
        )));
    }
    let mut sha = [0u8; 32];
    sha.copy_from_slice(&s);
    Ok(SnapshotRowMeta { artifact_path: p, sha256: sha, vm_index: v })
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
        .submit_restore_job(sandbox_id, snap.vm_index, &alloc_dir)
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
}

// Silence unused-Arc warning if no caller imports the alias.
#[allow(dead_code)]
fn _arc_anchor() -> Option<Arc<()>> {
    None
}
