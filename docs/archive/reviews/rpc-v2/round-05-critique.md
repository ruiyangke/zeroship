# RPC v2 Proposal Critique — Round 5

**Reviewer:** design-critic role (model: opus-4-7)
**Subject:** `docs/proposals/rpc-v2.md` (2,227 LOC, post round-4 reviser)
**Lens:** Did round-4 close the round-3 High and Medium findings? What new gaps did the patches introduce?

**Verdict:** Round-4 closed every round-3 finding cleanly and introduced no critical regressions. Six smaller issues remain — three structural (a hot-path latency cost in the fastcall two-step pattern is unmodeled; the wire-compat algorithm's `breakingOk` audit-log spec is missing; the multipart "schema-driven" rule has an edge case for `z.union`/`z.discriminatedUnion`), two consistency (the §3 `ctx.signal` `AbortSignal.any` claim conflicts with what's actually implementable; `kind: "raw"` keeping `idempotent: true` while opting out of envelope-decode raises caching questions), and one industry-fit issue (RFC 9457 problem-details vs custom `application/zs-error+json`).

---

## 1. Score

| Dimension | Score | One-liner |
| --- | --- | --- |
| Clarity | 87 | Sections are dense but well-organized. The new "How the wire mode is chosen" subsection in §4b is excellent — the priority-ordered list and four worked examples nail it. The new wire-compat algorithm (§13) is unambiguous. The multipart envelope spec is now precise. Two clarity gaps: (a) the kind-vs-raw decision table conflates "use case" with "Pick" — the table of "kind: 'stream' for typed JSON streams" doesn't say which `Accept` is the right default for a typed stream returning an object (NDJSON). The reader has to cross-reference §6 to figure out. (b) The §13 wire-compat algorithm uses pseudocode that doesn't line up with the §16 phase 10 LOC estimate — 450 LOC is for a subset (no enum-narrowing, no recursive widening), unclear which. |
| Soundness | 84 | Three soundness improvements landed: (a) the multipart `_zs.json` envelope is now fully spec'd; (b) the wire-compat check has a real algorithm with classified diff outcomes; (c) the streaming error-mid-stream protocol covers all four wires (NDJSON, SSE, AI SDK 5, octet-stream). Two new soundness issues: (a) the §3 `ctx.signal = AbortSignal.any([clientDisconnect, gatewayDeadline, isolateEviction])` claim — `AbortSignal.any` was added in DOM in 2024 and the runtime has it (`crates/runtime/src/web/dom/abort_signal.rs:80`), but the three input signals all need to be addressable from the worker, not just the gateway. `clientDisconnect` is a gateway-side notion (TCP close, HTTP/2 RST_STREAM); how does it propagate to the worker as an AbortSignal? The proposal says "the runtime constructs `ctx.signal` from the propagated header" implicitly, but the propagation-of-disconnect mechanism isn't specified. (b) The two-step fastcall pattern (`enqueue_json` returns `i32` ticket, then `awaitDispatch(ticket)` returns Promise) introduces a roundtrip — is the JS-side wrapper actually faster than just calling the kernel via a single async call? The "30-100 ns saved per RPC" claim is now suspect because the two-step adds two V8 boundary crossings instead of one. |
| Completeness | 86 | Major round-3 gaps closed (multipart envelope, wire-compat algo, streaming errors, RPC-on-RPC, anonymous idempotency). Remaining gaps: (a) the wire-compat algorithm's `breakingOk` attestation says "recorded in the audit log" but never specifies the audit log's surface; (b) `streamUrl` security warning relies on a `z.string().secret()` Zod refinement marker that isn't in Zod core — needs to be defined or sourced; (c) the `compio-redis` is the lock-primitive lever (per AGENTS.md "zero tokio") but §8's idempotency lock pseudocode is presented as language-agnostic — it should be cited; (d) no spec for `kind: "raw"` + `idempotent: true` cache-key collision when two different requests produce identical bodies but different signatures — the cache stores body+headers, so two requests with the same key but different `X-Signature` headers would collide; (e) the §3 `ctx.signal` `isolateEviction` case is named but the LRU-eviction interaction (worker.cache LRU evicting the isolate while the procedure is running) isn't specified — does eviction wait for in-flight to complete, or abort them? §15 OQ-2 mentions a 30s drain window but doesn't tie back to `ctx.signal`. |
| Feasibility | 86 | Phases now collapse to a clean 12-step plan. Phase 10 (wire-compat) is correctly factored out from phase 3 (manifest emission). Phase numbers are consistent (no more 0-vs-1 confusion). Two feasibility worries: (a) `zod-to-json-schema` is a third-party dep — its support for `z.discriminatedUnion`, `z.lazy`, `z.transform` is incomplete (per its own README); the proposal commits to it without a compatibility matrix; (b) the `compio-redis` Lua-script ownership-checked release is sound for a single Redis instance; for Redis Cluster the keyspace must hash to the same slot (the proposal doesn't say which keys are colocated — `key:lock` and `key:meta` likely need a `{prefix}` hash tag for cluster mode). |
| Industry-fit | 85 | Round-3 score of 82 holds; the round-4 patches don't move the needle here much. Two industry-fit notes: (a) `application/zs-error+json` is a custom media type. RFC 9457 (Problem Details for HTTP APIs, July 2023, replaces RFC 7807) defines `application/problem+json` — adoption in modern API stacks is rising (FastAPI, Hono). The proposal's custom type is fine but a `application/problem+json` compat mode could be a one-page extension. (b) The `Zs-Procedure-Version` header still has no industry comparator; we kept it after round-3 with the 3-version cap. Acceptable but novel. |
| Consistency | 80 | Big improvement: the `_zs.*` reservation now consistently appears in (validation rules in §7, multipart spec in §4b, streaming control frames in §6). The `breakingOk` attestation appears in (§13 wire-compat, §7 validation rules, §16 phase 3). The `kind: "raw"` keeps/opts-out lists are exhaustive. Three remaining inconsistencies: (a) §6 streaming says "stream procedures must declare `kind: "stream"`" but §4b says "a non-stream procedure returning a `ReadableStream` is a build error" — same rule stated twice with slightly different framing; pick one and refer; (b) §3 `ctx.headers` is `Object.freeze`'d but `ctx.url.searchParams.set(...)` "throws"; freezing a URL doesn't freeze its searchParams (URLSearchParams is a separate object) — the spec needs to clarify whether the freeze recurses; (c) §13 says "live-version cap: 3" but §16 phase 11 calls this "live-version cap enforcement" without restating the number. |
| Fit-with-platform | 89 | Stronger than round-3's 88. The fastcall two-step pattern explicitly cites `Headers.has` precedent. The `ctx.headers`/`ctx.url` `Object.freeze` is a clean V8 idiom. The `compio-redis` is implied but should be named. The §3 `AbortSignal.any` reference correctly cites the platform's native implementation. One mismatch: the `idempotent: true` storage path for `kind: "raw"` says "the gateway-side cache stores the entire `Response` body + headers" — but the gateway is HTTP-aware, not envelope-aware; storing Response bodies + headers is straightforward Rust, but the gateway-side idempotency table currently only stores envelope JSON. The phase plan should include "extend gateway idempotency storage to handle raw bodies" as part of phase 5 or phase 7. |
| **Composite** | **85** | **Spec is implementation-ready in most sections; six remaining items are pre-implementation polish.** |

Composite up 7 points (78→85). Approaching the 90 threshold. One more reviser pass should land it.

---

## 2. Round-3 follow-up — what got addressed

| Round-3 finding | Severity | Status |
| --- | --- | --- |
| **High-A** `_zs.json` multipart convention has no spec | High | ✅ **Resolved.** §4b "Multipart envelope spec" now spec'd with present/absent rules, content-types, reserved-prefix collision check. |
| **High-B** "transform sees parameter type" mechanism non-existent | High | ✅ **Resolved.** §4b "How the wire mode is chosen" now uses explicit signals (fn.config.wire, wrapper schema instanceof checks). Four worked examples cover the cases. |
| **High-C** Wire-compat check is one-sentence hand-wave | High | ✅ **Resolved.** §13 "Wire-compat check — algorithm" is now a full pseudocode spec with classified diff outcomes (safe vs. breaking) and a `breakingOk` bypass. |
| **High-D** kind: "raw" overlaps content-negotiated streaming | High | ✅ **Resolved.** §4b "kind: 'raw' opts out of / keeps" tables are explicit; the inactivity/max-lifetime applies to raw streaming responses; the picker table differentiates raw vs. stream. |
| **Medium-A** Phase 0 isolated `RpcError` is unimplementable | Medium | ✅ **Resolved.** §16 merged phase 0+1; the foundation phase delivers RpcError + dispatch + superjson + ALS-ctx as a unit. |
| **Medium-B** ctx.headers "read-only" mutation rule unspec'd | Medium | ✅ **Resolved.** §3 field table specifies `Object.freeze`; setters throw `TypeError`. |
| **Medium-C** Streaming error-mid-stream only NDJSON specified | Medium | ✅ **Resolved.** §6 "Stream control frames" covers NDJSON / SSE / AI SDK 5 / octet-stream-trailers. |
| **Medium-D** auth: "anon" + idempotent: true cross-user leak | Medium | ✅ **Resolved.** §8 "Anonymous mutations + idempotency" requires UUIDv4/v7 (high-entropy). Build warning + runtime gateway 400. |
| **Medium-E** RPC-on-RPC unspec'd | Medium | ✅ **Resolved.** §8 "RPC-on-RPC" specifies direct call (default) vs. forced-wire (rare). |
| **Medium-F** Streaming + traceparent per-chunk unspec'd | Medium | ✅ **Resolved.** §12 "Streaming spans" defaults to one-span-per-stream with span events; opt-in for per-chunk spans. |
| **Medium-G** Zs-Procedure-Version per-procedure granularity novel | Medium | ✅ **Acknowledged + resolved.** §13 "Live-version cap" hard-caps at 3, with platform-level rationale. |
| **Medium-H** Sec-Fetch-Site CSRF rule has corner cases | Medium | ✅ **Resolved.** §14 "CSRF for cookie-auth" makes Origin allowlist primary, Sec-Fetch-Site a strengthening, double-submit a fallback. |
| **Low-A** fastcall ABI sketched not specified | Low | ✅ **Resolved.** §5 now specifies the two-step `enqueue_json` (fastcall, returns i32 ticket) + `awaitDispatch` (slow-path, returns Promise) pattern. |
| **Low-B** streamUrl security note | Low | ✅ **Resolved.** §10 "streamUrl security note" + build warning. |
| **Low-C** `_zs_error` reserved field | Low | ✅ **Resolved.** §7 validation rules + §6 streaming control frames both reserve `_zs.*` / `_zs_*`. |
| **Low-D** subscription auth re-validation 5-min default | Low | ✅ **Resolved.** §14 explicit: `min(session.ttl / 4, 5 minutes)`, justified vs. Cloudflare DOs. |
| **Low-E** ctx.env merge order | Low | ✅ **Resolved.** §3 field table: runtime-process env < `defineApp.env` < secret manager (higher wins). |
| **Low-F** literal-only in dev mode | Low | ✅ **Resolved.** §7c new subsection: dev mode enforces same constraint to avoid prod-only surprise. |
| Nitpick `ctx.user.id` null check | Nit | ✅ **Resolved.** §3 example carries comment + table notes that `auth: "user"` narrows the type. |
| Nitpick Uint8Array vs ArrayBuffer | Nit | ✅ **Resolved.** §4 type table now splits the rows with rationale for the 32 KiB threshold. |
| Nitpick USER_ENTRY resolution | Nit | ✅ **Resolved.** §5 specifies the resolution priority (vite.config → package.json::main → src/index). |
| Nitpick `Vary: Origin` | Nit | ✅ **Resolved.** §6 now `Vary: Authorization, Origin, Accept-Encoding`. |
| Nitpick add_v2 collision-rule carve-out | Nit | ✅ **Resolved.** §2 "Carve-out for procedure versions". |

**Score**: 23 of 23 round-3 findings resolved. **Zero ducked.** The reviser is hitting on every cylinder.

---

## 3. New flaws introduced in round 4 — by severity

### CRITICAL

(none)

### HIGH

(none)

### MEDIUM

**Medium-α — Two-step fastcall pattern's perf claim needs reconsideration.**
- **Cite:** §5 "Gateway dispatch — fastcall hot path"; the new two-step pattern (`enqueue_json` returns ticket; `awaitDispatch(ticket)` returns Promise).
- **What's wrong:** The original claim was 30-100 ns saved per RPC by replacing `v8::Function::call` with `#[v8_method(fastcall)]`. With the two-step pattern, the JS side now pays *two* V8 ↔ Rust crossings per dispatch (enqueue + await). If the procedure body itself is a single `default.rpc` call from the JS wrapper, the wrapper looks like: `const ticket = dispatcher.enqueueJson(id, bytes); return dispatcher.awaitDispatch(ticket);` — the second call goes through the regular V8 call path (it returns a Promise, so it can't be fastcall). The per-call savings is now: one fastcall enqueue (~30-100 ns saved on that call) + one normal-call await (no savings). Net savings: ~30-100 ns *per RPC*, but only if the enqueue side is the bottleneck. Without a microbenchmark, the claim is unproven.
- **Why it matters:** The phase-1 LOC estimate (850) includes "fastcall dispatch entries" as load-bearing performance work. If the actual win is small or negative, the phase plan is misallocating implementation effort.
- **Direction:** Add a "Performance — measured" subsection in §5 with a concrete microbenchmark plan: compare "single async function call" vs. "fastcall enqueue + slow-path await" with a no-op procedure body, reporting per-call ns at 90/p99. Or: drop the two-step pattern and use a single async call (V8::Function::call + JS-side await) — at 200K req/s the per-call overhead is ~5 µs total; saving 30-100 ns is a 0.6-2% win, only meaningful if the rest of the dispatch path is similarly tuned. Document the decision either way.

**Medium-β — `ctx.signal` propagation of client disconnect is unspecified.**
- **Cite:** §3 field table: `ctx.signal = AbortSignal.any([clientDisconnect, gatewayDeadline, isolateEviction])`.
- **What's wrong:** The three input signals are named, but the *plumbing* for "client disconnected" reaching the worker isn't specified. Concretely:
  - The gateway accepts the request, opens an HTTP connection to the worker, and starts the dispatch.
  - Halfway through, the client closes the connection (TCP RST or HTTP/2 RST_STREAM).
  - The gateway sees the close. How does it tell the worker?
- Three options exist: (a) the gateway ungracefully closes the worker connection too (worker observes EOF on its response writer; this is what most reverse proxies do); (b) the gateway sends a side-channel signal (a control message on a separate channel); (c) the gateway holds the worker connection open and sends a "client disconnected" frame. The proposal says "AbortSignal.any" — implying (a) or (b) but not picking, and not specifying how the worker constructs the AbortSignal from the wire signal.
- **Why it matters:** Without this, the worker burns CPU on procedures whose clients have left. For long-running streams this is a real cost.
- **Direction:** Specify the mechanism. Recommendation: (a) — the gateway closes the worker connection on client disconnect; the runtime detects connection-close and aborts `ctx.signal`. Cite the existing infrastructure (the runtime already supports connection-close detection via the request body's stream). Add to §3 a sentence: "`clientDisconnect` is signaled by the gateway closing the worker-side request body stream; the runtime's request stream emits `error` which triggers the `AbortController` backing `ctx.signal`."

**Medium-γ — `breakingOk` audit log surface is unspec'd.**
- **Cite:** §13 "The build emits a warning, not an error, and records the attestation in the audit log so the platform team can audit if needed."
- **What's wrong:** "the audit log" is mentioned without defining the storage, retention, surfaceability, or who-can-read. Audit logs in regulated industries (SOC2, HIPAA-adjacent app deploys) are load-bearing.
- **Why it matters:** The attestation is a deliberate weakening of the wire-compat gate; it must be observable and revocable.
- **Direction:** Specify: `breakingOk` attestations are written to the control plane's `app_deploy_audit` table with `(app_id, deploy_id, procedure_id, version, fields_attested, deployed_by, deployed_at)`. The creator console exposes a "Wire-compat attestations" panel listing recent attestations. The platform team has read access via the control plane API. Retention: 1 year minimum (matches deploy retention).

**Medium-δ — `kind: "raw"` + `idempotent: true` cache-key collision.**
- **Cite:** §4b: "Idempotency (when `idempotent: true` is set; the gateway-side cache stores the entire `Response` body + headers)."
- **What's wrong:** The idempotency dedupe key is `(app_id, wireId, idempotency_key)` (§8). For a `kind: "raw"` procedure, the gateway can't introspect the request body (it's opaque). Two requests with the same `Idempotency-Key` but different bodies should hit case-2 (`ALREADY_EXISTS`). The "different input hash" check is `sha256(canonical-superjson(envelope))` — but raw procedures don't have a superjson envelope. What's the input hash for a raw request?
- **Why it matters:** Without a defined hash, the gateway either (a) skips the input-mismatch check for raw procedures (allows cross-payload key reuse silently — security/safety hole), or (b) hashes the entire raw body (includes timing-sensitive headers, which would defeat dedupe).
- **Direction:** Specify: for `kind: "raw"` + `idempotent: true`, the input hash is `sha256(method || url-with-query || sorted(content-relevant-headers) || body)` where content-relevant headers are `Content-Type`, `Content-Length`, `Content-Encoding`. Headers known to vary per request (`Date`, `traceparent`, `User-Agent`) are excluded. Document this in §8.

### LOW

**Low-α — `compio-redis` not cited in §8.**
- §8 says "Redis SETNX with TTL" but doesn't name the platform's Redis driver. Per AGENTS.md "zero tokio" invariant, this should be `compio-redis::SET key value NX EX 30`. Citation matters for new contributors.

**Low-β — `zod-to-json-schema` compatibility caveats unmentioned.**
- §13 wire-compat algorithm uses `zod-to-json-schema` for canonicalization. That library has known incomplete support for `z.discriminatedUnion`, `z.lazy`, custom `z.refine` predicates, and `z.transform`. The proposal commits to it without a compatibility matrix. Add a one-sentence note: "The wire-compat check uses `zod-to-json-schema` for canonicalization; schemas using `z.discriminatedUnion`, `z.lazy`, or non-trivial `z.transform` may produce incomplete diffs. Such schemas require explicit `breakingOk` attestation when changed."

**Low-γ — Wrapper-schema `z.union` / `z.discriminatedUnion` for File detection.**
- §4b's wire-mode rule: "the schema includes `z.instanceof(File)`, `z.instanceof(Blob)`, OR a `z.object({...})` whose top-level fields include any of those types." But what about `z.union([z.instanceof(File), z.string()])` (file *or* a URL)? The build can't statically pick a single wire — picking multipart breaks the string case; picking JSON base64-encodes the file. The proposal doesn't cover this.
- **Direction:** Build error: "Cannot statically determine wire mode for union of `File` and JSON-native types. Use `wire: 'multipart'` explicitly, or split into two procedures (`uploadByUrl`, `uploadByFile`)."

**Low-δ — Redis Cluster hash-slot colocation for idempotency keys.**
- §8 stores `key:lock` and `key:meta` under separate Redis keys. For Redis Cluster mode (the platform's prod environment per `compio-redis::cluster.rs`), these must hash to the same slot or `WATCH`/`MULTI`/Lua scripts fail. Use a `{prefix}` hash tag: `{idem:<key-hash>}:lock` and `{idem:<key-hash>}:meta`. Add a sentence to §8.

**Low-ε — Stream-vs-raw rule stated twice.**
- §4b says "a non-stream procedure returning a `ReadableStream` is a build error" and §6 has the same rule via "stream procedures must declare `kind: "stream"`". Pick one location and reference from the other.

**Low-ζ — `application/problem+json` compat mode worth mentioning.**
- RFC 9457 (Problem Details for HTTP APIs) is the modern industry default for structured error responses. Hono, FastAPI, Spring Boot, and others use `application/problem+json`. Our `application/zs-error+json` is functionally equivalent. A future-friendly move: support `Accept: application/problem+json` and emit a parallel envelope shape. Out of scope for now, but worth a §15 open question.

### NITPICK

- **§5 fastcall pseudocode `enqueue_json` documentation is verbose** — the comment "Per Headers.has fastcall precedent..." is in the body of the impl block; that's reference material, not API doc. Move to a paragraph above.
- **§13 example `add_v2` export name** — TypeScript-idiomatic would be `addV2` (camelCase). The underscore is a Rust-ism. Cosmetic.
- **§8 "Stripe pattern" claim** — §8 cites Stripe's idempotency model but the lock-holder-dies semantics in the proposal differ slightly (we use Redis SETNX; Stripe uses a server-side row lock). The differences are documented but the "Stripe pattern" framing could be more precise: "Stripe-inspired (Redis-side variant)."

---

## 4. Bottom line

The reviser closed every Round-3 finding and introduced zero criticals. The remaining work is six pre-implementation polish items:

1. (Medium-α) Validate or drop the two-step fastcall perf claim with a microbenchmark plan.
2. (Medium-β) Specify how client-disconnect propagates to `ctx.signal`.
3. (Medium-γ) Define the `breakingOk` audit-log surface.
4. (Medium-δ) Define the input-hash for `kind: "raw"` + `idempotent: true`.
5. Six Lows that are all 1-3 sentence patches.
6. A few nitpicks.

Composite **85**. One more reviser pass — focused on these six items — should land the proposal at ≥90 and ready to implement.

---

## 5. Files referenced

- `/home/ruiyang/Projects/appbase/.worktrees/rpc-v2-revise/docs/proposals/rpc-v2.md` — subject of review (2,227 LOC, post round-4 reviser)
- `/home/ruiyang/Projects/appbase/.worktrees/rpc-v2-revise/docs/reviews/round-03-critique.md` — round 3
- `/home/ruiyang/Projects/appbase/crates/runtime/src/web/dom/abort_signal.rs` — `AbortSignal.any` reference for §3
- `/home/ruiyang/Projects/appbase/crates/compio-redis/src/{client,cluster}.rs` — Redis driver platform invariant
- `/home/ruiyang/Projects/appbase/crates/runtime/src/web/headers.rs:689` — `#[v8_method(fastcall)]` precedent for §5
</content>
</invoke>