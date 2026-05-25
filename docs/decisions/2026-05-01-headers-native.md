# Native WHATWG `Headers` shipped

**Status:** Shipped 2026-05-01
**Long-form design:** [`docs/archive/headers-native.md`](../archive/headers-native.md)
**Implementation:** [`crates/runtime/src/web/headers.rs`](../../crates/runtime/src/web/headers.rs) (~930 LOC)

## Context

The JS-shimmed `Headers` (part of `embed/fetch.js`) diverged from the
WHATWG Fetch §2.2 spec in ways that broke WPT and leaked through to
creator apps: snapshot-iteration instead of live, missing
normalize-then-validate ordering, no `delete`/`has`/`get` name
validation, and ByteString boundary errors went through as plain
`Error` instead of `TypeError`. Native `Request`/`Response` need a
`[SameObject]` Headers backing — JS shim can't supply that.

## Decision

- Replace the JS shim with a native `#[v8_class] Headers` over `Vec<u8>` storage (lenient byte values per Fetch §2.2).
- Iteration is **live**: re-run sort-and-combine on every `next()` (WebIDL §3.7.10.2).
- Normalize value → validate name → guard step → list mutate, in that order, for `append`/`set`.
- Name validation required in `delete`/`has`/`get` (throws `TypeError` on invalid name).
- Storage shape forward-compatible with native Request/Response `[SameObject]` aliasing — no JS polyfill fallback path.
- Polyfill removal cadence: feature-flag native → cutover → remove polyfill (three separate landings).

## Consequences

- Unblocked native `Request`/`Response` (`[SameObject]` headers cache).
- WPT `fetch/api/headers` runnable for the first time.
- Post-ship perf work landed: `HeadersIterator` migrated to `#[v8_iterable]` (`5677051`) and `Headers.has` is now a fastcall method (`d2fea29`). These were possible because the surface is native.

## See also

- Implementing commits: `f90e264` runtime: delete Headers polyfill, install native unconditionally; `5677051` HeadersIterator → `#[v8_iterable(mode = live)]`; `d2fea29` Headers.has → fastcall.
- Follow-on perf: `8f85fb9` / `625bc59` (fetch-perf: native Headers state read on response inspect).
