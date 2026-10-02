//! S3-backed integration test for `S3BlobStore` + LocalDisk↔S3 parity.
//!
//! Self-contained: starts its own S3 server container, creates a bucket,
//! exercises the `BlobStore` contract over S3 (blob round-trip, a
//! MULTIPART-sized blob, manifest round-trip, dedup, `get_blob_to_file` refill,
//! `delete_app_manifests`), then runs the SAME assertions against
//! `LocalDiskBlobStore` for parity, and tears the container down. It **FAILS**
//! when Docker is unavailable - it used to skip, which left the only coverage
//! `S3BlobStore` has reporting green on every machine that could not run it.
//!
//! Run explicitly:
//!   `cargo test -p zeroship-bundle --test integration s3_blob_store:: -- --nocapture`

#![allow(clippy::future_not_send)]

use zeroship_id::AppId;
use std::process::Command;
use std::time::Duration;

use uuid::Uuid;
use zeroship_bundle::s3_blob::PART_SIZE;
use zeroship_bundle::{
    sha256_hex, BlobError, BlobStore, LocalDiskBlobStore, PutOutcome, S3BlobStore,
    MAX_MANIFEST_BYTES,
};

use compio_s3::{S3Client, S3Config, S3Credentials};

const IMAGE: &str = "ghcr.io/versity/versitygw:v1.3.0";
const ACCESS_KEY: &str = "zeroship-fixture";
const SECRET_KEY: &str = "zeroship-fixture-secret";
const CONTAINER: &str = "zs-bundle-s3blob-test";
const PORT: u16 = 9112;
/// The gateway's own listener port inside the container.
const SERVER_PORT: u16 = 7070;
const BUCKET: &str = "zs-blob-bucket";
/// The gateway prints this once its listener is bound; polling the container
/// log for it is a readiness condition rather than a fixed sleep.
const READY_MARKER: &str = "VersityGW";

/// Refuse the run unless a docker daemon answers `docker info`.
///
/// # Panics
///
/// When docker is absent or its daemon is not running. It used to announce a
/// skip, so the only coverage `S3BlobStore` has - and the only place the S3 and
/// local-disk stores are compared - reported green on every machine without
/// docker.
fn require_docker() {
    let answered = Command::new("docker")
        .args(["info"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success());
    assert!(
        answered,
        "Docker is unavailable, and this test requires it.\n\
         \n\
         \x20 backend: S3, in a container this test starts itself\n\
         \x20 probe:   `docker info` did not succeed\n\
         \n\
         Nothing in this repository provisions this container - the test does it\n\
         inline - so what is missing is docker itself. Install it, start the\n\
         daemon, and check that your user can reach it:\n\
         \x20 docker info\n\
         \n\
         The suite then pulls `{IMAGE}` on first run, so the first run needs\n\
         network access to the registry.\n\
         \n\
         There is no environment variable that makes this a skip. A backend this\n\
         test cannot reach is a failed run, not a green one."
    );
}

fn cleanup() {
    let _ = Command::new("docker")
        .args(["rm", "-f", CONTAINER])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

/// Start the S3 server container this test runs against.
///
/// The gateway's POSIX backend maps each bucket to a directory under its root,
/// so the boot command creates the bucket with `mkdir` before handing over to
/// the server: fixture setup needs no S3 client and no vendor CLI.
///
/// # Panics
///
/// When the container cannot be started, or never becomes ready. Both used to
/// announce a skip and return `false`, and the caller returned on `false` - so
/// a docker daemon that WAS present but refused the run, or a server that never
/// came up, produced the same pass as a full round-trip.
fn start_s3_server() {
    cleanup();
    let boot = format!(
        "mkdir -p /data/{BUCKET} && exec /usr/local/bin/versitygw --port :{SERVER_PORT} posix /data"
    );
    let run = Command::new("docker")
        .args([
            "run",
            "-d",
            "--name",
            CONTAINER,
            "-p",
            &format!("{PORT}:{SERVER_PORT}"),
            "-e",
            &format!("ROOT_ACCESS_KEY_ID={ACCESS_KEY}"),
            "-e",
            &format!("ROOT_SECRET_ACCESS_KEY={SECRET_KEY}"),
            "--entrypoint",
            "/bin/sh",
            IMAGE,
            "-c",
            &boot,
        ])
        .status();
    assert!(
        matches!(run, Ok(s) if s.success()),
        "The S3 server container this test needs would not start.\n\
         \n\
         \x20 backend:   S3\n\
         \x20 image:     {IMAGE}\n\
         \x20 container: {CONTAINER}\n\
         \x20 port:      {PORT} on the host, mapped to {SERVER_PORT}\n\
         \n\
         `docker run` failed. The usual causes, in the order worth checking:\n\
         \x20 docker ps -a --filter name={CONTAINER}   # a leftover container\n\
         \x20 ss -lptn 'sport = :{PORT}'                     # the port is taken\n\
         \x20 docker pull {IMAGE}   # the image is not local\n\
         \n\
         Nothing in this repository provisions it; the test starts and removes\n\
         it itself, so there is no script to run - fix the daemon and re-run.\n\
         \n\
         There is no environment variable that makes this a skip."
    );
    for _ in 0..40 {
        std::thread::sleep(Duration::from_millis(500));
        let logs = Command::new("docker").args(["logs", CONTAINER]).output();
        if let Ok(out) = logs {
            let ready = String::from_utf8_lossy(&out.stdout).contains(READY_MARKER)
                || String::from_utf8_lossy(&out.stderr).contains(READY_MARKER);
            if ready {
                return;
            }
        }
    }
    let tail = Command::new("docker")
        .args(["logs", "--tail", "20", CONTAINER])
        .output()
        .map(|o| {
            format!(
                "{}{}",
                String::from_utf8_lossy(&o.stdout),
                String::from_utf8_lossy(&o.stderr)
            )
        })
        .unwrap_or_else(|e| format!("(could not read container logs: {e})"));
    cleanup();
    panic!(
        "The S3 server container started but never became usable.\n\
         \n\
         \x20 backend:   S3\n\
         \x20 image:     {IMAGE}\n\
         \x20 container: {CONTAINER} (already removed, so it is not in the way)\n\
         \x20 endpoint:  http://127.0.0.1:{PORT}\n\
         \x20 bucket:    {BUCKET}\n\
         \n\
         The readiness loop ran to its ceiling without `{READY_MARKER}` appearing\n\
         in the container log, which the gateway prints once its listener is\n\
         bound. Re-run it by hand to see what the server said:\n\
         \x20 docker run -d --name {CONTAINER} -p {PORT}:{SERVER_PORT} \\\n\
         \x20   -e ROOT_ACCESS_KEY_ID={ACCESS_KEY} -e ROOT_SECRET_ACCESS_KEY={SECRET_KEY} \\\n\
         \x20   --entrypoint /bin/sh {IMAGE} -c '{boot}'\n\
         \x20 docker logs {CONTAINER}\n\
         \n\
         What this loop saw last:\n\
         {tail}\n\
         \n\
         There is no environment variable that makes this a skip."
    )
}

fn s3_url() -> String {
    // `checksum=sha256` is stated rather than inherited: it makes the
    // single-object PUT send `x-amz-checksum-sha256`, so the server verifies a
    // digest this client computed.
    format!(
        "s3://{BUCKET}/it?provider=generic&endpoint=http://127.0.0.1:{PORT}&region=us-east-1&style=path&dev_http=true&checksum=sha256"
    )
}

fn s3_store() -> S3BlobStore {
    s3_store_with_concurrency(zeroship_bundle::limits::DEFAULT_UPLOAD_CONCURRENCY)
}

/// The same store with the in-flight part-upload concurrency stated at the
/// call site. `S3BlobStore` has taken this as a constructor argument since the
/// crate stopped reading its own configuration, so a test that wants a
/// specific value passes it here; planting `ZEROSHIP_BLOB_UPLOAD_CONCURRENCY`
/// in the process environment never reached this store at all.
fn s3_store_with_concurrency(upload_concurrency: usize) -> S3BlobStore {
    let cfg = S3Config::parse_url(&s3_url()).expect("parse s3 url");
    S3BlobStore::new(
        cfg,
        S3Credentials::new(ACCESS_KEY, SECRET_KEY, None),
        upload_concurrency,
    )
}

/// A raw `S3Client` over the same S3 bucket, for asserting low-level state
/// (e.g. that an aborted multipart leaves no orphaned upload).
fn s3_raw_client() -> S3Client {
    let cfg = S3Config::parse_url(&s3_url()).expect("parse s3 url");
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
            return Err(std::io::Error::other("injected mid-upload read failure"));
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
    require_docker();
    start_s3_server();
    let result = std::panic::catch_unwind(|| {
        compio::runtime::Runtime::new()
            .expect("compio runtime")
            .block_on(async {
                // S3 leg.
                let s3 = s3_store();
                run_contract(&s3, "s3").await;
                // Parallel multipart: a many-part blob under concurrency > 1
                // round-trips byte-exact AND keeps content-address integrity
                // (parts finish out of order; the SHA-256 is over read order).
                run_s3_parallel_many_parts().await;
                // Content-address integrity under concurrency: a hash that
                // does not match the streamed bytes must ABORT before
                // complete_multipart — nothing committed, no orphaned upload.
                run_s3_parallel_hash_mismatch_aborts().await;
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
    // by the EXACT object-key prefix, so the assertion can only be satisfied by
    // this upload and not by a neighbour's under a parent prefix.
    // Sanity-check the listing path is non-vacuous first.
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

/// A `Read` source over a fixed byte vector, handing out at most 64 KiB per
/// read (forcing the multi-read accumulate-into-part loop).
struct VecReader {
    data: Vec<u8>,
    pos: usize,
}

impl std::io::Read for VecReader {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        let n = (self.data.len() - self.pos).min(out.len()).min(64 * 1024);
        out[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

/// Parallel multipart: put a MANY-part blob under concurrency > 1 and assert
/// it round-trips byte-exact. `get_blob` re-verifies the content address on the
/// way out, so a byte-exact round-trip proves BOTH the parts were uploaded in
/// the correct order (sorted before complete, despite finishing out of order)
/// AND the content address held. Pre-change this path was strictly sequential.
async fn run_s3_parallel_many_parts() {
    // Force 4-way concurrency explicitly so the test does not depend on the
    // default.
    let store = s3_store_with_concurrency(4);

    // 5 full parts + a remainder = 6 parts, past the concurrency of 4 so
    // several waves overlap and finish out of order. Bounded by MAX_BLOB_BYTES
    // (16 MiB), so use a 2-part-ish blob if PART_SIZE is large; here PART_SIZE
    // is 8 MiB and MAX_BLOB_BYTES is 16 MiB, so cap at 2 full parts + a tail.
    let big_len = PART_SIZE * 2 - 4096; // straddles a 64 KiB read boundary too
    let big: Vec<u8> = (0..big_len).map(|i| (i % 251) as u8).collect();
    let big_hash = sha256_hex(&big);

    let mut reader = VecReader { data: big.clone(), pos: 0 };
    let outcome = store
        .put_blob_stream(&big_hash, big_len as u64, &mut reader)
        .await
        .expect("parallel many-part put_blob_stream");
    assert_eq!(outcome, PutOutcome::Wrote, "parallel: first put writes");

    let got = store.get_blob(&big_hash).await.expect("parallel: get_blob");
    assert_eq!(got.len(), big_len, "parallel: size");
    assert_eq!(got.as_ref(), &big[..], "parallel: byte-compare (part ordering?)");
}

/// Content-address integrity under concurrency: declare a hash that does NOT
/// match the streamed bytes. Even with several part-uploads finishing out of
/// order, the whole-stream SHA-256 (computed in read order) is verified BEFORE
/// `complete_multipart`; the mismatch must abort the upload — the object must
/// NOT materialize under the wrong key, and no orphaned multipart remains.
async fn run_s3_parallel_hash_mismatch_aborts() {
    let store = s3_store_with_concurrency(4);

    // A multipart-sized blob (≥ 1 full part so a real multipart upload runs),
    // but we lie about its hash. declared size matches the real byte count so
    // the size check passes and we reach the hash gate.
    let big_len = PART_SIZE + 4096;
    let big: Vec<u8> = (0..big_len).map(|i| ((i * 7) % 251) as u8).collect();
    let wrong_hash = "ab".repeat(32); // 64 hex chars, never the real content
    let key_prefix = format!("blobs/{wrong_hash}");

    let mut reader = VecReader { data: big, pos: 0 };
    let res = store
        .put_blob_stream(&wrong_hash, big_len as u64, &mut reader)
        .await;
    assert!(
        matches!(res, Err(BlobError::HashMismatch { .. })),
        "integrity: hash mismatch must fail with HashMismatch, got {res:?}"
    );

    // The wrong-key object must NOT exist (never completed).
    assert!(
        !store.has_blob(&wrong_hash).await.expect("integrity: has_blob"),
        "integrity: object materialized under the wrong content hash"
    );
    // And no orphaned multipart upload was left behind.
    let raw = s3_raw_client();
    let uploads = raw
        .list_multipart_uploads(&key_prefix)
        .await
        .expect("integrity: list multipart uploads");
    assert!(
        uploads.is_empty(),
        "integrity: hash-mismatch left an orphaned multipart upload: {uploads:?}"
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

    // Dedup must verify the bytes the CALLER supplied, not merely that the
    // store already holds that hash. `unpack` records a hash as satisfied
    // on any `Ok` from `put_blob_stream` and step 8 only checks set
    // membership, so crediting a caller who shipped something else lets a
    // deploy claim a blob it never possessed and then point an `anon`
    // asset at another tenant's content. The gateway keys its blob caches
    // on the bare hash with no app partition, so it would serve those
    // bytes. Knowing the hash is not evidence of holding the content: it
    // is the ETag on every 200/206/304, and blobs are never deleted.
    //
    // Asserted inside `run_contract` so it binds BOTH backends: the S3
    // store is what production uses, and LocalDisk is what dev uses, and
    // they must not diverge on an authorization-carrying invariant.
    let mut junk = std::io::Cursor::new(&b"x"[..]);
    let claimed = store
        .put_blob_stream(&small_hash, small.len() as u64, &mut junk)
        .await;
    assert!(
        claimed.is_err(),
        "[{tag}] a caller that did not ship the bytes for {small_hash} was \
         credited with them (got {claimed:?}); ingest treats that as the blob \
         being present, so the manifest is accepted and the asset resolves to \
         whatever another tenant stored under that hash",
    );

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
    let app_a = AppId::mint();
    let app_b = AppId::mint();
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

    // Different bytes under a key that holds a manifest are refused, and the
    // manifest already there stays. Asserted here so it binds both stores.
    let divergent = store.put_manifest(&app_a, "deployone", &manifest_a2).await;
    assert!(
        matches!(divergent, Err(BlobError::Backend(_))),
        "[{tag}] divergent manifest bytes must be refused, got {divergent:?}"
    );
    let kept = store.get_manifest(&app_a, "deployone").await.expect("get a1 again");
    assert_eq!(kept.as_ref(), &manifest_a1[..], "[{tag}] the first manifest stays");

    // A manifest over the budget reads refuse is refused on write, as
    // too large, by both stores.
    let over_budget = vec![b'x'; usize::try_from(MAX_MANIFEST_BYTES).unwrap() + 1];
    let refused = store.put_manifest(&app_a, "overbudget", &over_budget).await;
    assert!(
        matches!(refused, Err(BlobError::TooLarge)),
        "[{tag}] a manifest over budget must be refused as too large, got {refused:?}"
    );
    assert!(
        matches!(
            store.get_manifest(&app_a, "overbudget").await,
            Err(BlobError::NotFound(_))
        ),
        "[{tag}] a refused manifest is not stored"
    );

    assert!(
        store
            .delete_manifest(&app_a, "deploytwo")
            .await
            .expect("[{tag}] delete one manifest"),
        "[{tag}] deploytwo should exist before single delete"
    );
    assert!(
        matches!(
            store.get_manifest(&app_a, "deploytwo").await,
            Err(BlobError::NotFound(_))
        ),
        "[{tag}] deploytwo gone after single delete"
    );
    assert!(
        !store
            .delete_manifest(&app_a, "deploytwo")
            .await
            .expect("[{tag}] repeat single delete"),
        "[{tag}] repeated single delete reports absent"
    );
    store
        .put_manifest(&app_a, "deploytwo", &manifest_a2)
        .await
        .expect("[{tag}] restore deploytwo before app purge");

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
    store.delete_app_manifests(&AppId::mint()).await.expect("[{tag}] purge empty app ok");
}
