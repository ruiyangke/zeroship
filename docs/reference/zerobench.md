# zerobench — Design

> Status: **design** · Version: **0.0.1** (new project, never released)
>
> This is a greenfield design. The existing `tools/bench/` code in the zeroship
> repo is experimental and unexposed; we'll reuse what's useful and rewrite the
> rest. zerobench becomes its own repo at v0.0.1 — no migration, no compat shim.

## 1. Goals and non-goals

### Goals

1. **Correct measurement by default.** Open-loop rate model, nanosecond HDR histograms, coordinated-omission-free latency.
2. **One engine, many transports.** HTTP/1.1, HTTP/2, SSE, WebSocket — all ride the same dispatcher, rate controller, recorder, and reporter.
3. **Zero interpreter on the hot path.** Workloads are *described* in Rhai (or CLI flags / request files), *executed* in pure Rust.
4. **Composable workload modeling.** Multi-step scenarios, response-driven variables, mixed weighted workloads, without scripting on the hot path.
5. **Small default binary.** Rhai scripting is a feature flag. CLI-only installs stay ~5MB.
6. **First-class diff tooling.** Every run writes a JSON artifact; `zerobench diff` reports regressions.

### Non-goals

- wrk compatibility (CLI flags, output format, Lua API). A `--wrk-compat` shim may ship as a convenience; the core does not compromise for it.
- Simulating browser workloads end-to-end (that's k6/artillery territory — out of scope).
- HTTPS certificate validation. Benchmarks target known hosts. `--insecure` remains the only mode.
- Coverage-of-correctness testing. zerobench is a load generator, not a contract tester.

## 2. Architectural model

Two strictly separated phases:

```
  Phase 1 — COMPILE                  Phase 2 — EXECUTE
  (runs once)                        (hot path)

  ┌────────────────────────┐         ┌────────────────────────┐
  │ Plan source            │         │ Rate controller        │
  │  • CLI args            │         │  ↓                     │
  │  • Request file        │         │ Dispatcher (N threads) │
  │  • Rhai script         │         │  ↓                     │
  │         ↓              │         │ Transport (H1/H2/WS/..) │
  │ Plan {                 │   ─→    │  ↓                     │
  │   scenarios, vars,     │         │ Extract + Check        │
  │   templates, rate, ... │         │  ↓                     │
  │ }                      │         │ Recorder → Report      │
  └────────────────────────┘         └────────────────────────┘

  Rhai engine dropped                100% Rust. No interpreter.
  before Phase 2 begins
  (unless on_response hooks)
```

**Invariant:** the execution engine never sees a plan source. It only sees a `Plan`. This keeps the hot path free of scripting concerns.

## 3. Core data model

```rust
pub struct Plan {
    pub scenarios:  Vec<Scenario>,
    pub weights:    Vec<f32>,         // index-aligned with scenarios, sums to 1.0
    pub vars:       VarRegistry,      // compile-time slot allocation
    pub rate:       RateProfile,
    pub duration:   Duration,
    pub warmup:     Option<Duration>,
    pub transport:  TransportKind,    // H1 | H2 | WS | SSE
}

pub struct Scenario {
    pub name:  String,
    pub steps: Vec<Step>,
}

pub enum Step {
    Request(RequestPlan),
    Pause(Duration),
    PauseRandom { min: Duration, max: Duration },
}

pub struct RequestPlan {
    pub method:  Method,
    pub url:     Template,
    pub headers: SmallVec<[(Template, Template); 8]>,
    pub body:    Option<BodySource>,
    pub extract: Vec<Extract>,
    pub checks:  Vec<Assertion>,
    pub on_response: Option<ResponseHook>,   // opt-in per-request Rhai callback
}

pub enum BodySource {
    Static(Bytes),                           // no {{...}} → pre-encoded
    Template(Template),                      // has {{...}}
    File(Arc<Mmap>),                         // whole file
    FilePool { files: Arc<[Mmap]>, strategy: PickStrategy },
}

pub enum PickStrategy { Random, RoundRobin }

pub enum Extract {
    JsonPath  { path: CompiledJsonPath, into: VarSlot },
    Header    { name: HeaderName,       into: VarSlot },
    StatusCode { into: VarSlot },
    RegexBody { re: regex::bytes::Regex, into: VarSlot, group: u32 },
}

pub enum Assertion {
    StatusEq(u16),
    StatusIn(SmallVec<[u16; 4]>),
    LatencyUnder(Duration),
    BodyContains(Bytes),
    JsonEq { path: CompiledJsonPath, value: serde_json::Value },
}

pub struct Template {
    parts: Vec<Part>,
    estimated_size: usize,
}

pub enum Part {
    Literal(Bytes),
    Uuid, Uuid4,
    NowMs, NowNs, NowIso,
    Counter(Rc<Cell<u64>>),                  // per-thread
    CounterGlobal(Arc<AtomicU64>),
    RandInt   { min: i64, max: i64 },
    RandHex   { bytes: usize },
    RandStr   { len: usize, alphabet: Alphabet },
    Env(Bytes),                              // resolved at parse → Literal (kept for error messages)
    Line(Arc<FixtureFile>),                  // mmap + newline-offset index
    VarRef(VarSlot),                         // reads from ScenarioContext
}
```

Discipline: **everything `Plan` references is `Send + Sync + 'static` once built.** The plan is cheaply cloneable (Arc'd internally) and shipped to each worker thread.

## 4. Transport trait

```rust
pub trait Transport: Send + 'static {
    /// Cheap-to-clone per-thread client (cyper::Client is Arc internally).
    type Client: Clone + Send + 'static;

    fn build_client(opts: &TransportOpts) -> Result<Self::Client>;

    async fn exchange(
        client: &Self::Client,
        plan:   &RequestPlan,
        ctx:    &mut ScenarioContext,
    ) -> Result<Response>;
}

pub struct Response {
    pub status:         u16,
    pub headers:        HeaderMap,
    pub body:           ResponseBody,    // Buffered(Bytes) or Stream for SSE
    pub bytes_received: u64,
    pub latency:        Duration,
}

pub enum ResponseBody {
    Buffered(Bytes),
    Stream(Pin<Box<dyn futures::Stream<Item = Result<Bytes>> + Send>>),
}
```

Three implementations at launch — **all compio-native**, built directly on hyper/h3/compio-ws. We depend on [`cyper-core`](https://github.com/compio-rs/cyper/tree/master/cyper-core) for the hyper↔compio IO bridge (~200 LOC: `HyperStream`, `CompioExecutor`, `CompioTimer`), but **not** on cyper's high-level client — its redirect/cookie/JSON/multipart/charset machinery is dead weight for a bench tool and hides the control we need.

- **`HttpTransport`** — wraps `hyper::client::conn::http1` and `http2` directly. We own the connection pool and lifecycle. ALPN decides H1 vs H2 on TLS.
- **`Http3Transport`** — wraps `h3` over `compio-quic`. One QUIC connection, N concurrent streams.
- **`SseTransport`** — uses our H1/H2 transport with the response body streamed; our layer adds SSE line-framing + chunk latency recording.
- **`WsTransport`** — wraps `compio-ws`. RFC 6455 handshake, frame codec, ping/pong.

Feature flags: `http-h1` (default), `http-h2`, `http-h3`, `ws`, `sse`.

### Why raw hyper, not cyper

cyper is reqwest-for-compio — designed for app developers. It brings redirect following, cookie jars, charset conversion, JSON body decoders, multipart — all dead weight for a bench tool. Worse, it manages its own connection pool and hides byte-level wire access, which costs us:

- **Byte-accurate wire counts** — we get *exactly* N bytes on socket by wrapping `HyperStream`'s inner stream in a counting adapter. cyper gives body bytes only.
- **Per-request TTFB** — `sender.send_request().await` returning = response headers arrived. Free measurement.
- **Explicit pool control** — we pre-open N connections in `build_client`, count connect errors per-slot.

Using hyper's low-level `client::conn::http1` and `http2::Builder` directly is ~500 LOC across the transport crate — less code than adapting cyper would require, with more control and smaller binary (~3-5MB savings by not pulling serde-json/encoding_rs/mime/url/tower as transitive deps).

### Connection-pool semantics (CLI `-c N`)

The meaning of "connections" shifts by protocol — which matches reality better than v1's uniform treatment:

| Protocol | `-c 300` means |
|----------|----------------|
| H1       | Up to 300 idle TCP connections in the pool (cyper opens more if demand exceeds) |
| H2       | Up to 300 concurrent streams multiplexed over ~1 TCP connection |
| H3       | Up to 300 concurrent streams multiplexed over ~1 QUIC connection |
| WS       | 300 concurrent long-lived WS connections |
| SSE      | 300 concurrent long-lived SSE connections |

Users who want exact control: `--max-conns N` caps pool size; `--h1-only` forces HTTP/1 even on HTTPS targets that offer H2.

### Measurement capabilities

- **Full per-request latency** — `Instant::now()` before `sender.send_request` and after body consumed.
- **Per-request TTFB** — time between send-request and response-headers arrival. Free from hyper's API shape.
- **Byte-accurate wire counts** — custom `CountingStream` wrapper around the raw socket, before TLS / before hyper. Counts encrypted bytes on wire (the right number for "Transfer/sec").
- **Per-connection error isolation** — we own the socket, we know which slot failed to connect vs which one died mid-request.

Not in v2.0:
- **Response decompression** — not the bench tool's job; user sends `Accept-Encoding: identity` by default.
- **Client cert auth** — if needed, surface via `--client-cert` passing rustls config directly.

## 5. Rate controller

Two modes, both implemented on the dispatcher side:

### Open-loop (default)

```rust
pub enum RateProfile {
    Constant(f64),                          // req/s
    Ramp { from: f64, to: f64, over: Duration },
    Stepped(Vec<(Duration, f64)>),          // step changes
    Saturate { max_concurrency: usize },    // closed-loop fallback
}
```

Implementation: a single rate-scheduler task emits **request tokens** (wrapped start-times) into an MPMC channel at the target rate. Worker tasks pull tokens and execute. Latency recorded as `now - token.intended_start`, so queue-time counts — no coordinated omission.

Back-pressure: if workers can't keep up, the scheduler drops tokens OR grows concurrency up to a bounded cap, configurable. Default: grow to cap, then drop and count as `errors_keepup`.

### Saturation mode

`--saturate -c N` → pure closed-loop. N persistent tasks, each one req-then-resp in a loop until deadline. No scheduler. Identical to today's `thread.rs` design but cleanly separated.

Rate syntax in scripts:
```rhai
rate("10k/s")                        // constant
rate("1k..10k over 30s")             // linear ramp
rate([(0s, 100), (5s, 1000), (10s, 10000)])  // stepped
saturate(300)                        // closed-loop with 300 conns
```

## 6. Recorder and reporter

### Stats storage

- Per-task: `TaskStats` (latency HDR in ns, per-assertion counters, per-extract counters, error categories). Owned by the task, zero contention.
- Per-second: a lightweight `LiveSnapshot` atomic aggregator fed by each task (Relaxed CAS batch every 100 samples). Used for TUI and streaming output.
- End of run: tasks drop their `TaskStats` into a collector, merged into `Summary`.

### Output modes

- **Terminal** (default): colored aligned table. Rate, actual rate, latency percentiles (p50/p90/p99/p99.9/max), errors by category, per-scenario breakdown if >1 scenario.
- **TUI** (`--tui`): live dashboard. See §6.1.
- **JSON** (`--format=json`): single blob. Full histogram serialized (HDR format).
- **JSONL stream** (`--format=jsonl`): one line per second, for piping to graphing tools.
- **Prometheus textfile** (`--format=prom`): for scheduled benchmark jobs exporting to Prom.

### 6.1. TUI dashboard

Live terminal dashboard via `ratatui` + `crossterm`, enabled with `--tui`. Feature-gated (`tui` cargo feature) so headless installs skip ratatui/crossterm deps.

Layout (subject to iteration during implementation):

```
┌─ zerobench ─────────────────── http://api.example.com ──── 18/30s ────┐
│                                                                         │
│  target 10,000 req/s           actual 9,994 req/s (99.94%)              │
│  ████████████████████████████████████████████▒▒▒▒▒  60% elapsed        │
│                                                                         │
├─ throughput ───────────────────────────────────────────────────────────┤
│  10k ┤                                                                  │
│      │  ▄▆█████████████████████▅                                       │
│   5k ┤  ███████████████████████████                                    │
│      │  ███████████████████████████                                    │
│    0 ┤──┴─────────┴─────────┴─────────┴─────────┴──── sec              │
│       0          5         10         15         18                    │
│                                                                         │
├─ latency (last 5s) ───┬─ errors ────────────┬─ scenarios ──────────────┤
│  p50   120µs          │  connect    0       │  purchase (30%) 3.0k/s   │
│  p90   450µs          │  read       0       │    p99 4.1ms             │
│  p99   2.1ms          │  write      0       │  browse   (70%) 7.0k/s   │
│  p99.9 8.4ms   ▲0.3   │  timeout    0       │    p99 1.8ms             │
│  max   22ms           │  keepup     0       │                          │
│                       │  4xx/5xx    0/0     │                          │
└───────────────────────┴─────────────────────┴──────────────────────────┘
 [q] quit  [p] pause render  [l] toggle log                            
```

Key behaviors:
- Refresh at 10 Hz (100ms). Terminal cursor never flickers (ratatui diffs frames).
- Throughput chart: rolling 30s window, sparkline style.
- Latency panel: computed from a streaming t-digest (cheap to query live, unlike HDR which needs full iteration). The HDR histogram is still the source-of-truth for final report.
- "▲0.3" next to p99.9 = delta from 10s ago, color-coded green/red for regressions.
- Errors and per-scenario panels update per tick.
- `q` exits early (records duration as elapsed, not target); `p` pauses redraw (measurement continues); `l` toggles a log pane showing failed assertions / sample errors.

TUI feeds from the same `LiveSnapshot` the JSONL streaming output uses — single aggregator, two consumers. No duplicate bookkeeping.

On exit, TUI restores the terminal and prints the normal terminal report to stdout (so you always get a pastable summary, even if you used `--tui`).

Example default output:

```
target rate    10000 req/s constant
actual rate    9994.2 req/s (99.94%)
duration       30.00s  |  total 299827 requests

latency        p50=120µs  p90=450µs  p99=2.1ms  p99.9=8.4ms  max=22.1ms
throughput     9994 req/s  |  min 9920 (at 00:17)  max 10041 (at 00:08)
errors         connect 0  read 0  write 0  timeout 0  keepup 0
               status 2xx=299827  4xx=0  5xx=0
assertions     status==200: 299827/299827 ✓
               latency<500ms: 299827/299827 ✓

scenarios
  purchase-flow (30%)  89931 req  p99=4.1ms  errors 0
  browse-only   (70%)  209896 req p99=1.8ms  errors 0
```

## 7. Phase 1 — plan construction

### CLI mode

All flags map to a single-scenario, single-step plan. No Rhai involved.

```
zerobench -r 10k -d 30s \
  -X POST \
  -H 'Auth: Bearer {{env:TOKEN}}' \
  --body '{"id":"{{uuid}}","ts":{{now_ms}}}' \
  --expect-status 201 \
  http://api/events
```

### Request-file mode

Raw HTTP/1.1 file with `{{...}}` interpolation. Extends simply:

```
# ./fixtures/purchase.http
POST /api/checkout HTTP/1.1
Content-Type: application/json
Authorization: Bearer {{env:TOKEN}}
Idempotency-Key: {{uuid}}

{"cart_id":"{{uuid}}","amount":{{rand_int:100:9999}}}
```

Directory of `.http` files → weighted random per iteration (weights in `weights.toml`).

### Rhai script mode

```rhai
// bench.rhai
let token     = var("token");
let order_id  = var("order_id");

scenario("purchase-flow", 0.3, |s| {
    s.step(
        POST("/login")
            .json(#{
                email:    env("USER_EMAIL"),
                password: env("USER_PASSWORD"),
            })
            .expect_status(200)
            .extract_json("$.token", token)
    );
    s.step(pause(50.ms));
    s.step(
        POST("/api/checkout")
            .header("Authorization", `Bearer ${token}`)
            .json(#{
                cart_id: "{{uuid}}",
                amount:  "{{rand_int:100:9999}}",
                ts:      "{{now_ms}}"
            })
            .expect_status_in([200, 201])
            .extract_json("$.order_id", order_id)
    );
});

scenario("browse-only", 0.7, |s| {
    s.step(GET("/api/feed").expect_status(200));
});

rate("10k/s");
duration("30s");
```

Registered Rhai functions (all plan-construction, none I/O-performing):

- `scenario(name, weight, body)` — add scenario
- `GET(url)` / `POST(url)` / `PUT(url)` / ... — begin `RequestBuilder`
- `.header(name, value)` / `.headers(#{...})` / `.json(obj)` / `.body(str)` / `.body_file(path)`
- `.expect_status(n)` / `.expect_status_in([...])` / `.expect_latency(under_duration)`
- `.extract_json(path, slot)` / `.extract_header(name, slot)` / `.extract_status(slot)`
- `.on_response(|res, ctx| { ... })` — opt-in hot-path hook
- `pause(duration)` / `pause_random(min, max)` — Step::Pause
- `var(name)` → `VarSlot`
- `env(name)` / `env(name, default)` — resolved at compile
- `rate(profile)` / `saturate(n)` / `duration(d)` / `warmup(d)`
- `load_file(path)` / `load_json(path)` — fixture loading at compile
- `transport("h1" | "h2" | "ws" | "sse")` — choose transport

Rhai's role is **bounded**: build a `Plan`, return. No I/O, no sleep, no randomness that leaks into the hot path (use `{{...}}` for that).

## 8. Variable passing and scenario context

Per iteration of a scenario, a `ScenarioContext` holds the extracted vars:

```rust
pub struct ScenarioContext {
    vars: SmallVec<[Option<Bytes>; 8]>,    // indexed by VarSlot
}
```

`Extract::JsonPath { path, into }` writes to `vars[into.0]` after parsing response.
`Part::VarRef(slot)` reads `vars[slot.0]` during template expansion.

Cross-scenario var sharing is **not supported** in v2.0 — each scenario iteration has its own context. If users need cross-scenario state (rare), they can use global counters or an external store — we don't invent a new state machine.

## 9. Hot-path hook semantics (the 5% escape)

When a step has `on_response: Some(ResponseHook)`, the dispatcher:

1. Executes the request via Transport.
2. Keeps the Rhai engine alive.
3. Marshals `(status, headers, body_bytes, ctx)` into Rhai.
4. Runs the hook (acquires per-thread Rhai engine — one engine cached per worker thread, not per request).
5. Marshals any mutations back (ctx updates only; no I/O from Rhai).

Per-call cost: ~500-1000ns (Rhai custom type access). Documented. Only triggers when explicitly opted in.

**The default path never calls Rhai.** The engine is dropped after Phase 1 unless at least one step has a hook.

## 10. File layout (Rust side)

```
zerobench/                (new standalone repo)
  crates/
    zerobench-core/       # Plan, Template, Transport trait, Dispatcher, Recorder
    zerobench-http/       # HttpTransport: hyper + cyper-core (H1/H2/H3 features)
    zerobench-ws/         # WsTransport: compio-ws
    zerobench-sse/        # SseTransport: streaming body + line parser
    zerobench-rhai/       # Rhai plan builder (feature = "script")
    zerobench-tui/        # Live dashboard (feature = "tui")
    zerobench-cli/        # binary
  Cargo.toml              # workspace
  README.md
  LICENSE                 # MIT
  .github/workflows/      # CI: test, release, publish
```

### Dependency footprint

Default `cargo install zerobench`: core + http (H1) + cli. Pulls `hyper` (http1 feature), `cyper-core`, `compio`, `compio-tls`, `rustls`, `httparse`. ~5-6MB binary.

Feature matrix:
- `--features h2` — +hyper http2. ~6-7MB
- `--features h3` — +h3 + compio-quic. +3MB
- `--features ws` — +compio-ws. +500KB
- `--features sse` — +SSE parser (tiny). +200KB
- `--features tui` — +ratatui + crossterm + tdigest. +2MB
- `--features script` — +rhai. +100KB
- `--features all` — all of the above

Full build (`--features all`): ~13-14MB.

## 11. Relationship to existing `tools/bench/`

The existing code in `tools/bench/` (zeroship repo) is experimental, unreleased, and used only by internal benchmark scripts. It has:
- A wrk-compat HTTP path (`thread.rs`) — recently debugged, works correctly at ~800K req/s. Salvageable as an H1 reference during development.
- Broken SSE/WS paths (sequential-connection bug). Not salvageable; reimplement.
- Lua scripting via mlua — deleted entirely.
- wrk-style reporting — replaced by new structured output.

During v0.0.1 bootstrap, we cherry-pick useful algorithms (connection open/close patterns, stats merging, report formatting ideas) but implement everything against the new architecture. Once zerobench 0.0.1 is consumable, `tools/bench/` is deleted from the zeroship repo.

## 12. Correctness fixes folded into v2

Every issue from the v1 review ships fixed:

- ✓ Spawn-per-connection (HTTP) — already fixed in v1, carried to v2 engine
- ✓ Spawn-per-connection (SSE, WS) — **fixed in v2** via common dispatcher
- ✓ Nanosecond histograms — stats layer uses ns everywhere
- ✓ Proper chunked parser — real chunk-size decoder, not substring search
- ✓ Connect/read/write timeouts wired through `--timeout`
- ✓ Connection reconnect on error — dispatcher reconnects transparently
- ✓ Coordinated omission — open-loop scheduler records queue time
- ✓ TCP_NODELAY set explicitly
- ✓ io_uring ring tuning: `RuntimeBuilder::with_entries(2048)`
- ✓ Request bytes allocated once (Arc<Bytes>, O(1) clone via refcount)
- ✓ No per-response header lowercasing (use `eq_ignore_ascii_case`)
- ✓ Three-way enum dispatch replaced with Transport trait

## 13. Open questions (require decision before plan)

### Q1. HTTP/2 in v2.0 or v2.1? — **RESOLVED**

**Adopt [cyper](https://github.com/compio-rs/cyper) as the HTTP transport.** cyper is compio-native (compio 0.18 matches zeroship's), based on hyper 1.4, and supports H1/H2/H3 out of the box. WebSocket via compio-ws sibling crate.

H1/H2/H3 all ship in v2.0. No deferral.

### Q2. Request-file format — **RESOLVED**

**Raw HTTP with curl compat.** One `.http` file = one request. Format is exactly what `curl --trace-ascii` emits, so you can paste-replay any recorded curl request. Templating via `{{...}}`. Directory of files = weighted scenarios via optional `scenarios.toml`.

```
# fixtures/checkout.http  (paste straight from `curl --trace-ascii`)
POST /api/checkout HTTP/1.1
Host: api.example.com
Content-Type: application/json
Authorization: Bearer {{env:TOKEN}}
Idempotency-Key: {{uuid}}

{"cart_id":"{{uuid}}","amount":{{rand_int:100:9999}}}
```

Usage:
```bash
zerobench -r 10k --request-file fixtures/checkout.http
zerobench -r 10k --requests ./fixtures/                  # dir with weights.toml
```

Parser: be lenient. Accept both `\r\n` and `\n` line endings (curl uses `\r\n` but text editors corrupt it). Skip lines starting with `#` (comments). Empty-line terminates headers. The rest is body (with `{{...}}` expanded per-request).

### Q3. Scenario selection algorithm — compared in detail

All three approaches, head-to-head:

| Criterion | (a) Weighted random | (b) Scheduled interleave | (c) Per-scenario rates |
|-----------|---------------------|--------------------------|------------------------|
| **Expressiveness** | Shares one rate across scenarios, weighted | Same as (a) | Each scenario has independent rate profile, including independent ramps/steps |
| **Short-run variance** | High — 1% scenario in 1k iters may show 5–15× | Zero — exact ratios every N iterations | Depends on per-scenario rate; effectively zero for steady rates |
| **Determinism** | Non-deterministic (unless seeded) | Deterministic sequence | Deterministic start times per scenario |
| **Server-side realism** | Good — looks like Poisson mixed traffic | Poor — regular pattern may bias caches / scheduler | Best — each scenario has its own traffic pattern, matching production |
| **Script ergonomics** | `scenario("x", 0.3, ...)` | `scenario("x", 0.3, ...)` | `scenario("x", ...).rate("3k/s")` |
| **Implementation complexity** | Simplest — one scheduler, one RNG call per token | Medium — Bresenham-style stride counter | Simplest engine-side — one scheduler per scenario, independently |
| **Handles ramps/steps per scenario** | No | No | Yes |
| **Total rate predictability** | Exact | Exact | Sum of per-scenario rates |

**Insight:** (c) is *strictly more expressive* than (a) and (b). Anything you can do with (a) you can do with (c) by setting `scenario_rate = total_rate × weight`. (c) additionally handles:

- "Check liveness at 10/s while hammering API at 10k/s"
- "Ramp login flow 0→100/s while steady-state browse at 5k/s"
- "Burst checkout at 500/s for 10s inside a 30s test"

**Decision:** Implement (c) as the engine primitive — one rate scheduler per scenario. Accept (a)-style syntax as syntactic sugar that compiles to (c):

```rhai
// Form 1 — (a) syntactic sugar: weights × global rate → per-scenario rates
rate("10k/s");
scenario("purchase", 0.3, |s| { ... });   // → 3k/s
scenario("browse",   0.7, |s| { ... });   // → 7k/s

// Form 2 — (c) explicit per-scenario rates
scenario("healthcheck", |s| { ... }).rate("100/s");
scenario("api",         |s| { ... }).rate("10k/s");
scenario("reindex",     |s| { ... }).rate("1..10/s over 30s");   // ramp

// Forms can mix? No — error at plan-construction if one scenario has .rate()
// and another has a weight. Keep semantics clean.
```

Engine side: the dispatcher spawns one `RateScheduler` per scenario, each emitting request tokens into a shared MPMC queue consumed by the worker pool. Workers pull tokens, execute the scenario (possibly multi-step), record stats tagged by scenario-id.

**No scheduled interleave (b).** The server-side realism concern is real — regular patterns bias real servers, and the variance win is marginal for runs >10k iters (which any meaningful bench is).

### Q4. Connection-pool policy under burst — **RESOLVED**

**Grow up to `--max-conns` ceiling, drop past ceiling as `errors_keepup`.**

When the rate scheduler emits a token but no connection is available:
1. If pool size < `--max-conns`: open a new connection in the background, queue the token briefly.
2. If pool size == `--max-conns`: drop the token, increment `errors_keepup`, record would-be-latency as "∞" for reporting.

`--max-conns` defaults to `max(1000, target_rate × 0.1)` so a bench targeting 10k req/s auto-grows up to 1000 conns. User can cap lower explicitly for servers with tight FD limits.

### Q5. Crate split and standalone repo — **RESOLVED**

**Split as its own top-level repo.** zerobench is a general-purpose benchmark tool, not zeroship-specific. Living inside `tools/bench` of the zeroship repo marries the two projects in ways that hurt both (shared deps, cargo workspace constraints, release cadence coupling).

Action:
1. Create `github.com/zeroship-dev/zerobench` (or similar org).
2. Bootstrap fresh — no git history migration needed (the existing `tools/bench/` isn't exposed). Copy/adapt useful code; rewrite the rest against the new architecture.
3. Cargo workspace inside the new repo as designed (`zerobench-core`, `zerobench-http`, `zerobench-ws`, `zerobench-sse`, `zerobench-rhai`, `zerobench-cli`).
4. zeroship's `run_benchmark.sh` pulls zerobench from crates.io (or a git dep until published).

The zeroship repo keeps only:
- `crates/runtime/benches/run_benchmark.sh` (the harness) — uses zerobench
- `crates/runtime/benches/scenarios.js` (the JS test fixtures)
- `crates/runtime/benches/node_server*.js` (the Node comparison servers)

Delete `tools/bench/` from the zeroship repo after the new repo is bootstrapped and consumed from crates.io.

### Q6. First version — **RESOLVED**

**Start at v0.0.1.** This is a new, unreleased project. No migration, no compat concerns. Semver discipline starts clean from here.

0.0.x for the build-out phase. 0.1.0 when the core is stable and we commit to the public surface. 1.0 far in the future.

### Q7. Colored terminal output

Recommendation: `--color=auto` (default; detect TTY, respect `NO_COLOR` env).

## 14. Not in v0.0.1 (deferred)

- Distributed load (multiple zerobench instances coordinating) — unclear need
- Gatling-style DSL extras (during-ramp state machines, looping) — possible later
- Response-body compression auto-decode
- Client cert auth via CLI (easy to add, low demand)
- Windows support (Linux + macOS only for v0.0.1)
