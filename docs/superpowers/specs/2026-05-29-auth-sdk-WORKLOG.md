# @zeroship/auth build — loop WORKLOG

Running log for the `feat/auth-sdk-popup` autonomous pilot loop. **Pilot mode (user offline
2026-05-29): decide forks myself, don't wait for review gate, commit-only NEVER push.** The spec
is `2026-05-29-auth-sdk-design.md` (~2992 lines, code-grounded). For implementation, feed each
subagent ONLY its slice's section (grep the spec), never the whole doc.

## State machine
`seed ✔` → `spec author+harden ✔ (61→62→72)` → `lock forks ✔` →
`convergence harden ✔ (→82, conditional GO)` → `apply blocker+3 majors+slice-split (running)` +
`Slice 1a impl (running)` → 1b-mech → 1c → 1d → 1b-endpoints → 2 → 3 → 4 → 5 → e2e.

## Design score history
61 → 62 → 72 (author+harden) → 81/82 final (convergence, GO). Reviewer verified grounded facts
against the live tree (wrapper_token.rs, router/auth.rs:561, consent.rs, 0002_auth.sql,
oidc_verify.rs, worker/cache.rs:40, sync.rs, lib.rs:116/136). Prior blockers closed.

## Decisions log (pilot rulings)
Fork rulings (from convergence): O8=manifest auth.scopes, O7=relay-reply-bounce,
S1=client_id-claim-binding (spike), S2=F4-B gateway HMAC pairwise. (Locked into the spec body.)

**Round-5 blocker + 3 majors — my rulings (being applied to the spec now):**
- **BLOCKER (mint substrate).** Redesign `/session?mint=1` so NO db connection/lock is held
  across the outbound Hydra refresh. Mechanism: (a) per-node in-process async single-flight
  coalescing concurrent mints for the same `anchor_id` into one Hydra refresh; (b) a short
  per-anchor cached minted token (TTL ≪ wrapper TTL) so rapid repeat mints skip Hydra; (c) rely
  on Hydra's already-configured refresh rotation grace (30s / reuse 3) to tolerate the rare
  cross-node concurrent mint (≤reuse_count valid rotations, no family revoke). ALSO migrate
  gateway `AppState.db` Arc<Client> → compio-postgres **Pool** for normal per-request DB work
  (update sessions::create/validate/revoke call sites). NO `pg_advisory_xact_lock` across HTTP.
- **MAJOR-1 (wrapper key rotation).** Gateway keeps current+previous ed25519 keys; Verifier
  accepts a wrapper signed by EITHER during an overlap ≥ wrapper TTL (10m)+skew; sign with
  current. Rotation = promote current→previous, gen new current. Document procedure; optional
  gateway JWKS exposing both pubkeys.
- **MAJOR-2 (shared breaker'd HTTP client).** oidc_rp holds ONE reused `cyper::Client` built at
  construction; circuit-breaker + bounded-timeout state attaches to it. No `Client::new()`/call.
- **MAJOR-3 (anchor abs cap).** `app_session_anchors.abs_expires_at = created_at + 30d` (anchor's
  own lifetime, set once, not slid). The 720h Hydra family ceiling is enforced solely by Hydra
  `invalid_grant` (gateway treats as anchor-dead → clear anchor+breadcrumb → SDK interactive
  login). Decouples the two; removes stale-cap.
- **Slice 1b SPLIT.** 1b-mech = wrapper_token refactor (pws_ sub, client_id binding, key list) +
  RouteEntry/CompiledRoute wire fields (oauth_client_id, sector_identifier) + route-sync
  population (mechanical, low-risk). 1b-endpoints = the 5 `/__zs/auth/*` endpoints + Liquibase
  changesets (app_session_anchors, token_revocations) + redesigned mint single-flight + Pool.
- **New slice order:** 1a → 1b-mech → 1c → 1d → 1b-endpoints → 2 → 3 → 4 → 5. (1c Bearer arm
  needs 1b-mech wrapper refactor; 1d per-app client must precede 1b-endpoints e2e.)
- **Residual notes to fold in:** confirm wrapper-revocation read-through cache TTL is seconds-scale
  (NOT the dpop_jti default); trace breadcrumb-cleared + 503 client_not_provisioned interaction (§4.3).

## Residual risks to watch during impl
mint-path connection pinning under Hydra brownout (breaker-before-anything fail-fast); gateway
"dumb" invariant deliberately stretched (watch surface growth, auth-sidecar escape hatch);
cross-node revocation latency bounded by cache TTL; faithful e2e needs 1d (per-app client) before
1b-endpoints; breadcrumb/anchor desync + provisioning race edge.

## Timeline
- 2026-05-29: seed `bdf04426`; research done; pilot authority; spec author+harden `w9eoznlnh`
  (61→62→72) committed `8c8ee374`; convergence `wohq6gm3g` → 82 conditional GO.
- 2026-05-29: applying blocker+majors+slice-split to spec; starting Slice 1a (env.auth dual-site).

## Next actions when woken
1. Review + commit the spec fixes (blocker+majors). Review + verify + commit Slice 1a.
2. Proceed down the slice order; each slice: implement (TDD, faithful test) → review (diff+tests)
   → commit. Bigger slices (1b/1c) get a code-reviewer subagent pass too. NEVER push.
