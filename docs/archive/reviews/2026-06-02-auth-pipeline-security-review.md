I have enough to synthesize. Producing the prioritized report now.

# Zeroship Auth Pipeline — Adversarial Security Review

## Executive Summary

**Overall posture: strong cryptographic core, with one critical multi-tenant authorization defect and a systemic gap in credential-change / logout session termination.**

The cryptographic and BFF-architecture invariants are implemented carefully and largely correctly. The ID-token / logout-token verifier resists algorithm-confusion and `alg:none`; PKCE custody and the no-token-to-browser BFF model hold; the immersive-iframe clickjacking + postMessage surface is fail-closed; the deleted `/password` oracle and `auth_internal_key` shared secret leave zero residue. These are real strengths verified against the code, not assumed.

The serious problems cluster in two places: (1) **a single Cedar default-role misconfiguration that grants every authenticated creator fleet-wide cross-tenant read** on app metadata, env-var/secret names, billing/earnings, and deploy history; and (2) **a family of session-lifecycle gaps** where password reset and backchannel logout fail to durably terminate gateway app-sessions because they never stamp the per-app family-revocation marker, never delete the 30-day anchor, and never revoke the Hydra refresh grant — letting `?mint=1` resurrect a session the user believed they killed.

### Top risks
1. **CRITICAL — `readonly` default platform-role = fleet-wide cross-tenant read IDOR** (every signed-up creator can enumerate and read every app's secrets-names/env/billing).
2. **HIGH — Password reset does not terminate gateway app-sessions** (no family marker, no anchor delete, no Hydra refresh revoke; attacker survives the victim's reset for up to 30 days via anchor `?mint=1`).
3. **HIGH — `load_principal_app_resources` reads UUID column as `String`** — guaranteed panic DoS on consent-grant + PAT-minting for any creator with an app membership.
4. **MEDIUM — Backchannel logout / "sign out everywhere" is resurrectable** via the same anchor + `?mint=1` path (no anchor delete, no Hydra refresh revoke).
5. **MEDIUM — `/device` RFC 8628 confirmation has no CSRF token** (sole exception in the auth form surface; only SameSite=Lax guards it).

### Strong points verified (done right)
- **ID-token / logout-token / DPoP alg-confusion is structurally blocked** — JWKS loader builds only asymmetric keys, key lookup pins `(kid, alg)`, HS*/`none` never enter the cache; iss/aud re-checked, at_hash/c_hash constant-time. (Findings 1.3, 6.4)
- **BFF no-token invariant holds** — the browser never receives an OAuth token; `env.auth` identity comes from a separate request-bound HMAC `ZeroShip-User` channel and cannot be spoofed via the dispatch envelope. (Finding 6.0)
- **Immersive-iframe browser surface is fail-closed** — empty/poison frame-ancestors allowlist keeps `XFO: DENY` + `frame-ancestors 'none'`; popup-callback reflects no query params; postMessage pinned to `location.origin`; SDK relay enforces origin + state. (Finding 2.3)
- **Redirect sinks constrained** — trailing-slash origin prefix + relative-path sanitizer (re-applied at sink) + Hydra registered-URI allowlist; no open redirect. (Finding 6.5)
- **`/password` oracle + `auth_internal_key` fully removed** — guarded by `deny_unknown_fields` parse-reject + negative SDK test; gateway→worker trust is solely the HMAC `worker_key`. (Finding 6.3)

### Findings by final severity
| Severity | Count |
| --- | --- |
| Critical | 1 |
| High | 2 |
| Medium | 3 |
| Low | 7 |
| Info (assurance / latent) | 6 |

> Note: two HIGH findings on password reset (0.0 and 3.0) describe the **same root issue** and are merged below. Several findings were down-ranked from their initial severity where a mitigating control was verified (see each verdict line).

---

## CRITICAL

### C1. `readonly` default platform-role grants every authenticated creator fleet-wide cross-tenant read
**Final severity: Critical — Confirmed (both lenses).**

**Location:** `crates/authz/src/entities.rs:142-166` (`COALESCE(r.role,'readonly')`); `policies/platform/readonly.cedar:1-17`; `crates/control/src/authz_guard.rs`; `crates/control/src/api.rs:124-162,552,576`; `crates/control/src/env_handlers.rs:82,179,239`; `crates/control/src/registry.rs:192-202`; `crates/authz/src/scope.rs:162-172`.

**Issue:** `load_user` computes the principal's platform role as `COALESCE(r.role, 'readonly')`. The only writer of `platform_admin_roles` is the admin-grant endpoint, so **every ordinary creator evaluates as `readonly`**. `readonly.cedar` permits `apps:read / env:read / secrets:read / billing:read / deployments:read / team:read / account:*` on an **unconstrained `resource`** — no `resource in principal.app_*_of` clause (contrast the creator policies). Cedar is permit-biased and runs schemaless, so the readonly permit fires for any `App`. The OAuth token-policy layer does not save it: `scopes_to_policy` emits the action on `Resource::Any`, not ownership-bound, so it AND-gates to Allow. The console/CLI clients legitimately carry platform read scopes, so a normal creator session satisfies the precondition.

**Exploit:** Sign up as an ordinary creator. With the standard console bearer: `GET /apps` enumerates **every app on the platform** (`list_apps` / `registry.list_apps` has no owner filter), then `GET /apps/{victim_id}`, `.../env` (env var names), the secrets-read endpoint (secret key names), `.../billing` (other creators' revenue/payout/earnings), and deploy history — all cross-tenant. A creator can even mint a PAT granting `apps:read` on `Resource::Any` for durable fleet-wide read.

**Fix:** Make the `COALESCE` default a true zero-privilege role (e.g. `'none'`) with no platform policy, so un-roled creators are authorized only through their `app_members`-bound creator policies; and/or constrain `readonly`/`support`/`billing` policies to explicitly-granted staff via a membership/role-bound clause. Revisit `scopes_to_policy` so OAuth tokens cannot grant reads outside the principal's memberships. Add a regression test: a creator owning app A must get Forbidden on app B's reads, and `GET /apps` must not leak non-member apps.

**Standard:** OWASP Top 10 A01:2021 Broken Access Control (object/function-level authz, IDOR); OWASP ASVS V4.1.3 / V4.2.1; the platform's own least-privilege multi-tenant isolation invariant.

**Verdicts:** Exploit lens → confirmed/critical (no mitigating control; token-policy layer is resource-unbound). Mitigation lens → confirmed/critical (no overriding forbid; handlers carry no membership pre-check). *Caveat: exposes secret names, not values — but fleet-wide billing/earnings + secret/env inventory under a low-privilege session is squarely the critical "cross-tenant IDOR on auth data" case.*

---

## HIGH

### H1. Password reset does not terminate gateway app-sessions — attacker survives the victim's reset (and resurrects via `?mint=1`)
**Final severity: High — Confirmed (both lenses, both finding-pairs). Merged from 0.0 and 3.0.**

**Location:** `crates/auth/src/identity/password_reset.rs:196-237`; `crates/auth/src/ui/reset.rs:245-310`; `crates/auth/src/store/users.rs:109-125`; `crates/gateway/src/router/auth.rs:1229-1296`; `crates/gateway/src/auth_token.rs:637-855,909-1104` (`?mint=1` / `rotate_family` / `do_refresh`); `crates/gateway/src/anchors.rs:234-270`; `crates/auth/src/hydra_client/sessions.rs`.

**Issue:** The reset completion path (`password_reset::complete`) runs an inline `UPDATE zeroship.users SET password_hash, updated_at` — it does **not** bump `credential_version` (unlike the canonical `users::update_password_hash`). `complete_password_reset_tx` deletes only `idp_sessions` + `gateway_sessions` rows and calls Hydra `delete_login_sessions` (SSO login session only). But the gateway's live app-session cookie (`__Host-zeroship_app_session`) is validated **100% statelessly** (signature + iss + exp + app + pws_), never reading `gateway_sessions`; its only revocation gate is the per-app family marker `is_family_revoked_since(client_id, pws_, iat)` in `zeroship.token_revocations`. Reset **never calls `revoke_family`**, never deletes the 30-day `app_session_anchors` row, and never revokes the Hydra refresh grant. So: deleting `gateway_sessions` is inert; `credential_version` gates only the IdP leg, not the gateway cookie; and the anchor + refresh grant survive.

**Exploit:** Attacker compromises the victim's password (the precondition reset exists to remediate), signs into a creator app, holding the ~15-min cookie + the 30-day `__Host-zeroship_app_anchor`. Victim resets the password. The reset rotates the IdP/Hydra login session but stamps no family marker — the attacker's cookie keeps validating to expiry; then `GET /__zeroship/auth/session?mint=1` reads the surviving anchor, runs a Hydra refresh (the login-session deletion does not revoke the refresh grant), and re-mints a fresh cookie whose `iat` post-dates any marker. The session is resurrected for up to the 30-day anchor / 720h Hydra ceiling. The "change password to evict the intruder" guarantee is broken precisely on the app-runtime tier where end-user data lives. Every peer path (`/signout` `browser_auth.rs:374-448`, control disconnect-app `oauth_grants_handlers.rs:240`, backchannel logout) writes the family marker; password reset — the recovery flow — omits all three teardown steps.

**Fix:** In the reset transaction, for every app the user holds a session/anchor with: write `token_revocations` family markers per `(client_id, pws_)` (the same writer `/signout` uses), delete `app_session_anchors` for the user, and revoke the Hydra refresh family / consent grants — not just the login session. Route the password update through `users::update_password_hash` to bump `credential_version` (defense-in-depth for the IdP gate). Regression test: a pre-reset gateway cookie and anchor must both be rejected post-reset.

**Standard:** OWASP ASVS v4 3.3.1/3.3.2 (terminate all active sessions on credential change); OWASP Session Management Cheat Sheet; OAuth 2.0 Security BCP RFC 9700 §4.14 (refresh-token revocation on credential change); NIST 800-63B session-binding.

**Verdicts:** All four verifier lenses (0.0 exploit+mitigation, 3.0 exploit+mitigation) → confirmed/high. No mitigating control: the claimed `delete_login_sessions`→BCL cascade does not fire (it terminates the SSO session, not refresh grants), and `gateway_sessions` deletion is inert against the stateless cookie.

---

### H2. `load_principal_app_resources` reads UUID column `app_members.app_id` as `String` — guaranteed panic DoS on consent + PAT minting
**Final severity: High — Confirmed exploit lens (high); mitigation lens corrected to medium. Final: High (down-weighted, see verdict).**

**Location:** `crates/authz/src/eval.rs:81-133` (`load_principal_app_resources`, called unconditionally at :87 by `is_authorized_anywhere`); `crates/authz/src/resource.rs:8` (`Resource::App.id: String`); `db/changelog/changesets/0004_control.sql:220` (`app_members.app_id UUID`); `crates/compio-postgres/src/row.rs:148-187` (panics on type mismatch); `crates/auth/src/ui/consent.rs:875`; `crates/control/src/token_handlers.rs:464`. Already-fixed sibling: `crates/authz/src/entities.rs:179-184`.

**Issue:** `load_principal_app_resources` builds `Resource::App { id: row.get("app_id") }` reading a `UUID` column into a `String`. compio-postgres `row.get::<String>` on a uuid column panics with `WrongType`. This is the identical bug just fixed in `entities.rs:183` (which reads `Uuid` then `.to_string()`s) — the eval.rs sibling was missed. `is_authorized_anywhere` calls it **unconditionally** before its probe loop, so the panic fires for any principal with ≥1 `app_members` row.

**Exploit:** Any creator who is a member (owner/editor/viewer) of at least one app — the normal state — triggers a worker panic / 500 the moment `is_authorized_anywhere` runs, which gates (1) the consent screen for platform-delegated OAuth scopes and (2) PAT minting. A normal creator granting a third-party app platform scopes, or minting a personal access token, hits a deterministic DoS. The covering test is env-gated (`AUTH_DB_URL`) and never runs in CI, so the type mismatch shipped unverified.

**Fix:** Read as `Uuid` then stringify, mirroring the entities.rs fix: `let app_id: Uuid = row.get("app_id"); Resource::App { id: app_id.to_string() }`. Add a non-env-gated regression test seeding one `app_members` row and exercising `is_authorized_anywhere` + the consent + PAT paths. Audit for other `row.get("app_id"/"id")` String reads against UUID columns.

**Standard:** OWASP ASVS V1.4 (fail-safe authorization decisions); OWASP Top 10 A04:2021 Insecure Design (availability of the authz subsystem); the project's faithful-test mandate (env-gated covering test never runs).

**Verdicts:** Exploit lens → confirmed/high (unconditional call, no guard, fails closed but DoSes two core flows for the primary user population). Mitigation lens → confirmed but corrected to medium (panic fails closed — no bypass/escalation — so it's an availability break). **Lead adjudication: keep High.** It is a deterministic, attacker-reachable DoS on consent + token-issuance for the platform's core population, on a never-CI-tested code path; the availability impact and shipped-unverified status justify the higher band.

---

## MEDIUM

### M1. Backchannel logout revokes the family marker but never deletes the anchor — "sign out everywhere" is resurrectable via `?mint=1`
**Final severity: Medium — Confirmed (both lenses).**

**Location:** `crates/gateway/src/backchannel_logout.rs:163-253` (per-app branch: `revoke_family` + delete `gateway_sessions` only); `crates/gateway/src/anchors.rs:234-270` (`read_live` checks only `revoked_at`/`abs_expires_at`); `crates/gateway/src/auth_token.rs:909-1104` (`rotate_family`/`do_refresh`, no marker re-check); `crates/core/src/wrapper_revocation.rs:109` (`revoked_after > iat`).

**Issue:** The per-app BCL branch writes the `(client_id, pws_)` family marker and deletes `gateway_sessions`, but never deletes the `app_session_anchors` row and never revokes the Hydra refresh grant. The `?mint=1` reload-recovery path skips the family-revocation fast-path, `read_live` ignores the marker, and `do_refresh` only fails on a Hydra `invalid_grant`. Since the marker rejects only tokens with `iat < revoked_after`, a freshly re-minted cookie's `iat` post-dates it and is honored. The contrasting `/signout` handler (`browser_auth.rs:374-448`) does all three teardown steps, proving BCL is missing two of them.

**Exploit:** A global/RP-initiated or admin-driven logout writes the marker + clears DB rows but leaves the anchor and Hydra refresh grant alive. A request carrying the still-live `__Host-zeroship_app_anchor` + same-origin `X-ZS-Auth` (i.e. the device being evicted) calls `?mint=1` after the marker instant; the anchor reads live, Hydra refresh succeeds, a cookie with `iat > revoked_after` is minted, and the user is silently logged back in. The advertised durable "every device" termination is defeated.

**Fix:** In the per-app BCL branch, also `anchors::delete_all_for_user` (or set `revoked_at`) and best-effort revoke the Hydra refresh family; and have `rotate_family`/`do_refresh` re-check the family marker after re-mint so a logout racing an in-flight refresh still wins.

**Standard:** OIDC Back-Channel Logout 1.0 §2.6 (RP MUST invalidate the session); OWASP ASVS 3.3.1; RFC 9700 §4.14.

**Verdicts:** Both lenses → confirmed/medium, no mitigating control (the code comment claiming the reload path "re-checks the family via Hydra" only holds on `invalid_grant`, which BCL never triggers). Precondition is possession of a live anchor on the device being evicted, so medium not high.

---

### M2. Device Authorization Grant confirmation (`POST /device`) has no CSRF token
**Final severity: Medium — Contested(needs-human). One lens confirmed/medium, one refuted (down to low).**

**Location:** `crates/auth/src/ui/device.rs:25-28,40-136` (`DeviceForm` has only `user_code`; no `csrf_valid` call); `crates/auth/src/ui/templates/device.html:8-13` (no hidden CSRF field); `crates/auth/src/sessions/login.rs:33-35` (session cookie `SameSite=Lax`); `crates/auth/src/csrf.rs`.

**Issue:** `POST /device` verifies a `user_code` at Hydra and, if the browser carries a valid `__Host-zsidp_session`, calls `accept_device_user_code(...)` — an identity-conferring, state-changing action — with **no CSRF check**. It is the sole exception in the auth form surface (login/signup/consent/forgot/reset/link/magic/verify/me/logout all validate the `__Host-zsidp_csrf` double-submit token). Its only guard is the `SameSite=Lax` session cookie.

**Exploit (contested):** Classic device-flow CSRF — attacker starts their own device flow, obtains a `user_code`, and lures a signed-in victim to auto-submit a cross-site `POST /device` to bind the **attacker's** device to the **victim's** identity. **Both verifiers agree this does not work end-to-end on modern browsers:** `SameSite=Lax` is not sent on cross-site POST navigation, so the request lands anonymous and is bounced to `/login`. The residual is a same-site attacker (XSS / vulnerable sibling subdomain), legacy/non-compliant UAs, or a future form-method/cookie regression — and the inconsistency of relying on incidental Lax for a grant-authorizing endpoint when the platform's own double-submit defense exists precisely because Lax is deemed insufficient. For `skip_consent=true` first-party clients the downstream consent is auto-accepted with no CSRF step, so a successful POST chains straight to token issuance.

**Fix:** Render the `__Host-zsidp_csrf` token into `DeviceForm` + the device template and run `csrf::parse_cookie`/`csrf::matches` before `accept_device_user_code`, exactly like the ten sibling handlers.

**Standard:** RFC 8628 §5.4 (device confirmation is a known CSRF target); RFC 9700 §4.13; OWASP ASVS v4 §4.2.2 / CSRF Prevention Cheat Sheet; OWASP Top 10 A01.

**Verdicts:** 1.1 exploit lens → uncertain (corrected low: Lax neutralizes the cross-site POST); 1.1 mitigation lens → confirmed/medium (Lax is only partial — skip_consent first-party path auto-accepts, Lax is the platform's own rejected sole-defense). 2.0 (duplicate) exploit lens → refuted (low); mitigation lens → uncertain (low). **Lead adjudication: Medium, marked Contested.** The missing token is a real, citable invariant violation worth fixing regardless; the headline cross-site session-hijack is neutralized by Lax on modern browsers, so the live severity hinges on the same-site/skip_consent residual — a human should confirm whether any first-party `skip_consent` client reaches `/device` and whether sibling-subdomain risk is in scope.

---

## LOW

### L1. `GET /session?mint=1` performs server-state-changing family rotation as a GET, Origin check intentionally disabled
**Final severity: Low — Confirmed (defense-in-depth; both lenses uncertain).**

**Location:** `crates/gateway/src/auth_token.rs:643-660,191-263,31-38`; `crates/gateway/src/main.rs:742-746`; `crates/gateway/src/router/dispatch.rs:415-426`.

**Issue:** `GET /session?mint=1` rotates the server-held refresh family (Hydra refresh + rewrite of stored token + DB row) — a state-changing op served over GET, with `require_origin=false` so a missing Origin is tolerated and the custom `X-ZS-Auth` header is the sole CSRF barrier. Contrary to OWASP guidance, state-changing operations should not use GET nor rest on a single custom-header gate.

**Exploit:** No working browser exploit: `X-ZS-Auth` is a non-simple header forcing a CORS preflight the endpoint never approves (no ACAO), a present foreign Origin is rejected, and Sec-Fetch-Site is enforced when present. Residual is non-browser/automation or future relaxation of the no-Origin tolerance.

**Fix:** Make family rotation a POST (or require Origin even on the mint GET — browsers send Origin on same-origin fetch too), keeping the custom-header requirement as an additional layer.

**Standard:** OWASP CSRF Prevention; RFC 9700 CSRF considerations.

**Verdict:** Both lenses → uncertain/low. Browser CSRF blocked by layered controls; the GET-state-change + Origin-optional design deviation stands as best-practice.

---

### L2. Authorize `redirect_uri` override validated by prefix-match rather than exact-match
**Final severity: Low — Confirmed (defense-in-depth).**

**Location:** `crates/gateway/src/browser_auth.rs:116-130` (`supplied.starts_with("{scheme}://{host}/")`); `crates/control/src/app_oauth_client.rs:242-259` (Hydra exact-match registration).

**Issue:** The gateway uses a prefix check, not RFC-required exact-match, against the registered callback. The trailing slash blocks the sibling-domain bypass, and Hydra independently enforces exact-match against the two registered URIs, so the gateway check is a redundant early-reject, not the boundary.

**Exploit:** None while Hydra registration holds — a same-origin-but-unregistered path passes the prefix check but Hydra rejects it. Flagged for the case where Hydra registration ever drifts to a wildcard/broadened set.

**Fix:** Replace the prefix check with exact-match against the app's registered callback set (the gateway already computes the canonical `.../popup-callback` default).

**Standard:** RFC 6749 §3.1.2.3; RFC 9700 §4.1.3; OWASP A01.

**Verdict:** Exploit lens → confirmed/low (Hydra is the real control); mitigation lens → uncertain/low (control neutralizes impact, not the gateway-layer deviation).

---

### L3. Token-redeem interstitial reuses the CSRF token verbatim as the CSP script-nonce
**Final severity: Low — Confirmed (latent footgun; both lenses).**

**Location:** `crates/auth/src/ui/mod.rs:207-224`; `crates/auth/src/ui/templates/token_redeem_interstitial.html:9-24`; `crates/auth/src/csrf.rs:5-6,38-42`; `crates/auth/src/headers.rs:169-184`.

**Issue:** `render_token_interstitial` sets `script-src ... 'nonce-<csrf>'` and the template emits `<script nonce="{{ csrf }}">` — the CSP nonce **is** the CSRF token, which is simultaneously written to a non-HttpOnly cookie and rendered as plaintext form-field values. CSP L3 expects a single-purpose, unexposed nonce.

**Exploit:** None — reading the nonce from cookie/DOM requires already-running script (circular); `script-src 'self'` means the nonce only gates inline script. The risk is conflating two secrets: a future change that logs/lengthens/reflects the CSRF token silently weakens the CSP nonce.

**Fix:** Generate an independent per-response nonce for the CSP `script-nonce`; keep the CSRF token solely for the double-submit field/cookie.

**Standard:** CSP Level 3 nonce guidance (per-response, single-purpose, unpredictable); separation of concerns.

**Verdict:** Both lenses → confirmed/low. No live bypass; surrounding controls (`no-store`, `frame-ancestors 'none'`, fresh per-response token) limit blast radius but don't satisfy the single-purpose-nonce principle.

---

### L4. Password-reset `complete` binds the new password by `users.email` JOIN, not the user_id captured at issue
**Final severity: Low — Contested(needs-human) latent. Both lenses uncertain (one corrected to info).**

**Location:** `crates/auth/src/identity/password_reset.rs:196-237` (`JOIN ... ON u.email = ml.email`); `:87-153` (issue persists email, no user_id); `db/changelog/changesets/0002_auth.sql:8-10` (`email CITEXT UNIQUE NOT NULL`).

**Issue:** `complete()` re-resolves the target by email rather than an immutable user_id captured at issue time. Deviates from OWASP Forgot-Password guidance to bind recovery tokens to an immutable identifier.

**Exploit:** None today — no email-change/account-recycle path exists (the only `DELETE FROM users` are `#[cfg(test)]`), and `email` is UNIQUE so the JOIN is 1:1. Latent: any future email-mutation feature silently turns an outstanding reset token into a cross-account password set.

**Fix:** Capture `user_id` in the reset row at issue and JOIN/filter `complete()` on it; treat email as display-only. Regression test that an email reassignment between issue and complete cannot retarget the reset.

**Standard:** OWASP Forgot Password Cheat Sheet; ASVS 2.5.x.

**Verdict:** Both lenses → uncertain (one corrected to info). A one-line correctness choice, not back-compat infrastructure — worth fixing pre-launch.

---

### L5. No account lockout — leaky-bucket rate limiting is the sole online-guessing defense; `locked_until` is never set
**Final severity: Low — Contested(needs-human). Exploit lens corrected to low; mitigation lens held medium.**

**Location:** `crates/auth/src/identity/credentials.rs:107-137`; `crates/auth/src/ratelimit.rs:21-36`; `crates/auth/src/identity/eligibility.rs:58-62`; `crates/auth/src/ui/signup.rs:104`, `reset.rs:105` (15-char minimum).

**Issue:** The login path enforces three leaky buckets (email+ip 5/15min, email 10/hr, IP 60/hr) but no progressive lockout. `locked_until` is read but **set only in a test fixture** — `LoginIneligible::Locked` is dead for automated brute force.

**Exploit:** A distributed attacker rotating IPs evades the per-IP bucket; the binding constraint is the per-email bucket (~10/hr, ~240/day) — sustained indefinitely against a weak-but-valid password, with no escalation, lockout, notification, or step-up despite `login_failure` audit rows.

**Fix:** Wire a real lockout (per-user failure counter → set `locked_until` with exponential backoff, reset on success — read side already exists), or document rate-limiting as the chosen control and tighten the per-email bucket; at minimum alert/step-up on sustained `login_failure` for one user_id.

**Standard:** OWASP ASVS v4 2.2.1; OWASP Authentication Cheat Sheet; NIST 800-63B 5.2.2.

**Verdict:** Exploit lens → confirmed but corrected low (15-char minimum + per-email throttle satisfies NIST's rate-limit requirement; no concrete compromise). Mitigation lens → uncertain/medium (per-email bucket is partial; weak-but-valid passwords + zero detection response remain). **Marked Contested** — severity hinges on password-policy effectiveness and whether detection/step-up is required.

---

### L6. Empty `worker_key` disables the bearer check and ZeroShip-User HMAC; no strength floor on a non-empty key
**Final severity: Low — Confirmed (both lenses).**

**Location:** `crates/worker/src/handler.rs:29-76`; `crates/worker/src/main.rs:285-298` (loopback-only bind guard); `crates/core/src/auth/mod.rs:81` (HMAC accepts any key length); `crates/core/src/config/secrets.rs:24-110` (no strength validator for worker_key, unlike stash_key/pairwise_salt).

**Issue:** Empty `worker_key` → `check_worker_auth` returns None (auth disabled) and the `ZeroShip-User` HMAC is verified against an empty key any party can compute. No strength/length validation exists; only presence (`require_unless_dev`) + a fail-closed loopback-only bind guard.

**Exploit:** Empty-key forgery is confined to loopback by the bind guard (fail-closed exit on non-loopback). The real residual: a **weak short non-empty key** passes presence-only validation, skips the bind guard entirely, can bind any interface, and is brute-forceable for `ZeroShip-User` HMAC forgery → full per-app user impersonation.

**Fix:** Apply the same ≥32-byte strength validator used for `stash_key`/`pairwise_salt` to `worker_key` outside `--dev-insecure`. Treat empty as "disabled" only under an explicit dev flag, and reject an empty key when a `ZeroShip-User` header is present rather than verifying against a zero-length key.

**Standard:** RFC 8725 (HMAC keys must be high-entropy; empty key must never validate); OWASP ASVS V6.2; fail-closed.

**Verdict:** Both lenses → confirmed/low. Empty-key path well-defended by loopback guard; missing strength floor is operator-misconfig-gated.

---

### L7. Client-supplied headers (incl. forged `ZeroShip-User` / `Authorization`) forwarded verbatim in the worker dispatch envelope
**Final severity: Low — Confirmed (both lenses; attractive-nuisance footgun).**

**Location:** `crates/gateway/src/router/dispatch.rs:1160-1166` (no denylist); `crates/gateway/src/proxy.rs:221-227` (verbatim into envelope `headers`); `crates/worker/src/handler.rs:48-76,227-235` (authoritative identity from separate HMAC channel); `crates/runtime/src/auth.rs:58`.

**Issue:** The gateway forwards all inbound client headers verbatim into the envelope `headers` field exposed as app JS `request.headers`, with no scrubbing of `zeroship-user`, `authorization`, `x-request-id`, etc. The platform identity (`env.auth`) is safe — it comes from the separate request-bound HMAC `ZeroShip-User` channel and cannot be spoofed. Residual: a creator app reading identity from the raw `request.headers` instead of `env.auth` would trust attacker-controlled values.

**Exploit:** `curl ... -H 'ZeroShip-User: <forged>'` or `-H 'Authorization: Bearer admin'` rides into the envelope; an app that authorizes off the raw header (not `env.auth`) grants the attacker that identity.

**Fix:** Strip platform-reserved headers (`zeroship-user`, `authorization`, `x-app-id`, `x-plan-id`, `x-request-id`, any `x-zs-*`) from the envelope `headers` before forwarding.

**Standard:** OWASP ASVS V13.2 (trust-boundary header handling); the AGENTS.md BFF identity invariant.

**Verdict:** Both lenses → confirmed/low. Platform contract sound; the unscrubbed reserved headers are a creator footgun.

---

## INFO / ASSURANCE & LATENT

### I1. (Down-ranked from HIGH) Production Hydra template binds the admin API to `0.0.0.0`
**Final severity: Low — Contested. Both lenses down-ranked HIGH→LOW.**

**Location:** `ops/hydra.yaml:7-9` (`serve.admin.host: 0.0.0.0`, loopback only a comment); `docs/reference/auth.md:131,165`; `docker-compose.yml:381-383`; `docs/runbooks/auth-deploy.md`.

**Issue:** The documented prod Hydra template defaults the unauthenticated admin API (port 4445) to `0.0.0.0`; loopback is operator-discipline comment only, not enforced by any zeroship startup validator (`is_loopback_url` governs only the client-side `--hydra-admin-url`).

**Exploit (down-ranked):** Verbatim copy to prod + an exposed :4445 (misconfigured SG / compromised neighbor / SSRF pivot) → unauthenticated `accept_login`/`accept_consent`, malicious `skip_consent` client registration, token introspection — a full auth bypass. **Both verifiers down-ranked to low:** the actually-mounted config is `hydra-dev.yaml`; the file is a never-mounted documentation template; pre-launch has no prod Hydra; managed-DB bootstrap roles (RDS/Cloud SQL) carry CREATEROLE; and the exploit needs compound operator error.

**Fix:** Invert the default to `serve.admin.host: 127.0.0.1` (safe-by-default, let dev compose override). Additionally enable Hydra admin auth / mTLS rather than relying solely on network isolation.

**Standard:** RFC 9700 §2; OWASP ASVS V1.14/V13; A05:2021.

**Verdict:** Exploit → confirmed/low; mitigation → uncertain/low. Secure-by-default hardening; **a human should confirm the prod deployment path never mounts `ops/hydra.yaml` and firewalls :4445.**

---

### I2. (Down-ranked from MEDIUM) Production Hydra template hardcodes a superuser DSN literal
**Final severity: Low — Contested. Exploit lens refuted (info); mitigation lens uncertain (low).**

**Location:** `ops/hydra.yaml:1` (`postgres://postgres:zeroship@...`); `db/changelog/changesets/0027_oauth_hydra_schema.sql:16-49`; `docker-compose.yml:371`; `docs/runbooks/auth-deploy.md:141-156`.

**Issue:** The prod template embeds the superuser DSN with a plaintext literal password, defeating the `oauth_hydra` least-priv schema isolation if deployed without env override; also commits a (dev) DB password to source, violating references-not-literals.

**Exploit (down-ranked):** A prod Hydra inheriting this DSN runs as superuser with BYPASSRLS over every tenant table and scatters `hydra_*` into the `zeroship` schema. **Refuted/uncertain:** the documented runbook always sets `-e DSN="$AUTH_DB_URL"` (env wins over YAML); the literal is the well-known dev password/hostname and fails closed against a real prod DB; the file is a never-mounted template. Note: the runbook itself uses a shared `$AUTH_DB_URL` and does not wire the `oauth_hydra` least-priv role — a separate gap.

**Fix:** Omit/placeholder the inline `dsn:` (Hydra reads it from env so it fails closed), document that prod MUST inject DSN as a secret reference to the `oauth_hydra` role, and apply the least-priv role in the prod runbook too.

**Standard:** References-not-literals invariant; OWASP ASVS V6/V2.10; A05:2021/A02:2021; RFC 9700 §2.6.

**Verdict:** Exploit → refuted (info); mitigation → uncertain/low. Secrets-hygiene + missing fail-closed default; the committed plaintext (dev) password and the runbook not using the least-priv role are the durable kernels.

---

### I3. (Down-ranked from MEDIUM) Restricted-CI silent-skip in changeset 0025 leaves RLS force-enabled while roles/BYPASSRLS grants are never created
**Final severity: Low — Confirmed both lenses (both corrected MEDIUM→LOW).**

**Location:** `db/changelog/changesets/0025_roles_rls.sql:84-101,213-217,226-270`; `docker-compose.yml:88` (`migrate` runs as `postgres`).

**Issue:** Role creation + grants + BYPASSRLS are wrapped in a DO block that silently `RETURN`s if `current_user` lacks CREATEROLE (and a broad `EXCEPTION WHEN insufficient_privilege` swallows grant failures), while the RLS changesets always run and FORCE row-level security. On a CREATEROLE-less migration principal, the least-priv roles never exist, BYPASSRLS bits are never set, yet RLS is force-enabled — and there is no startup-time assertion of `current_user`/`rolbypassrls`/`rolsuper`.

**Exploit (down-ranked):** A managed-DB principal without CREATEROLE applies the migration "successfully" (only NOTICEs); operators fall back to connecting every service as superuser, bypassing both RLS and grants. **Down-ranked:** the shipped `migrate` service runs as `postgres` (has CREATEROLE), so the path doesn't fire in the delivered config; the likely failure mode is loud ("role does not exist" at boot), not silent; RLS-without-roles fails closed.

**Fix:** Gate the silent-skip behind an explicit liquibase context (`restricted_ci=true`) so it fails the migration in prod, or split role provisioning out and assert at service startup that the connecting role is the intended least-priv role (refuse to boot otherwise).

**Standard:** OWASP ASVS V1.4 (verifiable enforcement points); A05:2021; fail-loud.

**Verdict:** Both lenses → confirmed/low. Real fail-loud/defense-in-depth gap; security impact contingent on managed-DB misconfig absent from shipped config.

---

### I4. (Confirmed assurance) ID-token / logout-token / DPoP alg-confusion + `alg:none` are structurally blocked
**Final severity: Info — Confirmed (positive verification).** *(Merges 1.3 and 6.4.)*

**Location:** `crates/core/src/oidc_verify.rs:291-348,446-517,551-561`; `crates/core/src/logout_token.rs:244-268`; `crates/core/src/dpop.rs:51-59`.

The JWKS loader constructs only asymmetric DecodingKeys (RS*/ES*/EdDSA), skipping HS*/`none` with a warn; key lookup pins `(kid, alg)`, so a forged `alg=HS256` finds no key (no HMAC-with-RSA-pubkey confusion) and `alg:none` fails header decode/key match. iss/aud re-checked post-decode, aud pinned to the per-app client_id, iat ±300s, nbf honored, at_hash/c_hash constant-time. DPoP allowlist asymmetric-only with `typ=dpop+jwt` pinned. **No action required** (optionally pin an explicit allowed-alg list for belt-and-suspenders). Standard: RFC 8725 §3.1; OIDC Core §3.1.3.7; RFC 9449 §4.2.

---

### I5. (Confirmed assurance) Immersive-iframe clickjacking + postMessage defenses are fail-closed
**Final severity: Info — Confirmed (positive verification).**

**Location:** `crates/auth/src/headers.rs:88-146,205-296`; `crates/gateway/src/browser_auth.rs:53-55,226-256`; `sdks/auth/src/internal/relay.ts:113-148`.

Route-aware frame-ancestors fails closed (empty/poison allowlist keeps `XFO: DENY` + `'none'`; un-serializable CSP restores the full strict default incl. re-adding XFO); wildcard/injection bytes rejected at both config and header-builder layers; popup-callback reflects only the server-generated nonce (differential test); postMessage pinned to `location.origin`; SDK relay enforces `ev.origin === expectedOrigin` + per-flow `state`; popup CSP `default-src 'none'`; sensitive responses `cache-control: no-store`. **No action required.** Standard: CSP L3 frame-ancestors; OWASP Clickjacking; HTML5 postMessage origin validation; RFC 7234.

---

### I6. (Confirmed assurance) `/password` oracle + `auth_internal_key` fully removed; redirect sinks constrained
**Final severity: Info — Confirmed (positive verification).** *(Merges 6.3 and 6.5.)*

**Location:** `crates/core/src/config/file.rs:87,469-489` (`deny_unknown_fields` rejects `auth_internal_key`); `sdks/auth/tests/client.test.ts:440-442` (negative test for `/password`); `crates/gateway/src/main.rs:406-411` + `crates/worker/src/main.rs:285-297` (worker_key fail-closed); `crates/gateway/src/browser_auth.rs:114-130`; `crates/gateway/src/router/dispatch.rs:1479,1566-1596`; `crates/gateway/src/auth_token.rs:88-124`.

No code consumes an internal shared secret to bypass auth; gateway→worker trust is solely the HMAC `worker_key`. Redirect sinks are constrained (trailing-slash origin prefix + relative-path sanitizer re-applied at sink + Hydra registered-URI allowlist). **No action required.** *Caveat (worth tracking): the gateway's own redirect host-prefix check is Host-spoofable since `extract_app_name` validates only the first DNS label and no base-domain check exists — Hydra's registered-URI allowlist is the actual backstop; consider adding base-domain canonicalization as defense-in-depth.* Standard: RFC 9700 §4.1; OWASP unvalidated-redirects.

---

### I7. (Latent) SameSite=Strict CSRF cookie inside the immersive iframe is correct only because console + auth share `zeroship.ai`
**Final severity: Info — Confirmed latent (both lenses).**

**Location:** `crates/auth/src/csrf.rs:50-53`; `crates/auth/src/headers.rs:58,89-109`; `crates/auth/src/config.rs:447-507`.

The framed login renders in a cross-origin iframe under the console; the `SameSite=Strict` CSRF cookie is delivered on the in-frame POST only because console + auth share eTLD+1. `is_concrete_frame_ancestor_origin` validates scheme/host shape only and accepts any https origin — including a cross-registrable-domain console. A future cross-site console would break login (Strict cookie withheld) or tempt a `Strict→None` downgrade that re-opens cross-site CSRF. No exploit in the current `*.zeroship.ai` topology. **Fix:** enforce/document that every `frame_ancestor_origins` entry MUST be same-site (eTLD+1) with the auth issuer host. Standard: RFC 6265bis SameSite; OWASP CSRF + SameSite.

---

### I8. (Latent) `is_pairwise_subject` is a `pws_`-prefix shape check, not a forgery gate
**Final severity: Info — Refuted as live issue; valid hardening note.**

**Location:** `crates/core/src/auth/mod.rs:125-131,201-218`; call sites `crates/gateway/src/auth_token.rs:687-691`, `crates/gateway/src/router/auth.rs:1231,1243`.

The predicate returns true for any `pws_<non-empty>`; the real privacy guarantee is that the gateway always re-derives via HMAC `derive_pairwise` and never trusts an inbound `pws_`. Both production call sites run the predicate only **after** cryptographic signature verification, so it is a minter-bug containment check, not a trust gate — refuted as exploitable. **Fix (optional):** tighten the predicate to validate base62 alphabet + `PAIRWISE_SUB_BODY_LEN`, or doc-warn that it is shape-only and never a trust gate. Standard: OWASP ASVS V1.4.

---

### I9. (Latent) Per-process backchannel-logout JTI cache leaves a multi-node replay window
**Final severity: Info — Confirmed (both lenses).**

**Location:** `crates/core/src/logout_token.rs:87-137`; `crates/gateway/src/backchannel_logout.rs:112-126`.

`LogoutJtiCache` is an in-process `Mutex<HashMap>` with no shared store; a captured `logout_token` (no `exp`, ±300s iat) replayed against a different gateway node within 600s isn't deduped and re-triggers revocation. Impact is bounded — BCL only revokes (idempotent), so replay yields nuisance forced-logout, and capture requires a privileged internal vantage; current topology is single-node pre-launch. **Fix (track for multi-node cutover):** back the jti cache with the shared revocation/redis store, or tighten the iat window. Standard: OIDC BCL 1.0 §2.6; RFC 8725 §3.10.

---

### I10. (Latent) `validate_control_key` XOR-fold iterates the shorter slice, leaking expected-length via timing
**Final severity: Info — Confirmed (both lenses).**

**Location:** `crates/core/src/auth/mod.rs:24-44,94-97`; `crates/worker/src/handler.rs:40`.

The comparator XOR-folds over `min_len`, so iteration count can leak the **expected** secret length (never byte content). The two hex-based callers (`validate_api_key`, `verify_hmac_sha256_hex`) compare fixed 64-char operands → no leak; only the raw worker bearer path has attacker-controlled length, leaking only the (non-secret) `worker_key` length against a CSPRNG key. **Fix (low priority):** use a fixed-iteration constant-time compare (`subtle::ConstantTimeEq` or `ring::constant_time::verify_slices_are_equal`). Standard: OWASP ASVS V6.2.x.

---

## Verification Notes

**Checked against the real code on `origin/main` (read-only).** Each finding was independently confirmed by reading the cited files end-to-end; final severities reflect the two-lens (exploit + mitigation) verdicts — down-ranked where a verified mitigating control exists, held where both lenses confirmed exploitability. Two findings refuted by both lenses were excluded upstream.

**Merges performed:** the two password-reset findings (0.0, 3.0) → H1 (same root: no gateway family-marker / anchor delete / Hydra refresh revoke on reset). The two `/device` CSRF findings (1.1, 2.0) → M2. The two alg-confusion assurances (1.3, 6.4) → I4. The two cross-service negative-verification findings (6.3, 6.5) → I6.

**Areas needing a human / live test:**
- **C1 (critical):** Confirm against a running control plane that a fresh creator's console bearer actually returns other creators' apps via `GET /apps` + reads `.../env`/`.../billing` — the code path is unambiguous but a live PoC closes it definitively.
- **M2 (`/device` CSRF, contested):** Verify whether any first-party `skip_consent` client reaches `/device`, and whether sibling-subdomain (`*.zeroship.ai`) XSS is in scope — these determine live severity above the SameSite=Lax floor.
- **L5 (lockout, contested):** A policy decision — confirm whether rate-limiting alone is the accepted control or a real lockout/step-up is required.
- **I1 / I2 (Hydra prod template, contested):** Confirm the production deployment path never mounts `ops/hydra.yaml`, firewalls admin port 4445, injects DSN as a secret reference, and wires the `oauth_hydra` least-priv role in the runbook (the runbook currently uses a shared `$AUTH_DB_URL`).
- **I3 (restricted-CI migration):** Confirm the target managed-DB bootstrap principal carries CREATEROLE; otherwise the role model silently no-ops.
- **H2 (panic DoS):** The covering test is env-gated (`AUTH_DB_URL`) and never runs in CI — a live DB test with one seeded `app_members` row would reproduce the panic and should be added as a non-gated regression test.