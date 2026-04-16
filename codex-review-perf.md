# Runtime and Worker Async/Performance Review

This review is a second pass focused specifically on async processing, non-blocking behavior, wakeups, buffering, and hot-path performance in `crates/runtime` and `crates/worker`.

## Findings

### 1. High: async completion is implemented as spin-wait polling, which wastes CPU under latency

- The worker loops on `rx.try_recv()` with `yield_now().await` at:
  - [crates/worker/src/handler.rs](/home/ruiyang/Projects/appbase/crates/worker/src/handler.rs:93)
  - [crates/worker/src/handler.rs](/home/ruiyang/Projects/appbase/crates/worker/src/handler.rs:207)
  - [crates/worker/src/handler.rs](/home/ruiyang/Projects/appbase/crates/worker/src/handler.rs:234)
- The runtime server does the same in:
  - [crates/runtime/src/serve.rs](/home/ruiyang/Projects/appbase/crates/runtime/src/serve.rs:408)
  - [crates/runtime/src/serve.rs](/home/ruiyang/Projects/appbase/crates/runtime/src/serve.rs:485)
- The underlying primitive is a custom `Rc<Cell<Option<T>>>` result slot at [crates/runtime/src/channel.rs](/home/ruiyang/Projects/appbase/crates/runtime/src/channel.rs:18), which has no waker support.

This turns waiting on async work into scheduler churn. Latency is paid in CPU instead of sleeping efficiently.

#### Improvements

- Replace `ResultSlot` with a real wakeable oneshot.
- Remove all `try_recv()` polling loops.
- Make handlers await completion directly instead of yielding or sleeping in short intervals.

### 2. High: worker cache and sync architecture duplicates async overhead per worker thread

- Each ntex worker initializes `cache::init_cache` and `sync::start_sync` in [crates/worker/src/main.rs](/home/ruiyang/Projects/appbase/crates/worker/src/main.rs:61).
- The cache is thread-local at [crates/worker/src/cache.rs](/home/ruiyang/Projects/appbase/crates/worker/src/cache.rs:20).
- Every loaded app creates its own runtime, warmup, and pump task at [crates/worker/src/cache.rs](/home/ruiyang/Projects/appbase/crates/worker/src/cache.rs:102) and [crates/worker/src/cache.rs](/home/ruiyang/Projects/appbase/crates/worker/src/cache.rs:125).

That means one app can be loaded once per worker thread. Memory use, cold-start cost, and background polling work all scale with thread count, not actual app demand.

#### Improvements

- Make the app cache process-wide or explicitly shard apps to specific threads.
- Avoid running one sync loop per worker thread.
- Make `MAX_ISOLATES` a process-level concept or rename it so operators are not misled.

### 3. High: the async fetch path fully buffers responses, increasing latency and memory pressure

- Fetch work is converted into futures and queued at [crates/runtime/src/runtime.rs](/home/ruiyang/Projects/appbase/crates/runtime/src/runtime.rs:890).
- `execute_fetch()` buffers the entire upstream body before completion at [crates/runtime/src/fetch.rs](/home/ruiyang/Projects/appbase/crates/runtime/src/fetch.rs:196).
- The body is fully read with `response.bytes().await` at [crates/runtime/src/fetch.rs](/home/ruiyang/Projects/appbase/crates/runtime/src/fetch.rs:283).

So even though fetch is async, the response is not streamed through the runtime. Large or slow responses increase tail latency and hold memory much longer than necessary.

#### Improvements

- Add a streaming fetch path backed by the existing stream infrastructure.
- Resolve the JS promise as soon as headers are ready and expose a stream for the body.
- Keep full buffering only for explicitly small-body cases.

### 4. Medium-High: zero-delay timer handling uses O(n) front removal

- `ready_timers.remove(0)` is used in:
  - [crates/runtime/src/runtime.rs](/home/ruiyang/Projects/appbase/crates/runtime/src/runtime.rs:1183)
  - [crates/runtime/src/runtime.rs](/home/ruiyang/Projects/appbase/crates/runtime/src/runtime.rs:1206)

This shifts the whole vector on every timer. Timer-heavy workloads, especially with many `setTimeout(0)` callbacks, will pay quadratic costs.

#### Improvements

- Change `ready_timers` from `Vec<u32>` to `VecDeque<u32>`.
- Use `pop_front()` instead of `remove(0)`.

### 5. Medium-High: stream buffering and reads use inefficient queue operations and extra copying

- JS stream reads remove buffered chunks with `stream.buffer.remove(0)` at [crates/runtime/src/streams.rs](/home/ruiyang/Projects/appbase/crates/runtime/src/streams.rs:108).
- Server-side stream readers drain full queues into fresh vectors at [crates/runtime/src/channel.rs](/home/ruiyang/Projects/appbase/crates/runtime/src/channel.rs:84).
- Chunked HTTP responses rebuild framing buffers for every chunk at [crates/runtime/src/serve.rs](/home/ruiyang/Projects/appbase/crates/runtime/src/serve.rs:467) and [crates/runtime/src/serve.rs](/home/ruiyang/Projects/appbase/crates/runtime/src/serve.rs:511).

This causes avoidable allocation and copying overhead, especially for long-lived SSE or streaming responses.

#### Improvements

- Use `VecDeque<Vec<u8>>` for stream buffers.
- Avoid `remove(0)` and full `drain()` collections in hot paths.
- Reuse chunk framing buffers where possible.

### 6. Medium-High: the surrounding async plumbing undermines the pump loop’s efficiency

- The pump loop itself is mostly event-driven at [crates/runtime/src/runtime.rs](/home/ruiyang/Projects/appbase/crates/runtime/src/runtime.rs:368).
- But handlers, stream forwarding, and websocket delivery keep converting queues into temporary collections and polling manually.
- WebSocket outgoing messages are drained into a temporary vector at [crates/runtime/src/serve.rs](/home/ruiyang/Projects/appbase/crates/runtime/src/serve.rs:776).

The architecture is optimized in the center and wasteful at the edges. The net effect is extra allocations and scheduling overhead in real traffic.

#### Improvements

- Replace more temporary vector drains with incremental queue consumption.
- Standardize on wakeable, event-driven primitives across worker and runtime.
- Profile allocations per request under streaming and websocket workloads.

### 7. Medium: standalone runtime server has head-of-line blocking per connection

- `handle_connection()` serially parses requests and fully awaits each response in [crates/runtime/src/serve.rs](/home/ruiyang/Projects/appbase/crates/runtime/src/serve.rs:147).

That is simple, but pipelined requests on the same socket will not make progress independently. One slow async request can stall later requests behind it on that connection.

#### Improvements

- Either explicitly disable pipelining assumptions or document that requests are strictly sequential per socket.
- Consider request caps and connection-level backpressure if this server is used beyond local/dev scenarios.

### 8. Medium: request buffering in the runtime server has no obvious hard cap

- `handle_connection()` keeps appending into `data` until a complete request is available at [crates/runtime/src/serve.rs](/home/ruiyang/Projects/appbase/crates/runtime/src/serve.rs:151).
- `content-length` is trusted for body assembly at [crates/runtime/src/serve.rs](/home/ruiyang/Projects/appbase/crates/runtime/src/serve.rs:182).

From a performance perspective this is risky. Slow or oversized requests can pin memory and a task for too long.

#### Improvements

- Add explicit limits on headers, request body size, and total buffered bytes per connection.
- Fail fast on oversized requests before allocating more.

### 9. Medium: cancellation remains largely ineffective, so async work outlives the request

- Fetch ignores cancellation in [crates/runtime/src/fetch.rs](/home/ruiyang/Projects/appbase/crates/runtime/src/fetch.rs:194).
- Worker timeout paths stop waiting but do not stop the underlying async work.

This is a performance issue as much as a correctness issue. Timed-out or disconnected requests can keep consuming upstream sockets, timers, pump capacity, and memory.

#### Improvements

- Propagate disconnect and timeout signals into runtime async work.
- Make fetch, timers, and streams cancelable.
- Add tests that prove abandoned work is actually stopped.

### 10. Medium: warmup and startup costs are synchronous and duplicated in expensive places

- Standalone runtime warms one isolate at [crates/runtime/src/serve.rs](/home/ruiyang/Projects/appbase/crates/runtime/src/serve.rs:989).
- Worker warms each loaded isolate at [crates/worker/src/cache.rs](/home/ruiyang/Projects/appbase/crates/worker/src/cache.rs:110).

The standalone server case is acceptable. The worker case becomes costly because of per-thread cache duplication.

#### Improvements

- Avoid duplicate warmups for the same app across worker threads.
- Cache initialization results at process scope where possible.

### 11. Medium-Low: some performance-oriented primitives are defined but not really enforced

- `MAX_PENDING_OPS` exists in [crates/runtime/src/state.rs](/home/ruiyang/Projects/appbase/crates/runtime/src/state.rs:86), but I do not see strong admission control tied to it.
- `init_async()` exists in [crates/worker/src/cache.rs](/home/ruiyang/Projects/appbase/crates/worker/src/cache.rs:38), but does not appear to be wired into startup.

These are signs that resource management is only partially implemented.

#### Improvements

- Enforce in-flight async work limits per runtime and per request.
- Make async resource initialization explicit and deterministic.

## Strong Parts

There are good ideas here:

- The runtime pump loop is mostly event-driven and conceptually sound: [crates/runtime/src/runtime.rs](/home/ruiyang/Projects/appbase/crates/runtime/src/runtime.rs:368).
- Stream readers in the worker use proper waker-based waiting: [crates/worker/src/handler.rs](/home/ruiyang/Projects/appbase/crates/worker/src/handler.rs:312).
- The WebSocket pump avoids some generic `select!` overhead with a custom combined future: [crates/runtime/src/serve.rs](/home/ruiyang/Projects/appbase/crates/runtime/src/serve.rs:864).

The main issue is that these strengths are not applied consistently across the full request lifecycle.

## Priority Order

If I were improving this specifically for async throughput and non-blocking behavior, I would do it in this order:

1. Replace result-slot polling with wakeable oneshots.
2. Eliminate per-thread worker cache duplication.
3. Stream fetch responses instead of fully buffering them.
4. Replace `Vec::remove(0)` queues with `VecDeque`.
5. Add real cancellation for timed-out and disconnected requests.
6. Add hard bounds for buffered request and stream memory.
7. Reduce temporary allocations in stream and websocket pumps.

## Overall Assessment

The pump loop is the healthiest part of the design. The code around it is where the performance debt lives: manual polling, duplicated background work, inefficient queue choices, and full buffering where streaming should exist.

The result is a system that can look fast in happy-path microbenchmarks but will waste CPU and memory under realistic async load, especially with many pending requests, long-lived streams, or slow upstream dependencies.
