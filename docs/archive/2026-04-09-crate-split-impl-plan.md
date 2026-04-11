# Crate Split Implementation Plan — Phase 1: v8-core + runtime-tokio

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Extract V8 dispatch primitives into `v8-core` crate, move the tokio event loop into `runtime-tokio` crate. All existing tests pass, no logic changes.

**Architecture:** Mechanical file moves. `v8-core` gets all V8 callbacks, state types, init code. `runtime-tokio` gets the tokio select! loop, isolate wrapper, hyper server, and all tests/benches. The old `runtime/` crate becomes a thin re-export shim (or is deleted and platform updated).

**Tech Stack:** Rust workspace, cargo

**Spec:** `docs/plans/2026-04-09-crate-split-design.md`

---

## File Structure

### New crate: `crates/v8-core/`

| File | From | Changes |
|------|------|---------|
| `src/lib.rs` | New | Module declarations, pub re-exports |
| `src/state.rs` | `runtime/src/state.rs` | Add `FetchRequest`, `spawned_fetches` field. Remove `stream_events_tx`. All types `pub`. |
| `src/request.rs` | `runtime/src/request.rs` | All functions `pub`. |
| `src/init.rs` | `runtime/src/init.rs` | `load_polyfills_and_modules` → `pub`. |
| `src/fetch.rs` | `runtime/src/fetch.rs` | Push `FetchRequest` into state instead of executing. Keep validate_url, parse_headers, error_json. Remove reqwest, do_fetch_*, streaming. |
| `src/modules.rs` | `runtime/src/modules.rs` | `pub`. |
| `src/crypto.rs` | `runtime/src/crypto.rs` | No changes. |
| `src/streams.rs` | `runtime/src/streams.rs` | Remove stream_events_tx usage. |
| `src/timers.rs` | `runtime/src/timers.rs` | No changes. |
| `src/kv.rs` | `runtime/src/kv.rs` | No changes. |
| `src/env.rs` | `runtime/src/env.rs` | No changes. |
| `src/url.rs` | `runtime/src/url.rs` | No changes. |
| `src/ops.rs` | `runtime/src/ops.rs` | No changes. |
| `src/cpu_timer.rs` | `runtime/src/cpu_timer.rs` | No changes. |
| `src/storage.rs` | `runtime/src/storage.rs` | No changes. |
| `src/embed/*.js` | `runtime/src/embed/*.js` | No changes. |

### New crate: `crates/runtime-tokio/`

| File | From | Changes |
|------|------|---------|
| `src/lib.rs` | New | Module declarations, re-export v8_core types + own types |
| `src/runtime.rs` | `runtime/src/runtime.rs` | Import from `v8_core`. Add fetch execution (drain spawned_fetches → reqwest). |
| `src/isolate.rs` | `runtime/src/isolate.rs` | Import from `v8_core`. |
| `src/fetch.rs` | New (extracted from `runtime/src/fetch.rs`) | `execute_fetch` — takes FetchRequest, returns OpResult via reqwest. Streaming fetch logic. |
| `src/server.rs` | `runtime/src/server.rs` | Import from `v8_core` + self. Binary: `v8-server`. |
| `src/lib.rs` tests | `runtime/src/lib.rs` tests | All `#[cfg(test)]` blocks. |
| `tests/examples.rs` | `runtime/tests/examples.rs` | Import from `runtime_tokio`. |
| `tests/wpt_runner.rs` | `runtime/tests/wpt_runner.rs` | Import from `runtime_tokio`. |
| `benches/v8_qps.rs` | `runtime/benches/v8_qps.rs` | Import from `runtime_tokio`. |
| `benches/echo_server.rs` | `runtime/benches/echo_server.rs` | No changes. |
| `benches/*.js, *.lua, *.sh, *.txt` | `runtime/benches/*` | No changes. |

### Modified: `crates/runtime/` → thin re-export shim

Keep the crate as a facade that re-exports from `runtime-tokio` so `platform/` doesn't need to change immediately:

```rust
// crates/runtime/src/lib.rs
pub use runtime_tokio::*;
```

Or: update platform to depend on `runtime-tokio` directly and delete `runtime/`.

### Modified: `crates/platform/`

Update `Cargo.toml` to depend on `runtime-tokio` instead of `runtime`.

---

## Task 1: Create v8-core crate scaffold

**Files:**
- Create: `crates/v8-core/Cargo.toml`
- Create: `crates/v8-core/src/lib.rs`
- Modify: `Cargo.toml` (workspace members)

- [ ] **Step 1: Create crate directory and Cargo.toml**

```toml
# crates/v8-core/Cargo.toml
[package]
name = "appbase-v8-core"
version = "0.1.0"
edition = "2024"
description = "V8 dispatch primitives — runtime-agnostic"

[dependencies]
v8 = "147"
libc = "0.2"
serde_json = "1"
url = "2"
ada-url = "3.4.4"
aws-lc-rs = "1"
base64 = "0.22"
futures = "0.3"
tokio-util = { version = "0.7", features = ["rt"] }
appbase-runtime-macros = { path = "../runtime_macros" }

[lints]
workspace = true
```

Note: NO tokio, NO reqwest, NO hyper, NO bytes. Only tokio-util for CancellationToken.

- [ ] **Step 2: Create empty lib.rs**

```rust
// crates/v8-core/src/lib.rs
#![allow(unsafe_code)]
```

- [ ] **Step 3: Add to workspace**

Add `"crates/v8-core"` to the `members` array in the root `Cargo.toml`.

- [ ] **Step 4: Verify**

Run: `cargo check -p appbase-v8-core`

- [ ] **Step 5: Commit**

```bash
git add crates/v8-core/ Cargo.toml
git commit -m "scaffold: create v8-core crate"
```

---

## Task 2: Move state.rs to v8-core

**Files:**
- Copy: `crates/runtime/src/state.rs` → `crates/v8-core/src/state.rs`
- Modify: `crates/v8-core/src/state.rs` (add FetchRequest, spawned_fetches, make all pub)
- Modify: `crates/v8-core/src/lib.rs` (add module)

- [ ] **Step 1: Copy state.rs to v8-core**

Copy the file. Then modify:

1. Add `FetchRequest` struct:
```rust
/// Descriptor for a fetch request — pushed by the V8 callback, executed by the runtime.
pub struct FetchRequest {
    pub op_id: u32,
    pub stream_id: u32,
    pub request_id: Option<u64>,
    pub method: String,
    pub url: String,
    pub headers_json: String,
    pub body: Option<String>,
    pub cancel: Option<CancellationToken>,
}
```

2. Add `spawned_fetches: Vec<FetchRequest>` to `RuntimeState`
3. Initialize as `Vec::new()` in `RuntimeState::new()`
4. Remove `stream_events_tx: Option<tokio::sync::mpsc::Sender<OpResult>>` (runtime-specific)
5. Make all `pub(crate)` fields `pub` (the crate boundary is now v8-core, consumers need access)

- [ ] **Step 2: Add module to lib.rs**

```rust
pub mod state;
```

- [ ] **Step 3: Verify**

Run: `cargo check -p appbase-v8-core`

- [ ] **Step 4: Commit**

```bash
git add crates/v8-core/src/state.rs crates/v8-core/src/lib.rs
git commit -m "feat(v8-core): add state.rs with FetchRequest, all types pub"
```

---

## Task 3: Move core modules to v8-core (ops, timers, kv, env, url)

**Files:**
- Copy: `runtime/src/ops.rs` → `v8-core/src/ops.rs`
- Copy: `runtime/src/timers.rs` → `v8-core/src/timers.rs`
- Copy: `runtime/src/kv.rs` → `v8-core/src/kv.rs`
- Copy: `runtime/src/env.rs` → `v8-core/src/env.rs`
- Copy: `runtime/src/url.rs` → `v8-core/src/url.rs`
- Modify: `v8-core/src/lib.rs`

- [ ] **Step 1: Copy all 5 files, update imports**

Each file imports `crate::event_loop::SharedState` or `crate::state::SharedState`. In v8-core, the import is `crate::state::SharedState` — same as current runtime since we already migrated to state.rs.

Make all `pub(crate)` items `pub`.

- [ ] **Step 2: Add modules to lib.rs**

```rust
pub mod ops;
pub mod timers;
pub mod kv;
pub mod env;
pub mod url;
```

- [ ] **Step 3: Verify**

Run: `cargo check -p appbase-v8-core`

- [ ] **Step 4: Commit**

```bash
git add crates/v8-core/src/{ops,timers,kv,env,url}.rs crates/v8-core/src/lib.rs
git commit -m "feat(v8-core): add ops, timers, kv, env, url modules"
```

---

## Task 4: Move crypto, streams, modules, storage, cpu_timer to v8-core

**Files:**
- Copy: `runtime/src/crypto.rs` → `v8-core/src/crypto.rs`
- Copy: `runtime/src/streams.rs` → `v8-core/src/streams.rs`
- Copy: `runtime/src/modules.rs` → `v8-core/src/modules.rs`
- Copy: `runtime/src/storage.rs` → `v8-core/src/storage.rs`
- Copy: `runtime/src/cpu_timer.rs` → `v8-core/src/cpu_timer.rs`
- Copy: `runtime/src/embed/` → `v8-core/src/embed/`
- Modify: `v8-core/src/lib.rs`

- [ ] **Step 1: Copy files, update imports**

For `streams.rs`: remove any `stream_events_tx` usage. The outbound stream forwarding will be handled by the runtime crate. Keep the V8 callback logic (enqueue, close, read) that works with RuntimeState.

For `crypto.rs`: make functions `pub` or `pub(crate)` as needed.

For `modules.rs`: make `pub`.

Copy `embed/` directory with all JS files.

- [ ] **Step 2: Add modules to lib.rs**

```rust
pub mod crypto;
pub mod streams;
pub mod modules;
pub mod storage;
#[cfg(target_os = "linux")]
pub mod cpu_timer;
```

- [ ] **Step 3: Verify**

Run: `cargo check -p appbase-v8-core`

- [ ] **Step 4: Commit**

```bash
git add crates/v8-core/src/{crypto,streams,modules,storage,cpu_timer}.rs crates/v8-core/src/embed/ crates/v8-core/src/lib.rs
git commit -m "feat(v8-core): add crypto, streams, modules, storage, cpu_timer, embed/*.js"
```

---

## Task 5: Move init.rs and request.rs to v8-core

**Files:**
- Copy: `runtime/src/init.rs` → `v8-core/src/init.rs`
- Copy: `runtime/src/request.rs` → `v8-core/src/request.rs`
- Modify: `v8-core/src/lib.rs`

- [ ] **Step 1: Copy init.rs**

Make `load_polyfills_and_modules`, `setup_globals`, `init_v8`, `thread_cpu_time` all `pub`.

The init.rs references `crate::crypto`, `crate::fetch`, `crate::streams`, `crate::kv`, `crate::env`, `crate::url` — all now in v8-core. Imports stay as `crate::*`.

- [ ] **Step 2: Copy request.rs**

Make all functions `pub`.

- [ ] **Step 3: Add modules + re-exports to lib.rs**

```rust
pub mod init;
pub mod request;

// Re-export common types for convenience
pub use init::{init_v8, RequestResult, HttpResult};
pub use modules::ModuleEntry;
pub use state::{SharedState, RuntimeState, IncomingRequest, RequestReply, RequestKind, 
                OpResult, FetchRequest, HttpStreamResult};
pub use storage::AppStorage;
```

- [ ] **Step 4: Verify v8-core compiles standalone**

Run: `cargo check -p appbase-v8-core`
This is the critical check — v8-core must compile with NO tokio runtime dependency.

- [ ] **Step 5: Commit**

```bash
git add crates/v8-core/src/{init,request}.rs crates/v8-core/src/lib.rs
git commit -m "feat(v8-core): add init.rs, request.rs — v8-core compiles standalone"
```

---

## Task 6: Move fetch.rs to v8-core (descriptor-only)

**Files:**
- Create: `v8-core/src/fetch.rs` (from `runtime/src/fetch.rs`, modified)
- Modify: `v8-core/src/lib.rs`

- [ ] **Step 1: Create v8-core's fetch.rs**

Copy `runtime/src/fetch.rs` but:
1. Keep: `raw_fetch_callback`, `validate_url`, `parse_headers`, `error_json`, `shared_client`, SSRF protection, `MAX_RESPONSE_SIZE`
2. Remove: `do_fetch_buffered`, `do_fetch_streaming_from_response`, `buffer_response`, all reqwest usage, `build_and_send_request`
3. Change `raw_fetch_callback`: instead of pushing a future into `spawned_ops`, push a `FetchRequest` into `state.spawned_fetches`:

```rust
// In raw_fetch_callback, replace the entire future-pushing block with:
state.borrow_mut().spawned_fetches.push(FetchRequest {
    op_id,
    stream_id,
    request_id,
    method,
    url,
    headers_json,
    body,
    cancel,
});
```

4. Remove `reqwest` import — v8-core doesn't depend on reqwest

Actually, `shared_client()` uses reqwest. Move it to runtime-tokio. v8-core's fetch.rs only has the V8 callback and validation.

- [ ] **Step 2: Add module to lib.rs**

```rust
pub mod fetch;
```

- [ ] **Step 3: Verify**

Run: `cargo check -p appbase-v8-core`

- [ ] **Step 4: Commit**

```bash
git add crates/v8-core/src/fetch.rs crates/v8-core/src/lib.rs
git commit -m "feat(v8-core): add fetch.rs — push FetchRequest descriptor, no reqwest"
```

---

## Task 7: Create runtime-tokio crate scaffold

**Files:**
- Create: `crates/runtime-tokio/Cargo.toml`
- Create: `crates/runtime-tokio/src/lib.rs`
- Modify: `Cargo.toml` (workspace members)

- [ ] **Step 1: Create Cargo.toml**

```toml
# crates/runtime-tokio/Cargo.toml
[package]
name = "appbase-runtime-tokio"
version = "0.1.0"
edition = "2024"
description = "Tokio-based event loop for appbase V8 runtime"

[dependencies]
appbase-v8-core = { path = "../v8-core" }
v8 = "147"
tokio = { version = "1", features = ["full"] }
tokio-util = { version = "0.7", features = ["rt"] }
hyper = { version = "1", features = ["server", "http1"] }
hyper-util = { version = "0.1", features = ["tokio"] }
http-body-util = "0.1"
bytes = "1"
reqwest = { version = "0.12", default-features = false, features = ["rustls-tls", "json"] }
serde_json = "1"
futures = "0.3"
mimalloc = { version = "0.1", default-features = false }

[[bin]]
name = "v8-server"
path = "src/server.rs"

[[bin]]
name = "echo-server"
path = "benches/echo_server.rs"

[[bench]]
name = "v8_qps"
harness = false

[dev-dependencies]
appbase-compiler = { path = "../compiler" }

[lints]
workspace = true
```

- [ ] **Step 2: Create lib.rs with re-exports**

```rust
// crates/runtime-tokio/src/lib.rs
pub mod runtime;
pub mod isolate;

// Re-export v8-core for convenience
pub use appbase_v8_core as v8_core;
pub use appbase_v8_core::{init_v8, RequestResult, HttpResult, ModuleEntry, AppStorage,
    IncomingRequest, RequestReply, RequestKind, OpResult, HttpStreamResult};
pub use runtime::Runtime;
pub use isolate::{Isolate, IsolatePool};
```

- [ ] **Step 3: Add to workspace**

Add `"crates/runtime-tokio"` to workspace members.

- [ ] **Step 4: Commit**

```bash
git add crates/runtime-tokio/ Cargo.toml
git commit -m "scaffold: create runtime-tokio crate"
```

---

## Task 8: Move runtime.rs and isolate.rs to runtime-tokio

**Files:**
- Copy: `runtime/src/runtime.rs` → `runtime-tokio/src/runtime.rs`
- Copy: `runtime/src/isolate.rs` → `runtime-tokio/src/isolate.rs`
- Create: `runtime-tokio/src/fetch.rs` (fetch execution via reqwest)

- [ ] **Step 1: Copy runtime.rs, update imports**

Change all `crate::` imports to `appbase_v8_core::` or `v8_core::`:
```rust
use appbase_v8_core::init::{init_v8, load_polyfills_and_modules, RequestResult};
use appbase_v8_core::state::{RuntimeState, SharedState, OpResult, ...};
use appbase_v8_core::request;
// etc.
```

Add fetch execution: in `collect_new_tasks`, drain `state.spawned_fetches` and spawn reqwest futures (move the fetch execution logic from the old fetch.rs here).

Add `stream_events_tx/rx` channel (runtime-specific, not in v8-core).

- [ ] **Step 2: Copy isolate.rs, update imports**

Same import changes.

- [ ] **Step 3: Create runtime-tokio/src/fetch.rs**

Extract the fetch execution logic (reqwest, do_fetch_buffered, do_fetch_streaming, buffer_response, build_and_send_request) from the old runtime/src/fetch.rs into this file. This is the runtime-specific fetch implementation.

```rust
// runtime-tokio/src/fetch.rs
use appbase_v8_core::state::{FetchRequest, OpResult};
use reqwest;

pub(crate) async fn execute_fetch(req: FetchRequest) -> OpResult { ... }
pub(crate) async fn execute_fetch_streaming(req: FetchRequest, 
    stream_events_tx: tokio::sync::mpsc::Sender<OpResult>) { ... }
```

- [ ] **Step 4: Verify**

Run: `cargo check -p appbase-runtime-tokio`

- [ ] **Step 5: Commit**

```bash
git add crates/runtime-tokio/src/{runtime,isolate,fetch}.rs
git commit -m "feat(runtime-tokio): move runtime.rs, isolate.rs, add fetch execution"
```

---

## Task 9: Move server, tests, benches to runtime-tokio

**Files:**
- Copy: `runtime/src/server.rs` → `runtime-tokio/src/server.rs`
- Copy: `runtime/tests/` → `runtime-tokio/tests/`
- Copy: `runtime/benches/` → `runtime-tokio/benches/`
- Move: test code from `runtime/src/lib.rs` → `runtime-tokio/src/lib.rs`

- [ ] **Step 1: Copy server.rs, update imports**

- [ ] **Step 2: Copy tests/ and benches/ directories**

Update import paths in `examples.rs`, `wpt_runner.rs`, `v8_qps.rs`:
```rust
// BEFORE:
use appbase_runtime::{Isolate, init_v8, ...};
// AFTER:
use appbase_runtime_tokio::{Isolate, init_v8, ...};
```

- [ ] **Step 3: Move lib.rs tests**

Copy the `#[cfg(test)] mod tests` block from `runtime/src/lib.rs` into `runtime-tokio/src/lib.rs`.

- [ ] **Step 4: Verify all tests pass**

Run: `cargo test -p appbase-runtime-tokio --lib`
Run: `cargo test -p appbase-runtime-tokio --test examples -- --skip weather`

- [ ] **Step 5: Commit**

```bash
git add crates/runtime-tokio/
git commit -m "feat(runtime-tokio): move server, tests, benches — all tests pass"
```

---

## Task 10: Update old runtime/ as re-export shim + update platform

**Files:**
- Modify: `crates/runtime/src/lib.rs` (gut it, re-export from runtime-tokio)
- Modify: `crates/runtime/Cargo.toml` (depend on runtime-tokio, remove direct deps)
- Modify: `crates/platform/Cargo.toml` (optionally switch to runtime-tokio)

- [ ] **Step 1: Make runtime/ a thin shim**

```rust
// crates/runtime/src/lib.rs
//! Thin re-export shim — all functionality is in appbase-runtime-tokio.
pub use appbase_runtime_tokio::*;
```

```toml
# crates/runtime/Cargo.toml
[package]
name = "appbase-runtime"
version = "0.1.0"
edition = "2024"

[dependencies]
appbase-runtime-tokio = { path = "../runtime-tokio" }
```

Remove all source files except lib.rs from runtime/src/ (they're now in v8-core and runtime-tokio).

- [ ] **Step 2: Verify platform compiles**

Run: `cargo check -p appbase-platform`

- [ ] **Step 3: Verify full workspace**

Run: `cargo test --workspace -- --skip weather --skip wpt`

- [ ] **Step 4: Commit**

```bash
git add -A
git commit -m "refactor: runtime/ is now a thin re-export shim over runtime-tokio"
```

---

## Task 11: Final verification and cleanup

- [ ] **Step 1: Run full test suite**

Run: `cargo test -p appbase-runtime-tokio --lib` — all tests pass
Run: `cargo test -p appbase-runtime-tokio --test examples -- --skip weather` — all pass
Run: `cargo test --workspace -- --skip weather --skip wpt` — workspace clean

- [ ] **Step 2: Verify v8-core has no tokio runtime dependency**

Run: `cargo tree -p appbase-v8-core | grep -i tokio`
Expected: only `tokio-util` (for CancellationToken), NOT `tokio` itself.

- [ ] **Step 3: Run benchmark**

Run: `cargo build --release --bin v8-server` (from runtime-tokio)
Verify: >=200K req/s for ping

- [ ] **Step 4: Commit**

```bash
git add -A && git commit -m "chore: crate split complete — v8-core + runtime-tokio verified"
```

---

## Self-Review

**Spec coverage:**
- v8-core with no runtime opinion: Tasks 1-6 ✓
- runtime-tokio with tokio event loop: Tasks 7-9 ✓
- FetchRequest descriptor in v8-core: Task 2 (state.rs) + Task 6 (fetch.rs) ✓
- Fetch execution in runtime-tokio: Task 8 ✓
- Platform works unchanged: Task 10 ✓
- All tests pass: Task 11 ✓
- No regression: Task 11 benchmark ✓

**Phase 2 (runtime-compio) is a separate plan** after Phase 1 ships clean.
