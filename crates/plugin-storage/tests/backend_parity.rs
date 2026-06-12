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
        .get(APP, BUCKET, key)
        .await
        .unwrap_or_else(|e| panic!("[{label}] buffered get: {e}"))
        .unwrap_or_else(|| panic!("[{label}] buffered get returned None"));
    assert_eq!(got, body, "[{label}] buffered get bytes");
    assert_eq!(meta.size, body.len() as u64, "[{label}] buffered get meta size");

    // get of a missing key => None
    assert!(
        backend.get(APP, BUCKET, "missing.txt").await.unwrap().is_none(),
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
        .get(APP, BUCKET, skey)
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
        backend.get(APP, BUCKET, key).await.unwrap().is_none(),
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
            });
    });

    minio_cleanup();
    if let Err(e) = result {
        std::panic::resume_unwind(e);
    }
}
