# Runtime and Worker Review

This review focuses on the `crates/runtime` and `crates/worker` paths with emphasis on security and performance. The repo was already dirty in unrelated areas, so this was a read-only code review against the current state.

## Findings

### 1. High: the worker exposes unauthenticated code-execution endpoints and trusts caller-supplied identity headers

- [crates/worker/src/main.rs](/home/ruiyang/Projects/appbase/crates/worker/src/main.rs:61) registers `/dispatch/{app_id}` and `/http-dispatch/{app_id}` on `0.0.0.0` with no authentication or source validation.
- [crates/worker/src/handler.rs](/home/ruiyang/Projects/appbase/crates/worker/src/handler.rs:51) and [crates/worker/src/handler.rs](/home/ruiyang/Projects/appbase/crates/worker/src/handler.rs:177) accept `zeroship-user` directly from the request and inject it into runtime auth state.

If this worker port is reachable outside a strictly private network, any caller can impersonate arbitrary users and invoke app code directly. This is the most serious issue in the current design.

#### Improvements

- Bind the worker to loopback or UDS by default, not `0.0.0.0`.
- Require authenticated internal transport between gateway and worker.
- Sign or MAC the forwarded user context instead of trusting a plain header.
- Reject direct external traffic at the worker even if network policy is expected to protect it.

### 2. High: outbound fetch SSRF protection is incomplete and can be bypassed

- [crates/runtime/src/fetch.rs](/home/ruiyang/Projects/appbase/crates/runtime/src/fetch.rs:26) validates only the original URL string.
- It blocks literal IPs and `localhost`, but does not resolve DNS before connecting.
- Redirect handling is only observed after completion at [crates/runtime/src/fetch.rs](/home/ruiyang/Projects/appbase/crates/runtime/src/fetch.rs:262), so redirect hops are not revalidated.

This means a hostile public hostname that resolves to a private address can pass validation, and a public URL can potentially redirect into internal address space.

#### Improvements

- Resolve hostnames and validate every resolved IP before connecting.
- Revalidate every redirect hop, not just the initial URL.
- Block private, loopback, link-local, multicast, and other non-routable ranges after resolution.
- Prefer an explicit egress allowlist or outbound proxy over string-based deny logic.

### 3. High: worker timeouts do not actually cancel runtime work

- Worker handlers time out locally in polling loops at [crates/worker/src/handler.rs](/home/ruiyang/Projects/appbase/crates/worker/src/handler.rs:92) and [crates/worker/src/handler.rs](/home/ruiyang/Projects/appbase/crates/worker/src/handler.rs:204).
- Requests store `CancelFlag::new()` at [crates/runtime/src/runtime.rs](/home/ruiyang/Projects/appbase/crates/runtime/src/runtime.rs:725) and [crates/runtime/src/runtime.rs](/home/ruiyang/Projects/appbase/crates/runtime/src/runtime.rs:815).
- `cleanup_cancelled_requests()` checks cancellation at [crates/runtime/src/runtime.rs](/home/ruiyang/Projects/appbase/crates/runtime/src/runtime.rs:1458), but I did not find a caller that ever invokes `cancel()`.
- [crates/runtime/src/state.rs](/home/ruiyang/Projects/appbase/crates/runtime/src/state.rs:209) explicitly documents `executing_request_cancel` as unused in the compio path.

So the client may receive a timeout while the isolate, timers, and fetches continue running in the background. That wastes CPU, memory, network resources, and makes overload recovery worse.

#### Improvements

- Propagate timeout and disconnect signals into the runtime.
- Make fetch, timers, and stream work observe cancellation and stop promptly.
- Remove dead cancellation scaffolding if it is not going to be used.
- Add tests that verify timed-out requests actually stop doing work.

### 4. Medium-High: per-thread caches multiply memory use and cold-start work by worker count

- The cache is thread-local at [crates/worker/src/cache.rs](/home/ruiyang/Projects/appbase/crates/worker/src/cache.rs:20).
- Each ntex worker initializes its own cache and sync loop in [crates/worker/src/main.rs](/home/ruiyang/Projects/appbase/crates/worker/src/main.rs:61).

With `N` worker threads, the same app can be loaded `N` times, each with its own V8 heap, warmup, pump task, and sync traffic. `MAX_ISOLATES=200` is therefore a per-thread limit, not a process-level limit.

#### Improvements

- Make the cache process-wide or explicitly shard apps to threads.
- Ensure each app is loaded once per process unless duplication is deliberate.
- Revisit `MAX_ISOLATES` semantics so the configured limit matches operator expectations.
- Avoid running a full sync loop per worker thread.

### 5. Medium-High: the worker burns CPU waiting for async results instead of sleeping on a waker

- The worker loops on `try_recv()` plus `yield_now().await` at [crates/worker/src/handler.rs](/home/ruiyang/Projects/appbase/crates/worker/src/handler.rs:97), [crates/worker/src/handler.rs](/home/ruiyang/Projects/appbase/crates/worker/src/handler.rs:211), and [crates/worker/src/handler.rs](/home/ruiyang/Projects/appbase/crates/worker/src/handler.rs:238).
- The result channel is only `Rc<Cell<Option<T>>>` at [crates/runtime/src/channel.rs](/home/ruiyang/Projects/appbase/crates/runtime/src/channel.rs:18), so there is no wakeup-based blocking.

This converts idle waiting into scheduler churn. It will get expensive under load or when many requests are pending on async work.

#### Improvements

- Replace the custom result slot with a real wakeable oneshot.
- Use awaitable completion instead of spin-yield loops.
- Add profiling around pending async request counts and scheduler overhead.

### 6. Medium: stream buffering is unbounded, so slow clients can cause memory growth

- `StreamWriter::push()` appends to an unbounded queue at [crates/runtime/src/channel.rs](/home/ruiyang/Projects/appbase/crates/runtime/src/channel.rs:58).
- `StreamForwarder::try_forward()` always succeeds at [crates/runtime/src/runtime.rs](/home/ruiyang/Projects/appbase/crates/runtime/src/runtime.rs:108) and explicitly avoids backpressure.

If a producer is faster than the downstream client, buffered chunks can grow without limit and turn streaming responses into a memory DoS vector.

#### Improvements

- Put a hard cap on queued bytes per stream.
- Add backpressure, drop policy, or abort policy for slow consumers.
- Expose metrics for queued stream bytes and dropped or aborted streams.

### 7. Medium: the control-plane HTTP client is too primitive for a security-sensitive path

- [crates/worker/src/sync.rs](/home/ruiyang/Projects/appbase/crates/worker/src/sync.rs:95) performs raw TCP instead of using a hardened HTTP client.
- It assumes default port 80 at [crates/worker/src/sync.rs](/home/ruiyang/Projects/appbase/crates/worker/src/sync.rs:102).
- It only sends `parsed.path()` at [crates/worker/src/sync.rs](/home/ruiyang/Projects/appbase/crates/worker/src/sync.rs:103), ignoring query strings.
- It treats everything after `\r\n\r\n` as the full response body at [crates/worker/src/sync.rs](/home/ruiyang/Projects/appbase/crates/worker/src/sync.rs:126), which is not a correct HTTP parser for chunked or compressed responses.

This is brittle even before the security angle. If `CONTROL_URL` is ever plain HTTP outside localhost, the bearer token is exposed on the wire.

#### Improvements

- Replace the hand-written client with a proper HTTP client implementation.
- Require HTTPS for non-local control plane communication.
- Validate response status and body framing correctly.
- Add timeouts, retries with limits, and connection reuse.

### 8. Medium: async work limits exist on paper but are not enforced

- `MAX_PENDING_OPS` is defined at [crates/runtime/src/state.rs](/home/ruiyang/Projects/appbase/crates/runtime/src/state.rs:86), but I did not find enforcement.

Without hard admission control, a hostile or buggy app can queue more async work than the runtime can drain, especially combined with public worker endpoints and missing cancellation.

#### Improvements

- Enforce maximum in-flight async ops per runtime and per request.
- Reject or shed new work once limits are reached.
- Add metrics and logging when limits trigger.

### 9. Medium-Low: the runtime relies on a fragile `unsafe impl Send` contract

- [crates/runtime/src/runtime.rs](/home/ruiyang/Projects/appbase/crates/runtime/src/runtime.rs:228) declares `unsafe impl Send for Runtime {}` even though it owns V8 state and `Rc<RefCell<...>>`.

The comment says the runtime stays on one compio thread, but the type system no longer enforces that invariant. This is the kind of assumption that can be broken later by refactor or by a new executor behavior.

#### Improvements

- Avoid `unsafe impl Send` if possible.
- If it must remain, isolate the invariant more aggressively and document exactly what executor guarantees it depends on.
- Add tests or debug assertions that detect cross-thread misuse.

### 10. Low: app `console.log` writes directly to process stdout

- [crates/runtime/src/init.rs](/home/ruiyang/Projects/appbase/crates/runtime/src/init.rs:231) prints application-controlled log lines directly.

This is noisy, slows hot paths, and allows log injection or formatting abuse. It is not a catastrophic issue, but it is poor operational hygiene.

#### Improvements

- Route logs through structured logging.
- Attach app and request metadata.
- Apply rate limits and truncation.

## Priority Order

If I were fixing this in sequence, I would do it in this order:

1. Lock down worker authentication and default bind behavior.
2. Fix SSRF and redirect validation in runtime fetch.
3. Implement real cancellation for timed-out and disconnected requests.
4. Replace spin-wait result handling with wakeable primitives.
5. Eliminate per-thread cache duplication and redundant sync loops.
6. Add hard bounds for async ops, stream buffering, and resource usage.
7. Replace the hand-rolled control-plane HTTP client.

## Overall Assessment

The code has a strong bias toward simplicity and fast local progress, but that simplicity is currently buying risk in the wrong places. The biggest theme is that trust boundaries are underdefined: the worker trusts its caller too much, the runtime trusts outbound network destinations too much, and resource-lifecycle controls are not strong enough once async work escapes the immediate request path.

The performance story is also mixed. There are some deliberate fast-path optimizations in the runtime, but the worker side undermines that with per-thread duplication, spin-wait polling, and unbounded buffering. The result is a design that may look fast in happy-path microbenchmarks while becoming fragile under load or adversarial traffic.
