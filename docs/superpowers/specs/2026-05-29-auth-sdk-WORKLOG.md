# @zeroship/auth build — loop WORKLOG

Running log for the `feat/auth-sdk-popup` autonomous pilot loop. **Pilot mode (user offline
2026-05-29): decide forks myself, don't wait for review gate, commit-only NEVER push.** Spec:
`2026-05-29-auth-sdk-design.md` (round-6). Feed each subagent ONLY its slice's spec section.

## State machine
spec author+harden ✔ → convergence ✔ (82 GO) → round-6 fixes ✔ →
**Subsystem 1 (foundation):** 1a ✔ · 1b-mech ✔ · 1c ✔ · 1d ✔ → **1b-endpoints (NEXT, solo)** →
**Subsystem 2 (SDK)** → 3 (scopes) → 4 (pairwise) → 5 (relay) → full e2e.

## Commit log (feat/auth-sdk-popup, commit-only)
- 5ee245d7  1a   env.auth AuthPlugin dual-site (5 faithful tests)
- 83936a83  1b-mech  wrapper_token refactor + RouteEntry oauth wire fields (critic 91)
- 3d96f7ea  1c   gateway Bearer arm (wrapper + raw-Hydra, client_id binding); critic 72→fixed
                   blocker (DPoP-downgrade) + per-app family-marker revocation (auth.token_revocations)
- d46ac990  1d   control per-app public PKCE client lifecycle + RouteEntry LEFT JOIN + per-app BCL;
                   critic 78→fixed (delete-leak, apex-clobber). Multi-host attach Slice-N-deferred.
  (+ doc commits bdf04426 / 8c8ee374 / 20101237 / c1744bc3)

## LESSON (parallelization) — IMPORTANT
Ran 1c+1d in parallel in the SAME worktree. They happened to produce DISJOINT file sets (1d
recognized 1c's in-flight files and steered clear), and combined build+tests were green — but it
was a real clobber RISK. RULE GOING FORWARD: run integration-heavy / cross-cutting slices SOLO;
only parallelize slices with provably NON-OVERLAPPING file sets, or use Agent isolation:'worktree'.
1b-endpoints touches the gateway core + DB + the same files as 1c → run it SOLO.

## Next: 1b-endpoints (SOLO, implement→code-critic→fix)
Scope: gateway browser_auth.rs with the 5 same-origin endpoints — GET /__zs/auth/authorize,
GET /__zs/auth/popup-callback (same-origin postMessage relay), POST /__zs/auth/token (CORS, PKCE
code + refresh grant proxy to Hydra; mints browser wrapper sub=pws_ later/Slice4, sets __Host-
zs_app_session anchor), GET /__zs/auth/session?mint=1 (reload-recovery), POST /__zs/auth/signout
(fix the missing handler) + optional /__zs/auth/jwks. Liquibase: auth.app_session_anchors (NOTE:
auth.token_revocations ALREADY added by 1c — do NOT duplicate). Mint = per-node in-process
single-flight keyed on anchor_id + short cached wrapper + Hydra rotation grace (NO db lock/conn
across the Hydra HTTP call). AppState.db Arc<Client>→compio-postgres Pool migration (update
sessions::create/validate/revoke* + anchor read/write call sites). is.authenticated breadcrumb,
same-origin-only CORS (Origin exact match, no credentialed reflection), CSRF via custom header +
state. Wire the previous-key loading (--prev-signing-key-file) for wrapper rotation here (1c left
it deferred with a TODO in main.rs). Faithful tests: N-parallel-mint=1-Hydra-call, foreign-Origin
reject, >30-min idle recovery, popup-callback relay shape, signout revokes. Live PG/Hydra gated.

## Decisions log (pilot rulings — in spec body)
Forks: O8 manifest auth.scopes · O7 relay-reply-bounce · S1 client_id binding (RFC9068, aud
fallback) · S2 F4-B gateway HMAC pairwise. Round-6: mint single-flight+Pool · wrapper key rotation
current+previous · oidc_rp one shared cyper::Client · anchor abs=created_at+30d.

## Review discipline
implement (TDD faithful) → code-critic (security) → code-fixer → I build the FULL affected crate
set + run all suites together + commit per slice. NEVER push.

## Next actions when woken
1. (done) committed 1c 3d96f7ea + 1d d46ac990 after combined-build/test verification.
2. Dispatch 1b-endpoints SOLO. On completion: full gateway+core+control build + all suites, verify
   mint single-flight + Pool + endpoints, commit.
3. Then Subsystem 2 (the @zeroship/auth SDK — restructure sdks/auth to subpath exports).
