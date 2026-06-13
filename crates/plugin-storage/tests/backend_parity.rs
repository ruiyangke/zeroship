//! Backend-parity integration tests: `LocalFs` vs `S3` (MinIO).
//!
//! Both backends are driven through the *same* `Backend` trait sequence —
//! buffered put/get, streaming put/get, list, delete — and asserted to
//! produce identical observable results. A large (> part-size) streaming
//! put → get → byte-compare exercises the S3 multipart path end to end.
//!
//! `LocalFs` always runs (temp dir). The `S3` leg starts its own MinIO
//! container and **skips cleanly** when Docker is unavailable, so CI / dev
//! machines without Docker stay green. The whole-object size caps were
//! removed as a buffering limit, so the large test really does stream.
//!
//! Run explicitly:
//!   `cargo test -p zeroship-plugin-storage --features s3 --test backend_parity -- --nocapture`

#![allow(clippy::future_not_send)]

use std::process::Command;
use std::time::Duration;

use bytes::Bytes;
use zeroship_plugin_storage::backend::{
    Backend, BoxByteStream, ChunkResult, ChunkSource, LocalFs,
};

const APP: &str = "app_test";
const BUCKET: &str = "uploads";

/// Generous buffered-get cap for the happy-path parity calls (well above any
/// object they read). The C2 cap behaviour is exercised separately below.
const GET_CAP: u64 = 64 * 1024 * 1024;

// ---------------------------------------------------------------------------
// Test chunk source / sink helpers
// ---------------------------------------------------------------------------

/// A [`ChunkSource`] that yields a fixed list of chunks once. Lets the tests
/// drive `put_stream` with a real multi-chunk stream (the streaming path),
/// not a single buffer.
struct VecChunks(std::collections::VecDeque<Bytes>);

impl VecChunks {
    fn new(chunks: Vec<Bytes>) -> Self {
        Self(chunks.into())
    }
}

#[async_trait::async_trait(?Send)]
impl ChunkSource for VecChunks {
    async fn next_chunk(&mut self) -> Option<ChunkResult> {
        self.0.pop_front().map(Ok)
    }
}

/// Drain a `BoxByteStream` into one contiguous buffer.
async fn drain(mut s: BoxByteStream) -> Vec<u8> {
    let mut out = Vec::new();
    while let Some(chunk) = s.next_chunk().await {
        out.extend_from_slice(&chunk.expect("stream chunk"));
    }
    out
}

// ---------------------------------------------------------------------------
// The shared parity sequence — runs against any Backend
// ---------------------------------------------------------------------------

async fn run_parity(backend: &dyn Backend, label: &str) {
    // ---- buffered put / get ----
    let key = "doc.txt";
    let body = b"hello, parity world";
    let n = backend
        .put(APP, BUCKET, key, body, Some("text/plain"))
        .await
        .unwrap_or_else(|e| panic!("[{label}] buffered put: {e}"));
    assert_eq!(n, body.len() as u64, "[{label}] buffered put size");

    let (got, meta) = backend
        .get(APP, BUCKET, key, GET_CAP)
        .await
        .unwrap_or_else(|e| panic!("[{label}] buffered get: {e}"))
        .unwrap_or_else(|| panic!("[{label}] buffered get returned None"));
    assert_eq!(got, body, "[{label}] buffered get bytes");
    assert_eq!(meta.size, body.len() as u64, "[{label}] buffered get meta size");

    // get of a missing key => None
    assert!(
        backend.get(APP, BUCKET, "missing.txt", GET_CAP).await.unwrap().is_none(),
        "[{label}] missing get must be None"
    );

    // ---- streaming put (multi-chunk) → buffered get ----
    let skey = "stream.bin";
    let chunks = vec![
        Bytes::from_static(b"abcdef"),
        Bytes::from_static(b"ghijkl"),
        Bytes::from_static(b"mnop"),
    ];
    let expect: Vec<u8> = chunks.iter().flat_map(|c| c.to_vec()).collect();
    let written = backend
        .put_stream(
            APP,
            BUCKET,
            skey,
            Box::new(VecChunks::new(chunks)),
            Some("application/octet-stream"),
        )
        .await
        .unwrap_or_else(|e| panic!("[{label}] streaming put: {e}"));
    assert_eq!(written, expect.len() as u64, "[{label}] streaming put size");

    let (sbuf, _m) = backend
        .get(APP, BUCKET, skey, GET_CAP)
        .await
        .unwrap()
        .unwrap_or_else(|| panic!("[{label}] streaming object get None"));
    assert_eq!(sbuf, expect, "[{label}] streaming put round-trip bytes");

    // ---- streaming get path ----
    let (_meta, stream) = backend
        .get_stream(APP, BUCKET, skey)
        .await
        .unwrap()
        .unwrap_or_else(|| panic!("[{label}] get_stream None"));
    assert_eq!(drain(stream).await, expect, "[{label}] get_stream bytes");

    // get_stream of missing => None
    assert!(
        backend.get_stream(APP, BUCKET, "nope.bin").await.unwrap().is_none(),
        "[{label}] missing get_stream must be None"
    );

    // ---- list ----
    let entries = backend.list(APP, BUCKET, "").await.unwrap();
    let keys: Vec<&str> = entries.iter().map(|e| e.key.as_str()).collect();
    assert!(keys.contains(&key), "[{label}] list missing {key}: {keys:?}");
    assert!(keys.contains(&skey), "[{label}] list missing {skey}: {keys:?}");

    // prefix-scoped list
    let pref = backend.list(APP, BUCKET, "stream").await.unwrap();
    assert!(
        pref.iter().all(|e| e.key.starts_with("stream")),
        "[{label}] prefix list leaked non-matching keys: {pref:?}"
    );
    assert!(
        pref.iter().any(|e| e.key == skey),
        "[{label}] prefix list missing {skey}"
    );

    // ---- delete ----
    assert!(backend.delete(APP, BUCKET, key).await.unwrap(), "[{label}] delete present");
    assert!(
        !backend.delete(APP, BUCKET, key).await.unwrap(),
        "[{label}] delete absent must be false"
    );
    assert!(
        backend.get(APP, BUCKET, key, GET_CAP).await.unwrap().is_none(),
        "[{label}] object lingered after delete"
    );

    // cleanup the stream object too
    backend.delete(APP, BUCKET, skey).await.unwrap();
}

/// Large streaming round-trip: put a > part-size object as many chunks, get
/// it back as a stream, byte-compare. Exercises S3 multipart (several full
/// parts + a short final part) with bounded memory.
async fn run_large_stream(backend: &dyn Backend, label: &str) {
    // Mirror the S3 backend's multipart part size (8 MiB) so the large
    // object straddles several parts on the S3 leg. Kept as a local const
    // (not imported from `backend::s3`) so the LocalFs leg builds without
    // the `s3` feature.
    const PART_SIZE: usize = 8 * 1024 * 1024;

    let key = "large.bin";
    // 2.5 parts: forces ≥ 2 full UploadParts plus a short last part on S3.
    let total = PART_SIZE * 5 / 2;
    // Deterministic, position-dependent bytes so a misordered/short part is
    // caught by the byte-compare, not just a length check.
    let chunk_len = 700 * 1024; // not a divisor of PART_SIZE — straddles parts
    let mut chunks = Vec::new();
    let mut produced = 0usize;
    let mut seed = 0u8;
    while produced < total {
        let len = chunk_len.min(total - produced);
        let mut buf = vec![0u8; len];
        for (i, b) in buf.iter_mut().enumerate() {
            *b = seed.wrapping_add((i % 251) as u8);
        }
        chunks.push(Bytes::from(buf));
        produced += len;
        seed = seed.wrapping_add(7);
    }
    let expect: Vec<u8> = chunks.iter().flat_map(|c| c.to_vec()).collect();
    assert_eq!(expect.len(), total);

    let written = backend
        .put_stream(APP, BUCKET, key, Box::new(VecChunks::new(chunks)), None)
        .await
        .unwrap_or_else(|e| panic!("[{label}] large put_stream: {e}"));
    assert_eq!(written, total as u64, "[{label}] large put size");

    let (meta, stream) = backend
        .get_stream(APP, BUCKET, key)
        .await
        .unwrap()
        .unwrap_or_else(|| panic!("[{label}] large get_stream None"));
    assert_eq!(meta.size, total as u64, "[{label}] large get meta size");
    let got = drain(stream).await;
    assert_eq!(got.len(), expect.len(), "[{label}] large length mismatch");
    assert!(got == expect, "[{label}] large byte-compare mismatch");

    backend.delete(APP, BUCKET, key).await.unwrap();
}

// ---------------------------------------------------------------------------
// C2 regression: buffered `get` must cap allocation by `max_bytes`.
//
// A backend that advertises a huge `Content-Length` (`meta.size`) must NOT
// drive `Vec::with_capacity(meta.size)` — the buffered `get` rejects it as a
// `TooLarge`-style error before allocating. A second backend whose advertised
// size is small but whose body streams past the cap must also be rejected
// (running-total guard). The streaming `get_stream` path is unaffected.
// ---------------------------------------------------------------------------

use std::time::SystemTime;

use zeroship_plugin_storage::backend::{ListEntry, ObjectMeta};

/// A fake backend whose `get_stream` reports a chosen `advertised_size` but
/// only ever yields `body` bytes. Lets the C2 test assert both the
/// pre-allocation check (advertised size) and the running-total check.
#[derive(Debug)]
struct LyingSizeBackend {
    advertised_size: u64,
    body: Vec<u8>,
}

struct OneShot(Option<Bytes>);

#[async_trait::async_trait(?Send)]
impl ChunkSource for OneShot {
    async fn next_chunk(&mut self) -> Option<ChunkResult> {
        self.0.take().map(Ok)
    }
}

#[async_trait::async_trait(?Send)]
impl Backend for LyingSizeBackend {
    async fn put_stream(
        &self,
        _app_id: &str,
        _bucket: &str,
        _key: &str,
        _body: zeroship_plugin_storage::backend::BoxChunkSource,
        _content_type: Option<&str>,
    ) -> Result<u64, String> {
        Ok(0)
    }

    async fn get_stream(
        &self,
        _app_id: &str,
        _bucket: &str,
        _key: &str,
    ) -> Result<Option<(ObjectMeta, BoxByteStream)>, String> {
        let meta = ObjectMeta {
            size: self.advertised_size,
            content_type: None,
            modified_at: SystemTime::UNIX_EPOCH,
        };
        let stream: BoxByteStream = Box::new(OneShot(Some(Bytes::from(self.body.clone()))));
        Ok(Some((meta, stream)))
    }

    async fn delete(&self, _: &str, _: &str, _: &str) -> Result<bool, String> {
        Ok(false)
    }

    async fn list(&self, _: &str, _: &str, _: &str) -> Result<Vec<ListEntry>, String> {
        Ok(vec![])
    }
}

#[test]
fn buffered_get_rejects_oversized_content_length() {
    compio::runtime::Runtime::new().expect("compio runtime").block_on(async {
        // (a) Advertised size far above the cap → reject before allocating.
        let huge = LyingSizeBackend {
            advertised_size: 8 * 1024 * 1024 * 1024, // 8 GiB advertised
            body: vec![0u8; 16],
        };
        let cap = 1024u64;
        let err = huge
            .get(APP, BUCKET, "k", cap)
            .await
            .expect_err("oversized Content-Length must be rejected, not buffered");
        assert!(err.contains("exceeds buffered-get cap"), "unexpected error: {err}");

        // (b) Advertised size lies small but the body streams past the cap →
        // the running-total guard rejects it.
        let liar = LyingSizeBackend {
            advertised_size: 4,
            body: vec![0u8; 4096],
        };
        let err = liar
            .get(APP, BUCKET, "k", cap)
            .await
            .expect_err("body exceeding the cap must be rejected mid-stream");
        assert!(err.contains("buffered-get cap"), "unexpected error: {err}");

        // Under-cap object still succeeds.
        let ok = LyingSizeBackend { advertised_size: 5, body: b"hello".to_vec() };
        let (buf, meta) = ok.get(APP, BUCKET, "k", cap).await.unwrap().unwrap();
        assert_eq!(buf, b"hello");
        assert_eq!(meta.size, 5);
    });
}

// ---------------------------------------------------------------------------
// LocalFs leg — always runs
// ---------------------------------------------------------------------------

#[test]
fn localfs_parity_and_large_stream() {
    let dir = std::env::temp_dir().join(format!("zs-storage-parity-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let backend = LocalFs::new(&dir);

    compio::runtime::Runtime::new()
        .expect("compio runtime")
        .block_on(async {
            run_parity(&backend, "localfs").await;
            run_large_stream(&backend, "localfs").await;
        });

    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// S3 (MinIO) leg — Docker-gated, self-contained container
// ---------------------------------------------------------------------------

const MINIO_ACCESS: &str = "minioadmin";
const MINIO_SECRET: &str = "minioadmin";
const MINIO_CONTAINER: &str = "zs-plugin-storage-minio-test";
const MINIO_PORT: u16 = 9113;
const MINIO_BUCKET: &str = "zs-storage-test";

fn docker_available() -> bool {
    Command::new("docker")
        .args(["info"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn minio_cleanup() {
    let _ = Command::new("docker")
        .args(["rm", "-f", MINIO_CONTAINER])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

fn start_minio() -> bool {
    minio_cleanup();
    let run = Command::new("docker")
        .args([
            "run",
            "-d",
            "--name",
            MINIO_CONTAINER,
            "-p",
            &format!("{MINIO_PORT}:9000"),
            "-e",
            &format!("MINIO_ROOT_USER={MINIO_ACCESS}"),
            "-e",
            &format!("MINIO_ROOT_PASSWORD={MINIO_SECRET}"),
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
                "exec",
                MINIO_CONTAINER,
                "mc",
                "alias",
                "set",
                "local",
                "http://127.0.0.1:9000",
                MINIO_ACCESS,
                MINIO_SECRET,
            ])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        if matches!(alias, Ok(s) if s.success()) {
            let mb = Command::new("docker")
                .args(["exec", MINIO_CONTAINER, "mc", "mb", "-p", &format!("local/{MINIO_BUCKET}")])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
            if matches!(mb, Ok(s) if s.success()) {
                return true;
            }
        }
    }
    eprintln!("skip: MinIO did not become ready / bucket create failed");
    minio_cleanup();
    false
}

#[cfg(feature = "s3")]
fn make_s3() -> zeroship_plugin_storage::S3 {
    use compio_s3::{S3Config, S3Credentials};
    let url = format!(
        "s3://{MINIO_BUCKET}/it?provider=minio&endpoint=http://127.0.0.1:{MINIO_PORT}&region=us-east-1&style=path&dev_http=true&checksum=none"
    );
    let cfg = S3Config::parse_url(&url).expect("parse minio url");
    zeroship_plugin_storage::S3::new(cfg, S3Credentials::new(MINIO_ACCESS, MINIO_SECRET, None))
}

#[cfg(feature = "s3")]
#[test]
fn s3_parity_and_large_stream() {
    if !docker_available() {
        eprintln!("skip: docker unavailable");
        return;
    }
    if !start_minio() {
        return;
    }

    let result = std::panic::catch_unwind(|| {
        let backend = make_s3();
        compio::runtime::Runtime::new()
            .expect("compio runtime")
            .block_on(async {
                run_parity(&backend, "s3").await;
                run_large_stream(&backend, "s3").await;
                // Parallel multipart: a many-part object with concurrency > 1
                // round-trips byte-exact (parts sorted by number before
                // complete, despite finishing out of order).
                run_s3_parallel_many_parts().await;
                // C1: an error mid-multipart-upload must explicitly abort the
                // upload (no orphaned parts, no process abort).
                run_s3_mid_upload_abort(&backend).await;
                // C1 under concurrency: an injected error with N part-uploads
                // in flight must still abort — no orphaned multipart upload.
                run_s3_parallel_mid_upload_abort().await;
                // H2: a stream over the part/size limit fails fast + aborts.
                run_s3_part_limit_fast_fail().await;
            });
    });

    minio_cleanup();
    if let Err(e) = result {
        std::panic::resume_unwind(e);
    }
}

/// A raw `compio_s3::S3Client` over the same MinIO bucket, for asserting that an
/// aborted multipart leaves no orphaned upload.
#[cfg(feature = "s3")]
fn s3_raw_client() -> compio_s3::S3Client {
    use compio_s3::{S3Config, S3Credentials};
    let url = format!(
        "s3://{MINIO_BUCKET}/it?provider=minio&endpoint=http://127.0.0.1:{MINIO_PORT}&region=us-east-1&style=path&dev_http=true&checksum=none"
    );
    let cfg = S3Config::parse_url(&url).expect("parse minio url");
    compio_s3::S3Client::new(cfg, S3Credentials::new(MINIO_ACCESS, MINIO_SECRET, None))
}

/// A `ChunkSource` that yields `before_err` bytes (in 64 KiB chunks) and then
/// returns an error — a stream that dies mid-upload after the multipart upload
/// + first part(s) exist.
#[cfg(feature = "s3")]
struct ErrAfterChunks {
    remaining: usize,
    errored: bool,
}

#[cfg(feature = "s3")]
#[async_trait::async_trait(?Send)]
impl ChunkSource for ErrAfterChunks {
    async fn next_chunk(&mut self) -> Option<ChunkResult> {
        if self.remaining == 0 {
            if self.errored {
                return None;
            }
            self.errored = true;
            return Some(Err("injected mid-upload chunk failure".to_string()));
        }
        let n = self.remaining.min(64 * 1024);
        self.remaining -= n;
        Some(Ok(Bytes::from(vec![0xEEu8; n])))
    }
}

/// C1 regression (plugin-storage `S3::put_stream`): a mid-upload error must
/// abort the multipart explicitly — no panic/process-abort, no orphaned upload.
#[cfg(feature = "s3")]
async fn run_s3_mid_upload_abort(backend: &zeroship_plugin_storage::S3) {
    // 8 MiB part size; yield 1.25 parts then error → create_multipart + ≥1
    // upload_part have run before the failure.
    const PART_SIZE: usize = 8 * 1024 * 1024;
    let obj_key = "c1-aborted.bin";
    // Exact stored-key prefix: MinIO's ListMultipartUploads only surfaces an
    // upload when the prefix reaches the key, not a parent directory prefix.
    let key_prefix = format!("{APP}/{BUCKET}/{obj_key}");

    // Precondition: the listing path is non-vacuous (it can see a live upload).
    let raw = s3_raw_client();
    {
        let up = raw
            .create_multipart(&key_prefix, "application/octet-stream")
            .await
            .expect("sentinel create_multipart");
        let listed = raw.list_multipart_uploads(&key_prefix).await.expect("sentinel list");
        assert!(!listed.is_empty(), "precondition: list must see in-progress upload");
        raw.abort_multipart(&key_prefix, &up).await.expect("sentinel abort");
    }

    let src = ErrAfterChunks { remaining: PART_SIZE + PART_SIZE / 4, errored: false };
    let res = backend
        .put_stream(APP, BUCKET, obj_key, Box::new(src), None)
        .await;
    assert!(res.is_err(), "C1: mid-upload error must propagate (no panic/abort)");

    let uploads = raw
        .list_multipart_uploads(&key_prefix)
        .await
        .expect("C1: list multipart uploads");
    assert!(
        uploads.is_empty(),
        "C1: mid-upload error left an orphaned multipart upload: {uploads:?}"
    );
}

/// H2 regression: a stream that would exceed the configured max object size
/// fails fast (and the C1-style abort leaves no orphaned upload).
#[cfg(feature = "s3")]
async fn run_s3_part_limit_fast_fail() {
    use zeroship_plugin_storage::limits::MAX_STREAM_OBJECT_BYTES_ENV;
    // Cap at 12 MiB so the first full 8 MiB part is flushed (creating a real
    // multipart upload) before the running total trips the cap — exercising the
    // fast-fail AND the C1 abort of an already-started upload.
    // SAFETY: single-threaded test; restored immediately after the call.
    std::env::set_var(MAX_STREAM_OBJECT_BYTES_ENV, &(12 * 1024 * 1024).to_string());
    let backend = make_s3();

    // 16 MiB of data through a 12 MiB cap → trips after the first 8 MiB part.
    let obj_key = "h2-toobig.bin";
    let chunks: Vec<Bytes> = (0..256).map(|_| Bytes::from(vec![0x11u8; 64 * 1024])).collect();
    let res = backend
        .put_stream(APP, BUCKET, obj_key, Box::new(VecChunks::new(chunks)), None)
        .await;
    std::env::remove_var(MAX_STREAM_OBJECT_BYTES_ENV);
    let err = res.expect_err("H2: oversized stream must fail fast");
    assert!(
        err.contains("max stream size") || err.contains("part limit"),
        "H2: unexpected error: {err}"
    );

    // The fast-fail must still abort any started multipart upload (C1 path).
    let raw = s3_raw_client();
    let key_prefix = format!("{APP}/{BUCKET}/{obj_key}");
    let uploads = raw
        .list_multipart_uploads(&key_prefix)
        .await
        .expect("H2: list multipart uploads");
    assert!(
        uploads.is_empty(),
        "H2: fast-failed upload left orphaned multipart(s): {uploads:?}"
    );
}
/// Parallel multipart: drive `put_stream` with MANY full parts under a
/// concurrency > 1 and assert the object round-trips byte-exact. The parts
/// finish out of completion order, so this proves the new code sorts the
/// `(part_number, ETag)` list ascending before `complete_multipart` — a
/// mis-sorted or duplicated list makes `complete_multipart` reject the upload.
/// (Pre-change this path was strictly sequential, so the sort line is new.)
#[cfg(feature = "s3")]
async fn run_s3_parallel_many_parts() {
    use zeroship_plugin_storage::limits::UPLOAD_CONCURRENCY_ENV;
    const PART_SIZE: usize = 8 * 1024 * 1024;

    // Force 4-way concurrency explicitly so the test does not depend on the
    // default. SAFETY: single-threaded test; restored after the call.
    std::env::set_var(UPLOAD_CONCURRENCY_ENV, "4");
    let backend = make_s3();

    let key = "parallel-many.bin";
    // 6 full parts + a short last part = 7 parts, well past the concurrency of
    // 4 so several waves of in-flight uploads overlap and finish out of order.
    let total = PART_SIZE * 6 + 123_456;
    // Deterministic, position-dependent bytes: any misordered/duplicated part
    // fails the byte-compare, not just a length check.
    let chunk_len = 1_000_000; // not a divisor of PART_SIZE → straddles parts
    let mut chunks = Vec::new();
    let mut produced = 0usize;
    while produced < total {
        let len = chunk_len.min(total - produced);
        let mut buf = vec![0u8; len];
        for (i, b) in buf.iter_mut().enumerate() {
            *b = ((produced + i) % 251) as u8;
        }
        chunks.push(Bytes::from(buf));
        produced += len;
    }
    let expect: Vec<u8> = chunks.iter().flat_map(|c| c.to_vec()).collect();
    assert_eq!(expect.len(), total);

    let written = backend
        .put_stream(APP, BUCKET, key, Box::new(VecChunks::new(chunks)), None)
        .await
        .unwrap_or_else(|e| panic!("parallel many-part put_stream: {e}"));
    std::env::remove_var(UPLOAD_CONCURRENCY_ENV);
    assert_eq!(written, total as u64, "parallel: written size");

    let (meta, stream) = backend
        .get_stream(APP, BUCKET, key)
        .await
        .unwrap()
        .expect("parallel: get_stream None");
    assert_eq!(meta.size, total as u64, "parallel: get meta size");
    let got = drain(stream).await;
    assert_eq!(got.len(), expect.len(), "parallel: length mismatch");
    assert!(got == expect, "parallel: byte-compare mismatch (part ordering?)");

    backend.delete(APP, BUCKET, key).await.unwrap();
}

/// C1 under concurrency: with several part-uploads in flight (concurrency = 4),
/// an injected reader error must still abort the multipart explicitly — the
/// other in-flight uploads are dropped/cancelled and no orphaned (billed)
/// multipart upload remains listable.
#[cfg(feature = "s3")]
async fn run_s3_parallel_mid_upload_abort() {
    use zeroship_plugin_storage::limits::UPLOAD_CONCURRENCY_ENV;
    const PART_SIZE: usize = 8 * 1024 * 1024;

    std::env::set_var(UPLOAD_CONCURRENCY_ENV, "4");
    let backend = make_s3();

    let obj_key = "c1-parallel-aborted.bin";
    let key_prefix = format!("{APP}/{BUCKET}/{obj_key}");

    // Precondition: the listing path can see a live upload (non-vacuous check).
    let raw = s3_raw_client();
    {
        let up = raw
            .create_multipart(&key_prefix, "application/octet-stream")
            .await
            .expect("sentinel create_multipart");
        let listed = raw.list_multipart_uploads(&key_prefix).await.expect("sentinel list");
        assert!(!listed.is_empty(), "precondition: list must see in-progress upload");
        raw.abort_multipart(&key_prefix, &up).await.expect("sentinel abort");
    }

    // Yield ~4.25 parts of bytes, then error → create_multipart + several
    // upload_part futures are in flight at the failure point.
    let src = ErrAfterChunks {
        remaining: PART_SIZE * 4 + PART_SIZE / 4,
        errored: false,
    };
    let res = backend
        .put_stream(APP, BUCKET, obj_key, Box::new(src), None)
        .await;
    std::env::remove_var(UPLOAD_CONCURRENCY_ENV);
    assert!(res.is_err(), "C1/parallel: mid-upload error must propagate");

    let uploads = raw
        .list_multipart_uploads(&key_prefix)
        .await
        .expect("C1/parallel: list multipart uploads");
    assert!(
        uploads.is_empty(),
        "C1/parallel: mid-upload error left an orphaned multipart upload: {uploads:?}"
    );
}
