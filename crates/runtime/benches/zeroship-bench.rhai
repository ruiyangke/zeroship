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

// RPC wire (appbase ee2fd5f): POST /_rpc/<method> with body = JSON
// array of positional args. No JSON-RPC envelope, no id, no
// method-in-body. The zeroship runtime dispatches by URL path.
fn rpc(base, method, params) {
    POST(`${base}/_rpc/${method}`)
        .header("Content-Type", "application/json")
        .body(params)
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
//
// v0.1.0 DSL: protocol-native primitives replace the old single-shot
// builders. `sse_hold` measures events/s + inter-event gap p99 across
// N subscribers held for a duration (the production question for SSE);
// `ws_echo_rtt` measures per-message RTT across N persistent
// connections sending at a fixed rate (production question for WS).
if env("BENCH_SKIP_STREAMING", "0") != "1" {
    let hold_for = env("BENCH_STREAM_HOLD", "10s");
    let stream_subs = parse_int(env("BENCH_STREAM_SUBS", "50"));
    let ws_rate = parse_int(env("BENCH_WS_MSG_RATE", "100"));

    scenario("sseHold", |s| {
        // `delay` (not `delay_ms`) — matches the server-side query
        // key in scenarios.js and node_server.js. A 10ms pace gives
        // meaningful inter-event-gap numbers; 0 would be a firehose
        // bounded only by the socket's send-buffer and would
        // measure backpressure, not production SSE cadence.
        s.step(
            sse_hold(`${base}/sse?chunks=100000&delay=10`, stream_subs, hold_for)
        );
    });
    scenario("wsEchoRtt", |s| {
        s.step(
            ws_echo_rtt(`${ws_base}/`, stream_subs, ws_rate)
                .payload(`{"type":"ping","ts":0}`)
        );
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
