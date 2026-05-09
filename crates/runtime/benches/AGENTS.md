# benches

Bench fixtures + runners for measuring the runtime against itself
(over time) and against Node.js (single + cluster).

## Where to start, by task

| If you're working on… | Start here |
| --- | --- |
| **Adding a new RPC scenario** | `scenarios.js` (export the procedure + register it in `_scenarios`) → `zeroship-bench.rhai` (add `if want.call("name") { scenario(...) }`) → `node_server.js` if you want a node-side comparison |
| **Running a single scenario across all slots** | `./run_zerobench.sh --scenario=httpGet` |
| **Running all scenarios against one slot** | `./run_zerobench.sh --target=v8-16w` |
| **Iterating fetch / RPC perf** | `./run_zerobench.sh --target=v8-16w --scenario=httpGet --duration=5s` |
| **Adding a new SSE / WS streaming scenario** | `zeroship-bench.rhai` (use `sse_hold` / `ws_echo_rtt` DSL) + extend `scenarios.js`'s `default.fetch` to serve the path |
| **What does this scenario do?** | `scenarios.js` — every RPC procedure lives there as an `export async function` |
| **What's a good baseline number?** | `results-YYYY-MM-DD-*.txt` — historical, immutable, named after the change that prompted them |
| **Why is the bench broken?** | `~/Projects/zerobench/ISSUES.md` (off-tree, in the zerobench repo) |

## Active files

| File | Role |
| --- | --- |
| `run_zerobench.sh` | Main runner — boots all four runtime slots + nginx, drives `zerobench` against each, parses + summarises per-scenario |
| `zeroship-bench.rhai` | The unified Rhai plan declaring every scenario (RPC, HTTP, SSE, WS) — gated by `BENCH_SCENARIO` env var |
| `scenarios.js` | JS bench fixture — `_scenarios` map drives RPC dispatch; `default.fetch` covers the WinterCG slow path; `default.fetchFast` covers the zeroship fast path; `default.rpc` covers the kernel RPC dispatch |
| `node_server.js` | Single-process node baseline — implements the same `/_zs/v1/<id>` RPC wire so cross-runtime benches measure apples-to-apples |
| `node_server_cluster.js` | Forked-cluster node baseline (N workers via `cluster` module) |
| `node_ws_server.js` | Standalone WS echo server (used by older `run_ws_benchmark.sh`) |
| `run_benchmark.sh` | Legacy wrk-based runner — superseded by `run_zerobench.sh`, kept for cross-checking |
| `run_ws_benchmark.sh` | Legacy WS-only runner — same caveat |
| `ntex-bench/` | Off-tree experiment comparing `ntex` HTTP layer to `zeroship-bench-server`'s raw `httparse` |

## Slot identifiers (for `--target`)

| Name | Binary | Port | What it tests |
| --- | --- | ---: | --- |
| `v8-1w` | `zeroship-bench-server` (1 worker) | 5100 | Single-thread runtime ceiling |
| `v8-16w` | `zeroship-bench-server` (N workers) | 5101 | The headline number — N-core scaling |
| `node-single` | `node node_server.js` | 4002 | Node `node:http` legacy baseline (plain `req`/`res`) |
| `node-cluster` | `node node_server_cluster.js` | 4003 | Node `node:http` cluster baseline (plain `req`/`res`) |
| `node-whatwg` | `node node_whatwg_server.js` | 4004 | Node `node:http` wrapped into WHATWG `(Request) => Response` — apples-to-apples vs zeroship `default.fetch` |
| `node-whatwg-cluster` | `node node_whatwg_server_cluster.js` | 4005 | Cluster variant of the WHATWG wrap |

`zeroship-bench-server` is the bench fixture binary built from
`crates/runtime/src/server.rs` — it embeds `scenarios.js` via
`include_str!` so the V8 isolate runs the same dispatch path as a
deployed app.

## Filter flags (set via runner args)

| Flag | Default | Effect |
| --- | --- | --- |
| `--scenario=NAME` | (all scenarios) | Run only `NAME`. Names are the rhai `scenario(...)` keys: `ping`, `fib10`, `setTimeout0`, `promiseChain`, `fetchEcho`, `sha256`, `hmacSign`, `randomUUID`, `aesEncrypt`, `ecdsaSign`, `httpGet`, `sseHold`, `wsEchoRtt` |
| `--target=NAME[,NAME...]` | (all 4 slots) | Restrict slots. Comma-separated for multi-select. Servers for non-selected slots aren't even started |
| `--duration=Ns` | `10s` | Per-scenario duration |
| `--conns=N` | `300` | Saturation conns |
| `--workers=N` | `16` | Server-side workers (also drives client threads via `CLIENT_THREADS`) |
| `--rate=Nk/s` | (saturate) | Switch to open-loop mode at a fixed rate |
| `--saturate` | (default) | Force saturate mode (closed-loop, full conn pool) |

## Required external setup

The runner expects these on `$PATH` / known locations:

- `zerobench` — the bench tool, expected at `~/Projects/zerobench/target/release/zerobench` (override via `ZEROBENCH=/path/to/binary`). Source: <https://github.com/ruiyangke/zerobench>
- `nginx` — fetched on demand via `nix build nixpkgs#nginx`. Used as the echo target so the runtime's HTTP layer is isolated from the fetch hot path
- `numactl` (optional, multi-NUMA only) — pins servers to node 0, client to node 1 to remove cross-node memory traffic from the measurement

## Result files convention

`results-YYYY-MM-DD-<slug>.txt` are **immutable historical records**.
Format is the runner's stdout verbatim. Naming: the date is the day
the snapshot was taken; the slug describes the change that triggered
the run (`after-fetch-perf`, `after-headers-cutover`, etc.). Older
snapshots are not regenerated when the bench script changes — they
freeze the data point.

When adding a new snapshot, name it after what changed: `git log
--oneline -1` on the commit that prompted the run is a good start.

## Common pitfalls

- **Bench reports impossible numbers (every scenario at ~1.2M req/s, every slot identical)**: the connection pool got reused across scenarios; one slot's idle nginx connections got drawn for the next slot's RPC calls. Hard symptom is `errors connect 0` on a port with no listener. See `~/Projects/zerobench/ISSUES.md`. Workaround: don't put a `nginxRaw`-style anchor scenario before scenarios that hit different ports.
- **Runtime servers die mid-run silently**: the runner's startup probe is a single curl per port; a server that segfaults after the probe but before the bench loop disappears without the runner noticing. The smoking gun is `errors connect 0` matched against `pgrep zeroship-bench-server` returning nothing.
- **NUMA flake**: on a multi-NUMA host without `numactl`, the bench reports correct numbers but with much higher variance. Check the header line — it says `Multi-NUMA detected but numactl not installed` if so.

## Doing perf work — the iteration loop

1. Capture a baseline: `./run_zerobench.sh --target=v8-16w --scenario=httpGet --duration=5s` and note the rps.
2. Make one change in `crates/runtime/src/...`.
3. Rebuild: `cargo build --release -p zeroship-runtime --bin zeroship-bench-server`.
4. Re-run the same command. Compare rps.
5. If the delta is ≥5% (machine variance is ~3%), commit. Otherwise revert.

Per-iteration time is dominated by the bench duration — keep it at
`--duration=5s` for tight loops, only run the full 10s sweep for the
final regression check.
