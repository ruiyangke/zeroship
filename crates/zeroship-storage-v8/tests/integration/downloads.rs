//! What a streamed download hands to JavaScript, through real V8.
//!
//! A backend yields frames the size of its transport's reads; the binding
//! gathers them into `readChunk` results of at most
//! `limits::DOWNLOAD_CHUNK_BYTES`. These cases pin the round-trip count that
//! gathering buys, and each guarantee gathering must keep: the bound on one
//! chunk, a prompt end for a small object, a partial chunk from a trickling
//! backend, no reading ahead of the consumer, a cancel that stops the backend
//! and resolves the read in flight, the body stall bound, metering, and bytes
//! that reach JavaScript exactly as stored.

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, SystemTime};

use bytes::Bytes;
use zeroship_runtime::channel::CancelFlag;
use zeroship_runtime::plugin::NativePlugin;
use zeroship_runtime::{
    init_v8, EnvSnapshot, FetchOutcome, ModuleEntry, RequestCtx, Runtime, SettledFetch,
};
use zeroship_storage::backend::{
    BoxByteStream, BoxChunkSource, ChunkResult, ChunkSource, ListPage, ListRequest, ObjectMeta,
};
use zeroship_storage::{Backend, LocalFs, Namespace, StorageError, StorageStore};
use zeroship_storage_v8::limits::DOWNLOAD_CHUNK_BYTES;
use zeroship_storage_v8::StorageBinding;
use zeroship_testkit::s3::{S3Server, StalledObject};

const APP_ID: &str = "app_downloads";

/// Reads `uploads/<x-key>` with raw `readChunk` calls, recording each chunk's
/// length and a position-weighted checksum of every byte.
const READ_ALL: &str = r#"
export default {
    async fetch(request, env) {
        const key = request.headers.get("x-key");
        const raw = await env.storage.getStream("uploads", key);
        if (raw === "null") return Response.json({ missing: true });
        const handle = JSON.parse(raw);
        const lengths = [];
        let sum = 0;
        let position = 0;
        try {
            for (;;) {
                const chunk = await env.storage.readChunk(handle.streamId);
                if (chunk === undefined) break;
                lengths.push(chunk.length);
                for (let i = 0; i < chunk.length; i++) {
                    sum = (sum + chunk[i] * ((position + i) % 65521 + 1)) % 4294967291;
                }
                position += chunk.length;
            }
        } catch (e) {
            return Response.json({ lengths, error: String(e && e.message || e) });
        }
        const afterEnd = await env.storage.readChunk(handle.streamId);
        return Response.json({ size: handle.size, lengths, sum, afterEnd: afterEnd === undefined });
    },
};
"#;

fn checksum(bytes: &[u8]) -> u64 {
    bytes.iter().enumerate().fold(0u64, |sum, (i, &b)| {
        (sum + u64::from(b) * ((i as u64) % 65521 + 1)) % 4_294_967_291
    })
}

fn pattern(len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| u8::try_from((i % 251) ^ (i / 251 % 251)).expect("both terms are below 251"))
        .collect()
}

fn s3_store(server: &S3Server, prefix: &str) -> StorageStore {
    let backend = zeroship_storage::S3::with_tuning(
        compio_s3::S3Config::parse_url(&server.url(prefix)).expect("S3 fixture configuration"),
        compio_s3::S3Credentials::new(server.access_key(), server.secret_key(), None),
        zeroship_storage::S3UploadTuning::DEFAULTS,
    );
    StorageStore::from_backend(Arc::new(backend))
}

/// Run `source` once against `store` as `app_id` and return its JSON reply.
fn run(
    store: StorageStore,
    meter: Option<Arc<zeroship_metering::Meter>>,
    app_id: &str,
    source: &str,
    headers: &[(&str, &str)],
) -> serde_json::Value {
    run_then(store, meter, app_id, source, headers, || {})
}

/// [`run`], calling `before_teardown` once the reply has settled and while
/// the runtime, and every download it still holds, is alive.
fn run_then(
    store: StorageStore,
    meter: Option<Arc<zeroship_metering::Meter>>,
    app_id: &str,
    source: &str,
    headers: &[(&str, &str)],
    before_teardown: impl FnOnce(),
) -> serde_json::Value {
    let modules = vec![ModuleEntry { specifier: "index.js".into(), source: source.into() }];
    let env_vars = HashMap::from([("APP_ID".to_owned(), app_id.to_owned())]);
    let headers: Vec<(String, String)> =
        headers.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect();
    compio::runtime::Runtime::new().unwrap().block_on(async move {
        init_v8();
        let plugin: Arc<dyn NativePlugin> = Arc::new(StorageBinding::new(store, meter));
        let runtime = Runtime::builder()
            .modules(modules)
            .env_vars(env_vars)
            .plugins(vec![plugin])
            .build();
        runtime.start_pump();
        let outcome = runtime.call_fetch_handler(
            "GET",
            "http://localhost/",
            &headers,
            "",
            &EnvSnapshot::empty(),
            RequestCtx::new(CancelFlag::new()),
        );
        let (status, body) = match outcome {
            FetchOutcome::Response { status, body, .. } => (status, body),
            FetchOutcome::Pending { rx, .. } => {
                match compio::time::timeout(Duration::from_secs(30), rx.recv())
                    .await
                    .expect("the download app did not settle")
                    .expect("the download app's dispatch failed")
                {
                    SettledFetch::Response { status, body, .. } => (status, body),
                    _ => panic!("the download app settled without a response"),
                }
            }
            _ => panic!("the download app returned an unexpected outcome"),
        };
        before_teardown();
        drop(runtime);
        let body = String::from_utf8_lossy(&body).into_owned();
        assert_eq!(status, 200, "download app failed: {body}");
        serde_json::from_str(&body).unwrap_or_else(|e| panic!("{e}: {body}"))
    })
}

fn put(store: &StorageStore, key: &str, bytes: &[u8]) {
    let storage = store.namespace(Namespace::app(APP_ID).unwrap());
    compio::runtime::Runtime::new().unwrap().block_on(async {
        storage.put_stream("uploads", key, Box::new(Frames::of(bytes, 1024 * 1024)), None).await.unwrap();
    });
}

fn lengths(reply: &serde_json::Value) -> Vec<usize> {
    reply["lengths"]
        .as_array()
        .unwrap_or_else(|| panic!("no chunk lengths in {reply}"))
        .iter()
        .map(|n| usize::try_from(n.as_u64().unwrap()).unwrap())
        .collect()
}

/// An upload body that hands `bytes` over in frames of `frame` bytes.
struct Frames {
    rest: Bytes,
    frame: usize,
}

impl Frames {
    fn of(bytes: &[u8], frame: usize) -> Self {
        Self { rest: Bytes::copy_from_slice(bytes), frame }
    }
}

#[async_trait::async_trait(?Send)]
impl ChunkSource for Frames {
    async fn next_chunk(&mut self) -> Option<ChunkResult> {
        if self.rest.is_empty() {
            return None;
        }
        let take = self.frame.min(self.rest.len());
        Some(Ok(self.rest.split_to(take)))
    }
}

/// One fixture, two objects: a large one whose S3 frames must be gathered,
/// and the control, a small one that must end at once rather than wait for
/// bytes that never come.
#[test]
fn s3_downloads_reach_v8_in_bounded_chunks_and_small_objects_end_at_once() {
    let server = S3Server::start();
    let store = s3_store(&server, "gather");
    let size = 3 * DOWNLOAD_CHUNK_BYTES + 12_345;
    let object = pattern(size);
    put(&store, "large.bin", &object);
    put(&store, "small.bin", b"hello");

    let reply = run(store.clone(), None, APP_ID, READ_ALL, &[("x-key", "large.bin")]);
    let lengths_large = lengths(&reply);
    assert!(reply["error"].is_null(), "the read failed: {reply}");
    assert_eq!(lengths_large.iter().sum::<usize>(), size, "every byte arrives once: {lengths_large:?}");
    assert_eq!(reply["sum"].as_u64(), Some(checksum(&object)), "the bytes arrive in order");
    assert!(
        lengths_large.iter().all(|&n| n <= DOWNLOAD_CHUNK_BYTES),
        "no chunk may exceed the download bound of {DOWNLOAD_CHUNK_BYTES}: {lengths_large:?}"
    );
    assert!(
        lengths_large.len() <= size.div_ceil(DOWNLOAD_CHUNK_BYTES),
        "{} readChunk round trips for {size} bytes; the S3 body's frames reached V8 \
         one by one instead of being gathered",
        lengths_large.len()
    );
    assert_eq!(reply["afterEnd"], true, "a read after the end resolves EOF");

    let reply = run(store, None, APP_ID, READ_ALL, &[("x-key", "small.bin")]);
    assert!(reply["error"].is_null(), "the read failed: {reply}");
    assert_eq!(lengths(&reply), vec![5], "a small object is not held back for more bytes");
    assert_eq!(reply["sum"].as_u64(), Some(checksum(b"hello")));
    assert_eq!(reply["afterEnd"], true);
}

/// An object's bytes reach JavaScript unchanged whatever text they spell: this
/// one spells text a runtime could mistake for one of its own internal ids,
/// and the binding hands it over without reading it.
#[test]
fn object_bytes_reach_javascript_unchanged_whatever_text_they_spell() {
    let root = tempfile::tempdir().unwrap();
    let store = StorageStore::from_backend(Arc::new(LocalFs::new(root.path())));
    let object = b"__zs_native_fetch#0\n";
    put(&store, "marker.bin", object);

    let reply = run(store, None, APP_ID, READ_ALL, &[("x-key", "marker.bin")]);
    assert!(reply["error"].is_null(), "the read failed: {reply}");
    assert_eq!(lengths(&reply), vec![object.len()]);
    assert_eq!(reply["sum"].as_u64(), Some(checksum(object)));
}

#[test]
fn a_streamed_read_meters_one_op_and_the_object_bytes() {
    let root = tempfile::tempdir().unwrap();
    let store = StorageStore::from_backend(Arc::new(LocalFs::new(root.path())));
    let app = zeroship_core::AppId::mint();
    let size = 2 * DOWNLOAD_CHUNK_BYTES + 7;
    let storage = store.namespace(Namespace::app(app.as_str()).unwrap());
    compio::runtime::Runtime::new().unwrap().block_on(async {
        let body = Frames::of(&pattern(size), 1024 * 1024);
        storage.put_stream("uploads", "metered.bin", Box::new(body), None).await.unwrap();
    });

    let meter = Arc::new(zeroship_metering::Meter::new());
    let reply = run(store, Some(Arc::clone(&meter)), app.as_str(), READ_ALL, &[("x-key", "metered.bin")]);
    assert_eq!(lengths(&reply).iter().sum::<usize>(), size, "{reply}");

    let events = meter.drain();
    let value = |name: &str| {
        events
            .iter()
            .filter(|e| e.subject.app.as_ref() == Some(&app) && e.meter == name)
            .map(|e| e.value)
            .sum::<u64>()
    };
    assert_eq!(value("storage_ops"), 1, "one getStream is one op: {events:?}");
    assert_eq!(value("storage_egress_bytes"), size as u64, "egress is the object size: {events:?}");
}

#[test]
fn a_stalled_s3_body_rejects_the_read() {
    let endpoint = StalledObject::start(4096, 1024 * 1024);
    let mut config =
        compio_s3::S3Config::parse_url(&endpoint.url("stall")).expect("endpoint configuration");
    config.timeouts.body = Duration::from_millis(300);
    let backend = zeroship_storage::S3::with_tuning(
        config,
        compio_s3::S3Credentials::new("stall", "stall-secret", None),
        zeroship_storage::S3UploadTuning::DEFAULTS,
    );
    let store = StorageStore::from_backend(Arc::new(backend));

    let reply = run(store, None, APP_ID, READ_ALL, &[("x-key", "stalled.bin")]);
    assert!(endpoint.has_stalled(), "the endpoint never reached its stall: {reply}");
    let error = reply["error"].as_str().unwrap_or_else(|| panic!("the stalled read did not fail: {reply}"));
    assert!(error.contains("body read timeout"), "the stall must surface as such: {error}");
}

/// A backend serving one scripted object: `frames` frames of `frame` bytes
/// (forever when `None`), then a clean end or a failure, each frame after
/// `pace`. It counts the bytes it hands out and notes when its source drops.
#[derive(Debug)]
struct Scripted {
    frame: usize,
    frames: Option<usize>,
    fails: bool,
    pace: Duration,
    pulled: Arc<AtomicU64>,
    dropped: Arc<AtomicBool>,
}

impl Scripted {
    fn new(frame: usize, frames: Option<usize>, fails: bool, pace: Duration) -> Arc<Self> {
        Arc::new(Self {
            frame,
            frames,
            fails,
            pace,
            pulled: Arc::default(),
            dropped: Arc::default(),
        })
    }

    fn pulled(&self) -> usize {
        usize::try_from(self.pulled.load(Ordering::Relaxed)).unwrap()
    }
}

struct ScriptedSource {
    frame: Bytes,
    left: Option<usize>,
    fails: bool,
    pace: Duration,
    pulled: Arc<AtomicU64>,
    dropped: Arc<AtomicBool>,
}

impl Drop for ScriptedSource {
    fn drop(&mut self) {
        self.dropped.store(true, Ordering::Release);
    }
}

/// Pending once, then ready. A scripted frame is never instantly available,
/// so a loop over frames lets the executor run the test's own timeout.
#[derive(Default)]
struct YieldOnce(bool);

impl std::future::Future for YieldOnce {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if self.0 {
            return Poll::Ready(());
        }
        self.0 = true;
        cx.waker().wake_by_ref();
        Poll::Pending
    }
}

#[async_trait::async_trait(?Send)]
impl ChunkSource for ScriptedSource {
    async fn next_chunk(&mut self) -> Option<ChunkResult> {
        if self.pace.is_zero() {
            YieldOnce::default().await;
        } else {
            compio::time::sleep(self.pace).await;
        }
        match self.left.as_mut() {
            Some(0) if self.fails => {
                return Some(Err(StorageError::Stream("storage: scripted backend failure".into())));
            }
            Some(0) => return None,
            Some(left) => *left -= 1,
            None => {}
        }
        self.pulled.fetch_add(self.frame.len() as u64, Ordering::Relaxed);
        Some(Ok(self.frame.clone()))
    }
}

#[async_trait::async_trait(?Send)]
impl Backend for Scripted {
    async fn put_stream(
        &self,
        _: &str,
        _: &str,
        _: &str,
        _: BoxChunkSource,
        _: Option<&str>,
    ) -> Result<u64, StorageError> {
        Err(StorageError::Backend("the scripted backend is read-only".into()))
    }

    async fn get_stream(
        &self,
        _: &str,
        _: &str,
        _: &str,
    ) -> Result<Option<(ObjectMeta, BoxByteStream)>, StorageError> {
        let meta = ObjectMeta {
            size: self.frames.map_or(u64::MAX, |n| (n * self.frame) as u64),
            content_type: None,
            modified_at: SystemTime::UNIX_EPOCH,
        };
        let source = ScriptedSource {
            frame: Bytes::from(vec![7u8; self.frame]),
            left: self.frames,
            fails: self.fails,
            pace: self.pace,
            pulled: Arc::clone(&self.pulled),
            dropped: Arc::clone(&self.dropped),
        };
        Ok(Some((meta, Box::new(source))))
    }

    async fn delete(&self, _: &str, _: &str, _: &str) -> Result<bool, StorageError> {
        Ok(false)
    }

    async fn list(&self, _: &str, _: &str, _: ListRequest<'_>) -> Result<ListPage, StorageError> {
        Ok(ListPage { entries: Vec::new(), cursor: None })
    }
}

/// Opens the scripted object and reads one chunk of it.
const READ_ONE: &str = r#"
export default {
    async fetch(request, env) {
        const handle = JSON.parse(await env.storage.getStream("uploads", "scripted.bin"));
        const first = await env.storage.readChunk(handle.streamId);
        return Response.json({ lengths: [first.length] });
    },
};
"#;

#[test]
fn a_reader_that_stops_stops_the_backend() {
    // A frame size that does not divide the chunk, so a frame straddles it.
    let frame = 100 * 1024;
    let backend = Scripted::new(frame, None, false, Duration::ZERO);
    let reply = run(StorageStore::from_backend(backend.clone()), None, APP_ID, READ_ONE, &[]);
    assert_eq!(lengths(&reply), vec![DOWNLOAD_CHUNK_BYTES], "{reply}");
    let pulled = backend.pulled();
    assert!(pulled > 0, "the backend served nothing");
    assert!(
        pulled <= DOWNLOAD_CHUNK_BYTES + frame,
        "the backend handed out {pulled} bytes for one chunk the reader asked for"
    );
}

#[test]
fn a_trickling_backend_hands_javascript_a_partial_chunk() {
    // Filling a whole chunk at this pace takes seconds; the gather budget
    // ends the read long before that.
    let frame = 2048;
    let backend = Scripted::new(frame, None, false, Duration::from_millis(25));
    let reply = run(StorageStore::from_backend(backend), None, APP_ID, READ_ONE, &[]);
    let lengths = lengths(&reply);
    assert_eq!(lengths.len(), 1, "{reply}");
    assert!(lengths[0] >= 2 * frame, "the read ended before its budget: {lengths:?}");
    assert!(
        lengths[0] < DOWNLOAD_CHUNK_BYTES,
        "the read waited for a whole chunk of trickled frames: {lengths:?}"
    );
}

/// Reads `x-before` chunks, then starts one more read, cancels the download
/// while that read is in flight, and reports how the read settled.
const CANCEL_IN_FLIGHT: &str = r#"
export default {
    async fetch(request, env) {
        const before = Number(request.headers.get("x-before"));
        const handle = JSON.parse(await env.storage.getStream("uploads", "scripted.bin"));
        for (let i = 0; i < before; i++) await env.storage.readChunk(handle.streamId);
        const inFlight = env.storage.readChunk(handle.streamId);
        await env.storage.cancelStream(handle.streamId);
        let settled;
        try {
            const value = await inFlight;
            settled = value === undefined ? "eof" : "a chunk of " + value.length;
        } catch (e) {
            settled = "a rejection: " + String(e && e.message || e);
        }
        const after = await env.storage.readChunk(handle.streamId);
        return Response.json({ settled, after: after === undefined });
    },
};
"#;

/// Cancel a read in flight after `before` reads, and check that it resolves
/// EOF, that the backend source is released before the runtime is torn down,
/// and that the backend handed out no more than `max_pulled` bytes.
fn cancel_in_flight(backend: &Arc<Scripted>, before: usize, max_pulled: usize) {
    let dropped = Arc::clone(&backend.dropped);
    let released_before_teardown = Arc::new(AtomicBool::new(false));
    let witness = Arc::clone(&released_before_teardown);
    let before = before.to_string();
    let reply = run_then(
        StorageStore::from_backend(backend.clone()),
        None,
        APP_ID,
        CANCEL_IN_FLIGHT,
        &[("x-before", &before)],
        move || witness.store(dropped.load(Ordering::Acquire), Ordering::Release),
    );
    assert_eq!(reply["settled"], "eof", "the cancelled read resolves EOF: {reply}");
    assert_eq!(reply["after"], true, "a read after cancel resolves EOF: {reply}");
    assert!(
        released_before_teardown.load(Ordering::Acquire),
        "the cancelled download kept its backend source until the runtime was torn down"
    );
    let pulled = backend.pulled();
    assert!(pulled <= max_pulled, "the backend handed out {pulled} bytes, past {max_pulled}");
}

#[test]
fn cancelling_a_read_in_flight_stops_the_backend_at_its_next_frame() {
    let frame = 1024;
    let backend = Scripted::new(frame, None, false, Duration::ZERO);
    cancel_in_flight(&backend, 0, frame);
}

#[test]
fn cancelling_a_read_the_carry_would_fill_resolves_eof() {
    // One frame holds this chunk and the next, so the cancelled read pulls
    // nothing.
    let frame = 2 * DOWNLOAD_CHUNK_BYTES + 1000;
    let backend = Scripted::new(frame, None, false, Duration::ZERO);
    cancel_in_flight(&backend, 1, frame);
}

#[test]
fn cancelling_a_read_that_meets_the_end_or_a_failure_resolves_eof() {
    // The cancelled read finds the carry and then the end, or a failure,
    // without a frame to notice the cancel by.
    let frame = DOWNLOAD_CHUNK_BYTES + 500;
    for fails in [false, true] {
        let backend = Scripted::new(frame, Some(1), fails, Duration::ZERO);
        cancel_in_flight(&backend, 1, frame);
    }
}
