// zeroship-bench.rhai — comprehensive runtime benchmark using zerobench.
//
// Covers the full request lifecycle: sync RPC, async RPC (timers, fetch,
// promises), crypto, HTTP dispatch, SSE streaming, and WebSocket
// round-trips — one unified Rhai file drives all three protocols.
//
// Run with:
//   zerobench run crates/runtime/benches/zeroship-bench.rhai -c 300
//
// The server must be running:
//   target/release/v8-server-compio --port=5101 --workers=16
//   target/release/echo-server --port=8888
//
// Each RPC scenario sends a JSON-RPC request to the /rpc endpoint. The
// V8 runtime dispatches to the matching export from scenarios.js. SSE
// and WS scenarios hit the built-in /sse and /ws endpoints.

// ---------------------------------------------------------------------------
// RPC scenarios (via /rpc endpoint)
//
// Host + port come from environment variables — lets the same rhai file
// drive benchmarks against multiple servers (v8-compio, node, node-cluster)
// by swapping BENCH_HOST / BENCH_PORT per invocation.
// ---------------------------------------------------------------------------

// env(name, default) returns the var if set, else the default string.
let host = env("BENCH_HOST", "127.0.0.1");
let port = env("BENCH_PORT", "5101");
let echo_port = env("BENCH_ECHO_PORT", "8888");
let base = `http://${host}:${port}`;
let ws_base = `ws://${host}:${port}`;
let echo_url = `http://${host}:${echo_port}`;

fn rpc(base, method, params) {
    POST(`${base}/rpc`)
        .header("Content-Type", "application/json")
        .body(`{"jsonrpc":"2.0","method":"${method}","params":${params},"id":1}`)
        .expect_status(200)
}

scenario("ping",         |s| { s.step(rpc(base, "ping", "[]")); });
scenario("fib10",        |s| { s.step(rpc(base, "fib", "[10]")); });
scenario("setTimeout0",  |s| { s.step(rpc(base, "timeout0", "[]")); });
scenario("promiseChain", |s| { s.step(rpc(base, "promiseChain", "[]")); });
scenario("fetchEcho",    |s| { s.step(rpc(base, "fetchExternal", `["${echo_url}"]`)); });
scenario("sha256",       |s| { s.step(rpc(base, "sha256", "[]")); });
scenario("hmacSign",     |s| { s.step(rpc(base, "hmacSign", "[]")); });
scenario("randomUUID",   |s| { s.step(rpc(base, "uuid", "[]")); });
scenario("aesEncrypt",   |s| { s.step(rpc(base, "aesEncrypt", "[]")); });
scenario("ecdsaSign",    |s| { s.step(rpc(base, "ecdsaSign", "[]")); });

// HTTP dispatch scenario (zeroship only — node doesn't implement /hello)
if env("BENCH_HTTP_GET", "1") != "0" {
    scenario("httpGet", |s| {
        s.step(GET(`${base}/hello`).expect_status(200));
    });
}

// SSE + WS scenarios (zeroship only — node baselines don't implement these
// endpoints identically; skip when BENCH_SKIP_STREAMING=1).
if env("BENCH_SKIP_STREAMING", "0") != "1" {
    scenario("sseStream", |s| {
        s.step(
            SSE(`${base}/sse?chunks=100&delay=0&size=50`)
                .expect_chunks(100)
        );
    });
    scenario("wsEcho", |s| {
        s.step(WS(`${ws_base}/`).message(`{"type":"ping","ts":0}`));
    });
}

// ---------------------------------------------------------------------------
// Configuration
//
// Open-loop by default so the multi-scenario run measures true server
// throughput (CO-free). The runner overrides duration/rate via CLI flags.
// ---------------------------------------------------------------------------

duration("10s");
rate("100k/s");
