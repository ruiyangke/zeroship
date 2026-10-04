//! Download throughput of one `env.storage` stream over the S3 fixture.
//!
//! The object is uploaded once through the Rust storage handle, then read
//! back three ways:
//!
//! - `backend`: the S3 backend's chunk source on its own, with no V8;
//! - `readChunk`: a worker isolate calling `env.storage.readChunk` in a loop;
//! - `Bucket.getStream`: the same isolate draining the `ReadableStream` that
//!   the `@zeroship/storage` SDK builds over `readChunk`, loaded from the
//!   SDK's built `dist` (run `pnpm build` first).
//!
//! Each line reports bytes per second, chunks per second and the average
//! chunk size, so a cost that scales with the chunk count (a V8 round trip,
//! a timer, an allocation) can be told apart from one that scales with bytes.
//!
//! ```text
//! cargo bench -p zeroship-storage-v8 --bench stream_read -- [MiB]
//! ```

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use zeroship_runtime::channel::CancelFlag;
use zeroship_runtime::plugin::NativePlugin;
use zeroship_runtime::{
    init_v8, EnvSnapshot, FetchOutcome, ModuleEntry, RequestCtx, Runtime, SettledFetch,
};
use zeroship_storage::backend::{ChunkResult, ChunkSource};
use zeroship_storage::{Namespace, StorageStore};
use zeroship_storage_v8::StorageBinding;
use zeroship_testkit::s3::S3Server;

const APP_ID: &str = "bench_app";
const BUCKET: &str = "bench";
const KEY: &str = "stream/read.bin";
const DEFAULT_MIB: u64 = 256;

/// Reads `KEY` with the mode named in the request URL and reports the byte
/// and chunk counts. The handler does no per-byte work, so the figures are
/// the transport's.
const READ_APP: &str = r#"
import { Bucket } from "./storage-sdk.js";

export default {
    async fetch(request, env) {
        const mode = new URL(request.url).searchParams.get("mode");
        let bytes = 0, chunks = 0;
        if (mode === "readChunk") {
            const handle = JSON.parse(await env.storage.getStream("bench", "stream/read.bin"));
            for (;;) {
                const chunk = await env.storage.readChunk(handle.streamId);
                if (chunk === undefined) break;
                bytes += chunk.length;
                chunks += 1;
            }
        } else {
            const { data, error } = await new Bucket("bench").getStream("stream/read.bin");
            if (error) throw error;
            const reader = data.body.getReader();
            for (;;) {
                const { done, value } = await reader.read();
                if (done) break;
                bytes += value.length;
                chunks += 1;
            }
        }
        return Response.json({ bytes, chunks });
    },
};
"#;

/// A deterministic upload body of `remaining` bytes in 1 MiB chunks.
struct Pattern {
    remaining: u64,
    chunk: bytes::Bytes,
}

#[async_trait::async_trait(?Send)]
impl ChunkSource for Pattern {
    async fn next_chunk(&mut self) -> Option<ChunkResult> {
        if self.remaining == 0 {
            return None;
        }
        let take = self.remaining.min(self.chunk.len() as u64);
        self.remaining -= take;
        Some(Ok(self.chunk.slice(..usize::try_from(take).unwrap())))
    }
}

fn store(server: &S3Server) -> StorageStore {
    let backend = zeroship_storage::S3::with_tuning(
        compio_s3::S3Config::parse_url(&server.url("bench")).expect("S3 fixture configuration"),
        compio_s3::S3Credentials::new(server.access_key(), server.secret_key(), None),
        zeroship_storage::S3UploadTuning::DEFAULTS,
    );
    StorageStore::from_backend(Arc::new(backend))
}

fn report(label: &str, bytes: u64, chunks: u64, elapsed: Duration) {
    let micros = elapsed.as_micros().max(1);
    let bytes_per_second = u128::from(bytes) * 1_000_000 / micros;
    let chunks_per_second = u128::from(chunks) * 1_000_000 / micros;
    let bytes_per_chunk = bytes / chunks.max(1);
    println!(
        "{label:>18}: {bytes} bytes in {chunks} chunks, {elapsed:.3?}, {}.{} MB/s, \
         {chunks_per_second} chunks/s, {}.{} KiB/chunk",
        bytes_per_second / 1_000_000,
        bytes_per_second / 100_000 % 10,
        bytes_per_chunk / 1024,
        bytes_per_chunk * 10 / 1024 % 10,
    );
}

/// The SDK's built bundle. A bundle older than its source would measure a
/// superseded SDK, so that is refused rather than read.
fn sdk_source() -> String {
    let package = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/storage");
    let bundle = package.join("dist/index.js");
    let modified = |path: &std::path::Path| {
        std::fs::metadata(path).and_then(|meta| meta.modified()).unwrap_or_else(|error| {
            panic!("{}: {error}; run `pnpm build` first", path.display())
        })
    };
    assert!(
        modified(&bundle) >= modified(&package.join("src/index.ts")),
        "{} is older than its source; run `pnpm build` first",
        bundle.display()
    );
    std::fs::read_to_string(&bundle)
        .unwrap_or_else(|error| panic!("read {}: {error}", bundle.display()))
}

fn read_in_isolate(store: StorageStore, sdk: &str, mode: &str) -> (u64, u64, Duration) {
    let modules = vec![
        ModuleEntry { specifier: "index.js".into(), source: READ_APP.into() },
        ModuleEntry { specifier: "storage-sdk.js".into(), source: sdk.into() },
    ];
    let url = format!("http://localhost/?mode={mode}");
    compio::runtime::Runtime::new().unwrap().block_on(async move {
        init_v8();
        let plugin: Arc<dyn NativePlugin> = Arc::new(StorageBinding::new(store, None));
        let runtime = Runtime::builder()
            .modules(modules)
            .env_vars(HashMap::from([("APP_ID".to_owned(), APP_ID.to_owned())]))
            .plugins(vec![plugin])
            .build();
        runtime.start_pump();
        let started = Instant::now();
        let outcome = runtime.call_fetch_handler(
            "GET",
            &url,
            &[],
            "",
            &EnvSnapshot::empty(),
            RequestCtx::new(CancelFlag::new()),
        );
        let (status, body) = match outcome {
            FetchOutcome::Response { status, body, .. } => (status, body),
            FetchOutcome::Pending { rx, .. } => match rx.recv().await.expect("pending reply") {
                SettledFetch::Response { status, body, .. } => (status, body),
                _ => panic!("read app did not settle with a response"),
            },
            _ => panic!("read app returned an unexpected outcome"),
        };
        let elapsed = started.elapsed();
        let body = String::from_utf8_lossy(&body).into_owned();
        assert_eq!(status, 200, "read app failed: {body}");
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();
        (json["bytes"].as_u64().unwrap(), json["chunks"].as_u64().unwrap(), elapsed)
    })
}

fn main() {
    let mib = std::env::args()
        .skip(1)
        .find_map(|arg| arg.parse::<u64>().ok())
        .unwrap_or(DEFAULT_MIB);
    let size = mib * 1024 * 1024;
    let sdk = sdk_source();
    let server = S3Server::start();
    let store = store(&server);
    let storage = store.namespace(Namespace::app(APP_ID).unwrap());

    compio::runtime::Runtime::new().unwrap().block_on(async {
        let chunk: Vec<u8> = (0..1024 * 1024u32).map(|i| (i % 251) as u8).collect();
        let body = Pattern { remaining: size, chunk: bytes::Bytes::from(chunk) };
        let stored = storage.put_stream(BUCKET, KEY, Box::new(body), None).await.unwrap();
        assert_eq!(stored, size);

        let started = Instant::now();
        let (meta, mut source) = storage.get_stream(BUCKET, KEY).await.unwrap().unwrap();
        let (mut bytes, mut chunks) = (0u64, 0u64);
        while let Some(chunk) = source.next_chunk().await {
            bytes += chunk.unwrap().len() as u64;
            chunks += 1;
        }
        assert_eq!(bytes, meta.size);
        report("backend", bytes, chunks, started.elapsed());
    });

    for mode in ["readChunk", "Bucket.getStream"] {
        let (bytes, chunks, elapsed) = read_in_isolate(store.clone(), &sdk, mode);
        assert_eq!(bytes, size, "{mode} read a different length");
        report(mode, bytes, chunks, elapsed);
    }
}
