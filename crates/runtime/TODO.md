# runtime TODO

Backlog ordered roughly by leverage.

## Done

### Crypto layout — promote kernel + lift node:crypto out of web/ (`feature/crypto-layout`)

The dual-surface crypto backend (`web/crypto/kernel/`) was promoted to
`base/crypto/` — it's not "web", it's the shared algorithm backend that
BOTH `web::crypto` (WebCrypto) and node:crypto call into. Lifting it to
`base/` (Chromium-style — foundational utilities shared across surface
APIs) makes the architecture visible and completes the three-way
symmetry with `web/crypto/` and `node/crypto/`.

`web/crypto_node/` was lifted to `node/crypto/`. node:crypto is a
Node-API surface, not a Web API. The new top-level `node/` folder
establishes the symmetry: `web/` for WHATWG/W3C, `node/` for Node-specific
APIs. Currently hosts only `crypto`, but is the natural home for
future native `node:*` modules (e.g. `node:zlib`, `node:os`).

Back-compat shims in `lib.rs` + `web/mod.rs` + `web/crypto/mod.rs` keep
all external paths resolving:
- `crate::crypto_node` (lib) → re-exports `web::crypto_node` → `node::crypto`
- `crate::web::crypto::kernel::*` → re-exports `crate::base::crypto`
- `crate::crypto_ops::*` → re-exports `crate::base::crypto` (for
  internal callers in `node::crypto`)

Pure rename + import-rewrite — zero semantic changes. Test counts match
baseline (16 crypto + 34 crypto_native + 69 crypto_node + 204 lib).

### Reorg the source tree (`feature/runtime-reorg`)

`crates/runtime/src/` is now grouped into four roots:

```
core/      runtime/state/dispatch/init/channel/modules/panic_util/plugin/cpu_timer/server/serve
webidl/    byte_string + clamp + enforce_range + usv_string
web/       Web API surface — base64, blob/, codec, crypto/, dom/, encoding/,
           fetch/{algorithms,body/,request,response,...}, headers, streams/,
           structured_clone, url/, websocket/
transport/ ssrf + client (per-thread cyper) + handler (kernel HTTP bridge)
```

Top-level survivors: `auth.rs`, `storage.rs`, `fetch_outcome.rs`, `embed/`
(now just `node-globals.js`). All old paths (`crate::fetch_native::…`,
`crate::dom::…`, `crate::state::…`, etc.) keep resolving via re-exports
in `lib.rs`, so external crates and tests didn't need migration.

Pure rename + import-rewrite — zero semantic changes. See commit message
for diff stats.

## Smaller items

- Per-class isolate-slot caching is per `__InstallSlot_X` types, but registration
  in `init.rs` happens via individual function calls. Could be unified via a
  `register_native_class!` macro — minor.
- `legacy_bridge.rs` (in `streams/`) is named misleadingly — still alive via
  `__zsBeginStreamForward`. Rename to `stream_bridge.rs` OR delete entirely if
  the Rust-only forwarder lands via fetch-js-delete.
- `state.rs` has `OpResult::StreamChunk` + `pending_fetches` etc. — some may be
  dead post-cleanup-rawfetch. Audit during reorg.

## Memory footprint

The runtime's per-isolate working set sits around 125 MB after warmup
(V8 baseline ~50 MB + native class init ~20 MB + scenarios bytecode
~20 MB + transient request state ~30 MB). At 16 workers per process,
that's ~2 GB resident. Three levers, listed by effort × impact:

### 1. Per-isolate `--max-old-space-size` cap

V8 has no heap cap today; it grows to multi-GB before GC pressure
kicks in. Capping old-gen forces earlier GC and bounds the worst
case.

- API: `v8::Isolate::CreateParams::heap_limits(initial, max)` —
  pass `max = 64 * 1024 * 1024` (or whatever the cap is).
- Wire via `RuntimeBuilder` so deployers can set it per-app.
- Risk: too low causes thrashing or OOM. Default off; opt-in via
  `RuntimeLimits::heap_limit_mb`. Document the trade-off in
  `docs/reference/runtime-limits.md` (file doesn't exist yet —
  create as part of this).
- Cuts total RSS from ~2 GB → ~1 GB at 16 workers.

### 2. Boot snapshot — `StartupData`

V8 supports startup snapshots: freeze the post-init heap (after
all native classes installed, after `scenarios.js`-equivalent
boot-time JS evaluated) into a binary blob. Each isolate boots
from the snapshot instead of re-running init.

- API: `v8::SnapshotCreator` build-time, `v8::Isolate::CreateParams::snapshot_blob`
  per-isolate boot.
- Two snapshots: (a) base — native classes + Web API surface;
  (b) per-app — base + the user's `default.fetch` + module
  graph evaluated. (b) is the bigger win for cold-start.
- Risk: snapshot must be re-built on every native API change.
  Add a build-time step in `crates/runtime/build.rs` that produces
  `target/zeroship-runtime-snapshot.bin`, included via `include_bytes!`.
- Cuts per-isolate boot from ~50–200 ms → ~5–10 ms. Saves
  ~20–30 MB per isolate (no init artifacts retained — already
  compiled into the snapshot).
- Cloudflare Workers technique. The biggest perf lever for
  multi-tenant cold starts.

### 4. Idle GC trigger

V8 only GCs under heap pressure or when the allocator hits a
threshold. During low-traffic windows the heap retains its
high-water-mark working set indefinitely — unfree-able from the
OS's point of view.

- API: `v8::Isolate::idle_notification_deadline(deadline_in_seconds)`
  hints V8 to spend up to N ms running incremental GC. Returns
  `true` when GC has caught up.
- Wire into the compio event loop as a "no requests for K seconds
  → fire idle GC" trigger. Per-isolate timer.
- Effort: ~30 LOC in `runtime.rs` — track `last_request_ts` per
  isolate, schedule idle ticks via `compio::time::interval`.
- Saves: depends on traffic profile. For per-app isolates that
  see bursty traffic, can free ~50–100 MB per isolate during
  idle windows.

(Note: original "drop --workers=16 to --workers=4" alternative
isn't a runtime concern — it's a deploy-time config.)

## Test infrastructure

- Hand tests + WPT runners are scattered across `tests/` flat. After reorg, mirror
  the new src layout: `tests/web/fetch/...`, `tests/web/url/...` etc.
- WPT runners share a lot of testharness shim code (sanitize, fetch-fixture stub,
  microtask draining loop). Extract to a shared `tests/wpt_harness.rs` module.
- The microtask drain loop (`for _ in 0..10 { scope.perform_microtask_checkpoint() }`)
  is hard-coded; flaky under chained-await patterns. Loop until stable.
