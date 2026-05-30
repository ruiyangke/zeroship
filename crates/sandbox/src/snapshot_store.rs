//! Snapshot artifact storage — L1 (per-worker disk) tier.
//!
//! Source-of-truth: docs/proposals/sandbox-snapshot-restore.md § 4.
//!
//! PR 3a (this file): skeleton + `LocalDiskSnapshotStore`. Provides
//! the trait, types, and a local-disk-only implementation suitable
//! for unit tests and dev-mode (no GCS). Subsequent PRs add:
//!
//! - PR 3c: AEAD wrap layer (AES-256-GCM-SIV per § 4.3)
//! - PR 3d: `GcsSnapshotStore` adapter (L2)
//! - PR 3?: LRU eviction with refcount pinning (§ 4.2)
//!
//! ## Artifact format
//!
//! A snapshot is a directory containing three files written by
//! Cloud Hypervisor's `ch-remote snapshot file://<dir>`:
//!
//!   - `config.json`     — VM config (~2.4 KB; per-alloc identity)
//!   - `state.json`      — VM state (~110 KB)
//!   - `memory-ranges`   — guest RAM (~1 GB)
//!
//! `LocalDiskSnapshotStore` keeps each sandbox's artifact at
//! `<root>/<sandbox-id>/{config,state,memory}.<ext>`, mirroring CH's
//! native layout 1:1 so `cloud-hypervisor --restore source_url=
//! file://<root>/<sandbox-id>` works with no path translation.
//!
//! ## Integrity hash
//!
//! Single SHA-256 over the canonical concatenation:
//!
//! ```text
//! H = sha256(
//!       "config.json"  || len_be(N1) || file_bytes_1 ||
//!       "memory-ranges"|| len_be(N2) || file_bytes_2 ||
//!       "state.json"   || len_be(N3) || file_bytes_3
//!     )
//! ```
//!
//! Names are alphabetical to give a deterministic order regardless
//! of FS traversal. Length prefixes prevent ambiguity at file
//! boundaries. Computed pre-AEAD so the hash authenticates the
//! plaintext (§ 4.3 trust chain).

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

/// Filenames in canonical order — also the set CH expects on restore.
pub const ARTIFACT_FILES: &[&str] = &["config.json", "memory-ranges", "state.json"];

/// Descriptor for a stored snapshot artifact.
#[derive(Debug, Clone)]
pub struct SnapshotMetadata {
    /// Canonical path (URI) — `gs://bucket/key` for L2, absolute
    /// filesystem path for L1-only. Stored in `sandbox.sandboxes.
    /// snapshot_artifact_path`.
    pub artifact_path: String,
    /// Single SHA-256 over the canonical artifact concatenation. See
    /// module doc.
    pub sha256: [u8; 32],
    /// CH binary version that produced this snapshot. Stored in pg
    /// for `snapshot_version_mismatch` detection on restore (§ 8).
    pub ch_version: String,
    /// Total bytes on disk (sum of three files). Operational metric.
    pub bytes: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum SnapshotError {
    #[error("snapshot not found: {0}")]
    NotFound(String),

    #[error(
        "snapshot integrity check failed: expected sha256={}, actual={}",
        hex::encode(.expected),
        hex::encode(.actual)
    )]
    ChecksumMismatch {
        expected: [u8; 32],
        actual: [u8; 32],
    },

    #[error("snapshot artifact missing required file: {0}")]
    MissingFile(&'static str),

    #[error("snapshot I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("snapshot artifact malformed: {0}")]
    InvalidArtifact(String),
}

/// Storage backend for snapshot artifacts. PR 3a ships only
/// [`LocalDiskSnapshotStore`]; later PRs add GCS + AEAD layers.
///
/// All methods are blocking. Async callers should use
/// `compio::runtime::spawn_blocking` (the existing pattern in
/// `crates/sandbox/src/persist.rs`).
pub trait SnapshotStore: Send + Sync {
    /// Persist a CH snapshot directory. The source directory must
    /// contain `config.json`, `state.json`, `memory-ranges` produced
    /// by `ch-remote snapshot file://<source_dir>`.
    ///
    /// Returns the [`SnapshotMetadata`] for storage in pg.
    fn put(
        &self,
        sandbox_id: &str,
        source_dir: &Path,
        ch_version: &str,
    ) -> Result<SnapshotMetadata, SnapshotError>;

    /// Restore an artifact to a target directory. Verifies the SHA-256
    /// against `expected_sha256`. The target directory will contain
    /// the three CH artifact files at the standard names; CH's
    /// `--restore source_url=file://<target_dir>` consumes them
    /// as-is.
    fn get(
        &self,
        sandbox_id: &str,
        target_dir: &Path,
        expected_sha256: &[u8; 32],
    ) -> Result<(), SnapshotError>;

    /// Delete the artifact. Idempotent — succeeds even if not present.
    fn delete(&self, sandbox_id: &str) -> Result<(), SnapshotError>;

    /// Verify the artifact's integrity without restoring it.
    ///
    /// Deep-verify: re-reads every artifact byte and recomputes the
    /// canonical SHA-256. For L2 (GCS) this means ~1 GB of egress
    /// per call — fine for manual integrity audits, too expensive
    /// for the periodic sweep cron. Sweeps should call
    /// [`verify_metadata_only`](Self::verify_metadata_only) instead.
    fn verify(
        &self,
        sandbox_id: &str,
        expected_sha256: &[u8; 32],
    ) -> Result<(), SnapshotError>;

    /// Fast-path integrity check using only store-side metadata.
    ///
    /// For backends that record the canonical SHA-256 in a side
    /// channel (e.g. GCS object custom metadata stamped at `put`
    /// time), this avoids re-streaming the artifact body. The
    /// default impl delegates to [`verify`](Self::verify) so the
    /// trait contract still holds for stores without a metadata
    /// channel (`LocalDiskSnapshotStore`, mocks).
    ///
    /// **Trust model.** A bucket-write attacker can substitute both
    /// the body and any metadata they control, so this fast-path is
    /// NOT a defense against a malicious bucket. It IS a defense
    /// against bit-rot / accidental corruption of the body without
    /// matching corruption of the metadata, which is the failure
    /// mode periodic sweeps target. Operators running an integrity
    /// audit (e.g. after a suspected compromise) call
    /// [`verify`](Self::verify) directly for the deep gate.
    fn verify_metadata_only(
        &self,
        sandbox_id: &str,
        expected_sha256: &[u8; 32],
    ) -> Result<(), SnapshotError> {
        self.verify(sandbox_id, expected_sha256)
    }
}

/// L1-only store backed by the local filesystem. No GCS, no AEAD,
/// no LRU eviction yet. Suitable for unit tests + single-worker dev.
#[derive(Debug, Clone)]
pub struct LocalDiskSnapshotStore {
    root: PathBuf,
}

impl LocalDiskSnapshotStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn artifact_dir(&self, sandbox_id: &str) -> PathBuf {
        self.root.join(sandbox_id)
    }
}

/// Hash the three artifact files in the canonical order described
/// in the module docstring. Streams files chunk-by-chunk so 1 GB
/// memory-ranges doesn't pin 1 GB of RAM in the controller.
fn compute_artifact_sha256(dir: &Path) -> Result<([u8; 32], u64), SnapshotError> {
    use std::io::{BufReader, Read};
    let mut hasher = Sha256::new();
    let mut total: u64 = 0;
    for &name in ARTIFACT_FILES {
        let path = dir.join(name);
        let len = match std::fs::metadata(&path) {
            Ok(m) => m.len(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(SnapshotError::MissingFile(name));
            }
            Err(e) => return Err(SnapshotError::Io(e)),
        };
        hasher.update(name.as_bytes());
        hasher.update(len.to_be_bytes());
        // R5-P1 (perf-r5, A3 slice 2): 1 MiB BufReader collapses
        // 16× the syscall amplification of the prior unbuffered
        // 64 KiB loop. On a 1 GB memory-ranges file the syscall
        // count drops 16384 → 1024 read(2)s. The Sha256 update is
        // still fed `&buf[..n]` chunks of arbitrary length so the
        // hash is byte-identical to the pre-buffered output.
        let f = std::fs::File::open(&path)?;
        let mut reader = BufReader::with_capacity(1 << 20, f);
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let n = reader.read(&mut buf)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
        }
        total = total
            .checked_add(len)
            .ok_or_else(|| SnapshotError::InvalidArtifact("size overflow".into()))?;
    }
    let h = hasher.finalize();
    let mut out = [0u8; 32];
    out.copy_from_slice(&h);
    Ok((out, total))
}

impl SnapshotStore for LocalDiskSnapshotStore {
    fn put(
        &self,
        sandbox_id: &str,
        source_dir: &Path,
        ch_version: &str,
    ) -> Result<SnapshotMetadata, SnapshotError> {
        // Compute hash before moving — the source is the input we
        // authenticate. If the move fails partway, the source still
        // has the canonical state.
        let (sha256, bytes) = compute_artifact_sha256(source_dir)?;

        let dest = self.artifact_dir(sandbox_id);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // Idempotent put: replace existing artifact (re-snapshot
        // semantics, § 4.1). Remove any prior contents, then move.
        if dest.exists() {
            std::fs::remove_dir_all(&dest)?;
        }
        std::fs::create_dir_all(&dest)?;
        for &name in ARTIFACT_FILES {
            std::fs::rename(source_dir.join(name), dest.join(name))
                .or_else(|_| {
                    // Cross-device — fall back to copy + remove.
                    std::fs::copy(source_dir.join(name), dest.join(name))
                        .and_then(|_| std::fs::remove_file(source_dir.join(name)))
                        .map(|_| ())
                })?;
        }

        Ok(SnapshotMetadata {
            artifact_path: dest.to_string_lossy().into_owned(),
            sha256,
            ch_version: ch_version.to_string(),
            bytes,
        })
    }

    fn get(
        &self,
        sandbox_id: &str,
        target_dir: &Path,
        expected_sha256: &[u8; 32],
    ) -> Result<(), SnapshotError> {
        let src = self.artifact_dir(sandbox_id);
        if !src.is_dir() {
            return Err(SnapshotError::NotFound(sandbox_id.to_string()));
        }
        // Verify before restoring — refuse to hand a corrupt artifact
        // to CH (which would hang indefinitely per the v50.2-era bug).
        let (actual, _) = compute_artifact_sha256(&src)?;
        if &actual != expected_sha256 {
            return Err(SnapshotError::ChecksumMismatch {
                expected: *expected_sha256,
                actual,
            });
        }

        std::fs::create_dir_all(target_dir)?;
        for &name in ARTIFACT_FILES {
            let from = src.join(name);
            let to = target_dir.join(name);
            // Source + dest live under the same `host_state_dir` root in
            // production, so `hard_link` is O(1) zero-copy — avoids the
            // 1 GB `memory-ranges` copy that piles onto the worker's
            // disk queue under c=4 wake stress (perf-r4 A3).
            //
            // A stale dest from a previous failed restore would make
            // `hard_link` return `AlreadyExists`; remove and retry.
            // Cross-device deployments fall back to copy.
            match std::fs::hard_link(&from, &to) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    std::fs::remove_file(&to)?;
                    std::fs::hard_link(&from, &to)?;
                }
                Err(e) if e.kind() == std::io::ErrorKind::CrossesDevices => {
                    tracing::warn!(
                        src = %from.display(),
                        dst = %to.display(),
                        "snapshot get: hard_link cross-device, falling back to copy",
                    );
                    std::fs::copy(&from, &to)?;
                }
                Err(e) => return Err(SnapshotError::Io(e)),
            }
            // R5-S5: harden the alloc-side path to 0o444 so neither
            // (a) CH writing back through a writable mmap nor (b) the
            // raw_exec driver chmod'ing the alloc dir can mutate the
            // canonical L1 inode through the hard-link alias. CH opens
            // `memory-ranges` O_RDONLY on `--restore` and reads it via
            // `read_volatile_from` (no MAP_SHARED writeback), so the
            // restore path is unaffected. Because `to` is a hard link
            // to the canonical inode, this also locks the L1 entry —
            // which is correct: L1 entries are immutable after `put`
            // (re-snapshot replaces the entire directory).
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&to, std::fs::Permissions::from_mode(0o444))?;
            }
        }
        Ok(())
    }

    fn delete(&self, sandbox_id: &str) -> Result<(), SnapshotError> {
        let dir = self.artifact_dir(sandbox_id);
        match std::fs::remove_dir_all(&dir) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(SnapshotError::Io(e)),
        }
    }

    fn verify(
        &self,
        sandbox_id: &str,
        expected_sha256: &[u8; 32],
    ) -> Result<(), SnapshotError> {
        let dir = self.artifact_dir(sandbox_id);
        if !dir.is_dir() {
            return Err(SnapshotError::NotFound(sandbox_id.to_string()));
        }
        let (actual, _) = compute_artifact_sha256(&dir)?;
        if &actual != expected_sha256 {
            return Err(SnapshotError::ChecksumMismatch {
                expected: *expected_sha256,
                actual,
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Minimal helper: create a fake CH snapshot dir with the three
    /// expected files. The contents are arbitrary; the test only
    /// cares that hashing + put + get round-trips.
    fn write_fake_artifact(dir: &Path) {
        std::fs::create_dir_all(dir).unwrap();
        for &name in ARTIFACT_FILES {
            let mut f = std::fs::File::create(dir.join(name)).unwrap();
            f.write_all(format!("contents-of-{name}").as_bytes())
                .unwrap();
        }
    }

    /// Each test isolates state in a process-unique temp dir. Avoids
    /// adding the `tempfile` crate just for these tests; the manual
    /// cleanup is fine for the small surface here.
    fn fresh_root() -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "zsbx-snap-test-{}-{}",
            std::process::id(),
            uuid::Uuid::now_v7().simple()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn cleanup(root: &Path) {
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn put_get_round_trips_with_matching_sha256() {
        let root = fresh_root();
        let store = LocalDiskSnapshotStore::new(root.join("store"));

        let src = root.join("src");
        write_fake_artifact(&src);

        let meta = store.put("sbx_test1", &src, "v51.1").unwrap();
        assert_eq!(meta.ch_version, "v51.1");
        assert!(meta.bytes > 0);
        // Source files have been moved (rename) into the store.
        assert!(!src.join("config.json").exists());

        let target = root.join("target");
        store.get("sbx_test1", &target, &meta.sha256).unwrap();
        for &name in ARTIFACT_FILES {
            assert!(target.join(name).is_file(), "{name} missing post-restore");
        }

        cleanup(&root);
    }

    #[test]
    fn get_with_wrong_sha256_returns_checksum_mismatch() {
        let root = fresh_root();
        let store = LocalDiskSnapshotStore::new(root.join("store"));

        let src = root.join("src");
        write_fake_artifact(&src);
        let meta = store.put("sbx_test2", &src, "v51.1").unwrap();

        let target = root.join("target");
        let mut bad = meta.sha256;
        bad[0] ^= 0xff;
        let err = store.get("sbx_test2", &target, &bad).unwrap_err();
        assert!(
            matches!(err, SnapshotError::ChecksumMismatch { .. }),
            "{err:?}"
        );

        cleanup(&root);
    }

    #[test]
    fn get_unknown_sandbox_returns_not_found() {
        let root = fresh_root();
        let store = LocalDiskSnapshotStore::new(root.join("store"));

        let target = root.join("target");
        let err = store
            .get("sbx_doesnotexist", &target, &[0u8; 32])
            .unwrap_err();
        assert!(matches!(err, SnapshotError::NotFound(_)), "{err:?}");

        cleanup(&root);
    }

    #[test]
    fn delete_is_idempotent() {
        let root = fresh_root();
        let store = LocalDiskSnapshotStore::new(root.join("store"));

        let src = root.join("src");
        write_fake_artifact(&src);
        let _ = store.put("sbx_test3", &src, "v51.1").unwrap();

        store.delete("sbx_test3").unwrap();
        // Second delete is a no-op.
        store.delete("sbx_test3").unwrap();
        // Verify after delete returns NotFound.
        assert!(matches!(
            store.verify("sbx_test3", &[0u8; 32]),
            Err(SnapshotError::NotFound(_))
        ));

        cleanup(&root);
    }

    #[test]
    fn put_replaces_existing_artifact_idempotently() {
        // § 4.1 re-snapshot semantics: subsequent put overwrites.
        let root = fresh_root();
        let store = LocalDiskSnapshotStore::new(root.join("store"));

        let src1 = root.join("src1");
        write_fake_artifact(&src1);
        let m1 = store.put("sbx_test4", &src1, "v51.1").unwrap();

        let src2 = root.join("src2");
        std::fs::create_dir_all(&src2).unwrap();
        for &name in ARTIFACT_FILES {
            let mut f = std::fs::File::create(src2.join(name)).unwrap();
            f.write_all(b"different-content-second-snapshot").unwrap();
        }
        let m2 = store.put("sbx_test4", &src2, "v52.0").unwrap();

        // SHA-256 must differ; sizes may differ.
        assert_ne!(m1.sha256, m2.sha256);
        assert_eq!(m2.ch_version, "v52.0");

        // Restore must use the second hash; first hash must fail.
        let target = root.join("target");
        let err = store.get("sbx_test4", &target, &m1.sha256).unwrap_err();
        assert!(matches!(err, SnapshotError::ChecksumMismatch { .. }));
        // Re-create target since the previous get may have created it.
        let target2 = root.join("target2");
        store.get("sbx_test4", &target2, &m2.sha256).unwrap();

        cleanup(&root);
    }

    #[test]
    fn missing_artifact_file_in_source_returns_missing_file() {
        let root = fresh_root();
        let store = LocalDiskSnapshotStore::new(root.join("store"));

        let src = root.join("src-missing");
        std::fs::create_dir_all(&src).unwrap();
        // Only write 2 of 3 files.
        for &name in &["config.json", "state.json"] {
            std::fs::File::create(src.join(name))
                .unwrap()
                .write_all(b"x")
                .unwrap();
        }

        let err = store.put("sbx_test5", &src, "v51.1").unwrap_err();
        assert!(matches!(err, SnapshotError::MissingFile(_)), "{err:?}");

        cleanup(&root);
    }

    /// A3-partial: `LocalDiskSnapshotStore::get` uses `hard_link`, not
    /// `copy`, when source + dest share a filesystem. Equality of the
    /// underlying inode is the canonical proof on Unix.
    #[cfg(unix)]
    #[test]
    fn local_disk_get_uses_hard_link_when_same_fs() {
        use std::os::unix::fs::MetadataExt;

        let root = fresh_root();
        let store = LocalDiskSnapshotStore::new(root.join("store"));

        let src = root.join("src");
        write_fake_artifact(&src);
        let meta = store.put("sbx_hardlink", &src, "v51.1").unwrap();

        let target = root.join("target");
        store.get("sbx_hardlink", &target, &meta.sha256).unwrap();

        let store_dir = root.join("store").join("sbx_hardlink");
        for &name in ARTIFACT_FILES {
            let src_md = std::fs::metadata(store_dir.join(name)).unwrap();
            let dst_md = std::fs::metadata(target.join(name)).unwrap();
            // Same filesystem under tempdir root → hard_link succeeds
            // and both names resolve to the same inode.
            assert_eq!(
                src_md.ino(),
                dst_md.ino(),
                "{name}: expected hard_link (inode equality), got copy",
            );
            assert_eq!(src_md.dev(), dst_md.dev(), "{name}: device id differs");
            // Link count is >= 2 (store entry + restored entry).
            assert!(
                src_md.nlink() >= 2,
                "{name}: nlink={} expected >= 2",
                src_md.nlink(),
            );
        }

        cleanup(&root);
    }

    /// A stale destination from a previous failed restore must not
    /// abort `get`: the `AlreadyExists` branch removes + relinks.
    #[cfg(unix)]
    #[test]
    fn local_disk_get_overwrites_stale_destination() {
        use std::os::unix::fs::MetadataExt;

        let root = fresh_root();
        let store = LocalDiskSnapshotStore::new(root.join("store"));

        let src = root.join("src");
        write_fake_artifact(&src);
        let meta = store.put("sbx_stale", &src, "v51.1").unwrap();

        let target = root.join("target");
        std::fs::create_dir_all(&target).unwrap();
        // Pre-populate with stale files (e.g., from a prior aborted
        // restore). The retry path should clean these up.
        for &name in ARTIFACT_FILES {
            std::fs::write(target.join(name), b"stale-prior-restore").unwrap();
        }

        store.get("sbx_stale", &target, &meta.sha256).unwrap();

        let store_dir = root.join("store").join("sbx_stale");
        for &name in ARTIFACT_FILES {
            let src_ino = std::fs::metadata(store_dir.join(name)).unwrap().ino();
            let dst_ino = std::fs::metadata(target.join(name)).unwrap().ino();
            assert_eq!(src_ino, dst_ino, "{name}: relink did not produce same inode");
        }

        cleanup(&root);
    }

    // NOTE: the `CrossesDevices` fallback path is not exercised here.
    // Simulating it requires two distinct filesystems (e.g., bind
    // mount + tmpfs), which CI runners don't reliably provide. The
    // branch is a small, type-checked tail; production-mode coverage
    // lives in the integration runbook (docs/runbooks/sandbox-nomad-ch.md).

    /// R5-S5: after `get`, the alloc-side hard links must be 0o444 so
    /// CH cannot write back through them and raw_exec cannot widen the
    /// alloc dir to taint the canonical L1 inode. Because the link is
    /// a hard link, the canonical L1 entry inherits the mode too —
    /// that's the intent (L1 is immutable post-`put`).
    #[cfg(unix)]
    #[test]
    fn local_disk_get_makes_alloc_side_read_only() {
        use std::os::unix::fs::PermissionsExt;

        let root = fresh_root();
        let store = LocalDiskSnapshotStore::new(root.join("store"));

        let src = root.join("src");
        write_fake_artifact(&src);
        let meta = store.put("sbx_ro", &src, "v51.1").unwrap();

        let target = root.join("target");
        store.get("sbx_ro", &target, &meta.sha256).unwrap();

        for &name in ARTIFACT_FILES {
            let md = std::fs::metadata(target.join(name)).unwrap();
            // Mask off the file-type bits — `mode()` includes S_IFREG.
            let perm = md.permissions().mode() & 0o777;
            assert_eq!(
                perm, 0o444,
                "{name}: expected 0o444, got {perm:o}",
            );
        }

        cleanup(&root);
    }
}
