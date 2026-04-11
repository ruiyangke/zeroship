# Streaming HTTP Response Design

**Date:** 2026-04-08
**Goal:** Enable streaming HTTP responses (SSE/AI token streaming) so response bodies are sent to clients chunk-by-chunk as they arrive, rather than buffered entirely in memory.

## Scope

**In scope:**
- Fetch pass-through streaming: `return await fetch(ai_api, { stream: true })` — body chunks flow from origin to client without entering V8 after headers
- Two-phase reply: headers sent immediately, body streamed via bounded channel
- Backpressure: slow client → channel fills → upstream pauses
- Streaming fetch reintroduction in fetch.rs (headers first, then chunks)

**Out of scope (follow-up):**
- JS-generated ReadableStream bodies (`new ReadableStream({ start(controller) { ... } })`) — requires JS-side reader pump
- Request body streaming (incoming)
- WebSocket

## Architecture

### Two-phase reply

The oneshot reply type changes from `Result<RequestResult, String>` to `Result<RequestReply, String>`:

```rust
pub enum RequestReply {
    /// Non-streaming: complete body in one shot (existing path).
    Complete(RequestResult),
    /// Streaming: headers immediately, body chunks via channel.
    Stream(HttpStreamResult),
}

pub struct HttpStreamResult {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body_rx: tokio::sync::mpsc::Receiver<Vec<u8>>,
    pub cpu_time: Duration,
    pub logs: Vec<String>,
}
```

### Data flow — fetch pass-through

```
JS: return await fetch("https://api.openai.com/...", { stream: true })

1. fetch() → raw_fetch_callback → spawns fetch task
2. Fetch task sends headers via stream_events_tx:
     OpResult::Completed { op_id, value: "{status, headers, stream_id, __stream: true}" }
3. Fetch task sends body chunks via stream_events_tx:
     OpResult::StreamChunk { stream_id, data, done: false }
     OpResult::StreamChunk { stream_id, data: [], done: true }

4. Runtime receives OpResult::Completed → enter_v8 → resolve fetch promise
   → JS handler returns Response → dispatch result contains __stream marker
5. Runtime detects __stream: creates (body_tx, body_rx), registers StreamForwarder
6. Runtime sends RequestReply::Stream { status, headers, body_rx }

7. Subsequent StreamChunk events → StreamForwarder → body_tx → body_rx → client
   (no V8 entry for pass-through chunks)
```

### Stream events channel

A dedicated `tokio::sync::mpsc` channel for streaming events, separate from the single-future `pending_ops`:

```rust
// In RuntimeState:
pub(crate) stream_events_tx: tokio::sync::mpsc::Sender<OpResult>,

// In Runtime:
stream_events_rx: tokio::sync::mpsc::Receiver<OpResult>,
```

Bounded at capacity 64. Fetch streaming tasks clone `stream_events_tx` and send chunks through it. The Runtime's select! loop has a branch for it:

```rust
Some(event) = self.stream_events_rx.recv() => {
    self.handle_op_result(event);
}
```

This exists alongside `pending_ops` (FuturesUnordered for single-result ops). The distinction:
- `pending_ops`: futures that produce exactly one `OpResult` (buffered fetch, crypto ops)
- `stream_events_rx`: channel for multi-event producers (streaming fetch, future WebSocket)

### StreamForwarder — backpressure-safe body forwarding

```rust
struct StreamForwarder {
    sender: tokio::sync::mpsc::Sender<Vec<u8>>,
    overflow: VecDeque<Vec<u8>>,
    max_overflow: usize,  // 64 chunks
}
```

When a `StreamChunk` arrives and a forwarder exists for that `stream_id`:
1. Drain overflow into sender (FIFO)
2. `try_send(data)` — if Ok, done
3. If Full, push into overflow VecDeque
4. If overflow full (>64 chunks), close the stream (explicit failure)
5. If done, drop the forwarder (signals EOF to body_rx)

Overflow retry: attempted on every subsequent chunk for the same stream. This provides natural batching — if the client catches up, the overflow drains in one pass.

### Fetch streaming reintroduction

`fetch.rs` currently always buffers (`do_fetch_buffered`). The streaming path is reintroduced for responses where content-length is unknown or >1MB:

```rust
async fn do_fetch_streaming(
    method: &str, url: &str, headers_json: &str, body: Option<&str>,
    op_id: u32, stream_id: u32, request_id: Option<u64>,
    stream_events_tx: tokio::sync::mpsc::Sender<OpResult>,
    cancel: Option<CancellationToken>,
) {
    let response = reqwest::get(url).await;
    
    // Send headers as OpResult::Completed (resolves the fetch promise)
    let headers_json = json!({
        "status": status, "headers": resp_headers,
        "stream_id": stream_id, "__stream": true,
        "url": final_url, "redirected": redirected,
    });
    stream_events_tx.send(OpResult::Completed {
        op_id, value: headers_json, request_id,
    }).await;

    // Stream body chunks
    while let Some(chunk) = response.chunk().await {
        if cancel.as_ref().map_or(false, |c| c.is_cancelled()) { break; }
        stream_events_tx.send(OpResult::StreamChunk {
            stream_id, data: chunk.to_vec(), done: false,
        }).await;  // backpressure: blocks if channel full
    }
    stream_events_tx.send(OpResult::StreamChunk {
        stream_id, data: vec![], done: true,
    }).await;
}
```

Decision: buffer vs stream is made in `raw_fetch_callback`:
- Content-length known and <=1MB → `do_fetch_buffered` → pushed into `spawned_ops`
- Otherwise → spawn `do_fetch_streaming` task that sends through `stream_events_tx`

For the server_handle path (multi-threaded I/O): `do_fetch_streaming` is spawned on the server handle. `stream_events_tx` is `Send` (it's `tokio::sync::mpsc`), so it works cross-thread.

### HTTP dispatch JS changes

The HTTP dispatch wrapper in `init.rs` (or `isolate.rs`) needs to detect streaming Response bodies. When `fetch()` returns a Response with `__stream: true` in the resolved JSON, the runtime knows this is a streaming response.

The flow:
1. JS handler calls `fetch(url)` → returns Promise
2. Promise resolves with `{ status, headers, stream_id, __stream: true }`
3. JS handler does `return resp` (or constructs a new Response with the same body)
4. The dispatch function returns the JSON with `__stream` marker
5. Runtime parses the dispatch result, detects `__stream`
6. Creates StreamForwarder for `stream_id`, sends `RequestReply::Stream`

For the HTTP dispatch path specifically:
```js
// Updated HTTP dispatch JS:
var result = handler(req);
if (result && typeof result.then === 'function') {
    return result.then(function(resp) {
        return resp.text().then(function(body) {
            // Check if body contains __stream marker (from fetch)
            try {
                var parsed = JSON.parse(body);
                if (parsed && parsed.__stream) {
                    // Pass through the stream marker
                    var respHeaders = [];
                    resp.headers.forEach(function(v, k) { respHeaders.push([k, v]); });
                    return JSON.stringify({
                        __stream: true,
                        status: resp.status,
                        headers: respHeaders,
                        stream_id: parsed.stream_id
                    });
                }
            } catch(e) {}
            // Non-streaming: return complete body
            var respHeaders = [];
            resp.headers.forEach(function(v, k) { respHeaders.push([k, v]); });
            return JSON.stringify({ status: resp.status, headers: respHeaders, body: body });
        });
    });
}
```

Actually, this approach is fragile — it relies on parsing the body to detect the marker. Better approach: the Response object from fetch should carry metadata about whether it's streaming. This can be done via a non-standard property set by the fetch polyfill:

```js
// In fetch.js polyfill, when fetch resolves with __stream:
resp.__streamId = parsedResult.stream_id;

// In HTTP dispatch:
if (resp.__streamId !== undefined) {
    return JSON.stringify({
        __stream: true,
        status: resp.status,
        headers: respHeaders,
        stream_id: resp.__streamId
    });
}
```

### V8Pool / router changes

`V8Pool::dispatch` currently returns `Result<RpcResult, String>`. For streaming, it needs to return either a complete result or a streaming result:

```rust
pub enum DispatchResult {
    Rpc(RpcResult),
    Stream(HttpStreamResult),
}

pub async fn dispatch(&self, app_id: &str, server_js: &str, body: String)
    -> Result<DispatchResult, String>
{
    // ... send IncomingRequest, await reply ...
    match reply {
        Ok(RequestReply::Complete(result)) => Ok(DispatchResult::Rpc(result.into())),
        Ok(RequestReply::Stream(stream)) => Ok(DispatchResult::Stream(stream)),
        Err(e) => Err(e),
    }
}
```

The router/HTTP handler:
```rust
match pool.dispatch(app_id, server_js, body).await? {
    DispatchResult::Rpc(result) => {
        Response::builder().body(Body::from(result.json))
    }
    DispatchResult::Stream(stream) => {
        let body = Body::from_stream(ReceiverStream::new(stream.body_rx)
            .map(|chunk| Ok::<_, std::io::Error>(Bytes::from(chunk))));
        let mut resp = Response::builder().status(stream.status);
        for (k, v) in &stream.headers {
            resp = resp.header(k.as_str(), v.as_str());
        }
        resp.body(body)
    }
}
```

### Runtime detection of streaming dispatch result

In `handle_incoming_request`, after the JS dispatch returns:

```rust
DispatchResult::Sync(json) => {
    // The json is a JSON-RPC response: {"jsonrpc":"2.0","result":...,"id":...}
    // The __stream marker is inside the "result" field.
    // detect_stream_marker parses the envelope, extracts the result,
    // and checks for __stream: true.
    if let Some(stream_info) = detect_stream_marker(&json) {
        // Create body channel
        let (body_tx, body_rx) = tokio::sync::mpsc::channel(16);
        // Register forwarder
        self.stream_forwarders.insert(stream_info.stream_id, StreamForwarder::new(body_tx));
        // Send streaming reply
        let logs = self.drain_request_logs(id);
        let _ = reply.send(Ok(RequestReply::Stream(HttpStreamResult {
            status: stream_info.status,
            headers: stream_info.headers,
            body_rx,
            cpu_time: cpu_elapsed,
            logs,
        })));
    } else {
        // Non-streaming: existing path
        let _ = reply.send(Ok(RequestReply::Complete(RequestResult { ... })));
    }
}
```

Same for the `Async` path — when a pending promise settles, check the result for `__stream`.

### File changes

| File | Change |
|------|--------|
| `state.rs` | Add `RequestReply`, `HttpStreamResult`, `stream_events_tx` field on RuntimeState |
| `runtime.rs` | Add `stream_events_rx` to Runtime + select! branch, `StreamForwarder` struct with overflow, `detect_stream_marker()`, streaming detection in dispatch result handling |
| `fetch.rs` | Reintroduce `do_fetch_streaming` for large/unknown responses, send via `stream_events_tx`. Decision logic: buffer <=1MB, stream otherwise |
| `isolate.rs` | Update oneshot type to `RequestReply`, unwrap `Complete` variant in `execute_request` and `execute_http` |
| `v8pool.rs` | Return `DispatchResult::Rpc` or `DispatchResult::Stream`, handle `RequestReply` enum |
| `server.rs` | Update for `RequestReply` type change |
| `embed/fetch.js` | Set `__streamId` on Response when fetch resolves with stream marker |
| `init.rs` or `isolate.rs` | HTTP dispatch JS detects `__streamId`, returns `__stream` marker |
| `benches/v8_qps.rs` | Update for `RequestReply` type change |

### Success criteria

1. All existing 88 + 29 tests pass (no regression)
2. Fetch pass-through: `curl -N http://localhost:4000/rpc` with an SSE-producing handler shows tokens arriving incrementally
3. Backpressure: slow client does not cause OOM — memory stays bounded
4. No V8 entry for pass-through stream chunks after headers
5. StreamForwarder overflow: full overflow closes stream explicitly (no silent data loss)
6. Cancellation: client disconnect cancels the upstream fetch
