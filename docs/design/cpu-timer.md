# CPU Time Enforcement — Design Document

## Problem

V8 isolates need CPU time limits (free tier: 50ms, pro: 5s). The current global
watchdog polls every 500ms — too coarse for 50ms limits (up to 10x overshoot).

## Solution: POSIX CPU Timer + Shared Pipe + 1 Watchdog Thread

```
POSIX timer (CLOCK_THREAD_CPUTIME_ID)       1 shared pipe       1 watchdog thread
  per V8 thread, armed per active period       (2 fds total)        (parked on read)
              │                                     │                      │
              │ CPU limit hit                       │                      │
              ▼                                     ▼                      ▼
  Signal handler (SIGRTMIN+1)              write(pipe, id)         read(pipe, id)
  [async-signal-safe only]                 [signal-safe]           [normal thread]
    1. flag.store(true)                                              │
    2. write(pipe_fd, app_id) ──────────────────────────────────────►│
                                                                     │
                                                       v8_handle.terminate_execution()
                                                       [safe: normal thread context]
                                                                     │
                                                                     ▼
                                                       V8 checks flag at next back-edge
                                                       → uncatchable exception → unwind
```

## Why Not Call TerminateExecution From the Signal Handler

V8's `TerminateExecution()` acquires a mutex internally (`ExecutionAccess`).
If the signal fires while V8 already holds that mutex (on the same thread),
the signal handler deadlocks trying to acquire it again.

The pipe-to-watchdog pattern avoids this: the signal handler only does
async-signal-safe operations (atomic store + pipe write). The watchdog thread
calls `terminate_execution()` in normal context where mutexes work correctly.

## Architecture

### Per V8 actor thread (created once at isolate startup)

```rust
struct CpuTimer {
    timer_id: libc::timer_t,     // POSIX timer tracking this thread's CPU
    thread_tid: i32,             // Linux kernel thread ID (gettid)
}
```

- `timer_create(CLOCK_THREAD_CPUTIME_ID, SIGEV_THREAD_ID → SIGRTMIN+1)`
- The timer measures CPU time of the V8 thread only (not wall time, not other threads)
- Created on the V8 thread itself (CLOCK_THREAD_CPUTIME_ID = "this thread")

### Per-process shared (1 total)

```rust
struct CpuTimerSystem {
    pipe_read: RawFd,            // watchdog reads from this
    pipe_write: RawFd,           // signal handlers write to this (signal-safe)
    handles: Mutex<HashMap<u64, v8::IsolateHandle>>,  // app_id → v8 handle
    watchdog_thread: JoinHandle<()>,
}
```

### Signal handler (global, registered once)

```rust
extern "C" fn on_cpu_timeout(sig: c_int, info: *mut siginfo_t, _ctx: *mut c_void) {
    // MUST be async-signal-safe: no malloc, no mutex, no panic

    // 1. Set atomic flag (single instruction, always safe)
    CPU_LIMIT_HIT.store(true, Ordering::Relaxed);

    // 2. Write app_id to pipe (write() is async-signal-safe)
    let app_id: u64 = /* extract from sigval */;
    unsafe { libc::write(PIPE_WRITE_FD, &app_id as *const _ as _, 8) };
}
```

### Watchdog thread (1 total, parked until signal fires)

```rust
fn watchdog_loop(pipe_read: RawFd, handles: Arc<Mutex<HashMap<u64, v8::IsolateHandle>>>) {
    loop {
        let mut app_id: u64 = 0;
        let n = unsafe { libc::read(pipe_read, &mut app_id as *mut _ as _, 8) };
        if n != 8 { continue; }

        // Normal thread context — safe to call V8
        let handles = handles.lock().unwrap();
        if let Some(handle) = handles.get(&app_id) {
            handle.terminate_execution();
        }
    }
}
```

## Timer Lifecycle

```
Isolate created:
  1. timer_create(CLOCK_THREAD_CPUTIME_ID, sigev) → timer_id
  2. Register v8_handle in CpuTimerSystem.handles

First request arrives (idle → active):
  3. timer_settime(timer_id, cpu_limit)  ← ARM

Request completes:
  4. If more requests in-flight: rearm timer for remaining budget
     If last request: timer_settime(timer_id, 0)  ← DISARM

Timer fires (CPU limit exceeded):
  5. Signal handler → pipe write → watchdog → terminate_execution()
  6. V8 unwinds, actor drains pending replies with error
  7. cancel_terminate_execution() → isolate recovers
  8. Next request: rearm timer with fresh budget

Isolate evicted:
  9. timer_delete(timer_id)
  10. Unregister from CpuTimerSystem.handles
```

## Cost Analysis

### Per isolate
- 1 POSIX timer (~300 bytes kernel memory)
- 1 timer_create syscall at startup (~1μs)
- timer_settime on active↔idle transitions (~200ns each)

### Per process (shared)
- 1 pipe (2 file descriptors)
- 1 watchdog thread (parked on read(), zero CPU until timer fires)
- Signal handler registered once

### At scale (1000 isolates, 100K req/s)

| Resource | Cost |
|----------|------|
| Timers | 1000 (kernel handles millions) |
| Threads | 1 (shared watchdog) |
| File descriptors | 2 (one pipe) |
| Syscalls/sec (steady traffic) | ~few K (arm/disarm on state transitions) |
| Syscalls/sec (idle) | 0 (event-driven, no polling) |
| CPU overhead | <0.1% of one core |

### Comparison

| | Global watchdog (polling) | POSIX timer (this design) |
|---|---|---|
| Precision (50ms limit) | ±10ms (20% overshoot) | **Exact** (0%) |
| Precision (5s limit) | ±10ms (0.2%) | Exact |
| Threads | 1 | 1 |
| Syscalls/sec (idle) | 100K (polling all isolates) | **0** |
| Portability | Linux + macOS | **Linux only** |
| Signal handling | None | Required (unsafe) |

## Budget Model

### Per-request (concurrent mode)

In concurrent mode, multiple requests share one V8 thread. CPU cannot be
attributed to individual requests (promises interleave). The timer tracks
**per-isolate aggregate CPU**.

Budget allocation per active period:
```
free tier:   50ms CPU per active period (resets when isolate goes idle)
pro tier:    5s CPU per active period
enterprise:  30s CPU per active period
```

When a request completes, the remaining budget carries over to the next
request. When all requests complete (isolate idle), the budget resets.

### Per-request tracking (future: promise hooks)

For per-request CPU attribution (billing, per-request limits for cooperative
code), use V8 promise hooks (Before/After). This complements the POSIX timer:

- Promise hooks: per-request CPU accounting (billing precision)
- POSIX timer: per-isolate hard limit (safety, catches infinite loops)

## Platform Support

| Platform | CPU timer | Wall-time fallback |
|----------|-----------|-------------------|
| Linux | POSIX timer (CLOCK_THREAD_CPUTIME_ID) | Not needed |
| macOS | Not available | Global watchdog polling (10ms) |
| Windows | Not available | Global watchdog polling (10ms) |

On non-Linux platforms, fall back to the global watchdog with
`pthread_getcpuclockid` for cross-thread CPU measurement (macOS)
or wall-time-only enforcement (Windows).

## Signal Choice

Use `SIGRTMIN + 1` (real-time signal), not `SIGALRM`:
- `SIGALRM` conflicts with tokio's timer implementation and libc sleep()
- Real-time signals are queued (not coalesced) — each timer fire is delivered
- Real-time signals have siginfo_t with sigval for passing app_id

## Recovery

After termination:
1. V8 event loop returns error (uncatchable exception)
2. Actor calls `v8_isolate.cancel_terminate_execution()`
3. Pending replies drained with "CPU time limit exceeded"
4. Isolate stays alive, serves next request
5. Timer is rearmed with fresh budget

This matches workerd behavior — isolates survive CPU timeouts.

## Implementation Plan

1. Add `nix` crate dependency (for timer_create, sigaction, gettid)
2. Create `CpuTimerSystem` — shared pipe + watchdog thread (1 per process)
3. Create `CpuTimer` — per-isolate POSIX timer
4. Register signal handler (SIGRTMIN+1) at process startup
5. Integrate with actor: arm on first request, disarm on last
6. Keep global watchdog as fallback for non-Linux platforms
7. Test: `while(true){}` should be killed at exactly 50ms CPU
