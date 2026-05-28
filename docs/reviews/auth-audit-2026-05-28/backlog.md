# Auth Audit Backlog — 2026-05-28

**Branch:** `proposal/auth-server` · HEAD `840c9a80` (after R0 fixes)
**Reviewers:** codex gpt-5.x R1-R5 (read-only audit) + Claude opus R0 (3 known-bug fixes)
**Sources:** `/tmp/codex-review-r{1,2,3,4,5}.md`

This is the prioritized fix list. Severity follows the reviewers' confidence ratings (≥75% to be a finding). De-duplicated where two reviewers flagged the same root cause.

---

## 🔥 CRITICAL — fix first

### C1 — Wrapper `aud` is issued per app but never enforced
**Found by:** R5-C1 (95%)
**Files:** `crates/gateway/src/wrapper_token.rs:309-314`, `crates/gateway/src/router/auth.rs:216-235`, `crates/gateway/src/dpop_exchange.rs:190`
**Issue:** `Verifier::new` sets `validation.validate_aud = false`. The wrapper carries `aud = Host` from the exchange endpoint, but dispatch only checks `cnf.jkt`. A wrapper minted for `app-a.zeroship.ai` can be replayed to `app-b.zeroship.ai` by the same DPoP key (proof is freshly signed for app B's `htu`).
**Impact:** Cross-app wrapper replay — the per-app binding promise is unenforced.
**Fix:** Enable audience validation in `Verifier`, accept current request Host, add a regression test.

### C2 — Malformed wrappers downgrade to raw hydra introspection
**Found by:** R5-C2 (90%)
**File:** `crates/gateway/src/router/auth.rs:240-266`
**Issue:** Documented invariant says malformed wrappers must NOT fall through. Current code falls through on any wrapper-verify error (any `JWT.Err`).
**Impact:** An attacker who presents a known-bad wrapper bypasses cnf.jkt enforcement. The downgrade is silent.
**Fix:** Detect "looks like a JWT / has `typ:at+jwt`" and hard-reject on wrapper-verify failure; only fall through for opaque hydra tokens (no `.` separators or non-JWT shape).

---

## 🚨 HIGH — fix this round

### H1 — Stash HMAC default + no length enforcement
**Found by:** R1-H1 (90%), R3-M2 (80%) — same root cause
**Files:** `crates/auth/src/config.rs:37-47`, `crates/auth/src/main.rs:46-52`
**Issue:** Default `dev-only-stash-signing-key-…` is accepted in non-dev mode; short keys also accepted; only a warning at boot.
**Impact:** Forge OAuth stash / pending-link tokens — bypasses state/PKCE/linking protections.
**Fix:** Refuse boot unless key is explicitly provided and ≥32 bytes, except when `--insecure-dev=true`.

### H2 — Rate-limit token bucket is raceable
**Found by:** R1-H2 (95%), R4-H2 (95%) — same root cause
**Files:** `crates/auth/src/ratelimit.rs:58-77`, `crates/auth/src/store/ratelimit.rs:30-78`
**Issue:** Read state → compute in Rust → upsert. Two concurrent requests see the same bucket; both pass.
**Impact:** Login/magic-link throttle bypass with parallel requests.
**Fix:** Move consume-or-fail into one SQL statement under `SELECT … FOR UPDATE` or use server-side atomic decrement; init missing buckets atomically.

### H3 — Password-reset tokens redeemable as magic-login tokens
**Found by:** R2-H1 (90%)
**Files:** `crates/auth/src/identity/magic_link.rs:136`, `crates/auth/src/identity/password_reset.rs:50`, `crates/auth/src/ui/magic.rs:358`
**Issue:** `magic_link::redeem` doesn't filter by `purpose='login'`. A 60-minute reset token in `auth.magic_links` can be presented to `/magic/verify` and converted to a login session.
**Impact:** Purpose confusion: reset → login bypass.
**Fix:** Scope `magic_link::redeem` SQL to `purpose = 'login'`; assert purpose at the UI handler too.

### H4 — OAuth-only account auto-links without provider trust
**Found by:** R2-H2 (85%), R3-H1 (90%) — related issue chain
**Files:** `crates/auth/src/identity/linker.rs:192,218,233`, `crates/auth/src/ui/oauth_google.rs:390`
**Issue:** Existing-user auto-link doesn't require `provider_trusted_for_email`. New-user create allows `email_verified=true` even when `provider_trusted_for_email=false`. A future provider with verified-but-non-authoritative emails can claim arbitrary addresses.
**Impact:** Account preempt / takeover via email collision with untrusted provider.
**Fix:** Require `provider_trusted_for_email == true` for all email-collision auto-linking AND auto-create. Untrusted-provider path → explicit confirmation flow (must enter password / verify ownership another way).

### H5 — Magic-completion 6-digit code has no attempt limit
**Found by:** R2-H3 (90%)
**Files:** `crates/auth/src/ui/magic.rs:498`, `crates/auth/src/ui/magic.rs:563`
**Issue:** `/magic/complete` has no rate limit or failed-attempt counter before checking the 6-digit code. Attacker who initiates magic-link for victim receives `csrf_nonce` on requesting page and can brute-force 1M codes in 5 min.
**Impact:** Account takeover by code brute-force.
**Fix:** Add per-`csrf_nonce`, per-email, per-IP attempt counters. Invalidate completion row after 5 wrong attempts.

### H6 — ID-token `at_hash`/`c_hash` not verified
**Found by:** R5-H1 (90%)
**Files:** `crates/gateway/src/oidc_rp.rs:224`, `crates/control/src/oidc_rp.rs:183`, `crates/core/src/oidc_verify.rs:322`
**Issue:** Both RPs have `access_token` and `code` but `verify_id_token` only checks issuer/audience/nonce. `at_hash`/`c_hash` claims in the ID token are silently ignored.
**Impact:** Code-substitution attacks not detected (RFC 7636 mitigates PKCE but `at_hash` is the OIDC-level defense).
**Fix:** Compute `at_hash` and `c_hash` per OIDC Core §3.1.3.6, compare when present.

### H7 — ID-token `iat` has no bounded freshness check
**Found by:** R5-H2 (85%), R3-L1 (80%) — same root cause
**File:** `crates/core/src/oidc_verify.rs:328-334`
**Issue:** `exp`, `iss`, `aud`, signature, nonce checked; `iat` accepted as anything. No `nbf` either.
**Impact:** A leaked ancient ID token replays indefinitely (until OAuth refresh token also rotates).
**Fix:** Bound `iat` to `now() ± 5 min` (skew tolerance). Check `nbf` if present.

### H8 — BCL `jti` replay cache absent
**Found by:** R5-H3 (95%)
**Files:** `crates/core/src/logout_token.rs:54-57`, `crates/gateway/src/backchannel_logout.rs:60-75`, `crates/control/src/backchannel_logout.rs:66-81`
**Issue:** `core::logout_token::verify` surfaces `jti` but doesn't enforce uniqueness. Neither gateway nor control plane RP handlers maintain a cache.
**Impact:** A captured logout_token can be replayed forever; can be used to repeatedly disrupt a victim's session.
**Fix:** Add a `JtiCache` (same pattern as `crates/core::dpop`'s) to BCL receivers, TTL = max(logout_token TTL + skew, 5 min).

### H9 — BCL `events` claim too lax
**Found by:** R5-H4 (90%)
**File:** `crates/core/src/logout_token.rs:189-193`
**Issue:** Only checks `contains_key(BCL_EVENT)`. RFC requires `events` to be exactly `{ "http://schemas.openid.net/event/backchannel-logout": {} }`.
**Impact:** Lower-bar acceptance — extra events could be exploited if a future verifier ever switches on them.
**Fix:** Enforce: exactly one key, exactly the BCL URI, exactly `{}` value.

### H10 — JWK stale-key retirement is unreachable
**Found by:** R4-H1 (90%)
**File:** `crates/auth/src/cron/jwk_rotation.rs:72-190`
**Issue:** Rotation resets `last_rotated_at = NOW()`, then retirement compares the same timestamp against `rotation_days + retain_days`. Threshold never reached on normal ticks.
**Impact:** Old hydra signing keys accumulate forever. JWKS bloats; compromised-key blast radius extends indefinitely.
**Fix:** Track `retired_at` separately, or retire BEFORE updating `last_rotated_at`, or query the previous timestamp.

### H11 — Hydra admin calls have no timeout
**Found by:** R4-H3 (95%)
**Files:** `crates/auth/src/hydra_client/mod.rs:{55,79,98,118}`
**Issue:** Every admin call awaits the transport without a deadline.
**Impact:** A hung hydra admin connection deadlocks login/consent/logout/bootstrap/cron indefinitely.
**Fix:** Wrap every call in `compio::time::timeout(Duration::from_secs(10), …)`; on timeout return a hydra-error result.

---

## 🟡 MEDIUM

### M1 — Signup/forgot are not rate-limited
**Found by:** R1-M1 (med), R2-M1 (overlap)
**Files:** `ui/signup.rs:82-116`, `ui/forgot.rs:70-83`
**Fix:** Add per-IP + per-email-normalized buckets.

### M2 — Password reset does NOT revoke existing sessions
**Found by:** R1-M2 (med)
**File:** `ui/reset.rs:142-159`
**Fix:** After `users::update_password_hash`, DELETE FROM `auth.sessions` WHERE user_id = … and `auth.gateway_sessions`, `auth.console_sessions`.

### M3 — Unlink-last-identity guard is race-prone
**Found by:** R2-M2 (med)
**File:** `ui/me.rs:125-175`
**Fix:** Wrap orphan-check + delete in transaction; or use conditional DELETE that asserts ≥1 other credential remains in the WHERE clause.

### M4 — Token-redemption secrets in GET URLs
**Found by:** R2-M3 (med)
**Files:** `ui/{verify,reset,magic,link}.rs`
**Fix:** GET landing page → auto-POST redemption with CSRF form. Configure access logs to strip query for these routes.

### M5 — OAuth stash has no signed expiry or single-use marker
**Found by:** R3-M1 (85%)
**File:** `ui/oauth_stash.rs:24-104`
**Fix:** Add `exp` claim to the stash payload (signed); rotate single-use marker in DB OR include `nonce` in a server-side ledger.

### M6 — Bootstrap + JWK rotation has no single-writer coordination
**Found by:** R4-M1 (90%)
**Files:** `bootstrap/keys.rs`, `cron/jwk_rotation.rs`
**Fix:** Wrap in `pg_advisory_lock(hash('auth_jwk_bootstrap'))` or similar.

### M7 — Hydra admin client has no auth / non-loopback guard
**Found by:** R4-M2 (85%)
**Files:** `config.rs:16`, `hydra_client/mod.rs:55`
**Fix:** Fail-closed on non-loopback admin URL unless `--allow-remote-hydra-admin` is set.

### M8 — Magic-link consumes one-shot state before hydra accept
**Found by:** R4-M3 (85%)
**Files:** `identity/magic_link.rs:136`, `ui/magic.rs:{453,648,786}`
**Fix:** Two-phase: mark `consume_pending` first → call hydra → mark `consumed` only on success → on failure clear pending and let user retry.

### M9 — Audit retention leaves PII-bearing events forever
**Found by:** R4-M4 (90%)
**Files:** `cron/audit_retention.rs:14`, `ui/oauth_{google,github}.rs`
**Fix:** Add a default retention bucket for unclassified events; make `oauth_link_needs_confirmation` either bucketed for cleanup OR scrub PII from the `detail` field.

### M10 — DPoP `htm` comparison is case-insensitive
**Found by:** R5-M1 (85%)
**File:** `crates/core/src/dpop.rs:213-220`
**Fix:** Exact match (HTTP method strings are uppercase per RFC; reject lowercase).

### M11 — DPoP `jti` replay cache is per-process
**Found by:** R5-M2 (95%)
**File:** `crates/core/src/dpop.rs:764-767`
**Fix:** PG-backed cache table `auth.dpop_jti` (or `gateway.dpop_jti`) with TTL sweep. Documented limitation today; needs implementation before multi-node deploy.

### M12 — Signing key file permissions unchecked
**Found by:** R5-M3 (95%)
**File:** `crates/gateway/src/signing.rs:35-38`
**Fix:** Refuse to load if mode is world/group-readable; warn if it's `0600` but owner isn't current uid.

### M13 — `ZeroShip-User` HMAC is not request-bound
**Found by:** R5-M4 (80%)
**Files:** `gateway/oidc_rp.rs:539-542`, `core/auth.rs:81-83`
**Fix:** Include a request-scoped nonce or short timestamp in the HMACed payload so headers can't be replayed across requests in the rare scenario where they leak (unlikely but cheap to defend).

---

## 🔵 LOW

### L1 — Error pages leak internal/upstream error strings
**Found by:** R1-L1 (low)
**Files:** `ui/login.rs:51-55`, `templates/error.html:5-6`
**Fix:** Server logs the full error; HTML shows a stable public message + opaque request ID.

### L2 — Expired token rows aren't swept
**Found by:** R2-L1 (low)
**Files:** `cron/mod.rs`, `store/migrations.rs:{54,73,85}`
**Fix:** New cron `cron::token_sweep` — DELETE expired+consumed rows from `auth.magic_links`, `auth.magic_completions`, `auth.email_verifications`, `auth.password_resets`. Grace period 7 days.

### L3 — Legacy migration is dead back-compat
**Found by:** R4-L1 (85%)
**File:** `store/migrations.rs:192,211,241`
**Fix:** Per AGENTS.md "no back-compat shims" — delete `migrate_legacy_auth_users` entirely.

---

## ℹ️ INFO

### I1 — `audit::emit_strict` is unused
**Found by:** R1-I1 (info)
**File:** `audit.rs:61-68`
**Fix:** Delete per pre-launch posture.

---

## Open questions

From R1: "Is deployment guaranteed to reject `AUTH_INSECURE_DEV=true`, default `AUTH_STASH_SIGNING_KEY`, and `AUTH_MAILER=stdout` outside local dev?" → resolved by H1's "fail-closed on default key".

From R2: "Are reverse proxy and app access logs configured to omit query strings for token routes?" → operator concern; document in `auth-deploy.md`.

From R2: "Does `cyper` enforce platform TLS certificate validation on SNS `SigningCertURL` fetches?" → needs verification (not flagged as a finding because confidence < 75%).

From R2: "Is Hydra's `redirect_to` strictly allow-listed for every client before auth server handlers reflect it into `Location`?" → hydra enforces redirect-URI exact-match for clients; the `redirect_to` from `accept_login`/`accept_consent` is hydra-built, not user-supplied. Safe.

---

## Resolved already (R0 fix bundle, commits `06f2522c`, `840c9a80`, `84ab332b`)

- ✅ `__Host-` cookie prefix incompatible with `--insecure-dev` (5 cookie modules + ~20 callsites)
- ✅ `/logout` handler missing (new module + template + 2 regression tests)
- ✅ `?t=` → `?token=` normalization (consistent across `/reset`, `/verify`, `/magic/{verify,await}`, `/link`)

R1's "CRITICAL build break" (CSRF parser signature change) was a snapshot of R0 mid-fix and is resolved by the final commit.

---

## Verified safe (from the reviews — what wasn't found)

- CSRF token entropy: 128-bit CSPRNG, constant-time compare on equal-length inputs
- Argon2id parameters: OWASP 2026 (m=19456 KiB, t=2, p=1)
- Magic-link / verification / reset tokens: 32 random bytes (256-bit), atomic UPDATE…RETURNING redemption, stored hashed
- Production session cookies: `__Host-` + Path=/ + HttpOnly + SameSite=Lax + Secure + no Domain
- Security headers: CSP without unsafe-inline, X-Frame-Options DENY, nosniff, no-referrer, COOP/CORP, HSTS preload, no-store
- HTML templates: askama auto-escape (no `|safe` overrides found)
- Plaintext email fallbacks: present for verification, magic, reset, suspicious-activity
- Postmark webhook: Basic auth before payload parse
- SES-SNS webhook: SignatureVersion check, SigningCertURL allow-list, RSA-SHA1 verify
- PKCE: 32 CSPRNG bytes, base64url no-pad verifier, S256 challenge
- Google ID-token: nonce + issuer + audience bound
- GitHub email: primary + verified + non-noreply required
- Existing-password-account collision → `NeedsConfirmation` flow (no auto-link)
- Unlink: refuses to remove last sign-in method
- `subtle::ConstantTimeEq` used on every MAC compare
