# Native WHATWG Streams design

**Date:** 2026-05-01
**Status:** Draft v2 — implementation pending
**Spec:** WHATWG Streams Standard — https://streams.spec.whatwg.org/
**Spec source:** https://github.com/whatwg/streams/blob/main/index.bs
**Reference impl:** https://github.com/whatwg/streams/tree/main/reference-implementation
**WebIDL:** https://webidl.spec.whatwg.org/ (§3.7.10 async iterable;
§3.2.10 `[EnforceRange]`; §3.2.5 `[Exposed]`; §3.7.4.2
`unrestricted double`)
**Depends on:** none — this design is the foundation for
`docs/proposals/compression-streams-native.md` (which imports
its `NativeTransformer` trait, see §VIII.3).
**Unblocks:** native CompressionStream/DecompressionStream,
native fetch body bridge, native pipeThrough on internal
encoder hand-off, WPT regression for the streams suite.

## Revision history

- **v1 (2026-05-01)** — Initial design covering the entire
  WHATWG spec surface (every IDL interface, every named
  algorithm). Replaces the value-preserving JS skeleton in
  `crates/runtime/src/embed/streams.js` and the vendored
  web-streams-polyfill v3.3.3 (3700 LOC) at
  `crates/runtime/src/embed/streams-polyfill.js`. Designed
  pure-native on V8 + Rust + compio with **zero tokio**.
- **v2 (2026-05-01, this revision)** — Critic-driven revision
  addressing 14 CRITICAL findings, 20 MAJOR findings, 18 MINOR
  findings, 14 missing-concept additions, 7 unverifiable claims,
  and 10 process flaws from the round-1 review at
  `/tmp/zeroship-reviews/streams-review.md`. Major changes:
  - **D-15** rewritten — gates on `[[state]]==='closed'`, NOT on
    `[[closeRequested]]` (verified against ref impl
    `ReadableByteStreamControllerRespond`).
  - **D-3 / OpResult** — design replaces "no new infrastructure"
    claim with a new `OpResult::JsValue` variant carrying
    `v8::Global<v8::Value>` (since the existing
    `OpResult::Completed.value: String` cannot carry V8 chunks).
    Verified: `crates/runtime/src/state.rs:592-596` confirms the
    String-only field.
  - **D-12 / microtask** — fictitious `scope.enqueue_microtask`
    API replaced with the real `Isolate::enqueue_microtask(&mut self, Local<Function>)`
    (verified against `v8-147.1.0/src/isolate.rs:1676-1680`),
    plus the closure-to-Function bridge spelled out.
  - **D-11 / tee** — terminology fixed: each branch is an
    independent ReadableStream with its own controller and
    queue, driven by a shared pullAlgorithm.
  - **§IV / async iterator** — `is_finished` slot removed; we
    track "finished" via `reader.stream === undefined` (matches
    ref impl `ReadableStreamAsyncIterator-impl.js`).
  - **§VIII.3 / `NativeTransformer::transform`** — switched to
    async return type (`Pin<Box<dyn Future<…>>>`) for spec-faithful
    `transformPromise = transformAlgorithm(chunk)` semantics.
  - **§II.8 / `[[writeRequests]]`** — slot type corrected to
    `VecDeque<v8::Global<v8::Promise>>` (Promises, not Resolvers
    — Resolvers stored separately).
  - **§VI.3 / SizeAlgorithm** — `this` value passes `undefined`
    (per WebIDL `Function callback`), and Symbol return throws
    TypeError at the IDL boundary (no silent NaN coercion).
  - **§XVI.2 / WPT inventory** — recounted; 105 files (was 92),
    plus crashtests (10) and transferable resources (11)
    classified separately.
  - **§XVIII / hours** — WPT iteration step recalibrated from
    14h to 32h.
  - **§XIV.1 / `#[v8_async_method]`** — borrow-across-await
    discipline spelled out (re-acquire `Box<Self>` pointer per
    poll, not pin).
  - **§XVII / compression dep #5** — internal-field count
    discussion clarified; the design carries 2 effective slots
    (Box<TSControllerState> in field 0 + a private-symbol
    `transformerCodec` mirror) so the compression doc's
    "codec-pointer-near-state" requirement is met.
  - **§IX.1 / pipeTo** — handler order matches the ref impl's
    exact sequence; rejection-swallow on pipeLoop made explicit;
    "in parallel" vs microtask semantics clarified.
  - **§XX / open questions** — items that the spec already
    answers moved out of "Open questions" into the relevant
    algorithm sections.
  - **Missing concepts §XXII** — new section covering 14 items
    the critic found absent: cross-realm error propagation,
    structuredClone DataCloneError, Promise prototype tampering
    hardening, realm-aware Promise creation, microtask hop
    counts, GC of underlying-source/-sink callbacks, detached
    buffer detection on every chunk path, sync iterable fallback
    in `ReadableStream.from`, `type` field validation, errors
    in `start()`, tee/disturb interaction, cross-piping (one
    source, two pipeTo dests), HWM byte-streams without
    auto-allocate, `releaseLock` during pipeTo's reader use.

## Top matter

### Goals

1. **Full WHATWG Streams compliance.** Every interface in
   https://streams.spec.whatwg.org/ at parity with the spec —
   no "v1 subset". Pass the entire WPT `streams/` suite minus
   the transferable subdirectory (D-7 below).
2. **Replace web-streams-polyfill.** Delete the 3700-LOC vendored
   polyfill at `crates/runtime/src/embed/streams-polyfill.js`
   and the value-path JS skeleton at
   `crates/runtime/src/embed/streams.js`. The runtime ships one
   streams implementation, in Rust, with per-isolate native
   classes installed during `setup_globals`.
3. **Back compression and fetch.** Honour the explicit
   dependency contract in
   [compression-streams-native.md §Dependencies](compression-streams-native.md)
   so `CompressionStream` / `DecompressionStream` and the
   internal `Content-Encoding` decompression hook can construct
   native TransformStreams from Rust transformers without going
   through public JS APIs (D-9, §I.1).
4. **Compio-native async.** Promise resolution flows through
   `v8::PromiseResolver` driven from compio tasks. Zero tokio.
   No `Send`/`Sync` constraints inside the isolate (single-
   threaded per AGENTS.md "V8 per thread, one isolate per app").
5. **Spec-faithful, byte-faithful, observable-event-faithful.**
   Promise prototype tampering, `Object.defineProperty`
   accessors on Array/Object prototypes, and microtask ordering
   inside `tee`/`pipeTo` are all observable from JS; we use the
   same spec-defined hardening pattern as the ref impl
   (`uponPromise`, `transformPromiseWith` — see §VII.4) to
   keep observably-correct behaviour.

### Non-goals (explicit)

- **Transferable streams across MessagePort boundaries** (§9
  of the spec). D-7: out of scope for v1. The runtime does
  not ship a native MessagePort; transferring a stream into
  a Worker would require both. Workerd similarly defers (its
  `standard.h` does not implement transferable receivers).
  WPT `streams/transferable/` (13 files + 11 in `resources/`)
  is excluded from the v1 pass target. When MessagePort
  arrives, this proposal's storage shape (Rc<RefCell<…>> per
  slot) is forward-compatible: each transferable stream
  becomes a `SetUpCrossRealmTransformReadable` /
  `SetUpCrossRealmTransformWritable` pair plumbed over a
  native MessagePort.

  Note on the IDL: per critic finding #41, even though §9 is
  out of scope, the IDL on `ReadableStream`, `WritableStream`,
  `TransformStream` keeps `[Exposed=*]` only in v1 — the
  `[Transferable]` attribute is ADDED in v2 alongside the
  MessagePort implementation. This avoids
  `structuredClone({transfer:[s]})` declaring the stream
  transferable while the transfer machinery throws an opaque
  error (see Missing-concept #2).
- **`ReadableStream.from(asyncIterable)` user-passable
  iterables with non-trivial cleanup**. The static-method
  `from` (§3.2.1) IS in scope and shipped. Step 3.4 (calling
  `iter.return` during cancel) only fires reliably on native
  iterables; user-defined async-iterables that throw inside
  `return` produce an unhandled-promise warning, mirroring
  Chromium and Firefox.
- **HTTP/2 push streams as `ReadableStream`.** Out of scope (no
  HTTP/2 server push).
- **Owning-type chunks** (`UnderlyingSource.type = "owning"`) —
  spec §3.2.4 marks this as **not standardised**; only Chromium
  ships behind a flag for `MessagePort` / `VideoFrame`. WPT
  `readable-streams/owning-type*.js` (3 files) skipped.

### Status

Draft v2 — **near-complete implementation** on
`feature/streams-native`. Every IDL surface in spec §3-§5 ships native;
async iteration + ReadableStream.from landed; only the polyfill
cutover (D-19) remains.

**Landed (feature/streams-native):**

| Commit | Scope |
|--------|-------|
| `37504ad4` | runtime-macros: `EnforceRangeU64` newtype + extraction (§XIV.8) |
| `98d9b369` | runtime: `OpResult::JsValue` + `ResolveValue` (D-3 / §VII.5) |
| `c4537d1a` | streams: queue + slots + budget primitives (§V.3, §VI.1, §XII / D-18) |
| `2b0047c7` | streams: `enqueue_microtask` + `upon_promise` + `set_promise_is_handled_to_true` (D-12 / §VII.3-§VII.4) |
| `5a0bee5d` | streams: `ByteLengthQueuingStrategy` + `CountQueuingStrategy` (§6.2, §6.3, D-17) |
| `20b47470` | streams: `ReadableStream` + `ReadableStreamDefaultController` + `ReadableStreamDefaultReader` plus `NativeSource` trait + `from_native_source` (D-9) |
| `af1d7a84` | streams: WPT runner for `streams/readable-streams/` (constructor, general, default-reader) — 68/68 pass |
| `5e05f5b8` | streams: `WritableStream` + `WritableStreamDefaultController` + `WritableStreamDefaultWriter` plus `NativeSink` trait + `from_native_sink` (D-9) |
| `0ea0be77` | streams: WPT runner for `streams/writable-streams/` — 110/120 pass, 10 skip (AbortSignal) |
| `b1ff1836` | streams: native `TransformStream` + controller + cross-class algorithms (§II.11-§II.12) |
| `ab25d201` | streams: WPT runner for `streams/transform-streams/` |
| `c07c7102` | streams: native `pipeTo` / `pipeThrough` / `tee` (§IX, §X) |
| `1bca8e86` | streams: pipe + tee Native ReadRequest + WPT runners — 100% pipe (170/170), 100% tee (26/26) |
| `1e24bcbf` … `121837c3` | streams: byte streams / BYOB (§3.7-§3.8, D-6, D-15) — full byte controller + BYOB reader + BYOBRequest + byte-tee. WPT BYOB 185/185 |
| `591c33af` | streams: async iter (`values` + `@@asyncIterator`) + `ReadableStream.from` (§3.4.6 / D-8 / §IV) |
| `5db45fc0` | streams: async iter ongoing-promise sequencing + WPT runner — 41/41 |
| `25ac170f` | streams TS: shared `[[finishPromise]]` across abort/close/source-cancel (§5.4.6.{8,9,10}) |

**WPT compliance summary (this branch):**

| Suite | Pass | Fail | Skip |
|-------|-----:|-----:|-----:|
| readable-streams (constructor/general/default-reader) | 68 | 0 | 0 |
| writable-streams | 110 | 0 | 10 |
| transform-streams | 64 | 6 (deferred-known) | 0 |
| piping | 170 | 0 | 0 |
| tee | 26 | 0 | 0 |
| BYOB | 185 | 0 | 0 |
| async-iterator | 41 | 0 | 0 |
| **Total** | **664** | **6 (deferred-known)** | **10 (AbortSignal)** |

**Test count this branch:** 111 hand-written streams.rs integration
tests, plus 7 WPT runners (one cargo test each).

**Not yet shipped** — pending dispatches:
- Polyfill cutover (D-19) — native and polyfill coexist while the
  final cleanup lands.
- The 6 deferred-known TS WPT failures (controller.error/cancel
  ordering edge cases involving WS abort-pipeline interactions).

**Macro deviation:** §XIV.1's `#[v8_async_method]` extension is
deferred. Per §XIV.6, plain methods returning `v8::Local<v8::Promise>`
already work through the existing `Local` arm of `gen_call_return`.
Rather than build a full async-fn-to-Promise wrapper, stream methods
will allocate a `v8::PromiseResolver` synchronously, push a future
to `state.spawned_ops`, and the runtime loop drives the promise via
`OpResult::JsValue` (D-3, already wired). This matches the spec's
"either store-and-resolve-synchronously or route through compio"
pattern from §VII.5 and avoids the borrow-across-await machinery
in §XIV.1. If a future stream method genuinely needs `&mut self`
across `.await`, the macro can grow then.

Post-completion: file as a date-prefixed ADR under
`docs/decisions/`. The Decisions table below is the immutable
contract; everything else is illustrative.

### Decisions (settled)

| # | Decision | Rationale | Section |
|---|----------|-----------|---------|
| **D-1** | Pure native: every spec interface (ReadableStream, WritableStream, TransformStream, all controllers, all readers, BYOBRequest, queuing strategies) is a `#[v8_class]` Rust struct. No JS shim, no web-streams-polyfill fallback. | Single source of truth; eliminates the value-path/byte-path duality the JS skeleton accumulated; one GC graph; no "spec checks reject foreign stream class" hazards. | §I |
| **D-2** | Internal-slot storage rule, sharpened (was inconsistent in v1): **each spec slot lives in exactly ONE location.** A slot lives in a Rust struct field if and only if (a) its value is purely Rust-side data the spec never requires JS-identity preservation for AND (b) no Rust callsite needs to read it as a `v8::Local`. Otherwise the slot lives in a V8 private symbol. Slots that need both (e.g. `[[controller]]` — the wrapper IS observably `===`-comparable, but the controller's *state* is heavy Rust data) live as a **wrapper-in-priv-sym + state-in-Rc<RefCell>** pair, with the priv sym as the canonical identity store and the Rc<RefCell> reachable only via `controller_state(wrapper) -> &RefCell<State>`. There is no "mirror" — exactly one location per slot. | Spec algorithms must be observably indistinguishable from "directly modify `[[…]]`". The single-source rule, audited slot-by-slot in §XV, is the entire consistency model. | §V, §XV |
| **D-3** | Promise plumbing: `v8::PromiseResolver::new(scope)` for every spec promise. Async resolution from compio tasks uses a NEW `OpResult::JsValue` variant carrying `{ resolver: v8::Global<v8::PromiseResolver>, value: ResolveValue }` (where `ResolveValue` is an enum: Bytes, JsGlobal, Undefined, Reject). Verified: existing `OpResult::Completed.value: String` at `crates/runtime/src/state.rs:594-596` cannot carry V8 chunks. Stream chunks are arbitrary V8 values; new infrastructure is required. | The existing String channel is wrong for streams (it round-trips chunks through UTF-8). Either we add `OpResult::JsValue` or an out-of-band registry keyed by op id; the variant is simpler and reuses the existing dispatch loop in `runtime.rs`. | §VII.5 |
| **D-4** | Single-threaded per isolate: every Rust struct is `!Send + !Sync`. No `Mutex`/`RwLock` anywhere. Inter-class references use `Rc<RefCell<…>>`. | AGENTS.md "V8 per thread, one isolate per app". A `Send` constraint would force `Arc<Mutex<…>>` and slow the read fast-path. workerd takes the same `kj::Own` (RAII single-thread) approach. | §V |
| **D-5** | Queue storage: `VecDeque<QueueEntry>` with size tracking in a separate `f64` field for `[[queueTotalSize]]`. NOT a multi-consumer ring buffer. Each tee branch has its OWN queue (see D-11). | The spec's queue is single-consumer; tee creates *new* streams with their own queues fed from a shared source-side reader. Workerd's multi-consumer optimisation breaks observable `desiredSize` after partial-tee consumption (its own bug tracker mentions this; we don't reproduce). | §VI |
| **D-6** | Byte stream / BYOB (§3.7): full implementation in v1, including `pendingPullIntos`, `respondWithNewView`, `min` parameter on `read({…min})`, auto-allocate-chunk-size, ArrayBuffer detachment via `TransferArrayBuffer`. | Compression's lenient-deflate path and fetch's HTTP body forwarding both want byte streams; deferring BYOB would force the body bridge to keep the legacy slot path alive indefinitely. | §III, §VI |
| **D-7** | Transferable streams (§9): out of scope. WPT `streams/transferable/` (13 files + 11 in `resources/`) skipped. v1 IDL drops `[Transferable]`; v2 adds it together with the MessagePort implementation. | No native MessagePort. See Non-Goals. | (none) |
| **D-8** | Async iteration (§3.4.6): native, not via JS-side wrapping. `[Symbol.asyncIterator]` and `values(options)` produce a Rust iterator object that holds a `reader: v8::Global<v8::Object>`. The iterator detects "finished" via `reader.stream === undefined` post-release (matches ref impl `ReadableStreamAsyncIterator-impl.js`). `preventCancel` defaults `false`. | Spec compliance plus removing the `for-await-of` boilerplate currently in `streams.js` lines 320-341. | §IV |
| **D-9** | "Internal" stream construction API (Rust-only, not JS-visible): `ReadableStream::from_native_source(scope, …)`, `WritableStream::from_native_sink(…)`, `TransformStream::from_native_transformer(…)`. These build the same V8 objects the public constructor does, but accept Rust traits. | Compression's BLOCKER-4 fix requires this. Internal fetch decompression chains a TransformStream onto a body without ever exposing a JS ReadableStream that could be locked first. | §VIII |
| **D-10** | `pipeThrough` on a stream the caller owns: routes through public `pipeTo` (lock checks fire). `pipeThrough` on internal hand-offs (compression, fetch decode): uses a *parallel* internal `pipe_native_internal` that bypasses lock acquisition because both ends are still being constructed and have NEVER been observable from JS (see C-12 invariant in §IX.4). | The spec's `pipeThrough` algorithm calls `pipeTo` and locks both streams; doing this on an internally-constructed body before user code sees it is observably indistinguishable from the spec, but doesn't trip our own lock check on the next-microtask user `body.getReader()` call. | §IX |
| **D-11** | tee: each branch is an **independent ReadableStream** with its OWN ReadableStreamDefaultController (or ReadableByteStreamController for byte tee) and its OWN queue. The two branches share a single source-side reader via a SHARED `pullAlgorithm` that issues one read on the source per pull and enqueues the chunk into BOTH branches' controllers. There is no shared queue. (v1 fix: critic C-6 — the v1 wording was misleading.) Branches' cancel is composite: cancellation propagates to the source only when both branches cancel. | Spec; required for compression's potential future "tee response and stream both decoded + raw" feature. | §X |
| **D-12** | Microtask ordering: every `queueMicrotask(…)` step in the spec maps to V8's `Isolate::enqueue_microtask(microtask: Local<Function>)` (verified `v8-147.1.0/src/isolate.rs:1676-1680`). Closure-to-Function bridging uses a one-shot `FunctionTemplate` whose `External` data carries an `Rc<RefCell<Option<Box<dyn FnOnce>>>>` (§VII.3). The spec's `Promise.resolve().then(steps)` pattern is NOT equivalent to `queueMicrotask` — `Promise.resolve()` adds two microtasks (resolve + then handler) per ECMA-262 PromiseReactionJob, whereas `queueMicrotask` adds one. Stream algorithms that say "queue a microtask" use `enqueue_microtask`; stream algorithms that say "react to a promise" (`uponPromise`/`uponFulfillment`) use the promise chain. | WPT `readable-streams/tee.any.js` "should not pull more chunks than were specified" specifically counts microtask hops; mismatching one for the other fails. | §VII |
| **D-13** | Locking: each ReadableStream/WritableStream has a `[[reader]]` / `[[writer]]` V8 private symbol. Acquiring sets it; release clears it. `getReader()` throws TypeError if `[[reader]]` set; `pipeTo`/`pipeThrough`/`tee` throw if `locked === true`. | Spec. | §V, §X |
| **D-14** | `ReadableStream.from(asyncIterable)` (§3.2.1): supported in v1. Algorithm: `let iterator = GetIterator(asyncIterable, async)`; on the `async`-flag iteration error, fall back to `GetIterator(asyncIterable, sync)` per ECMA-262 — the sync iterator is wrapped via `IteratorClose`-aware async adapter. Maps to a native UnderlyingSource whose `pull` invokes `iterator.next()` once per pull. | Spec; tested by WPT `readable-streams/from.any.js`. The sync-fallback is missing-concept #8. | §III.1 |
| **D-15** | `BYOBRequest.respond(bytesWritten)` semantics — gates on **stream state**, not controller `closeRequested`. Per ref impl `ReadableByteStreamControllerRespond` (verified): `if (state === 'closed') { if (bytesWritten !== 0) throw TypeError; } else { assert(state === 'readable'); if (bytesWritten === 0) throw TypeError; if (bytesFilled + bytesWritten > byteLength) throw RangeError; }`. **Crucially**: after `controller.close()` with non-empty queue, `closeRequested === true` but `state === 'readable'` — `respond(0)` MUST throw TypeError in this window. (v1 said the opposite — fixed in v2.) | Spec text; WPT `readable-byte-streams/respond-after-enqueue.any.js`. | §III.4 |
| **D-16** | Detached buffer handling: every BYOB path checks `buffer.is_detached()` (V8 API: `ArrayBuffer::was_detached`) before use. **Every** chunk path — including the WritableStream `write(chunk)` and TransformStream input — also checks `chunk.buffer.was_detached()` per missing-concept #7. | WPT `readable-byte-streams/non-transferable-buffers.any.js` plus enqueue-with-detached-buffer.any.js. | §III.4 |
| **D-17** | Strategies: `ByteLengthQueuingStrategy` and `CountQueuingStrategy` are full `#[v8_class]` types. Their `size` is a *per-realm* function (one shared function reused across instances), per WPT `queuing-strategies-size-function-per-global.window.js`. The QueuingStrategyInit dictionary is required-when-present (`new ByteLengthQueuingStrategy()` throws because `init.highWaterMark` is required); see fix to critic #33. | WPT requires `Object.is(s1.size, s2.size) === true` for same-realm strategies. | §XI |
| **D-18** | Per-isolate concurrent stream cap: 65,536 streams. Excess constructions throw `RangeError("too many concurrent streams")`. **Justification (was thin air in v1):** target 200K req/s × 32-thread worker = ~6,250 req/s/thread, with mean lifecycle 50ms = 312 streams in flight per thread; 65,536 is 200× headroom. Cap is per-thread per-isolate. If creator apps grow streaming chains (e.g. tee × N branches × pipeThrough × M transforms) and approach this, raise to u32::MAX with a `track_alloc/free` instrumentation pass instead of a fixed cap. | The cap is a guard against runaway construction; it is NOT a perf limit. | §XII |
| **D-19** | Polyfill removal cadence: three landings — (1) ship native behind feature flag `runtime_native_streams`, polyfill remains default; (2) flip default to native, polyfill remains as fallback; (3) delete polyfill JS files entirely. The native cutover (step 2) ALSO deletes `crates/runtime/src/embed/streams.js` (the JS-side bridge skeleton, NOT the polyfill). The polyfill at `streams-polyfill.js` is deleted in step 3 only. (v1 conflated these — fixed.) | Risk control. Identical pattern to headers-native.md. | §XIII |
| **D-20** | Spec algorithm naming in Rust: every spec abstract operation gets a Rust function with the same name in `snake_case`. Lives in `crates/runtime/src/streams/algorithms.rs` for cross-class operations (`ReadableStreamFulfillReadRequest`, `ReadableStreamCancel`, etc.) and in the relevant class file for class-local operations (e.g. `ReadableStreamDefaultControllerEnqueue` in `readable_default_controller.rs`). The split rule: an algorithm named after a class (e.g. `ReadableStreamDefaultController…`) lives in that class's file; an algorithm named after a stream operation (e.g. `ReadableStream…`, `TransformStream…`) that doesn't operate on a single class's state lives in `algorithms.rs`. | Reduces the cognitive load of cross-referencing the spec; reviewer can grep for the exact spec name. | §III–VI |


## I. Architecture overview

Two systems share no state at the V8 level:

1. **Public IDL surface.** Each interface listed below installs
   a `#[v8_class]` on the global object. The class wrapper holds
   a `Box<{Class}State>` in V8 internal field 0 (existing macro
   pattern from `crates/runtime/src/text_encoding.rs` and
   `crates/runtime/src/headers.rs`).
2. **Internal slot map.** Every spec internal slot (`[[state]]`,
   `[[storedError]]`, `[[reader]]`, `[[controller]]`, `[[queue]]`,
   `[[strategyHWM]]`, `[[strategySizeAlgorithm]]`,
   `[[pendingPullIntos]]`, etc.) lives in EXACTLY ONE place per
   D-2 — see §V.2 storage-rule audit and §XV per-class table.

The boundary between (1) and (2) is sharp: V8 callbacks delegate
to Rust methods on the boxed state; Rust methods that need to
read JS values reach into V8 private slots via the `slots.rs`
helpers (§V.3).

```
┌────────────────────────────────────────────────────────────┐
│ V8 isolate                                                 │
│  ┌──────────────────┐    ┌──────────────────┐              │
│  │ globalThis.      │    │ globalThis.      │  …           │
│  │ ReadableStream   │    │ WritableStream   │              │
│  └────────┬─────────┘    └────────┬─────────┘              │
│           │ instance                │                       │
│           ▼                          ▼                       │
│  ┌──────────────────┐    ┌──────────────────┐              │
│  │ JS wrapper obj   │    │ JS wrapper obj   │              │
│  │ ┌──────────────┐ │    │ ┌──────────────┐ │              │
│  │ │ slot[0]:     │ │    │ │ slot[0]:     │ │              │
│  │ │  Box<RSState>│ │    │ │  Box<WSState>│ │              │
│  │ └──────┬───────┘ │    │ └──────┬───────┘ │              │
│  │   private        │    │   private        │              │
│  │   symbols:       │    │   symbols:       │              │
│  │   reader,        │    │   writer,        │              │
│  │   controllerObj, │    │   controllerObj, │              │
│  │   storedError    │    │   storedError    │              │
│  └──────────────────┘    └──────────────────┘              │
│                                                            │
│           Box<RSState> (Rust)                              │
│  ┌─────────────────────────────────────────────┐           │
│  │ state: Cell<StreamState>                    │           │
│  │ disturbed: Cell<bool>                       │           │
│  │ controller: Rc<RefCell<RSControllerState>>  │           │
│  │ pending_read_requests: VecDeque<ReadRequest>│           │
│  │ self_weak: WeakV8Ref                        │           │
│  └─────────────────────────────────────────────┘           │
└────────────────────────────────────────────────────────────┘
```

### I.1. Native-construction API (D-9)

Three Rust-only entry points that build the same V8 objects the
public constructors do, but accept Rust traits:

```rust
impl ReadableStream {
    /// Build a JS ReadableStream wrapping a Rust source. Used by:
    /// - fetch response body bridge
    /// - CompressionStream's internal output side
    pub fn from_native_source<S: NativeSource + 'static>(
        scope: &mut v8::PinScope,
        source: S,
        strategy: QueuingStrategy,
    ) -> v8::Local<'_, v8::Object>;

    /// Internal slot read for `[[controller]]`, callable from Rust
    /// without going through any JS-visible accessor.
    pub fn controller_slot<'s>(
        scope: &mut v8::PinScope<'s, '_>,
        rs: v8::Local<v8::Object>,
    ) -> Rc<RefCell<ReadableStreamControllerState>>;
}

impl WritableStream {
    pub fn from_native_sink<S: NativeSink + 'static>(
        scope: &mut v8::PinScope,
        sink: S,
        strategy: QueuingStrategy,
    ) -> v8::Local<'_, v8::Object>;
}

impl TransformStream {
    pub fn from_native_transformer<T: NativeTransformer + 'static>(
        scope: &mut v8::PinScope,
        transformer: T,
        writable_strategy: QueuingStrategy,
        readable_strategy: QueuingStrategy,
    ) -> v8::Local<'_, v8::Object>;

    /// `[[readable]]` slot read — used by GenericTransformStream
    /// getters in CompressionStream.
    pub fn readable_slot<'s>(
        scope: &mut v8::PinScope<'s, '_>,
        ts: v8::Local<v8::Object>,
    ) -> v8::Local<'s, v8::Value>;

    pub fn writable_slot<'s>(
        scope: &mut v8::PinScope<'s, '_>,
        ts: v8::Local<v8::Object>,
    ) -> v8::Local<'s, v8::Value>;
}
```

Note (§I.1 boundary, addressing critic #49): `from_native_*` are
the ONLY public construction surface for Rust callers. There is
no `pub fn ReadableStream::new(rust_struct) -> Self` — the Box
lifetime is owned by V8 once the wrapper exists. Callers stay
above the `from_native_*` line; they never see the `Box<RSState>`.

`NativeSource`, `NativeSink`, `NativeTransformer` are the trait
shapes documented in §VIII. **Critical** (addressing C-12,
critic #12): `from_native_*` is documented `#[doc(hidden)]` AND
guarded by an explicit invariant: callers MUST NOT pass a JS-
bridged source/sink (i.e. one that wraps an existing JS
ReadableStream/WritableStream object). The public lock-acquiring
path is the only correct way to wrap a JS-visible stream as a
NativeSource — passing one through `from_native_source` would
turn `pipe_native_internal` into a lock-bypass backdoor. Pull
requests adding a JS-bridged Source/Sink type MUST update the
invariant.

### I.2. File layout

```
crates/runtime/src/streams/
├── mod.rs                       (new) module root, public exports
├── readable.rs                  (new) ReadableStream class + methods
├── readable_default_controller.rs   (new) DefaultController class
├── readable_byte_controller.rs  (new) ByteStreamController class
├── readable_default_reader.rs   (new) DefaultReader class
├── readable_byob_reader.rs      (new) BYOBReader class
├── byob_request.rs              (new) BYOBRequest class
├── writable.rs                  (new) WritableStream class
├── writable_controller.rs       (new) WritableDefaultController class
├── writable_writer.rs           (new) WritableDefaultWriter class
├── transform.rs                 (new) TransformStream class
├── transform_controller.rs      (new) TransformDefaultController class
├── strategies.rs                (new) ByteLength + Count strategies
├── queue.rs                     (new) VecDeque<QueueEntry> +
│                                      [[queueTotalSize]] invariant
├── pipe.rs                      (new) ReadableStreamPipeTo
├── tee.rs                       (new) ReadableStreamDefaultTee +
│                                      ReadableByteStreamTee
├── async_iter.rs                (new) async iteration objects
├── pull_into.rs                 (new) PullIntoDescriptor + helpers
├── algorithms.rs                (new) Cross-class spec abstract
│                                      operations (ReadableStreamCancel,
│                                      ReadableStreamFulfillReadRequest,
│                                      ReadableStreamFromIterable, etc.)
│                                      Per D-20 split rule.
├── slots.rs                     (new) V8 private symbol helpers
├── budget.rs                    (new) D-18 concurrent stream cap
└── promise_resolve.rs           (new) D-3 resolve-from-Rust helpers
                                       (uses OpResult::JsValue)

crates/runtime/src/lib.rs        (modified) +pub mod streams;
crates/runtime/src/init.rs       (modified) install all classes
crates/runtime/src/state.rs      (modified) add OpResult::JsValue
                                            variant (D-3)

crates/runtime/src/embed/streams.js
                                  (deleted in D-19 step 2 — JS bridge)
crates/runtime/src/embed/streams-polyfill.js
                                  (deleted in D-19 step 3 — polyfill)

crates/runtime-macros/src/v8_class.rs  (modified) §XIV
crates/runtime-macros/src/lib.rs       (modified) §XIV

crates/runtime/tests/
├── streams.rs                    (new) hand-written smoke tests
├── streams_native.rs             (new) internal-API tests
├── wpt_streams.rs                (new) WPT runner
└── wpt/streams/                  (vendored from web-platform-tests
                                   at a pinned commit; see §XXI)
```

## II. Interface surface — exhaustive

The spec inventory (cross-referenced against
https://streams.spec.whatwg.org/#dom-readablestream through
end-of-spec):

| # | Interface | Section | Internal-field count | LOC est. |
|---|-----------|---------|----------------------|----------|
| 1 | `ReadableStream` | §3.2 | 1 (Box<RSState>) | ~600 |
| 2 | `ReadableStreamGenericReader` (mixin) | §3.3 | n/a — mixin | (folded into 3,4) |
| 3 | `ReadableStreamDefaultReader` | §3.4 | 1 | ~250 |
| 4 | `ReadableStreamBYOBReader` | §3.5 | 1 | ~280 |
| 5 | `ReadableStreamDefaultController` | §3.6 | 1 | ~350 |
| 6 | `ReadableByteStreamController` | §3.7 | 1 | ~600 |
| 7 | `ReadableStreamBYOBRequest` | §3.8 | 1 | ~120 |
| 8 | `WritableStream` | §4.2 | 1 | ~400 |
| 9 | `WritableStreamDefaultController` | §4.3 | 1 | ~280 |
| 10 | `WritableStreamDefaultWriter` | §4.4 | 1 | ~280 |
| 11 | `TransformStream` | §5.2 | 1 + 1 priv-sym `transformerCodec` (compression-dep #5) | ~300 |
| 12 | `TransformStreamDefaultController` | §5.3 | 1 | ~150 |
| 13 | `ByteLengthQueuingStrategy` | §6.2 | 1 | ~60 |
| 14 | `CountQueuingStrategy` | §6.3 | 1 | ~60 |

### II.1. `ReadableStream` (§3.2)

**IDL (§3.2.1):**
```webidl
[Exposed=*]
interface ReadableStream {
  constructor(optional object underlyingSource, optional QueuingStrategy strategy = {});

  static ReadableStream from(any asyncIterable);

  readonly attribute boolean locked;

  Promise<undefined> cancel(optional any reason);
  ReadableStreamReader getReader(optional ReadableStreamGetReaderOptions options = {});
  ReadableStream pipeThrough(ReadableWritablePair transform, optional StreamPipeOptions options = {});
  Promise<undefined> pipeTo(WritableStream destination, optional StreamPipeOptions options = {});
  sequence<ReadableStream> tee();

  async iterable<any>(optional ReadableStreamIteratorOptions options = {});
};
```

(Note: `[Transferable]` removed in v1 IDL per critic #41 + D-7
deferral — reinstated in v2.)

**Internal slots (§3.2.5):** `[[controller]]`, `[[Detached]]`
(deferred), `[[disturbed]]`, `[[reader]]`, `[[state]]`,
`[[storedError]]`. Per-slot storage in §XV.1.

**Rust struct:**

```rust
pub struct ReadableStreamState {
    state: Cell<StreamState>,
    disturbed: Cell<bool>,
    /// Source of truth for [[controller]]. Variant tagged by
    /// underlyingSource.type. Mutually exclusive per stream.
    controller: ControllerVariant,
    /// Self-pointer back to the V8 wrapper object. The controller
    /// reaches back to the stream wrapper to read [[reader]] etc.
    self_weak: WeakV8Ref,
}

pub enum StreamState { Readable, Closed, Errored }

pub enum ControllerVariant {
    Default(Rc<RefCell<DefaultControllerState>>),
    Byte(Rc<RefCell<ByteControllerState>>),
}
```

`WeakV8Ref` is a thin wrapper around `v8::Weak<v8::Object>`.
Per critic #22: `upgrade()` returns `Option<v8::Local<'s,
v8::Object>>`. None occurs ONLY if the wrapper has been GC'd
(impossible in practice — see GC analysis below). Algorithms
that call `upgrade()` and find None treat it as a logic error
(unreachable in our single-isolate single-thread setting; the
controller is always reachable while the stream wrapper is alive,
because the controller is held by the wrapper's internal field's
Box, which holds the controller's `Rc`, which keeps it alive).
The only path that sees `None` legitimately is during the V8
weak finalizer for the stream's internal-field Box; that
finalizer detaches the controller before any user code can see
it. So algorithms that hold a `WeakV8Ref` at user-callback time
unconditionally `unwrap()`.

### II.2. `ReadableStreamGenericReader` (§3.3, mixin)

```webidl
interface mixin ReadableStreamGenericReader {
  readonly attribute Promise<undefined> closed;
  Promise<undefined> cancel(optional any reason);
};
```

Internal slots: `[[closedPromise]]` (Promise), `[[stream]]`
(ReadableStream | undefined). Both V8 private symbols. Folded
into Default/BYOB reader install codegen.

### II.3. `ReadableStreamDefaultReader` (§3.4)

**IDL:**
```webidl
[Exposed=*]
interface ReadableStreamDefaultReader {
  constructor(ReadableStream stream);
  Promise<ReadableStreamReadResult> read();
  undefined releaseLock();
};
ReadableStreamDefaultReader includes ReadableStreamGenericReader;
```

**Internal slots:** (mixin) + `[[readRequests]]`.

**Read-request struct (§3.4.4):** the spec defines THREE
independent algorithms per request — `chunkSteps(chunk)`,
`closeSteps()`, `errorSteps(error)` — invoked exactly once
total per request lifetime (chunkSteps OR closeSteps OR
errorSteps — never two; see ref impl
`ReadableStreamFulfillReadRequest` which `shift()`s the
request and calls one of them).

Critic #15 fix: a single `FnOnce` closure consuming an enum
variant correctly models this: the closure is called exactly
once with one of three outcomes.

```rust
pub struct ReadRequest {
    pub kind: ReadRequestKind,
}
pub enum ReadRequestKind {
    /// User-facing read() call: resolve a promise resolver with
    /// {value, done}.
    Js {
        resolver: v8::Global<v8::PromiseResolver>,
    },
    /// Internal pipe loop / native consumer.
    /// The closure dispatches once on a single NativeReadOutcome
    /// — the trio chunkSteps/closeSteps/errorSteps is encoded as
    /// the three variants of NativeReadOutcome.
    Native {
        sink: Box<dyn FnOnce(&mut v8::PinScope, NativeReadOutcome) + 'static>,
    },
}
pub enum NativeReadOutcome {
    Chunk(JsChunk),
    Close,                              // closeSteps()
    Error(v8::Global<v8::Value>),       // errorSteps(error)
}
```

`JsChunk` is `v8::Global<v8::Value>` for default streams; for
byte streams the Native sink can ask for the chunk as a
`Vec<u8>` (zero-copy view if the queue entry is byte-typed).

### II.4. `ReadableStreamBYOBReader` (§3.5)

```webidl
[Exposed=*]
interface ReadableStreamBYOBReader {
  constructor(ReadableStream stream);

  Promise<ReadableStreamReadResult> read(
    ArrayBufferView view,
    optional ReadableStreamBYOBReaderReadOptions options = {});

  undefined releaseLock();
};
ReadableStreamBYOBReader includes ReadableStreamGenericReader;

dictionary ReadableStreamBYOBReaderReadOptions {
  [EnforceRange] unsigned long long min = 1;
};
```

**Internal slots:** (mixin) + `[[readIntoRequests]]`.

**`min` validation (critic #28):** the spec performs three
checks at the WebIDL boundary, BEFORE any algorithm runs:
1. If `min === 0`, throw `TypeError`.
2. If `view.[[ArrayLength]] === 0`, throw `TypeError`.
3. If `view.byteLength === 0` (detached), throw `TypeError`.
4. If `min > view.byteLength / elementSize`, throw `RangeError`.

The boundary check lives in the BYOBReader's `read` method
**before** the call to `ReadableStreamBYOBReaderRead`. WPT
`readable-byte-streams/read-min.any.js` covers this exhaustively.

### II.5. `ReadableStreamDefaultController` (§3.6)

**IDL:**
```webidl
[Exposed=*]
interface ReadableStreamDefaultController {
  readonly attribute unrestricted double? desiredSize;
  undefined close();
  undefined enqueue(optional any chunk);
  undefined error(optional any e);
};
```

**Internal slots (§3.6.5):** `[[cancelAlgorithm]]`,
`[[closeRequested]]`, `[[pullAgain]]`, `[[pullAlgorithm]]`,
`[[pulling]]`, `[[queue]]`, `[[queueTotalSize]]`, `[[started]]`,
`[[strategyHWM]]`, `[[strategySizeAlgorithm]]`, `[[stream]]`.

**Internal methods:** `[[CancelSteps]](reason)`,
`[[PullSteps]](readRequest)`, `[[ReleaseSteps]]()`.

**Storage:**

```rust
pub struct DefaultControllerState {
    cancel_algorithm: AlgorithmFn,
    pull_algorithm: AlgorithmFn,
    start_promise: Option<v8::Global<v8::Promise>>,
    close_requested: Cell<bool>,
    pull_again: Cell<bool>,
    pulling: Cell<bool>,
    started: Cell<bool>,
    queue: RefCell<VecDeque<ValueQueueEntry>>,
    queue_total_size: Cell<f64>,
    strategy_hwm: f64,
    strategy_size: SizeAlgorithm,
    stream_weak: WeakV8Ref,
}
```

Critic #37: rename `AlgorithmFn` → `UnderlyingCallback` is
considered but rejected — the spec uses "algorithm" too
(`pullAlgorithm`, `cancelAlgorithm`); we match. Documentation
clarifies the union: the spec distinguishes "user-supplied
JS callback" (e.g. `underlyingSource.pull`) from "spec-defined
algorithm" (e.g. `TransformStreamDefaultSourcePullAlgorithm`),
but both are stored in the same `AlgorithmFn` enum because at
runtime they're called identically — the variant tag tells us
whether to invoke a `v8::Global<Function>` or a Rust closure.

```rust
pub enum AlgorithmFn {
    /// User-supplied JS callback (from underlyingSource/Sink/Transformer
    /// dictionaries). `this` is the underlying-* dictionary itself.
    Js {
        function: v8::Global<v8::Function>,
        this_obj: v8::Global<v8::Object>,
    },
    /// Rust closure (NativeSource/Sink/Transformer impls and
    /// TransformStreamDefaultSink* helpers).
    /// Returns a future per critic #5 — the spec algorithm is async.
    Native(Box<dyn FnMut(NativeAlgArgs) -> Pin<Box<dyn Future<Output = Result<v8::Global<v8::Value>, v8::Global<v8::Value>>> + 'static>>>),
    /// No-op algorithm (default for missing user callbacks).
    Noop,
}
```

### II.6. `ReadableByteStreamController` (§3.7)

**IDL:**
```webidl
[Exposed=*]
interface ReadableByteStreamController {
  readonly attribute ReadableStreamBYOBRequest? byobRequest;
  readonly attribute unrestricted double? desiredSize;
  undefined close();
  undefined enqueue(ArrayBufferView chunk);
  undefined error(optional any e);
};
```

**Internal slots (§3.7.1):** `[[autoAllocateChunkSize]]`,
`[[byobRequest]]`, `[[cancelAlgorithm]]`, `[[closeRequested]]`,
`[[pullAgain]]`, `[[pullAlgorithm]]`, `[[pulling]]`,
`[[pendingPullIntos]]`, `[[queue]]`, `[[queueTotalSize]]`,
`[[started]]`, `[[strategyHWM]]`, `[[stream]]`.

**Pull-into descriptor (used in `pendingPullIntos`):**
```text
{ buffer, bufferByteLength, byteOffset, byteLength,
  bytesFilled, minimumFill, elementSize, viewConstructor,
  readerType: enum {default, byob, none} }
```

`readerType: none` corresponds to a chunk filled while the reader
was released; the descriptor stays in `pendingPullIntos` until
enqueueable.

**Storage (D-2 single-source rule applied — fix to critic #34):**
`[[byobRequest]]` lives ONLY in the V8 private symbol
`byobRequestObj` on the controller wrapper. The Rust state
DOES NOT mirror it. Reads from algorithms go via
`read_slot(scope, controller_obj, "byobRequestObj")`.
Invalidation calls `delete_slot(scope, controller_obj,
"byobRequestObj")` plus clears the request's own `[[controller]]`
and `[[view]]` slots.

```rust
pub struct ByteControllerState {
    auto_allocate_chunk_size: Option<u64>,
    cancel_algorithm: AlgorithmFn,
    pull_algorithm: AlgorithmFn,
    close_requested: Cell<bool>,
    pull_again: Cell<bool>,
    pulling: Cell<bool>,
    started: Cell<bool>,
    pending_pull_intos: RefCell<VecDeque<PullIntoDescriptor>>,
    queue: RefCell<VecDeque<ByteQueueEntry>>,
    queue_total_size: Cell<f64>,
    strategy_hwm: f64,
    stream_weak: WeakV8Ref,
    // [[byobRequest]] is in the V8 priv sym `byobRequestObj` on
    // the controller wrapper — NOT here.
}
```

**Byte-queue invariant (critic C-9, missing in v1):**

> **INVARIANT:** `queue_total_size == sum(byte_length for entry in queue)`.

Maintained by exactly these algorithms (every other read-only
access of `queue_total_size` may rely on the invariant):
- `EnqueueValueWithSize` (`enqueue_value_with_size`) — adds
  `entry.byte_length` to total.
- `DequeueValue` (`dequeue_value`) — subtracts the popped
  entry's byte_length.
- `ResetQueue` (`reset_queue`) — clears queue, sets total to 0.
- `ReadableByteStreamControllerHandleQueueDrain` checks
  `queue_total_size === 0` (the invariant lets it skip an O(n)
  walk).
- `ReadableByteStreamControllerProcessPullIntoDescriptorsUsingQueue`
  (returns `filledPullIntos: Vec<PullIntoDescriptor>` — critic
  #38: ref impl returns the list of filled-and-popped
  descriptors so the caller commits them; we match).

### II.7. `ReadableStreamBYOBRequest` (§3.8)

```webidl
[Exposed=*]
interface ReadableStreamBYOBRequest {
  readonly attribute ArrayBufferView? view;
  undefined respond([EnforceRange] unsigned long long bytesWritten);
  undefined respondWithNewView(ArrayBufferView view);
};
```

**Internal slots:**
- `[[controller]]` — ReadableByteStreamController | undefined
- `[[view]]` — ArrayBufferView | null

**Storage (D-2 single-source rule, fix to critic #19 nested
RefCell deadlock):**

```rust
pub struct BYOBRequestState {
    // No fields — both slots are V8 private symbols on the
    // wrapper. This eliminates the nested-RefCell deadlock
    // path the v1 design risked when InvalidateBYOBRequest
    // ran while the controller was already borrowed.
}
```

Reads/writes:
- `[[controller]]` → `controllerObj` priv sym (lookup gets
  `v8::Local<Object>`; `controller_state(controller_obj)`
  retrieves the `Rc<RefCell<ByteControllerState>>` from its
  internal field 0).
- `[[view]]` → `viewObj` priv sym.

`InvalidateBYOBRequest` (§3.7.x) clears both via
`delete_slot(scope, request_obj, "controllerObj")` and
`delete_slot(scope, request_obj, "viewObj")` — plain V8
operations, no RefCell.

### II.8. `WritableStream` (§4.2)

**IDL:**
```webidl
[Exposed=*]
interface WritableStream {
  constructor(optional object underlyingSink, optional QueuingStrategy strategy = {});
  readonly attribute boolean locked;
  Promise<undefined> abort(optional any reason);
  Promise<undefined> close();
  WritableStreamDefaultWriter getWriter();
};
```

**Internal slots (§4.2.5):** `[[backpressure]]`, `[[closeRequest]]`
(Promise | undefined), `[[controller]]`, `[[Detached]]`,
`[[inFlightWriteRequest]]` (Promise), `[[inFlightCloseRequest]]`
(Promise), `[[pendingAbortRequest]]` ({promise, reason,
wasAlreadyErroring}), `[[state]]` (writable/closed/erroring/
errored), `[[storedError]]`, `[[writeRequests]]` (list of
**Promises**), `[[writer]]`.

**Storage (critic C-13, #26 fix):**
`[[writeRequests]]` stores **Promises**, not Resolvers. The
PromiseResolvers are held separately in the writer's
`pending_write_resolvers` so we can resolve/reject them; the
`[[writeRequests]]` list holds the Promises returned to JS by
`writer.write()` (used by `WritableStreamFinishErroring` to
iterate and call `rejectPromise(promise, e)` on each). This is
spec-faithful and matches the ref impl.

```rust
pub struct WritableStreamState {
    state: Cell<WSState>,
    backpressure: Cell<bool>,
    close_request: RefCell<Option<v8::Global<v8::Promise>>>,
    in_flight_write_request: RefCell<Option<v8::Global<v8::Promise>>>,
    in_flight_close_request: RefCell<Option<v8::Global<v8::Promise>>>,
    pending_abort_request: RefCell<Option<PendingAbortRequest>>,
    /// SPEC-FAITHFUL: list of Promises (the values returned to JS
    /// from writer.write()). Resolvers are held by the writer.
    write_requests: RefCell<VecDeque<v8::Global<v8::Promise>>>,
    /// PromiseResolvers that drive the [[writeRequests]] Promises.
    /// Same length as write_requests.
    write_request_resolvers: RefCell<VecDeque<v8::Global<v8::PromiseResolver>>>,
    controller: Rc<RefCell<WSControllerState>>,
    self_weak: WeakV8Ref,
}

pub enum WSState { Writable, Closed, Erroring, Errored }

pub struct PendingAbortRequest {
    promise: v8::Global<v8::PromiseResolver>,
    reason: v8::Global<v8::Value>,
    was_already_erroring: bool,
}
```

`[[storedError]]` and `[[writer]]` use V8 private symbols.

### II.9. `WritableStreamDefaultController` (§4.3)

**IDL:**
```webidl
[Exposed=*]
interface WritableStreamDefaultController {
  readonly attribute AbortSignal signal;
  undefined error(optional any e);
};
```

**Internal slots (§4.3.5):** `[[abortAlgorithm]]`,
`[[abortController]]` (the source of `signal`),
`[[closeAlgorithm]]`, `[[queue]]`, `[[queueTotalSize]]`,
`[[started]]`, `[[strategyHWM]]`, `[[strategySizeAlgorithm]]`,
`[[stream]]`, `[[writeAlgorithm]]`.

`AbortController` / `AbortSignal` are already provided by the
existing native runtime (see `crates/runtime/src/embed/events.js`
+ `runtime-macros`'s AbortSignal hook). v1 doesn't add new
abort infrastructure; the controller calls
`abortController.abort(reason)` on the existing class.

Verified (critic #48): `globalThis.AbortSignal` IS a real V8
class with native-side identity (via the macro pattern in
`events.js` + the AbortSignal hook in `runtime-macros`); the
WritableStreamDefaultController's `signal` getter returns the
JS-visible AbortSignal wrapper, and the controller's
`underlying-sink.write(chunk, controller)` callback receives
the same-identity object. Aborting via `abortController.abort()`
fires synchronously on the listener path in JS-land.

### II.10. `WritableStreamDefaultWriter` (§4.4)

**IDL:**
```webidl
[Exposed=*]
interface WritableStreamDefaultWriter {
  constructor(WritableStream stream);
  readonly attribute Promise<undefined> closed;
  readonly attribute unrestricted double? desiredSize;
  readonly attribute Promise<undefined> ready;
  Promise<undefined> abort(optional any reason);
  Promise<undefined> close();
  undefined releaseLock();
  Promise<undefined> write(optional any chunk);
};
```

**Internal slots:** `[[closedPromise]]`, `[[readyPromise]]`,
`[[stream]]`. (`[[desiredSize]]` is computed.)

**Promise lifecycle (critic C-13, #44 fix):** the spec
distinguishes "create new promise" (`newPromise`) from "resolve
existing promise". `WritableStreamDefaultWriterEnsureReadyPromiseRejected`
re-creates `[[readyPromise]]` as already-rejected (replaces the
old promise wholesale); `WritableStreamUpdateBackpressure(stream,
false)` resolves the existing readyPromise. We model this with
a **paired storage** for each promise:

```rust
pub struct WSWriterState {
    // Paired storage: the Promise (returned to JS via the getter)
    // and its Resolver (used by Rust to fulfill/reject). When the
    // spec says "create a new promise", we drop both and allocate
    // a fresh PromiseResolver, exposing its promise.
    closed_promise: RefCell<v8::Global<v8::Promise>>,
    closed_resolver: RefCell<v8::Global<v8::PromiseResolver>>,
    ready_promise: RefCell<v8::Global<v8::Promise>>,
    ready_resolver: RefCell<v8::Global<v8::PromiseResolver>>,
}
```

The getters return the current Promise (V8 private symbol
mirror). On `EnsureReadyPromiseRejected`: replace the
RefCell<Promise/Resolver> pair with a fresh pre-rejected pair,
update the priv sym `readyPromise` to the new promise. On
backpressure release: just call `resolver.resolve(scope, undef)`.

### II.11. `TransformStream` (§5.2)

**IDL:**
```webidl
[Exposed=*]
interface TransformStream {
  constructor(
    optional object transformer,
    optional QueuingStrategy writableStrategy = {},
    optional QueuingStrategy readableStrategy = {});
  readonly attribute ReadableStream readable;
  readonly attribute WritableStream writable;
};
```

**Internal slots:** `[[backpressure]]`, `[[backpressureChangePromise]]`,
`[[controller]]`, `[[Detached]]`, `[[readable]]`, `[[writable]]`.

`[[readable]]` and `[[writable]]` are V8 private symbols on the
TransformStream wrapper; the `readable` and `writable` getters
read these. Compression's design needs `readable_slot` /
`writable_slot` Rust accessors that bypass JS prototype lookup
(its MINOR-20).

Critic #45 fix: the design does NOT add `[NewObject]` to the
getters — same readable across reads is the correct WebIDL
shape. Wording polished.

`[[backpressureChangePromise]]` lifecycle (critic #52, missing
in v1): the spec's `TransformStreamSetBackpressure` first
**resolves** the existing change-promise then creates a fresh
pre-pending one. We model this with the same paired-storage
pattern as `[[readyPromise]]`:

```rust
pub struct TransformStreamState {
    backpressure: Cell<bool>,
    bp_change_promise: RefCell<v8::Global<v8::Promise>>,
    bp_change_resolver: RefCell<v8::Global<v8::PromiseResolver>>,
    // ...
}
```

`TransformStreamSetBackpressure(stream, b)`:
1. If `bp_change_resolver` exists, `resolve(undefined)` the
   current resolver (any pending writable side unblocks).
2. Allocate fresh PromiseResolver pair; replace.
3. Update `backpressure = b`.

### II.12. `TransformStreamDefaultController` (§5.3)

**IDL:**
```webidl
[Exposed=*]
interface TransformStreamDefaultController {
  readonly attribute unrestricted double? desiredSize;
  undefined enqueue(optional any chunk);
  undefined error(optional any reason);
  undefined terminate();
};
```

**Internal slots:** `[[cancelAlgorithm]]`, `[[finishPromise]]`,
`[[flushAlgorithm]]`, `[[stream]]`, `[[transformAlgorithm]]`.

### II.13. `ByteLengthQueuingStrategy` (§6.2)

```webidl
[Exposed=*]
interface ByteLengthQueuingStrategy {
  constructor(QueuingStrategyInit init);
  readonly attribute unrestricted double highWaterMark;
  readonly attribute Function size;
};

dictionary QueuingStrategyInit {
  required unrestricted double highWaterMark;
};
```

**IDL boundary (critic #33 fix):** the dictionary `init` is
required (no `?`); calling `new ByteLengthQueuingStrategy()` with
no argument throws TypeError at the WebIDL boundary because the
required argument is missing. Calling it with `{}` (missing
`highWaterMark`) ALSO throws TypeError because `highWaterMark` is
a required dictionary member. The macro's dictionary parser (XIV.9)
emits both checks.

**Internal slot:** `[[highWaterMark]]`.

The `size(chunk)` returns `chunk.byteLength`. WPT
`queuing-strategies-size-function-per-global.window.js` requires
that `size` is the same Function object across instances within
a realm. Our install codegen creates the function template once
per isolate.

### II.14. `CountQueuingStrategy` (§6.3)

Identical to ByteLength but `size()` returns `1`.


## III. Algorithms — exhaustive

Every named algorithm from the spec, with section reference and
the Rust function that implements it. The naming convention is
spec-name → snake_case (D-20).

### III.1. ReadableStream algorithms (§3.9.1)

| Spec algorithm | Rust function | Notes |
|----------------|---------------|-------|
| `InitializeReadableStream` | `readable_stream_initialize` | |
| `IsReadableStreamLocked` | `is_readable_stream_locked` | |
| `ReadableStreamCancel` | `readable_stream_cancel` | See §XII.1 |
| `ReadableStreamClose` | `readable_stream_close` | |
| `ReadableStreamError` | `readable_stream_error` | |
| `ReadableStreamFromIterable` | `readable_stream_from_iterable` | Critic #8 / D-14: tries `GetIterator(x, async)` first, falls back to `GetIterator(x, sync)` per ECMA-262, wraps sync as async. |
| `ReadableStreamAddReadIntoRequest` | `readable_stream_add_read_into_request` | |
| `ReadableStreamAddReadRequest` | `readable_stream_add_read_request` | |
| `ReadableStreamFulfillReadIntoRequest` | `readable_stream_fulfill_read_into_request` | |
| `ReadableStreamFulfillReadRequest` | `readable_stream_fulfill_read_request` | Calls one of chunkSteps/closeSteps/errorSteps exactly once. |
| `ReadableStreamGetNumReadIntoRequests` | `readable_stream_get_num_read_into_requests` | |
| `ReadableStreamGetNumReadRequests` | `readable_stream_get_num_read_requests` | |
| `ReadableStreamHasBYOBReader` | `readable_stream_has_byob_reader` | |
| `ReadableStreamHasDefaultReader` | `readable_stream_has_default_reader` | |

### III.2. ReadableStream readers (§3.9.2)

| Spec algorithm | Rust function |
|----------------|---------------|
| `AcquireReadableStreamDefaultReader` | `acquire_readable_stream_default_reader` |
| `AcquireReadableStreamBYOBReader` | `acquire_readable_stream_byob_reader` |
| `ReadableStreamDefaultReaderRead` | `readable_stream_default_reader_read` |
| `ReadableStreamDefaultReaderRelease` | `readable_stream_default_reader_release` |
| `ReadableStreamDefaultReaderErrorReadRequests` | `…error_read_requests` |
| `ReadableStreamBYOBReaderRead` | `readable_stream_byob_reader_read` |
| `ReadableStreamBYOBReaderRelease` | `readable_stream_byob_reader_release` |
| `ReadableStreamBYOBReaderErrorReadIntoRequests` | `…error_read_into_requests` |
| `ReadableStreamReaderGenericCancel` | `…generic_cancel` |
| `ReadableStreamReaderGenericInitialize` | `…generic_initialize` |
| `ReadableStreamReaderGenericRelease` | `…generic_release` |
| `SetUpReadableStreamDefaultReader` | `set_up_readable_stream_default_reader` |
| `SetUpReadableStreamBYOBReader` | `set_up_readable_stream_byob_reader` |

### III.3. ReadableStreamDefaultController algorithms (§3.10)

| Spec algorithm | Rust function |
|----------------|---------------|
| `SetUpReadableStreamDefaultController` | `set_up_readable_stream_default_controller` |
| `SetUpReadableStreamDefaultControllerFromUnderlyingSource` | `…_from_underlying_source` |
| `ReadableStreamDefaultControllerCallPullIfNeeded` | `readable_stream_default_controller_call_pull_if_needed` |
| `ReadableStreamDefaultControllerCanCloseOrEnqueue` | `…can_close_or_enqueue` |
| `ReadableStreamDefaultControllerClearAlgorithms` | `…clear_algorithms` |
| `ReadableStreamDefaultControllerClose` | `…close` |
| `ReadableStreamDefaultControllerEnqueue` | `…enqueue` |
| `ReadableStreamDefaultControllerError` | `…error` |
| `ReadableStreamDefaultControllerGetDesiredSize` | `…get_desired_size` |
| `ReadableStreamDefaultControllerHasBackpressure` | `…has_backpressure` |
| `ReadableStreamDefaultControllerShouldCallPull` | `…should_call_pull` |

### III.4. ReadableByteStreamController algorithms (§3.11)

| Spec algorithm | Rust function |
|----------------|---------------|
| `SetUpReadableByteStreamController` | `set_up_readable_byte_stream_controller` |
| `SetUpReadableByteStreamControllerFromUnderlyingSource` | `…_from_underlying_source` |
| `ReadableByteStreamControllerCallPullIfNeeded` | `…call_pull_if_needed` |
| `ReadableByteStreamControllerClearAlgorithms` | `…clear_algorithms` |
| `ReadableByteStreamControllerClearPendingPullIntos` | `…clear_pending_pull_intos` |
| `ReadableByteStreamControllerClose` | `…close` |
| `ReadableByteStreamControllerCommitPullIntoDescriptor` | `…commit_pull_into_descriptor` |
| `ReadableByteStreamControllerConvertPullIntoDescriptor` | `…convert_pull_into_descriptor` |
| `ReadableByteStreamControllerEnqueue` | `…enqueue` |
| `ReadableByteStreamControllerEnqueueChunkToQueue` | `…enqueue_chunk_to_queue` |
| `ReadableByteStreamControllerEnqueueClonedChunkToQueue` | `…enqueue_cloned_chunk_to_queue` (used by both default and byte tee branches) |
| `ReadableByteStreamControllerEnqueueDetachedPullIntoToQueue` | `…enqueue_detached_pull_into_to_queue` |
| `ReadableByteStreamControllerError` | `…error` |
| `ReadableByteStreamControllerFillHeadPullIntoDescriptor` | `…fill_head_pull_into_descriptor` |
| `ReadableByteStreamControllerFillPullIntoDescriptorFromQueue` | `…fill_pull_into_descriptor_from_queue` |
| `ReadableByteStreamControllerFillReadRequestFromQueue` | `…fill_read_request_from_queue` |
| `ReadableByteStreamControllerGetBYOBRequest` | `…get_byob_request` |
| `ReadableByteStreamControllerGetDesiredSize` | `…get_desired_size` |
| `ReadableByteStreamControllerHandleQueueDrain` | `…handle_queue_drain` |
| `ReadableByteStreamControllerInvalidateBYOBRequest` | `…invalidate_byob_request` |
| `ReadableByteStreamControllerProcessPullIntoDescriptorsUsingQueue` | `…process_pull_into_descriptors_using_queue` (returns `Vec<PullIntoDescriptor>` — list of filled-and-popped descriptors per critic #38) |
| `ReadableByteStreamControllerProcessReadRequestsUsingQueue` | `…process_read_requests_using_queue` |
| `ReadableByteStreamControllerPullInto` | `…pull_into` |
| `ReadableByteStreamControllerRespond` | `…respond` (D-15: gates on stream `[[state]]`, not controller `closeRequested`) |
| `ReadableByteStreamControllerRespondInClosedState` | `…respond_in_closed_state` |
| `ReadableByteStreamControllerRespondInReadableState` | `…respond_in_readable_state` |
| `ReadableByteStreamControllerRespondInternal` | `…respond_internal` |
| `ReadableByteStreamControllerRespondWithNewView` | `…respond_with_new_view` (validates `view.byteOffset` aligns with descriptor's element size — missing in v1) |
| `ReadableByteStreamControllerShiftPendingPullInto` | `…shift_pending_pull_into` |
| `ReadableByteStreamControllerShouldCallPull` | `…should_call_pull` |

**D-15 algorithm sketch (corrected from v1):** the Rust
function mirrors the ref impl exactly:

```rust
pub fn readable_byte_stream_controller_respond(
    scope: &mut v8::PinScope,
    controller: &Rc<RefCell<ByteControllerState>>,
    bytes_written: u64,
) -> Result<(), v8::Global<v8::Value>> {
    debug_assert!(!controller.borrow().pending_pull_intos.borrow().is_empty());
    let first_descriptor = controller.borrow().pending_pull_intos.borrow()[0].clone();
    let stream_obj = controller.borrow().stream_weak.upgrade(scope).unwrap();
    let stream_state = stream_state_get(stream_obj);

    if stream_state == StreamState::Closed {
        if bytes_written != 0 {
            return Err(make_type_error_global(scope,
                "bytesWritten must be 0 when calling respond() on a closed stream"));
        }
    } else {
        debug_assert!(stream_state == StreamState::Readable);
        if bytes_written == 0 {
            return Err(make_type_error_global(scope,
                "bytesWritten must be greater than 0 when calling respond() on a readable stream"));
        }
        if first_descriptor.bytes_filled + bytes_written as usize > first_descriptor.byte_length {
            return Err(make_range_error_global(scope, "bytesWritten out of range"));
        }
    }

    // TransferArrayBuffer per spec — detaches old, returns new ArrayBuffer
    // with same backing store. Ref impl 'firstDescriptor.buffer = TransferArrayBuffer(...)'.
    let transferred = transfer_array_buffer(scope, &first_descriptor.buffer);
    controller.borrow().pending_pull_intos.borrow_mut()[0].buffer = transferred;

    readable_byte_stream_controller_respond_internal(scope, controller, bytes_written)
}
```

### III.5. WritableStream algorithms (§4.5)

(Same table as v1 — the algorithm names map 1:1 to spec names.)

### III.6. WritableStream writers (§4.6)

(Same table as v1.)

### III.7. WritableStreamDefaultController algorithms (§4.7)

(Same table as v1.)

### III.8. TransformStream algorithms (§5.4)

| Spec algorithm | Rust function |
|----------------|---------------|
| `InitializeTransformStream` | `transform_stream_initialize` |
| `TransformStreamError` | `transform_stream_error` |
| `TransformStreamErrorWritableAndUnblockWrite` | `…error_writable_and_unblock_write` |
| `TransformStreamUnblockWrite` | `…unblock_write` |
| `TransformStreamSetBackpressure` | `transform_stream_set_backpressure` |
| `SetUpTransformStreamDefaultController` | `set_up_transform_stream_default_controller` |
| `SetUpTransformStreamDefaultControllerFromTransformer` | `…_from_transformer` |
| `TransformStreamDefaultControllerClearAlgorithms` | `…clear_algorithms` |
| `TransformStreamDefaultControllerEnqueue` | `…enqueue` |
| `TransformStreamDefaultControllerError` | `…error` |
| `TransformStreamDefaultControllerPerformTransform` | `…perform_transform` (awaits `transformPromise = transformAlgorithm(chunk)` per critic #5) |
| `TransformStreamDefaultControllerTerminate` | `…terminate` |
| `TransformStreamDefaultSinkWriteAlgorithm` | `transform_stream_default_sink_write_algorithm` |
| `TransformStreamDefaultSinkAbortAlgorithm` | `…abort_algorithm` |
| `TransformStreamDefaultSinkCloseAlgorithm` | `…close_algorithm` |
| `TransformStreamDefaultSourcePullAlgorithm` | `transform_stream_default_source_pull_algorithm` |
| `TransformStreamDefaultSourceCancelAlgorithm` | `…cancel_algorithm` |

### III.9. Pipe (§3.9.1.7) and tee (§3.5)

| Spec algorithm | Rust function |
|----------------|---------------|
| `ReadableStreamPipeTo` | `readable_stream_pipe_to` |
| `ReadableStreamTee` | `readable_stream_tee` (dispatcher) |
| `ReadableStreamDefaultTee` | `readable_stream_default_tee` |
| `ReadableByteStreamTee` | `readable_byte_stream_tee` |

Each is a multi-hundred-line algorithm; see §IX–§X.

### III.10. Queuing strategies (§6.4 / §7)

| Spec algorithm | Rust function |
|----------------|---------------|
| `ExtractHighWaterMark` | `extract_high_water_mark` |
| `ExtractSizeAlgorithm` | `extract_size_algorithm` |
| `ValidateAndNormalizeHighWaterMark` | `validate_and_normalize_high_water_mark` |

`ValidateAndNormalizeHighWaterMark` rules (critic #42 fix):
- If hwm is NaN, throw RangeError("highWaterMark must not be NaN").
- If hwm < 0, throw RangeError("highWaterMark must be non-negative").
- Infinity is allowed (spec permits; results in unbounded queue).

### III.11. Queue with sizes (§8.1)

| Spec algorithm | Rust function |
|----------------|---------------|
| `EnqueueValueWithSize` | `enqueue_value_with_size` |
| `DequeueValue` | `dequeue_value` |
| `PeekQueueValue` | `peek_queue_value` |
| `ResetQueue` | `reset_queue` |

All four maintain the byte-queue invariant from §II.6.

### III.12. Other supporting (§3.9.1, §8.3)

| Spec algorithm | Rust function |
|----------------|---------------|
| `CopyDataBlockBytes` | `copy_data_block_bytes` |
| `CanTransferArrayBuffer` | `can_transfer_array_buffer` |
| `TransferArrayBuffer` | `transfer_array_buffer` |
| `CloneAsUint8Array` | `clone_as_uint8_array` |
| `IsDetachedBuffer` | `is_detached_buffer` (calls `v8::ArrayBuffer::was_detached`) |
| `IsNonNegativeNumber` | `is_non_negative_number` |
| `GetIterator` / `IteratorNext` | Use existing v8 helpers |

**Total: ~80 algorithms.** Cross-referenced against the WHATWG
reference implementation — all covered.

## IV. Async iteration (§3.4.6)

Spec: `ReadableStream` includes
`async iterable<any>(optional ReadableStreamIteratorOptions options = {});`
which auto-defines `[Symbol.asyncIterator]` and `values(options)`.

```webidl
dictionary ReadableStreamIteratorOptions {
  boolean preventCancel = false;
};
```

The default iterator object's `next()` calls the underlying
reader's `read()`; `return(value)` calls `reader.cancel(value)`
unless `preventCancel`. The iterator's `@@toStringTag` is
`"ReadableStream Async Iterator"` (verified per WebIDL §3.7.10.4
async-iterable IDL definition; the `@@toStringTag` is set by
the WebIDL machinery).

**Implementation (v2 — fix to critic #4):**

```rust
#[v8_class]
pub struct ReadableStreamAsyncIterator {
    /// The reader, held by V8 priv sym `readerObj` on the iterator wrapper.
    /// Per ref impl ReadableStreamAsyncIterator-impl.js, the iterator
    /// stores _reader directly (not _stream). When the reader's
    /// generic-release happens, reader._stream becomes undefined; we
    /// detect "finished" via that condition.
    /// (No is_finished slot — fabricated in v1, removed in v2.)
    prevent_cancel: bool,
    // reader: v8::Global<v8::Object> lives in V8 priv sym `readerObj`
    // for D-2 single-source-rule compliance.
}

#[v8_to_string_tag = "ReadableStream Async Iterator"]
#[v8_inherit_intrinsic = "AsyncIteratorPrototype"]
impl ReadableStreamAsyncIterator {
    /// next() per spec §3.4.6:
    ///   1. Let reader be this.[[reader]].
    ///   2. If reader.[[stream]] is undefined → return resolved
    ///      promise of {value: undefined, done: true}.
    ///   3. Else create a read-request whose chunkSteps fulfills
    ///      with {value: chunk, done: false}; closeSteps releases
    ///      the reader and fulfills with {value: undefined, done: true};
    ///      errorSteps releases the reader and rejects.
    #[v8_async_method]
    async fn next(&self, scope: &mut v8::PinScope)
        -> Result<v8::Global<v8::Value>, v8::Global<v8::Value>>
    {
        let reader_obj = read_slot(scope, /* iter wrapper */, "readerObj");
        // Detect "finished" exactly per ref impl: reader._stream undefined.
        let stream_slot = read_slot(scope, reader_obj, "stream");
        if stream_slot.is_undefined() {
            return Ok(make_iter_result_global(scope, undefined, true));
        }
        // Issue a read; map outcomes to {value, done} or rejection.
        // ... (uses ReadableStreamDefaultReaderRead with a Native sink)
    }

    /// return(arg) per spec §3.4.6 step 5 / ref impl ReadableStreamAsyncIterator-impl.js:
    ///   IF preventCancel:
    ///     - release the reader (ReadableStreamReaderGenericRelease)
    ///     - return promiseResolvedWith(undefined) wrapped per WebIDL into
    ///       {value: arg, done: true} by the async-iter wrapper.
    ///   ELSE:
    ///     - cancelPromise = ReadableStreamReaderGenericCancel(reader, arg)
    ///     - release the reader
    ///     - return cancelPromise's chain via uponPromise(undefined,
    ///       ...) → WebIDL formats into {value: arg, done: true}.
    ///
    /// (v2 fix: v1 said "resolves with {value, done: true}" — this is
    /// what the WebIDL async-iter wrapper produces from a fulfillment
    /// promise of `undefined`; the iterator's own return(arg) returns
    /// the cancel-promise mapped through `then(undefined)`.)
    #[v8_name = "return"]
    async fn return_(&self, scope: &mut v8::PinScope, arg: v8::Local<v8::Value>)
        -> Result<v8::Global<v8::Value>, v8::Global<v8::Value>>
    {
        let reader_obj = read_slot(scope, /* iter wrapper */, "readerObj");
        if self.prevent_cancel {
            readable_stream_reader_generic_release(scope, reader_obj);
            // WebIDL async-iter wrapper takes `undefined` and produces
            // {value: arg, done: true}.
            return Ok(make_undefined_global(scope));
        }
        let cancel_promise = readable_stream_reader_generic_cancel(scope, reader_obj, arg);
        readable_stream_reader_generic_release(scope, reader_obj);
        // Map to fulfillment-of-undefined (WebIDL wrapper completes the rest).
        let mapped = transform_promise_with(scope, cancel_promise,
            |_| make_undefined_global(scope));
        // Convert to await semantics for #[v8_async_method].
        await_promise(scope, mapped).await
    }
}
```

Wiring on `ReadableStream`:

```rust
#[v8_class]
impl ReadableStream {
    #[v8_method]
    fn values(&self, scope: &mut v8::PinScope, options: v8::Local<v8::Value>)
        -> Result<v8::Local<v8::Value>, OpError>
    {
        let prevent_cancel = parse_iter_options(scope, options)?;
        let reader = acquire_readable_stream_default_reader_object(scope, /* this */)?;
        let iter = install_async_iterator(scope, reader, prevent_cancel);
        Ok(iter)
    }

    #[v8_async_iterator]
    fn async_iterator(&self, scope: &mut v8::PinScope)
        -> v8::Local<v8::Value>
    {
        // @@asyncIterator → values({}) per spec WebIDL aliasing.
        self.values_with_options(scope, default_iter_options())
    }
}
```

The `#[v8_async_iterator]` attribute is a new macro affordance
(§XIV.3) that installs the method on `%Symbol.asyncIterator%`.

## V. Internal slots — V8 layout

### V.1. Internal-field count per class

All classes have internal-field count 1 except `TransformStream`
which has 1 + a private-symbol named slot for the codec pointer
(satisfying compression dep #5; see §XVII).

| Class | Field 0 | V8 priv syms |
|-------|---------|--------------|
| ReadableStream | `Box<ReadableStreamState>` | `reader`, `storedError`, `controllerObj` |
| ReadableStreamDefaultReader | `Box<DefaultReaderState>` | `closedPromise`, `stream` |
| ReadableStreamBYOBReader | `Box<BYOBReaderState>` | `closedPromise`, `stream` |
| ReadableStreamDefaultController | `Box<DefaultControllerState>` | `streamObj`, `cancelAlgFn`, `pullAlgFn`, `sizeAlgFn` (only when an alg is JS-supplied) |
| ReadableByteStreamController | `Box<ByteControllerState>` | `streamObj`, `byobRequestObj`, `cancelAlgFn`, `pullAlgFn` |
| ReadableStreamBYOBRequest | `Box<BYOBRequestState>` (empty struct — both slots in priv syms) | `controllerObj`, `viewObj` |
| WritableStream | `Box<WritableStreamState>` | `writer`, `storedError`, `controllerObj` |
| WritableStreamDefaultController | `Box<WSControllerState>` | `streamObj`, `writeAlgFn`, `closeAlgFn`, `abortAlgFn`, `sizeAlgFn` |
| WritableStreamDefaultWriter | `Box<WSWriterState>` | `closedPromise`, `readyPromise`, `streamObj` |
| TransformStream | `Box<TSState>` | `readableObj`, `writableObj`, `controllerObj`, `transformerCodec` (compression dep #5 — codec pointer mirror) |
| TransformStreamDefaultController | `Box<TSControllerState>` | `streamObj`, `transformAlgFn`, `flushAlgFn`, `cancelAlgFn` |
| ByteLengthQueuingStrategy | `Box<ByteLengthQS>` | (none — `size` is a per-realm shared function via SharedState slot) |
| CountQueuingStrategy | `Box<CountQS>` | (same) |
| ReadableStreamAsyncIterator | `Box<AsyncIterState>` | `readerObj` |

### V.2. The single-source storage rule (D-2 audit)

Critic finding C-11 (D-2 inconsistent application) is fixed by
auditing each slot to live in EXACTLY one place:

| Storage rule | Examples |
|--------------|----------|
| **Pure Rust field** — slot is purely Rust-side data, never observed as a `v8::Local` or compared by JS identity | `[[state]]`, `[[disturbed]]`, `[[started]]`, `[[pulling]]`, `[[pullAgain]]`, `[[closeRequested]]`, `[[queue]]` (entries hold `v8::Global` per-chunk, but the queue itself is Rust), `[[queueTotalSize]]`, `[[strategyHWM]]`, `[[autoAllocateChunkSize]]`, `[[backpressure]]` |
| **Pure V8 private symbol** — slot stores a JS value whose JS-side identity matters, and Rust never holds it as anything other than a `v8::Global` | `[[storedError]]`, `[[reader]]`, `[[writer]]`, `[[closedPromise]]`, `[[readyPromise]]`, `[[backpressureChangePromise]]`, `[[byobRequest]]` (after fix to C-11), `[[view]]`, `[[stream]]` (the writer/reader's back-ref to the stream wrapper) |
| **Wrapper-in-priv-sym + state-in-Rc** — slot has both an observable JS-identity (the controller/writer wrapper) AND heavy Rust-side state. The PRIV SYM is the canonical identity store; the Rc<RefCell<State>> is reachable only via `controller_state(wrapper)` which retrieves it from the wrapper's internal field 0 | `[[controller]]` (wrapper in `controllerObj` priv sym; state in the Box stored in the wrapper's own field 0) |

There is no other category. Every slot in §XV maps to one of
these three rules.

### V.3. Private symbol lookup helpers (`slots.rs`)

```rust
pub fn private_sym(scope: &mut v8::PinScope, name: &'static str) -> v8::Local<v8::Private> {
    let state: SharedState = scope.get_slot::<SharedState>().unwrap().clone();
    let mut s = state.borrow_mut();
    if let Some(g) = s.private_syms.get(name) {
        return v8::Local::new(scope, g);
    }
    let key = v8::String::new(scope, name).unwrap();
    let priv_ = v8::Private::for_api(scope, Some(key));
    let global = v8::Global::new(scope, priv_);
    s.private_syms.insert(name, global.clone());
    v8::Local::new(scope, &global)
}

pub fn read_slot<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    obj: v8::Local<v8::Object>,
    name: &'static str,
) -> v8::Local<'s, v8::Value> {
    let priv_ = private_sym(scope, name);
    obj.get_private(scope, priv_).unwrap_or_else(|| v8::undefined(scope).into())
}

pub fn write_slot(scope: &mut v8::PinScope, obj: v8::Local<v8::Object>,
                  name: &'static str, value: v8::Local<v8::Value>) {
    let priv_ = private_sym(scope, name);
    obj.set_private(scope, priv_, value);
}

pub fn delete_slot(scope: &mut v8::PinScope, obj: v8::Local<v8::Object>, name: &'static str) {
    let priv_ = private_sym(scope, name);
    obj.delete_private(scope, priv_);
}
```

The 30-or-so private symbol names are interned at isolate
creation; runtime cost per slot access is one HashMap lookup
(amortised O(1)) plus one V8 private read.

### V.4. Slot accessor pattern + soundness (critic #16)

```rust
fn locked_getter_callback(scope: &mut v8::PinScope, args: v8::FunctionCallbackArguments, mut rv: v8::ReturnValue) {
    let __ext = args.this().get_internal_field(scope, 0).unwrap();
    // SAFETY: the External pointer is kept alive by the V8 wrapper
    // object. The wrapper holds a strong reference to External; External's
    // backing Box is dropped only by the V8 weak finalizer that runs
    // AFTER all callbacks for the object have completed. The macro's
    // existing pattern in `text_encoding.rs::TextEncoder` and
    // `headers.rs::Headers` enforces this — the finalizer runs at GC
    // boundaries via `v8::cppgc::SetFinalizer` (or equivalent
    // weak-callback). Single-thread per isolate, no concurrent finalizer
    // possible during JS callback execution.
    let state = unsafe { &*(External::cast(__ext).value() as *const ReadableStreamState) };
    let locked = !read_slot(scope, args.this(), "reader").is_undefined();
    rv.set(v8::Boolean::new(scope, locked).into());
}
```

The macro's invariant (verified in `v8_class.rs`): the `Box<Self>`
is stored as `External` in field 0; the External is set when the
wrapper is constructed and removed only by the weak finalizer. V8
weak finalizers are scheduled at GC boundaries that occur strictly
between JS callbacks (single-threaded per isolate). Therefore no
JS callback can observe a dangling pointer.


## VI. Backpressure / queuing

### VI.1. Queue entry

```rust
/// Default-stream queue entry (any-typed chunks).
pub struct ValueQueueEntry {
    value: v8::Global<v8::Value>,
    size: f64,
}

/// Byte-stream queue entry (already-detached ArrayBuffer + slice).
pub struct ByteQueueEntry {
    buffer: v8::Global<v8::ArrayBuffer>,
    byte_offset: usize,
    byte_length: usize,
}
```

### VI.2. desiredSize

`controller.desiredSize` per §3.10.7 / §3.11.4:
- if state == errored → null
- if state == closed → 0
- else → strategyHWM − queueTotalSize

The macro getter codegen for `Option<f64>` returns JS
`null` for `None` and a `v8::Number` for `Some(x)` (per
critic #46; verified against `gen_call_return` line ~590-600
in `lib.rs`, which handles `Result<Option<f64>, OpError>`'s
None arm via `rv.set(v8::null(scope).into())`).

### VI.3. Strategy size algorithm (critic #10, #27 fix)

```rust
pub enum SizeAlgorithm {
    /// CountQueuingStrategy (canonical, shared per realm)
    Count,
    /// ByteLengthQueuingStrategy (canonical, shared per realm)
    ByteLength,
    /// User-supplied JS function
    Js(v8::Global<v8::Function>),
    /// Default for default streams when user provided nothing
    DefaultCount,
}

impl SizeAlgorithm {
    pub fn invoke(
        &self,
        scope: &mut v8::PinScope,
        chunk: v8::Local<v8::Value>,
    ) -> Result<f64, v8::Global<v8::Value>> {
        match self {
            SizeAlgorithm::Count | SizeAlgorithm::DefaultCount => Ok(1.0),
            SizeAlgorithm::ByteLength => {
                if let Ok(view) = v8::Local::<v8::ArrayBufferView>::try_from(chunk) {
                    Ok(view.byte_length() as f64)
                } else if let Ok(buf) = v8::Local::<v8::ArrayBuffer>::try_from(chunk) {
                    Ok(buf.byte_length() as f64)
                } else {
                    Err(make_type_error_global(scope, "chunk has no byteLength"))
                }
            }
            SizeAlgorithm::Js(fn_g) => {
                let fn_l = v8::Local::new(scope, fn_g);
                // CRITIC #27 FIX: this = undefined per WebIDL `Function callback`,
                // NOT the global. Spec WebIDL §3.7 "Calling a callback function"
                // step "this is undefined".
                let this = v8::undefined(scope);
                let mut tc = v8::TryCatch::new(scope);
                let result = fn_l.call(&mut tc, this.into(), &[chunk]);
                if tc.has_caught() {
                    return Err(v8::Global::new(&mut tc, tc.exception().unwrap()));
                }
                let result_v = result.unwrap();

                // CRITIC #10 FIX: per WebIDL `unrestricted double`, conversion
                // throws TypeError on Symbol (and exotic types). Don't silently
                // coerce to NaN. We use V8's `to_number` which throws on Symbol
                // (per ECMA-262 ToNumber); on success, extract f64.
                let n_l = result_v.to_number(&mut tc);
                if tc.has_caught() {
                    return Err(v8::Global::new(&mut tc, tc.exception().unwrap()));
                }
                let n = n_l.unwrap().value();
                // n CAN still be NaN if e.g. the function returned NaN; that's
                // fine — IsNonNegativeNumber will throw RangeError downstream.
                Ok(n)
            }
        }
    }
}
```

WPT `readable-streams/bad-strategies.any.js` covers the Symbol/
unrestricted-double rejection and the NaN/negative path; both
behaviours match.

WPT `queuing-strategies-size-function-per-global.window.js`
demands `Object.is(s1.size, s2.size)` for same-realm strategies.
Our class-install codegen stashes the function template once per
isolate (in SharedState's `byte_length_size_fn` /
`count_size_fn` slots); the `size` getter returns the shared
`v8::Global<v8::Function>` (§XIV.7 sketch).

### VI.4. pullIfNeeded recursion guard (critic #18 wording fix)

Spec `[[pullAgain]]`: when `pull` is called while a previous
pull is still in flight, set `pullAgain = true`; when the
in-flight pull settles, if pullAgain, re-call pull and clear the
flag.

```rust
pub fn readable_stream_default_controller_call_pull_if_needed(
    scope: &mut v8::PinScope,
    controller: &Rc<RefCell<DefaultControllerState>>,
) {
    if !readable_stream_default_controller_should_call_pull(controller) {
        return;
    }
    if controller.borrow().pulling.get() {
        controller.borrow().pull_again.set(true);
        return;
    }
    debug_assert!(!controller.borrow().pull_again.get());
    controller.borrow().pulling.set(true);

    let pull_promise = controller.borrow().pull_algorithm.invoke(scope);

    // Promise reaction. Per critic #17/#18: this is uponPromise,
    // NOT plain then. uponPromise wraps `then(F, R)` then `then(undefined,
    // rethrowAssertionErrorRejection)` to forward unrecoverable errors
    // to the V8 default rejection handler. Match the ref impl.
    let controller_clone = controller.clone();
    upon_promise(scope, pull_promise,
        Some(Box::new(move |scope, _value| {
            controller_clone.borrow().pulling.set(false);
            if controller_clone.borrow().pull_again.get() {
                controller_clone.borrow().pull_again.set(false);
                readable_stream_default_controller_call_pull_if_needed(scope, &controller_clone);
            }
        })),
        Some(Box::new(move |scope, reason| {
            readable_stream_default_controller_error(scope, &controller_clone, reason);
        })),
    );
}
```

`upon_promise` semantics (critic #17 fix): the spec's
`uponPromise(p, onF, onR)` is `PerformPromiseThen(PerformPromiseThen(p, onF, onR), undefined, rethrowAssertionErrorRejection)`.
We implement it as a Rust helper in `promise_resolve.rs`:

```rust
pub fn upon_promise(
    scope: &mut v8::PinScope,
    promise: v8::Local<v8::Promise>,
    on_fulfilled: Option<Box<dyn FnOnce(&mut v8::PinScope, v8::Local<v8::Value>)>>,
    on_rejected: Option<Box<dyn FnOnce(&mut v8::PinScope, v8::Local<v8::Value>)>>,
) {
    let mid = chain_then(scope, promise, on_fulfilled, on_rejected);
    // Forward any unrecoverable error (e.g. assertion failures in onF/onR)
    // to the V8 default unhandled-rejection handler.
    chain_then(scope, mid, None, Some(Box::new(rethrow_assertion_error_rejection)));
}
```

`transform_promise_with` (used by tee/pipeTo for "fulfillment-
mapped" promises like `cancelPromise.then(undefined)`) is just
`chain_then(scope, p, fulfilled_or_None, rejected_or_None)`.

### VI.5. Backpressure observation

| Stream type | `[[backpressure]]` true means |
|-------------|-------------------------------|
| ReadableStream | (no spec-level backpressure flag — implicit via `desiredSize <= 0`) |
| WritableStream | `desiredSize <= 0` → writer.ready stays unresolved until desiredSize > 0 |
| TransformStream | `[[backpressure]]` flag controls when writable-side pull resumes; flips with `[[backpressureChangePromise]]` |

WritableStream's per-writer `ready` promise lives via the
paired-storage pattern (§II.10). Resolved from
`WritableStreamUpdateBackpressure(stream, false)`.

### VI.6. Synchronous multi-enqueue (critic backpressure note)

When a transformer enqueues N chunks synchronously inside a
single `transform()` call, backpressure should be evaluated
once after the call returns, not per-enqueue. The Rust
implementation: `TransformStreamDefaultControllerPerformTransform`
awaits the transformer's promise, then calls
`TransformStreamSetBackpressure` once based on the readable-side
controller's final desiredSize. This matches the spec's
`TransformStreamDefaultControllerEnqueue` flow where
`backpressure` is set at the END of enqueue, not inside
the user's synchronous loop.

## VII. Async ergonomics under compio

### VII.1. The compio model

- compio is single-threaded per isolate (per AGENTS.md "V8 per
  thread, one isolate per app").
- The runtime drives an event loop in
  `crates/runtime/src/runtime.rs` that pumps `state.spawned_ops`
  futures against compio's executor.
- Promise resolution from Rust is wired via
  `state.pending_resolvers: HashMap<u32, v8::Global<v8::PromiseResolver>>`
  + `OpResult` enum in `crates/runtime/src/state.rs:592`. We
  ADD an `OpResult::JsValue` variant (D-3) for stream chunks.

### VII.2. What "in parallel" means for streams (critic #31 fix)

The spec text "in parallel, perform the following steps"
(EcmaScript Internal Steps style) does NOT mean "spawn a compio
task". For streams, all algorithms run on the isolate thread.
"In parallel" in stream algorithms is a hint that execution
can interleave with other algorithms — but it's still
single-threaded.

The translation rules:

| Spec text | Rust mechanism | When |
|-----------|----------------|------|
| "Queue a microtask to perform steps" | `enqueue_microtask(scope, closure)` (one microtask) | tee chunk-delay-by-one-microtask; pipeTo's read→write yield boundary |
| "React to promise p with steps" / "Upon fulfillment / rejection" | `upon_promise(scope, p, onF, onR)` (uses `Promise.prototype.then`-equivalent V8 native) | Algorithm waits for an existing promise (e.g. user's `pull(controller)` returned promise) |
| "Perform the following in parallel" | (no compio task — it's the same isolate thread) — instead, schedule via a microtask to break the call stack | Rare; mostly applies to constructor-time start completion |
| "Wait for stream to settle" | `upon_promise(scope, closed_promise, …)` | pipeTo signal handlers, tee error forwarding |
| Compio task with bytes resolution | `state.spawned_ops.push(future)` posting `OpResult::JsValue` on completion | NetworkSource's `pull` reading from a TCP stream |

The fictitious-API claim of v1 D-12 (`scope.enqueue_microtask`)
is replaced with:

### VII.3. Microtask scheduling — the real V8 API

V8's `Isolate::enqueue_microtask(microtask: Local<Function>)`
(verified against `v8-147.1.0/src/isolate.rs:1676-1680`) takes
a Function, not a Rust closure. The closure-to-Function bridge:

```rust
/// Helper in `streams/promise_resolve.rs`. Wraps a Rust closure
/// in a one-shot V8 Function and enqueues it on the isolate's
/// default MicrotaskQueue.
pub fn enqueue_microtask(scope: &mut v8::PinScope, cb: Box<dyn FnOnce(&mut v8::PinScope)>) {
    // The closure is consumed once. Heap-allocate via Rc<RefCell<Option<…>>>
    // so the FunctionTemplate's C callback can take it.
    let cb_holder: Rc<RefCell<Option<Box<dyn FnOnce(&mut v8::PinScope)>>>> =
        Rc::new(RefCell::new(Some(cb)));

    fn callback(scope: &mut v8::PinScope, args: v8::FunctionCallbackArguments, _rv: v8::ReturnValue) {
        let ext_v = args.data();
        let ext = v8::Local::<v8::External>::try_from(ext_v).unwrap();
        // SAFETY: pointer originates from `Rc::into_raw` below; we reconstruct
        // the Rc to take ownership and then drop it after running the closure.
        let raw = ext.value() as *const RefCell<Option<Box<dyn FnOnce(&mut v8::PinScope)>>>;
        let rc = unsafe { Rc::from_raw(raw) };
        let cb_opt = rc.borrow_mut().take();
        if let Some(cb) = cb_opt {
            cb(scope);
        }
        // Rc dropped here; the FunctionTemplate is single-shot, so this is
        // the unique reconstitution. (No leak path.)
    }

    let func_tmpl = v8::FunctionTemplate::new(scope, callback);
    let raw_ptr = Rc::into_raw(cb_holder) as *mut std::ffi::c_void;
    let ext = v8::External::new(scope, raw_ptr);
    func_tmpl.set_data(ext.into());
    let func = func_tmpl.get_function(scope).unwrap();

    // The actual V8 API: Isolate::enqueue_microtask(Local<Function>).
    // PinScope has `as_isolate_mut` → &mut Isolate; we call it there.
    scope.as_isolate_mut().enqueue_microtask(func);
}
```

This is materially different from the existing
`init.rs::queue_microtask_callback` which uses
`Promise.resolve().then(callback)` (visible to userland Promise
prototype tampering — see Missing-concept #3, "Promise prototype
tampering"). Stream algorithms use the direct `enqueue_microtask`
path so they bypass userland-tampered `Promise.prototype.then`.

### VII.4. Promise chaining helper — `upon_promise` and `transform_promise_with`

```rust
/// Spec uponPromise(p, onF, onR):
///   PerformPromiseThen(PerformPromiseThen(p, onF, onR), undefined, rethrow…)
///
/// Critic #17 fix: this is NOT plain then(onF, onR). The double-then
/// pattern means rejections from onF are forwarded to rethrow…, which
/// surfaces them as unhandled-promise events (catching genuine bugs in
/// our Rust callback layer rather than silently swallowing).
pub fn upon_promise(
    scope: &mut v8::PinScope,
    promise: v8::Local<v8::Promise>,
    on_fulfilled: Option<Box<dyn FnOnce(&mut v8::PinScope, v8::Local<v8::Value>)>>,
    on_rejected: Option<Box<dyn FnOnce(&mut v8::PinScope, v8::Local<v8::Value>)>>,
);

/// Spec transformPromiseWith(p, onF, onR) — aliased uponPromise's first
/// then layer (the spec uses this for "react to p with a fulfillment
/// step that returns x" patterns; #17 critic noted v1's transform_promise
/// was named ambiguously). Renamed react_to_promise_with for clarity.
pub fn react_to_promise_with(
    scope: &mut v8::PinScope,
    promise: v8::Local<v8::Promise>,
    on_fulfilled: Option<Box<dyn FnOnce(&mut v8::PinScope, v8::Local<v8::Value>) -> v8::Local<v8::Value>>>,
    on_rejected: Option<Box<dyn FnOnce(&mut v8::PinScope, v8::Local<v8::Value>) -> v8::Local<v8::Value>>>,
) -> v8::Local<v8::Promise>;

/// Spec setPromiseIsHandledToTrue(p) — used by pipeTo's
/// `setPromiseIsHandledToTrue(pipeLoop())` call to swallow
/// the rejection because shutdown handlers handle errors via
/// the installed forward/backward error paths.
/// Implements: PerformPromiseThen(p, undefined, rethrowAssertionErrorRejection).
pub fn set_promise_is_handled_to_true(scope: &mut v8::PinScope, p: v8::Local<v8::Promise>);
```

These three helpers replace the misnamed `chain_promise` /
`transform_promise` from v1.

### VII.5. Resolver storage and resolution from compio (D-3 detailed)

Critic C-2 fix: the existing `OpResult::Completed.value: String`
field cannot carry V8 chunks. We add:

```rust
// In crates/runtime/src/state.rs:
pub enum OpResult {
    Completed { op_id: u32, value: String, request_id: Option<u64> },
    Failed { op_id: u32, error: String, request_id: Option<u64> },
    StreamChunk { stream_id: u32, data: Vec<u8>, done: bool },
    Cancelled,

    /// NEW (D-3): resolution of a stream-related promise that carries
    /// a V8 value. Chunks from native sources, resolved cancel/abort
    /// promises, etc.
    JsValue {
        op_id: u32,
        resolver: v8::Global<v8::PromiseResolver>,
        value: ResolveValue,
        request_id: Option<u64>,
    },
}

pub enum ResolveValue {
    /// Resolve with `undefined`.
    Undefined,
    /// Resolve with a Uint8Array constructed from these bytes.
    Bytes(Vec<u8>),
    /// Resolve with a previously-stored v8 value (e.g. a chunk that
    /// was passed in by JS).
    JsGlobal(v8::Global<v8::Value>),
    /// Resolve with a freshly-built read result `{value, done}`.
    ReadResult { value: ResolveValueInner, done: bool },
    /// Reject with this error.
    Reject(v8::Global<v8::Value>),
}

pub enum ResolveValueInner {
    Undefined,
    Bytes(Vec<u8>),
    JsGlobal(v8::Global<v8::Value>),
}
```

The runtime loop (`runtime.rs`) handles `OpResult::JsValue` by
entering V8, materialising the `value` enum into a
`v8::Local<v8::Value>`, and calling `resolver.resolve(scope, v)`
or `.reject(...)`. Since `v8::Global<v8::PromiseResolver>` is
held inside the `JsValue` variant, the resolver is naturally
GC-rooted until the variant is consumed by the runtime loop.

For each stream-method that returns a Promise (e.g.
`reader.read`, `writer.write`, `stream.cancel`, `pipeTo`), we
allocate a `v8::PromiseResolver`, return its promise, and the
resolver is either:
- **Stored in the relevant Rust state** (e.g. `[[readRequests]]`
  for reader.read) and resolved synchronously when the algorithm
  fires `chunkSteps`/`closeSteps`/`errorSteps`; OR
- **Routed through `OpResult::JsValue`** when the resolution is
  driven by a compio task (e.g. native fetch body bytes
  arriving from the network).

V8's microtask checkpoint runs between callback returns; the
runtime's main loop also performs `perform_microtask_checkpoint`
after each compio task posts an OpResult (existing behaviour
in `crates/runtime/src/runtime.rs`).

## VIII. Native traits (D-9)

Three Rust traits accepted by the native-construction APIs (§I.1).

### VIII.1. `NativeSource`

```rust
pub trait NativeSource: 'static {
    fn start(&mut self, _controller: &mut NativeReadableController)
        -> Result<(), v8::Global<v8::Value>> { Ok(()) }

    fn pull(&mut self, controller: &mut NativeReadableController)
        -> Pin<Box<dyn Future<Output = Result<(), v8::Global<v8::Value>>> + 'static>>;

    fn cancel(&mut self, _reason: Option<v8::Global<v8::Value>>)
        -> Pin<Box<dyn Future<Output = Result<(), v8::Global<v8::Value>>> + 'static>>
    { Box::pin(async { Ok(()) }) }

    fn is_byte_source(&self) -> bool { false }
    fn auto_allocate_chunk_size(&self) -> Option<u64> { None }
}
```

### VIII.2. `NativeSink`

```rust
pub trait NativeSink: 'static {
    fn start(&mut self, _controller: &mut NativeWritableController)
        -> Result<(), v8::Global<v8::Value>> { Ok(()) }

    fn write(&mut self, chunk: NativeChunk, controller: &mut NativeWritableController)
        -> Pin<Box<dyn Future<Output = Result<(), v8::Global<v8::Value>>> + 'static>>;

    fn close(&mut self)
        -> Pin<Box<dyn Future<Output = Result<(), v8::Global<v8::Value>>> + 'static>>
    { Box::pin(async { Ok(()) }) }

    fn abort(&mut self, _reason: Option<v8::Global<v8::Value>>)
        -> Pin<Box<dyn Future<Output = Result<(), v8::Global<v8::Value>>> + 'static>>
    { Box::pin(async { Ok(()) }) }
}

pub enum NativeChunk { Bytes(Vec<u8>), Value(v8::Global<v8::Value>) }
```

### VIII.3. `NativeTransformer` — async transform (critic C-5 fix)

```rust
pub trait NativeTransformer: 'static {
    /// CRITIC C-5 FIX: returns a future, matching the spec's
    /// `transformPromise = controller._transformAlgorithm(chunk)`
    /// semantic where the transform is awaited before the next write
    /// is admitted. CPU-bound transformers (compression) wrap the
    /// sync work in `Box::pin(async move { ... })` trivially; future
    /// async transformers (Web Crypto, network lookups) work natively.
    fn transform(
        &mut self,
        chunk: &[u8],
        controller: &mut NativeTransformController,
    ) -> Pin<Box<dyn Future<Output = Result<(), v8::Global<v8::Value>>> + 'static>>;

    fn flush(&mut self, controller: &mut NativeTransformController)
        -> Pin<Box<dyn Future<Output = Result<(), v8::Global<v8::Value>>> + 'static>>
    { Box::pin(async { Ok(()) }) }

    /// Called exactly once on stream cancel/error (compression dep #2).
    /// The NativeTransformController.cancelled flag (see below) ensures
    /// idempotency at the controller level; this trait method is invoked
    /// by Rust at most once.
    fn cancel(&mut self, _reason: v8::Local<v8::Value>) {}
}

pub struct NativeTransformController {
    /// Compression dep #2: single-cancel invariant. Set to true by
    /// `error()`, `terminate()`, or stream-side cancel propagation.
    /// Once true, `cancel()` on the trait fires exactly once and then
    /// further enqueue/error/terminate are no-ops.
    cancelled: Cell<bool>,
    // ... internal Rc back to TSControllerState
}

impl NativeTransformController {
    pub fn enqueue(&mut self, scope: &mut v8::PinScope, bytes: &[u8]) -> Result<(), JsError>;
    pub fn error(&mut self, scope: &mut v8::PinScope, exc: v8::Local<v8::Value>);
    pub fn terminate(&mut self);
    pub fn desired_size(&self) -> Option<f64>;
}
```

The compression design's "Dependencies on sibling projects"
section commits to five things; see §XVII for the reconciliation
table.

## IX. Pipe operations (§3.2.4 + §3.9.1)

### IX.1. `pipeTo(dest, options)` — public

The Rust implementation matches the WHATWG reference impl
(`reference-implementation/lib/abstract-ops/readable-streams.js`)
exactly. Critic #8 / C-8 fix: the handler installation order
matches the ref impl one-to-one.

```rust
pub fn readable_stream_pipe_to(
    scope: &mut v8::PinScope,
    source_obj: v8::Local<v8::Object>,
    dest_obj: v8::Local<v8::Object>,
    prevent_close: bool,
    prevent_abort: bool,
    prevent_cancel: bool,
    signal: Option<v8::Local<v8::Object>>,
) -> v8::Local<v8::Promise> {
    debug_assert!(!is_readable_stream_locked(source_obj));
    debug_assert!(!is_writable_stream_locked(dest_obj));

    let reader = acquire_readable_stream_default_reader(scope, source_obj);
    let writer = acquire_writable_stream_default_writer(scope, dest_obj);

    set_disturbed(source_obj, true);

    let pipe_state = Rc::new(RefCell::new(PipeState::new(
        source_obj, dest_obj, reader, writer,
        prevent_close, prevent_abort, prevent_cancel, signal,
    )));

    let resolver = v8::PromiseResolver::new(scope).unwrap();
    let promise_global = v8::Global::new(scope, resolver.get_promise(scope));
    pipe_state.borrow_mut().resolver = v8::Global::new(scope, resolver);

    // STEP 1: Signal abort handler.
    if let Some(sig) = signal {
        if abort_signal_aborted(scope, sig) {
            // EARLY RETURN: spec "If signal is aborted, perform abortAlgorithm and return".
            run_abort_algorithm(scope, &pipe_state);
            return v8::Local::new(scope, &promise_global);
        }
        register_abort_listener(scope, sig, pipe_state.clone());
    }

    // STEP 2: Forward error — source.errored → shutdown via WritableStreamAbort
    install_is_or_becomes_errored(scope, source_obj, reader_closed_promise(scope, &reader),
        Box::new({
            let pipe_state = pipe_state.clone();
            let dest_obj = dest_obj.clone();
            move |scope, error| {
                if !prevent_abort {
                    shutdown_with_action(scope, &pipe_state,
                        SwAction::WritableStreamAbort(dest_obj.clone(), error.clone()),
                        true, error);
                } else {
                    shutdown(scope, &pipe_state, true, error);
                }
            }
        }));

    // STEP 3: Backward error — dest.errored → shutdown via ReadableStreamCancel
    install_is_or_becomes_errored(scope, dest_obj, writer_closed_promise(scope, &writer),
        Box::new({
            let pipe_state = pipe_state.clone();
            let source_obj = source_obj.clone();
            move |scope, error| {
                if !prevent_cancel {
                    shutdown_with_action(scope, &pipe_state,
                        SwAction::ReadableStreamCancel(source_obj.clone(), error.clone()),
                        true, error);
                } else {
                    shutdown(scope, &pipe_state, true, error);
                }
            }
        }));

    // STEP 4: Forward close — source.closed → shutdown via WritableStreamDefaultWriterCloseWithErrorPropagation
    install_is_or_becomes_closed(scope, source_obj, reader_closed_promise(scope, &reader),
        Box::new({
            let pipe_state = pipe_state.clone();
            let writer = writer.clone();
            move |scope| {
                if !prevent_close {
                    shutdown_with_action(scope, &pipe_state,
                        SwAction::WritableStreamDefaultWriterCloseWithErrorPropagation(writer.clone()),
                        false, none_global());
                } else {
                    shutdown(scope, &pipe_state, false, none_global());
                }
            }
        }));

    // STEP 5: Backward close — synchronous initial check
    if writable_stream_close_queued_or_in_flight(dest_obj)
        || ws_state(dest_obj) == WSState::Closed
    {
        let dest_closed_err = make_type_error_global(scope,
            "the destination writable stream closed before all data could be piped to it");
        if !prevent_cancel {
            shutdown_with_action(scope, &pipe_state,
                SwAction::ReadableStreamCancel(source_obj.clone(), dest_closed_err.clone()),
                true, dest_closed_err);
        } else {
            shutdown(scope, &pipe_state, true, dest_closed_err);
        }
    }

    // STEP 6: Spawn the pipe loop. CRITIC C-8 FIX (rejection swallow):
    // setPromiseIsHandledToTrue(pipeLoop()) — pipeLoop returns a Promise
    // that may reject, but ALL rejection paths are handled by the four
    // shutdown handlers above; pipeLoop's own rejection is intentionally
    // swallowed via setPromiseIsHandledToTrue. This is spec-faithful per
    // ref impl line ~235.
    let pipe_loop_promise = spawn_pipe_loop(scope, pipe_state.clone());
    set_promise_is_handled_to_true(scope, pipe_loop_promise);

    v8::Local::new(scope, &promise_global)
}
```

`shutdown_with_action`, `shutdown`, `wait_for_writes_to_finish`,
and `pipe_step` mirror the spec's named functions one-to-one.

### IX.1.1. PipeState borrow protocol (critic #20)

`PipeState` is held in `Rc<RefCell<…>>` and accessed from inside
async callbacks. The borrow-acquire-release protocol:

1. Each top-level callback (handler closure, pipe-loop step
   resolution, abort listener) acquires `pipe_state.borrow_mut()`
   at entry.
2. Before calling out to any user code (writer.write,
   reader.read, signal.removeEventListener), drop the borrow
   (let it go out of scope at the end of the synchronous block).
3. After re-entering on the next microtask / async resolution,
   re-acquire `borrow_mut()`.
4. NEVER hold a borrow across a `.await` or callback registration
   that returns control to V8.

This pattern matches the ref impl's "let X = state.X; ... use X"
copy-out style. Violations would panic at runtime; tests
include a stress case with many overlapping shutdowns.

### IX.1.2. The 8-combination matrix (preventClose × preventAbort × preventCancel)

Each of the four shutdown paths above gates on one or two of
the prevent flags:

| Trigger | preventClose | preventAbort | preventCancel | Action |
|---------|:-:|:-:|:-:|--------|
| source.errored | — | T | — | shutdown(true, error) |
| source.errored | — | F | — | shutdownWithAction(WritableStreamAbort(dest, error)) |
| dest.errored | — | — | T | shutdown(true, error) |
| dest.errored | — | — | F | shutdownWithAction(ReadableStreamCancel(source, error)) |
| source.closed | T | — | — | shutdown() |
| source.closed | F | — | — | shutdownWithAction(WritableStreamDefaultWriterCloseWithErrorPropagation) |
| dest.closed (sync init) | — | — | T | shutdown(true, destClosedErr) |
| dest.closed (sync init) | — | — | F | shutdownWithAction(ReadableStreamCancel(source, destClosedErr)) |

The `[[finalReason]]` stash semantics (which error wins when
multiple triggers fire near-simultaneously) are encoded by
`shuttingDown`: the first shutdown sets `shuttingDown=true`;
subsequent triggers no-op. Order of resolution: first one
wins.

### IX.2. AbortSignal handling

The spec calls `signal.addEventListener('abort', abortAlgorithm)`
and `signal.removeEventListener('abort', abortAlgorithm)` at
shutdown. The runtime already provides AbortSignal natively
via existing `crates/runtime/src/embed/events.js`. Rust calls
through V8: get `signal.addEventListener` from the prototype,
call it with `("abort", listener_fn)`. The listener installs and
fires synchronously upon `signal.abort(reason)`.

### IX.3. `pipeThrough(transform, options)` — public

```rust
#[v8_method]
fn pipe_through(
    &self, scope: &mut v8::PinScope,
    transform: v8::Local<v8::Value>,
    options: v8::Local<v8::Value>,
) -> Result<v8::Local<v8::Value>, OpError> {
    let pair = read_pair(scope, transform)?;
    let pipe_options = parse_pipe_options(scope, options)?;

    // Critic missing-concept #12: cross-piping. If pair.writable is
    // already locked (e.g. a previous pipeTo claimed it), the lock-check
    // throws here. WPT crashtests/cross-piping.html.
    if is_readable_stream_locked(self_obj_v8) {
        return Err(OpError::type_error("ReadableStream.pipeThrough: source already locked"));
    }
    if is_writable_stream_locked(pair.writable) {
        return Err(OpError::type_error("ReadableStream.pipeThrough: writable already locked"));
    }

    let pipe_promise = readable_stream_pipe_to(
        scope, self_obj_v8, pair.writable,
        pipe_options.prevent_close, pipe_options.prevent_abort,
        pipe_options.prevent_cancel, pipe_options.signal,
    );
    set_promise_is_handled_to_true(scope, pipe_promise);

    Ok(pair.readable)
}
```

### IX.4. Internal pipe (D-10) — used by compression / fetch decode

```rust
/// Pipe a NativeSource directly into a NativeSink, with a
/// TransformStream optionally in the middle, without ever
/// constructing JS-visible ReadableStream/WritableStream
/// wrappers.
///
/// CRITIC C-12 INVARIANT (#[doc(hidden)]):
/// `pipe_native_internal` ONLY accepts Rust trait inputs that are
/// truly Rust-side (not wrappers around JS-bridged ReadableStream/
/// WritableStream objects). The lock-checks are bypassed because
/// nothing exists for JS code to lock — the streams are not
/// JS-visible. If a future caller needs to wrap a JS stream as a
/// NativeSource, they MUST use the public lock-acquiring path
/// (acquire a default reader on the JS stream, drive it through
/// the public APIs). Pull requests adding a JS-bridged Source/Sink
/// type without a lock-acquiring shim violate this invariant.
#[doc(hidden)]
pub fn pipe_native_internal<S: NativeSource + 'static, T: NativeSink + 'static>(
    scope: &mut v8::PinScope,
    source: S,
    sink: T,
    transformer: Option<Box<dyn NativeTransformer + 'static>>,
) -> Pin<Box<dyn Future<Output = Result<(), v8::Global<v8::Value>>>>>;
```

For compression's fetch hook: response body bytes flow
`Source(network) → CodecTransformer → BodySink`. The final
wrapping into a JS ReadableStream happens once via
`ReadableStream::from_native_source`.


## X. Tee operations (§3.5)

### X.1. Default tee (§3.5.2) — corrected terminology (D-11, critic C-6)

**Each branch is an independent ReadableStream with its OWN
ReadableStreamDefaultController and its OWN queue.** The two
branches share a SINGLE source-side reader via a SHARED
`pullAlgorithm`. There is no shared queue. The ref impl
algorithm (`reference-implementation/lib/abstract-ops/readable-streams.js`
ReadableStreamDefaultTee):

1. Acquire a default reader on the source.
2. Allocate two new ReadableStreams via SetUpReadableStream*FromUnderlyingSource
   with a shared-pull underlying source (`underlyingSource1`,
   `underlyingSource2`) — these constructors create the new
   streams' controllers and queues.
3. The shared `pullAlgorithm` issues ONE read on the source
   reader and, on chunk:
   - QUEUE A MICROTASK to delay the chunk by exactly one
     microtask (spec wants this delay so that source-side
     synchronous errors win the race over synchronously-available
     reads).
   - In that microtask, enqueue the chunk into BOTH branches'
     controllers (each into its own queue).
4. cancel1Algorithm/cancel2Algorithm: set canceled1/canceled2.
   When both are true, call `ReadableStreamCancel(source,
   compositeReason)` and resolve the shared cancelPromise.

```rust
pub fn readable_stream_default_tee(
    scope: &mut v8::PinScope,
    source_obj: v8::Local<v8::Object>,
    clone_for_branch2: bool,
) -> [v8::Local<v8::Object>; 2] {
    // Note (critic #25): public tee always passes clone_for_branch2 = false.
    // The clone path is reserved for future structuredClone-aware callers
    // and is currently never reachable from JS.
    let reader = acquire_readable_stream_default_reader(scope, source_obj);

    let tee_state = Rc::new(RefCell::new(TeeState {
        reading: false,
        read_again: false,
        canceled1: false, canceled2: false,
        reason1: None, reason2: None,
        cancel_promise_resolver: v8::Global::new(scope, v8::PromiseResolver::new(scope).unwrap()),
        branch1_obj: None,
        branch2_obj: None,
        clone_for_branch2,
    }));

    // Branch 1: independent ReadableStream with its OWN controller & queue.
    // Pull algorithm is the SHARED pullAlgo.
    let pull_algo = make_default_tee_pull_algo(tee_state.clone(), reader.clone());
    let cancel1 = make_default_tee_cancel_algo(tee_state.clone(), 1);
    let cancel2 = make_default_tee_cancel_algo(tee_state.clone(), 2);

    let branch1_obj = ReadableStream::from_native_source_with_algos(
        scope,
        /* startAlg */ noop_alg(),
        /* pullAlg */ pull_algo.clone(),
        /* cancelAlg */ cancel1,
        QueuingStrategy::default(),
    );
    let branch2_obj = ReadableStream::from_native_source_with_algos(
        scope,
        noop_alg(),
        pull_algo,                 // SAME pullAlg — drives both branches
        cancel2,
        QueuingStrategy::default(),
    );

    tee_state.borrow_mut().branch1_obj = Some(v8::Global::new(scope, branch1_obj));
    tee_state.borrow_mut().branch2_obj = Some(v8::Global::new(scope, branch2_obj));

    chain_reader_closed_rejection(scope, &reader, tee_state.clone());

    [branch1_obj, branch2_obj]
}
```

The `pullAlgo` issues one read via
`readable_stream_default_reader_read(scope, &reader,
read_request)`. The read-request's `chunkSteps` enqueues the
chunk into BOTH `branch1_controller` and `branch2_controller`,
delayed by one microtask via `enqueue_microtask`.

### X.2. Byte tee (§3.5.3)

Significantly more complex. Each branch is a byte stream. The
tee uses either a default reader OR a BYOB reader on the source
depending on which branch's controller has a pending BYOB
request.

Key complications:
- When neither branch has a BYOB request, use a default reader;
  on read, enqueue into both branches' controllers (cloning
  one chunk via `CloneAsUint8Array` for branch2 — used by
  `ReadableByteStreamControllerEnqueueClonedChunkToQueue`).
- When at least one branch has a BYOB request, switch to a BYOB
  reader; pull-into the requesting branch's view; on chunk,
  `respondWithNewView` on the requesting branch and enqueue
  a clone into the other.
- Reader switching: release current reader, acquire the other.
- Critic #16 byte-tee detail: `branch1._controller._pendingPullIntos.length > 0`
  during close-steps requires `respond(0)` on the current
  pending — the `respond_in_closed_state` algorithm handles this.

```rust
pub fn readable_byte_stream_tee(
    scope: &mut v8::PinScope,
    source_obj: v8::Local<v8::Object>,
) -> [v8::Local<v8::Object>; 2] {
    let mut reader = acquire_readable_stream_default_reader(scope, source_obj);
    let tee_state = Rc::new(RefCell::new(ByteTeeState::new()));

    let pull1 = make_byte_tee_pull_algo(tee_state.clone(), 1);
    let pull2 = make_byte_tee_pull_algo(tee_state.clone(), 2);
    let cancel1 = make_byte_tee_cancel_algo(tee_state.clone(), 1);
    let cancel2 = make_byte_tee_cancel_algo(tee_state.clone(), 2);

    let branch1_obj = ReadableStream::from_native_byte_source_with_algos(
        scope, noop_alg(), pull1, cancel1, /* hwm */ 0.0);
    let branch2_obj = ReadableStream::from_native_byte_source_with_algos(
        scope, noop_alg(), pull2, cancel2, /* hwm */ 0.0);

    tee_state.borrow_mut().branch1_obj = Some(v8::Global::new(scope, branch1_obj));
    tee_state.borrow_mut().branch2_obj = Some(v8::Global::new(scope, branch2_obj));

    forward_reader_error(scope, &reader, tee_state.clone());

    [branch1_obj, branch2_obj]
}
```

The reader-switching is in helpers `pull_with_default_reader` /
`pull_with_byob_reader`.

### X.3. Cancel propagation across branches

```rust
fn cancel_algorithm_for_branch(
    state: &Rc<RefCell<TeeState>>,
    source_obj: v8::Global<v8::Object>,
    branch_idx: u8,
    reason: v8::Local<v8::Value>,
    scope: &mut v8::PinScope,
) -> v8::Local<v8::Promise> {
    {
        let mut s = state.borrow_mut();
        if branch_idx == 1 {
            s.canceled1 = true;
            s.reason1 = Some(v8::Global::new(scope, reason));
        } else {
            s.canceled2 = true;
            s.reason2 = Some(v8::Global::new(scope, reason));
        }
        let both = s.canceled1 && s.canceled2;
        if both {
            let composite = v8::Array::new(scope, 2);
            composite.set_index(scope, 0, /* reason1 */).unwrap();
            composite.set_index(scope, 1, /* reason2 */).unwrap();
            let source_cancel_p = readable_stream_cancel(scope, &source_obj, composite.into());
            forward_to(scope, &s.cancel_promise_resolver, source_cancel_p);
        }
    }
    let resolver = v8::Local::new(scope, &state.borrow().cancel_promise_resolver);
    resolver.get_promise(scope)
}
```

### X.4. tee/disturb interaction (missing-concept #11)

The spec allows `tee()` on a disturbed but un-locked stream
(`tee()` acquires a reader; disturb is a one-way bit). A stream
whose reader was previously released remains valid for tee. The
v1 design didn't address this; v2 explicitly notes: tee's
`acquire_readable_stream_default_reader(source_obj)` succeeds on
any unlocked stream regardless of `[[disturbed]]`.

## XI. Locking semantics

### XI.1. ReadableStream lock state

`stream.locked` getter (§3.2.5.1): True iff `[[reader]]` slot
is not undefined.

`getReader()`: throws TypeError if `[[reader]]` set; else
constructs new reader, sets `[[reader]]` on the stream, sets
`[[stream]]` on the reader.

`reader.releaseLock()`:
1. If `[[stream]]` is undefined, return.
2. If `[[readRequests]]` non-empty, **error its read requests
   with TypeError** (`ReadableStreamDefaultReaderErrorReadRequests`).
3. Clear `[[reader]]` on the stream and `[[stream]]` on the
   reader.

### XI.2. WritableStream lock state

Mirrors ReadableStream's pattern with `[[writer]]`.

### XI.3. Lock acquisition for pipeTo / pipeThrough / tee

- `pipeTo`: acquires reader on source, writer on dest. Both
  released on shutdown (success or error) via `*ReleaseLock`
  in finalize().
- `pipeThrough`: acquires reader on `this`, writer on
  `transform.writable`. Returns `transform.readable` (which
  the caller can `getReader` on later). The locks on `this`
  and `transform.writable` are released when the background
  pipeTo completes.
- `tee`: acquires a default OR BYOB reader on source. Source
  remains locked for the lifetime of both branches. When both
  branches cancel, the source reader is released as part of
  `ReadableStreamCancel`'s flow.

### XI.4. Double-lock and pipe-lock errors

WPT `readable-streams/general.any.js` covers `getReader` after
existing reader → TypeError. `piping/crashtests/cross-piping.html`
covers the double-pipe case (one source, two pipeTo dests) —
the second `pipeTo` rejects because the source's lock check
fires. Missing-concept #12 fix: design now covers cross-piping
via the pipeThrough lock check above.

### XI.5. `releaseLock` during pipeTo's reader use (missing-concept #14)

User code cannot directly access pipeTo's internal reader (the
ref impl's `AcquireReadableStreamDefaultReader` produces a
non-JS-exposed reader). However, the pipeTo's reader can become
errored if the underlying source's controller calls `.error(e)`
— pipeTo observes this via the reader's `closed` promise
chained through `isOrBecomesErrored(source, reader._closedPromise, …)`.
The shutdown handler then runs `WritableStreamAbort` on the
dest (or `shutdown` if `preventAbort=true`). This is exactly
what step 2 of `readable_stream_pipe_to` installs.

## XII. Cancellation and error propagation

### XII.1. ReadableStream.cancel(reason)

```rust
pub fn readable_stream_cancel(
    scope: &mut v8::PinScope,
    stream: v8::Global<v8::Object>,
    reason: v8::Local<v8::Value>,
) -> v8::Local<v8::Promise> {
    set_disturbed(stream, true);

    let state = stream_state(stream);
    if state == StreamState::Closed {
        return resolved_undefined_promise(scope);
    }
    if state == StreamState::Errored {
        return rejected_with_stored_error_promise(scope, stream);
    }

    readable_stream_close(scope, stream);

    // BYOB readers: their pending readIntoRequests resolve with
    // {value: undefined, done: true}.
    if let Some(reader) = read_slot_object(scope, stream, "reader") {
        if is_byob_reader(reader) {
            let into_reqs = std::mem::take(&mut byob_reader_state(reader).read_into_requests);
            for req in into_reqs {
                req.close_steps(scope, undefined);
            }
        }
    }

    let source_cancel_promise = controller_cancel_steps(scope, stream, reason);

    // Critic #18 fix: the spec uses "react to sourceCancelPromise with a
    // fulfillment step that returns undefined." That's react_to_promise_with
    // (renamed in v2 from transform_promise) with onFulfilled = (_) => undefined.
    react_to_promise_with(
        scope,
        source_cancel_promise,
        Some(Box::new(|scope, _value| v8::undefined(scope).into())),
        None,
    )
}
```

### XII.2. WritableStream.abort(reason)

(Same shape as v1 — calls into `WritableStreamFinishErroring`,
abort algorithm chain.)

### XII.3. pipeTo cancellation

Per §3.9.1.7 step 14, on signal abort the abort-algorithm
runs `WritableStreamAbort(dest, reason)` and
`ReadableStreamCancel(source, reason)` in `Promise.all`-style
waiting (the spec's `waitForAllPromise(actions.map(action =>
action()))`). preventAbort/preventCancel filter the actions list.

### XII.4. TransformStream error propagation

Per §5.4.1.2 (`TransformStreamError`):
- `ReadableStreamDefaultControllerError(stream._readable._controller, e)`
- `TransformStreamErrorWritableAndUnblockWrite(stream, e)`

Both sides see the same error. Per `TransformStreamSetBackpressure`,
backpressure changes resolve the change-promise so any pending
write on the writable side unblocks.

### XII.5. Second-cancel-while-first-pending

Critic note: if `cancel(reason1)` is in flight and JS calls
`cancel(reason2)`, the second resolves to the same promise as
the first (the spec `ReadableStreamCancel` is idempotent on
already-closed/errored streams; once closed, it returns
`promiseResolvedWith(undefined)`). For a stream still
transitioning, the first cancel completes; the second sees
`state === closed` and returns immediately.

## XIII. V8 internal-fields layout

(Already covered in §V.1.)

Cross-reference vs. compression's design — see §XVII for the
explicit reconciliation table. **TransformStream uses 1
internal field PLUS the `transformerCodec` V8 private symbol
on the wrapper** to satisfy compression dep #5's "codec pointer
near state" intent. See §XVII row #5.

## XIV. Macro extensions required

The existing `#[v8_class]` macro
(`crates/runtime-macros/src/v8_class.rs`) supports:
- Method/getter/setter/constructor classification
- Internal-field 0 with weak-finalizer reclamation
- `Vec<u8>` from ArrayBufferView
- ByteString newtype, `Vec<Vec<u8>>` returns, `Option<Vec<u8>>`
  returns (recently added by headers/codec landings)
- `#[v8_name]`, `#[v8_to_string_tag]`, `#[v8_inherit_intrinsic]`,
  `#[reject_shared]` (verified against current
  `crates/runtime-macros/src/v8_class.rs:111-244`)
- Lifetime-tied `Local<'s, _>` returns (via the headers/codec
  iterator landing)

This design forces these additional extensions:

### XIV.1. `#[v8_async_method]` — async method returning Promise

Currently the macro only supports async free functions
(`#[zeroship_op(async)]`). Streams need async methods on
classes (e.g. `reader.read()`, `writer.write(chunk)`,
`stream.cancel(reason)`).

**Borrow-across-await discipline (critic #23 fix):** the macro
generates a sync wrapper that:
1. Allocates a `v8::PromiseResolver`, returns its promise.
2. Reads the `Box<Self>` pointer from V8 internal field 0
   (this is a raw `*const Self` derived from `External::value()`).
3. Spawns the async body onto `state.spawned_ops`. The body
   does NOT capture `&mut self` directly. Instead, it captures
   the V8 wrapper's `v8::Global<v8::Object>` and a pointer
   recipe; on each `.await` resumption, the body re-acquires
   `&mut Self` from `wrapper.get_internal_field(scope, 0)`.
   This avoids holding a Rust reference across the `.await`
   point (which would either require pinning — incompatible
   with the V8-allocated Box — or carry a stale pointer if the
   isolate's internal-field layout changed; in practice it
   doesn't, but we re-acquire for safety).
4. On future completion, the runtime loop materialises the
   resolution via `OpResult::JsValue` (D-3) and calls
   `resolver.resolve(scope, v)` or `.reject`.

```rust
#[v8_class]
impl ReadableStreamDefaultReader {
    #[v8_async_method]
    async fn read(
        &mut self,                      // re-acquired per poll, see above
        scope: &mut v8::PinScope,
    ) -> Result<v8::Global<v8::Value>, v8::Global<v8::Value>> {
        // body that may .await; on each resumption, scope and self are
        // freshly bound to the live PinScope and the still-valid Box.
    }
}
```

**Estimate (recalibrated):** ~250 LOC macro work, ~5h.

### XIV.2. Lifetime-tied `Local<'s, _>` return on getters/methods

Already shipped (headers/codec). Streams reuses verbatim.
~30 LOC, ~0h (re-verification only).

### XIV.3. `#[v8_async_iterator]` — install on `@@asyncIterator`

```rust
#[v8_class]
impl ReadableStream {
    #[v8_async_iterator]
    fn async_iterator(&self, scope: &mut v8::PinScope) -> v8::Local<v8::Value> {
        self.values_with_options(scope, default_iter_options())
    }
}
```

Macro work: install the method on the well-known
`%Symbol.asyncIterator%` instead of a string-keyed property.
~30 LOC, ~1.5h.

### XIV.4. `#[v8_throws]` — methods that throw without `Result`

**Decision: deferred.** `Result<(), OpError>` works fine. Critic
process flaw #5 noted that 33 controller methods would each
need `Result<...>` — this is unavoidable given the spec's
algorithm shape (any of them can throw on closed/errored
state). The cost is uniform `Result<(), OpError>` returns,
which the macro already handles via `gen_call_return`. Not a
blocker; revisit if maintainers report ergonomic friction.

### XIV.5. `Rc<RefCell<…>>` field access — implementation pattern, not macro work

The macro extracts `Box<Self>` from internal field 0; class
methods then do `let stream = self.stream_weak.upgrade(scope).unwrap();`.

Critic process flaw #6 fix: the `stream_weak.upgrade(scope).unwrap()`
pattern repeats across ~30 controller methods. **Mitigation:** a
helper macro `with_stream!(self, scope, stream => { ... })` or
a method `self.with_stream(scope, |stream| ...)` that centralises
the upgrade-or-error pattern. ~50 LOC of helpers,  ~1h.

### XIV.6. Promise return from sync method

Critic finding #1 (unverifiable) — verified: `gen_call_return`
in `crates/runtime-macros/src/lib.rs:524-650` (NOT lines
530-533 as v1 cited; the actual `Local` arm is at line 644).
Promise is a subtype of `v8::Local<v8::Value>`; the existing
`Local` arm handles `v8::Local<v8::Promise>` returns. No
extension needed.

### XIV.7. Strategy `size` shared-function getter

```rust
impl ByteLengthQueuingStrategy {
    pub fn install(scope: &mut v8::PinScope) -> v8::Local<v8::FunctionTemplate> {
        // ... existing macro install ...

        // Build the shared `size` Function once per realm.
        let size_fn = build_byte_length_size_function(scope);
        scope.get_slot::<SharedState>().unwrap().borrow_mut()
            .byte_length_size_fn = Some(v8::Global::new(scope, size_fn));
    }

    #[v8_getter]
    fn size(&self, scope: &mut v8::PinScope) -> v8::Local<v8::Value> {
        let g = scope.get_slot::<SharedState>().unwrap().borrow().byte_length_size_fn.clone().unwrap();
        v8::Local::new(scope, &g).into()
    }
}

fn build_byte_length_size_function(scope: &mut v8::PinScope) -> v8::Local<v8::Function> {
    // Standard pattern: FunctionTemplate::new(scope, callback) → .get_function(scope).
    let tmpl = v8::FunctionTemplate::new(scope, byte_length_size_callback);
    tmpl.get_function(scope).unwrap()
}

fn byte_length_size_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    // Spec ByteLengthQueuingStrategy size: chunk.byteLength
    let chunk = args.get(0);
    if let Ok(view) = v8::Local::<v8::ArrayBufferView>::try_from(chunk) {
        rv.set(v8::Number::new(scope, view.byte_length() as f64).into());
    } else if let Ok(buf) = v8::Local::<v8::ArrayBuffer>::try_from(chunk) {
        rv.set(v8::Number::new(scope, buf.byte_length() as f64).into());
    } else {
        // Spec: byteLength access on non-buffer object — JS engine throws TypeError.
        // We propagate via TryCatch boundary in the caller.
        let exc = make_type_error_local(scope, "chunk.byteLength: not a buffer-like");
        scope.throw_exception(exc);
    }
}
```

~30 LOC per class, no macro work needed. Estimate: 0.5h.

### XIV.8. `[EnforceRange] u64` extraction (critic #24 fix)

Per WebIDL §3.2.10 `[EnforceRange] unsigned long long`:
- Convert to Number (ToNumber).
- If NaN/+Inf/-Inf, throw TypeError.
- If value > 2^64-1 or < 0, throw TypeError.
- For values above 2^53 (JS Number precision boundary), the
  spec allows ANY value representable; we reject anything that
  doesn't round-trip through Number → BigInt. WPT
  `readable-byte-streams/read-min.any.js` tests `Number.MAX_SAFE_INTEGER + 1`
  (2^53 + 1) which JS Number cannot represent precisely.
  Spec answer: `[EnforceRange]` rejects with TypeError.

Implementation: ~50 LOC macro work, ~2.5h. Adds a new arm to
`gen_extract` mirroring the `u32` arm with extended bounds.

### XIV.9. Dictionary argument extraction

Pipe options, queuing strategy init, BYOBReader options,
iterator options — all WebIDL dictionaries. Hand-rolled helper
functions in `streams/dictionaries.rs`:

```rust
pub fn parse_pipe_options(
    scope: &mut v8::PinScope,
    options: v8::Local<v8::Value>,
) -> Result<PipeOptions, OpError> {
    // If options is undefined, return defaults.
    // Else if not an object, throw TypeError.
    // Else extract preventClose/preventAbort/preventCancel/signal.
    // ...
}

pub fn parse_queuing_strategy_init(
    scope: &mut v8::PinScope,
    init: v8::Local<v8::Value>,
) -> Result<QueuingStrategyInit, OpError> {
    // Critic #33: `init` is required (no `?`); if undefined, throw
    // TypeError. If `highWaterMark` not present, throw TypeError.
}
```

~5 dictionaries × ~40 LOC each = ~200 LOC, no macro change.
Estimate: 1.5h.

### XIV.10. Macro extension summary (recalibrated)

| # | Extension | LOC | Hours |
|---|-----------|-----|-------|
| XIV.1 | `#[v8_async_method]` | ~250 | 5h |
| XIV.2 | Lifetime-tied Local return (already shipped) | n/a | 0h |
| XIV.3 | `#[v8_async_iterator]` | ~30 | 1.5h |
| XIV.5 | `with_stream!` borrow helper | ~50 | 1h |
| XIV.7 | Shared-function strategy getter | ~30 × 2 | 1h |
| XIV.8 | `[EnforceRange] u64` extraction | ~50 | 2.5h |
| XIV.9 | Dictionary parser helpers | ~200 | 1.5h |
| **Total** | | **~640** | **~12.5h** |

XIV.4 / XIV.6 are no-cost (deferred / already supported).

## XV. Storage-strategy decisions (per slot, D-2 audited)

Per-class summary applying the single-source rule from §V.2.
Critic C-11 fix: every slot lives in EXACTLY one place; there
are no "Rust + priv sym mirror" rows.

### XV.1. ReadableStream

| Slot | Where | Rule |
|------|-------|------|
| [[state]] | Rust `Cell<StreamState>` | pure data |
| [[storedError]] | V8 priv sym `storedError` | JS identity |
| [[disturbed]] | Rust `Cell<bool>` | pure data |
| [[reader]] | V8 priv sym `reader` | wrapper identity |
| [[controller]] | V8 priv sym `controllerObj` (canonical) + Rust state retrievable via `controller_state(controllerObj)` | wrapper-in-priv-sym + state-in-Rc |
| [[Detached]] | (deferred D-7) | n/a |

### XV.2. ReadableStreamDefaultController

| Slot | Where | Rule |
|------|-------|------|
| [[stream]] | V8 priv sym `streamObj` | wrapper identity (back-ref) |
| [[queue]], [[queueTotalSize]], [[strategyHWM]] | Rust | pure data |
| [[strategySizeAlgorithm]] | Rust `SizeAlgorithm` enum | shape change (canonical Count/ByteLength variants live here; the Js variant holds a `v8::Global<Function>`) |
| [[pullAlgorithm]], [[cancelAlgorithm]] | Rust `AlgorithmFn` enum | shape change |
| [[started]], [[pulling]], [[pullAgain]], [[closeRequested]] | Rust `Cell<bool>` | pure data |

### XV.3. ReadableByteStreamController

| Slot | Where | Rule |
|------|-------|------|
| [[stream]] | V8 priv sym `streamObj` | wrapper identity |
| [[queue]], [[queueTotalSize]], [[strategyHWM]] | Rust | pure data (queue holds `v8::Global<ArrayBuffer>` per entry) |
| [[pendingPullIntos]] | Rust `VecDeque<PullIntoDescriptor>` | pure data + `v8::Global<ArrayBuffer>` per entry |
| [[byobRequest]] | V8 priv sym `byobRequestObj` ONLY (fix to critic C-11/#34) | wrapper identity |
| [[autoAllocateChunkSize]] | Rust `Option<u64>` | pure data |
| [[pullAlgorithm]], [[cancelAlgorithm]] | Rust `AlgorithmFn` enum | shape change |

### XV.4. WritableStream

| Slot | Where | Rule |
|------|-------|------|
| [[state]] | Rust `Cell<WSState>` | pure data |
| [[storedError]] | V8 priv sym `storedError` | JS identity |
| [[backpressure]] | Rust `Cell<bool>` | pure data |
| [[writer]] | V8 priv sym `writer` | wrapper identity |
| [[controller]] | V8 priv sym `controllerObj` + Rc state retrievable via `controller_state` | hybrid (per D-2) |
| [[closeRequest]] | Rust paired storage (Promise + Resolver in `RefCell<Option<…>>`) | promise lifecycle (replaceable) |
| [[inFlightWriteRequest]], [[inFlightCloseRequest]] | Rust `RefCell<Option<v8::Global<Promise>>>` (Promises, not Resolvers — fix to critic #44) | spec specifies Promise type |
| [[pendingAbortRequest]] | Rust `Option<PendingAbortRequest>` struct | pure data + `v8::Global<Value>` reason |
| [[writeRequests]] | Rust `VecDeque<v8::Global<Promise>>` (Promises — fix to C-13/#26) + parallel `write_request_resolvers` | spec specifies Promise type |

### XV.5. TransformStream

| Slot | Where | Rule |
|------|-------|------|
| [[readable]], [[writable]] | V8 priv syms `readableObj`, `writableObj` | wrapper identity |
| [[controller]] | V8 priv sym `controllerObj` + Rc state | hybrid |
| [[backpressure]] | Rust `Cell<bool>` | pure data |
| [[backpressureChangePromise]] | Rust paired storage `(Promise, Resolver)` (fix to critic #52 — replaceable) | promise lifecycle |
| (compression dep #5) `transformerCodec` | V8 priv sym holding an `External` to the codec pointer | wrapper-near-state mirror for compression's RAII Drop ordering |

