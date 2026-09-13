# Gateway central-database decoupling

**Status. PROPOSED. NOTHING IN THIS DOCUMENT IS IMPLEMENTED.** No deletion has
happened, no table has moved, no grant has been revoked. What exists is the
mechanism this document proposes to shrink, plus one piece of machinery that
makes most of the shrinking a deletion rather than a construction: the
control-plane push feed already carries the only fact the gateway reads from
the central database on the authenticated dispatch path.

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
per worker thread and its floor is held whether or not the thread serves
traffic. The **latency** term scales with the number of separate transactions
on the login path and not with the number of statements, because every helper
opens its own transaction bracket. Both terms are invisible in the only
topology this repository ships, where the gateway and the database sit on one
network, and both become the dominant cost the moment the gateway is placed for
proximity to users rather than proximity to the database. That is the threshold
this proposal is written against. **The sequence in section 5 addresses the
latency term and the per-request dependency; it does not move the connection
floor** - see section 6 and open decision 7.

---

## How to read this document

**Provenance.** Every claim tagged MEASURED was re-derived by opening the file
on 2026-09-05, on `main`. An investigation survey produced the underlying
findings; where they disagreed with the brief that dispatched them, the
correction is recorded in the corrections section rather than silently applied.
Some of those corrections invert the shape of a step, so read section 9 before
relying on section 5.

**Tags.**

- **MEASURED** - I opened the code and the claim came out of a file.
- **DESIGNED** - a shape this proposal argues for. It does not exist.
- **UNVERIFIED** - stated because it matters, not established. Each one names
  what would settle it.

**On numbers.** This document carries no magnitudes. Where a quantity is
load-bearing, the constant is named with its symbol and its file so a reader
can read the current value; where a set is load-bearing, the **instrument** is
named - a path plus a symbol, or a command - so a reader re-derives the set
rather than trusting a tally here; and where a quantity must hold over time the
proposal asks for a gate arm rather than asserting a value. The reasoning is
local and measured: the sibling proposal landed full of figures, the commits
after it exist only to repair those figures, and the last of them put fresh
figures in its own subject line. A number in a durable artifact is a
maintenance obligation that nothing enforces.

That policy was applied to digits first and to spelled-out tallies second. An
earlier revision of this document said it carried no magnitudes while carrying
code-derived tallies spelled out in words, which a digit grep cannot see. **Do
not audit this file with a digit grep.** The words are the exposure.

**On line numbers.** `tests/doc_citation_gate.sh` checks that a cited path
exists. By a decision recorded in its own header it does not check that a cited
line is the right line, and its header records occasions on which line citations
in this tree drifted wholesale while staying green. This document therefore
cites **paths and symbol names only**. Where a line matters to an argument, the
code is quoted, ASCII-normalised: the tree's comments contain em-dashes that
this file cannot reproduce, so search a distinctive word rather than a whole
quoted line.

**This document is NOT covered by that gate.** Its arms name specific proposal
globs and none of them matches a `2026-09-05-*` file, so every path cited below
is checked by nothing today. The gate's own header states the enrolment policy:
clean a proposal, then add it by name on the same commit. **Adding this file to
a `check_citations` arm, with that arm's floor raised, is part of the change
that lands the first step** - not a follow-up.

---

# 1. The problem

## What scales with what

MEASURED. **Connections scale with the fleet, not with load.** The gateway's
pool is an `Rc<Pool>` in a `thread_local!` keyed by DSN, built lazily on each
worker thread's first database touch (`crates/zeroship-gateway/src/db.rs`,
`checkout`). The per-pool ceiling is `gateway.db_pool_size` in
`crates/zeroship-gateway/src/config.rs`, passed to `Pool::connect` in
`libs/compio-postgres/src/pool.rs`, which sets `min_idle: defaults.min_idle.min(max_size)`
- so a floor is established at construction and the housekeeper's documented
duties keep it there ("Evict idle connections past idle_timeout (keep
min_idle)" and "Refill to min_idle"). The pool count per host is the ntex worker
count, and the gateway sets none: `grep -rn '\.workers(' crates/zeroship-gateway/src`
returns nothing, so the framework default governs. That default is
`available_parallelism().map_or(2, NonZeroUsize::get)` in `ntex-server`'s
`pool.rs`, which on Linux respects the container's cgroup CPU quota - so the
pool count tracks the gateway container's CPU allotment, not the host's core
count. Multiply by replicas.

The direction that matters: **adding gateway capacity to serve more users
consumes central-database connections even when the added capacity is idle**,
because the floor is per pool and the pool count is per allotted core. Nothing
in `deploy/` sets `max_connections`, so the server default governs. This term is
a function of how the fleet is shaped, not of how much traffic it serves, and it
is the term that fails first at scale. **No step below moves it** - see open
decision 7 for the knobs that would.

MEASURED. **Latency on the login path is dominated by the transaction count,
not the statement count.** Each gateway store helper opens its own transaction,
binds an RLS GUC through `crates/zeroship-gateway/src/rls.rs`, issues one
statement, and commits. `sessions::create`, `anchors::create`,
`identities::upsert` and `relay_alias_for` each pay that full bracket
separately, and they are not batched because the RLS GUCs differ: sessions and
anchors bind the tenant-app setting, identities binds the tenant-client setting.
Some of them do not even share a pool checkout.

The direction that matters: **each additional separate transaction costs a full
round trip per statement in its bracket, so the cost is set by how many
transactions the path opens, and folding statements into fewer transactions
buys more than removing statements does.** That is the term section 5 moves.

## Where the sign flips

MEASURED. The compose topology places the gateway, auth and PostgreSQL on one
Docker network. The evidence is an absence: `deploy/compose/docker-compose.yml`
declares no top-level `networks:` key at all (its top-level keys are `services:`
and `volumes:`), so Compose's implicit `default` network applies and every
service joins it. The one `networks:` key in the file is a service-level alias
block on `caddy`, which declares hostname aliases and no topology. And no file
under `deploy/` configures a region or a zone. In that topology both terms above
are small enough to be invisible, and no measurement in this tree says otherwise
for this path.

**The flip happens when the gateway is placed apart from the database, and it
is a placement decision, not a load one.** The moment a gateway serves users
from a location that is not the database's location, the login path pays its
transaction count multiplied by the inter-zone round trip, and the connection
floor is paid over a link that is expensive to hold open. Nothing in the
current deployment forces that choice; the sibling proposal's multi-zone
section is where it gets forced.

Any magnitude for this path is currently unmeasured. The multiplier that
circulated during the investigation is not one; see correction 10.

---

# 2. What the gateway actually uses the database for

MEASURED, and the census matters more than the count. **The gateway role's
table grants are spread across several migrations, not one.** Enumerate them
with `grep -rn zeroship_gateway db/migrations-ts/`; the files that carry them
today are `db/migrations-ts/20260702000900_grants.ts`,
`db/migrations-ts/20260812000000_gateway_token_revocations_update.ts` (which
widens `token_revocations` with `update`) and
`db/migrations-ts/20260816000100_service_assertion_replay.ts` (which grants
select/insert/update/delete on `service_assertion_replay` plus `usage` on its
schema). A census taken from the first file alone undercounts the role and
misses one of its two dead grants.

**Two granted tables are dead.** `signing_keys` has **no reference in
`crates/zeroship-gateway/src`** - the gateway obtains OP keys over HTTP through
the JWKS client on `GateState` instead. `service_assertion_replay` is
unreachable from the gateway too: `crates/zeroship-gateway/Cargo.toml` declares
no `zeroship-authn` dependency and
`grep -rn 'service_assertion_replay\|service_authn' crates/zeroship-gateway/src`
returns nothing. Both grants should be revoked whatever else happens, and step 1
owns the migration that does it.

**One granted privilege is dead in a worse way.** The gateway holds
`["select", "insert", "delete"]` on `zeroship.token_revocations`. Production uses
the select (`revoked_after_for`, `is_family_revoked_since`) and the insert
(`revoke_family`, from `crates/zeroship-gateway/src/browser_auth.rs` and
`crates/zeroship-gateway/src/backchannel_logout.rs`). Every
`DELETE FROM zeroship.token_revocations` in the gateway crate sits inside
`#[cfg(test)] mod tests` in `crates/zeroship-gateway/src/router/auth.rs` -
verify with that grep against the module's `#[cfg(test)]` boundary. So the
internet-facing process can erase revocation markers, un-revoking every
signed-out family, to serve tests. That is a strictly worse over-grant than the
dead `signing_keys` select, and step 1 revokes it and re-points the test deletes
at a fixture connection with a different role.

MEASURED, and it is worth stating because a reader may find the opposite in the
deployment: the comment above the gateway's DSN in
`deploy/compose/docker-compose.yml` describes the role as one that "only reads
sessions". The gateway writes several of these tables. Believe the code.

## What the gateway touches, and what the data IS

| Table | Rate | What the data IS | Who else touches it |
| --- | --- | --- | --- |
| `token_revocations` | per request, cached on the dispatch arms and uncached on the `/session` fast path | a per-`(client_id, sub)` cutoff instant; a globally replicable fact | auth writes and sweeps it; control reads it into the push feed |
| `gateway_sessions` | per login, and per reload on the mint path | an inventory record whose only reader is elsewhere | auth's `list_by_user` reads it, `revoke_one_for_user` deletes from it |
| `app_session_anchors` | per login, plus a read-modify-write per reload | the encrypted server-held OP refresh family - genuinely gateway-owned mutable secret state | auth stamps `revoked_at` atomically with a credential change |
| `app_user_identities` | per login | a pairwise-and-relay mapping another service owns | auth holds every lifecycle verb; control revokes on grant deletion |
| `audit_events` | per back-channel logout | a security event record in another service's table | auth owns, writes and reaps it |

The classification in the third column is the one that decides the work.
`token_revocations` does not need to be *read* from a database; it needs to be
*known*, and section 3 shows it already is. `gateway_sessions` and
`audit_events` are the gateway reaching into another service's storage to hand
it a row. `app_user_identities` is a mapping auth owns, and section 4 shows the
gateway's write is already redundant. `app_session_anchors` is the one table
where the gateway is the right owner - the refresh family is encrypted under
`anchor_enc_key` on `GateState` (`crates/zeroship-gateway/src/lib.rs`), whose
stated invariant is that the family never leaves the gateway in plaintext - and
section 5 argues it stays.

---

# 3. The finding that changes the shape of the work

**The pushed snapshot already carries the per-request fact. The per-request
half of this proposal is a DELETION, not a construction.** That is the headline
and it should be read before the sequence.

MEASURED. Two mechanisms decide the same thing on the same path, and one
subsumes the other.

- **Pushed.** `RouteCache::credential_authentication_allowed` in
  `crates/zeroship-gateway/src/sync.rs` rejects on a stale snapshot, then
  rejects if `denied_principals` contains the subject, then consults a
  `family_revocations` map populated by `update_snapshot` from the
  control-plane pull and rejects with
  `.is_none_or(|revoked_after| *revoked_after <= issued_at)`.
- **Polled.** `family_revocation_decision` in
  `crates/zeroship-gateway/src/router/auth.rs` consults the short-TTL
  `RevocationCache` in `crates/zeroship-authz/src/wrapper_revocation.rs` and on
  a miss issues a `MAX(revoked_after)` select against
  `zeroship.token_revocations`, rejecting with `family_revoked_at`, which is
  `revoked_after.is_some_and(|ra| ra > iat)`.

The pushed check computes the polled check's predicate **and rejects on more
besides** - on a stale snapshot, and on the denied-principal set. It is not the
same rule; it strictly dominates it. That direction is the safe one for a
deletion, and it is worth saying explicitly so a reviewer does not go looking
for a case where the pushed check is weaker. There is none.

The legs below make the polled read removable.

**Order.** MEASURED. On every arm the pushed check runs first and returns early.
In `resolve_bearer_user_header` it returns `BearerOutcome::Invalid` before the
database read; in `resolve_app_session_user_header_inner` it returns
`CookieOutcome::None` before the database read. Both are in
`crates/zeroship-gateway/src/router/auth.rs`, and both reach the snapshot
through the free function `credential_authentication_allows` in that same file,
which forwards to `RouteCache::credential_authentication_allowed` in
`crates/zeroship-gateway/src/sync.rs`. Those two symbols differ by one letter
and only the second is a method; do not conflate them. The pushed check also
runs a second time after the read on both arms.

The `/session` fast path in `crates/zeroship-gateway/src/auth_token.rs` has the
same order: its own `credential_authentication_allows` gates entry to the block
that performs the read, and a second call re-checks after it. A reviewer read
only the inner lines and reported the order inverted there; it is not. See
correction 13.

**The database read is not a staleness fallback.** MEASURED, and this is the
leg that most looks like it should go the other way.
`credential_authentication_allowed` opens with

```rust
if !self.sync_freshness.is_fresh(freshness_budget) {
    return false;
}
```

so a stale snapshot rejects, and rejection short-circuits before the read on
every arm. **The database read is unreachable in exactly the case where the
pushed data is untrustworthy.** It covers no fail-open.

**Retention is symmetric today, and the brief said otherwise - but nothing
enforces the symmetry.** MEASURED, and this correction is the one that turned
step 1 from a trade-off into a deletion, so state it precisely. The pushed set
is bounded by an interval **inlined as a SQL literal** in the family-revocation
query in `crates/zeroship-control/src/registry.rs`. The table is swept at the
constant `WRAPPER_REVOCATION_RETENTION_HOURS`, bound as a query parameter by
`sweep_expired_families` in `crates/zeroship-authz/src/wrapper_revocation.rs`
and wired into auth's cron in `crates/zeroship-auth/src/cron/token_sweep.rs`.
**These are two independent literals that agree numerically today.** Nothing
imports one from the other, nothing compares them, and no test goes red if one
moves. There is no symbol on the control side precisely because the value is
inlined - which is why the earlier phrasing, "the same constant expressed
twice", was wrong rather than loose. Rounding does match: the control-plane
query ceilings to whole seconds with `CEIL(EXTRACT(EPOCH ...))` and
`revoked_after_for` ceilings the same way, so the polled path opens no rounding
gap the pushed one does not.

After step 1 that agreement becomes safety-critical, because the control-side
literal is then the only bound on the window in which a pushed revocation still
applies. Step 1 must therefore land with the boundary made mechanical: bind
`WRAPPER_REVOCATION_RETENTION_HOURS` as the control query's interval parameter,
or, if a parameterised interval is unacceptable there, add a gate arm that reads
both spellings and rules that they agree, declaring the number of boundary pairs
it ruled on and a floor per `tests/lib/gate_arms.sh`. **Step 1 must not land on
a symmetry that nothing enforces.**

**Window completeness.** MEASURED for the constants, UNVERIFIED for the
enumeration. The pushed window only drops a revocation that matters if a live
credential's `iat` is older than the window, and every credential ceiling in the
tree is inside it: `SESSION_TOKEN_TTL_SECS` in
`crates/zeroship-gateway/src/session_token.rs`, `ACCESS_TOKEN_TTL_SECS` in
`crates/zeroship-auth/src/oidc/issuer.rs`, and the issuer's own ceiling
`PLATFORM_TOKEN_MAX_TTL_SECS` in `crates/zeroship-core/src/device_grant.rs`.
No writer back-dates: every production writer stamps the PostgreSQL clock and
upserts through `GREATEST`, so a marker only moves forward.

**One caveat, stated rather than buried.** The session cookie's `iat` is not its
mint time. `crates/zeroship-gateway/src/session_token.rs` carries

```rust
iat: m.credential_iat.min(now),
```

which inherits the originating credential's issuance instant across re-signs.
The clamp is upper-only; nothing enforces a floor. The completeness argument
therefore rests on every production feeder of `credential_iat` being a token
minted in the same request. **That enumeration is UNVERIFIED here.** What would
settle it: enumerate every production constructor of the `credential_iat` field
that reaches `session_token`'s minter, with
`grep -rn 'credential_iat' crates/zeroship-gateway/src` minus the `#[cfg(test)]`
blocks, and establish for each that its value came from a token redeemed in that
request. Even if it were breached, the database read would not be a backstop -
the same sweeper deletes the marker at the same boundary, so both mechanisms
would accept the stale-`iat` cookie identically.

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
separate cache. That is the design in step 1 - and it is not a one-line
insertion; see the overlay requirement there, which is the single most
important implementation constraint in this document.

## Which sites are in scope

MEASURED, and this enumeration changed after review. Enumerate the gateway's
reads of `zeroship.token_revocations` with

```
grep -rn 'revoked_after_for\|is_family_revoked_since\|family_revocation_decision\|session_cookie_family_revoked' crates/zeroship-gateway/src
```

minus the `#[cfg(test)]` blocks. The production set splits in two.

- **The polled reads on the authenticated dispatch and `/session` fast paths.**
  `family_revocation_decision` on both dispatch arms in
  `crates/zeroship-gateway/src/router/auth.rs`, and
  `session_cookie_family_revoked` in `crates/zeroship-gateway/src/auth_token.rs`
  on the `GET /__zeroship/auth/session` fast path. These are the ones step 1
  deletes. They are all gated by the pushed check, per the order leg above.
- **The rotation F4 post-refresh re-check**, also in
  `crates/zeroship-gateway/src/auth_token.rs`, which calls
  `zeroship_authz::wrapper_revocation::revoked_after_for` directly, uncached, on
  its own pool checkout. **This one is NOT in scope and must not be deleted.**
  It is not a revocation-freshness read; it is a TOCTOU fence against a teardown
  landing *during* a rotation, and step 5 argues it stays. Section 6 lists it
  among the things this proposal does not do.

An earlier revision of this section counted the first group as if it were the
whole set, which is exactly the shape that invites an implementer to delete the
fence. The instrument above returns both groups; the split is the claim.

MEASURED, and it changes which site to delete first: `session_cookie_family_revoked`
is not cache-backed at all. It performs a pool checkout and a select on **every**
call with no cache consultation anywhere. It is the most expensive site to keep
and the cheapest to remove.

---

# 4. Ownership

## `app_user_identities` belongs to auth, and the gateway's write is already redundant

MEASURED. Auth holds every lifecycle verb on this table: the mint inside
`mint_access_token`, the relay-alias mint and revoke in
`crates/zeroship-auth/src/store/relay.rs`, the password-reset teardown in
`crates/zeroship-auth/src/identity/password_reset.rs`, account deletion in
`crates/zeroship-auth/src/store/users.rs`, and the userinfo and introspect
reads. Auth's own comments say it owns the table.

Auth's `access_identity_upsert_sql` in
`crates/zeroship-auth/src/oidc/authorization_code.rs` and the statement inside
`identities::upsert` in `crates/zeroship-gateway/src/identities.rs` separately
implement the mapping write. Each preserves the subject and alias while clearing
revocation on re-grant. The gateway's SQL-text assertion has been replaced with
database behavior tests; the statements remain independently maintained.

MEASURED, and **this refutes the premise the investigation started from.** The
gateway's own comment justifying its write says the cookie path "retains its own
write because it also mints sessions from external providers", and cites the H1
password-reset teardown (security finding F1). The code contradicts it:

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

**Both gateway writes are redundant today.** But redundancy is not the whole
picture, and the earlier draft of this section read the remaining mechanism
backwards.

MEASURED. **The gateway's upsert is also the only live cross-service comparison
of the two pairwise derivations, and it fails closed.** Its SQL ends
`WHERE zeroship.app_user_identities.pairwise_sub = EXCLUDED.pairwise_sub`, which
makes the rows-affected zero on a mismatch, and the Rust turns that into an
error:

```rust
if mapped != 1 {
    return Err(GatewayError::Db(
        "app_user_identities pairwise binding changed".to_string(),
    ));
}
```

The module's own doc states the intent: "Configuration drift fails closed
instead of replacing the subject used for recall." Auth's arm is the same shape
and is terminal in the same way (`"pairwise identity binding changed"` in
`crates/zeroship-auth/src/oidc/authorization_code.rs`). So deleting the
gateway's write deletes a detector, and **the evidence goes with the deletion**:
after step 2, a divergence between the gateway's derivation and auth's produces
a signed cookie whose `sub` does not match the stored identity row, silently,
instead of a loud server error. That is why step 2's premise test is not "the
row exists" but "the stored `pairwise_sub` is byte-equal to the `sub` the
gateway put in the cookie", and why the sector-resolution question in section 8
had to be settled rather than left open.

MEASURED. `identities::lookup_pairwise_sub` in the same module has no
production caller; every caller is a test. It should be deleted with the upsert.

## The `revoked_at` race, which nobody defends

MEASURED. Auth un-revokes on re-consent, control revokes on grant deletion
(`crates/zeroship-control/src/oauth_grants_handlers.rs`), and the gateway
un-revokes as a side effect of every upsert. Control's own comment says the
write "does NOT serialize against auth's re-consent un-revoke (no shared
advisory lock)" and that the race is "closed STRUCTURALLY on the read side"
because `resolve_active_alias` forwards only when a live `oauth_grants` row
still exists.

Read that carefully: **the column has several writers and is deliberately not
the authority.** Deleting the gateway's writer removes one arm of a race that
its own participants already route around. That is a strictly good outcome and
it costs nothing.

## The pairwise salt in more processes than need it: a confidentiality argument, not a tidiness one

MEASURED. The same salt is loaded by auth (`auth.pairwise_salt_file` in
`crates/zeroship-auth/src/config.rs`), by control (`pairwise_salt` in
`crates/zeroship-control/src/config.rs`) and by the gateway (`pairwise_salt` on
`GateState` in `crates/zeroship-gateway/src/lib.rs`). `derive_pairwise` in
`crates/zeroship-core/src/auth/mod.rs` is a pure HMAC over the canonicalized
global user id and the sector identifier. The salt is the only secret in it.

**Be precise about what an attacker gains from the salt, because overstating it
would justify the wrong work.** With the salt, and a candidate global user id, an
attacker can compute any user's pseudonym for any sector. That yields
**linkability** - the pseudonyms a user carries across every app become
computable from one another, which defeats the exact property the pairwise
scheme exists to provide - and **membership confirmation** - for a named user
and a named app, testing presence by deriving the value and matching it against
subjects visible in rows, headers or logs.

**It does not yield authentication.** The pairwise subject is an identifier, not
a bearer credential; deriving one does not mint a session. The loss is
confidentiality of the identity graph, not a bypass, which is what makes this a
consolidation argument rather than an emergency - and every additional process
holding the salt is another process whose compromise deanonymizes the platform's
users, with the internet-facing one carrying the widest blast radius.

MEASURED, and it bounds what step 2 buys: **removing the gateway's identity
write does not remove the gateway's need for the salt.** Enumerate the gateway's
production derivation sites with `grep -rn 'derive_pairwise' crates/zeroship-gateway/src`,
minus the `#[cfg(test)]` blocks - note that in
`crates/zeroship-gateway/src/router/auth.rs`,
`crates/zeroship-gateway/src/sync.rs` and
`crates/zeroship-gateway/src/router/dispatch.rs` every occurrence is
test-gated. **Searching for the
`pairwise_sub` helper alone undercounts**, because `issue_interactive_session_cookie`
in `crates/zeroship-gateway/src/auth_token.rs` calls
`zeroship_core::auth::derive_pairwise` directly and never goes through that
helper. The other production sites are the helper itself, `teardown_per_app_user`
in `crates/zeroship-gateway/src/backchannel_logout.rs`, and `signout` in
`crates/zeroship-gateway/src/browser_auth.rs`.

Whether the gateway could take the pairwise subject from the OP token response
instead of deriving it - the bearer arm already treats the OP's `sub` as pairwise
and checks it with `is_pairwise_subject` - is **UNVERIFIED**. What would settle
it: run the instrument above and, for each production site it returns, establish
whether an OP-issued subject for the same `(user, sector)` is already in scope at
that point. If every one is, the gateway can drop the salt entirely, and that is
a larger security win than anything else in this document.

---

# 5. The sequence

The steps below are independently landable and ordered by value, with one
caveat: step 5's position is conditional on an experiment named in open
decision 6, and that experiment should be run **before** the order is finalised,
not after. Each step names the red test that fails before it and passes after;
where a step's test is a *premise* test that must be green before the change
rather than after, that is said explicitly, because the two are not
interchangeable.

## Step 1. Delete the polled family-revocation reads; apply gateway-side revocations locally

DESIGNED. Remove `family_revocation_decision` from both dispatch arms in
`crates/zeroship-gateway/src/router/auth.rs` and
`session_cookie_family_revoked` from the `/session` fast path in
`crates/zeroship-gateway/src/auth_token.rs`. **Leave the rotation F4
post-refresh re-check in that same file untouched** - see section 3's scope
split and step 5. In the same change, have the gateway's own revocation writers
- the signout path in `crates/zeroship-gateway/src/browser_auth.rs` and the
back-channel logout path in `crates/zeroship-gateway/src/backchannel_logout.rs`
- apply the revocation into `RouteCache`'s `family_revocations` map instead of
invalidating a separate cache.

**The local write must survive the next poll, and this is not automatic.**
MEASURED: `update_snapshot` in `crates/zeroship-gateway/src/sync.rs` REPLACES
the whole authentication snapshot:

```rust
*authentication = AuthenticationSnapshot {
    denied_principals: denied,
    denied_subjects_by_user: denied_by_user,
    family_revocations,
};
```

A locally applied revocation inserted into that map is therefore destroyed by
the next successful poll whose control-side select ran before the gateway's
write committed - and the signed-out user is authenticated again on that node
until a later poll carries the marker. Today that window is covered, because
`RevocationCache::invalidate` forces a miss and the database read sees the
committed row. **A naive step 1 would trade a fail-closed property for a
fail-open one.**

The precedent for the fix is in the same function: `denied_subjects_by_user` is
already carried across a snapshot swap, via `prior_by_user`. The design is to
give `family_revocations` the same shape - merge the incoming snapshot into the
prior map keyed per `(client_id, subject)`, taking the later `revoked_after`,
rather than replacing it - bounded by the same retention boundary as the sweeper
so the locally-written half cannot grow without limit. Equivalently, keep a
separate locally-written overlay consulted alongside the pushed map. Either
shape is acceptable; **shipping neither is not.**

Also in this step, and in the same forward migration under `db/migrations-ts/`:
revoke `delete` on `zeroship.token_revocations` from `zeroship_gateway`, revoke
`select` on it once the reads above are gone, leaving `insert` only; revoke
`select` on `zeroship.signing_keys`; and revoke the `service_assertion_replay`
grants. Re-point the test deletes in `crates/zeroship-gateway/src/router/auth.rs`
at a fixture connection under a role that still holds the privilege.

Delete `RevocationCache` from `crates/zeroship-authz/src/wrapper_revocation.rs`
with `REVOCATION_CACHE_TTL_SECS` and `REVOCATION_CACHE_MAX_ENTRIES`. **The
gateway is its only consumer** - every reference outside its defining file is
under `crates/zeroship-gateway/`, verifiable with
`grep -rn 'RevocationCache' crates/ | grep -v wrapper_revocation.rs`. Keep
`sweep_expired_families`, `revoke_family`, `revoked_after_for` and
`family_revoked_at`: auth's cron, introspect and userinfo still use them, and
the F4 re-check still calls `revoked_after_for`.

**Buys.** The last per-request central-database dependency on the authenticated
dispatch arms, and the uncached checkout on the `/session` fast path. After this
step **the authenticated dispatch arms and the `/session` fast path need
PostgreSQL for nothing.** The anchor mint and rotation path still does, by
design; see step 5 and section 6.

**Buys, second and unclaimed until review.** Today every polled revocation site
is wrapped in `if let Some(db_cfg) = state.db.as_ref()`, and the cookie arm's
comment says the gate is skipped in smoke mode. A gateway running without a
database therefore performs **no family-revocation check at all** on either
dispatch arm - a mechanism present in the source and unreachable in that
configuration. After this step the only check is
`credential_authentication_allowed`, which is not conditioned on `state.db`, so
smoke mode gains a check it silently lacks. That is a security improvement, not
a side effect.

**Costs.** Cross-node revocation timeliness becomes bounded by the control-plane
poll rather than by the cache TTL. Those two are **not the same kind of knob**:
`poll_interval` in `crates/zeroship-gateway/src/config.rs` is operator-settable
(a shared key, env twin `ZEROSHIP_POLL_INTERVAL`; there is no `gateway.`-prefixed
spelling and an operator who sets one is setting nothing), while
`REVOCATION_CACHE_TTL_SECS` in `crates/zeroship-authz/src/wrapper_revocation.rs`
is a compile-time constant with no config surface - `RevocationCache::new`
hard-wires it and only `with_ttl_and_capacity` overrides it in-process. Read both
values; do not take a comparison from this document. Which bound is acceptable is
open decision 1.

**RED TESTS**, all failing today.

1. With the revocation cache TTL forced to zero via the existing test helper
   `set_revocation_cache_ttl` in `crates/zeroship-gateway/src/router/auth.rs`
   (already used by `revocation_cache_honors_revocation_after_ttl_expiry`), and
   the configured DSN pointed at an unreachable address, an authenticated cookie
   dispatch and a `GET /__zeroship/auth/session` must both succeed. Today the
   miss returns `RevocationDecision::Unavailable` and the arm rejects, and
   `session_cookie_family_revoked` fails closed on checkout failure. This needs
   no new instrument and no wall-clock span: there is no exposed pool-checkout
   counter to assert on, only `metrics.connections_created` in
   `libs/compio-postgres/src/pool.rs`, which a warm checkout does not move.
2. **The overlay test.** Apply a local family revocation, then deliver a control
   snapshot through `update_snapshot` that does NOT contain that marker, and
   assert the next request is still rejected. This is the test that binds the
   design above. A "revocation honoured with the database unavailable" test does
   NOT bind it: with PostgreSQL down no poll succeeds, `update_snapshot` never
   runs, and the naive design passes.
3. A no-database gateway rejects a revoked family. Red today, per the smoke-mode
   gap above.

**One gate arm, and it must rule on values rather than spellings.** A shell arm
that reads `WRAPPER_REVOCATION_RETENTION_HOURS`, the interval literal in
`crates/zeroship-control/src/registry.rs`, and the credential ceilings
(`SESSION_TOKEN_TTL_SECS`, `ACCESS_TOKEN_TTL_SECS`, `PLATFORM_TOKEN_MAX_TTL_SECS`),
and rules that the control window is at least the sweeper retention and that
both exceed the widest ceiling. It declares the number of boundary relations it
ruled on and a floor, per `tests/lib/gate_arms.sh`. A second shell arm may rule
on the negative privilege claim by grepping `db/migrations-ts/` for the revoked
grants, where a grep really is the whole truth.

**Do NOT ask for an arm that counts `crate::db::checkout` call sites reachable
from the dispatch entry points.** Every gate in this tree is a shell script, and
a shell script cannot compute reachability through async Rust; what it would
actually compute is a grep over a hand-listed file set, which stays green when a
checkout appears in a helper the list does not name. It would declare a count and
clear a floor over a set that is not the set the claim is about. Red test 1 binds
the property behaviourally and no spelling can fake it.

## Step 2. Delete the gateway's `app_user_identities` writes and revoke its grant

DESIGNED. Delete both `identities::upsert` call sites in
`crates/zeroship-gateway/src/auth_token.rs`, delete `identities::upsert` and
`identities::lookup_pairwise_sub`, and revoke `insert` and `update` on
`zeroship.app_user_identities` from `zeroship_gateway` in a new forward
migration under `db/migrations-ts/`.

**Buys.** Removes the byte-identical SQL fork, removes a full transaction from
the login path, removes one arm of the `revoked_at` race, and narrows the edge
process's write surface on a table carrying the identity graph.

**Costs, stated plainly.** It deletes a live fail-closed detector of divergence
between the gateway's pairwise derivation and auth's - see section 4. That cost
is acceptable only because the divergence path is now measured shut (section 8,
former open decision 3): `app_oauth_clients.sector_identifier` is `NOT NULL`,
immutable after insert by trigger, and written by a single control-plane
transaction that inserts the `oauth_clients` row and its `app_oauth_clients`
extension together. If that changes, this cost reopens.

Auth needs no change: its `insert` privilege was granted separately in
`db/migrations-ts/20260818000300_credential_lifecycle.ts` precisely so it could
create the row, and it sets the tenant-client GUC immediately before its upsert.

**PREMISE TEST, which must be green BEFORE the deletion.** Drive a full
code-exchange login against a live auth with the gateway's write path disabled,
and assert that the `app_user_identities` row exists **and that its stored
`pairwise_sub` is byte-equal to the `sub` the gateway puts in the session cookie
for the same `(user, client)`**. Row existence alone is not the property; subject
equality is the property the deleted guard enforced. If that fails, the premise
in section 4 is wrong and this step does not land.
`tests/e2e_dev_vs_deployed_login.sh` is the harness closest to this shape.

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

(ASCII-normalised; the source spells that dash as an em-dash.)

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
`Issuer::pairwise_subject` is pure. The missing input is the sector.

**Two things about that are easy to get wrong and both are load-bearing.**

First, **the mint changes shape.** Because the row does not exist at consent
time, `mint_alias_at_consent` cannot stay an UPDATE onto an existing row; it
becomes an `INSERT ... ON CONFLICT` that CREATES the `app_user_identities` row
with the derived `pairwise_sub`. Consent therefore becomes the first writer of
that row for a `(client, user)` pair. That is compatible with auth's existing
insert grant from `db/migrations-ts/20260818000300_credential_lifecycle.ts` and
needs no new privilege - but it is a new ownership fact, and it reinforces step 2
rather than conflicting with it.

Second, **do not add a fresh sector read.** Widening `load_native_oauth_client`
with its own sector select would make consent a third independent derivation
site alongside `mint_access_token` and the gateway, differing from the mint's
resolution by whatever defaulting rule it happens to spell - and both existing
writers carry the immutable-binding guard whose failure arm is terminal, so a
mismatch would break the token mint permanently for that pair. `load_client` in
`crates/zeroship-auth/src/oidc/authorization_code.rs` already resolves the
sector with the defaulting rule the mint uses. **Have the consent handler call
that resolution**, so there is one derivation rather than two that must be kept
equal. Step 2 deletes a fork for being a fork; step 3 must not create one.

The step is therefore: mint the alias at consent, deriving the subject from
`load_client`'s sector resolution, then return the alias in the OP token
response so the gateway reads it from a call it already makes, then delete
`relay_alias_for` and `identities::lookup_relay_email` from the gateway.

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

**STATE THIS FIRST, because section 7's decisive argument applies to this step
and an earlier draft applied it only to the option it had already rejected.**
`gateway_sessions` is `setRls({ enabled: true, forced: true })` with a
`tenant_isolation` policy on `app_id` in
`db/migrations-ts/20260702000800_policies_rls.ts`, and the gateway writes it as
the non-`BYPASSRLS` `zeroship_gateway` role with the GUC bound in-transaction
(`rls::set_tenant_app(&tx, params.app_id)` inside `sessions::create`,
`crates/zeroship-gateway/src/sessions.rs`). Auth is created **with**
`bypassRls`. **Moving this write to auth does not relocate a
PostgreSQL-enforced tenant boundary; it converts it into one a handler in a
BYPASSRLS process promises.** Auth says so about this very table: `list_by_user`
in `crates/zeroship-auth/src/store/sessions.rs` notes that "the role is
`BYPASSRLS`, so the gateway table's per-tenant policy does not apply". The same
shape is already visible in auth's `mint_access_token`, which sets
`zeroship.tenant_client` via `set_config(..., true)` before its upsert while
holding BYPASSRLS, so the GUC reads as a fence and enforces nothing.

That cost is why open decision 2 matters more than this step's mechanics: if
`gateway_sessions` should not exist, no boundary is traded at all. If it should,
the alternative to moving it is keeping the write in the gateway and fixing the
defects below in place. **Do not present this step as "the row lands next to its
only reader" without the boundary cost beside it.**

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

**Buys.** A transaction leaves the login path, the gateway loses write grants,
and the row lands next to its only reader.

**Costs.** The RLS boundary trade above; and a new hop unless the record rides
the token exchange the gateway already performs. Deciding whether the table
should exist at all is open decision 2.

**RED TEST.** List a user's app sessions after a span longer than the idle
window during which the session was continuously used, and assert the session is
still listed and still revocable. It fails today.

## Step 5. Leave `app_session_anchors` where they are, and fix the single-flight scope instead

DESIGNED, and it is deliberately not a move.

MEASURED. The anchor path is a read-modify-write whose zero-rows return **is**
the revocation fence. `update_rotated_family` in
`crates/zeroship-gateway/src/anchors.rs` documents that a zero return means the
anchor was revoked between the caller's `read_live` and the persist, and the
caller consumes it as `LoginRequired`. The paired check - the F4 post-refresh
re-check - reads the family marker **directly from the database**, and it lives
in `crates/zeroship-gateway/src/auth_token.rs`, not in the anchors module a
reader would search first. Its comment
says why. Quoted with two elisions: the first drops the comment's inline
restatement of the cache TTL, which is the constant `REVOCATION_CACHE_TTL_SECS`
in `crates/zeroship-authz/src/wrapper_revocation.rs` and exactly the kind of
transcribed number this document declines to carry; the second drops the clause
"reject before signing", which is substance and is restated here rather than
silently dropped. The text is ASCII-normalised.

```
//       torn down DURING the rotation [...] We read
//       the marker DIRECTLY from the DB (NOT the [...] stale local cache),
//       since this is the authoritative cross-node revocation record.
```

**A pushed snapshot cannot satisfy that read, and an HTTP hop would widen a
TOCTOU window that was closed on purpose. The anchor writes must not be queued,
made asynchronous, or routed through another service, and step 1 must not delete
this read.** This is the step that says do not do the obvious thing.

What should change is the coalescing scope. MEASURED: `RotationSingleFlight` in
`crates/zeroship-gateway/src/anchors.rs` lives in a `thread_local!`, so there is
one map per worker thread per node, and the worker count is whatever ntex's
default resolves to (section 1). Its own doc comment justifies that scope by
claiming cross-thread concurrency "is absorbed by the short cached wrapper +
OP's rotation grace" - while the module header of the same file records that the
cached-wrapper columns were deleted in the BFF redesign. **Believe the code: only
the OP grace remains, and it is single-use.**

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
auth, asserting whether a `kill_family` log line appears. The single-flight tests
in `crates/zeroship-gateway/tests/auth_token_anchors_test.rs` are in-process and
drive `anchors::with_single_flight` directly; none exercises cross-thread
concurrency against a real OP.

**RED TEST, conditional on the above being confirmed.** Concurrent same-anchor
mints across worker threads, asserting no family kill and no `login_required`.

## The durable outbox was considered and does not fit

MEASURED, so that this is not re-proposed. A real outbox exists - a redb
write-ahead log appended before publish, with a drain task and a bounded retry
backlog, in `crates/zeroship-metering/src/outbox.rs` - and the gateway already
runs one via `build_usage_outbox` and `spawn_outbox_task` in
`crates/zeroship-gateway/src/main.rs`. It does not fit these writes for several
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
  section 1.
- **It does not move the connection floor.** No step changes `min_idle`, the
  per-thread pool shape, or the ntex worker count, and step 5 keeps the anchor
  path, so every thread that ever serves a login still constructs a pool and
  still pays the floor. What the sequence reduces is *utilisation* - per-request
  and some per-login checkouts. The only floor effect is statistical, from
  threads that never serve a login never building a pool, and it disappears
  under sustained login traffic. Open decision 7 names the knobs that would
  actually move it.
- **It does not delete the rotation F4 post-refresh re-check.** That read of
  `zeroship.token_revocations` in `crates/zeroship-gateway/src/auth_token.rs` is
  a TOCTOU fence, not a revocation-freshness read, and step 5 keeps it. Step 1
  deletes the polled reads on the authenticated dispatch and `/session` fast
  paths and nothing else.
- **It does not route the RLS-fenced tables through auth.** That is the maximal
  version of this proposal and section 7 argues against it. Step 4 is a narrower
  case of the same trade and states its own cost.
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
`db/migrations-ts/20260702000100_schema_roles_extensions.ts`. The gateway's
`app_session_anchors`, `app_user_identities` and `gateway_sessions` all carry
forced RLS with a `tenant_isolation` policy in
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
appearance of a boundary. It is why step 5 keeps anchors in the gateway, why
step 4 must move the session *record* without moving the anchor read that shares
its RLS binding, and why step 4 states the trade in bold rather than burying it.

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

**Step 1 does not trade fail-closed for fail-open ONCE THE OVERLAY SHIPS, and a
reader will expect the opposite.** Both mechanisms fail closed, the pushed one
gates the polled one, and the pushed one rejects on staleness. State this
explicitly in the change that lands it, because "you removed a revocation check"
reads as a weakening and it is not one. **Without the overlay it IS one**: the
snapshot replacement in `update_snapshot` erases a locally applied revocation on
the next poll, which is a fail-open window that does not exist today. That is
the whole reason the overlay is part of the step rather than a follow-up.

**Step 3 is the step that can go fail-open on the projection, and it must be
designed against that.** Today `relay_alias_for` in
`crates/zeroship-gateway/src/auth_token.rs` returns `None` on a database failure
and the caller projects an empty email; the comment says "the projection must
never leak the real email on a blip". The real address is live in memory at that
point - it is in the token claims and it is written to the session row. **If the
alias arrives in the token response instead, an error arm that falls back to what
it already has leaks the user's real email address to the app.** The obligation
is prose in `crates/zeroship-gateway/src/identities.rs` today, not a type. Making
it a type - a wrapper that cannot be constructed from the claims-derived address
on the projection path - is part of step 3, not a follow-up.

**Step 4 trades a PostgreSQL-enforced tenant boundary for a code-enforced one.**
Stated in step 4 and in the boundary argument above. It is the reason open
decision 2 must be answered before step 4 is scheduled.

## Smaller things that get worse or stay bad

- Deleting the polled read removes the one place where a same-node signout is
  strictly more timely than the push. Step 1's local-application design is what
  keeps that property; if the design ships without it, signout timeliness
  regresses to the poll interval **and, without the overlay, past it**.
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
   bound is the control-plane poll (`poll_interval` in
   `crates/zeroship-gateway/src/config.rs`, a shared key with env twin
   `ZEROSHIP_POLL_INTERVAL`) rather than the cache TTL constant
   `REVOCATION_CACHE_TTL_SECS` in
   `crates/zeroship-authz/src/wrapper_revocation.rs`. The first is
   operator-settable; the second is a compile-time constant with no config
   surface, so "tune it" is not an available answer. Read both values before
   deciding. If the requirement is tighter than a poll, the answer is a push
   channel for revocations - not a per-request database read, which as section 3
   shows adds nothing today.

2. **Should `gateway_sessions` exist at all?** It stores the user's real email
   that no production statement reads, it has no retention sweep, it grows by a
   row per page load, and its idle column is never slid. The alternative is to
   derive the user's app-session list from `app_session_anchors`, which the
   gateway already maintains with a lifecycle. Moving the table (step 4) and
   deleting it are different decisions and only the operator can pick - and
   because step 4 trades an RLS-enforced boundary for a code-enforced one,
   deleting is the option that costs no boundary.

3. **The sector-divergence hazard is MEASURED SHUT, and this was an open
   decision until review.** It asked whether `app_oauth_clients.sector_identifier`
   is ever set after client creation, and proposed settling it by reading every
   writer of the column in `crates/zeroship-control`. That procedure could not
   have answered it. The column is `t.text().notNull()` in
   `db/migrations-ts/20260702000200_control_tables.ts`, and
   `db/migrations-ts/20260702000700_functions_triggers_comments.ts` installs
   `app_oauth_clients_sector_identifier_immutable`, a BEFORE UPDATE OF trigger
   calling `app_oauth_clients_reject_sector_change`, which raises on any change.
   The NULL that auth's `load_client` in
   `crates/zeroship-auth/src/oidc/authorization_code.rs` defaults to the client id
   comes from a LEFT JOIN miss in its own SQL, not from a nullable column. The
   single production writer of `app_oauth_clients` is `upsert_db_rows` in
   `crates/zeroship-control/src/app_oauth_client.rs`, which inserts the
   `oauth_clients` row and the `app_oauth_clients` row in the same transaction.
   The only other production writer of `zeroship.oauth_clients` is
   `reconcile_platform_cli_client` in
   `crates/zeroship-auth/src/oidc/device_token.rs`, a fixed reserved client id
   that never receives an app extension row. **The residual question, if one is
   wanted, is not the one that was asked**: it is whether an `oauth_clients` row
   can exist without its `app_oauth_clients` row for a client that later becomes
   an app client, and it is answered by reading the writers of
   `zeroship.oauth_clients` in **auth**, not the writers of the column in control.

4. **Does the anchor encryption key stay gateway-only?** `anchor_enc_key` on
   `GateState` exists so the refresh family never leaves the gateway in
   plaintext. Any design that hands anchor handling to auth either moves the key
   - and auth already holds the OP-side refresh tokens, which makes the
   encryption pointless - or needs two calls. This proposal assumes gateway-only
   and step 5 depends on that assumption.

5. **Can the gateway drop the pairwise salt entirely?** UNVERIFIED. The bearer
   arm already treats the OP's `sub` as pairwise, so an OP-issued subject may be
   in scope at every site where the gateway currently derives one. Settle it with
   the instrument in section 4 - `grep -rn 'derive_pairwise' crates/zeroship-gateway/src`
   minus the test-gated blocks - remembering that
   `issue_interactive_session_cookie` bypasses the `pairwise_sub` helper, so a
   search for the helper undercounts. If every production site can take an
   OP-issued subject, the salt leaves the internet-facing process and the
   confidentiality argument in section 4 is answered outright. This is a larger
   security win than any step in section 5 and it is not costed here because the
   enumeration has not been done.

6. **Does the anchor reload-storm family kill actually happen?** UNVERIFIED per
   step 5. It changes step 5 from a scope cleanup into a defect fix, and it
   changes the priority of that step relative to the rest - a user-visible logout
   storm under concurrent reloads outranks steps 2 through 4. The experiment is
   named in step 5 and it is cheap. **Run it before finalising the order**, so
   the sequence is not left conditioned on an open decision.

7. **How should the connection floor be addressed, given that no step here
   touches it?** Section 1 names it as the term that fails first at scale and
   section 6 records that the sequence does not move it. The knobs are: setting
   an explicit ntex worker count in the gateway (there is no `.workers(` call
   today), setting `min_idle` to zero for the gateway's pool so a constructed
   pool holds no floor, or giving the gateway a shared pool shape rather than one
   per thread. Each is small and independently landable, and each changes
   warm-up latency in exchange, which is why it is an operator decision rather
   than a step.

---

# 9. Corrections

Claims made during this investigation and in earlier revisions of this document
that turned out to be wrong, recorded rather than silently repaired.
Corrections 1 and 2 change the shape of a step; corrections 13 onward were
found by review of this document itself rather than of the code.

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
   history."** FALSE. `sweep_expired_families` deletes at the same boundary the
   push window uses, and it is wired into auth's cron. The asymmetry the brief
   presented as the reason to keep the read does not exist. This is what turned
   step 1 from a trade-off into a deletion. **But the repair over-claimed in the
   other direction**: an earlier revision called the two boundaries "the same
   constant expressed twice". They are two independent literals - a `const`
   bound as a query parameter in `crates/zeroship-authz/src/wrapper_revocation.rs`,
   and an inlined SQL interval in `crates/zeroship-control/src/registry.rs` -
   that agree numerically and are linked by nothing. Section 3 now says so and
   step 1 owns making the agreement mechanical.

3. **"`gateway_sessions` has no production reader."** Conflates two claims. The
   *function* `sessions::validate` has no production caller; the *table* has a
   production reader in another crate, `list_by_user` in
   `crates/zeroship-auth/src/store/sessions.rs`, plus production delete sites in
   auth. The table is not write-only, it is written by one service and read by
   another - which is what makes step 4 a move rather than a deletion.

4. **"The polled path is uniformly cache-backed."** Wrong for the `/session`
   fast path: `session_cookie_family_revoked` in
   `crates/zeroship-gateway/src/auth_token.rs` consults no cache and performs a
   checkout and a select on every call. **And the enumeration around that
   correction was itself short**: an earlier revision framed the gateway's
   polled reads as the whole production set, omitting the rotation F4
   re-check in the same file, which step 5 insists must stay. Section 3 now
   names the instrument and states the split.

5. **The gateway's grant census was taken from one migration.** An earlier
   revision counted the gateway role's tables in
   `db/migrations-ts/20260702000900_grants.ts` and presented the result as a
   census of the role. The role's grants also live in
   `db/migrations-ts/20260812000000_gateway_token_revocations_update.ts` and
   `db/migrations-ts/20260816000100_service_assertion_replay.ts`, and the second
   of those is a dead grant of exactly the kind the section recommended
   revoking. A per-table privilege split across files is not visible to a
   single-file reading.

6. **"The gateway is the only writer of `app_session_anchors`."** Not stated in
   the brief but assumed by its framing. Auth updates `revoked_at` on that table
   in production statements in
   `crates/zeroship-auth/src/identity/password_reset.rs` and
   `crates/zeroship-auth/src/store/users.rs`, each atomic with the credential
   change it accompanies, under a grant it already holds. The ownership question
   for anchors is not gateway-versus-auth; both already write it, and auth's half
   is the half that must stay atomic with a credential change.

7. **"The gateway holds one pool per host."** It holds one per worker thread,
   and the gateway sets no worker count, so the count comes from ntex's default
   - `available_parallelism().map_or(2, NonZeroUsize::get)` in `ntex-server`'s
   `pool.rs`, which respects the container's cgroup CPU quota. An earlier
   revision asserted "the host's logical CPU count", which is the wrong quantity
   inside a container and was uncited to the dependency. The floor is larger and
   less controllable than the brief's framing implied, which strengthens the case
   for decoupling - and section 6 now records that no step in this proposal moves
   it.

8. **"The grants file shows auth cannot insert into `app_user_identities`."**
   Reading only `db/migrations-ts/20260702000900_grants.ts` gives that
   impression. `db/migrations-ts/20260818000300_credential_lifecycle.ts` grants
   the insert separately, with a comment saying it exists so auth can create the
   identity row. Step 2 needs no new grant.

9. **"`identities::lookup_relay_email` has one production caller."** That is the
   call inside `relay_alias_for`. `relay_alias_for` itself has several production
   callers, one of which rides a refresh-token grant rather than a code grant -
   which is why step 3 must also touch
   `crates/zeroship-auth/src/oidc/refresh.rs`.

10. **Brief errata, kept short because neither carries a design consequence.**
    The cross-continent latency multiplier came from a table in
    `docs/architecture/data-system.md` about creator-database co-location, not
    from any measurement of the gateway login path, and no topology under
    `deploy/` configures regions; any magnitude for this path is unmeasured. And
    `credential_authentication_allowed` is in `crates/zeroship-gateway/src/sync.rs`,
    a top-level module, not a router submodule; the brief's line numbers were
    right for the file it named wrongly.

11. **My own, while checking claim 2's neighbour.** I first checked for
    `signing_keys` across the whole `crates/zeroship-gateway/` tree and found
    matches, which reads as a refutation of "the gateway never touches it". The
    matches are doc comments in
    `crates/zeroship-gateway/tests/oidc_rp_e2e.rs`. "No reference in
    `crates/zeroship-gateway/src`" is right; "no reference in the crate"
    would have been wrong. A grep that does not distinguish production from test
    answers a different question than the one asked.

12. **Two stale doc comments found in passing, recorded so nobody reasons from
    them.** `RotationSingleFlight` in `crates/zeroship-gateway/src/anchors.rs`
    justifies its per-thread scope by citing "the short cached wrapper", while
    the module header of the same file records that the cached-wrapper columns
    were deleted. And `GateState` in `crates/zeroship-gateway/src/lib.rs`
    describes a metering flush task and a control usage-ingest endpoint, neither
    of which exists. In both cases believe the code.

13. **REFUTED objection, recorded because the refutation is the useful part.** A
    reviewer reported that on the `/session` fast path the order is inverted -
    that `session_cookie_family_revoked` is evaluated first and the pushed check
    second - which would have falsified the "the read is unreachable when the
    pushed data is untrustworthy" leg for that site. It is not inverted. In
    `crates/zeroship-gateway/src/auth_token.rs` the `is_pairwise_subject` and
    `credential_authentication_allows` conjunction is the outer `if` that gates
    entry to the block performing the read; the second
    `credential_authentication_allows` inside is the post-read re-check, matching
    both dispatch arms. Reading the inner lines without the enclosing condition
    produces the inverted reading. The text stands unchanged.

14. **REFUTED in part: the gate arm step 1 originally asked for could not have
    been built.** It asked for an arm counting production `crate::db::checkout`
    call sites "reachable from the dispatch entry points". Gates in this tree are
    shell scripts and cannot compute Rust reachability; the arm would have ruled
    on a grep over a hand-listed file set while declaring a count and clearing a
    floor, which is the ceremony of `tests/lib/gate_arms.sh` applied to the wrong
    set. Step 1 now binds that property behaviourally and reserves shell arms for
    the two claims a grep really does settle: the revoked grants, and the
    agreement of the retention boundaries against the credential ceilings.

15. **My own, about this document's exposure to its own policy.** The "no
    magnitudes" policy was enforced against digits and not against words, so an
    earlier revision carried code-derived tallies spelled out - table counts,
    call-site counts, feeder counts - each of which a refactor invalidates and
    none of which a digit grep can see. Every one is now a shape statement plus a
    named instrument. The lesson is the one this tree keeps relearning: a sweep
    that matches nothing reports success.
