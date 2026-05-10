# RPC v2 Proposal Critique — 2026-05-05

**Reviewer:** design-critic agent (model: opus-4-7)
**Subject:** `docs/proposals/rpc-v2.md` (1,389 LOC, last touched 2026-05-04)
**Lens:** does the design as written still match the platform that landed around it?
**Verdict in one sentence:** the proposal was a strong tRPC-meets-Server-Actions sketch when it was drafted, but it predates four landmark pieces (native ALS, native Request/Response/FormData/Streams, the `#[v8_class]/#[v8_method(fastcall)]` toolchain, and HMAC-signed `ZeroShip-User`); read against today's platform it specifies-around things that should now be primitives, leaks JS-side ceremony where Rust kernels are now available, and quietly ships a wire format that already mismatches its own client SDK.

---

## 1. Score

Sub-scores against the design-critic rubric. 80+ = production-ready; 90+ = AWS/Cloudflare-grade.

| Dimension | Score | One-liner |
| --- | --- | --- |
| Clarity | 72 | Sections are crisp, examples are concrete. But §3 contradicts itself ("no ALS gymnastics" vs. native ALS landed); §6 gives three streaming wires; §7 uses authoring sugar (`children:`) that doesn't appear in the wire shape, which the reader has to reverse-engineer. |
| Soundness | 48 | Critical: §3 ambient-context rests on a request-id slot that's the wrong primitive now that V8 `ContinuationPreservedEmbedderData` ships. §4 advertises superjson on the wire but the auto-emitted client stub strips `meta`. §6 streaming model assumes the AI-SDK Data Stream Protocol that's been deprecated in AI SDK 5 (now `x-vercel-ai-ui-message-stream` / start-delta-end). §8 idempotency mutex is unspecified for cross-worker. |
| Completeness | 40 | Missing entirely: observability (no `traceId` plumbing spec), CSRF for cookie-auth (mentions origins, never the SameSite/Origin/CSRF-token interaction), CDN cache key spec, multipart/binary protocol (§11 H is one paragraph for what is a 200-LOC subsystem), `ctx.signal` propagation across worker hops, schema migration, scope/permissions, in-flight cancellation during deploy. Open questions §15 doesn't even list any of these. |
| Feasibility | 55 | Phases 1-7 are estimated by LOC alone. No integration risk listed (V8 isolate per app + 1M idempotency keys → eviction interplay isn't mentioned). The "Re-invocation is benign for `async function*`" excuse-doc in `runtime.rs` is *promoted to the proposal*, when it should be a fast path the kernel actually owns. |
| Industry-fit | 55 | Borrows tRPC's procedure builders without their type inference, RSC's "use server" without RSC's reference-graph, AI-SDK without their current protocol, gRPC's error codes without HTTP/JSON-RPC's compatibility hooks. Result: it imports the syntax from each, but cherry-picks the shallow layer of each. |
| Consistency | 45 | §1 says "no `defineRpc`, no `.useQuery()`" then §10 ships `.useQuery()`. §1 says "path is the only marker" then §6 shows `Idempotency-Key` from `fn.config.idempotent: true` with no path. §15 says open questions are "Resolved" but the resolution sometimes contradicts the surrounding section. The wire surface of "subscription" is documented twice with different framings (§6 JSON frames vs. §10 reconnect). |
| Fit-with-platform | 30 | The proposal repeatedly hand-waves around things the platform has already solved — and ignores things it hasn't. ALS native (ignored). FormData / Blob native (ignored). `#[v8_method(fastcall)]` (not mentioned, but the `default.rpc` dispatch is exactly the workload that justifies it). The ZeroShip-User HMAC chain (§3 says `user()` reads from a slot but never specifies who fills it). The platform has primitives the proposal does not draw on. |
| **Composite** | **49** | **Solid skeleton, stale assumptions, structurally underspecified for its current ambition.** |

The proposal scores higher than its current implementation because the words on the page describe more than what's been built (per `rpc-architecture-critique-2026-05-05.md` the impl is 62/100 and well-designed half-built); the trouble is the doc has aged faster than the codebase even at 65 KB.

---

## 2. Executive summary — what's most damning, ranked

1. **§3 "Ambient context" is built on the wrong primitive.** The doc explicitly says: *"Single request per concurrent invocation in V8 isolates → no AsyncLocalStorage gymnastics; the slot is keyed by the kernel's in-flight request id."* (line 198). This is now backwards. Native `AsyncLocalStorage` landed (ISS-01, `crates/runtime/src/node/async_hooks/als.rs`) backed by V8's `ContinuationPreservedEmbedderData`, which propagates automatically across `await`, microtask, `.then`, generator yield. The proposal's "single request per invocation" is true; the implication "therefore we don't need ALS" is now inverted — ALS is precisely the right primitive *because* it's free across awaits, while the request-id slot the proposal references requires the kernel to remember to repaint it on every V8 turn (the `executing_request_id` dance in `crates/runtime/src/auth.rs`). **The proposal recommends a worse mechanism than the one already shipped.**

2. **The wire format is described two ways and the gateway version doesn't match the client.** §4 advertises superjson `{ json, meta? }` as the contract. The vite-plugin's auto-stub at `sdks/vite-plugin/src/transform.ts:250` emits `JSON.stringify({ json: input })` — no `meta`. The kernel's fast-path parser (`crates/runtime/src/core/runtime.rs:2755-2797`) reads `body.json` and discards `meta`. The real `@zeroship/rpc-client` does include `meta`. So the proposal's "Date round-trips as Date" promise is true if you import `client<App>()` manually and false if you use the imported function. The proposal does not pick a winner. The pre-existing impl review (§ Critical-2 of `rpc-architecture-critique-2026-05-05.md`) flags this; the design doc still contains it.

3. **`RpcError` should be a native `#[v8_class]` and isn't.** §6 specifies a class with brand-check semantics ("non-`RpcError` redacts to INTERNAL"). Today the platform has `DOMException` as a real `#[v8_class]` (with `name`/`code`/proper prototype chain). The proposal's `RpcError` is a JS class in `sdks/server/src/index.ts:154` — a 9-line stub. The brand check ("is this an RpcError?") that the redaction pipeline depends on is unspecified: how does the kernel tell `instanceof RpcError` from `instanceof Error`? Realm-locality. Cross-isolate. The right answer is the same answer DOMException uses (`#[v8_class]` + intrinsic prototype). The proposal omits the design.

4. **The proposal doesn't anticipate native-Web binding for FormData / Blob / File.** §11 H ("file upload (large args)") is one paragraph: "superjson encodes `Blob` as base64 (small files OK) or for >1 MB the SDK auto-routes via streaming `multipart/form-data`." The platform now has `FormData` (`#[v8_class]` + `#[v8_iterable(mode=live)]`), `Blob`, `File`, `ReadableStream` — all native. The natural design is: a procedure declared as `(form: FormData) => …` parses the request via native `Request.formData()` (which is already in the runtime's Request impl); the wire is the standard multipart format every browser already speaks; the gateway content-type-routes between JSON and multipart at the edge. The proposal does none of this — it treats Blob as "encode as base64" (a JSON-shaped retrofit). Result: every Server-Actions-style `<form action={uploadAvatar}>` use case requires the SDK to allocate, base64-encode, and re-decode the entire blob in JS, when the runtime already ingests it natively.

5. **Streaming wire is locked to a deprecated AI-SDK protocol.** §6 specifies the line-prefixed Data Stream Protocol (`0:`, `2:`, `e:`, `d:`). AI SDK 5 (current as of 2026-05) deprecated that and shipped UI Message Streams with `x-vercel-ai-ui-message-stream: v1` (start/delta/end with text-block IDs). The proposal would ship as the *legacy* SDK adapter the day it lands. There is no version-discriminator in the URL or content-type; the `Accept` header bears the contract. Worse: §6 conflates "AI-SDK ecosystem" with "all of streaming" — for a generic data stream that isn't an LLM token feed, the line-prefix protocol is bizarre cargo. RSC streaming, Hono streams, and gRPC-Web all use either chunked JSON-lines or HTTP/2 server-push with content-type `application/x-ndjson` — none use the AI-SDK shape.

6. **`ctx` is not a real platform handle.** The proposal models `ctx` as an opaque positional arg threaded through `default.rpc(name, input, ctx)`. The current `ctx` (`runtime.rs:1007-1037`) is a frozen singleton with `waitUntil` and `passThroughOnException` and *nothing else*. The proposal's helpers (`user()`, `signal()`, `traceId()`, `idempotencyKey()`) read from a request-id slot. But the real Cloudflare/Workers/Deno pattern (and the Node 22 pattern) is `ctx` is *populated by the host before the procedure runs* — `ctx.user`, `ctx.signal`, `ctx.requestId`, `ctx.idempotencyKey`, `ctx.waitUntil` — and reaches the procedure through the parameter list, not through a global-style accessor. With native ALS, the platform could give creators *both*: explicit `ctx` argument *and* `getRequestContext()` for transitively-deep code. The proposal picks neither cleanly.

7. **Discovery rule conflicts with shipped code AND with RSC/Server Actions practice.** §1 says: "Path is the only marker. A file is server-only iff it lives at `src/server.{ts,tsx,js,jsx}` or anywhere under `src/server/`. The legacy `"use server"` directive is no longer accepted." But the actual shipped code requires file-level `"use server"` PLUS wrapper imports from `@zeroship/server` (per `rpc-architecture-critique-2026-05-05.md` §High-2 and `sdks/server/src/wrappers.ts`). Two incompatible discovery models exist simultaneously. Worse, RSC and Next.js Server Actions both ship *function-level* `"use server"` with reference-graph detection — strictly more powerful than any path-based scheme — and the proposal locks in the weaker model permanently in §1.

8. **Idempotency design has a hole.** §8 specifies `(app_id, wireId, idempotency_key)` in `zeroship.kv` (Redis-backed in prod). The "same key while the original is still in flight" branch resolves to "blocks on a per-key mutex with a short timeout (default 30 s)". A per-key mutex *across workers* requires a distributed lock — Redis SETNX with timeout, Redlock, or a coordinator. The proposal mentions Redis but does not specify the lock primitive. With one V8 isolate per app and N workers fronted by CHWBL, the second request can land on a *different* worker, where a process-local mutex helps zero. The proposal's silence on this is a future correctness bug.

9. **Versioning section is wrong about how wire-stable proposals actually work.** §13 says: "URL prefix `/_zs/v1/`. Breaking wire changes → bump to `/_zs/v2/`." Good. Then: "Per-procedure versioning via the wireId — declare `todos.add.v2` next to `todos.add`; deprecate the old over a sunset window." That's *application-level* versioning passed off as protocol-level. Stripe/AWS ship procedure versioning via a header (`Stripe-Version: 2024-09-30`) or sticky-per-key URL prefix; both have intentional in-flight semantics. The proposal's `todos.add.v2` is just a different procedure with a string convention — fine for app-author intent, but it means the *gateway* has no notion of "v1 procedure deprecated" and can't surface a warning header, can't aggregate metrics across versions, can't alert on usage of pinned-deprecated. The proposal pretends versioning is a creator concern; in production it's a platform concern.

10. **The proposal does not integrate with `zeroship.meter.*`.** Billing-metering exists (`docs/reference/billing-metering.md`, 25+ metrics). An RPC call should be a meter unit by default. The proposal's §12 lists "Method-level dashboards" but never wires `zeroship.meter.increment("rpc.invocations", { procedure: id })` into the synthetic SSR entry. Every existing competitor (Stripe, Cloudflare, AWS Lambda) makes *every API call* a billable event by construction, with the gateway emitting the meter event so creators don't need to write it. The proposal punts to "the creator console", which means the creator pays for the wiring.

---

## 3. Detailed flaws by severity

### CRITICAL

**Critical-1 — §3 "Ambient context" recommends a worse primitive than what's shipped.**
- **Cite:** §3, line 198: *"Single request per concurrent invocation in V8 isolates → no AsyncLocalStorage gymnastics; the slot is keyed by the kernel's in-flight request id."*
- **What's wrong:** as of ISS-01, `crates/runtime/src/node/async_hooks/als.rs` ships native `AsyncLocalStorage` backed by V8's `ContinuationPreservedEmbedderData`, which propagates *automatically* across every await, microtask, `.then`, generator yield. The proposal's `__zs_bind_request_ctx` / `__zs_get_request_ctx` mechanism (§3 line 168) requires the kernel to repaint `executing_request_id` on every V8 entry (see `crates/runtime/src/auth.rs:55-59`). Per the inline comment in `auth.rs:13-22`, an earlier `thread_local!` revision was wrong; the current per-request-id approach is correct *but more brittle than ALS*. ALS is wired so that every continuation captures its own context with zero kernel cooperation.
- **Why it matters now:** the proposal's helpers (`user()`, `idempotencyKey()`, `traceId()`, `signal()`) bind their lookup site to the moment of invocation (the kernel's `executing_request_id`). If a procedure calls a 3rd-party JS library that schedules a `setTimeout(() => user(), 1000)`, the timer's V8 turn re-enters the runtime; `executing_request_id` may be a different request by then. ALS would solve this by construction.
- **Direction:** rewrite §3 to use `AsyncLocalStorage` as the storage primitive. The platform has it. `ctx_user_storage`, `ctx_signal_storage`, `ctx_request_id_storage` — one ALS per ambient field. The synthetic SSR entry calls `Promise.all([userStore.run(user, () => …), …])` to pre-populate. The host populates ALS in *Rust* before V8 ever sees the procedure (the kernel has direct access to the Map slot via `ContinuationPreservedEmbedderData`). This is a simpler, more correct, and faster design than the request-id slot dance.

**Critical-2 — Wire format pretends to be superjson but the auto-emitted stub doesn't ship `meta`.**
- **Cite:** §4 ("Cross-boundary types"), §6 ("Wire protocol — Mutation"). The wire shape is `{ "json", "meta": {...} }`.
- **What's wrong:** the pre-existing implementation review (`rpc-architecture-critique-2026-05-05.md` Critical-2) flagged that the auto-stub uses `JSON.stringify({ json: input })` — no `meta`. The kernel's fast-path parser reads `body.json` and discards `meta`. Two clients (auto-stub from the vite-plugin and `client<App>()` from `@zeroship/rpc-client`) target the same wire with observably different revival behavior. **The design doc does not say which is canonical.** It mentions "superjson" as the wire transformer but the wire envelope spec at §4 is `{ json, meta }`, not "always superjson". The kernel can't tell which the user meant.
- **Why it matters now:** for any procedure returning a `Date`, the auto-stub path returns an ISO string (no revival). The manual-client path returns a real `Date`. End-to-end types claim `Date`. The TS surface lies for half of users.
- **Direction:** the proposal must specify a single envelope contract. Either (a) wire is *always* `{ json, meta? }` and the kernel always emits `meta` for any value containing a non-JSON-native (Date, BigInt, Map, Set, RegExp, URL, Uint8Array), and the auto-stub uses superjson too; or (b) wire is plain JSON and the type table in §4 is wrong. The proposal currently chooses neither.

**Critical-3 — `RpcError` brand check is structurally underspecified.**
- **Cite:** §6 ("Error redaction"), line 583-602.
- **What's wrong:** the redaction rule is "anything that is not an `RpcError` instance is redacted before it reaches the wire." `instanceof` across V8 contexts is realm-fragile (cross-realm promises, isolate-local class identity). The proposal says nothing about the brand mechanism. With the `#[v8_class]` + `#[v8_state_marker]` toolchain that landed in MAC-01, `RpcError` should be a real native class — same shape as `DOMException` — with brand discrimination by internal-field marker, not by `instanceof`.
- **Why it matters now:** every "throw an Error" in a transitive npm dep (e.g. an undici fetch failure) ships its full message + stack to the wire today (per impl review Critical-3). The brand mechanism is the *single* knob between "credentials leak" and "redacted INTERNAL." The proposal hand-waves this as a class.
- **Direction:** specify `RpcError` as `#[v8_class]` with `name: ByteString = "RpcError"`, `code: ZsErrorCode` (a `WebIdlEnum`), `details: any?`, `retryable: bool`, `exposeMessage: bool`, plus a kernel-side brand check via internal-field tag (the same machinery DOMException uses). The dispatch path's `v8_exception_to_error_value` consults the brand, not `instanceof`.

**Critical-4 — Streaming protocol locks in a deprecated AI-SDK shape.**
- **Cite:** §6 ("Stream — POST + SSE"), line 506-526.
- **What's wrong:** AI SDK 5 (current as of 2026-05) deprecated the line-prefixed Data Stream Protocol that this proposal's wire is built on. The new contract is UI Message Streams gated by `x-vercel-ai-ui-message-stream: v1`, with start/delta/end framing and per-text-block IDs. The proposal is shipping the *prior* protocol — and worse, blessing it as the canonical zeroship streaming wire ("the same wire convention as Vercel `ai-sdk` and the React ecosystem").
- **Why it matters now:** any creator who imports `useChat` from `ai/react` in May 2026 gets the new protocol; the zeroship wire returns the old. The interop the proposal claims doesn't actually exist anymore.
- **Direction:** decouple the streaming wire from a specific SDK version. Pick a content-negotiated transport (HTTP/2 server-push for chunked binary, NDJSON `application/x-ndjson` for line-delimited JSON, SSE only when the client explicitly asks for `text/event-stream`). The AI-SDK adapter becomes a *content-type-aware* response shaper that picks the SDK's current protocol. Don't bake the SDK's wire into the platform's wire.

**Critical-5 — Distributed idempotency lock is unspecified.**
- **Cite:** §8 ("Idempotency"), line 905: *"second request blocks on a per-key mutex with a short timeout (default 30 s)"*.
- **What's wrong:** with N workers behind CHWBL, the same idempotency key can land on different workers. A per-process mutex doesn't help. The proposal says storage is `zeroship.kv` (Redis), which is the right place for the lock primitive, but the proposal doesn't pick one (SETNX-with-TTL? Redlock? Server-side script? blocking BLPOP?).
- **Why it matters now:** Stripe's idempotency model (which §8 cites) uses a server-side row lock on a mutating record, not a Redis lock. The platform is one CHWBL hash collision away from running the same handler twice on a "key reuse during in-flight" path.
- **Direction:** specify the lock primitive. SETNX with TTL=30s + a polling loop with bounded retry is the simplest correct version. Document the failure mode (lock holder dies → 30s wait → second request runs). Document the failure mode of the holder dying *during* the handler (the result is never written, the second request runs after timeout — same as Stripe).

### HIGH

**High-1 — `ctx` is a positional arg, not a primitive.**
- **Cite:** §1, examples in §3.
- **What's wrong:** the proposal models `ctx` as the third arg of `default.rpc(name, input, ctx)`. The runtime's frozen singleton `ctx_obj` (`runtime.rs:1007-1037`) carries `waitUntil` + `passThroughOnException` and nothing else. Then §3 introduces a *parallel* set of helpers (`user()`, `request()`, `idempotencyKey()`) that read from a different mechanism (the request-id slot). Two ambient-context paths, one structural.
- **Why it matters now:** Cloudflare Workers, Deno Deploy, and Vercel Edge all converged on `ctx` *containing* the ambient surface (`ctx.user`, `ctx.signal`, `ctx.idempotencyKey`, `ctx.waitUntil`). The proposal splits them — `ctx.waitUntil` is on the param, `idempotencyKey()` is a global function. This is a worse design.
- **Direction:** unify on one ambient model. With native ALS, the most defensible shape is: `default.rpc(name, input, ctx)` where `ctx` is a *real, request-fresh* object (built by the kernel before the procedure runs, not a frozen singleton) populated with `user`, `signal`, `idempotencyKey`, `requestId`, `waitUntil`, `headers`, `meter`. Helpers like `getRequestContext()` (ALS-backed) are only for transitively-deep code that can't thread `ctx` down. The proposal currently flips this: helpers are first-class, `ctx` is a leftover param.

**High-2 — The reference-graph detection (#168) is not even sketched.**
- **Cite:** §1 ("path is the only marker"), §5 ("transform algorithm").
- **What's wrong:** RSC and Next.js Server Actions ship *function-level* `"use server"` with reference-graph: any function reachable from a client module that has the directive becomes an RPC. This eliminates the file-segregation rule. The proposal explicitly *rejects* this: "A file is either fully server (path matches `src/server.{ts,tsx,js,jsx}` or `src/server/**`) or fully client" (line 450).
- **Why it matters now:** the platform aspires to AI-built apps. AI generates code; the AI doesn't naturally segregate by directory. The function-level directive lets a single file have client utilities + server actions, which is the de-facto pattern in Next.js. Locking out reference-graph permanently is a *strategic* mistake, not just a feature gap.
- **Direction:** the proposal should design for both. File-level discovery is the simpler 80% case; function-level + reference-graph handles the rest. The build can do both passes (per-file directive scan, then a closeBundle reference-graph walk) at modest cost. The proposal's current §5 transform algorithm explicitly forbids it ("Reject other shapes (classes, plain consts, objects) with a build error" — line 384).

**High-3 — FormData/Blob/multipart is a one-paragraph afterthought.**
- **Cite:** §11 H, line 1316-1320.
- **What's wrong:** "superjson encodes `Blob` as base64 (small files OK) or — for >1 MB — the SDK auto-routes via a streaming `multipart/form-data` POST that the gateway recognizes by `Content-Type` and forwards as a chunked body." That's the entire spec. No size threshold rationale. No protocol for multipart fields. No streaming-body protocol on the server side. No way for the client to surface an upload progress event. No way for the procedure to receive a `ReadableStream` for Tee'd processing. The native `FormData` (`#[v8_class]`, live iterables, multipart parser) and `Blob` (`#[v8_class]`) primitives the platform now ships are entirely ignored.
- **Why it matters now:** RSC/Next.js Server Actions' value-prop is `<form action={fn}>` — first-class FormData. tRPC has no FormData story (the proposal copies the omission). The platform aspires to compete with both, but the design doc treats binary as an afterthought.
- **Direction:** dedicate a section. A procedure typed `(form: FormData) => …` receives a real native FormData (parsed by the runtime's existing multipart parser). A procedure typed `(file: File) => …` receives the file as a `File` (not base64). The Content-Type negotiation is done at the gateway. SDKs auto-pick (`fetch` with FormData → multipart; everything else → JSON-superjson).

**High-4 — `ctx.signal` propagation is unspecified.**
- **Cite:** §3 helper table (`signal()` returns AbortSignal, never throws).
- **What's wrong:** the proposal lists `signal()` once and never specifies (a) what causes it to abort (timeout? client disconnect? gateway shutdown? deploy-induced isolate eviction?), (b) whether outgoing fetches the procedure makes inherit the signal automatically, (c) whether DB/storage primitives respect it. The platform now has native `AbortController`/`AbortSignal` (`crates/runtime/src/web/dom/abort_signal.rs`); the procedure-side wiring is missing.
- **Why it matters now:** for streaming procedures, `signal` is the *only* way for the server to detect "client disconnected, stop work". For long-running queries, the gateway-imposed timeout has to be observable. For deploys, the runtime needs to be able to cancel in-flight procedures gracefully.
- **Direction:** specify what aborts trigger `signal`: client disconnect, gateway-imposed timeout, isolate eviction. Specify that all native primitives (`zeroship.db.*`, `zeroship.storage.*`, `fetch`) take `signal` as an option. Specify how the timeout `fn.config.timeout` interacts with the gateway-level deadline (tighter wins).

**High-5 — Manifest emission has a build-time/runtime-state coupling problem.**
- **Cite:** §5 "Manifest emission (closeBundle)", §7 "Wire shape".
- **What's wrong:** wireIds derive from current source on every build (good) but `defineApp({ resources })` declarations layer on (also good) — yet the merge is a build-time artifact. There's no provision for *runtime* policy updates. If a creator wants to lower a rate limit without redeploying, the design has no answer.
- **Why it matters now:** AWS API Gateway, Cloudflare Workers, and Stripe all separate *artifact* (the function/handler) from *config* (the policy). The proposal couples them — every rate-limit edit is a redeploy.
- **Direction:** split the manifest into "artifact manifest" (worker entry, modules, blobs, immutable per build) and "policy manifest" (resources, rate limits, mutable post-deploy). The control plane already pushes route updates every 5s; piggyback policy updates on that channel.

**High-6 — Subscriptions section is sketched, not designed.**
- **Cite:** §6 ("Subscription — WebSocket"), §10 ("Subscriptions"), §15 OQ-4.
- **What's wrong:** §6 specifies a JSON-frame protocol (`{"t":"hello",...}`). §10 says "Auto-reconnect with exponential backoff. Unmount triggers unsubscribe. No manual lifecycle." §15 OQ-4 says "WebSocket upgrades need to land on a worker that holds state for that subscription. CHWBL hashes by app+session. Probably fine; verify end-to-end." That's three different framings: protocol, hooks, and routing. Nothing specifies (a) backpressure (server emits faster than client drains), (b) credit-based flow control, (c) replay on reconnect (can the client resume from where it dropped?), (d) per-subscription resource accounting.
- **Why it matters now:** tRPC subscriptions, gRPC server-streaming, and Convex live queries all converged on credit-based flow control. The proposal doesn't even acknowledge the problem.
- **Direction:** either lift subscriptions to a standalone proposal (this one is mostly about stateless RPCs), or design the framing seriously: backpressure protocol, replay/cursor, server-emit budget, max-in-flight.

**High-7 — Dispatch fast-path doesn't use `#[v8_method(fastcall)]`.**
- **Cite:** §5 "End-to-end runtime call path", §16 phase table.
- **What's wrong:** the kernel's `default.rpc` invocation goes through `v8::Function::call` (`runtime.rs:2826`). For an isolate that's been warmed and serving the same handful of procedures repeatedly, this is the kind of workload the macro's CFunction shim was built for (per `crates/runtime-macros/TODO.md` ROI table). Headers.has just got migrated to fastcall (`headers.rs:689`); the RPC dispatch is *more* hot. The proposal doesn't mention fastcall.
- **Why it matters now:** every RPC call's TurboFan-inlinable opportunity is wasted. At 200K req/s (the platform's target), this matters.
- **Direction:** the synthetic SSR entry's `_zsRpc` should be a candidate for becoming a `#[v8_method(fastcall)]` ABI surface — or at least, the kernel-side dispatch should be specced to use fastcall for the hot path (id lookup → arg unmarshal → handler call).

**High-8 — Auth wiring is described as "session" but the platform's HMAC-signed `ZeroShip-User` is unmentioned.**
- **Cite:** §3 "Ambient context", §6 "Wire protocol — Authorization: Bearer <jwt>", §11 G "auth-aware errors".
- **What's wrong:** the proposal mixes two auth surfaces. §6 says `Authorization: Bearer <jwt>` is the wire credential. The platform actually uses two: (a) Bearer JWT for service-to-service / mobile, (b) `__zs_session` cookie + `ZeroShip-User` HMAC-signed header for browser sessions (per `AGENTS.md` "How they connect" diagram and `crates/gateway/src/proxy.rs:378`). The proposal says `user()` "throws Unauthenticated if no session" — but never specifies which mechanism populates the runtime's user state. Per the impl review (Critical-1), nothing currently does — `set_request_user` is dead code.
- **Why it matters now:** the design doc must be the canonical place where the auth chain is described end-to-end. It is not.
- **Direction:** add a "Auth chain" subsection to §3. Specify both surfaces, the HMAC verification step (which today is a stub), the user-state population step, the ALS-backed lookup. Make `user()` a thin wrapper over native `ctx.user` populated by the gateway/worker chain.

### MEDIUM

**Medium-1 — `kind` inference by name regex is a footgun.**
- **Cite:** §1 "What gets inferred" table — `kind: "query"` if name matches `/^(get|list|find|search|count|read)/`.
- **What's wrong:** This is a stringly-typed convention masquerading as configuration. `getValue()` becomes a query — fine. `getCustomerByEmail()` becomes a query — fine. `searchAndDestroy()` becomes a query — wrong (mutating). `lookupOrCreate()` doesn't match the regex but is mutating. The exception requires the creator to set `kind` explicitly, but most won't think to.
- **Why it matters now:** GET requests are cacheable; the gateway will serve stale results on a `searchAndDestroy()` call because its name said "search". Cache poisoning and idempotency assumptions break.
- **Direction:** make `kind` *required* unless the wrapper supplies it (`query()`, `mutation()`, etc.). Drop name-regex inference entirely. The wrappers carry the type-level kind; the proposal already requires them in §1's "Level 2", and the impl already enforces them (per `wrappers.ts`). Update §1 to acknowledge the wrapper requirement; drop the regex.

**Medium-2 — `idempotent: true` on a query is undefined.**
- **Cite:** §6 "Mutation — POST" (Idempotency-Key required if procedure idempotent: true), §1 inferred-kind table.
- **What's wrong:** queries are idempotent by HTTP convention (GET is safe). Mutations opt in. But the proposal doesn't say what happens if a creator sets `query.config.idempotent = true`. Build error? Silent ignore? The wire only requires the header for mutations.
- **Why it matters now:** type confusion. A creator wraps `getThing` in `mutation(...)` (because the name violates the regex), declares it `idempotent: true`, and now it's a `POST` that requires a header. None of this is in §6.
- **Direction:** make `idempotent` only valid on mutations. Build error otherwise. Document the rule.

**Medium-3 — Per-procedure `timeout` interacts poorly with streaming.**
- **Cite:** §7 "Resource shape" → `timeout: { ms: 5000 }`.
- **What's wrong:** for `kind: "stream"`, "timeout" is ambiguous. Time-to-first-byte? Time-to-completion? Inactivity timeout? The proposal doesn't say. The phrase "the worker handler aborts after 5s" suggests TTC, which would terminate every long-running stream.
- **Why it matters now:** chat streaming, observability streams, all violate this.
- **Direction:** for streams, replace `timeout` with `inactivityTimeout` (no data for N seconds → abort) and `maxLifetime` (hard kill at N seconds, default 30 minutes). The proposal currently has only one knob and uses the wrong semantics for streams.

**Medium-4 — Batching is scoped only to queries, but tRPC and JSON-RPC let mutations batch.**
- **Cite:** §6 "Batching", line 622: *"Only queries batch."*
- **What's wrong:** the rationale ("Mutations and streams stay individual — different consistency / streaming semantics") is misleading. Mutations within a single client tick can absolutely batch — they remain ordered, they don't violate consistency, they just don't share an HTTP request. Stripe, AWS Step Functions, JSON-RPC all let mutations batch. The proposal's "only queries" rule throws away a real perf win.
- **Why it matters now:** an optimistic-update React component might fire `add()` + `markRead()` + `notify()` in a single tick. Today they're three separate POSTs. Batched, they're one.
- **Direction:** allow mutations to batch *unless* one declares `idempotent: true` (idempotency keys per request need separate handling) or one is part of a `transaction` middleware chain. Specify the result-failure semantics explicitly (one fails, others have already run — same as JSON-RPC batch).

**Medium-5 — `ctx.signal` is not propagated to outgoing fetches.**
- **Cite:** §3 helper table.
- **What's wrong:** the proposal lists `signal()` but doesn't say what consumers honor it. Native `fetch` honors `signal` via the AbortController arg; native DB ops should too. The proposal is silent on the propagation chain.
- **Why it matters now:** if the gateway times out at 5s and the procedure is stuck in a 30s `db.query`, the lack of propagation means the worker continues consuming resources after the response was lost.
- **Direction:** mandate that every native zeroship op (db, kv, storage, fetch, meter) accepts a `signal` option and uses it. The default `signal` is the procedure's `ctx.signal`. The proposal should add this to the §3 helper table.

**Medium-6 — `ctx.requestId` and `traceparent` propagation are mentioned once, never specified.**
- **Cite:** §12 "Operability" — "Trace propagation: W3C `traceparent` end-to-end".
- **What's wrong:** the operability table is a single-line claim. No spec on (a) how the request id is generated (UUIDv7? gateway-emitted?), (b) how `traceparent` is created at the gateway and threaded into the procedure, (c) whether outgoing fetches inherit the trace context, (d) whether `traceId()` returns the W3C TraceID or a zeroship-internal id (those are different shapes).
- **Why it matters now:** OTel-instrumented apps will fail to correlate spans across the gateway/worker boundary.
- **Direction:** dedicate a subsection. UUIDv7 for request id (matches `typed_id` invariant). `traceparent` per W3C — gateway creates if absent, propagates. Outgoing fetches inherit via auto-injection. `traceId()` returns the W3C trace-id (32-char hex), not the request-id.

**Medium-7 — Cache-Control / ETag / Last-Modified protocol is one line.**
- **Cite:** §6 "Query — GET" example: `Cache-Control: max-age=30, stale-while-revalidate=60, private`.
- **What's wrong:** ETag generation is unspecified. Conditional GET (If-None-Match) handling is unspecified. Last-Modified is not mentioned. CDN-friendliness (Vary header, public vs private cache, age propagation) is missing.
- **Why it matters now:** Cloudflare/Fastly cache hit rate is a 10x cost lever. The proposal punts.
- **Direction:** specify ETag = sha256(canonical-superjson-of-result)[:16]. Specify If-None-Match → 304. Specify Vary: Authorization. Specify CDN-cacheable = `auth: "anon"` AND `kind: "query"` AND `cache.public: true`.

**Medium-8 — The proposal doesn't define request-size limits independently of `max_input_bytes`.**
- **Cite:** §7 "Resource shape" mentions `max_input_bytes`.
- **What's wrong:** `max_input_bytes` is the only knob. There's no `max_output_bytes` (a procedure can return 1GB), no `max_request_duration` separate from `timeout` (the gateway-imposed wall clock), no `max_concurrent_per_user` (the procedure can be called 10K times in parallel by one user, only rate-limit-throttled). The output-size knob in particular is unspecified.
- **Why it matters now:** memory pressure attacks. Cost overruns.
- **Direction:** add `max_output_bytes`, `max_concurrent_per_user`, `max_concurrent_per_app`. Document defaults.

**Medium-9 — `defineApp` config extraction in §7 is described as a tree merge, but the platform `Function()`-evals the config file (per impl review High-6).**
- **Cite:** §7 "Authoring — defineApp in src/server/config.ts".
- **What's wrong:** the proposal documents the user-facing shape, but doesn't specify how the build extracts it. The impl uses regex-strip-imports + `new Function()` (per `manifest.ts:475-525`), which is a security/correctness bug. The design doc should specify "AST-walk the `defineApp` argument as a literal" (which is what `transform.ts::literalize` does). It doesn't.
- **Why it matters now:** the gap between "what the doc says" and "what the impl does" creates AI-built-app footguns where computed expressions silently fail.
- **Direction:** specify "config must be a literal — no computed expressions, no env-var reads. The build AST-walks it." Either that's the rule and the impl needs to comply, or the rule is "config is evaluated" and the impl needs a sandboxed eval — the proposal must pick one.

**Medium-10 — `meter` integration is missing.**
- **Cite:** §12 "Operability".
- **What's wrong:** `zeroship.meter.*` is a primitive (per AGENTS.md kernel surface). Every RPC call should fire `meter.increment("rpc.invocations", { procedure: id, status })` automatically. The proposal lists "Method-level dashboards" as auto-generated from the manifest but doesn't wire the metering. AWS API Gateway makes this automatic.
- **Why it matters now:** billing accuracy.
- **Direction:** specify that the gateway emits a meter event on every RPC response. The fields: app_id, procedure, status, duration_ms, bytes_in, bytes_out. Include a §"Metering" subsection.

### LOW

**Low-1 — "Single dispatch path" claim is overstated.**
- §5 "End-to-end runtime call path" describes one path; §1 mentions hooks (`useQuery`); §10 ships `__makeProcedure` + `__makeServerProcedure`; §1 mentions the synthetic entry. The actual code (per impl review Medium-4) has *parallel* dispatchers in `dev-bootstrap` and `rpc-registry`. The doc claim is misleading.

**Low-2 — `defineApp.children:` sugar isn't in the wire shape but is in every example.**
- §7 shows `children: { ... }` consistently in the authoring section. The wire shape uses the flat `rpc:todos.add` keys. Readers must mentally expand. Documenting the flatten algorithm explicitly (not just one bullet) would help.

**Low-3 — `defineApp({ rpc: { dev: true } })` is a global override that's hard to reason about.**
- §6 "Error redaction" lists this as a way to disable redaction. But a single global flag means the *staging* env is identically open. A per-environment knob (env var or build-time `--env staging`) would be more conventional.

**Low-4 — The `application/zs-error+json` content-type is documented but the success content-type is "application/json".**
- §6 mentions `Content-Type: application/json` for success. Errors use `application/zs-error+json`. This bifurcation is asymmetric — a typed client must branch on content-type to know whether to parse as a value or an error. Either drop the custom content-type for errors (status code carries the signal) or also use `application/zs-result+json` for success (full bifurcation). Asymmetric is the worst of both.

**Low-5 — `idempotency_key` storage cap (§15 OQ-5) is a problem statement, not a design.**
- "Ship with a hard cap per app (e.g., 1 M live keys); evict oldest on overflow with a log line." That's a sentence. What's the eviction policy? LRU? FIFO? What happens to a request whose key was just evicted (false-cache-miss)?

**Low-6 — Phase table (§16) doesn't list the specific kernel/runtime/macros work the design requires.**
- E.g., "make `RpcError` a `#[v8_class]`" is not a phase. "Wire ALS as the `ctx` backing primitive" is not a phase. The phases are JS-side; the Rust-side dependencies are invisible.

**Low-7 — §17 References list cites tRPC v11 but the design imports v10-style types.**
- tRPC v11 changed the type inference to use a context-builder pattern. The proposal's `client<App>()` shape is closer to v10. References should match the design.

### NITPICK

- **§1 "Level 1" example has type ambiguity:** `function list({ limit = 20, cursor }: { limit?: number; cursor?: string })` — `cursor` is required at the type level (no `?`) but has no default. Cosmetic, but creates a type-checker false negative.
- **§4 "Cost: ~2 KB client lib, 5–10% wire overhead, sub-millisecond per call":** these numbers are unsourced. "Sub-millisecond per call" for a Map/Set serialization at 100KB is questionable.
- **§6 idempotency error example uses `Retry-After: <seconds remaining in TTL>`:** but `Retry-After` is for rate-limiting / 503 / 429, not for `409 Conflict`. RFC 9110 §10.2.3 lists 503 and 3xx as the `Retry-After` clients. Custom usage; flag it or pick a different header.
- **§7 manifest "transformer": "superjson":** the value is a string in the manifest, but no enum is defined. Future "json+brotli" or "msgpack" adds aren't specced.
- **§9 Vercel AI-SDK example:** `useChat({ api: rpc.chat.completion.streamUrl({ model: "claude-opus-4-7" }) })` — `claude-opus-4-7` is the literal model id (per the user's environment); should be a more generic placeholder for a public-facing doc.
- **§10 "TanStack Query feature" table:** "Stale-while-revalidate — Manifest's `cacheable.swr` becomes default `staleTime` per procedure" — the field is `cache.swr`, not `cacheable.swr`. Naming drift.
- **§11 Scenario H "**1 MB** auto-routes":** the threshold is unsourced. Browser URL limit is ~6KB (§6 already cites this); the multipart route is for any non-trivial blob. 1MB is an arbitrary cliff.
- **§12 "Method-level dashboards — Auto-generated in creator console from manifest":** vague. Which metrics? Which time windows?

---

## 4. Stale-vs-current matrix

| Assumption in the proposal | Current platform state | Severity |
| --- | --- | --- |
| §3 line 198: "no AsyncLocalStorage gymnastics" | Native ALS shipped (`crates/runtime/src/node/async_hooks/als.rs`), backed by `ContinuationPreservedEmbedderData`, *propagates automatically* | Critical |
| §3 line 168: "Backed by `__zs_bind_request_ctx` / `__zs_get_request_ctx`" | Those exist but are now the *wrong* primitive — ALS is strictly better | Critical |
| §3 helper table: `request()` "constructs lazily; not free" | `Request` is a native `#[v8_class] #[v8_state_marker]` — accessor surfacing is now O(1) read of a Box, *almost* free | Major |
| §4: "wire is JSON. Plain `JSON.stringify` loses Date, BigInt..." (with superjson as the fix) | True, but the platform has structured cloning via `crates/runtime/src/web/structured_clone.rs` for cross-realm transfer, and FormData/Blob/File natively. The "JSON-only" framing forecloses richer wires | Major |
| §6: "AI-SDK Data Stream Protocol" with `0:`/`2:`/`d:` line prefixes | Deprecated in AI SDK 5; current is UI Message Streams (`x-vercel-ai-ui-message-stream: v1`, start/delta/end) | Critical |
| §6 "Error redaction": `RpcError` instance brand check | `RpcError` is a 9-line stub; brand check requires `#[v8_class]` + internal-field tag like `DOMException` | Critical |
| §1 "Path is the only marker. The legacy `"use server"` directive is no longer accepted" | Impl requires *both* file-level `"use server"` AND wrapper imports; design doc and code disagree | Critical |
| §3 list of helpers includes `idempotencyKey()`, `traceId()` | Not implemented; `ctx_obj` (frozen singleton) carries only `waitUntil` and `passThroughOnException` | Major |
| §7 manifest emits `"transformer": "superjson"` | Vite-plugin emits this string, but the auto-stub uses plain `JSON.stringify` — wire field lies about wire behavior | Critical |
| §11 H "Blob/file upload": "auto-routes via streaming `multipart/form-data`" | Native FormData/Blob/File ship; the SDK doesn't auto-route; design doc never specifies the protocol | Major |
| §11 G "auth-aware errors": "End user's session expires mid-call" | Auth integration with `__zs_session` cookie + `ZeroShip-User` header is half-built; the design doc doesn't specify the interaction | Critical |
| §12: "Trace propagation: W3C `traceparent` end-to-end" | Not implemented anywhere; gateway doesn't emit it; runtime has no `traceId()` source | Major |
| §5 "Synthetic entry assembly" — "The map is built at module top-level via static imports" | Impl walks `Object.keys(_zsUser)` at runtime (not static imports); design and impl differ | Major |
| §5 "End-to-end runtime call path" assumes `default.rpc` is one path | Two parallel dispatchers exist (`dev-bootstrap` + `rpc-registry`); design overstates simplicity | Medium |
| §6 "Subscription — WebSocket" with JSON frames | Gateway has scaffolding (`subscription_affinity_key`, `is_websocket_upgrade`); worker side absent | Major |
| §7 manifest schema "transformer" enum | No enum defined; just string values with no extensibility plan | Low |
| §8 "Server-side dedupe keyed by (app_id, wireId, idempotency_key)... `zeroship.kv` (Redis-backed in prod)" | KV exists, but the distributed lock primitive is unspecified | Critical |
| §13 versioning: "`todos.add.v2` next to `todos.add`" | Procedure-level versioning has no metric/alert/deprecation-warning machinery | Major |
| §15 OQ-1: "Resolved (Zod-direct)" | Impl ships, but with top-level await on optional `zod` import — see impl review High-5 | Medium |

---

## 5. Missed-opportunity matrix

| Opportunity | Status | What to add | Why |
| --- | --- | --- | --- |
| Native `RpcError` class | Today: 9-line JS stub | `#[v8_class] #[v8_state_marker(RpcError)]` with internal-field brand, `ZsErrorCode` `WebIdlEnum`, fastcall `is_zs_error` helper for kernel | Brand check survives realm boundaries; redaction reliable; matches DOMException pattern |
| ALS-backed `ctx` populated by host | Today: §3 says "no ALS"; host doesn't populate ctx | Use ALS for `user`, `signal`, `idempotencyKey`, `requestId`. Kernel writes the slot in Rust before V8 ever sees the procedure | Free across awaits; no `executing_request_id` repaint; matches Workers/Deno pattern |
| Native FormData / Blob marshal | Today: §11 H is one paragraph; superjson base64 | Procedure typed `(form: FormData) => …` receives parsed multipart via Request.formData() (native). Procedure typed `(file: File) => …` receives File. Gateway content-type-routes | First-class `<form action={fn}>` → competitive with Server Actions/Remix |
| Gateway fast path for RPC URLs | Today: gateway forwards everything to worker | Gateway lookup → dispatch → native arg unmarshal → V8 call (skip JS-side dispatch). Use `#[v8_method(fastcall)]` for the V8-side entry | Halves dispatch latency on hot path |
| Reference-graph RPC detection | Today: §1 forbids it ("path is the only marker") | Walk Vite module graph in closeBundle. Imports from `"use server"` modules → RPC stubs. Function-level `"use server"` in mixed files | Strictly more flexible than path-based. Matches RSC/Server Actions |
| `#[v8_method(fastcall)]` on dispatch hot path | Today: not used | Replace `v8::Function::call` with a fastcall-shimmed entry. Headers.has already shows the pattern | TurboFan-inlinable; measurable per-call savings |
| Native ReadableStream as procedure return | Today: AsyncIterator is the only spec | Allow procedure to `return new ReadableStream(...)` directly; runtime maps to chunked HTTP. The platform has it | Bypass the AI-SDK shape entirely for non-LLM streams |
| AbortSignal propagation | Today: §3 lists `signal()`; semantics unspecified | Make `signal` part of `ctx`. Auto-pass to all native ops (db, kv, storage, fetch). Kernel triggers abort on client disconnect / timeout / eviction | Resource hygiene; matches Workers Web Crypto / Deno pattern |
| Native superjson encoder/decoder | Today: superjson is JS-side | Implement superjson at the kernel boundary: parse `{ json, meta }` once in Rust, pass to V8 with revival-instructions inline. Symmetric on encode | Hot-path of Date/BigInt round-trip; eliminates the auto-stub vs manual-client mismatch |
| Native `meter` event on RPC | Today: not wired | Gateway emits `meter.increment("rpc.invocations", {...})` on every response. Fields: app, procedure, status, duration, bytes | Billing accuracy; competitor-parity (AWS, Stripe) |
| `traceparent` end-to-end | Today: §12 mentions, no impl | Gateway creates W3C trace-id if absent. Inject into worker via header. ALS `traceId` slot. Auto-inject into outgoing fetches | OTel correlation works |
| Idempotency key in `ctx` | Today: gateway has the store, runtime has nothing | Add `ctx.idempotencyKey` populated by gateway. Replace stub `idempotencyKey(): never` with a real read | Per impl review High-4 |
| Schema-as-OpenAPI emission | Today: §15 OQ-1 says "out of scope; revisit when there's real demand" | OpenAPI is the path the proposal commits to *if/when* multi-language clients become a need. The schema is already in `fn.config.input` (Zod) | Future path requires `zod-to-openapi`; designing the URL/route shape now (one line in the proposal) saves a future breaking change |

---

## 6. Industry comparison

| Capability | This proposal | RSC / Server Actions | tRPC v11 | Hono RPC | Effect RPC | gRPC-Web |
| --- | --- | --- | --- | --- | --- | --- |
| **Function discovery** | Path-based + wrapper-import (impl); proposal §1 says path-only — they disagree | Function-level `"use server"` + reference-graph (auto from imports) | Manual builder (`t.router({ ... })`) | Manual `app.get/post(...)` registration | Schema-first manual | Manual `.proto` registration |
| **Wire format** | Superjson (advertised), plain JSON in auto-stub (impl). Mismatch | RSC payload (binary, with FormData / File / Blob first-class) | Pluggable transformer (superjson/devalue/...) | Plain JSON | Schema-encoded (Effect Schema) | Protobuf binary or proto-JSON |
| **Type safety** | `client<App>()` with manual `App` type. No `.d.ts` virtual emission | Phantom-type flows from server to client through bundler | End-to-end inference from router | Inferred via `app.get<typeof router>()` | Schema → Type via `Schema.Type<typeof S>` | Codegen from `.proto` |
| **Streaming** | SSE with deprecated AI-SDK Data Stream Protocol | RSC streaming JSX via Suspense | Subscriptions (full bidi WebSocket) | Native Web Streams | Effect-native streams (typed) | Server-streaming via long-poll trailers |
| **Error model** | `RpcError` class (stub) + gRPC-style code enum; redaction unimplemented | `notFound()`, `redirect()`, raw throws | `TRPCError` + code enum; type-flowed | Manual `HTTPException` | Tagged errors via Effect | gRPC status codes |
| **Auth context** | `user()` global throwing `Unauthenticated`; ALS not used | `cookies()` / `headers()` ALS-backed | `ctx` arg from `createContext()`; ALS in v11 | `c.get('user')` from middleware | Effect Layers (DI) | Metadata interceptors |
| **FormData / binary** | Base64 in superjson; "auto-routes for >1MB" (paragraph) | First-class `<form action={fn}>` | Plugin (community) | Native `c.req.formData()` | Schema-encoded | Limited (text-only Web) |
| **Idempotency** | Spec §8; lock primitive unspecified | None native | None native | None native | None native | None native |
| **Subscriptions** | WS with JSON frames; sketched, not designed | Not supported (Server Actions are one-shot) | Full WebSocket support, auto-reconnect, type-safe | Plugin via Hono websocket | Effect streams over any transport | Bidi streaming via long-poll/HTTP/2 |
| **Metering / billing** | "Method-level dashboards" promise; not wired | None native | None native | None native | None native | None native |
| **Schema-first** | Optional Zod via `fn.config.input` | None | Optional Zod | Manual via Hono validators | Required Effect Schema | Required `.proto` |

**Score per row vs. zeroship's proposal:**

| vs. | Discovery | Wire | Types | Streaming | Errors | Auth-ctx | Binary | Idempotency | Subs | Meter |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| RSC/Server Actions | **lose** (locked-out reference-graph) | **lose** (Blob/FormData second-class) | tie (with virtual `.d.ts` emission, would tie) | tie | tie | **lose** (RSC has cookies/headers ALS-backed; this is hand-wavy) | **lose** (massive gap) | **win** (RSC has none) | **win** (RSC has none) | tie (neither) |
| tRPC v11 | tie | tie (when superjson actually works) | **lose** (manual `App` type vs. inferred) | **lose** (tRPC has bidi WS) | tie | tie | tie (tRPC also poor) | **win** | **lose** (tRPC has full subs) | tie |
| Hono RPC | tie | **win** (Hono is plain JSON) | tie | **win** (Hono streams are basic) | **win** | tie | **lose** (Hono has native FormData) | **win** | tie | tie |
| Effect RPC | **win** (zeroship is simpler authoring) | tie | **lose** (Effect Schema is more rigorous) | tie | **lose** (Effect's tagged errors are stronger) | **lose** (Effect Layers > globals) | **win** (Effect doesn't do binary natively) | **win** | tie | tie |
| gRPC-Web | **win** (zeroship is simpler) | **lose** (gRPC-Web is more efficient binary) | tie (codegen) | **win** (gRPC-Web streams are constrained) | **lose** (gRPC has the canonical model) | **win** | **win** (gRPC-Web is text-mostly too) | **win** | **win** | tie |

**Net:** the proposal's value-prop is "tRPC-grade DX for AI-built apps with Server-Actions-style discovery" — but it falls short on both flanks. Worse types than tRPC, worse discovery than Server Actions, worse FormData than Server Actions/Hono, worse subscriptions than tRPC, worse error model than Effect. The platform can do better given the primitives that have shipped.

---

## 7. Recommended overhaul scope

If this proposal is rewritten, here's what stays, what changes, what gets deleted, ordered by how blocking each is for shipping.

### Must rewrite before shipping (blocks Phase 5)

1. **§3 "Ambient context" — replace request-id slot with native ALS.**
   - Drop the "no ALS gymnastics" line. ALS *is* the gymnastics-free version. Repaint §3 around `ctx` as a real per-request object, populated by the host before invocation, with ALS-backed transitively-deep accessors.

2. **§4 / §6 — pick one wire format.**
   - Either superjson `{ json, meta }` is wire-mandatory and the auto-stub must use it, or plain JSON is wire-mandatory and §4 is wrong about Date round-tripping.

3. **§6 "Error redaction" — design `RpcError` as `#[v8_class]`.**
   - Internal-field brand. `code` as `WebIdlEnum`. Gate redaction on the brand, not `instanceof`.

4. **§6 "Stream — POST + SSE" — decouple from the AI-SDK protocol.**
   - Pick a content-negotiated wire (NDJSON for line-delimited, SSE only when `Accept: text/event-stream`, native ReadableStream for chunked binary). The AI-SDK adapter is a content-shaper, not the spine.

5. **§8 "Idempotency" — specify the distributed lock primitive.**
   - Redis SETNX with TTL, or pick a different approach. The current text is unimplementable across N workers.

### Should rewrite (improves correctness & competitive position)

6. **§1 "Authoring" — drop "path is the only marker"; design for function-level `"use server"` with reference-graph.**
   - Path-based becomes the simplification, not the rule. Reference-graph handles mixed files (which AI-generated code tends to produce).

7. **§11 H "file upload" — promote to a §"Binary & FormData" section.**
   - Procedure typed `(form: FormData) => …` receives native FormData. Procedure typed `(req: Request) => …` is a "raw escape hatch" for esoteric uses. No base64 fallback; multipart on the wire when the type calls for it.

8. **§3 / §6 — `ctx` is a real object, populated by the host, with explicit fields.**
   - `ctx.user`, `ctx.signal`, `ctx.idempotencyKey`, `ctx.requestId`, `ctx.headers`, `ctx.waitUntil`. Helpers `user()` etc. are thin wrappers over `getRequestContext()` (ALS-backed). Match Workers/Deno conventions.

9. **§7 — split manifest into "artifact" and "policy" halves.**
   - Artifact (worker + modules + blobs) is immutable per build. Policy (resources, rate limits) is hot-reloadable post-deploy.

10. **§6 / §10 — subscriptions need a dedicated proposal.**
    - Backpressure, replay, credit-based flow control, server-emit budget — all unspecified. This proposal should reference a future "rpc-subscriptions" doc and not pretend to design them in three paragraphs.

### Should add (currently missing)

11. **§"Auth chain" — end-to-end auth specification.**
    - `__zs_session` cookie, `ZeroShip-User` HMAC, gateway verification, worker-side population of `ctx.user`, ALS-backed `user()`. Today the doc and impl disagree.

12. **§"Observability" — `traceparent`, request-id, structured logs, meter events.**
    - W3C trace propagation. UUIDv7 request IDs. Auto-meter on every RPC. Per-procedure metrics in the gateway.

13. **§"Cache" — ETag, If-None-Match, CDN-friendliness rules.**
    - Hash-based ETag. Conditional GET. `Vary: Authorization`. Public vs private cache.

14. **§"Limits" — output size, concurrent calls, deploy-in-flight.**
    - `max_output_bytes`, `max_concurrent_per_user`, in-flight request handling during deploy.

### Can keep largely as-is

- §2 "Wire identity" — generally sound. Production-mode gate is good. Could add OpenAPI route emission.
- §6 "Errors" code enum — the gRPC-inspired enum is correct.
- §7 "Per-field merge rules" — sound. The `override: [...]` marker is good.
- §9 / §10 React Query integration — solid design. Would need to inherit any ctx-shape changes.
- §11 worked scenarios — stay, but H needs a rewrite.

### Should delete

- §13 "Per-procedure versioning via the wireId" — punts a platform concern to creators. Either design real versioning (header-based, deprecation-aware) or remove the line.
- §15 OQ-1 ("Resolved (Zod-direct)") — should not be in "open questions" since it's resolved. Move to body.
- §15 OQ-3 ("Error redaction... Resolved") — same. Either it's design-complete (move to §6) or it's still open.
- §1 "What gets inferred — kind: query if name matches /^(get|list|find|search|count|read)/" — drop this regex. Wrappers carry the kind.

### Open question the proposal should add

- "How does in-flight cancellation work during a deploy that swaps the user bundle?"
- "Can a creator declare a procedure as `auth: 'workload-identity'` (service-to-service, mTLS / signed token)?"
- "Are RPCs same-app-only or can creators expose procedures to other apps' workers?"
- "What's the contract for a procedure that returns a `Response` directly (e.g., for custom headers / status)? §11 doesn't list the case but §6 mentions it."
- "How does the platform price RPC: per call? per ms? per byte? Today billing-metering has the metrics; the wire-up is missing."

---

## Bottom line

The proposal was a strong sketch. It is now a stale sketch. The gap between "what the doc says" and "what the platform can do" has widened by four major shipped pieces (native ALS, native Web fetch primitives, the `#[v8_class]` toolchain, HMAC-signed user). The right next move is a section-by-section pass that asks: *which of these problems is now a primitive?* A surprising number are. The 1,389-line doc could shrink to ~900 if every JS-side scaffolding the proposal designs is replaced with a pointer to "the kernel handles this."

The single biggest design error is §3's request-id slot. Native ALS is strictly better, the platform has it, and the proposal *recommends against using it*. That one paragraph forecloses two of the proposal's hardest sub-problems (auth-context propagation, idempotency-key threading) by using the wrong primitive. Fix that, and three other sections collapse.

The single biggest opportunity is the gateway fast path with `#[v8_method(fastcall)]` dispatch, native FormData ingestion, and ALS-populated `ctx`. Today the proposal hands the dispatch path to JS; the kernel could own it end-to-end at lower latency and higher type-safety. The phase plan in §16 doesn't even mention this work.

The single biggest competitive risk is the function-discovery rule. Server Actions and RSC have *function-level* `"use server"` + reference-graph; the proposal locks in path-based forever. Reverse this in the rewrite.

---

## Files referenced

- `/home/ruiyang/Projects/appbase/docs/proposals/rpc-v2.md` — subject of review
- `/home/ruiyang/Projects/appbase/docs/reviews/rpc-architecture-critique-2026-05-05.md` — pre-existing impl-vs-proposal review (not duplicated; cited at points 2, 3, 6, 7)
- `/home/ruiyang/Projects/appbase/crates/runtime/src/node/async_hooks/als.rs` — native ALS backing for the `ctx` redesign
- `/home/ruiyang/Projects/appbase/crates/runtime/src/auth.rs` — current request-id slot mechanism the proposal §3 rests on
- `/home/ruiyang/Projects/appbase/crates/runtime/src/web/dom/exception.rs` — DOMException pattern for `RpcError` to follow
- `/home/ruiyang/Projects/appbase/crates/runtime/src/web/dom/form_data.rs` — native FormData for the Binary section to use
- `/home/ruiyang/Projects/appbase/crates/runtime/src/web/headers.rs:689` — `#[v8_method(fastcall)]` precedent
- `/home/ruiyang/Projects/appbase/crates/runtime/src/web/fetch/request.rs` — native Request for `request()` helper
- `/home/ruiyang/Projects/appbase/sdks/server/src/index.ts` — `RpcError` stub today
- `/home/ruiyang/Projects/appbase/sdks/rpc-client/src/encoding.ts` — superjson at the SDK layer (vs. plain JSON at the auto-stub)
- `/home/ruiyang/Projects/appbase/crates/runtime/src/core/runtime.rs:2629-2691` — RPC fast path, `parse_envelope_body`
- `/home/ruiyang/Projects/appbase/crates/gateway/src/idempotency.rs` — gateway store; lock primitive missing
