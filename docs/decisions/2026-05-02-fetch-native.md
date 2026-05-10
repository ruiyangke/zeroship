# Native WHATWG Fetch shipped

**Status:** Shipped 2026-05-02
**Long-form design:** [`docs/proposals/fetch-native.md`](../proposals/fetch-native.md)
**Implementation:** [`crates/runtime/src/web/fetch/`](../../crates/runtime/src/web/fetch/) (~5,400 LOC: request.rs, response.rs, body/, algorithms.rs)

## Context

The runtime carried a 705-LOC JS polyfill at `embed/fetch.js` plus
a 386-LOC Rust dispatch shim at `transport/handler.rs`. The
polyfill diverged from WHATWG Fetch on body semantics, abort
chains, and Request/Response identity — silently breaking modern
libraries (langchain, LangGraph, AI SDK, Stripe SDK, OpenAI SDK)
that rely on `response.body.getReader()` for SSE and
`request.signal` propagation. We ran zero of WPT
`fetch/api/{headers,request,response,abort}` because the polyfill
was too far from spec for the harness work to be worthwhile.

## Decision

- Replace `embed/fetch.js` and the dispatch shim with native `#[v8_class] Request` / `Response` types backed by `cyper` (compio + hyper, no tokio).
- All body operations route through native streams (`from_native_source` / `pipe_native_internal` / `from_native_sink`).
- `Request.headers` / `Response.headers` are `[SameObject]` native `Headers` instances with the proper guard machinery (request / request-no-cors / response / immutable / none).
- Honor the `Content-Encoding` decompression hook from compression-streams-native.
- Cutover via three feature-flag landings: native behind flag → flip default → slim `fetch.js` to DOMException + stream bridge → drop `__rawFetch` global.

## Consequences

- Modern web libraries work end-to-end on zeroship for the first time.
- Deletion of `crates/runtime/src/embed/fetch.js` (705 LOC).
- WPT `fetch/api/{headers,request,response,abort}` runnable.
- Post-ship perf work: typed enums for Request/Response init dicts (`7417cc1`), lazy Request Headers + native `Response.json` (`5bb35bb`), `ResponseTemplateSlot` Eternals (`6fa5422`). MAC-01 migrated Request/Response to `#[v8_state_marker]` (`16451c8`, `ec2d3dc`).

## See also

- Implementing commits: `afabe99` native fetch() — algorithms + V8 entry + 57 unit tests; `6c78d60` cutover landing 2c (native default); `ac8265a` cutover landing 3a (slim fetch.js); `af200e7` drop `__rawFetch` global; `d59c457` Merge feature/fetch-native.
- Related ADRs: [streams-native](./2026-05-02-streams-native.md), [headers-native](./2026-05-01-headers-native.md), [compression-streams-native](./2026-05-01-compression-streams-native.md).
