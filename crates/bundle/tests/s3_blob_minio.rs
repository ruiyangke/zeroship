//! MinIO-backed integration test for `S3BlobStore` + LocalDisk↔S3 parity.
//!
//! Self-contained: starts its own MinIO container, creates a bucket, exercises
//! the `BlobStore` contract over S3 (blob round-trip, a MULTIPART-sized blob,
//! manifest round-trip, dedup, `get_blob_to_file` refill, `delete_app_manifests`),
//! then runs the SAME assertions against `LocalDiskBlobStore` for parity, and
//! tears the container down. **Skips cleanly** when Docker is unavailable.
//!
//! Run explicitly:
//!   `cargo test -p zeroship-bundle --test s3_blob_minio -- --nocapture`

#![allow(clippy::future_not_send)]

use std::process::Command;
use std::time::Duration;

use uuid::Uuid;
use zeroship_bundle::s3_blob::PART_SIZE;
use zeroship_bundle::{
    sha256_hex, BlobError, BlobStore, LocalDiskBlobStore, PutOutcome, S3BlobStore,
};

use compio_s3::{S3Client, S3Config, S3Credentials};

const ACCESS_KEY: &str = "minioadmin";
const SECRET_KEY: &str = "minioadmin";
const CONTAINER: &str = "zs-bundle-s3blob-minio-test";
const PORT: u16 = 9112;
const BUCKET: &str = "zs-blob-bucket";

fn docker_available() -> bool {
    Command::new("docker")
        .args(["info"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn cleanup() {
    let _ = Command::new("docker")
        .args(["rm", "-f", CONTAINER])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

fn start_minio() -> bool {
    cleanup();
    let run = Command::new("docker")
        .args([
            "run",
            "-d",
            "--name",
            CONTAINER,
            "-p",
            &format!("{PORT}:9000"),
            "-e",
            &format!("MINIO_ROOT_USER={ACCESS_KEY}"),
            "-e",
            &format!("MINIO_ROOT_PASSWORD={SECRET_KEY}"),
            "minio/minio",
            "server",
            "/data",
        ])
        .status();
    if !matches!(run, Ok(s) if s.success()) {
        eprintln!("skip: failed to start MinIO container");
        return false;
    }
    for _ in 0..40 {
        std::thread::sleep(Duration::from_millis(500));
        let alias = Command::new("docker")
            .args([
                "exec", CONTAINER, "mc", "alias", "set", "local",
                "http://127.0.0.1:9000", ACCESS_KEY, SECRET_KEY,
            ])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        if matches!(alias, Ok(s) if s.success()) {
            let mb = Command::new("docker")
                .args(["exec", CONTAINER, "mc", "mb", "-p", &format!("local/{BUCKET}")])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
            if matches!(mb, Ok(s) if s.success()) {
                return true;
            }
        }
    }
    eprintln!("skip: MinIO did not become ready / bucket create failed");
    cleanup();
    false
}

fn s3_url() -> String {
    format!(
        "s3://{BUCKET}/it?provider=minio&endpoint=http://127.0.0.1:{PORT}&region=us-east-1&style=path&dev_http=true"
    )
}

fn s3_store() -> S3BlobStore {
    let cfg = S3Config::parse_url(&s3_url()).expect("parse minio url");
    S3BlobStore::new(cfg, S3Credentials::new(ACCESS_KEY, SECRET_KEY, None))
}

/// A raw `S3Client` over the same MinIO bucket, for asserting low-level state
/// (e.g. that an aborted multipart leaves no orphaned upload).
fn s3_raw_client() -> S3Client {
    let cfg = S3Config::parse_url(&s3_url()).expect("parse minio url");
    S3Client::new(cfg, S3Credentials::new(ACCESS_KEY, SECRET_KEY, None))
}

/// A `Read` source that yields `before_err` bytes (in 64 KiB reads) and then
/// fails with an I/O error — modelling a stream that dies mid-upload, after the
/// multipart upload + first part(s) have been created. The C1 fix must abort
/// that multipart explicitly (awaited), leaving no orphaned upload.
struct ErrAfter {
    remaining: usize,
    errored: bool,
}

impl std::io::Read for ErrAfter {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        if self.remaining == 0 {
            if self.errored {
                return Ok(0);
            }
            self.errored = true;
            return Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                "injected mid-upload read failure",
            ));
        }
        let n = self.remaining.min(out.len()).min(64 * 1024);
        for b in &mut out[..n] {
            *b = 0xEE;
        }
        self.remaining -= n;
        Ok(n)
    }
}

fn local_store() -> (LocalDiskBlobStore, std::path::PathBuf) {
    let mut root = std::env::temp_dir();
    root.push(format!("zsbundle-localparity-{}", Uuid::new_v4().simple()));
    let s = LocalDiskBlobStore::new(root.clone()).expect("local store");
    (s, root)
}

#[test]
fn s3_blob_store_roundtrip_and_parity() {
    if !docker_available() {
        eprintln!("skip: docker unavailable");
        return;
    }
    if !start_minio() {
        return;
    }
    let result = std::panic::catch_unwind(|| {
        compio::runtime::Runtime::new()
            .expect("compio runtime")
            .block_on(async {
                // S3 leg.
                let s3 = s3_store();
                run_contract(&s3, "s3").await;
                // C1 regression: an error mid-multipart-upload must abort the
                // upload explicitly, not leak orphaned parts or abort the
                // process.
                run_c1_mid_upload_abort(&s3).await;
                // Local-disk leg — identical assertions for parity.
                let (local, root) = local_store();
                run_contract(&local, "local").await;
                std::fs::remove_dir_all(&root).ok();
            });
    });
    cleanup();
    if let Err(e) = result {
        std::panic::resume_unwind(e);
    }
}

/// C1 regression: induce an error mid-multipart-upload and assert
/// (a) `put_blob_stream` returns the error (no panic / process abort), and
/// (b) the multipart upload was aborted — no orphaned upload remains listable.
///
/// The error is injected AFTER ≥ 1 full part, so a real multipart upload + part
/// exist on the server before the failure; only an explicit awaited abort can
/// reclaim them. (Pre-fix, the abort was a detached `spawn` in `Drop`, which
/// could panic off-runtime / never be polled, leaking the upload.)
async fn run_c1_mid_upload_abort(store: &S3BlobStore) {
    // Declare a size big enough to force multipart (≥ 1 full part + more), but
    // make the reader die partway. The hash is arbitrary (we never complete).
    let declared = (PART_SIZE * 2) as u64;
    // Yield 1.25 parts of bytes, then error — guarantees create_multipart +
    // at least one upload_part have run before the failure.
    let mut reader = ErrAfter {
        remaining: PART_SIZE + PART_SIZE / 4,
        errored: false,
    };
    let fake_hash = "ee".repeat(32); // 64 hex chars; never matches real content

    // The store maps hash → logical key `blobs/<hash>`. List multipart uploads
    // by the EXACT object-key prefix (MinIO's ListMultipartUploads only
    // surfaces an upload when the prefix reaches the key, not a parent
    // "directory" prefix). Sanity-check the listing path is non-vacuous first.
    let key_prefix = format!("blobs/{fake_hash}");
    {
        let raw = s3_raw_client();
        let up = raw
            .create_multipart(&key_prefix, "application/octet-stream")
            .await
            .expect("sentinel create_multipart");
        let listed = raw
            .list_multipart_uploads(&key_prefix)
            .await
            .expect("sentinel list");
        assert!(
            !listed.is_empty(),
            "precondition: list_multipart_uploads must see an in-progress upload"
        );
        raw.abort_multipart(&key_prefix, &up).await.expect("sentinel abort");
    }

    let result = store
        .put_blob_stream(&fake_hash, declared, &mut reader)
        .await;
    assert!(result.is_err(), "C1: mid-upload error must propagate (no panic/abort)");

    // The fix's guarantee: the multipart upload created mid-stream was aborted
    // explicitly, so no orphaned (billed) upload remains.
    let raw = s3_raw_client();
    let uploads = raw
        .list_multipart_uploads(&key_prefix)
        .await
        .expect("C1: list multipart uploads");
    assert!(
        uploads.is_empty(),
        "C1: mid-upload error left an orphaned multipart upload: {uploads:?}"
    );
}

/// Drive the full `BlobStore` contract against any backend. Run identically
/// for S3 and LocalDisk to prove behavioural parity.
async fn run_contract<S: BlobStore + ?Sized>(store: &S, tag: &str) {
    // ---- small blob: put_blob → get_blob round-trip ----
    let small = b"hello content-addressed blob".to_vec();
    let small_hash = sha256_hex(&small);
    let outcome = store
        .put_blob(&small_hash, &small)
        .await
        .unwrap_or_else(|e| panic!("[{tag}] put small: {e}"));
    assert_eq!(outcome, PutOutcome::Wrote, "[{tag}] first put writes");
    assert!(store.has_blob(&small_hash).await.expect("has"), "[{tag}] has");

    let got = store.get_blob(&small_hash).await.expect("get small");
    assert_eq!(got.as_ref(), &small[..], "[{tag}] small round-trip");

    // Idempotent dedup: a second put of the same hash dedups.
    let outcome2 = store
        .put_blob(&small_hash, &small)
        .await
        .expect("put small again");
    assert_eq!(outcome2, PutOutcome::Deduped, "[{tag}] second put dedups");

    // ---- MULTIPART-sized blob: forces the streaming multipart path on S3 ----
    // PART_SIZE + a remainder so we exercise (full part)+(short last part).
    let big_len = PART_SIZE + 4096;
    let big: Vec<u8> = (0..big_len).map(|i| (i % 251) as u8).collect();
    let big_hash = sha256_hex(&big);
    let outcome = store
        .put_blob(&big_hash, &big)
        .await
        .unwrap_or_else(|e| panic!("[{tag}] put big: {e}"));
    assert_eq!(outcome, PutOutcome::Wrote, "[{tag}] big first put writes");

    let got_big = store.get_blob(&big_hash).await.expect("get big");
    assert_eq!(got_big.len(), big_len, "[{tag}] big size");
    assert_eq!(got_big.as_ref(), &big[..], "[{tag}] big round-trip");

    // ---- get_blob_to_file: streaming refill into an open temp file ----
    let mut tmp = std::env::temp_dir();
    tmp.push(format!("zsblob-refill-{tag}-{}", Uuid::new_v4().simple()));
    let file = compio::fs::OpenOptions::new()
        .write(true)
        .read(true)
        .create_new(true)
        .open(&tmp)
        .await
        .expect("open temp");
    let n = store
        .get_blob_to_file(&big_hash, &file, Some(big_len as u64), 64 * 1024 * 1024)
        .await
        .unwrap_or_else(|e| panic!("[{tag}] get_blob_to_file: {e}"));
    assert_eq!(n, big_len as u64, "[{tag}] refilled byte count");
    drop(file);
    let on_disk = std::fs::read(&tmp).expect("read refilled");
    assert_eq!(on_disk, big, "[{tag}] refilled bytes match");
    assert_eq!(sha256_hex(&on_disk), big_hash, "[{tag}] refilled hash matches");
    std::fs::remove_file(&tmp).ok();

    // A refill with the WRONG expected_size must fail (size-check before commit).
    let mut tmp2 = std::env::temp_dir();
    tmp2.push(format!("zsblob-refill-bad-{tag}-{}", Uuid::new_v4().simple()));
    let file2 = compio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp2)
        .await
        .expect("open temp2");
    let bad = store
        .get_blob_to_file(&big_hash, &file2, Some((big_len - 1) as u64), 64 * 1024 * 1024)
        .await;
    assert!(bad.is_err(), "[{tag}] wrong expected_size must fail");
    drop(file2);
    std::fs::remove_file(&tmp2).ok();

    // ---- missing blob ----
    let absent = sha256_hex(b"this-was-never-stored");
    assert!(!store.has_blob(&absent).await.expect("has absent"), "[{tag}] absent");
    assert!(
        matches!(store.get_blob(&absent).await, Err(BlobError::NotFound(_))),
        "[{tag}] get absent → NotFound"
    );

    // ---- manifest keyspace: put → get → immutable, plus app purge ----
    let app_a = Uuid::new_v4();
    let app_b = Uuid::new_v4();
    let manifest_a1 = br#"{"v":1,"app":"a","deploy":"one"}"#.to_vec();
    let manifest_a2 = br#"{"v":1,"app":"a","deploy":"two"}"#.to_vec();
    let manifest_b1 = br#"{"v":1,"app":"b","deploy":"one"}"#.to_vec();
    store.put_manifest(&app_a, "deployone", &manifest_a1).await.expect("put a1");
    store.put_manifest(&app_a, "deploytwo", &manifest_a2).await.expect("put a2");
    store.put_manifest(&app_b, "deployone", &manifest_b1).await.expect("put b1");

    let got_a1 = store.get_manifest(&app_a, "deployone").await.expect("get a1");
    assert_eq!(got_a1.as_ref(), &manifest_a1[..], "[{tag}] manifest a1 round-trip");

    // Identical replay of the same key is success (idempotent immutable write).
    store
        .put_manifest(&app_a, "deployone", &manifest_a1)
        .await
        .expect("[{tag}] identical manifest replay is ok");

    // get of a missing manifest → NotFound.
    assert!(
        matches!(store.get_manifest(&app_a, "nope").await, Err(BlobError::NotFound(_))),
        "[{tag}] missing manifest → NotFound"
    );

    // delete_app_manifests(app_a) removes a's manifests but leaves b's, and
    // does NOT touch content-addressed blobs.
    store.delete_app_manifests(&app_a).await.expect("purge a");
    assert!(
        matches!(store.get_manifest(&app_a, "deployone").await, Err(BlobError::NotFound(_))),
        "[{tag}] a1 gone after purge"
    );
    assert!(
        matches!(store.get_manifest(&app_a, "deploytwo").await, Err(BlobError::NotFound(_))),
        "[{tag}] a2 gone after purge"
    );
    let still_b = store.get_manifest(&app_b, "deployone").await.expect("b survives");
    assert_eq!(still_b.as_ref(), &manifest_b1[..], "[{tag}] b untouched by a's purge");
    // Blobs are not app-owned — both still present.
    assert!(store.has_blob(&small_hash).await.expect("blob survives purge"), "[{tag}] small blob survives");
    assert!(store.has_blob(&big_hash).await.expect("big blob survives purge"), "[{tag}] big blob survives");

    // Idempotent: purge again (now empty) is success; purge an app that never
    // had manifests is success.
    store.delete_app_manifests(&app_a).await.expect("[{tag}] repeat purge ok");
    store.delete_app_manifests(&Uuid::new_v4()).await.expect("[{tag}] purge empty app ok");
}
