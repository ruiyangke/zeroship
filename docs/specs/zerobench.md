# zerobench — Next-gen HTTP Benchmark Tool

## Overview

`zerobench` is a high-performance HTTP benchmark tool built on compio/io_uring. It's a superset of wrk with native support for SSE streaming, WebSocket, NUMA-aware CPU pinning, Lua scripting, and a live TUI dashboard.

```
zerobench -t8 -c300 -d10s http://localhost:8080/api          # HTTP (wrk-compatible)
zerobench --sse -c50 -d30s http://localhost:8080/stream       # SSE streaming
zerobench --ws -c100 -d10s ws://localhost:8080/echo           # WebSocket
zerobench --tui -t8 -c300 -d10s http://localhost:8080/api     # Live dashboard
```

## Goals

1. **wrk-compatible** — same CLI flags, same output format, same Lua scripting API. Drop-in replacement.
2. **SSE native** — measures TTFB, chunk latency, chunks/sec. No curl hacks.
3. **WebSocket native** — measures round-trip latency, msg/sec. Full RFC 6455.
4. **NUMA-aware** — pin threads and memory to NUMA nodes for consistent, reproducible results.
5. **io_uring** — compio backend for maximum throughput on Linux. Falls back to polling on macOS/Windows.
6. **Live TUI** — real-time throughput graph and latency histogram via ratatui.

## Architecture

```
zerobench binary
├── CLI (clap-style flags, wrk-compatible)
├── Config (threads, connections, duration, protocol, NUMA)
├── Thread pool
│   └── Per thread:
│       ├── compio event loop (io_uring on Linux)
│       ├── N connections (TCP or TLS via rustls)
│       └── Protocol handler (HTTP | SSE | WebSocket)
├── Stats (lock-free per-thread, merged at end)
│   ├── HDR histogram (nanosecond latency)
│   ├── Throughput counters
│   └── Error counters
├── Lua scripting (mlua, wrk-compatible callbacks)
├── TUI dashboard (ratatui + crossterm, optional --tui)
└── Reporter (wrk-compatible text + optional JSON)
```

### Thread Model

Like wrk: N OS threads, each with its own compio event loop and M/N connections. Threads share nothing — each collects its own stats. At the end, stats are merged across threads.

```
Thread 0 (pinned to CPU 0):
  compio loop → 37 connections → HTTP handler → local histogram
Thread 1 (pinned to CPU 1):
  compio loop → 38 connections → HTTP handler → local histogram
...
Thread 7 (pinned to CPU 7):
  compio loop → 38 connections → HTTP handler → local histogram

After duration:
  merge histograms → compute percentiles → print report
```

### NUMA / CPU Pinning

```
--numa 0        Pin all threads to NUMA node 0 (auto-detects CPUs)
--cpu 0-7       Pin threads to CPU range 0-7
--cpu 0,2,4,6   Pin threads to specific CPUs
```

Implementation: `libc::sched_setaffinity` per thread. NUMA memory binding via `libc::set_mempolicy` or `libc::mbind`. Auto-detection reads `/sys/devices/system/node/`.

## Protocol Modes

### HTTP Mode (default)

Standard HTTP/1.1 with keep-alive. Connection lifecycle:

```
connect → [TLS handshake] → send request → read response → record latency → repeat
                                                                              ↑
                                                             keep-alive reuse
```

Measures:
- Requests/sec (throughput)
- Latency: avg, stdev, max, percentiles (p50, p75, p90, p99, p99.9)
- Bytes/sec (transfer rate)
- Errors: connect, read, write, timeout, HTTP status errors

### SSE Mode (`--sse`)

Opens long-lived connections, reads Server-Sent Events. Connection lifecycle:

```
connect → send GET → read response headers (record TTFB) → loop:
  read line → if "data:" → record chunk + chunk latency
  until "data: [DONE]" or duration expires or connection closes
```

Measures:
- Time to first byte (TTFB): avg, p50, p99, max
- Chunks/sec: per connection and total
- Chunk latency: time between consecutive chunks, avg, p50, p99
- Active/completed streams

SSE parser: line-based, handles `data:`, `event:`, `id:`, `retry:` fields per the SSE spec.

### WebSocket Mode (`--ws`)

Opens WebSocket connections (RFC 6455 handshake over HTTP/1.1 upgrade). Connection lifecycle:

```
connect → HTTP upgrade handshake → loop:
  send message (from Lua or --body) → read response → record RTT
  until duration expires
```

Measures:
- Messages/sec (throughput)
- Round-trip time: avg, p50, p99, max
- Bytes/sec
- Frame types: text, binary, ping/pong

WebSocket codec: minimal RFC 6455 implementation — frame header parsing, masking (client frames), fragmentation not needed for benchmarking.

## CLI Interface

```
zerobench [OPTIONS] <URL>

Core (wrk-compatible):
  -t, --threads <N>       Threads (default: CPU count or CPU count/2 with NUMA)
  -c, --connections <N>   Total connections (default: 100)
  -d, --duration <T>      Duration: 10s, 1m, 5m (default: 10s)
  -s, --script <FILE>     Lua script
  -H, --header <H>        Add header (repeatable)
      --body <DATA>       Request body (switches to POST)
      --method <M>        HTTP method (default: GET)
      --timeout <T>       Socket timeout (default: 2s)
      --latency           Print detailed latency percentiles

Protocol:
      --sse               SSE streaming mode
      --ws                WebSocket mode

Performance:
      --numa <NODE>       Pin to NUMA node
      --cpu <RANGE>       Pin to CPU range (e.g., 0-7 or 0,2,4,6)
      --tls               Force TLS

Output:
      --tui               Live terminal dashboard
      --json              JSON output (alongside text)
```

## Lua Scripting (wrk-compatible)

Same API as wrk's Lua scripting:

```lua
-- HTTP: generate custom requests
function request()
    wrk.method = "POST"
    wrk.body = '{"key": "value"}'
    wrk.headers["Content-Type"] = "application/json"
    return wrk.format()
end

-- HTTP: process responses
function response(status, headers, body)
    if status ~= 200 then
        wrk.errors = wrk.errors + 1
    end
end

-- Final reporting
function done(summary, latency, requests)
    io.write(string.format("P99: %.2fms\n", latency:percentile(99) / 1000))
end
```

Extended callbacks for new modes:

```lua
-- SSE: process each chunk
function chunk(data, event_type)
    -- data is the SSE "data:" field value
end

-- WebSocket: generate messages
function message()
    return "ping " .. os.time()
end

-- WebSocket: process responses
function on_message(data, is_binary)
end
```

## TUI Dashboard (`--tui`)

Live terminal UI via ratatui + crossterm. Enabled with `--tui` flag.

Layout:

```
┌─ zerobench ─────────────────────────────────────────────────┐
│ URL  threads  connections  duration          Elapsed: 4.2s  │
├─────────────────────────────────────────────────────────────┤
│ Throughput (req/s)          │ Latency Distribution          │
│                             │                               │
│ 850k ┤         ╭──────     │ ██████████████████ 180µs p50   │
│ 700k ┤    ╭────╯           │ ████████████████████ 890µs p99 │
│ 400k ┤╭───╯               │ █████████████████████ 5.2ms max│
│      └┴────┴────┴────     │                               │
│       0s   1s   2s   3s   │                               │
├─────────────────────────────────────────────────────────────┤
│ Total: 3,412,800 req  │  Errors: 0  │  12.4 MB/s           │
└─────────────────────────────────────────────────────────────┘
```

Updates every 100ms. Falls back to text output if terminal doesn't support TUI.

## Crate Structure

```
tools/bench/
  Cargo.toml
  src/
    main.rs       — CLI parsing, thread spawning, orchestration
    config.rs     — Config struct, validation, flag parsing
    thread.rs     — Worker thread: compio event loop + connection management
    http.rs       — HTTP/1.1 request/response codec
    sse.rs        — SSE event parser (line-based)
    ws.rs         — WebSocket frame codec (RFC 6455)
    stats.rs      — HDR histogram, throughput counters, per-thread + merge
    lua.rs        — Lua scripting bridge (mlua)
    numa.rs       — NUMA detection, CPU affinity (sched_setaffinity)
    tui.rs        — ratatui dashboard (optional --tui)
    report.rs     — wrk-compatible text output + JSON
    tls.rs        — TLS via rustls
```

## Dependencies

```toml
[dependencies]
compio = { version = "0.18", features = ["io", "net", "runtime", "macros", "time"] }
mlua = { version = "0.10", features = ["luajit", "vendored"] }
ratatui = "0.29"
crossterm = "0.28"
rustls = "0.23"
libc = "0.2"
clap = { version = "4", features = ["derive"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
hdrhistogram = "7"
```

## Implementation Order

1. **Phase 1: HTTP mode** — CLI, thread pool, HTTP codec, stats, wrk-compatible output. This alone replaces wrk.
2. **Phase 2: NUMA + CPU pinning** — sched_setaffinity, NUMA detection.
3. **Phase 3: Lua scripting** — mlua integration, wrk-compatible callbacks.
4. **Phase 4: SSE mode** — SSE parser, chunk latency, TTFB measurement.
5. **Phase 5: WebSocket mode** — RFC 6455 codec, message RTT.
6. **Phase 6: TUI dashboard** — ratatui live display.
7. **Phase 7: TLS** — rustls integration.
8. **Phase 8: JSON output** — structured results.
