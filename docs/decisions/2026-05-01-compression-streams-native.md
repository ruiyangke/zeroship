# Native `CompressionStream` / `DecompressionStream` shipped

**Status:** Shipped 2026-05-01 (codec layer); CompressionStream/DecompressionStream classes landed 2026-05-05 in `ed3125e`
**Long-form design:** [`docs/archive/compression-streams-native.md`](../archive/compression-streams-native.md)
**Implementation:** [`crates/runtime/src/web/streams/compression.rs`](../../crates/runtime/src/web/streams/compression.rs)

## Context

The previous implementation wrapped a polyfill `TransformStream`
inside a `#[v8_class]`. Critic round-1 surfaced two architectural
blockers: V8 finalization ordering between two Globals, and
`pipeThrough` on a body the fetch body-bridge had already locked.
Both came from the codec living in Rust while its host
TransformStream lived in JS. Internal `Content-Encoding`
decompression in fetch was also broken — encoded bytes passed
through unchanged, so `.text()` / `.json()` returned garbage on
real-world API responses.

## Decision

- Pure-native pivot (round-2): `CompressionStream`/`DecompressionStream` are Rust `#[v8_class]` types that construct a native TransformStream with a codec-backed transformer slot — no JS hop in `transform`/`flush`/`cancel`.
- Single Rust-side `Codec` trait powers both the public API and fetch's internal `Content-Encoding` decompression.
- Internal decompression in `fetch` is wired during native response-body construction via direct codec hand-off — it does not call public `pipeThrough` (avoiding the body-bridge lock).
- Backends: `flate2` (gzip/deflate/deflate-raw) + `brotli`.
- Depends on streams-native landing first (the native TransformStream).

## Consequences

- Two architectural blockers disappear under the native-host architecture.
- Fetch responses with `Content-Encoding` now decode correctly without user code intervention.
- The codec layer is the bulk of the work; shipping both surfaces together was materially cheaper than shipping them separately.

## See also

- Implementing commits: `d361cc2` runtime: add flate2 + brotli for native compression streams; `40fe581` runtime: codec layer for CompressionStream / DecompressionStream; `ed3125e` runtime: EventSource + CompressionStream/DecompressionStream.
- Related ADRs: [streams-native](./2026-05-02-streams-native.md), [fetch-native](./2026-05-02-fetch-native.md).
