//! GCS L2 + tiered (L1 → L2) snapshot stores.
//!
//! Source-of-truth: `docs/proposals/sandbox-snapshot-restore.md` § 4.
//!
//! ## What's here today (v1)
//!
//! - [`GcsSnapshotStore`]: **type-only skeleton.** Every method
//!   returns `SnapshotError::InvalidArtifact("GcsSnapshotStore: not
//!   yet implemented; gated by SANDBOX_SNAPSHOT_USE_GCS=false in
//!   dev")`. The shape (bucket + service-account placeholders) is
//!   fixed so the tiered composition + the production wiring can
//!   land without churning the L2 type. **Real GCS HTTP I/O lands
//!   in a follow-up PR** — explicitly not in v1, no http client in
//!   the `zeroship-sandbox` deps yet.
//!
//! - [`TieredSnapshotStore`]: production composition. L1 = local
//!   disk (PR 3a), L2 = GCS (this file, stub). Reads always hit L1
//!   first; writes spawn a fire-and-forget compio task to also
//!   upload to L2. Operationally the v1 controller runs against
//!   L1-only — the tiered impl is in tree so we can swap in the
//!   real GCS adapter without touching the call sites.
//!
//! ## v1 caveat for `put`
//!
//! Background L2 upload is fire-and-forget. Since the v1 GCS stub
//! always errors, the spawned task logs a `tracing::warn!` and the
//! L1-only durability story is what ships. **TODO** in the body
//! flags this for the GCS-PR follow-up.
//!
//! ## v1 caveat for `get`
//!
//! L1 miss → L2 attempt directly (no side-effect "fetch back to
//! L1"). When real GCS lands, the side-effect populates L1 so the
//! next get short-circuits — but for v1 we keep the get path
//! straight-line.

use std::path::Path;

use crate::snapshot_store::{SnapshotError, SnapshotMetadata, SnapshotStore};

/// L2 GCS snapshot store. **Skeleton only — every method errors.**
/// The shape is stable so the tiered composition + the production
/// wiring don't change when the real adapter lands.
#[derive(Debug, Clone)]
pub struct GcsSnapshotStore {
    /// Target bucket (e.g. `zeroship-snapshots-prod`).
    pub bucket: String,
    /// Service-account identity for the bucket. Real impl will
    /// resolve a token via the GCE metadata server or a JSON key
    /// file; the placeholder here is just a tag for logging /
    /// configuration parity.
    pub service_account: String,
}

impl GcsSnapshotStore {
    /// Construct a stub. No I/O happens at construct time.
    pub fn new(bucket: impl Into<String>, service_account: impl Into<String>) -> Self {
        Self {
            bucket: bucket.into(),
            service_account: service_account.into(),
        }
    }

    /// Build the canonical object key for a sandbox's snapshot.
    /// Stable across put/get so an out-of-band restore (operator
    /// `gsutil cp gs://bucket/key/...`) is unambiguous.
    pub fn object_prefix(&self, sandbox_id: &str) -> String {
        format!("snapshots/v1/{sandbox_id}/")
    }

    fn not_yet() -> SnapshotError {
        SnapshotError::InvalidArtifact(
            "GcsSnapshotStore: not yet implemented; gated by SANDBOX_SNAPSHOT_USE_GCS=false in dev"
                .into(),
        )
    }
}

impl SnapshotStore for GcsSnapshotStore {
    fn put(
        &self,
        _sandbox_id: &str,
        _source_dir: &Path,
        _ch_version: &str,
    ) -> Result<SnapshotMetadata, SnapshotError> {
        Err(Self::not_yet())
    }

    fn get(
        &self,
        sandbox_id: &str,
        _target_dir: &Path,
        _expected_sha256: &[u8; 32],
    ) -> Result<(), SnapshotError> {
        // Surface as NotFound — the tiered store treats this as
        // "L2 doesn't have it" rather than as a hard error so the
        // L1-only ops shape isn't broken by a stub L2.
        //
        // Rationale: when GCS is genuinely unreachable in production,
        // the real impl returns transient I/O which the caller can
        // retry. Until that exists the tiered store needs a definite
        // "not present" so a get on an L1-miss surfaces a clear
        // NotFound to the restore handler (PR 3e).
        Err(SnapshotError::NotFound(format!(
            "{sandbox_id}: GCS L2 not implemented (stub)"
        )))
    }

    fn delete(&self, _sandbox_id: &str) -> Result<(), SnapshotError> {
        // Idempotent contract: real impl returns Ok on missing
        // object; for the stub we mirror by returning Ok so
        // tiered.delete cleanups don't perma-fail in dev.
        Ok(())
    }

    fn verify(
        &self,
        sandbox_id: &str,
        _expected_sha256: &[u8; 32],
    ) -> Result<(), SnapshotError> {
        Err(SnapshotError::NotFound(format!(
            "{sandbox_id}: GCS L2 not implemented (stub)"
        )))
    }
}

// ────────────────────────────────────────────────────────────────────
// Tiered store
// ────────────────────────────────────────────────────────────────────

/// Two-tier snapshot store. L1 = local disk per worker; L2 = GCS
/// (object storage). All hot-path reads hit L1; L2 is consulted only
/// when L1 misses (e.g. takeover lands the row on a different
/// worker).
///
/// **Type parameters** so callers can swap in their own L2 (e.g.
/// `MockL2` for tests). Production wires
/// `TieredSnapshotStore<LocalDiskSnapshotStore, GcsSnapshotStore>`.
pub struct TieredSnapshotStore<L1, L2> {
    pub l1: L1,
    pub l2: std::sync::Arc<L2>,
}

impl<L1: std::fmt::Debug, L2: std::fmt::Debug> std::fmt::Debug
    for TieredSnapshotStore<L1, L2>
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TieredSnapshotStore")
            .field("l1", &self.l1)
            .field("l2", &self.l2)
            .finish()
    }
}

impl<L1, L2> TieredSnapshotStore<L1, L2> {
    pub fn new(l1: L1, l2: L2) -> Self {
        Self { l1, l2: std::sync::Arc::new(l2) }
    }
}

impl<L1, L2> SnapshotStore for TieredSnapshotStore<L1, L2>
where
    L1: SnapshotStore,
    L2: SnapshotStore + 'static,
{
    fn put(
        &self,
        sandbox_id: &str,
        source_dir: &Path,
        ch_version: &str,
    ) -> Result<SnapshotMetadata, SnapshotError> {
        // L1 is authoritative on the synchronous path: any failure
        // here aborts the snapshot. The artifact lands on local disk
        // first so the source-VM teardown can proceed without
        // waiting on the GCS upload.
        let meta = self.l1.put(sandbox_id, source_dir, ch_version)?;

        // Background L2 upload, fire-and-forget. v1 stub: this will
        // log a warn and exit. **TODO (GCS PR):** add a retry loop
        // (capped exponential backoff, max ~5 min) and a metric for
        // l2_upload_pending / l2_upload_failed_total so operators
        // see the L2-lag during a worker drain.
        //
        // We can't `clone` arbitrary L2; require Arc-shareable above.
        let l2 = self.l2.clone();
        let sandbox_id = sandbox_id.to_string();
        // L2 stub doesn't need source_dir (it errors anyway), but
        // the real impl will need the artifact bytes. For the stub
        // we pass a non-existent path; real impl will pass the L1
        // artifact_path so it streams from local disk → GCS.
        let artifact_path = std::path::PathBuf::from(meta.artifact_path.clone());
        let ch_version_owned = ch_version.to_string();
        let sha256 = meta.sha256;
        compio::runtime::spawn(async move {
            // Synchronous I/O inside the task — the stub returns
            // immediately. When real GCS lands, wrap in
            // spawn_blocking like the rest of the controller.
            match l2.put(&sandbox_id, &artifact_path, &ch_version_owned) {
                Ok(m) => {
                    if m.sha256 != sha256 {
                        tracing::warn!(
                            sandbox_id = %sandbox_id,
                            "tiered: L2 returned a different sha256; possible re-encrypt drift"
                        );
                    } else {
                        tracing::debug!(
                            sandbox_id = %sandbox_id,
                            "tiered: L2 upload completed"
                        );
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        sandbox_id = %sandbox_id,
                        error = %e,
                        "tiered: L2 upload failed (v1 fire-and-forget; GCS PR adds retry)"
                    );
                }
            }
        })
        .detach();

        Ok(meta)
    }

    fn get(
        &self,
        sandbox_id: &str,
        target_dir: &Path,
        expected_sha256: &[u8; 32],
    ) -> Result<(), SnapshotError> {
        // L1 first.
        match self.l1.get(sandbox_id, target_dir, expected_sha256) {
            Ok(()) => return Ok(()),
            Err(SnapshotError::NotFound(_)) => {
                // Fall through to L2.
            }
            Err(other) => {
                // L1 had something but it was corrupt / IO-broken —
                // return that error directly. We don't fall back to
                // L2 on integrity errors because the operator wants
                // to know about the corrupt L1 artifact (don't paper
                // over with a silent re-fetch).
                return Err(other);
            }
        }
        // L2 (stub returns NotFound; real impl talks to GCS).
        // **v1**: no L1 side-effect after a successful L2 fetch; the
        // restore handler will re-snapshot at the next idle eviction
        // and that will re-populate L1. Real GCS PR adds a "restore-
        // back-fill" op so L1 caches the artifact for the next
        // restore on the same worker.
        self.l2.get(sandbox_id, target_dir, expected_sha256)
    }

    fn delete(&self, sandbox_id: &str) -> Result<(), SnapshotError> {
        // Best-effort delete from both. Operationally we want
        // delete to *succeed* whenever at least one tier succeeded;
        // a hard fail on both surfaces a real error.
        let l1_err = self.l1.delete(sandbox_id).err();
        let l2_err = self.l2.delete(sandbox_id).err();
        match (l1_err, l2_err) {
            (None, None) => Ok(()),
            (Some(e1), Some(e2)) => Err(SnapshotError::InvalidArtifact(format!(
                "tiered delete: L1 failed ({e1}); L2 failed ({e2})"
            ))),
            (Some(e), None) | (None, Some(e)) => {
                // One tier failed, the other succeeded — log and
                // succeed. Operational tolerance: the artifact is
                // gone from at least one tier, and the operator can
                // grep the warn for the survivor.
                tracing::warn!(
                    sandbox_id = %sandbox_id,
                    error = %e,
                    "tiered delete: one tier failed; treating as success"
                );
                Ok(())
            }
        }
    }

    fn verify(
        &self,
        sandbox_id: &str,
        expected_sha256: &[u8; 32],
    ) -> Result<(), SnapshotError> {
        match self.l1.verify(sandbox_id, expected_sha256) {
            Ok(()) => Ok(()),
            Err(SnapshotError::NotFound(_)) => self.l2.verify(sandbox_id, expected_sha256),
            Err(other) => Err(other),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot_store::{LocalDiskSnapshotStore, ARTIFACT_FILES};
    use std::io::Write;
    use std::path::PathBuf;
    use std::sync::Mutex;

    fn fresh_root() -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "zsbx-tiered-test-{}-{}",
            std::process::id(),
            uuid::Uuid::now_v7().simple()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn cleanup(root: &Path) {
        let _ = std::fs::remove_dir_all(root);
    }

    fn write_fake_artifact(dir: &Path) {
        std::fs::create_dir_all(dir).unwrap();
        for &name in ARTIFACT_FILES {
            let mut f = std::fs::File::create(dir.join(name)).unwrap();
            f.write_all(format!("body-{name}").as_bytes()).unwrap();
        }
    }

    /// `MockL2` records every call so tests can assert on whether
    /// L2 was consulted. Always-fails on `put` (so the background
    /// task exits via the warn log) and `get`/`verify`.
    #[derive(Default)]
    struct MockL2 {
        put_calls: Mutex<u32>,
        get_calls: Mutex<u32>,
        delete_calls: Mutex<u32>,
        verify_calls: Mutex<u32>,
        delete_succeeds: bool,
    }
    impl SnapshotStore for MockL2 {
        fn put(
            &self,
            _sandbox_id: &str,
            _source_dir: &Path,
            _ch_version: &str,
        ) -> Result<SnapshotMetadata, SnapshotError> {
            *self.put_calls.lock().unwrap() += 1;
            Err(SnapshotError::InvalidArtifact("mock L2: always fails put".into()))
        }
        fn get(
            &self,
            sandbox_id: &str,
            _target_dir: &Path,
            _expected_sha256: &[u8; 32],
        ) -> Result<(), SnapshotError> {
            *self.get_calls.lock().unwrap() += 1;
            Err(SnapshotError::NotFound(format!("mock L2: {sandbox_id}")))
        }
        fn delete(&self, _sandbox_id: &str) -> Result<(), SnapshotError> {
            *self.delete_calls.lock().unwrap() += 1;
            if self.delete_succeeds {
                Ok(())
            } else {
                Err(SnapshotError::InvalidArtifact("mock L2: delete fails".into()))
            }
        }
        fn verify(
            &self,
            sandbox_id: &str,
            _expected_sha256: &[u8; 32],
        ) -> Result<(), SnapshotError> {
            *self.verify_calls.lock().unwrap() += 1;
            Err(SnapshotError::NotFound(format!("mock L2: {sandbox_id}")))
        }
    }

    #[compio::test]
    async fn put_writes_to_l1_and_get_short_circuits_l2() {
        let root = fresh_root();
        let l1 = LocalDiskSnapshotStore::new(root.join("store"));
        let l2 = MockL2::default();
        let tier = TieredSnapshotStore::new(l1, l2);

        let src = root.join("src");
        write_fake_artifact(&src);
        let meta = tier.put("sbx_l1_hit", &src, "v51.1").unwrap();
        assert!(meta.bytes > 0);

        let target = root.join("target");
        tier.get("sbx_l1_hit", &target, &meta.sha256).unwrap();
        // L2 should NOT have been consulted on get (L1 had it).
        // (L2 *might* have been consulted on the spawned put task,
        // which is racy — assert specifically on get_calls.)
        assert_eq!(*tier.l2.get_calls.lock().unwrap(), 0, "L2 must not be consulted on L1 hit");

        cleanup(&root);
    }

    #[compio::test]
    async fn get_misses_l1_consults_l2_and_returns_not_found() {
        let root = fresh_root();
        let l1 = LocalDiskSnapshotStore::new(root.join("store"));
        let l2 = MockL2::default();
        let tier = TieredSnapshotStore::new(l1, l2);

        let target = root.join("target");
        let err = tier
            .get("sbx_unknown", &target, &[0u8; 32])
            .unwrap_err();
        assert!(matches!(err, SnapshotError::NotFound(_)), "expected NotFound, got {err:?}");
        // L2 should have been consulted exactly once.
        assert_eq!(*tier.l2.get_calls.lock().unwrap(), 1);

        cleanup(&root);
    }

    #[compio::test]
    async fn put_succeeds_even_when_l2_always_fails() {
        // Background L2 upload is fire-and-forget; put returns OK
        // on L1 success.
        let root = fresh_root();
        let l1 = LocalDiskSnapshotStore::new(root.join("store"));
        let l2 = MockL2::default();
        let tier = TieredSnapshotStore::new(l1, l2);

        let src = root.join("src");
        write_fake_artifact(&src);
        let meta = tier.put("sbx_l2_fails", &src, "v51.1").unwrap();
        assert!(meta.bytes > 0);

        cleanup(&root);
    }

    #[compio::test]
    async fn delete_returns_ok_when_one_side_fails() {
        // Operational tolerance: artifact gone from at least one
        // tier → succeed.
        let root = fresh_root();
        let l1 = LocalDiskSnapshotStore::new(root.join("store"));
        let l2 = MockL2 { delete_succeeds: false, ..Default::default() };
        let tier = TieredSnapshotStore::new(l1, l2);

        let src = root.join("src");
        write_fake_artifact(&src);
        let _ = tier.put("sbx_del_one_fails", &src, "v51.1").unwrap();

        // L1 will succeed (idempotent); L2 will error. Tier
        // returns Ok.
        tier.delete("sbx_del_one_fails").unwrap();

        cleanup(&root);
    }

    #[compio::test]
    async fn delete_returns_err_when_both_sides_fail() {
        // Construct an L1 that errors on delete by deleting the
        // root from under it. Easier path: use a non-existent root
        // and test L2-only failure. Simpler: skip — `LocalDiskSnapshotStore`
        // is idempotent on missing artifacts. Instead, use a
        // `BothFailL1` mock + `MockL2` to force the dual-failure.
        struct BothFailL1;
        impl SnapshotStore for BothFailL1 {
            fn put(&self, _: &str, _: &Path, _: &str) -> Result<SnapshotMetadata, SnapshotError> {
                unreachable!()
            }
            fn get(&self, _: &str, _: &Path, _: &[u8; 32]) -> Result<(), SnapshotError> {
                unreachable!()
            }
            fn delete(&self, _: &str) -> Result<(), SnapshotError> {
                Err(SnapshotError::InvalidArtifact("L1 fail".into()))
            }
            fn verify(&self, _: &str, _: &[u8; 32]) -> Result<(), SnapshotError> {
                unreachable!()
            }
        }

        let l2 = MockL2 { delete_succeeds: false, ..Default::default() };
        let tier = TieredSnapshotStore::new(BothFailL1, l2);
        let err = tier.delete("sbx_both_fail").unwrap_err();
        assert!(
            matches!(err, SnapshotError::InvalidArtifact(_)),
            "expected dual-failure error, got {err:?}"
        );
    }

    #[compio::test]
    async fn local_disk_l1_with_gcs_l2_stub_round_trips_via_l1() {
        // Composition test: production-shaped types.
        let root = fresh_root();
        let l1 = LocalDiskSnapshotStore::new(root.join("store"));
        let l2 = GcsSnapshotStore::new("zeroship-snapshots-test", "stub-sa");
        let tier = TieredSnapshotStore::new(l1, l2);

        let src = root.join("src");
        write_fake_artifact(&src);
        let meta = tier.put("sbx_real_compose", &src, "v51.1").unwrap();

        let target = root.join("target");
        tier.get("sbx_real_compose", &target, &meta.sha256).unwrap();
        for &name in ARTIFACT_FILES {
            assert!(target.join(name).is_file(), "{name} missing");
        }

        cleanup(&root);
    }

    #[test]
    fn gcs_object_prefix_is_versioned() {
        let g = GcsSnapshotStore::new("b", "sa");
        let p = g.object_prefix("sbx_xyz");
        assert!(p.starts_with("snapshots/v1/"));
        assert!(p.ends_with('/'));
    }
}
