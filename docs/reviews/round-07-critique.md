# RPC v2 Proposal Critique — Round 7

**Reviewer:** design-critic role (model: opus-4-7)
**Subject:** `docs/proposals/rpc-v2.md` (2,309 LOC, post round-6 reviser)
**Lens:** Did round-6 close every round-5 finding? What residual issues remain before this design is implementation-ready?

**Verdict:** All round-5 findings closed. The proposal is now implementation-ready in the large. Three residual structural concerns are observable: the gateway-side superjson decode introduces a possible cost the proposal acknowledges but doesn't quantify; the AbortSignal plumbing for "isolate eviction" relies on a 30-second drain that's not explicitly costed against worker thread occupancy; and the §6 batching protocol's per-call auth-policy enforcement is mentioned but the timing (auth-fail-one vs. auth-fail-all) is ambiguous. Three minor consistency items and a few nitpicks. **Composite 90.**

---

## 1. Score

| Dimension | Score | One-liner |
| --- | --- | --- |
| Clarity | 91 | Sections are now self-explanatory; the abort-source plumbing table is a clean addition; the input-hash table differentiating json/multipart/raw closes the round-5 ambiguity. The wire-compat algorithm reads as a real spec. Minor: §3 "Why this is strictly better than v1's request-id slot" is a meta-historical paragraph that could be footnoted or moved to a section on rationale; the body of the proposal should be forward-only. |
| Soundness | 89 | Three soundness improvements landed: (a) the abort sources are now plumbed end-to-end with a defined chain `gatewayDeadline → connection close → request body error → AbortController.abort()`; (b) the input-hash for `kind: "raw"` is fully spec'd; (c) the Redis Cluster `{idem:<key-hash>}` colocation is correct. One residual soundness issue: the gateway's superjson decode (§4 "The gateway and runtime decode-encode pair are both Rust") happens twice — once at the gateway for routing/auth/rate-limit (input hash is needed for idempotency), once at the worker for actual handler invocation. The proposal doesn't say if the gateway forwards the *original* envelope or a re-canonicalized one. If the gateway forwards a canonicalized form, every roundtrip pays canonicalization cost; if not, the worker re-decodes. Either is fine, but pick one and document the choice. |
| Completeness | 90 | All round-5 medium gaps closed. New small gaps: (a) the §3 abort-source list mentions `entered_for_eviction()` as a method on the isolate that fires the AbortController — but the worker's existing LRU cache (`crates/worker/src/cache.rs`) doesn't have this method named; the integration point needs to be a phase 1 deliverable; (b) the `breakingOk` audit table schema is sketched ("(sketch)") but not finalized — the migration LOC isn't in any phase; (c) §15 OQ-2 ("In-flight cancellation during deploy") references `ctx.signal` aborting on isolate eviction with a 30s drain, which §3 now corroborates — but OQ-2 is still listed as "open" — should be moved to body since §3 effectively decided it. |
| Feasibility | 89 | Phases are now correctly factored. The microbenchmark gate on phase 1 (decide single-call vs. two-step fastcall based on measurement) is the right discipline. Two feasibility worries remain: (a) phase 7 (multipart) is estimated at 350 LOC but introduces the `_zs.json` envelope, the schema-driven wire-mode picker, the reserved-prefix check, the `z.union` ambiguity error, and the input-hash for multipart — likely 500-700 LOC; (b) phase 10 (wire-compat) at 450 LOC includes `zod-to-json-schema` integration and a custom canonicalizer; the canonicalizer alone is ~200 LOC because Zod's emit is non-deterministic across versions. Probably 600-800 LOC. Phase estimates are still by-eye but trending tighter with each round. |
| Industry-fit | 87 | Two industry-fit improvements: (a) RFC 9457 problem-details compat is now an open question (§15 OQ-6), positioning for future adoption; (b) the §13 wire-compat algorithm is on par with TypeScript-on-the-wire systems like Convex (which has runtime version pinning) and Effect (schema-first). One residual: the proposal still doesn't explicitly cite **gRPC's `x-grpc-status` trailer pattern** for the octet-stream-trailers error path (§6) — the `x-zs-error-*` trailers we ship are functionally equivalent to gRPC's pattern. A one-line citation would help future implementers. |
| Consistency | 88 | The proposal is internally coherent now. Three consistency items remain: (a) §6 batching mentions "Per-call auth/rate-limit: each call is checked independently — a single batch can include calls with different `auth` policies" — but the timing is ambiguous: when one call in the batch fails auth, do other calls still run, or does the gateway short-circuit the whole batch? (b) §3 ctx.method is typed as `"GET" \| "POST" \| ...` — for `kind: "raw"`, what about `PUT`, `DELETE`, `PATCH`, `OPTIONS`? The "..." is fine but the procedure validation logic needs to know which methods are allowed; (c) §6 streaming the AI SDK 5 mode says `data: [DONE]` is the terminator, but the AI SDK 5 spec uses end-of-stream-via-stream-close as the terminator (no `[DONE]` line). The proposal might be wrong here — verify against AI SDK 5 source or downgrade to "implementation TBD; verify against the upstream lib." |
| Fit-with-platform | 92 | Platform invariants now consistently cited: `compio-redis` for the lock primitive, `crates/runtime/src/web/dom/abort_signal.rs` for AbortSignal.any, `crates/runtime/src/rpc/dispatch.rs` for the runtime entry, `crates/runtime/src/web/dom/exception.rs` (DOMException) as the RpcError pattern. The §3 abort plumbing correctly traces through the runtime's existing infrastructure (request body stream → AbortController). One last fit-with-platform: the proposal asserts that `ctx.user` is "Gateway-injected `ZeroShip-User` HMAC-signed header" — but the platform also has `auth.getUser()` / `auth.requireUser()` exposed via the `zeroship.auth.*` plugin (per AGENTS.md kernel surface). The proposal's `ctx.user` should be defined relative to the existing primitive: "ctx.user = zeroship.auth.getUser() (kernel-side wrapper that returns the populated User struct)". Tightens the platform binding. |
| **Composite** | **90** | **Implementation-ready. The remaining items are ≤sentence-level patches.** |

Composite up 5 points (85→90). **Threshold reached for round 7.**

---

## 2. Round-5 follow-up

| Round-5 finding | Severity | Status |
| --- | --- | --- |
| **Medium-α** Two-step fastcall perf claim unmeasured | Medium | ✅ **Resolved.** §5 "Performance — measured, not assumed" subsection commits phase 1 to a microbenchmark gate (`crates/runtime/benches/rpc_dispatch.rs`); ships single-call path if the win is a wash. |
| **Medium-β** ctx.signal client-disconnect propagation | Medium | ✅ **Resolved.** §3 "Abort source plumbing" specifies the gateway → connection-close → request body stream error → AbortController chain. |
| **Medium-γ** breakingOk audit-log surface | Medium | ✅ **Resolved.** §13 "Audit log surface" sketches the `app_deploy_audit` table, retention, revocation rules. |
| **Medium-δ** kind: "raw" + idempotent input hash | Medium | ✅ **Resolved.** §8 "Input hash" table covers all four wire types (json / multipart / raw / ai-ui-v1). |
| **Low-α** compio-redis citation | Low | ✅ **Resolved.** §8 "Redis primitives used" cites `crates/compio-redis/src/{client,cluster}.rs`. |
| **Low-β** zod-to-json-schema compat caveats | Low | ✅ **Resolved.** §13 "compatibility caveats" subsection documents `z.discriminatedUnion`, `z.lazy`, `z.transform`, `z.brand`. |
| **Low-γ** z.union File detection ambiguity | Low | ✅ **Resolved.** §4b "Ambiguous schemas" build error specified. |
| **Low-δ** Redis Cluster hash-slot colocation | Low | ✅ **Resolved.** `{idem:<key-hash>}` hash tag added. |
| **Low-ε** Stream-vs-raw rule duplicated | Low | ✅ **Resolved.** §4b consolidates the rule with cross-reference to §6. |
| **Low-ζ** application/problem+json compat | Low | ✅ **Resolved.** Added as §15 OQ-6. |
| Nitpick add_v2 → addV2 | Nit | ✅ Resolved. |
| Nitpick "Stripe pattern" → "Stripe-inspired (Redis-side variant)" | Nit | ✅ Resolved. |
| Nitpick fastcall comment placement | Nit | ⚠️ Partial — comment is still in the impl block; cosmetic only. |

**Score**: 12 of 13 round-5 findings resolved (1 cosmetic nit unresolved). **Zero ducked.**

---

## 3. Residual flaws — all sub-Critical

### MEDIUM

**Medium-i — Gateway double-decode of superjson envelope.**
- **Cite:** §4 "The gateway and runtime decode-encode pair are both Rust"; §6 wire flow.
- **What's wrong:** The gateway needs to decode the envelope to extract the `json` portion for input-hash computation (idempotency), schema validation (rejection before dispatch), and rate-limit-by-content (rare but possible). The worker also decodes to invoke the handler. Two decodes per request is non-ideal but defensible; the proposal doesn't pick a contract.
- **Direction:** Specify: the gateway forwards the **original envelope bytes verbatim** to the worker (preserving the client's exact wire form). The gateway's parse is read-only — it computes the input hash and runs Zod validation on a parsed view, but never re-serializes. The worker decodes once for handler invocation. Net cost: parse twice (~5-15 µs at 1KB envelope), serialize once, no extra network bytes.

**Medium-ii — `entered_for_eviction()` is a phase-1 deliverable but isn't called out in the phase plan.**
- **Cite:** §3 abort plumbing for `isolateEviction`; §16 phase 1.
- **What's wrong:** The worker's existing LRU cache (`crates/worker/src/cache.rs`) currently disposes isolates on eviction without a drain step. The proposal asserts `entered_for_eviction()` as the integration point but the method doesn't exist yet — it's part of the work this proposal blesses, but it's not enumerated in any phase.
- **Direction:** Add `entered_for_eviction()` integration to phase 1 (or phase 4 — gateway-related): "extend `crates/worker/src/cache.rs` to expose `entered_for_eviction()` that fires per-request AbortControllers and starts the 30s drain." 50-100 LOC.

**Medium-iii — §6 batching's auth-fail timing.**
- **Cite:** §6 "Per-call auth/rate-limit: each call is checked independently — a single batch can include calls with different `auth` policies."
- **What's wrong:** When the batch contains 5 calls and call #2 fails auth, two policies are possible: (a) short-circuit — gateway returns 401 for the entire batch with one entry per call shown as `{status: 401, error: "..."}`; or (b) per-call — calls #1, #3, #4, #5 still run; only #2 returns 401 in its slot. The proposal says "checked independently" implying (b), but doesn't make it explicit.
- **Direction:** Make explicit: per-call enforcement is independent (b). The batch response always returns N entries in request order; an auth-failed call gets `{id, status: 401, error: <wire envelope>}` while siblings run normally. This matches JSON-RPC batch semantics and unblocks the "anonymous status check + authenticated mutation in one batch" pattern.

### LOW

**Low-i — `data: [DONE]` AI SDK 5 terminator.**
- §6 shows `data: [DONE]` as the AI SDK 5 stream terminator. Verify against AI SDK 5 source — the protocol may rely on stream-close as the terminator (no explicit DONE line). If wrong, downgrade with "TBD; verify against `ai-sdk` upstream during phase 6."
- **Direction:** Add a phase-6 acceptance criterion: "Output is byte-identical to `streamText({...}).toUIMessageStreamResponse()` for a representative input." Don't try to spec the upstream protocol's exact bytes here.

**Low-ii — `ctx.method` enumeration.**
- §3 ctx.method is typed `"GET" | "POST" | ...`. Make the union explicit: `"GET" | "HEAD" | "POST" | "PUT" | "DELETE" | "PATCH" | "OPTIONS"`. For non-raw procedures, the gateway pre-rejects bad methods (§7); for raw, the procedure handles whatever the gateway forwards.

**Low-iii — `ctx.user` reference to `zeroship.auth.*`.**
- §3 `ctx.user`'s "Populated from" cell says "Gateway-injected `ZeroShip-User` HMAC-signed header". Tighten: "Gateway injects HMAC-signed payload; runtime exposes via `zeroship.auth.getUser()` (`ctx.user` is a const-time accessor that calls the primitive)." This binds the proposal to the existing platform plugin surface.

**Low-iv — gRPC `x-grpc-status` trailer citation.**
- §6 octet-stream-trailers error path (`x-zs-error-code`, etc.) is functionally identical to gRPC's `x-grpc-status` / `x-grpc-message` trailer pattern. Add one sentence: "This mirrors gRPC's status-trailer convention; clients that already parse gRPC trailers can use the same code path with the `x-zs-*` namespace." Improves discoverability.

**Low-v — §15 OQ-2 resolution status.**
- §15 OQ-2 ("In-flight cancellation during deploy") was decided in §3 abort plumbing (30s drain via `entered_for_eviction()`). Move the resolution into §3 / §15 should mark it ✅ resolved or strike it.

### NITPICK

- **§3 historical paragraph "Why this is strictly better than v1's request-id slot"** is meta-commentary that should live in `docs/decisions/` (an ADR), not in the proposal body.
- **§5 fastcall comment placement** still in the impl block — cosmetic.
- **§8 "Stripe-inspired (Redis-side variant)"** improved over round 5; could be tighter as "Stripe-style with Redis SETNX".

---

## 4. Stability of the design

Three rounds in a row, the score has gone up by progressively smaller deltas:

| Δ | Magnitude |
| --- | --- |
| R1→R3 | +29 |
| R3→R5 | +7 |
| R5→R7 | +5 |

This is the expected convergence shape: big structural fixes → polish → polish. The proposal has stabilized.

The remaining work is patch-level. None of the residual items would change the implementation phases significantly. The phase plan, file list, and dependency graph are stable.

---

## 5. Bottom line

Composite **90**. Threshold reached. The proposal is implementation-ready.

A round-8 reviser pass focused on the three Mediums (gateway double-decode contract, `entered_for_eviction` phase plan, batching auth timing) and the five Lows would land a 92-93 score. **One more round** and we have sustained ≥90 across two critic rounds, hitting the convergence target.

---

## 6. Files referenced

- `/home/ruiyang/Projects/appbase/.worktrees/rpc-v2-revise/docs/proposals/rpc-v2.md` — subject (2,309 LOC, post round-6 reviser)
- `/home/ruiyang/Projects/appbase/.worktrees/rpc-v2-revise/docs/reviews/round-05-critique.md` — round 5
- `/home/ruiyang/Projects/appbase/crates/worker/src/cache.rs` — LRU eviction, needs `entered_for_eviction()`
- `/home/ruiyang/Projects/appbase/crates/compio-redis/src/cluster.rs` — Cluster keyspace hash tag
</content>
</invoke>