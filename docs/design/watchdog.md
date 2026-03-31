# Watchdog & Execution Limits — Design Document

## Problem

A single `while(true){}` in user JS permanently hangs an isolate thread. No CPU or wall-time limits are enforced. The current per-isolate watchdog thread doesn't scale (1000 apps = 1000 extra threads).

## Requirements

1. **1 watchdog for all isolates** — O(1) threads, not O(N)
2. **Wall-time AND CPU-time limits** — wall catches slow I/O, CPU catches tight loops
3. **Per-plan configurable** — free=10s wall/50ms CPU, pro=30s/5s, enterprise=300s/30s
4. **Per-request tracking** — attribute timeout to the specific request
5. **Two-phase: warn then kill** — callback at 80%, terminate at 100%
6. **Graceful recovery** — isolate stays alive, serves next request after timeout
7. **Integration with metering** — timeout recorded as CPU/wall usage

## Architecture

```
                        GlobalWatchdog (1 thread)
                        ┌──────────────────────────┐
                        │ entries: DashMap<AppId,   │
                        │   WatchdogEntry {         │
                        │     v8_handle,            │
                        │     thread_id,            │
                        │     wall_limit,           │
                        │     cpu_limit,            │
                        │     state: AtomicU8,      │
                        │     last_activity: AtomicU64, │
                        │   }                       │
                        │ >                         │
                        │                           │
                        │ loop every 500ms:         │
                        │   for each entry:         │
                        │     wall = now - last_activity │
                        │     cpu = read_thread_cpu(thread_id) │
                        │     if cpu > cpu_limit:   │
                        │       terminate(v8_handle)│
                        │     elif wall > wall_limit: │
                        │       terminate(v8_handle)│
                        └──────────────────────────┘
                              ▲          │
                    register/ │          │ terminate_execution()
                    unregister│          │
                    ping      │          ▼
                        ┌─────┴──────────────────┐
                        │ V8 Actor threads       │
                        │                        │
                        │ app1: v8 thread         │
                        │ app2: v8 thread         │
                        │ ...                     │
                        └────────────────────────┘
```

## Cross-Thread CPU Time Reading

Linux allows reading another thread's CPU time via `pthread_getcpuclockid`:

```rust
fn thread_cpu_clock_id(thread: libc::pthread_t) -> libc::clockid_t {
    let mut clock_id: libc::clockid_t = 0;
    unsafe { libc::pthread_getcpuclockid(thread, &mut clock_id) };
    clock_id
}

fn read_thread_cpu_time(clock_id: libc::clockid_t) -> Duration {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    unsafe { libc::clock_gettime(clock_id, &mut ts) };
    Duration::new(ts.tv_sec as u64, ts.tv_nsec as u32)
}
```

This lets the watchdog measure CPU time of ANY V8 thread from its own thread. No signal handling needed.

## WatchdogEntry Lifecycle

```
IsolatePool.get_or_create("app1"):
  1. Spawn V8 actor thread
  2. Actor registers with GlobalWatchdog:
     watchdog.register("app1", v8_handle, thread_id, wall_limit, cpu_limit)

Actor receives RPC request:
  3. watchdog.start_request("app1")  // records start time + cpu baseline

Actor completes RPC response:
  4. watchdog.end_request("app1")    // clears active request

Watchdog tick (every 500ms):
  5. For each entry with active request:
     - Read CPU: clock_gettime(entry.cpu_clock_id) - entry.cpu_baseline
     - Read wall: Instant::now() - entry.request_start
     - If CPU > cpu_limit OR wall > wall_limit:
       entry.v8_handle.terminate_execution()
       entry.state = Terminated

IsolatePool evicts "app1":
  6. watchdog.unregister("app1")

Actor detects termination:
  7. V8 event loop returns error
  8. Drain pending replies with "Execution limit exceeded"
  9. Actor recovers (V8 can serve next request after CancelTerminateExecution)
```

## State Machine per Entry

```
    register()
        │
        ▼
    ┌─ Idle ◄───────────────────────┐
    │                               │
    │  start_request()              │  end_request()
    │                               │
    ▼                               │
    Active ──── timeout ──► Terminated
    │                          │
    │                          │ actor handles error,
    │                          │ calls recover()
    │                          │
    └──────────────────────────┘
```

## Per-Request Tracking

In concurrent mode, multiple requests are in-flight. The watchdog tracks the OLDEST active request:

```rust
struct WatchdogEntry {
    app_id: String,
    v8_handle: v8::IsolateHandle,
    cpu_clock_id: libc::clockid_t,   // pre-computed, cached
    wall_limit: Duration,
    cpu_limit: Duration,

    // Per-request tracking (oldest request wins)
    oldest_request_start: AtomicU64,  // Instant as epoch nanos, 0 = no active request
    cpu_at_oldest_start: AtomicU64,   // CPU nanos at request start, for delta
    active_request_count: AtomicU32,  // number of in-flight requests
}
```

When the first request arrives (`active_request_count` goes 0→1), record the start times.
When the last request completes (`active_request_count` goes 1→0), clear the start times.
The watchdog checks the OLDEST request's wall/CPU time — if any request exceeds the limit, ALL in-flight requests are terminated (V8 is single-threaded, can't terminate just one).

## Configuration (from plan)

```toml
[plans.free.limits]
wall_time_ms = 10000     # 10s wall time per request
cpu_time_ms = 50         # 50ms CPU time per request (CF Workers free tier)

[plans.pro.limits]
wall_time_ms = 30000     # 30s wall time
cpu_time_ms = 5000       # 5s CPU time

[plans.enterprise.limits]
wall_time_ms = 300000    # 5 min wall time
cpu_time_ms = 30000      # 30s CPU time
```

## Recovery After Termination

V8's `TerminateExecution()` raises an uncatchable pseudo-exception. After catching it:

```rust
// In the actor's poll_fn, when event loop returns error:
Poll::Ready(Err(e)) => {
    if is_termination_error(&e) {
        // V8 was terminated by watchdog
        // Cancel the termination so V8 can serve the next request
        runtime.v8_isolate().cancel_terminate_execution();

        // Drain pending replies with error
        for (_, tx) in pending_replies.borrow_mut().drain() {
            let _ = tx.send(Err("Execution time limit exceeded".into()));
        }

        // DON'T exit — the isolate can recover and serve the next request
        // Re-enter the poll_fn loop
        cx.waker().wake_by_ref();
        Poll::Pending
    } else {
        // Actual JS error — exit
        Poll::Ready(())
    }
}
```

This is what workerd does — isolates survive CPU timeouts and continue serving. Only heap limit violations are fatal.

## Implementation Plan

1. Create `crates/isolate/src/watchdog.rs` — GlobalWatchdog struct
2. Instantiate once in the pool (or server layer)
3. Actor registers on startup, unregisters on shutdown
4. Actor calls start_request/end_request around dispatch
5. Watchdog thread polls every 500ms using cross-thread CPU measurement
6. Remove per-isolate watchdog threads from actor.rs
