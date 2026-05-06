//! RPC v2 phase 1 — Wave F microbench.
//!
//! Measures the production `default.rpc(name, input, ctx)` dispatch
//! path end-to-end: `extract_zs_v1_id` → `parse_envelope_body` →
//! `RpcContext::build_js_object` (frozen Headers/URL + AbortController)
//! → `with_rpc_context_in_als` (ContinuationPreservedEmbedderData
//! save/install/restore) → `rpc_fn.call` (the V8 function call this
//! wave is here to measure) → `classify_rpc_return` →
//! `JSON.stringify` envelope wrap.
//!
//! Workloads (input → echoed back as the response body):
//!
//!   tiny      `{ "n": 1 }`                                       ~16 B
//!   small     single nested object                              ~230 B
//!   medium    array of 50 small objects                         ~3.6 KB
//!   large     array of 1000 records                              ~50 KB
//!   multipart same body as `medium`, multipart Content-Type     ~3.6 KB
//!
//! The "multipart" workload exists to exercise the dispatch path with
//! a multipart-shaped Content-Type — phase 1.7 will add real FormData
//! parsing; right now the kernel just sees the header and forwards.
//! The point is to confirm dispatch overhead is roughly constant
//! across header shapes, not to measure FormData decoding.
//!
//! Per `docs/proposals/rpc-v2.md` §5, this baseline decides whether
//! phase 1 stays on the single-call ABI or amends to a two-step
//! `#[v8_method(fastcall)] enqueue` shape. If single-call dispatch is
//! a small fraction of typical procedure latency, single-call wins;
//! the two-step ABI's fastcall savings (proposal estimates 30-100 ns)
//! aren't worth the extra slow-path round trip on `awaitDispatch`.

use std::time::Duration;

use criterion::{
    black_box, criterion_group, criterion_main, BatchSize, BenchmarkId, Criterion, Throughput,
};

use zeroship_runtime::channel::CancelFlag;
use zeroship_runtime::runtime::Runtime;
use zeroship_runtime::{init_v8, EnvSnapshot, FetchOutcome, ModuleEntry, RequestCtx};

// ---------------------------------------------------------------------------
// Workload builders — all return `{"json": <value>}` envelope bodies
// ---------------------------------------------------------------------------

fn tiny_body() -> String {
    // 16 B body. Smallest realistic RPC: one int.
    r#"{"json":{"n":1}}"#.to_string()
}

fn small_body() -> String {
    // ~230 B body. Single nested object — the median CRUD shape.
    let inner = serde_json::json!({
        "id": "usr_01HJQK2A8R000000000000000",
        "email": "alice@example.com",
        "name": "Alice Example",
        "role": "admin",
        "createdAt": "2026-05-05T00:00:00Z",
        "updatedAt": "2026-05-05T00:00:00Z",
        "preferences": { "theme": "dark", "locale": "en-US" }
    });
    format!(r#"{{"json":{}}}"#, serde_json::to_string(&inner).unwrap())
}

fn medium_body() -> String {
    // ~3.6 KB body. Array of 50 small objects — list/page response shape.
    let arr: Vec<_> = (0..50)
        .map(|i| {
            serde_json::json!({
                "id": format!("rec_{i:08x}"),
                "title": format!("Item number {i}"),
                "amount": i * 13,
                "tag": "blue"
            })
        })
        .collect();
    format!(
        r#"{{"json":{}}}"#,
        serde_json::to_string(&arr).unwrap()
    )
}

fn large_body() -> String {
    // ~50 KB body. Array of 1000 records — bulk fetch / report shape.
    let arr: Vec<_> = (0..1000)
        .map(|i| {
            serde_json::json!({
                "id": format!("rec_{i:08x}"),
                "k": i,
                "v": format!("value-{}-{}", i, i * 7),
            })
        })
        .collect();
    format!(
        r#"{{"json":{}}}"#,
        serde_json::to_string(&arr).unwrap()
    )
}

// ---------------------------------------------------------------------------
// Runtime fixture
// ---------------------------------------------------------------------------

/// Echo procedure exported as `default.rpc`. The kernel detects this
/// at module init and caches it as `rpc_fn` — the Tier-1 fast path,
/// the same one production routes hit.
const ECHO_MODULE: &str = r#"
export default {
    rpc: (id, input, ctx) => input,
};
"#;

fn build_runtime() -> Runtime {
    init_v8();
    let modules = vec![ModuleEntry {
        specifier: "index.js".into(),
        source: ECHO_MODULE.into(),
    }];
    Runtime::builder().modules(modules).build()
}

/// Drive a single `POST /_zs/v1/<id>` call against `runtime` and return
/// the response body. Asserts a 2xx so a misconfigured fixture fails
/// loudly during warmup instead of silently measuring an error path.
#[inline]
fn drive_one(
    runtime: &Runtime,
    method: &str,
    url: &str,
    headers: &[(String, String)],
    body: &str,
) -> String {
    let env = EnvSnapshot::empty();
    let ctx = RequestCtx::new(CancelFlag::new());
    let outcome = runtime.call_fetch_handler(method, url, headers, body, &env, ctx);
    match outcome {
        FetchOutcome::Response { status, body, .. } => {
            assert!(
                (200..300).contains(&status),
                "non-2xx in bench fixture: status={status} body={body}"
            );
            body
        }
        // Echo is sync — Pending/Stream/WSUpgrade would mean the
        // fixture regressed. Fail loudly rather than silently spin
        // up a compio runtime in the inner loop. Tag rather than
        // {:?}-format because FetchOutcome carries non-Debug fields.
        FetchOutcome::Stream { .. } => panic!("unexpected Stream outcome in echo bench"),
        FetchOutcome::Pending { .. } => panic!("unexpected Pending outcome in echo bench"),
        FetchOutcome::WebSocketUpgrade { .. } => {
            panic!("unexpected WebSocketUpgrade in echo bench")
        }
    }
}

// ---------------------------------------------------------------------------
// Bench groups
// ---------------------------------------------------------------------------

fn bench_dispatch(c: &mut Criterion) {
    let runtime = build_runtime();

    // Warm up the fast path once before sampling — the first call
    // through call_fetch_handler initializes `rpc_fn`, builds the
    // ctx_obj singleton, and primes a few V8 caches. We want
    // steady-state numbers, not first-call setup cost.
    let warm_url = "http://localhost/_zs/v1/echo";
    let warm_headers = vec![("content-type".to_string(), "application/json".to_string())];
    for _ in 0..16 {
        drive_one(&runtime, "POST", warm_url, &warm_headers, r#"{"json":{}}"#);
    }

    let json_headers = vec![("content-type".to_string(), "application/json".to_string())];
    let multipart_headers = vec![(
        "content-type".to_string(),
        "multipart/form-data; boundary=----zsBoundaryAAAA".to_string(),
    )];

    let workloads: Vec<(&str, String, &Vec<(String, String)>)> = vec![
        ("tiny", tiny_body(), &json_headers),
        ("small", small_body(), &json_headers),
        ("medium", medium_body(), &json_headers),
        ("large", large_body(), &json_headers),
        // multipart: same body shape as `medium`, multipart-shaped
        // Content-Type. Dispatch overhead should be ~constant — the
        // header is just stored, not parsed. Phase 1.7 adds real
        // FormData decoding.
        ("multipart", medium_body(), &multipart_headers),
    ];

    let mut group = c.benchmark_group("rpc_dispatch");
    group.measurement_time(Duration::from_secs(3));
    group.warm_up_time(Duration::from_secs(1));

    for (name, body, headers) in &workloads {
        // Throughput in bytes lets Criterion show ns/B alongside ns/iter.
        group.throughput(Throughput::Bytes(body.len() as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(name),
            body,
            |b, body| {
                b.iter_batched_ref(
                    || body.clone(),
                    |body| {
                        let out = drive_one(
                            &runtime,
                            "POST",
                            "http://localhost/_zs/v1/echo",
                            headers,
                            body,
                        );
                        black_box(out);
                    },
                    BatchSize::SmallInput,
                );
            },
        );
    }
    group.finish();
}

criterion_group!(benches, bench_dispatch);
criterion_main!(benches);
