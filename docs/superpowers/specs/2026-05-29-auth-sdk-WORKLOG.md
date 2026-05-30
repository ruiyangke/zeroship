# @zeroship/auth build — loop WORKLOG

Running log for the `feat/auth-sdk-popup` autonomous pilot loop. **Pilot mode (user offline
2026-05-29): decide forks myself, don't wait for review gate, commit-only NEVER push.** The spec
is `2026-05-29-auth-sdk-design.md` (round-6, code-grounded). For implementation, feed each
subagent ONLY its slice's section (grep the spec), never the whole doc.

## State machine
spec author+harden ✔ (61→62→72) → convergence ✔ (→82 GO) → round-6 blocker+majors+slice-split ✔ →
**Slice 1a ✔ (committed 5ee245d7)** → `1b-mech (next/running)` → 1c → 1d → 1b-endpoints → 2 → 3 → 4 → 5 → e2e.

## Design score history
61→62→72 (author+harden) → 81/82 (convergence GO) → round-6 fixes applied (blocker + 3 majors +
slice split). Reviewer verified grounded facts against the live tree. Spec is solid.

## Slice order (post round-6)
1a → **1b-mech** → 1c → 1d → 1b-endpoints → 2 (SDK) → 3 (scopes) → 4 (pairwise) → 5 (relay).
- **1b-mech** (mechanical, low-risk): wrapper_token.rs Issuer/Verifier refactor (pws_-capable sub,
  client_id binding, current+previous key list / kid) + RouteEntry/CompiledRoute wire fields
  (oauth_client_id, sector_identifier, Option) + route-sync population. WIRE BREAK → update ALL
  producers/consumers/fixtures in one patch. NO endpoints, NO DB, NO pairwise derivation (Slice 4),
  NO Bearer arm (1c).
- 1c: gateway Bearer arm (wrapper + raw-Hydra, issuer discriminator, client_id binding) + S1 live-Hydra spike.
- 1d: control per-app public PKCE client lifecycle + redirect-URI reconciliation + per-app BCL.
- 1b-endpoints: 5 `/__zs/auth/*` endpoints + Liquibase (app_session_anchors, token_revocations) +
  mint single-flight + AppState.db→Pool migration.

## Decisions log (pilot rulings — all now in the spec body)
Forks: O8=manifest auth.scopes, O7=relay-reply-bounce, S1=client_id binding (spike), S2=F4-B gateway HMAC pairwise.
Round-6: BLOCKER mint = per-node single-flight + cached wrapper + Hydra rotation grace + AppState.db→Pool
(no lock across HTTP). MAJOR-1 wrapper ed25519 current+previous key (accept either by kid, overlap ≥ TTL+skew).
MAJOR-2 oidc_rp one reused cyper::Client + breaker. MAJOR-3 anchor abs=created_at+30d set once (family ceiling
via Hydra invalid_grant). Residual: wrapper-revocation cache TTL seconds-scale; breadcrumb×503 → RECOVERING +
client_not_provisioned (503 retryable keeps breadcrumb, only 401 clears).

## Review discipline (per slice)
implement (TDD, faithful test exercising the REAL path) → I review diff + RE-RUN tests myself →
bigger/riskier slices (1b-mech wire+crypto, 1c Bearer, 1b-endpoints) ALSO get a code-reviewer subagent
pass → fix → commit per slice. NEVER push.

## Timeline
- 2026-05-29: seed bdf04426; research; pilot authority; author+harden w9eoznlnh (61→62→72) `8c8ee374`;
  convergence wohq6gm3g →82 GO `20101237`.
- 2026-05-29: Slice 1a (env.auth AuthPlugin dual-site) impl+verified+committed `5ee245d7` (5 faithful tests).
- 2026-05-29: spec-fix reviser a834 → round-6 (all 6 rulings landed; spot-checked OK). Committing; starting 1b-mech.

## Next actions when woken
1. (done this iter) commit round-6 spec; launch 1b-mech.
2. On 1b-mech impl done: code-reviewer pass → fix → I build+test (cargo build -p zeroship-core/gateway/control/worker;
   wrapper_token unit tests; RouteEntry round-trip) → commit.
3. Continue 1c → 1d → 1b-endpoints → SDK → scopes → pairwise → relay. NEVER push.
