# Runtime limits & memory levers

The V8 runtime exposes a small set of per-isolate knobs that bound CPU,
wall-clock, heap, and idle behavior. Each lever is independent — set
only what you need.

## Idle GC

V8 only runs major GC under heap pressure or when its allocator hits an
internal threshold. During quiet windows the heap retains its
high-water-mark working set indefinitely — unfree-able from the OS's
perspective. For per-app isolates that see bursty traffic, this can
mean ~50–100 MB of unreclaimed RSS per isolate while the app sits idle.

The idle-GC ticker fires `Isolate::low_memory_notification` once the
runtime has gone quiet for a configurable threshold. V8 responds by
running a full GC, releasing the working set back to the OS allocator.

### API

```rust
let rt = Runtime::builder()
    .modules(modules)
    .idle_gc_after_ms(30_000)   // default; pass 0 to disable
    .build();
rt.start_pump();
```

Defaults:

- `DEFAULT_IDLE_GC_AFTER` = 30 s — long enough to skip burst traffic,
  short enough that a per-app isolate idle for a minute releases memory
  before the next request lands.
- Tick cadence is `min(10 s, idle_gc_after_ms)` — short test thresholds
  still get a tick within the window.

The ticker is one compio task per isolate, holding a `Weak<RuntimeInner>`
so the runtime drops without a join. Setting `idle_gc_after_ms(0)` opts
out and skips spawning the task entirely (used by tests that want to
dispose-test the timer threading).

### Tradeoffs

- **Threshold too short** → frequent full-GC pauses. A 1 s threshold
  with bursty traffic punishes p99 even when the app is busy. The
  pump-driven `last_request_ts` reset (every settled async event)
  helps, but a long-running async procedure that yields rarely will
  still see the timer expire.
- **Threshold too long** → memory retained indefinitely on cold apps.
  At the 30 s default, the worst case is ~30 s × creator-app-count of
  unreclaimed RSS — bounded by per-isolate heap caps (lever 1) anyway.
- **The hook is `low_memory_notification`, not `IdleNotificationDeadline`**.
  The v8 = "147" Rust binding doesn't currently expose
  `IdleNotificationDeadline`, so we use `low_memory_notification` —
  which triggers a synchronous full GC instead of an incremental
  budget. Slightly heavier per-fire, but that fires only on idle, so
  the pause is invisible to in-flight requests.

### Test signal

`Runtime::idle_gc_fire_count()` returns the cumulative number of times
the ticker has invoked `low_memory_notification`. This is the test-
visible counter — sampling V8 heap stats before/after is too noisy on a
small test heap.

## (Reserved for heap-cap lever)

The per-isolate `--max-old-space-size` cap (TODO.md "Memory footprint"
lever 1) lives in `RuntimeLimits::heap_limit_bytes`. Section to be
filled in by the heap-cap PR.
