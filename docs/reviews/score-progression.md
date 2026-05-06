# RPC v2 Proposal — Score Progression

Each row is one critic round. Reviser rounds happen in between (not scored).

| Round | Composite | Clarity | Soundness | Completeness | Feasibility | Industry-fit | Consistency | Fit-with-platform | Top-3 flaws |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 1 | 49 | 72 | 48 | 40 | 55 | 55 | 45 | 30 | (1) §3 picks request-id slot over native ALS; (2) wire format superjson/JSON mismatch; (3) RpcError not a #[v8_class] |
| 3 | 78 | 80 | 76 | 75 | 78 | 82 | 70 | 88 | (1) `_zs.json` multipart envelope underspec; (2) build-time wire-compat algorithm hand-waved; (3) streaming error-mid-stream protocol only specified for NDJSON |
| 5 | 85 | 87 | 84 | 86 | 86 | 85 | 80 | 89 | (1) two-step fastcall perf claim unmeasured; (2) `ctx.signal` client-disconnect propagation unspec; (3) `breakingOk` audit-log surface unspec |
| 7 | 90 | 91 | 89 | 90 | 89 | 87 | 88 | 92 | (1) gateway double-decode contract unspec; (2) `entered_for_eviction()` not in phase plan; (3) batch auth-fail timing ambiguous |
| 9 | 92 | 93 | 91 | 92 | 92 | 91 | 92 | 94 | (1) `ctx.url.searchParams` freeze recursion (phase-1 decision); (2) per-procedure version cap of 3 is novel; (3) §4b "4b" anchor is non-standard |

## Convergence

Two consecutive rounds at ≥90: round 7 (90) and round 9 (92). Convergence target met after 9 critic rounds (8 reviser rounds in between, 1 already-baseline at start). Total rounds: 9 critics + 4 visible revisers (rounds 2, 4, 6, 8) = 13 ≤ 20-round budget.

Round-over-round deltas: 49 → 78 (+29) → 85 (+7) → 90 (+5) → 92 (+2). Standard diminishing-returns curve.
</content>
</invoke>