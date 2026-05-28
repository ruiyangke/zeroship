# Round 1 — Auth flow state machines: findings

Total: 12 findings (1 critical, 8 high, 3 medium, 0 low).

## CRITICAL

### C1. Password reset leaves Hydra SSO sessions alive
**File:** `crates/auth/src/ui/reset.rs:228-260`, `crates/auth/src/ui/login.rs:60-68`
**Severity rationale:** A password reset is the account-recovery boundary users rely on to evict compromised browsers. The reset transaction deletes local `auth.sessions`, gateway sessions, console sessions, and reset tokens, but never calls Hydra's login-session revocation API. A browser with an existing Hydra remembered login can start a fresh OIDC flow after the reset; Hydra returns `skip=true`, and `/login` immediately `accept_login`s the stale subject without checking any local revocation state.
**Reproducer:** Sign in to an RP with `remember=true` so Hydra has a login session. From another browser, redeem a password reset token for the same user. In the original browser, start a new authorization request. `admin.get_login` returns `skip=true`, `login.rs` accepts the login at lines 60-68, and the RP receives a fresh authorization result without the new password.
**Suggested fix:** Add Hydra session revocation to the password-reset completion path, using the same subject string passed to `accept_login` (`user_id.to_string()`) and `HydraAdmin::delete_login_sessions`. Treat failure to revoke Hydra as a reset-completion failure or persist an outbox retry that blocks skip sessions until it succeeds. Also make the login skip path validate the local user row and reject stale/locked/revoked subjects before calling `accept_login`.

## HIGH

### H1. Account locks are bypassed by non-password session minting paths
**File:** `crates/auth/src/ui/login.rs:60-68`, `crates/auth/src/ui/magic.rs:439-568`, `crates/auth/src/ui/magic.rs:757-857`, `crates/auth/src/ui/oauth_google.rs:264-377`, `crates/auth/src/ui/oauth_github.rs:255-361`
**Severity rationale:** `locked_until` is enforced in password login and `/link`, but every other session minting path ignores it. A locked user can still get a session through Hydra's skip path, an already-linked OAuth provider, or a magic link. That makes account lockout inconsistent and bypassable.
**Reproducer:** Create a user, link a GitHub or Google identity, then set `auth.users.locked_until = NOW() + INTERVAL '1 hour'`. Password login fails because `login.rs` falls back to the dummy hash, but an OAuth callback for the linked provider proceeds to `sessions::create`, and a magic link for the same email proceeds through `find_or_create_magic_user` to session creation. If the user already has a Hydra login session, `/login` skip accepts it with no DB check.
**Suggested fix:** Centralize a login eligibility check that loads the user by id/email and rejects `locked_until > NOW()` before every `sessions::create` and before the Hydra skip accept path. Use the same helper in password, magic, OAuth, link, device, and any future session minting handlers so account state has one source of truth.

### H2. `/link` is an unthrottled password oracle for collided OAuth accounts
**File:** `crates/auth/src/ui/link.rs:141-221`, `crates/auth/src/identity/linker.rs:275-302`
**Severity rationale:** The account-link confirmation form verifies the existing local password but has no rate-limit bucket, failed-attempt counter, lockout integration, or pending-token consumption on failure. The token is stateless and reusable until expiry. A domain-re-registration attacker who can authenticate to the upstream provider for a collided email receives a valid `/link?token=...` and can brute-force the victim's local password through a separate endpoint from `/login`, bypassing the login buckets.
**Reproducer:** Seed a local user with `password_hash` for `victim@example.com`. Complete an OAuth callback as a provider account that reports the same verified email; the callback returns `NeedsConfirmation` and redirects to `/link?token=...`. GET `/link` to obtain a CSRF cookie, then POST password guesses repeatedly. Each failure returns a fresh form and token; no `ratelimit::consume` call or attempt counter runs.
**Suggested fix:** Add rate limits keyed by pending user id, token hash, and IP before Argon2 verification, and feed failures into the same lockout policy used by password login. Prefer a server-side `pending_links` table with attempts, expiry, and atomic consume instead of a purely stateless token; consume or cool down the pending link after repeated failures.

### H3. OAuth and link flows commit local side effects before validating the Hydra challenge outcome
**File:** `crates/auth/src/ui/oauth_google.rs:238-340`, `crates/auth/src/ui/oauth_github.rs:229-328`, `crates/auth/src/ui/link.rs:224-284`
**Severity rationale:** OAuth callbacks and link confirmation create users, insert provider identities, and create local sessions before the Hydra `login_challenge` is known to be acceptable. If the challenge is bogus, expired, or already used, `accept_login` fails after local auth state has changed. In the `/link` case this can permanently attach the attacker's provider subject to the victim after a correct password, even though the user only sees an error page.
**Reproducer:** Start `/oauth/github/start?login_challenge=bogus` and complete GitHub. For a new email, `resolve_or_link` creates `auth.users` and `auth.identities`, then `sessions::create` succeeds, and only then `accept_login` fails. For an email collision, use the returned `/link?token=...`, enter the correct local password, and observe `identities::link` succeeds before the stale challenge fails. A later valid OAuth flow for that provider subject now logs in as the linked user.
**Suggested fix:** Validate the Hydra login challenge with `get_login` at OAuth start and again immediately before local side effects. For `/link`, store pending links server-side and make provider-link finalization idempotent and contingent on a still-valid challenge. If local state must be written before `accept_login`, add compensating rollback for identity/session rows on failure and tests for expired/invalid challenges.

### H4. Concurrent token issuance can leave multiple active magic, reset, or verification tokens
**File:** `crates/auth/src/identity/magic_link.rs:105-129`, `crates/auth/src/identity/password_reset.rs:80-118`, `crates/auth/src/identity/verification.rs:66-96`
**Severity rationale:** Each issuer implements "one active token" as `UPDATE old rows consumed_at=NOW()` followed by `INSERT new row`, with no transaction, advisory lock, or partial unique constraint. Two concurrent requests for the same email/user can both run the superseding update before either insert commits, then both insert fresh unconsumed tokens. This breaks the documented single-active-token invariant for login, reset, and verification links.
**Reproducer:** Fire two concurrent `/magic/start` POSTs for the same email on separate DB connections. If both execute the superseding `UPDATE` before either inserts, both email links contain unconsumed login-purpose rows and both can be redeemed. The same interleaving works for two `/forgot` POSTs and for any future verification resend.
**Suggested fix:** Enforce the invariant in the database. Use a transaction plus `pg_advisory_xact_lock` keyed by `(email,purpose)` or add a partial unique index such as one unconsumed row per `(email,purpose)` and retry on conflict. Keep the supersede and insert in one atomic unit for all three issuers.

### H5. Password reset does not invalidate outstanding login-purpose magic links or completions
**File:** `crates/auth/src/ui/reset.rs:250-256`, `crates/auth/src/identity/magic_link.rs:149-197`, `crates/auth/src/ui/magic.rs:698-857`
**Severity rationale:** Reset completion only marks `purpose='reset'` rows consumed. Any previously issued login-purpose magic link for the same email remains redeemable after the password changes and after sessions are revoked. A password reset therefore does not close all bearer credentials that can mint a new session for the account.
**Reproducer:** Request a magic login link for a user but do not click it. Request and redeem a password reset for the same email. After reset completes, click the old magic login link; `magic_link::redeem_pending` still finds the `purpose='login'` row and the magic handler mints a new session.
**Suggested fix:** In the password-reset transaction, consume every outstanding `auth.magic_links` row for the email that can authenticate the user, including `purpose='login'`, and expire/delete `auth.magic_completions` for that email. Longer term, add a `password_changed_at` or credential epoch check so any login token issued before the reset is rejected even if a row was missed.

### H6. Cross-device magic-link GET consumes the login token before the user completes login
**File:** `crates/auth/src/ui/magic.rs:371-473`, `crates/auth/src/ui/magic.rs:574-624`
**Severity rationale:** A GET to `/magic/verify` without the requesting-device cookie takes the cross-device branch, creates a completion code, and finalizes the magic token as consumed before any code is entered on the original device. Email security scanners and link previewers commonly fetch links without cookies; they can burn the user's only login link and, for unknown emails, create a verified local user row just by fetching the URL.
**Reproducer:** Request a magic link. Before the user clicks it, have a scanner or curl fetch `/magic/verify?token=...&login_challenge=...` without the `zsidp_magic_csrf` cookie. The handler creates `auth.magic_completions`, calls `magic_link::finalize_consume`, and renders the code to the scanner. When the user later opens the email link, `redeem_pending` returns invalid/expired.
**Suggested fix:** Make the first cross-device GET scanner-safe. Do not finalize the login token or create a verified user until an explicit user gesture, such as a POST from an interstitial, or until `/magic/complete` succeeds. Same-device cookie-present redemption can remain one-click; cookie-missing redemption should be a pending, non-consuming preview state with short expiry.

### H7. Wrong-code attempts can race a correct in-flight magic completion and corrupt finalization
**File:** `crates/auth/src/ui/magic.rs:810-831`, `crates/auth/src/ui/magic.rs:977-1040`
**Severity rationale:** `consume_pending` first checks for an in-flight reservation, then attempts the correct-code update. If a correct submission reserves the row after another request's in-flight check but before that request reaches the wrong-code update, the wrong-code update still matches because it does not exclude `consumed_pending_at IS NOT NULL`. Enough racing wrong attempts can set `consumed_at` while the correct handler is between `accept_login` and `finalize_consume`, causing Hydra to be accepted but the user to receive an error with no session cookie.
**Reproducer:** Create a cross-device completion row. Submit the correct code and several wrong-code POSTs concurrently for the same `csrf_nonce`, arranged so wrong requests pass the initial in-flight SELECT before the correct request sets `consumed_pending_at`. The wrong-code updates increment attempts and can set `consumed_at`; then the correct request's `finalize_consume` returns false after `accept_login` already succeeded.
**Suggested fix:** Make the wrong-code update ignore actively reserved rows by adding the same stale-pending predicate used by the correct-code update. Better, collapse the in-flight check and mutation into one statement that returns `InFlight`, `WrongCode`, or `Reserved` under row lock semantics. Add a regression test with concurrent correct and wrong completions.

### H8. Logout revokes by Hydra `sid` instead of the actual local session cookie
**File:** `crates/auth/src/ui/logout.rs:133-143`, `crates/auth/src/ui/logout.rs:173-184`
**Severity rationale:** The local `auth.sessions` id is generated by `sessions::create` and placed in `__Host-zsidp_session`; the login `accept_login` calls do not pass that UUID to Hydra as the Hydra session id. Logout assumes `info.sid` is the local UUID and revokes by that value, so the real local session row can remain valid until expiry. Clearing the browser cookie is not enough if the cookie was stolen or copied before logout.
**Reproducer:** Sign in and save the `__Host-zsidp_session` cookie value. Perform RP-initiated logout. If Hydra's `sid` is not the local UUID, `Uuid::parse_str(&info.sid)` either skips revocation or revokes no row, while the response only clears the browser cookie. Reuse the saved local cookie against `/me`; `sessions::validate` can still slide and accept it.
**Suggested fix:** Parse the local session cookie from the logout POST request and revoke that `auth.sessions` row directly. If Hydra's `sid` also needs to map to local sessions, store that mapping explicitly at login instead of assuming the ids match.

## MEDIUM

### M1. Starting a magic login invalidates password-reset tokens for the same email
**File:** `crates/auth/src/identity/magic_link.rs:105-113`
**Severity rationale:** `magic_link::issue` consumes all unconsumed `auth.magic_links` rows for the email without filtering by purpose. A login magic-link request therefore invalidates active password-reset links. Because `/magic/start` is unauthenticated aside from CSRF and rate limits, an attacker who knows a victim's email can interfere with account recovery by repeatedly starting magic-login flows.
**Reproducer:** Request a password reset for `victim@example.com` and keep the reset link unused. From any browser with its own CSRF token, POST `/magic/start` for `victim@example.com`. The `UPDATE auth.magic_links SET consumed_at=NOW() WHERE email=$1 AND consumed_at IS NULL` consumes the reset row. The original `/reset` link now reports invalid/expired.
**Suggested fix:** Scope superseding to the token purpose being issued: `WHERE email = $1::citext AND purpose = $2 AND consumed_at IS NULL`. Keep cross-purpose invalidation only in explicit high-security events like password reset completion, where it is deliberate and audited.

### M2. Reset and verification tokens are consumed before downstream state changes succeed
**File:** `crates/auth/src/ui/reset.rs:91-151`, `crates/auth/src/ui/verify.rs:39-75`, `crates/auth/src/identity/password_reset.rs:134-148`, `crates/auth/src/identity/verification.rs:108-124`
**Severity rationale:** Both reset and verification redeem the one-shot token first, then perform follow-up work that can fail. If the user lookup, password hash, password update transaction, or `email_verified_at` update fails, the token is already consumed and cannot be retried. This is especially brittle for verification because the code comments refer to a future resend path that does not exist in these handlers.
**Reproducer:** Redeem a valid reset token while forcing `users::find_by_email` or `complete_password_reset` to fail after `password_reset::redeem` updates `consumed_at`. The user receives "internal error" or "user not found", but a retry with the same link is invalid. Likewise, force the `UPDATE auth.users SET email_verified_at` in `/verify` to fail after `verification::redeem`; the email remains unverified and the token is spent.
**Suggested fix:** Use a pending/finalize token state like magic links, or update the user and consume the token in a single transaction. For verification, a single `UPDATE ... FROM auth.email_verifications ... RETURNING` can mark the user verified and consume the token atomically. For reset, reserve the token, hash the password, then finalize token consumption only inside the password/session revocation transaction.

### M3. Signup treats every user-creation database error as duplicate-email success
**File:** `crates/auth/src/ui/signup.rs:156-166`, `crates/auth/src/ui/signup.rs:241-244`
**Severity rationale:** The signup handler intentionally hides duplicate emails, but it currently swallows all `users::create` errors and redirects to `/login` as if account creation succeeded. A transient database error, missing extension, permission issue, or non-unique constraint failure leaves the user with no account and no verification email while the UI reports the normal path.
**Reproducer:** Make `users::create` return a non-`23505` database error, for example by revoking insert privilege or forcing a transient DB failure after rate-limit succeeds. The handler logs "duplicate or otherwise", sets `created = None`, skips verification issuance, and redirects to `/login`; the submitted credentials cannot sign in.
**Suggested fix:** Preserve enumeration resistance only for the duplicate-email SQLSTATE. Inspect the underlying DB error and return the uniform signup success path for `23505`; for all other database errors, render the generic support error and emit an audit event so operators can distinguish real signup failures from existing-account submissions.

## LOW

No low findings.

## Areas reviewed and clean

- `crates/auth/src/ui/login.rs::post` — password verification uses Argon2 through `spawn_blocking`, with dummy-hash padding for absent, locked, and OAuth-only users.
- `crates/auth/src/ui/signup.rs::post`, `forgot.rs::post`, `reset.rs::post`, `link.rs::post`, `logout.rs::post`, `magic.rs::start`, and `magic.rs::complete` — state-mutating POSTs perform CSRF verification against the shared CSRF cookie.
- `crates/auth/src/csrf.rs` — CSRF cookies are host-prefixed in production and use `SameSite=Strict`; token comparison is constant-time for equal-length inputs.
- `crates/auth/src/sessions/login.rs` and `crates/auth/src/ui/oauth_stash.rs` — session and OAuth stash cookies set `Path=/`, `HttpOnly`, `SameSite=Lax`, and `Secure` in production.
- `crates/auth/src/ui/oauth_google.rs::callback` — callback verifies the signed stash, `state`, PKCE verifier, Google ID-token signature, and OIDC nonce before linking.
- `crates/auth/src/ui/oauth_github.rs::callback` — callback verifies the signed stash, `state`, PKCE verifier, and uses GitHub's verified primary email selection before linking.
- `crates/auth/src/identity/password_reset.rs::redeem` and `crates/auth/src/identity/verification.rs::redeem` — token redemption itself uses atomic `UPDATE ... RETURNING` single-use semantics.
- `crates/auth/src/identity/magic_link.rs::redeem_pending` — login token reservation uses atomic `UPDATE ... RETURNING` and prevents concurrent double reservation within the pending window.
