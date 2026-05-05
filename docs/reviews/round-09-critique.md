# RPC v2 Proposal Critique — Round 9

**Reviewer:** design-critic role (model: opus-4-7)
**Subject:** `docs/proposals/rpc-v2.md` (2,314 LOC, post round-8 reviser)
**Lens:** Did round-8 close all round-7 findings? Is the design implementation-ready end-to-end? What stays open as a known acceptable trade-off vs. a real gap?

**Verdict:** All round-7 Mediums and Lows are closed. The design is now a complete, internally-consistent specification ready for implementation. Three minor items remain and are acceptable as-is for proposal-acceptance: the `ctx.url.searchParams` freeze recursion question is documented as platform-runtime detail; the Zs-Procedure-Version novelty is documented and capped; and the AI SDK 5 stream byte-format has a phase-6 verification gate. **Composite 92.**

---

## 1. Score

| Dimension | Score | One-liner |
| --- | --- | --- |
| Clarity | 93 | Sections read top-to-bottom as a spec, not a sketch. The new "Gateway / worker decode contract" subsection in §4 nails the round-7 Medium-i. The §3 abort plumbing is now unambiguous. The phase plan is actionable end-to-end. One residual: the proposal still mixes "round-NN" reviewer references in the body (e.g., §7 "round-01 Medium-2"); these are useful for the reviewer but distract from the spec voice. Future-state would be to fold these into a single rationale appendix. |
| Soundness | 91 | The end-to-end correctness story is now coherent: ALS-backed ctx, single superjson wire, gateway forwards bytes verbatim, distributed lock with cluster-safe hash tags, abort propagation through three named sources, content-negotiated streaming with consistent error frames across all 4 wires, RFC-7230-style `Vary` + ETag for cache. One residual soundness item: §3 says `ctx.url` is `Object.freeze`'d but `URL.searchParams` is a separate `URLSearchParams` object that the freeze doesn't recurse into automatically. The proposal needs to either freeze the whole tree (recursion is platform-runtime detail) or accept that `ctx.url.searchParams.append('...')` is a bug-source. Down-classified: the platform's Headers/URL freeze idiom is implementation-level; spec is OK to defer. |
| Completeness | 92 | All round-7 gaps closed. Phase 1 now lists the LRU `entered_for_eviction()` extension and the microbenchmark gate. Batch auth-fail timing is explicit. OQ-2 is moved to body (resolved). The §4 decode contract eliminates the gateway double-decode question. Remaining items are forward-pointing rather than gaps: the wire-compat algorithm's actual safe/breaking classification table for narrowed/widened types is full but listing a known-incomplete `zod-to-json-schema` will require iteration as schemas grow more complex. Acceptable. |
| Feasibility | 92 | Phase plan is now stable: 12 phases, dependency-DAG'd, with native (Rust) phases first, vite-plugin (TS) following, control-plane (gateway) gating the public wire, and feature phases (idempotency, streaming, multipart) layered on after the core. Phase 1 LOC went from 850 → 950 with the eviction extension and microbenchmark, which is the right trade. One residual feasibility note: phase 10 (wire-compat) is still an estimate and the canonicalizer is an unknown — may grow LOC. Acceptable as a phase 10 mid-implementation decision. |
| Industry-fit | 91 | Strong industry alignment: RSC discovery (file + function `"use server"` + reference graph), AI SDK 5 stream format, Stripe-inspired idempotency with Redis SETNX, gRPC trailer convention for octet-stream errors, W3C Trace Context, RFC 9457 problem-details as a future compat option. The proposal now cites the comparators where appropriate. One thoughtful trade-off: per-procedure version routing (§13) is novel — capped at 3 live versions. Acceptable as a deliberate platform choice with documented rationale. |
| Consistency | 92 | The proposal is now internally coherent. `_zs.*` reservation appears consistently (validation rules, multipart spec, streaming control frames). The wire mode picker is referenced from one source (§4b "How the wire mode is chosen"). The `kind: "raw"` opts-out / opts-in tables enumerate everything. Phase numbers consistent; cross-references (§N → §M) all resolve. Three small residual items remain: (a) the section "## 4b" anchor is unusual (`4b` instead of plain `5`); breaks heading hierarchy slightly (the rest are bare integers). (b) The proposal still has a paragraph in §3 ("Why this is strictly better than v1's request-id slot") that's pure rationale; cosmetic only. (c) `ctx.url.searchParams` freeze recursion as noted in soundness. |
| Fit-with-platform | 94 | This is the strongest dimension and has been all along since round-3. Every claim is now bound to specific platform paths: `crates/runtime/src/web/dom/abort_signal.rs` for `AbortSignal.any`, `crates/runtime/src/web/dom/exception.rs` for the DOMException pattern, `crates/runtime/src/node/async_hooks/als.rs` for ALS, `crates/compio-redis/src/cluster.rs` for Redis Cluster, `crates/worker/src/cache.rs` for the eviction extension, `crates/runtime/benches/rpc_dispatch.rs` as the benchmark home. The platform invariants (zero tokio, V8-per-thread, typed_id, HMAC-signed ZeroShip-User) are honored by every section that touches them. |
| **Composite** | **92** | **Implementation-ready. Sustained ≥90 across two consecutive critic rounds — convergence target met.** |

---

## 2. Round-7 follow-up

| Round-7 finding | Severity | Status |
| --- | --- | --- |
| **Medium-i** Gateway double-decode of superjson | Medium | ✅ **Resolved.** §4 "Gateway / worker decode contract" specifies forward-bytes-verbatim; gateway parses read-only. |
| **Medium-ii** entered_for_eviction not in phase plan | Medium | ✅ **Resolved.** Phase 1 now lists "Extend `crates/worker/src/cache.rs` with `entered_for_eviction()`". |
| **Medium-iii** Batching auth-fail timing | Medium | ✅ **Resolved.** §6 batching is explicit: per-call enforcement; failed call gets `{status: 401, error}` in its slot; siblings continue. |
| **Low-i** AI SDK 5 [DONE] terminator | Low | ✅ **Resolved.** Phase 6 has acceptance gate: byte-identical to upstream. |
| **Low-ii** ctx.method enumeration | Low | ✅ **Resolved.** Now `"GET" \| "HEAD" \| "POST" \| "PUT" \| "DELETE" \| "PATCH" \| "OPTIONS"`. |
| **Low-iii** ctx.user → zeroship.auth.* | Low | ✅ **Resolved.** Field table cell now references `zeroship.auth.getUser()` as the underlying primitive. |
| **Low-iv** gRPC trailer citation | Low | ✅ **Resolved.** §6 octet-stream now cites gRPC's grpc-status/message trailer convention. |
| **Low-v** OQ-2 resolution | Low | ✅ **Resolved.** §15 OQ-2 marked resolved; full mechanism described inline. |
| Nitpick fastcall comment placement | Nit | ⚠️ Cosmetic; left as-is (in-impl comment is acceptable). |
| Nitpick "Stripe pattern" framing | Nit | ✅ Resolved as "Stripe-inspired (Redis-side variant)". |
| Nitpick §3 historical paragraph | Nit | ⚠️ Left as-is (rationale embedded in proposal body; acceptable). |

**Score**: 8 of 11 round-7 findings resolved (3 cosmetic nits acceptable as-is). **Zero ducked.**

---

## 3. Residual items — all acceptable as-is

### MINOR (acceptable as platform-runtime detail)

**Minor-i — `ctx.url.searchParams` freeze recursion.**
- **Cite:** §3 field table — `ctx.url` is `Object.freeze`'d.
- **What's wrong:** Native `URL` in V8 has a `searchParams` property that returns a separate `URLSearchParams` object. `Object.freeze(url)` freezes `url` but doesn't recursively freeze `url.searchParams`, so `ctx.url.searchParams.set('foo', 'bar')` could mutate the underlying parameter store unless the runtime separately freezes the searchParams or wraps it.
- **Why it matters:** Subtle gotcha for procedure authors; if uncaught, "read-only ctx" is a polite lie.
- **Direction:** This is platform-runtime detail. The proposal correctly delegates to "the runtime emits an `Object.freeze`'d wrapper" — phase 1's implementation will need to either (a) freeze searchParams as part of the URL wrapper construction, or (b) wrap searchParams in a separate `Proxy` that throws on setters. Either is fine; this is a 5-line decision at implementation time. **Acceptable to defer to phase 1.**

**Minor-ii — Per-procedure version cap of 3 is novel.**
- **Cite:** §13 "Live-version cap".
- **What's wrong:** No industry comparator ships per-procedure version routing — Stripe (global), GitHub (global), AWS (URL prefix). Our 3-version cap is justified but operationally untested.
- **Direction:** Acceptable. The cap is conservative (3 is what most teams will actually need). If the cap proves too low at scale, the platform team can lift it. Documented decision.

**Minor-iii — `ctx.method` for raw procedures.**
- **Cite:** §3 field table; §4b raw escape-hatch.
- **What's wrong:** A `kind: "raw"` procedure can receive any of the 7 methods. The gateway forwards them verbatim. A raw procedure's logic must dispatch on `ctx.method` if it cares. The proposal doesn't show a worked example.
- **Direction:** Acceptable as documented. The §11 worked scenarios include scenario I (webhook handler) which is a `kind: "raw"` example. Could add a method-dispatch line to that example for clarity, but not load-bearing.

### NITPICK

- **§4b heading "## 4b. Binary, FormData, File"** — non-standard sub-section number. Could renumber as a true §5 with the rest shifting, but this would invalidate all cross-references. Acceptable.
- **§3 "Why this is strictly better than v1's request-id slot"** — rationale could move to an ADR but its presence in the spec helps reviewers understand the design choice. Acceptable.
- **`(round-01 Medium-N)` references throughout** — useful for the review process, slight noise in the final spec. Could be folded into an "Evolution" appendix in a later cleanup pass. Acceptable.

---

## 4. Convergence verdict

| Round | Composite |
| --- | --- |
| 1 | 49 |
| 3 | 78 |
| 5 | 85 |
| 7 | 90 |
| **9** | **92** |

Two consecutive rounds at ≥90: round 7 (composite 90) and round 9 (composite 92). The convergence target stipulated by the orchestrator is met.

The proposal has stabilized. Round-over-round score deltas: +29 → +7 → +5 → +2. Diminishing returns as expected. Round 11 (if run) would likely produce 93-94 with patches to the three Minors above; not blocking.

---

## 5. Bottom line

**Composite 92. Convergence achieved.** The RPC v2 proposal is implementation-ready.

What the proposal accomplishes:
- Round-1's three damning findings (ALS, single wire, native RpcError) are resolved at the foundation level.
- Eight major sections were added or rewritten: §1 RSC-aligned discovery; §3 ALS-backed ctx with full abort plumbing; §4 single-wire superjson with gateway/worker decode contract; §4b first-class FormData/File with multipart envelope spec; §6 content-negotiated streaming with consistent error frames; §8 distributed idempotency lock with Cluster-safe Redis colocation; §13 platform-aware procedure versioning with wire-compat check algorithm; §14 end-to-end auth chain; §12 metering/observability with W3C Trace Context.
- Every section binds to specific platform paths (`crates/runtime/src/...`, `crates/compio-redis/src/...`, `crates/worker/src/...`).
- 12-phase implementation plan with dependency DAG, microbenchmark gate, and acceptance criteria.

What's deliberately out of scope (acceptable for v1):
- Subscriptions full contract (deferred to `rpc-subscriptions.md`).
- Multi-language typed clients (deferred to OpenAPI emission).
- Cross-app RPC (deferred).
- RFC 9457 problem-details compat (acknowledged in §15 OQ-6).

What stays open as known acceptable trade-offs:
- `ctx.url.searchParams` freeze recursion (phase 1 decision).
- Per-procedure version cap of 3 (deliberate platform choice).
- `Zs-Procedure-Version` header is novel (justified, capped, monitored).

The proposal is ready to ship to implementation.

---

## 6. Files referenced

- `/home/ruiyang/Projects/appbase/.worktrees/rpc-v2-revise/docs/proposals/rpc-v2.md` — subject (2,314 LOC, post round-8 reviser)
- `/home/ruiyang/Projects/appbase/.worktrees/rpc-v2-revise/docs/reviews/round-07-critique.md` — round 7
- `/home/ruiyang/Projects/appbase/.worktrees/rpc-v2-revise/docs/reviews/score-progression.md` — score history
</content>
</invoke>