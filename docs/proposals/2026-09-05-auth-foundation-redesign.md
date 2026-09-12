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
primitive with no object it revokes, and the trust root sits inside the processes
that face the internet and run creator code.

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
and `crates/zeroship-plugin-workflow/src/client.rs`, each inside the worker.

**P3. There is no service identity, so every internal hop borrows a shared
symmetric root, and that root authenticates the hop as well as signing
identity.** In
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

**Read the uniformity as the defect, not as the remedy.** What is wrong is that
every hop borrows the SAME root, not that every hop needs the same credential
profile. Those are different claims, and only the first is established by the
evidence above. Carried into section 8 unqualified, this paragraph reads as
licence for one credential shape everywhere, and section 8 records what that
reading produced.

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
it is absent. One verb, divergent entry paths, divergent meanings, and no place in
the model where "a session" is a thing every path produces.

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
it is longer than either. For a teardown that writes a marker without deleting the
anchor, the marker is swept while the capability it was written against is still
alive. Task #209 as worded understates its own finding.

**P7. OAuth client authentication is a distinction the model carries and nothing
enforces.** Per-app clients are brokered, and the gateway derives the secret for
any app's client from a master it holds. The OP cannot attribute a token exchange
to an app more strongly than it can attribute it to the gateway, so per-app
clients are a naming scheme, not an isolation boundary, and every control built
on client identity inherits that ceiling silently.

**P8. Identity is a derivation replicated across processes with divergent supply
shapes and no agreement check.** `derive_pairwise` is called independently in
auth, the gateway and control. The salt reaches auth as raw file bytes and
reaches the others through the config `Secret` layer, whose file loader strips a
trailing newline - a hazard stated verbatim in
`crates/zeroship-core/src/config/file.rs`. A divergent salt does not error; it
produces non-colliding subjects, so revocations write markers nobody presents and
the system reports "nothing to revoke" rather than "salt mismatch". The
instrument reads clean precisely because the thing it measures is broken.

**P9. The sector is too fine, and suspension has neither a scope nor a writer.**
Too fine: the sector is the app apex, so no cross-app subject exists to revoke and
apps sharing a database cannot agree who a user is (task #72).

The single `users` namespace serving creators and app end users is **not** the
defect - decision D-D settles that one account per human is correct, and an
earlier draft of this paragraph filed the shared namespace as a granularity error.
Findings that rode on that clause survive it. First, the model carries no SCOPE
for a suspension: with one namespace and no per-audience handle, the only lever
abuse response has is person-wide and destructive. What that scope should be is
settled as decision D-E - per project - and section 3.2 states where it hangs.
Second, the lever is unwired -
`disabled_at`, the one column meaning "suspended, not deleted", is read across the
auth store, the identity paths, the OIDC endpoints, `zeroship-authn` and
`crates/zeroship-control/src/registry.rs`, and written only from test targets. A
column that is read everywhere and never written means every reader believes a
dead branch is live.

**P10. One fact, hand-copied spellings, pinned by tests that assert a literal
against themselves.** The identity upsert is byte-identical in
`crates/zeroship-gateway/src/identities.rs` and the auth store; the registered
callback path sets exist in the gateway and in
`crates/zeroship-control/src/app_oauth_client.rs`; the marker upsert is spelled
by hand in auth, control and the gateway's shared helper. A test asserting a
literal against its own copy measures nothing about a cross-file invariant, so
the instrument is green in exactly the state the invariant is violated.

**P11. Stores and columns shaped like enforcement that enforce nothing.**
`gateway_sessions.revoked_at` is written by back-channel logout and read only by
`crates/zeroship-gateway/src/sessions.rs`'s `validate`, which has no production
caller, and doc comments elsewhere say the request path deliberately does not use
it. `check_api_key` in `crates/zeroship-gateway/src/auth.rs` is defined
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
  tree with a real rotation story and it becomes the template for the session
  secret and for service-key rotation.
- **The stateless session cookie with kid rotation** and local verify on the hot
  path. The problem is the revocation model around it, not the cookie.
- **Mandatory by construction rather than by configuration** - the property
  under today's mandatory PKCE for every client including confidential ones,
  mandatory nonce, exact `redirect_uri` match and RFC 9207 `iss`. Reconciled
  against 7.2 under D-A, which removes the client population rather than the
  property: the `iss` rejection survives at flow step [7]; the nonce and the
  exact-redirect binding move onto `__Host-zs_flow`, enforced server-side rather
  than advertised in metadata. PKCE has no client left to protect and is retired
  with them - `crates/zeroship-core/src/pkce.rs` and
  `crates/zeroship-auth/src/oidc/authorization_code.rs` as an OAuth grant go in
  7.2 - and the leg it protected becomes a redemption over the authenticated
  service channel, where the redeemer proves an identity rather than proving it
  once held a random string. Retiring the mechanism is authorised; weakening
  "mandatory by construction" is not.
- **Header hygiene at the dispatch boundary**: stripping inbound
  `zeroship-user`, `authorization`, `x-app-id` and the `x-zs-` family in
  `collect_forwarded_headers` / `is_reserved_header`, plus the request-id and
  issuance-window binding. That binding is what the header actually buys; say so
  in the rustdoc instead of the false independence claim.
- **`policy::enforce` running before the isolate lease**, so
  `env.auth.requireUser` is not the fence, and the SEC-2 canonical-path agreement
  so the gateway's gate and the worker's re-parse cannot disagree.
- **Failing closed on a decode error rather than defaulting.** `load_client` is
  where that property lives today, inside
  `crates/zeroship-auth/src/oidc/authorization_code.rs`, which 7.2 deletes under
  D-A. The function has no survivor; the property is not retired with it, and
  carries forward as an obligation on every decode the session and grant paths
  replace it with. `register_signed_token` refusing to release a token whose key
  row is no longer active or retiring is the same property on the key path, and
  it survives unchanged.
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

Each is justified by what breaks without it.

**PERSON** - `zeroship.users`, typed id `usr_`. One namespace for every human.
"Creator" is not an identity kind; it is a membership edge on a project.

**Settled by the operator as decision D-D** (section 10): one account per human,
serving the creator and the end user, confirmed rather than assumed. Recorded at
the point of definition so it is not reopened later as taste. What lands with it:
it is **UNVERIFIED** that the namespaces are shared in the tree TODAY - that
marker stands as a measurement, its experiment is in section 11, and its answer
tells step 5 whether it adopts an existing shared namespace or has to merge
separate ones. **The membership edge the settled model names now EXISTS at the
project level, and this paragraph said it did not** - see C9.
`db/migrations-ts/20260906000000_organization_entity_model.ts` drops the
APP-scoped `zeroship.app_members` that
`db/migrations-ts/20260702000200_control_tables.ts` created and replaces it with
organization membership NARROWED per project: `zeroship.organization_members`
carries the seat, `zeroship.project_members` reduces it, and
`zeroship_authz::authority::effective_project_rank` composes the pair by minimum
so a project row can grant or ceiling but never widen. So "a membership edge on
a project" is a description of the tree rather than an implication this design
has to discharge. It remains independent of D-E either way: an audience-scoped
suspension reads the grant row, not a membership edge, so it needs no per-role
handle.

Carries `credential_epoch` (the generalised `credential_version`) and
`account_status`, a state machine over `active`, `deletion_scheduled` and
`anonymized`, replacing the separate lifecycle columns that today are read
everywhere and written from tests. One column for the account lifecycle, one
predicate, one feed field, and a production writer for every variant.

**`suspended` is NOT a variant of that column.** Under decision D-E (section 10) a
suspension is scoped to an audience, so it hangs off the GRANT row as
`subject_status` over `active` and `suspended` - one row per (person, audience),
which is per project for a project audience. The grant is where the subject
itself is stored, so the status OF a subject is stored beside it, and it is
enforced by the same validating read that already requires the grant to exist.
No second enforcement path, which is D-E's whole reason for choosing this scope.

**Why the variants differ in scope, stated so a reader does not "fix" them back
into one uniform column.** `deletion_scheduled` and `anonymized` are person-scoped
because an account is deleted as an account: there is no coherent reading of
"delete this person in project P only" while the same person keeps the account,
and D-D confirms that. A suspension is an abuse response to conduct, the conduct
happens inside one audience, and D-E scopes the response to where the conduct
was. A person-scoped column cannot express an audience-scoped suspension, and a
per-audience row cannot express an account deletion, so the states live on
different rows rather than sharing one enum by reflex.

The table keeps its current name. A rename to `people` was considered and
rejected: it is churn across the auth crate that binds nothing. The typed id
is the canonical platform `UserId`: the platform corpus authors `usr_` typed-id
text with no database default, and Rust mints the value before insertion.

*Without it:* nothing. It is the only stored identity fact.

**AUDIENCE** - a closed sum: `Platform` or `Project(project_id)`. This is the
unit a subject and a grant are scoped to, and it replaces `sector_identifier`,
the per-app OAuth `client_id`, and the CLI pseudo-client with one value.

**Settled by the operator as decision D-B** (section 10): the audience unit is the
Project. The sum's variants are a decision, not a design premise. **The
choice is recoverable, and that is why the sum can be closed now:** a later
organization layer would be a NEW audience variant rather than a re-derivation of
the sum.

**That recoverability clause has stopped being hypothetical.** An organization
layer HAS landed, above the project rather than beside it. The CHOICE recorded in
D-B is untouched by this document - the audience unit is still the Project - but
the condition it was recorded under is gone, and section 10.2 item 10 puts the
consequent question to the operator instead of answering it here.

`Project`, not `App`. That is task #72, and the unit is settled rather than
contingent: with app-scoped sectors there is no unit between "one app" and "the
platform", so cross-app teardown has no object.

**THE PROJECT ENTITY EXISTS. This paragraph asserted the opposite, and the
correction is C9.** `db/migrations-ts/20260906000000_organization_entity_model.ts`
creates `zeroship.projects`: `id` text PRIMARY KEY under a `projects_id_shape`
check for `^prj_[0-9A-Za-z]{22}$`, minted by
`zeroship_core::project_id::ProjectId` in `crates/zeroship-core/src/project_id.rs`,
a `slug` unique per organization, and `projects_organization_identity_key` over
`(id, organization_id)`. Membership landed with it as `zeroship.project_members`,
and `zeroship.app_members` was DROPPED in the same migration. `Resource::Project`
is live in `crates/zeroship-authz/src/resource.rs`, the narrowing rule is
`zeroship_authz::authority::effective_project_rank` in
`crates/zeroship-authz/src/authority.rs`, and
`crates/zeroship-control/src/organizations.rs` serves the create, read and
membership routes. So every `projects.id` foreign key in the sketches below
points at a live table carrying exactly the `prj_` spelling this paragraph used
to tell a reader to reconcile. The prefix collision it warned about is still
live and still not a join: `zeroship.sandboxes.project_id` in
`db/migrations-ts/20260702000500_sandbox_tables.ts` carries its own `prj_` check
for a derived dedup key belonging to the extracted sandbox subsystem, with no
foreign key into this corpus.

**What the landed entity leaves for step 9, and it is a real obligation rather
than nothing.** `projects.id` carries a NON-DEFAULT catalog collation: the
migration applies `COLLATE "C"` through a `raw` island, which the migration
engine's pre-migration catalog snapshot cannot see. A `sessions.project_id` or
`grants.project_id` authored as plain `text` and given its foreign key in the
SAME migration will be REFUSED, because the lowering compares a freshly authored
`text` against a live `text COLLATE "C"`. That is not a hazard being predicted
here; it is why `db/migrations-ts/20260906000200_apps_project_ownership_key.ts`
exists as its own file. Step 9 therefore adds and collates the column in one
migration and adds the foreign key in a later one. The identity domain has also
converged: `zeroship.users.id` and every `person_id` reference use canonical
`UserId` text, while `project_id` uses its own typed-id prefix. The domains
remain distinct by prefix and foreign-key target.

*Without it:* subjects are either global, so apps correlate users across the
platform, or per-app, so a project's apps cannot agree on a user.

*What the unit enables later, recorded and deliberately not designed here:*
per-project external identity - "this project's users authenticate against this
customer's own provider". It does not exist today: provider configuration is
platform-level and `db/migrations-ts/` contains no per-app or per-project
provider table. The Project is the natural place to hang such a configuration,
and without a project unit there would be no correct place for it. D-B makes that
feature cheap; this document stops there.

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
grant_id           -> grants.id  NOT NULL ON DELETE CASCADE
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

`projects.id` here, and on the grant row below, resolves to a LIVE table - see
AUDIENCE above, and C9 for what this paragraph used to say. What step 9 still
owes is not the entity but the ORDERING: the column is authored and collated
`C` in one migration and the foreign key added in a later one, because the
engine cannot see a `raw` collation island inside the migration it is lowering
against.

**`grant_id` is NOT NULL, and stating why is the point of this paragraph.** Every
audience has a grant row, Platform included: the grant is one row per (person,
audience), and Platform IS an audience under the closed sum above. So the platform
session hangs off a platform grant exactly as the project session hangs off a
project grant. An earlier draft of this sketch made the column nullable and
annotated it "NULL iff platform". Nothing justified that special case, and it cost
the design the property every other row buys - the suspension predicate rides the
statement that already resolves the grant, so a NULL there left that predicate
vacuous for precisely the audience that governs deploying. Removing the NULL
**removes a nullable column and a special case; it adds no mechanism**, and that is
the whole argument for it. A later reader who reintroduces the NULL for the
platform case will be reintroducing that hole, not repairing an oversight.

One table replaces `idp_sessions`, `gateway_sessions`, `app_session_anchors`,
`oauth_refresh_tokens`, `device_grants` and `token_revocations`.

Columns that earn their place individually. `parent_session_id` makes "log this
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
subject_status  'active' | 'suspended'        -- audience-scoped, decision D-E
suspended_at, suspended_cause
first_consented_at, updated_at
UNIQUE (person_id, audience_kind, project_id)
```

`subject_status` is here rather than on the person because decision D-E scopes a
suspension to the audience. The unique key IS the scope: for a project audience
the row is per project, and the platform-audience row is what a suspension of the
person's own deploy authority names - a separate act from suspending them as an
end user in someone else's project.

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

That list is only correct because the third-party OIDC provider capability is
going away, which decision D-A settles (section 10). While it was open, `oac_`
client ids might have had to become a real concept with a real registration. The
operator's reasoning is why they do not: a per-app client id today is an
auto-provisioned side effect of `zeroship deploy` rather than a registration, so
it is a spelling rather than a concept.

**D-A deletes one DIRECTION, and the directions are easy to confuse.** What goes
is the OUTWARD direction - a creator app or the platform acting as an OIDC
provider TO a third-party relying party. CONSUMING external identity providers is
untouched, and the classification above is exactly that shape: a federated
identity is an authentication method under the person. The live arms are kept -
`crates/zeroship-auth/src/identity/oauth/google.rs`,
`crates/zeroship-auth/src/identity/oauth/github.rs`,
`crates/zeroship-auth/src/ui/oauth_google.rs`,
`crates/zeroship-auth/src/ui/oauth_github.rs`, and the link step in
`crates/zeroship-auth/src/identity/linker.rs` - and 7.6 MERGES their stashes in
`crates/zeroship-auth/src/ui/oauth_stash.rs` rather than deleting them. There is
no SAML anywhere in the tree, so "SSO" here means those OAuth arms and nothing
more. Flow step [4]'s "password / magic / federation / TOTP" is the same
statement in the flow, and does not contradict this.

### 3.3 Credentials

Each row answers: what distinction does it carry, and what enforces it?

| Credential | Shape | Distinction | Enforced by |
|---|---|---|---|
| **Session secret** `zss_` | opaque CSPRNG, stored as versioned keyed HMAC on the session row, rotates on use with reuse detection | this browser or device is this session | the validating UPDATE on `zeroship.sessions`; reuse kills the row |
| **Access assertion** | Ed25519 JWT, `typ: zs-access+jwt`, claims `iss, aud, sub, sid, epoch, iat, exp, scopes, amr, auth_time` plus profile projection | hot-path presentation with no database read; `aud` separates Platform from `Project(pid)` | local signature plus `aud` equality; `sid` and `epoch` are the revocation handles |
| **Service assertion, full profile** | the existing `crates/zeroship-core/src/service_assertion.rs`: per-service Ed25519, `svc-assertion+jwt`, mandatory single-use `jti` | which *service* is calling, on an edge that must also be unreplayable | peer JWKS by `kid` plus the replay store in `crates/zeroship-authn/src/service_replay.rs` |
| **Service assertion, transport only** | the same per-service Ed25519 keypair, the same `svc-assertion+jwt` shape and the same `kid` resolution against the same peer JWKS, with no `jti` claimed and no store consulted | which *service* is calling | peer JWKS by `kid` |
| **Identity envelope** `ZeroShip-User` | JSON plus signature, bound to the dispatch request id and an issuance window | gateway-asserted end-user identity on the worker hop | the gateway's Ed25519 private key, verified under its public half |
| **Flow envelope** `__Host-zs_flow` | HMAC, purpose-tagged, `iat`/`exp` inside the payload | this browser started this flow | one decode with constant-time compare and server-side expiry |
| **One-time secret** | CSPRNG, hashed at rest, purpose-tagged row | which flow may redeem it | the consume UPDATE's `WHERE purpose = $2 AND consumed_at IS NULL AND expires_at > now()` |
| **Workflow signal token** `wst_` | unchanged | a machine capability on one run, with no person behind it | unchanged; the only bearer capability that is neither a session nor a service |
| **Credential epoch / session epoch** | not credentials, columns | everything issued before this moment is void | the join in every session validate |

That is the entire inventory. Everything else in the tree is deleted or merged.

**Why the service assertion is inventoried as distinct profiles rather than as
one credential.** The single-use claim is not free and is not local: the module
doc of `crates/zeroship-core/src/service_assertion.rs` describes it as an atomic
put-if-absent into a store shared by every replica of the callee, and the
statement that performs it is in `crates/zeroship-authn/src/service_replay.rs`.
So an edge carrying the full profile pays a shared-store WRITE per call, and the
write is on the request's critical path because the claim must win before the
call is admitted. That cost buys unreplayability and is worth spending where an
edge needs it. It must be chosen per edge and never inherited by default, which
is what a single row would have caused: section 8 assigns each edge its profile
by call rate, and section 11 records what the unsplit row produced before this
revision. The `kid` resolution, the trust bundle and the keypair are the same
under each profile, so a peer that can verify one can verify the other.

**The identity envelope's binding is why the dispatch hop can take the
transport-only profile.** Binding the envelope to the dispatch request id and to
an issuance window is itself a replay bound on that hop: an envelope replayed
outside its window or against a different request id does not verify. That is a
design decision recorded here, not a caveat - F5 in section 6 states the same
premise from the fence side, and section 8's dispatch tier depends on it.

**The cookie split at an app origin survives the merge prior.**
`__Host-zs_session` (HttpOnly, Secure, SameSite=Strict, long) carries the session
secret; `__Host-zs_access` (HttpOnly, Secure, SameSite=Lax, short) carries the
access assertion. They are one credential kind in distinct roles: a **minting**
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
                  the split database roles, see F16

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

Read the auth row as the concrete form of decision D-C (section 10): **the master
signing key lives in the auth process, and there is no external signer.** That is
the arrangement the operator decided on, stated here so a reader of section 3
alone cannot mistake it for provisional or add a signer row later believing one
was always intended. Section 9 records the risk this accepts and the one
condition that reopens it.

### 3.5 Distribution: one feed shape, on the transport that already exists

Verifiers need facts they do not own: routes, revocations, and public keys.

The gateway already pulls the route table on a sleep loop in
`crates/zeroship-gateway/src/sync.rs`, and
`zeroship_core::readiness::staleness_budget` already expresses "this pulled fact
is too old to act on". **This design adds one feed to that shape and invents no
new transport.** The revocation feed is an append-only, monotonically sequenced
table owned by auth, carrying session revocations, epoch floors, account-status
changes and per-audience suspensions; verifiers hold a cursor and a last-success
instant. A suspension entry names the audience it applies to, because under D-E
that is its scope, and a verifier that could not tell which audience was
suspended would have to fall back to a global refusal. JWKS rides the same poll.

**Fail-closed rule.** A verifier whose feed has not advanced within the staleness
budget refuses to authenticate. `RequiredPrincipal::User` routes answer 503;
`Anonymous` routes still serve. This mirrors the freshness gate the route table
already has, which is one of the things the current tree gets right.

This replaces **every** current revocation mechanism - the pushed snapshot in
`crates/zeroship-control/src/registry.rs` and the polled cache in
`crates/zeroship-authz/src/wrapper_revocation.rs`. Neither dominates the other
today: the pushed one adds coverage and the staleness refusal, the polled one
adds recency and applies no age filter. One feed with a cursor and a fail-closed
bound has each property and needs no ordering coincidence between separate
literals.

### 3.6 The network premise, and the half it does not buy

**Stated operator premise:** the platform's own services run inside a private
network. Nothing in this design derives a security property from that premise
without saying so here.

**What the premise buys.** Confidentiality on internal hops does not require
mutual TLS, and replay protection is unnecessary on the dispatch hop. Record
what dropping mTLS actually is, so a later reader does not file it as a flag
somebody declined to set: neither `crates/zeroship-gateway/Cargo.toml` nor
`crates/zeroship-worker/Cargo.toml` declares rustls. Each reaches TLS only
transitively, through the `cyper` feature selection in the root `Cargo.toml`. So
mutual TLS between those processes would be new transport work in each crate,
not configuration, and the premise is what makes not doing that work legitimate.

**What the premise does NOT buy, and this is the load-bearing half.** A private
network treats every process inside it as equally trusted. `zeroship-worker`
executes arbitrary creator code and is inside the perimeter by construction, so
the perimeter's trust assumption is false for the one process whose code the
platform does not write. The boundary that matters is therefore
worker-to-everything, and the network does not hold it. What holds it is
`crates/zeroship-runtime/src/transport/ssrf.rs`: `validate_url` fences at the
string level, `SsrfResolver` fences at the DNS level after resolution, and each
consults `is_blocked_ip` through `is_blocked_ip_under_dev` so the blocklist has
a single source of truth rather than a copy per layer.

**So internal auth still has a job under the premise.** The job is stopping a
compromised worker from impersonating the gateway to auth or to control. That
job needs asymmetric keys - a credential the worker verifies but cannot mint -
which is step 3. It does not need replay defence, because impersonation is a
forgery question and not a repetition question. The premise narrows what
internal auth is FOR; it does not remove it.

---

## 4. The flows

### 4.1 End-user app login

The gateway is no longer an OIDC Relying Party. There is no PKCE verifier at the
edge, no stash cookie, no per-app OAuth client, no broker secret, no id_token, no
`at_hash`. The handshake is first-party between our own processes, and the
landing code is redeemed over the authenticated service channel - strictly
stronger than PKCE, because the redeemer proves an identity rather than proving
it once held a random string.

That the handshake is between our own processes is the operator's stated
reasoning for decision D-A, which authorises 7.2's deletion outright rather than
conditionally.

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
                                                                        / TOTP, then resolve
                                                                        or create the grant
                                                                        for (person, Platform)
                                                                        and INSERT the PLATFORM
                                                                        session with
                                                                          grant_id = that grant
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
                                         FULL PROFILE - rate: per login
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

Step [8]'s mechanism is unchanged by the profile split and keeps the single-use
`jti`. The rate is what makes it affordable, so the rate is written at the step
rather than left to be inferred: this edge fires per login, so its store write is
proportional to logins. Note also, for the route census in F6 and for the set
discussion in step 2, that `/internal/session/bootstrap` is an `/internal/`
route on the AUTH service. The prefix spans services, so a census taken over the
control plane alone does not see this route at all.

The top-level redirect leg differs only in [1] and [9]: a full-page 302 rather
than a popup, landing on the original path instead of posting a message. **It
produces the same session row and the same cookies.** That is the fix for
P4's sharpest instance - today one login shape mints an anchor and the other does
not, and signout keys off the anchor.

Steady state, every dispatched request, with no database read in any process:

```
 BROWSER --__Host-zs_access--> GATEWAY
                                 verify Ed25519 vs OP JWKS; typ, iss, exp, skew
                                 aud == this route's project_id
                                 revocation feed: fresh within the staleness
                                   budget, else DENY; sid not revoked after iat;
                                   epoch >= epoch floor; the account is live AND
                                   the subject is not suspended in THIS aud
                                   (D-E: suspension is scoped to the audience,
                                    so the check is too)
                                 route policy: RequiredPrincipal, scopes
                                 CSRF: unsafe method requires exact Origin
                                 strip reserved headers and reserved cookies
                                 sign ZeroShip-User with the GATEWAY PRIVATE key,
                                   bound to the request id and issuance window
                               --> WORKER
                                     verify the envelope under the GATEWAY PUBLIC
                                       key (a key the worker cannot mint with)
                                     verify the transport under the same JWKS,
                                       SIGNATURE ONLY: no jti, no replay store
                                     own feed check, independently
                                     policy::enforce BEFORE the isolate lease
                                     --> creator code, env.auth principal
```

**The transport credential on that hop is signature only, and the block above is
why.** A single-use `jti` is a WRITE into the store shared by every replica of
the callee, so claiming one here would put a shared-store write on the
per-request path and contradict this block's own "no database read in any
process" in the more expensive direction. Note the asymmetry that makes the
substitution unavailable: a read can be cached, or served from a replica; a
single-use claim cannot be cached, because caching it is exactly what defeats
the property it exists for. The replay bound this hop needs is already supplied
by the identity envelope's request-id and issuance-window binding, per 3.3.
Signature only is the floor here, not an option - a credential-free hop would
leave the worker accepting any caller that can reach it, and section 6's F5 has
nothing to present.

Refresh, the only path that touches the database:

```
 BROWSER --__Host-zs_session--> GATEWAY --svc-assertion--> AUTH
                                                             ONE statement:
                                                               rotate the secret
                                                               WHERE hash matches
                                                                 AND revoked_at IS NULL
                                                                 AND idle/absolute live
                                                                 AND users.account_status
                                                                     = 'active'
                                                                 AND users.credential_epoch
                                                                     = sessions.credential_epoch
                                                                 AND the grant row for THIS
                                                                     session's audience still
                                                                     exists (FK) AND its
                                                                     subject_status
                                                                     = 'active'
                                                               RETURNING successor, epoch,
                                                                 scopes
                                                             zero rows -> login_required,
                                                               no partial state
                                                             presented-but-already-rotated
                                                               outside the idem window
                                                               -> REVOKE the session
                                                             else mint the assertion
 BROWSER <--the cookies------- GATEWAY <-------------------
```

Note what is absent: no anchor, no marker consulted, no `rotation_started_at`
comparison, no rows-affected gate after the fact. There is one read, it is the
authority, and the mint is downstream of it.

**The rate on the gateway-to-auth leg of that block is per refresh, so per
session per access-assertion TTL, and it carries the full profile.** That makes
it the highest-rate full-profile edge this design ships, and it is the one place
the assertion TTL leaves operations and enters the store's load budget. Open
decision 4 describes that TTL as the mint-load and partition-tolerance knob; it
is also the replay-store write-rate knob, and shortening it to buy recency buys
store writes at the same ratio. Read those effects together when the value is
chosen.

Note also what the same read does with the suspension: the account predicate is
person-scoped and the status predicate is audience-scoped, and they are in ONE
statement. That is why decision D-E chose the audience as the scope - a suspension
is enforced by the statement MINT-READS-ROW already requires, so the design gains
no second enforcement path. A per-app scope could not have been enforced here at
all: the mint does not know which app the person visits next.

### 4.2 Creator CLI login

RFC 8628 device authorization is kept - it is a genuine cross-device handshake and
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
                                         grant_id = the (person, Platform) grant
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
element from CLI modules, in the same file whose `post_form_with_headers`
already pipes the body through stdin specifically to keep secrets off argv. Today
`whoami` decodes the payload locally without verifying the signature and makes no
network call while unexpired.

Further changes with no new mechanism. `control` and `migrate-server` get
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
    PersonInAudience { person: PersonId, audience: Audience },
    PersonEverywhere(PersonId),
    Project(ProjectId),
}
pub async fn revoke(tx, sel: Selector, cause: Cause)     -> Vec<SessionId>;
pub async fn bump_epoch(tx, sel: Selector, cause: Cause) -> Vec<SessionId>;
```

There is no second spelling of the revocation write, and no path that writes a
session row without its feed entry.

`PersonEverywhere` makes global teardown a first-class verb. That is correct for
deletion and anonymisation, which are person-scoped by nature. **A suspension
calls `PersonInAudience` with the audience it names** - `Project(pid)` for the
end-user act, `Platform` for the deploy-authority act - settled as decision D-E:
the status is audience-scoped, so the teardown that accompanies it is
audience-scoped as well. `Platform` is an ordinary value of that field, not a case
the selector has to be widened for, because a platform session hangs off a
platform grant like any other. Do not reach for
`PersonEverywhere` here - that selector is for the account lifecycle, and using it
for a suspension re-creates the global answer D-E rejected.

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
| Suspend a person in a project - `PersonInAudience{Project(pid)}` (D-E) | intact | IMMEDIATE (that project) | `<= W` | project-scoped purge | suppressed | n/a | n/a |
| Suspend a person's platform audience - `PersonInAudience{Platform}` | IMMEDIATE | intact | `<= W` | purpose-scoped purge | intact | n/a | n/a |
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

**Column F covers each service-assertion profile, and the split in 3.3 leaves
its row unchanged.** Recall is the assertion lifetime plus the `kid` leaving the
peer JWKS under either profile, because that is what the key withdrawal acts on.
State the negative explicitly, because the row invites an improvement that would
be false: **the replay store is not a revocation lever.** It refuses a repeated
`jti` and says nothing whatsoever about a withdrawn key, so adding it to this row
while tiering the profiles would claim a property it does not have and would make
the full-profile edges look recallable faster than the transport-only ones. They
are not.

**The suspend rows follow decision D-E, and no row here is provisional.** Column A
is the platform session - the person's own deploy authority - and column B is the
project session, the person as an end user. A suspension in a project touches
column B for THAT project and leaves column A alone, which is the whole content of
D-E: a report against a person acting as an end user in someone else's project
must not stop that person deploying their own apps. Stopping deployment is the
separate act on the platform audience, which is why it has its own row rather than
being folded in. **That row names the same call with a different audience value**,
so it has a writer rather than describing an effect nothing in this section
produces - which is what it did while the selector could not name `Platform`.
**The deletion and anonymise rows stay person-scoped** - an
account is deleted as an account, and D-D confirms that; the audience scoping does
not spread to them.

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
No real inbox is ever projected on any path. The alias readers become one
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
V8 is escaped, because they are resident in one process. This design does not
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
private half is in the worker, and neither is the other. **On the dispatch hop
the transport rides the transport-only profile of 3.3** - signature verified
under the peer JWKS, no `jti` claimed. Signature only, never credential-free:
the red test below has to present a transport credential, so a hop with no
transport credential makes this fence unwritable and leaves the worker accepting
any caller that reaches it.

*Red:* a test presenting a valid transport credential together with an identity
envelope signed by the transport key, asserting refusal. When this passes, the
rustdoc on `encode_user_header` becomes accurate and
`crates/zeroship-worker/src/policy.rs` stops resting on a false premise. What the
request-id and issuance-window binding still buys is replay bounding; say only
that.

**F6. Every internal edge has a peer-verified caller check, and the profile is
chosen by rate.**

*Title note:* this fence used to be titled "every internal endpoint", and the
mechanism reached the gateway-to-worker hop. That hop is not an internal
endpoint - it is the app data path - so the old title contradicted the old
mechanism, and one of them had to be wrong. The edge is what the fence ranges
over.

*Mechanism:* peer-verified asymmetric transport on every internal edge, with the
single-use `jti` claimed only where the call rate is proportional to app loads
or control-plane events rather than to end-user traffic. Step 2 in section 8
assigns each edge its profile and states the rate the assignment rests on.

*Red:* a route census arm enumerating every registered route on every service and
requiring each to name a guard **of the right kind**. What makes the arm bind
rather than decorate:

- It rules on the guard KIND per route, not merely that a guard is named. An arm
  satisfied by the existence of a guard passes a route wearing a shared bearer,
  which is exactly what `check_auth` in
  `crates/zeroship-control/src/internal.rs` is today - so the arm would print
  full coverage over the defect this design exists to remove.
- It reads every router builder on a service, not one. At control the
  `/internal/` prefix is registered in `crates/zeroship-control/src/main.rs` AND
  by `configure` in `crates/zeroship-control/src/workflow_instance_api.rs`, which
  is where `/internal/workflows/signals/ingress` lives; the auth service has its
  own builder carrying `/internal/session/bootstrap`, per 4.1. An arm written
  against one builder per service sees neither route and prints exactly what full
  coverage prints.

This is the family instrument, not the instance fix -
`workflow_advance_internal` is the instance, and the tree already has a harness
that drives it, `tests/e2e_gateway_workflow_advance_authz.sh`, whose exploit arm
must flip from advance to refusal. That arm exercises the CALLER-side check, so
it flips only if the guard lands on that handler's inbound edge; see step 2 for
why the handler's inbound and forwarding edges take different profiles.

**F7. The worker cannot read another app's decrypted environment by credential.**

*Mechanism:* control decides, and what it hands over is a LEASE. The worker
presents only its own service identity; control consults its own placement view -
which the worker cannot write - and issues the environment for an app only if
that app is assigned to that node. The narrowing is computed by the party that is
not being narrowed. The lease is scoped to the app it names and bounded in time.

*Red:* an integration arm in which node N holds a valid service identity and the
app is assigned elsewhere, and the environment is refused. Impossible to write
today, because the worker holds `control_key` and every env request succeeds.

*Enumerate every enforcement point before treating one mutation as refuting the
guard, and note that the enumeration is direction-independent by construction.*
The **placement equality** is the invariant whichever way the lease travels - it
is the fence, and it is present under a pulled lease and under a pushed one. The
**`jti` single-use** rides whichever direction carries the request, so which
process presents the assertion follows from step 4's open lease-direction
question rather than being fixed here. Do not read this entry as presuming the
worker presents an app id; under a pushed lease that presentation does not
happen, and an enumeration written around it would name an enforcement point the
chosen direction does not have. Bounded honestly by G7.

**F8. A revoked session cannot be presented after `W`, and cannot be re-minted at
all.**

*Mechanism:* independent bounds. The assertion `exp` bounds presentation
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

**F10. A password change or deletion request ends every session without
enumerating them. A suspension ends the suspended audience's sessions, and only
those.**

*Mechanism:* `credential_epoch`, joined in the same UPDATE that slides the idle
window. This is `crates/zeroship-auth/src/store/sessions.rs` generalised to every
audience. A suspension is NOT an epoch bump: under D-E it is `subject_status` on
the grant row, and the same validating UPDATE that resolves the grant reads its
status. Different column, same statement, no second enforcement path. This holds
for the platform audience as well as a project one, and only because `grant_id` is
NOT NULL: a platform session resolves a platform grant, so the predicate is a
predicate there too rather than a comparison against a missing row.

*Scope, stated here so the fence is not read as uniform:* password change and
deletion are person-scoped by nature and are covered unconditionally. Suspension
is audience-scoped by D-E and deliberately cannot be folded into the person-scoped
epoch, because an epoch that is person-scoped cannot leave one audience's sessions
alive.

*Red:* an arm per lifecycle transition that bumps the epoch and asserts a refresh
on an older session is refused, plus a paired arm that suspends one audience and
asserts the OTHER audience's session still refreshes. That pair runs in each
direction - suspend `Project(pid)` and the platform session must still refresh;
suspend `Platform` and the project session must still refresh while deploying is
refused. Running only the project direction leaves the deploy-authority act
unbound, which is the state this fence was in while `grant_id` was nullable.
Mutation: delete the epoch
equality predicate and the first arm must fail; delete the grant-status predicate
and the suspended half of each pair must fail.

**F11. Every status variant has a production writer.**

*Mechanism:* a control-plane suspend and reinstate endpoint taking a (person,
audience) pair - the signature D-E settles - which stamps `grants.subject_status`
and revokes that audience's sessions in one statement, calling `PersonInAudience`
with the audience it was handed; plus the account-lifecycle writer for
`users.account_status`. `Platform` is an ordinary value of that argument, so the
platform-audience suspend row is written by this endpoint and not by a second one.

*Red:* a gate arm ruling on the variant set of each status enum - floor declared
beside the enum it rules on - requiring at least one non-test writer per variant.
An audience-scoped status makes the arm stronger rather than weaker: a writer that
stamps every audience at once fails the pairing arm in F10. This is the family fix
for
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
copy is refused in review. Today the identity upserts are byte-identical across
crates, each pinned by a test asserting a substring of itself.

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

*Honest caveat, stated in the design rather than discovered later:* splitting the
role inside one process is defence-in-depth against a route-confusion bug,
**not** a boundary
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

*Mechanism:* one reserved-name list consumed by the header strip and by a new
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
Correlation resistance against a party that observes the auth origin and an
app origin - the platform can always correlate; the guarantee is that *projects*
cannot. Protection against a compromised `zeroship-auth` - and that is now an
ACCEPTED RISK taken by the operator under decision D-C, not an open question
awaiting an external signer. Section 9 records the risk and names the one
condition that reopens it: the platform holding regulated data.

---

## 7. What this deletes

By path and symbol. Pre-launch, so each is a deletion, not a deprecation.

### 7.1 Mechanisms verified to bind nothing

**LANDED, AND THIS SUBSECTION WAS WRONG IN THREE PLACES.** Step 1 executed it
against the tree; where the tree disagreed, the tree won and the correction is
recorded inline below rather than silently applied. The pattern in all three is
the same: an item was named by the family it LOOKED like it belonged to, and its
real caller set was somewhere else.

- `crates/zeroship-gateway/src/auth.rs` - the whole module (`check_api_key`).
  Verified: no caller, production or test. DELETED at step 1, with the
  `pub mod auth;` in `crates/zeroship-gateway/src/lib.rs`. Note the trap the
  deletion had to avoid: `crates/zeroship-gateway/src/router/auth.rs` is a
  DIFFERENT module, also declared `pub mod auth;`, and it owns the whole
  per-request identity arm. A sweep keyed on the module name deletes the wrong
  one.
- The api-key family. Deleted at step 1: `api_key_hash` on `RouteEntry`
  in `crates/zeroship-core/src/types.rs`, the hash half of the mint in
  `crates/zeroship-control/src/registry.rs`, `zeroship.apps.api_key_hash` (by
  `db/migrations-ts/20260905000100_drop_app_api_key_hash.ts`, following the
  corpus convention of a new drop migration rather than an edit to an applied
  file), the create-response injection in `crates/zeroship-control/src/api.rs`,
  and the field from the published `AppRecord` in `sdks/control/src/index.ts`.

  **KEPT, and this is the correction.** `hash_api_key` and `validate_api_key` in
  `crates/zeroship-core/src/auth/mod.rs` are NOT api-key-only: they are the
  shared OAuth client-secret hashing primitives, with production callers in
  `crates/zeroship-control/src/{bootstrap_builder,oauth_clients,app_oauth_client}.rs`
  and `crates/zeroship-auth/src/oidc/refresh.rs`. Deleting them here breaks two
  crates; they belong to 7.2's dependency, not to step 1.

  **KEPT, then RENAMED, and the rename is what that reasoning was always asking
  for.** Every one of those production callers hashes or verifies an OAuth
  client secret and nothing else, so the names were describing a credential the
  functions never handled. Once the app-level key was deleted they became the
  only "api key" symbols left in the auth path, and a reader looking for the app
  key would find them and wire up a client-secret check - the same class of
  defect this whole step removes. They are now `hash_client_secret` and
  `validate_client_secret`, hashing and comparison unchanged. The auth crate's
  `client_secret_hash` wrapper went with them: it had no callers, and after the
  rename it was a second spelling of the function it forwarded to.

  **THAT "KEPT" IS REVERSED, and the reversal is the more useful record.** Step 1
  kept `AppRecord::api_key` and `zeroship.apps.api_key` on the grounds that the
  field had "two production readers". Re-measured before deleting them, that
  phrase was doing far more work than the facts support. There is no branch
  anywhere on the value: `Registry::create_app` mints it, `row_to_record` copies
  it into the struct, `dev_provision` prints it, and one `#[cfg(test)]` fixture
  builds a record with a placeholder. No comparison, no policy lookup, no
  refusal. Nothing can even present such a credential - no `X-Api-Key` reader
  exists in any Rust or TypeScript source - and the value never leaves the
  process, because `#[serde(skip_serializing)]` plus the create-response test
  keep it out of the only body that serializes an `AppRecord`. So "production
  readers" meant a `println!` in a dev-provisioning tool plus a struct field
  nothing consumes, and a dozen harnesses scraping that printed line back into a
  header no server reads.

  The whole family is therefore DELETED: the column (by
  `db/migrations-ts/20260905000200_drop_app_api_key.ts`, whose header carries the
  reasoning for owning no app-level key at all), the struct field, the mint, the
  six SELECT lists, the `dev_provision` print, every fixture INSERT and every
  inert `X-Api-Key` header. The harness gates that keyed on a non-empty key were
  cut rather than adapted - they asserted on a value that proved nothing - and
  replaced where a real signal was wanted by a gate on the app's own id.

  `tests/secret_beside_its_hash_gate.sh` still refuses the pair-shape this bullet
  named. Its stated caveat - that a plaintext column with NO hash sibling is
  invisible to it - is unchanged and still true; it simply no longer has
  `zeroship.apps.api_key` as its live example.
- `crates/zeroship-control/src/identity_bridge.rs` - the whole module. DELETED
  at step 1, with `crates/zeroship-control/tests/identity_bridge_test.rs`, its
  only caller. `provision_or_link` had test callers only; `fetch_email_verified`
  and its URL helper had NO caller at all, not even a test, so the module was
  deader than this bullet first said. Its header claimed the function sits on
  the bearer read path; what actually does is
  `zeroship_authn::platform_cli::materialize_default_grants`, called from
  `crates/zeroship-control/src/authz_guard.rs` and
  `crates/zeroship-migrate-server/src/auth.rs`, and the header's reasoning moved
  onto that function.

  Two records the deletion would otherwise have taken with it, kept here because
  no surviving code site is theirs. **`fetch_email_verified` was deliberately
  fail-closed**: it returned `Ok(false)` on transport failure, timeout, non-2xx,
  body-read failure AND malformed JSON, so that an unavailable admin API could
  never enable email-based account merge - the bool fed straight into
  `provision_or_link`'s merge decision. And it was the tree's only written record
  of the **GoTrue admin-API email-confirmation contract**: `GET
  <supabase>/auth/v1/admin/users/<subject>` under both an `authorization` bearer
  and an `apikey` header, with confirmation read as "the `email_confirmed_at`
  field is present and not null". If Supabase consumption is ever revisited,
  that is how it was read and why the failure arm was chosen.
- `crates/zeroship-gateway/src/sessions.rs` - **NOT the whole module, and this is
  the second correction.** `validate` was production-dead and is DELETED, with
  its rustdoc rehomed as a note in the same file so the refutation it carried
  survives the function: finding a revocation check uncalled is not finding the
  property missing, and the enforcement truth is the per-app family marker the
  request path reads. Its three gateway integration tests kept their assertions
  and now read the row themselves.

  Five other public items in that module DO have production callers - `create`
  and `NewSession` (`crates/zeroship-gateway/src/auth_token.rs` and
  `handle_auth_callback` in `crates/zeroship-gateway/src/router/dispatch.rs`),
  `latest_sid_for_user` (`auth_token.rs`), and both revoke helpers
  (`crates/zeroship-gateway/src/backchannel_logout.rs`). `IDLE_MINUTES` and
  `ABSOLUTE_HOURS` are consumed by `create`. The module becomes deletable after
  7.2 removes the OIDC RP, the back-channel-logout handler and
  `handle_auth_callback`; not before.

  One consequence to carry: with `validate` gone, NOTHING slides
  `idle_expires_at`. It is stamped at `create` and never bumped, and
  `IDLE_MINUTES`' doc now says so instead of describing a sliding window that no
  longer exists.
- `zeroship.jwk_key_state` in `db/migrations-ts/20260702000300_auth_oauth_tables.ts`
  - no reader, no writer. Confirmed by a case-insensitive whole-tree search:
  zero Rust hits of any kind, and no fixture INSERT. DROPPED at step 1 by
  `db/migrations-ts/20260905000000_drop_jwk_key_state.ts`, whose comment carries
  what the table bought (per-key `created_at`, so JWK retirement could age keys
  individually) and what replaced it (`zeroship.signing_keys`, which shipped as
  an ADDITION rather than the planned rename, leaving this as residue).
  Following the corpus's existing drop convention, the CREATE, the grant in
  `db/migrations-ts/20260702000900_grants.ts` and the owner-registry entry in
  `policies/platform-table-owners.json` are left in place: they run before the
  drop, and the prior `drop_*` migrations in this corpus did the same.
- `post_logout_redirect_uris` in `crates/zeroship-control/src/app_oauth_client.rs`
  - a public function with no production caller and no column to write into.
  DELETED at step 1, along with `APP_CLIENT_PREFIX` in the same module - an
  additional dead `pub const` alias this subsection did not name, found by
  enumerating the module's whole public surface. It is the kind of item rustc's
  dead-code lint cannot see, which is why it survived.

  A comment at the deletion site carries the obligation the removal must not
  take with it: `docs/proposals/2026-06-30-op-p0-spec-threat-model.md` specifies
  an open-redirect guard for RP-initiated logout - a supplied
  `post_logout_redirect_uri` must exact-match a registered entry for the
  resolved `client_id`, and on no match the OP renders a local 400 rather than
  redirecting. The capability was never built. Deleting the function without
  recording that is how the next author rebuilds it without the check.

### 7.2 The OAuth apparatus between our own processes

**AUTHORISED by the operator as decision D-A** (section 10). This subsection is
not conditional on anything; the reasoning is recorded at its close and in D-A.

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
  `crates/zeroship-control/src/oauth_grants_handlers.rs` and
  `crates/zeroship-control/src/device_handlers.rs`.
- `crates/zeroship-control/src/oauth_clients.rs` - the WHOLE module. An earlier
  draft of this list wrote "the per-app half"; the module has no halves.
  `reconcile_oauth_clients` is its only public entry, it is boot-time
  reconciliation of FIRST-PARTY clients from configuration, and per-app `oac_`
  clients appear in it only as an exclusion inside the private `prune`. Under D-A
  no first-party relying party of the platform OP survives either - the gateway
  stops being an RP above, and the platform CLI client id goes in 7.5 - so
  nothing is left for the module to reconcile. Deleting only a "per-app half"
  would leave a boot-time registrar for relying parties whose
  client-authentication story this subsection has removed.
- `derive_broker_secret` in `crates/zeroship-core/src/auth/mod.rs`,
  `verify_broker_secret` in `crates/zeroship-auth/src/oidc/issuer.rs`, and the
  broker secret settings on each side.
- Tables: `oauth_clients`, `oauth_grants`, `oauth_authorization_codes`,
  `oauth_refresh_tokens`, `app_oauth_clients`, `app_user_identities`,
  `idp_sessions`, `gateway_sessions`, `identity_links`, `device_grants`, and the
  brokered-requires-secret-basic CHECK.

**The capability this removes, stated plainly:** a creator app can no longer act
as an OIDC provider to a third-party relying party. Nothing depends on that
today. If it becomes a product it is a first-class feature with real registration
and real secrets, not an auto-provisioned side effect of `zeroship deploy`.

**The reasoning, which is the durable part of the decision.** What exists today is
not the feature: there is no registration, no third-party consent, no scope model
and no documentation. It is OAuth ceremony between our own processes, and
the third-party capability falls out only because that plumbing hands every app a
client id. Keeping the side door open does not get the platform closer to the
feature; it only keeps the plumbing complicated.

**Deleted direction: OUTWARD only.** Consuming external identity providers is
unaffected and is KEPT - the Google and GitHub arms under
`crates/zeroship-auth/src/identity/oauth/` and `crates/zeroship-auth/src/ui/`, and
the link step in `crates/zeroship-auth/src/identity/linker.rs`; 7.6 MERGES their
stashes in `crates/zeroship-auth/src/ui/oauth_stash.rs` rather than deleting them,
and there is no SAML anywhere in the tree. An editor sweeping the word OAuth out
of the auth crate under D-A deletes the wrong direction. Per-project external
identity, where a project's users authenticate against that customer's own
provider, does not exist today and is not designed here; it is a future feature
that D-B's Project unit makes cheap, by supplying the first correct place to hang
such a configuration.

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

**Each deletion below names its replacement.** A deletion list that says what
goes and not what arrives is how a hop ends up unauthenticated between steps,
with the deletion section and the sequence section each looking correct on its
own.

- `control_key`: `check_auth` in `crates/zeroship-control/src/internal.rs`, the
  worker's use, the gateway setting, and the environment twin.
  *Replaced by:* the full profile on the privileged control routes, plus the
  lease inversion in step 4 for the environment specifically.
- `worker_key`: `check_worker_auth` and `verified_user_json` in
  `crates/zeroship-worker/src/handler.rs`, the settings on gateway, worker and
  control, and the environment twin.
  *Replaced by:* on the dispatch hop, the transport-only peer credential of 3.3
  for `check_worker_auth`'s job, and the asymmetric `ZeroShip-User` envelope of
  step 3 for `verified_user_json`'s job. Those are separate replacements for
  separate jobs, which is the whole content of F5.
- `derive_app_scoped_control_token` and `verify_app_scoped_control_token` in
  `crates/zeroship-core/src/auth/mod.rs`, and the bespoke HMAC arm of
  `check_app_scoped_auth` in
  `crates/zeroship-control/src/workflow_instance_api.rs`.
  *Replaced by:* the placement decision in step 4. A narrowing computed by the
  narrowed party is replaced by a decision taken by the other party, not by
  another derivation.
- The unauthenticated arm and its `TODO(DW-signed-transport)` in
  `crates/zeroship-gateway/src/router/dispatch.rs`, and
  `workflow_advance_unsigned` on the worker.
  *Replaced by, and the edges take different profiles:* the INBOUND edge of
  `workflow_advance_internal` - which is where that `TODO` sits, and which checks
  no caller credential at all today - takes the full profile, because its rate is
  per advance. Its OUTBOUND leg is the ordinary dispatch hop: the handler
  forwards through `proxy::forward_workflow_advance` over the hash ring, signing
  with `worker_key` today, and it takes the transport-only profile like every
  other dispatch. Applying one sentence to the handler rather than to its edges
  re-imports the store write onto the forwarding path.
- The `ZeroShip-User` HMAC family in `crates/zeroship-core/src/auth/mod.rs`,
  replaced by Ed25519. `derive_pairwise`, `derive_pairwise_salt`,
  `constant_time_eq` and `extract_bearer` are KEPT.

### 7.5 CLI

- The Supabase arm in `crates/zeroship-cli/src/auth.rs` and the corresponding
  provider variant in the verifier: `cmd_login` routes every variant into one
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

- The signed or random browser cookies become one `__Host-zs_flow`: the CSRF
  double-submit in `crates/zeroship-auth/src/csrf.rs`, the magic-link CSRF
  cookie, the TOTP challenge cookie in
  `crates/zeroship-auth/src/sessions/totp_challenge.rs`, and the provider
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
  single-writer at auth. **Read section 11 before touching these** - the session,
  anchor and identity policies genuinely bind today.
- The control-plane grants on `zeroship.device_grants` in
  `db/migrations-ts/20260702000900_grants.ts`; control's device flow is already
  gone and the grant is the residue with teeth.

### 7.8 Wire what is built and unwired

- `crates/zeroship-core/src/service_assertion.rs`,
  `crates/zeroship-core/src/service_identity.rs` and
  `crates/zeroship-authn/src/service_replay.rs` are NOT deleted. They are wired.
  Do not invent a fourth HMAC scheme. **Their backing table is already
  provisioned** - see section 11.
- **The replay store is wired on the full-profile edges only.** Which edges those
  are is section 8's step 2, and the reason is the store write it costs per call.
- **The transport-only profile is a CODE CHANGE to
  `crates/zeroship-core/src/service_assertion.rs`, not a wiring choice, and the
  obvious implementation is the one that module was written against.** Its own
  doc states that every check is a hard rejection, with no warn-and-continue arm
  and no configuration that turns one off, citing CVE-2020-15222 and the Keycloak
  `cache-embedded-mtls-enabled` case where an optional hardening flag silently did
  nothing across version lines. So the variant ships as a separately NAMED profile
  with its own verifier entry point, on the same Ed25519 mechanism and the same
  peer JWKS - never as a flag on the existing verifier, because a flag is
  precisely the failure that module exists to refuse, and a flag would weaken
  every edge that keeps the full profile. The instruction above not to invent
  another HMAC scheme still binds and is not weakened by this: the mechanism is
  unchanged, only the claim set differs.
- `users.account_status` and `grants.subject_status` each get a production writer.
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
session, anchor and identity tables - which is the evidence that those are live
fences and the others are not.

**Step 1. Delete what binds nothing.** `check_api_key` and the api-key columns,
`identity_bridge`, the gateway's `sessions::validate`, `jwk_key_state`, the
orphan public function in the app OAuth client module.
*Red test:* a control-plane test asserting the create-app path neither RETURNS
an app-level key nor STORES one - a live-PG assertion that `zeroship.apps` has no
key-shaped column and that the returned record round-trips, so nothing is being
withheld from the response - plus a gate arm refusing a plaintext secret column
stored beside its own hash. No premise.

**Step 2. Wire peer-verified service identity on every internal edge, with the
profile chosen by RATE rather than by URL prefix.** Land the route census arm
(F6) in the same change.

The tiers, and the measured rate each rests on:

- **DISPATCH - the gateway-to-worker hop, per request.** Asymmetric signature
  only: the transport-only profile of 3.3, no `jti`, no shared store, ever. The
  replay property is not what that hop needs, and the identity envelope is
  already bound to the dispatch request id and an issuance window. This tier is
  what keeps 4.1's steady-state block true.
- **POLLED - `/internal/routes` per poll per gateway, `/internal/versions` per
  poll per worker.** These sit on a replacement path owned by
  `docs/proposals/2026-09-05-app-metadata-distribution.md`, whose shape is not
  settled there and which that document does not describe as a deletion of these
  endpoints. **Step 2 therefore specifies no credential profile for them and
  depends on none.** Whatever survives that work inherits the POLLED reasoning:
  the rate is per poll per process, so a single-use claim is proportional to
  pollers rather than to traffic, and the tier is affordable if it is still
  needed at all.
- **RARE AND PRIVILEGED - the full profile, signed plus single-use `jti`.**
  `/internal/apps/{app_id}/env` is the privileged one and fires per app load:
  `load_on_demand` in `crates/zeroship-worker/src/handler.rs` reaching
  `fetch_app_env` in `crates/zeroship-worker/src/sync.rs`, plus a refetch on a
  version change during reconcile. `/internal/apps/{app_id}` is the same rate,
  through `fetch_app_version`. `/internal/workflows/signals/ingress` is per
  signal. `workflow_advance_internal`'s INBOUND edge is per advance, and it
  checks no caller credential at all today - the `TODO(DW-signed-transport)` in
  `crates/zeroship-gateway/src/router/dispatch.rs` sits inside that handler. The
  handler's outbound leg is the dispatch hop and takes the dispatch tier; see
  7.4.

Store writes are then proportional to app loads, signals and advances rather
than to end-user traffic.

**What step 2 still owes the dispatch hop.** Only the `jti` and replay half
leaves that hop; the key-distribution half stays here and is what step 3
consumes: enrolling the gateway's public key with the worker, resolving it by
`kid`, and rotating it. Read the tiering as removing a claim from that hop, never
as removing the hop from this step - step 3's premise and F4's "absent a
configured gateway public key the worker refuses to start" have no supplier
otherwise.

*Premise:* the replay store's backing table is provisioned - it is; see section
11 - and the assertion round-trip test in `crates/zeroship-core/tests/` is green.
*Red test:* `tests/e2e_gateway_workflow_advance_authz.sh`'s exploit arm flips
from advance to refusal, and a control test refusing a shared-secret bearer on
the environment endpoint.

**What "every internal edge" does NOT range over, and why the URL prefix is the
wrong census.** Of the routes registered under `/internal/` at control in
`crates/zeroship-control/src/main.rs`: the polled ones are on the replacement
path above. Workflow-advance is not among them at all - it is registered at the
GATEWAY, under `/__zeroship/internal/workflow-advance` in
`crates/zeroship-gateway/src/router/dispatch.rs`, and it is a proxy rather than
an API, so it is inventoried by edge above and in 7.4. That is the prefix
spanning services again, in the other direction from
`/internal/session/bootstrap` at auth. `/internal/billing/reconcile` and
`/internal/spend/reconcile` are cron triggers exposed as HTTP; they are named
here as OUT OF SCOPE for step 2 rather than redesigned, because what they need is
a decision about how cron reaches a service, and this step is not the place to
take it. And `/internal/webhooks/stripe` is **externally reachable and
Stripe-signed**: it is registered in the same `/internal/` block, but its guard
is `verify_stripe_signature` in
`crates/zeroship-control/src/stripe_handlers.rs`, not `check_auth` in
`crates/zeroship-control/src/internal.rs`. It is on the internal prefix and it is
not internal. **Note the misplacement explicitly**, because anyone writing a
network policy or a firewall rule off that prefix gets it wrong in the direction
that breaks Stripe delivery. Do not move that route here, and do not extend the
service assertion to it: implemented as "also accept a service assertion", that
sentence adds an additional accepted credential to an externally reachable
route, which is worse than the delivery outage it was trying to avoid.

*Landed. Where peer keys come from, and two things the step surfaced.*

**Peer keys are CONFIGURED, not fetched, and this contradicts 3.5's "JWKS rides
the same poll".** Each binary takes two paths - its own PKCS#8 private key and
one JWKS-shaped document naming every peer's public half - through
`crates/zeroship-core/src/service_peers.rs`. Three reasons, and the first is the
decisive one: internal hops are cleartext HTTP here, so a polled JWKS is
substitutable by exactly the adversary an asymmetric peer credential exists to
stop. Second, the only feed reaching both the gateway and the worker is served by
CONTROL, so distributing the GATEWAY's identity key over it would let control
substitute the gateway's identity - a trust root 3.4 places nowhere near control.
Third, F4 words step 3's fence as "absent a CONFIGURED gateway public key the
worker refuses to start", and a fetch has not happened at startup. The trust
bundle's own rustdoc already argued this and was already right. **3.5 should be
corrected rather than implemented as written.** The cost is unattended rotation;
the bundle carries several keys per issuer, so a rotation is still expressible
without downtime.

**F3 and the advance edge are incompatible in the END STATE, and step 6 inherits
the problem.** `workflow_advance_internal`'s inbound edge takes the full profile,
so the GATEWAY must claim a `jti` in a store every gateway replica shares. F3
says the gateway holds no database credential at all. Both cannot be true. The
implementation uses the gateway's existing per-thread pool and says so at
`PoolReplayStore` in `crates/zeroship-gateway/src/main.rs`; whoever lands step 6
has to move this edge off the gateway or re-tier it, and that comment is where
they will find out.

**Absence refuses rather than disabling, and that is the inversion.** A process
with no key material serves no guarded edge and reaches no guarded peer
(`ServiceAuth::unconfigured`). Step 2 delivers the request-time half of that;
the STARTUP half was step 3's, and is now paid - see the note under step 3.

**Step 3. Make the identity envelope asymmetric.** The gateway signs with its own
Ed25519 private key; the worker verifies under the public half; the empty-key
escape is deleted and a missing public key refuses startup.
*Premise:* step 2, for peer key distribution - specifically the enrolment of the
gateway's public key with the worker, its resolution by `kid`, and its rotation.
Step 2's dispatch tier drops the `jti` and the replay store from that hop and
keeps exactly this half, which is what this step consumes.
*Red test:* F4's worker-side forgery test, which cannot even be written today.
This step alone makes the `encode_user_header` rustdoc true.

*Landed, in two parts, and the second part is worth reading before step 6.* The
asymmetric envelope came first; the STARTUP refusal followed, and closing it
turned up two things neither this step nor F4 had anticipated.

**The refusal belongs in the LOADER, not in the three `main`s.** Every binary
that holds a peer bundle read two paths and had its own "neither was configured"
branch, and the natural body of such a branch is to carry on. Putting the
refusal in `ServiceKeyring::load` in `crates/zeroship-core/src/service_peers.rs`
means there is nothing left for such a branch to do, so the escape cannot be
reintroduced one binary at a time. An unset path gets its own error naming the
SETTING, because an empty path reaches the filesystem as `""` and comes back as
a not-found naming no file - the least actionable sentence a boot log can carry.

**IT WAS ORDERED BEHIND UNRELATED SUBSYSTEMS IN BOTH BINARIES, WHICH MADE IT
CONDITIONAL ON THEM.** The worker built its `ServiceAuth` inside `WorkerConfig`,
after `db_posture::validate_database_url` CONNECTS; the gateway built its own
after the broker master secret is read. A worker with no peer document and no
database therefore reported the DATABASE - so the fence the worker cannot serve
a request without was gated on the one subsystem the dispatch tiering was chosen
to keep it independent of. Both loads are now hoisted above that work. Whoever
lands step 6 should keep them there: an F3 that moves the gateway's database
away must not silently move this fence with it.

**Three binaries refuse; two correctly do not.** `zeroship-auth` and
`zeroship-migrate-server` mint no assertion and verify none, so they hold no
peer bundle and have nothing to refuse.

*Gate:* `tests/service_peer_boot_gate.sh`, which launches the real worker and
gateway. Its `configured` control does not assert exit 0 and does not need to -
it requires the key-material refusal to be ABSENT and a named LATER refusal to
be PRESENT, which is positive evidence the process walked THROUGH the fence.
That shape is not stylistic: a mutation that logged the refusal and then carried
on left an exit-code-only gate fully green, because the later refusal supplied
the non-zero exit.

**Step 4. The environment becomes a lease control grants, not a fetch the worker
performs.** `control_key` and the app-scoped derivation are deleted.

*The inversion, stated plainly, because it is the content of the step.* Today the
worker presents a root credential and control complies: `fetch_app_env` in
`crates/zeroship-worker/src/sync.rs` passes `config.control_key`, so the worker
asserts its own entitlement and control has nothing to check it against. After,
the worker presents only its own service identity, and CONTROL consults its own
placement view - which the worker cannot write - to decide whether that worker
should hold that app's environment.

*Why a lease and not a fetch result.* A lease is scoped to the app it names and
bounded in time. A fetch result is neither: it lives in worker memory until the
process dies, so a placement change has no effect on environment already handed
over. The process-ownership table in 3.4 already lists env leases and placement
decisions under what `zeroship-control` MAY MINT, so this step aligns the
sequence with the model rather than adding to it.

*How control comes to HAVE a placement view, settled 2026-09-07.* There is no
app-to-worker assignment to look up, and that is not an oversight: `HashRing::select`
in `crates/zeroship-gateway/src/proxy.rs` hashes the app onto the ring and returns
the first worker whose in-flight count is under `max_per_worker`, falling back to
the least loaded when all are saturated, and workers load apps on demand and evict
by LRU (`evict_lru` in `crates/zeroship-worker/src/cache.rs`). Placement is
emergent and load-dependent, and one app may be served by several workers at once.
So the placement view cannot be an observation control collects. It is a FUNCTION
control computes: per app, the CHWBL primary plus a bounded number of ring
successors over a roster control already holds, published on the route feed control
already owns and the gateway already pulls. The gateway keeps routing and keeps
bounded-load spillover, but spills only WITHIN that eligible set. Control answers
an environment request by comparing the caller's per-instance identity against the
set it computed itself - satisfying F7's "computed by the party that is not being
narrowed" literally rather than by proxy. The successor count is a config symbol,
not a constant in this prose; it bounds spillover to the eligible set instead of
the fleet, which makes it a capacity parameter that is also a security parameter.

*Why the ring key and the eligible set are ONE change, stated mechanically rather
than as a slogan.* `HashRing::new` in `crates/zeroship-gateway/src/proxy.rs`
derives every vnode position by hashing the worker's URL. `HashRing::select`
hashes the app's ring key, walks from there, and returns the first worker under
`max_per_worker`.

So there are two rings in play the moment control starts computing an eligible
set: control's, ordered by the ring keys it MINTS, and the gateway's, ordered by
the URLs it was CONFIGURED with. Those orders are unrelated. A gateway routing on
URL order would leave the eligible set on almost every dispatch, and the fence
would read as a routing bug rather than as a refusal. The two must therefore
consume the SAME ring key: control mints it, publishes it on the feed the gateway
already pulls, and the gateway builds its ring from that instead of from the URL.
Landing either half alone is not a partial fence; it is a disagreement.

**The spillover fallback is the half to watch.** `select` walks for a worker
under the cap and, finding none, falls back to the LEAST-LOADED worker in the
fleet. That fallback must be confined to the eligible set too. Leave it fleet-wide
and the narrowing holds exactly until the fleet is busy, which is when an attacker
would want it to fail and when nobody is reading logs. A fence with a
load-dependent bypass is worse than none, because it tests green.

*The health monitor, and the trap waiting in it.* An eligible set computed over
`status = 'active'` is a set of instances that were once active, not ones that are
alive: enrolment is per boot, nothing transitions a row, and a crash-looping worker
leaves a fresh `active` row per attempt. Something must observe liveness.

**That something must NOT write its observation into `status`.** The column is a
closed set over what a WRITER DECLARED, and
`db/migrations-ts/20260907000300_worker_instances.ts` already says why readiness is
excluded: readiness is a probe result, it is derived, it expires, and it belongs to
whatever performed the probe. Admitting it would make one column carry two kinds of
fact and leave readers disagreeing about which they hold.

The trap, if that rule is broken: `gone` is TERMINAL, there is no path back, and a
worker only re-enrols by restarting. A monitor that writes `gone` on a failed probe
therefore turns a transient network blip into the permanent eviction of a healthy
worker, recoverable only by killing it. The blast radius grows with fleet size and
peaks during exactly the partial-partition conditions that produced the blip.

So: the monitor holds observed liveness in its own short-lived state, and the
eligible-set computation intersects declared `active` with recently-observed-healthy.
The table keeps what control declared; the monitor keeps what it saw; neither is
written into the other. What may promote a long-unhealthy instance to `gone` is a
retention decision with an operator in it, not a probe timeout - and it is not
settled here.

### Step 4's delivery order

This step grew into four pieces that land separately. They are numbered here
rather than renumbering the sequence, because the citations elsewhere in this
file name step 4 as a whole.

**4a. The registry.** LANDED. Control writes `zeroship.worker_instances`, derives
the advertised address from the enrolment connection, mints the ring key, and
resolves an instance's key back from its own ACTIVE row.

**4b. Worker boot enrolment.** LANDED. The worker generates an Ed25519 keypair at
boot, in memory, enrols presenting only its listening port and public key, and
refuses to start when control refuses it.

**4c. The health monitor.** LANDED. `crates/zeroship-control/src/worker_health.rs`
sweeps the declared-enrolled set, probes each at its derived address, and holds
the reading in its own `HealthView`. It issues exactly one statement against the
registry, the SELECT that reads the set; it never writes `status`, and it ships
no reaper.

The view is PROCESS-LOCAL for now, spawned in `main.rs` beside the crons. Its
reader is 4d, and `AppState` is a struct literal with no builder, so hanging an
unread field off it today would mean editing every construction site to carry
something nothing consumes. The change that reads the view is the change that
should move it onto the state.

`latest` returns an `Option`, because NEVER PROBED and UNHEALTHY are different
facts: collapsing them would make an instance ineligible for the sweep that
follows its own enrolment. `healthy_within` requires FRESHNESS, so a monitor that
stopped sweeping cannot leave the fleet looking permanently healthy.

*Red test, and it is a PAIR because either arm alone passes against the wrong
thing:* enrol an instance, then kill the process WITHOUT touching its row.
(i) The monitor must report it unhealthy while the row still reads `active` - the
two disagreeing, with the monitor right, is the whole point. (ii) The row's
`status` must STILL read `active` afterwards, which is what proves the monitor did
not write its observation into the column. The paired control is a live instance,
which must report healthy and also leave its row untouched.
*The arms must observe the PROBE RESULT and the ROW, never a log line.* A monitor
that logs "unhealthy" and changes nothing prints what a working one prints; that
exact substitution survived three of four arms when it was tried on the worker's
startup refusal.

BOTH ARMS ARE MUTATION-BOUND, EACH BY ITS OWN MUTATION, which is what makes the
pair a pair rather than one assertion with a spare. Making `tick` write `gone` on
a failed probe - the catastrophic implementation - reddens only arm (ii), which
reports the row reading `gone` where `active` was required. Making `probe` return
healthy unconditionally reddens only arm (i), which reports the dead instance
observed healthy. Neither mutation reddens the other's arm, so neither assertion
is carrying the other.

**4d. The eligible set.** NOT STARTED. Design above, and it carries the ring-key
coupling: control's ring and the gateway's must consume the same key or they are
two different orders.
*Red test:* the gateway dispatches an app only to a worker in the set control
published for it, proven by a fleet where the set is a strict subset. The arm that
matters is the SECOND one: fill every worker in the set to `max_per_worker` and
assert the dispatch does NOT reach a worker outside it. Without that arm the fence
is untested exactly where it fails - a load-dependent bypass tests green.
*Precondition:* `tests/e2e_platform.sh` must be green first. It is the only harness
covering three-worker routing consistency and cross-worker isolation, and it is
red today for a reason attributed to the organization re-rooting but not proven.
Landing 4d against a standing red makes a real regression indistinguishable from
it.

*Prerequisite, and it is unavoidable under any variant of this step.* Per-instance
worker identity must land first. `ServiceKeyring::load` in
`crates/zeroship-core/src/service_peers.rs` reads one signing key per BINARY ROLE
and `WORKER_SERVICE_NAME` is a constant, so every replica in a fleet mints
byte-identical claims. Until a worker can be told apart from its peers, an
eligible-set comparison has nothing to compare, and the step's fence is
unwritable - which is what F7 already says about itself.

*SETTLED 2026-09-07, because the obvious two shapes each break a fence and an
implementer stopped rather than pick one.* The worker holds TWO keyrings, and
which one is used where is part of the design rather than an implementation
detail:

- **The ROLE keyring**, loaded from the operator's key file under `svc/worker`,
  is used for the ENROLMENT CALL ONLY. It is what the worker already holds, and
  enrolment must authenticate as the role because at that moment no instance
  exists.
- **The INSTANCE keyring**, built on a keypair generated at boot in memory,
  mints under `svc/worker/<wkr_id>` and is addressed as `svc/worker` via the
  `addressed_as` split. Everything after enrolment uses it.

Two keyrings rather than one is forced, not chosen. `ServiceKeyring::from_parts`
builds the minter from the issuer at construction, so the issuer cannot be
changed afterwards and minting under the instance name REQUIRES constructing on
the instance key. Keeping the role key and merely relabelling it is refused by
that same constructor as `OwnKeyUnderForeignIssuer`, and correctly: the peer
bundle publishes that key under `svc/worker`, so every holder of the bundle
would accept the instance's signature as the role's.

**The instance keyring's own-key check must be REPLACED, not inherited, and this
is the load-bearing half.** `from_parts` refuses a process whose own public half
is published under a FOREIGN issuer. Against a key generated at boot that
refusal is vacuous by construction - a fresh key is in no bundle - so carrying it
over unchanged would make the worker the one process whose F4 own-key check
cannot fire, while still reporting exactly what a check that ruled and approved
reports. The instance keyring therefore refuses when its public half appears in
the bundle AT ALL. That predicate has content on a boot-generated key: it fires
on a key collision or on a planted key, and it is false on every honest boot.

The `UserEnvelopeSigner` minted by `from_parts` is the gateway's capability. Its
live consumers are in `crates/zeroship-gateway/src/router/auth.rs`, where
`decode_user_header` and its neighbours call the accessor.

**This paragraph named `crates/zeroship-gateway/src/oidc_rp.rs` until 2026-09-08,
and that was WRONG in the way this file warns about elsewhere.** That file names
the TYPE in a parameter position and never calls the accessor. The claim came from
grepping the type name and reading a type-position match as a consumer, which is
spelling rather than behaviour. Search for the CALL when the question is who
exercises a capability.

Part 2 must confirm the worker's copy has no consumer before it constructs a
second one, and must not mint one on the instance key on the strength of this
paragraph alone.

*How control learns an instance key, SETTLED 2026-09-07.* Control must verify an
assertion whose `iss` names an instance and whose signature is over a key only
control has ever seen - it wrote the row. `ServiceTrustBundle::keys_for` is an
exact-string lookup over the OPERATOR's peer file, and nothing puts an instance
key there.

Control resolves the instance key BEFORE verifying, and hands verification a
bundle carrying that one extra key. It does not gain a key-resolver seam that
verification calls back into. `zeroship-core` is a leaf crate holding
inter-service wire types; giving it a database-shaped trait so a verifier can
read a row would invert that, and the alternative costs nothing - the role and
instance are now separable at parse time, so control can ask "does this issuer
name an instance?" and do the lookup itself before any verification begins.

Two constraints on that augmented bundle, both load-bearing. The instance key is
published under the INSTANCE issuer only, never under the role: publishing it
under the role would let one instance's key verify an assertion attributed to the
role, which is the collapse the whole split exists to prevent. And the operator
file remains the only source for ROLE keys - an instance row must never be able
to introduce or replace a key for `svc/worker` itself.

Left open deliberately: this is a database read on control's hottest service
edge. Cache it when there is a measurement saying it matters, not before, and
note that any cache needs an invalidation story for a revoked instance - a cached
key outliving its revocation is the failure mode, and it is worse than the read.

*What making enrolment MANDATORY costs, stated before it is paid.* Control's
enrolment envelope has no declaration anywhere in the tree - not in the compose
topology, not in any harness under `tests/`, not in any ops config - so control
boots with a closed envelope and refuses every enrolment as `envelope_unset`.
A worker that refuses to start on a failed enrolment therefore starts NOWHERE
until the envelope is declared where workers are expected to boot. That is the
honest cost of the F4 shape and it is not avoidable by a worker-side setting:
the refusal is control's.

*The constraint on ever hardening enrolment, decided before this proposal and
still binding.* Making enrolment a BOUNDARY rather than a distinguisher means
answering "who deserves a credential, and how" - which
`docs/proposals/2026-08-16-service-identity.md` calls LAYER 3, the attestation and
bootstrap layer, and names SPIFFE/SPIRE and cloud workload identity as its
occupants. That proposal declined to adopt it, and the reason is a product
constraint rather than a preference: zeroship is self-hosted into unknown
environments, and "a default that imposes infrastructure (a CA, a SPIRE cluster, a
service mesh) is not deployable by a user on a single VPS."

**So no hardening design here may require SPIRE, a service mesh, a CA, or a cloud
provider's workload identity.** What this tree adopted is LAYER 2 only: identity
presented as a signed JWT-shaped assertion. The `spiffe://` spelling in
`ServiceIssuer` is a NAMING CONVENTION and nothing more - there is no issuing
authority, no attestation, no rotation, no agent, and nothing named SPIRE, SVID or
workload API appears in any crate. It was chosen so the identifiers would carry
unchanged into X.509 SANs if mTLS ever arrived. Do not read it as a commitment to
the ecosystem it borrows from.

The consequence is worth stating rather than discovering: with LAYER 3 excluded,
the strongest available fence is bounded by what the operator can provision by
hand and what control can observe for itself. A design that reaches past that has
left the product's deployment story, whatever its security merit.

*The derivation is defeated by the shipped edge, and the fence that exists to
catch it cannot fire.* Measured on 2026-09-08. `deploy/ops/Caddyfile`'s control
block is `handle /v1/*` to the migration service plus an UNFILTERED catch-all
`handle { reverse_proxy control:9090 }`, so `/internal/workers/enrol` is forwarded
from the public edge. Control sets no `trust_proxy` and its default is false, so
`EnrolmentEnvelope`'s `ProxyFronted` arm cannot fire. Caddy sits inside the
declared enrolment network, so the peer check approves.

Two consequences, different in kind. The ADMISSION test bounds nothing once the
edge forwards the route - though the role key is still required, so this is not an
unauthenticated hole. Worse, the DERIVATION is defeated: every enrolment behind
the proxy records the PROXY'S address. That is exactly the "everything is the
proxy" collapse the arm was written to prevent, and it is silent because THE
ARM'S INPUT IS A DECLARATION RATHER THAN AN OBSERVATION. It is not an
interception - the recorded address is the proxy's, not an attacker's - but it
becomes a dispatch failure the moment the eligible set reads that column.

**Recommendation: refuse `/internal/*` AT THE EDGE, and gate that refusal.** Add a
`handle /internal/*` block ahead of the catch-all that answers without proxying.
It is the smallest change, it matches what `/internal` already means, and it fixes
BOTH consequences at once: internal callers then reach control directly on the
bridge network, so the observed peer is the real worker again and the derivation
recovers on its own. Because the fix is edge configuration that nothing in Rust
would notice drifting, it must be paired with a gate over the Caddyfile -
`tests/deploy_scripts_gate.sh` already enforces a collision boundary on that same
file, so the precedent and the place both exist.

*Rejected, with reasons.* Deriving proxy-frontedness from an OBSERVATION: control
cannot tell per request whether it sits behind a proxy - a forwarded header is a
hint an attacker also controls, and "every enrolment arrives from one address" is
a statistical signal, not a verdict on the request in hand. A fence that needs
several samples cannot refuse the first one. NARROWING the declared network to
exclude the edge: it works, but it demands the operator enumerate which addresses
inside their own subnet are not workers, which is exactly the error-prone
inventory the derivation exists to avoid.

*The stronger alternative, deliberately not recommended yet.* Bind enrolment to a
listener that is not the public one, so the route is unreachable from the edge by
construction rather than by configuration. That survives Caddyfile drift, which
the recommendation does not. It costs a second bind, its own config, and a compose
topology that routes to it - and it should be taken if enrolment ever gates
something an attacker wants, which is what the eligible set will make true.

The loopback arm was a SEPARATE and now-fixed defect, and conflating the two
would leave the real one unpaid. That arm sat above the network comparison, so
no declaration could admit a single-host deployment; it now rules through the
declared networks like any other address. Fixing it made a single-host
declaration EXPRESSIBLE. It did not make one EXIST.

*REJECTED, with reasons, so it is not re-proposed: having the GATEWAY sign an
attestation of its own routing decision* and letting the worker relay it to
control. It is superficially attractive because the gateway already decides
placement and already holds a signing key, so it looks like one more caller rather
than a new capability. It fails on this proposal's own terms. The 3.4 table gives
the gateway `ZeroShip-User` envelopes and nothing else, and gives control the env
leases and placement decisions; the attestation adds a second mint to the process
that terminates every creator-app request and moves the placement decision off
control, running the thesis backwards. F7 requires control to consult a view the
worker cannot write; under the attestation control consults nothing and verifies a
signature over a claim a third party computed, which 7.4 rules out in terms - a
narrowing computed by the narrowed party is replaced by a decision taken by the
other party, not by another derivation, and moving the deriving party one process
sideways is still a derivation. It also puts adversary-influenced input into the
decision, and it breaks the worker's background reconcile loop, which has no
dispatch to bind a token to and would fall back to serving a stale environment -
reintroducing SEC-7 silently. Worst of all it is invisible to its own guard: F2's
custody-manifest arm refuses a secret-classed field on the gateway's config, and
minting needs no new field because the key is already there, so the instrument
built to catch this shape would have stayed green.

*Open decision, NOT decided here: whether the lease is PULLED or PUSHED.* Pulled
means the worker asks on app load; pushed means control hands the environment
over when it assigns the app. Push is the stronger property, because a process
that cannot request secrets cannot be tricked into requesting the wrong ones.
Pull matches today's flow and is the smaller change. **Step 4 works either way;
the placement-decides inversion is what matters, and it is what this step
delivers.** Recorded in 10.2 so the decision has a register entry rather than
living only here, and F7's enforcement enumeration is written to be
direction-independent for the same reason.

*Premise:* steps 2 and 3, and per-instance worker identity, which is a
prerequisite rather than a part of this step: until a worker is distinguishable
from its peers there is nothing for the eligible-set comparison to compare.
*Red test:* F7's cross-node refusal arm - a worker holding a valid identity that
is NOT in the app's eligible set is refused the environment, with the control
being a worker that IS in the set and is served. Both arms are unwritable before
the prerequisite lands, which is what F7 already says about itself.

**Step 5. The session object, and MINT-READS-ROW.** Create `zeroship.sessions`
and `zeroship.grants`, move the refresh-family rotation algorithm onto the session
row unchanged, introduce `ValidatedSession`, and make auth the sole minter.
*Premise:* step 0's verdict is recorded, because this step is what makes the live
RLS fences unnecessary rather than merely removed.
*Red test:* revoke a session, then attempt a mint, and assert refusal; mutation -
delete the `revoked_at IS NULL` predicate and the test must fail. Plus the
compile-level check: removing the `ValidatedSession` parameter fails the build.

**Step 6. Move minting off the edge and delete the RP path.** The gateway becomes
a verifier and a relay; the anchor, the stash, the RP module and the gateway's
database credential go.
*Premise:* step 5, and the revocation feed with its fail-closed staleness gate
must land in the SAME change - see section 9. **The RP deletion itself no longer
waits on a decision:** D-A authorises it, and the blocker that stood here while
old decision 5 was open is gone. What still bears on this step is open decision 7
- if the platform session cookie does not attach across the SDK's same-site
iframe leg, this step ships the popup and top-level shapes only. That is a shape
constraint on the step, not a condition on the deletion.
*Red test:* an interactive redirect login followed by signout, asserting a
subsequent request is refused. Today that is a no-op because the anchor is
absent. Second arm: the dependency-closure gate (F3) refuses a database driver in
the gateway's closure.

**Step 7. Grants as rows, revocation as DELETE.** `token_revocations` and the
whole marker family go; the derived retention constant lands.
*Premise:* step 5.
*Red test:* F9's grant-revoke-then-refresh arm, which today succeeds; plus F17's
recomputation arm. This closes task #209 by deletion.

**Step 8. The status columns, their writers, and the variant gate.**
*Premise:* step 5, which creates the person row's `account_status` as well as the
grant row the audience-scoped `subject_status` hangs on. D-E settles the scope, so
this step carries no decision blocker; the refusal arm has one shape and it is the
paired one.
*Red test:* F11's variant-writer arm, red today; plus F10's pair - suspend a
person in one project, assert that project's session is refused and the platform
session still refreshes. Writing only the refused half would pass under a global
suspension too, so the allowed half is what binds the scope. Then the mirror:
suspend the platform audience, assert the platform session is refused and the
project session still refreshes. That direction is what proves the deploy-authority
act has a writer, and it is unwritable unless `grant_id` is NOT NULL.

**Step 9. Audience becomes the project.** Task #72. The unit is settled by D-B and
the entity now EXISTS, so this step carries neither a decision blocker nor an
entity blocker. **Both former blockers were discharged outside this design, not
by it**, which is why the step got smaller without anyone working on it:
`db/migrations-ts/20260906000000_organization_entity_model.ts` creates
`zeroship.projects` and `zeroship.project_members` and DROPS the app-scoped
`zeroship.app_members` from
`db/migrations-ts/20260702000200_control_tables.ts`, so the membership edge this
step used to have to move has already moved. The step is now unblocked and
unstarted, which is a different status from blocked.
*Premise:* step 5 and step 7, because the sector is stored on the grant row.
*What is left, stated as work rather than as a dependency.* First, move the
sector off the app: `crates/zeroship-auth/src/oidc/authorization_code.rs` and its
siblings `crates/zeroship-auth/src/oidc/refresh.rs` and
`crates/zeroship-auth/src/oidc/backchannel_logout.rs` still resolve it per app as
`COALESCE(aoc.sector_identifier, ac.client_id)` over `zeroship.app_oauth_clients`,
and a project-level sector is named under DELIBERATELY NOT HERE in the entity
migration's own header. Second, author `grants.project_id` and
`sessions.project_id` under the collation ordering the AUDIENCE section states -
add-and-collate in one migration, foreign key in a later one - or the lowering
refuses the pair.
*Red test:* a control differing in one variable - apps of one project produce
equal subjects, distinct projects produce unequal ones. Moving the sector back to
the app apex fails each half.

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

**The operator has taken that bet.** Decision D-C (section 10): no external
signer, the master signing key lives in the auth process. The sentence above does
not soften; its STATUS changes. It was the tiebreaker of an open question and it
is now a live ACCEPTED RISK, and the paragraph above it - the salt permanent and
unrotatable by construction, the signing key, the keyring, the TOTP at-rest key,
write access to users, sessions and grants - is the reasoning for accepting it,
not decoration around something resolved. **What REOPENS it is the design's own
tiebreaker: the platform holding regulated data.** At that point the question
stops being engineering taste and is decided by the data. A risk with no stated
reopening condition is a risk nobody ever re-examines, which is why the condition
is written here rather than left to judgement.

**The successor step is the REOPENING PATH**, named so it is not rediscovered and
so a reopening does not start from scratch: a signing service only auth can reach,
minting from a `ValidatedSession` with a keep-out interface. Sharpened by an
observation from the investigation - **the minter also owns the revocation store,
so a compromised minter can erase the record of what it minted.** The keep-out
interface must therefore be mint-with-witness only, no key export AND no
revocation write. The design is shaped so this is one seam behind one type, not a
rewrite. Nothing here is scheduled; this is what the reopening condition builds.

**Fail-closed revocation converts an auth outage into an authentication
outage.** Once a verifier's feed is older than the staleness budget it refuses
every authenticated route. A partition between a zone and auth takes every
`RequiredPrincipal::User` route in that zone to 503 after that interval;
anonymous routes keep serving. There is no configuration in which the revocation
bound and the outage tolerance are large together, because after the feed budget
the next bound is the assertion TTL and that is also the mint-load knob. This is
stated rather than hidden behind a cached fallback, because a cached fallback is
precisely how the tree ended up with a marker that expires before the capability
it revokes.

**Step 6 is the one place this trades fail-closed for fail-open if it is split.**
Today the gateway can fall back to a live database read on a revocation cache
miss. If the gateway's database credential is removed BEFORE the feed's
fail-closed staleness gate is in place, the window between those changes is a
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
session state.** True for zeroship-hosted apps by construction. It would stop
being true if the platform acted as an identity PROVIDER to third-party relying
parties that keep their own sessions - the OUTWARD direction. **That decision has
been taken: D-A deletes the capability, outward provision is not a roadmap item,
and the deletion is authorised rather than conditional.**

Read "outward" strictly, because this is the most confusable phrase in the
document and it sits beside mechanisms that are kept. It says nothing about
CONSUMING external providers, which is the opposite direction, is unaffected by
D-A, and gives no third party a session of ours to receive a logout for. If
outward provision is ever revived, back-channel logout is part of that feature's
design, not a regression against this one.

**A role split inside one process is not a boundary.** F16 is defence-in-depth
against a route-confusion bug in the OP. It is written here with that caveat
attached so nobody repeats it without one.

---

## 10. Decisions

Settled ones are recorded in 10.1 as D-A through D-E, each with what was decided,
who decided it, and why. The rest stay open in 10.2, which **keeps its original
numbering**: sections 7.2, 7.7, 9 and 11 address these items by number, nothing in
the tree checks that a cross-reference resolves, and renumbering would silently
re-point them. Settled items therefore keep their numbers as pointers rather than
being removed. The suspension-scope question was split out of item 1 as item 8
because the operator had not answered it then; it is answered now, as D-E, and
item 8 survives only as a pointer to that entry.

### 10.1 Settled by the operator

**D-A. The third-party OIDC provider capability is DELETED.** Decided by the
operator. Section 7.2 is AUTHORISED, not conditional.

*Why, and this reasoning is the durable part.* What exists today is not the
feature. It is OAuth ceremony between OUR OWN processes, gateway and auth,
and because that plumbing gives every app a client id, the third-party
capability falls out as an auto-provisioned side effect of deploy. There is no
registration, no third-party consent, no scope model, no documentation. Keeping
the side door open does not get the platform closer to the feature; it only keeps
the plumbing complicated. If an integration ecosystem becomes a product it is
built as a first-class feature with real registration and real secrets.

*Direction, recorded because the directions are easy to confuse and the
confusion could later be read as "the redesign deleted SSO".* D-A deletes the
OUTWARD direction only: the platform or a creator app acting as an OIDC provider
TO a third-party relying party. **Consuming external identity providers is
unaffected.** The design already classifies a federated identity as an
authentication method under the person, which is the adapter shape the operator
asked about. The live arms stay:
`crates/zeroship-auth/src/identity/oauth/google.rs`,
`crates/zeroship-auth/src/identity/oauth/github.rs`,
`crates/zeroship-auth/src/ui/oauth_google.rs`,
`crates/zeroship-auth/src/ui/oauth_github.rs`, and the link step in
`crates/zeroship-auth/src/identity/linker.rs`. There is no SAML anywhere in the
tree, so "SSO" here means those OAuth arms and nothing more.

**D-B. The audience unit is the Project.** Decided by the operator: project level.

*The second half of this decision as originally recorded - "with no organization
container above it for now, and task #82 stays deferred" - is now FALSE as a
statement about the tree, and is struck rather than reworded.* Organizations
landed on main in
`db/migrations-ts/20260906000000_organization_entity_model.ts` with rows,
membership, a closed role ladder, invites and a re-rooted billing subject; #82 is
not deferred. **The operator's CHOICE is untouched by that and is not reopened
here: the audience unit is still the Project.** What changed is the condition the
choice was recorded under, and item 10 below puts the consequent question to the
operator.

*Why the choice is recoverable, which is the reason it can be settled now:* a
later organization layer would be a NEW audience variant rather than a
re-derivation of the closed sum. That escape hatch is the one this document
still relies on, and it is now the live question rather than the contingency.
*What the settlement does NOT answer:* whether subjects are stable across a
project reparent. That consequence is an unresolved design question, not a
decision, and nothing else in this document states it. It is also currently
UNPERFORMABLE: `crates/zeroship-control/src/registry.rs` writes `apps.project_id`
once, at create, and no route updates it - so the operation the open question
asks about cannot be exercised even to observe what it does.

*What the unit enables, recorded and deliberately not designed here.* Per-project
external identity - "this project's users authenticate against this customer's own
provider" - does NOT exist today: provider configuration is platform-level, and
`db/migrations-ts/` contains no per-app or per-project provider table. The Project
is the natural place to hang such a configuration, and without a project unit
there would be no correct place for it. D-B makes it cheap; that is the whole
record.

**D-C. The master signing key lives in the auth process.** Decided by the
operator: no external signer.

*What the rejected alternative bought and cost:* an external signer buys "the salt
and the signing key are unreachable from any process handling an internet
request", and costs a process, a deployment and a failure mode. *Status:* section
9's statement of the bet stands and is not softened - it is now a live ACCEPTED
RISK rather than an open question. *What REOPENS it:* the design's own tiebreaker,
the platform holding regulated data. Section 9 names the successor step, which is
what a reopening builds.

**D-D. One account per human.** Decided by the operator: one account serves the
creator and the end user alike. "Creator" is a membership edge, not an identity
kind. The design already assumed this; it is now confirmed rather than assumed.

*Still UNVERIFIED, and the marker stands:* whether the namespaces are shared in
the tree TODAY. That is a measurement, its experiment is in section 11, and its
answer tells step 5 whether it adopts an existing shared namespace or has to merge
separate ones. *What D-D does NOT settle:* the scope of a suspension. That was
split out as item 8 so the account answer could not be read as answering it, and
it is settled separately as D-E.

**D-E. A suspension is PROJECT-SCOPED.** Decided by the operator: the platform
suspends a person within a project - not globally, and not per app.

*Why, and this reasoning is the durable part.* Everything else in the model is
already project-scoped. Audience is `Platform | Project(pid)`, the subject is
derived per project, the grant is one row per (person, audience), and the session
hangs off the grant. Scoping a suspension the same way means it is enforced by THE
SAME validating read as everything else, under MINT-READS-ROW: `subject_status` on
the grant row is a predicate in the statement that already resolves the grant. No
second enforcement path is created, and none has to be built.

*Per-APP was considered and rejected.* The model has no app-level object to hang a
status on, and a per-app status could not be enforced at mint at all, because the
mint does not know which app the person will visit next. Enforcing it would need a
dispatch-time mechanism consulted per request - a second enforcement path, and
precisely the shape this redesign exists to remove.

*Global was considered and rejected earlier.* One status per person means a report
against a person acting as an end user in someone else's project would stop that
person deploying their own apps.

*The consequence to state explicitly, because it will be asked.* A creator banning
a user from the creator's OWN app is not platform state. It is the creator's app
data under the creator's rules, written like any other app data. The platform
holds nothing for it - which is why nothing finer than a project needs to exist.

*Where this lands, so the decision is not made only in one place:*
`grants.subject_status` in 3.2 and the grant sketch, the NOT NULL `sessions.grant_id`
that makes the status a live predicate for the platform audience as well, the
feed's per-audience
suspension entry in 3.5, the gateway verify list and the refresh statement in 4.1,
the `PersonInAudience` selector and the suspend rows in 5, F10 and F11 in 6, and
step 8 in 8. Each of those names D-E.

*What the audience scope forced, recorded because it reads as an addition and is
not one:* naming `Platform` as a suspendable audience deleted a nullable column and
a special case rather than introducing a mechanism. The selector stopped being able
to name only a project, and the session row stopped exempting the platform case
from the foreign key every other session carries. See the `grant_id` paragraph in
3.2 for why the exemption was the defect.

**D-F. Worker identity becomes PER-INSTANCE, and it is a DISTINGUISHER rather than
a boundary.** Decided by the operator, who asked for a worker registry and health
monitor. Control writes the row; the worker generates an Ed25519 keypair at boot,
in memory, never on disk, and enrols the public half.

*Why, and this is the durable part.* Step 4's fence is unwritable without it. Every
replica loads the same `svc/worker` key file and mints byte-identical claims, so an
eligible-set comparison has nothing to compare. Per-instance identity is what makes
the narrowing WRITABLE - it is not itself the narrowing.

*What it does NOT buy, stated so nothing later reads it as more.* Enrolment
authenticates with the SHARED role key, so a holder of that key can enrol as many
instances as it likes and each is as genuine as the last. What it buys is
attribution, per-instance revocation, and a countable event. Every artifact that
describes it is forbidden from calling it a boundary. The only mechanism that would
make it one is item 12.

**D-G. No enrolment hardening may require LAYER 3 infrastructure.** Decided by the
operator, and it re-affirms a call already made in
`docs/proposals/2026-08-16-service-identity.md`.

*Why.* That proposal split identity into LAYER 2 - how a credential is PRESENTED,
X.509 over mTLS or a signed JWT, pick one - and LAYER 3, WHO gets a credential and
how, which is attestation, rotation and bootstrap, and where SPIFFE/SPIRE and cloud
workload identity live. This tree adopted LAYER 2 only, because "a default that
imposes infrastructure (a CA, a SPIRE cluster, a service mesh) is not deployable by
a user on a single VPS." Hardening enrolment IS a layer 3 question, which is
precisely why the bar has to be stated rather than assumed.

*A naming convention is not a commitment.* The `spiffe://` spelling in
`ServiceIssuer` has no issuing authority, no attestation, no rotation and no agent
behind it; nothing named SPIRE, SVID or workload API appears in any crate. It was
chosen so identifiers would carry unchanged into X.509 SANs if mTLS ever arrived.

*The consequence, stated rather than discovered.* With layer 3 excluded, the
strongest available fence is bounded by what an operator can provision by hand and
what control can observe for itself. A design reaching past that has left the
product's deployment story whatever its security merit.

**D-H. Four design calls settled while building step 4, recorded here because each
is easy to get wrong in the same direction.** Settled during implementation; the
reasoning for each is in step 4 and is not repeated.

- *The worker holds TWO keyrings* - the role one for the enrolment call only, an
  instance one for everything after - and that is FORCED, not preferred:
  `from_parts` builds the minter from the issuer, so the issuer cannot change after
  construction. The instance keyring's own-key check is REPLACED rather than
  inherited, because the inherited one is vacuous against a key generated seconds
  earlier.
- *Control resolves an instance key from its own ACTIVE row*, before verification,
  rather than gaining a callback into `zeroship-core`. That crate is a leaf of
  inter-service wire types and must not learn about databases. The `status` filter
  in that lookup IS per-instance revocation; there is no second mechanism.
- *The ring key and the eligible set land TOGETHER.* Control's ring is ordered by
  minted ring keys and the gateway's by configured URLs; those orders are
  unrelated, so shipping either half alone is a disagreement rather than a partial
  fence.
- *Observed liveness stays OUT of the declared `status` column.* `gone` is terminal
  with no path back, so a monitor writing it on a failed probe converts a transient
  blip into the permanent eviction of a healthy worker, worst during exactly the
  partition that caused the blip.

### 10.2 The numbered items, settled ones marked in place

1. **SETTLED as D-D, account half only.** The suspension half of this item was
   split out as item 8 and is settled separately as D-E. The number is kept rather
   than reclaimed, so that nothing after it shifts.

2. **SETTLED as D-C.** No external signer; the reopening condition is recorded in
   D-C and in section 9.

3. **SETTLED as D-B.** Project level. The clause about no organization container
   is SUPERSEDED rather than reworded: organizations landed, task #82 is not
   deferred, and the question that raises is item 10. The reparent-stability
   consequence recorded under D-B is still unresolved, and is now also
   unperformable - nothing updates `apps.project_id`, so the operation the
   question asks about cannot be exercised.

4. **The access-assertion TTL and the feed staleness budget.** OPEN, and
   deferrable to when step 6 is written. The first is the mint-load and
   partition-tolerance knob; the second is the recall bound. They are separate
   symbols in this design deliberately, but each needs a value, and each belongs
   to operations rather than to this document.

5. **SETTLED as D-A.** Section 7.2 stands and is authorised.

6. **Does `zeroship_worker` keep REPLICATION?** OPEN, and out of scope here. This
   design removes BYPASSRLS from that role. REPLICATION is on the same migration
   and is a data-plane question tied to CDC ownership. Section 7.7 addresses this
   item by number.

7. **Does the platform session cookie remain readable across the same-site
   iframe leg the SDK uses?** OPEN. The design assumes it does (same registrable
   domain, hence same-site). UNVERIFIED - a measurement nobody has taken; the
   experiment is in section 11, which addresses this item by number. If it does
   not, step 6's popup and top-level shapes are the only entries and the iframe
   leg is dropped.

8. **SETTLED as D-E.** A suspension is project-scoped: it stops the person as an
   end user in the project it names, and does not stop that person deploying their
   own apps. The status moved off the person and onto the grant row, so the answer
   is carried by the model rather than by a marking at each site; the sites that
   used to carry a provisional marking are listed in D-E and now name the decision
   instead. The number is kept as a pointer, per the rule above.

9. **Is the environment lease PULLED by the worker on app load, or PUSHED by
   control when it assigns the app?** OPEN, and deliberately not decided in step
   4. Push is the stronger property: a process that cannot request secrets cannot
   be tricked into requesting the wrong ones. Pull matches today's flow and is the
   smaller change. Step 4's placement-decides inversion holds under either, which
   is why the step does not wait on this. What DOES depend on it is which process
   presents an assertion on that edge, so F7's enforcement enumeration is written
   direction-independently: the placement equality is the invariant either way,
   and the `jti` claim rides whichever direction carries the request. Appended
   rather than inserted, per the numbering rule above.

10. **Now that organizations EXIST, does an organization layer change what a
    SUBJECT or a SUSPENSION is scoped to - or is Project still the right unit,
    with organizations sitting above it for ownership and billing?** OPEN, and
    deliberately not answered in this document. **This is not a request to
    re-litigate a settled decision.** D-B and D-E stand exactly as recorded. It
    is put here because both were decided when an organization above the project
    was a hypothetical the operator was choosing AGAINST, and it is now a live
    entity with rows, membership, a role ladder and the platform's entire billing
    subject re-rooted onto it. The operator answered a question about a choice;
    this is a question about a fact. Appended rather than inserted, per the
    numbering rule above.

    *The reading that keeps Project, and it is the stronger one on privacy.* An
    organization is a company and may run unrelated products; correlating one
    human across them is a leak rather than a feature, and an
    organization-scoped subject produces exactly that correlation by
    construction. That argument is already written into the tree - the module
    documentation of `crates/zeroship-core/src/organization_id.rs` states the id
    seeds no subject derivation, appears in no token `aud`, and is never handed
    to app code. On this reading nothing in section 3.2 moves: the audience sum
    stays closed as `Platform | Project(pid)`, and the organization is an
    ownership and billing root that identity never sees. D-E follows unchanged,
    because a conduct suspension belongs where the conduct happened.

    *The reading that reopens it, which is about suspension rather than about
    subjects.* A creator-side suspension is ALREADY organization-scoped and
    already shipped:
    `db/migrations-ts/20260906000100_apps_organization_and_billing_subject.ts`
    declares `organization_billing_status` with one row per organization over
    `active | past_due | suspended`, and
    `crates/zeroship-control/src/registry.rs` reaches every app of that
    organization by a plain equi-join on `apps.organization_id`. So the platform
    already stops a party at organization granularity for money, and D-E would
    stop a person at project granularity for conduct. Two suspensions at two
    scopes may be exactly right - they answer different questions about
    different parties, and this design argues elsewhere that a person-scoped
    column cannot express an audience-scoped state - or it may be the seam where
    a later reader finds two stop mechanisms and cannot say which governs.
    There is also one shape the Project unit cannot express at all: banning a
    person from every product a single company runs is, under D-E, a repeated act
    with no object that names it once.

    *Why the tree's own answer does not settle this.* The
    `crates/zeroship-core/src/organization_id.rs` paragraph RESTATES D-B; it does
    not re-derive it with organizations present. Reading it as the answer would
    be taking a consequence of the decision as evidence for the decision.

    *What is NOT in tension, stated so the question stays narrow.* The audience
    sum's closedness is unaffected either way - D-B's own recoverability clause
    says an organization audience would be a NEW variant, not a re-derivation.
    And pairwise subjects are still APP-scoped in the tree today
    (`crates/zeroship-auth/src/oidc/authorization_code.rs`), so whichever way
    this goes, step 9 is a move rather than a rewrite.

11. **Is a GATED EDGE REFUSAL enough to stop the proxy defeating the address
    derivation, or should enrolment bind to a listener that is not the public
    one?** OPEN. The measured defect and the recommendation are in step 4: the
    edge forwards `/internal/*` unfiltered, control's `trust_proxy` is unset so
    the `ProxyFronted` arm cannot fire, and the proxy sits inside the declared
    network, so every enrolment behind it records the PROXY'S address.

    *The trade, stated so a decision is possible.* Refusing the route at the edge
    is one config block plus a gate, and it fixes both halves at once because
    internal callers then reach control directly and the observed peer is the real
    worker again. Its weakness is that it is edge configuration: correct today,
    silently wrong if the Caddyfile drifts, which is why the gate is not optional.
    A separate non-public listener survives that drift because the route becomes
    unreachable by construction, at the cost of a second bind, its own config and
    compose routing. The trigger for preferring it is stated in step 4: take it
    once enrolment gates something an attacker wants, which item 4d makes true.

12. **Should worker instances take STATIC per-instance keys, provisioned by the
    operator, instead of the boot-generated ones D-F settles?** OPEN, and it is
    the only mechanism that turns enrolment from a distinguisher into a real
    boundary WITHOUT the layer 3 infrastructure D-G bars.

    *The trade.* Today a holder of the shared `svc/worker` key can enrol any
    number of instances. If each instance instead authenticated with a key only
    that instance holds, a role-key holder could not mint new ones - a genuine
    boundary. The cost is autoscaling: every new instance needs an operator step
    before it can join, which is exactly the property a fleet that scales on
    demand cannot have. This is a product decision about deployment shape, not a
    security decision with an obviously right answer, which is why it is here
    rather than settled.

    *What weakens the urgency, and it is worth weighing.* `svc/worker`'s allowlist
    grants carry NO app scope today, so a role-key holder already reaches every
    app's environment WITHOUT enrolling. Registration therefore grants an attacker
    little at present. The deadline is item 4d, when being enrolled starts deciding
    who RECEIVES dispatched traffic - and dispatch forwards cookies and the
    gateway-signed user envelope. Raise the fence before that lands, not after.

13. **The registry's unfinished lifecycle: four questions the operator owns.**
    OPEN, grouped because they share one root - `status` is a closed set of three
    values with a writer for one.

    - What promotes an instance to `gone`? Nothing writes it, `gone` is terminal,
      and control holds no `DELETE`, so rows accumulate one per boot, all reading
      `active`, none with a process behind them.
    - Should `ring_key`'s width be pinned in the schema? It is deliberately a
      minting decision rather than a wire constant today.
    - Should `(advertise_host, advertise_port)` carry a UNIQUE? It was omitted on
      purpose: a `gone` row would otherwise block the same worker re-registering
      after a restart.
    - What is the successor count k for the eligible set? It bounds spillover, so
      it is a capacity parameter that is also a security parameter.

---

## 11. Corrections

Claims made during this investigation that turned out wrong, and what is true.

**How each was established differs, and the difference matters.** C1 through C9
were reached by READING the working tree; nothing was executed for them. C10
through C12 were reached by EXECUTION - running a suite, applying a mutation and
confirming it changed the right thing, querying a live database - during the work
that implemented step 4. A read can only find a claim that contradicts the source;
only execution finds a claim the source appears to support.

Several kinds appear here. C8 records an error in what this document PRESCRIBED,
not in what it observed. C9 records an observation that was CORRECT WHEN TAKEN and
then repeated until it was not - a failure of process rather than of reading.
C10 through C12 are errors made while BUILDING what this document specifies, two
of them written into durable artifacts and corrected there rather than quietly
deleted.

The entries whose lessons outlive their subjects are C9, C10, C11 and C12. They
describe, in four different disguises, one failure: **a mechanism or a claim that
appears to rule on something and does not.** A fence above the check that would
have admitted a legitimate case; a citation naming a file that holds a type
rather than a call; a benefit listed beside true ones with nothing implementing
it; an observation reused past the moment it was true. Read those four together
before adding a fence, a citation, or a benefit to this document.

**C1. The service-assertion replay table IS provisioned. Earlier write-ups said it
exists only as a DDL string inside a test, and that claim was published carrying a
verification badge.** It is created, indexed and granted by
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
gateway role is created WITHOUT `bypassRls`, holds grants on each in
`db/migrations-ts/20260702000900_grants.ts`, and sets the matching
transaction-local GUCs in `crates/zeroship-gateway/src/rls.rs`. **Those are real
fences.** This design removes them, and the removal is legitimate only by the
argument made table by table: the anchor and gateway-session tables cease to
exist, and the identity table becomes single-writer at auth with no other tenant
to isolate. That
argument has to be made, not assumed - which is why step 0 exists.

**C3. The revocation sweep is not missing a DELETE grant.** An earlier note said
it would fail for lack of that grant;
`db/migrations-ts/20260811000000_auth_token_revocations_delete.ts` grants it. The
base grants file omits it, which is why the claim looked right.

**C4. `sessions::validate` names distinct functions in distinct crates, and only
the gateway's is unwired.** The GATEWAY's
`crates/zeroship-gateway/src/sessions.rs` version has no production caller, and
doc comments in `crates/zeroship-gateway/src/auth_token.rs` and
`crates/zeroship-gateway/src/router/auth.rs` say the request path deliberately
does not use it. The AUTH store's same-named function in
`crates/zeroship-auth/src/store/sessions.rs` has production callers under
`crates/zeroship-auth/src/ui/`. Earlier write-ups blurred them, which would have
deleted a live function.

**C5. The app-scoped control token has production callers, and every one of them
is inside the worker.** `crates/zeroship-worker/src/handler.rs` and
`crates/zeroship-plugin-workflow/src/client.rs`. The conclusion is unchanged and
slightly stronger: the plugin runs inside the worker process, so each derivation
is computed from a root the deriving process holds.

**C6. The workflow-advance reachability question already has an instrument.**
Earlier write-ups listed "is this edge reachable from the public internet" as
unverified with a new experiment attached. `tests/e2e_gateway_workflow_advance_authz.sh`
already boots real binaries and drives exactly that claim, including a phase that
puts the repository's Caddy rule in front of the gateway. **Check the instrument
built to answer the question before building another one.** UNVERIFIED by me: I
read the harness, I did not run it.

**C7. The retention ordering is inverted, not merely unlinked.** Task #209 says
the push window and the sweep retention "agree by coincidence". They do, and the
finding is worse: each is shorter than `ANCHOR_ABS_DAYS`, so for a teardown that
writes a marker without deleting the anchor, the marker is swept
while the capability it was written against is still alive.

**C8. A DESIGN ERROR IN THIS DOCUMENT'S OWN PRESCRIPTION, caught before
implementation.** Every entry above is a claim about the tree; this one is a
claim this document made about what to build, and it was wrong.

Step 2 read "wire service assertions on every internal edge", naming the
gateway-to-worker dispatch hop among them, and F6 named the same set with a
single-use `jti`. **As written that put a shared-store WRITE on the per-request
path.** `crates/zeroship-core/src/service_assertion.rs` makes `jti` REQUIRED and
single use a MUST - its module doc cites CVE-2020-15222 - and admits no per-edge
opt-out, no warn-and-continue arm and no configuration that turns a check off.
The claim is a write against a table shared by every replica of the callee:
`crates/zeroship-authn/src/service_replay.rs`, backed by
`db/migrations-ts/20260816000100_service_assertion_replay.ts`. So a faithful
implementer would have built exactly that, and would have been right to - the
profile admits no opt-out and the step said every edge.

The corroborating evidence, recorded rather than only the verdict:

- **It contradicted this document.** 4.1's steady-state block claims "no database
  read in any process" for every dispatched request, and the prescription would
  have added a write to that same path.
- **It pointed opposite to a sibling proposal from the same effort.**
  `docs/proposals/2026-09-05-gateway-central-database-decoupling.md`'s step 1
  deletes the family-revocation READ from the gateway's dispatch path. The
  redesign would have replaced a read a sibling proposal is removing with a
  heavier write on the same path.
- **The asymmetry is what makes it worse than the read it would have replaced.**
  A read can be cached, or served from a replica. A single-use claim cannot be
  cached, because caching it is what defeats the property. So the mitigation
  available to the mechanism being deleted is unavailable to the mechanism that
  would have replaced it.

*What changed:* 3.3 inventories the service assertion as a full profile and a
transport-only profile on the same mechanism; 3.6 states the network premise the
tiering rests on and the half it does not buy; step 2 assigns each edge a profile
by call rate and rules on the membership of the `/internal/` family; F6 is
restated by edge and by guard kind; step 4 becomes a lease. The security
properties are unchanged - this is a revision of which mechanism carries which
property on which edge.

**C9. THE PROJECT ENTITY EXISTS, AND THE PROCESS FAILURE THAT PUT ITS ABSENCE IN
THIS DOCUMENT IS THE DURABLE PART OF THIS ENTRY.** The stale claim itself is
trivia; how it survived is not.

*What is true.* `zeroship.organizations`, `zeroship.projects`,
`zeroship.project_members`, `zeroship.organization_members`,
`zeroship.organization_roles` and `zeroship.organization_invites` are created by
`db/migrations-ts/20260906000000_organization_entity_model.ts`, which also DROPS
`zeroship.app_members`. `db/migrations-ts/20260906000100_apps_organization_and_billing_subject.ts`
re-roots the billing subject from a human onto the organization, and
`db/migrations-ts/20260906000200_apps_project_ownership_key.ts` binds an app to
its project and organization with one composite key. `Resource::Project` and
`Resource::Organization` are live in `crates/zeroship-authz/src/resource.rs`. The
AUDIENCE section, the session sketch, step 9, decision D-B and numbered item 3
each asserted the absence; each is corrected in place.

*The failure.* The absence was established once, early, by a single grep for a
projects table in the migration corpus. **It was true when it was taken.** It was
then reported as a standing fact and repeated for hours - into this document,
into a task, into a sibling proposal - without ever being re-derived, while the
tree moved underneath it. Every downstream conclusion inherited the staleness
rather than the measurement: step 9's "ENTITY blockers" clause, D-B's deferral
half, and the repeated report that the organization container was deferred.

*The rule it violated, which this project already holds and was applying to
figures the whole time: a number carried forward goes stale.* A NEGATIVE
existence result is the worst case of that rule, and worth stating as its own
discipline. A figure at least invites "as of when"; an absence does not.
"There is no projects table" reads identically whether it was measured a minute
ago or a day ago, and it carries no version, no timestamp and no unit that
would look wrong once it drifted. So the discipline is stricter for an absence
than for a count: **re-derive it at the moment it is USED to block something,
not at the moment it is discovered.** The instrument here was one grep, and it
cost nothing to re-run.

*Blast radius outside this file, recorded rather than repaired here.*
`docs/proposals/2026-09-05-app-metadata-distribution.md` still presents the
`app_members` and `creator_billing_status` join as a measured current fact; both
of those tables are gone or re-rooted. It survives the citation gate because
that gate checks that a cited PATH exists, never that a described SHAPE still
does - which is the same blindness in a different instrument.

*What C9 does not license.* It re-decides nothing. The landed model contradicts
the CONDITION D-B was recorded under, not its content; section 10.2 item 10
states that as a question for the operator and leaves it open.

**C10. A FENCE I WROTE MADE A LEGITIMATE DEPLOYMENT INEXPRESSIBLE, AND I CALLED IT
POLICY.** `EnrolmentEnvelope::derive_address` refused a loopback peer in an arm
ABOVE the declared-network comparison. The operator could therefore declare
`127.0.0.0/8` and still be refused, so no single-host deployment could enrol -
which is every developer machine and every harness that launches a worker. The
arm read as caution and behaved as a defect: a fence whose declared input cannot
express a case the operator states outright is not a policy. Fixed at
`2f697c01e` by letting the networks rule on loopback, keeping `ProxyFronted` and
the unspecified-address arm unconditional.

*The lesson is in how it was caught.* A suite of REFUSALS cannot detect a fence
that refuses too much - every refusal arm passes against a fence that refuses
everything. The regression pair that binds it now differs in the declared
networks and nothing else, and under a mutation restoring the old ordering the
refusal arm stays GREEN while only the admit arm reddens.

**C11. THIS DOCUMENT NAMED THE WRONG CONSUMER OF A CAPABILITY, BECAUSE A TYPE
NAME WAS READ AS A CALL.** It recorded `crates/zeroship-gateway/src/oidc_rp.rs`
as the only non-definition consumer of `UserEnvelopeSigner`. That file names the
TYPE in a parameter position and never calls the accessor; every live call is in
`crates/zeroship-gateway/src/router/auth.rs`. Corrected at `e9d7f21a0`.

The error came from grepping the type name and treating a type-position match as
a consumer. **When the question is who EXERCISES a capability, search for the
CALL.** The same blindness has a sibling worth stating with it: a test that binds
a fence usually names the BEHAVIOUR, not the callee, so searching test files for
an internal function name can report "no coverage" over a suite that covers it.
Both were hit in one session.

**C12. AN UNEARNED CLAIM WAS WRITTEN BESIDE A TRUE ONE, WHICH IS WHERE THEY
SURVIVE.** `db/migrations-ts/20260907000300_worker_instances.ts` listed what
per-instance identity buys as "attribution, per-instance revocation, a countable
and RATE-LIMITABLE event". Nothing rate-limits enrolment: no budget, no quota, no
duplicate check on the endpoint. Corrected at `516531c68`.

The word sat one line below that file's careful refusal to call the mechanism a
boundary - which is exactly why it survived review. In a list of benefits, beside
claims that are true, an unearned one reads as delivered rather than as possible.
**Say "could" in a design and "does" only where something does.** Enrolment IS a
discrete event that COULD be bounded; that it is not remains open.

**Still UNVERIFIED, with the experiment for each.**

- *Whether the secret-write and env CLI verbs return 403 under a `zeroship login`
  token.* Derived from reads - the CLI's requested scope, the required
  action in `crates/zeroship-control/src/env_handlers.rs`, and the ceiling
  evaluation in `crates/zeroship-authz/src/eval.rs` - not observed. Experiment:
  against a live control plane run a secret list (expect allow) and a secret set
  (expect deny) and read each status. One variable apart, so it is an oracle
  rather than an anecdote.
- *Whether creators and app end users share one person namespace.* Control holds
  only select on the users table and reaches principals through the link and
  grant tables, which is consistent with one table serving each, but nothing
  states it. Experiment: check whether a principal id in the identity-link table
  can also appear as the global user id on an app identity row. This no longer
  DECIDES anything - D-D settles the design - but it verifies whether today's
  tree already satisfies the settled account model, which is what tells step 5
  whether it adopts an existing shared namespace or has to merge separate ones.
  The UNVERIFIED marker stands until it is run.
- *Whether the browser attaches the platform session cookie for the SDK's
  same-site iframe leg with third-party cookies blocked.* Same registrable domain
  means same-site, so Lax should apply, but I did not exercise it. Experiment: an
  arm in `tests/e2e_auth_ui.sh` driving the iframe leg in real Chromium with
  third-party cookies blocked. This decides open decision 7.
- *Whether any deployment outside this repository rate-limits the device
  authorization endpoint.* Established by enumeration over the auth crate and
  `deploy/ops/Caddyfile`; a production edge elsewhere could impose one.
