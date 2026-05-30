# @zeroship/auth build — loop WORKLOG

`feat/auth-sdk-popup` autonomous pilot loop. **Pilot mode (user offline 2026-05-29): decide forks
myself, don't wait for review gate, commit-only NEVER push.** Spec: `2026-05-29-auth-sdk-design.md`
(round-6). Feed each subagent ONLY its slice's spec section. Per-slice: implement (TDD faithful) →
code-critic (security) → code-fixer → I build the FULL affected crate set + run all suites + commit.

## State machine
**Subsystem 1 (foundation) — COMPLETE ✔** (1a, 1b-mech, 1c, 1d, 1b-pool, 1b-anchors, 1b-browser) →
**Subsystem 2 (SDK): 2a (NEXT, solo) → 2b** → 3 (scopes) → 4 (pairwise, REDUCED) → 5 (relay) →
oidc_rp-breaker follow-up → full live e2e (compose stack).

## Commit log (feat/auth-sdk-popup, commit-only)
1a 5ee245d7 · 1b-mech 83936a83 · 1c 3d96f7ea · 1d d46ac990 · 1b-pool 740f009f ·
1b-anchors 4ea94e3b · 1b-browser aaebed3d  (doc commits between).
Gateway surface now live: /__zs/auth/{authorize, popup-callback, token, session?mint=1, signout, jwks}
+ Bearer arm + per-app clients + anchors + mint single-flight + wrapper key rotation.

## F4-B PAIRWISE — partial (browser wrapper done). Slice 4 REDUCED to: project pws_ at the
ZeroShip-User boundary for the OTHER arms still emitting the global UUID (1c raw-Hydra Bearer,
cookie/sessions, DPoP) + the auth.app_user_identities mapping table. derive_pairwise() exists in core.

## Next: Subsystem 2 — @zeroship/auth SDK (sdks/auth)
**2a (SOLO): package restructure + HEADLESS CLIENT.** Restructure sdks/auth to subpath exports
(. /client /react /server /types) mirroring sdks/rpc (tsup multi-entry, tsconfig with DOM lib,
.attw.json esm-only, lint:pkg). Implement createAuthClient (Auth0/Supabase parity):
signInWithOAuth({provider?,scopes?,popup?}), exchangeCodeForSession(code), getSession() (local,
cheap), getUser() (server-validated via /session), refreshSession(), onAuthStateChange(SIGNED_IN|
SIGNED_OUT|TOKEN_REFRESHED|USER_UPDATED), signOut({scope}). Session={access_token,refresh_token?,
expires_at,token_type,user,scopes}. cacheLocation memory(default)|localstorage, pluggable ICache +
CacheManager (key clientId::audience::scope, @@user@@ id-token entry), Web Worker refresh-token
isolation (memory+useRefreshTokens), navigator.locks getTokenSilently serialization,
zs.<host>.is.authenticated breadcrumb + early-return checkSession, popup mechanics (sync
window.open FIRST), the /__zs/auth/popup-callback postMessage handler (consume
{type:'zs:authorization_response',response:{code,state}} — validate e.origin===appOrigin),
reload-recovery via GET /__zs/auth/session?mint=1. Talks to the Subsystem-1 gateway endpoints.
Make DOM/fetch INJECTABLE so the cache/lock/session-lifecycle/event logic is faithfully unit-tested
in node (fake window/fetch/storage); set up the test DOM env as needed. ESM-only, externalize
zeroship/@zeroship/*.
**2b (after): React adapter** (./react: AuthProvider, useAuth, SignInButton/SignIn popup launcher;
mount = handleRedirectCallback if code+state else checkSession) **+ server entry** (.: getUser/
requireUser/isLoggedIn/signOut on the now-wired env.auth [1a]; FIX the old broken signOut path).

## Backlog / follow-ups (do before final e2e)
- **oidc_rp shared cyper::Client + circuit-breaker (§8.7 / round-6 major-2):** every oidc_rp outbound
  call (post_token, introspect, exchange_code_public, refresh_token_public, revoke_token_public)
  currently builds a per-call cyper::Client with no breaker. Refactor to ONE reused client + bounded
  timeout + breaker so a Hydra brownout can't exhaust the gateway. Dedicated slice.
- BroadcastChannel name 'zs:auth' vs spec app-ref-scoped — decide + align.
- gateway clippy doc-lints in new browser_auth/oidc_rp code (cosmetic).
- Slice 4 cross-arm pws_ + app_user_identities; Slice 5 relay (separate sub-spec).

## Decisions log (in spec body): O8 manifest auth.scopes · O7 relay-reply-bounce · S1 client_id
binding (RFC9068, aud fallback) · S2 F4-B gateway HMAC pairwise · round-6 mint single-flight+Pool ·
wrapper key rotation current+prev · anchor abs=created_at+30d.

## LESSON: run integration-heavy slices SOLO; only parallelize provably-disjoint file sets or use
isolation:'worktree'. ## Live-infra debt: full /token→anchor→/session + 1d Hydra-admin provisioning
gated on a live compose stack (Hydra + migrated PG) — bring up at the END for the real e2e.
