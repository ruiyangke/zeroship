# Runtime limits

The worker-facing per-app limit shape is `AppRuntimeLimits` in [crates/core/src/types.rs](../../crates/core/src/types.rs):

- `cpu_limit_ms`
- `wall_timeout_ms`
- `heap_limit_mb`

The runtime-side shape is `RuntimeLimits` in [crates/runtime/src/core/runtime.rs](../../crates/runtime/src/core/runtime.rs):

- `cpu_limit`
- `wall_timeout`
- `heap_limit_bytes`

Idle GC is separate. It is a `RuntimeBuilder` knob, not part of `AppRuntimeLimits`.

## Current knobs

| Knob | Where it lives | Default |
| --- | --- | --- |
| `cpu_limit_ms` | `AppRuntimeLimits` → `RuntimeLimits.cpu_limit` | unset |
| `wall_timeout_ms` | `AppRuntimeLimits` → `RuntimeLimits.wall_timeout` | unset |
| `heap_limit_mb` | `AppRuntimeLimits` → `RuntimeLimits.heap_limit_bytes` | 128 MB when unset |
| `idle_gc_after_ms(ms)` | `RuntimeBuilder` only | 30 s |

## Request body size

A creator app's inbound request body is capped at **4 MiB**. It is a platform
constant, not a per-app knob: `MAX_REQUEST_BODY_BYTES` in
[crates/core/src/dispatch_frame.rs](../../crates/core/src/dispatch_frame.rs).

The gateway and the worker both enforce it, and they enforce it at two
different sizes on purpose. The gateway applies the cap to the request body it
receives. The worker receives that body wrapped in a dispatch frame - a length
prefix plus a metadata block carrying method, URL and headers - so its own
limit, `MAX_DISPATCH_FRAME_BYTES`, is the body cap plus that overhead. A worker
limit set to the body cap alone would reject requests the gateway had already
accepted.

Both tiers buffer the whole body in memory before dispatching. The cap
multiplied by in-flight concurrency is therefore the worst-case footprint,
which is what keeps this number modest. Large uploads belong in `env.storage`,
where the bytes go straight to object storage instead.

Over the cap, the caller gets `413` with a JSON body. A bare `400` from the
HTTP layer would mean the limit was applied by the framework's own default
rather than by this one.

## CPU limit

CPU enforcement is implemented in [crates/runtime/src/core/cpu_timer.rs](../../crates/runtime/src/core/cpu_timer.rs).

- Linux uses a POSIX timer on `CLOCK_THREAD_CPUTIME_ID`.
- When the timer fires, a watchdog thread calls `v8::IsolateHandle::terminate_execution()`.
- The runtime then cancels the terminating request and surfaces `"CPU time limit exceeded"` for that request.

The fast path is no-op when `cpu_limit` is unset.

## Wall timeout

Wall timeout is stored on `RuntimeLimits` and read through `Runtime::wall_timeout()` in [crates/runtime/src/core/runtime.rs](../../crates/runtime/src/core/runtime.rs). The serve path applies it while waiting for a request to finish in [crates/runtime/src/core/serve.rs](../../crates/runtime/src/core/serve.rs).

Unset means there is no runtime wall-clock cap.

## Heap limit

Heap caps are configured in [crates/runtime/src/core/runtime.rs](../../crates/runtime/src/core/runtime.rs):

- `RuntimeBuilder::heap_limit_mb(mb)` converts MB to bytes
- `RuntimeInner::new_with_plugins(...)` uses `128 * 1024 * 1024` when no cap is supplied
- a near-heap-limit callback grows the cap modestly and terminates execution after 5 consecutive hits

The regression tests live in [crates/runtime/tests/heap_limits.rs](../../crates/runtime/tests/heap_limits.rs).

## Idle GC

Idle GC is builder-only in [crates/runtime/src/core/runtime.rs](../../crates/runtime/src/core/runtime.rs):

- `RuntimeBuilder::idle_gc_after_ms(ms)`
- default constant: `DEFAULT_IDLE_GC_AFTER = 30_000ms`
- `0` disables the idle-GC ticker

When the runtime stays quiet past the threshold, the ticker calls `Isolate::low_memory_notification()`. The regression tests live in [crates/runtime/tests/idle_gc.rs](../../crates/runtime/tests/idle_gc.rs).

## Code map

- [crates/core/src/types.rs](../../crates/core/src/types.rs) — `AppRuntimeLimits`
- [crates/runtime/src/core/runtime.rs](../../crates/runtime/src/core/runtime.rs) — `RuntimeLimits`, `RuntimeBuilder`, heap callback, idle GC
- [crates/runtime/src/core/cpu_timer.rs](../../crates/runtime/src/core/cpu_timer.rs) — Linux CPU timer plumbing
- [crates/runtime/src/core/serve.rs](../../crates/runtime/src/core/serve.rs) — wall-timeout enforcement in the serve path
