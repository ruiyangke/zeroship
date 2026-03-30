# Appbase Memory Model

## Overview

Appbase uses deno_core (V8) for JavaScript execution with rusqlite (SQLite) for persistence. This document describes the memory architecture for multi-tenant deployments.

## Key Finding: V8 Engine Memory is Shared

V8 is initialized **once per process** via `std::sync::Once`. All isolates share the V8 engine code, JIT compiler, builtins, and ICU data through OS copy-on-write memory pages.

```
Process memory:
┌──────────────────────────────────────┐
│ Shared (loaded once):                │
│   V8 engine + JIT compiler   ~10MB   │
│   V8 builtins (Array, etc.)   ~5MB   │
│   ICU data (i18n)             ~5MB   │
│                               ~20MB  │
├──────────────────────────────────────┤
│ Isolate 1 (per-app):                │
│   JS heap                     ~2MB   │
│   deno_core state            ~0.3MB  │
│   SQLite                     ~0.1MB  │
│                              ~2.4MB  │
├──────────────────────────────────────┤
│ Isolate 2:                   ~2.4MB  │
├──────────────────────────────────────┤
│ Isolate N:                   ~2.4MB  │
└──────────────────────────────────────┘
```

### Source Code Verification

From `deno_core-0.395.0/runtime/setup.rs`:

```rust
pub fn init_v8(...) {
    static DENO_INIT: Once = Once::new();
    DENO_INIT.call_once(move || v8_init(v8_platform, snapshot, expose_natives));
}
```

V8 platform initialization (`v8::V8::InitializePlatform` + `v8::V8::Initialize`) happens exactly once. Subsequent `JsRuntime::new` calls create isolates that share the engine.

## Measured Numbers

### Single Isolate Breakdown

Measured via `/proc/<pid>/smaps_rollup`:

| Metric | Size | Meaning |
|--------|------|---------|
| RSS | 28 MB | Total resident memory |
| Private_Dirty | 5 MB | Unique allocations (heap, stack) |
| Private_Clean | 21 MB | File-backed, shareable via mmap |
| Shared_Clean | 1.7 MB | Already shared with other processes |
| Anonymous | 5 MB | Actual heap memory |

The 28MB RSS for a single isolate is misleading — 21MB is file-backed (V8 binary, libraries) and will be shared across isolates via copy-on-write.

### Multi-Isolate Scaling

Measured by creating 10 isolates in one process:

```
Isolate  1: +20.2 MB  (includes shared V8 engine)
Isolate  2:  +2.4 MB  (incremental)
Isolate  3:  +2.4 MB
Isolate  4:  +2.4 MB
...
Isolate 10:  +2.4 MB

Marginal cost per isolate: ~2.4 MB
```

### Multi-Tenant Projections

| Warm Isolates | Memory | Server Size |
|---------------|--------|-------------|
| 1 | 28 MB | Any |
| 10 | 44 MB | Any |
| 100 | 260 MB | 512MB |
| 500 | 1.2 GB | 2GB |
| 1,000 | 2.4 GB | 4GB |
| 5,000 | 12 GB | 16GB |
| 10,000 | 24 GB | 32GB |

## Comparison with Cloudflare Workers

| | Cloudflare Workers (workerd) | Appbase (deno_core) |
|---|---|---|
| Per-isolate cost | ~3-5 MB | ~2.4 MB |
| Shared engine | ~38 MB (custom V8 build) | ~20 MB |
| Pointer compression | Yes (4-byte pointers) | No (8-byte) |
| ICU | Shared across process | Shared via OS COW |

Appbase achieves comparable per-isolate memory efficiency to Cloudflare Workers without any custom V8 modifications.

## Optimization Levers

### Already implemented

- V8 snapshots (faster startup, slightly lower memory via shared compiled code)
- serde_v8 (direct V8 ↔ Rust object passing, no JSON intermediate allocations)

### Available if needed

| Optimization | Expected Savings | Effort |
|---|---|---|
| V8 heap limits (cap at 8MB per isolate) | Prevents runaway growth | Low |
| Isolate pooling with eviction | Only keep active apps warm | Medium |
| V8 pointer compression | ~40% heap reduction (~1.4MB/isolate) | High (custom V8 build) |
| Strip ICU from V8 | ~5MB shared reduction | High (custom V8 build) |

### Isolate Pool Strategy (recommended for production)

```
On request for app "X":
  1. Check pool for warm isolate
  2. If found → reuse (0ms startup)
  3. If not found → create new isolate (7-12ms startup)
  4. After response, return isolate to pool
  5. Evict idle isolates after 60s
  6. Cap pool at max_isolates (based on available RAM)
```

With a 4GB server and 60s idle timeout:
- Max warm isolates: ~1,500
- Active isolates at any moment: ~50-200
- Actual memory usage: ~500MB-1GB

## CPU Time Metering

Each request is metered using `CLOCK_THREAD_CPUTIME_ID` which only counts actual CPU cycles (not I/O wait). This matches Cloudflare Workers' billing model.

Measured CPU times:
- Noop dispatch: ~4µs
- DB insert: ~0.5ms CPU (vs 14ms wall clock — difference is SQLite fsync)
- DB find (100 items): ~0.5ms CPU

## Benchmarks

Run with `cargo bench -p appbase-runtime`:

| Benchmark | Time |
|---|---|
| runtime_startup | 12.0ms |
| runtime_startup_with_snapshot | 7.0ms |
| rpc_dispatch_noop | 4.4µs |
| rpc_insert | 1.3ms |
| rpc_find_100_items | 16.3ms |
| rpc_batch_4_calls | 24.8ms |

HTTP throughput (release build, `hey`):

| Test | Requests/sec |
|---|---|
| Static HTML (50 concurrent) | 90,150 |
| Static HTML (100 concurrent) | 98,972 |
| RPC addTodo (10 concurrent) | 702 |
