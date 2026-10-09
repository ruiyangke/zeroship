//! What a request stopped for CPU can still do with storage.
//!
//! A stopped request's continuations still run, so the binding refuses every
//! storage call they make, and the downloads the request opened are released
//! at the stop rather than held until the isolate goes: nothing it left
//! running may read them, and they count against the app's live-download cap.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use zeroship_runtime::channel::CancelFlag;
use zeroship_runtime::plugin::NativePlugin;
use zeroship_runtime::{
    init_v8, EnvSnapshot, FetchOutcome, ModuleEntry, RequestCtx, Runtime, SettledFetch,
};
use zeroship_storage::backend::{ChunkResult, ChunkSource};
use zeroship_storage::{LocalFs, Namespace, StorageStore};
use zeroship_storage_v8::limits::{max_live_get_streams_per_app, DOWNLOAD_CHUNK_BYTES};
use zeroship_storage_v8::StorageBinding;

const APP_ID: &str = "app_stopped_readers";

/// `/stopped` opens a download, reads one chunk of it, leaves a storage call
/// in flight whose continuation reports what storage it can still reach, and
/// then burns CPU until it is stopped. `/read` reads the whole object, and
/// `/outcome` returns what the first stopped request's continuation reported.
const APP: &str = r#"
let report;
const outcome = new Promise((resolve) => { report = resolve; });
async function attempt(call) {
    try {
        await call();
        return "admitted";
    } catch (error) {
        return error.code ?? String(error);
    }
}
export default {
    async fetch(request, env) {
        const where = new URL(request.url).pathname;
        if (where === "/outcome") return Response.json(await outcome);
        const handle = JSON.parse(await env.storage.getStream("uploads", "big.bin"));
        if (where === "/read") {
            let total = 0;
            for (;;) {
                const chunk = await env.storage.readChunk(handle.streamId);
                if (chunk === undefined) break;
                total += chunk.length;
            }
            return Response.json({ size: handle.size, total });
        }
        await env.storage.readChunk(handle.streamId);
        env.storage.get("uploads", "small.bin").then(async () => report({
            readChunk: await attempt(() => env.storage.readChunk(handle.streamId)),
            get: await attempt(() => env.storage.get("uploads", "small.bin")),
        }));
        let x = 0;
        for (let i = 0; i < 300000000; i++) { x = (x + 1) % 2147483647; }
        return Response.json({ survived: true, x });
    },
};
"#;

/// An upload body handed over in one frame.
struct Whole(Bytes);

#[async_trait::async_trait(?Send)]
impl ChunkSource for Whole {
    async fn next_chunk(&mut self) -> Option<ChunkResult> {
        (!self.0.is_empty()).then(|| Ok(std::mem::take(&mut self.0)))
    }
}

/// Call `path`, answered at once or through the pump: its status and body, or
/// the error it was ended with.
async fn call(
    runtime: &Runtime,
    path: &str,
) -> Result<(u16, Vec<u8>), zeroship_runtime::runtime::DispatchError> {
    match runtime.call_fetch_handler(
        "GET",
        &format!("http://localhost{path}"),
        &[],
        "",
        &EnvSnapshot::empty(),
        RequestCtx::new(CancelFlag::new()),
    ) {
        FetchOutcome::Response { status, body, .. } => Ok((status, body)),
        FetchOutcome::Pending { rx, .. } => {
            match compio::time::timeout(Duration::from_secs(30), rx.recv())
                .await
                .unwrap_or_else(|_| panic!("{path} is answered"))?
            {
                SettledFetch::Response { status, body, .. } => Ok((status, body)),
                _ => panic!("{path} answers with a buffered response"),
            }
        }
        _ => panic!("{path} answers with a buffered response"),
    }
}

fn json(answer: Result<(u16, Vec<u8>), zeroship_runtime::runtime::DispatchError>) -> serde_json::Value {
    let (status, body) = answer.expect("a response");
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
    serde_json::from_slice(&body).expect("JSON body")
}

/// More requests than the app may hold downloads open are each stopped while
/// holding one, and a live request still opens and reads the object to its
/// end: every stopped request's download was released at its stop. The first
/// stopped request's continuation reports that its storage calls are refused,
/// a read of its own download and a plain read alike.
#[test]
fn a_stopped_request_releases_its_downloads_and_is_refused_its_storage_calls() {
    let root = tempfile::tempdir().unwrap();
    let store = StorageStore::from_backend(Arc::new(LocalFs::new(root.path())));
    let size = 2 * DOWNLOAD_CHUNK_BYTES + 1;
    let storage = store.namespace(Namespace::app(APP_ID).unwrap());
    compio::runtime::Runtime::new().unwrap().block_on(async {
        storage.put_stream("uploads", "big.bin", Box::new(Whole(Bytes::from(vec![7; size]))), None).await.unwrap();
        storage.put("uploads", "small.bin", b"small", None).await.unwrap();
    });

    let stopped_readers = max_live_get_streams_per_app() + 1;
    compio::runtime::Runtime::new().unwrap().block_on(async move {
        init_v8();
        let plugin: Arc<dyn NativePlugin> = Arc::new(StorageBinding::new(store, None));
        let runtime = Runtime::builder()
            .modules(vec![ModuleEntry { specifier: "index.js".into(), source: APP.into() }])
            .env_vars(HashMap::from([("APP_ID".to_owned(), APP_ID.to_owned())]))
            .plugins(vec![plugin])
            .cpu_limit(Duration::from_millis(20))
            .build();
        runtime.start_pump();

        for reader in 0..stopped_readers {
            match call(&runtime, "/stopped").await {
                Err(error) => assert!(
                    error.message.contains("CPU time limit exceeded"),
                    "stopped reader {reader} must open its download and be stopped for CPU: {error:?}"
                ),
                Ok(_) => panic!("stopped reader {reader} must be stopped, not answered"),
            }
        }
        assert_eq!(
            json(call(&runtime, "/outcome").await),
            serde_json::json!({ "readChunk": "request_cancelled", "get": "request_cancelled" }),
            "a stopped request's storage calls are refused",
        );
        assert_eq!(
            json(call(&runtime, "/read").await),
            serde_json::json!({ "size": size, "total": size }),
            "a live request still opens a download and reads it to its end",
        );
    });
}
