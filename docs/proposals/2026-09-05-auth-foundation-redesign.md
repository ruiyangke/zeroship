# The auth foundation redesign

Status: proposal. Date: 2026-09-05. Scope: identity, sessions, revocation, and
service-to-service authentication across `zeroship-auth`, `zeroship-gateway`,
`zeroship-worker`, `zeroship-control` and `zeroship-cli`.

---

## 1. What this is and what it is not

This is a refactor of the auth *foundation*. It changes what the identity model
is made of, not which checks are missing from it.

It is **not** a defect sweep. The defects catalogued during the investigation
are used here only as evidence about the shape - each one is offered as the
predicted output of a structure, and the structure is what changes. Fixing them
one at a time is the failure mode this document exists to avoid, because the
same shape regenerates them: the tree already contains a correct fix to the
grant-revoke cascade that a later mint undoes, a correct narrowing of the
worker's control-plane token that the worker itself derives, and a correct
identity signature verified under the same secret that authenticates the hop.
Every one of those is a competent local fix that the model defeats.

It is also not a threat model, a rollout plan, or a set of numbers. Where a
quantity is load-bearing it is named as a symbol and a gate arm is called for to
re-measure it; no value is asserted in this document.

**What must be true when this lands.** One process mints human identity. The
internet-facing process holds no key whose disclosure forges an identity. The
process that executes creator code holds no platform-root credential. Revocation
has an object, and no credential is minted without reading that object.

**Everything in the working tree is fair game.** Pre-launch: no users, no
tenants, no deployed apps. There are no migration shims in this design and no
deprecation aliases. Where a shape is replaced, the old one is deleted in the
same change.

---

## 2. The diagnosis, compressed

### 2.1 The generator

Zeroship has a strong set of authentication *mechanisms* and no *model* under
them. Identity is a pure derivation nobody arbitrates, revocation is a marker
primitive with no object it revokes, and the trust root sits inside the two
processes that face the internet and run creator code.

That last fact is why the defects recur rather than accumulate randomly. A
reviewer asking "does this check bind?" must answer "against whom?", and for
every check downstream of the gateway the answer is "against nobody, the gateway
can already do it". The review procedure itself cannot separate a real fence
from a decorative one, so decorative fences accrue at the rate they are written.

Stated as the rule this design is derived from, sharper than the AGENTS.md
invariant it specialises:

> A credential a process can MINT is not revocable by any record that process is
> not required to read.

### 2.2 The structural problems, ranked

**P1. The trust root is the internet-facing edge.** `zeroship-gateway`
terminates untrusted HTTP for every creator app and simultaneously holds the OP
broker master, the pairwise salt, the session signing key, the anchor encryption
key, the OIDC stash key, `worker_key`, `control_key`, and write access to the
identity tables. No downstream fence at the edge is falsifiable while that is
true.

**P2. The process that executes creator code holds platform-root credentials.**
`zeroship-worker` runs untrusted V8 and holds `control_key`, which fetches any
app's decrypted environment over HTTP, and `worker_key`, which mints identity.
`derive_app_scoped_control_token` is a real narrowing correctly verified by
`check_app_scoped_auth` - and it is computed, inside the worker, from the root
the worker holds. A narrowing computed by the party being narrowed is
appearance. Its production callers are `crates/zeroship-worker/src/handler.rs`
and `crates/zeroship-plugin-workflow/src/client.rs`, both inside the worker.

**P3. There is no service identity, so every internal hop borrows a shared
symmetric root, and one root does two unrelated jobs.** In
`crates/zeroship-worker/src/handler.rs`, `check_worker_auth` compares the bearer
against `config.worker_key` and `verified_user_json` passes the same value as the
MAC key over `ZeroShip-User`. The rustdoc on `encode_user_header` in
`crates/zeroship-gateway/src/oidc_rp.rs` claims the identity signature survives a
bearer bypass; that is false by construction, and
`crates/zeroship-worker/src/policy.rs` quotes the false sentence as its own
justification. The degraded configuration is worse than either mechanism alone:
`check_worker_auth` allows on an empty `worker_key` while `verified_user_json` is
not skipped and verifies an HMAC under the empty key. With no per-service
identity, a handler that must reach a worker has to hold the platform's most
powerful symmetric key, which is exactly why `workflow_advance_internal` in
`crates/zeroship-gateway/src/router/dispatch.rs` checks no caller credential,
carries `TODO(DW-signed-transport)`, and then signs the worker hop with
`worker_key`.

**P4. There is no session or grant object, so teardown is a hand-written
enumeration over stores, re-derived per event.** Correctness of a teardown is a
property of the author's enumeration at the time of writing, so every new
credential kind silently un-covers every existing teardown and no test can
notice, because there is nothing whose completeness a test could assert. The
sharpest evidence: `anchors::create`'s only production caller is
`crates/zeroship-gateway/src/auth_token.rs`, so the SDK popup login mints an
anchor and the interactive redirect login does not, while
`crates/zeroship-gateway/src/browser_auth.rs`'s `signout` resolves the user from
the anchor cookie and returns `signout_cleared` before touching the database when
it is absent. One verb, two entry paths, two meanings, and no place in the model
where "a session" is a thing both paths produce.

**P5. Revocation records gate presentation; nothing gates minting.** The marker
in `token_revocations` is compared against a credential's `iat`, and in
`rotate_family` against `rotation_started_at`. The anchor is a minting capability
whose absolute life is `ANCHOR_ABS_DAYS`, and no mint path consults the marker
against the anchor's own creation. `revoke_grant_cascade` in
`crates/zeroship-control/src/oauth_grants_handlers.rs` correctly deletes the
grant, correctly stamps `app_user_identities.revoked_at` and correctly upserts
the marker - and degrades to alias suppression at the next page load, because the
mint outruns it.

**P6. The retention ordering is not merely unlinked, it is inverted.** The
correctness condition is `sweep retention >= push window >= longest recallable
credential lifetime`. The push window is a bare SQL `INTERVAL` literal in
`crates/zeroship-control/src/registry.rs`; the sweep uses
`WRAPPER_REVOCATION_RETENTION_HOURS` in
`crates/zeroship-authz/src/wrapper_revocation.rs`; the longest recallable
capability is `ANCHOR_ABS_DAYS` in `crates/zeroship-gateway/src/anchors.rs`, and
it is longer than either. For the one teardown that writes a marker without
deleting the anchor, the marker is swept while the capability it was written
against is still alive. Task #209 as worded understates its own finding.

**P7. OAuth client authentication is a distinction the model carries and nothing
enforces.** Per-app clients are brokered, and the gateway derives the secret for
any app's client from a master it holds. The OP cannot attribute a token exchange
to an app more strongly than it can attribute it to the gateway, so per-app
clients are a naming scheme, not an isolation boundary, and every control built
on client identity inherits that ceiling silently.

**P8. Identity is a derivation replicated across processes with two supply
shapes and no agreement check.** `derive_pairwise` is called independently in
auth, the gateway and control. The salt reaches auth as raw file bytes and
reaches the others through the config `Secret` layer, whose file loader strips a
trailing newline - a hazard stated verbatim in
`crates/zeroship-core/src/config/file.rs`. A divergent salt does not error; it
produces non-colliding subjects, so revocations write markers nobody presents and
the system reports "nothing to revoke" rather than "salt mismatch". The
instrument reads clean precisely because the thing it measures is broken.

**P9. Granularity is wrong at both ends.** Too fine: the sector is the app apex,
so no cross-app subject exists to revoke and two apps sharing a database cannot
agree who a user is (task #72). Too coarse: one users namespace serves creators
and app end users, and `disabled_at` - the one column meaning "suspended, not
deleted" - is read across the auth store, the identity paths, the OIDC endpoints,
`zeroship-authn` and `crates/zeroship-control/src/registry.rs`, and written only
from test targets. A distinction the model fails to carry means abuse response
has one destructive lever; a distinction it carries but never writes means every
reader believes a dead branch is live.

**P10. One fact, several hand-copied spellings, pinned by tests that assert a
literal against themselves.** The identity upsert is byte-identical in
`crates/zeroship-gateway/src/identities.rs` and the auth store; the registered
callback path sets exist in both the gateway and
`crates/zeroship-control/src/app_oauth_client.rs`; the marker upsert is spelled
by hand in auth, control and the gateway's shared helper. A test asserting a
literal against its own copy measures nothing about a cross-file invariant, so
the instrument is green in exactly the state the invariant is violated.

**P11. Stores and columns shaped like enforcement that enforce nothing.**
`gateway_sessions.revoked_at` is written by back-channel logout and read only by
`crates/zeroship-gateway/src/sessions.rs`'s `validate`, which has no production
caller and two doc comments elsewhere saying the request path deliberately does
not use it. `check_api_key` in `crates/zeroship-gateway/src/auth.rs` is defined
and never called, beside a plaintext `apps.api_key` column stored next to its own
hash. `crates/zeroship-control/src/identity_bridge.rs`'s `provision_or_link` has
only test callers and a header claiming it sits on the bearer read path. Each is
a template: a store adjacent to a stateless path invites the next author to add a
check to the store, the check is inert, and it becomes evidence for the author
after that.

### 2.3 What is GOOD today and must survive

A redesign that discards these is worse than none. Every one is named so an
implementer can check it survived.

- **`derive_pairwise` canonicalises its subject** before hashing. That is the
  only reason independent callers ever agreed. Keep the function pure and keep
  the canonicalisation even after there is one caller.
- **The fail-closed rebinding guard** on the identity upsert,
  `ON CONFLICT ... WHERE pairwise_sub = EXCLUDED.pairwise_sub`. It converts a
  derivation drift into a refusal instead of a silent re-bind. It is the only
  thing standing between P8 and silent identity corruption. When the duplicate
  write is collapsed, the survivor keeps it.
- **`credential_version` as a data dependency, not an enumeration.**
  `crates/zeroship-auth/src/store/sessions.rs`'s validating UPDATE joins the
  users table and requires equality in the same statement, so bumping the version
  invalidates sessions without anyone listing them. This is the correct answer to
  P4, already working, in one tier. Generalise the shape; do not replace it with
  a bigger enumeration.
- **Single-statement invariants over check-then-act.**
  `issue_authorization_code`'s consume folds lifecycle and `credential_version`
  into the statement that marks the code used; `password_reset::complete` is one
  CTE; `anchors::read_live` is RLS-scoped so the read *is* the bind check. Each
  is unfalsifiable-by-race by construction.
- **The refresh family.** Versioned HMAC keyring (verify-with-any,
  sign-with-newest), rotation with reuse detection and family kill, AEAD-sealed
  single-retry idempotency with AAD bound to the predecessor hash and family id,
  in `crates/zeroship-auth/src/oidc/refresh.rs`. It is the only credential in the
  tree with a real rotation story and it becomes the template for both the
  session secret and service-key rotation.
- **The stateless session cookie with kid rotation** and local verify on the hot
  path. The problem is the revocation model around it, not the cookie.
- **Mandatory PKCE for every client including confidential ones, mandatory
  nonce, exact `redirect_uri` match, RFC 9207 `iss`.**
- **Header hygiene at the dispatch boundary**: stripping inbound
  `zeroship-user`, `authorization`, `x-app-id` and the `x-zs-` family in
  `collect_forwarded_headers` / `is_reserved_header`, plus the request-id and
  issuance-window binding. That binding is what the header actually buys; say so
  in the rustdoc instead of the false independence claim.
- **`policy::enforce` running before the isolate lease**, so
  `env.auth.requireUser` is not the fence, and the SEC-2 canonical-path agreement
  so the gateway's gate and the worker's re-parse cannot disagree.
- **`load_client` failing closed** on a decode error rather than defaulting, and
  `register_signed_token` refusing to release a token whose key row is no longer
  active or retiring.
- **The static nonce-CSP relay page** (`popup_callback_html` / `popup_csp` in
  `crates/zeroship-gateway/src/browser_auth.rs`) and its pinning test. The only
  interpolation is the server nonce.
- **The dev tier's distinct cookie name** in
  `crates/zeroship-runtime/src/core/dev_auth.rs`: dev-only by construction, not
  dev-only by flag.
- **The tree's convention of writing "an earlier version of this comment said
  the opposite - believe the code"**, as `crates/zeroship-auth/src/csrf.rs` does.
  That norm is why this audit was possible. Extend it to every doc this design
  corrects.

---

## 3. The model

### 3.1 The governing invariant

**MINT-READS-ROW.** No credential in this system is issued except from a
validating read of a `zeroship.sessions` row, and that read is the same statement
that enforces liveness, expiry, the session epoch and the person's credential
epoch.

Section 6 states how this is bound. It is not a check; it is a type.

### 3.2 Identity concepts

Four, each justified by what breaks without it.

**PERSON** - `zeroship.users`, typed id `usr_`. One namespace for every human.
"Creator" is not an identity kind; it is a membership edge on a project. Carries
`credential_epoch` (the generalised `credential_version`) and `subject_status`,
one column with a real state machine over `active`, `suspended`,
`deletion_scheduled` and `anonymized`, replacing the separate lifecycle columns
that today are read everywhere and written from tests. One column, one predicate,
one feed field, and a variant with a production writer.

The table keeps its current name. A rename to `people` was considered and
rejected: it is churn across the auth crate that binds nothing. The typed id
does change - today the id defaults to a bare `uuidV4()` in
`db/migrations-ts/20260702000300_auth_oauth_tables.ts`, in violation of the
AGENTS.md typed_id invariant.

*Without it:* nothing. It is the only stored identity fact.

**AUDIENCE** - a closed sum: `Platform` or `Project(project_id)`. This is the
unit a subject and a grant are scoped to, and it replaces `sector_identifier`,
the per-app OAuth `client_id`, and the CLI pseudo-client with one value.

`Project`, not `App`. That is task #72, and it is a premise of this design
rather than a follow-up: with app-scoped sectors there is no unit between "one
app" and "the platform", so cross-app teardown has no object.

*Without it:* subjects are either global, so apps correlate users across the
platform, or per-app, so a project's apps cannot agree on a user.

**SUBJECT** - a pure derivation computed in one process:

```
subject(Platform,     person) = person.id                       // usr_...
subject(Project(pid), person) = "pws_" + derive_pairwise(salt, pid, person.id)
```

Stored on the grant row under the existing fail-closed rebinding guard, so the
reverse lookup needs no second table and the guard lives in one file. In the end
state `zeroship-auth` is the sole holder of the salt and the sole caller of the
derivation. That deletes P8 by removing its precondition rather than by adding a
check - which is the right instrument for a failure mode whose signature is a
clean instrument reading.

**SESSION** - `zeroship.sessions`, typed id `ses_`. This is the object the whole
design turns on.

```
id                 ses_...
person_id          -> users.id            ON DELETE CASCADE
audience_kind      'platform' | 'project'
project_id         -> projects.id NULL    ON DELETE CASCADE  (NULL iff platform)
grant_id           -> grants.id  NULL     ON DELETE CASCADE  (NULL iff platform)
parent_session_id  -> sessions.id NULL    ON DELETE CASCADE
kind               'browser' | 'cli' | 'device_pending'
epoch              bigint, per-session
credential_epoch   copied from users at creation
secret_hash        keyed HMAC, versioned keyring
secret_key_version
prev_secret_hash   NULL outside the idempotency window
rotated_at
idem_response_enc  AEAD, AAD bound to (prev_secret_hash, id)
idem_expires_at
amr, acr, auth_time, scopes, label
created_at, idle_expires_at, absolute_expires_at, revoked_at
```

One table replaces `idp_sessions`, `gateway_sessions`, `app_session_anchors`,
`oauth_refresh_tokens`, `device_grants` and `token_revocations`.

Three columns earn their place individually. `parent_session_id` makes "log this
human out of everything they reached from this login" a tree delete instead of an
HTTP fan-out, which is what deletes back-channel logout. `credential_epoch` makes
"a password change kills every session" a data dependency rather than an
enumeration. `epoch` separates "narrow this app's scopes" from "end this
session"; without it they are the same operation, and consent narrowing has to
log the human out.

Folding `device_grants` in as `kind = 'device_pending'` is not tidiness: that
table has no sweeper anywhere in the tree today, and rows leave it only by
redemption, denial, or an expiry noticed during a poll.

*Without it:* P4 in full.

**GRANT** - `zeroship.grants`, one row per (person, audience).

```
id, person_id, audience_kind, project_id, subject, scopes, relay_email,
first_consented_at, updated_at
UNIQUE (person_id, audience_kind, project_id)
```

**No `revoked_at`.** Revocation is `DELETE`, and sessions cascade off it. That
single choice is the fix for the whole P5 family: the object that could re-mint
is the same row the revoke removes, so a marker never has to outlive a
capability.

It replaces `oauth_grants`, `app_user_identities`, `principal_grants` and
`identity_links`. The platform-audience row is where CLI scope authority lives,
so the issuable-scope constant stops being an authority ceiling and becomes a
seed. `app_user_identities.revoked_at` - a column with more clearers than
setters, every clearer a login, and readers that disagree about what it means -
ceases to exist.

**What is NOT an identity concept**, though the current tree treats each as one:
`oac_` client ids, `sector_identifier`, `sid` as a separate keyspace,
`refresh_family_id`, `identity_links` markers, federated identities (they are an
*authentication method*, kept as such under the person), the CLI client
registration, provider discriminators. Every one is a spelling of Audience or
Session.

### 3.3 Credentials

Each row answers: what distinction does it carry, and what enforces it?

| Credential | Shape | Distinction | Enforced by |
|---|---|---|---|
| **Session secret** `zss_` | opaque CSPRNG, stored as versioned keyed HMAC on the session row, rotates on use with reuse detection | this browser or device is this session | the validating UPDATE on `zeroship.sessions`; reuse kills the row |
| **Access assertion** | Ed25519 JWT, `typ: zs-access+jwt`, claims `iss, aud, sub, sid, epoch, iat, exp, scopes, amr, auth_time` plus profile projection | hot-path presentation with no database read; `aud` separates Platform from `Project(pid)` | local signature plus `aud` equality; `sid` and `epoch` are the revocation handles |
| **Service assertion** | the existing `crates/zeroship-core/src/service_assertion.rs`: per-service Ed25519, `svc-assertion+jwt`, mandatory single-use `jti` | which *service* is calling | peer JWKS by `kid` plus the replay store in `crates/zeroship-authn/src/service_replay.rs` |
| **Identity envelope** `ZeroShip-User` | JSON plus signature, bound to the dispatch request id and an issuance window | gateway-asserted end-user identity on the worker hop | the gateway's Ed25519 private key, verified under its public half |
| **Flow envelope** `__Host-zs_flow` | HMAC, purpose-tagged, `iat`/`exp` inside the payload | this browser started this flow | one decode with constant-time compare and server-side expiry |
| **One-time secret** | CSPRNG, hashed at rest, purpose-tagged row | which flow may redeem it | the consume UPDATE's `WHERE purpose = $2 AND consumed_at IS NULL AND expires_at > now()` |
| **Workflow signal token** `wst_` | unchanged | a machine capability on one run, with no person behind it | unchanged; the only bearer capability that is neither a session nor a service |
| **Credential epoch / session epoch** | not credentials, columns | everything issued before this moment is void | the join in every session validate |

That is the entire inventory. Everything else in the tree is deleted or merged.

**Two cookies at an app origin, and the split survives the merge prior.**
`__Host-zs_session` (HttpOnly, Secure, SameSite=Strict, long) carries the session
secret; `__Host-zs_access` (HttpOnly, Secure, SameSite=Lax, short) carries the
access assertion. They are one credential kind in two roles: a **minting**
capability that always costs a database read, and a **presenting** capability
that never does. Collapsing them one way costs a database read per request;
collapsing them the other makes the hot-path credential a minting capability,
which is precisely the anchor defect. Strict on the refresh half is
browser-enforced and is what stops a cross-site navigation from rotating a
session.

Deleted from the cookie inventory outright: the non-HttpOnly authentication
breadcrumb. It carries no capability and the SDK's first probe of a page load is
unconditional, so it optimises nothing that is not already covered.

### 3.4 Process ownership

```
                          HOLDS                                MAY MINT
  zeroship-auth   pairwise salt (sole holder, raw file bytes)   sessions
                  access-assertion Ed25519 private key          access assertions
                  session-secret HMAC keyring (versioned)       one-time secrets
                  TOTP key, flow-cookie key                     flow envelopes
                  upstream federation client secrets
                  own service Ed25519 private key
                  --- SOLE WRITER of users / sessions / grants ---
                  two database roles, see 6.9

  zeroship-gateway  own service Ed25519 private key             ZeroShip-User envelopes
                    peer JWKS (auth, control, worker publics)     (nothing else)
                    route table + revocation feed
                    NO salt. NO identity signing key.
                    NO symmetric root. NO database credential.

  zeroship-worker   own service Ed25519 private key             nothing
                    peer JWKS, revocation feed
                    NO control_key. NO worker_key. NO salt.
                    NO BYPASSRLS.

  zeroship-control  env master key, Stripe keys                 env leases
                    own service Ed25519 private key             placement decisions
                    peer JWKS
                    NO salt. NO worker_key. NO control_key.
```

Read the gateway row as the thesis. **The trust root moves off the process that
terminates every creator-app request and runs adjacent to creator code, onto the
process whose whole job is rendering a login form.** The gateway becomes *unable*
to mint an end-user identity, because it holds no key that produces one. Section
9 states honestly how far that goes and what it costs.

### 3.5 Distribution: one feed shape, on the transport that already exists

Verifiers need three facts they do not own: routes, revocations, and public keys.

The gateway already pulls the route table on a sleep loop in
`crates/zeroship-gateway/src/sync.rs`, and
`zeroship_core::readiness::staleness_budget` already expresses "this pulled fact
is too old to act on". **This design adds one feed to that shape and invents no
new transport.** The revocation feed is an append-only, monotonically sequenced
table owned by auth, carrying session revocations, epoch floors and person-status
changes; verifiers hold a cursor and a last-success instant. JWKS rides the same
poll.

**Fail-closed rule.** A verifier whose feed has not advanced within the staleness
budget refuses to authenticate. `RequiredPrincipal::User` routes answer 503;
`Anonymous` routes still serve. This mirrors the freshness gate the route table
already has, which is one of the things the current tree gets right.

This replaces **both** current revocation mechanisms - the pushed snapshot in
`crates/zeroship-control/src/registry.rs` and the polled cache in
`crates/zeroship-authz/src/wrapper_revocation.rs`. Neither dominates the other
today: the pushed one adds coverage and the staleness refusal, the polled one
adds recency and applies no age filter. One feed with a cursor and a fail-closed
bound has both properties and needs no ordering coincidence between two literals.

---

## 4. The two flows

### 4.1 End-user app login

The gateway is no longer an OIDC Relying Party. There is no PKCE verifier at the
edge, no stash cookie, no per-app OAuth client, no broker secret, no id_token, no
`at_hash`. The handshake is first-party between two of our own processes, and the
landing code is redeemed over the authenticated service channel - strictly
stronger than PKCE, because the redeemer proves an identity rather than proving
it once held a random string.

```
BROWSER (app origin)          GATEWAY (app origin)            AUTH (auth origin)
=====================================================================================
 [1] SDK signIn(), or a 302 from a User-required route
     |
     |  GET /__zeroship/auth/start
     +--------------------------->
                                 [2] resolve route -> project_id
                                     mint __Host-zs_flow
                                       {purpose:'login', nonce, project_id, exp}
                                     302 -> auth/login?p=<project>&n=H(nonce)
     <---------------------------+
     |
     |  top-level nav, or popup / same-site iframe
     |  GET /login?p&n
     +------------------------------------------------------------->
                                                                    [3] resolve the
                                                                        platform session
                                                                        cookie -> ONE
                                                                        validating UPDATE
                                                                        (epoch + lifecycle
                                                                         + expiry)
                                                                    [4] if none: password /
                                                                        magic / federation
                                                                        / TOTP, creating the
                                                                        PLATFORM session
                                                                    [5] grant for (person,
                                                                        Project(p))?
                                                                        no  -> consent
                                                                        yes -> continue
                                                                    [6] INSERT sessions
                                                                          audience=Project(p)
                                                                          parent = platform ses
                                                                          grant_id = grant.id
                                                                          epoch  = 1
                                                                          credential_epoch
                                                                        INSERT one_time_secrets
                                                                          purpose='app_bootstrap'
                                                                          payload={session_id,
                                                                                   H(nonce)}
     <-------------------------------------------------------------+
     |  302 -> app origin /__zeroship/auth/land?code=LC&iss=<auth>
     |
     +--------------------------->
                                 [7] reject a mismatched iss
                                     read __Host-zs_flow, check nonce
                                     POST auth /internal/session/bootstrap
                                       Authorization: svc-assertion+jwt
                                       body {code, nonce}
                                     --------------------------------->
                                                                    [8] verify the service
                                                                        assertion, jti single-use
                                                                        consume the code in ONE
                                                                        conditional statement
                                                                        re-validate the SESSION
                                                                        ROW  (MINT-READS-ROW)
                                                                        mint zss_ secret
                                                                        mint access assertion
                                                                          aud=Project(p)
                                                                          sub=pws_, sid, epoch
                                     <---------------------------------+
                                 [9] Set-Cookie __Host-zs_session (Strict, long)
                                     Set-Cookie __Host-zs_access  (Lax, short)
                                     clear __Host-zs_flow
                                     body {user, scopes, expires_at}
     <---------------------------+
```

The landing code is bound to an HttpOnly, origin-locked nonce cookie the page
cannot read. That is a stronger binding than a `sessionStorage` verifier and it
needs no HMAC key at the edge and no server-side stash.

The top-level redirect leg differs only in [1] and [9]: a full-page 302 rather
than a popup, landing on the original path instead of posting a message. **It
produces the same session row and the same two cookies.** That is the fix for
P4's sharpest instance - today one login shape mints an anchor and the other does
not, and signout keys off the anchor.

Steady state, every dispatched request, with no database read in any process:

```
 BROWSER --__Host-zs_access--> GATEWAY
                                 verify Ed25519 vs OP JWKS; typ, iss, exp, skew
                                 aud == this route's project_id
                                 revocation feed: fresh within the staleness
                                   budget, else DENY; sid not revoked after iat;
                                   epoch >= epoch floor; person status active
                                 route policy: RequiredPrincipal, scopes
                                 CSRF: unsafe method requires exact Origin
                                 strip reserved headers and reserved cookies
                                 sign ZeroShip-User with the GATEWAY PRIVATE key,
                                   bound to the request id and issuance window
                               --> WORKER
                                     verify the envelope under the GATEWAY PUBLIC
                                       key (a key the worker cannot mint with)
                                     verify the transport under the same JWKS
                                     own feed check, independently
                                     policy::enforce BEFORE the isolate lease
                                     --> creator code, env.auth principal
```

Refresh, the only path that touches the database:

```
 BROWSER --__Host-zs_session--> GATEWAY --svc-assertion--> AUTH
                                                             ONE statement:
                                                               rotate the secret
                                                               WHERE hash matches
                                                                 AND revoked_at IS NULL
                                                                 AND idle/absolute live
                                                                 AND users.subject_status
                                                                     = 'active'
                                                                 AND users.credential_epoch
                                                                     = sessions.credential_epoch
                                                                 AND the grant row still
                                                                     exists (FK)
                                                               RETURNING successor, epoch,
                                                                 scopes
                                                             zero rows -> login_required,
                                                               no partial state
                                                             presented-but-already-rotated
                                                               outside the idem window
                                                               -> REVOKE the session
                                                             else mint the assertion
 BROWSER <--both cookies------- GATEWAY <-------------------
```

Note what is absent: no anchor, no marker consulted, no `rotation_started_at`
comparison, no rows-affected gate after the fact. There is one read, it is the
authority, and the mint is downstream of it.

### 4.2 Creator CLI login

RFC 8628 device authorization is kept - it is a genuine two-device handshake and
the standard is the right shape. What is discarded is its OAuth *output*:
redemption yields a session, not a token pair, and `offline_access` ceases to
exist because the session is the refresh credential.

```
CLI                        CONTROL                   AUTH                    BROWSER
=====================================================================================
 [1] GET control/.well-known/oauth-protected-resource
     ------------------->
       {authorization_servers:[auth], audiences:{control, migrate}}
     <-------------------
     REFUSE a non-https issuer unless --insecure-local AND the host is loopback
     (today discovery is scheme-blind)

 [2] POST auth /device/authorization        (rate-limited per client and per peer)
     ---------------------------------->
                                       INSERT sessions
                                         kind='device_pending'
                                         hashed device code, user code, expiry
     <----------------------------------
     print verification_uri_complete and the user code to stderr

 [3]                                                          human opens it
                                                     <---------------------
                                       validate the platform session cookie
                                       CSRF via __Host-zs_flow
                                       eligibility check
                                       APPROVE: kind='cli', person, amr,
                                         auth_time, credential_epoch,
                                         parent = the approving session

 [4] POST auth /device/token   (poll, honours slow_down)
     ---------------------------------->
                                       SELECT ... FOR UPDATE, re-check epoch and
                                         lifecycle, mint zss_ and a platform
                                         access assertion for aud=control
     <----------------------------------
 [5] write a temp file with the mode set AT CREATION, then rename over token.json
       { session_secret, access_token, expires_at, issuer }

 [6] any verb:  Authorization: Bearer <platform assertion>
     delivered in-process, or via curl --config - on STDIN. NEVER on argv.
     ------------------->
                         verify Ed25519 vs OP JWKS
                         typ, iss, exp, aud == the CONTROL audience
                         revocation feed: sid live, feed fresh. NO database read
                         Cedar: token scopes as ceiling, then owner policy

 [7] zeroship migrate asks for the MIGRATE audience.
     A control-audience assertion is REFUSED there.

 [8] zeroship logout
     POST auth /session/revoke {session_secret}     -- best effort
     ---------------------------------->
                                       UPDATE sessions SET revoked_at
                                         WHERE id = <resolved>
                                            OR parent_session_id = <resolved>
                                       + feed entry, same transaction
     DELETE token.json UNCONDITIONALLY - even if the POST failed, even if the
     file does not parse
```

Steps [6] and [8] are where the current CLI is weakest. Today `cmd_logout` in
`crates/zeroship-cli/src/auth.rs` contacts no server and refuses to run at all on
a file it cannot parse, so a copy of the credential file taken beforehand is a
live self-renewing session. Today the bearer is handed to `curl` as an argv
element from several CLI modules, in the same file whose `post_form_with_headers`
already pipes the body through stdin specifically to keep secrets off argv. Today
`whoami` decodes the payload locally without verifying the signature and makes no
network call while unexpired.

Two further changes with no new mechanism. `control` and `migrate-server` get
**distinct audiences**; today they bind the same shared audience key with the
same default, so `aud` separates nothing and any future narrowing that assumed it
did would be wrong. And the scope set a `zeroship login` token can obtain must
cover the verbs the CLI ships: the secret-write and env verbs are advertised in
help text while the issuable-scope ceiling omits them. UNVERIFIED by me that they
return 403 in practice; the experiment is in section 11.

---

## 5. The revocation matrix

Every teardown is one call into one module, which writes the session rows and the
feed entries in **one transaction**:

```rust
pub enum Selector {
    Session(SessionId),
    Tree(SessionId),                                  // and every descendant
    PersonInProject { person: PersonId, project: ProjectId },
    PersonEverywhere(PersonId),
    Project(ProjectId),
}
pub async fn revoke(tx, sel: Selector, cause: Cause)     -> Vec<SessionId>;
pub async fn bump_epoch(tx, sel: Selector, cause: Cause) -> Vec<SessionId>;
```

There is no second spelling of the revocation write, and no path that writes a
session row without its feed entry.

Columns: **A** platform session secret. **B** project session secret.
**C** access assertion, already presented. **D** one-time secrets.
**E** relay alias. **F** service assertions. **G** `wst_` signal tokens.

`IMMEDIATE` means the next mint or the next validating read refuses.
`<= W` means an already-minted assertion stays presentable for at most `W`.

| Teardown event | A | B | C | D | E | F | G |
|---|---|---|---|---|---|---|---|
| Sign out one device (platform origin) | IMMEDIATE | IMMEDIATE (tree) | `<= W` | untouched (G3) | intact | n/a | n/a |
| Sign out one project (app origin) | intact | IMMEDIATE | `<= W` | untouched (G3) | intact | n/a | n/a |
| Sign out everywhere | IMMEDIATE | IMMEDIATE | `<= W` | purpose-scoped purge | intact | n/a | n/a |
| Revoke a project grant | intact | IMMEDIATE (FK cascade) | `<= W` | project-scoped purge | GONE (row deleted) | n/a | n/a |
| Narrow scopes without revoking | intact | intact | `<= W` (epoch bump) | untouched | intact | n/a | n/a |
| Password change or reset | IMMEDIATE (epoch) | IMMEDIATE (epoch) | `<= W` | reset and magic purged | intact | n/a | n/a |
| TOTP enrol or remove | intact | intact | intact | untouched | intact | n/a | n/a (G1) |
| Suspend the person | IMMEDIATE | IMMEDIATE | `<= W` | all purged | suppressed | n/a | n/a |
| Deletion requested | IMMEDIATE | IMMEDIATE | `<= W` | all purged | GONE (grants cascade) | n/a | n/a |
| Reaper anonymise or hard delete | IMMEDIATE | IMMEDIATE | `<= W` | GONE (FK) | GONE (FK) | n/a | n/a |
| CLI logout | IMMEDIATE | intact (separate tree) | `<= W` | untouched | intact | n/a | n/a |
| Session-secret reuse detected | IMMEDIATE | IMMEDIATE | `<= W` | untouched | intact | n/a | n/a |
| Project deleted | intact | IMMEDIATE (FK) | `<= W` | project-scoped purge | GONE (FK) | n/a | n/a |
| App archived or deleted | intact | intact | `<= W` (aud) | untouched | intact | n/a | app-scoped |
| Relay-alias abuse auto-revoke | intact | intact | `<= W` (G2) | untouched | IMMEDIATE | n/a | n/a |
| Service key rotated or withdrawn | n/a | n/a | n/a | n/a | n/a | `<= MAX_ASSERTION_LIFETIME`, then the `kid` leaves the peer JWKS | n/a |
| App rotates its signal key | n/a | n/a | n/a | n/a | n/a | n/a | IMMEDIATE |

Where `W = min(the access-assertion TTL, the revocation feed staleness budget
plus one poll interval)`.

**Every IMMEDIATE above is one statement, and every one is the same statement
family**: a row update keyed on the session tree, or a `DELETE` whose foreign
keys cascade. No teardown enumerates stores. A new credential kind that hangs off
a session row inherits every row of this matrix for free, which is exactly what
the current design cannot do.

### The gaps, named and bounded

**G1 - TOTP enrolment change tears nothing down.** Deliberate, and kept from the
current tree with its own reasoning in `crates/zeroship-auth/src/ui/totp.rs`: the
route already demands the proof a teardown would be forcing. Recorded so the
blank row is not read as an omission.

**G2 - relay-alias abuse suppresses forwarding without ending sessions.** Apps
holding a live assertion keep seeing the alias *string* until the next mint;
forwarding stops immediately because alias resolution is a fresh read at the OP.
No real inbox is ever projected on any path. Both alias readers become one
function - today the gateway's lookup omits the grant-existence half of the auth
store's predicate.

**G3 - one-time secrets are purpose-scoped, not blanket-purged, on device
signout.** Deliberate: signing out one browser must not invalidate a pending
email-verification link the person opens from their phone. A password change
purges the reset and magic purposes, which are the ones an attacker with the old
password could hold.

**G4 - the presentation window `W`.** Deliberate, and it is the design's only
recall latency. Its shape matters more than its size: `W` is bounded above by the
assertion TTL *unconditionally*, because MINT-READS-ROW forbids a re-mint. The
feed only lowers it. If the feed dies entirely, exposure degrades to the TTL and
no further; if the feed goes stale past its budget, the gateway refuses rather
than admits. "Degrade to the TTL, never past it" is exactly what today's anchor
lacks.

**G5 - `wst_` signal tokens survive every person-teardown.** Deliberate: they
are app-issued machine capabilities with no person behind them, and a person's
logout must not break an app's webhook. Their lever is the app's signal key.

**G6 - an in-flight dispatch is not aborted.** A revocation landing after
`policy::enforce` does not kill a running isolate. One request continues to its
wall-clock budget, which is a runtime limit, not a security symbol.

**G7 - intra-worker isolation is not a credential property.** Removing
`control_key` stops a worker from fetching an arbitrary app's environment *by
credential*. It does not stop app A's code from reaching app B's environment if
V8 is escaped, because both are resident in one process. This design does not
claim that property. Naming it is the point: today's app-scoped token reads as
though it were that boundary.

---

## 6. What binds each fence

Every guarantee, its mechanism, and what goes red when the mechanism stops
holding. A guarantee with no red condition is a decoration and does not appear
here. Each gate named as new is a shell script under `tests/` sourcing
`tests/lib/gate_arms.sh`, with its floor declared beside the code that produces
its count, per `tests/gate_arm_census.sh`.

**F1. No credential is minted except from a validating read of a session row.**

*Mechanism:* the assertion signer and the session-secret minter accept a
`ValidatedSession` value. `ValidatedSession` has a private field and no public
constructor; it is producible only by the validating UPDATE in the auth session
store.

```rust
pub struct ValidatedSession(());   // private field: unconstructable elsewhere

pub async fn validate_and_slide(...) -> Result<(SessionRow, ValidatedSession), Denied>;

impl Issuer {
    pub fn sign(&self, claims: Claims, _proof: ValidatedSession) -> String;
}
```

*Red:* nothing "goes red" - **a mint path that skips the read does not compile.**
Removing the parameter fails the build at every call site. A gate arm additionally
refuses a `ValidatedSession` literal constructed outside the session-store module
and refuses a test-only constructor reachable from a non-test path.

*Why this mechanism:* the recurring defect is a check that reads as protection
while nothing reaches it. A witness type cannot be reached around, cannot be
commented out at one call site, and needs no test to stay honest.

**F2. The gateway cannot mint an end-user identity.**

*Mechanism:* key material. The gateway holds no assertion signing key, no
pairwise salt and no session HMAC keyring. Its only private key signs the
`ZeroShip-User` envelope.

*Red:* a custody-manifest gate arm derived from each binary's config contract
(the config is already machine-readable through the config-contract macros):
refuse any secret-classed field appearing in more than one binary, and refuse any
secret-classed field at all on the gateway's config. Re-adding `pairwise_salt` to
the gateway turns it red at the manifest, before a line of logic exists to misuse
it. Note this is stronger than a runtime check precisely because the defect class
is runtime checks nobody reaches.

**F3. The gateway holds no database credential.**

*Mechanism:* the crate declares no database driver, and the corpus creates no
gateway role.

*Red:* a dependency-closure arm in the same gate - the gateway's closure contains
no Postgres driver. This is a link-time property, so "I will just add a small
query" fails at `cargo build`, not at review. A second arm diffs roles created in
`db/migrations-ts/` against a declared service list and refuses a role with no
owning service.

**F4. The worker cannot forge an end-user identity.**

*Mechanism:* asymmetry. `ZeroShip-User` is signed with the gateway's Ed25519
private key and verified in the worker under the gateway's public half, which is
all the worker holds.

*Red:* a worker-side test that mints an envelope using only material available to
the worker's configuration and asserts verification FAILS. Today's equivalent
test cannot exist, because the worker holds the signing key and the assertion
would trivially pass. Delete alongside: the empty-key escape - absent a
configured gateway public key the worker refuses to start.

**F5. The identity signature and the transport credential are independent.**

*Mechanism:* the transport is a service assertion verified under the caller's
published key; the identity is verified under the OP's published key. Neither
private half is in the worker, and neither is the other.

*Red:* a test presenting a valid transport credential together with an identity
envelope signed by the transport key, asserting refusal. When this passes, the
rustdoc on `encode_user_header` becomes accurate and
`crates/zeroship-worker/src/policy.rs` stops resting on a false premise. What the
request-id and issuance-window binding still buys is replay bounding; say only
that.

**F6. Every internal endpoint has a caller check.**

*Mechanism:* service assertions with single-use `jti` on `/internal/*`, on the
gateway-to-worker hop, and on `workflow_advance_internal`.

*Red:* a route census arm enumerating every registered route on every service
from its router builder and requiring each to name a guard. This is the family
instrument, not the instance fix - `workflow_advance_internal` is the instance,
and the tree already has a harness that drives it,
`tests/e2e_gateway_workflow_advance_authz.sh`, whose exploit arm must flip from
advance to refusal.

**F7. The worker cannot read another app's decrypted environment by credential.**

*Mechanism:* control decides. The worker presents a service assertion naming
itself plus an app id; control checks its own placement view - which the worker
cannot write - that the app is assigned to that node. The narrowing is computed
by the party that is not being narrowed.

*Red:* an integration arm in which node N presents a valid assertion for an app
assigned elsewhere and receives a refusal. Impossible to write today, because the
worker holds `control_key` and every env request succeeds. Enumerate both
enforcement points before treating one mutation as refuting the guard: the
placement equality and the `jti` single-use. Bounded honestly by G7.

**F8. A revoked session cannot be presented after `W`, and cannot be re-minted at
all.**

*Mechanism:* two, independent. The assertion `exp` bounds presentation
unconditionally; the feed lowers it, and the staleness budget makes a dead feed
deny rather than admit.

*Red:* paired e2e arms differing in one variable. Revoke plus fresh feed must
deny. Revoke plus a feed frozen past the budget must deny. No revoke plus fresh
feed must allow. Neutralising the freshness check must flip the second.

**F9. Revoking a grant ends every session it authorised.**

*Mechanism:* `sessions.grant_id` is a foreign key with `ON DELETE CASCADE` and
the revoke is a `DELETE`. PostgreSQL enforces it; no application code enumerates.

*Red:* an arm that revokes a grant and then attempts a refresh with the project
session secret, asserting refusal. Today that sequence succeeds.

**F10. A password change, suspension or deletion request ends every session
without enumerating them.**

*Mechanism:* `credential_epoch`, joined in the same UPDATE that slides the idle
window. This is `crates/zeroship-auth/src/store/sessions.rs` generalised to all
audiences.

*Red:* an arm per lifecycle transition that bumps the epoch and asserts a refresh
on an older session is refused. Mutation: delete the equality predicate and the
arm must fail.

**F11. `subject_status` has a production writer for every variant.**

*Mechanism:* a control-plane suspend and reinstate endpoint that stamps the
status and bumps the credential epoch in one statement.

*Red:* a gate arm ruling on the variant set - floor declared beside the enum -
requiring at least one non-test writer per variant. This is the family fix for
`disabled_at`: today the column is read across the auth store, the identity
paths, the OIDC endpoints and control's registry, and written only from tests.

**F12. Exactly one writer of `sessions.revoked_at`.**

*Mechanism:* the `revoke` / `bump_epoch` module.

*Red:* a gate arm enumerating statements naming `sessions.revoked_at` outside
that module. This is what makes P4 structurally unrepeatable rather than fixed
once.

**F13. The pairwise subject cannot silently diverge across processes.**

*Mechanism:* there is one process. `zeroship-auth` is the sole holder of the salt
and the sole caller of the derivation.

*Red:* a dependency-census arm refusing `derive_pairwise` or the salt config key
outside `zeroship-auth`. This beats a fingerprint check because the divergence
failure reads as success, and a check that must be remembered is the wrong
instrument for a failure whose signature is a clean reading.

**F14. Cross-file invariants have cross-file instruments, or one definition.**

*Mechanism:* one identity upsert, one alias predicate, one scope vocabulary, no
duplicated callback path sets - all achieved by deletion, not by a second copy.

*Red:* a gate arm refusing a `const` SQL string in one crate that is a substring
of a `const` SQL string in another. A test asserting a literal against its own
copy is refused in review. Today the two identity upserts are byte-identical in
two crates, each pinned by a test asserting a substring of itself.

**F15. Every forced-RLS policy in the corpus binds something.**

*Mechanism:* a gate arm over `db/migrations-ts/`: for every table with forced
RLS, require at least one role that lacks BYPASSRLS at its final state across the
whole corpus AND holds a grant on that table.

*Red:* the new `rls_binding_gate.sh` under `tests/`. It goes red today on the
spend and secrets family, and it would have gone red the day
`db/migrations-ts/20260818000200_worker_database_authority.ts` gave the worker
BYPASSRLS - a one-line attribute change whose blast radius nothing currently
reports. **This is the highest-leverage new instrument in the design**, because it
mechanises the whole defect class for a family at once and counts nothing, so it
cannot go stale.

**F16. The one auth path taking an attacker-supplied tenant key is
database-fenced.**

*Mechanism:* split the OP's database identity. `zeroship_auth_rt` -
**no BYPASSRLS** - serves the per-request path: the token endpoint, the identity
write, the alias read, bound by tenant policies keyed on the transaction-local
GUC. `zeroship_auth_admin` - BYPASSRLS - serves cron, account lifecycle and
cross-audience listing. Today
`crates/zeroship-auth/src/oidc/authorization_code.rs` sets the GUC and the role
bypasses the policy it feeds, so the ceremony is inert. **Do not delete the
`set_config`. Delete the BYPASSRLS.**

*Red:* a live arm where `zeroship_auth_rt` sets the GUC to one audience and
attempts a write for another; PostgreSQL refuses. Mutation: grant that role
BYPASSRLS and the arm goes green, which proves the fence is the role attribute
and not the SQL.

*Honest caveat, stated in the design rather than discovered later:* two roles in
one process is defence-in-depth against a route-confusion bug, **not** a boundary
against a compromised OP. That is precisely the claim the current tree makes
dishonestly.

**F17. Retention outlives the presentation window.**

*Mechanism:* the feed retention is a `const` expression derived from the
assertion TTLs, the staleness budget and the clock-skew tolerance, in one module
in `zeroship-core`, with a compile-time assertion. Raising an assertion TTL
without the derivation fails to compile. Note what is absent from the right-hand
side: the session secret's lifetimes, because MINT-READS-ROW means a swept feed
entry cannot resurrect a revoked session.

*Red:* a gate arm that recomputes the ordering from the definitions rather than
asserting a value. Task #209 closes structurally: there is nothing left to keep
ordered by hand.

**F18. Declared route scopes are never silently inert.**

*Mechanism:* `crates/zeroship-bundle/src/manifest.rs` refuses
`RequiredPrincipal::Anonymous` carrying non-empty `required_scopes` at build
time.

*Red:* a unit test asserting the refusal plus the negative control. Today the
combination validates and `crates/zeroship-bundle/src/compiled.rs` admits before
the scope gate.

**F19. App code cannot present a platform cookie or forge an identity header.**

*Mechanism:* one reserved-name list consumed by both the header strip and a new
cookie strip in `collect_forwarded_headers`.

*Red:* a gate arm iterating the reserved list, dispatching with each name set,
asserting absence from the worker-visible envelope. Adding a name without wiring
the strip fails.

**F20. Postgres role passwords are not repository literals.**

*Mechanism:* the corpus creates login roles without passwords; `zeroship dev
init` sets dev passwords and operators set production ones.

*Red:* a gate arm over `db/migrations-ts/` refusing any password literal equal to
its role name. Today every login role in
`db/migrations-ts/20260702000100_schema_roles_extensions.ts` is created exactly
that way, and `ifNotExists` means the exposure is at first provisioning, which is
every fresh cluster.

**Fences deliberately NOT claimed.** Intra-worker environment isolation (G7).
Correlation resistance against a party that observes both the auth origin and an
app origin - the platform can always correlate; the guarantee is that *projects*
cannot. Protection against a compromised `zeroship-auth` (section 9).

---

## 7. What this deletes

By path and symbol. Pre-launch, so each is a deletion, not a deprecation.

### 7.1 Mechanisms verified to bind nothing

- `crates/zeroship-gateway/src/auth.rs` - the whole module (`check_api_key`).
  Verified: no caller, production or test.
- `zeroship.apps.api_key` and `apps.api_key_hash` in
  `db/migrations-ts/20260702000200_control_tables.ts`; `api_key_hash` on
  `RouteEntry` in `crates/zeroship-core/src/types.rs`; the mint in
  `crates/zeroship-control/src/registry.rs`; the create-response injection in
  `crates/zeroship-control/src/api.rs`; `hash_api_key` and `validate_api_key` in
  `crates/zeroship-core/src/auth/mod.rs`. A plaintext secret column stored beside
  its own hash, gating nothing.
- `crates/zeroship-control/src/identity_bridge.rs` - the whole module.
  `provision_or_link` has only test callers and a header claiming it sits on the
  bearer read path.
- `crates/zeroship-gateway/src/sessions.rs` - the whole module, including
  `validate` (no production caller) and `IDLE_MINUTES` / `ABSOLUTE_HOURS`.
  Note this is the GATEWAY's module; the auth store's same-named function has
  real production callers and is kept.
- `zeroship.jwk_key_state` in `db/migrations-ts/20260702000300_auth_oauth_tables.ts`
  - no reader, no writer.
- `post_logout_redirect_uris` in `crates/zeroship-control/src/app_oauth_client.rs`
  - a public function with no production caller and no column to write into.

### 7.2 The OAuth apparatus between two of our own processes

The largest deletion, and P7 is why. Client authentication for the one
server-to-server redemption that remains is the gateway node's service assertion:
asymmetric, per-node, enrolled, replay-checked, and strictly stronger than a
derived shared secret.

- `crates/zeroship-gateway/src/oidc_rp.rs` - entire, including `OidcRp`, `Stash`,
  `build_authorize_redirect`, `finish_callback`, `exchange_code_public`,
  `refresh_token_public`, `revoke_token_public`, `encode_user_header` and the
  registered callback path set.
- `crates/zeroship-gateway/src/anchors.rs` - entire, and
  `zeroship.app_session_anchors`.
- `crates/zeroship-gateway/src/identities.rs` - entire, and with it the second
  byte-identical copy of the identity upsert.
- `crates/zeroship-gateway/src/backchannel_logout.rs`,
  `crates/zeroship-auth/src/oidc/backchannel_logout.rs`,
  `crates/zeroship-core/src/logout_token.rs`, and `zeroship.oidc_session_clients`.
  Back-channel logout exists to push a logout to relying parties holding their
  own session state; in this model no relying party holds session state, and the
  fire-and-forget hop it deletes has no retry, no queue and no reconciliation.
- `crates/zeroship-gateway/src/rls.rs`, `crates/zeroship-gateway/src/db.rs`, and
  the gateway's database settings.
- `crates/zeroship-gateway/src/auth_token.rs`'s minting half, and the `Issuer`
  half of `crates/zeroship-gateway/src/session_token.rs`. The verifier shape moves
  into `zeroship-core` so gateway, worker and control verify with one
  implementation.
- `crates/zeroship-gateway/src/router/dispatch.rs`'s `start_oidc_redirect` and
  `handle_auth_callback`.
- In auth: `crates/zeroship-auth/src/oidc/authorization_code.rs` as an OAuth
  grant, `crates/zeroship-auth/src/oidc/refresh.rs` as a grant (its rotation
  algorithm moves onto the session row unchanged), and the OP metadata
  advertisements with nothing behind them - `end_session_endpoint`, and the token
  and authorization signing-alg lists that `authenticate_client` does not
  implement. `claims_supported`'s `auth_time` / `amr` / `acr` entries become
  TRUE rather than being removed: the session row carries them and the assertion
  projects them.
- `crates/zeroship-core/src/pkce.rs`, `crates/zeroship-core/src/oidc_verify.rs`,
  `sdks/auth/src/internal/pkce.ts` and the verifier storage in
  `sdks/auth/src/internal/transaction.ts`.
- `crates/zeroship-control/src/app_oauth_client.rs`,
  `crates/zeroship-control/src/oauth_clients.rs` (the per-app half),
  `crates/zeroship-control/src/oauth_grants_handlers.rs` and
  `crates/zeroship-control/src/device_handlers.rs`.
- `derive_broker_secret` in `crates/zeroship-core/src/auth/mod.rs`,
  `verify_broker_secret` in `crates/zeroship-auth/src/oidc/issuer.rs`, and the
  broker secret settings on both sides.
- Tables: `oauth_clients`, `oauth_grants`, `oauth_authorization_codes`,
  `oauth_refresh_tokens`, `app_oauth_clients`, `app_user_identities`,
  `idp_sessions`, `gateway_sessions`, `identity_links`, `device_grants`, and the
  brokered-requires-secret-basic CHECK.

**The capability this removes, stated plainly:** a creator app can no longer act
as an OIDC provider to a third-party relying party. Nothing depends on that
today. If it becomes a product it is a first-class feature with real registration
and real secrets, not an auto-provisioned side effect of `zeroship deploy`. See
open decision 5.

### 7.3 The marker primitive and its unlinked constants

- `crates/zeroship-authz/src/wrapper_revocation.rs` - entire, including
  `WRAPPER_REVOCATION_RETENTION_HOURS` and `REVOCATION_CACHE_TTL_SECS`.
- `zeroship.token_revocations`, its grants
  (`db/migrations-ts/20260811000000_auth_token_revocations_delete.ts`,
  `db/migrations-ts/20260812000000_gateway_token_revocations_update.ts`), and the
  bare `INTERVAL` literal in `crates/zeroship-control/src/registry.rs`.
- The hand-spelled marker upserts in `crates/zeroship-auth/src/store/users.rs`,
  `crates/zeroship-auth/src/identity/password_reset.rs`,
  `crates/zeroship-auth/src/oidc/refresh.rs` and
  `crates/zeroship-control/src/oauth_grants_handlers.rs`.

### 7.4 Shared symmetric roots

- `control_key`: `check_auth` in `crates/zeroship-control/src/internal.rs`, the
  worker's use, the gateway setting, and the environment twin.
- `worker_key`: `check_worker_auth` and `verified_user_json` in
  `crates/zeroship-worker/src/handler.rs`, the settings on gateway, worker and
  control, and the environment twin.
- `derive_app_scoped_control_token` and `verify_app_scoped_control_token` in
  `crates/zeroship-core/src/auth/mod.rs`, and the bespoke HMAC arm of
  `check_app_scoped_auth` in
  `crates/zeroship-control/src/workflow_instance_api.rs`.
- The unauthenticated arm and its `TODO(DW-signed-transport)` in
  `crates/zeroship-gateway/src/router/dispatch.rs`, and
  `workflow_advance_unsigned` on the worker.
- The `ZeroShip-User` HMAC family in `crates/zeroship-core/src/auth/mod.rs`,
  replaced by Ed25519. `derive_pairwise`, `derive_pairwise_salt`,
  `constant_time_eq` and `extract_bearer` are KEPT.

### 7.5 CLI

- The Supabase arm in `crates/zeroship-cli/src/auth.rs` and the corresponding
  provider variant in the verifier: `cmd_login` routes both variants into one
  device flow that hardcodes the platform provider, so the arm is reachable only
  from a hand-edited credential file.
- `userinfo_from_platform_token`, replaced by a verified call.
- The platform CLI client id, its registered and issuable scope lists, the
  `offline_access` scope constant and the hand-written standard-scope filter in
  `crates/zeroship-core/src/auth_provider/platform.rs`. The scope vocabulary is
  derived from the scope enum; the platform grant row is the ceiling. That
  filter's `offline_access` entry is load-bearing platform-wide today, because
  the authz scope parser errors on any token outside its closed vocabulary.
- `materialize_default_grants` in `crates/zeroship-authn/src/platform_cli.rs`,
  which folds into seeding the platform grant row.
- Every bearer passed to `Command::new("curl")` as an argv element in
  `crates/zeroship-cli/src/main.rs`, `crates/zeroship-cli/src/migrate.rs`,
  `crates/zeroship-cli/src/secrets.rs` and `crates/zeroship-cli/src/auth.rs`.

### 7.6 Merges within the surviving surface

- Five signed or random browser cookies become one `__Host-zs_flow`: the CSRF
  double-submit in `crates/zeroship-auth/src/csrf.rs`, the magic-link CSRF
  cookie, the TOTP challenge cookie in
  `crates/zeroship-auth/src/sessions/totp_challenge.rs`, and both provider
  stashes in `crates/zeroship-auth/src/ui/oauth_stash.rs`. Keep the stash's
  shape - it embeds `iat` and `exp` and enforces them server-side, where the
  gateway's stash carries no time claim at all and delegates its lifetime to the
  user agent.
- The emailed one-time token modules (`crates/zeroship-auth/src/identity/magic_link.rs`,
  `crates/zeroship-auth/src/identity/verification.rs`,
  `crates/zeroship-auth/src/identity/password_reset.rs`, the account-link token in
  `crates/zeroship-auth/src/identity/linker.rs`, the authorization code and the
  device code) become one purpose-tagged table with one consume statement, so the
  single-statement consume race is fixed once rather than once per module. The
  purpose is a typed enum, not a string.
- `crates/zeroship-auth/src/ui/consent.rs`'s dedicated per-consent Postgres
  connection, when the shared client and the bounded refresh pool are already
  registered on that handler's state.
- The user projection in `crates/zeroship-gateway/src/auth_token.rs` gains
  `scopes`. They are already in the signed cookie and in the identity header, so
  projecting them leaks no capability, and it makes `AuthClient.hasScope` capable
  of returning true - today it is constant-false, because
  `sdks/auth/src/internal/transport.ts` reads a key the projection does not emit.
  The bearer arm and the cookie arm become one projection function, so they
  cannot return different user objects for the same human.

### 7.7 Migrations

- Every login role's password literal in
  `db/migrations-ts/20260702000100_schema_roles_extensions.ts`, and the gateway
  role itself.
- The BYPASSRLS attribute on the worker role in
  `db/migrations-ts/20260818000200_worker_database_authority.ts`. Its REPLICATION
  attribute is a data-plane question this design does not decide (open decision
  6).
- In `db/migrations-ts/20260702000800_policies_rls.ts`: the forced-RLS policies
  on the spend and secrets family, which bind no role; and the policies on the
  session, anchor and identity tables, whose tables cease to exist or become
  single-writer at auth. **Read section 11 before touching these** - two of them
  genuinely bind today.
- The control-plane grants on `zeroship.device_grants` in
  `db/migrations-ts/20260702000900_grants.ts`; control's device flow is already
  gone and the grant is the residue with teeth.

### 7.8 Wire what is built and unwired

- `crates/zeroship-core/src/service_assertion.rs`,
  `crates/zeroship-core/src/service_identity.rs` and
  `crates/zeroship-authn/src/service_replay.rs` are NOT deleted. They are wired.
  Do not invent a fourth HMAC scheme. **Their backing table is already
  provisioned** - see section 11.
- `subject_status` gets a production writer.
- The manifest refusal for `Anonymous` carrying scopes gets built.

---

## 8. The sequence

Each step is independently landable and ordered by value delivered per unit of
disruption. Each names the RED TEST that fails before and passes after. A
**premise** is a test that must be green BEFORE the step begins.

**Step 0 (premise for everything after step 3).** Land `rls_binding_gate.sh`
under `tests/` and run it as a measurement. It goes red today. Do not delete a
single RLS policy before this gate exists and its verdict per table is recorded.
*Red test:* the gate itself, red on the spend and secrets family and green on the
session, anchor and identity tables - which is the evidence that those three are
live fences and the others are not.

**Step 1. Delete what binds nothing.** `check_api_key` and the api-key columns,
`identity_bridge`, the gateway's `sessions::validate`, `jwk_key_state`, the
orphan public function in the app OAuth client module.
*Red test:* a control-plane test asserting the create-app response carries no
api-key field, plus a gate arm refusing a plaintext secret column stored beside
its own hash. No premise.

**Step 2. Wire service assertions on every internal edge.** `/internal/*` at
control, the gateway-to-worker hop, and `workflow_advance_internal`. Land the
route census arm (F6) in the same change.
*Premise:* the replay store's backing table is provisioned - it is; see section
11 - and the assertion round-trip test in `crates/zeroship-core/tests/` is green.
*Red test:* `tests/e2e_gateway_workflow_advance_authz.sh`'s exploit arm flips
from advance to refusal, and a control test refusing a shared-secret bearer on
the environment endpoint.

**Step 3. Make the identity envelope asymmetric.** The gateway signs with its own
Ed25519 private key; the worker verifies under the public half; the empty-key
escape is deleted and a missing public key refuses startup.
*Premise:* step 2, for peer key distribution.
*Red test:* F4's worker-side forgery test, which cannot even be written today.
This step alone makes the `encode_user_header` rustdoc true.

**Step 4. Control decides the environment fetch.** The worker presents a service
assertion; control checks its own placement view. `control_key` and the app-scoped
derivation are deleted.
*Premise:* steps 2 and 3.
*Red test:* F7's cross-node refusal arm.

**Step 5. The session object, and MINT-READS-ROW.** Create `zeroship.sessions`
and `zeroship.grants`, move the refresh-family rotation algorithm onto the session
row unchanged, introduce `ValidatedSession`, and make auth the sole minter.
*Premise:* step 0's verdict is recorded, because this step is what makes two of
the three live RLS fences unnecessary rather than merely removed.
*Red test:* revoke a session, then attempt a mint, and assert refusal; mutation -
delete the `revoked_at IS NULL` predicate and the test must fail. Plus the
compile-level check: removing the `ValidatedSession` parameter fails the build.

**Step 6. Move minting off the edge and delete the RP path.** The gateway becomes
a verifier and a relay; the anchor, the stash, the RP module and the gateway's
database credential go.
*Premise:* step 5, and the revocation feed with its fail-closed staleness gate
must land in the SAME change - see section 9.
*Red test:* an interactive redirect login followed by signout, asserting a
subsequent request is refused. Today that is a no-op because the anchor is
absent. Second arm: the dependency-closure gate (F3) refuses a database driver in
the gateway's closure.

**Step 7. Grants as rows, revocation as DELETE.** `token_revocations` and the
whole marker family go; the derived retention constant lands.
*Premise:* step 5.
*Red test:* F9's grant-revoke-then-refresh arm, which today succeeds; plus F17's
recomputation arm. This closes task #209 by deletion.

**Step 8. `subject_status`, its writer, and its variant gate.**
*Premise:* step 5, for the epoch join.
*Red test:* F11's variant-writer arm, red today; plus a suspend-then-refresh
refusal arm.

**Step 9. Audience becomes the project.** Task #72.
*Premise:* step 5 and step 7, because the sector is stored on the grant row.
*Red test:* a pair differing in one variable - two apps of one project produce
equal subjects, two projects produce unequal ones. Moving the sector back to the
app apex fails both halves.

**Step 10. The auth role split and the RLS cleanup.** `zeroship_auth_rt` without
BYPASSRLS on the per-request path, `zeroship_auth_admin` for lifecycle; delete
the policies the gate proves bind nothing.
*Premise:* step 0 (the gate) and step 6 (the gateway role is gone).
*Red test:* F16's live cross-audience write refusal, with the BYPASSRLS mutation
as its control.

**Step 11. The CLI.** Server-side logout, unconditional file delete, bearer off
argv, verified `whoami`, distinct control and migrate audiences, the scope
ceiling widened to cover the shipped verbs, the Supabase arm deleted.
*Premise:* step 5 (there is a session to revoke).
*Red test:* copy the credential file, run `zeroship logout`, and assert the copy
is refused. Today it is accepted for the family's remaining life.

**Step 12. The cookie merge, the one-time-secret merge, the manifest refusal, the
reserved-cookie strip, the role-password removal.** Independent cleanups, each
with its own arm from section 6 (F14, F18, F19, F20).
*Premise:* none, except that F18 must land with `zeroship-bundle`'s own tests.

---

## 9. Risks and what gets worse

**The trust root is concentrated, not eliminated.** `zeroship-auth` becomes the
whole identity authority, and it is still reachable from the internet: a public
login form, a public device-approval page, public federation callbacks. A remote
code execution there yields the pairwise salt (permanent and unrotatable by
construction), the assertion signing key, the session-secret keyring, the TOTP
at-rest key, and write access to users, sessions and grants. That is a *larger*
concentration than the gateway holds today, in a smaller process.

I claim the trade is correct and I want to be precise that the argument is
qualitative. The gateway's attack surface is proxying arbitrary creator traffic,
parsing arbitrary Host headers, resolving arbitrary paths, and sitting one
process boundary from creator code. Auth's is form posts and OAuth callbacks with
no routing, no proxying and no bundle execution. Smaller surface, larger prize.
**That is a bet, not a proof**, and if it is wrong the failure is worse than
today's.

The successor step is named so it is not rediscovered: a signing service only
auth can reach, minting from a `ValidatedSession` with a keep-out interface.
Sharpened by an observation from the investigation - **the minter also owns the
revocation store, so a compromised minter can erase the record of what it
minted.** The keep-out interface must therefore be mint-with-witness only, no key
export AND no revocation write. The design is shaped so this is one seam behind
one type, not a rewrite.

**Fail-closed revocation converts an auth outage into an authentication
outage.** Once a verifier's feed is older than the staleness budget it refuses
every authenticated route. A partition between a zone and auth takes every
`RequiredPrincipal::User` route in that zone to 503 after that interval;
anonymous routes keep serving. There is no configuration in which the revocation
bound and the outage tolerance are both large, because after the feed budget the
next bound is the assertion TTL and that is also the mint-load knob. This is
stated rather than hidden behind a cached fallback, because a cached fallback is
precisely how the tree ended up with a marker that expires before the capability
it revokes.

**Step 6 is the one place this trades fail-closed for fail-open if it is split.**
Today the gateway can fall back to a live database read on a revocation cache
miss. If the gateway's database credential is removed BEFORE the feed's
fail-closed staleness gate is in place, the window between those two changes is a
period in which a stale or absent revocation view admits rather than denies.
**The feed, its cursor and its staleness refusal must land in the same change
that removes the database credential.** Do not sequence them apart for
reviewability.

**Recency regresses relative to today.** The current polled path does a live
database read on a cache miss, so worst-case recency is a cache TTL. Buying "the
gateway has no database" costs that. The compensation is that exposure is now
bounded above by the assertion TTL unconditionally, which today it is not.

**Availability during minting.** The mint path is a database write at auth, and
its rate scales with active sessions divided by the assertion TTL. A partitioned
zone degrades from serving, to serving until assertions expire, to hard down.
That is the honest cost of moving minting off the edge.

**Concentrating the one-time-secret modules concentrates a correctness
property.** "A reset token cannot be redeemed as a magic link" becomes a single
`WHERE purpose = $2`. That is the right shape and it is also one place where a
bug affects every flow at once. The predicate is in the same statement as the
consume so there is no check-then-act window; the residual risk is a wrong
purpose at a call site, which is why the purpose is a typed enum and not a
string.

**Deleting back-channel logout is correct only while no relying party holds
session state.** True for zeroship-hosted apps by construction. It stops being
true the moment the platform federates outward. If that is on the roadmap the
decision should be taken now, not after this lands (open decision 5).

**Two roles in one process is not a boundary.** F16 is defence-in-depth against a
route-confusion bug in the OP. It is written here with that caveat attached so
nobody repeats it without one.

---

## 10. Open decisions for the operator

1. **Do creators and app end users share one person namespace, and should a
   suspension in one role kill the other?** The design assumes one namespace,
   which the tree appears to have already, but `subject_status` is person-scoped,
   so an abuse suspension against an app end user would also stop that human
   deploying. The alternative is status per (person, audience), which costs the
   simplicity of one predicate. UNVERIFIED that the namespaces are shared today;
   the experiment is in section 11.

2. **Does the assertion signing key stay in the auth process, or move behind an
   external signer now?** Section 9 states the bet. The external signer buys
   "the salt and the signing key are unreachable from any process handling an
   internet request" and costs a process, a deployment and a failure mode. If the
   platform will hold regulated data, this is decided by that rather than by
   engineering taste.

3. **Is `Project` the right sector unit, or does an organization container sit
   above it?** Task #82 is deferred. This design commits to project-scoped
   subjects; a later organization layer would be a new audience variant rather
   than a re-derivation, but the choice affects whether subjects are stable
   across a project reparent.

4. **The access-assertion TTL and the feed staleness budget.** The first is the
   mint-load and partition-tolerance knob; the second is the recall bound. They
   are separate symbols in this design deliberately, but both need values, and
   both belong to operations rather than to this document.

5. **Will a creator app ever need to be an OIDC provider to a third-party relying
   party?** If yes, per-app clients and back-channel logout should be redesigned
   as a first-class feature now rather than deleted and re-added. If no, section
   7.2 stands.

6. **Does `zeroship_worker` keep REPLICATION?** This design removes BYPASSRLS
   from that role. REPLICATION is on the same migration and is a data-plane
   question tied to CDC ownership, which is out of scope here.

7. **Does the platform session cookie remain readable across the same-site
   iframe leg the SDK uses?** The design assumes it does (same registrable
   domain, hence same-site). UNVERIFIED; the experiment is in section 11. If it
   does not, step 6's popup and top-level shapes are the only two entries and the
   iframe leg is dropped.

---

## 11. Corrections

Claims made during this investigation that turned out wrong, and what is true.
Each was established by reading the working tree today. Nothing was executed.

**C1. The service-assertion replay table IS provisioned. Three separate
write-ups said it exists only as a DDL string inside a test, and one of them
marked that claim as verified.** It is created, indexed and granted by
`db/migrations-ts/20260816000100_service_assertion_replay.ts`, which establishes
the `service_authn` schema, the `service_assertion_replay` table and its expiry
index, and grants select, insert, update and delete on it to the control,
gateway, worker and auth roles, plus schema usage, plus an explicit revoke of
CREATE. The test file's own header says production establishes the table there.
**The work item is "wire the minter and the verifier", not "provision the
table".** A false verification badge is worse than an unmarked error, and this
one nearly produced a migration that already exists.

**C2. Forced RLS is inert on some tables and LIVE on others; an earlier reading
called it uniformly inert.** In
`db/migrations-ts/20260702000800_policies_rls.ts` the tenant policies on the
spend and secrets family bind nothing, because those tables are reachable only by
a role created with `bypassRls` in
`db/migrations-ts/20260702000100_schema_roles_extensions.ts`. But the policies on
`app_session_anchors`, `gateway_sessions` and `app_user_identities` DO bind: the
gateway role is created WITHOUT `bypassRls`, holds grants on all three in
`db/migrations-ts/20260702000900_grants.ts`, and sets the matching
transaction-local GUCs in `crates/zeroship-gateway/src/rls.rs`. **Those are real
fences.** This design removes them, and the removal is legitimate only by the
argument made table by table: two of the three tables cease to exist, and the
third becomes single-writer at auth with no second tenant to isolate. That
argument has to be made, not assumed - which is why step 0 exists.

**C3. The revocation sweep is not missing a DELETE grant.** An earlier note said
it would fail for lack of one;
`db/migrations-ts/20260811000000_auth_token_revocations_delete.ts` grants it. The
base grants file omits it, which is why the claim looked right.

**C4. `sessions::validate` is two different functions and only one is
unwired.** The GATEWAY's `crates/zeroship-gateway/src/sessions.rs` version has no
production caller, and two doc comments in
`crates/zeroship-gateway/src/auth_token.rs` and
`crates/zeroship-gateway/src/router/auth.rs` say the request path deliberately
does not use it. The AUTH store's same-named function in
`crates/zeroship-auth/src/store/sessions.rs` has several production callers under
`crates/zeroship-auth/src/ui/`. Earlier write-ups blurred them, which would have
deleted a live function.

**C5. The app-scoped control token has more than one production caller, and both
are inside the worker.** `crates/zeroship-worker/src/handler.rs` and
`crates/zeroship-plugin-workflow/src/client.rs`. The conclusion is unchanged and
slightly stronger: the plugin runs inside the worker process, so both derivations
are computed from a root the deriving process holds.

**C6. The workflow-advance reachability question already has an instrument.**
Earlier write-ups listed "is this edge reachable from the public internet" as
unverified with a new experiment attached. `tests/e2e_gateway_workflow_advance_authz.sh`
already boots real binaries and drives exactly that claim, including a phase that
puts the repository's Caddy rule in front of the gateway. **Check the instrument
built to answer the question before building another one.** UNVERIFIED by me: I
read the harness, I did not run it.

**C7. The retention ordering is inverted, not merely unlinked.** Task #209 says
the push window and the sweep retention "agree by coincidence". They do, and the
finding is worse: both are shorter than `ANCHOR_ABS_DAYS`, so for the one
teardown that writes a marker without deleting the anchor, the marker is swept
while the capability it was written against is still alive.

**Still UNVERIFIED, with the experiment for each.**

- *Whether the secret-write and env CLI verbs return 403 under a `zeroship login`
  token.* Derived from three reads - the CLI's requested scope, the required
  action in `crates/zeroship-control/src/env_handlers.rs`, and the ceiling
  evaluation in `crates/zeroship-authz/src/eval.rs` - not observed. Experiment:
  against a live control plane run a secret list (expect allow) and a secret set
  (expect deny) and read both statuses. One variable apart, so it is an oracle
  rather than an anecdote.
- *Whether creators and app end users share one person namespace.* Control holds
  only select on the users table and reaches principals through the link and
  grant tables, which is consistent with one table for both, but nothing states
  it. Experiment: check whether a principal id in the identity-link table can
  also appear as the global user id on an app identity row. This decides open
  decision 1.
- *Whether the browser attaches the platform session cookie for the SDK's
  same-site iframe leg with third-party cookies blocked.* Same registrable domain
  means same-site, so Lax should apply, but I did not exercise it. Experiment: an
  arm in `tests/e2e_auth_ui.sh` driving the iframe leg in real Chromium with
  third-party cookies blocked. This decides open decision 7.
- *Whether any deployment outside this repository rate-limits the device
  authorization endpoint.* Established by enumeration over the auth crate and
  `deploy/ops/Caddyfile`; a production edge elsewhere could impose one.
