# Auth review findings, 2026-08-14

A continuous review pass over auth and the auth UI, verified against a real
Postgres and (for the UI tier) a real browser. Each finding below states how it
was measured, not just what was read. Items marked FIXED landed in the commit
named beside them; items marked OPEN are proven but not yet acted on.

The companion document `2026-08-14-auth-ui-findings.md` covers the browser-tier
accessibility pass. This one covers cookies, test coverage, and documentation
that disagreed with the code.

---

## 1. The CSRF cookie was not `HttpOnly` - FIXED (`ed596318f`)

`crates/auth/src/csrf.rs` set `__Host-zsidp_csrf` without `HttpOnly`, and the
module doc presented that as a requirement:

> Cookie is **NOT** `HttpOnly` - the inline `<script nonce>` reads it for
> the hidden form field (that's the "double-submit" pattern).

No such script exists, and the claim is what stopped anyone checking.

**How it was measured.** Three independent searches, all empty:

1. Every template carrying an inline script (`reset.html`,
   `token_redeem_interstitial.html`, `device_supabase.html`) was read in full.
   They scrub history, auto-submit a form, and drive a GoTrue sign-in. None
   touches `document.cookie`.
2. Every consumer of the cookie in the tree is `csrf::parse_cookie` on the
   server (`login.rs`, `consent.rs`, `forgot.rs`, `reset.rs`, `me.rs`,
   `sessions.rs`, `link.rs`, `totp.rs`, `device.rs`, `account_deletion.rs`).
3. A repo-wide search for `zsidp_csrf` outside `crates/auth` returns only
   documentation and one comment.

`ui::login::render_challenge` (`crates/auth/src/ui/login.rs:258-275`) shows the
actual mechanism: a single `csrf::generate_token()` feeds both the template's
`{{ csrf }}` and `csrf::set_cookie`, in one response. Double-submit needs the
cookie to be SENT, not to be READABLE.

**The tell.** The dev tier had already worked this out and drew the opposite
conclusion. `sdks/bootstrap/src/dev-auth.ts` kept its own dev CSRF cookie
`HttpOnly` and cited this very sentence as the reason production could not:

> prod's cookie is non-HttpOnly because its inline script READS it to populate
> the field; the dev GET renders the token into BOTH the cookie and the field
> server-side, so no JS read is needed and we keep the cookie `HttpOnly` (a
> stricter dev variant).

The mechanism it describes for dev is exactly the mechanism production uses. So
the dev tier was strictly harder to attack than production, on the basis of a
sentence that was never true.

**Fix.** `HttpOnly` added; the module doc, `dev-auth.ts`, and
`docs/reference/auth-dev-tier.md` rewritten to state the real mechanism and to
record that the old claim was false, so the next reader does not re-derive it.

**Verification.** `crates/auth/src/csrf.rs` gained
`set_cookie_is_http_only`, which fails on the pre-fix code printing the exact
cookie:

```
cookie: __Host-zsidp_csrf=tok; Path=/; SameSite=Strict; Secure; Max-Age=3600
```

Full auth gate after the change: `tests/run_auth_suite.sh` reports
`577 tests passed, 0 unexpected skips, 14 allowlisted (floor 505)`.

**Bounding the severity.** This was not remotely exploitable on its own. Reading
the cookie requires script execution on the auth origin, and the CSP there is
`script-src 'self'` plus per-page nonces with no `unsafe-inline`. It is
defence-in-depth that was free and was being declined for a stated reason that
did not hold.

---

## 2. Fourteen gateway auth tests never ran, and could not pass - FIXED (`77de1cd0e`, `5646d3654`)

`GATEWAY_ANCHORS_DB_URL` is set nowhere in the repository, so 13 tests in
`crates/gateway/tests/auth_token_anchors_test.rs` and 1 in
`browser_auth_test.rs` returned without executing. They cover, among other
things:

- code exchange sets both the session and anchor cookies, with no token in the body
- the per-app relay alias is substituted so an app never sees the real email
- missing alias fails closed to an empty email rather than disclosing one
- anchor absolute expiry is `created_at + 30d` and does not slide
- reload recovery mints a fresh cookie with exactly one OP refresh
- back-channel logout revokes a session whose `sid` survived a refresh
- `invalid_grant` deletes the anchor and forces re-login
- a refresh/reset race fails closed with no fresh cookie

`tests/run_auth_suite.sh` documented the situation honestly and deferred it,
recording that pointing the variable at a provisioned database ran 23 tests of
which 11 failed, the first on `initial login must succeed, left: 400`.

**Reproduced exactly.** With the variable pointed at a migrated database:
`12 passed; 11 failed`.

**Root cause - the fixture, not the handler.** The gateway log names it:

```
/token: id_token verify failed error=at_hash missing while access token
binding was requested app=myapp
```

`session_post` calls `verify_id_token(.., Some(&tokens.access_token), ..)`
(`crates/gateway/src/auth_token.rs:478`), which makes `at_hash` mandatory
(`crates/core/src/oidc_verify.rs:513`). The mock OP in the test file never
minted one, and minted its ID token and access token independently, so even a
present hash would not have matched.

This is a stale fixture, not a product defect, and that was checked rather than
assumed: the REAL OP does mint `at_hash`, at
`crates/auth/src/oidc/issuer.rs:536`, using `oidc_at_hash` - SHA-512 over the
access token, leftmost 256 bits, base64url unpadded - which is exactly the
EdDSA branch the verifier computes. Handler correct, production issuer correct,
mock wrong.

**Fix.** `MockOP::id_token` and `MockOP::rotated_id_token` now take the access
token they are issued with and carry `at_hash` over it, and both token-endpoint
arms mint the access token once and reuse that value in the body. The hash comes
from the real OP's own `oidc_at_hash`, so the mock cannot drift from the issuer
it stands in for; that is not circular, because the code under test is
`zeroship_core`'s verifier in a different crate, which recomputes the hash.

**What this does NOT prove.** These tests exercise the gateway against a mock
OP, so they pin the gateway's half of the contract. They would not catch the
real OP dropping `at_hash`, because the mock computes it from the OP's own
helper. That direction is covered by the auth crate's own OIDC suites.

---

## 3. `docs/reference/auth.md` disagreed with the code on cookies - FIXED (`ed596318f`)

Two errors in the same short section, both in the structured part rather than
the prose:

- "All auth cookies are `HttpOnly`, `SameSite=Lax`, and `Path=/`." The
  `HttpOnly` half was false for `__Host-zsidp_csrf` (finding 1, now true). The
  `SameSite` half was false for two cookies: `__Host-zsidp_csrf` and
  `__Host-zeroship_app_anchor` are `Strict`, not `Lax`.
- The table gave `__Host-zeroship_app_session` a lifetime of 12 h. It is
  `SESSION_TOKEN_TTL_SECS`, which is `15 * 60`
  (`crates/gateway/src/session_token.rs:67`). The cookie is a signed
  `zeroship-sess+jwt` assertion whose lifetime is its own `exp`; the durable
  credential is the 30-day anchor.

All eight `SameSite` values were read at their construction sites before the
table was rewritten, not inferred from nearby comments.

---

**Why no gate caught this, which matters more than the bug.** `run_auth_suite.sh`
guards coverage two ways: a MINIMUM passed-count (floor 505) and a skip census.
The floor could not have caught this and never will: a test that gates on a
missing DSN and returns early reports `ok`, so all 13 were counted as PASSING
the whole time. Before: 577 passed, 14 allowlisted. After: 589 passed, 1
allowlisted. The pass count barely moved because those tests were already in it.

Only the census saw them, and only because they announce. The 7 OIDC suites that
gate with a silent `let Some(fx) = ... else { return; }` announce nothing, and
the file already says so. A pass count cannot distinguish a test that ran from a
test that declined to.

---

## 4. `browser_auth_test` is not in the auth gate's binary list - FIXED

`tests/run_auth_suite.sh` enumerated the other `AUTH_DB_URL`-gated binaries and
named `zeroship-gateway:auth_token_anchors_test`, but not
`zeroship-gateway:browser_auth_test`, whose `signout_local_...` test gates on the
same variable. The blanket workspace run builds it; nothing ran its gated body
with a database. Added to the list alongside the `GATEWAY_ANCHORS_DB_URL` export
(`5646d3654`); the gate now reports `589 tests passed, 0 unexpected skips,
1 allowlisted`, and the one remaining skip is the SMTP sink.

---

## 5. A per-app revocation test passed for the wrong reason - FIXED (`fa9762ce7`)

`router::auth::tests::bearer_raw_op_revocation_is_per_app_not_global` failed
against a live database. It is DB-gated in `--lib`, and the gate runs gateway
INTEGRATION binaries only, so nothing ever ran it either.

The first read looked alarming - app A's revocation appearing to revoke app B,
which would mean one app's logout ending sessions in another. It was not that.
The gateway log named the real cause:

```
raw OP Bearer route client_id is not a per-app OAuth client - rejecting
  expected=oac_app_a
raw OP Bearer route client_id is not a per-app OAuth client - rejecting
  expected=oac_app_b
```

BOTH tokens were refused on a shape check before revocation was consulted at
all. A per-app client id is `oac_<base62(app uuid)>`, which the arm parses back
to an app id (`typed_id.rs:348`); the fixture used the placeholders
`oac_app_a`/`oac_app_b`, which do not parse. Its `aud` was wrong too - the arm
requires that app's `app:{app_id}` resource audience, and the fixture sent
`http://api.zeroship.localhost`.

**So the interesting half is not the failure, it is the pass.** The app A arm
asserted "revoked family must reject app A's token" and was satisfied by a token
rejected as MALFORMED. An `Invalid` outcome is consistent with every rejection
reason there is, so on its own it licenses nothing. The property this test names
was never exercised in either direction.

Fixed by using `op_app_binding(OP_APP_A_UUID)` - a helper a sibling test in the
same file already uses correctly - with matching resource audiences, and a fresh
per-run sub (the old fixed sub left a revocation row, so each run's "not
revoked" arm depended on the previous run).

**Confirmed the repaired test discriminates**, rather than trusting green:
revoking app B instead of app A turns it red on the app A assertion, and
restoring it turns it green. Full gateway `--lib`: 404 passed, 0 failed.

**Still open:** `oac_app`, `oac_myapp`, `oac_other` and `oac_revparity` remain as
placeholder client ids elsewhere in `router/auth.rs`. Any test that reaches the
raw-OP-bearer arm with one of those is rejected on the same shape check. Whether
each such test asserts something that survives that rejection has NOT been
checked one by one; the two in `bearer_raw_op_revocation_is_per_app_not_global`
did not.

---

## 6. The `CI` fail-loud guard has never fired - OPEN, low priority

`auth_token_anchors_test.rs` panics if `CI` is set while
`GATEWAY_ANCHORS_DB_URL` is not, so the coverage hole cannot survive in CI. The
mechanism is sound - `declared_env!` resolves to `std::env::var`
(`crates/core/src/config/env.rs:20`), with no registry gate that would swallow
it - but it has never run: the repository has no GitHub Actions history
(`gh run list` returns HTTP 404), so `.github/workflows/ci.yml` is aspirational.
Worth knowing before treating any CI-only guard as load-bearing.

---

## Checked and found sound

Recorded so the next pass does not spend time re-deriving them.

- **Device-flow CSRF.** An archived 2026-06-02 review reported `POST /device`
  approving a device with no CSRF check. It is present now:
  `crates/auth/src/ui/device.rs:151` calls `csrf_valid` before approval, and
  `device.html` renders the token into both forms.
- **The Supabase device page's cross-origin `fetch`es.** The page calls GoTrue
  and control from the browser while the baseline CSP is `connect-src 'self'`,
  which would silently break it. There is a dedicated `supabase_device_csp`
  that widens `connect-src` to exactly those two origins.
- **That page carries no CSRF token, deliberately.** Its approval POST is
  authenticated by a `Bearer` access token obtained in-page from GoTrue and
  sends no cookies, so a cross-site attacker cannot forge it. The test asserting
  the token's ABSENCE is correct.
- **`return_to` open-redirect validation.** `crates/auth/src/return_to.rs`
  rejects scheme-relative, backslash-folded, absolute, and control-character
  forms, and its test table covers the post-decode shapes.
- **Password length parity.** `signup.html` and `reset.html` both use
  `minlength="15"`.
