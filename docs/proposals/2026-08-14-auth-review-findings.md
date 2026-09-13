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

`crates/zeroship-auth/src/csrf.rs` set `__Host-zsidp_csrf` without `HttpOnly`, and the
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

`ui::login::render_challenge` (`crates/zeroship-auth/src/ui/login.rs:258-275`) shows the
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

**Verification.** `crates/zeroship-auth/src/csrf.rs` gained
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

## 2. Gateway auth database cases did not run - FIXED (`77de1cd0e`, `5646d3654`)

The original anchor and browser auth integration targets returned without
executing when `GATEWAY_ANCHORS_DB_URL` was absent. Anchor cases now live in
`crates/zeroship-gateway/src/auth_token/tests/` and own migrated PostgreSQL
containers. Their coverage includes:

- code exchange sets both the session and anchor cookies, with no token in the body
- the per-app relay alias is substituted so an app never sees the real email
- missing alias fails closed to an empty email rather than disclosing one
- anchor absolute expiry follows `ANCHOR_ABS_DAYS` and does not slide
- reload recovery rotates the stored family and mints a fresh cookie
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
(`crates/zeroship-gateway/src/auth_token.rs:478`), which makes `at_hash` mandatory
(`crates/zeroship-core/src/oidc_verify.rs:513`). The mock OP in the test file never
minted one, and minted its ID token and access token independently, so even a
present hash would not have matched.

This is a stale fixture, not a product defect, and that was checked rather than
assumed: the REAL OP does mint `at_hash`, at
`crates/zeroship-auth/src/oidc/issuer.rs:536`, using `oidc_at_hash` - SHA-512 over the
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

## 3. `docs/reference/auth.md` disagreed with the code on cookies - FIXED (`ed596318f`)

Two errors in the same short section, both in the structured part rather than
the prose:

- "All auth cookies are `HttpOnly`, `SameSite=Lax`, and `Path=/`." The
  `HttpOnly` half was false for `__Host-zsidp_csrf` (finding 1, now true). The
  `SameSite` half was false for two cookies: `__Host-zsidp_csrf` and
  `__Host-zeroship_app_anchor` are `Strict`, not `Lax`.
- The table gave `__Host-zeroship_app_session` a lifetime of 12 h. It is
  `SESSION_TOKEN_TTL_SECS`, which is `15 * 60`
  (`crates/zeroship-gateway/src/session_token.rs:67`). The cookie is a signed
  `zeroship-sess+jwt` assertion whose lifetime is its own `exp`; the durable
  credential is the 30-day anchor.

All eight `SameSite` values were read at their construction sites before the
table was rewritten, not inferred from nearby comments.

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

**The other placeholder client ids were then audited, and they are sound.**
`oac_app`, `oac_myapp`, `oac_other` and `oac_revparity` are still used across
`router/auth.rs`, so the obvious worry is that those tests are vacuous too. They
are not, and the reason is structural: the shape check has exactly ONE call site
(`router/auth.rs:710`), and it sits AFTER signature verification and AFTER the
`client_id` binding check (`:697`). A token only reaches it if it is correctly
signed AND its `client_id` claim matches what the route expects.

Each remaining test fails earlier, for the reason it actually names:

- `bearer_non_jwt_token_is_not_user_session` and
  `bearer_unrecognized_iss_jwt_is_not_user_session` assert `NotUserSession`,
  which the shape check never returns (it returns `Invalid`). The client id is
  irrelevant to them.
- The wrong-signing-key test is rejected at signature verification, before the
  arm reads any claim.
- `bearer_raw_op_client_id_mismatch_rejected` presents `oac_app_a` at `oac_app_b`
  and dies at the binding check on `:697`, which is precisely its subject.
- The `scope_cookie_req` tests drive the COOKIE arm, which never calls the shape
  check at all.

`bearer_raw_op_revocation_is_per_app_not_global` was the only test that signed
its token with the JWKS key AND matched the expected client id, so it was the
only one that got as far as `:710`. That is why it alone was vacuous - not bad
luck, but the one fixture that had everything else right.

---

## 6. A failed sign-in was never announced - FIXED (`8fbf5e34f`)

Measured in real Chromium against a live auth server. The accessibility tree
after a failed login:

```text
- generic [ref=e7]: invalid email or password
- textbox "Email" [active] [ref=e10]
- textbox "Password" [ref=e12]
```

A bare `generic` node is not announced, and neither field reported an invalid
state. Someone using a screen reader who typed the wrong password got no signal
that anything had failed - the page simply changed.

All eleven error banners are now `role="alert" id="form-error"`, and login,
signup, forgot and reset wire `aria-invalid` + `aria-describedby` to it. Both
login fields point at the ONE banner deliberately: the message does not say
which of email or password was wrong (enumeration defense), so there is no
per-field message to point at.

The render test's control is the part that matters: on a CLEAN render the
attributes must be ABSENT. Hardcoding `aria-invalid` unconditionally would
satisfy every positive assertion while telling every first-time visitor their
empty form was already wrong.

---

## 7. Every Unlink button had the same name - FIXED (`223db4968`)

`me.html` renders one Unlink button per linked identity with identical visible
text. A screen-reader user tabbing the page hears "Unlink, button" once per
provider with nothing to tell them apart, and the wrong choice is destructive -
it can remove the account's last sign-in method, which the code already guards
against in `refuses_unlink_when_orphans_account`.

**Why the browser spec missed it.** The a11y pass asserted that every form
control HAS an accessible name, and passed: "Unlink" is a name. The property
that matters is that the names are DISTINCT, and a presence check cannot see the
difference. Fixed with `aria-label="Unlink {provider}"`, visible text unchanged.

---

## 8. The consent screen's scope descriptions had no rule - FIXED (`66a1a0989`)

`consent.html` writes `class="scope-desc"` on the sentence that tells a user what
an app is asking permission to do. `style.css` had no rule for it, so it rendered
inline straight after the label: "See invoices See invoices and plan." on one
line. The neighbouring `.scope-tag`, which only marks a scope "(unrecognized)",
WAS styled - the less important of the two got the rule.

Bounded before it was called a defect: descriptions are genuinely populated,
loaded per app-declared scope by `SELECT scope_id, label, description`, so this
renders for real users on the one screen whose entire purpose is understanding
what is being approved.

---

## 9. `.primary` only worked inside `.buttons` - FIXED (`66a1a0989`)

The rule was `.buttons button.primary`, which matches the consent Allow/Deny
pair and NOT the two standalone primary buttons - logout's "Sign out" and
device's "Authorize {app}" - because neither sits in a `.buttons` row.

Invisible today only because the base `button` rule happens to set the same two
declarations. The moment that base style changes, both silently stop being
primary. A class should mean the same thing wherever it is written.

Found by auditing every class in every template against the stylesheet rather
than by reading. Three others came back as intentional unstyled hooks
(`auth-shell` is redundant with the `body` layout; the per-provider oauth
classes would break dark mode, and `.oauth-button` already styles them fully),
and they are named in `template_css_test.rs` with those reasons rather than
silently tolerated.

---

## 10. Six of nine security headers were asserted nowhere - FIXED (`317b95f74`)

The response checks now cover HSTS, content-type sniffing, referrer policy,
permissions policy, cross-origin policies, cache control and framing headers.
`security_headers_test.rs` drives framed and unframed HTTP routes; unit tests
inside `headers.rs` exercise the header writer directly. The Caddy source-text
assertions and exported test-only framing accessor have been retired. Service
response coverage does not verify the deployed reverse proxy's behavior.

The new test caught a wrong assumption on its first run, which is the best thing
it could have done. It was written asserting `X-Frame-Options: DENY` on
`/login`; the fixture configures a console origin, so `/login` takes the FRAMED
arm and correctly DROPS that header. An empty XFO there is right, not missing.
It now drives both arms - framed `/login` and unframed `/forgot` - because
without the unframed partner every assertion in the framed block is equally
consistent with the headers being absent everywhere.

---

## 11. Re-clicking a verification link says "session expired" - OPEN

`crates/zeroship-auth/src/ui/verify.rs:90-102` collapses three outcomes into one arm.
`verification::redeem_and_mark_verified` returns `Ok(None)` when the token is
invalid, when it is expired, AND when it has already been redeemed; all three
render `PublicErrorMessage::SessionExpired` - the words "session expired" plus
`Error code: session_expired`.

The already-redeemed case is not a rare one. People double-click links in mail
clients, forward the mail to themselves, and - the case that costs the most -
some corporate mail scanners fetch every link in an incoming message before the
recipient ever sees it, which consumes the single-use token. The user then
clicks their own link and is told a session expired.

Three things are wrong with that for the user: nothing expired, their email IS
verified, and the page offers no next step. The honest version needs no
weakening of the single-use property and no distinguishing of the three cases -
something closer to "This link is no longer valid. If you have already verified,
sign in." with a link to `/login` would be true of all three arms at once.

**Why this is filed rather than fixed.** There is an in-flight
`docs/proposals/2026-08-14-error-message-quality.md` doing a read-only census of
user-reachable error text across `crates/`, `sdks/` and `libs/`, explicitly
"investigation only; nothing implemented". Rewording one auth error while a
taxonomy for all of them is being designed would pre-empt its conclusions. The
evidence belongs to that decision; whoever lands it should pick this up.

---

## 12. TOTP 2FA cannot be switched on by a user - OPEN (status corrected)

`docs/feature-map.md` marked TOTP 2FA green, which that file's own legend defines
as "Implemented, wired, and exercised end-to-end". It is implemented and it is
exercised - `crates/zeroship-auth/tests/second_factor/enrollment.rs` drives the
account routes and observes credential state and removal notices. It is not WIRED.

The three enrolment routes are POST-only (`/me/2fa/enroll`, `/me/2fa/confirm`,
`/me/2fa/disable`, `server.rs:156-166`), so reaching them needs a caller, and a
repo-wide search finds none outside the routes themselves, the tests, and two
docs. `me.html` - the account page that owns the `/me` namespace those routes
sit under - renders exactly three sections: the profile, linked accounts, and
"Link an account", the last of which even says "coming soon". Two-factor is not
mentioned. So `totp_challenge.html` is reachable only for an account somehow
enrolled by other means.

Corrected to yellow ("Core works, but a documented sub-capability or wiring is
incomplete") with the gap named in the Notes column. The status is what was
fixed; BUILDING the enrolment UI is left open deliberately - it needs a QR or
`otpauth://` render, a confirm step, and one-time backup-code display, and where
that surface belongs (this page, or the console) is a product decision, not a
review one.

**A claim in the first version of this entry was too strong, and the correction
is the more useful finding.** It said "a user cannot turn 2FA on". What was
actually verified is narrower: no caller exists IN THIS REPOSITORY.

Checking the neighbouring rows is what exposed the overreach. Two more green
auth features have the same shape:

- **IdP session management.** `GET /me/sessions` returns JSON
  (`ui/sessions.rs:88`), `POST /me/sessions/{id}/revoke` is POST-only, and
  `me.html` links to neither.
- **GDPR deletion.** `POST /me/delete` and `/me/delete/cancel` are POST-only
  with no template; `me.html` has no delete affordance.

A JSON-returning `GET` is the shape of an API for a single-page console, not of
a page a browser was meant to render. AGENTS.md documents a "Creator Dashboard
(web UI)" living in the separate `zeroship-builder` repository, which is a
plausible consumer for all three - and is not visible from here.

So the honest statement for all three is "no in-repo caller", not "unreachable
by users". The yellow status still stands on the map's own wording, since green
requires "wired" and in-tree they are not, but the Notes now say what was
measured rather than what it implied.

**The general lesson, worth more than the row.** Three features share one
explanation, and finding the third is what made the first one's story fall
apart. A single instance invites the most alarming reading that fits it; the
pattern across siblings is what bounds it. Checking whether a finding has
neighbours should come BEFORE deciding what it means, not after.

Two greps missed this before one found it, which is worth recording: the routes
are `/me/2fa/*`, not the `/totp/*` the handler module name suggests. Searching
for the module's name rather than the route's spelling returns nothing and reads
exactly like "no callers, as expected".

---

## 13. Anchor database verification depended on an environment guard — resolved

Anchor cases now run in the gateway library under ordinary `cargo test`.
Private fixtures in `crates/zeroship-gateway/src/auth_token/tests/` own their
migrated PostgreSQL containers. Missing Docker or failed migrations fail the
run; database verification no longer depends on `CI` or a configured URL.

---

## Checked and found sound

Recorded so the next pass does not spend time re-deriving them.

- **Session fixation.** `threat_model.rs::session_id_rotates_post_login_success`
  drives two logins and asserts the cookie value differs. It runs in the gate.
- **Magic link.** 15-minute token TTL matched by the cookie `Max-Age`, an
  `HttpOnly` per-device CSRF nonce, and the nonce cookie cleared after a
  successful redeem so it cannot be replayed. Three dedicated test files.
- **Rate limits.** Every bucket's doc comment was checked against its numbers
  (`LOGIN_EIP` 5/900s = 5 per 15 min, `TOTP_VERIFY` 5/900s, and so on). No
  drift, and the per-(email, ip) plus per-account layering means rotating IPs
  still hits the account-level backstop.
- **`/logout`.** A GET confirmation page whose action is a POST carrying the
  CSRF token - not a GET side effect.
- **TOTP challenge input.** No `maxlength`/`pattern`, correctly: the field also
  accepts backup codes, which are longer than six digits.
- **`can_grant == false` on consent.** Not a dead end; the handler sets
  `grant_error` so the page explains itself.

- **Device-flow CSRF.** An archived 2026-06-02 review reported `POST /device`
  approving a device with no CSRF check. It is present now:
  `crates/zeroship-auth/src/ui/device.rs:151` calls `csrf_valid` before approval, and
  `device.html` renders the token into both forms.
- **The Supabase device page's cross-origin `fetch`es.** The page calls GoTrue
  and control from the browser while the baseline CSP is `connect-src 'self'`,
  which would silently break it. There is a dedicated `supabase_device_csp`
  that widens `connect-src` to exactly those two origins.
- **That page carries no CSRF token, deliberately.** Its approval POST is
  authenticated by a `Bearer` access token obtained in-page from GoTrue and
  sends no cookies, so a cross-site attacker cannot forge it. The test asserting
  the token's ABSENCE is correct.
- **`return_to` open-redirect validation.** `crates/zeroship-auth/src/return_to.rs`
  rejects scheme-relative, backslash-folded, absolute, and control-character
  forms, and its test table covers the post-decode shapes.
- **Password length parity.** `signup.html` and `reset.html` both use
  `minlength="15"`.
