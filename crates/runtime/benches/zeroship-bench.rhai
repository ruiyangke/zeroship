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
//   target/release/zeroship-bench-server --port=5101 --workers=16
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

// BENCH_SCENARIO=ping runs only the matching scenario; empty string
// runs everything. Used by the runner's `--scenario=NAME` flag for
// per-scenario perf iteration.
let only = env("BENCH_SCENARIO", "");
let want = |name| only == "" || only == name;

// RPC wire (zeroship v1, /_zs/v1/<id> with superjson `{ json }`
// envelope). Body is a single JSON value wrapped under `json` —
// `null` for no-arg procedures, `<value>` for single-arg. Response
// arrives as `{ json: <value> }`. Status 200 on success; 4xx/5xx
// for validation/dispatch errors.
fn rpc(base, method, body) {
    POST(`${base}/_zs/v1/${method}`)
        .header("Content-Type", "application/json")
        .body(body)
        .expect_status(200)
}

// No-arg procedures: empty `{}` body. The dispatcher unwraps to
// `undefined`, which all the no-arg fixtures ignore.
if want.call("ping")         { scenario("ping",         |s| { s.step(rpc(base, "ping",         "{}")); }); }
if want.call("fib10")        { scenario("fib10",        |s| { s.step(rpc(base, "fib",          `{"json":10}`)); }); }
if want.call("setTimeout0")  { scenario("setTimeout0",  |s| { s.step(rpc(base, "timeout0",     "{}")); }); }
if want.call("promiseChain") { scenario("promiseChain", |s| { s.step(rpc(base, "promiseChain", "{}")); }); }
if want.call("fetchEcho")    { scenario("fetchEcho",    |s| { s.step(rpc(base, "fetchExternal", `{"json":"${echo_url}"}`)); }); }
if want.call("sha256")       { scenario("sha256",       |s| { s.step(rpc(base, "sha256",       "{}")); }); }
if want.call("hmacSign")     { scenario("hmacSign",     |s| { s.step(rpc(base, "hmacSign",     "{}")); }); }
if want.call("randomUUID")   { scenario("randomUUID",   |s| { s.step(rpc(base, "uuid",         "{}")); }); }
if want.call("aesEncrypt")   { scenario("aesEncrypt",   |s| { s.step(rpc(base, "aesEncrypt",   "{}")); }); }
if want.call("ecdsaSign")    { scenario("ecdsaSign",    |s| { s.step(rpc(base, "ecdsaSign",    "{}")); }); }

// HTTP dispatch (GET /hello via the WinterCG fetch path).
if want.call("httpGet") {
    scenario("httpGet", |s| {
        s.step(GET(`${base}/hello`).expect_status(200));
    });
}

// Streaming scenarios — `sse_hold` measures events/s + inter-event gap
// p99 across N subscribers held for a duration (production question
// for SSE); `ws_echo_rtt` measures per-message RTT across N persistent
// connections sending at a fixed rate (production question for WS).
let hold_for = env("BENCH_STREAM_HOLD", "10s");
let stream_subs = parse_int(env("BENCH_STREAM_SUBS", "50"));
let ws_rate = parse_int(env("BENCH_WS_MSG_RATE", "100"));

if want.call("sseHold") {
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
}
if want.call("wsEchoRtt") {
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
