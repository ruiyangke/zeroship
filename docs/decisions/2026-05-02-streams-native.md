# Native WHATWG Streams shipped

**Status:** Shipped 2026-05-02
**Long-form design:** [`docs/archive/streams-native.md`](../archive/streams-native.md)
**Implementation:** [`crates/runtime/src/web/streams/`](../../crates/runtime/src/web/streams/) (~25 files: readable, writable, transform, controllers, readers, byte/BYOB, queues, strategies, tee, pipe, compression, async-iter)

## Context

The runtime carried a value-preserving JS skeleton at
`embed/streams.js` plus a vendored `web-streams-polyfill v3.3.3`
(~3700 LOC) at `embed/streams-polyfill.js`. The polyfill diverged
from the WHATWG Streams Standard in subtle places (close vs.
close-requested gating, microtask scheduling, tee semantics,
SizeAlgorithm thisArg, async iter finished-tracking). It also
forced fetch and compression to JS-hop through the polyfill on
every chunk.

## Decision

- Cover the full WHATWG spec surface as native `#[v8_class]` types: every IDL interface, every named algorithm.
- Pure-native on V8 + Rust + compio with **zero tokio**.
- Replace `embed/streams.js` and `embed/streams-polyfill.js` entirely (no fallback path).
- Extend `OpResult` with a `JsValue` variant (`v8::Global<v8::Value>`) so V8 chunks can flow through the existing op-result machinery.
- Use the real `Isolate::enqueue_microtask` API for spec-faithful microtask scheduling.
- Tee, async iterator, `SizeAlgorithm`, `[[writeRequests]]`, `NativeTransformer::transform` all match reference-impl semantics (round-2 fixed the bugs in v1).
- Cutover via three feature-flag landings: native behind `ZEROSHIP_NATIVE_STREAMS` → flip default → delete polyfill.

## Consequences

- Unblocks native fetch body bridge, native CompressionStream, native pipeThrough on internal encoder hand-off.
- WPT regression for the streams suite became runnable (`66b3bb9` showed 90.4% pass on encoding/streams).
- Post-ship migrations: `ReadableStreamDefaultReader`, `ReadableStreamBYOBReader`, `WritableStreamDefaultWriter`, and `TransformStream` migrated to `#[v8_constructor(post_init)]` once MAC-02 landed.

## See also

- Implementing commits: `1cb5af6` native ReadableStream + DefaultController + DefaultReader; `d4b22e6` streams cutover landing 1 (feature flag); `aabd74e` streams cutover landing 2b (native default); `4133eb6` streams cutover landing 3b (delete polyfill); `12128cd` Merge feature/streams-native.
- Related ADRs: [fetch-native](./2026-05-02-fetch-native.md), [compression-streams-native](./2026-05-01-compression-streams-native.md), [macro-constructor-post-init](./2026-05-04-macro-constructor-post-init.md).
