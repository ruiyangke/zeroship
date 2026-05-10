# RPC v2 Proposal Critique — Round 3

**Reviewer:** design-critic role (model: opus-4-7)
**Subject:** `docs/proposals/rpc-v2.md` (1,881 LOC, post round-2 reviser)
**Lens:** Did the round-2 reviser address every Critical and High finding from round 1? What new issues did the rewrite introduce?

**Verdict in one sentence:** The rewrite addresses all five round-1 Critical findings (ALS, single wire, native `RpcError`, AI SDK 5, distributed lock) and most of the High findings (RSC-aligned discovery, FormData first-class, fastcall, auth chain, observability), but it underspecifies four newly-introduced mechanisms (the HMAC-signed `ZeroShip-User` payload format, the wire-compat check at deploy time, the `_zs.json` multipart convention, the streaming wire's error-mid-stream protocol) and ships several internal contradictions (kind: "raw" vs §6 streaming; `requireUser()` vs `ctx.user` accessor semantics; phase-0 `RpcError` listed before any procedure dispatch exists).

---

## 1. Score

| Dimension | Score | One-liner |
| --- | --- | --- |
| Clarity | 80 | Sections crisp; the new §4b multipart subsection is well-organized; §3 `ctx` field table is excellent. But §4b's "transform sees the parameter type" mechanism is hand-waved (TypeScript types are erased — how does the build see the type?). §6 streaming has three modes but the framing of when each is chosen is "the SDK picks" without a hard table of `(output type, environment) → wire`. |
| Soundness | 76 | ALS-based `ctx` is correct (matches the platform); `RpcError` `#[v8_class]` is correct; SETNX+EX gateway-side lock is correct; AI SDK 5 stream protocol is correct. Three soundness flaws remain: (a) the `_zs.json` multipart convention has no spec — what character set, what encoding, what about nested superjson `meta`?; (b) the wire-compat check (§13 "build runs a wire-compat check at deploy time against the previous deploy's manifest") is described in one sentence with no algorithm; (c) `kind: "raw"` and §6's three streaming wires conflict — a raw procedure that returns `Response` with `Content-Type: application/x-ndjson` bypasses the SDK content negotiation, which means two paths produce the same wire, so why have content negotiation? |
| Completeness | 75 | Major gaps closed (FormData, observability, meter, traceparent, auth chain, lock primitive, deprecation/sunset). New gaps: (a) **no spec for `Headers`/`URL` ctx field mutation rules** (the table says "read-only view; mutations forbidden" but doesn't say what happens on attempted mutation — silent ignore? throw?); (b) **the streaming error-mid-stream protocol for SSE and AI SDK 5 modes is unspecified** — only NDJSON's `_zs_error` sentinel is shown; (c) **WebSocket auth re-validation timing** says "every 5 min (configurable)" but doesn't specify the failure-mode JSON frame shape; (d) **no spec for `auth: "anon"` + `idempotent: true` interaction** (anonymous mutations with idempotency keys — possible footgun: idempotency keyed by what user partition?); (e) **RPC-on-RPC (one procedure calling another)** is unspecified — direct call vs. wire roundtrip; (f) **no test/spec story** — the proposal lists implementation phases but never specifies acceptance criteria. |
| Feasibility | 78 | Phases are now LOC-and-dependency-tagged, with phase 0 (native `RpcError`) cleanly separable from the rest. But phase 0 + phase 1 ship Rust crates that do not yet exist (`crates/runtime/src/rpc/{dispatch,error,superjson}.rs`) and the LOC estimates are by-eye. Phase 5's "gateway-side SETNX flow with Lua-script release" is real but "build runs a wire-compat check at deploy time" (§13) is not in any phase. Phase 4's "CHWBL routing" is already shipped — so what's the actual delta there? The phase boundaries leak. |
| Industry-fit | 82 | RSC-aligned discovery is now explicit (matches React 19 / Next.js 16). AI SDK 5 stream protocol is current. tRPC v11 ALS-backed context is acknowledged. Stripe-pattern idempotency lock is correct. Two industry-fit gaps remain: (a) the `kind: "raw"` escape hatch is the **Hono pattern** (handler returns `Response` directly), which the proposal doesn't cite; (b) the proposal's `Zs-Procedure-Version` header is novel — Stripe uses `Stripe-Version` (single global); GitHub uses `X-GitHub-Api-Version`; AWS uses URL prefix or `Accept-Version`. The proposal picks per-procedure granularity, which is *more* powerful but also more operationally complex than any cited industry comparator. The choice isn't justified. |
| Consistency | 70 | Major contradictions: (a) §3 `ctx.headers: Headers (native)` says "read-only view; mutations forbidden" but Headers is `#[v8_class]` mutable by default — does the kernel construct a frozen instance, or does it intercept setters? Spec gap; (b) §4b says "the transform sees the parameter type" for routing FormData/File, but TypeScript types **are erased at build time** — the transform can't see types unless the wrapper is used (`mutation({ input: z.object({ avatar: z.instanceof(File) })`) or a marker is on the function. The text reads as if pure-TS-types determine the wire, which is wrong; (c) §6 says streaming uses `application/x-ndjson` by default but §4b's "Procedure that returns Blob/ReadableStream" says raw bytes go on `application/octet-stream` — a `ReadableStream<Todo>` then has *two* candidate wires (NDJSON because it's a typed stream, octet-stream because it's a ReadableStream). Which wins? (d) §10 React-Query "subscriptions" hooks reference an unsubscribe lifecycle, but §6 explicitly **defers** the subscription protocol to a separate proposal. Why are hooks specified before the wire is? (e) §13 "default version pinned in manifest" but §13 example shows `versions: { "1": { default: true }, "2": { default: false } }` — what happens if both are deprecated? What's the fallback? |
| Fit-with-platform | 88 | This is the dimension that moved most. ALS (now used), `#[v8_class]` (now used for `RpcError`), `#[v8_method(fastcall)]` (now used on dispatch), native FormData/Blob/File (now first-class), HMAC-signed `ZeroShip-User` (now spec'd), `zeroship.meter.*` (now wired). Two remaining mismatches: (a) §6 says the gateway "decodes the superjson envelope" — but the gateway has no V8; superjson decode is plausibly fine in pure Rust (per §4 "superjson is implemented natively in `crates/core/src/superjson.rs` (new)"), but the dual decode-once gateway / decode-again-in-V8 path isn't justified — why decode twice?; (b) the `compio-postgres` / `compio-redis` invariant (zero tokio) is not cited anywhere in the lock-primitive section — the design says "Redis SETNX with TTL" but doesn't say "we use `compio-redis::SET key value NX EX 30`"; the implication is fine, but the platform invariant should be explicit. |
| **Composite** | **78** | **Major structural fixes landed; spec is now production-ready in the large; ten concrete underspecifications remain to close before implementation.** |

Composite up 29 points (49→78). Round-1's three damning findings are gone. The remaining gaps are pre-implementation polish items — none rise to Critical, but two reach High.

---

## 2. Round-1 follow-up — what got addressed

| Round-1 finding | Severity | Status in this draft | Notes |
| --- | --- | --- | --- |
| **Critical-1** §3 ambient context on request-id slot | Critical | ✅ **Resolved.** §3 fully rewritten around ALS. `ctx` is a real object populated by Rust before V8 runs the procedure. `getRequestContext()` for transitively-deep code. The "no AsyncLocalStorage gymnastics" line is replaced with "ALS is the gymnastics-free version." |
| **Critical-2** Wire format superjson/JSON mismatch | Critical | ✅ **Resolved.** §4 is unambiguous: superjson is the canonical envelope, every emitter ships `{json, meta?}`. Native superjson encoder/decoder in Rust (gateway and worker). |
| **Critical-3** `RpcError` brand check via `instanceof` | Critical | ✅ **Resolved.** §6 specifies `RpcError` as `#[v8_class] #[v8_state_marker]` with internal-field brand, mirroring `DOMException`. `is_rpc_error(scope, val)` is the Rust-side brand check. The `WebIdlEnum` ZsErrorCode is correct. |
| **Critical-4** Streaming locked to deprecated AI-SDK v4 | Critical | ✅ **Resolved.** §6 streaming is content-negotiated. AI SDK 5 UI Message Stream is opt-in via `wire: "ai-ui-v1"`. The default for typed streams is NDJSON. SSE only when `Accept: text/event-stream`. Octet-stream for raw binary. |
| **Critical-5** Distributed idempotency lock unspecified | Critical | ✅ **Resolved.** §8 specifies gateway-side SETNX with EX TTL, Lua-script release, lock TTL = `max(timeout × 2, 30s) + 5s`. Failure modes documented (lock holder dies; Redis failover; LRU eviction). |
| **High-1** `ctx` is positional-arg, not primitive | High | ✅ **Resolved.** §3 makes `ctx` the canonical surface. Helpers `user()` / `requestId()` are wrappers over `getRequestContext()`. |
| **High-2** Reference-graph detection (#168) not sketched | High | ✅ **Resolved.** §1 specifies file-level + function-level `"use server"` with reference-graph walk, mirroring RSC. Strict-mode requires explicit directive. |
| **High-3** FormData/Blob/multipart afterthought | High | ✅ **Resolved.** §4b is a full subsection. Native FormData/File first-class. Multipart wire spec'd. Progressive-enhancement form pattern documented. |
| **High-4** `ctx.signal` propagation unspecified | High | ✅ **Resolved.** §3 `ctx.signal` is `AbortSignal.any([clientDisconnect, gatewayDeadline, isolateEviction])`, auto-passed to all native ops. |
| **High-5** Manifest build-time/runtime coupling | High | ✅ **Resolved.** §7c artifact / policy split. `manifest.artifact` is immutable per build; `manifest.resources` is hot-reloadable post-deploy via `zeroship policy push`. |
| **High-6** Subscriptions sketched not designed | High | ✅ **Acknowledged.** §6 defers full subscription contract to `rpc-subscriptions.md`; this proposal commits only to upgrade routing + JSON-frame envelope minimums. |
| **High-7** Dispatch fast-path doesn't use `#[v8_method(fastcall)]` | High | ✅ **Resolved.** §5 specifies `RpcDispatcher` `#[v8_class]` with `#[v8_method(fastcall)]` `dispatch_json` and `dispatch_multipart`. |
| **High-8** Auth wiring uses unspecified ZeroShip-User chain | High | ✅ **Resolved.** §14 fully specifies the HMAC-signed payload, gateway → worker handoff, defense-in-depth, CSRF for cookie-auth. |
| Medium-1 `kind` regex inference | Medium | ✅ **Deleted.** §1 explicitly drops the regex. Wrappers carry the kind. |
| Medium-2 `idempotent: true` on a query | Medium | ✅ **Resolved.** §7 validation: build error. §8 reinforces: mutations without `idempotent: true` may not include `Idempotency-Key`. |
| Medium-3 Per-procedure timeout for streams | Medium | ✅ **Resolved.** §7 kind-aware timeout: `{ms}` for query/mutation, `{inactivityMs, maxLifetimeMs}` for stream/subscription. |
| Medium-4 Batching scoped to queries | Medium | ✅ **Resolved.** §6 batching now allows mutations + queries; per-call idempotency keys; per-call failure isolation. |
| Medium-5 `ctx.signal` not propagated to outgoing fetches | Medium | ✅ **Resolved.** §3 mandates auto-passing to all native ops. |
| Medium-6 `traceparent` propagation unspecified | Medium | ✅ **Resolved.** §12 specifies W3C trace context level 2, gateway creates if absent, runtime auto-injects on outgoing fetches. |
| Medium-7 ETag / If-None-Match unspecified | Medium | ✅ **Resolved.** §6 specifies `sha256(canonical-superjson)[:16]`, `Vary: Authorization`, CDN-cacheable conditions. |
| Medium-8 No `max_output_bytes` etc. | Medium | ✅ **Resolved.** §4b table includes `max_output_bytes`, `max_concurrent_per_user`, `max_concurrent_per_app`. |
| Medium-9 `defineApp` config eval | Medium | ✅ **Resolved.** §7 specifies "literal-only — no computed expressions, no env-var reads, no imports of values. Build AST-walks." |
| Medium-10 Meter integration missing | Medium | ✅ **Resolved.** §12 defines per-call meter events; "billing is a free side-effect." |
| Low-1 "Single dispatch path" overstated | Low | ✅ **Resolved.** §5 makes the single path a single dispatcher class with two fastcall entries. |
| Low-3 `dev: true` is global override | Low | ⚠️ **Partial.** §6 acknowledges per-environment knob via `--env staging` build flag, but the example still shows `defineApp({ rpc: { dev: true } })` as the primary mechanism. The per-environment story isn't sketched. |
| Low-4 `application/zs-error+json` asymmetry | Low | ⚠️ **Acknowledged but kept.** §6 explicitly chooses asymmetric (custom error content-type, plain JSON success) "because successes are nearly always application/json already and the value of branding errors specifically is high." Defensible but ambivalent. |
| Low-5 Idempotency storage cap | Low | ✅ **Resolved.** §8 specifies LRU eviction via `MEMORY POLICY allkeys-lru`. |
| Low-7 References cite tRPC v11 but design uses v10 types | Low | ✅ **Improved.** §17 references list now includes "tRPC v11 — `createContext()` + ALS-backed context." |
| Nitpick `Retry-After` for 409 | Nit | ✅ **Resolved.** §8 explicitly uses `X-Original-Completed-At` instead. |
| Nitpick `claude-opus-4-7` placeholder | Nit | ✅ **Resolved.** §6 example uses `claude("opus")`; §9 uses `<modelId>`. |
| Nitpick `cacheable.swr` naming drift | Nit | ✅ **Resolved.** §10 now says `cache.swr`. |
| Nitpick `1MB` cliff | Nit | ✅ **Improved.** §4b table makes 32 KB/1 MiB/100 MiB explicit thresholds with rationale. |

**Score**: 25 of 27 round-1 findings closed; 2 partial; **0 ducked**. Strong reviser pass.

---

## 3. New flaws introduced — by severity

### CRITICAL

(none)

### HIGH

**High-A — `_zs.json` multipart convention has no spec.**
- **Cite:** §4b ("the transform builds multipart with `_zs.json` + the file part. The handler receives them as native `File` and the parsed JSON object.") and the wire example showing `Content-Disposition: form-data; name="_zs.json"` carrying `{ "json": { "caption": "..." } }`.
- **What's wrong:** The proposal asserts the `_zs.json` part exists but doesn't specify (a) is it required when there's no JSON portion to send (or only File parts)?; (b) what's the part's `Content-Type`? `application/json`? `application/json; charset=utf-8`?; (c) does it carry the superjson envelope (`{ json, meta }`) or just the `json` half?; (d) does its presence change the wire negotiation (e.g., its absence means "pure FormData; pass through to handler as FormData")?; (e) what's the field-naming collision rule — what if a creator's FormData has a part literally named `_zs.json`?
- **Why it matters:** This is the single most-load-bearing detail of the multipart story; without it, the runtime cannot deterministically dispatch a multipart request. Two implementations will disagree on the boundary between "pure FormData procedure" and "structured-input-plus-files procedure."
- **Direction:** Add a §4b subsection "Multipart envelope spec." Specify: (1) `_zs.json` is **always present** when the procedure has any non-Blob/File field; its `Content-Type` is `application/json`; it carries the **full superjson envelope** with the structured fields (`{ "json": {...}, "meta": {...} }`); (2) When the procedure parameter is exactly `FormData` (raw), `_zs.json` is **absent** and the runtime hands the parsed multipart directly to the handler; (3) The reserved `_zs.*` prefix is forbidden in user form fields — build error if the form schema declares a field named `_zs.json` or other `_zs.*`; (4) Boundary-collision case: runtime detects and 400s with `INVALID_ARGUMENT`.

**High-B — The "transform sees the parameter type" mechanism is non-existent.**
- **Cite:** §4b ("The transform sees the parameter type and tags the procedure with `wire: "multipart"` in the manifest.") and §10 (`__makeProcedure` references the wire mode).
- **What's wrong:** TypeScript types are **erased at build time**. The transform is rolldown/oxc, which parses syntax but doesn't run the type checker. The transform cannot "see" `(form: FormData) =>` and decide on multipart routing — it sees a parameter with an annotation that may or may not be `FormData`, with no way to confirm without invoking `tsc`. The actual mechanism that the proposal *means* is one of: (a) the wrapper carries the input schema (`mutation({ input: z.instanceof(FormData) })`) and the build inspects the schema; (b) the user explicitly sets `fn.config.wire = "multipart"`; (c) the build inspects the annotation **as a syntactic shape**, accepting only specific bare type references (`FormData`, `File`, `Blob`). Each has tradeoffs the proposal doesn't explore.
- **Why it matters:** Without this clarification, the §4b mechanism reads as if pure-TS-types determine wire routing, which is a build-time correctness issue (an AI-built app that types its parameter as `Blob | string` because it accepts both could land on the wrong wire silently).
- **Direction:** Pick one of three. Recommend (a) or (b) hybrid: the wrapper schema (Zod `z.instanceof(FormData)`, `z.instanceof(File)`, `z.instanceof(Blob)`) is the canonical signal, with `fn.config.wire = "multipart"` as the manual override. Drop "the transform sees the parameter type" — replace with "the wrapper's input schema declares the wire shape; the build emits `wire: "multipart"` when the schema includes a FormData/File/Blob branch."

**High-C — The wire-compat check at deploy time is a one-sentence hand-wave.**
- **Cite:** §13 ("The build runs a wire-compat check at deploy time against the previous deploy's manifest (stored in the control plane). Breaking changes without a version bump emit a build error in production: ...").
- **What's wrong:** The check is described in one sentence with no algorithm. Critical questions unanswered: (a) what's the canonical comparison — sha-256 of the schema canonicalized, or per-field structural diff?; (b) does the comparison consider Zod schema literals, or only declared types?; (c) Zod schemas can be programmatic (`z.union([Foo, Bar])` where Foo/Bar are imports) — does the canonicalization recurse?; (d) what's the diff output presented to the user? "The field `text` was required, now optional default" — but who computes that? `zod-to-json-schema` then JSON Patch?; (e) how does the deploy-time check interact with the literal-only `defineApp` rule (§7) — the same constraint must apply to schemas, but the current text only asserts it for resources; (f) what's the storage shape of the prior manifest? Just the latest, or last-N?; (g) what happens when the check disagrees with the creator's intent (false positive)?
- **Why it matters:** This is the platform's only protection against silent wire breaks. Without a real algorithm, the check is unimplementable.
- **Direction:** Either spec the algorithm (recommended: per-procedure JSON-schema diff using `zod-to-json-schema`, with a configurable allowlist of "non-breaking" diffs — adding optional input field, adding output field; everything else triggers the gate), or downgrade the section to "future work; not in initial release; current production gate is the explicit `id` requirement on every procedure." The middle ground is unworkable.

**High-D — `kind: "raw"` and content-negotiated streaming overlap, with unspecified resolution.**
- **Cite:** §4b ("`kind: "raw"` opts out of all envelope/superjson handling. The procedure is invoked with a native `Request` object; its return value is serialized as the response.") vs. §6 streaming wire table.
- **What's wrong:** A `kind: "raw"` procedure can return `new Response(stream, { headers: { "Content-Type": "application/x-ndjson" } })`. This is now the *exact* same wire as a `kind: "stream"` procedure with `Accept: application/x-ndjson`. Two paths produce the same wire; the gateway has to disambiguate. Worse: idempotency, ETag, content-negotiation (the SDK picking SSE vs NDJSON vs octet-stream) all skip for `kind: "raw"` — but a raw procedure *streaming* loses the inactivity-timeout semantics of `kind: "stream"`. The proposal doesn't say.
- **Why it matters:** The escape-hatch overlaps with the primary path. Either escape-hatch loses access to all stream affordances (no flow control, no auto-emitted meter `rpc.stream_chunks`, no inactivity timeout), or the kernel has to inspect raw response bodies — circular.
- **Direction:** Document the explicit tradeoffs. `kind: "raw"` opts out of: superjson envelope, ETag, content-negotiation, structured arg validation, `rpc.stream_chunks` meter, NDJSON sentinel framing. It keeps: auth, rate-limit, idempotency, `ctx`. If the procedure returns a streaming Response, the *runtime* still applies inactivity timeout (defaulting to 30s, configurable via `fn.config.inactivityMs`). Make this an explicit table. Or: forbid `kind: "raw"` from returning streaming responses (force the use of `kind: "stream"`).

### MEDIUM

**Medium-A — Phase 0 ships `RpcError` before any callsite for it.**
- **Cite:** §16 phase table — phase 0 = `RpcError` `#[v8_class]`; phase 1 = dispatch + ctx; phase 4 = gateway routing.
- **What's wrong:** Phase 0 says "Brand check + redaction wired into the dispatch path" but the dispatch path is phase 1. Phase 0 is unimplementable in isolation: you need at minimum a JS-side test harness or an early-stub `default.rpc` that throws/catches an `RpcError`. The phase ordering implies an isolated `RpcError` class shipping without dispatch — and at that point, what does it do?
- **Why it matters:** Phasing is implementation guidance; broken phases turn the proposal into a wishlist.
- **Direction:** Either merge phases 0 and 1 (recommend), or define what phase 0's deliverable actually is in isolation: "Native `RpcError` class registered on every isolate; constructor + getters; `is_rpc_error` brand check; consumed by the existing JS-side `RpcError` stub (which is replaced) — no dispatch wiring yet." Make phase 1's "wired into the dispatch path" the actual integration step.

**Medium-B — `ctx.headers` "read-only" semantics underspecified.**
- **Cite:** §3 field table.
- **What's wrong:** The cell says "Read-only view; mutations forbidden." But native `Headers` is `#[v8_class]` and mutable by default (its setters are real). What does "read-only" mean? Three choices: (a) `Object.freeze()` the JS wrapper (V8-safe; straightforward); (b) the kernel constructs `Headers` with an internal-state flag that makes setters throw; (c) the kernel constructs a *clone* and mutations are silently ignored. Each has tradeoffs (a is the simplest; b is the most ergonomic but requires a runtime-macros change; c is the most surprising). The proposal silently picks (a) by saying "mutations forbidden" but doesn't spec.
- **Why it matters:** This sets a precedent for any future "read-only" wrapper of a normally-mutable native primitive (headers in `ctx`, `URL` in `ctx`, `Request` in `ctx` for raw procedures).
- **Direction:** Specify (a). Add a sentence: "The `ctx.headers` instance is `Object.freeze`'d; attempted setters throw `TypeError`. Same for `ctx.url`."

**Medium-C — Streaming error-mid-stream protocol is only specified for NDJSON.**
- **Cite:** §6 streaming. NDJSON's `_zs_done` and `_zs_error` sentinels are shown. SSE and AI SDK 5 modes are silent.
- **What's wrong:** A stream procedure that throws mid-stream — what frames does the wire emit?
  - For SSE: a `data:` line with what JSON? With `event:` field set to what?
  - For AI SDK 5 UI Message Stream: the protocol's spec includes `{type: "error", ...}` parts; but the proposal doesn't say which RPC error code maps to which UI Message error shape.
  - For NDJSON: the `_zs_error` sentinel format isn't fully shown — is it `{json: null, meta: {_zs_error: <wire envelope>}}` or `{json: null, meta: {_zs_error: {code, message, details, retryable, requestId, traceId}}}`? Latter is the wire envelope; former implies double-wrapping.
- **Why it matters:** Mid-stream errors are common (LLM hits rate-limit; client disconnects half-way; DB times out). Without a spec, every backend produces a different shape and clients can't generically recover.
- **Direction:** Add a "Streaming errors" subsection in §6. NDJSON: `{ "json": null, "meta": { "_zs_error": <wire envelope as in success path> } }` (full envelope; no double-wrap; superjson `meta` for the envelope itself goes inside). SSE: `event: error\ndata: <wire envelope>\n\n`. AI SDK 5: emit a v1 protocol `{type:"error", errorText: <message>, errorCode: <code>}` chunk, then `[DONE]`.

**Medium-D — `auth: "anon"` + `idempotent: true` interaction is unspecified.**
- **Cite:** §1 wrappers (`mutation({ idempotent: true, ... })`); §8 idempotency keyed by `(app_id, wireId, idempotency_key)`.
- **What's wrong:** Anonymous mutations with idempotency keys are possible. Two anonymous clients could pick the same `Idempotency-Key` (not coordinated). The dedupe key is `(app_id, wireId, idempotency_key)` — it has no user partition. So client A and client B's "same key" requests would dedupe to A's response.
- **Why it matters:** Cross-user response leakage. Specifically: `auth: "anon"` mutations are typically things like contact-form submissions or sign-up flows; `Idempotency-Key` collisions there are unlikely but not impossible, and the failure mode is data leak (B sees A's response).
- **Direction:** Either (a) include the user (or `ses_*` from cookie, or client IP, or the request's full origin/UA fingerprint) in the dedupe key — but none of these are stable for `auth: "anon"`; (b) require client-side cryptographically random keys (UUIDv4 / UUIDv7) for `auth: "anon"` mutations — collision probability becomes astronomical; (c) forbid `idempotent: true` + `auth: "anon"` outright. Recommendation: (b) — require keys to pass a randomness check (high-entropy bit count) for `auth: "anon"`, and document the rule.

**Medium-E — RPC-on-RPC unspecified.**
- **Cite:** Nowhere. Search of the proposal turns up no mention of one server function calling another.
- **What's wrong:** A common pattern: a server function `add()` calls another server function `recompute()` to invalidate caches. With the seamless model, `import { recompute } from "../actions/cache"` from inside `add` would resolve to the same module (since both are server bundles), and the call would be **direct** — bypassing rate limits, idempotency, validation, metering.
- **Why it matters:** (a) Performance: direct call is cheaper than wire roundtrip — ✓ desirable; (b) Auditability: no `rpc.requests` event for the inner call — ✗ undesirable for billing; (c) Validation skip: the inner function's Zod schema is bypassed — ✗ undesirable for safety.
- **Direction:** Add an "RPC-on-RPC" subsection. Specify: direct call is the default (for performance). The function still runs validation if its wrapper is invoked (the wrapper enforces it). The meter increments locally. Explicit "rpc.fanout" event tagged with the inner procedure's id. If the creator wants wire-fidelity (true HTTP roundtrip with rate-limiting, etc.), they call the URL via `fetch` explicitly.

**Medium-F — Streaming + `traceparent` per-chunk isn't specified.**
- **Cite:** §12 traceparent propagation says outgoing fetches inherit. But in a stream, the procedure may not make outgoing fetches; the trace lifecycle ends with the stream closure.
- **What's wrong:** A 30-minute streaming procedure has one trace; tracing tools that expect a span-per-event (e.g., `streamText` emitting tokens) get nothing. AI SDK 5's protocol has per-text-block IDs; OpenTelemetry conventions for streams emit a span per chunk or per chunk-batch. The proposal doesn't say.
- **Why it matters:** Operational visibility for streams.
- **Direction:** Specify: each stream emits one span per emit boundary, with `parentSpanId = <trace-id>:<gateway-span-id>`. Per-chunk spans are an opt-in via `defineApp({ observability: { streamChunkSpans: true } })`; default is one span per stream.

**Medium-G — `Zs-Procedure-Version` granularity is novel.**
- **Cite:** §13.
- **What's wrong:** The proposal picks per-procedure header-routed versioning. Industry comparators: Stripe (`Stripe-Version: 2024-09-30`) is global per account; GitHub `X-GitHub-Api-Version` is global per request; AWS API Gateway uses URL prefix. None ship per-procedure granularity. The reason: per-procedure granularity multiplies operational surface (one creator's app could have 100 procedures, each pinned to 5 versions = 500 (procedure × version) cells in the gateway hot path). The gateway lookup table grows by `O(versions)` per procedure.
- **Why it matters:** Lookup-table size and operational complexity; per-procedure-version metrics fan out by `O(procedures × versions)`.
- **Direction:** Either (a) downgrade to "future work" and start with global app-version pinning (`Zs-App-Version` analogous to Stripe-Version); (b) cap procedure versions (e.g., max 3 live versions per procedure in the artifact); (c) keep per-procedure but add a justification subsection explaining the cost/benefit. Recommendation: (b) — keep per-procedure, document a hard cap.

**Medium-H — `Sec-Fetch-Site: same-origin` CSRF check has corner cases.**
- **Cite:** §14 CSRF rule "Sec-Fetch-Site: same-origin + Sec-Fetch-Mode: cors (browser-emitted; can't be forged from cross-site contexts)".
- **What's wrong:** `Sec-Fetch-*` headers are from the Fetch Metadata Request Headers spec. Coverage:
  - Modern browsers (Chrome 76+, Firefox 91+, Safari 16.4+) emit them.
  - Older browsers (older Safari, IE, older mobile WebViews) **don't**, and the headers are absent (not `cross-site`). The proposal's rule treats absence as "unsatisfied" — older browser users get rejected.
  - Curl, Postman, all tooling — don't emit them. Bearer-only requests skip CSRF; cookie-only requests with curl get rejected. The proposal acknowledges Bearer skips CSRF, but a developer hitting their dev cookie session via `curl --cookie` is now blocked.
- **Why it matters:** Operational footgun. Most platforms accept the Origin header alone for cookie-auth CSRF; `Sec-Fetch-*` is a strict-but-narrow strengthening, not a replacement.
- **Direction:** Make the rule "Origin allowlist is the primary check; `Sec-Fetch-Site: same-origin` is an alternative; double-submit token is the third alternative; any one suffices." (The proposal says "either or or" already, but "Sec-Fetch-Site" presence is *required* for that branch — clarify by stating "if Origin allowlist passes, Sec-Fetch-Site need not be checked.")

### LOW

**Low-A — `dispatch_json` and `dispatch_multipart` fastcall ABIs are sketched, not specified.**
- §5 shows them with `&self`, `wire_id`, `input_bytes` / `request`. Real fastcall ABIs in the platform have specific argument-marshalling rules (e.g., `Headers.has`'s fastcall takes only V8 primitives). `v8::Local<v8::Promise>` as a return type is unusual for fastcall — TurboFan inlining typically requires void / primitive returns. The proposal hand-waves the ABI; the real wiring needs to thread Promise resolution back via a JS-side resolver that the kernel signals. Acknowledge this; cite the `Headers.has` fastcall as the reference but note Promise-returning fastcalls may require a separate idiom.

**Low-B — `streamUrl(input)` security implication.**
- §10: `rpc.chat.completion.streamUrl({})` returns a URL with the input pre-encoded. For idempotent queries this is fine. For non-idempotent or auth-sensitive parameters, the URL might end up in a browser cache or referer header. The proposal doesn't note this. A short cautionary line would help.

**Low-C — `_zs_error` sentinel collides with a hypothetical user field.**
- A creator's Zod schema could include a field literally named `_zs_error`. The reserved prefix should be enforced as a build error in user schemas (analogous to the `_zs.json` form field reservation in High-A).

**Low-D — Subscription auth re-validation 5-min default needs a justification.**
- §14: "Re-validation on long-lived sockets every 5 min (configurable)." Why 5 min? Industry comparators: AWS AppSync revokes via control-plane signal (no polling); Convex sessions have a 1-hour TTL; Cloudflare Durable Objects use connection-scoped tokens. The 5-min poll is novel. Either justify or cite.

**Low-E — `ctx.env` ordering with `defineApp({ ... })` env declarations.**
- §3 says `ctx.env = merged vars + secrets`. But the merge order isn't specified. Does the deploy-time `defineApp.env` override worker-process env vars? Or vice versa? Or are they namespaced? Spec.

**Low-F — `defineApp` literal-only rule is enforced at build time but not in dev.**
- §7c says config must be a literal. Dev mode (`vite dev`) currently runs the config file as a module. The proposal doesn't say whether dev mode honors the same constraint (so that the prod-only constraint doesn't surprise creators at deploy time).

### NITPICK

- **§3 example uses `ctx.user.id` without null check.** The field table says `User | null`; the example assumes non-null. A line "`auth: 'user'` mutations are guaranteed non-null `ctx.user`" would help.
- **§4 type table conflates `Uint8Array` and `ArrayBuffer`.** Different JS shapes; superjson treats them distinctly. The table should split rows.
- **§4b "32 KB" inline-Uint8Array threshold is unsourced.** Where does 32 KB come from? base64 overhead? Browser URL limit? Pick a rationale or cite.
- **§5 synthetic entry's USER_ENTRY placeholder.** What's the resolution rule? `package.json::main`? `vite.config.ts::input`? Spec.
- **§6 `Vary: Authorization, Accept-Encoding`.** `Accept-Encoding` is fine but should also include `Origin` for CORS-cached responses.
- **§13 `add_v2` example has `id: "todos.add"` — same as `add`.** With both exported in the same file, the wireId-collision rule from §2 should fire. The proposal must explicitly carve out: "two procedures with the same `id` and different `version` fields are NOT a collision; they're two versions of one procedure."

---

## 4. Industry comparison — the moved-needle dimension

Round-1 score on industry-fit: 55. This draft: 82. The differences:

| Capability | This proposal (R3) | RSC / Server Actions | tRPC v11 | Hono RPC |
| --- | --- | --- | --- | --- |
| **Function discovery** | File + function `"use server"` + reference-graph (matches RSC) | Same | Manual builder | Manual `app.get/post(...)` |
| **Wire format** | superjson `{json, meta?}` mandatory; one path | RSC payload (binary; FormData first-class) | Pluggable transformer | Plain JSON |
| **Type safety** | Inferred `client<App>()` + virtual `.d.ts` for seamless | Phantom-type bundler-flowed | End-to-end inference | Inferred via `app.get<typeof router>()` |
| **Streaming** | Content-negotiated (NDJSON / SSE / octet / AI SDK 5) | RSC streaming JSX | Subscriptions (full bidi WS) | Native Web Streams |
| **Error model** | Native `RpcError` `#[v8_class]` + brand check + redaction | `notFound()`, `redirect()`, raw throws | `TRPCError` + code | Manual `HTTPException` |
| **Auth context** | ALS-backed `ctx`; `ZeroShip-User` HMAC chain | `cookies()` / `headers()` ALS-backed | `ctx` arg, ALS in v11 | `c.get('user')` |
| **FormData / binary** | Native FormData/File first-class; multipart wire | First-class `<form action={fn}>` | Plugin (community) | Native `c.req.formData()` |
| **Idempotency** | Spec'd; SETNX gateway lock | None | None | None |
| **Subscriptions** | Sketch only; full proposal deferred | None | Full WS, auto-reconnect, type-safe | Plugin |
| **Metering** | Auto-emitted per call | None | None | None |
| **OTel** | W3C trace context; auto-inject | None | Plugin | None |

This is no longer a sketch that imports the syntax of each. It's a design that picks an opinionated pattern for each row and justifies the choice. The proposal now wins on idempotency and metering vs. all comparators, ties RSC on FormData/discovery, ties tRPC on type-safety. It still loses to tRPC on subscriptions (deferred), to RSC on FormData binary (RSC's RSC-payload is more efficient than multipart). Both losses are deliberate scope choices.

---

## 5. Bottom line

The reviser pass moved the proposal from "stale tRPC sketch" to "platform-aware design." All five round-1 Criticals are resolved. The remaining gaps are at the implementation-detail level — important but not blocking the high-level design.

Composite **78** is the right number: the design is no longer wrong (round 1 was 49 because *the wrong primitive was chosen*); it's now mostly right but underspecified at four particular spots: (a) the multipart `_zs.json` envelope; (b) the build-time wire-compat algorithm; (c) the streaming error-mid-stream protocol across NDJSON/SSE/AI SDK 5; (d) the kind: "raw" vs. content-negotiated streaming overlap.

For round 4 the reviser should target these four High-* findings and the seven Mediums. After that, the score should be in the high 80s; another round of nitpick polish should land it ≥90.

---

## 6. Files referenced

- `/home/ruiyang/Projects/appbase/.worktrees/rpc-v2-revise/docs/proposals/rpc-v2.md` — subject of review (1,881 LOC, post round-2 reviser)
- `/home/ruiyang/Projects/appbase/.worktrees/rpc-v2-revise/docs/reviews/round-01-critique.md` — round 1
- `/home/ruiyang/Projects/appbase/crates/runtime/src/web/dom/exception.rs` — DOMException reference for `RpcError`
- `/home/ruiyang/Projects/appbase/crates/runtime/src/web/dom/form_data.rs` — native FormData reference
- `/home/ruiyang/Projects/appbase/crates/runtime/src/node/async_hooks/als.rs` — ALS reference for §3
- `/home/ruiyang/Projects/appbase/docs/reference/billing-metering.md` — meter taxonomy referenced in §12
</content>
</invoke>