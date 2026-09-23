# Auth Flows and Trust Boundaries

Auth here is three planes that meet at two hard boundaries.

The **creator platform** authenticates humans who build apps. `zeroship-auth` owns every human
credential check and is the OpenID Provider. `zeroship-control` is a bearer-only resource server:
it accepts an access token the platform OP issued and decides against a Cedar policy engine. It
runs no interactive login of its own.

The **app runtime** authenticates humans who use a creator's app. The gateway validates an
end-user credential on the app's own origin and converts it into a request-bound, signed identity
assertion. The worker verifies that assertion before creator code can observe it through
`env.auth`, and rules on the deploy's declared route policy again before entering the isolate.

The **service plane** is how platform processes address each other: ed25519 self-signed JWT
assertions resolved against a per-issuer trust bundle, plus one shared secret still carrying a
named subset of Control's internal routes.

The two boundaries that matter are the ones with a process running untrusted code on the far
side. The worker executes creator JavaScript, so nothing it holds may be able to mint an identity.
V8 executes creator JavaScript, so nothing reachable from a binding may be able to forge a
principal. Everything below is organised around keeping those two true.

```
                     C R E A T O R   P L A T F O R M
   browser ---------> zeroship-auth --------------> zeroship-control
   (creator)   login  (IdP + OpenID Provider)  OP    (resource server,
                          |                 bearer    Cedar decisions)
                          | OP-signed tokens
                          v
   ================== TRUST BOUNDARY 1 =========================
   only the gateway holds a key that can mint an identity envelope
                          |
   browser ---------> zeroship-gateway ------------> zeroship-worker
   (app user)  cookie   (BFF + OIDC RP     assertion   (V8 host,
               or        + router)        + envelope    second fence)
               bearer                                        |
   ================== TRUST BOUNDARY 2 =========================
   V8 sees a parsed identity object: no key, no verifier, no transport
                                                             v
                                                       creator code
                                                        (env.auth)
```

The gateway holds a private ed25519 key. The worker holds only the public half, published under
the gateway's issuer in a peer document. The worker can check the gateway's signature and cannot
produce one; that asymmetry is what makes the envelope survive a bypass of the transport
credential rather than merely restating it. `crates/zeroship-core/src/user_envelope.rs` carries
the argument in full.

---

## Names an identity travels under

One human has several identifiers, and which one is on the wire is load-bearing.

| Name | Shape | Where it is legitimate |
| --- | --- | --- |
| Platform user id | `usr_...` (`zeroship_core::UserId`) | Inside the platform. Never on an app-origin credential. |
| Pairwise subject | `pws_...` | Per (human, app). The only subject an app ever sees. |
| Relay alias | `token@relay-domain` | The address an app sees in place of the real inbox. |
| App OAuth client | `oac_...` | Identifies one creator app to the OP. |

The projection is `zeroship_core::auth::derive_pairwise` in `crates/zeroship-core/src/auth/mod.rs`:

```
pws = "pws_" + base36(HMAC-SHA256(salt, user_id || ":" || sector))[:PAIRWISE_SUB_BODY_LEN]
```

Deterministic, so the gateway derives it per request with no database read and a re-login yields a
stable subject. Two apps are two sectors, so the same human is two unrelated subjects and app
JavaScript cannot correlate across apps. `derive_pairwise_salt` takes a dedicated secret rather
than any rotatable operational key, because `pws_` is the permanent foreign key an app stores for
"who is this user": rotating it rotates every app's stored references. The same salt must be
configured on the gateway and on Control, which is why the derivation lives in one crate.

The OP applies the same projection through `Issuer::pairwise_subject` in
`crates/zeroship-auth/src/oidc/issuer.rs`, which is why a raw OP access token and a gateway-minted
session cookie carry the same subject for the same human and app. The exception is a *brokered*
client: `exchange_authorization_code` in `crates/zeroship-auth/src/oidc/authorization_code.rs`
picks `PrincipalIdTokenMint` (global `UserId`) for a brokered client and `IdTokenMint` (pairwise)
otherwise. `load_client` treats that flag as fail-closed: the `brokered` flag,
`token_endpoint_auth_method` and `client_secret_hash` all propagate a decode error rather than
defaulting, and a brokered client that is not `client_secret_basic` is refused as an invariant
violation.

`zeroship_core::auth::is_pairwise_subject` is a *shape* inverse, not a trust gate. It proves a
string looks like a `derive_pairwise` output; every call site runs it only after a cryptographic
check has already established authenticity. It contains a mint bug that failed to project, and
never decides trust.

The relay alias is minted at consent by `mint_alias_at_consent` in
`crates/zeroship-auth/src/store/relay.rs` and reused on re-grant rather than rotated. Inbound mail
resolves through `resolve_active_alias`, which ANDs a local revocation flag with a structural
existence check on the grant ledger, so a revoked grant closes the alias without the two writers
sharing a lock.

---

## 1. The creator platform

### 1.1 Auth authenticates the human

`configure` in `crates/zeroship-auth/src/server.rs` builds the whole surface, folding in
`health::configure`, `oidc::authorization_code::configure` and `oidc::userinfo::configure`; the
token-endpoint configurer reaches `oidc::refresh::configure`, which reaches
`oidc::introspect::configure`. Three groups share one process:

- **Browser UI**: `/login` and `/login/2fa`, `/signup`, `/consent` with its accept and deny posts,
  `/device`, `/logout`, `/link`, `/me` and the self-service routes under it
  (`unlink/{provider}`, `sessions`, `sessions/{id}/revoke`, `delete`, `delete/cancel`,
  `2fa/enroll`, `2fa/confirm`, `2fa/disable`), `/magic/*`, `/verify`, `/forgot`, `/reset`, and the
  upstream `/oauth/google/*` and `/oauth/github/*` legs. Handlers are one module per flow under
  `crates/zeroship-auth/src/ui/`. The upstream legs are the only conditionally registered routes:
  `configure` takes the two provider flags and registers no dead routes for a provider with no
  credentials.
- **OpenID Provider**, under the `/oauth2` scope. See 1.2.
- **Webhooks and hooks**: `/webhooks/postmark`, `/webhooks/ses-sns`, `/webhooks/relay-inbound`
  (`crates/zeroship-auth/src/ui/webhooks.rs`) and `/hooks/gotrue/send-email`
  (`crates/zeroship-auth/src/ui/gotrue_email_hook.rs`). The relay-inbound route is the one carrying
  its own payload cap, because it buffers a raw body before it can authenticate anything.

**The browser IdP session is opaque plus a database read, not a signed token.** `COOKIE_NAME` in
`crates/zeroship-auth/src/sessions/login.rs` is `__Host-zsidp_session` and its value is the raw
`zeroship.idp_sessions` row id. `create` in `crates/zeroship-auth/src/store/sessions.rs` inserts by
selecting from `zeroship.users`, refusing a disabled, anonymized, deletion-requested or
deletion-scheduled principal at insert time. `validate` is a single `UPDATE ... RETURNING` that
both checks liveness and slides the idle window forward, so a page view is activity; it enforces
the hard lifecycle predicates and `credential_version` equality but deliberately not a soft
`locked_until`. The cookie value never rotates: the idle window slides and the absolute window does
not. Revocation is `revoke` and the IDOR-guarded `revoke_one_for_user`, which filters on both the
row id and the owning user.

A separate concept with a confusingly similar name lives in
`crates/zeroship-auth/src/session_store.rs`: the **refresh-token session**. Its invariant is that
no credential is issued except from a validating read of a `zeroship.sessions` row joined to
`zeroship.grants`, enforced by a witness type `ValidatedSession` with private fields and no public
constructor, producible only by `create`, `rotate` and `replay`. Only secret hashes are stored; the
family is one row whose secret rotates in place, with the superseded hash retained in a previous
slot so reuse detection covers exactly the immediately preceding generation. `oidc/refresh.rs`
opens by stating that nothing in that file mints, and every mint routes through a
`ValidatedSession`.

Auth's other origin cookies are all `__Host-`-prefixed and each has one job: the double-submit
CSRF cookie (`crates/zeroship-auth/src/csrf.rs`), the signed second-factor challenge
(`crates/zeroship-auth/src/sessions/totp_challenge.rs`, carrying the user, credential version,
return target and which first factor was satisfied), the magic-link CSRF cookie
(`crates/zeroship-auth/src/ui/magic.rs`), and the upstream OAuth stashes
(`crates/zeroship-auth/src/ui/oauth_stash.rs`, carrying state, PKCE verifier, nonce and return
target under an HMAC).

Credential checking itself is centralised. `verify_password_credentials` in
`crates/zeroship-auth/src/identity/credentials.rs` is the one place a password is checked: rate
limits over three buckets, user lookup, a dummy-hash timing defence so an unknown email costs the
same as a known one, Argon2 verification off the reactor, an opaque refusal for a soft lock,
eligibility, and its own audit emission. Second-factor completion routes back through
`ui::login::post_2fa`, which re-checks the credential version against the live row before
completing the flow the challenge names.

Two flows are worth reading for their fail-closed shape. `complete` in
`crates/zeroship-auth/src/identity/password_reset.rs` is one statement that selects the target by
the immutable user id captured at issue time, bumps the credential version, clears the lockout
counters, consumes the token, revokes every app session anchor, and writes family revocation
markers for both the app identity mapping and live refresh sessions. And `request` in
`crates/zeroship-auth/src/ui/account_deletion.rs` calls Control's erasure preflight *before*
writing anything, rendering a blocked page on a refusal and a distinct unavailable page when the
question could not be answered at all.

Cross-cutting controls: `crates/zeroship-auth/src/csrf.rs` (pure double submit, compared with a
non-short-circuiting accumulator; the CSP script nonce is deliberately generated independently of
the CSRF token), `crates/zeroship-auth/src/headers.rs`, `crates/zeroship-auth/src/audit.rs` (with
`emit_strict` for security-critical transitions, where a failed insert propagates instead of being
swallowed), and the leaky-bucket limiter in `crates/zeroship-authn/src/rate_limit.rs` applied at
login, signup, magic start and complete, link, forgot, reset, TOTP verify and device user-code
entry. Scheduled cleanup is `crates/zeroship-auth/src/cron/`: `account_reaper.rs` (which re-runs
the erasure preflight before the delete, and whose advisory lock is hygiene rather than the
ownership fence), `audit_retention.rs` (which opens its own connection per tick because the audit
table's tamper trigger admits deletes only under a transaction-local GUC), `token_sweep.rs`, and
`signing_key_retention.rs`.

The framing posture couples to the edge. `crates/zeroship-auth/src/headers.rs` serves
`frame-ancestors 'none'` plus `X-Frame-Options: DENY` by default, and relaxes to
`frame-ancestors 'self' <origins>` while dropping `X-Frame-Options` entirely on the routes a
console may embed, since a user agent honouring `frame-ancestors` ignores the older header. The
allowlist is `[auth].frame_ancestor_origins` in `deploy/ops/zeroship.toml`. That is why `console`
is a reserved hostname label: holding it means holding the one origin permitted to frame the real
login page.

### 1.2 Auth is the OpenID Provider

The OP is `crates/zeroship-auth/src/oidc/`. Its issuer is `{public_url}` plus `OP_PATH_PREFIX` from
`crates/zeroship-core/src/device_grant.rs`, and the token and device-authorization paths are
mounted from `TOKEN_PATH` and `DEVICE_AUTHORIZATION_PATH` in that same module, which is also what
`discovery_metadata` renders, so the document and the mount cannot drift.

| Endpoint | Entry symbol | File |
| --- | --- | --- |
| `/oauth2/authorize` | `authorize_get`, `authorize_post` | `oidc/authorization_code.rs` |
| `/oauth2/device/authorization` | `device_authorization` | `oidc/device_token.rs` |
| `/oauth2/token` | `token_post` | `oidc/authorization_code.rs` |
| `/oauth2/revoke` | `revoke_post` | `oidc/refresh.rs` |
| `/oauth2/introspect` | `introspect_post` | `oidc/introspect.rs` |
| `/oauth2/userinfo` | `userinfo` | `oidc/userinfo.rs` |
| `/oauth2/logout` | `ui::logout::get`, `ui::logout::post` | `ui/logout.rs` |
| discovery and JWKS | `jwks`, `openid_configuration`, `oauth_authorization_server` | `oidc/metadata.rs` |

`token_post` dispatches on the grant type to `exchange_authorization_code`,
`refresh::exchange_refresh_token`, or `device_token::exchange_device_code`. PKCE is `S256`-only.
Redeeming a consumed authorization code does not merely fail: it triggers
`revoke_replayed_authorization_code_lineage`, which revokes the subject's sessions for that client.

**Client authentication is one function for every client-authenticated endpoint.**
`authenticate_client` in `crates/zeroship-auth/src/oidc/refresh.rs` is called from the
authorization-code, refresh, device-code, introspect and revoke paths, because RFC 6749 forbids the
authorization-code grant from having a laxer rule than refresh for the same client. Brokered
clients take a branch before the method match: `authenticate_brokered_client` in
`authorization_code.rs` derives and constant-time compares against the current and rotation-window
previous platform master, and stores no secret hash at all. Otherwise the registered
`token_endpoint_auth_method` selects between no secret, HTTP Basic, and a form post, and a presented
credential of the wrong kind is refused rather than tried the other way.

Client rows live in `zeroship.oauth_clients`, and Auth only reads them. They are written by
Control: `reconcile_oauth_clients` in `crates/zeroship-control/src/oauth_clients.rs` reconciles the
first-party set at boot, and `ensure_app_client` in
`crates/zeroship-control/src/app_oauth_client.rs` writes the per-app `oac_` client together with
its `sector_identifier` in one transaction. The
per-app clients are the gateway-brokered ones: app code and browsers hold no secret, and the
gateway derives the per-app secret from the shared master. The CLI's own client is the exception:
`reconcile_platform_cli_client` in `crates/zeroship-auth/src/oidc/device_token.rs` writes it at
Auth's boot, which is why Control's pruning excludes it.

`crates/zeroship-auth/src/oidc/claims.rs` owns exactly one identity-claim projection, shared by the
ID token and UserInfo: `scope_gated_identity_claims` unlocks `email` and `email_verified` on the
`email` scope, `name` and `picture` on `profile`, and projects nothing else. UserInfo additionally
requires the `openid` scope, so a plain resource token is not an identity oracle.

Back-channel logout is **outbound only** here. `crates/zeroship-auth/src/oidc/backchannel_logout.rs`
records relying-party participation during code exchange and fans out on logout, session revoke
and account deletion. Auth exposes no receiving endpoint; the receiver is the gateway (section 4.3).

Signing keys are files, never database rows. `crates/zeroship-auth/src/oidc/signing.rs` loads PEM or
DER and rejects group- or world-readable permission bits; the database holds public JWK metadata
only, and `publish_active_key` runs under an advisory lock as one conditional update so a
terminally retired row can never be reactivated. JWKS serving passes each row through a public-field
allowlist and errors rather than serving a key missing a required field.

### 1.3 Control is a bearer-only resource server

Control accepts a bearer and decides. There is no `/authorize`, `/callback`, `/login`, `/session`,
`/token`, `/logout` or `/consent` route in `crates/zeroship-control/src/`. What resembles OAuth
there is the RFC 9728 protected-*resource* metadata document (`protected_resource_metadata` in
`crates/zeroship-control/src/device_handlers.rs`), which is the resource-server side of the
protocol, and the bearer-gated grant listing and revocation in
`crates/zeroship-control/src/oauth_grants_handlers.rs`. The console is an ordinary
gateway-fronted app that reaches Control with an OAuth access token.

**Principal resolution** is `BearerVerifier::verify_bearer` in `crates/zeroship-authn/src/lib.rs`,
producing a `VerifiedPrincipal`. It routes on the token's unverified issuer to a configured
provider, then, for a platform OP token, requires the expected audience, parses the subject as a
canonical `UserId` (a bare UUID or an app id is refused), checks the token family has not been
revoked, parses the scope string, and lowers the scopes to a policy. Every arm converges on
`require_active_principal`, which refuses a disabled, anonymized, deletion-requested or
deletion-scheduled user. There is no session-cookie path and no second issuance authority in this
crate.

**The guard is an extractor, not middleware.** `AuthzGuard` in
`crates/zeroship-control/src/authz_guard.rs` implements `FromRequest`; a handler opts in by taking
it as an argument. `guard_from_bearer` extracts the header, verifies it, and on a first-seen CLI
principal calls `materialize_default_grants` (`crates/zeroship-authn/src/platform_cli.rs`) and then
*re-verifies* so the request authorizes against the rows just written.

**The action is named at the call site, not derived from the route.** There is no route-to-action
table. Each handler calls `authz.require(Action::..., Resource::..., &state)` before opening a
transaction, deliberately on the shared client, because a decision row inside a rolled-back
transaction would erase the record of the refusal.
`crates/zeroship-control/src/organizations.rs` states that discipline in its module documentation.

**The engine is Cedar.** `enforce` in `crates/zeroship-authz/src/eval.rs` is the decision entry. It
takes an `AuthzContext` (principal, optional token policy, action, resource, time, request address,
request id) and:

1. resolves the caller's authority from the database, never from a cache;
2. assembles exactly two entities, the user and the request's resource;
3. when a token policy is present, runs a **principal-only pass** against the static platform
   bands first, so a token cannot outlive the seat that justified it;
4. runs a second pass against the token's own lowered policy;
5. refuses on a Cedar *evaluation* error rather than reporting it, because Cedar skips a policy
   that errors and returns an ordinary deny that is indistinguishable in the audit table from an
   honest non-match;
6. writes the decision to `zeroship.authz_decisions`.

Two design choices exist to keep that audit row honest: the refusal on evaluation error above, and
a deliberately wide `appliesTo` in `deploy/policies/zeroship.cedarschema`, so a legitimate
cross-type question from the consent probe is a deny with a row rather than a request-construction
error with none.

`crates/zeroship-authz/build.rs` generates nothing. It parses the schema and every policy file and
runs the same strict validation the runtime loader runs, failing on warnings as well as errors; the
policies reach the binary by `include_str!` in `crates/zeroship-authz/src/engine.rs`.

**Bands are allow-lists.** No shipped policy file contains a `forbid`. Separation is expressed as
the absence of a permit, on the stated reasoning that an erroring `forbid` does not forbid while a
band that was never written cannot be reached.

**Authority is two ranks, re-derived per request.** `resolve` in
`crates/zeroship-authz/src/authority.rs` reads `zeroship.users`, the organization and project
membership tables and the `zeroship.organization_roles` ladder, and returns email-verified,
account-locked, an effective rank and a billing rank. The two are different axes: the billing role
outranks admin on money while admin outranks it on everything else.
`effective_project_rank` is the pure narrowing rule: an organization rank at or above the
project-wide role applies on every project; below it, the effective rank is the minimum of the
organization and project ranks, and a missing project seat is zero. A missing admin row narrows
rather than widens. Billing rank never narrows, because there is no per-project invoice. A failed
query is an error, never a degradation to rank zero.

**The scope vocabulary is the action vocabulary minus operator approval.**
`crates/zeroship-authz/src/scope.rs` pins that as a set difference rather than restating a list.
The one action outside the consent vocabulary is `migrations:approve`
(`Action::AppsApproveMigration`): it is absent from the delegatable scopes, appears in no policy
band, and is declared in the schema only so a request for it can be constructed. Note the shape
mismatch a reader must not smooth over: the variant is named for apps, the wire id is
`migrations:approve`, and the separate action for actually running a migration is
`database:migrate`, which *is* delegatable and *is* banded.

**A second authorization front door exists.** `crates/zeroship-migrate-server/src/auth.rs` builds
its own context and calls the same `enforce`, and adds a fence Cedar does not express: an
organization-owner rank check with no per-project narrowing, on the reasoning that a migration
rewrites the app's schema, which is the least reversible thing the platform lets a creator do.

### 1.4 Consent bounds a token, in three stacked narrowings

A grant is a row in `zeroship.oauth_grants`, written only by the OP
(`persist_consent_grant` in `crates/zeroship-auth/src/oidc/authorization_code.rs`, from the
consent-accept handler). Before a scope can be granted, `crates/zeroship-auth/src/ui/consent.rs`
partitions the request: identity scopes and the app's own declared scopes are self-grantable;
reserved platform and organization prefixes are delegated and must each pass
`is_authorized_anywhere` (`crates/zeroship-authz/src/eval.rs`) for the consenting human; an unknown
scope rejects the whole consent.

The three narrowings are ANDed:

1. **Registration ceiling** -- the client's registered scopes, validated at Control boot.
2. **Consent ceiling** -- `consent_covers` refuses a redemption whose scopes the stored grant no
   longer covers, so a revoked or narrowed grant closes issuance.
3. **Request-time intersection** -- `BearerVerifier` parses the token's scope claim (one unknown
   token fails the whole string, so a pre-sweep token is refused outright) and, for the CLI client,
   intersects against the live grant rows before lowering to a policy.

One detail matters for reading the bands: `scopes_to_policy` emits `Resource::Any`, so **the token
narrows the action and nothing else**. The rank comparison in the static bands is the whole tenant
fence.

Revocation is `revoke_grant_cascade` in `crates/zeroship-control/src/oauth_grants_handlers.rs`: one
transaction on a dedicated connection that deletes the grant, revokes the app identity mapping, and
writes a `zeroship.token_revocations` family marker keyed on the pairwise subject derived for that
app's sector, so the live access token dies too rather than only the relay alias.

### 1.5 The CLI

`zeroship login` (`cmd_login` then `login_device_flow` in `crates/zeroship-cli/src/auth.rs`) runs an
RFC 8628 device-authorization grant. It discovers the issuer by asking **Control** for its
protected-resource metadata and taking the advertised authorization server, then drives the
**OP's** device-authorization and token endpoints. Control runs no device grant of its own:
`crates/zeroship-control/src/device_handlers.rs` serves only the metadata document, and the
authorization server it advertises is the same issuer `BearerVerifier` pins tokens to, so the
credential the CLI comes back with is one Control will accept. The flow refuses a response that
carries no refresh token rather than storing a
short-lived access token alone, and the "signed in as" line is decoded locally from the token
rather than fetched.

Credentials go to a private-mode file under the CLI's state directory (`credentials_path`), and
`load_credentials` refreshes through `refresh_platform_credentials` before expiry, writing the
rotated family before returning because a refresh-token reuse would revoke it. Every Control call
attaches the access token as a bearer through `resolve_bearer_token` in
`crates/zeroship-cli/src/main.rs`, with an explicit flag and an environment variable taking
precedence over the stored credential.

One other CLI credential is live and is not a user credential: `cmd_join_token` in
`crates/zeroship-cli/src/dev.rs` mints a worker enrolment token with an operator-held signer key
(section 3.4).

---

## 2. The app runtime

### 2.1 What a browser holds on an app origin

Three cookies, all `__Host-`-prefixed, all set by the gateway on the app's own origin. The prefix
forces `Path=/`, no `Domain`, and `Secure` unconditionally, in local development as well as in a
deployment.

| Cookie | Constant | What it is |
| --- | --- | --- |
| `__Host-zeroship_app_session` | `oidc_rp::APP_SESSION_COOKIE` | The interactive credential: a gateway-signed `zeroship-sess+jwt` identity assertion, `SameSite=Lax`, short-lived. |
| `__Host-zeroship_app_anchor` | `anchors::ANCHOR_COOKIE` | Reload recovery: names a `zeroship.app_session_anchors` row, `SameSite=Strict`, `ANCHOR_ABS_DAYS`. |
| `__Host-zs_oidc_stash` | `oidc_rp::STASH_COOKIE` | The signed PKCE, state and nonce stash for one interactive dance, `STASH_MAX_AGE_SECS`. |

The session cookie is **signed, never encrypted, and never a capability**. Its claims
(`session_token::SessionClaims`) are the identity projection the worker needs plus the app binding:
`sub` is the pairwise subject, `email` is the relay alias or empty, `app` is the per-app `oac_`
client id, plus `iss`, `iat`, `exp`, `auth_time`, `amr` and `scopes`. It is stamped
`session_token::SESSION_TOKEN_TYP` and `session_token::Verifier` hard-rejects any other `typ`, so
an RFC 9068 access token can never be replayed as a session cookie and the cookie can never be
presented on the bearer arm. No resource server accepts it as authorization.

It is signed by `GateState.signing_key`, loaded from `gateway.signing_key_file`, with
`gateway.prev_signing_key_file` folded into `session_token::Verifier::with_previous` during a
rotation overlap. The issuer stamped into it is the gateway's `public_url`, so repointing the
gateway's public hostname moves session issuance and not merely a route.

Two rows back the cookie server-side, neither read on the per-request path:
`zeroship.gateway_sessions` (`sessions::AppSession`, the audit and revocation record, and where the
*global* user id and the real email live) and `zeroship.app_session_anchors`
(`crates/zeroship-gateway/src/anchors.rs`), holding the rotating OP refresh family encrypted with
AES-256-GCM under `GateState.anchor_enc_key`. The refresh family never leaves the gateway in
plaintext, not to the browser and not at rest.

Both tables force row-level security and the gateway connects as a non-bypass role, so every
operation runs inside a transaction that first sets a tenant GUC:
`crates/zeroship-gateway/src/rls.rs`, `set_tenant_app` and `set_tenant_client`. The setting is
transaction-local, so a pooled connection cannot leak a tenant to the next checkout, and an unset
GUC fails closed because the policy predicate is null. This is why `anchors::read_live` doubles as
the cross-app bind check and why every revocation is per app.

`crates/zeroship-gateway/src/identities.rs` owns the durable reverse map,
`zeroship.app_user_identities`, keyed on the app client and the global user, carrying the pairwise
subject and the relay alias. `upsert` refuses a changed projection rather than overwriting it, and
`lookup_relay_email` is the email-claim swap source whose every failure mode yields an empty email,
never the real inbox as a fallback. The map exists because durable teardown needs to reverse a
pairwise subject back to a human: Auth's password-reset and account-deletion paths join it to learn
which families to revoke.

### 2.2 The two credential arms

`resolve_auth` then `resolve_auth_inner` in `crates/zeroship-gateway/src/router/auth.rs`. The
bearer arm is ordered first, and auth runs inside `execute_resource_tree` before the action match,
so a static asset and a worker forward are gated identically.

**Bearer, a raw OP access JWT.** `resolve_bearer_user_header` peeks the issuer for routing only; a
bearer whose issuer is not the OP draws a 401 on *every* route including anonymous ones, because it
asserts a different scheme rather than an expired session. A raw OP token is verified by
`oidc_rp::verify_access_jwt` and then bound by the caller: the `client_id` claim must equal the
route's OAuth client, the audience must carry the app, the route must have a sector identifier
(otherwise a fail-closed 503 while Control finishes provisioning), and the subject must pass
`is_pairwise_subject`.

**Cookie.** `resolve_app_session_user_header_inner` parses the cookie and verifies it *locally*
with `session_token::Verifier`: signature by `kid`, issuer, expiry, `typ`, and the app binding. No
database read. State-changing methods additionally pass an anti-CSRF gate on exact origin match
plus consistent fetch metadata; failing it **drops the credential** rather than answering 403, so
the request continues as if unauthenticated.

Both arms then run the two revocation checks in section 4.

The OP's verification keys are fetched, not configured: `zeroship_core::oidc_verify::JwksCache`
holds them with a TTL and one forced refresh on an unknown key id, and the same cache serves ID
token, access token and logout token verification. The session cookie's key is not in that cache;
it is local key material, and no gateway endpoint publishes it.

### 2.3 The gateway as OIDC relying party

Two flows reach the OP, and they differ in who is trusted for state and nonce.

**Interactive.** A protected route on an HTML navigation redirects through
`oidc_rp::OidcRp::build_authorize_redirect`, and a 401 from the worker on an HTML navigation does
the same. The gateway generates state, nonce and the PKCE verifier server-side into the signed
stash cookie (`oidc_rp::Stash::encode` and `Stash::decode`, constant-time MAC compare), and
`OidcRp::finish_callback` verifies all three: state against the stash, nonce through
`zeroship_core::oidc_verify::verify_id_token`, and PKCE by sending the verifier to the OP. It also
checks the stashed client against the route's client and the RFC 9207 issuer parameter.

**SDK popup.** `browser_auth::authorize` builds the authorize URL for a browser that holds its own
PKCE verifier, and `auth_token::session_post` exchanges the code. PKCE is mandatory and `S256` is
enforced, the redirect URI is exact-matched against a registered set, and the issuer parameter is
checked; **state and nonce on this leg are browser-supplied and are not verified by the gateway** --
they are the SDK's own cross-flow guard, and the token exchange passes no expected nonce.

Both builders request `offline_access`, and the code-exchange path hard-fails when the token
response carries no refresh token. Of the three tokens the gateway receives: the ID token is
verified and is the identity source and is never forwarded; the access token stays server-side and
is used for the ID token's `at_hash` binding and to recover the granted scopes when the token
response omits them; the refresh token is encrypted at rest in the anchor row and rotated in place.
Client authentication to the OP is the derived per-app broker secret, from
`gateway.broker_secret_file`.

### 2.4 Route policy, enforced twice

A deploy declares per-route policy in its manifest. The compiled form is
`zeroship_bundle::compiled::EffectivePolicy`, carrying `auth: RequiredPrincipal` and
`required_scopes`, unioned along the resource inheritance chain so a child can only add.

`zeroship_bundle::RequiredPrincipal` in `crates/zeroship-bundle/src/rule.rs` has exactly two
variants, `Anonymous` and `User`. There is no platform-admin principal, and the enum's own
documentation records why a third string is not how to add one: a level no arm can distinguish is a
level that is not enforced, and a rank function that made it *win a merge* is what made it read as
implemented to an auditor. `packages/vite-plugin/src/manifest.ts` refuses the spelling at build
time.

**First fence, the gateway.** `resolve_auth` admits an anonymous route whether or not identity
resolved, and admits a `User` route only when an arm produced a header. Scope gating follows RFC
6750 section 3.1 and fires **only on `User` routes**: an anonymous route is part of the app's
public surface and must never 403 a signed-in visitor over a scope inherited from a broad parent,
because that makes being logged in strictly worse than being logged out. The comparison is
`zeroship_bundle::compiled::scopes_satisfied` over the scopes read back out of the freshly built
envelope, so whichever arm authenticated there is one enforcement point and one scope source. A
scope failure answers 403 with an insufficient-scope challenge, distinct from the 401 or redirect
an unauthenticated request gets.

**Second fence, the worker.** `enforce` in `crates/zeroship-worker/src/policy.rs` calls
`zeroship_bundle::compiled::admit` before creator code is entered and answers
`Refusal::{NoPrincipal, MissingScopes, UnreadableUrl}`, with a body whose error code begins
`platform_` and which carries a `refused_by` field, so an operator can tell a platform refusal from
an app throw without knowing the app. The module states its own justification: the envelope's
guarantee is about a caller with direct network access to the worker, and that sentence means
nothing unless the worker itself rules on identity; and `env.auth.requireUser()` is a convenience
for reading identity, so a creator who forgets to call it would otherwise have no fence at all.
Resolution is shared with the gateway through `zeroship_bundle::compiled`, so the two tiers cannot
disagree about which resource a path names or what its effective policy is. A path matching no
declared resource is admitted there deliberately: the worker is not a router and must not invent a
second routing verdict.

### 2.5 The identity envelope

`zeroship_gateway::oidc_rp::encode_user_header` builds it, and `WorkerUser` in the same file is the
JSON payload: `id` (the pairwise subject), `email` (relay alias or empty; the bearer arm always
sends empty rather than trusting a bearer's email claim), `name`, `avatar` (serialised even when
absent, so `"avatar" in user` answers the same in dev and deployed), `email_verified`, and
`scopes`. Scopes are a permanent kernel-contract field: app code reads them through
`env.auth.getUser().scopes`, and they are always present, empty when the credential carries none,
so the shape is stable across every arm.

The wire format is in `crates/zeroship-core/src/user_envelope.rs`:

```
<base64(user_json)>.<request_id>.<issued_at_unix_secs>.<kid>.<base64url(signature)>
```

The signature covers the first four segments, `kid` included. Signing the `kid` matters: without it
a valid envelope could be relabelled under another trusted key id, and a verifier that resolves its
key by an unauthenticated label is choosing its own oracle. `request_id` binds the envelope to one
dispatch and the issuance timestamp bounds its life to `MAX_AGE_SECS`, with `FUTURE_SKEW_SECS` of
tolerance, so a leaked header is not a standing credential.

There is no unkeyed mode. `UserEnvelopeVerifier::for_issuer` returns
`EnvelopeKeyError::NoKeyForIssuer` when the peer document carries no key for the gateway, so a
misprovisioned deployment fails at startup rather than serving with the check silently off, and
`ServiceAuth::user_envelope_signer` returns `None` for a process that loaded no key, so such a
process emits no envelope rather than an unsigned one. Both gateway call sites fail the request
closed on `None`.

The gateway reads its own envelope back through `UserEnvelopeSigner::own_verifier`
(`router::auth::decode_user_header`), so the principal it acts on for scope gating and for
idempotency partitioning is exactly the principal the worker will see. On the worker,
`verified_user_json` in `crates/zeroship-worker/src/handler.rs` requires the header, a parseable
request id, and a configured verifier, and calls `UserEnvelopeVerifier::verify_for_request`. An
envelope arriving at a worker with no verifier configured is a refusal, not a pass-through and not
a downgrade to anonymous.

The gateway strips reserved headers from the inbound request before forwarding
(`crates/zeroship-gateway/src/router/dispatch.rs`), so a client cannot supply its own
`ZeroShip-User`, `Authorization`, app or plan id, request id, or any `x-zs-` header.

### 2.6 `env.auth` inside V8

`AuthPlugin` in `crates/zeroship-runtime/src/auth.rs` registers `getUser` and `requireUser` under
the `auth` namespace. It is stateless and shared: both callbacks read the current request's user out
of `RuntimeState`, so one instance is correct for every app on every worker thread. It is registered
at three sites: `create_plugins` in `crates/zeroship-worker/src/cache.rs` (the path every production
end-user app runs on), the workflow host's plugin vector in
`crates/zeroship-worker/src/workflow_host.rs`, and the CLI's `zeroship serve` vector in
`crates/zeroship-cli/src/main.rs`.

The verified JSON is stored per request, keyed by request id, by `set_request_user`, and released on
every terminal path by `clear_request_user`. It is deliberately not a thread-local: on an async
runtime a thread-local is read by whichever continuation resumes, so a handler calling `getUser()`
after an `await` would see whichever request last touched the thread. `current_user` therefore
prefers the invocation context V8 preserves across continuations, then the executing request id,
then a WebSocket connection's user bound at upgrade; an anonymous frame is authoritative and does
not fall through to another request's identity.

`requireUser` throws an error carrying a 401 status and a code taken from
`ZsErrorCode::Unauthenticated` rather than a spelled-out literal, because that code is a wire token:
the RPC client lifts it verbatim and an app's re-authentication hook branches on exactly that value.

V8 never sees a key, a verifier, or the transport credential. It sees a parsed object. An identity
does not automatically become a database actor either: a creator-supplied actor passes through
`sanitize_app_actor` in `crates/zeroship-data-orm/src/protection/unmask.rs` before it reaches SQL.

### 2.7 The dev tier

`pnpm dev` runs `zeroship serve` with no gateway and no hosted OP, so the Vite plugin's dev-auth
layer mints a locally signed `__zeroship_dev_session` cookie
(`crates/zeroship-runtime/src/core/dev_auth.rs`, `DEV_SESSION_COOKIE`). `resolve_dev_user_json` is
called by `crates/zeroship-runtime/src/core/serve.rs` before dispatch and returns *the same*
`user_json` shape the gateway produces, which then flows through the identical
`Runtime::call_fetch_handler_with_user` path, so `env.auth.getUser()` and the RPC `currentUser()`
have the same contract in both tiers and only the producer of the cookie differs.

It is dev-only by construction. `DevAuthSettings` carries two conditions resolved once at process
start -- dev mode, and a secret the Vite plugin generates per dev server -- and with either absent
no cookie resolves however valid it looks. The cookie name and signing differ from production, so a
stray production cookie is ignored by the dev decoder and the reverse. Because the conditions are
inputs rather than environment reads inside the resolver, a test proving one is load-bearing flips a
field instead of racing the process environment.

---

## 3. The service plane

### 3.1 The peer document

`crates/zeroship-core/src/service_peers.rs` is the only place a service's own private key or a
peer's public key is loaded, so both ends of every internal edge read the same shapes and derive the
same key id. The document is JWKS-shaped with one addition: each key carries an issuer.

That addition is the security property. RFC 8725 section 3.8 requires the verification key to be
resolved *from* the issuer, and a bare JWKS array carries no issuer at all: a flat pool in which any
service's key validates any service's assertion. The key id is derived rather than trusted -- it is
the RFC 7638 thumbprint (`thumbprint_key_id`) -- and a document that also states one must agree.

Peer keys are configured, not fetched, for reasons specific to this deployment. The transport a
fetched document would ride is the one assertions exist to stop trusting: the gateway, worker and
auth crates declare no TLS, so internal hops are cleartext and a polled JWKS would be substitutable
by exactly the adversary an asymmetric credential is for. And the only feed reaching both gateway
and worker is Control's, so distributing the gateway's identity key over Control's feed would make
Control able to substitute the gateway's identity. The cost is unattended rotation; rotation stays
expressible without downtime because a bundle may carry several keys for one issuer at once.

No key may be shared between issuers, and independent refusals hold that at startup, none subsuming
another: `load_peer_bundle` refuses a *document* publishing one public key under two issuers;
`ServiceKeyring::from_parts` refuses a *pair* whose private key is published under any issuer but
this process's own; `InstanceSigningKey::into_keyring` refuses a key drawn at boot that the document
publishes at all. A missing or unparseable document refuses startup in `ServiceKeyring::load`,
because a process that boots and then refuses every guarded edge is indistinguishable from a healthy
one until traffic arrives.

One document is handed to every service, and that grants nothing extra: a verified identity still
has to match the audience the callee is addressed by and the endpoint allowlist, so holding a peer's
public key is the ability to check that peer's signature and nothing else.

### 3.2 Assertions: two profiles

`crates/zeroship-core/src/service_assertion.rs` holds the minter, the verifier and the replay store.
A service mints a short-lived JWT naming the callee it is about to call, stamped
`SERVICE_ASSERTION_TYP`, so a user access token and a service assertion are unmistakable for one
another in both directions.

The verifier's checks, in order: the expected audience is a well-formed issuer identifier; the `typ`
matches exactly; the signing key is resolved *from* the issuer then narrowed by key id, never
against a flat pool; the signature verifies under a pinned algorithm; audience, issuer and subject
agree; the expiry is present, in the future, and within the lifetime ceiling the *callee* sets
(`MAX_ASSERTION_LIFETIME`, with `CLOCK_SKEW_TOLERANCE`); and the `jti` is claimed atomically in a
store shared by every replica of the callee. Every failure returns the same
`AuthError::CredentialRejected`, so the error is not an oracle telling a prober which check it
tripped. The one verdict spelled differently is `AuthError::StoreUnavailable`, the same refusal, so
a database outage that refuses every caller at once is distinguishable from an attack.

Those checks up to the replay claim are the transport-only profile
(`TransportAssertionVerifier`). The claim is what the full profile (`ServiceAssertionVerifier`)
adds, and it is a write against a shared store on the request's critical path. The profile is
therefore chosen per edge by call rate and never inherited by default: the gateway-to-worker
dispatch hop runs per end-user request and takes the transport-only profile, and the identity
envelope's binding to the dispatch request id and its issuance window bounds replay there instead.
They are two types rather than one type with a flag, because an optional check is an untested check.
The mechanism tags differ too, so a full-profile edge cannot be satisfied by a transport
verification.

The replay key is the issuer and the `jti` joined, so one service cannot burn another's. The
Postgres store is `claim_replay_key` in `crates/zeroship-authn/src/service_replay.rs`: one upsert
whose affected-row count is the verdict, with a conflict clause that lets an expired row be
reclaimed without a sweeper. Retention runs past the last instant any clock still accepts the
assertion, which is why `MAX_REPLAY_STORE_CLOCK_SKEW` exists alongside the verifier's own leeway.
The in-memory store in `service_assertion.rs` is correct for a single-replica deployment and for
tests, and is explicitly not sufficient for a replicated callee, where "single use" would degrade to
"single use per replica".

### 3.3 The endpoint allowlist

`crates/zeroship-core/src/service_identity.rs` declares every internal route as a `ServiceEndpoint`
(destination service, method, path template) under `endpoints`, and `service_allowlist` pairs each
service principal with the endpoints it may reach. `authorize` finds the row matching a verified
identity and asks whether it contains the endpoint. A true result does not replace any
delegated-user or resource-scope check that endpoint owns.

The rows, by principal:

- `svc/control` reaches the workflow management, schedule and journal-ensure endpoints and the
  worker log read.
- `svc/auth` reaches the gateway's back-channel logout endpoint and Control's erasure preflight. It
  holds no shared key: that single call is on an assertion precisely so the process rendering the
  login form does not also hold the route table, the version feed and both reconcile triggers.
- `svc/workflow` reaches Control's queue deployment-hold pair and the migration service's schema
  bundle apply. Routing the bundle through Control would move workflow artifacts into Control,
  which is the leak the bundle path removes.
- `svc/gateway` reaches Control's route feed and the worker's dispatch endpoint.
- `svc/worker` reaches Control's version, app, environment, data-key and binding reads, its own
  retire and renew, the workflow register, assignment, renew, release and job endpoints, the policy
  lease, CDC subscribe, and journal ensure.
- `svc/migrate-server` mints but reaches nothing.

The route declaration and the authorization are one statement on both ends: `configure` in
`crates/zeroship-worker/src/handler.rs` registers the dispatch route from the endpoint's own path
template, and `configure` in `crates/zeroship-control/src/internal.rs` registers Control's internal
routes from the same constants, so the route a service serves and the route a caller is admitted to
cannot drift into two copies.

`crates/zeroship-workflow-server/src/auth.rs` shows the pattern at its most developed.
`WorkflowAuth` distinguishes a **role** assertion, verified against the process-wide peer
bundle, from an
**instance** assertion, where the issuer must carry the `svc/worker` principal, its instance segment
parses as a worker id, the public key comes from the active-worker registry, and a single-key trust
bundle is built for that exact issuer before verification runs. The two are never fallbacks for each
other. The schedule endpoints add a further explicit check that the issuer is Control's, and the job
endpoints revalidate the worker mid-operation rather than trusting the entry check for the whole
call.

### 3.4 Worker join

A worker holds no credential of its own before it joins. `crates/zeroship-core/src/worker_join.rs`
defines three documents and one wire format: the signer import (Control's trust anchor -- an id, a
public key, and the execution zones that signer may mint for), the signer credential (the private
half, held by whoever mints, which is what the CLI's `cmd_join_token` reads), the join token
(`mint_join_token` and `verify_join_token`, a JWT with its own `typ` so it can neither be presented
where an assertion is expected nor accept one in its place), and the join proof
(`join_proof_message`, a detached signature over the token, the presented public key and the claimed
port -- not a JWT, because the signer of it has no identity yet and so has no issuer to put in one).

`WORKER_JOIN_PATH` is a bare path and deliberately not a `ServiceEndpoint`: there is nothing for the
allowlist to grant a caller that holds no service identity. Control verifies it in
`crates/zeroship-control/src/worker_join.rs` against its own signer registry.

A captured token admits workers the captor controls, up to its remaining uses, until its expiry, in
the one zone it names. It cannot register a key whose private half the presenter does not hold,
because the join request is signed by that key; and when the token carries a confirmation claim it
cannot register any key but the one the issuer had in mind. That bound is why the expiry is short
and the use count is a cap rather than a formality.

After joining, a worker's grants are held by its *instance* identity. `verify_worker_instance` in
`crates/zeroship-control/src/internal.rs` resolves the registered public key for the instance named
by the assertion's issuer and verifies under the full profile against a one-key bundle. Control
refuses a bare role-arity `svc/worker` assertion outright, and no process holds such a key, so every
grant in the worker row is reachable only by a joined, live instance. Retire and renew take no
instance selector: Control acts on the instance whose key verified the call, so no worker can hold
another's identity open.

### 3.5 The shared control key, and what still rides it

`control_key` is one secret several platform processes hold, declared as the shared identity
`CONTROL_KEY` in `crates/zeroship-config-macros/src/shared.rs` and consumed by
`crates/zeroship-control/src/config.rs`, `crates/zeroship-gateway/src/config.rs`,
`crates/zeroship-worker/src/config.rs` and `crates/zeroship-migrate-server/src/config.rs`. Auth
explicitly does not hold it: `crates/zeroship-auth/src/config.rs` lists `CONTROL_KEY` among its
retired settings and asserts no argument carries it.

Control's `/internal/*` surface is split between the two mechanisms, by caller set rather than
convenience. `check_service_auth` in `crates/zeroship-control/src/internal.rs` names one service
under a key only that service holds; `check_auth` in the same file proves only that the caller read
the same file Control did. Each endpoint still on `check_auth` is one whose caller set is exactly
the processes holding that file.

On the shared key today: `get_routes` (the gateway's route and snapshot feed), `get_versions`, and
the two reconcile triggers. On per-instance assertions: `get_app_version`, `get_app_env`,
`get_app_data_key`, `get_app_bindings`, `renew_worker_instance` and `retire_worker_instance`.
`join_worker_instance` is on neither, for the reason in 3.4.

The gateway's route pull is plaintext by construction: `http_get_inner` in
`crates/zeroship-gateway/src/sync.rs` opens a raw TCP stream and writes the key as a bearer header,
and `validate_control_url` refuses an `https://` control URL rather than silently downgrading it
and sending the key in cleartext on the plain-HTTP port.

`ServiceEndpoint::CONTROL_ROUTES` is declared and granted to `svc/gateway` in the allowlist, but
`get_routes` runs `check_auth`. The grant describes an edge the handler does not check.

### 3.6 Workflow signals

A workflow signal today carries **no credential**, because it never crosses a boundary.
`AppWorkflows::signal` in `crates/zeroship-workflow/src/service/app.rs` is reached from creator
JavaScript through the V8 class and the app backend, and its authority is structural: the caller
already holds an `AppWorkflows` bound to one app id and one policy binding. What it checks is
validity and ordering -- the run, the signal type, request idempotency, the ingress epoch fence, and
the policy's input cap.

There is no internet-facing signal ingress. No route in the gateway's server builder, the workflow
server's route table, or Control's configurers exposes one; the gateway crate contains no reference
to workflows at all. The token-authenticated ingress path that exists in
`crates/zeroship-workflow/src/service/ingress.rs` is covered in section 6.

Run lifecycle is likewise in-process: pause, resume, cancel and restart go from the V8 class through
`AppBackend` to `AppWorkflows`, not through Control.

Toward the journal, workflow execution authenticates as the worker, carrying a `WorkerIdentity` and
a task token whose hash is matched on inspection
(`crates/zeroship-workflow/src/service/types.rs`, `crates/zeroship-workflow/src/service/tasks.rs`).
Tenant isolation during replay is the schema binding rather than a user identity: `env.auth`
resolves to nothing during replay, because the per-request user is set only on the HTTP dispatch
path.

---

## 4. Revocation and logout

Three mechanisms, at three latencies.

### 4.1 The pushed snapshot

The gateway polls Control's route feed and applies a whole `GatewaySnapshot`
(`crates/zeroship-core/src/types.rs`): routes, principal lifecycle, and family revocations.
`RouteCache::update_snapshot` in `crates/zeroship-gateway/src/sync.rs` derives an offline denylist
from it. `GatewayPrincipalLifecycle::blocks_authentication` is true when a principal is disabled,
anonymized, deletion-requested or deletion-scheduled, and the row carries the pairwise subjects
already issued for that human, so denials stay valid after a route is removed and no gateway pull
has to expand users by routes.

Both credential arms require a fresh snapshot that allows the subject, in every mode. This is what
blocks a disabled or deleting principal with no request-path database read.

### 4.2 The per-app family marker

`crates/zeroship-authz/src/wrapper_revocation.rs` holds a revocation cutoff per client and subject
in `zeroship.token_revocations`. One row per family, so no reader enumerates live token ids, and the
rule every reader applies is to reject when the cutoff is later than the credential's issuance
instant. `revoked_after_for` returns the cutoff as a rounded-up timestamp rather than a boolean, so
one cached value serves every credential in the family; rounding up is load-bearing, because
rounding down would collapse a revoke in the same second as the mint back to the issuance instant
and read as "not revoked".

`family_revocation_decision` in `crates/zeroship-gateway/src/router/auth.rs` answers from
`GateState.revocation_cache` when the entry is fresh, comparing locally with no round trip, and on
a miss reads once and caches the result, negative results included. It fails closed in the way that
matters: a miss that cannot reach the database returns `RevocationDecision::Unavailable` *without*
caching anything, so the next request retries rather than serving a guessed answer, and every arm
maps it to the same rejection as a revocation. Keeping them distinct variants is what keeps the two
log lines honest.

Because the read is cached, a revocation written on another node is honoured within the cache TTL
rather than instantly; a same-node writer busts the entry immediately. With no gateway database
configured only this secondary check is skipped, and the pushed snapshot still applies.

The same marker is read by `BearerVerifier` for Control's CLI bearer and by the OP's introspection
and UserInfo paths, and is written by the gateway's signout and back-channel logout, by Control's
grant revocation, and by Auth's password reset.

### 4.3 Back-channel logout

`crates/zeroship-gateway/src/backchannel_logout.rs` implements the OIDC BCL 1.0 relying-party
endpoint. It is registered at the gateway host rather than per app subdomain, because the URI
must be stable for every `backchannel_logout_uri` registered with the OP.

The handler peeks the token's audience for *routing only*, resolving it to one app through the route
cache, then verifies with `zeroship_core::logout_token::verify` against the OP's JWKS: issuer,
audience, expiry, the BCL `typ`, the required claim set, the events shape, the rule that a nonce
must be absent, and at least one of subject or session id. Replay is answered success without
re-running side effects, guarded by `GateState.logout_jti_cache` plus a process-local in-flight
claim that closes the check-then-act race before an id is promoted into the cache.

Revocation prefers the session id and falls back to the token's subject at that app. Because the
session and anchor tables are RLS-scoped per app, a back-channel logout is per app by construction.

**The transport itself is not additionally authenticated.** `post_logout_token` in
`crates/zeroship-auth/src/oidc/backchannel_logout.rs` sends the form body with no `Authorization`
header, and the gateway's handler reads none. What authenticates the edge is the OP's signature over
the logout token. See section 6.

### 4.4 What a signout clears

The app-origin signout route resolves the anchor under RLS (so a cross-app replay resolves to
nothing), derives the pairwise subject, writes the per-app family marker and busts the local cache
entry, deletes the anchor row or every anchor for that user on a global scope, makes a best-effort
RFC 7009 revocation call to the OP, and clears the session, anchor and breadcrumb cookies. The
authoritative step is the family marker: the short-lived signed cookie is not revoked by deleting a
row, it is revoked by a cutoff both arms consult.

---

## 5. Key and secret custody

| Material | Held by | Purpose |
| --- | --- | --- |
| `gateway.signing_key_file` and its previous-key pair | gateway | Signs the app-session cookie. |
| `gateway.service_key_file` | gateway | Mints dispatch assertions **and** signs identity envelopes. |
| `*.service_peers_file` | every service | Peer public halves, indexed by issuer. |
| `gateway.stash_signing_key` | gateway | MACs the PKCE, state and nonce stash cookie. |
| `pairwise_salt` | gateway, control | The `pws_` projection. Permanent; rotating it re-keys every app's stored user references. |
| `anchor_enc_key` (derived at boot) | gateway | AES-256-GCM for the refresh family at rest. |
| `gateway.broker_secret_file` | gateway | Derives the per-app OP client secret. |
| `control_key` | control, gateway, worker, migrate-server | Bearer on the `check_auth` subset of Control's internal routes. |
| OP signing keys | auth | ID, access and logout tokens; retired by `cron/signing_key_retention.rs`. |
| TOTP encryption key | auth | AES-256-GCM over enrolled secrets, bound to the user id as AAD. |
| Join signer credential | operator | Mints worker enrolment tokens. |

Two deserve emphasis. The gateway's service key does two jobs: what the gateway asserts about a
*service* (itself) and what it asserts about an *end user* both carry its signature, and the worker
checks both under the one published public half. And it is deliberately distinct from the
session-cookie key, because one key doing both is the shape `crates/zeroship-gateway/src/lib.rs`
records as removed.

Derivation helpers live in `crates/zeroship-core/src/auth/mod.rs`: `constant_time_eq`, which
iterates a fixed number of times over the *expected* length and folds a length mismatch into the
accumulator rather than exiting early, so timing depends on nothing attacker-controlled;
`hash_client_secret` and `validate_client_secret`, a bare unsalted digest that is correct for a
machine-generated high-entropy secret and wrong for anything a human chooses; and
`derive_broker_secret` with `validate_broker_master`.

---

## 6. Declared but not wired

These exist in the tree with no live caller. They are listed so a reader does not mistake a
definition for a flow.

**Token-authenticated workflow signal ingress.** `AppWorkflows::ingest_signal`,
`issue_signal_token` and `revoke_signal_tokens` in
`crates/zeroship-workflow/src/service/ingress.rs`, and the capability codec behind them
(`mint_signal_capability` and `verify_signal_capability` in
`crates/zeroship-workflow/src/service/capability.rs`) have no caller outside that crate's own tests,
in Rust or in any published package. They are also unreachable by configuration: the only production
implementation of the resource provider,
`crates/zeroship-worker/src/workflow_host.rs`, sets `signal_authority: None`, so the authority
lookup refuses on every production host. And no HTTP route anywhere exposes ingress.

**The app-capability codec.** `mint_app_capability` and `verify_app_capability` in the same
`capability.rs` are referenced only from that file's own test module, and are not re-exported.

**`sign_workflow_signal_token` and `verify_workflow_signal_token`**
(`crates/zeroship-core/src/workflow_signal_token.rs`) have no callers outside the test in the same
module. Its documentation describes enforcement at a control-plane ingress terminus, and no such
terminus exists. Its typed-id prefix is still reserved in `crates/zeroship-id/src/typed_id.rs`.

**`derive_app_scoped_control_token` and `validate_app_scoped_control_token`**
(`crates/zeroship-core/src/auth/mod.rs`) have no callers anywhere, in production code or tests. The
documentation describes a runtime-side derivation verified against an app id request header; no such
header is read anywhere in the tree.

**Inbound service-assertion verification on the gateway.** The gateway builds a full-profile
verifier at startup for the stated purpose of the inbound back-channel logout edge, and
`endpoints::GATEWAY_BACKCHANNEL_LOGOUT` is declared and granted to `svc/auth`. Neither end uses it:
Auth's outbound post carries no `Authorization` header, and the gateway's handler reads none. The
gateway's `ServiceAuth` is live for minting outbound and for signing envelopes, and verifies nothing
inbound.

**Workflow run management through Control.** `WORKFLOW_MANAGE` and `WORKFLOW_MANAGEMENT_STATUS` are
served by the workflow server and granted to `svc/control`, and the corresponding
`ControlCoordinator` methods in `crates/zeroship-workflow-client/src/control.rs` have no caller
under `crates/zeroship-control/`. Control's live workflow calls are journal ensure, the schedule
trio, and assignment verification.

**Policy conditions and deny effects.** `Condition::IpRange` and `Condition::TimeWindow` and
`Effect::Deny` in `crates/zeroship-authz/` are constructed only in tests: the one production
producer of a policy, `scopes_to_policy`, emits an allow with no conditions. The request address and
minute-of-day are supplied truthfully on every request and read by no live policy.

**The CLI's Supabase provider.** `--provider=supabase` parses
(`crates/zeroship-cli/src/auth.rs`), and `cmd_login` routes both variants into the same platform
device flow, writing the platform provider. The Supabase refresh and userinfo arms are therefore
reachable only by hand-editing the stored credential file.

---

## 7. The edge is the authority on claimed names

An app's name is its hostname label: the gateway derives the app from the first label of the `Host`
header, and the edge serves creator apps off a wildcard. So a name the platform edge already claims
is a name a creator app may not register.

`deploy/ops/Caddyfile` declares the host blocks. `crates/zeroship-control/src/reserved_names.rs`
holds `EDGE_ROUTING`, classifying each claimed label as `Shadowed` (the edge terminates the host
before the gateway sees it) or `Reachable` (the edge proxies it to the gateway, which resolves it as
an ordinary creator app). `auth` and `control` are shadowed; `api` and `console` are reachable.

The two classes fail differently. A shadowed name registered by a creator silently never receives a
request, and in any deployment whose edge is not this Caddyfile, the OIDC issuer origin resolves
through the wildcard to whoever holds the name. A reachable name gives a creator arbitrary content
on a platform *origin*, and for `console` that origin is the one permitted to frame the real login
page by the `frame_ancestor_origins` allowlist.

The reserved list is pinned to the edge rather than restating it. `is_reserved_app_name` matches
case-insensitively, because hostnames are case-insensitive and reserving a name case-sensitively
would leave the host it actually resolves to unprotected. The labels are deployment-invariant: every
host in the Caddyfile is written as a literal label against a domain variable, so one list covers
every deployment of this edge.

---

## Where to look next

- Creator-facing contract, with no implementation detail: `docs/reference/auth.md`.
- Route matching, CORS, rate limits and the rest of the dispatch chain:
  `docs/architecture/gateway-routing.md`.
- Isolate lifecycle and the binding surface `env.auth` sits in: `docs/architecture/runtime.md`.
- What Control owns and how a deploy reaches it: `docs/architecture/control-plane.md`.
- How an identity becomes a database actor, and the masking rules that follow:
  `docs/architecture/data-system.md`.
