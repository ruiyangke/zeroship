# @zeroship/auth build — loop WORKLOG

`feat/auth-sdk-popup` autonomous pilot loop. **Pilot mode (user offline 2026-05-29): decide
forks myself, don't wait for review gate, commit-only NEVER push.** Spec: `2026-05-29-auth-sdk-design.md`
(round-6). Feed each subagent ONLY its slice's spec section. Per-slice: implement (TDD faithful) →
code-critic (security) → code-fixer → I build the FULL affected crate set + run all suites + commit.

## State machine
**Subsystem 1 (foundation):** 1a ✔ · 1b-mech ✔ · 1c ✔ · 1d ✔ · 1b-pool ✔ · 1b-anchors ✔ →
**1b-browser (NEXT, solo)** → **Subsystem 2 (SDK)** → 3 (scopes) → 4 (pairwise, REDUCED) →
5 (relay) → full live e2e.

## Commit log (feat/auth-sdk-popup, commit-only)
- 5ee245d7 1a  env.auth AuthPlugin dual-site
- 83936a83 1b-mech  wrapper_token refactor + RouteEntry oauth wire fields (critic 91)
- 3d96f7ea 1c  gateway Bearer arm (wrapper + raw-Hydra, client_id bind); 72→fixed DPoP-downgrade
                 blocker + per-app family-marker revocation (auth.token_revocations)
- d46ac990 1d  control per-app public PKCE client lifecycle + RouteEntry LEFT JOIN + per-app BCL; 78→fixed
- 740f009f 1b-pool  gateway DB → per-worker compio-postgres pool (sandbox precedent, behavior-preserving)
- 4ea94e3b 1b-anchors  /token + /session?mint=1 + app_session_anchors + mint single-flight; 72→fixed
                 blocker (single-flight leak) + majors (token-log leak, G4 UUID, family-id, CI-skip).
  (doc commits: bdf04426/8c8ee374/20101237/c1744bc3/b059a6bd)

## F4-B PAIRWISE — partially DONE (brought forward in 1b-anchors)
`core::auth::derive_pairwise(salt, global_user_id, sector) = pws_<base62(HMAC-SHA256(salt,uid:sector))>`
EXISTS. The browser wrapper (/token, /session) mints `sub=pws_` (fails closed 503 if sector
unprovisioned). GateState.pairwise_salt derived in main.rs. ⇒ **Slice 4 is REDUCED to:** project
`pws_` at the ZeroShip-User boundary for the OTHER arms that still emit the global UUID — 1c's
raw-Hydra Bearer path, the cookie/sessions path, the DPoP path — for consistency; PLUS the
`auth.app_user_identities` (app_id, global_user_id, pws_, relay_email) mapping table (needed by relay).

## Next: 1b-browser (SOLO, implement→code-critic→fix)
GET /__zs/auth/authorize (build Hydra /oauth2/auth URL: per-app client from route oauth_client_id,
PKCE challenge from query, state, nonce, scopes, prompt; 302; serves popup + silent iframe).
GET /__zs/auth/popup-callback (same-origin HTML relay: postMessage {type:'zs:authorization_response',
response:{code,state}|{error}} to opener/parent, own origin target; BroadcastChannel/localStorage
fallback for COOP). POST /__zs/auth/signout (FIX missing handler: revoke family marker + Hydra revoke,
clear anchor cookie + breadcrumb, optional RP-logout; scope local|global). GET /__zs/auth/jwks (wrapper
current+previous pubkeys) + wire --prev-signing-key-file in main.rs (1c/1b-pool left it deferred w/
TODO). Reuse anchors.rs cookie helpers + auth_token same-origin guard. Faithful tests: authorize URL
shape, popup-callback postMessage envelope + foreign-opener reject, signout revokes+clears.

## Decisions log (pilot rulings — in spec body)
Forks: O8 manifest auth.scopes · O7 relay-reply-bounce · S1 client_id binding (RFC9068, aud fallback)
· S2 F4-B gateway HMAC pairwise. Round-6: mint single-flight+Pool · wrapper key rotation current+prev
· oidc_rp one shared cyper::Client · anchor abs=created_at+30d.

## LESSON: run integration-heavy slices SOLO (no overlapping parallel edits in one worktree); only
parallelize provably-disjoint file sets or use Agent isolation:'worktree'. (1c+1d worked out disjoint
but was risky.)

## Live-infra debt (for the final e2e): full /token→anchor→/session and 1d Hydra-admin provisioning
are gated on a live compose stack (Hydra + migrated Postgres). Bring it up at the END for the real e2e.
