# zerobench Phase 2: SSE, WebSocket, Lua, TUI

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Add SSE streaming, WebSocket, Lua scripting, and TUI dashboard to the zerobench HTTP benchmark tool.

**Architecture:** Each protocol mode is a separate module with its own connection lifecycle. Lua hooks into all modes via callbacks. TUI runs on a dedicated thread reading stats snapshots.

**Tech Stack:** compio, mlua (LuaJIT), ratatui + crossterm, tungstenite (WebSocket)

---

## File Structure

```
tools/bench/src/
  main.rs        — (modify) add --sse, --ws, --tui flags, dispatch to protocol mode
  config.rs      — (modify) add sse/ws/tui config fields
  thread.rs      — (modify) dispatch to http/sse/ws worker based on mode
  sse.rs         — SSE connection lifecycle: TTFB, chunk parsing, chunk latency
  ws.rs          — WebSocket connection lifecycle: upgrade, msg send/recv, RTT
  lua.rs         — mlua bridge: wrk-compatible callbacks + SSE/WS extensions
  tui.rs         — ratatui live dashboard: throughput graph, latency histogram
  stats.rs       — (modify) add SSE stats (chunks, TTFB) and WS stats (messages, RTT)
  report.rs      — (modify) add SSE and WS report formats
```

---

### Task 1: SSE stats + report

**Files:**
- Modify: `tools/bench/src/stats.rs` — add SseThreadStats, SseSummary
- Modify: `tools/bench/src/report.rs` — add print_sse_format

Add SSE-specific stats alongside the existing HTTP stats:
- `ttfb_histogram` — time to first byte per connection
- `chunk_latency_histogram` — time between consecutive chunks
- `total_chunks` — total chunks received across all connections
- `active_streams` / `completed_streams`

SSE report format:
```
Running 30s SSE test @ http://localhost:8080/stream
  50 connections
  TTFB        Avg      p50      p99      Max
              1.2ms    0.9ms    4.1ms    12.3ms
  Chunks/s    312,450 (total)   6,249 (per conn)
  Chunk Lat   0.01ms   0.01ms   0.08ms   1.2ms
  15,622,500 chunks in 30.00s
```

---

### Task 2: SSE protocol handler

**Files:**
- Create: `tools/bench/src/sse.rs`
- Modify: `tools/bench/src/config.rs` — add `sse: bool` field
- Modify: `tools/bench/src/thread.rs` — dispatch to SSE worker when `config.sse`
- Modify: `tools/bench/src/main.rs` — add `--sse` flag

SSE connection lifecycle:
1. TCP connect
2. Send GET request with `Accept: text/event-stream`
3. Read response headers → record TTFB
4. Loop: read lines, parse SSE events (`data:`, `event:`, `id:`)
5. Record chunk latency (time between data: lines)
6. Until `data: [DONE]` or duration expires or connection closes

SSE line parser: buffer-based, splits on `\n`, handles `data:`, `event:`, `id:`, `retry:` fields.

---

### Task 3: WebSocket protocol handler

**Files:**
- Create: `tools/bench/src/ws.rs`
- Modify: `tools/bench/src/config.rs` — add `ws: bool` field
- Modify: `tools/bench/src/stats.rs` — add WsThreadStats
- Modify: `tools/bench/src/thread.rs` — dispatch to WS worker
- Modify: `tools/bench/src/main.rs` — add `--ws` flag
- Modify: `tools/bench/src/report.rs` — add print_ws_format

WebSocket connection lifecycle:
1. TCP connect
2. HTTP upgrade handshake (key exchange per RFC 6455)
3. Loop: send message → read response → record RTT
4. Until duration expires

Minimal RFC 6455 implementation:
- Frame header parsing (opcode, length, masking)
- Client mask generation (random 4 bytes)
- Text frame send/receive
- Ping/pong handling
- Close frame handling

WS report format:
```
Running 10s WebSocket test @ ws://localhost:8080/echo
  100 connections
  Msg Stats    Avg      p50      p99      Max
    RTT       0.15ms   0.12ms   0.45ms   2.1ms
    Msg/Sec   85,200
  852,000 messages in 10.00s
```

---

### Task 4: Lua scripting

**Files:**
- Create: `tools/bench/src/lua.rs`
- Modify: `tools/bench/Cargo.toml` — add mlua dependency
- Modify: `tools/bench/src/thread.rs` — call Lua hooks
- Modify: `tools/bench/src/main.rs` — load script file

wrk-compatible Lua callbacks:
- `setup(thread)` — called once per thread at start
- `init(args)` — called once globally
- `request()` — return custom request bytes (HTTP mode)
- `response(status, headers, body)` — process response (HTTP mode)
- `done(summary, latency, requests)` — custom reporting

Extended callbacks:
- `chunk(data)` — SSE mode, called per chunk
- `message()` — WS mode, return message to send
- `on_message(data)` — WS mode, process received message

wrk global object:
- `wrk.method`, `wrk.path`, `wrk.headers`, `wrk.body`
- `wrk.format()` — build HTTP request from current state
- `wrk.thread`, `wrk.connections`

---

### Task 5: TUI dashboard

**Files:**
- Create: `tools/bench/src/tui.rs`
- Modify: `tools/bench/Cargo.toml` — add ratatui + crossterm
- Modify: `tools/bench/src/main.rs` — spawn TUI thread when --tui
- Modify: `tools/bench/src/stats.rs` — add StatsSnapshot for real-time reporting

TUI layout (ratatui + crossterm):
- Top bar: URL, threads, connections, elapsed time
- Main area: throughput graph (sparkline or line chart)
- Middle: latency percentile bars
- Bottom: totals (requests, errors, bytes)
- Updates every 100ms
- Graceful terminal restore on exit (crossterm raw mode)

StatsSnapshot: a lightweight struct that threads publish periodically (every 100ms) for the TUI to read. Uses atomic counters or a channel.

---

### Task 6: TLS support

**Files:**
- Create: `tools/bench/src/tls.rs`
- Modify: `tools/bench/Cargo.toml` — add compio-tls or rustls
- Modify: `tools/bench/src/thread.rs` — TLS handshake for https:// URLs

TLS via compio's rustls integration or raw rustls:
- Auto-detect from URL scheme (https:// → TLS)
- `--tls` flag to force TLS
- Certificate verification (skip with `--insecure` if needed)

---

### Task 7: Integration tests + benchmarks

- Test SSE against our v8-server-compio `/sse` endpoint
- Test WebSocket against our v8-server-compio WebSocket endpoint
- Test Lua scripting with wrk-compatible scripts
- Test TUI renders without crashing (headless mode)
- Compare zerobench SSE vs our custom sse_bench.rs tool
- Run full benchmark suite and save results
