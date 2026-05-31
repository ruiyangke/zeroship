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
- **R2 ✓** `41803822` SDK client rip-out: DELETED cache.ts (token store)/worker.ts (silent-refresh)/locks.ts; removed
  getAccessToken[WithPopup] + Session.{access_token,refresh_token,token_type} + cacheLocation/useRefreshTokens. Session
  is identity-only {user,expires_at,scopes}; refreshSession re-mints the COOKIE (?mint=1, no token); TOKEN_REFRESHED→
  SESSION_REFRESHED. New tests/identity.test.ts BFF-invariant regression suite. Workflow `wg17q1irs` (critic 95, all 8
  booleans true). Re-verified: tsc clean, 73/73 tests, tsup ESM+DTS build; grep confirms no token surface. **BFF invariant
  holds end-to-end (gateway + SDK).**
- **IdP-prune** merge `/consent/{accept,deny}`→`/consent/decision`; remove `/magic/complete` + `/device`; downgrade `/readyz`.
- **R5 (RESHAPING)** console = a REGULAR app on the standard dev+prod runtime (owner directive 2026-05-30, not just a
  pseudo-app). Requires R4 (grant-gated platform capabilities) as PREREQUISITE — the console needs creator/control-plane
  powers a sandboxed worker-app lacks; server-side privileged grant, never browser. Analysis workflow `wci5rpjec` running
  (verdict + phased path pending). Bootstrap = install-time platform-app seed deploy. Heavy build/AI-gen likely stays a
  called backend service (worker capability envelope).
- **R4** grant-gated platform capabilities — now a PREREQUISITE of R5 (was a follow-on). Formalize the server-side
  privileged-capability mechanism (likely @zeroship/control SDK call from the worker with a scoped creator credential,
  not a broad native primitive).

### Console-as-regular-app — APPROVED 2026-05-30. Build: R4 → kernel convergence → R5. Commit-only.
Design doc: `docs/superpowers/specs/2026-05-30-console-as-regular-app-design.md` (formalizes analysis workflow `wci5rpjec`).
Verdict: console = REAL regular app on the standard dev+prod runtime, FIRST-PARTY PRIVILEGE TIER. Surprise: the builder is
ALREADY ZS-standard authored (server.ts discovered, uses @zeroship/kv+ui, has a deploy script) — a cutover, not a port.
Privilege via R4: server-side `@zeroship/auth/server getAccessToken({audience:control,scopes})` → worker→control
`POST /internal/power-token` (control_key Rust-side, never JS) → control re-derives user from signed ZeroShip-User header +
grant-ceiling via AuthzGuard. Control bearer NEVER reaches the browser. SECURITY INVARIANT: an ordinary creator app cannot
mint an aud=control token. **LOCKED DECISIONS:** (1) SSE-over-fetch sufficient — gateway WS-subscription 501 is NOT a console
blocker; (2) SHARED worker pool, NO dedicated trusted tier — safety rests on no-ambient-authority (control_key never JS,
identity re-derived server-side); platform-privileged flag only permits declaring reserved scopes; (3) console on enterprise
plan (no CPU/wall cap); (4) internal services reached via allowlisted PUBLIC hostnames, no SSRF carve-out. BLOCKER to clear (Phase 2):
multi-node worker registers only DbPlugin — must wire KvPlugin+StoragePlugin into `crates/worker/src/cache.rs create_plugins()`
(the console imports @zeroship/kv).

### Console build progress (R4→kernel→R5)
- **Phase 1 / R4 ✓** `610ad10d` grant-gated power-token mint. Runtime-mediated `env.auth.getAccessToken` async op (control_key
  never JS); control `/internal/power-token` fail-closed chain (channel→identity-from-signed-header→grant-ceiling→step-up→
  trusted_oauth_clients audience boundary); ed25519 zs-power+jwt 300s; AuthzGuard::guard_from_power_token; @zeroship/auth/server
  getAccessToken/fetchAs (server-only). Migration via NEW changeset 0011 (not editing 0006). Workflow `wm3nryq4l` (critic 90, all 8
  security booleans, RED-TEAM boundary_holds=true / no escalation across 7 vectors). Re-verified on live PG: power_token_test 10/10,
  control_key_is_never_js_reachable, full control + runtime 227 + worker 18 + gateway 27 + SDK 77. Step-up decision: env:write/apps:write
  NOT step-up (env:* config vs secrets:* split); deferred minors: split-DB doc note, bounded-60s header replay (v1-ok).
- **Phase 2 / kernel convergence ✓** `a3de8399` KvPlugin(Redis, shared not redb) + StoragePlugin(LocalFs shared volume)
  into multi-node worker create_plugins(); --kv-url/--storage-root config + compose redis service; faithful kernel test
  (real dispatch path, proven non-vacuous). Workflow `wpso8dogz` (critic 96, all 7 booleans). Re-verified: worker 21/21.
- **R4 SIMPLIFIED** `d4898dd0` (owner directive: power-token mint too complex → ENV-var control key, defer env.auth). Forward-
  removed all R4 machinery (env.auth.getAccessToken op, control /internal/power-token + PowerTokenIssuer, AuthzGuard arm,
  anchor auth_time + changeset 0011, SDK getAccessToken/fetchAs) preserving Phase 2 + pre-R4 + the env.auth NAMESPACE.
  Full-R4 preserved in design doc under "Future: full-R4". Workflow `wi7fogn7f` (critic 96, all 7 booleans). Re-verified on
  live PG/Redis: control all green (power_token_test gone), runtime 227 + auth_plugin 7, worker 21 (Phase 2 faithful test
  intact), gateway 27, SDK 73; dropped orphaned dev-DB auth_time column. **MVP control mechanism = env-var key + @zeroship/control.**
- **R5a ✓** `3abd461b` console BUILDS to .zship (was broken: dangling imports to removed bespoke request-context/auth — the
  surface R5 replaces). Identity via platform currentUser() (ZeroShip-User); control-client.ts → @zeroship/control with a
  SERVER-ONLY `ZS_CONTROL_SERVICE_TOKEN` (a control PAT — simplest credential AuthzGuard's bearer path accepts), never
  browser-exposed. MVP coarseness (documented, deferred to full-R4): control scopes to the PAT owner — per-creator isolation
  NOT enforced control-side; ZeroShip-Acting-User header is an attribution breadcrumb control doesn't read; console (trusted
  first-party) self-scopes. Workflow `wdx3qy11d` (critic 90, all 6 booleans). Re-verified: dist/app.zship (1.23MB, worker + 22
  rpc), credential absent from dist/assets, only apps/zeroship-builder touched, builder 21/21.
- **R5-seed ✓** `b9009917` `--bootstrap-console` install-time seed (additive, idempotent, boot-gated never-HTTP-route):
  control.apps row (enterprise plan) + public PKCE client (explicit sector_identifier) + ingest .zship + service-token
  env (ZS_CONTROL_SERVICE_TOKEN, a control PAT, server-only). DOGFOOD FIX (same commit): the console surfaced a TS↔Rust
  rate-limit wire drift — added RateLimitPer::User to crates/bundle + per-user bucketing in gateway compute_bucket_id + 5
  regression tests. Workflow `wabcj40r6` (critic 93). Re-verified: control + bundle + gateway full suites green (gateway lib 294).
- **R5 ingest-fix ✓** `93c53a9b` builder config override markers (/auth/* → ["auth","publicly_accessible"]; /api/preview/* →
  ["auth","rate_limit"]) so the console .zship passes Manifest::validate. **The REAL console .zship now BUILDS + INGESTS
  end-to-end** (seed test trial-ingests the real artifact, no fallback). Console is a DEPLOYABLE regular app.
- **R5-CUTOVER (NEXT — DESTRUCTIVE, CHECKPOINT BEFORE PROCEEDING)** the interdependent coordinated flip, paused for owner
  go-ahead (irreversible RP deletion + deployment routing + split-brain window):
  1. `--bootstrap-console` seed (additive): INSERT console.apps row (platform-privileged flag) + app_oauth_clients public PKCE
     client (explicit sector_identifier) + ingest the prebuilt .zship blob + set deploy_hash; in-process at control boot, NEVER an
     HTTP route. Mirror bootstrap_builder.rs. Also seed/inject the `ZS_CONTROL_SERVICE_TOKEN` (a control PAT) as the console app's
     server env.
  2. Login migration: console bespoke RP (oauth.ts/session.ts/oauth-store.ts + builderFetch/http.ts) → @zeroship/auth client +
     gateway /__zs/auth/* on the console host.
  3. Routing flip: ops/Caddyfile + compose console.* from control:9090 → gateway:8000.
  4. Control-side rip: delete crates/control oidc_rp.rs + console_sessions.rs + require_console_session (control = pure API
     resource server). DO steps 2-4 in ONE coordinated patch to avoid split-brain auth.
  5. Runtime envelope: console on enterprise plan (no CPU/wall cap); shared worker pool (no trusted tier, per owner).
  6. Faithful E2E: deployed console .zship via gateway→worker — login (popup) + a control read + a deploy + SSE chat + sandbox
     round-trip; assert the control credential never reaches the browser. Bootstrap: install-time `--bootstrap-console` seed (mirrors bootstrap_builder.rs), in-process,
never an HTTP route. Cutover DELETES the bespoke RP (builder oauth.ts/session.ts/oauth-store.ts/control-client.ts) + control
oidc_rp.rs + require_console_session — same patch; control becomes a pure API resource server. Repoint Caddy console → gateway.
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
