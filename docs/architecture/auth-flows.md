# Auth Flows and Trust Boundaries

Zeroship auth crosses two systems. In the creator platform, Auth authenticates
humans and acts as the OIDC provider, Gateway gives the browser an app-scoped
BFF session, and Control accepts bearer credentials and makes creator
authorization decisions. In the app runtime, Gateway validates an end-user
credential and converts it into a request-bound identity assertion that Worker
verifies before exposing it through `env.auth`. Control is a bearer-only resource
server, not an OIDC RP (`crates/control/src/lib.rs:507-510`,
`crates/control/src/authz_guard.rs:36-44`). The shipped topology currently seeds
no console and routes its host to Gateway's not-found response
(`deploy/ops/Caddyfile:58-65`).

## Evidence convention

`VERIFIED` means the cited implementation, its caller, and the relevant data or
validation path were read at HEAD `adc8be6200b40c8d4007829a6c591f9d2a075184`.
`INFERRED` means a stated consequence follows from those verified mechanics but
was not demonstrated by an executed end-to-end test. File references are
`path:line` or `path:start-end`. An absence claim in FINDINGS names both the
scoped search and the positive route, caller, or state-machine inventory that
was inspected; an empty search alone is never treated as proof.

In diagrams, a process "holds" a secret when its configuration loads or uses
that value. Shipped container-readable filesystem custody is broader than this
process-level map and is called out separately in Finding 6.

## 1. Creator browser auth

Auth owns the human credential checks and the auth-origin IdP session. Gateway
owns the app-origin BFF session. These are different cookies and different
signers.

### 1.1 Signup

```text
+--------------------------------------------------------------+
| Browser holds password and CSRF value                        |
+--------------------------------------------------------------+
                               |
                               v
+--------------------------------------------------------------+
| TRUST BOUNDARY: browser to Auth public origin                |
+--------------------------------------------------------------+
                               |
                               v
+--------------------------------------------------------------+
| Auth decides rate limits, password policy, and duplicate UX  |
| Auth hashes password and creates verification token          |
+--------------------------------------------------------------+
                               |
                               v
+--------------------------------------------------------------+
| Postgres stores user password token hash and session row     |
+--------------------------------------------------------------+
                               |
                               v
+--------------------------------------------------------------+
| Auth sets opaque IdP cookie and sends 24h raw token by email |
| No token is signed; Postgres session row is authoritative    |
+--------------------------------------------------------------+
```

VERIFIED walk-through:

1. Signup validates double-submit CSRF, normalizes the email, applies IP and
   email limits, enforces the 15-character password policy, runs Argon2id, and
   uses a uniform duplicate-account response (`crates/auth/src/ui/signup.rs:87-200`,
   `crates/auth/src/identity/password.rs:17-35`).
2. Auth stores only a SHA-256 hash of a 32-byte verification token and gives it a
   24-hour lifetime (`crates/auth/src/identity/verification.rs:34-35`,
   `crates/auth/src/identity/verification.rs:56-125`).
3. Signup sends verification best effort and creates an IdP session even if mail
   delivery fails; verification is deliberately decoupled from login
   (`crates/auth/src/ui/signup.rs:202-278`).

### 1.2 Email verification

```text
+----------------------------------------------------------+
| Mailbox holds raw 24h verification token                 |
+----------------------------------------------------------+
                             |
                             v
+----------------------------------------------------------+
| TRUST BOUNDARY: mailbox and browser to Auth              |
+----------------------------------------------------------+
                             |
                             v
+----------------------------------------------------------+
| Auth renders safe GET interstitial; browser posts token  |
+----------------------------------------------------------+
                             |
                             v
+----------------------------------------------------------+
| Auth decides live one-use hash and marks email verified  |
| No signature; the token-hash row is authoritative        |
+----------------------------------------------------------+
```

VERIFIED walk-through:

1. Auth permits one live verification token per user and stores its SHA-256 hash,
   while the mailbox receives the raw 24-hour token
   (`crates/auth/src/identity/verification.rs:34-35`,
   `crates/auth/src/identity/verification.rs:56-125`).
2. The email link renders a GET interstitial, while the POST atomically consumes
   the live hash and sets `email_verified_at`
   (`crates/auth/src/ui/verify.rs:45-129`,
   `crates/auth/src/identity/verification.rs:153-203`).

### 1.3 Password login

```text
+----------------------------------------------------------+
| Browser holds password and CSRF value                    |
+----------------------------------------------------------+
                             |
                             v
+----------------------------------------------------------+
| TRUST BOUNDARY: browser to Auth public origin            |
+----------------------------------------------------------+
                             |
                             v
+----------------------------------------------------------+
| Auth decides password, lifecycle, lockout, and rate gate |
+----------------------------------------------------------+
                             |
                             v
+----------------------------------------------------------+
| Auth creates IdP session or signs 5m factor challenge    |
+----------------------------------------------------------+
                             |
                             v
+----------------------------------------------------------+
| Browser receives opaque session or factor-one cookie     |
+----------------------------------------------------------+
```

VERIFIED walk-through:

1. `/login` resolves SSO hints, validates CSRF, and delegates password checking
   to a shared verifier with IP/account limits, a dummy Argon2 path, lifecycle
   checks, lockout, and audit (`crates/auth/src/ui/login.rs:35-86`,
   `crates/auth/src/ui/login.rs:129-248`,
   `crates/auth/src/identity/credentials.rs:81-344`).
2. Password-only login creates the session immediately. TOTP-enabled login
   instead creates a 5-minute HMAC challenge bound to user, credential version,
   return target, and completed factor
   (`crates/auth/src/ui/login.rs:283-348`,
   `crates/auth/src/sessions/totp_challenge.rs:24-149`).

### 1.4 TOTP login challenge

```text
+----------------------------------------------------------+
| Browser holds Auth-signed factor-one cookie and code     |
+----------------------------------------------------------+
                             |
                             v
+----------------------------------------------------------+
| TRUST BOUNDARY: browser to Auth public origin            |
+----------------------------------------------------------+
                             |
                             v
+----------------------------------------------------------+
| Auth holds HMAC and TOTP AES keys; verifies code         |
+----------------------------------------------------------+
                             |
                             v
+----------------------------------------------------------+
| Auth consumes backup if used and creates IdP session     |
+----------------------------------------------------------+
```

VERIFIED: `/login/2fa` rechecks the challenge version and limits, decrypts the TOTP
   secret, accepts one adjacent 30-second step, or atomically consumes an
   Argon2-backed backup code before creating the IdP session
   (`crates/auth/src/ui/login.rs:351-580`,
   `crates/auth/src/identity/totp.rs:90-117`,
   `crates/auth/src/identity/totp.rs:166-230`).

### 1.5 TOTP enrollment and management

```text
+------------------------------------------------------------+
| Browser holds live IdP cookie and CSRF                     |
+------------------------------------------------------------+
                              |
                              v
+------------------------------------------------------------+
| TRUST BOUNDARY: browser to Auth public origin              |
+------------------------------------------------------------+
                              |
                              v
+------------------------------------------------------------+
| Auth process holds AES key; DB stores encrypted secret     |
| Browser receives raw provisioning secret                   |
+------------------------------------------------------------+
                              |
                              v
+------------------------------------------------------------+
| Auth decides session CSRF and any required reauth          |
| Confirm returns one-time plaintext backup codes            |
| Confirmed replacement or disable requires reauth           |
| No credential is signed in this management flow            |
+------------------------------------------------------------+
```

VERIFIED: enroll, confirm, and disable all require a live IdP session plus CSRF.
First enrollment and a pending replacement expose the provisioning secret
without reauth; replacing a confirmed factor or disabling it requires current
TOTP or password reauth (`crates/auth/src/ui/totp.rs:65-197`,
`crates/auth/src/ui/totp.rs:199-375`). The browser receives the raw provisioning
secret and, after confirmation, the one-time plaintext backup codes
(`crates/auth/src/ui/totp.rs:134-195`, `crates/auth/src/ui/totp.rs:261-296`). Auth
holds separate stash-HMAC and TOTP-AES keys (`crates/auth/src/config.rs:128-151`).

### 1.6 IdP session validation

```text
+----------------------------------------------------------+
| Browser holds opaque Secure HttpOnly Lax session ID      |
+----------------------------------------------------------+
                             |
                             v
+----------------------------------------------------------+
| TRUST BOUNDARY: browser to Auth public origin            |
+----------------------------------------------------------+
                             |
                             v
+----------------------------------------------------------+
| Auth decides row revocation version lifecycle and age    |
| Postgres atomically slides the 30m idle deadline         |
+----------------------------------------------------------+
                             |
                             v
+----------------------------------------------------------+
| Auth accepts until 12h absolute deadline or revocation   |
| No signature; the Postgres session row is authoritative  |
+----------------------------------------------------------+
```

VERIFIED: the `__Host-zsidp_session` cookie is an opaque UUID with Secure, HttpOnly,
   and SameSite=Lax attributes. Its row has a 30-minute sliding idle deadline
   and 12-hour absolute deadline
   (`crates/auth/src/sessions/login.rs:1-39`). Validation atomically slides the
   idle window and checks revocation, credential version, and lifecycle
   deadlines (`crates/auth/src/store/sessions.rs:35-118`).

### 1.7 Magic-link login

```text
+--------------------------------------------------------------+
| Browser starts with email, CSRF, and approved authorize URL  |
+--------------------------------------------------------------+
                               |
                               v
+--------------------------------------------------------------+
| TRUST BOUNDARY: browser and later mailbox to Auth            |
+--------------------------------------------------------------+
                               |
                               v
+--------------------------------------------------------------+
| Auth rate limits and stores token hash plus device nonce     |
+--------------------------------------------------------------+
                               |
                               v
+--------------------------------------------------------------+
| Mailbox holds raw 15m token                                  |
+--------------------------------------------------------------+
                               |
                               v
+--------------------------------------------------------------+
| Auth reserves token on POST and decides same vs cross        |
| Same device proves nonce; cross device proves 5m code        |
+--------------------------------------------------------------+
                               |
                               v
+--------------------------------------------------------------+
| If TOTP applies Auth signs a 5m factor challenge             |
+--------------------------------------------------------------+
                               |
                               v
+--------------------------------------------------------------+
| Auth verifies TOTP then creates opaque IdP session           |
| No final token signature; the session row is authoritative   |
+--------------------------------------------------------------+
```

VERIFIED walk-through:

1. Start accepts only an authorization-shaped return target, validates CSRF,
   applies email/IP limits, and creates a raw 32-byte token plus a device nonce
   (`crates/auth/src/ui/magic.rs:119-171`,
   `crates/auth/src/ui/magic.rs:177-383`).
2. Postgres stores the token hash rather than the raw token, plus the plaintext
   device nonce and request metadata. It permits one active token per email and
   purpose and gives it a 15-minute life; redemption uses a 5-second in-flight lease
   (`crates/auth/src/identity/magic_link.rs:1-27`,
   `crates/auth/src/identity/magic_link.rs:83-307`).
3. The email GET is an interstitial. The POST reserves the token, checks the
   same-device nonce, finds or creates the user, and rechecks lifecycle
   (`crates/auth/src/ui/magic.rs:441-650`). Unknown email creates an already
   verified account (`crates/auth/src/ui/magic.rs:1318-1347`).
4. A same-device completion can require TOTP before session issue. A cross-device
   completion additionally requires a six-digit, 300-second code, rate limits,
   attempt bounds, and target binding
   (`crates/auth/src/ui/magic.rs:665-784`,
   `crates/auth/src/ui/magic.rs:786-819`,
   `crates/auth/src/ui/magic.rs:891-953`,
   `crates/auth/src/ui/magic.rs:965-1275`).

### 1.8 Password reset

```text
+--------------------------------------------------------+
| Browser requests reset; mailbox holds raw 60m token    |
+--------------------------------------------------------+
                            |
                            v
+--------------------------------------------------------+
| TRUST BOUNDARY: browser and mailbox to Auth            |
+--------------------------------------------------------+
                            |
                            v
+--------------------------------------------------------+
| Auth verifies CSRF, limits, live token, password rule  |
| Auth hashes new password and decides reset             |
+--------------------------------------------------------+
                            |
                            v
+--------------------------------------------------------+
| One transaction bumps credential version and revokes   |
| refresh, gateway families, anchors, and sessions       |
+--------------------------------------------------------+
                            |
                            v
+--------------------------------------------------------+
| Auth clears authentication state and redirects login   |
| No credential is signed in this reset flow             |
+--------------------------------------------------------+
```

VERIFIED walk-through:

1. Forgot-password uses CSRF and email/IP limits but returns an
   enumeration-resistant response (`crates/auth/src/ui/forgot.rs:40-196`).
2. The reset credential is 32 random bytes, hash-only in Postgres, one active
   per user, and valid for 60 minutes
   (`crates/auth/src/identity/password_reset.rs:1-57`).
3. Reset checks the live token, password policy, and limits. Its transaction
   replaces the password, bumps `credential_version`, clears lockout, consumes
   the token, and writes app-family and refresh-family revocation state
   (`crates/auth/src/ui/reset.rs:57-193`,
   `crates/auth/src/identity/password_reset.rs:236-376`).
4. The outer operation deletes IdP and gateway session rows, revokes anchors,
   removes pending magic links, audits, and intentionally redirects to `/login` without
   minting a fresh IdP session (`crates/auth/src/ui/reset.rs:187-193`,
   `crates/auth/src/ui/reset.rs:211-331`).

### 1.9 Upstream Google login

```text
+------------------------------------------------------------+
| Browser holds Auth-signed state PKCE and nonce stash       |
+------------------------------------------------------------+
                              |
                              v
+------------------------------------------------------------+
| PUBLIC TRUST BOUNDARY: browser callback to Auth            |
+------------------------------------------------------------+
                              |
                              v
+------------------------------------------------------------+
| TRUST BOUNDARY: Auth to Google OIDC                        |
+------------------------------------------------------------+
                              |
                              v
+------------------------------------------------------------+
| Auth holds client secret; Google verifies code secret PKCE |
| Google signs ID JWT and returns paired access token        |
| Auth verifies JWKS issuer audience nonce and at_hash       |
+------------------------------------------------------------+
                              |
                              v
+------------------------------------------------------------+
| Auth linker decides identity and account eligibility       |
| Auth issues opaque IdP session or pending-link token       |
+------------------------------------------------------------+
```

VERIFIED walk-through:

1. Auth signs a 10-minute, provider-specific stash containing state, PKCE
   verifier, nonce, and return target in a Secure HttpOnly Lax cookie
   (`crates/auth/src/ui/oauth_stash.rs:23-65`,
   `crates/auth/src/ui/oauth_stash.rs:68-185`).
2. Auth verifies the signed stash and state, then sends the code, client secret,
   and PKCE verifier to Google's token endpoint for validation and exchange.
   Auth validates the returned ID JWT's JWKS signature, issuer, audience, nonce,
   and binding to the paired access token through `at_hash`
   (`crates/auth/src/ui/oauth_google.rs:78-244`,
   `crates/auth/src/identity/oauth/google.rs:67-210`).
3. Auth trusts email only for verified Gmail or hosted-domain identities before
   handing the provider subject to the shared linker
   (`crates/auth/src/ui/oauth_google.rs:408-424`).
4. The linker returns an existing/new user or a signed pending-link path; Auth
   rechecks account eligibility and otherwise creates the opaque IdP session
   (`crates/auth/src/ui/oauth_google.rs:260-405`).

### 1.10 Upstream GitHub login

```text
+----------------------------------------------------------+
| Browser holds Auth-signed state and PKCE stash           |
+----------------------------------------------------------+
                             |
                             v
+----------------------------------------------------------+
| PUBLIC TRUST BOUNDARY: browser callback to Auth          |
+----------------------------------------------------------+
                             |
                             v
+----------------------------------------------------------+
| TRUST BOUNDARY: Auth to GitHub OAuth and API             |
+----------------------------------------------------------+
                             |
                             v
+----------------------------------------------------------+
| Auth verifies signed stash and state; holds client secret|
| GitHub validates code secret and PKCE at token endpoint  |
| Auth checks returned scope profile and selected email    |
+----------------------------------------------------------+
                             |
                             v
+----------------------------------------------------------+
| Auth linker decides identity and account eligibility     |
| Auth issues opaque IdP session or pending-link token     |
+----------------------------------------------------------+
```

VERIFIED walk-through:

1. Auth signs the same 10-minute provider-bound stash, including state and PKCE
   verifier, before redirecting to GitHub
   (`crates/auth/src/ui/oauth_stash.rs:23-65`,
   `crates/auth/src/ui/oauth_github.rs:78-170`).
2. Auth checks the signed stash and state, then sends the code, client secret,
   and PKCE verifier to GitHub's token endpoint. After GitHub returns an access
   bearer, Auth checks its scope and queries `/user` and `/user/emails`; only a
   primary, verified, non-noreply email is accepted
   (`crates/auth/src/ui/oauth_github.rs:78-395`,
   `crates/auth/src/identity/oauth/github.rs:65-213`).
3. The linker returns an existing/new user or a signed pending-link path; Auth
   rechecks account eligibility and otherwise creates the opaque IdP session
   (`crates/auth/src/ui/oauth_github.rs:260-395`).

### 1.11 Account linking

```text
+----------------------------------------------------------+
| Provider callback supplies verified subject and email    |
+----------------------------------------------------------+
                             |
                             v
+----------------------------------------------------------+
| TRUST BOUNDARY: provider callback to Auth                |
+----------------------------------------------------------+
                             |
                             v
+----------------------------------------------------------+
| Auth computes provider-specific email trust decision     |
| Auth linker decides existing link new user auto-link     |
| or Auth-signed 10m pending-link confirmation             |
+----------------------------------------------------------+
                             |
                             v
+----------------------------------------------------------+
| Only pending branch continues; browser holds signed token|
+----------------------------------------------------------+
                             |
                             v
+----------------------------------------------------------+
| TRUST BOUNDARY: browser proves collision ownership       |
| Password is required; if TOTP applies Auth signs 5m step |
+----------------------------------------------------------+
                             |
                             v
+----------------------------------------------------------+
| Auth verifies optional TOTP then inserts identity        |
| Auth creates opaque IdP session                          |
+----------------------------------------------------------+
```

VERIFIED walk-through:

1. The Google and GitHub handlers compute the provider-specific email trust
   decision before calling the shared linker. The linker receives that Auth
   decision with the provider identity, then logs in an existing identity,
   auto-links only a trusted OAuth-only collision,
   creates an eligible new account, or signs a 10-minute pending-link credential
   when password ownership must be proved
   (`crates/auth/src/ui/oauth_google.rs:408-424`,
   `crates/auth/src/ui/oauth_github.rs:398-415`,
   `crates/auth/src/identity/linker.rs:1-31`,
   `crates/auth/src/identity/linker.rs:94-175`,
   `crates/auth/src/identity/linker.rs:195-289`).
2. Only the pending-link branch enters `/link`. It checks CSRF, limits, password,
   and lifecycle. If the account has TOTP, Auth signs a five-minute completed-
   password challenge and inserts the identity only after TOTP; otherwise it
   inserts directly and creates an IdP session
   (`crates/auth/src/ui/link.rs:82-113`,
   `crates/auth/src/ui/link.rs:140-432`,
   `crates/auth/src/ui/link.rs:477-558`).

### 1.12 Account unlinking

```text
+----------------------------------------------------------+
| Browser holds live IdP cookie CSRF and provider name     |
+----------------------------------------------------------+
                             |
                             v
+----------------------------------------------------------+
| TRUST BOUNDARY: browser to Auth public origin            |
+----------------------------------------------------------+
                             |
                             v
+----------------------------------------------------------+
| Auth decides provider input CSRF and session owner       |
+----------------------------------------------------------+
                             |
                             v
+----------------------------------------------------------+
| Postgres lock proves another sign-in method remains      |
| Auth deletes the identity; no credential is signed       |
+----------------------------------------------------------+
```

VERIFIED: `/me/unlink/{provider}` validates the provider segment, double-submit
CSRF, and live IdP session before calling the guarded store operation
(`crates/auth/src/server.rs:115-123`, `crates/auth/src/ui/me.rs:85-143`). The
advisory-locked transaction deletes the identity only if a password or another
provider remains (`crates/auth/src/store/identities.rs:123-212`); the handler
audits either refusal or success (`crates/auth/src/ui/me.rs:153-212`).

### 1.13 Creator BFF and the gateway-signed app session

```text
+--------------------------------------------------------------+
| Browser holds PKCE verifier, state, nonce; no OP bearer      |
+--------------------------------------------------------------+
                               |
                               v
+--------------------------------------------------------------+
| TRUST BOUNDARY: browser to app-origin Gateway                |
+--------------------------------------------------------------+
                               |
                               v
+--------------------------------------------------------------+
| Gateway decides app exact callback S256 and origin           |
| Gateway holds broker master and dedicated pairwise salt      |
| Gateway holds anchor AES key and Ed25519 session signing key |
+--------------------------------------------------------------+
                               |
                               v
+--------------------------------------------------------------+
| TRUST BOUNDARY: browser follows redirect to Auth origin      |
+--------------------------------------------------------------+
                               |
                               v
+--------------------------------------------------------------+
| Browser presents Auth-origin IdP cookie                      |
| Auth decides client consent PKCE scopes and issues code      |
+--------------------------------------------------------------+
                               |
                               v
+--------------------------------------------------------------+
| TRUST BOUNDARY: Gateway broker to Auth token endpoint        |
+--------------------------------------------------------------+
                               |
                               v
+--------------------------------------------------------------+
| Auth consumes code and signs 15m access and ID JWTs          |
| Auth issues rotating refresh token                           |
+--------------------------------------------------------------+
                               |
                               v
+--------------------------------------------------------------+
| Gateway verifies ID JWT and signs 15m app-session JWT        |
| Postgres stores audit row and 30d encrypted-refresh anchor   |
+--------------------------------------------------------------+
                               |
                               v
+--------------------------------------------------------------+
| Browser holds HttpOnly Lax session and Strict anchor ID      |
+--------------------------------------------------------------+
```

VERIFIED walk-through:

1. The SDK creates PKCE, state, and nonce and stores the transaction client-side
   (`sdks/auth/src/client.ts:294-321`,
   `sdks/auth/src/internal/transaction.ts:1-27`). Gateway `/authorize` resolves
   the route, requires S256, state, nonce, exact callback allowlisting, and adds
   `offline_access` (`crates/gateway/src/browser_auth.rs:57-180`).
   Its process holds separate broker-master, pairwise-salt, anchor-encryption,
   and app-session signing material (`crates/gateway/src/oidc_rp.rs:49-71`,
   `crates/gateway/src/lib.rs:192-231`,
   `crates/gateway/src/main.rs:396-434`).
2. Auth `/oauth2/authorize` validates the closed client, exact redirect, scopes,
   S256, nonce, IdP session, prompt, and consent
   (`crates/auth/src/oidc/authorization_code.rs:283-448`). It stores a hashed,
   one-use 60-second code bound to client, redirect, PKCE, scopes, nonce, user
   version, and IdP session (`crates/auth/src/oidc/authorization_code.rs:451-501`).
3. Token exchange authenticates the broker client and atomically consumes only
   a still-live, correctly bound code and consent
   (`crates/auth/src/oidc/authorization_code.rs:535-692`). The per-client broker
   secret is HKDF-derived from a master shared by Gateway and Auth
   (`crates/core/src/auth/mod.rs:83-96`,
   `crates/auth/src/oidc/authorization_code.rs:1342-1385`).
4. Auth signs 15-minute access and ID tokens and issues a rotating refresh family
   only for permitted `offline_access`; the family is idle-bounded at 7 days and
   absolute-bounded at 30 days
   (`crates/auth/src/oidc/authorization_code.rs:694-861`,
   `crates/auth/src/oidc/refresh.rs:30-35`).
5. The SDK callback checks state and sends only code, verifier, and redirect URI
   (`sdks/auth/src/client.ts:324-384`,
   `sdks/auth/src/internal/transport.ts:170-204`). Gateway verifies the ID token
   with `expected_nonce=None`, then projects the pairwise subject and relay
   email (`crates/gateway/src/auth_token.rs:446-550`). The missing nonce check is
   Finding 8.
6. Gateway writes the audit row and an app-RLS-bound anchor containing the
   AES-GCM-encrypted refresh token, signs its own EdDSA app-session JWT, and sets
   both cookies (`crates/gateway/src/auth_token.rs:554-708`). The signed cookie is
   15 minutes; the opaque Strict anchor is a fixed 30 days
   (`crates/gateway/src/session_token.rs:62-67`,
   `crates/gateway/src/anchors.rs:53-100`).
7. `GET /__zeroship/auth/session` verifies a live cookie or uses the anchor to
   rotate the OP refresh family and re-sign a cookie
   (`crates/gateway/src/auth_token.rs:710-969`). The browser never receives OP
   access or refresh tokens in this BFF flow
   (`sdks/auth/src/internal/transport.ts:170-210`).

### 1.14 App-origin Gateway signout

```text
+------------------------------------------------------------+
| App browser holds session anchor and breadcrumb cookies    |
| Browser posts same-origin Gateway signout                  |
+------------------------------------------------------------+
                              |
                              v
+------------------------------------------------------------+
| TRUST BOUNDARY: app browser to Gateway                     |
+------------------------------------------------------------+
                              |
                              v
+------------------------------------------------------------+
| Gateway decides local or app-global scope                  |
| Gateway writes family marker and deletes anchor rows       |
| Gateway holds anchor AES key and decrypts refresh token    |
+------------------------------------------------------------+
                              |
                              v
+------------------------------------------------------------+
| TRUST BOUNDARY: Gateway broker to Auth revoke endpoint     |
| Gateway presents broker-derived client secret              |
| Auth verifies client; refresh-family revoke is best effort |
+------------------------------------------------------------+
                              |
                              v
+------------------------------------------------------------+
| Gateway clears session anchor and breadcrumb cookies       |
| No credential is signed in this signout flow               |
+------------------------------------------------------------+
```

VERIFIED: Gateway signout requires the custom header and exact origin, resolves the
RLS-bound anchor, derives the app pairwise subject, writes the family marker,
and deletes one or all app-user anchors
(`crates/gateway/src/browser_auth.rs:304-473`). It decrypts each server-held OP
refresh token with the anchor key and authenticates to Auth with a
broker-derived client secret for best-effort family revocation, then clears all
three app cookies (`crates/gateway/src/browser_auth.rs:475-519`,
`crates/gateway/src/oidc_rp.rs:462-506`). The authoritative-write
fail-open behavior is Finding 11.

### 1.15 Auth IdP browser logout

```text
+----------------------------------------------------------+
| Browser holds IdP cookie and CSRF; posts logout          |
+----------------------------------------------------------+
                             |
                             v
+----------------------------------------------------------+
| TRUST BOUNDARY: browser to Auth public origin            |
+----------------------------------------------------------+
                             |
                             v
+----------------------------------------------------------+
| Auth decides CSRF and session ID then attempts revoke    |
| Auth clears cookie and hands recorded RPs to emitter     |
| No browser credential is signed in this flow             |
+----------------------------------------------------------+
```

VERIFIED: Auth-native logout checks CSRF, parses an optional UUID cookie, and
attempts an idempotent IdP-row revoke. It asks the back-channel emitter to
notify RPs recorded for that session and clears the cookie
(`crates/auth/src/ui/logout.rs:55-129`). It signs no new browser credential.

### 1.16 OIDC back-channel logout

```text
+------------------------------------------------------------+
| Auth process holds OP private key                          |
| Postgres holds recorded RP participation rows              |
+------------------------------------------------------------+
                              |
                              v
+------------------------------------------------------------+
| Auth attempts signed logout JWT and POST per RP audience   |
+------------------------------------------------------------+
                              |
                              v
+------------------------------------------------------------+
| TRUST BOUNDARY: Auth OP to Gateway BCL endpoint            |
+------------------------------------------------------------+
                              |
                              v
+------------------------------------------------------------+
| Gateway verifies JWT and replay ID, then revokes           |
| app family audit sessions and anchors                      |
| OP refresh-family revoke is best effort                    |
+------------------------------------------------------------+
```

VERIFIED walk-through:

1. Auth stores each RP that received an ID token in Postgres, later loads those
   participation rows, and attempts to sign and POST a short logout token for
   each registered back-channel URI
   (`crates/auth/src/oidc/backchannel_logout.rs:32-84`,
   `crates/auth/src/oidc/backchannel_logout.rs:77-126`,
   `crates/auth/src/oidc/backchannel_logout.rs:147-194`).
2. Gateway selects the app from unverified audience only for routing, then
   verifies the OP signature and audience, claims an in-flight replay ID, and
   tears down the app-scoped session family and anchors, then attempts OP
   refresh-family revocation best effort
   (`crates/gateway/src/backchannel_logout.rs:45-110`,
   `crates/gateway/src/backchannel_logout.rs:150-373`).

### 1.17 User-visible session list

```text
+----------------------------------------------------------+
| Browser holds IdP cookie and requests session list       |
+----------------------------------------------------------+
                             |
                             v
+----------------------------------------------------------+
| TRUST BOUNDARY: browser to Auth public origin            |
+----------------------------------------------------------+
                             |
                             v
+----------------------------------------------------------+
| Auth decides session ownership; Postgres lists IdP and   |
| Gateway audit rows                                       |
| No credential is signed by this read flow                |
+----------------------------------------------------------+
```

VERIFIED: the authenticated `/me/sessions` handler validates the IdP session and
lists the user's IdP sessions plus Gateway audit rows; no credential is signed
(`crates/auth/src/ui/sessions.rs:1-97`,
`crates/auth/src/store/sessions.rs:162-220`).

### 1.18 IdP session revoke from the session list

```text
+----------------------------------------------------------+
| Browser holds IdP cookie CSRF and selected IdP row ID    |
+----------------------------------------------------------+
                             |
                             v
+----------------------------------------------------------+
| TRUST BOUNDARY: browser to Auth public origin            |
+----------------------------------------------------------+
                             |
                             v
+----------------------------------------------------------+
| Auth decides CSRF session ownership and selected row     |
| Postgres sets revoked_at                                 |
+----------------------------------------------------------+
                             |
                             v
+----------------------------------------------------------+
| Auth best-effort signs and sends BCL JWTs for those RPs  |
+----------------------------------------------------------+
```

VERIFIED: the revoke handler checks CSRF and ownership, updates the selected IdP
row's `revoked_at`, and best-effort invokes the BCL signer for recorded RPs
(`crates/auth/src/ui/sessions.rs:98-164`,
`crates/auth/src/store/sessions.rs:249-273`).

### 1.19 App audit-session revoke from the session list

```text
+----------------------------------------------------------+
| Browser holds IdP cookie CSRF and selected app row ID    |
+----------------------------------------------------------+
                             |
                             v
+----------------------------------------------------------+
| TRUST BOUNDARY: browser to Auth public origin            |
+----------------------------------------------------------+
                             |
                             v
+----------------------------------------------------------+
| Auth decides CSRF session ownership and selected row     |
| Auth deletes Gateway audit row and signs no credential   |
+----------------------------------------------------------+
```

VERIFIED: for `kind=app`, the handler checks the same CSRF and ownership boundary
but deletes only the `gateway_sessions` row
(`crates/auth/src/ui/sessions.rs:98-171`,
`crates/auth/src/store/sessions.rs:249-273`). It does not revoke the self-contained
Gateway cookie or anchor; see Finding 2.

### 1.20 Account deletion lifecycle

```text
+----------------------------------------------------------+
| Browser holds live IdP cookie and CSRF                   |
+----------------------------------------------------------+
                             |
                             v
+----------------------------------------------------------+
| TRUST BOUNDARY: browser to Auth public origin            |
+----------------------------------------------------------+
                             |
                             v
+----------------------------------------------------------+
| Auth decides live session CSRF and deletion eligibility  |
| Auth transaction disables user, schedules 30d erasure    |
| Auth bumps version and revokes sessions and families     |
+----------------------------------------------------------+
                             |
                             v
+----------------------------------------------------------+
| After commit Auth best-effort signs and sends BCL JWTs   |
| Confirmation email is also best effort                   |
+----------------------------------------------------------+
                             |
                             v
+----------------------------------------------------------+
| Control credential paths reject inactive owner           |
+----------------------------------------------------------+
                             |
                             v
+----------------------------------------------------------+
| After grace, hourly Auth reaper hard deletes or          |
| anonymizes user according to financial-history policy    |
+----------------------------------------------------------+
```

VERIFIED walk-through:

1. `/me/delete` requires the IdP cookie and CSRF, then atomically stamps
   `disabled_at`, deletion request and schedule, bumps `credential_version`,
   revokes refresh families and IdP rows, and deletes gateway audit rows
   (`crates/auth/src/ui/account_deletion.rs:41-109`,
   `crates/auth/src/store/users.rs:327-437`). Auth also emits back-channel
   logout and sends a best-effort confirmation message after the transaction
   (`crates/auth/src/ui/account_deletion.rs:62-107`,
   `crates/auth/src/oidc/backchannel_logout.rs:147-180`).
2. Control's common bearer convergence rejects disabled, deletion-requested, or
   anonymized owners (`crates/authn/src/lib.rs:338-376`). This is current behavior
   from the recent lifecycle merge, not a finding.
3. After the 30-day grace period, the hourly compio reaper hard-deletes a user
   without financial history or anonymizes PII while retaining required
   financial rows (`crates/auth/src/cron/account_reaper.rs:57-65`,
   `crates/auth/src/cron/account_reaper.rs:127-220`). The user-facing cancel path
   cannot currently authenticate after step 1; see Finding 23.

### 1.21 Connected-app grant listing

```text
+----------------------------------------------------------+
| Caller holds a platform OAuth or Supabase bearer         |
| Auth or Supabase signs that bearer                       |
+----------------------------------------------------------+
                             |
                             v
+----------------------------------------------------------+
| TRUST BOUNDARY: caller to Control grant-list endpoint    |
+----------------------------------------------------------+
                             |
                             v
+----------------------------------------------------------+
| Control verifies bearer and active owner                 |
| Cedar decides AccountRead under the scope wrapper        |
+----------------------------------------------------------+
                             |
                             v
+----------------------------------------------------------+
| Control returns only that owner's active consent rows    |
| Control signs no credential                              |
+----------------------------------------------------------+
```

VERIFIED: `GET /me/oauth-grants` uses the common bearer guard, calls
`AuthzGuard::require(AccountRead, Any)`, and queries grants only for the
verified principal ID. The result is connected-client metadata, granted
scopes, and timestamps; no credential is issued
(`crates/control/src/oauth_grants_handlers.rs:29-80`,
`crates/control/src/oauth_grants_handlers.rs:234-238`,
`crates/control/src/authz_guard.rs`, `AuthzGuard::require`;
`crates/authn/src/lib.rs`, `BearerVerifier::verify_bearer`).

### 1.22 Connected-app grant revocation

```text
+------------------------------------------------------------+
| Caller holds a platform OAuth or Supabase bearer           |
| Auth or Supabase signs that bearer                         |
+------------------------------------------------------------+
                              |
                              v
+------------------------------------------------------------+
| TRUST BOUNDARY: caller to Control grant-revoke endpoint    |
+------------------------------------------------------------+
                              |
                              v
+------------------------------------------------------------+
| Control verifies active owner; Cedar decides AccountWrite  |
+------------------------------------------------------------+
                              |
                              v
+------------------------------------------------------------+
| TRUST BOUNDARY: Control to authoritative shared Postgres   |
| One transaction deletes consent, revokes relay alias,      |
| and writes the app pairwise token-family marker            |
| Control signs no credential                                |
+------------------------------------------------------------+
                              |
                              v
+------------------------------------------------------------+
| Auth and Gateway reject credentials older than the marker  |
| Raw bearer and cookie readers use the same pairwise key     |
+------------------------------------------------------------+
                              |
                              v
+------------------------------------------------------------+
| App client may hold refresh; Gateway holds anchor key      |
| Existing Auth refresh row and Gateway anchor remain live   |
+------------------------------------------------------------+
                              |
                              v
+------------------------------------------------------------+
| TRUST BOUNDARY: client or Gateway to Auth token endpoint   |
+------------------------------------------------------------+
                              |
                              v
+------------------------------------------------------------+
| Auth decides refresh valid and signs newer access          |
| Gateway decides old marker passes and signs newer cookie   |
+------------------------------------------------------------+
```

VERIFIED walk-through:

1. `DELETE /me/oauth-grants/{client_id}` uses the common bearer guard and
   requires Cedar `AccountWrite` before validating the bounded client ID
   (`crates/control/src/oauth_grants_handlers.rs:82-100`,
   `crates/control/src/oauth_grants_handlers.rs:234-243`,
   `crates/control/src/authz_guard.rs:48-91`).
2. On a dedicated connection, one transaction deletes the caller's consent
   row, stamps the matching relay alias revoked, derives the app pairwise
   subject, and upserts its token-family marker
   (`crates/control/src/oauth_grants_handlers.rs:101-155`,
   `crates/control/src/oauth_grants_handlers.rs:158-231`). Control signs no
   replacement credential.
3. Auth UserInfo and introspection reject access tokens whose `iat` predates
   the marker, and Gateway applies the same marker rule to its app-session
   cookie path
   (`crates/authz/src/wrapper_revocation.rs:44-103`,
   `crates/gateway/src/router/auth.rs:89-139`,
   `crates/auth/src/oidc/userinfo.rs:73-166`,
   `crates/auth/src/oidc/introspect.rs:97-153`). Gateway's raw OP bearer path
   validates the issuer-projected subject and uses it unchanged as the marker
   key (`crates/gateway/src/router/auth.rs:625-670`).
4. The transaction touches neither Auth refresh rows nor Gateway
   `app_session_anchors`. A rejected cookie falls through to a still-live
   anchor, Gateway decrypts its refresh credential and sends it to Auth, Auth
   rotates and signs a newer access token, and Gateway signs a fresh app cookie
   (`db/migrations-ts/20260702000600_constraints_indexes_fks.ts:126-128`,
   `crates/gateway/src/auth_token.rs:740-949`,
   `crates/gateway/src/auth_token.rs:1060-1101`). Gateway's post-refresh check
   rejects only a marker written at or after the rotation started, so a
   disconnect marker whose stored epoch precedes the later rotation passes
   (`crates/gateway/src/auth_token.rs:1210-1239`).
5. A refresh token held directly by the app client has the same underlying
   gap: Auth refresh does not recheck consent, so it can rotate and sign a
   post-marker access token. These independent revocation gaps are Finding 7
   (`crates/auth/src/oidc/refresh.rs:435-590`).

## 2. CLI auth

### 2.1 Control device grant (DELETED)

Control used to run a second RFC 8628 flow of its own: `/api/device/auth`
minted a `provider = 'platform'` row, `/api/device/approve` bound a principal
to it (with a Supabase browser leg that posted a GoTrue bearer), and
`/api/device/token` exchanged the approved row for a deploy token through
Auth's `/internal/platform-token` mint under a dedicated `platform_mint_key`.

All of it is gone. `zeroship login` moved onto the OP's own device grant
(section 2.2), which hands the CLI a bounded access token plus a rotating
refresh family the deleted flow could not issue, and the parallel flow had no
caller left. The scope narrowing the mint performed - intersecting a token's
scopes with the principal's stored `zeroship.principal_grants` - now runs on
control's bearer path at request time (`crates/authn/src/lib.rs`,
`platform_cli_entitlement`), so it applies to a token already in a creator's
hand rather than only at issuance.

The Supabase DEPLOY path went with it, per
`docs/decisions/2026-06-30-self-contained-auth-replace-hydra.md` line 32.
Supabase remains an upstream social login (line 13 of the same ADR), and
control still resolves a GoTrue bearer to a principal through
`identity_links` on its ordinary bearer path.

### 2.2 The OP RFC 8628 grant `zeroship login` drives

```text
+----------------------------------------------------------+
| Generic public OAuth client holds OP device code         |
+----------------------------------------------------------+
                             |
                             v
+----------------------------------------------------------+
| TRUST BOUNDARY: public OAuth client to Auth OP           |
+----------------------------------------------------------+
                             |
                             v
+----------------------------------------------------------+
| Auth decides registered public client and scope validity |
| Auth stores hash, user code, client binding for 10m      |
+----------------------------------------------------------+
                             |
                             v
+----------------------------------------------------------+
| Browser approves on same Auth device page and session    |
+----------------------------------------------------------+
                             |
                             v
+----------------------------------------------------------+
| TRUST BOUNDARY: approval browser to Auth public origin   |
+----------------------------------------------------------+
                             |
                             v
+----------------------------------------------------------+
| Auth locks row, checks lifecycle and client binding      |
| Auth signs 15m access JWT and issues optional refresh    |
+----------------------------------------------------------+
```

VERIFIED walk-through:

1. Discovery advertises `/oauth2/device/authorization`, and `/oauth2/token`
   dispatches the device-code grant, so this implementation is reachable
   (`crates/auth/src/oidc/metadata.rs:96-123`,
   `crates/auth/src/oidc/authorization_code.rs:36-46`,
   `crates/auth/src/oidc/authorization_code.rs:603-615`).
2. Start requires a registered, unbrokered public client with token auth method
   `none`; requested scopes must be a subset of client registration. Auth stores
   a 10-minute hashed code, plaintext user code, client binding, scopes, and
   poll interval (`crates/auth/src/oidc/device_token.rs:124-187`).
3. The shared `/device` page performs human approval. Poll authenticates the
   public client, locks the OP row, verifies binding and expiry, and implements
   RFC 8628 `slow_down` by increasing the stored interval
   (`crates/auth/src/oidc/device_token.rs:298-403`).
4. Redemption locks the owner and checks credential version, disabled,
   deletion-requested, and anonymized state. It deletes the row, signs a
   15-minute access token, and issues refresh only for allowed `offline_access`
   (`crates/auth/src/oidc/device_token.rs`, `exchange_device_code_locked`).
5. This IS the flow `zeroship login` drives. The CLI first reads control's RFC
   9728 metadata (`GET {control}/.well-known/oauth-protected-resource`) to
   learn which OP control accepts tokens from, then runs the grant against that
   issuer (`crates/cli/src/auth.rs`, `discover_authorization_server` and
   `login_device_flow`).
6. For the reserved `zeroship-cli` client the minted access token is a PLATFORM
   PRINCIPAL token - `sub` is the `zeroship.users` UUID, `aud` is control's
   `oauth_audience` - and both the device redemption and the later refresh
   rotation mint it through one helper
   (`crates/auth/src/oidc/device_token.rs`, `mint_grant_access_token`). The
   refresh row stores that same principal subject, so `kill_family` writes the
   `zeroship.token_revocations` marker keyed `(zeroship-cli, principal UUID)`
   that control's bearer read path consults - which is what recalls an
   outstanding access token rather than waiting out its 15 minutes.

## 3. App end-user auth

The BFF exchange in section 1.13 is the SDK-driven end-user entry path. A second,
automatic RP path exists for protected HTML navigation, and both credentials
converge at Gateway's route authorization gate.

### 3.1 Automatic protected-route OIDC RP

```text
+------------------------------------------------------------+
| Browser requests protected HTML without app credential     |
+------------------------------------------------------------+
                              |
                              v
+------------------------------------------------------------+
| PUBLIC TRUST BOUNDARY: browser to Gateway                  |
+------------------------------------------------------------+
                              |
                              v
+------------------------------------------------------------+
| Gateway decides redirect and signs 10m stash cookie        |
| Browser holds state nonce PKCE verifier client return      |
+------------------------------------------------------------+
                              |
                              v
+------------------------------------------------------------+
| TRUST BOUNDARY: Gateway RP to Auth OP                      |
+------------------------------------------------------------+
                              |
                              v
+------------------------------------------------------------+
| Auth authenticates and consents; Auth signs OP tokens      |
+------------------------------------------------------------+
                              |
                              v
+------------------------------------------------------------+
| Gateway verifies stash, nonce, ID JWT, client, at_hash     |
| Gateway signs 15m app-session cookie; no anchor is made    |
+------------------------------------------------------------+
```

VERIFIED walk-through:

1. Gateway distinguishes protected HTML navigation from API traffic and only
   redirects the HTML case (`crates/gateway/src/router/dispatch.rs:2625-2664`).
2. It creates PKCE, state, nonce, and a 10-minute HMAC-signed stash returned in
   a browser-held HttpOnly cookie, requests `openid offline_access email
   profile`, and redirects to Auth (`crates/gateway/src/oidc_rp.rs:148-200`,
   `crates/gateway/src/oidc_rp.rs:1017-1041`).
3. Callback verifies stash, state, client binding, token response, nonce, ID-token
   signature, issuer, audience, expiry, and `at_hash`
   (`crates/gateway/src/oidc_rp.rs:202-347`).
4. Dispatch intercepts the callback, maps identity, writes a gateway audit row,
   and signs the 15-minute cookie
   (`crates/gateway/src/router/dispatch.rs:978-996`,
   `crates/gateway/src/router/dispatch.rs:2718-2890`). It does not create the
   30-day anchor used by the BFF path; see Finding 12.

### 3.2 Signed-cookie request to `env.auth`

```text
+--------------------------------------------------------+
| Browser holds Gateway-signed app-session cookie        |
+--------------------------------------------------------+
                            |
                            v
+--------------------------------------------------------+
| PUBLIC TRUST BOUNDARY: browser to Gateway              |
+--------------------------------------------------------+
                            |
                            v
+--------------------------------------------------------+
| Gateway verifies typ kid EdDSA issuer exp app pws      |
| Gateway checks marker when DB exists and route scopes  |
+--------------------------------------------------------+
                            |
                            v
+--------------------------------------------------------+
| Gateway signs request-bound ZeroShip-User with         |
| worker_key and sends worker_key bearer                 |
+--------------------------------------------------------+
                            |
                            v
+--------------------------------------------------------+
| PRIVATE TRUST BOUNDARY: Gateway to Worker              |
+--------------------------------------------------------+
                            |
                            v
+--------------------------------------------------------+
| Worker verifies bearer, HMAC, request ID, age          |
| Worker binds JSON to V8 invocation                     |
+--------------------------------------------------------+
                            |
                            v
+--------------------------------------------------------+
| ISOLATE TRUST BOUNDARY: env.auth getUser requireUser   |
+--------------------------------------------------------+
```

VERIFIED walk-through:

1. Gateway resolves route policy, evaluates a bearer before a cookie, and makes
   the anonymous/user/admin decision
   (`crates/gateway/src/router/auth.rs:142-221`,
   `crates/gateway/src/router/auth.rs:283-398`). Cookie-authenticated mutations
   additionally require exact-origin CSRF posture
   (`crates/gateway/src/router/auth.rs:400-460`).
2. The cookie verifier pins `typ`, `kid`, EdDSA, issuer, expiry, and the route's
   OAuth client through the `app` claim
   (`crates/gateway/src/session_token.rs:219-327`). Gateway then requires a
   `pws_` subject and, when a DB is configured, checks the per-app family marker
   through a short read-through cache (`crates/gateway/src/router/auth.rs:928-1021`,
   `crates/authz/src/wrapper_revocation.rs:117-250`).
3. Gateway strips forged inbound identity and platform headers, constructs the
   authoritative identity from verified claims, and sends a worker-key bearer
   plus `ZeroShip-User`
   (`crates/gateway/src/router/dispatch.rs:2423-2467`,
   `crates/gateway/src/router/dispatch.rs:2536-2556`,
   `crates/gateway/src/proxy.rs:532-568`).
4. `ZeroShip-User` is an HMAC over the JSON, dispatch request UUID, and issue
   time. It is valid for at most 60 seconds with 5 seconds of future skew
   (`crates/core/src/auth/mod.rs:20-21`,
   `crates/core/src/auth/mod.rs:322-433`).
5. Worker first constant-time verifies the shared worker-key bearer, then checks
   the HMAC, matching request UUID, and age before entering V8
   (`crates/worker/src/handler.rs:67-116`,
   `crates/worker/src/handler.rs:205-220`,
   `crates/worker/src/handler.rs:385-398`).
6. Production registers `AuthPlugin`; `getUser()` returns the bound object or
   null and `requireUser()` throws a canonical 401 `UNAUTHENTICATED`
   (`crates/worker/src/cache.rs:207-215`,
   `crates/runtime/src/auth.rs:25-51`,
   `crates/runtime/src/auth.rs:75-109`,
   `crates/runtime/src/auth.rs:115-204`). Runtime does not independently verify
   the OP or app cookie.

### 3.3 Raw OP bearer request

```text
+------------------------------------------------------+
| Non-browser client holds Auth-signed OP access JWT   |
+------------------------------------------------------+
                           |
                           v
+------------------------------------------------------+
| PUBLIC TRUST BOUNDARY: client to Gateway             |
+------------------------------------------------------+
                           |
                           v
+------------------------------------------------------+
| Gateway verifies at+jwt kid EdDSA iss exp claims     |
| Gateway binds route client_id and app audience       |
| Gateway checks marker when DB exists and projects    |
+------------------------------------------------------+
                           |
                           v
+------------------------------------------------------+
| Same worker_key and ZeroShip-User boundary as 3.2    |
+------------------------------------------------------+
                           |
                           v
+------------------------------------------------------+
| Worker exposes verified projection through env.auth  |
+------------------------------------------------------+
```

VERIFIED walk-through:

1. Auth signs native RFC 9068 access tokens with a per-client pairwise subject
   (`crates/auth/src/oidc/authorization_code.rs:1387-1405`,
   `crates/auth/src/oidc/issuer.rs:410-429`).
2. Gateway pins access-token type, EdDSA, key ID, issuer, expiry, and required
   claims (`crates/gateway/src/oidc_rp.rs:545-577`,
   `crates/gateway/src/oidc_rp.rs:673-780`). It then requires the route client ID
   and `app:{app_id}` resource audience before the optional DB-backed marker
   check and identity projection (`crates/gateway/src/router/auth.rs:654-825`).
3. Gateway requires the verified `sub` to have the exact pairwise shape, uses
   it unchanged for lifecycle and family-marker checks, and forwards that same
   value to Worker (`crates/gateway/src/router/auth.rs:625-692`). It does not
   derive a second subject.
4. A successful arm constructs the same request-bound HMAC header and reaches
   Worker and `env.auth` exactly as in section 3.2
   (`crates/gateway/src/router/auth.rs:795-819`,
   `crates/worker/src/handler.rs:88-116`).

### 3.4 OP refresh

```text
+----------------------------------------------------------+
| Client holds opaque refresh token and, if required,      |
| its registered client secret                             |
+----------------------------------------------------------+
                             |
                             v
+----------------------------------------------------------+
| TRUST BOUNDARY: OAuth client to Auth token endpoint      |
+----------------------------------------------------------+
                             |
                             v
+----------------------------------------------------------+
| Auth holds refresh HMAC keyring and replay-cache AEAD key|
| Auth authenticates client and decides family eligibility |
+----------------------------------------------------------+
                             |
                             v
+----------------------------------------------------------+
| Auth stores token HMACs and encrypted replay response    |
| Auth signs new access JWT and issues random refresh      |
+----------------------------------------------------------+
```

VERIFIED: `/oauth2/token` dispatches refresh separately. Auth authenticates the
client by its registered `none`, Basic, or POST method, then locks the rotating
family and owner, enforces idle and absolute deadlines and a scope subset,
detects reuse, rotates the refresh credential, and signs a new access JWT
(`crates/auth/src/oidc/authorization_code.rs:603-615`,
`crates/auth/src/oidc/refresh.rs:386-590`,
`crates/auth/src/oidc/refresh.rs:910-958`). Auth loads a versioned HMAC keyring
for lookup and a separate AEAD key for its idempotent replay cache; Postgres
stores no refresh token in plaintext. The short-lived AEAD replay blob
deliberately contains the successor refresh token so Auth can decrypt and
return one lost-response retry (`crates/auth/src/oidc/refresh.rs:141-163`,
`crates/auth/src/oidc/refresh.rs:212-293`,
`crates/auth/src/oidc/refresh.rs:809-887`).

### 3.5 OP UserInfo

```text
+--------------------------------------------------------+
| OAuth client holds Auth-signed OP access JWT           |
+--------------------------------------------------------+
                            |
                            v
+--------------------------------------------------------+
| TRUST BOUNDARY: OAuth client to Auth UserInfo          |
+--------------------------------------------------------+
                            |
                            v
+--------------------------------------------------------+
| Auth verifies signature family marker openid scope     |
| Auth reverse maps pairwise subject and checks user     |
+--------------------------------------------------------+
                            |
                            v
+--------------------------------------------------------+
| Auth decides and returns scope-gated profile claims    |
+--------------------------------------------------------+
```

VERIFIED: `/oauth2/userinfo` accepts a bearer access token, verifies signature
and family marker, requires `openid`, reverse maps pairwise `sub` to a global
user, rejects a missing or disabled user, and scope-gates returned claims
(`crates/auth/src/oidc/userinfo.rs:18-24`,
`crates/auth/src/oidc/userinfo.rs:73-166`). The reverse-map availability gap for
generic public clients is Finding 26.

### 3.6 OP token introspection

```text
+--------------------------------------------------------+
| OAuth client holds its credential and token to check   |
+--------------------------------------------------------+
                            |
                            v
+--------------------------------------------------------+
| TRUST BOUNDARY: OAuth client to Auth introspection     |
+--------------------------------------------------------+
                            |
                            v
+--------------------------------------------------------+
| Auth authenticates client and binds token client ID    |
| Auth holds refresh HMAC keyring for opaque lookup      |
| Auth decides family or refresh row is currently live   |
+--------------------------------------------------------+
                            |
                            v
+--------------------------------------------------------+
| Auth returns active state and claims; signs no token   |
+--------------------------------------------------------+
```

VERIFIED: `/oauth2/introspect` requires authenticated client credentials, binds
the token to that client, and checks either access-token family state or the
live rotating refresh row. Opaque refresh lookup loads the HMAC keyring
(`crates/auth/src/oidc/introspect.rs:19-95`,
`crates/auth/src/oidc/introspect.rs:97-153`,
`crates/auth/src/oidc/introspect.rs:129-139`).

### 3.7 OP token revocation

```text
+--------------------------------------------------------+
| OAuth client holds its credential and token to revoke  |
+--------------------------------------------------------+
                            |
                            v
+--------------------------------------------------------+
| TRUST BOUNDARY: OAuth client to Auth revocation        |
+--------------------------------------------------------+
                            |
                            v
+--------------------------------------------------------+
| Auth authenticates client and binds token ownership    |
| Auth holds refresh HMAC keyring for opaque lookup      |
| Auth decides whether matching family can be revoked    |
+--------------------------------------------------------+
                            |
                            v
+--------------------------------------------------------+
| Auth kills access or refresh family; signs no token    |
+--------------------------------------------------------+
```

VERIFIED: `/oauth2/revoke` authenticates according to client registration,
returns the RFC-style non-disclosing response for unknown or foreign tokens,
and kills the matching access or refresh family
(`crates/auth/src/oidc/refresh.rs:593-708`,
`crates/auth/src/oidc/refresh.rs:910-958`). Its opaque refresh-token branch
loads that keyring before hash lookup (`crates/auth/src/oidc/refresh.rs:640-658`).

## 4. Programmatic auth

### 4.1 Personal access tokens (DELETED)

Control used to be a second issuance authority of its own. It held an Ed25519
signing key, `POST /me/tokens` minted a `pat+jwt` credential lasting up to 365
days with a stored Cedar wrapper policy, `GET /me/tokens` and
`DELETE /me/tokens/{id}` managed them, and the shared `BearerVerifier` tried
local PAT verification against `zeroship.permission_tokens` before it tried
OAuth introspection. Migrated verified the same credential class.

All of it is gone: the three routes and their handler module, the
`permission_tokens` table
(`db/migrations-ts/20260817000000_drop_permission_tokens.ts`), the audit columns
only a PAT ever populated
(`db/migrations-ts/20260817000100_drop_audit_token_columns.ts`), the `PatIssuer`
and `PatClaims` types, the token-policy lookup in the authz evaluator, and the
`token_id` field those carried through `AuthzContext` and the control-plane
guard. Control's and migrated's `--signing-key-file` inputs went with them:
both existed only to build a `PatIssuer`. Gateway's `--signing-key-file` is a
DIFFERENT consumer - it signs app-session wrapper tokens - and survives.

The reason is that a PAT was a SECOND ISSUANCE AUTHORITY. The platform is meant
to have exactly one, the OP, with control a pure bearer resource server; a
control-plane signing key minting year-long credentials was the largest
counterexample to that. Removing it makes the OP the sole issuer. The operator
decision is section 11 of `docs/proposals/2026-08-16-cli-token-issuance.md`
("do not support PAT token, this is a security decision"): removed, not
hardened.

Long-lived automation is now served by the OP refresh family on the device-flow
credential (`crates/auth/src/oidc/refresh.rs`), which already implements
rotation, reuse detection and an absolute family expiry. That is a better
trade than the credential it replaces: a short access token plus a revocable
long-lived family, both issued by the OP.

The one genuine loss is NON-INTERACTIVE CREATION. A PAT could be minted by API,
so an unattended system could bootstrap its own credential; a device grant
needs a human once per setup, after which the family self-renews.

`BearerVerifier::verify_bearer` is now OAuth-only, and every bearer control
accepts reaches one of the two arms of `oauth_guard_from_bearer`
(`crates/authn/src/lib.rs`). They are deliberately asymmetric. A platform OAuth
bearer (`ProviderAuthz::OAuthScope`) must carry the expected audience, is
refused when its token family is revoked, and derives its wrapper policy from
its own scopes through `zeroship_authz::scopes_to_policy`; for the
`zeroship-cli` client those scopes are first intersected with the principal's
stored `zeroship.principal_grants`. A Supabase GoTrue bearer
(`ProviderAuthz::GoTrueRole`) must carry role `authenticated`, resolves to a
principal through `zeroship.identity_links`, and derives its wrapper from that
principal's `principal_grants` rather than from anything in the token. Both
converge on the same active-principal lifecycle check.

Both arms therefore hand `authz::enforce` a wrapper policy built out of the
closed OAuth scope vocabulary, and nothing else can now supply one. One Cedar
action is outside that vocabulary; see Finding 30.

### 4.2 Delegated creator bearer for migration apply

```text
+----------------------------------------------------------------+
| Caller holds platform OAuth bearer or Supabase bearer          |
| Auth signs the OAuth bearer; Supabase signs the GoTrue bearer  |
+----------------------------------------------------------------+
                                |
                                v
+----------------------------------------------------------------+
| TRUST BOUNDARY: caller to Control migration route              |
+----------------------------------------------------------------+
                                |
                                v
+----------------------------------------------------------------+
| Control verifies bearer lifecycle and AppsDeploy               |
| Control forwards the same raw bearer                           |
+----------------------------------------------------------------+
                                |
                                v
+----------------------------------------------------------------+
| TRUST BOUNDARY: Control to Migrated                            |
+----------------------------------------------------------------+
                                |
                                v
+----------------------------------------------------------------+
| Migrated holds no signing key of its own                       |
| Migrated verifies the forwarded platform bearer                |
| Cedar plus app-owner check decides; no new token signed        |
+----------------------------------------------------------------+
```

VERIFIED walk-through:

1. Control rate limits the migration route, uses `AuthzGuard` to require
   `AppsDeploy` for the app, then extracts the already accepted caller bearer
   (`crates/control/src/migrations_api.rs:70-118`).
2. Control forwards that raw bearer and request ID to the configured Migrated
   URL (`crates/control/src/migrations_api.rs:147-196`). The shipped compose URL
   is plain `http://migrated:9091`
   (`deploy/compose/docker-compose.yml:305-316`).
3. Migrated extracts the bearer and independently invokes the shared
   `BearerVerifier`, preserving the caller's scope-derived wrapper policy and
   the owner lifecycle check, then requires Cedar `AppsDeploy` plus app
   ownership before applying (`crates/migrated/src/api.rs:79-108`,
   `crates/migrated/src/auth.rs`, `ControlPlaneAuthenticator::verify_action`).
   Auth signs the platform OAuth JWTs and Supabase signs the GoTrue JWTs
   (`crates/auth/src/oidc/issuer.rs:453-473`,
   `crates/core/src/auth_provider/supabase.rs:312-390`); since the PAT class
   was deleted (section 4.1) Migrated holds no signing key at all. Unlike
   Control, Migrated configures a platform-only OAuth provider, so a Supabase
   bearer accepted by Control fails here (`crates/control/src/main.rs`,
   `crates/migrated/src/main.rs`); see Finding 17. It signs no replacement
   credential.

### 4.3 Workflow signal-capability issuance

```text
+----------------------------------------------------------+
| Mint caller must hold app ID and app-scoped Control HMAC |
+----------------------------------------------------------+
                             |
                             v
+----------------------------------------------------------+
| TRUST BOUNDARY: mint caller to Control endpoint          |
+----------------------------------------------------------+
                             |
                             v
+----------------------------------------------------------+
| Control verifies app HMAC and run or active deploy       |
| Control decides type allowlist and TTL at most 24h       |
+----------------------------------------------------------+
                             |
                             v
+----------------------------------------------------------+
| Control holds master key and per-app HMAC signing secret |
| Control signs wst_ capability; caller receives plaintext |
+----------------------------------------------------------+
```

VERIFIED walk-through:

1. The concrete `@zeroship/control` helpers post an app ID, signal types, and
   TTL to run- or topic-token endpoints using the bearer configured on that
   client (`sdks/control/src/index.ts:275-299`). Control derives the asserted
   app from the header only after verifying the app-scoped HMAC bearer; a
   platform OAuth or Supabase bearer does not satisfy this endpoint
   (`crates/control/src/workflow_instance_api.rs:326-405`). The creator-facing
   run helper is not wired to this live route; see Finding 14.
2. Run issuance requires a live, nonterminal run and captures its current
   signal epoch. Topic issuance requires an active deploy. Both accept at most
   16 signal types and cap TTL at 24 hours
   (`crates/control/src/workflow_instance_api.rs:49-55`,
   `crates/control/src/workflow_instance_api.rs:580-588`,
   `crates/control/src/workflow_instance_api.rs:2129-2266`).
3. Control creates a random 32-byte per-app HMAC secret when needed and stores
   only its master-key-encrypted form. It signs app, run or topic, allowed
   types, expiry, and run epoch into the `wst_` value
   (`crates/control/src/workflow_instance_api.rs:749-861`,
   `crates/core/src/typed_id.rs:526-622`). The plaintext capability is returned
   to the caller; no Gateway or Worker signs in this flow.

### 4.4 Public workflow signal ingress

```text
+------------------------------------------------------------+
| External caller holds wst_ capability in the JSON body     |
+------------------------------------------------------------+
                              |
                              v
+------------------------------------------------------------+
| TRUST BOUNDARY: public caller to Gateway exact POST path   |
| Gateway rate limits; it does not validate the wst_ token   |
+------------------------------------------------------------+
                              |
                              v
+------------------------------------------------------------+
| Gateway holds control_key and forwards body with bearer    |
+------------------------------------------------------------+
                              |
                              v
+------------------------------------------------------------+
| TRUST BOUNDARY: Gateway to Control                         |
| Control verifies control_key then wst_ HMAC and claims     |
| Control decides replay, epoch, app, type, run or topic     |
| No credential is signed on ingress                         |
+------------------------------------------------------------+
```

VERIFIED walk-through:

1. Gateway exposes exactly `POST /__zeroship/v1/signal` and
   `POST /__zeroship/signals/v1`, applies a source-IP placeholder rate limit,
   and forwards the raw JSON body. It neither parses nor forwards the public
   request's Authorization header; it adds its broad `control_key` bearer
   (`crates/gateway/src/main.rs:583-600`,
   `crates/gateway/src/signal_ingress.rs:67-125`).
2. Control first authenticates Gateway's `control_key`, then reads the `wst_`
   token from `IngressSignalBody.token`
   (`crates/control/src/workflow_instance_api.rs:137-144`,
   `crates/control/src/workflow_instance_api.rs:356-385`,
   `crates/control/src/workflow_instance_api.rs:2317-2331`).
3. Control uses the unverified app claim only to select and decrypt that app's
   candidate HMAC secrets, then verifies the MAC, expiry, allowed type, and
   enabled app. It enforces the current epoch for a run token and replay through
   unique journal or broadcast keys before delivery
   (`crates/control/src/workflow_instance_api.rs:830-938`,
   `crates/control/src/workflow_instance_api.rs:2350-2518`,
   `crates/control/src/workflow_instance_api.rs:2535-2587`). A run token can be
   consumed once per authorized signal type; a topic token is one-use overall
   (`db/migrations-ts/20260705000000_durable_workflows_journal.ts:169-188`,
   `db/migrations-ts/20260705000000_durable_workflows_journal.ts:281-285`). The
   public reference documents a different path/header/body shape and an invalid
   48h example; see Finding 14.

Search method for revocation: the complete workflow route inventory was read
(`crates/control/src/workflow_instance_api.rs:3279-3350`), and a scoped search
for signal-key status writes, `signal_epoch` assignments, and token revocation
found no general revoke endpoint. The only epoch increment is a restart whose
deploy target changes (`crates/control/src/workflow_instance_api.rs:2746-2757`,
`crates/control/src/workflow_instance_api.rs:2852-2888`). Topic tokens have no
epoch, so replay, expiry, or out-of-band key change are their only invalidation
mechanics.

## 5. Service-to-service auth

### 5.1 Broad `control_key`

```text
+--------------------------------------------------------+
| Operator gives key to Control Gateway Worker           |
| Migrated also resolves and retains the same key        |
+--------------------------------------------------------+
                            |
                            v
+--------------------------------------------------------+
| Gateway or Worker sends Bearer control_key to Control  |
+--------------------------------------------------------+
                            |
                            v
+--------------------------------------------------------+
| TRUST BOUNDARY: internal caller to Control             |
+--------------------------------------------------------+
                            |
                            v
+--------------------------------------------------------+
| Control constant-time compares possession only         |
| Decision authorizes broad internal endpoint            |
| No message signature or presenter identity is used     |
+--------------------------------------------------------+
```

VERIFIED walk-through:

1. Control, Gateway, Worker, and Migrated resolve `control_key`
   (`crates/control/src/config.rs:328-341`,
   `crates/gateway/src/config.rs:44-50`,
   `crates/worker/src/config.rs:54-64`,
   `crates/migrated/src/config.rs:101-103`,
   `crates/migrated/src/main.rs:86-106`). Gateway uses it for route sync and
   Worker uses it for versions and decrypted app environment fetches
   (`crates/gateway/src/sync.rs:174-200`,
   `crates/worker/src/sync.rs:466-485`,
   `crates/worker/src/sync.rs:565-616`). Migrated's retained field has no request
   use found; see Finding 28.
2. Control extracts the bearer, refuses an empty configured key, and compares in
   constant time before serving internal data
   (`crates/control/src/internal.rs:16-40`,
   `crates/core/src/auth/mod.rs:26-67`). The credential has no timestamp, nonce,
   or presenter identity; its lifetime and revocation are global config rotation.
3. Gateway route sync explicitly permits only `http` for its hand-written sync
   client and sends the broad key in the bearer header
   (`crates/gateway/src/sync.rs:209-260`). The resulting network assumption is
   Finding 18.

### 5.2 Gateway-to-Worker `worker_key` dispatch

```text
+--------------------------------------------------------+
| Operator gives worker_key to Gateway and Worker        |
+--------------------------------------------------------+
                            |
                            v
+--------------------------------------------------------+
| Gateway sends worker_key and may sign user identity    |
+--------------------------------------------------------+
                            |
                            v
+--------------------------------------------------------+
| TRUST BOUNDARY: Gateway to Worker                      |
+--------------------------------------------------------+
                            |
                            v
+--------------------------------------------------------+
| Worker always verifies bearer possession               |
| If user header exists it verifies HMAC request ID age  |
| Worker decides whether app dispatch may enter V8       |
+--------------------------------------------------------+
```

VERIFIED: Gateway and Worker hold `worker_key`; Gateway always sends it as a
bearer and, for an authenticated request, uses the same bytes to sign the
request-bound identity HMAC
(`crates/gateway/src/config.rs:44-50`, `crates/worker/src/config.rs:54-64`,
`crates/core/src/auth/mod.rs:322-433`). Worker always constant-time verifies the
bearer. It verifies identity HMAC, request ID, and age only when a
`ZeroShip-User` header is present; anonymous dispatch correctly has no identity
assertion (`crates/worker/src/handler.rs:67-116`,
`crates/worker/src/handler.rs:205-220`). This proves shared-key possession, not
a distinct Gateway instance identity.

### 5.3 Control-to-Worker `worker_key` log retrieval

```text
+--------------------------------------------------------+
| Operator gives worker_key to Control and Worker        |
+--------------------------------------------------------+
                            |
                            v
+--------------------------------------------------------+
| Control sends raw bearer to Worker log endpoint        |
+--------------------------------------------------------+
                            |
                            v
+--------------------------------------------------------+
| TRUST BOUNDARY: Control to Worker                      |
+--------------------------------------------------------+
                            |
                            v
+--------------------------------------------------------+
| Worker verifies possession and decides log access      |
| No message signature or caller identity is provided    |
+--------------------------------------------------------+
```

VERIFIED: Control and Worker hold the key, Control presents it for log retrieval,
and Worker constant-time verifies it before serving the endpoint
(`crates/control/src/config.rs:328-341`,
`crates/worker/src/config.rs:54-64`,
`crates/control/src/api.rs:2162-2222`,
`crates/worker/src/logs.rs:48-56`).

### 5.4 App-scoped HMAC derived from `control_key`

```text
+--------------------------------------------------------+
| Worker Rust holds raw control_key outside V8           |
+--------------------------------------------------------+
                            |
                            v
+--------------------------------------------------------+
| Worker signs app_id with HMAC SHA256 control_key       |
+--------------------------------------------------------+
                            |
                            v
+--------------------------------------------------------+
| Native workflow call sends bearer plus app ID header   |
+--------------------------------------------------------+
                            |
                            v
+--------------------------------------------------------+
| TRUST BOUNDARY: Worker native plugin to Control        |
+--------------------------------------------------------+
                            |
                            v
+--------------------------------------------------------+
| Control parses app ID, recomputes HMAC, compares       |
| Decision authorizes workflow operations for that app   |
+--------------------------------------------------------+
```

VERIFIED: the derivation is exactly `HMAC-SHA256(control_key, app_id)`, and the
raw key must remain in Rust rather than enter V8
(`crates/core/src/auth/mod.rs:157-174`). The workflow plugin sends the derived
bearer and app ID (`crates/plugin-workflow/src/client.rs:124-164`,
`crates/plugin-workflow/src/client.rs:230-258`). Control parses the asserted app
ID, derives the expected token, and constant-time compares before the app-scoped
decision (`crates/control/src/workflow_instance_api.rs:326-354`,
`crates/control/src/workflow_instance_api.rs:387-405`). The token has no expiry,
nonce, or per-app revocation row; rotating `control_key` revokes every derived
token.

### 5.5 Workflow replay output-read bearer inside V8

```text
+----------------------------------------------------------+
| Worker Rust holds control_key and signs app-scoped HMAC  |
+----------------------------------------------------------+
                             |
                             v
+----------------------------------------------------------+
| TRUST BOUNDARY: Worker Rust injects bearer into V8       |
+----------------------------------------------------------+
                             |
                             v
+----------------------------------------------------------+
| Runtime bridge JS in creator isolate holds derived bearer|
| JS sends bearer and app ID when an output ref is read    |
+----------------------------------------------------------+
                             |
                             v
+----------------------------------------------------------+
| TRUST BOUNDARY: workflow isolate to Control              |
+----------------------------------------------------------+
                             |
                             v
+----------------------------------------------------------+
| Control recomputes HMAC and decides app output read      |
| Control signs no new credential                          |
+----------------------------------------------------------+
```

VERIFIED: Worker derives the app-scoped bearer from raw `control_key` and puts
the bearer, app ID, and Control URL in the workflow runtime envelope
(`crates/worker/src/handler.rs:490-522`). The live inline workflow bridge inside
the creator's V8 isolate retains that configuration and sends it when a lazy
workflow output reference is read (`crates/runtime/src/core/init.rs:693-701`,
`crates/runtime/src/core/init.rs:764-780`,
`crates/runtime/src/core/init.rs:1493-1505`). Parallel bootstrap and workflows
SDK implementations carry the same transport
(`sdks/bootstrap/src/dispatcher.ts:490-503`,
`sdks/bootstrap/src/dispatcher.ts:550-569`,
`sdks/workflows/src/journal.ts:1008-1031`). Control
extracts the app ID, recomputes the HMAC, and permits only the app-scoped output
lookup; it signs no response credential
(`crates/control/src/workflow_instance_api.rs:326-405`,
`crates/control/src/workflow_instance_api.rs:1788-1871`).

### 5.6 OIDC broker secret

```text
+--------------------------------------------------------+
| Gateway process loads current broker master            |
| Auth process loads current and optional previous       |
+--------------------------------------------------------+
                            |
                            v
+--------------------------------------------------------+
| Gateway derives per-client secret and presents by POST |
+--------------------------------------------------------+
                            |
                            v
+--------------------------------------------------------+
| TRUST BOUNDARY: Gateway broker client to Auth OP       |
+--------------------------------------------------------+
                            |
                            v
+--------------------------------------------------------+
| Auth bypasses stored Basic pin and accepts Basic/POST  |
| Auth re-derives and constant-time verifies             |
| No message is signed; this authenticates the client    |
+--------------------------------------------------------+
```

VERIFIED: Gateway derives the presented client secret from its current master
and sends it as `client_secret` in the form body. Auth's broker-first branch
accepts either Basic or POST despite the database row being pinned to Basic,
then re-derives it and accepts its current or optional previous master during
rotation (`crates/core/src/auth/mod.rs:83-96`,
`crates/gateway/src/config.rs:93-94`,
`crates/auth/src/oidc/issuer.rs:159-224`,
`crates/gateway/src/oidc_rp.rs:248-270`,
`crates/auth/src/oidc/refresh.rs:925-935`,
`crates/auth/src/oidc/authorization_code.rs:1342-1385`). Possession
authenticates the broker client for code, refresh, introspection, and revoke
operations; the registration-method mismatch is Finding 19
(`crates/auth/src/oidc/issuer.rs:853-860`,
`crates/auth/src/oidc/introspect.rs:58-66`).

### 5.7 Confidential OAuth client-secret issuance

```text
+------------------------------------------------------------+
| Platform administrator holds a Control-accepted bearer     |
+------------------------------------------------------------+
                              |
                              v
+------------------------------------------------------------+
| TRUST BOUNDARY: administrator to Control OAuth-client API  |
+------------------------------------------------------------+
                              |
                              v
+------------------------------------------------------------+
| Control verifies bearer and decides PlatformPoliciesWrite  |
| Control generates 32 random bytes; signs nothing           |
+------------------------------------------------------------+
                              |
                              v
+------------------------------------------------------------+
| Postgres stores only the secret hash                       |
| Administrator receives plaintext once and must hold it     |
+------------------------------------------------------------+
```

VERIFIED: Control first applies the normal `AuthzGuard`, then requires the
global `PlatformPoliciesWrite` Cedar action, which the shipped policy grants to
platform administrators rather than ordinary creators. For a non-public client it
generates 32 random bytes, persists only `hash_api_key(secret)`, and includes
the plaintext exactly in the create response; no credential is signed in this
flow (`crates/control/src/oauth_handlers.rs:63-127`,
`deploy/policies/platform/admin.cedar:1-10`,
`crates/control/src/oauth_handlers.rs:374-409`,
`crates/control/src/oauth_handlers.rs:440-444`).

### 5.8 Confidential OAuth client authentication

```text
+--------------------------------------------------------------+
| OAuth client holds its registered plaintext secret           |
+--------------------------------------------------------------+
                               |
                               v
+--------------------------------------------------------------+
| TRUST BOUNDARY: confidential client to Auth OP               |
+--------------------------------------------------------------+
                               |
                               v
+--------------------------------------------------------------+
| Auth pins stored Basic or POST method and checks hash        |
| Current Control writers create Basic or public none only     |
| Auth decides code refresh introspection or revoke request    |
+--------------------------------------------------------------+
                               |
                               v
+--------------------------------------------------------------+
| Grant path: Auth signs token; query or revoke signs none     |
+--------------------------------------------------------------+
```

VERIFIED: Auth's shared client-authentication function is used by authorization
code, refresh, introspection, and revocation. For a non-brokered client it
requires the stored `client_secret_basic` or `client_secret_post` method and
verifies the presented secret against the stored hash
(`crates/auth/src/oidc/refresh.rs:910-958`,
`crates/auth/src/oidc/refresh.rs:1004-1016`). Successful grant paths proceed to
Auth's token signer; introspection and revocation return state or mutate state
without signing a new credential
(`crates/auth/src/oidc/authorization_code.rs:694-719`,
`crates/auth/src/oidc/introspect.rs:58-139`,
`crates/auth/src/oidc/refresh.rs:628-708`). Control's registration API accepts
only Basic or public `none`, and its two automatic writers hardcode Basic, so
the non-brokered stored-POST branch has no production writer
(`crates/control/src/oauth_handlers.rs:245-274`,
`crates/control/src/bootstrap_builder.rs:128-155`,
`crates/control/src/app_oauth_client.rs:555-574`); see Finding 28. Brokered
clients are a separate branch and deliberately accept their derived secret by
Basic or form POST (`crates/auth/src/oidc/authorization_code.rs:1342-1385`).

### 5.9 OP signing-key lifecycle

```text
+--------------------------------------------------------+
| Auth alone loads OP Ed25519 private key into process   |
+--------------------------------------------------------+
                            |
                            v
+--------------------------------------------------------+
| Auth decides issuance and advances expiry watermark    |
| Auth signs token before returning it                   |
+--------------------------------------------------------+
                            |
                            v
+--------------------------------------------------------+
| TRUST BOUNDARY: Auth issuer to JWT consumers           |
| Consumers validate with public Auth JWKS               |
+--------------------------------------------------------+
                            |
                            v
+--------------------------------------------------------+
| Hourly Auth cron retires key after last token horizon  |
+--------------------------------------------------------+
```

VERIFIED: Auth loads the OP private Ed25519 material and publishes public
metadata
(`crates/auth/src/oidc/issuer.rs:217-225`,
`crates/auth/src/oidc/issuer.rs:293-405`). Issuance advances the exact
`max_issued_expires_at` watermark before returning a token
(`crates/auth/src/oidc/issuer.rs:766-799`). The compio cron runs immediately and
hourly, retiring a replaced key only after its last possible live token plus
JWKS cache and clock-skew windows
(`crates/auth/src/cron/signing_key_retention.rs:19-32`,
`crates/auth/src/cron/signing_key_retention.rs:45-106`). This is current behavior
from the recent signing-key merge, not a finding.

### 5.10 Workflow advance from Control through Gateway

```text
+----------------------------------------------------------+
| Control scheduler holds no hop credential and posts JSON |
+----------------------------------------------------------+
                             |
                             v
+----------------------------------------------------------+
| UNENFORCED TRUST BOUNDARY: Control to Gateway listener   |
+----------------------------------------------------------+
                             |
                             v
+----------------------------------------------------------+
| Gateway checks Host body route account and spend only    |
| No caller proof signature or nonce; Gateway forwards     |
+----------------------------------------------------------+
                             |
                             v
+----------------------------------------------------------+
| Gateway and Worker hold worker_key; no body signature    |
+----------------------------------------------------------+
                             |
                             v
+----------------------------------------------------------+
| TRUST BOUNDARY: Gateway to Worker                        |
| Worker verifies key and explicit unsigned-enable flag    |
+----------------------------------------------------------+
```

VERIFIED walk-through:

1. Control's scheduler posts the workflow request with only `content-type`; it
   supplies no bearer, signature, nonce, or caller identity
   (`crates/control/src/cron/workflow_engine.rs:194-232`).
2. Gateway mounts the internal path on its normal listener. The complete handler
   rejects creator-app Hosts, parses the asserted run and app, checks route,
   account, and spend state, but authenticates no caller; its TODO names the
   missing signature and nonce (`crates/gateway/src/main.rs:582-586`,
   `crates/gateway/src/router/dispatch.rs:99-160`).
3. Gateway forwards to Worker's unsigned workflow path with the shared
   `worker_key` bearer but no body signature
   (`crates/gateway/src/proxy.rs:239-270`). Worker verifies the key and refuses
   unless the hidden unsigned flag is explicitly enabled
   (`crates/worker/src/handler.rs:525-554`). The flag defaults off, and enabling
   it with a non-loopback bind is refused
   (`crates/worker/src/main.rs:228-240`). The missing first-hop authentication is
   Finding 15; current default reachability constraints lower its severity.

## Credential classes

All table entries in this section and the trust-boundary summary are VERIFIED;
INFERRED consequences appear only where explicitly labeled in FINDINGS.

| Credential | Issuer | Validator | Lifetime | Revocable | Authorizes |
|---|---|---|---|---|---|
| Auth UI CSRF double-submit token | Auth random generator | Auth constant-time cookie/form matcher | 1h | Yes, fresh page/cookie or expiry | One browser's state-changing Auth UI forms, not identity (`crates/auth/src/csrf.rs:1-79`, `crates/auth/src/ui/signup.rs:87-105`) |
| Password hash | User chooses; Auth hashes | Auth password verifier | Until changed | Yes, replace/reset and `credential_version` | Factor one for IdP login (`crates/auth/src/identity/password.rs:17-114`, `crates/auth/src/identity/credentials.rs:81-344`) |
| TOTP or backup code | Auth generates enrollment material | Auth TOTP verifier | Until disabled; backup is one use | Yes | Factor two for IdP login or linking (`crates/auth/src/identity/totp.rs:90-117`, `crates/auth/src/identity/totp.rs:166-230`) |
| IdP session cookie | Auth | Auth plus authoritative Postgres row | 30m idle, 12h absolute | Yes, row/version/lifecycle | Auth-origin login and OIDC SSO (`crates/auth/src/sessions/login.rs:1-39`, `crates/auth/src/store/sessions.rs:75-118`) |
| TOTP challenge cookie | Auth HMAC signer | Auth | 5m | Yes, key or credential version | Proves completed factor one (`crates/auth/src/sessions/totp_challenge.rs:24-149`) |
| Email verification token | Auth random generator | Auth hash lookup | 24h, one use | Yes, supersede or consume | Mark one email verified (`crates/auth/src/identity/verification.rs:34-35`, `crates/auth/src/identity/verification.rs:56-203`) |
| Magic-link token | Auth random generator | Auth hash and reservation | 15m, one use | Yes, supersede or consume | Passwordless login for bound target (`crates/auth/src/identity/magic_link.rs:1-27`, `crates/auth/src/identity/magic_link.rs:83-307`) |
| Magic same-device nonce | Auth random generator | Auth exact cookie-to-row comparison | Linked 15m lifetime | Yes, supersede, consume, or expiry | Skip cross-device completion for the browser that started magic (`crates/auth/src/identity/magic_link.rs:105-149`, `crates/auth/src/ui/magic.rs:573-580`) |
| Magic completion code | Auth | Auth bound completion row | 5m, attempts bounded | No supported early revoke; consume or expiry | Cross-device magic completion (`crates/auth/src/ui/magic.rs:891-953`, `crates/auth/src/ui/magic.rs:965-1275`) |
| Password-reset token | Auth random generator | Auth hash lookup | 60m, one use | Yes, supersede or consume | Replace password and revoke families (`crates/auth/src/identity/password_reset.rs:1-57`, `crates/auth/src/identity/password_reset.rs:236-376`) |
| Upstream OAuth stash | Auth HMAC signer | Auth | 10m | Yes, stash-key rotation | One Google/GitHub callback (`crates/auth/src/ui/oauth_stash.rs:23-185`) |
| Pending-link token | Auth HMAC signer | Auth | 10m | Yes, key rotation | Continue one collision-confirmed link (`crates/auth/src/identity/linker.rs:94-175`) |
| Google authorization code | Google | Google token endpoint, with Auth as client | Provider-defined; short and one use | No supported early revoke here; consume or expiry | Exchange under client, redirect, and PKCE bindings (`crates/auth/src/identity/oauth/google.rs:67-153`) |
| Google ID JWT plus access bearer | Google | Auth JWKS and claim verifier, including `at_hash` | Provider-defined and short | No local recall by Auth; expiry/provider governs | Establish verified Google subject and profile (`crates/auth/src/identity/oauth/google.rs:166-210`) |
| GitHub code and access bearer | GitHub | GitHub token and profile endpoints via Auth | Provider-defined; code is one use | Yes, provider controls credential | Establish verified GitHub subject and email (`crates/auth/src/identity/oauth/github.rs:65-213`) |
| Upstream OAuth client secret | Google or GitHub app registration | Provider token endpoint | Provider/config lifetime | Yes, provider rotation | Authenticate Auth during upstream code exchange (`crates/auth/src/config.rs:164-223`, `crates/auth/src/identity/oauth/google.rs:117-153`, `crates/auth/src/identity/oauth/github.rs:112-155`) |
| Browser PKCE, state, and nonce transaction | Browser SDK random generator | Gateway and Auth for PKCE; SDK for state; BFF nonce check is missing, Finding 8 | In-flight popup, normally at most 60s | Yes, completion, mismatch, timeout, or tab lifetime | Bind the browser redirect and code exchange, not resource access (`sdks/auth/src/client.ts:294-384`, `sdks/auth/src/internal/transaction.ts:1-27`, `crates/gateway/src/browser_auth.rs:57-180`) |
| OP authorization code | Auth random generator | Auth token endpoint and DB | 60s, one use | No supported early revoke; consume or expiry | Exchange for OP tokens under exact bindings (`crates/auth/src/oidc/authorization_code.rs:451-692`) |
| App OP access JWT | Auth Ed25519 signer | Gateway, UserInfo, introspection, or revocation | 15m default | Yes, family marker where checked | Scoped app resource access; raw Gateway path has known lifecycle gap (`crates/auth/src/oidc/issuer.rs:22-33`, `crates/gateway/src/router/auth.rs:654-825`, `crates/auth/src/oidc/introspect.rs:98-126`, `crates/auth/src/oidc/refresh.rs:628-655`) |
| OP ID JWT | Auth Ed25519 signer | OIDC client, including Gateway | 15m default | No, no per-token recall | Authentication response, not resource authority (`crates/auth/src/oidc/issuer.rs:22-33`, `crates/gateway/src/auth_token.rs:446-498`) |
| OP refresh token | Auth random generator | Auth rotating hash store | 7d idle, 30d absolute | Yes, explicit family revoke/delete/reuse kill; connected-grant revoke misses it, Finding 7 | Mint new OP token set (`crates/auth/src/oidc/refresh.rs:30-35`, `crates/auth/src/oidc/refresh.rs:320-382`, `crates/auth/src/oidc/refresh.rs:435-590`, `crates/auth/src/oidc/refresh.rs:628-705`, `crates/auth/src/oidc/refresh.rs:780-805`, `crates/auth/src/oidc/refresh.rs:1169-1195`) |
| OIDC back-channel logout JWT | Auth Ed25519 signer | Gateway plus replay cache | 2m; replay ID held 10m | No, expiry and replay check | App-family and anchor teardown (`crates/auth/src/oidc/issuer.rs:22-33`, `crates/auth/src/oidc/issuer.rs:695-737`, `crates/core/src/logout_token.rs:87-108`, `crates/gateway/src/backchannel_logout.rs:98-177`) |
| Gateway app-session JWT cookie | Gateway Ed25519 signer | Gateway local verifier | 15m plus verifier leeway | Yes when DB exists, family marker | Identity and scopes for one app (`crates/gateway/src/session_token.rs:58-104`, `crates/gateway/src/session_token.rs:165-207`, `crates/gateway/src/router/auth.rs:977-1021`) |
| Gateway reload anchor | Gateway | Gateway RLS-bound Postgres lookup | 30d absolute | Yes, row delete or `revoked_at` plus OP family; connected-grant revoke misses it, Finding 7 | Recover and re-sign app session, not dispatch directly (`crates/gateway/src/anchors.rs:177-260`, `crates/auth/src/identity/password_reset.rs:343-348`) |
| Gateway OIDC stash | Gateway HMAC signer | Gateway callback | 10m | Yes, key rotation | One automatic RP callback (`crates/gateway/src/oidc_rp.rs:148-311`, `crates/gateway/src/oidc_rp.rs:1017-1041`) |
| OP device and user codes | Auth | Auth OP token and shared device page | 10m, one use | No supported early revoke; consume or expiry | RFC 8628 OP grant (`crates/auth/src/oidc/device_token.rs:124-187`, `crates/auth/src/oidc/device_token.rs:298-518`) |
| Control device and user codes | Control | Control poll and Auth device page | 10m, one use | No supported early revoke; consume or expiry | `zeroship login` approval (`crates/control/src/device_handlers.rs:152-222`, `crates/control/src/device_handlers.rs:304-536`) |
| CLI platform access JWT | Auth, requested by Control | Control or Migrated OAuth verifier | 12h maximum | No supported recall path; a family marker would be honored and lifecycle is checked | Fixed deploy-scope subset (`crates/control/src/device_handlers.rs:70-89`, `crates/authn/src/lib.rs:320-376`, `crates/core/src/device_grant.rs:34-43`, `crates/migrated/src/main.rs:166-179`) |
| Supabase GoTrue bearer | Configured Supabase project | Control Supabase verifier | Provider JWT lifetime | No local recall | General Control access under mapped grants and Cedar, including device approval (`crates/core/src/auth_provider/supabase.rs:312-390`, `crates/authn/src/lib.rs:378-445`, `crates/control/src/device_handlers.rs:815-958`) |
| `ZeroShip-User` | Gateway HMAC signer | Worker | 60s, request-ID bound | No, no independent row | Identity for exactly one dispatch (`crates/core/src/auth/mod.rs:322-433`) |
| `worker_key` bearer | Operator config | Worker | Config lifetime | Yes, global rotation | Gateway dispatch and Control log retrieval (`crates/worker/src/handler.rs:67-116`, `crates/control/src/api.rs:2162-2222`) |
| `control_key` bearer | Operator config | Control | Config lifetime | Yes, global rotation | Broad internal Control API (`crates/control/src/internal.rs:16-40`, `crates/control/src/internal.rs:82-263`) |
| App-scoped Control HMAC | Worker Rust derives | Control | `control_key` lifetime | Yes, global rotation only | Workflow operations for asserted app ID (`crates/core/src/auth/mod.rs:157-174`, `crates/control/src/workflow_instance_api.rs:326-405`) |
| Workflow signal capability `wst_` | Control per-app HMAC signer | Control | At most 24h; run once per allowed type, topic once total | No general revoke; run epoch may stale on deploy-changing restart | Post an allowed signal to one bound run or topic (`crates/core/src/typed_id.rs:526-622`, `crates/control/src/workflow_instance_api.rs:2350-2587`, `crates/control/src/workflow_instance_api.rs:2746-2757`, `crates/control/src/workflow_instance_api.rs:2852-2888`) |
| Broker-derived client secret | Gateway derives from master | Auth re-derives | Master lifetime with current/previous overlap | Yes, master rotation | Per-client code, refresh, introspection, and revoke authentication (`crates/core/src/auth/mod.rs:83-96`, `crates/auth/src/oidc/issuer.rs:853-860`, `crates/auth/src/oidc/introspect.rs:58-66`) |
| Registered OAuth client secret | Control random generator | Auth stored-hash verifier | Until client deletion or replacement | Yes, delete or replace client | Confidential-client code, refresh, introspection, and revoke authentication (`crates/control/src/oauth_handlers.rs:63-127`, `crates/auth/src/oidc/refresh.rs:910-958`) |
| Legacy `X-Api-Key` | Route configuration | Unwired Gateway helper | Config lifetime | Yes, route change | Nothing on current dispatch path; see Finding 28 (`crates/gateway/src/auth.rs:1-27`) |

## Trust-boundary summary

| Component | Trusted to assert or hold | Must prove before accepting or forwarding |
|---|---|---|
| Browser | Password, TOTP, raw email token, PKCE verifier, state, nonce, and cookies in its origin | CSRF, state, PKCE, nonce where implemented, and possession of the presented human credential (`crates/auth/src/server.rs:63-205`, `sdks/auth/src/client.ts:294-384`) |
| CLI | Device code while polling, then the returned platform access JWT; the Unix writer forces mode 0600 | CLI checks platform provider and token presence; Control must validate pending-row state and approval, request a fixed `zeroship-cli` token, and verify subject, fixed client, audience, and scopes before returning it (`crates/cli/src/auth.rs:164-217`, `crates/cli/src/auth.rs:332-340`, `crates/cli/src/auth.rs:451-465`, `crates/control/src/device_handlers.rs:304-536`, `crates/control/src/device_handlers.rs:673-813`) |
| Auth | Human identity, lifecycle result, consent, OP claims, social-link policy, OP private key, provider secrets | Password/TOTP or upstream protocol, live session/user, client/redirect/scope/PKCE, consent, and signing-key issuance eligibility (`crates/auth/src/identity/credentials.rs:81-344`, `crates/auth/src/oidc/authorization_code.rs:283-861`, `crates/auth/src/oidc/issuer.rs:766-799`) |
| Google/GitHub | Provider subject and selected profile facts | Auth must verify their protocol response and apply its stricter email policy (`crates/auth/src/identity/oauth/google.rs:67-210`, `crates/auth/src/identity/oauth/github.rs:65-213`) |
| Gateway | App route, cookie validity, OP JWT validity, pairwise projection, route auth level/scopes, request identity HMAC, public signal transport | Signature/type/issuer/expiry, app/client/audience, CSRF for cookie mutation, family marker when DB is configured, and route policy; it does not validate `wst_` (`crates/gateway/src/router/auth.rs:142-460`, `crates/gateway/src/router/auth.rs:654-1021`, `crates/gateway/src/signal_ingress.rs:67-125`) |
| Control plus authn/authz | Creator principal, OAuth provider result, current owner authority, the scope-derived wrapper policy | Bearer cryptography and DB state and principal lifecycle; owner/wrapper Cedar only where a handler calls `require`. Every accepted bearer now carries a wrapper built from the closed scope vocabulary, so one Cedar action is unreachable, Finding 30 (`crates/authn/src/lib.rs`, `BearerVerifier::verify_bearer` and `oauth_guard_from_bearer`; `crates/control/src/authz_guard.rs`, `AuthzGuard::require`) |
| Control workflow API | App-scoped operations and signed `wst_` claims; master and per-app HMAC keys | Mint requires app-scoped HMAC and live target; ingress requires Gateway `control_key`, then capability HMAC, expiry, type, target, epoch, and replay (`crates/control/src/workflow_instance_api.rs:326-405`, `crates/control/src/workflow_instance_api.rs:2129-2266`, `crates/control/src/workflow_instance_api.rs:2317-2587`) |
| Migrated plus authn/authz | Independently verified creator principal and migration policy result | Reverify the forwarded raw bearer, active principal, app ownership, Cedar deploy action, and operator-ceiling intersection (`crates/migrated/src/api.rs:79-108`, `crates/migrated/src/auth.rs:63-143`, `crates/migrated/src/policy.rs:110-133`, `crates/migrated/src/apply.rs:214-239`) |
| External signal caller | A plaintext `wst_` capability and its permitted payload | Present the capability in the JSON body; Gateway proves nothing about it and Control proves HMAC, expiry, type, target, epoch, and replay (`crates/gateway/src/signal_ingress.rs:67-125`, `crates/control/src/workflow_instance_api.rs:2317-2587`) |
| Control scheduler | Workflow run and app IDs in the advance JSON body | It currently proves no caller identity to Gateway; see Finding 15 (`crates/control/src/cron/workflow_engine.rs:194-232`, `crates/gateway/src/router/dispatch.rs:99-160`) |
| Worker | A `worker_key` holder and an optional fresh request-bound identity assertion; topology expects Gateway | Always prove shared bearer; only when a user header exists, prove HMAC, request UUID, and age before V8 entry (`crates/worker/src/handler.rs:67-116`, `crates/worker/src/handler.rs:205-220`, `crates/worker/src/logs.rs:48-56`) |
| Runtime and creator app | The identity JSON Worker bound to the current invocation | Only presence for `requireUser`; it does not revalidate upstream credentials (`crates/runtime/src/auth.rs:75-109`, `crates/runtime/src/auth.rs:115-204`) |
| Postgres | Atomic one-time consumption, session/revocation state, grants, policies, and tenant RLS | Callers must use the correct row lock, owner predicate, or tenant GUC (`crates/auth/src/oidc/authorization_code.rs:619-692`, `crates/gateway/src/anchors.rs:26-40`) |
| Compose container filesystem | Control, Migrated, Gateway, and Auth can each read the complete mounted secrets directory | No per-secret filesystem custody is enforced by the shipped root-run image; see Finding 6 (`deploy/compose/docker-compose.yml:367-373`, `deploy/compose/docker-compose.yml:447-449`, `deploy/compose/docker-compose.yml:497-500`, `deploy/compose/docker-compose.yml:758-761`, `deploy/Dockerfile:197-229`) |
| Service network | No identity assertion beyond possession of bearer keys, the forwarded caller bearer, or a fetched JWKS | Peer identity, privacy, and trust-anchor integrity are external assumptions on application-permitted HTTP hops; see Finding 18 (`crates/gateway/src/sync.rs:209-260`, `crates/control/src/migrations_api.rs:162-196`, `crates/core/src/auth_provider/platform.rs:197-230`) |

## Known in-progress work

These two items were provided as known-open work and are not counted as new
findings.

1. Raw OP bearer lifecycle. VERIFIED: the complete Gateway arm verifies the JWT,
   client and audience, the family marker when DB is configured, and identity
   projection, but never reads the global user row
   (`crates/gateway/src/router/auth.rs:654-825`). The scoped
   search was
   `rg 'disabled_at|deletion_requested_at|anonymized_at|locked_until|require_active' crates/gateway/src crates/gateway/tests`;
   it found no lifecycle check in that path. Control's bearer convergence does
   perform the positive lifecycle check
   (`crates/authn/src/lib.rs:338-376`). A separate branch is adding the missing
   Gateway check.
2. Platform mint caller allowance. VERIFIED: the dedicated mint bearer has no
   caller identity or caller policy. Auth intersects requested scopes with the
   subject's `principal_grants` and applies the TTL cap
   (`crates/auth/src/oidc/device_token.rs:604-715`), while Control currently
   supplies its fixed deploy allowlist
   (`crates/control/src/device_handlers.rs:620-754`). That is a subject-grant
   ceiling, not a caller-specific allowance. A separate branch is adding the
   caller-allowance half.

## FINDINGS

### 1. RESOLVED: Raw OP access tokens keep the issuer projection

The audit snapshot originally found a second pairwise derivation in Gateway.
Commit `290c85e0a` removed that path before the auth-suite diagnosis on
2026-08-16. Auth derives the app access-token subject from the global UUID and
client sector (`crates/auth/src/oidc/authorization_code.rs:1430-1475`,
`crates/auth/src/oidc/issuer.rs:410-429`,
`crates/auth/src/oidc/issuer.rs:847-851`). Gateway now requires the verified
`claims.sub` to have the exact pairwise shape, copies it unchanged for the
lifecycle and family-marker checks, and emits that same value in
`ZeroShip-User` (`crates/gateway/src/router/auth.rs:625-692`). The former
`project_pairwise` helper no longer exists. `derive_pairwise` still hashes a
non-UUID input verbatim (`crates/core/src/auth/mod.rs:302-318`), but the raw
bearer path no longer calls it.

The real-OP E2E now sends the access token through a local Worker boundary,
verifies the request-bound `ZeroShip-User` header there, and asserts that its
`id` equals Auth's one projection from the global UUID and sector
(`crates/gateway/tests/oidc_rp_e2e.rs`). The Gateway fixture deliberately uses
a different pairwise salt, so a second Gateway derivation cannot satisfy the
assertion.

The deterministic suite failures observed at `e62b60dc7` were stale fixtures,
not this resolved production path. For the eight Gateway failures,
instrumentation measured a stored
`pws_seed_8ed3d71548e04205832647f488c5a77e` against the requested
`pws_6LttJUCDnqZy1AhlkD9k`: the relay fixture fabricated the former while the
cookie mint computed the latter through `auth_token::pairwise_sub`. The strict
immutable-binding guard correctly refused the swap. One Auth failure
constructed a logout-token issuer without publishing its key, so registered
token issuance correctly refused it as untrusted for issuance.

The "nine failures" count was itself an artifact: `cargo test` stops at the
first failing target, and the logout binary sorts early, so THIRTY later Auth
binaries never ran at all, taking 161 of the 185-test delta with them. That
figure is measured, not estimated: reduce a completed suite log to one
`<binary> <passed>` line per target in cargo's run order, cut at the last
`zeroship-auth` target, and sum everything after `oidc_backchannel_logout_test`
(the reproduction is written out in `NOTES.md`). This paragraph said "six" until
that measurement was run; the conclusion it supported - that truncation, not the
repaired failures, produced the delta - holds a fortiori.

Fixing it revealed a third fixture defect of the same family: ten binaries
published an OP signing key into the one shared suite database, and TWO of the
seeds were shared - 42 across three of those binaries and 43 across two (this
also read "three seeds used twice" before it was checked) - so a binary
publishing a duplicate kid after an intervening publish retired it hit the same
correct-and-failing-closed refusal. Test fixtures now derive that key from
their own target name (`crates/auth/tests/common/mod.rs`).

### 2. HIGH: App-session revoke deletes a row Gateway does not authorize from

VERIFIED: `/me/sessions/{id}/revoke` presents `gateway_sessions` rows as active
app sessions and, for `kind=app`, deletes only that row
(`crates/auth/src/ui/sessions.rs:1-24`,
`crates/auth/src/ui/sessions.rs:98-171`,
`crates/auth/src/store/sessions.rs:222-274`). Gateway explicitly does not consult
the row for a request; it validates the self-contained cookie and family marker
(`crates/gateway/src/sessions.rs:1-15`,
`crates/gateway/src/auth_token.rs:636-639`,
`crates/gateway/src/auth_token.rs:710-789`). The endpoint also does not delete
the reload anchor.

INFERRED impact: the endpoint can return `revoked=true` while a copied cookie
remains usable until expiry and the surviving anchor can mint another cookie.
The listed app-session idle time is also not a reliable activity signal because
ordinary requests never slide that audit row.

### 3. RESOLVED BY DELETION: PAT issuance discarded the caller's OAuth scope ceiling

What was found, kept so the defect stays legible: `create_token` claimed an
interactive OAuth/BFF session was required but only rejected
`guard.token_id.is_some()`. Every OAuth bearer, including a `zeroship-cli`
token, arrives as `token_id=None` with a scope-derived wrapper policy, so it
passed. The grant-subset check then built a fresh context with both
`token_id=None` and `token_policy=None`, so it consulted only the subject's
static authority; the caller's own scope ceiling never entered the decision.
The control test suite called that shape interactive while authenticating with
`client_id=zeroship-cli`.

INFERRED impact as recorded: a valid low-scope, empty-scope, or 12-hour CLI
OAuth bearer could mint a PAT lasting up to 365 days carrying any permission
the underlying subject held. The request-time owner-and-wrapper intersection
was sound; the defect was the issuance-time caller boundary.

RESOLVED BY DELETION, not by adding the missing ceiling check: `POST /me/tokens`
and control's `token_handlers` module are gone, so there is no issuance-time
caller boundary left to get wrong. The operator decision is section 11 of
`docs/proposals/2026-08-16-cli-token-issuance.md`, which names this finding as
the one that deletion answers. See section 4.1. Every citation this finding
originally carried pointed into control's token-handler module or its test
binary, both of which were deleted with the routes, so the statement above is
made in prose with no file reference left to resolve.

### 4. RESOLVED BY DELETION: PAT token management bypassed the token's policy

What was found: `list_tokens` and `delete_token` extracted `AuthzGuard` but
never called its `require` method; they proceeded straight to an owner-ID
predicate. `AuthzGuard::require` is the only bridge from an authenticated
principal to `authz::enforce`, where current owner authority and the caller's
wrapper policy are intersected (`crates/control/src/authz_guard.rs`,
`crates/authz/src/eval.rs`, `enforce`). Authentication, active-token lookup and
owner lifecycle still ran before the handler.

INFERRED impact as recorded: any active PAT for an owner, including a deny-only
or narrowly scoped one, could enumerate and revoke every sibling PAT for that
owner. The owner predicate blocked cross-owner access, but the advertised
token-policy ceiling was absent on those two authorization decisions.

RESOLVED BY DELETION: `GET /me/tokens`, `DELETE /me/tokens/{id}` and the handler
module are gone, and so is the `zeroship.permission_tokens` table they read
(`db/migrations-ts/20260817000000_drop_permission_tokens.ts`). No route reaches
that decision any more. See section 4.1.

### 5. HIGH: `auth: "admin"` is enforced exactly as `auth: "user"`

VERIFIED: Gateway combines user and admin in the same cookie and bearer decision
arms without checking an administrator claim
(`crates/gateway/src/router/auth.rs:329-345`,
`crates/gateway/src/router/auth.rs:384-395`). The manifest type itself says
admin is currently an alias for user
(`crates/bundle/src/rule.rs:131-155`), and the RPC reference warns that any
signed-in end user reaches such a procedure (`docs/reference/rpc.md:122-131`).

This is a trust level exposed in configuration but not enforced in the decision
point. It should either acquire a real predicate or disappear under the
repository's pre-launch no-compat policy.

### 6. HIGH: Shipped containers can read one another's private auth material

VERIFIED: `zeroship dev init` creates the Auth, Gateway, and Control private
signing keys, broker master, refresh HMAC keyring, refresh idempotency AEAD key,
and pairwise salt in one secrets directory (`crates/cli/src/dev.rs:195-255`).
Compose mounts that complete directory read-only into Control, Migrated,
Gateway, and Auth rather than mounting each process's declared inputs
(`deploy/compose/docker-compose.yml:367-373`,
`deploy/compose/docker-compose.yml:447-449`,
`deploy/compose/docker-compose.yml:497-500`,
`deploy/compose/docker-compose.yml:758-761`). The shared runtime image creates
no non-root user and sets no `USER` (`deploy/Dockerfile:197-229`).

Search method: every `FROM` stage in `deploy/Dockerfile` was inspected, and
`rg '^USER([[:space:]]|$)' deploy/Dockerfile` returned no directive. Thus the
earlier process-level statements about which binary loads a secret do not form
a container filesystem boundary.

INFERRED impact: compromise of any one of these root-run native containers can
read private signing and broker material assigned to other services, collapsing
the intended signer and verifier custody split. Read-only mounting prevents
modification, not exfiltration.

### 7. HIGH: Connected-app revoke leaves refresh and BFF recovery live

VERIFIED: the handler comment says it revokes the grant and active tokens, but
the disconnect transaction deletes the `oauth_grants` row, revokes the relay
alias, and writes an access-token family marker without updating
`oauth_refresh_tokens` or `app_session_anchors`
(`crates/control/src/oauth_grants_handlers.rs:82-84`,
`crates/control/src/oauth_grants_handlers.rs:101-155`,
`crates/control/src/oauth_grants_handlers.rs:158-231`). The refresh exchange
authenticates the client, checks `refresh_allowed`, then decides from the stored
refresh row and family state; a valid row is rotated and passed to the normal
access-token signer
(`crates/auth/src/oidc/refresh.rs:435-590`,
`crates/auth/src/oidc/authorization_code.rs:1387-1409`). Family markers reject
only access tokens whose `iat` predates the marker, so Auth introspection reports
a newly minted post-marker token active
(`crates/authz/src/wrapper_revocation.rs:44-103`,
`crates/auth/src/oidc/introspect.rs:97-153`).

The Gateway BFF path makes the gap directly exploitable on current code without
depending on the now-resolved Finding 1. A marked cookie falls through to
anchor recovery; the still-live anchor drives an Auth refresh, and Gateway
signs the rotated facts
into a new cookie
(`crates/gateway/src/auth_token.rs:740-949`,
`crates/gateway/src/auth_token.rs:1060-1101`). The authoritative post-refresh
check rejects markers written at or after the rotation start; a disconnect
marker below a later rotation start passes
(`crates/gateway/src/auth_token.rs:1210-1239`).

Search method: scoped `rg 'oauth_grants' crates/auth/src/oidc/refresh.rs`
returned no match. That empty result was not treated as proof by itself: the
complete positive refresh decision and rotation path above was read, the grant
revoke transaction was read statement by statement, and the migration's full
anchor/grant/refresh FK inventory has independent app, client, and user
cascades but no grant-to-refresh or grant-to-anchor edge
(`db/migrations-ts/20260702000600_constraints_indexes_fks.ts:126-128`,
`db/migrations-ts/20260702000600_constraints_indexes_fks.ts:174-180`).

INFERRED impact: a surviving anchor restores BFF authorization on current code,
and possession of an otherwise-live refresh token can restore an active access
token after the owner disconnects the app. This remains a bug after the
raw-Gateway pairwise defect in Finding 1 is fixed because each new credential's
`iat` legitimately passes the intended marker comparison. Gateway signs the
new cookie with an empty email, the revoked reverse-map alias still makes
UserInfo fail, and inbound relay mail stays disabled
(`crates/gateway/src/auth_token.rs:841-949`,
`crates/gateway/src/identities.rs:112-151`,
`crates/auth/src/oidc/userinfo.rs:141-166`,
`crates/auth/src/store/relay.rs:229-250`), but revocation of app authorization
is not final.

### 8. MEDIUM: The BFF stores a nonce but never verifies it

VERIFIED: the SDK generates, transmits, and stores a nonce
(`sdks/auth/src/client.ts:294-321`,
`sdks/auth/src/internal/transaction.ts:1-27`). Completion checks state and sends
code, verifier, and redirect URI, but no nonce
(`sdks/auth/src/client.ts:324-384`,
`sdks/auth/src/internal/transport.ts:170-204`). Gateway passes
`expected_nonce=None` to ID-token verification and comments that the SDK already
guards it (`crates/gateway/src/auth_token.rs:446-485`).

Search method: `rg 'txn\.nonce|\.nonce' sdks/auth/src --glob '*.ts'` found nonce
generation and storage but no read or comparison on completion. State and S256
PKCE still provide material defenses, so this is ranked below the direct
identity and revocation failures, but the asserted OIDC boundary is absent.

### 9. RESOLVED: Supabase device approval would be cross-origin without CORS

Auth's Supabase `/device` page sent `Authorization` and `application/json`
from the Auth origin to Control `/api/device/approve`, which the shipped Caddy
topology puts on a different origin, with no CORS or preflight handler
anywhere in `deploy/ops` or `crates/control/src`. RESOLVED BY DELETION rather
than by adding CORS: the Supabase deploy path is retired, the page and the
endpoint are both gone, and no browser request crosses that boundary any more.

### 10. MEDIUM: First consent loses the relay alias until re-consent

VERIFIED: consent is the only production relay-alias mint, but it can only
update a Gateway-created identity row
(`crates/auth/src/ui/consent.rs:197-243`,
`crates/auth/src/store/relay.rs:56-120`). On first consent that row does not yet
exist, so the function returns `None`; Gateway later inserts the pairwise row but
deliberately leaves `relay_email` untouched
(`crates/auth/src/store/relay.rs:121-135`,
`crates/gateway/src/identities.rs:32-77`). A stored grant then lets later logins
skip consent (`crates/auth/src/store/relay.rs:82-92`). Gateway converts a missing
alias to the empty string rather than the real inbox
(`crates/gateway/src/auth_token.rs:545-550`,
`crates/gateway/src/router/auth.rs:795-808`).

INFERRED impact, also documented by the implementation's measured account: a
normal first grant of `email` gives that app `email: ""` on every later login
until explicit re-consent or grant deletion. This fails closed for privacy but
fails the granted identity contract.

### 11. MEDIUM: Explicit logout succeeds after authoritative revocation failure

VERIFIED: Gateway signout logs a family-marker write failure and continues to
delete what it can, clear cookies, and return success
(`crates/gateway/src/browser_auth.rs:382-519`). The family marker, not the anchor
or audit row, is the check that rejects a still-live signed cookie
(`crates/gateway/src/session_token.rs:30-50`). Native Auth logout likewise logs a
session-row revoke failure, clears the local cookie, and succeeds
(`crates/auth/src/ui/logout.rs:55-129`), while the unchanged row remains valid
under the normal session predicate
(`crates/auth/src/store/sessions.rs:87-118`).

INFERRED impact: during a DB write failure, a copied 15-minute app cookie or
copied IdP cookie remains usable even though the user saw successful logout.

### 12. MEDIUM: The automatic RP requests and discards a refresh credential

VERIFIED: the protected-HTML RP requests `offline_access`
(`crates/gateway/src/oidc_rp.rs:148-199`). `finish_callback` deserializes the
refresh token but returns only claims, path, and scopes
(`crates/gateway/src/oidc_rp.rs:219-225`,
`crates/gateway/src/oidc_rp.rs:314-347`,
`crates/gateway/src/oidc_rp.rs:836-848`). Its only caller writes an audit row and
15-minute cookie, not an anchor
(`crates/gateway/src/router/dispatch.rs:2793-2890`). By contrast, the BFF path
encrypts the refresh token and creates the 30-day anchor
(`crates/gateway/src/auth_token.rs:512-532`,
`crates/gateway/src/auth_token.rs:554-708`).

Search method: `rg 'tr\.refresh_token|finish_callback\(' crates/gateway/src`
found no refresh use and one caller. INFERRED: protected HTML navigation must
redo OIDC after 15 minutes while Auth has created a refresh family that no RP
state can use.

### 13. MEDIUM: The two RFC 8628 implementations have already drifted

VERIFIED: the shared vocabulary explicitly identifies two state machines in one
table and records prior producer/consumer drift
(`crates/core/src/device_grant.rs:1-26`). Current differences remain:

- OP requires and later binds a registered public `client_id`; Control accepts
  arbitrary nonempty input, does not store it, and always mints for
  `zeroship-cli` (`crates/auth/src/oidc/device_token.rs:124-147`,
  `crates/auth/src/oidc/device_token.rs:355-380`,
  `crates/control/src/device_handlers.rs:167-202`,
  `crates/auth/src/oidc/device_token.rs:673-701`).
- OP rejects scopes outside client registration; Control silently filters to
  four deploy scopes (`crates/auth/src/oidc/device_token.rs:140-147`,
  `crates/control/src/device_handlers.rs:620-670`).
- OP increases the stored interval on `slow_down`; Control keeps the fixed
  interval (`crates/auth/src/oidc/device_token.rs:387-403`,
  `crates/control/src/device_handlers.rs:378-393`).
- Control rate limits unauthenticated start; OP start has no request/IP input or
  limit call (`crates/control/src/device_handlers.rs:152-163`,
  `crates/auth/src/oidc/device_token.rs:109-193`).

Both delete an expired row only when that exact device code is polled
(`crates/control/src/device_handlers.rs:364-375`,
`crates/auth/src/oidc/device_token.rs:382-385`). Search method:
`rg -n device_grants crates/auth/src/cron crates/control/src/cron
db/migrations-ts` found no sweeper; inspection of the token sweep inventory also
omits device grants (`crates/auth/src/cron/token_sweep.rs:37-58`,
`crates/auth/src/cron/token_sweep.rs:101-156`). INFERRED: unpolled starts leave
expired rows indefinitely, and the unauthenticated OP start can grow them once
an eligible public client exists.

### 14. MEDIUM: The workflow signal public contract is not wired to live ingress

VERIFIED: the reference documents nested run/topic paths, a signal token in the
Authorization header, and an `Idempotency-Key` header
(`docs/reference/workflows.md:547-567`). Gateway actually registers only two
exact paths and ignores the external Authorization header; Control expects the
token inside the forwarded JSON body
(`crates/gateway/src/main.rs:583-600`,
`crates/gateway/src/signal_ingress.rs:67-125`,
`crates/control/src/workflow_instance_api.rs:137-144`). Both documented `48h`
mint examples exceed Control's 24-hour maximum
(`docs/reference/workflows.md:573-594`,
`crates/control/src/workflow_instance_api.rs:49-55`,
`crates/control/src/workflow_instance_api.rs:580-588`).

The workflows SDK declares `WorkflowRun.createSignalToken`, but its complete
native run-method inventory and backend trait contain no such operation; the
separate Control SDK helper is the only concrete client implementation
(`sdks/workflows/src/index.ts:158-167`,
`crates/plugin-workflow/src/v8_class.rs:448-531`,
`crates/plugin-workflow/src/backend.rs:17-27`,
`sdks/control/src/index.ts:275-299`). Search method:
`rg -n 'createSignalToken|create_signal_token' crates/plugin-workflow
crates/runtime crates/worker sdks/workflows/src sdks/control/src` found the
workflows interface and Control helper but no run-object implementation.

The real-stack test helpers also mint and send without the now-required
app-scoped bearer, although the harness launches Control with a strong nonempty
key (`crates/control/tests/durable_workflows_keystone_e2e.rs:2556-2648`,
`tests/lib/runtime_secrets.sh:123-141`,
`tests/e2e_durable_workflows.sh:591-599`). INFERRED: users following the
reference cannot reach the live ingress contract, the promised run helper is
unavailable at runtime, and that real-stack success scenario cannot pass the
mint boundary as configured, so it does not currently prove the intended path.

### 15. MEDIUM: Workflow advance does not authenticate Control to Gateway

VERIFIED: Control's scheduler posts a JSON body with only `content-type`; it
presents no bearer, MAC, signature, nonce, or caller identity
(`crates/control/src/cron/workflow_engine.rs:194-232`). Gateway exposes the path
on its normal listener, trusts body-supplied run and app IDs after route,
account, and spend checks, and carries an explicit TODO for signature and nonce
verification (`crates/gateway/src/main.rs:582-586`,
`crates/gateway/src/router/dispatch.rs:99-160`). Gateway adds `worker_key` only
on the second hop (`crates/gateway/src/proxy.rs:239-270`).

Worker's unsigned endpoint verifies that shared key, defaults disabled, and
cannot be enabled with a non-loopback bind
(`crates/worker/src/handler.rs:525-554`,
`crates/worker/src/main.rs:228-240`). INFERRED: the default constraints reduce
current reachability, but when the feature is deliberately enabled, the first
hop still has no cryptographic way to distinguish Control from another caller
that can reach Gateway.

### 16. RESOLVED BY DELETION: Migrated loaded the PAT private signing key only to verify

What was found: Migrated required Control's PAT signing-key file
(`--signing-key-file` / `ZEROSHIP_MIGRATED_SIGNING_KEY_FILE`) and built a full
`PatIssuer` from it. That type retained the PKCS#8 private bytes and exposed
issuance as well as verification, yet Migrated passed it only into the shared
bearer verifier. Search method: `rg 'pat_issuer|PatIssuer|\.issue\(' crates/migrated/src`
found construction, verifier injection and tests, but no issuance call. The
INFERRED remedy recorded at the time was to hand Migrated a public verification
key instead, restoring the Control-signer / Migrated-verifier custody split.

RESOLVED BY DELETION rather than by splitting the key: `PatIssuer` no longer
exists, and both Migrated's and Control's `--signing-key-file` inputs were
removed with it, since building a `PatIssuer` was the only thing either did
with one. Migrated now holds no signing key at all and verifies platform OAuth
bearers through the OP's published JWKS (`crates/migrated/src/main.rs`). Note
that Gateway's identically named `--signing-key-file` is a different consumer -
it signs app-session wrapper tokens - and is untouched.

### 17. MEDIUM: Control and Migrated disagree on accepted OAuth issuers

VERIFIED: in Supabase mode Control constructs a verifier set containing
Supabase and, when configured, the platform OP
(`crates/control/src/main.rs:118-160`). After Control authenticates and
authorizes a migration apply, it forwards the caller's raw bearer unchanged
(`crates/control/src/migrations_api.rs:70-118`,
`crates/control/src/migrations_api.rs:147-196`). Migrated independently builds a
platform-only provider (`crates/migrated/src/main.rs:217-237`).

INFERRED impact: a Supabase bearer that is valid for general Control operations
and passes `AppsDeploy` is rejected by Migrated, so the parallel verifier
implementations disagree across one request. Reverification is the correct
boundary; the accepted issuer set is the drift.

### 18. MEDIUM: Service auth uses application-permitted plain HTTP

VERIFIED: Gateway route sync rejects schemes other than `http` and states that
the key travels in clear, then writes the bearer in a hand-built TCP request
(`crates/gateway/src/sync.rs:209-260`). The same possession credential authorizes
decrypted app environment data and other broad internal Control endpoints
(`crates/control/src/internal.rs:82-263`).

This is not isolated to `control_key`. Control also forwards the creator's
raw OAuth or PAT bearer to Migrated over an HTTP-default service URL
(`deploy/compose/docker-compose.yml:305-316`,
`crates/control/src/migrations_api.rs:147-196`).

Gateway sends raw `worker_key` and any signed identity over raw TCP to the
Compose-generated HTTP worker URLs. Control sends the same key to worker log
endpoints, whose configured default is HTTP
(`crates/gateway/src/proxy.rs:159-173`,
`crates/gateway/src/proxy.rs:532-568`,
`deploy/compose/docker-compose.yml:470-482`,
`crates/control/src/api.rs:2209-2228`,
`crates/control/src/config.rs:76-78`). Worker sends both broad `control_key`
requests and app-scoped HMAC workflow requests to the shipped HTTP Control URL
(`crates/worker/src/sync.rs:565-600`,
`crates/plugin-workflow/src/client.rs:124-164`,
`crates/plugin-workflow/src/client.rs:230-263`,
`deploy/compose/docker-compose.yml:573-584`). Gateway also sends its
broker-derived OAuth client secret to Auth in form bodies, with an HTTP default
Auth base (`crates/gateway/src/oidc_rp.rs:248-270`,
`crates/gateway/src/oidc_rp.rs:371-407`,
`crates/gateway/src/config.rs:171-172`).

The integrity side is broader than bearer disclosure. Compose configures both
Control and Migrated to fetch Auth's signing trust anchor over HTTP. Gateway's
HTTP-capable Auth base similarly determines its JWKS URL
(`deploy/compose/docker-compose.yml:272-284`,
`deploy/compose/docker-compose.yml:440-446`,
`deploy/compose/docker-compose.yml:478-486`,
`crates/gateway/src/oidc_rp.rs:87-110`). The platform verifier trusts the fetched
key that matches `kid` and EdDSA, then checks the token's claimed issuer
(`crates/core/src/auth_provider/platform.rs:72-92`,
`crates/core/src/auth_provider/platform.rs:197-230`).

The design therefore relies on network isolation or an external confidential
hop for these possession credentials, identity assertions, and trust anchors;
the application layer proves neither channel confidentiality nor peer identity.
Severity depends on deployment isolation, but the boundary is not enforced by
these protocols. INFERRED: an on-path actor on an HTTP JWKS fetch can substitute
its own public key and sign a token carrying the expected issuer, turning
missing transport integrity into token forgery.

### 19. MEDIUM: OAuth client registration fields do not constrain Auth

VERIFIED: Control requires `grant_types` and `response_types` in registration
JSON, but validation checks neither. Persistence uses only the presence of
literal `refresh_token` to set `refresh_allowed` and discards every response
type and all other grant values
(`crates/control/src/oauth_handlers.rs:20-32`,
`crates/control/src/oauth_handlers.rs:245-274`,
`crates/control/src/oauth_handlers.rs:374-409`). Auth permits authorization code
for every loaded client and device flow for every public, non-brokered client,
regardless of those advertised grant lists; only refresh checks the stored flag
(`crates/auth/src/oidc/authorization_code.rs:290-330`,
`crates/auth/src/oidc/device_token.rs:124-148`,
`crates/auth/src/oidc/refresh.rs:440-449`).

The authentication-method field also does not constrain brokered clients.
Control stores every brokered app client as `client_secret_basic`, while
Gateway presents its derived secret as form POST. Auth's broker-first branch
accepts either Basic or POST and bypasses the stored-method check
(`crates/control/src/app_oauth_client.rs:559-574`,
`crates/gateway/src/oidc_rp.rs:248-270`,
`crates/auth/src/oidc/refresh.rs:925-955`,
`crates/auth/src/oidc/authorization_code.rs:1342-1385`). The live flows are
deliberately closed to code response type and authenticated by a valid secret,
so this is not an authentication bypass today. It is an asserted registration
boundary that Auth does not enforce.

### 20. MEDIUM: Several CLI paths put bearer tokens in child argv

VERIFIED: authenticated userinfo, migration, deploy/app, and secret operations
spawn `curl` with the complete `Authorization: Bearer ...` header as a command
argument (`crates/cli/src/auth.rs:275-294`,
`crates/cli/src/auth.rs:511-533`,
`crates/cli/src/migrate.rs:226-257`,
`crates/cli/src/main.rs:635-700`,
`crates/cli/src/secrets.rs:484-509`). The saved credential file is mode 0600 on
Unix, but that at-rest protection is separate from process arguments
(`crates/cli/src/auth.rs:332-340`,
`crates/cli/src/auth.rs:451-465`). INFERRED: on systems where other principals
or process monitors can inspect child argv, the bearer is exposed for the
duration of each request. Passing headers through curl config on stdin or using
an in-process client would avoid that channel.

### 21. MEDIUM: The no-DB startup warning states the opposite of runtime behavior

VERIFIED: Gateway warns that all auth-gated requests will return 401 when no DB
is configured (`crates/gateway/src/main.rs:364-379`). In fact, raw bearer and
signed-cookie arms still authenticate and merely skip family-marker revocation
(`crates/gateway/src/router/auth.rs:142-153`,
`crates/gateway/src/router/auth.rs:761-819`,
`crates/gateway/src/router/auth.rs:988-1021`). A test deliberately proves a
valid cookie works without DB
(`crates/gateway/tests/auth_token_anchors_test.rs:762-793`).

INFERRED impact: an operator can believe authentication is disabled while valid
but unrevocable credentials are being accepted.

### 22. MEDIUM: The direct login page offers a magic flow it cannot start

VERIFIED: direct GET `/login` defaults `return_to=/me`, and its template always
shows a magic-login form carrying that value
(`crates/auth/src/ui/login.rs:52-53`,
`crates/auth/src/ui/templates/login.html:51-61`). Magic start accepts only a
syntactically valid `/oauth2/authorize` return target
(`crates/auth/src/ui/magic.rs:119-171`,
`crates/auth/src/oidc/auth_request.rs:63-72`). A regression test explicitly
asserts that `/me` is invalid
(`crates/auth/tests/e2e_magic_native.rs:481-494`). The visible direct-login
option is therefore unreachable as configured.

### 23. MEDIUM: Account-deletion cancellation cannot authenticate

VERIFIED: requesting deletion atomically disables the user, bumps their
credential version, and revokes every IdP session
(`crates/auth/src/store/users.rs:327-437`). The only cancel route requires an
IdP cookie and calls `sessions::validate` before it can learn which user to
reenable (`crates/auth/src/ui/account_deletion.rs:112-150`,
`crates/auth/src/ui/account_deletion.rs:220-238`). Normal login also rejects the
disabled, deletion-requested account through the shared eligibility path
(`crates/auth/src/identity/credentials.rs:81-344`). The confirmation email links
only to `/me`, not to a separate cancellation credential
(`crates/auth/src/ui/account_deletion.rs:155-195`).

Search method: the complete Auth route inventory contains only the session-gated
POST cancel route (`crates/auth/src/server.rs:138-149`), and a full-tree search
for `cancel_deletion` found production callers only in that handler; the other
uses are store-level tests. INFERRED: after a successful request there is no
supported way for the user to obtain the live IdP session required to cancel
during the advertised 30-day grace period.

### 24. MEDIUM: OP discards authentication provenance before minting ID tokens

VERIFIED: the IdP-session table stores `auth_time`, `amr`, and `acr`, but the
loaded Rust `Session` already omits `auth_time`
(`db/migrations-ts/20260702000300_auth_oauth_tables.ts:172-183`,
`crates/auth/src/store/sessions.rs:7-28`,
`crates/auth/src/store/sessions.rs:35-72`). The authorization-code model and
stored code then omit `amr` and `acr` as well
(`crates/auth/src/oidc/authorization_code.rs:120-131`,
`crates/auth/src/oidc/authorization_code.rs:451-483`), and both ID-token mint
branches hardcode them to `None`
(`crates/auth/src/oidc/authorization_code.rs:755-812`). Gateway faithfully
copies the absence into its audit row and cookie
(`crates/gateway/src/auth_token.rs:552-593`,
`crates/gateway/src/auth_token.rs:675-685`,
`crates/gateway/src/session_token.rs:85-104`).

Search across Gateway, Control, and Runtime found storage and forwarding but no
current step-up consumer. INFERRED: present behavior is mostly lost audit data,
but any future freshness or factor policy would receive no provenance despite
the claim fields already existing.

### 25. MEDIUM: CLI `--provider=supabase` is inert and its refresh path has no producer

VERIFIED: both provider values enter the same Control device function without
passing the selection (`crates/cli/src/auth.rs:80-109`). The only production
login constructor always saves `provider=platform`, an empty refresh token, and
no Supabase endpoint or anonymous key
(`crates/cli/src/auth.rs:198-217`). Help still advertises both values
(`crates/cli/src/main.rs:1014-1015`), while Supabase refresh and userinfo branches
remain (`crates/cli/src/auth.rs:275-329`).

Search method: `rg -n 'Credentials\s*\{|provider:\s*"supabase"|token_endpoint:\s*Some|anon_key:\s*Some|userinfo_url:\s*Some'
crates/cli --glob '*.rs'` found no production Supabase credential constructor.
Under the pre-launch no-backcompat policy these branches are unreachable state,
not a compatibility path.

### 26. MEDIUM: UserInfo depends on a Gateway-only reverse-map writer

VERIFIED: native app and generic public-client tokens issued through
`issue_access_token` carry a pairwise subject, while the separate platform
principal helper deliberately keeps the global UUID
(`crates/auth/src/oidc/issuer.rs:410-429`,
`crates/auth/src/oidc/issuer.rs:453-473`). UserInfo cannot load an app user
directly from the pairwise value; it first requires a live
`app_user_identities` reverse-map row and returns `invalid_token` when none exists
(`crates/auth/src/oidc/userinfo.rs:109-166`). Control can register public OAuth
clients and the OP device flow can issue their tokens without any Gateway hop
(`crates/control/src/oauth_handlers.rs:63-100`,
`crates/auth/src/oidc/device_token.rs:124-187`,
`crates/auth/src/oidc/device_token.rs:468-511`).

Search method: a full-tree search for
`INSERT INTO zeroship.app_user_identities` and `identities::upsert` found the
only production writer in Gateway
(`crates/gateway/src/identities.rs:52-77`). Auth UserInfo tests manually insert
the prerequisite row (`crates/auth/tests/oidc_userinfo_test.rs:424`). INFERRED:
a generic public client that obtains `openid` tokens directly from Auth cannot
use the advertised UserInfo endpoint unless an unrelated Gateway projection has
already created its mapping.

### 27. LOW: `GET /session` drops avatar from its response

VERIFIED: the signed cookie declares and receives `avatar`
(`crates/gateway/src/session_token.rs:92-100`,
`crates/gateway/src/session_token.rs:186-198`). Initial POST returns it
(`crates/gateway/src/auth_token.rs:690-707`), but the GET fast path passes `None`
and comments that the cookie has no picture
(`crates/gateway/src/auth_token.rs:759-780`). A session read therefore changes a
present avatar to null.

### 28. LOW: Dead, duplicate, and unreachable auth code retains misleading state

VERIFIED items, each paired with a positive live path or complete scoped search:

- Legacy `check_api_key` has no production caller. `rg 'check_api_key'` finds its
  definition, a feature-map admission, and tests only
  (`crates/gateway/src/auth.rs:1-27`, `docs/feature-map.md:519`). Gateway's
  non-JWT bearer fallback explicitly remains reserved and returns 401
  (`crates/gateway/src/router/auth.rs:822-824`).
- `verification::redeem` consumes without marking verified, while the live UI
  uses `redeem_and_mark_verified`
  (`crates/auth/src/identity/verification.rs:127-151`,
  `crates/auth/src/ui/verify.rs:83-90`). `password_reset::redeem` consumes
  without changing the password, while live reset uses `complete`
  (`crates/auth/src/identity/password_reset.rs:170-198`,
  `crates/auth/src/ui/reset.rs:250`). Full-tree call searches found only tests
  for both weaker consumers.
- Device handlers support `status='denied'`, but neither UI writes it; production
  writers only set `approved`
  (`crates/control/src/device_handlers.rs:414-423`,
  `crates/auth/src/oidc/device_token.rs:423-425`,
  `crates/auth/src/ui/templates/device.html:13-28`,
  `crates/auth/src/oidc/device_token.rs:244-296`). A production search for
  `status = 'denied'` and `SET status` found only a test writer.
- `platform_access_token_enc` exists in the device-grant schema but has no
  production read or write
  (`db/migrations-ts/20260702000300_auth_oauth_tables.ts:82-100`,
  `crates/control/tests/device_handlers_test.rs:885-890`). Full-tree search for
  the column found only schema and tests.
- Gateway retains current and previous raw private signing keys after deriving
  its issuer and verifier (`crates/gateway/src/main.rs:184-257`,
  `crates/gateway/src/lib.rs:192-215`). A scoped search for `.signing_key` and
  `.prev_signing_key` under `crates/gateway/src` found no `GateState` reads; the
  old private key therefore remains resident unnecessarily.
- Workflow replay and its app-scoped output-read bearer exist in three copies.
  The live Worker path invokes the runtime's inline `__zsWorkflowDispatch`
  (`crates/runtime/src/core/init.rs:1493-1505`,
  `crates/runtime/src/core/init.rs:1645`,
  `crates/runtime/src/core/init.rs:2171-2178`). Bootstrap installs a separate
  global copy that its tests import (`sdks/bootstrap/src/dispatcher.ts:213-216`,
  `sdks/bootstrap/src/dispatcher.ts:1411-1438`,
  `sdks/bootstrap/tests/workflow-dispatch-determinism.test.ts:1-22`). A third
  journal implementation is imported by workflows tests but is not a package
  export (`sdks/workflows/src/journal.ts:205-221`,
  `sdks/workflows/src/journal.ts:959-1031`,
  `sdks/workflows/package.json:13-22`). Search method: full-tree
  `rg '__zsWorkflowDispatch|from .*journal' crates sdks` found the production
  invocation only in the runtime copy and source-journal imports only in tests.
- Auth implements a non-brokered stored `client_secret_post` branch, but
  Control's registration API accepts only Basic or public `none` and both
  automatic client writers hardcode Basic
  (`crates/auth/src/oidc/refresh.rs:937-958`,
  `crates/control/src/oauth_handlers.rs:245-274`,
  `crates/control/src/bootstrap_builder.rs:128-155`,
  `crates/control/src/app_oauth_client.rs:559-574`). A full-tree search for
  `client_secret_post` found metadata, parsing, verification, and tests but no
  production writer for that non-brokered stored state. Brokered POST is live
  through a separate derived-secret branch, as mapped in Section 5.6.
- The workflow signal-key schema admits `zeroship-hmac` and `provider:stripe`
  verifiers, but production mint and verify code uses only `bearer-signing`.
  It also admits key lifecycle states with no production status writer, while
  new minting may select a `retiring` key
  (`db/migrations-ts/20260705000000_durable_workflows_journal.ts:149-167`,
  `crates/control/src/workflow_instance_api.rs:49-55`,
  `crates/control/src/workflow_instance_api.rs:779-861`). Scoped searches for
  all three verifier values and updates to `workflow_signal_keys` found the
  unused values only in the schema and no Rust lifecycle transition.
- `clear_request_user` has no caller; runtime removes the same map entries
  directly on terminal cleanup (`crates/runtime/src/auth.rs:68-73`,
  `crates/runtime/src/core/runtime.rs:3357-3405`). A workspace search found only
  its definition.
- `migrated` declares `control_key` but uses it only for configured-state
  reporting (`crates/migrated/src/config.rs:101-103`,
  `crates/migrated/src/main.rs:100-106`). A crate-scoped search found no request
  authentication use.
- `BearerVerifier.trusted_oauth_clients` is stored and exposed but not used in
  bearer verification (`crates/authn/src/lib.rs:173-203`). Searches for the
  getter and field uses found no decision-path read; Control separately uses its
  own set during client registration (`crates/control/src/oauth_handlers.rs:85`).

### 29. LOW: Comments and reference docs describe auth that no longer exists

VERIFIED drift:

- `docs/reference/auth.md:301-319` describes a Gateway DPoP proof and
  introspection path, while `docs/feature-map.md:527` says the DPoP arm was
  removed. Scoped searches for `DPoP`, proof fields, and introspection in
  Gateway source and tests found no such request path; the live path is plain
  Bearer plus local JWKS verification
  (`crates/gateway/src/router/auth.rs:654-825`).
- `crates/gateway/src/oidc_rp.rs:945-954` calls the cookie an opaque 12-hour DB
  session, but the implementation immediately below makes a 15-minute JWT
  (`crates/gateway/src/oidc_rp.rs:960-1014`).
- `crates/gateway/src/auth_token.rs:710-718` calls `gateway_sessions` the primary
  GET source, but the implementation verifies cookie claims and a family marker
  (`crates/gateway/src/auth_token.rs:740-789`).
- `crates/gateway/src/router/auth.rs:895-917` describes an uncached revocation
  query, but the live path uses the read-through cache
  (`crates/gateway/src/router/auth.rs:977-1004`).
- `crates/runtime/src/auth.rs:1-7` says identity comes only from the cookie, while
  the raw bearer arm reaches the same header (`crates/gateway/src/router/auth.rs:654-819`).
- `crates/worker/src/handler.rs:67-72` says an empty `worker_key` disables auth
  for a loopback dev bind, but the binary unconditionally validates a strong
  key before binding (`crates/worker/src/main.rs:48-68`,
  `crates/worker/src/main.rs:217-226`).
- RESOLVED BY DELETION. The `pat_issuer` field on control's `AppState` carried a
  doc comment saying PATs reused the gateway's `--signing-key-file` key
  material, which was false in the shipped topology: Compose gave Control and
  Gateway different key files. The field, its comment and control's
  `--signing-key-file` are all gone with the PAT class (section 4.1), so the
  claim no longer exists to be wrong. Gateway keeps its own signing key
  (`deploy/compose/docker-compose.yml:512`).
- `crates/control/src/lib.rs`, the `pairwise_salt` field, says the salt derives
  from the Gateway
  stash key and names wrapper/DPoP readers. The implementation uses a dedicated
  permanent pairwise secret (`crates/core/src/auth/mod.rs:235-264`) and the DPoP
  arm is gone.
- Control startup and Compose comments still claim a deployable console is
  seeded through `--bootstrap-console`
  (`crates/control/src/main.rs:806-823`,
  `deploy/compose/docker-compose.yml:787-793`). The Dockerfile and Caddy comments
  state that the console and flag were removed
  (`deploy/Dockerfile:71-76`, `deploy/ops/Caddyfile:58-65`). Search method:
  `rg 'bootstrap_console|bootstrap-console|console_zship|console-zship|console_host|console-host'
  crates/control/src deploy/compose deploy/Dockerfile` found the supposed
  wiring only in comments, not a field, flag, or call.
- Several references present `SANDBOX_TOKEN` as a live Control-to-sandbox bearer
  (`docs/architecture/overview.md:41-44`,
  `docs/architecture/distributed.md:72-81`,
  `docs/architecture/builder.md:15-27`,
  `docs/reference/env-vars.md:449-456`). Compose still assigns the literal to
  Control while its adjacent comment says nothing reads it
  (`deploy/compose/docker-compose.yml:335-358`). Search method:
  `rg 'SANDBOX_URL|SANDBOX_TOKEN|sandbox_url|sandbox_token' crates/control crates/core crates/cli --glob '*.rs'`
  returned no reader; the positive environment inventory and extracted-sandbox
  notes were inspected, so this is stale configuration/documentation, not a live
  service-auth flow in this map.
- Signup tells users they can resend verification, but Auth's complete route
  inventory has no resend endpoint
  (`crates/auth/src/ui/signup.rs:202-210`,
  `crates/auth/src/ui/signup.rs:268-270`,
  `crates/auth/src/server.rs:33-205`). The route inventory was inspected and a
  full Auth-tree search for `resend` and `verification` found provider/mail
  terminology but no resend handler.

### 30. HIGH: `migrations:approve` is outside the scope vocabulary, so no bearer can carry it

VERIFIED. `zeroship_authz::Action` has 17 variants; `zeroship_authz::Scope` has
16, and `Scope::action` maps them 1:1 (`crates/authz/src/scope.rs:6-23`,
`crates/authz/src/scope.rs:47-66`). The one variant with no scope is
`AppsApproveMigration`, whose Cedar id is `migrations:approve`
(`crates/authz/src/action.rs:18`, `crates/authz/src/action.rs:41`).
`Scope::parse` is a closed vocabulary that returns `ParseScopeError::Unknown`
for anything else, so that string cannot enter a scope set
(`crates/authz/src/scope.rs:118-138`).

`authz::enforce` is a two-evaluation intersection: when the caller carries a
wrapper policy it evaluates owner authority against the static policy set
first, then re-evaluates against the WRAPPER ALONE
(`crates/authz/src/eval.rs:38-76`, `crates/authz/src/eval.rs:56-62`). The
static platform `admin` universal-allow takes no part in that second decision,
which is what makes this a gap rather than an operator inconvenience: the role
that `Action::AppsApproveMigration`'s own doc comment names as the holder is
evaluated only in the first pass.

With personal access tokens deleted (section 4.1) there is exactly one producer
of a wrapper policy left. Control's `AuthzGuard` is built solely from a
`VerifiedPrincipal` (`crates/control/src/authz_guard.rs`,
`impl From<VerifiedPrincipal> for AuthzGuard`), the only producer of one is
`BearerVerifier::verify_bearer`, and it now delegates every bearer to
`oauth_guard_from_bearer` (`crates/authn/src/lib.rs`). That function's platform
arm sets the wrapper to `zeroship_authz::scopes_to_policy(&scopes)` and its
GoTrue arm derives it from `principal_grants` parsed through the same
`Scope::parse` (`crates/authz/src/scope.rs:162-172`). Both wrappers are
therefore drawn entirely from the 16-scope vocabulary, and the missing action
cannot appear in either. Migrated reaches `enforce` through the same
`BearerVerifier` and copies the same wrapper into its `AuthzContext`
(`crates/migrated/src/auth.rs`, `ControlPlaneAuthenticator::verify_action` and
`authorize`).

INFERRED impact: the second evaluation denies `migrations:approve` for every
authenticated caller, so Migrated's migration-approval route is unreachable by
anyone. It is registered (`crates/migrated/src/api.rs:25-26`) and its handler
requires that action before doing anything else
(`crates/migrated/src/api.rs:124-140`). That route is exactly the operator-only
approval gate a creator holding `apps:deploy` is supposed to be unable to
self-satisfy; it is now equally unusable by the operator.

This is a CONSEQUENCE of the PAT deletion, not a reason to reverse it. Before
it, a PAT's stored wrapper policy was authored as arbitrary Cedar and could
name any action, so the operator path existed only because a second issuance
authority existed.

The same defect had a second instance that is already fixed, which is worth
recording because it shows the shape of the available answers. Control's three
`/admin/oauth-clients` routes required `Action::PlatformPoliciesWrite`, which
likewise had no scope. They were moved onto the gate every other `/admin/*`
route uses - `team:write` on the synthetic platform org, which IS in the
vocabulary - and `PlatformPoliciesWrite` was deleted
(`crates/control/src/admin_handlers.rs`, `require_platform_admin`;
`crates/control/src/oauth_handlers.rs:68`,
`crates/control/src/oauth_handlers.rs:131`,
`crates/control/src/oauth_handlers.rs:162`).

Not fixed here for `migrations:approve`, and deliberately not fixed by
inventing a scope: whether it should get a scope token (widening the
creator-facing consent vocabulary with an operator power), be re-gated on an
existing operator-only resource the way the `/admin/*` routes were, or move
behind a separate operator credential class is an open design decision, and
picking one in a documentation pass would be picking it by accident. Note that
the re-gating answer is not a drop-in here: `migrations:approve` is deliberately
exempt from Migrated's app-owner check (`crates/migrated/src/auth.rs:169`,
`requires_app_owner`), so whatever replaces it has to keep the action
operator-only rather than owner-satisfiable. Search method:
`rg 'AppsApproveMigration|migrations:approve' crates/` returned the action
definition and its Cedar-id mapping, the single route family above, the
owner-check exemption, and tests - no scope token, and no second producer of a
wrapper policy. The control test fixtures state the same absence in prose
(`crates/control/tests/common/authz_fixture.rs`).
