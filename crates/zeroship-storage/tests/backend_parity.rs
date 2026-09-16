//! Backend-parity integration tests: `LocalFs` vs `S3` (MinIO).
//!
//! Both backends are driven through the *same* `Backend` trait sequence —
//! buffered put/get, streaming put/get, list, delete — and asserted to
//! produce identical observable results. A large streaming put → get →
//! byte-compare exercises the S3 multipart path end to end.
//!
//! `LocalFs` always runs (temp dir). The `S3` leg starts its own MinIO
//! container and **FAILS** when Docker is unavailable: skipping it would let a
//! machine without Docker report the same green as a machine that had actually
//! compared the two backends, and comparing them is the entire point of the
//! file. Nothing caps the whole-object size, so the large test really does
//! stream.
//!
//! Run explicitly:
//!   `cargo test -p zeroship-storage --features s3 --test backend_parity -- --nocapture`

#![allow(clippy::future_not_send)]

#[cfg(feature = "s3")]
#[path = "../../../tests/fixtures/s3.rs"]
mod s3_fixture;
use zeroship_storage::StorageError;
#[cfg(feature = "s3")]
use std::time::Duration;

use bytes::Bytes;
use zeroship_storage::backend::{
    Backend, BoxByteStream, ChunkResult, ChunkSource, ListPage, ListRequest, LocalFs,
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

/// Every key under `prefix`, obtained by paging to exhaustion. Used by the
/// assertions that care about *which* keys exist rather than about paging.
async fn all_keys(backend: &dyn Backend, prefix: &str, label: &str) -> Vec<String> {
    let mut keys = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let page = backend
            .list(APP, BUCKET, ListRequest { prefix, cursor: cursor.as_deref(), limit: 1000 })
            .await
            .unwrap_or_else(|e| panic!("[{label}] list {prefix:?}: {e}"));
        keys.extend(page.entries.into_iter().map(|e| e.key));
        match page.cursor {
            Some(c) => cursor = Some(c),
            None => return keys,
        }
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
    // The content type the creator set on `put` must survive the round trip
    // IDENTICALLY on every backend: an object served off the shared LocalFs
    // volume in a multi-node deployment must not lose the type that S3 keeps.
    assert_eq!(
        meta.content_type.as_deref(),
        Some("text/plain"),
        "[{label}] buffered get must round-trip the put content type"
    );

    // Same object, metadata read via the STREAMING path — `getStream` reports
    // `contentType` from this `ObjectMeta`, so it needs its own assertion.
    let (smeta, sstream) = backend
        .get_stream(APP, BUCKET, key)
        .await
        .unwrap_or_else(|e| panic!("[{label}] get_stream for content type: {e}"))
        .unwrap_or_else(|| panic!("[{label}] get_stream for content type returned None"));
    assert_eq!(
        smeta.content_type.as_deref(),
        Some("text/plain"),
        "[{label}] get_stream must round-trip the put content type"
    );
    drop(drain(sstream).await);

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

    let (sbuf, m) = backend
        .get(APP, BUCKET, skey, GET_CAP)
        .await
        .unwrap()
        .unwrap_or_else(|| panic!("[{label}] streaming object get None"));
    assert_eq!(sbuf, expect, "[{label}] streaming put round-trip bytes");
    assert_eq!(
        m.content_type.as_deref(),
        Some("application/octet-stream"),
        "[{label}] streaming put must round-trip its content type"
    );

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
    let keys = all_keys(backend, "", label).await;
    assert!(
        keys.iter().any(|k| k == key),
        "[{label}] list missing {key}: {keys:?}"
    );
    assert!(
        keys.iter().any(|k| k == skey),
        "[{label}] list missing {skey}: {keys:?}"
    );

    // prefix-scoped list
    let pref = all_keys(backend, "stream", label).await;
    assert!(
        pref.iter().all(|k| k.starts_with("stream")),
        "[{label}] prefix list leaked non-matching keys: {pref:?}"
    );
    assert!(
        pref.iter().any(|k| k == skey),
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

/// Content-type parity beyond the plain round-trip: the *absent* type, and
/// what an overwrite does to the type the previous writer set.
///
/// These three cases are where a naive "write a sidecar next to the object"
/// implementation diverges from S3 even after the basic round trip passes:
///
/// - `put(.., None)` must land the same advertised type on both backends. S3
///   has always sent `application/octet-stream` when the caller gives nothing,
///   so that is the contract `LocalFs` has to match — not `None`.
/// - Overwriting with a NEW type must replace the old one, never keep it.
/// - Overwriting with NO type must fall back to the default, never leave the
///   previous writer's type attached to the new writer's bytes. That last one
///   is the dangerous direction: bytes and type disagreeing is worse than a
///   missing type.
async fn run_content_type_parity(backend: &dyn Backend, label: &str) {
    // ---- absent content type → the octet-stream default, not None ----
    let key = "ct-default.bin";
    backend
        .put(APP, BUCKET, key, b"no type given", None)
        .await
        .unwrap_or_else(|e| panic!("[{label}] ct default put: {e}"));
    let (_b, meta) = backend
        .get(APP, BUCKET, key, GET_CAP)
        .await
        .unwrap()
        .unwrap_or_else(|| panic!("[{label}] ct default get None"));
    assert_eq!(
        meta.content_type.as_deref(),
        Some("application/octet-stream"),
        "[{label}] a put with no content type must advertise the octet-stream default"
    );

    // ---- overwrite with a different type → the NEW type wins ----
    let key = "ct-overwrite.bin";
    backend
        .put(APP, BUCKET, key, b"first", Some("text/plain"))
        .await
        .unwrap_or_else(|e| panic!("[{label}] ct overwrite put 1: {e}"));
    backend
        .put(APP, BUCKET, key, b"second", Some("application/json"))
        .await
        .unwrap_or_else(|e| panic!("[{label}] ct overwrite put 2: {e}"));
    let (body, meta) = backend
        .get(APP, BUCKET, key, GET_CAP)
        .await
        .unwrap()
        .unwrap_or_else(|| panic!("[{label}] ct overwrite get None"));
    assert_eq!(body, b"second", "[{label}] ct overwrite bytes");
    assert_eq!(
        meta.content_type.as_deref(),
        Some("application/json"),
        "[{label}] overwrite must replace the previous content type"
    );

    // ---- overwrite with NO type → the default, NOT the stale prior type ----
    // A sidecar that is written-but-never-cleared passes every assertion above
    // and fails this one, handing the new bytes the old writer's type.
    backend
        .put(APP, BUCKET, key, b"third", None)
        .await
        .unwrap_or_else(|e| panic!("[{label}] ct overwrite put 3: {e}"));
    let (body, meta) = backend
        .get(APP, BUCKET, key, GET_CAP)
        .await
        .unwrap()
        .unwrap_or_else(|| panic!("[{label}] ct clear get None"));
    assert_eq!(body, b"third", "[{label}] ct clear bytes");
    assert_eq!(
        meta.content_type.as_deref(),
        Some("application/octet-stream"),
        "[{label}] an untyped overwrite must NOT inherit the previous type"
    );

    // ---- delete → re-put with no type → still the default ----
    // Any per-object metadata a backend keeps on the side must not outlive the
    // object it describes and reattach itself to a later one.
    assert!(
        backend.delete(APP, BUCKET, key).await.unwrap(),
        "[{label}] ct delete present"
    );
    backend
        .put(APP, BUCKET, key, b"fourth", None)
        .await
        .unwrap_or_else(|e| panic!("[{label}] ct re-put after delete: {e}"));
    let (_b, meta) = backend
        .get(APP, BUCKET, key, GET_CAP)
        .await
        .unwrap()
        .unwrap_or_else(|| panic!("[{label}] ct re-put get None"));
    assert_eq!(
        meta.content_type.as_deref(),
        Some("application/octet-stream"),
        "[{label}] metadata must not survive the delete of the object it described"
    );

    // ---- listing must not surface any sidecar/companion file as an object ----
    // A metadata file stored beside the object would otherwise show up as a
    // phantom key that a creator never wrote and cannot get/delete.
    let keys = all_keys(backend, "ct-", label).await;
    let mut expected = vec!["ct-default.bin".to_string(), "ct-overwrite.bin".to_string()];
    expected.sort_unstable();
    let mut actual = keys.clone();
    actual.sort_unstable();
    assert_eq!(
        actual, expected,
        "[{label}] list surfaced unexpected keys (sidecar leaking as an object?)"
    );

    backend.delete(APP, BUCKET, "ct-default.bin").await.unwrap();
    backend.delete(APP, BUCKET, "ct-overwrite.bin").await.unwrap();
}

/// `list` pagination parity.
///
/// Both backends must honour `limit` exactly, report `cursor = Some(_)` iff
/// more keys remain, and resume from that cursor to yield every remaining key
/// exactly once in ascending key order.
///
/// Does NOT catch: cursor stability across concurrent mutation (a key written
/// between two pages may or may not appear — deliberately unspecified), nor
/// the COST of producing a page on either backend.
async fn run_list_pagination_parity(backend: &dyn Backend, label: &str) {
    const N: usize = 7;
    const LIMIT: usize = 3;
    let expected: Vec<String> = (0..N).map(|i| format!("pg/k{i}.txt")).collect();
    for k in &expected {
        backend
            .put(APP, BUCKET, k, b"x", None)
            .await
            .unwrap_or_else(|e| panic!("[{label}] pagination put {k}: {e}"));
    }

    let mut seen: Vec<String> = Vec::new();
    let mut cursor: Option<String> = None;
    let mut pages = 0usize;
    loop {
        let page = backend
            .list(APP, BUCKET, ListRequest { prefix: "pg/", cursor: cursor.as_deref(), limit: LIMIT })
            .await
            .unwrap_or_else(|e| panic!("[{label}] list page: {e}"));
        assert!(
            page.entries.len() <= LIMIT,
            "[{label}] page returned {} entries for limit {LIMIT}",
            page.entries.len()
        );
        pages += 1;
        assert!(pages <= N + 1, "[{label}] pagination did not terminate");
        seen.extend(page.entries.iter().map(|e| e.key.clone()));
        match page.cursor {
            Some(c) => cursor = Some(c),
            None => break,
        }
    }
    assert_eq!(
        pages,
        N.div_ceil(LIMIT),
        "[{label}] {N} keys at limit {LIMIT} must take {} pages",
        N.div_ceil(LIMIT)
    );
    assert_eq!(
        seen, expected,
        "[{label}] paging must yield every key exactly once, in ascending order"
    );

    // A page that exactly exhausts the listing must still say so: asking for
    // more than remains reports `cursor = None`, not a phantom next page.
    let whole = backend
        .list(APP, BUCKET, ListRequest { prefix: "pg/", cursor: None, limit: N * 2 })
        .await
        .unwrap_or_else(|e| panic!("[{label}] whole list: {e}"));
    assert_eq!(whole.entries.len(), N, "[{label}] whole listing size");
    assert!(
        whole.cursor.is_none(),
        "[{label}] a complete listing must report cursor = None"
    );

    for k in &expected {
        backend.delete(APP, BUCKET, k).await.unwrap();
    }
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

use zeroship_storage::backend::ObjectMeta;

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
        _body: zeroship_storage::backend::BoxChunkSource,
        _content_type: Option<&str>,
    ) -> Result<u64, StorageError> {
        Ok(0)
    }

    async fn get_stream(
        &self,
        _app_id: &str,
        _bucket: &str,
        _key: &str,
    ) -> Result<Option<(ObjectMeta, BoxByteStream)>, StorageError> {
        let meta = ObjectMeta {
            size: self.advertised_size,
            content_type: None,
            modified_at: SystemTime::UNIX_EPOCH,
        };
        let stream: BoxByteStream = Box::new(OneShot(Some(Bytes::from(self.body.clone()))));
        Ok(Some((meta, stream)))
    }

    async fn delete(&self, _: &str, _: &str, _: &str) -> Result<bool, StorageError> {
        Ok(false)
    }

    async fn list(&self, _: &str, _: &str, _: ListRequest<'_>) -> Result<ListPage, StorageError> {
        Ok(ListPage { entries: Vec::new(), cursor: None })
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
        assert!(err.to_string().contains("exceeds buffered-get cap"), "unexpected error: {err}");

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
        assert!(err.to_string().contains("buffered-get cap"), "unexpected error: {err}");

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
            run_content_type_parity(&backend, "localfs").await;
            run_list_pagination_parity(&backend, "localfs").await;
            run_large_stream(&backend, "localfs").await;
        });

    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// S3 (MinIO) leg — Docker-gated, self-contained container
// ---------------------------------------------------------------------------

#[cfg(feature = "s3")]
fn make_s3(minio: &s3_fixture::Minio) -> zeroship_storage::S3 {
    make_s3_tuned(minio, zeroship_storage::S3UploadTuning::DEFAULTS)
}

/// The same MinIO-backed backend with the upload knobs stated at the call
/// site. Tests that need a non-default concurrency or stream ceiling pass one
/// here; nothing plants a process-global environment variable to do it.
#[cfg(feature = "s3")]
fn make_s3_tuned(
    minio: &s3_fixture::Minio,
    tuning: zeroship_storage::S3UploadTuning,
) -> zeroship_storage::S3 {
    zeroship_storage::S3::with_tuning(minio.config("it"), minio.credentials(), tuning)
}

#[cfg(feature = "s3")]
#[test]
fn s3_parity_and_large_stream() {
    let minio = s3_fixture::Minio::start();

    let backend = make_s3(&minio);
    compio::runtime::Runtime::new()
        .expect("compio runtime")
        .block_on(async {
            run_parity(&backend, "s3").await;
            run_content_type_parity(&backend, "s3").await;
            run_list_pagination_parity(&backend, "s3").await;
            run_large_stream(&backend, "s3").await;
            // Concurrent parts must be ordered correctly at completion.
            run_s3_parallel_many_parts(&minio).await;
            // A slow producer must not starve the in-flight upload futures.
            run_s3_slow_producer_overlap(&minio).await;
            // Producer errors must abort uploads without leaving orphaned parts.
            run_s3_mid_upload_abort(&backend, &minio).await;
            run_s3_parallel_mid_upload_abort(&minio).await;
            // Size and part-count limits must also abort incomplete uploads.
            run_s3_part_limit_fast_fail(&minio).await;
        });
}

/// A raw `compio_s3::S3Client` over the same MinIO bucket, for asserting that an
/// aborted multipart leaves no orphaned upload.
#[cfg(feature = "s3")]
fn s3_raw_client(minio: &s3_fixture::Minio) -> compio_s3::S3Client {
    compio_s3::S3Client::new(minio.config("it"), minio.credentials())
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
            return Some(Err(StorageError::Stream("injected mid-upload chunk failure".to_string())));
        }
        let n = self.remaining.min(64 * 1024);
        self.remaining -= n;
        Some(Ok(Bytes::from(vec![0xEEu8; n])))
    }
}

/// A `ChunkSource` that emulates a SLOW async producer: it yields `chunk`-sized
/// buffers with a real `compio::time::sleep` delay BETWEEN chunks (a slow V8
/// `ReadableStream` / constrained uplink), until `total` bytes are produced.
///
/// The bytes are position-dependent so a misordered/duplicated/dropped part
/// fails the byte-compare, not merely a length check.
#[cfg(feature = "s3")]
struct SlowChunks {
    produced: usize,
    total: usize,
    chunk: usize,
    delay: Duration,
    first: bool,
}

#[cfg(feature = "s3")]
#[async_trait::async_trait(?Send)]
impl ChunkSource for SlowChunks {
    async fn next_chunk(&mut self) -> Option<ChunkResult> {
        if self.produced >= self.total {
            return None;
        }
        // Delay BEFORE every chunk after the first. This is the inter-chunk
        // stall the select-overlap loop must tolerate: the producer and the
        // in-flight `UploadPart`s are driven concurrently, so a stalled
        // producer cannot leave their deadlines ticking unpolled.
        if self.first {
            self.first = false;
        } else {
            compio::time::sleep(self.delay).await;
        }
        let len = self.chunk.min(self.total - self.produced);
        let mut buf = vec![0u8; len];
        for (i, b) in buf.iter_mut().enumerate() {
            *b = ((self.produced + i) % 251) as u8;
        }
        self.produced += len;
        Some(Ok(Bytes::from(buf)))
    }
}

/// A SLOW producer — real inter-chunk delays, total spanning several parts —
/// must still COMPLETE the multipart upload byte-exact. The select-overlap loop
/// drives the producer and the in-flight PUTs concurrently so a
/// slow-but-progressing source finishes.
#[cfg(feature = "s3")]
async fn run_s3_slow_producer_overlap(minio: &s3_fixture::Minio) {
    use zeroship_storage::S3UploadTuning;
    const PART_SIZE: usize = 8 * 1024 * 1024;

    // Concurrency 4 so multiple PUTs are in flight while the producer stalls.
    let backend = make_s3_tuned(minio, S3UploadTuning {
        concurrency: 4,
        ..S3UploadTuning::DEFAULTS
    });

    let key = "slow-producer.bin";
    // Fed as small chunks with a real inter-chunk delay: cumulative producer
    // stall spread across the upload. The select-overlap loop keeps the
    // in-flight PUTs advancing through every stall, so the upload completes
    // byte-exact instead of hanging / timing out a part.
    let total = PART_SIZE * 3 + 100_000;
    let chunk = 1024 * 1024;
    let src = SlowChunks {
        produced: 0,
        total,
        chunk,
        delay: Duration::from_millis(1200),
        first: true,
    };

    // Rebuild the expected bytes with the SAME position-dependent formula.
    let mut expect = vec![0u8; total];
    for (i, b) in expect.iter_mut().enumerate() {
        *b = (i % 251) as u8;
    }

    let written = backend
        .put_stream(APP, BUCKET, key, Box::new(src), None)
        .await
        .unwrap_or_else(|e| panic!("HIGH-2 slow-producer put_stream: {e}"));
    assert_eq!(written, total as u64, "HIGH-2: written size");

    let (meta, stream) = backend
        .get_stream(APP, BUCKET, key)
        .await
        .unwrap()
        .expect("HIGH-2: get_stream None");
    assert_eq!(meta.size, total as u64, "HIGH-2: get meta size");
    let got = drain(stream).await;
    assert_eq!(got.len(), expect.len(), "HIGH-2: length mismatch");
    assert!(got == expect, "HIGH-2: byte-compare mismatch (ordering/overlap?)");

    backend.delete(APP, BUCKET, key).await.unwrap();
}

/// C1 regression (plugin-storage `S3::put_stream`): a mid-upload error must
/// abort the multipart explicitly — no panic/process-abort, no orphaned upload.
#[cfg(feature = "s3")]
async fn run_s3_mid_upload_abort(backend: &zeroship_storage::S3, minio: &s3_fixture::Minio) {
    // 8 MiB part size; yield 1.25 parts then error → create_multipart + ≥1
    // upload_part have run before the failure.
    const PART_SIZE: usize = 8 * 1024 * 1024;
    let obj_key = "c1-aborted.bin";
    // Exact stored-key prefix: MinIO's ListMultipartUploads only surfaces an
    // upload when the prefix reaches the key, not a parent directory prefix.
    let key_prefix = format!("{APP}/{BUCKET}/{obj_key}");

    // Precondition: the listing path is non-vacuous (it can see a live upload).
    let raw = s3_raw_client(minio);
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
async fn run_s3_part_limit_fast_fail(minio: &s3_fixture::Minio) {
    use zeroship_storage::S3UploadTuning;
    // Cap at 12 MiB so the first full 8 MiB part is flushed (creating a real
    // multipart upload) before the running total trips the cap - exercising the
    // fast-fail AND the C1 abort of an already-started upload. The ceiling is
    // an argument to this backend, so it binds THIS upload and nothing else in
    // the process.
    let backend = make_s3_tuned(minio, S3UploadTuning {
        max_stream_bytes: 12 * 1024 * 1024,
        ..S3UploadTuning::DEFAULTS
    });

    // 16 MiB of data through a 12 MiB cap → trips after the first 8 MiB part.
    let obj_key = "h2-toobig.bin";
    let chunks: Vec<Bytes> = (0..256).map(|_| Bytes::from(vec![0x11u8; 64 * 1024])).collect();
    let res = backend
        .put_stream(APP, BUCKET, obj_key, Box::new(VecChunks::new(chunks)), None)
        .await;
    let err = res.expect_err("H2: oversized stream must fail fast");
    assert!(
        err.to_string().contains("max stream size") || err.to_string().contains("part limit"),
        "H2: unexpected error: {err}"
    );

    // The fast-fail must still abort any started multipart upload (C1 path).
    let raw = s3_raw_client(minio);
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
async fn run_s3_parallel_many_parts(minio: &s3_fixture::Minio) {
    use zeroship_storage::S3UploadTuning;
    const PART_SIZE: usize = 8 * 1024 * 1024;

    // Force 4-way concurrency explicitly so the test does not depend on the
    // default.
    let backend = make_s3_tuned(minio, S3UploadTuning {
        concurrency: 4,
        ..S3UploadTuning::DEFAULTS
    });

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
async fn run_s3_parallel_mid_upload_abort(minio: &s3_fixture::Minio) {
    use zeroship_storage::S3UploadTuning;
    const PART_SIZE: usize = 8 * 1024 * 1024;

    let backend = make_s3_tuned(minio, S3UploadTuning {
        concurrency: 4,
        ..S3UploadTuning::DEFAULTS
    });

    let obj_key = "c1-parallel-aborted.bin";
    let key_prefix = format!("{APP}/{BUCKET}/{obj_key}");

    // Precondition: the listing path can see a live upload (non-vacuous check).
    let raw = s3_raw_client(minio);
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
