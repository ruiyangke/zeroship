# Decoupling app identity from database identity

## What this delivers, and what it costs

**"Many apps, one database" is what this design serves.** The creator owns the database and authors
its schema; apps hold graded DML grants on it. No app holds DDL authority, so there is no
owner-app to transfer and no multi-writer schema ownership - section 13 treats that as a different
project.

**An app sees exactly ONE database.** A schema *is* the app's database. The model is a monorepo:
one `zeroship.jsonc`, several apps, one migration source and one set of generated types per
database.

**Databases are provisioned, never auto-created**, on the D1-to-Workers model: create a database,
then bind an app to it. Deploying an app does not conjure one. This belongs to the decoupling
rather than sitting beside it - while `zeroship deploy` can bring a schema into existence, app
identity and database identity are still welded together at the moment that matters most.

**Two costs, both PostgreSQL constraints rather than choices:**

1. **Classified columns lose plaintext reactivity for everyone, including the owner** (cost 2 of
   section 13). One published column set per table per decode stream.
2. **Blanket table grants and prospective default privileges are deleted** (cost 3), so every apply
   must regenerate explicit per-column grants inside the DDL transaction. A migration that fails to
   do so leaves the database unreadable rather than over-readable.

**Scope limit that ships with it:** `readwrite` and `readonly` grants are restricted to apps under
the same creator - which the workspace model satisfies by construction. Cross-creator co-grants are
blocked on O1 (section 14): the unmask policy is authored by the READING app, and no server-side
mechanism reaches it. The same-creator restriction ships without O1 resolved.

**The worker's stale-binding epoch fence is enforced by PostgreSQL, not by a Rust authorization
comparison.** The per-grant role name carries it - `zs_bind_<gid>_e<E>` - and `SET LOCAL ROLE` on
that name is the first statement of the setup batch, so a rotated epoch fails in the database and no
worker code path can skip it. Section 6 has the mechanism and the measurements that settled it.

**Implementation status: design only.** Datastore, Database, Grant and the CDC relay do not exist in
this tree. Target-state prose is not shipped code; see `2026-08-26-runtime-db-binding-decision-log.md`.

**Five consequences are measured, not assumed**, and each is recorded with its measurement: the
role fence works per grant with `WITH SET FALSE`; primary-key narrowing makes all four single-row
write verbs work with column-scoped read authority; the CDC stream is per-app today and platform
journals are already excluded; the table-level grants that would defeat column grants are
latent, not live, because no column grants exist yet; and the two costs an epoch-bearing role name
was suspected of - cross-database apply serialization and superlinear `SET ROLE` - both measure at
zero.

---

An app id is a tenant. It is not a schema name, not a role name, not an encryption salt, and
not a publication key. Today it is all five, by string identity, and that is what makes a database
that outlives its app, or one that two apps share, unrepresentable.

The shape:

- **`Datastore`** - one physical PostgreSQL database (or one SQLite file directory).
  Operator-owned. Creators never name one.
- **`Database`** - one schema inside a Datastore, physically named `db_<dbsid>`. This is the
  unit that is owned, migrated, granted, published and dropped. It replaces "the app's schema"
  everywhere.
- **`Grant`** - the edge, keyed `(app_id)` since an app binds to exactly one database, carrying
  `database_id` and a capability, with capability `readwrite` | `readonly`. There is no `owner`
  capability an app can hold; DDL authority belongs to the creator (section 3).

Enforcement is PostgreSQL role membership, not an in-process check. The worker connects once as
`zeroship_worker`, holds no inherited privilege **over app data**, and narrows per transaction with a
single `SET LOCAL ROLE "zs_bind_<gid>_e<E>"` - the per-grant, epoch-bearing role of 2.2b.

**Two exceptions, both deliberate.** (1) The worker holds `zeroship_workflow_owner` by a plain
`GRANT` with no inherit option
(`db/migrations-ts/20260818000200_worker_database_authority.ts:43`), boot *requires* that membership
(`crates/zeroship-worker/src/db_posture.rs:101-103`) and the fence exempts it by name
(`AMBIENT_MEMBERSHIP_EXEMPTION`, `crates/zeroship-worker/src/db_posture.rs:13`). That role **owns**
every app's journal schema, owner privilege cannot be revoked, and the workflow store never narrows -
it opens with a bare `batch_execute("BEGIN")`
(`crates/zeroship-plugin-workflow/src/store/pg.rs:373`). Section 11 keeps journals app-keyed across
datastores, which replicates the exception into each one. (2) The replication plane is not fenced at
all - see 5.1, and the boot posture *requires* `REPLICATION` and `BYPASSRLS` on that same login
(`crates/zeroship-worker/src/db_posture.rs:96-100`).

So the accurate claim is narrower than it reads: **role membership fences the SQL executor plane,
and nothing else.** Anyone reading it as "the worker cannot reach another tenant's bytes" is wrong on
two paths.

### Why the role is per grant and not per database

**A role per database is unrevocable under co-tenancy. Measured on 17.11.** `SET ROLE` authorizes
against the transitive closure of the memberships held by the role that *connected*, and in this
design that is always the single shared worker login - never the app. Revoking the app's edge is a
control-plane fact with no database consequence while any parallel edge survives:

| topology | after `REVOKE zs_db_1_rw FROM <the app's role>` |
| --- | --- |
| worker serves **one** app | `permission denied to set role` - appears to work |
| worker serves **two** apps, both holding the database role | **1 row, unchanged** |
| control: revoke the second app's edge too, emptying the closure | `permission denied to set role` |

The revoked app's membership is genuinely gone and the worker's reach is untouched, because the
worker's own closure is what the check reads. Interposing a per-app hub role between the worker and
the database role does not help and was measured not to: the closure is only empty when *no* app on
that worker holds the role. Two hops buy nothing one hop does not; transitivity is why, not a
workaround for it. **Under either of those shapes revocation would be fenced by the in-process
binding table and by nothing else** - the exact property this section opens by disclaiming.

Two further measurements on 17.11 close the neighbouring doors:

- **Cross-app reach is open today.** A worker serving app A (ns_1) and app B (ns_2), dispatching for
  A, ran `SET ROLE zs_db_2_rw; SELECT FROM ns_2.t` and got a row. PostgreSQL cannot tell which app is
  dispatching.
- **Nesting does not narrow.** Assuming a hub role and then a database role succeeds - the check is
  against `session_user`, not `current_user`, so `SET ROLE` is a **lateral move inside the closure**,
  never a one-way narrowing. Any design that assumes narrowing composes is wrong.
- `SET SESSION AUTHORIZATION` as the non-superuser worker is denied, so the cheap "re-point
  `session_user` per checkout" variant does not exist.

### What works: a role per GRANT, and `WITH SET FALSE` on its database edge

Reached independently by two reviewers, measured by one on 18.4 and by me on 17.11:

```
CREATE ROLE zs_bind_<gid>_e<E> NOLOGIN;
GRANT zs_db_<N>_<cap>    TO zs_bind_<gid>_e<E> WITH SET FALSE;   -- inherits, cannot be assumed directly
GRANT zs_bind_<gid>_e<E> TO zeroship_worker    WITH INHERIT FALSE; -- assumable, never ambient
```

The data plane's setup batch issues `SET LOCAL ROLE "zs_bind_<gid>_e<E>"` - same statement, same
batch position, no extra round trip. Revocation is `REVOKE` on both edges in one control-plane
transaction. `<E>` is the schema epoch; section 6 covers what rotating it does.

| probe (17.11, unprivileged login, two grants on ONE database) | result |
| --- | --- |
| bare `SELECT`, no narrowing | `permission denied for schema` - fail-closed holds |
| worker assumes the **database** role directly | **`permission denied to set role`** - `SET FALSE` blocks the shortcut |
| narrow to grant A's role, read | 1 row |
| `REVOKE zs_db_1_rw FROM` grant A's role, read as A | **`permission denied for schema`** |
| co-tenant grant B, same database | **1 row, unaffected** |

**`WITH SET FALSE` is the load-bearing clause.** Without it the worker can assume the database role
directly and the chain is decorative - which is exactly why the hub shapes above pass on one app and
fail on two. On 18.4 the same agent measured the revoke landing on **the very next statement of an
already-open transaction**, same backend, no reconnect, and a re-grant restoring service on that
same warm connection.

**This keeps exact confinement.** A grant role inherits exactly one database role, so per-statement
confinement stays one database - unlike assuming a per-app hub, whose closure unions every database
that app can reach. Confinement and revocation stop being a trade.

**The worker may never hold a direct membership in a database role.** One such grant, added for
convenience or by a provisioning path that predates this rule, restores the unrevocable behaviour
measured above, and nothing in PostgreSQL will complain, because both memberships are individually
legal. That is a catalog-checkable invariant, and `db_posture`'s boot check is already shaped to
enforce it: it walks every membership the worker holds
(`crates/zeroship-worker/src/db_posture.rs:22-40` states why it counts rows rather than pairs and
why it is deny-by-default rather than name-matched).

Costs, stated: role count gains `#grants x #live epochs`, with live epochs capped at two by section
6's reaper; every bind, unbind and epoch rotation is shared-catalog DDL serialized through the
control plane, which needs rate-limiting against grant-flapping; and the boot posture gains two
catalog-checkable arms - `inherit_option = false` on every `zs_bind_*` membership, and
`pg_has_role(login, <database role>, 'SET') = false` for every database role.

**For CDC, fan-out comes from control's authoritative Grant topology.** The relay resolves the
Database behind each pgoutput relation and fans it to active grantees; it does not derive authority
from `pg_auth_members` or a worker refresh map. Grant changes are revision barriers that purge old
relay and worker queues before control exposes them. This is target design; there is no such
topology or CDC relay in the tree today, and 5.3 carries that path's own answer.

**The irreducible limit, which no option closes.** PostgreSQL has no server-side notion of *which app
a shared-login session is acting for* - `session_user` is fixed at authentication. Measured: while
narrowed to one grant role, the same session can `SET LOCAL ROLE` to a sibling grant role. So every
available fence decides whether a grant is **alive**, never whether the worker picked the grant
matching the dispatch. That binding is worker-side, enforced by Rust provenance and the absence of a
raw-SQL surface - which is what `AGENTS.md`'s "privilege follows the PROCESS" invariant predicts, and
per-app logins would not change it either, since the process would then hold every tenant's
credential.

Two things go the other way and are stated as costs, not omissions: logical decoding consults no
ACL and no RLS, so CDC's server-side column projection is fenced by publication column lists and
nothing else; cross-creator table sharing is refused because the unmask policy is authored by the
*reading* app and there is no artifact anywhere that the owner controls.

Everything below is measured. Section 14 lists what remains unmeasured and what each item blocks.

---

## 0. What the ask decomposes into

"Multiple apps share one database" is already the shipped topology and needs no work. The worker
takes one DSN for the whole process (`crates/zeroship-worker/src/config.rs:59-60`), and every app
lives in a schema inside it named literally after the app id
(`crates/zeroship-migrate-server/src/apply.rs:257`, `let schema = app_id.to_string();`).

| Ask | Status today | Verdict |
| --- | --- | --- |
| N apps in one physical database, separate schemas | ships | zero cost |
| N apps reaching the SAME tables, same creator | unrepresentable | delivered, priced below |
| N apps reaching the same tables, DIFFERENT creators | unrepresentable | refused, section 3 |
| One app reaching N databases | unrepresentable | **out of scope** - an app sees exactly one database |

**The last row is a decision, not a deferral.** Dropping it is what keeps `env.db.users` a
collection rather than a binding, keeps database resolution `f(app_id)`, and keeps the
mismatched-pair bug class - right role, wrong schema - out of existence rather than bounded by an
argument. Section 9 has the creator-facing half; 2.1 has the security half.

---

## 1. Entities and identity

```
ds_<base62 uuidv7>    Datastore  { engine, cluster_id, dsn_secret_ref, resource_key }
dbs_<base62 uuidv7>   Database   { datastore_id, owner (the CREATOR, never an app - see 4),
                                   physical schema = "db_<dbsid>" }
grant                 PK (app_id)  -- ONE database per app, see below
                     { database_id, capability, granted_to_principal, state }
                       capability = readwrite | readonly     -- no owner, see 3
```

**Every created database gets its own typed id** - `dbs_<base62 uuidv7>`, the same UUIDv7 + base62 +
prefix shape every other entity uses (`crates/zeroship-core/src/typed_id.rs`). That Database id, not
an app id, names the physical schema, migrator role, per-capability roles and apply lock. The
Datastore id instead names the shared publication and slot. Both are target identities.

**Why `dbs` and not `db`.** Every prefix in `typed_id.rs` is three lowercase letters - `app`, `crd`,
`mig` - and its doc comments state the shape `^[a-z]{3}_[A-Za-z0-9]{22}$`. Pick `dbs` to match.

**That shape is a convention, not a parser constraint.** `parse` is `split_once('_')` followed by a
base62 decode of the remainder (`crates/zeroship-core/src/typed_id.rs:139-145`);
`parse_with_prefix` (`:190-202`) compares the prefix to an expected string. Neither enforces length
or charset, and the regex appears only in doc comments, so `db_<22 chars>` would parse. The choice
stands on uniformity with the rest of the tree, and on nothing the parser does.

The physical schema is `db_<dbsid>`, a PostgreSQL identifier under no such convention.

**The grant's identity is the app id, because the grant table is keyed on it.** `<gid>` in
`zs_bind_<gid>_e<E>` is therefore an app id, and the role is per grant because a grant is per app.
Measured on 18.4, `max_identifier_length` is 63 and PostgreSQL truncates past it silently, so the
arithmetic has to be checked rather than assumed: `zs_bind_` (8) plus a typed app id (26) plus `_e`
and the epoch's digits leaves ample headroom, but the epoch sits at the **end** of the name, so a
future prefix change that pushes past 63 would collapse two epochs onto one role rather than error.
The composer must refuse a name it would have truncated.

`Datastore` is a different thing: the physical PostgreSQL database a Database is *placed on*,
operator-owned and never named by a creator. Many Databases sit on one Datastore.

**A DATABASE IS ADDRESSED BY ITS ID, ALWAYS** (operator decision 13, 2026-08-30). There is no
`(workspace, name) -> database_id` resolution anywhere. The creator writes the `dbs_...` id, the CLI
sends it, and the migration service takes it in the URL.

**THIS REVERSES OPERATOR DECISION 6 OF 2026-08-29**, which held the opposite - that the id is an
internal implementation identity, that a creator addresses a database by a workspace-local name, and
that nothing in the creator surface, the CLI output, the generated types or an error message may
contain a `dbs_...` string. That paragraph stood here for one day and is withdrawn.

**The reversal is a consequence, not a change of mind about the same facts.** Decision 6 rested on
this argument, which was the second of its two "sharp edges":

> A route that takes the id in its URL is an exposure. The apply route as currently written is
> `POST /v1/databases/{database_id}/migrations/apply`, which a creator cannot call without holding
> the id. Either that route is control-plane-internal and the creator-facing surface addresses the
> database by name, or the decision above is not being kept.

That reasoning only held while the control plane stood in front and forwarded. **Operator decision 11
removes the forward** (section 4), so the premise is gone: there is no control-plane-internal route
for the id to hide behind, and inventing one purely to keep the id hidden would be building a service
boundary to serve a naming preference.

It is also worth being precise about what the id is not. **An id is not a capability.** Authorization
is "principal may migrate N iff principal owns N", evaluated on the database itself, so a creator
learning a `dbs_...` string gains nothing. Hiding it was only ever ergonomics.

**This matches the pattern already shipped for apps rather than inventing a second one.**
`schema/project-v1.json` defines `app` as the deploy target's app id (uuid) or name, absent on a
fresh project and appended by the first `zeroship deploy`. A database now works the same way, in the
same file, with the same lifecycle.

**What survives from the withdrawn decision is one narrower point, and it is an ergonomic one.** The
physical schema is named `db_<dbsid>`, so PostgreSQL puts it in error text on a constraint violation,
a permission denial or a failed migration. That is no longer a leak to prevent - the id is public -
but a raw physical schema name is still a poor thing to show a creator, and mapping it to something
readable remains worthwhile. It is now a message-quality item, not a security requirement, and it
must not be cited as one.

**The grant key is `(app_id)`.** An app binds to exactly one database, so the app id alone
identifies the row and the database cannot be ambiguous at any call site. There is no
`binding_name`: a binding name exists only to disambiguate between several databases an app can
see.

The relationship is still many-to-many in the direction that matters: **many apps may point at one
database**, which is what "multiple apps share a database" means. Only the reverse - one app,
several databases - is closed.

The physical schema name derives from the **database** id. That single change is what breaks
`crates/zeroship-migrate-server/src/apply.rs:257` and every `quote_ident(app_id)` site in
`crates/zeroship-schema/src/query.rs`, and
breaking them is the point. `crates/zeroship-plugin-db/src/broker.rs:78-79` states the conflation
in its own words - "`schema` is conflated with `app_id` (every app has its own schema named after
`app_id`)"; this is what un-states it.

A DSN never leaves the control plane and the operator config. `Datastore.dsn_secret_ref` names a
platform secret. The worker is configured with a *set* of DSNs and indexes them by
`DbResourceKey`, which already exists, is already a SHA-256 digest chosen so a DSN password cannot
reach `Debug` or a log line, and is already documented as "the identity of one *database's*
resources" (`crates/zeroship-plugin-db/src/service.rs:46`, `:187`, `:193`, `:208`). It needs no
change. Only its cardinality is wrong, and that follows from `DbServiceConfig` holding one URL.

Creators create databases; the control plane places them on a datastore. Bring-your-own-datastore
is out of scope: "which physical database may an app reach" is a privileged decision, and handing
it to the creator surface hands a DSN to the app plane.

---

## 2. Tenant isolation and how it is enforced

### 2.1 What actually protects a tenant today, and what the decoupling changes

The property is: **the set of bytes reachable by a dispatch is a pure function of a
server-injected app id, computed in Rust, with no creator input.** The app id is read from
`SharedState.env_vars["APP_ID"]`; the schema is that string; the role is
`format!("app_{app_id}_role")` (`crates/zeroship-plugin-db/src/auth/bootstrap.rs:148`).

The per-app role is real enforcement but narrower than it reads. `zeroship_worker` is granted
membership in **every** per-app role (`crates/zeroship-migrate-server/src/apply.rs:1173`,
`GRANT {runtime_role_q} TO {worker_q} WITH INHERIT FALSE`), so `SET LOCAL ROLE` never fails for a
wrong app id. What the role fences is a *mismatched pair* - right role, wrong schema. A
*consistently* wrong resolution executes cleanly. That bug class does not exist today only because
resolution is the identity function.

**Resolution stays `f(app_id)`.** With one database per app there is no binding name in creator
code and nothing for the creator to select with, so that bug class never comes into existence. A
security property absent by construction is stronger than one bounded by an argument.

What remains is the ordinary requirement that the app-to-database mapping be server-injected and
never creator-supplied, which 2.2(a) covers.

### 2.2 The mechanism, in three parts, all required

**(a) Resolution is server-injected, and it terminates in Rust.** The control plane resolves the
app's grants at deploy time and hands the worker a binding:

```
DbBinding { db: "dbs_01J...", schema: "db_01J...",
            ds: <DbResourceKey>, cap: "readwrite", epoch: 7 }
```

**One binding, not a map**, because an app sees one database. Nothing creator-supplied selects it -
there is no name for creator code to pass, so the injected value is the whole of the resolution.

**It does NOT travel in the worker-internal `env_vars` map, and this is a correction.** An earlier
draft of this section prescribed injecting it there, "on the same path as `APP_ID`", and defended
the choice on the ground that creator `vars` cannot shadow a worker-internal entry. That defence is
true and answers the wrong question. Shadowing is a *forgery* concern; the requirement here is
*disclosure*. `crates/zeroship-runtime/src/core/init.rs:3516-3520` copies every entry of that map
into `process.env`, so the prescribed vehicle would have published both ids to app JS through
`JSON.parse(process.env.ZEROSHIP_DB_BINDING)` or any npm package that walks `Object.keys`.

That contradicts `docs/architecture/data-system.md:62`, "Both ids are internal. Neither is exposed
to creators," and `:68`, which holds the datastore id stricter still because it names a shared
resource: it is "a co-tenancy oracle, and it makes noisy-neighbour and resource-exhaustion attacks
aimable rather than speculative." The `ds` field is the sharpest edge. `DbResourceKey` is a SHA-256
digest, stable per datastore (`crates/zeroship-plugin-db/src/service.rs:44-52`); two apps under one
actor that read equal digests have confirmed co-residency.

**The rule this establishes, stated so the next binding-shaped value does not have to rediscover
it.** The worker-internal `env_vars` map is a *disclosure channel* by construction. It may carry
only identifiers the app already possesses. Today it carries exactly two, and both qualify:
`APP_ID` and `ZEROSHIP_DEPLOY_ID` (`crates/zeroship-worker/src/cache.rs:467`, `:474`). An
identifier that names a resource shared with another tenant - the datastore key always, and the
database id as soon as databases are shared - must never enter it. Being unforgeable is not
sufficient; the map is readable.

**The vehicle instead is the channel that already exists.** `DbServiceConfig`
(`crates/zeroship-plugin-db/src/lib.rs:454`) already carries the DSN to the plugin without passing
through V8, and `DbBinding` (`crates/zeroship-plugin-db/src/binding.rs`) is already the per-isolate
identity every `Db` and `Collection` wrapper travels with. The binding extends that struct rather
than adding a fourth env var. App JS never needs these values: `env.db` methods are native ops, so
the plugin reads the binding in Rust at the moment the op runs, and nothing is serialised into the
isolate to be read back out.

Note the asymmetry that makes this workable: `DbBinding` is minted today from `ZEROSHIP_DEPLOY_ID`,
which *is* an env var, and that stays correct - a deploy id is the app's own. The change is not
"stop using injection", it is "stop using the readable channel for the values that are not the
app's to know."

`epoch` is the schema epoch the deploy was gated against. The worker does not compare it in Rust to
authorize a transaction; it composes the role name the setup batch sends (section 6).

An app whose binding is absent is a hard refusal with the same shape as `collection_not_declared`
(`crates/zeroship-plugin-db/src/descriptor.rs:1-31`, whose comment on why there is deliberately no
`Option` applies verbatim one axis up). `cap` is `readwrite` or `readonly`; there is no `owner`
capability an app can hold.

**(b) The grant is a PostgreSQL role membership, and the session narrows to exactly one database.**

```
zs_db_<dbsid>_mig     owns schema db_<dbsid>                    (replaces the per-app migrator)
zs_db_<dbsid>_rw      USAGE on db_<dbsid> + column-listed DML    (replaces app_<id>_role's grants)
zs_db_<dbsid>_ro      USAGE on db_<dbsid> + column-listed SELECT
zs_bind_<gid>_e<E>    NOLOGIN, no privileges of its own; inherits exactly ONE database role
zeroship_worker       LOGIN, member of zs_bind_<gid>_e<E> WITH INHERIT FALSE, per live (grant, epoch)
```

- Granting: `CREATE ROLE zs_bind_<gid>_e<E> NOLOGIN`, then the two edges of the block above. One
  control-plane transaction.
- Revoking: `REVOKE` on both edges, in one control-plane transaction. **The role is not dropped**, and
  that is deliberate: `SET LOCAL ROLE` then fails `42501 permission denied to set role` rather than
  `22023 role does not exist`, which is the split the error taxonomy rests on (6.3), and re-granting
  restores service on the same warm connection (6.1). A `DROP ROLE` belongs to exactly two places -
  the epoch reaper (6.2) and app teardown (section 11) - and both of those mean something a revoke
  does not.
- The data plane issues `SET LOCAL ROLE "zs_bind_<gid>_e<E>"` - **not** the database role, which
  `WITH SET FALSE` puts out of reach, and not an app principal, which does not exist. It replaces
  `set_local_role_sql` / `tx_session_setup_sql`
  (`crates/zeroship-plugin-db/src/auth/bootstrap.rs:161-163`, `:204-212`, `:226-233`) unchanged in
  shape and cost: still one statement in the same simple-query batch as the DB-1 timeout guards,
  applied by the same two functions (`crates/zeroship-plugin-db/src/exec.rs:293` and
  `crates/zeroship-plugin-db/src/transaction/mod.rs:211`).

Narrowing to a database role transitively is free and exact. Measured on PostgreSQL 18.4, over a
login reaching two database roles through one intermediate:

| Probe | Result |
| --- | --- |
| `BEGIN; SET LOCAL ROLE zs_db_m_rw; SELECT FROM ns_m.secrets` | 1 row, `current_user = zs_db_m_rw` |
| `BEGIN; SET LOCAL ROLE zs_db_m_rw; SELECT FROM ns_t.secrets` | `ERROR: permission denied for schema ns_t` |

`SET ROLE` resolves membership transitively, which is why the design reaches the database role's
privileges without ever unioning them onto the live session ambiently, and why per-statement
confinement is exactly one database at all times. No PL/pgSQL assertion, no `pg_has_role` check, no
extra round trip. It is also why the intermediate must be per grant rather than per app: see "Why
the role is per grant and not per database" above.

**(c) The session inherits nothing. Fail-closed is a grant option, not a role attribute.**

`zeroship_worker` carries the `INHERIT` role attribute
(`db/migrations-ts/20260818000200_worker_database_authority.ts:35`), so if its memberships were
granted plainly, its session would hold the union of every tenant's privileges *before* any
`SET LOCAL ROLE` ran, and any code path reaching SQL without the setup batch would read everything.
That is not hypothetical - see 2.3.

Measured on 18.4, three arms differing in one variable:

| Configuration | Bare `SELECT FROM ns_t.secrets` as the login role |
| --- | --- |
| plain `GRANT` of the intermediate to `w`, `w` INHERIT | **1 row returned** |
| same grant, `ALTER ROLE w NOINHERIT` | **1 row returned** |
| `GRANT ... TO w WITH INHERIT FALSE`, `w` INHERIT | `ERROR: permission denied for schema ns_t` |

The role attribute does nothing here. PostgreSQL 16+ records `inherit_option` per membership at
grant time (`SELECT inherit_option FROM pg_auth_members` returned `f` only in the third arm), and
the pre-existing membership stays inheriting when the attribute is flipped. **The grant must carry
`WITH INHERIT FALSE`.** `SET LOCAL ROLE` still works under it - verified in the same cluster,
including to a transitively-reachable database role.

**The posture this needs already ships for the app-role shape, and is re-pointed rather than
invented.** `crates/zeroship-migrate-server/src/apply.rs:1173` already issues
`GRANT {runtime_role_q} TO {worker_q} WITH INHERIT FALSE`, and
`crates/zeroship-worker/src/db_posture.rs:86-107` refuses boot on superuser, createrole, createdb,
missing replication or bypassrls, and missing workflow-owner membership, over a query that COUNTS
every inheriting membership row rather than checking a pair
(`INHERITED_MEMBERSHIPS_SQL`, `crates/zeroship-worker/src/db_posture.rs:42-60`). The change is the
role family it names: `zs_bind_*` instead of `app_*_role`, plus the second arm asserting
`pg_has_role(login, <database role>, 'SET') = false`. That is the arm that makes the fence
fail-closed by construction rather than by every call site remembering.

### 2.3 The unfenced execution sites, which must close in the same change

The role fence is applied by exactly two functions:
`crates/zeroship-plugin-db/src/exec.rs:293` (`query_postgres_pool_with_autocommit_role`) and
`crates/zeroship-plugin-db/src/transaction/mod.rs:211` (`apply_per_app_role`). These do not:

- `crates/zeroship-plugin-db/src/crud/unmask.rs:816` takes `pg.pool_handle()` and issues
  `INSERT INTO "{app_id}"."__zeroship_audit_unmask"` at `:817-822` with no role and no wrapping
  transaction. Two more sites in the same file do the same (`:492`, `:599`).
- `crates/zeroship-plugin-db/src/crud/mask_drift.rs:405`, `:726`, `:804`, `:925` do the same, one
  of them selecting the **plaintext parent column**.

Today the blast radius of an unfenced statement is one app's schema. Under many-to-many it is every
database in the datastore. Two changes, both mandatory:

1. `WITH INHERIT FALSE` (2.2c) makes an unfenced statement fail rather than succeed.
2. The pool handle stops being reachable from CRUD code. The role-applying wrapper becomes the only
   route to a connection, so a future unfenced site fails to compile rather than reading everything.

### 2.4 Column-level GRANT replaces the descriptor as the masking authority

`crates/zeroship-plugin-db/src/descriptor.rs:1-3` makes the creator-authored, unsigned runtime
descriptor "the data plane's SOLE schema authority", and `:10-31` records that the live-catalog
read was deleted because "the catalog could only ever agree with the descriptor or be stale". That
reasoning holds exactly while one app owns the database exclusively. It is also already the weak
link on a private database, because nothing below the descriptor objects.

The migration service emits column-level grants from the **owner's own IR**, withholding every
column whose classification is not `none` and granting the column that holds the mask instead.

*The transcripts in this section and in 5.2 use a probe table whose columns are literally
`id, name, ssn, ssn_masked, dob`. **`ssn_masked` is that probe's own name, not a platform
convention** - SC-6's storage flip (`3fd54f177`) deleted the `_masked` sibling entirely, and the
field's own column now holds the mask while `__zs_raw__ssn` holds the plaintext. The measurements are
about PostgreSQL grant and publication semantics, which do not depend on the names, so they stand as
recorded; only read them for the semantics, not for the naming.*

Measured on 18.4:

| Grant state on `ns_t.patients` | `SELECT ssn` as `zs_db_t_rw` |
| --- | --- |
| table-level `GRANT SELECT` **plus** `GRANT SELECT (id, name, ssn_masked)` | **plaintext returned** |
| column list only, table-level revoked | `ERROR: 42501 permission denied for table patients` |
| column list only, `SELECT id, name, ssn_masked` | 1 row, masked value |
| column list only, column `dob` added by a later `ALTER TABLE` | `ERROR: permission denied` |

Three consequences, all load-bearing:

- **THE COLUMN-GRANT MODEL BREAKS EVERY WRITE VERB AS THE BUILDERS EMIT THEM. Measured on 17.11.**
  `zeroship-schema/src/query.rs` emits `RETURNING *` at **twelve** sites (insert, updateOne,
  insertMany, updateMany, delete, soft-delete/restore, upsert, findOrCreate). Under a column-only
  grant, `*` expands to columns the narrowed role cannot read:

  | statement | result as `app_login` holding `SELECT (id, pub), INSERT (id, pub), UPDATE (id, pub)` |
  | --- | --- |
  | `INSERT ... RETURNING *` | `ERROR: permission denied for table t` |
  | `INSERT ... RETURNING id, pub` | 1 row |
  | `SELECT *` | `ERROR: permission denied for table t` |
  | `SELECT id, pub` | 1 row |
  | `UPDATE ... RETURNING *` | `ERROR: permission denied for table t` |

  **This prerequisite is now DONE** (`a22ede156`). All twelve sites emit a projection built from the
  descriptor's `readable` set and `storage.valueColumn`, so the v2 payload is load-bearing in Rust
  rather than a version tag nothing reads. `SELECT *` was already gone. The decisive test mints a
  role holding `INSERT`/`UPDATE` on the raw column and **not** `SELECT` - a grant `*` cannot express -
  and runs seven verbs under column-scoped SELECT/INSERT/UPDATE, plus PostgreSQL's necessarily
  table-scoped DELETE privilege, with a control substituting `RETURNING *` back and getting `42501`
  on each. DELETE grants no read access to the withheld column.

  **The remainder is now closed.** Four single-row verbs narrowed with
  `WHERE ctid = (SELECT ctid FROM ... LIMIT 1)`, and `ctid` is a **system column that
  column-level `SELECT` does not cover** - measured, the same role is refused `SELECT ctid` (42501)
  while served `SELECT id`. They now narrow through the immutable `id TEXT PRIMARY KEY` and lock the
  selected row with `FOR UPDATE`. The live column-grant test keeps the direct `ctid` refusal as its
  control, then executes update, soft-delete, restore and purge successfully; purge also carries
  the table DELETE privilege PostgreSQL requires because that verb has no column form. A second
  live arm executes the replacement data-plan's bounded update and delete.

  **A shipping defect surfaced on the way, and it is worth reading as evidence about the tests rather
  than about upsert.** Every PostgreSQL upsert was already broken: `DO UPDATE SET "version" =
  COALESCE("version", 0) + 1` has target and `excluded` both in scope, so PostgreSQL refuses the
  statement with `42702 column reference "version" is ambiguous`, and `doc_has_version` is always
  false on the dispatch path - so every upsert took that branch. It survived because the upsert tests
  that **execute** run on SQLite, which accepts the unqualified reference, while the PostgreSQL ones
  only **compare strings** - and one of them asserted the broken literal, so it went red when the bug
  was fixed. Any claim in this document that rests on a string-compared SQL test should be read with
  that in mind.

- **A table-level grant defeats a column list.** Column grants add, they never subtract. The
  blanket `GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA` at
  `crates/zeroship-migrate-server/src/apply.rs:1229` must be **deleted**, not supplemented.
- **`ALTER DEFAULT PRIVILEGES` has no column-list form**, so the prospective table-level rules at
  `crates/zeroship-migrate-server/src/apply.rs:1231-1234` must be deleted too. Every apply
  regenerates the explicit per-column grants inside the same transaction as the DDL.
- **Schema evolution fails closed by a PostgreSQL property**, not by a reconciler: a column added
  after the grant carries no ACL entry and is unreadable. The migration service therefore carries no
  correctness obligation beyond emitting the list.

The producer is the right one. The migration service already writes classification and mask kind as
`COMMENT ON COLUMN` sentinels - `zero-migrate:enc:<mode>:<keyId>:<wraps>` and
`zero-migrate:mask:kind=<kind>,classification=<class>`
(`crates/zeroship-migrate-backend/src/mask_codec.rs:102-110`, `:201-209`) - and
is the one process in the tree that does not execute creator code, so this satisfies the AGENTS.md
privilege invariant with no `SECURITY DEFINER` wrapper and no system-schema state. A creator
migration cannot widen the ACL back open: the CONFINED ceiling grants exactly
`schema.create_table`, `schema.rename` and `safety.destructive_ops` and nothing else
(`crates/zeroship-migrate-server/policies/confined.policy.toml`, whole file), and no `access.grant`
or `sql.raw` key appears in it.

The runtime descriptor is demoted from security boundary to shape declaration. That is its correct
altitude, and it is true whether or not many-to-many ships.

### 2.5 RLS is available on the query path and void on the decode path

Measured on 18.4, one variable changed:

| Current role | `count(*)` on an RLS table with `USING (owner = current_user)` |
| --- | --- |
| `w` (login, BYPASSRLS) | 1 |
| `zs_db_t_rw` via `SET LOCAL ROLE` from that same login | 0 |

`BYPASSRLS` does not follow through `SET ROLE`; it is an attribute of the current role. So once the
session is narrowed, RLS is enforceable even though the worker login must hold `BYPASSRLS`
(`crates/zeroship-worker/src/db_posture.rs:34-37`). RLS is not required by this design and is not
proposed here, but it becomes usable, which it is not today.

It is void on the CDC path, which is section 5.

---

## 3. Which sharing is permitted, and which is refused

**Permitted: the CREATOR owns the database and authors its schema; apps hold DML grants on it.**

- `readwrite` - DML on granted columns. No DDL.
- `readonly` - SELECT on granted columns.

**There is no `owner` app capability.** Migration authority is not something an app can hold: it
belongs to the workspace, and the migrator role `zs_db_<dbsid>_mig` is named by no app. An app's
grant only ever says what DML it may do.

That removes a class of problem rather than solving one. With no app holding DDL authority there is
no ownership transfer, no ping-pong between apps, and nothing for an app deletion to cascade into -
the database outlives every app that binds to it, which is the whole point of giving it its own
identity.

**`readwrite` and `readonly` grants are restricted to apps under the same creator** - which the
workspace model satisfies by construction, since every app in one `zeroship.jsonc` has one creator.
The restriction only bites for cross-creator sharing, and it is not because roles are insufficient
but because one authority is unreachable by any server-side mechanism:

> **The unmask authorization policy is authored by the reading app.**
> `crates/zeroship-plugin-db/src/crud/mask_policy.rs:8-30` states it: the policy comes from "the
> creator's own source, at boot, and nowhere else on the PG arm" - the app declares
> `defineMaskPolicy()`, `installSchema` flushes it into that isolate's cache, and "There is no
> durable policy store on PG" (the `__zeroship_admin` routines were deleted 2026-08-27).
> `check_unmask_authorization` consults the per-app cached policy
> (`crates/zeroship-plugin-db/src/crud/unmask.rs:318`, `:327`). A co-grant-holder ships a permissive
> policy in its **own** bundle and unmasks the owner's PII/PHI/PCI. The owner has no artifact
> anywhere that the co-tenant's isolate consults.

The catalog sentinels do **not** close this. They carry mask kind and classification; they do not
carry the actor-to-classification policy. Column grants (2.4) fence the plaintext parent but the
unmask path is a *privileged, audited* read the platform deliberately provides, and it is gated by
a document the reader wrote. Where the owner-side policy lives is open question O1.

**Refused permanently: two apps under different creators writing the same tables**, for that reason
plus two structural ones:

1. **A PostgreSQL schema has exactly one owner, and ownership IS the migrator's authority.**
   `ALTER SCHEMA {proj_q} OWNER TO {role_q}` (`crates/zeroship-migrate-server/src/provisioning.rs:143`).
   The protective REVOKE on the journal was deleted precisely because owner privileges are implicit
   and unrevokable, and the note accepting it reasons "it is their database and corrupting it breaks
   only them" (`crates/zeroship-migrate-server/src/provisioning.rs:164-171`). Both halves are false
   under cross-creator sharing, and
   there is no second owner slot to allocate.
2. **The apply lock is on the wrong axis and the journal has no tenant column.** The only apply-time
   lock that runs is `pg_advisory_xact_lock(hashtextextended($1, 0))` over the publication name
   (`crates/zeroship-migrate-server/src/publication.rs:86`), and the publication name is a hash of
   the app id. Two apps in one database take *different* keys and their DDL interleaves with no
   mutual exclusion.

Within one creator, a co-grant-holder mis-declaring a mask policy is not a boundary crossing. Across
creators it is, and no role fixes it.

---

## 4. Ownership and migration of a shared database

- **A database is owned by the CREATOR, not by an app.** Operator decision, 2026-08-29:
  *"the creator has full control of the db, even breaking changes, the creator has to take the
  risk; the db migration is not coupled with app."* The model is a monorepo - several apps in one
  workspace sharing one migration source and one set of generated types.

  So `Database` carries the **creator's** ownership and no app column at all: schema authority never
  passes through an app identity, and the migrator role is named by no app.
- **ONE route, and the control plane is not on it** (operator decisions 11 and 13, 2026-08-30). The
  CLI calls the migration service directly, addressing the database by id.

  ```
  creator -> POST /v1/databases/{database_id}/migrations/apply    (on zeroship-migrate-server)
  ```

  **The CLI reuses the control URL and the EDGE routes the path there** (operator decision 14,
  2026-08-30). No new config key, flag or environment variable: `control` already resolves through a
  four-step precedence with printed provenance, and a second endpoint is a second thing to omit.

  This closes a gap the other decisions left open. The migration service has no separate public host,
  and its raw port remains loopback-only for operator tunnelling. The tracked edge now exposes its
  creator routes by splitting the control host before the control catch-all. Deleting the forward
  without that split would leave the CLI with no route, not merely no URL.

  ```
  http://control.{$ZEROSHIP_DOMAIN} {
    handle /v1/*           { reverse_proxy migrate-server:9091 }
    handle                 { reverse_proxy control:9090 }
  }
  ```

  The shape is already used one block above it: `auth.<domain>` splits `/oauth2/*` and
  `/.well-known/*` off before its catch-all.

  **A shared hostname is not the control plane being in the path.** The request never reaches
  control - Caddy hands it to `migrate-server` directly, no control code runs and no second
  authorization happens. The deleted forward was a SERVICE in the path; this is a DNS name. The
  distinction is written down because "migrations go to control.<domain>" invites exactly the proxy
  that was just removed.

  Two costs, taken deliberately: the edge config becomes load-bearing (a missing rule yields control's
  404, and an apply that reaches nothing can still write a ledger row - see the empty-apply path), and
  the control plane must never define a `/v1/*` route. The second gets a gate arm rather
  than a convention. `Caddyfile` is the LOCAL edge; a production ingress needs the same rule.

  **THIS REPLACES A TWO-ROUTE SPLIT** in which a creator-facing
  `POST /v1/projects/{project}/databases/{name}/migrations/apply` was resolved to an id and forwarded
  to a control-plane-internal id-bearing route. Both the split and the forward are deleted.
  The DELETED `crates/zeroship-control/src/migrations_api.rs` (214 lines) went with them.

  **Deleting the forward costs nothing in authorization, because it was never the authorization.**
  `crates/zeroship-migrate-server/src/api.rs:98` already calls
  `verify_action(token, app_id, Action::AppsDeploy, ...)` against `ControlPlaneAuthenticator`, which
  holds its own `control_pg` client, its own `PolicySet` and its own `BearerVerifier`
  (`crates/zeroship-migrate-server/src/auth.rs:42-46`) and reads the creator's own bearer. The
  control hop performed an authorization that the migration service then performed again. Removing it
  removes the duplicate, not the check.

  It also retires a hazard the split created. An "internal" route invites the belief that it is a
  privileged plane, and the day one accepts the control key instead of the caller's bearer, every
  creator holding a `dbs_...` inherits platform authority over that database. There is now no internal
  route to confuse, and the surviving route demands the caller's own bearer.

  `crates/zeroship-authz/src/resource.rs:14-16` gains `Resource::Database { id }` - it is `App { id }`
  and `Any` today - with the policy **"principal may migrate N iff principal owns N"**, checked
  against the creator directly with no app indirection. **This is the load-bearing change in the whole
  re-key**, not the 116 `app_id` occurrences across the migration service's seven files: those are
  mechanical, and `crates/zeroship-migrate-server/src/auth.rs:79` building `Resource::App { id }` is
  not.

  **The addressing decision does NOT unblock authorization.** "Principal owns N" still requires an
  owner, so the project/workspace row below is still required. It was the ADDRESSING that depended on
  `(project, name)`; the authorization never did.
  There is one `zeroship.jsonc` for the whole workspace, so a `Database` hangs off the
  **project/workspace** row rather than a bare creator account, and "the apps that share this schema"
  is a structural fact of the config rather than a convention several files have to agree on. See
  section 9 for what that costs the project file.
- **The apply lock moves to the database**, and it must be SESSION-scoped, not transaction-scoped.
  `pg_advisory_xact_lock` releases at the first COMMIT, and the apply commits many times (below), so
  it cannot hold across one. The engine already takes a session lock around a whole plan and releases
  it explicitly (`crates/zeroship-migrate-postgres/src/backend/session.rs:60-77`,
  `crates/zeroship-migrate-core/src/engine.rs:1382-1412`); the remaining change is its logical key,
  from the app-as-project to the database. The old 32-bit `hashtext` collision defect was removed on
  2026-08-29 by splitting `hashtextextended` into two `int4` keys. That transport-width fix does not
  perform the app-to-database rekey proposed here.
- **The apply advances the schema epoch. THE EPOCH ROTATION IS ATOMIC; THE APPLY AS A WHOLE IS NOT,
  AND CANNOT BE.** T4 writes the new epoch, mints every `zs_bind_<gid>_e<E+1>` role, widens the
  publication and emits the marker in one transaction. Reaping `E-1` is the separate T1 before DDL.
  **An apply that cannot reap `E-1` refuses before DDL and never advances to `E+1`** (6.2), keeping
  the cluster-shared catalog bounded and fail-closed rather than leaking a role family per migration.

  The DDL cannot join it. `crates/zeroship-migrate-core/src/engine.rs:2656` states the engine's
  contract in its own words - "everything ahead of it commits in its own transaction" - and the host
  loops the engine once per IR file (`crates/zeroship-migrate-server/src/apply.rs:617`, `:625`), whose
  own comment records that earlier files are already committed when a later one fails (`:779`). The
  engine's crash recovery is journal-driven on exactly that basis. A wrapping transaction would have to
  swallow the journal bootstrap and would destroy that recovery model, so "one transaction covering the
  DDL" is not a thing this engine can be asked for. An earlier version of this bullet asked for it.

  **THE STRUCTURE, settled 2026-08-29 from two independent designs.** All SUBTRACTION from the
  catalog happens before any DDL commits; all ADDITION happens after every DDL has committed:

      L    host takes a SESSION advisory lock on the database key, held to U
      P    preflight: lower every IR file, refuse a denied plan
      S    record_submitted on the control connection - the audit row opens
      T1   one transaction: head FOR UPDATE, reap E-1 roles, claim, shrink this Database's publication members
      D1..DN  the DDL, engine-journalled, EVERY file passing LockMode::AlreadyHeld
      T4   one widen transaction: mint E+1 roles, widen those members, advance the head to E+1,
           and emit the marker - iff the effective committed schema delta requires rotation
      C    mark_applied / mark_failed on the control connection
      U    release the lock

  **A SERVING APP IS NEVER FENCED.** The apply mints `E+1` and drops `E-1`; it never touches `E`. So
  "what unfences an app when the process dies mid-apply" has the answer NOTHING HAS TO - every partial
  crash state leaves the app serving on `E`. Recovery is a plain retry: the engine journal skips
  completed DDL, and the head's recorded journal state decides whether the rotation still owes a
  rotation. A retry after a crash between the last DDL and T4 finds every version already applied and
  must STILL rotate; keying that on "did this run apply anything" strands the database at `E` with a
  `v2` schema forever.

  **The reap of `E-1` moves to the front, and this supersedes the bundled form this bullet used to
  specify.** The settled rule is that an apply which cannot drop `E-1` refuses to advance to `E+1`.
  That refusal is only fail-closed if it happens BEFORE anything commits. Bundled into the rotation it
  means: N DDL transactions committed, the schema at `v2`, the epoch stuck at `E`, and every retry
  failing on the same `DROP ROLE` forever - worse than the leak it guards. At the front the identical
  refusal costs a clean 409 with zero side effects. The failure it guards is real and measured: a bind
  role that has been granted a privilege OF ITS OWN cannot be dropped ("cannot be dropped because some
  objects depend on it / DETAIL: privileges for table t"), which is exactly the violation of "no
  privileges of its own" this design already forbids.

  **The cost of moving it, stated:** an isolate at `E-1` loses its grace at the START of the apply
  rather than the end, and recovers by re-resolving to `E`. THAT RECOVERY DOES NOT EXIST YET - the
  worker composes its role name locally (`crates/zeroship-plugin-db/src/auth/bootstrap.rs:148-150`)
  and there is no resolve step on the connection path. So the front reap is correct ONLY once the
  binding producer lands, which the epoch needs regardless.

  **The advisory lock is taken by the HOST**, not by the engine per file. This prerequisite landed in
  `8dcd79628` on 2026-08-29: the host acquires once on its pinned session, passes
  `LockMode::AlreadyHeld` for every IR file, and releases after the full set. Before that change the
  engine released the lock at the end of the first plan, so every later file ran unlocked.

  **Publication reconciliation is replaced, not reused or aliased.** The current app-keyed wrapper
  and `ALTER PUBLICATION ... SET TABLE` body cannot safely edit a shared Datastore publication. The
  target uses the CDC design's locked per-member shrink-before-DDL and widen-plus-marker bracket.

  **STILL OPEN:** the ledger cannot close exactly once across a crash, and this is structural rather
  than a defect to fix. The audit row and the schema live in DIFFERENT DATABASES - the store opens its
  own connection on the control DSN - so no transaction spans both. The ledger is therefore an audit
  projection with at-least-once closure, and the authority for "what schema does this database have" is
  the app database's journal plus its head row, which do move together. Any design that treats the
  ledger as authoritative is claiming a distributed transaction it does not have.
- **Migrator role is `zs_db_<dbsid>_mig`**, named by no app. One migrator forever, so the ownership
  ping-pong and the silently-orphaned `ALTER DEFAULT PRIVILEGES` rules cannot occur.
- **`ExecutorConfig::new(project_id, project_schema, policy)` stops taking the app id three times**
  (`crates/zeroship-migrate-server/src/apply.rs:280-298`). All three become `db_<dbsid>`. The engine
  already contemplates several apps under one project schema; the host stops collapsing the axis.
  `SchemaScope::Allowlist(Vec<String>)` already exists with a working case-insensitive `permits`
  (`crates/zeroship-migrate-ir/src/policy.rs:36`, `:64`), so multi-schema confinement is representable
  today and only the binder is scalar.
- **The deploy gate is SCALAR, and the row it matches against is re-keyed onto the DATABASE.**
  `crates/zeroship-control/src/registry.rs:460-486` predicates the deploy UPDATE on ONE
  `descriptor_sha256` matching the newest `applied` row, and `Manifest.runtime_descriptor`
  (`crates/zeroship-bundle/src/manifest.rs:172`) keeps its present shape. What changes is the
  subquery: it reads `WHERE m.app_id = $3` today (`crates/zeroship-control/src/registry.rs:478`), and
  an apply belongs to a database, not to an app. So `zeroship.app_schema_applies` keys
  `(database_id, migration_id)` and the predicate resolves the app's database through its grant
  first. **Keeping `app_id` in that key would give each co-tenant its own apply rows for one
  physical migration**, which is the same conflation this design removes, one table over. It is also
  what makes binding a second app to an existing database work at all: with a database-keyed row, the
  second app's first deploy matches the apply that already ran; with an app-keyed row it finds none.

  **The bundle carries a hash, never a database id.** That is deliberate and it is also what keeps
  the id off the creator surface: a `.zship` is an artifact a creator builds and can open. The
  binding comes from the control plane's grant table, never from the bundle - the bundle asserts
  hashes, not preconditions.
- **Every app's deploy gate checks the database's applied hash**, and under the monorepo model this
  is NOT co-tenant coupling. All apps in the workspace build against one migration source and one
  generated-types artifact, so a schema change means *rebuild the workspace* - the same mechanics as
  changing a shared library, with the creator owning the risk.

  The gate exists because an app built against schema v1 must not be SERVED against schema v2, or
  it reads columns that have moved. That is correctness, not politics between apps.
  The creator resolves it by rebuilding and redeploying, which is the expected workflow rather than
  a cost to be weighed.

  The residual, which is real but is the creator's to take: apps deploy independently, so there is a
  window in which one app is rebuilt and another is not. Breaking changes are permitted; the
  platform's job is to make the mismatch loud, not to prevent it.

---

## 5. CDC, and the fence that actually binds it

### 5.1 Logical decoding consults no ACL and no RLS

Measured on 18.4. The same role that gets `ERROR: permission denied for table patients` on
`SELECT ssn` receives the plaintext `555-44-3333` in the decoded stream when the publication has no
column list. The decode path in this tree runs on the worker's own login connection
(`crates/zeroship-plugin-db/src/change_stream_pg.rs:184` passes `self.backend.url()`), and that role
is required to hold `REPLICATION` and `BYPASSRLS`
(`crates/zeroship-worker/src/db_posture.rs:96-100`;
`db/migrations-ts/20260818000200_worker_database_authority.ts:35`). Column grants, RLS and
`SET LOCAL ROLE` are all executor-side. Decoding does not go through the executor.

The tuple reaches subscribers verbatim: `ChangeEvent.new_tuple` is documented as "Text-encoded
column values for the affected row", "Populated by the WAL consumer from pgoutput Insert/Update/Delete
frames" (`crates/zeroship-plugin-db/src/broker.rs:97-112`).

### 5.1a What is datastore-scoped and what is cluster-scoped

The measurements below remain right, but their conclusion moved on 2026-08-30. This section
previously chose one publication per Database and one slot per (Datastore, worker). Measured on 18.4:

| object | catalog `relisshared` | consequence |
| --- | --- | --- |
| `pg_publication`, `pg_publication_rel` | **`f`** | publications are DATASTORE-scoped: per-database, and the same name in two databases of one cluster is two independent objects |
| `pg_authid`, `pg_auth_members` | **`t`** | roles and memberships are CLUSTER-shared: every grant role and every live epoch competes in one namespace |
| replication slots | n/a - `pg_replication_slots` is a shared-memory view | CLUSTER-scoped, see below |

Directly probed rather than inferred: the same publication name created in two databases of one
cluster coexists, each database's `pg_publication` showing exactly its own row and not the other's.
That proves publication locality and independent names, not one publication per Database schema.
The target uses one shared publication in each Datastore.

**Slot budget is cluster-scoped, and the ceiling is low.** `max_replication_slots` defaults to **10** and is
`context = postmaster`, so raising it is a restart. Ten slots created in one database of a cluster
are all visible from a *different* database of that cluster, and the eleventh - created from that
other database - fails **`SQLSTATE 53400`, "all replication slots are in use"**. Slot cardinality is
therefore a cluster budget shared by every datastore tenant. Budget and namespace do not decide
decode scope or ownership: one logical slot decodes only its own Datastore, and relay ownership
removes the worker multiplier. One slot per Datastore means at most ten Datastores on the stock
cluster, fewer when anything else consumes a slot.

`max_slot_wal_keep_size` measures as **`-1`** - unbounded WAL retention - on a stock server, so one
abandoned slot can grow `pg_wal` until the cluster dies. It is `context = sighup`, so bounding it is
a reload rather than a restart; that is a blast-radius cap, not a fix, and it does not clean up an
abandoned slot.

### 5.2 The publication column list is the only server-side fence, with one column set per table

**UNRESOLVED, and it is the most fragile load-bearing fact in this document.** Two independent
reviews reached this collision separately. The measurement below is on **18.4 only, on a single
INSERT** - an INSERT emits only a new tuple. `UPDATE` and `DELETE` emit an **old** tuple, and whether
a column list filters that image was never measured, on any version.

That matters because the tree already records the fix that collides with it.
`crates/zeroship-plugin-db/src/wal_consumer.rs:551-560`: a `DELETE` under the default replica
identity carries a key-only old tuple, *"which is why a subscription filtered on a non-key column can
miss a delete. **Fixing that needs REPLICA IDENTITY FULL on published tables**"*. Under
`REPLICA IDENTITY FULL` **every** column is an identity column, and PostgreSQL requires a
publication's column list to include the replica-identity columns - so a list that excludes
`__zs_raw__<field>` cannot coexist with the fix.

**So the one server-side fence this design has forecloses the one recorded fix the subscription
feature needs.** This document contained zero occurrences of "replica identity" before this
paragraph.

**Now measured, on PostgreSQL 17.11, and the failure mode is worse than the incompatibility.**

| step | result |
| --- | --- |
| column list under `REPLICA IDENTITY DEFAULT` | works; the withheld column appears nowhere in the decoded stream, and `INSERT`/`UPDATE`/`DELETE` all decode |
| `ALTER TABLE ... REPLICA IDENTITY FULL` with the column list already in place | **accepted, no error** |
| `UPDATE` after that | `ERROR: cannot update table "t"` - *"Column list used by the publication does not cover the replica identity"* |
| `DELETE` after that | same error |
| `CREATE PUBLICATION ... (cols)` while the identity is already `FULL` | **accepted, no error** |

**Neither DDL step refuses, in either order. The failure is at DML time, on the creator's write
path.** So the incompatible combination is silently configurable, and the symptom is not a CDC fault
but a table that has become append-only - every `UPDATE` and `DELETE` failing for a creator who
changed neither.

Three consequences:

1. **The two features are mutually exclusive as designed.** Either classified columns are withheld
   from the wire by a column list, or subscriptions can filter deletes on non-key columns. Not both.
   Whichever is given up must be given up explicitly.
2. **Ordering does not save it.** Because both DDL steps are accepted, no provisioning sequence
   produces a safe combination, and no bracketing rule detects one.
3. **This needs a refusal at the authoring boundary.** If a database has a classified column and
   something asks for `REPLICA IDENTITY FULL` on that table, the migration service must refuse with a
   reason - PostgreSQL will not, and the creator will learn about it when their writes stop.

Measured on 18.4 against the same INSERT, decoded through `pg_logical_slot_peek_binary_changes` with
`pgoutput`:

| `publication_names` | Relation message columns | Tuple |
| --- | --- | --- |
| `p_full` (no column list) | `id, name, ssn, ssn_masked, dob` | contains `555-44-3333` |
| `p_cols` = `FOR TABLE ns_t.patients (id, name, ssn_masked)` | `id, name, ssn_masked` | plaintext absent |
| `p_cols,p_full` together | - | `ERROR: cannot use different column lists for table "ns_t.patients" in different publications` |

Two facts fall out, and the second is the one that decides the design:

- A publication column list **does** filter what logical decoding emits. This is a genuine
  server-side fence, not an in-process filter.
- **PostgreSQL refuses conflicting column lists for one table across the publications named on one
  decode stream.** So "a publication per grant, each with its own column list, all on one slot" is
  impossible. A table has exactly one published column set per stream.

### 5.3 The decisions

- **One relay-owned slot and one pgoutput stream per Datastore.** Two independent reasons are
  measured. Slots replicate decode work, they do not partition it: five slots decoding the same
  40,002 changes cost 1,335 ms against 309 ms for one
  (`docs/proposals/2026-08-26-runtime-db-binding-00-index.md:291-301`, with the PostgreSQL sources
  checked in REL_16 and REL_18 to confirm no output-plugin filter runs before decode). The slot budget
  is a **cluster** budget with a stock ceiling of 10 (5.1a), while decode is bound to one Datastore.
  Relay ownership removes the worker term; the relay fans out that one stream.
- **One relay-owned shared publication per Datastore.** Its membership is the union of every
  Database's safe table projections plus the CDC design's exact heartbeat exception. Creating or
  migrating a Database edits only its member entries under the Datastore publication mutex. The
  current app-keyed `publication_name(app_id)` and reconciler are replaced, not retained or aliased;
  one Database must never run `ALTER PUBLICATION ... SET TABLE` over the shared object.
- **The published column set per table is the INTERSECTION over every grant on the database, and
  the plaintext parent of any column with `classification != none` is never published to anyone.**
  This follows directly from 5.2: one column set per table, and the shared stream serves the weakest
  reader.

  **The column names inverted on 2026-08-28 and this rule inverts with them.** SC-6's storage flip
  (`3fd54f177`) made the field's own column hold the **mask** and moved the real value to
  `__zs_raw__ssn`; the `"ssn_masked" AS "ssn"` substitution is deleted and there is no mask sibling
  any more. So the rule is now: **publish `ssn`** - which holds the mask - **and exclude
  `__zs_raw__ssn`**, which holds the plaintext. The property is unchanged and the mechanism is
  simpler, because the column a publication would name by default is now the safe one. Anything
  written against the old layout - "publish the sibling, exclude the parent" - is inverted and would
  publish the plaintext.
- **The relay performs the fan-out.** It resolves
  `(datastore_id, physical_schema) -> database_id -> active Grant -> app_id` and sends an app-keyed
  frame to each active grantee. The current worker-local namespace filter and decode loop are
  deleted; the broker's `(app_id, collection)` routing table remains app-keyed. Each Datastore stream
  still passes one `publication_names` entry, now the Datastore-keyed shared publication.

**The two costs, both named.**

1. **The owner loses plaintext reactivity on classified columns.** A subscription never carries the
   plaintext parent, for anybody, including the app that owns the database. Reads still do. This is
   the price of one shared decode stream, and it is the correct price: CDC is the one path where no
   executor-side check runs, so it must be fenced by what is *not sent* rather than by who is asking.
2. **Grant changes pay a relay-and-worker revision barrier.** The query path still fails at the next
   transaction. For subscriptions, the relay and frozen workers purge old-generation queues before
   control exposes a revoke or rebind. This replaces the refresh-lag conclusion; the availability
   cost is reconnecting healthy apps that shared a response with the changed app.

**The measurement remains, but its conclusion moved with relay ownership.** Verified on 18.4: both
four-argument `pg_logical_emit_message` overloads have `proacl` NULL, and the WAL `M` frame carries no
emitting role, so the default ACL makes a marker forgeable; absence of creator raw SQL is not a
boundary. The target revokes both exact overloads from `PUBLIC`, grants no worker, app or relay role,
and has the separate migration service emit `(database_id, database_epoch)` in the widen
transaction. The role name remains the worker's epoch fence; the epoch also enters WAL so the relay
learns publication shape and epoch from the same ordered stream.

---

## 6. What fences a stale binding

A binding can go stale two ways, and they are different questions with the same answer.

1. **The grant was revoked.** "Does this app still hold a live grant to this database" -
   authorization.
2. **The schema moved under it.** "Is the shape this isolate was built against still the shape the
   database has" - the schema epoch.

**Both are answered by whether `SET LOCAL ROLE "zs_bind_<gid>_e<E>"` succeeds**, and neither is
answered by anything the worker compares. Role membership answers the first; the `_e<E>` in the name
answers the second.

### 6.1 Revocation

**Nothing app-keyed, and no incarnation token.**

Measured on 18.4, on one held connection with the backend pid printed on both sides of the revoke:

| Step | Result |
| --- | --- |
| `pg_backend_pid()` | 188 |
| `BEGIN; SET LOCAL ROLE zs_db_t_rw; SELECT ...; COMMIT` | 1 row |
| second connection: `REVOKE` the database role from the intermediate | `REVOKE ROLE` |
| `pg_backend_pid()` | **188** - same backend, no reconnect |
| `BEGIN; SET LOCAL ROLE zs_db_t_rw` | `ERROR: 42501 permission denied to set role "zs_db_t_rw"` |

No cache, no isolate eviction, no version poll, no incarnation token. The grant is a database object;
a revoked grant stops working because PostgreSQL says so. Re-granting restores access on the same
connection, which a monotonic id with permanent tombstones cannot express - and revoke-then-regrant is
a legitimate state, while "same app id, different app" is not reachable at all, because typed ids are
UUIDv7 and never reused (`crates/zeroship-core/src/typed_id.rs`).

**The precise revocation bound, and it differs by configuration.** Measured on 18.4:

| Session runs as | REVOKE lands while a transaction is open | Bound |
| --- | --- | --- |
| a narrowed role that holds the schema privilege | the in-flight transaction **continues** - the assumed role holds it directly | one in-flight transaction |
| a role that only *inherits* the schema privilege | the very next statement fails `42501 permission denied for schema` | one statement |

This design assumes the grant role, which inherits the database role, so the bound is **one in-flight
transaction**, capped by the existing guards `DB_IDLE_IN_TX_TIMEOUT_MS = 15_000` and
`DB_STATEMENT_TIMEOUT_MS = 30_000` (`crates/zeroship-plugin-db/src/auth/bootstrap.rs:192`, `:195`).
Neither constant bounds total transaction duration on its own - a transaction issuing sub-30-second
statements with sub-15-second gaps runs indefinitely - and `transaction_timeout` is set nowhere in
the tree. Closing that is open question O2. The trade is deliberate: narrowing buys exact
per-statement confinement (2.2b) and costs one transaction of revocation lag instead of one
statement.

### 6.2 The schema epoch, enforced by PostgreSQL

**The role name carries the epoch, so the fence is a condition the worker FAILS rather than a
function it CALLS.** An apply that changes the schema advances the database's epoch from `E` to
`E+1`, mints `zs_bind_<gid>_e<E+1>` for every live grant on that database, and drops
`zs_bind_<gid>_e<E-1>`. An isolate built against `E-2` therefore fails at `SET LOCAL ROLE`, which is
the **first statement of the setup batch that already exists** (2.2b). No code path can skip,
forget, or be talked out of it: the batch is the only route to a usable connection.

**Steady-state cost: zero.** The epoch is a substring of a role name the batch already sends.

**The alternative was to append `SELECT epoch ...` to the setup batch and compare in Rust**, at one
index lookup inside an existing round trip. It was rejected. Two objections were raised against the
role-name form - both would have sunk it - and both were measured away:

- **Does `CREATE ROLE` inside the apply bracket serialize applies across other databases?** Roles are
  cluster-shared (5.1a), so this was the live worry: it would put a cluster-wide serialization point
  in every migration. Method: session 1 holds an uncommitted `CREATE ROLE` in one database
  (precondition proved by `pg_stat_activity` showing it `active`); session 2 issues `CREATE ROLE` in
  a different database of the same cluster with `lock_timeout = '3s'`, so blocking surfaces as an
  error rather than a hang; the control is the same statement with no holder. Control returned
  `CREATE ROLE`; the concurrent case returned `CREATE ROLE` in 108 ms. **No cross-database
  serialization.**
- **Is `SET ROLE` superlinear in `pg_auth_members`?** If it were, the role graph would tax every
  query on the platform. Method: grow the shared catalog, then time 2000 `SET ROLE` plus 2000
  `RESET ROLE` server-side in a plpgsql loop, so client round trips are excluded and the same N runs
  at every scale.

  | `pg_auth_members` rows | `pg_authid` rows | 2000 x (`SET ROLE` + `RESET ROLE`) |
  | --- | --- | --- |
  | 3 | 20 | 6 ms |
  | 103 | 120 | 6 ms |
  | 1103 | 1120 | 6 ms |
  | 6103 | 6120 | 6 ms |

  **Flat across a 2000x growth** - about 1.5 us per statement at both ends.

So the comparison form adds a statement to every setup batch forever to avoid a catalog cost that
measures at zero, and puts the fence somewhere a future refactor can remove. The name form cannot be
removed without removing the connection.

**Live epochs are capped at two, and the cap is fail-closed.** The reaper is part of the apply rather
than a background sweep: **an apply that cannot drop epoch `E-1`'s roles refuses to advance to
`E+1`.** That converts an unbounded leak into the cluster-shared catalog into a bounded one, and it
is what gives an isolate mid-flight across an apply one epoch of grace rather than none.

**What is unmeasured and must not be read as covered:** per-backend membership cache construction at
CONNECT time. The `SET ROLE` figures above are on an established backend; a new backend still pays to
build its membership set. Pooling amortizes that cost; it does not remove it. It is named as cost 7
in section 13 and must be measured before the role graph ships.

### 6.3 Error taxonomy

**The SQLSTATEs separate cleanly for revocation, and collide for the epoch.**
`is_missing_per_app_session_role` today matches SQLSTATE **22023 `invalid_parameter_value`** with the
exact message `role "<X>" does not exist`
(`crates/zeroship-plugin-db/src/error.rs:230-243`), and
`from_pg_per_app_session_setup` collapses it into `SCHEMA_NOT_PROVISIONED`
(`crates/zeroship-plugin-db/src/error.rs:251-268`, `:191`). A revoked grant is **42501** with
`permission denied to set role`, measured above. So:

- `42501` at the session-setup site -> `GRANT_REVOKED`. Terminal, 403-shaped, never retried, never
  falls back to the pool.
- `22023` + `role does not exist` -> today, `SCHEMA_NOT_PROVISIONED`. Never migrated. Retryable
  after a migrate.

**The second arm is the one the epoch breaks, and it must be split in the same change.** A reaped
epoch role is *dropped*, so a stale isolate gets `22023 role "zs_bind_<gid>_e<E>" does not exist` -
the same code and the same message shape as "this app was never migrated", which is a different
condition with a different remedy. Telling those apart needs no message sniffing, because the
classifier already composes the exact role name it expects and matches the server's message against
it (`crates/zeroship-plugin-db/src/error.rs:237-242`); it only needs to compose the epoch-bearing
name and to know, from the injected binding, whether the app holds a live grant at all. A missing
epoch role under a live grant is `SCHEMA_EPOCH_STALE` and is **retryable** - the same condition
`Verdict::ReResolve` already carries
(`crates/zeroship-plugin-db/src/transaction/reducer/identity.rs:253-257`). Collapsing it into
`SCHEMA_NOT_PROVISIONED` would tell a creator to run a migration that has already run.

### 6.4 The worker-side epoch input is the missing piece

The worker's stale-binding epoch consumer ships and is tested.
`crates/zeroship-plugin-db/src/transaction/reducer/identity.rs:97` defines `SchemaEpoch`;
`:313-315` compares the observed epoch against the expected one and returns `Verdict::ReResolve`,
which `:253-257` documents as the retryable verdict that rolls the attempt back and makes the caller
re-resolve rather than follow the new epoch in place.

What has no input is `crates/zeroship-plugin-db/src/transaction/driver.rs:106`, which mints
`SchemaEpoch::new(0)` for the expectation and echoes it into the observation at `:121`. Its own
comment says so at `:100-101`: "The wiring is real; the *input* is not yet", and at `:113-116`:
"the day a record exists, this is the one function that has to change."

For the worker stale-binding fence, building the epoch is **supplying one input to a classifier that
already ships**. It does not build the separate WAL marker, relay or epoch rendezvous; those remain
designed-not-built prerequisites in the CDC proposal.

### 6.5 The system schema, which does not exist and must be created

`__zeroship_admin` was deleted on 2026-08-27 - six tables and 32 definer-rights routines - under the
`AGENTS.md` invariant that a privileged call the worker can make is not a boundary.
`crates/zeroship-plugin-db/src/auth/bootstrap.rs:15-18` records that **nothing replaced it**, and
`db/migrations-ts/` provisions no such schema. One live statement still names it and therefore fails
on every database: the PITR placeholder at
`crates/zeroship-plugin-db/src/backend/postgres.rs:1286`, whose own comment at `:770-776` says the
schema "NO LONGER EXISTS" and "the INSERT below therefore fails on every database".

This design creates it, and the shape is the invariant's one permitted use - state a separate service
writes and the worker only reads:

- **An installer in `db/migrations-ts/`.** The deleted version was installed only by
  `ensure_admin_schema`, which was `#[cfg(any(test, feature = "test-helpers"))]`, which is why
  deleting it cost nothing and why creating it now is genuinely new work rather than a restoration.
- **Exactly one table**, holding the current schema epoch per database, written by the migration
  service inside the apply transaction that mints the new epoch's roles. Writing the row and rotating
  the roles in one transaction is what stops the recorded epoch and the catalog from disagreeing. Its
  grant posture is read-only to everyone but the migration service, and **the data plane still reads
  nothing** - the role name carries the epoch precisely so no query has to. The control plane reads
  it to compose the binding it injects (2.2a). A data-plane read here would reintroduce the live
  catalog dependency `crates/zeroship-plugin-db/src/descriptor.rs:1-31` exists to have removed.
- **Zero worker-callable functions.** No `SECURITY DEFINER`, no `EXECUTE ... TO PUBLIC`, and no
  `GRANT USAGE ON SCHEMA` to any app or grant role. That `USAGE` grant was the reachability
  precondition for every public `EXECUTE` in the deleted version, so an acceptance arm that audits
  the routine grants while leaving `USAGE` in place is checking the lock and not the door.
- **No `PUBLIC` write grant on anything.** The deleted schema had exactly one -
  `GRANT INSERT, UPDATE, SELECT ON ... pitr_targets TO PUBLIC` - and a provisioner written by porting
  the old installer's statements would port it.

---

## 7. Encryption

Today the key is `Hkdf::<Sha256>::new(Some(app_id.as_bytes()), root)`
(`crates/zeroship-plugin-db/src/encryption/keys.rs:373-374`, reached via `derive_key(&root, app_id)`
at `:297`), the root is process-wide and scoped only by `key_id`
(`lookup_root(&self, key_id)` at `:208`), and `canonical_aad(collection, column, row_pk)` binds the
wire version, collection, column and pk and **nothing namespacing**
(`crates/zeroship-plugin-db/src/encryption/aad.rs:75-98`). It fails in opposite directions on the two
new axes: co-grant-holders derive different keys and get an AEAD failure on data they are entitled to
read; one app across two databases derives one key with no database in the AAD, so a ciphertext for
`(users, ssn, usr_01)` lifted from one database verifies in the other - the exact relocation oracle
`row_pk` binding exists to stop, reopened one level up.

**Salt on the database, AAD binds the database, wire version `0x02`:**

```
derive_key(root, database_id)
canonical_aad(WIRE_VERSION_V2, database_id, collection, column, row_pk)
```

`crates/zeroship-plugin-db/src/encryption/aad.rs:87-93` already says the version MUST become a
parameter when `0x02` ships and that binding it
first makes a downgrade fail the tag. This is that change.

**Two consequences that must be stated together.**

**The inheritance fence is confirmed across three PostgreSQL majors.** Measured
directly, two arms differing only in the grant's inherit option:

| major | `server_version_num` | default `GRANT` | `GRANT ... WITH INHERIT FALSE` |
| --- | --- | --- | --- |
| 16.14 | `160014` | row returned | `permission denied for schema` |
| 17.11 | `170011` | row returned | `permission denied for schema` |
| 18.4 | `180004` | row returned | `permission denied for schema` |

`SET ROLE` continues to work in every arm, so narrowing is unaffected. The
option is therefore not a 16-only detail that a later major withdraws.

**The two role failures are distinguishable by SQLSTATE alone.** Measured on
16.14 from a real non-superuser session - the distinction is invisible to a
superuser, because `SET ROLE` permission is checked against `session_user`, so
a probe connected as `postgres` sees the second case succeed:

| condition | SQLSTATE |
| --- | --- |
| role does not exist | `22023` `invalid_parameter_value` |
| role exists, session is not a member | `42501` `insufficient_privilege` |

So `SCHEMA_NOT_PROVISIONED` and `GRANT_REVOKED` split on the code, with no
message sniffing. The existing classifier already matches on `SqlState`
(`crates/zeroship-plugin-db/src/error.rs:230-243` pins `22023` **plus** the
exact role name, because `22023` is the generic bad-GUC code shared with
`SET statement_timeout = 'yes'`); `42501` needs no such qualifier, being
specific to the membership check.

**The batch cannot reorder it, for a structural reason.** The setup really is a
multi-statement simple query - `tx.simple_query(&setup_sql)`
(`crates/zeroship-plugin-db/src/exec.rs:322`) over
`autocommit_local_session_setup_sql`
(`crates/zeroship-plugin-db/src/auth/bootstrap.rs:226-231`) - but its first
statement is `SET LOCAL ROLE`, followed by `statement_timeout` and
`lock_timeout`. PostgreSQL aborts a simple-query batch at the first failing
statement and emits exactly one `ErrorResponse`, so the role error is the only
error there is; nothing later runs to compete with it. Measured: that batch from
a non-member session returns `permission denied to set role` and nothing else.

The ordering is therefore load-bearing twice over. **If a statement is ever
placed before `SET LOCAL ROLE` in that batch, its failure masks the role failure
and the taxonomy silently collapses** - and because the epoch rides that same
role name (6.2), the same reordering would also make a rotated epoch surface as
whatever the earlier statement failed with.

**This closes section 15's item 2 only, and nothing else.** The other items
remain measured on 18.4 or 17.11 alone - the revocation bound, the column-list
grant, `BYPASSRLS` under `SET ROLE`, and the logical-decoding column filter have
NOT been re-run on 16. Do not read the table above as re-measuring the
probe set. Item 2 also remains open for any major below 16, where the
per-membership inherit option does not exist.

First, encryption stops fencing co-grant-holders. The `app_id` salt today means a co-tenant physically
cannot decrypt; that is a fail-closed accident of a mechanism built for cross-tenant *replay*, and it
fences the owner out of the co-tenant's rows symmetrically. Under a database salt, anyone holding a
grant on the database can decrypt. **If a column must be readable by the owner only, that is a
column-level GRANT** (2.4) - and, on the subscription path, a column the publication does not carry
(5.3). Encryption becomes purely at-rest, which is what it should have been. The remedy is stated in
both places precisely because the CDC path does not honour the first one.

Second, ordering. Changing the salt changes every derived key; changing the AAD changes every tag.
Neither is a rename; both are re-encrypt-everything. Pre-launch there is nothing to re-encrypt, so
**this lands in the same change that makes database ids exist**, not after.

---

## 8. Usage attribution for billing

**Op counts stay keyed on the app and are NOT re-keyed.** `db_reads` / `db_writes` /
`db_rows_written` are emitted against the server-injected app id at the op boundary
(`crates/zeroship-plugin-db/src/exec.rs:71-73`, `:84`), and the app that issued the op consumed the
compute. A mechanical `app_id -> database_id` sweep would break this and must exclude these by name.

**The billing principal for a database is the CREATOR, because no app owns one.** Storage and WAL
are properties of the database, and the database hangs off the workspace (section 4). Any metric
that measures the *resource* rather than the *op* therefore attributes to the creator's account, and
any metric that measures an op attributes to the app that issued it. Those are two different keys on
purpose; collapsing them is what makes one app pay for a co-tenant's bytes.

**The database is a dimension, not a key - and the dimension has no producer today.**
`UsageEvent.dims: BTreeMap<String, String>` exists on the wire type
(`crates/zeroship-core/src/usage_event.rs:37`) and is constructed empty at drain
(`crates/zeroship-metering/src/meter.rs:302`). It is empty because the counter carries nothing to put
in it: `Meter::increment(&self, app_id, metric, n)` keys `(app_id, metric)`
(`crates/zeroship-metering/src/meter.rs:144`, `:195`) and `MeterHandle::record(&self, metric, n)`
takes no third axis
(`crates/zeroship-metering/src/lib.rs:80`). Populating `dims` means re-keying `AppCounters` to carry a
database. That is metering-core work, not filling in an existing field, and it must be scheduled as
such. Folding the database into the metric *name* is closed: `zeroship.billing_metrics` is PK'd on
`metric` alone and metric names are cluster-global.

The aggregate PK stays `(app_id, period, metric)`, so the database dimension is observable and
auditable but does not reach the spend engine. That is deliberate: spend is an app-level control.

**A producer that measures the resource is required, and it needs a home.** Today `db_reads` counts
ops issued, and nothing anywhere reads `pg_stat_database`, `pg_stat_statements`, `pg_database_size` or
any relation-size function in production code. Op counts proxy cost only while each app's ops and each
app's database are the same object. Decouple them and two apps post identical `db_reads` while one
holds 400 GB and the other 40 MB. The worker cannot fix this - it only knows its own ops.

The design needs a per-database `db_bytes_stored` (summed `pg_total_relation_size`) and a
per-datastore `db_wal_retained_bytes`, attributed to the database's **creator**. In this tree the
process holding the privileged replication connection *is* the worker
(`crates/zeroship-plugin-db/src/change_stream_pg.rs:184` +
`crates/zeroship-worker/src/db_posture.rs:96-100`), and there is no CDC relay crate. So this producer
is a new privileged service, not a free rider on an existing one, and it must be costed as one. It is
open question O3 and it does not block the isolation work.

**Two suppression holes acquire a victim and must close.**

- *Failure is free.* Every emit sits after the `?`, and a test asserts it: "the FAILED query did NOT
  bill" (`crates/zeroship-plugin-db/src/exec.rs:1295`). With `DB_STATEMENT_TIMEOUT_MS = 30_000`, a
  statement PostgreSQL kills does thirty seconds of database work and bills zero. On a private
  database that is self-harm; on a shared datastore it is a co-tenant's latency. Emit
  `db_statement_us` in **both** arms, measured at the op boundary. This knowingly reverses a
  documented invariant, and the regression test at
  `crates/zeroship-plugin-db/src/exec.rs:1290-1296` changes with it.
- *Subscriptions are entirely unmetered today.* `openSubscription()` provisions a slot and consumer,
  and there is not one meter call in `subscription.rs`, `wal_consumer.rs` or `change_stream_pg.rs`.
  A logical slot retains WAL for the **whole cluster**, so the current abandoned-slot path pins WAL
  generated by every co-tenant on the datastore with nothing billing it. In the target the relay owns
  the shared slot; meter subscription time to the app and retained WAL to the creator.

**Spend enforcement stays app-keyed and request-shaped.** Throttling an app's requests does stop the
db work it issues, because every op rides a dispatch. What it cannot do is protect a shared datastore
from an app comfortably under its limit. Per-datastore admission control is a new policy surface, out
of scope here, and named as cost 11 in section 13.

---

## 9. The creator-facing `env.db` surface

**One `zeroship.jsonc` for the whole workspace** (operator decision, 2026-08-29). The databases are
declared once, at workspace level; the apps are declared beside them and bind to them.

```jsonc
// zeroship.jsonc -- ONE file for the workspace
"databases": {
  "main":      { "id": "dbs_...", "migrations": "./db/main",      "out": "./generated/zeroship/main" },
  "analytics": { "id": "dbs_...", "migrations": "./db/analytics", "out": "./generated/zeroship/analytics" }
},
"apps": {
  "storefront": { "app": "<uuid>", "database": "main"      },  // SINGULAR - one db per app
  "admin":      { "app": "<uuid>", "database": "main"      },  // two apps, one shared db
  "reporting":  { "app": "<uuid>", "database": "analytics" }
}
```

**Each database entry carries its `dbs_...` id, and that is what goes on the wire** (operator
decision 13, 2026-08-30). It is absent on a fresh project and appended by the first create, exactly
as `schema/project-v1.json` already specifies for `app`: "the deploy target's app id (uuid) or name.
Absent on a fresh project: the first `zeroship deploy` auto-creates the app and appends its id here."
One lifecycle, one pattern, one file.

**The map key is a LOCAL LABEL, not a resolvable name, and the difference is the whole of decision
13.** `"main"` exists so `"database": "main"` can refer to an entry a few lines above it, the same way
`"storefront"` labels an app whose real identity is the uuid beside it. Nothing sends `"main"` to a
server and nothing resolves it there. The withdrawn design had the control plane resolve
`(workspace, name) -> database_id` over the wire; this is a reference inside one file that the CLI
dereferences locally before it makes any request.

The workspace may declare several databases; **an app binds to exactly one**. That is the whole of
the "single schema" decision, and it is what keeps `env.db.users` alive.

Declaring the databases ONCE is the point: one migration source and one `out` directory per
database, shared by every app in the workspace, structurally rather than by convention. Nothing can
drift two apps onto different generated types from the same migrations, because there is only one
place the pair is written.

**This is a bigger change to the project file than "`migrations` becomes a map".** Today
`schema/project-v1.json:30-34` declares `app` as a **single string** - one deploy target per file -
so the workspace shape requires it to go plural.

**And it collides with a rule that exists for a good reason.** The environments block requires
`app` and `control` and makes them explicitly NON-inheritable, because "an environment that names a
control and inherits the root app is exactly the silent cross-targeting this rule exists to
prevent" (`schema/project-v1.json:66-67`). With N apps, an environment has to name a deploy target
**per app**, and the anti-cross-targeting property has to survive that. Getting this wrong points a
production environment at a staging app id, silently, which is precisely the failure the current
scalar shape was written to make impossible. It is the sharpest open detail in this section.

```ts
await env.db.users.find({ where: { active: true } });     // UNCHANGED
await env.db.orders.insert({ ... });                     // refused at type level when readonly

await env.db.transaction(async (tx) => {                 // UNCHANGED
  await tx.users.update(...);
  await tx.orders.insert(...);
});
```

**`env.db.users` survives, and one-database-per-app is what saves it.** `installSchema` plants
collections directly on the target with `Object.defineProperty`
(`sdks/bootstrap/src/install-schema.ts:952`), alongside `transaction` (`:960`) and `live`
(`:1522`), so with several bindings a binding named `analytics` and a collection named `analytics`
would be the same key. With one database there is no binding level and no collision. The creator
surface does not change at all: no call-site sweep, no regenerated types, no edits to
`docs/reference/db.md`, `examples/starter/` or `tests/golden_path.sh`.

`RESERVED_ENV_DB_NAMES` (`sdks/bootstrap/src/install-schema.ts:1137`) keeps its present meaning, and
`transaction` and
`openSubscription` stay where they are.

The plugin constraint that motivated the binding level is also moot: a second plugin claiming the
`db` namespace panics by design (`crates/zeroship-runtime/src/core/plugin.rs:191-193`), but one
database per app means one plugin, one binding, no contention.

`DbBinding` gains a third field, `database_id` (`crates/zeroship-plugin-db/src/binding.rs:14-18`). It
is already `Clone + Eq + Hash`, already minted once per isolate from live state, and already the key
for the descriptor store (`crates/zeroship-plugin-db/src/context.rs:616-623`), so threading it is
mechanical. Deciding what goes in the field is section 2; the type change is cheap.

Per-thread resources become maps keyed by `DbResourceKey`. Today `ThreadDbContext`
(`crates/zeroship-plugin-db/src/context.rs:176`) holds one `pool` (`:178`), one `db_url` (`:182`),
one `resource_key` (`:391`) and one `backend` (`:431`), and registering a second URL makes
`install_db_resources` (`:659`) report a change and the caller call `clear_pool` (`:578`) - so a
second datastore today tears down the first. That is the shape a worker serving several datastores
has to change; the target already exists one module over as
`OPERATOR_POOLS: HashMap<DbResourceKey, Rc<Pool>>`.

---

## 10. Transactions

**`env.db.transaction()` keeps its present shape and covers exactly one database**, because an app
sees exactly one. There is no second database for a callback to reach into, so the multi-database
transaction problem is closed by the entity model rather than by a runtime check.

**That is the reason to keep it closed, and it is worth stating because it is the strongest argument
against ever reopening one-app-many-databases.** `pending_emits`
(`crates/zeroship-plugin-db/src/context.rs:271`) exists so a subscriber cannot observe a row that is
not yet durable: mutations inside a transaction queue their `ChangeEvent` and the settle path fires
the queue on COMMIT or drops it on ROLLBACK, with `savepoint_emit_marks` (`:251`) extending it to
savepoint frames after a flat buffer once published an event for a row a `ROLLBACK TO SAVEPOINT` had
discarded. **That mechanism is defined relative to one commit point.** Independent per-database
transactions would give N commit points with no ordering: one commits and publishes, the other rolls
back, and a subscriber has observed a half-transaction the creator wrote as one. Real 2PC buys
atomicity and costs a prepared-transaction lifecycle, an orphan reaper and
`max_prepared_transactions` capacity planning on a path that deliberately bounds tenant
connection-hold *because* a parked transaction is an exhaustion vector. A prepared transaction is
that vector with the timeout removed.

Consequences, mechanical, and smaller than they would be under several databases per app: `tx_conns`
(`:188`), `tx_claims` (`:207`), `tx_waiters` (`:218`), `savepoint_depths` (`:236`),
`savepoint_emit_marks` (`:251`) and `pending_emits` (`:271`) stay keyed on `app_id`, because
`app_id` still determines the database. `TxRoute` (`crates/zeroship-plugin-db/src/tx_route.rs:73-78`)
is unchanged.

**The continuation slot stays keyed on the app id, and that is not an accident of the old shape.**
`TxRoute::capture` plants and compares a V8 continuation-preserved value
(`crates/zeroship-plugin-db/src/tx_route.rs:83-98`), and its own comment names why: "SEC-1 is
structural here rather than incidental: a co-resident app's callback plants ITS app_id in the
continuation slot, so the comparison below fails and this app routes to its own pool connection".
The planted key must never become a creator-facing name: two co-resident apps both calling their
database `main` must not compare equal. `TxRoute` has one production constructor taking
`&mut v8::PinScope`, so no dispatch site can be missed and the compiler enforces that.

---

## 11. Teardown

`DROP SCHEMA IF EXISTS "<app_id>" CASCADE` (`crates/zeroship-plugin-db/src/drop_namespace.rs:161`)
followed by `drop_per_app_role` (`:173`) becomes **two operations on two different subjects**, and
the existing ordering does not simply move over: it is app-keyed end to end and three of its five
steps change meaning.

**Delete an app** - no data is destroyed, and none of the five-step database ordering below runs:

1. `REVOKE` both edges of the app's grant role, then `DROP ROLE zs_bind_<gid>_e<E>` for every live
   epoch. That is the complete teardown of an app's access, and it is instant.
2. Nothing else. The app owns no database, so there is no database to cascade into and no 409 to
   raise. **A deleted app never destroys data another app can still read** - which under the old
   app-equals-schema shape was not a property, it was an identity.

**Delete a database** - only when its grant set is empty. The five steps of
`crates/zeroship-plugin-db/src/drop_namespace.rs:11-27` are the right *order*, and each one is
re-keyed:

| step today | under the decoupling |
| --- | --- |
| 1. subscription gate, keyed on the app | keyed on the database: refuse while any subscription on any grant-holder is observable |
| 2. drain broker via `subscription_app_dropped` | the broker's `(app_id, collection)` routing table is unchanged, so this fans out to every grant holder rather than to one app |
| 3. consumer cancel **plus slot teardown** | **No shared CDC object is dropped.** The relay-owned slot, stream and publication live for the Datastore; remove only this Database's publication members and fan-out routes (5.3) |
| 4. `DROP SCHEMA "<app_id>" CASCADE` | `DROP SCHEMA "db_<dbsid>" CASCADE` |
| 5. `DROP ROLE "app_<id>_role"` | `DROP ROLE zs_db_<dbsid>_{mig,rw,ro}`, still after the schema so no objects depend on them |

Step 3 is the one that fails silently if it is ported rather than re-keyed, which is why it is
tabulated rather than described.

The workflow journal schema `app_<uuid>`
(`crates/zeroship-migrate-server/src/provisioning.rs:216-217`, duplicated deliberately at
`crates/zeroship-plugin-workflow/src/store/pg.rs:112-114`) **stays app-keyed** - a workflow run is
app state, not database state - and with one database per app it has one home: the datastore holding
that app's database. Hoisting it to a platform datastore is cleaner but makes every workflow step a
cross-database write, and the two derivations that no compiler keeps in sync would both change
anyway.

---

## 12. SQLite dev tier

One file per database, `zs-db-<dbsid>.sqlite`, ATTACHed under alias `db_<dbsid>`. The existing
`attach_app_file` (`crates/zeroship-plugin-db/src/backend/sqlite/mod.rs:760`) already *is* a
per-database handle under a different name - `zs-<app_id>.sqlite` attached under the app id, with an
`app_id_cache` dedup set because SQLite errors on a duplicate alias (`:115-118`). The dedup key becomes
the database id.

Two fidelity gaps, both real and both written into `docs/reference/sqlite-divergences.md` rather than
discovered:

- **Grants are not enforceable.** SQLite has no roles and no column ACLs. The dev tier's grant fence
  is the Rust resolution layer only; PostgreSQL's is the catalog. This is the same posture masking
  already has on both tiers.
- **The schema epoch has no carrier.** It rides a role name on PostgreSQL (6.2), and SQLite has no
  roles, so the dev tier gets no epoch fence from the database. A dev-tier equivalent is owed and is
  not specified here; what must NOT happen is a Rust comparison added on the SQLite arm only, because
  a fence that exists on one tier and not the other is how a divergence becomes a surprise. Whatever
  it is, it belongs in `docs/reference/sqlite-divergences.md` beside the grants entry.
- **One writer per file.** Two apps sharing a database contend for a single writer lock that
  PostgreSQL would not impose, so a shared database behaves *worse* in dev than in production - the
  inverse of the usual direction, and the one that gets filed as a bug.

The cross-app FK validator in `crates/zeroship-plugin-db/src/cross_app_fk.rs` is dead code - its own
header says "THIS VALIDATOR HAS NO PRODUCTION CALL SITE" and its only caller is
`cfg(any(test, feature = "test-helpers"))` (`:20-30`) - and is **deleted, not updated**. The live rule
is `reject_cross_app_ref` in the engine plus schema-qualified REFERENCES rendering, restated as **"a
foreign key stays inside one database"**. That is a different predicate on a different input, and the
current one is wrong in both directions: it blanket-refuses cross-schema refs that a shared datastore
makes legal, and it permits same-app refs that cross a database boundary and cannot exist.

---

## 13. What this costs

1. **Revocation is immediate-at-next-transaction on the query path and revision-barriered on the
   subscription path.** The barrier can reconnect healthy apps that shared a relay response.
2. **Classified columns lose plaintext reactivity for everyone, including the owner.** One published
   column set per table per decode stream is a PostgreSQL constraint, not a choice.
3. **Blanket table grants and prospective default privileges are deleted.** Every apply regenerates
   explicit per-column grants inside the DDL transaction. A migration that fails to regenerate them
   leaves the database unreadable rather than over-readable, which is the right failure direction and
   is still a failure.
4. **A schema change means rebuilding the workspace.** Every app in the workspace builds against one
   migration source and one generated-types artifact, and the deploy gate refuses an app built
   against v1 from being served against v2. Apps deploy independently, so there is a window in which
   one is rebuilt and another is not. This is ordinary shared-dependency mechanics and the creator
   owns the risk; the platform's job is to make the mismatch loud, not to prevent it.
5. **Role count grows to `apps + 3 x databases + grants x live epochs`**, with live epochs capped at
   two by 6.2's reaper, against `apps x 1` today. Roles and memberships are cluster-shared
   (`relisshared = t`, 5.1a). `SET ROLE` itself measures flat across a 2000x growth in
   `pg_auth_members` (6.2), so the per-statement cost is answered.
6. **Every bind, unbind and epoch rotation is shared-catalog DDL** serialized through the control
   plane, which needs rate-limiting against grant-flapping. `CREATE ROLE` inside the apply bracket
   was measured NOT to serialize applies in other databases of the same cluster (6.2), so the cost is
   control-plane throughput, not cross-tenant blocking.
7. **Per-backend membership cache construction at CONNECT time is unmeasured.** It is a different
   cost from `SET ROLE` - paid once per new backend rather than per statement, amortized by pooling
   but not removed. Measure it before the role graph ships. Not estimated.
8. **The `SET LOCAL ROLE` change is invisible to every existing test.** Nothing today fails if the
   fence is refactored away, because nothing today can be revoked. Three mandatory regression tests:
   (a) grant, query succeeds; REVOKE from a separate connection; **the same warm isolate on the same
   pooled connection** fails with `GRANT_REVOKED`, with no eviction and no restart. (b) a statement
   issued without the session-setup batch fails with `permission denied`, proving the
   `WITH INHERIT FALSE` posture rather than the presence of a call. (c) rotate the epoch, reap `E-1`,
   and assert the stale isolate gets `SCHEMA_EPOCH_STALE` rather than `SCHEMA_NOT_PROVISIONED` (6.3).
9. **Worker boot becomes fatal on any bad datastore.** `db_posture` proves the worker is
   NOSUPERUSER/NOCREATEROLE and cannot write platform tables; it must run per datastore, plus the new
   `inherit_option` arm, and a partial pass would serve some apps and 500 others behind a security
   gate that half-ran. Refuse to boot. An availability regression traded for a boundary.
10. **Metering-core work that looks free and is not.** `dims` is a wire field with no counter behind
    it; adding the database dimension re-keys `AppCounters`.
11. **A per-datastore admission control does not exist.** Spend limits throttle an app's requests;
    they cannot protect a shared datastore from an app under its limit. New policy surface, out of
    scope, and a real gap the moment a datastore is shared.

**What sharing buys, stated honestly.** A shared datastore amortizes connections, CDC decode and
provisioning. A shared database buys cross-app joins, cross-app foreign keys and shared reads.
**And it does buy shared schema evolution, because the creator owns the schema and no app does.**
Three sibling apps that all want to add a column to a shared `users` table edit one migration source
in one workspace and rebuild; there is no owner app to arbitrate with and no fourth schema-owner app
to invent. What this design does NOT deliver is shared evolution across *creators*: two creators
jointly evolving one schema needs adjudicated multi-writer DDL, in which the migrator stops being
least-privilege-by-ownership and becomes a policy-adjudicated writer arbitrating per-table claims
between peer drafts, with no merge rule for two peers under escalation-reject. That puts
creator-influenced policy inside the one service trusted precisely because it does not execute
creator code. If the requirement is cross-creator shared evolution, this is the wrong design and
multi-writer DDL is the actual project.

---

## 14. Open questions, resolved prerequisites, and what each blocks

**O1. Where does the owner-side unmask policy live, and what reads it?**
The reading app authors the policy that gates unmasking of the owner's data
(`crates/zeroship-plugin-db/src/crud/mask_policy.rs:8-30`,
`crates/zeroship-plugin-db/src/crud/unmask.rs:318`, `:327`). Column grants fence the plaintext parent
but not the platform's own privileged unmask path. The catalog sentinels carry kind and
classification, not an actor-to-classification policy, so landing a catalog read does not close it.
*Blocks:* cross-creator co-grants, and nothing else. The same-creator restriction ships without it.

**O2. What bounds total transaction duration?**
Section 6's revocation bound is one in-flight transaction. `DB_STATEMENT_TIMEOUT_MS` bounds one
statement and `DB_IDLE_IN_TX_TIMEOUT_MS` bounds one idle gap; neither bounds the transaction, and
`transaction_timeout` is set nowhere in the tree. `env.db.transaction()` holds a dedicated connection
for the whole JS callback, which is the shape that defeats both. *Blocks:* publishing a numeric
revocation-lag guarantee. Does not block the mechanism.

**O3. Who runs the resource-measuring producer?**
Per-database `db_bytes_stored` and per-datastore `db_wal_retained_bytes` need a privileged connection
in a process that does not execute creator code. Today the process holding the replication connection
is the worker (`crates/zeroship-plugin-db/src/change_stream_pg.rs:184`), and there is no CDC relay
crate. *Blocks:* fair billing on
a shared datastore. Does not block isolation.

**O4. RESOLVED: how is `pg_logical_emit_message` made authoritative?**
`proacl` is NULL on 18.4 (both four-argument overloads, verified) so EXECUTE is public, and the WAL
`M` frame carries no emitting role. The PostgreSQL-18 target revokes both exact overloads from
`PUBLIC`; worker, app and relay roles get no grant, and the separate migration service emits the
widen transaction's epoch marker. There is no PostgreSQL-16 branch or compatibility alias. This
target is designed, not built; trusting the marker remains blocked until Datastore provisioning
applies and verifies both revokes.

**O5. What re-validates "same creator" after issuance?**
The predicate is "same owning user id", evaluated once at grant issuance.
`crates/zeroship-authz/src/resource.rs:14-16` has only `App` and `Any`, and the sole production write
to `app_members` is one INSERT at app creation
(`crates/zeroship-control/src/registry.rs:295`; every other write in the tree is a test). The first
app-transfer or team feature silently turns every existing co-grant into a cross-creator grant - the
exact configuration section 3 refuses. The grant must key on the principal and re-check on every
binding-table refresh, or a transfer must enumerate and refuse-or-revoke outstanding co-grants.
*Blocks:* any app-transfer or team-membership feature shipping after co-grants exist.

**O6. Does the CONFINED ceiling need a grant-authority key?**
Column grants are emitted by the migration service from the owner's IR. Nothing in the ceiling
vocabulary describes ACL authorship (`confined.policy.toml` grants `schema.create_table`,
`schema.rename`, `safety.destructive_ops` and nothing else), so a creator draft cannot widen an ACL
today by absence rather than by rule. *Blocks:* nothing now. It becomes load-bearing the moment any
`access.*` key is granted to a creator draft.

**O7. What identity scope does a shared database imply, and what derives `sector_identifier` from
it?**
Two apps sharing one database write DIFFERENT pairwise subjects for the SAME human, because the
sector is per-app: `zeroship.app_oauth_clients.sector_identifier` is `NOT NULL`
(`db/migrations-ts/20260702000200_control_tables.ts:56`) and there is one row per app
(`db/migrations-ts/20260812000100` upserts `ON CONFLICT (app_id)`). A shared `users` table therefore
gets two rows per person, and a foreign key written by app A does not join to a row app B created.
Nothing raises; the data is simply wrong.

This document did not previously mention it. "Identity" in the title and in section 1 means APP
identity versus DATABASE identity - system entities, not people - and none of the fifteen sections
covers end users. O5 is the nearest and is about the CREATOR holding the grant.

**The window is open now and closes at the first post-launch login.** The sector is immutable by a
PostgreSQL trigger (`db/migrations-ts/20260702000700_functions_triggers_comments.ts:7` and `:25`,
raw SQL because the DSL cannot express `UPDATE OF <column>`), and `:43`/`:45` record why:
`oauth_refresh_tokens.sub` stores a SNAPSHOT of the derived subject for refresh-family kill markers,
so changing the sector de-aligns stored markers from live access-token subjects and revocation
silently stops matching. That failure needs stored refresh tokens, and there are none - no tenants,
no end users, no rows. Today this is a derivation change plus a schema move with no data migration;
after launch it is a re-key of every refresh family whose failure mode is silent.

*Blocks:* the delivered row of the table at `:196` - N apps reaching the same tables under one
creator. That is the headline this design exists to deliver, so this is not a late-binding detail. It
is entangled with the workspace/team container decision, because "project", "workspace" and
"explicit grant" are three different answers to what the sector should key on.

---

## 15. What must be true before implementation starts

Unless an item says otherwise, it was measured on PostgreSQL **18.4** in a throwaway container
created and destroyed for the purpose - roles and memberships are cluster-shared objects (5.1a), so
role DDL must never be run against `:5455`, `:5440` or any shared instance.

**Established. Build on these.**

1. **`SET ROLE` resolves membership transitively, and that is a hazard as much as a mechanism.** A
   session reaches a database role's privileges through an intermediate without assuming the
   intermediate, and the assumed role cannot reach a sibling database
   (`permission denied for schema`). But transitivity resolves against the **login** role's closure,
   which under co-tenancy is the union of every grant the worker serves - which is why the
   intermediate must be per grant (section 2). And `SET ROLE` is a *lateral* move within that
   closure, never a narrowing: measured, a session already narrowed to one role can assume any other
   in the closure.
2. **`GRANT <role> TO <login> WITH INHERIT FALSE` makes a bare, unfenced SELECT fail while
   `SET LOCAL ROLE` still works.** `ALTER ROLE <login> NOINHERIT` does **not** do this: PG 16+
   records `inherit_option` per membership at grant time and the existing membership stays
   inheriting. Measured on 16.14, 17.11 and 18.4 - the best-evidenced item here.
3. **`WITH SET FALSE` on the intermediate-to-database edge blocks the shortcut.** The worker cannot
   assume the database role directly, so the per-grant edge cannot be bypassed. Without it the chain
   is decorative and revocation fails under co-tenancy (section 2).
4. **`REVOKE` on a second connection makes the next transaction on the same already-established
   backend fail at `SET LOCAL ROLE` with SQLSTATE 42501** `permission denied to set role`. Same pid,
   no reconnect. Re-granting restores it. An **in-flight** transaction that has already assumed the
   role continues to succeed, so the bound is one transaction rather than one statement (6.1).
5. **A table-level `GRANT SELECT` defeats a column list**; with the table-level grant revoked, a
   column-list grant denies the withheld column with 42501 and permits the granted ones. A column
   added after the grant is denied - fail-closed on schema evolution by a PostgreSQL property.
   **`RETURNING *` is incompatible with column grants**, which is why all twelve emitting sites now
   build an explicit projection (2.4). The four single-row verbs also narrow through the readable
   primary key rather than `ctid`, so the projection and target selection both respect the grant.
6. **`BYPASSRLS` does not follow through `SET ROLE`.** The same login sees 1 row as itself and 0 rows
   after narrowing to a `NOBYPASSRLS` role.
7. **Logical decoding consults no column ACL**: a role denied `SELECT ssn` receives the plaintext in
   the decoded stream. A publication **column list does** filter the decoded output, and PostgreSQL
   **refuses** conflicting column lists for one table across publications on one decode stream. It is
   also **incompatible with `REPLICA IDENTITY FULL`**, and neither DDL step refuses - the writes
   break at DML time (5.2).
8. **`pg_logical_emit_message` has `proacl = NULL`** on 18.4, with two four-argument overloads; the
   target revokes both from `PUBLIC` before trusting the migration service's WAL marker.
9. **Publications are datastore-scoped; slot namespace and budget are cluster-scoped; decode is
   datastore-scoped.** The relay owns one publication, slot and stream per Datastore. The stock slot
   ceiling is 10 with `SQLSTATE 53400` past it, so the cluster holds at most ten Datastores, fewer
   when anything else consumes a slot (5.1a).
10. **The identity and epoch types the design needs already ship.**
    `crates/zeroship-plugin-db/src/transaction/reducer/identity.rs:27-35` defines
    `AuthorityIdentity` carrying `incarnation: u64`, and `:97` defines `SchemaEpoch`; `:313-315`
    already returns `Verdict::ReResolve` on an epoch mismatch. Both are deliberately opaque -
    compared only for equality, with `for_app` (`:39`) named for today's axis - so re-keying onto a
    compared only for equality, with `for_app` (`:40`) named for today's axis - so re-keying onto a
    database or a grant is a change at construction sites, not a redesign. **But "nothing is at
    stake" would be wrong**: the comparison order and the terminal-denial semantics exist and are
    tested, and any change to the axis must keep the classifier's identity-before-lifecycle order
    intact.

**Must be settled before the first line is written.**

- **Re-run items 1-9 on the PostgreSQL major the platform actually deploys.** Except where noted,
  every measurement above is 18.4 or 17.11. Item 2 in particular depends on per-membership
  `inherit_option`, which is 16+; item 5's behaviour is stable but the SQLSTATE surface is worth
  re-confirming. A design whose fence is a version-dependent grant option must know its floor.
- **Measure per-backend membership cache construction at connect time** (cost 7). `SET ROLE` is
  measured flat; the cost a *new* backend pays to build its membership set is not, and the role graph
  multiplies exactly the catalog that cost reads.
- **`compio-postgres` must surface SQLSTATE 42501 distinguishably from the multi-statement
  session-setup batch.** The taxonomy in section 6 splits `GRANT_REVOKED` from
  `SCHEMA_NOT_PROVISIONED` on the code, and the existing classifier
  (`crates/zeroship-plugin-db/src/error.rs:230-243`) matches provenance plus an exact role name. If
  the driver collapses or reorders errors from a simple-query batch whose first statement fails, the
  split does not exist. Unverified.
- **The pooled-connection reset must be re-proved against a narrowed role.** The mechanism is already
  built for this: `crates/zeroship-plugin-db/src/exec.rs:301-311` runs the role and timeout guards via
  `SET LOCAL` inside an explicit transaction so they auto-revert at COMMIT and at the implicit
  ROLLBACK on drop, covering "a setup error, a query error, OR a cancellation between setup and the
  would-be reset". That reasoning does not change when the role names a database instead of an app,
  but the residue it prevents does: a leaked role today is one app's own schema, and under
  many-to-many it is a co-tenant's. Needs a test that checks out, narrows, cancels mid-flight, and
  asserts the next checkout cannot reach the first database. Unverified against a narrowed role.
- **The error taxonomy must split the epoch case from the never-migrated case** (6.3). Both surface
  as `22023 role "..." does not exist`, and collapsing them tells a creator to run a migration that
  has already run. Nothing in the tree distinguishes them today, because no role name carries an
  epoch today.
- **Section 3's refusal is a decision, not a finding.** Cross-creator table sharing is out of scope
  for this design. If the operator wants it, O1 must be answered first and the answer changes what
  the migration service owns.
- **Section 13's closing paragraph is the acceptance test for the whole design.** It delivers shared
  schema evolution *within a creator's workspace* and refuses it *across creators*. If the real
  requirement is that two creators jointly evolve one schema, this design does not deliver it and no
  amount of grant plumbing will. That must be confirmed before anything is built.
