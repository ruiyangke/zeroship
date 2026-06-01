# zerobench

`zerobench` is not implemented in this repository. What lives here is the zeroship benchmark harness under [`crates/runtime/benches/`](../../crates/runtime/benches/), which boots local targets and drives an external `zerobench` binary against them.

## Main workflow

The entry point is [`run_zerobench.sh`](../../crates/runtime/benches/run_zerobench.sh).

It:

- builds `zeroship-bench-server`
- boots up to six targets
- downloads `nginx` via `nix` for the `fetchEcho` target
- runs `zerobench run` against the shared Rhai plan in [`zeroship-bench.rhai`](../../crates/runtime/benches/zeroship-bench.rhai)
- prints a scenario-first comparison table

By default it expects `zerobench` at `~/Projects/zerobench/target/release/zerobench`. Override with `ZEROBENCH=/path/to/zerobench`.

## Targets

[`run_zerobench.sh`](../../crates/runtime/benches/run_zerobench.sh) knows these slots:

| Target | Port | Notes |
| --- | ---: | --- |
| `v8-1w` | 5100 | `zeroship-bench-server` with 1 worker |
| `v8-16w` | 5101 | `zeroship-bench-server` with `WORKERS` workers |
| `node-single` | 4002 | Node baseline |
| `node-cluster` | 4003 | Node cluster baseline |
| `node-whatwg` | 4004 | Node WHATWG wrapper baseline |
| `node-whatwg-cluster` | 4005 | Node WHATWG cluster baseline |

## What gets exercised

The shared plan is [`zeroship-bench.rhai`](../../crates/runtime/benches/zeroship-bench.rhai). It currently drives:

- RPC over `POST /__zeroship/v1/<id>`
- HTTP fetch over `GET /hello`
- SSE over `GET /sse?...`
- WebSocket RTT over `ws://.../`

Scenarios currently declared in the Rhai file:

- `ping`
- `fib10`
- `setTimeout0`
- `promiseChain`
- `fetchEcho`
- `sha256`
- `hmacSign`
- `randomUUID`
- `aesEncrypt`
- `ecdsaSign`
- `httpGet`
- `sseHold`
- `wsEchoRtt`

The server-side fixture code lives in [`scenarios.js`](../../crates/runtime/benches/scenarios.js). Node comparison servers live alongside it in [`node_server.js`](../../crates/runtime/benches/node_server.js) and [`node_whatwg_server.js`](../../crates/runtime/benches/node_whatwg_server.js).

## Common commands

```bash
./crates/runtime/benches/run_zerobench.sh
./crates/runtime/benches/run_zerobench.sh --target=v8-16w --scenario=httpGet --duration=5s
./crates/runtime/benches/run_zerobench.sh --rate=200k
```

Supported runner flags are:

- `--duration=<span>`
- `--conns=<n>`
- `--workers=<n>`
- `--rate=<n>`
- `--saturate`
- `--scenario=<name>`
- `--target=<name[,name...]>`

## Supporting tools

- [`sse_bench.rs`](../../crates/runtime/benches/sse_bench.rs): standalone raw-TCP SSE probe
- [`run_benchmark.sh`](../../crates/runtime/benches/run_benchmark.sh): legacy `wrk` runner, kept for cross-checking
- [`run_ws_benchmark.sh`](../../crates/runtime/benches/run_ws_benchmark.sh) + [`ws_benchmark.js`](../../crates/runtime/benches/ws_benchmark.js): legacy WS-only benchmark
- benchmark snapshots in [`crates/runtime/benches/`](../../crates/runtime/benches/)

## Host requirements

The current harness assumes:

- `zerobench` is already built locally
- `node` is available for the comparison servers
- `nix` is available so the runner can fetch `nginx`
- `numactl` is optional; the runner uses it automatically on multi-NUMA hosts
