Live verification confirms the key facts: user `7f4d758a` has a live anchor but 0 identity rows (H1 cookie-only gap), 11 apps but only 3 app_members rows (creator self-authority gap), and `zeroship_control` has SELECT/INSERT/DELETE but **no UPDATE** on platform_admin_roles (f4.0 over-restriction). All load-bearing claims are corroborated. Writing the report.

---

# RED TEAM REPORT — zeroship Auth Pipeline (post-hardening, commits 7345acbc..HEAD)

**Date:** 2026-06-02 · **Scope:** 15 hardening fixes C1/H1/H2/M1/M2/L1–L7/I1–I3/I7/I8/I10 · **Method:** code read + live PG (`localhost:5440`) + live Hydra (admin `:4445`, public `:4444`) reproduction · **Dual-lens:** every claim carries an independent reproduction-lens and control-lens verdict.

---

## VERDICT

**The fixes largely HOLD on their stated security goals — but the pass shipped two genuine regressions and left two real bypasses, one of them defeating the headline H1 control for the exact user population the default SDK produces.**

The *cross-tenant* and *identity-forgery* hardening is solid. C1's zero-privilege default, the TOKEN⊂USER two-call enforce, Cedar resource-scoped creator policies, the L7 worker-dispatch scrub, alg-confusion/JWT defenses, PKCE/state/redirect_uri binding, M2 device CSRF, and the I8 pairwise gate all withstood direct attack and live probing (see Assurance). What did *not* hold:

1. **H1 is bypassed for cookie-only SDK users (HIGH, Confirmed-both-lenses).** The default `@zeroship/auth` BFF popup flow never writes `app_user_identities`, so H1's family-marker CTE writes **zero** markers and the victim's live session cookie survives the password reset for its full ~15-min TTL. Live-confirmed: the real cookie-BFF user `7f4d758a` has a live anchor and **0** identity rows. The H1 regression test masks this by pre-seeding the row.

2. **L5 account lockout is a no-recovery victim-DoS regression (HIGH/MEDIUM, Confirmed-both-lenses).** An attacker who knows a victim's email can permanently deny *all* login methods with ~1 request/hour, and the victim has **no escape** — not password, not magic-link, not OAuth, not even a successful password reset (which pointedly does not clear `locked_until`). This is the dominant over-restriction the pass introduced.

3. **C1 over-restricted: no creator self-authority path exists (HIGH, Confirmed-both-lenses).** C1 correctly closed the cross-tenant IDOR, but left no path that binds a creator to their *own* apps. `create_app` writes no `app_members` owner row (live: 11 apps, 3 members), so ordinary creators are denied every action on their own apps, cannot create apps, and cannot view/revoke their own OAuth grants. **A committed test (`oauth_token_with_apps_read_can_list_apps`) ships red (403 vs 200).**

4. **f1.1 / a1.1 — the `?mint=1` rotation fails open against concurrent revocation (HIGH/MEDIUM, Confirmed control-lens / partial repro-lens).** `update_rotated_family` discards rows-affected and `do_refresh` never re-checks `token_revocations` after the Hydra refresh, so an in-flight rotation racing H1 re-signs a fresh cookie. The `ceil()/floor()` whole-second marker quantization narrows but does not fully close the window.

5. **f4.0 — least-priv DSN bricks the platform-admin surface (MEDIUM, Confirmed-both-lenses).** I2/I3 granted `zeroship_control` no UPDATE on `platform_admin_roles`/`platform_policies`, but the handlers use `INSERT ... ON CONFLICT DO UPDATE`, which Postgres rejects without UPDATE. Live-confirmed grant matrix (SELECT/INSERT/DELETE only). Every `POST /admin/users/{id}/role` and `PUT /admin/platform-policies/{id}` returns 500. Fail-closed but functionally bricks admin role management.

**Top residual risks:** (1) cookie-only session survives password reset (H1 hole); (2) account-lockout weaponization with no recovery; (3) creators locked out of their own platform (C1 over-restriction) + admins unable to grant roles (f4.0) — i.e. the platform's self-service and admin surfaces are broken by the hardening; (4) the DPoP raw-opaque path remains bearer-equivalent (pre-existing, not a fix regression).

---

## CONFIRMED / CONTESTED FINDINGS (by severity)

### F1 — HIGH — H1 password reset does not evict cookie-only SDK sessions · **Confirmed (both lenses)**
*(dedup of a1.0 + f1.0 — same gap from attack and incomplete-fix angles)*

- **Kill-chain:** Victim logs in via the default `@zeroship/auth` popup → `POST /__zeroship/auth/session` mints `__Host-zeroship_app_session` (15-min TTL) + an anchor, **without** writing `app_user_identities`. Attacker holds/steals the cookie. Victim (or admin) does a password reset. H1 `complete()`'s `revoked_families` CTE JOINs `app_user_identities` → **0 rows → 0 family markers**. The gateway cookie arm's *sole* revocation gate is that marker (`credential_version` is never referenced in the gateway crate). Anchor is revoked (blocks re-mint) but the live cookie keeps authenticating on every dispatch until exp.
- **Exact path:** writer of `app_user_identities` is `identities::upsert` via `project_pairwise` (`crates/gateway/src/router/auth.rs:554`), called only from the DPoP (:829) and Bearer (:1051) arms — never the cookie mint path (`auth_token.rs mint_session_from_code`) nor `resolve_app_session_user_header_inner` (:1200). `relay.rs:98-101 mint_alias_at_consent` returns `Ok(None)`, writes nothing. Marker CTE: `crates/auth/src/identity/password_reset.rs:278-284`. Gate: `router/auth.rs:1263`.
- **Asymmetry:** the gateway-side teardowns (M1 `backchannel_logout.rs:191`, `/signout`) derive `pws_` via `derive_pairwise(salt, user, sector)` *independent* of `app_user_identities` and DO close the cookie. Only the auth-service reset is broken — auth holds neither the pairwise salt nor the sector.
- **Fix:** have H1 enumerate every `(app_client_id, sector)` the user has a live anchor/session for (gateway-side mapping) and write the marker per app; OR make the gateway own reset-triggered teardown; OR have the cookie arm consult `credential_version`. Stop the regression test pre-seeding `app_user_identities` (`password_reset_test.rs:723`) — drive the real cookie-only mint so it fails pre-fix.
- **Live evidence:** user `7f4d758a` — 1 live anchor, **0** identity rows; H1's marker-source query returns empty. Window bounded to `SESSION_TOKEN_TTL_SECS = 900` (`session_token.rs:59`).
- **Verdicts:** repro=confirmed (high) · control=confirmed/refuted-block (high). No blocking control; only mitigant is the 15-min TTL + anchor revoke preventing re-mint.

### F2 — HIGH — L5 lockout weaponized into a no-recovery victim DoS · **Confirmed (both lenses)**
*(dedup of a6.0 + f6.0)*

- **Kill-chain:** Attacker who knows the victim's email POSTs `/login?login_challenge=<any valid>` with 5 wrong passwords → `record_login_failure` → `locked_until = NOW()+60s`, exponential to 3600s cap. While locked, *every* method rejects the legitimate victim: password (arm 5 Ineligible), magic-link (`eligibility::check_user_eligible` → Locked), OAuth Google/GitHub (same gate), **and a successful password reset does not clear `locked_until`/`failed_login_count`**. `failed_login_count` is zeroed only by `reset_login_failures`, called only on a successful password login (`credentials.rs:272`) — unreachable while locked. ~1 wrong POST/hour at the cap sustains the lock indefinitely, well under the 10/hr per-email bucket. No self-service or admin unlock exists.
- **Exact path:** `crates/auth/src/store/users.rs:149-202` (backoff + record), `credentials.rs:176-194` (arm 5), `eligibility.rs:58-62`, `magic.rs:531/908`, `oauth_google.rs:316`, `oauth_github.rs:305`, `password_reset.rs:261-268` (UPDATE touches only password_hash/credential_version).
- **Fix:** clear `failed_login_count`/`locked_until` on password-reset complete and on magic-link/OAuth success (verified-email/federated-login is strong owner-present evidence); and/or lock the *source*, not the account, or delay-not-hard-deny with a verified-email unlock.
- **Live evidence:** reproduced on `zeroship.users` — 5 failures → 60s lock; reset UPDATE leaves `is_locked=t`; expire + 1 failure → 120s re-lock.
- **Verdicts:** repro=confirmed (honest scope: sustained DoS at ~1 req/window, not literal one-shot permanent → one verifier sets medium) · control=confirmed (high). **Net severity HIGH** — both lenses confirm reproduction; the down-rank is only on the "permanent vs sustained" framing, not on exploitability.

### F3 — HIGH — Creator has no self-authority over own apps/account (C1 over-restriction) · **Confirmed (both lenses)**
*(dedup of f0.0 + f4.1)*

- **Kill-chain (legit-access break, not exploit):** C1 set `DEFAULT_PLATFORM_ROLE="none"` (`entities.rs:25`). The two-call enforce runs the owner-without-token Cedar check first (`eval.rs:46-53`); role "none" matches no platform policy and an un-membered creator matches no `resource is App && resource in principal.app_*_of` creator policy → immediate Deny. (a) creator denied every action on their OWN app; (b) `create_app` requires `AppsWrite`/`Resource::Any` (`api.rs:90`), which no creator policy grants → ordinary creators cannot create apps; (c) `list_grants`/`revoke_grant` gate `AccountRead/Write` on `Resource::Any` (`oauth_grants_handlers.rs:35,90`), granted only by `readonly` → a default user cannot view/revoke their own OAuth grants.
- **Root cause:** `registry.rs create_app` (:126-163) inserts only into `zeroship.apps`, never an `app_members` owner row, and never receives the principal id. No production `INSERT INTO app_members` exists (every one is test/consent-seed); no trigger.
- **Shipped red test:** `crates/control/tests/authz_guard_oauth_test.rs:379` asserts 200, fails at HEAD with `left: 403 right: 200`. The hardening shipped a failing test masquerading as green hardening.
- **Fix:** make `create_app` write an `app_members(owner)` row binding the authenticated principal *and* broaden its gate; add a self-service Cedar policy granting `account:read/write` (self) + `apps:read` to the default role; then update the red test to the intended contract. **Preserve** the cross-tenant DENY (correct C1 goal — `two_call_test.rs:271 unroled_creator_denied_cross_tenant_reads` pins it).
- **Live evidence:** 11 apps, only 3 `app_members` rows — most apps already have no owner; enforce() probe denies AppsRead/EnvRead/SecretsRead/AppsDeploy/BillingRead on a creator's own app, flips to Allow when an owner row is inserted.
- **Verdicts:** repro=confirmed (high) · control=confirmed (high). Caveat: f4.1's onboarding writer lives outside this repo (builder service); if it also omits the owner row, every creator is locked out — **needs human confirmation** of the out-of-band provisioner.

### F4 — HIGH/MEDIUM — `?mint=1` rotation fails open against concurrent revocation (H1/M1 TOCTOU) · **Contested (needs-human on the timing window)**
*(dedup of a1.1 + f1.1)*

- **Kill-chain:** Attacker holding a live anchor fires `GET /__zeroship/auth/session?mint=1` (passes L1 CSRF). `anchors::read_live` returns the live anchor and enters `do_refresh`. Concurrently the victim's H1 reset commits (revokes anchor + writes family marker). `do_refresh` (`auth_token.rs:961-1114`) does the Hydra refresh — **which succeeds because H1 deliberately leaves the Hydra grant alive** (`password_reset.rs:229-233`) — then `update_rotated_family` matches 0 rows but **discards rows-affected** (`anchors.rs:292`, `tx.execute`), returns `Ok(RotationOk)`, and re-signs a fresh cookie with `iat=now()`. No post-refresh `token_revocations` re-check, no anchor re-read. Next dispatch: `revoked_after > iat` = false → session resurrected.
- **Partial block:** the marker is `ceil()`-rounded to whole seconds (`wrapper_revocation.rs:70-86`) and `iat` is `floor()`-ed; the cookie `iat` is always stamped *after* the marker (read_live→reset→refresh→sign ordering). One verifier (live-DB arithmetic): a marker at 1000.9 (ceil 1001) and cookie at 1001.1 (floor 1001) → `1001 > 1001` = false → **resurrected**, and the Hydra RTT (tens-to-hundreds of ms) crosses such boundaries on a meaningful fraction of attempts. The other verifier: a sub-second refresh lands `iat ≤ ceil(marker)` → Revoked, requiring a pathologically slow (~2s) refresh.
- **Bound:** one-shot — `read_live` filters `revoked_at IS NULL`, so after the reset commits every subsequent `?mint=1` fails closed; impact = one ~15-min cookie, no renewal.
- **M1 note:** a1.1's M1 framing is **incorrect** — M1 writes no `token_revocations` marker (only flips `gateway_sessions.revoked_at` + deletes anchors), so the marker-race mechanism does not apply to M1. The durable race is H1-only.
- **Fix:** `update_rotated_family` returns rows-affected; `do_refresh` treats 0 rows as `LoginRequired`; re-check `is_family_revoked_since` inside the rotation tx; for H1, also revoke the Hydra refresh grant on reset.
- **Verdicts:** repro=partial (medium — ceil defense holds against the realistic sub-second case) · control=confirmed (high — live-DB shows the same-whole-second fail-open). **Net: HIGH severity for the fail-open code defect (discarded rows-affected, no post-refresh re-check are unambiguous), with the practical exploit window CONTESTED** — needs a live gateway HTTP harness to settle the realistic hit-rate. Close the fail-open regardless of the timing debate.

### F5 — HIGH — DPoP raw-opaque token theft → impersonation (no `cnf.jkt` sender-constraint) · **Confirmed (both lenses) — pre-existing, NOT a fix regression**
*(dedup of a2.0 + f2.1)*

- **Kill-chain:** Steal a victim's opaque DPoP-bound access token, self-sign a fresh proof with the attacker's own keypair (`ath=SHA256(token)`, correct htm/htu/jti/iat). The proof signature verifies against the JWK embedded *in the proof header* (`dpop.rs:198`); the proof's thumbprint (`verified.jkt`, `dpop.rs:244`) is never compared to any token-bound `cnf.jkt`. Introspection returns `active:true` bound to the right client_id → accepted as the victim.
- **Exact path:** `crates/gateway/src/router/auth.rs:646 resolve_dpop_user_header` consumes only `verified.jti` (:706); `jkt` discarded. Deferral documented at `auth.rs:281-284, 718-724`.
- **Why HIGH not critical:** gated by an out-of-band precondition (opaque token is non-browser-exposed — the SPA uses the cookie path; zero DPoP in `sdks/auth`). Blast radius bounded by per-app `client_id` binding (`auth.rs:764`, blocks cross-app replay) and per-app family-marker revocation (`auth.rs:807`). Live: Hydra `TokenInfo` carries no `cnf` field and registered clients have no DPoP config — there is genuinely nothing to bind against today.
- **Fix:** enforce `cnf.jkt` ↔ proof thumbprint once Hydra surfaces it (Phase 8). Not introduced by this pass; the pass actually *reduced* blast radius via 6c/6d.
- **Verdicts:** repro=partial/confirmed (high) · control=partial/confirmed (high). Possession == impersonation for the raw-opaque path until the precondition (token exfiltration) is met.

### F6 — MEDIUM — Least-priv DSN bricks platform-admin role/policy management (I2/I3 over-restriction) · **Confirmed (both lenses)**

- **Kill-chain (fail-closed availability break):** `zeroship_control` holds only SELECT/INSERT/DELETE on `platform_admin_roles`/`platform_policies` (`0025_roles_rls.sql:200-201`), but `grant_platform_role` (`admin_handlers.rs:108-110`) and `upsert_platform_policy` (:357-359) use `INSERT ... ON CONFLICT DO UPDATE`, which Postgres rejects at permission-check time without UPDATE — even on a first non-conflicting insert. Every `POST /admin/users/{id}/role` and `PUT /admin/platform-policies/{id}` → `permission denied` → 500.
- **Live evidence:** confirmed grant matrix (no UPDATE); `SET ROLE zeroship_control` → `ERROR: permission denied for table platform_admin_roles` / `platform_policies`; plain INSERT/DELETE succeed (isolating the missing UPDATE verb). Prod wiring: `docker-compose.yml:107` runs control as `zeroship_control`. The smoke test connects as superuser (`admin_handlers_test.rs:25`), masking it.
- **Fix:** GRANT UPDATE on the two tables, or rewrite the handlers to avoid `ON CONFLICT DO UPDATE`.
- **Verdicts:** repro=confirmed (medium) · control=confirmed (medium). Fail-closed — no escalation, but the admin surface is bricked.

### F7 — MEDIUM — DPoP wrong-password / dummy-hash timing oracle residual · **Contested (medium vs low)**
*(dedup of a6.1 + f6.2)*

- **Kill-chain:** The dummy-hash equalizes Argon2 wall-time, but post-verify DB work diverges: arm 6a (absent/OAuth-only, `credentials.rs:202-216`) = 1 audit INSERT; arm 6b (real password account, :220-243) = `record_login_failure` (1–2 UPDATEs) **then** audit. The extra serialized PG round-trip makes real password accounts measurably slower → statistical email-enumeration oracle.
- **Measurement split:** one verifier modeled the arms on the live DB and got a clean ~2.0ms/attempt separation (6a ~2.0ms vs 6b ~4.1ms, zero distribution overlap) → **medium**; the other benchmarked the bare UPDATE at ~0.06–0.13ms loopback, 2–3 orders below the ~tens-of-ms Argon2 floor and below jitter → **low**.
- **Fix:** issue an equivalent no-op write on the 6a arm, or move `record_login_failure` off the latency-visible path.
- **Verdicts:** mechanism confirmed by all lenses; magnitude **contested**. **Net MEDIUM** (the asymmetry is real and on the latency-visible path; whether it's a usable oracle over a network needs a live HTTP timing harness). Cheap to equalize regardless.

---

## FIX-REGRESSIONS (bypass / over-restriction / missed-sibling)

| ID | Class | Fix | Verdict | Severity |
|---|---|---|---|---|
| **F1** | bypass + missed-population | H1 family-marker teardown | Confirmed both | HIGH |
| **F2 / f6.0** | over-restriction (weaponized) | L5 lockout (no recovery path) | Confirmed both | HIGH |
| **F3 / f0.0 / f4.1** | over-restriction (+ shipped red test) | C1 default role "none" | Confirmed both | HIGH |
| **F4 / f1.1** | bypass (fail-open TOCTOU) | `?mint=1` rotation vs H1 | Confirmed control / partial repro | HIGH (window contested) |
| **F6 / f4.0** | over-restriction (fail-closed) | I2/I3 least-priv DSN | Confirmed both | MEDIUM |
| **F7 / f6.2** | incomplete-fix | dummy-hash enumeration defense | mechanism confirmed | MEDIUM (magnitude contested) |
| **f6.1** | missed-sibling | L5 lockout (not on `/link`) | Confirmed | LOW |
| **f2.0** | missed-sibling | L7 reserved-header scrub (not on auth-host proxy) | Confirmed (inert today) | LOW |
| **f3.0** | incomplete-fix | L2 exact-match (not re-checked on token leg) | Confirmed (Hydra backstops) | LOW |
| **f3.1 / a3.0 / f3.0-dev** | missed-sibling / dev posture | I1 Hydra admin loopback (dev binds 0.0.0.0) | Confirmed dev exposure | LOW (prod blocked) |
| **f0.1** | incomplete-fix (latent) | Stripe path-supplied `creator_id` (no principal bind) | Refuted live / latent | LOW |
| **a5.0 / a5.1** | over-restriction / browser | L5 mild lockout DoS; breadcrumb clobber | Self-recovering / UX-only | LOW |

**Notable detail:** three of the four HIGH regressions (F1, F2, F3) **shipped with green or masking tests** — the H1 test pre-seeds `app_user_identities`; the C1 change left `oauth_token_with_apps_read_can_list_apps` red; the f4.0 smoke test connects as superuser. This is the same faithful-e2e gap pattern flagged in prior reviews: the tests do not exercise the production path.

---

## ASSURANCE — attacks cleanly BLOCKED (what now holds)

The hardening's *security* goals are robust. The following were attempted and cleanly blocked (live-verified where it's a DB/Hydra/HTTP behavior):

**Cross-tenant (the C1/H2 core):**
- C1 cross-tenant read IDOR — **blocked** by `DEFAULT_PLATFORM_ROLE="none"` + Cedar `resource is App && resource in principal.app_*_of`. (The DENY is correct; only self-authority was over-cut — F3.)
- OAuth `scopes_to_policy` `Resource::Any` bypass of TOKEN⊂USER — **blocked** by the two-call enforce (`eval.rs:45-54`) intersecting token authority with the principal's static authority on the concrete resource.
- Stale/colliding Cedar entity-cache cross-tenant — **blocked** (principal_id in cache key + resource-scoped invalidation).
- Cross-tenant admin actions (suspend/role-grant/audit-lock/members) via a creator — **blocked** (Org-resource authority held only by `platform_role=="admin"`).
- Cross-tenant env/secret read via `/internal/apps/{id}/env` — **blocked** by `control_key` service-secret + constant-time compare (I10).
- H2 uuid-read panic-DoS — **both** authz uuid reads fixed; no further `String`-typed `app_id` reads in the authz path (the eval.rs/entities.rs sibling pattern is fully covered).
- Stripe earnings/unlink cross-tenant billing IDOR — **blocked today**: no creator-grantable `Billing*`/`Resource::Any` policy exists (latent only — f0.1).

**Session persistence:**
- M1 backchannel logout cookie teardown — **does NOT share F1's gap** (derives `pws_` independently).
- Anchor reload-recovery re-mint after H1/M1 — the working control; `read_live` fails closed once revoked.
- Revocation-cache 5s staleness as a resurrection window — bounded, no durable session.

**Identity forgery (all blocked):**
- Forge / smuggle / replay `ZeroShip-User` HMAC header at the worker — blocked (HMAC + L7 dispatch scrub).
- alg-confusion / `alg:none` on every Hydra-issued JWT (id_token, access JWT, session cookie, logout_token, DPoP) — blocked.
- I8 pairwise shape-gate used to inject a forged `pws_` — blocked.
- Cross-app token confusion (replay app A's token at app B) — blocked by per-app `client_id` binding.
- Logout-token forgery; session-cookie audience confusion — blocked.

**OAuth/OIDC (all blocked, several live-verified against Hydra):**
- PKCE strip / downgrade-to-plain — blocked (`oauth2.pkce.enforced=true`, S256-only).
- Auth-code cross-client / cross-session injection — blocked (code↔client + code↔challenge + id_token aud/c_hash binding).
- redirect_uri manipulation / open redirect — blocked at both layers (gateway L2 exact-match + Hydra; live: mismatched token-leg redirect_uri → `invalid_grant`).
- Scope/consent escalation, `skip_consent` abuse, implicit-grant downgrade, `prompt=none` leak — blocked.
- M2 device-flow CSRF — blocked (double-submit Strict cookie + I1 loopback).
- State/nonce CSRF + popup-callback relay XSS — blocked (static body, nonce-only interpolation, strict CSP L3, `targetOrigin=location.origin`).

**Privilege escalation (all blocked):**
- PAT Any-grant self-escalation to platform admin; OAuth `team:write` console bearer at admin endpoint; cross-app `team:write` PAT subset; direct write to `platform_admin_roles` by the least-priv role; deny-statement abuse in PAT wrapper — all blocked. (The C1 default-role sibling hunt found no privileged default.)

**Browser surface (all blocked):**
- Clickjack/in-iframe CSRF of `auth.zeroship.ai/login`; BroadcastChannel/localStorage relay forgery from a same-site sibling; postMessage to a foreign origin; reflected XSS via `login_challenge`/`error`/`client_name`; CSRF on every state-changing POST; L2 same-origin-unregistered redirect; I7 Strict-cookie iframe handling without a Strict→None downgrade; L3 CSP nonce prediction — all blocked.

**Credential & abuse:**
- Cross-device magic 6-digit brute force; password-reset token retarget/reuse (L4 + single-use); distributed credential guessing beating the rate limiter — all blocked.

---

## VERIFICATION NOTES & ITEMS NEEDING HUMAN/LIVE CONFIRMATION

**Verified live this session (PG `localhost:5440` / Hydra `:4445`/`:4444`):**
- 15 hardening commits present (`7345acbc..253687e7`).
- Hydra dev admin reachable unauthenticated: `GET /admin/clients` → 200, `docker port` → `4445/tcp -> 0.0.0.0:4445`.
- H1 gap (F1): cookie-BFF user `7f4d758a` has 1 live anchor, **0** `app_user_identities` rows → 0 markers.
- C1 over-restriction (F3): 11 apps, 3 `app_members` rows → most apps ownerless.
- f4.0 over-restriction (F6): `zeroship_control` grants = SELECT/INSERT/DELETE only, no UPDATE on `platform_admin_roles`.

**Needs a human / live confirmation:**
1. **F4 timing window** — the two verifiers split on the realistic `?mint=1`-vs-H1 hit rate (sub-second-refresh-safe vs same-whole-second-fail-open). Settle with a live gateway HTTP harness firing concurrent `?mint=1` + reset. The fail-open code defect (discarded rows-affected, no post-refresh re-check) should be fixed regardless.
2. **F3 out-of-band provisioner** (f4.1) — confirm the builder/onboarding service (outside this repo) writes the `app_members` owner row; if not, every creator is locked out platform-wide.
3. **F7 oracle magnitude** — confirm over the real HTTP path whether the ~2ms `record_login_failure` delta survives network jitter as a usable enumeration oracle (medium) or is swamped (low).
4. **I1 prod deployment** (a3.0/f3.1) — no in-repo manifest mounts the loopback `ops/hydra.yaml`; confirm the prod path firewalls/loopback-binds `:4445` and never reuses `hydra-dev.yaml`. The Hydra admin API has **no built-in auth** — network isolation is the only boundary; consider mTLS/admin-auth as defense-in-depth.

**Test-integrity flag:** F1, F3, and F6 each shipped with a test that hides the regression (pre-seeded row / red committed test / superuser-DSN smoke test). Recommend a faithful-e2e pass that drives the real cookie-only mint, the real creator-self-service path, and the real least-priv DSN before re-landing the fixes.