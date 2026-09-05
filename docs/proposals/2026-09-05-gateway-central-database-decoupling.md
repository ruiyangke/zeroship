# Gateway central-database decoupling

**Status. PROPOSED. NOTHING IN THIS DOCUMENT IS IMPLEMENTED.** No deletion has
happened, no table has moved, no grant has been revoked. What exists is the
mechanism this document proposes to shrink, plus one piece of machinery that
makes most of the shrinking a deletion rather than a construction: the
control-plane push feed already carries the only fact the gateway reads from
the central database on the per-request path.

This proposal is deliberately narrower than its sibling,
`docs/proposals/2026-09-05-app-metadata-distribution.md`. That document asks
how per-app metadata should be distributed to the edge at all. This one asks a
smaller question with a mostly-subtractive answer: what is the gateway still
doing with a connection to the platform's central PostgreSQL, and which of
those things should stop.

---

## The problem, in one paragraph

The gateway is the process placed closest to end users. The central database is
the process placed closest to the control plane. Every table the gateway
touches is a coupling between those two placements, and the coupling has two
independent cost terms that behave differently. The **connection** term scales
with the size of the edge fleet and not with traffic, because the pool is built
per worker thread. The **latency** term scales with the number of separate
transactions on the login path and not with the number of statements, because
every helper opens its own transaction bracket. Both terms are invisible in the
only topology this repository ships, where the gateway and the database sit on
one network, and both become the dominant cost the moment the gateway is placed
for proximity to users rather than proximity to the database. That is the
threshold this proposal is written against.

---

## How to read this document

**Provenance.** Every claim tagged MEASURED was re-derived by opening the file
on 2026-09-05, on `main`. Four investigation agents produced the underlying
survey; where their findings disagreed with the brief that dispatched them, the
correction is recorded in the corrections section rather than silently applied.
Two of those corrections invert the shape of a step, so read section 9 before
relying on section 5.

**Tags.**

- **MEASURED** - I opened the code and the claim came out of a file.
- **DESIGNED** - a shape this proposal argues for. It does not exist.
- **UNVERIFIED** - stated because it matters, not established. Each one names
  what would settle it.

**On numbers.** This document contains no magnitudes by policy. Where a
quantity is load-bearing, the constant is named with its symbol and its file so
a reader can read the current value, and where a quantity must hold over time
the proposal asks for a gate arm rather than asserting a value. The reasoning
is local and measured: the sibling proposal landed full of figures and the four
commits after it exist only to repair those figures, the last of which put
fresh figures in its own subject line. A number in a durable artifact is a
maintenance obligation that nothing enforces.

**On line numbers.** `tests/doc_citation_gate.sh` checks that a cited path
exists. By a decision recorded in its own header it does not check that a cited
line is the right line, and its header records two separate occasions on which
line citations in this tree drifted wholesale while staying green. This
document therefore cites **paths and symbol names only**. Where a line matters
to an argument, the code is quoted.

---

# 1. The problem

## What scales with what

MEASURED. **Connections scale with the fleet, not with load.** The gateway's
pool is an `Rc<Pool>` in a `thread_local!` keyed by DSN, built lazily on each
worker thread's first database touch (`crates/zeroship-gateway/src/db.rs`,
`checkout`). The per-pool ceiling is `gateway.db_pool_size` in
`crates/zeroship-gateway/src/config.rs`; the floor is set by the pool
constructor in `libs/compio-postgres/src/pool.rs`, which clamps its default
`min_idle` to the configured `max_size`. The number of pools per host is the
worker count, and the gateway sets none: there is no `.workers(` anywhere in
`crates/zeroship-gateway/src`, so ntex's default applies and the worker count
is the host's logical CPU count. Multiply by replicas.

The direction that matters: **adding gateway capacity to serve more users
consumes central-database connections even when the added capacity is idle**,
because the floor is per pool and the pool count is per core. Nothing in
`deploy/` sets `max_connections`, so the server default governs. This term is a
function of how the fleet is shaped, not of how much traffic it serves, and it
is the term that fails first at scale.

MEASURED. **Latency on the login path is dominated by the transaction count,
not the statement count.** Each gateway store helper opens its own transaction,
binds an RLS GUC through `crates/zeroship-gateway/src/rls.rs`, issues one
statement, and commits. `sessions::create`, `anchors::create`,
`identities::upsert` and `relay_alias_for` each pay that full bracket
separately, and the first is not batched with the rest because the RLS GUCs
differ: sessions and anchors bind the tenant-app setting, identities binds the
tenant-client setting. Two of them do not even share a pool checkout.

The direction that matters: **each additional separate transaction costs a full
round trip per statement in its bracket, so the cost is set by how many
transactions the path opens, and folding statements into fewer transactions
buys more than removing statements does.**

## Where the sign flips

MEASURED. The compose topology places the gateway, auth and PostgreSQL on one
Docker network (`deploy/compose/docker-compose.yml` declares a single
`networks:` key), and no file under `deploy/` configures a region or a zone. In
that topology both terms above are small enough to be invisible, and no
measurement in this tree says otherwise for this path.

**The flip happens when the gateway is placed apart from the database, and it
is a placement decision, not a load one.** The moment a gateway serves users
from a location that is not the database's location, the login path pays its
transaction count multiplied by the inter-zone round trip, and the connection
floor is paid over a link that is expensive to hold open. Nothing in the
current deployment forces that choice; the sibling proposal's multi-zone
section is where it gets forced.

**A caution about the figure that circulated during this investigation.** The
cross-continent multiplier used in the design conversation comes from a table
in `docs/architecture/data-system.md` about creator-database co-location. It is
illustrative, it describes a different path, and it was never a measurement of
gateway login. Any magnitude for this path is currently unmeasured. If a
magnitude is needed to justify a step, measure it against a gateway and a
database that are genuinely apart; do not carry that table's figure forward.

---

# 2. What the gateway actually uses the database for

MEASURED. The gateway role is granted six tables in
`db/migrations-ts/20260702000900_grants.ts`. It uses five. The sixth,
`signing_keys`, has **zero references in `crates/zeroship-gateway/src`** - the
gateway obtains OP keys over HTTP through the JWKS client on `GateState`
instead. That grant is dead and should be revoked whatever else happens.

MEASURED, and it is worth stating because a reader may find the opposite in the
deployment: the comment above the gateway's DSN in
`deploy/compose/docker-compose.yml` describes the role as one that "only reads
sessions". The gateway writes five tables. Believe the code.

## Classified by rate

| Table | Rate | What the data IS |
| --- | --- | --- |
| `token_revocations` | per request, behind a cache | a per-`(client_id, sub)` cutoff instant |
| `gateway_sessions` | per login, and per reload on the mint path | an inventory record another service reads |
| `app_session_anchors` | per login, plus a read-modify-write per reload | the encrypted server-held OP refresh family |
| `app_user_identities` | per login | a pairwise mapping another service owns |
| `audit_events` | per back-channel logout | a security event record in another service's table |

## Classified by what the data IS, which is the classification that decides the work

**A globally replicable fact.** `token_revocations` is a small, append-mostly,
monotonic set of cutoff instants. Nothing about it needs to be read from a
database; it needs to be *known*. Section 3 shows it already is.

**A record whose only reader is elsewhere.** `gateway_sessions` is written by
the gateway and read by auth. `audit_events` is written by the gateway into a
table auth owns, writes, and reaps. In both cases the gateway is reaching into
another service's storage to hand it a row.

**A mapping another service owns.** `app_user_identities` carries the pairwise
subject and the relay alias. Auth holds every lifecycle verb on it. Section 4
shows the gateway's write is already redundant.

**Genuinely gateway-owned mutable secret state.** `app_session_anchors` holds
the OP refresh family encrypted under a key the gateway alone has
(`anchor_enc_key` on `GateState`, `crates/zeroship-gateway/src/lib.rs`), whose
stated invariant is that the family never leaves the gateway in plaintext. This
is the one table where the gateway is the right owner, and section 5 argues it
stays.

---

# 3. The finding that changes the shape of the work

**The pushed snapshot already carries the per-request fact. The per-request
half of this proposal is a DELETION, not a construction.** That is the headline
and it should be read before the sequence.

MEASURED. Two mechanisms compute the same predicate on the same path.

- **Pushed.** `RouteCache::credential_authentication_allowed` in
  `crates/zeroship-gateway/src/sync.rs` consults a `family_revocations` map
  populated by `update_snapshot` from the control-plane pull, and rejects with
  `.is_none_or(|revoked_after| *revoked_after <= issued_at)`.
- **Polled.** `family_revocation_decision` in
  `crates/zeroship-gateway/src/router/auth.rs` consults the short-TTL
  `RevocationCache` in `crates/zeroship-authz/src/wrapper_revocation.rs` and on
  a miss issues a `MAX(revoked_after)` select against `zeroship.token_revocations`,
  rejecting with `family_revoked_at`, which is
  `revoked_after.is_some_and(|ra| ra > iat)`.

Those are the same rejection rule. Four legs make the polled one removable.

**Order.** MEASURED. On both dispatch arms the pushed check runs first and
returns early. In `resolve_raw_op_bearer` the pushed check returns
`BearerOutcome::Invalid` before the database read; in
`resolve_app_session_user_header_inner` it returns `CookieOutcome::None` before
the database read. Both functions are in
`crates/zeroship-gateway/src/router/auth.rs`. The pushed check also runs a
second time after the read on both arms.

**The database read is not a staleness fallback.** MEASURED, and this is the
leg that most looks like it should go the other way.
`credential_authentication_allowed` opens with

```rust
if !self.sync_freshness.is_fresh(freshness_budget) {
    return false;
}
```

so a stale snapshot rejects, and rejection short-circuits before the read. **The
database read is unreachable in exactly the case where the pushed data is
untrustworthy.** It covers no fail-open.

**Retention is symmetric, and the brief said otherwise.** MEASURED. The pushed
set is bounded by the interval in the family-revocation query in
`crates/zeroship-control/src/registry.rs`. The table is swept at the constant
`WRAPPER_REVOCATION_RETENTION_HOURS` by `sweep_expired_families` in
`crates/zeroship-authz/src/wrapper_revocation.rs`, wired into auth's cron in
`crates/zeroship-auth/src/cron/token_sweep.rs`. The two boundaries are the same
constant expressed twice. **The database does not see more history than the
push.** Rounding matches too: the control-plane query ceilings to whole seconds
and so does `revoked_after_for`, so the polled path opens no gap the pushed one
does not.

**Window completeness.** MEASURED. The pushed window only drops a revocation
that matters if a live credential's `iat` is older than the window, and both
credential TTLs - `SESSION_TOKEN_TTL_SECS` in
`crates/zeroship-gateway/src/session_token.rs` and `ACCESS_TOKEN_TTL_SECS` in
`crates/zeroship-auth/src/oidc/issuer.rs` - are far inside it, with the issuer's
own ceiling `PLATFORM_TOKEN_MAX_TTL_SECS` in
`crates/zeroship-core/src/device_grant.rs` still inside it. No writer
back-dates: every production writer stamps the PostgreSQL clock and upserts
through `GREATEST`, so a marker only moves forward.

**One caveat, stated rather than buried.** The session cookie's `iat` is not its
mint time. `crates/zeroship-gateway/src/session_token.rs` carries

```rust
iat: m.credential_iat.min(now),
```

which inherits the originating credential's issuance instant across re-signs.
The clamp is upper-only; nothing enforces a floor. The completeness argument
therefore rests on every production feeder being a token minted in the same
request, which the investigation verified for all three feeders. Even if that
were breached, the database read would not be a backstop - the same sweeper
deletes the marker at the same boundary, so both mechanisms would accept the
stale-`iat` cookie identically.

## What deleting the polled read actually costs

Timeliness, in two narrow places, and nothing else.

- **Same-node signout.** MEASURED. The gateway's own revocation writers bust
  the local cache immediately (`RevocationCache::invalidate`, called from
  `crates/zeroship-gateway/src/browser_auth.rs` and
  `crates/zeroship-gateway/src/backchannel_logout.rs`), so a signout is visible
  to the next request on that process before the push could carry it.
- **A control-plane stall.** There is a window between the poll interval and
  the staleness budget (`max(poll_interval * factor, floor)` in
  `crates/zeroship-core/src/readiness.rs`) during which the snapshot is honoured
  while stale and the database read would be current. After the budget expires
  the pushed check rejects everything anyway.

**Both are recoverable without any database read**, by applying the gateway's
own revocation writes into the local `family_revocations` map instead of into a
separate cache. That is the design in step 1.

MEASURED, and it changes which site to delete first: the third polled site is
not cache-backed at all. `session_cookie_family_revoked` in
`crates/zeroship-gateway/src/auth_token.rs`, called from the
`GET /__zeroship/auth/session` fast path, performs a pool checkout and a select
on **every** call with no cache consultation anywhere. It is the most expensive
of the three sites to keep and the cheapest to remove.

---

# 4. Ownership

## `app_user_identities` belongs to auth, and the gateway's write is already redundant

MEASURED. Auth holds every lifecycle verb on this table: the mint inside
`mint_access_token`, the relay-alias mint and revoke in
`crates/zeroship-auth/src/store/relay.rs`, the password-reset teardown in
`crates/zeroship-auth/src/identity/password_reset.rs`, account deletion in
`crates/zeroship-auth/src/store/users.rs`, and the userinfo and introspect
reads. Auth's own comments say it owns the table.

MEASURED. **The upsert SQL is forked byte-identical across two crates.**
`access_identity_upsert_sql` in
`crates/zeroship-auth/src/oidc/authorization_code.rs` and
`identity_upsert_sql` in `crates/zeroship-gateway/src/identities.rs` are both
`const fn`s returning the same string literal with the same line-continuation
spelling. Nothing links them and nothing keeps them equal.

MEASURED, and **this refutes the premise the investigation started from.** The
gateway's own comment justifying its write says the cookie path "retains its own
write because it also mints sessions from external providers", and cites the F1
password-reset teardown. The code contradicts it:

- `mint_session_from_code` in `crates/zeroship-gateway/src/auth_token.rs` calls
  `exchange_code_public` as its unconditional first step, and
  `exchange_code_public` in `crates/zeroship-gateway/src/oidc_rp.rs` appends
  `grant_type=authorization_code` and posts to the OP's token endpoint.
- The interactive callback in `crates/zeroship-gateway/src/router/dispatch.rs`
  likewise runs `finish_callback`, which posts the same grant to the same
  endpoint, before `issue_interactive_session_cookie`.
- That endpoint's `authorization_code` arm calls `mint_access_token`
  unconditionally, and `mint_access_token` performs the identical upsert and
  commits before the gateway sees a success status.

External-provider federation does not escape this: Google and GitHub are
upstream of the OP (their routes are registered in
`crates/zeroship-auth/src/server.rs`), so such a login still redeems an OP
authorization code at the OP's own token endpoint.

**Both gateway writes are redundant today**, and because both upserts derive the
pairwise subject with the same pure function over the same sector column, the
gateway's copy cannot even repair a mismatch - its own `ON CONFLICT ... WHERE`
guard turns a divergence into a server error rather than a differing row.

MEASURED. `identities::lookup_pairwise_sub` in the same module has zero
production callers; every caller is a test. It should be deleted with the
upsert.

## The three-way `revoked_at` race, which nobody defends

MEASURED. Three services write `app_user_identities.revoked_at`: auth
un-revokes on re-consent, control revokes on grant deletion
(`crates/zeroship-control/src/oauth_grants_handlers.rs`), and the gateway
un-revokes as a side effect of every upsert. Control's own comment says the
write "does NOT serialize against auth's re-consent un-revoke (no shared
advisory lock)" and that the race is "closed STRUCTURALLY on the read side"
because `resolve_active_alias` forwards only when a live `oauth_grants` row
still exists.

Read that carefully: **the column has three writers and is deliberately not the
authority.** Deleting the gateway's writer removes one arm of a race that its
own participants already route around. That is a strictly good outcome and it
costs nothing.

## The pairwise salt in three processes: a confidentiality argument, not a tidiness one

MEASURED. The same salt is loaded by auth (`auth.pairwise_salt_file` in
`crates/zeroship-auth/src/config.rs`), by control (`pairwise_salt` in
`crates/zeroship-control/src/config.rs`) and by the gateway (`pairwise_salt` on
`GateState` in `crates/zeroship-gateway/src/lib.rs`). `derive_pairwise` in
`crates/zeroship-core/src/auth/mod.rs` is a pure HMAC over the canonicalized
global user id and the sector identifier. The salt is the only secret in it.

**Be precise about what an attacker gains from the salt, because overstating it
would justify the wrong work.** With the salt an attacker can compute any
user's pseudonym for any sector, given a candidate global user id. That yields
two things:

1. **Linkability.** The pseudonyms a user carries across every app become
   computable from one another, which defeats the exact property the pairwise
   scheme exists to provide. The identity graph of the platform becomes
   readable to anyone holding the salt and any set of observed subjects.
2. **Membership confirmation.** For a named user and a named app, an attacker
   can test whether that user is present by deriving the value and matching it
   against subjects visible in rows, headers or logs.

**It does not yield authentication.** The pairwise subject is an identifier, not
a bearer credential; deriving one does not mint a session. So the loss is
confidentiality of the identity graph, not a bypass - and that is what makes
this a consolidation argument rather than an emergency. Every additional
process holding the salt is another process whose compromise deanonymizes the
whole platform's users, and the blast radius of an edge process is the largest
of the three because it is the one exposed to the internet.

MEASURED, and it bounds what step 2 buys: **removing the gateway's identity
write does not remove the gateway's need for the salt.** The gateway still
derives in `pairwise_sub` on the session-mint path, in the back-channel-logout
handler and in the signout handler, all in
`crates/zeroship-gateway/src`. Whether the gateway could take the pairwise
subject from the OP token response instead of deriving it - the bearer arm
already treats the OP's `sub` as pairwise and checks it with
`is_pairwise_subject` - is **UNVERIFIED**. What would settle it: enumerate every
production `derive_pairwise` call in the gateway and, for each, establish
whether an OP-issued subject for the same `(user, sector)` is already in scope
at that point. If all three are, the gateway can drop the salt entirely, and
that is a larger security win than anything else in this document.

---

# 5. The sequence

Five steps, ordered by value. Each is independently landable. Each names the
red test that fails before it and passes after; where a step's test is a
*premise* test that must be green before the change rather than after, that is
said explicitly, because the two are not interchangeable.

## Step 1. Delete the polled family-revocation read; apply gateway-side revocations locally

DESIGNED. Remove `family_revocation_decision` from both dispatch arms in
`crates/zeroship-gateway/src/router/auth.rs` and
`session_cookie_family_revoked` from the `/session` fast path in
`crates/zeroship-gateway/src/auth_token.rs`. In the same change, have the
gateway's own revocation writers - the signout path in
`crates/zeroship-gateway/src/browser_auth.rs` and the back-channel logout path
in `crates/zeroship-gateway/src/backchannel_logout.rs` - insert into the local
`family_revocations` map on `RouteCache` instead of invalidating a separate
cache. Delete `RevocationCache` from
`crates/zeroship-authz/src/wrapper_revocation.rs` if the gateway was its only
consumer; keep `sweep_expired_families`, which auth's cron drives.

**Buys.** The last per-request central-database dependency on the dispatch path,
and the uncached checkout on the `/session` fast path. After this step, a
gateway serving authenticated traffic needs the control plane and needs
PostgreSQL for nothing on the request path.

**Costs.** Cross-node revocation timeliness becomes bounded by the poll
interval (`gateway.poll_interval` in `crates/zeroship-gateway/src/config.rs`)
rather than by the cache TTL constant in
`crates/zeroship-authz/src/wrapper_revocation.rs`. Both are configured
intervals of the same order; which is acceptable is open decision 1.

**RED TEST.** Two, and both fail today.

1. An authenticated dispatch performs **no pool checkout**, asserted over a
   sequence of requests spanning longer than the revocation cache TTL constant
   so that the cache provably expires mid-test. Today the post-expiry request
   reads the database.
2. A revocation written by this gateway process is honoured by the same
   process's next request **with the database unavailable**. Today the local
   `family_revocations` map is populated only from the control pull, so the
   local write is invisible to it and the test fails.

**This needs a gate arm, not a comment.** The property "the authenticated
dispatch path performs zero database checkouts" is exactly the kind of claim
this repo protects mechanically. The arm should count production
`crate::db::checkout` call sites reachable from the dispatch entry points and
rule on that set, declaring the number it ruled on and a floor per
`tests/lib/gate_arms.sh`. Do not assert the count in prose; let the arm
re-measure it.

## Step 2. Delete the gateway's `app_user_identities` writes and revoke its grant

DESIGNED. Delete both `identities::upsert` call sites in
`crates/zeroship-gateway/src/auth_token.rs`, delete `identities::upsert` and
`identities::lookup_pairwise_sub`, and revoke `insert` and `update` on
`zeroship.app_user_identities` from `zeroship_gateway` in a new forward
migration under `db/migrations-ts/`.

**Buys.** Removes the byte-identical SQL fork, removes a full transaction from
the login path, removes one arm of the three-way `revoked_at` race, and narrows
the edge process's write surface on a table carrying the identity graph.

**Costs.** None, *if* the premise holds. Auth needs no change: its `insert`
privilege was granted separately in
`db/migrations-ts/20260818000300_credential_lifecycle.ts` precisely so it could
create the row, and it sets the tenant-client GUC immediately before its upsert.

**PREMISE TEST, which must be green BEFORE the deletion.** Drive a full
code-exchange login against a live auth with the gateway's write path disabled,
and assert the `app_user_identities` row exists with the expected pairwise
subject. If that fails, the premise in section 4 is wrong and this step does not
land. `tests/e2e_dev_vs_deployed_login.sh` is the harness closest to this shape.

**RED TEST, which fails before and passes after.** Run a full login with
`insert` and `update` on `zeroship.app_user_identities` revoked from the
gateway role. Today the gateway's upsert fails and the mint returns an internal
error; after the change the login succeeds and the row is present, written by
auth.

## Step 3. Fix the relay-alias ordering bug in auth, then delete the gateway's alias lookup

DESIGNED, and it fixes a live defect rather than only moving work.

MEASURED. `mint_alias_at_consent` in `crates/zeroship-auth/src/store/relay.rs`
begins by selecting the identity row and returns `Ok(None)` when it is absent:

```rust
let Some(row) = existing.first() else {
    // Row not written yet. NOTHING mints it afterwards - see the arm in
    // this function's doc comment. The gateway's "lazy-mint on read-through
    // miss" that this line used to name does not exist.
    return Ok(None);
};
```

Its only production caller is the consent accept handler in
`crates/zeroship-auth/src/ui/consent.rs`, which runs **before** the
authorization code is issued and therefore before the identity row exists. The
handler logs a warning saying so. On a first login the alias is never minted,
nothing mints it later, and consent short-circuits on every subsequent login -
so the gateway spends a full transaction per login to read a null and project
an empty email to the app.

MEASURED, and it refutes the stated reason for not fixing it in place. The
module doc says the insert at consent "needs `pairwise_sub` ... the auth issuer
can derive it once a token is issued". The consent handler already takes the
issuer and already holds the user id and the client id, and
`Issuer::pairwise_subject` is pure. The only missing input is the sector, which
`load_native_oauth_client` does not select.

The step is therefore: mint the alias at consent by widening that client load to
carry the sector, then return the alias in the OP token response so the gateway
reads it from a call it already makes, then delete `relay_alias_for` and
`identities::lookup_relay_email` from the gateway.

**Buys.** An app stops receiving an empty email where it should receive an
alias. A pool checkout and a transaction leave every login.

**Costs.** The OP token response shape changes, which is free pre-launch
provided every producer and consumer changes in the same patch. It touches
`crates/zeroship-auth/src/oidc/refresh.rs` as well, because `relay_alias_for`
has a caller on the refresh-driven rotation path, and that file does not read
or write `app_user_identities` today.

**RED TEST.** An end-to-end login asserting the app receives a relay-domain
address rather than an empty string. It fails today for a first-consent user,
which is the common case. `tests/e2e_dev_vs_deployed_login.sh` already covers
the deployed-versus-dev shape this defect was originally observed in.

## Step 4. Move `gateway_sessions` to auth, and fix it on the way rather than porting it

DESIGNED. The table is written by the gateway and read by auth. Have the
gateway hand auth the session record on the call it already makes, and drop the
gateway's `insert` and `update` grant.

MEASURED, and it corrects the brief: **the function has no production caller,
but the table does have a production reader, in another crate.**
`sessions::validate` in `crates/zeroship-gateway/src/sessions.rs` is called only
by tests. But `list_by_user` in `crates/zeroship-auth/src/store/sessions.rs`
selects the table for the user's active-session list, and
`revoke_one_for_user` deletes from it, both mounted as routes in
`crates/zeroship-auth/src/server.rs`.

MEASURED. Two live defects travel with this table and must not be ported.

1. **Nothing in production slides `idle_expires_at`.** The only slider is the
   uncalled `sessions::validate`; auth's same-named function targets
   `idp_sessions` via `VALIDATE_SESSION_SQL` in
   `crates/zeroship-auth/src/store/sessions.rs`. Since `list_by_user` filters on
   `idle_expires_at > NOW()`, an app session disappears from the user's session
   list after the idle window regardless of activity, and its per-session revoke
   becomes unreachable. An index for a slide that does not happen exists in
   `db/migrations-ts/20260702000600_constraints_indexes_fks.ts`.
2. **The table has no retention and stores the real email.** The mint path
   inserts rather than upserts, `checkSession` in `sdks/auth/src/client.ts`
   probes the mint path on every fresh page load, and no sweeper in
   `crates/zeroship-auth/src/cron/` names either session table.

**Buys.** A transaction leaves the login path, the gateway loses two more write
grants, and the row lands next to its only reader.

**Costs.** A new hop unless the record rides the token exchange the gateway
already performs. Deciding whether the table should exist at all is open
decision 2.

**RED TEST.** List a user's app sessions after a span longer than the idle
window during which the session was continuously used, and assert the session is
still listed and still revocable. It fails today.

## Step 5. Leave `app_session_anchors` where they are, and fix the single-flight scope instead

DESIGNED, and it is deliberately not a move.

MEASURED. The anchor path is a read-modify-write whose zero-rows return **is**
the revocation fence. `update_rotated_family` in
`crates/zeroship-gateway/src/anchors.rs` documents that a zero return means the
anchor was revoked between the caller's `read_live` and the persist, and the
caller in `crates/zeroship-gateway/src/auth_token.rs` consumes it as
`LoginRequired`. The paired check reads the family marker **directly from the
database**, and the comment says why. Quoted with two elisions, one of which
drops the comment's inline restatement of the cache TTL - that value is the
constant `REVOCATION_CACHE_TTL_SECS` in
`crates/zeroship-authz/src/wrapper_revocation.rs`, and the comment's copy of it
is exactly the kind of transcribed number this document declines to carry:

```
//       torn down DURING the rotation [...] We read
//       the marker DIRECTLY from the DB (NOT the [...] stale local cache),
//       since this is the authoritative cross-node revocation record.
```

**A pushed snapshot cannot satisfy that read, and an HTTP hop would widen a
TOCTOU window that was closed on purpose. The anchor writes must not be queued,
made asynchronous, or routed through another service.** This is the step that
says do not do the obvious thing.

What should change is the coalescing scope. MEASURED: `RotationSingleFlight` in
`crates/zeroship-gateway/src/anchors.rs` lives in a `thread_local!`, so there is
one map per worker thread per node, and the worker count is the logical CPU
count. Its own doc comment justifies that scope by claiming cross-thread
concurrency "is absorbed by the short cached wrapper + OP's rotation grace" -
while the module header of the same file records that the cached-wrapper columns
were deleted in the BFF redesign. **Believe the code: only the OP grace remains,
and it is single-use.**

UNVERIFIED, and it is the most consequential unverified claim in this document.
The investigation's reading of the two state machines says that concurrent
same-anchor mints landing on different threads present the same ciphertext,
that the OP routes the loser through `replay_or_kill` in
`crates/zeroship-auth/src/oidc/refresh.rs`, that `kill_family` runs, and that
the gateway maps the resulting `invalid_grant` to `LoginRequired` and deletes
the anchor - so a reload storm logs the user out. **This was established by
reading, not by observing a kill.** What would settle it: concurrent
`GET /__zeroship/auth/session?mint=1` requests sharing one anchor cookie against
a live gateway with more worker threads than concurrent requests and a live
auth, asserting whether a `kill_family` log line appears. No test in
`crates/zeroship-gateway/tests/auth_token_anchors_test.rs` exercises this; the
only single-flight test is an in-process unit test that never touches a real OP.

**RED TEST, conditional on the above being confirmed.** Concurrent same-anchor
mints across worker threads, asserting no family kill and no `login_required`.

## The durable outbox was considered and does not fit

MEASURED, so that this is not re-proposed. A real outbox exists - a redb
write-ahead log appended before publish, with a drain task and a bounded retry
backlog, in `crates/zeroship-metering/src/outbox.rs` - and the gateway already
runs one via `build_usage_outbox` and `spawn_outbox_task` in
`crates/zeroship-gateway/src/main.rs`. It does not fit these writes for four
independent reasons, the last of which is decisive: it is hard-typed to
`UsageEvent`; it publishes to a Kafka-family stream while every consumer of
these rows is a SQL reader; redb is single-writer so a second one needs its own
WAL identity; and **it is disabled in the topology this repository ships**,
because no service under `deploy/` sets metering brokers, so `build_usage_outbox`
takes its disabled arm and `spawn_disabled_drain_task` discards everything.

---

# 6. What this does NOT do

- **It does not remove the gateway's database.** `app_session_anchors` stays,
  and with it the pool, the RLS bind helpers in
  `crates/zeroship-gateway/src/rls.rs`, and the connection floor described in
  section 1. The connection term is *reduced* by removing per-request and
  some per-login checkouts; it is not eliminated.
- **It does not route the RLS-fenced tables through auth.** That is the maximal
  version of this proposal and section 7 argues against it.
- **It does not change the control-plane route feed's shape.** Whether that feed
  should be a full-table pull at all is the sibling proposal's question, and
  step 1 makes the gateway depend on it *more*, which is an input to that
  design and not a decision this document takes.
- **It does not change the pairwise-subject scoping.** Issue #72 (subjects are
  scoped per app, so two apps sharing a database cannot agree on who a user is)
  is untouched.
- **It does not address app-name case handling.** Issue #208 is untouched.
- **It does not start the CDC relay**, and nothing here needs it.
- **It does not change the gateway's `iss` identity, the session cookie format,
  or the anchor encryption scheme.**
- **It does not build a durable write queue.** See the outbox note above.

---

# 7. Risks and what gets worse

## The decisive argument against the maximal version, and it is a boundary argument

MEASURED. `zeroship_gateway` is created **without** `bypassRls`;
`zeroship_auth` and `zeroship_control` are created **with** it, in
`db/migrations-ts/20260702000100_schema_roles_extensions.ts`. Three of the
gateway's tables - `app_session_anchors`, `app_user_identities` and
`gateway_sessions` - carry forced RLS with a `tenant_isolation` policy in
`db/migrations-ts/20260702000800_policies_rls.ts`.

The gateway leans on that as a **fence**, not as hygiene. The signout handler in
`crates/zeroship-gateway/src/browser_auth.rs` says so in place: the RLS-scoped
anchor read "both loads the anchor AND enforces the former post-hoc
`anchor.app_id == route.app_id` check". A cookie replayed against the wrong app
resolves to nothing because PostgreSQL says so.

Auth's own code states the other half: `list_by_user` in
`crates/zeroship-auth/src/store/sessions.rs` notes that "the role is
`BYPASSRLS`, so the gateway table's per-tenant policy does not apply".

**Routing those writes through auth does not relocate the fence. It deletes
it**, and replaces a boundary PostgreSQL enforces with one an HTTP handler in a
BYPASSRLS process promises. That is precisely the shape AGENTS.md's "privilege
follows the PROCESS, not the function" invariant refuses. Every other cost in
this analysis is a tuning question; this one is a boundary being replaced by the
appearance of a boundary. It is why step 5 keeps anchors in the gateway and why
step 4 must move the session *record* without moving the anchor read that shares
its RLS binding.

## Availability moves in both directions, and the steps differ

MEASURED. Steady-state authenticated dispatch today needs PostgreSQL and needs
auth for nothing: the cookie arm verifies locally, and its comment says "NO DB,
NO network" apart from the revocation gate. So auth can be entirely down and
logged-in users keep working.

- **Step 1 raises availability.** The pushed and polled checks run in AND, so
  today's availability is already the minimum of control-plane and PostgreSQL.
  Removing the polled half removes a conjunct.
- **Step 1 also concentrates the dependency.** Afterwards, control-plane
  freshness is the *only* input to the revocation decision. That is already
  true in the failing direction - `credential_authentication_allowed` returns
  false before the first successful poll, because `is_fresh` in
  `crates/zeroship-core/src/readiness.rs` is false until `mark_success` runs, so
  a freshly booted gateway rejects authenticated traffic until its first poll
  lands, and the database read cannot mitigate it because the pushed check
  short-circuits first. Step 1 does not create that; it removes the last thing
  that could have masked it.
- **The maximal version lowers availability.** Every cache miss would become an
  auth call, and auth is also the process serving unauthenticated login,
  signup and OAuth traffic. Back-channel logout becomes circular: auth pushes a
  logout token to the gateway, which would then call back into auth to revoke.

## What trades fail-closed for fail-open

**Step 1 does NOT trade fail-closed for fail-open, and a reader will expect the
opposite.** Both mechanisms fail closed, the pushed one gates the polled one,
and the pushed one rejects on staleness. State this explicitly in the change
that lands it, because "you removed a revocation check" reads as a weakening
and it is not one.

**Step 3 is the step that can go fail-open, and it must be designed against
that.** Today `relay_alias_for` in `crates/zeroship-gateway/src/auth_token.rs`
returns `None` on a database failure and the caller projects an empty email; the
comment says "the projection must never leak the real email on a blip". The real
address is live in memory at that point - it is in the token claims and it is
written to the session row. **If the alias arrives in the token response
instead, an error arm that falls back to what it already has leaks the user's
real email address to the app.** The obligation is prose in
`crates/zeroship-gateway/src/identities.rs` today, not a type. Making it a type
- a wrapper that cannot be constructed from the claims-derived address on the
projection path - is part of step 3, not a follow-up.

## Smaller things that get worse or stay bad

- Deleting the polled read removes the one place where a same-node signout is
  strictly more timely than the push. Step 1's local-application design is what
  keeps that property; if the design ships without it, signout timeliness
  regresses to the poll interval.
- Step 4 does not by itself fix the missing idle slide or the missing retention.
  It creates the opportunity to fix them and the obligation not to port them.
- The gateway keeps the pairwise salt after every step here. See section 4 and
  open decision 5.
- A separate stale-prose hazard, noted because anyone reasoning about gateway
  storage will hit it: `GateState`'s doc comment in
  `crates/zeroship-gateway/src/lib.rs` describes a metering flush task POSTing to
  a control usage-ingest endpoint. Neither the symbol nor the endpoint exists;
  the real wiring is the Kafka outbox in
  `crates/zeroship-gateway/src/main.rs`. Do not reason from that comment.

---

# 8. Open decisions for the operator

1. **What is the required cross-node revocation timeliness?** After step 1 the
   bound is the poll interval (`gateway.poll_interval` in
   `crates/zeroship-gateway/src/config.rs`) rather than the cache TTL constant
   in `crates/zeroship-authz/src/wrapper_revocation.rs`. Both are configured
   intervals of the same order and either is defensible. If the requirement is
   tighter than a poll, the answer is a push channel for revocations - not a
   per-request database read, which as section 3 shows adds nothing today.

2. **Should `gateway_sessions` exist at all?** It stores the user's real email
   that no production statement reads, it has no retention sweep, it grows by a
   row per page load, and its idle column is never slid. The alternative is to
   derive the user's app-session list from `app_session_anchors`, which the
   gateway already maintains with a lifecycle. Moving the table (step 4) and
   deleting it are different decisions and only the operator can pick.

3. **Is `app_oauth_clients.sector_identifier` ever set after client creation?**
   UNVERIFIED, and it decides whether a latent hazard is real. Auth's
   `load_client` in `crates/zeroship-auth/src/oidc/authorization_code.rs`
   defaults a null sector to the client id, while the gateway fails closed on a
   null sector. If auth writes a row under the defaulted sector and the column is
   later populated, auth's own `WHERE ... pairwise_sub = EXCLUDED.pairwise_sub`
   guard matches zero rows and the mint fails permanently for that
   `(user, client)` pair. What would settle it: read every writer of that column
   in `crates/zeroship-control`.

4. **Does the anchor encryption key stay gateway-only?** `anchor_enc_key` on
   `GateState` exists so the refresh family never leaves the gateway in
   plaintext. Any design that hands anchor handling to auth either moves the key
   - and auth already holds the OP-side refresh tokens, which makes the
   encryption pointless - or needs two calls. This proposal assumes gateway-only
   and step 5 depends on that assumption.

5. **Can the gateway drop the pairwise salt entirely?** UNVERIFIED. The bearer
   arm already treats the OP's `sub` as pairwise, so an OP-issued subject may be
   in scope at every site where the gateway currently derives one. If it is, the
   salt leaves the internet-facing process and the confidentiality argument in
   section 4 is answered outright. This is a larger security win than any step
   in section 5 and it is not costed here because the enumeration has not been
   done.

6. **Does the anchor reload-storm family kill actually happen?** UNVERIFIED per
   step 5. It changes step 5 from a scope cleanup into a defect fix, and it
   changes the priority of that step relative to the rest. The experiment is
   named in step 5 and it is cheap.

---

# 9. Corrections

Claims made during this investigation that turned out to be wrong, recorded
rather than silently repaired. The first two change the shape of a step.

1. **"Auth's upsert does not fire on the cookie-only mint path."** FALSE, and
   this was the premise the ownership work was dispatched on. It came from the
   gateway's own comment, which says the cookie path keeps its write "because it
   also mints sessions from external providers". Both gateway write paths run a
   `grant_type=authorization_code` post to the OP's token endpoint in the same
   request, immediately before their own write, and that grant calls
   `mint_access_token` unconditionally. Both gateway writes are redundant today,
   and external-provider federation does not escape it because federation sits
   upstream of the OP. Step 2 changed from "give auth the capability" to
   "delete".

2. **"The pushed revocation set is bounded, so the database read sees more
   history."** FALSE. `sweep_expired_families` deletes at the same constant the
   push window uses, and it is wired into auth's cron. The asymmetry the brief
   presented as the reason to keep the read does not exist. This is what turned
   step 1 from a trade-off into a deletion.

3. **"`gateway_sessions` has no production reader."** Conflates two claims. The
   *function* `sessions::validate` has no production caller; the *table* has a
   production reader in another crate, `list_by_user` in
   `crates/zeroship-auth/src/store/sessions.rs`, plus three production delete
   sites in auth. The table is not write-only, it is written by one service and
   read by another - which is what makes step 4 a move rather than a deletion.

4. **"The polled path is uniformly cache-backed."** Wrong for one of the three
   sites. `session_cookie_family_revoked` in
   `crates/zeroship-gateway/src/auth_token.rs`, on the `/session` fast path,
   consults no cache and performs a checkout and a select on every call.

5. **"`credential_authentication_allowed` lives in a router submodule."** It is
   in `crates/zeroship-gateway/src/sync.rs`, a top-level module. There is no
   sync module under the router directory; the line numbers the brief gave were
   right for the file it named wrongly.

6. **"The gateway is the only writer of `app_session_anchors`."** Not stated in
   the brief but assumed by its framing. Auth updates `revoked_at` on that table
   in two production statements, in
   `crates/zeroship-auth/src/identity/password_reset.rs` and
   `crates/zeroship-auth/src/store/users.rs`, each atomic with the credential
   change it accompanies, under a grant it already holds. The ownership question
   for anchors is not gateway-versus-auth; both already write it, and auth's half
   is the half that must stay atomic with a credential change.

7. **"The gateway holds one pool per host."** It holds one per worker thread,
   and the gateway sets no worker count, so the count is the host's logical CPU
   count. The connection floor is therefore larger and less controllable than
   the brief's framing implied - which strengthens the case for decoupling and
   was recorded because it cuts in the proposal's favour.

8. **"The grants file shows auth cannot insert into `app_user_identities`."**
   Reading only `db/migrations-ts/20260702000900_grants.ts` gives that
   impression. `db/migrations-ts/20260818000300_credential_lifecycle.ts` grants
   the insert separately, with a comment saying it exists so auth can create the
   identity row. Step 2 needs no new grant.

9. **"`identities::lookup_relay_email` has one production caller."** That is the
   call inside `relay_alias_for`. `relay_alias_for` itself has three production
   callers, one of which rides a refresh-token grant rather than a code grant -
   which is why step 3 must also touch
   `crates/zeroship-auth/src/oidc/refresh.rs`.

10. **The cross-continent latency multiplier.** It came from a table in
    `docs/architecture/data-system.md` about creator-database co-location, not
    from any measurement of the gateway login path, and no topology under
    `deploy/` configures regions. Any magnitude for this path is unmeasured.
    Section 1 states the shape and the threshold instead.

11. **My own, while checking claim 2's neighbour.** I first checked for
    `signing_keys` across the whole `crates/zeroship-gateway/` tree and found
    matches, which reads as a refutation of "the gateway never touches it". The
    matches are doc comments in
    `crates/zeroship-gateway/tests/oidc_rp_e2e.rs`. "Zero references in
    `crates/zeroship-gateway/src`" is right; "zero references in the crate"
    would have been wrong. A grep that does not distinguish production from test
    answers a different question than the one asked.

12. **Two stale doc comments found in passing, recorded so nobody reasons from
    them.** `RotationSingleFlight` in `crates/zeroship-gateway/src/anchors.rs`
    justifies its per-thread scope by citing "the short cached wrapper", while
    the module header of the same file records that the cached-wrapper columns
    were deleted. And `GateState` in `crates/zeroship-gateway/src/lib.rs`
    describes a metering flush task and a control usage-ingest endpoint, neither
    of which exists. In both cases believe the code.
