# @zeroship/auth build — loop WORKLOG (FINAL)

`feat/auth-sdk-popup` autonomous pilot loop. **Pilot (user offline 2026-05-29): decided forks myself,
no review gate, commit-only NEVER pushed.** Specs: `2026-05-29-auth-sdk-design.md` + `2026-05-29-relay-email-design.md`.

## BFF RESHAPE (2026-05-30) — IN PROGRESS. NOT pushed.
User-directed security-first pivot to a Backend-For-Frontend model: the browser holds HttpOnly
cookies + an identity projection only — NO power token, NO client-held JWT. Power token stays
server-side; authorization is enforced at the resource server per-operation; internal-first means
grant-gated platform ops (no token minted). Design: `docs/superpowers/specs/2026-05-30-auth-bff-session-redesign.md`
(+ signed-stateless-cookie addendum). Slice plan: R1 → R1b → R1c → R1d → R2 → IdP-prune → R5 → R4.
- **R1 ✓** `3da799ee` browser path → identity + cookie, no power token. `d57c303d` /signout app-binding keyed by app UUID.
- **R1b ✓** `c8de0cc6` signed STATELESS session cookie (new `session_token.rs`, local verify, no per-request store lookup);
  merged `/token`→`/session`; SDK transport migrated to cookie-only.
- **R1c ✓** `62663ce6` removed `/dpop-exchange` + `/jwks` + orphaned wrapper-token machinery (`wrapper_token.rs`,
  Bearer-wrapper arm, DPoP-wrapper fast-path, dead helpers, `WRAPPER_TTL_SECS`). KEPT raw-Hydra Bearer +
  DPoP-introspection arms (non-browser clients). Workflow `wrumlay3b` (critic 94, 0B/0M); I re-verified:
  gateway 355/0 on live PG+Hydra, no dangling refs, no new broken doc links. Renamed `build_state_with_wrapper*`
  → `build_state_with_session*`; annotated superseded Phase-8 plan pointer in `docs/reference/auth.md`.
- **R1d ✓** `80c8b5d6` short-TTL (5s) read-through `RevocationCache` (core) → cookie hot path fully DB-free on a
  hit. Caches family latest `revoked_after` (Option<i64>, negative-cached), judged `> iat` LOCALLY; new core
  `revoked_after_for` (ceil'd epoch), `is_family_revoked_since` delegates; all 3 arms via `family_revocation_decision`;
  same-node bust via `invalidate()` at /signout + per-app BCL; fail-closed preserved. Workflow `wxo62pxo3` (critic 95,
  all 6 security booleans true). Re-verified: gateway 359/0 + core green on live PG+Hydra. **GATEWAY-SIDE RESHAPE DONE.**
- **R2** (next) SDK client rip-out (cacheLocation/InMemoryCache/LocalStorageCache/CacheManager/Web-Worker/navigator.locks/
  getAccessToken — transport already migrated in R1b). BFF invariant: the browser NEVER holds a power token; identity-only.
- **IdP-prune** merge `/consent/{accept,deny}`→`/consent/decision`; remove `/magic/complete` + `/device`; downgrade `/readyz`.
- **R5** console collapse (console = pseudo-app on gateway `/__zs/auth/*`; delete builder bespoke RP).
- **R4** grant-gated platform capabilities (formalize primitive-side grant check).

## STATUS: ✅ FEATURE-COMPLETE + LIVE-E2E-VALIDATED + POLISH + ARCHITECTURAL-REVIEW-HARDENED. NOT pushed.

### Architectural review round (2026-05-30) — DONE, all findings discharged
A careful 6-lens line-by-line review (21 agents + adversarial verification) scored the build 73
"fix-these-first" and found a coherent cluster of CONFIRMED COMPOSITIONAL defects the per-slice
reviews structurally couldn't catch (independently-correct slices that didn't compose):
- 8a910bb6 (Batch A): BLOCKER /__zs/auth/dpop-exchange leaked the global UUID + real email (Phase-8
  minter never reconciled with Slice 4/5) → now projects pws_ + relay alias, fails closed; +
  self-describing subject invariant (both wrapper fast-paths reject UUID-sub wrappers); + revocation
  unified on (client_id, pws_) across all 3 writers + 4 readers (was a dead check — writer keyed pws_,
  readers keyed global UUID); + control grant-revoke & per-app BCL now write the marker; + live email
  re-resolve. M1 (canonicalize UUID in the single derive_pairwise) + M2 (delete dead global denylist).
- 7bc41f4d (Batch B): BLOCKER relay revoke↔re-consent race (the §6 shared-lock "proof" matched no
  real primitive) → closed STRUCTURALLY via EXISTS(control.oauth_grants) in resolve_active_alias
  (grant-absent ⇒ alias inert in both commit orders; §10 race test live); + pairwise_salt its own
  dedicated secret (was fused to the rotatable stash key); + cookie-name overload split
  (__Host-zs_app_anchor); + SDK relay state-filter; + provider threaded as idp_hint; + honest relay
  abuse auto-revoke.
- 126fa5be: the 2 minor residuals (wrapper email fail-closed in no-DB mode; wrapper_token doc drift).
RECONFIRM (adversarial, on the fixed code): privacy / revocation-coherence / relay-race ALL CLOSED
with file:line evidence; final sweep green (build clean; core 170, gateway 306, control 85, auth 158,
SDK 77, runtime auth_plugin 7, + all PG integration suites; tree clean).
**Systemic lesson: per-slice critic passes can't catch cross-slice composition defects — a whole-build
architectural review with adversarial verification was essential and found 2 real privacy/security
blockers that were unit-test-green.**

## STATUS: ✅ FEATURE-COMPLETE + LIVE-E2E-VALIDATED + POLISH ROUND COMPLETE. NOT pushed.
The whole-vision Supabase/Auth0-style in-app-popup auth SDK is built across all 5 subsystems, every
slice critic-reviewed → fixed → verified, and validated against a real Hydra+Postgres+mailpit stack.

### Polish round (2026-05-30) — DONE
- d196adff: DPoP introspection arm now does per-app family-marker revocation (Bearer parity).
- f014d7be: investigated relay revoke-cascade pooling → KEPT the dedicated connection (a shared-conn
  BEGIN/COMMIT would break txn isolation); added a regression test that locks that in.
- 2808f4e8: aligned the spec BroadcastChannel name to the code ('zs:auth').
- bedf4e39: bounded JWKS-refresh fetch timeout + reused client (last §8.7 resilience gap).
- FINAL SWEEP green: build all 8 auth crates clean; core 165, bundle 10, gateway 302, control 85,
  auth 156, runtime auth_plugin 7, @zeroship/auth SDK 71 — 0 failed; working tree clean.
- REMAINING (genuinely optional, NOT done): a wire-level browser/popup e2e (needs the slow Rust
  service image build) + cosmetic clippy doc-lints / too_many_lines (-W warnings, pre-existing-class).
  Nothing functional left. **Awaiting the user: push / PR decision.**

## Commit log (feat/auth-sdk-popup, commit-only — 20 feat/fix commits + docs)
1a 5ee245d7 · 1b-mech 83936a83 · 1c 3d96f7ea · 1d d46ac990 · 1b-pool 740f009f · 1b-anchors 4ea94e3b ·
1b-browser aaebed3d · 2a 77e2d18c · 2b c5afa5ae · oidc_rp-breaker c7f905d7 · 3a b2e087cc · 3b 441c0a01 ·
4 b5af568f · 5a 2cec959d · 5b 344f0108 · 5c 09f46d83 · live-e2e-fixes b1d1a28d · 3c 86cd6496 ·
dpop-introspect-bind cc5a48e8. (+ doc commits; relay sub-spec GO f95c7638.)

## What shipped
- **Per-app OAuth foundation:** gateway `/__zs/auth/{authorize, popup-callback, token, session?mint=1,
  signout, jwks}` (same-origin so no ITP), Bearer arm (wrapper + raw-Hydra, client_id-bound), per-app
  public PKCE clients (control lifecycle on deploy), app_session_anchors + per-node mint single-flight,
  wrapper ed25519 key rotation, per-worker compio-postgres pool, per-thread Hydra client + circuit
  breaker, env.auth.{getUser,requireUser}.
- **`@zeroship/auth` SDK:** ESM `.`/`./client`/`./react`/`./types`; createAuthClient (signInWithOAuth
  popup, exchangeCodeForSession, getSession/getUser/refreshSession/onAuthStateChange/signOut,
  getAccessToken[WithPopup], hasScope/requestScopes), cacheLocation memory(default)|localstorage,
  navigator.locks, breadcrumb-gated checkSession, AuthProvider/useAuth/SignInButton. 71 tests.
- **Declared scopes:** manifest auth.scopes → control registry + per-app Hydra allowlist → consent
  two-namespace classifier (self-grant fix) → token scope claim → WorkerUser.scopes → env.auth; route
  required_scopes → 403 (Anon-short-circuited). SDK hasScope/requestScopes + scope_required mapError.
- **Pairwise:** derive_pairwise; every arm projects pws_ (global UUID never reaches apps);
  app_user_identities mapping.
- **Relay email:** alias mint at consent, inbound webhook (provider-sig gate), forwarding (real inbox
  only in RCPT TO — proven via .formatted()), reply→bounce, loop/rate/suppression guards, revocation
  cascade, email-claim swap (apps see pws_ id + relay alias only).

## Live e2e (b1d1a28d): 43/43 Liquibase changesets apply to a fresh DB; 12/13 DB-gated suites RUN+PASS
live vs real Hydra+Postgres(+mailpit) — auth_token_anchors proves pws_≠UUID / email→alias / single-flight
/ reload LIVE; consent 21, control oauth_grants 13 (revoke→alias cascade), DPoP 3, headless
authorization_code+PKCE dance, relay inbound spoof-gate+revoke→bounce. Found+fixed 3 bugs (plaintext SMTP
transport [major], clap bool flags, oidc_rp_e2e fixture). (oidc_rp_e2e's 1 fail was a fixture coupling, fixed.)

## OPTIONAL backlog (NONE block; for the user to greenlight — all well-specified):
1. JWKS-refresh breaker: core::oidc_verify::JwksCache::refresh still does cyper::Client::new() per fetch,
   no breaker. LOW value — 5-min cached, NOT the brownout hot path (the oidc_rp breaker c7f905d7 already
   covers the token/introspect mint path). In core (shared by gateway+control), so a self-contained
   breaker/timeout there.
2. DPoP-introspection per-app revocation: the DPoP introspection-fallback arm trusts Hydra `active`
   (global revocation) and does not do the per-app family-marker check the Bearer arm does. Rare path
   (opaque DPoP tokens).
3. control auth_pg Arc<Client> → Pool: relay_revoke + the cross-schema cascade open per-call dedicated
   conns. Cleaner with a pool (the gateway already migrated in 1b-pool). Moderate refactor.
4. Wire-level gateway-HTTP e2e: drive /__zs/auth/* over the wire behind a deployed app (needs the slow
   Rust service image build). Handler logic is already covered by the live DB-gated tests.
5. Trivia: BroadcastChannel name 'zs:auth' vs the spec's app-ref-scoped; gateway clippy doc-lints +
   resolve_dpop_user_header too_many_lines (-W warnings, pre-existing-class).

## WHEN THE USER RETURNS: give the full summary; THEY decide push / open a PR / do any optional backlog.
Still commit-only, nothing pushed. (Loop kept alive at a long idle heartbeat; no more heavy work spawned
autonomously — the meaningful build is done.)

## Decisions (in specs): O8 manifest auth.scopes · O7 relay-reply-bounce · S1 client_id binding · S2 F4-B
gateway HMAC pairwise · mint single-flight+Pool · wrapper key rotation · anchor abs=created_at+30d ·
scope:global signout=this-app-all-devices · app_user_identities(app_client_id=oac_, pairwise_sub) ·
email_verified describes the real inbox · 403=scope_required(SDK)/insufficient_scope(WWW-Authenticate).
## LESSON: integration-heavy slices SOLO; parallelize only provably-disjoint (sdks vs gateway, doc vs code).
