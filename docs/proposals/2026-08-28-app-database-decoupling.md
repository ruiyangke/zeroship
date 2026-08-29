# Decoupling app identity from database identity

An app id is a tenant. It is not a schema name, not a role name, not an encryption salt, and
not a publication key. Today it is all five, by string identity, and that is what makes
"one app, many databases" unrepresentable.

The shape:

- **`Datastore`** - one physical PostgreSQL database (or one SQLite file directory).
  Operator-owned. Creators never name one.
- **`Namespace`** - one schema inside a Datastore, physically named `ns_<nsid>`. This is the
  unit that is owned, migrated, granted, published and dropped. It replaces "the app's schema"
  everywhere.
- **`Grant`** - the many-to-many edge, `(app_id, namespace_id, binding_name, capability)`, with
  capability `owner` | `readwrite` | `readonly`.

Enforcement is PostgreSQL role membership, not an in-process check. The worker connects once as
`zeroship_worker`, holds no inherited privilege **over app data**, and narrows per transaction with a
single `SET LOCAL ROLE "zs_ns_<nsid>_<cap>"`.

**Two exceptions, both deliberate, both previously unstated here.** (1) The worker holds
`zeroship_workflow_owner` by a plain `GRANT` with no inherit option
(`db/migrations-ts/20260818000200_worker_database_authority.ts:43`), boot *requires* that membership
(`crates/zeroship-worker/src/db_posture.rs:103-105`) and the fence exempts it by name
(`AMBIENT_MEMBERSHIP_EXEMPTION`, `db_posture.rs:13`). That role **owns** every app's journal schema,
owner privilege cannot be revoked, and the workflow store never narrows - it opens with a bare
`batch_execute("BEGIN")` (`crates/zeroship-plugin-workflow/src/store/pg.rs:373`). Section 11 keeps
journals app-keyed across datastores, which replicates the exception into each one. (2) The
replication plane is not fenced at all - see 5.1, and the boot posture *requires* `REPLICATION` and
`BYPASSRLS` on that same login.

So the accurate claim is narrower than the one this paragraph used to make: **role membership fences
the SQL executor plane, and nothing else.** Anyone reading it as "the worker cannot reach another
tenant's bytes" is wrong on two paths. Revoking a grant is one `REVOKE`; the next transaction on
the same already-pooled connection fails with SQLSTATE 42501 and no eviction, restart or cache
flush. There is no incarnation token, no version counter, and no pre-query lookup that the
worker could be wrong about.

Two things go the other way and are stated as costs, not omissions: logical decoding consults no
ACL and no RLS, so CDC is fenced by publication column lists and by nothing else; and cross-creator
table sharing is refused, because the unmask authorization policy is authored by the *reading*
app and there is no artifact anywhere that the owner controls.

Everything below is measured. Section 14 lists what remains unmeasured and what each item blocks.

---

## 0. What the ask decomposes into

"Multiple apps share one database" is already the shipped topology and needs no work. The worker
takes one DSN for the whole process (`crates/zeroship-worker/src/config.rs:59-60`), and every app
lives in a schema inside it named literally after the app id
(`crates/zeroship-migrate-server/src/apply.rs:257`, `let schema = app_id.to_string();`).

| Ask | Status today | Cost |
| --- | --- | --- |
| N apps in one physical database, separate schemas | ships | zero |
| One app reaching N databases | unrepresentable | expensive, mechanical, bounded |
| N apps reaching the SAME tables, same creator | unrepresentable | buildable, priced below |
| N apps reaching the same tables, DIFFERENT creators | unrepresentable | refused, section 3 |

---

## 1. Entities and identity

```
ds_<base62 uuidv7>   Datastore  { engine, cluster_id, dsn_secret_ref, resource_key }
ns_<base62 uuidv7>   Namespace  { datastore_id, owner_app_id, physical schema = "ns_<nsid>" }
grant                PK (app_id, namespace_id), UNIQUE (app_id, binding_name)
                     { capability, granted_to_principal, state }
```

The physical schema name derives from the **namespace** id. That single change is what breaks
`apply.rs:257` and every `quote_ident(app_id)` site in `crates/zeroship-schema/src/query.rs`, and
breaking them is the point. `crates/zeroship-plugin-db/src/broker.rs:78-79` states the conflation
in its own words - "`schema` is conflated with `app_id` (every app has its own schema named after
`app_id`)"; this is what un-states it.

A DSN never leaves the control plane and the operator config. `Datastore.dsn_secret_ref` names a
platform secret. The worker is configured with a *set* of DSNs and indexes them by
`DbResourceKey`, which already exists, is already a SHA-256 digest chosen so a DSN password cannot
reach `Debug` or a log line, and is already documented as "the identity of one *database's*
resources" (`crates/zeroship-plugin-db/src/service.rs:46`, `:187`, `:193`, `:208`). It needs no
change. Only its cardinality is wrong, and that follows from `DbServiceConfig` holding one URL.

Creators create namespaces; the control plane places them on a datastore. Bring-your-own-datastore
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
membership in **every** per-app role (`crates/zeroship-migrate-server/src/apply.rs:1092`,
`GRANT {runtime_role_q} TO {worker_q}`), so `SET LOCAL ROLE` never fails for a wrong app id. What
the role fences is a *mismatched pair* - right role, wrong schema. A *consistently* wrong
resolution executes cleanly. That bug class does not exist today only because resolution is the
identity function.

Decoupling makes resolution `f(app_id, binding_name)`, and `binding_name` comes from creator code
(`env.db.analytics.users`). **That is the security delta, and it is not staleness.** It is bounded
by 2.2(a): the binding table is per-isolate and server-injected and contains only that app's own
grants, so a consistently-wrong resolution can only select another of the *same app's* namespaces.
Reaching a co-tenant requires the injected table itself to be wrong, which is a control-plane
defect, not a data-plane one.

### 2.2 The mechanism, in three parts, all required

**(a) Resolution is server-injected, exactly like `APP_ID`.** The control plane resolves the app's
grants at deploy time and injects a binding table into the isolate on the same path as `APP_ID`
(the worker-internal `env_vars` map):

```
ZEROSHIP_DB_BINDINGS = {
  "main":      { ns: "ns_01J...", schema: "ns_01J...", ds: <DbResourceKey>, cap: "owner"    },
  "analytics": { ns: "ns_01K...", schema: "ns_01K...", ds: <DbResourceKey>, cap: "readonly" }
}
```

The creator's string indexes this table and **never reaches SQL**. An unknown binding is a hard
refusal with the same shape as `collection_not_declared`
(`crates/zeroship-plugin-db/src/descriptor.rs:1-31`, whose comment on why there is deliberately no
`Option` applies verbatim one axis up). Creator `vars` shadowing does not apply: user vars override
`process.env` on collision, but this path reads the worker-internal map, which is the same reason
metering is unforgeable.

**(b) The grant is a PostgreSQL role membership, and the session narrows to exactly one namespace.**

```
zs_ns_<nsid>_mig   owns schema ns_<nsid>                     (replaces the per-app migrator)
zs_ns_<nsid>_rw    USAGE on ns_<nsid> + column-listed DML     (replaces app_<id>_role's grants)
zs_ns_<nsid>_ro    USAGE on ns_<nsid> + column-listed SELECT
zs_app_<appid>     NOLOGIN, no privileges of its own; a membership hub and nothing else
zeroship_worker    LOGIN, member of zs_app_<A> WITH INHERIT FALSE, for every A it serves
```

- Granting: `GRANT "zs_ns_<N>_rw" TO "zs_app_<A>"`. One statement.
- Revoking: `REVOKE "zs_ns_<N>_rw" FROM "zs_app_<A>"`. One statement.
- The data plane issues `SET LOCAL ROLE "zs_ns_<N>_<cap>"` - **not** the app principal - resolved
  from the binding table. It replaces `set_local_role_sql` / `tx_session_setup_sql`
  (`crates/zeroship-plugin-db/src/auth/bootstrap.rs:162`, `:205-210`, `:227-231`) unchanged in
  shape and cost: still one statement in the same simple-query batch as the DB-1 timeout guards.

Narrowing to a namespace role is free and exact. Measured on PostgreSQL 18.4, with
`w -> zs_app_t -> {zs_ns_t_rw, zs_ns_m_rw}`:

| Probe | Result |
| --- | --- |
| `BEGIN; SET LOCAL ROLE zs_ns_m_rw; SELECT FROM ns_m.secrets` | 1 row, `current_user = zs_ns_m_rw` |
| `BEGIN; SET LOCAL ROLE zs_ns_m_rw; SELECT FROM ns_t.secrets` | `ERROR: permission denied for schema ns_t` |

`SET ROLE` resolves membership transitively, so the app principal never has to be assumed. **The
app's privileges are therefore never unioned onto a live session**, and per-statement confinement
is exactly one namespace at all times. No PL/pgSQL assertion, no `pg_has_role` check, no extra
round trip.

**(c) The session inherits nothing. Fail-closed is a grant option, not a role attribute.**

`zeroship_worker` is `INHERIT` today
(`db/migrations-ts/20260818000200_worker_database_authority.ts:35`) and is a member of every
per-app role, so its session already holds the union of every tenant's privileges *before* any
`SET LOCAL ROLE` runs. Any code path that reaches SQL without the session-setup batch reads
everything. That is not hypothetical - see 2.3.

Measured on 18.4, three arms differing in one variable:

| Configuration | Bare `SELECT FROM ns_t.secrets` as the login role |
| --- | --- |
| `GRANT zs_app_t TO w` (default), `w` INHERIT | **1 row returned** |
| same grant, `ALTER ROLE w NOINHERIT` | **1 row returned** |
| `GRANT zs_app_t TO w WITH INHERIT FALSE`, `w` INHERIT | `ERROR: permission denied for schema ns_t` |

The role attribute does nothing here. PostgreSQL 16+ records `inherit_option` per membership at
grant time (`SELECT inherit_option FROM pg_auth_members` returned `f` only in the third arm), and
the pre-existing membership stays inheriting when the attribute is flipped. **The grant must carry
`WITH INHERIT FALSE`.** `SET LOCAL ROLE` still works under it - verified in the same cluster,
including to a transitively-reachable namespace role.

`crates/zeroship-worker/src/db_posture.rs:22-59` checks superuser, createrole, createdb,
replication, bypassrls and workflow-owner membership. It gains an arm asserting
`inherit_option = false` on every `zs_app_*` membership the worker holds, and boot refuses
otherwise. This is the arm that makes the fence fail-closed by construction rather than by every
call site remembering.

### 2.3 The unfenced execution sites, which must close in the same change

The role fence is applied by exactly two functions:
`crates/zeroship-plugin-db/src/exec.rs:293` (`query_postgres_pool_with_autocommit_role`) and
`crates/zeroship-plugin-db/src/transaction/mod.rs:252`. These do not:

- `crates/zeroship-plugin-db/src/crud/unmask.rs:804` takes `pg.pool_handle()` and issues
  `INSERT INTO "{app_id}"."__zeroship_audit_unmask"` at `:821` with no role and no wrapping
  transaction.
- `crates/zeroship-plugin-db/src/crud/mask_drift.rs:392`, `:713`, `:791`, `:912` do the same, one
  of them selecting the **plaintext parent column**.

Today the blast radius of an unfenced statement is one app's schema. Under many-to-many it is every
namespace in the datastore. Two changes, both mandatory:

1. `WITH INHERIT FALSE` (2.2c) makes an unfenced statement fail rather than succeed.
2. The pool handle stops being reachable from CRUD code. The role-applying wrapper becomes the only
   route to a connection, so a future unfenced site fails to compile rather than reading everything.

### 2.4 Column-level GRANT replaces the descriptor as the masking authority

`crates/zeroship-plugin-db/src/descriptor.rs:1-3` makes the creator-authored, unsigned runtime
descriptor "the data plane's SOLE schema authority", and `:10-31` records that the live-catalog
read was deleted because "the catalog could only ever agree with the descriptor or be stale". That
reasoning holds exactly while one app owns the namespace exclusively. It is also already the weak
link on a private namespace, because nothing below the descriptor objects.

The migration service emits column-level grants from the **owner's own IR**, withholding every
column whose classification is not `none` and granting the column that holds the mask instead.

*The transcripts in this section and in 5.2 use a probe table whose columns are literally
`id, name, ssn, ssn_masked, dob`. **`ssn_masked` is that probe's own name, not a platform
convention** - SC-6's storage flip (`3fd54f177`) deleted the `_masked` sibling entirely, and the
field's own column now holds the mask while `__zs_raw__ssn` holds the plaintext. The measurements are
about PostgreSQL grant and publication semantics, which do not depend on the names, so they stand as
recorded; only read them for the semantics, not for the naming.*

Measured on 18.4:

| Grant state on `ns_t.patients` | `SELECT ssn` as `zs_ns_t_rw` |
| --- | --- |
| table-level `GRANT SELECT` **plus** `GRANT SELECT (id, name, ssn_masked)` | **plaintext returned** |
| column list only, table-level revoked | `ERROR: 42501 permission denied for table patients` |
| column list only, `SELECT id, name, ssn_masked` | 1 row, masked value |
| column list only, column `dob` added by a later `ALTER TABLE` | `ERROR: permission denied` |

Three consequences, all load-bearing:

- **A table-level grant defeats a column list.** Column grants add, they never subtract. The
  blanket `GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA` at
  `crates/zeroship-migrate-server/src/apply.rs:1148` must be **deleted**, not supplemented.
- **`ALTER DEFAULT PRIVILEGES` has no column-list form**, so the prospective table-level rules at
  `apply.rs:1150-1153` must be deleted too. Every apply regenerates the explicit per-column grants
  inside the same transaction as the DDL.
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
| `zs_ns_t_rw` via `SET LOCAL ROLE` from that same login | 0 |

`BYPASSRLS` does not follow through `SET ROLE`; it is an attribute of the current role. So once the
session is narrowed, RLS is enforceable even though the worker login must hold `BYPASSRLS`
(`crates/zeroship-worker/src/db_posture.rs:34-37`). RLS is not required by this design and is not
proposed here, but it becomes usable, which it is not today.

It is void on the CDC path, which is section 5.

---

## 3. Which sharing is permitted, and which is refused

**Permitted: a namespace has exactly one owner app; other apps hold DML grants on it.**

- `owner` - migrates it, sole author of its schema.
- `readwrite` - DML on granted columns. No DDL.
- `readonly` - SELECT on granted columns.

**`readwrite` and `readonly` grants are restricted to apps under the same creator.** Not because
roles are insufficient, but because one authority is unreachable by any server-side mechanism:

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
   only them" (`provisioning.rs:164-171`). Both halves are false under cross-creator sharing, and
   there is no second owner slot to allocate.
2. **The apply lock is on the wrong axis and the journal has no tenant column.** The only apply-time
   lock that runs is `pg_advisory_xact_lock(hashtextextended($1, 0))` over the publication name
   (`crates/zeroship-migrate-server/src/publication.rs:86`), and the publication name is a hash of
   the app id. Two apps in one namespace take *different* keys and their DDL interleaves with no
   mutual exclusion.

Within one creator, a co-grant-holder mis-declaring a mask policy is not a boundary crossing. Across
creators it is, and no role fixes it.

---

## 4. Ownership and migration of a shared namespace

- **One owner app per namespace.** `Namespace.owner_app_id`, not null.
- **New route** `POST /v1/namespaces/{namespace_id}/migrations/apply`.
  `crates/zeroship-authz/src/resource.rs:14-16` gains `Resource::Namespace { id }` - it is `App { id }`
  and `Any` today - with the policy "principal may migrate N iff principal is an owner of
  `N.owner_app_id`". The control-plane forwarding hop re-verifies the caller's own bearer and adds no
  authority, which is already the right posture; only the resource type is new.
- **The apply lock moves to the namespace**: `pg_advisory_xact_lock(hashtextextended('ns:' || <nsid>, 0))`,
  taken on the datastore connection before any DDL.
- **Migrator role is `zs_ns_<nsid>_mig`**, named by no app. One migrator forever, so the ownership
  ping-pong and the silently-orphaned `ALTER DEFAULT PRIVILEGES` rules cannot occur.
- **`ExecutorConfig::new(project_id, project_schema, policy)` stops taking the app id three times**
  (`crates/zeroship-migrate-server/src/apply.rs:280-298`). All three become `ns_<nsid>`. The engine
  already contemplates several apps under one project schema; the host stops collapsing the axis.
  `SchemaScope::Allowlist(Vec<String>)` already exists with a working case-insensitive `permits`
  (`crates/zeroship-migrate-ir/src/policy.rs:36`, `:64`), so multi-schema confinement is representable
  today and only the binder is scalar.
- **Deploy gate becomes a conjunction.** `crates/zeroship-control/src/registry.rs:460-486` predicates
  the deploy UPDATE on one `descriptor_sha256` matching the newest `applied` row. It becomes: for
  **every** binding in the manifest, the newest applied row for `(app_id, namespace_id)` matches that
  binding's descriptor hash. `Manifest.runtime_descriptor`
  (`crates/zeroship-bundle/src/manifest.rs:172`) becomes a map keyed by binding name;
  `zeroship.app_schema_applies` gains `namespace_id` and re-keys `(app_id, namespace_id, migration_id)`.
  The *list* of namespaces comes from the control plane's grant table, never from the bundle - the
  bundle asserts hashes, not preconditions. **A partial apply blocks the deploy**, which is stricter
  than today and correct: the masking story rests on this ordering.
- **A non-owner's deploy gate checks the owner's applied hash.** A reader builds against the owner's
  published descriptor. This is a real coupling with a real cost - section 12.

---

## 5. CDC, and the fence that actually binds it

### 5.1 Logical decoding consults no ACL and no RLS

Measured on 18.4. The same role that gets `ERROR: permission denied for table patients` on
`SELECT ssn` receives the plaintext `555-44-3333` in the decoded stream when the publication has no
column list. The decode path in this tree runs on the worker's own login connection
(`crates/zeroship-plugin-db/src/change_stream_pg.rs:184` passes `self.backend.url()`), and that role
is required to hold `REPLICATION` and `BYPASSRLS`
(`crates/zeroship-worker/src/db_posture.rs:34-37`;
`db/migrations-ts/20260818000200_worker_database_authority.ts:35`). Column grants, RLS and
`SET LOCAL ROLE` are all executor-side. Decoding does not go through the executor.

The tuple reaches subscribers verbatim: `ChangeEvent.new_tuple` is documented as "Text-encoded
column values for the affected row", "Populated by the WAL consumer from pgoutput Insert/Update/Delete
frames" (`crates/zeroship-plugin-db/src/broker.rs:97-112`).

### 5.2 The publication column list is the only server-side fence, and there is exactly one per table

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
paragraph. Before any shared-datastore CDC is built, measure the column list against `UPDATE` and
`DELETE`, on the deployed major, under both replica identities - and if they are incompatible, say
which of the two features is given up.

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

- **Slot per (datastore, worker), not per namespace.** Slots replicate decode work, they do not
  partition it: five slots decoding the same 40,002 changes cost 1,335 ms against 309 ms for one
  (`docs/proposals/2026-08-26-runtime-db-binding-00-index.md:291-301`, with the PostgreSQL sources
  checked in REL_16 and REL_18 to confirm no output-plugin filter runs before decode). One slot,
  fanned out in-process.
- **Publication per namespace**, membership = non-`__zeroship_` tables in `ns_<N>`.
  `crates/zeroship-migrate-server/src/publication.rs:19` already filters `WHERE n.nspname = $1`; only
  the key changes. This fixes the defect where two apps in one database each
  `ALTER PUBLICATION ... SET TABLE` (`publication.rs:46`) the other's members away and the removed
  tenant's live queries silently stop updating.
- **The published column set per table is the INTERSECTION over every grant on the namespace, and
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
- **The filter becomes a fan-out.** `if rel.namespace != self.app_id { return; }`
  (`crates/zeroship-plugin-db/src/wal_consumer.rs:605`) becomes a lookup of `rel.namespace` in the
  worker's namespace-to-grant-holders map, delivering to each app holding a read grant. The broker's
  `(app_id, collection)` routing table is unchanged in shape.
  `crates/zeroship-plugin-db/src/wal_consumer.rs:383-384` already passes a single-element
  `publication_names`, so the wire shape does not change.

**The two costs, both named.**

1. **The owner loses plaintext reactivity on classified columns.** A subscription never carries the
   plaintext parent, for anybody, including the app that owns the namespace. Reads still do. This is
   the price of one shared decode stream, and it is the correct price: CDC is the one path where no
   executor-side check runs, so it must be fenced by what is *not sent* rather than by who is asking.
2. **Revocation lags on the subscription path.** Delivery is fenced by an in-process map, so a revoked
   reader keeps receiving events for up to one binding-refresh interval, where revocation on the query
   path is immediate at the next transaction. Making it stronger costs one slot per namespace and the
   measured 4.32x. The lag is taken.

Separately, the epoch marker `pg_logical_emit_message` is forgeable. Verified on 18.4: two
four-argument overloads, `proacl` NULL on both, and the WAL `M` frame carries no emitting role. The
only thing holding it up is that creator code has no raw-SQL surface. Under sharing that is a
cross-tenant trust edge rather than a single-tenant footnote (open question O4).

---

## 6. What fences a stale binding

**Nothing app-keyed, and no incarnation token.** A handle points at a namespace, so the question
before a query is "does this app still hold a live grant to this namespace" - authorization, not
identity. Role membership answers it, in the database, where the worker cannot forge it.

Measured on 18.4, on one held connection with the backend pid printed on both sides of the revoke:

| Step | Result |
| --- | --- |
| `pg_backend_pid()` | 188 |
| `BEGIN; SET LOCAL ROLE zs_ns_t_rw; SELECT ...; COMMIT` | 1 row |
| second connection: `REVOKE zs_ns_t_rw FROM zs_app_t` | `REVOKE ROLE` |
| `pg_backend_pid()` | **188** - same backend, no reconnect |
| `BEGIN; SET LOCAL ROLE zs_ns_t_rw` | `ERROR: 42501 permission denied to set role "zs_ns_t_rw"` |

No cache, no isolate eviction, no version poll, no incarnation token. The grant is a database object;
a revoked grant stops working because PostgreSQL says so. Re-granting restores access on the same
connection, which a monotonic id with permanent tombstones cannot express - and revoke-then-regrant is
a legitimate state, while "same app id, different app" is not reachable at all, because typed ids are
UUIDv7 and never reused (`crates/zeroship-core/src/typed_id.rs`).

**The precise revocation bound, and it differs by configuration.** Measured on 18.4:

| Session runs as | REVOKE lands while a transaction is open | Bound |
| --- | --- | --- |
| the narrowed namespace role (`SET LOCAL ROLE zs_ns_t_rw`) | the in-flight transaction **continues** - the assumed role holds the schema privilege directly | one in-flight transaction |
| the app principal, inheriting (`SET LOCAL ROLE zs_app_t`) | the very next statement fails `42501 permission denied for schema ns_t` | one statement |

This design runs as the narrowed role, so the bound is **one in-flight transaction**, capped by the
existing guards `DB_IDLE_IN_TX_TIMEOUT_MS = 15_000` and `DB_STATEMENT_TIMEOUT_MS = 30_000`
(`crates/zeroship-plugin-db/src/auth/bootstrap.rs:192`, `:195`). Neither constant bounds total
transaction duration on its own - a transaction issuing sub-30-second statements with sub-15-second
gaps runs indefinitely - and `transaction_timeout` is set nowhere in the tree. Closing that is open
question O2. The trade is deliberate: narrowing buys exact per-statement confinement (2.2b) and costs
one transaction of revocation lag instead of one statement.

**Error taxonomy gains an arm, and the SQLSTATEs separate cleanly.**
`is_missing_per_app_session_role` today matches SQLSTATE **22023 `invalid_parameter_value`** with the
exact message `role "<X>" does not exist`
(`crates/zeroship-plugin-db/src/error.rs:230-243`), and
`from_pg_per_app_session_setup` collapses it into `SCHEMA_NOT_PROVISIONED` (`:251-268`, `:191`). A
revoked grant is **42501** with `permission denied to set role`, measured above. So:

- `22023` + `role does not exist` -> `SCHEMA_NOT_PROVISIONED`. Never migrated. Retryable after a migrate.
- `42501` at the session-setup site -> `GRANT_REVOKED`. Terminal, 403-shaped, never retried, never
  falls back to the pool.

The discriminator stays provenance-first and exact-name-matched, which is why it does not need message
sniffing.

---

## 7. Encryption

Today the key is `Hkdf::<Sha256>::new(Some(app_id.as_bytes()), root)`
(`crates/zeroship-plugin-db/src/encryption/keys.rs:373-374`, reached via `derive_key(&root, app_id)`
at `:297`), the root is process-wide and scoped only by `key_id`
(`lookup_root(&self, key_id)` at `:208`), and `canonical_aad(collection, column, row_pk)` binds the
wire version, collection, column and pk and **nothing namespacing**
(`crates/zeroship-plugin-db/src/encryption/aad.rs:75-98`). It fails in opposite directions on the two
new axes: co-grant-holders derive different keys and get an AEAD failure on data they are entitled to
read; one app across two namespaces derives one key with no namespace in the AAD, so a ciphertext for
`(users, ssn, usr_01)` lifted from one namespace verifies in the other - the exact relocation oracle
`row_pk` binding exists to stop, reopened one level up.

**Salt on the namespace, AAD binds the namespace, wire version `0x02`:**

```
derive_key(root, namespace_id)
canonical_aad(WIRE_VERSION_V2, namespace_id, collection, column, row_pk)
```

`aad.rs:87-93` already says the version MUST become a parameter when `0x02` ships and that binding it
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

The ordering is therefore load-bearing. **If a statement is ever placed before
`SET LOCAL ROLE` in that batch, its failure masks the role failure and the
taxonomy silently collapses.**

**This closes section 15's item 2 only, and nothing else.** Items 1 and 3
through 8 remain measured on 18.4 alone - the revocation bound, the column-list
grant, `BYPASSRLS` under `SET ROLE`, and the logical-decoding column filter have
NOT been re-run on 16 or 17. Do not read the table above as re-measuring the
probe set. Item 2 also remains open for any major below 16, where the
per-membership inherit option does not exist.

First, encryption stops fencing co-grant-holders. The `app_id` salt today means a co-tenant physically
cannot decrypt; that is a fail-closed accident of a mechanism built for cross-tenant *replay*, and it
fences the owner out of the co-tenant's rows symmetrically. Under a namespace salt, anyone holding a
grant on the namespace can decrypt. **If a column must be readable by the owner only, that is a
column-level GRANT** (2.4) - and, on the subscription path, a column the publication does not carry
(5.3). Encryption becomes purely at-rest, which is what it should have been. The remedy is stated in
both places precisely because the CDC path does not honour the first one.

Second, ordering. Changing the salt changes every derived key; changing the AAD changes every tag.
Neither is a rename; both are re-encrypt-everything. Pre-launch there is nothing to re-encrypt, so
**this lands in the same change that makes namespace ids exist**, not after.

---

## 8. Usage attribution for billing

**Op counts stay keyed on the app and are NOT re-keyed.** `db_reads` / `db_writes` /
`db_rows_written` are emitted against the server-injected app id at the op boundary
(`crates/zeroship-plugin-db/src/exec.rs:71-73`, `:84`), and the app that issued the op consumed the
compute regardless of which namespace it landed in. A mechanical `app_id -> namespace_id` sweep would
break this and must exclude these by name.

**The namespace is a dimension, not a key - and the dimension has no producer today.**
`UsageEvent.dims: BTreeMap<String, String>` exists on the wire type
(`crates/zeroship-core/src/usage_event.rs:37`) and is constructed empty at drain
(`crates/zeroship-metering/src/meter.rs:302`). It is empty because the counter carries nothing to put
in it: `Meter::increment(&self, app_id, metric, n)` keys `(app_id, metric)`
(`meter.rs:144`, `:195`) and `MeterHandle::record(&self, metric, n)` takes no third axis
(`crates/zeroship-metering/src/lib.rs:80`). Populating `dims` means re-keying `AppCounters` to carry a
namespace. That is metering-core work, not filling in an existing field, and it must be scheduled as
such. Folding the namespace into the metric *name* is closed: `zeroship.billing_metrics` is PK'd on
`metric` alone and metric names are cluster-global.

The aggregate PK stays `(app_id, period, metric)`, so the namespace dimension is observable and
auditable but does not reach the spend engine. That is deliberate: spend is an app-level control.

**A producer that measures the resource is required, and it needs a home.** Today `db_reads` counts
ops issued, and nothing anywhere reads `pg_stat_database`, `pg_stat_statements`, `pg_database_size` or
any relation-size function in production code. Op counts proxy cost only while each app's ops and each
app's database are the same object. Decouple them and two apps post identical `db_reads` while one
holds 400 GB and the other 40 MB. The worker cannot fix this - it only knows its own ops.

The design needs a per-namespace `db_bytes_stored` (summed `pg_total_relation_size`) and a
per-datastore `db_wal_retained_bytes`, attributed to the namespace's **owner** app. In this tree the
process holding the privileged replication connection *is* the worker
(`change_stream_pg.rs:184` + `db_posture.rs:34-37`), and there is no CDC relay crate. So this producer
is a new privileged service, not a free rider on an existing one, and it must be costed as one. It is
open question O3 and it does not block the isolation work.

**Two suppression holes acquire a victim and must close.**

- *Failure is free.* Every emit sits after the `?`, and a test asserts it: "the FAILED query did NOT
  bill" (`crates/zeroship-plugin-db/src/exec.rs:1295`). With `DB_STATEMENT_TIMEOUT_MS = 30_000`, a
  statement PostgreSQL kills does thirty seconds of database work and bills zero. On a private
  database that is self-harm; on a shared datastore it is a co-tenant's latency. Emit
  `db_statement_us` in **both** arms, measured at the op boundary. This knowingly reverses a
  documented invariant, and the regression test at `exec.rs:1290-1296` changes with it.
- *Subscriptions are entirely unmetered.* `openSubscription()` provisions a slot and a consumer, and
  there is not one meter call in `subscription.rs`, `wal_consumer.rs` or `change_stream_pg.rs`. A
  logical slot retains WAL for the **whole database**, so an abandoned subscription pins WAL generated
  by every co-tenant with nothing billing it. Meter `db_subscription_seconds` per open subscription,
  and bill retained WAL to the namespace owner.

**Spend enforcement stays app-keyed and request-shaped.** Throttling an app's requests does stop the
db work it issues, because every op rides a dispatch. What it cannot do is protect a shared datastore
from an app comfortably under its limit. Per-datastore admission control is a new policy surface, out
of scope here, and named in section 12.

---

## 9. The creator-facing `env.db` surface

```jsonc
// zeroship.jsonc  -- schema/project-v1.json:112-127, `migrations` becomes a map
"databases": {
  "main":      { "migrations": "./db/main",      "out": "./generated/zeroship/main" },
  "analytics": { "migrations": "./db/analytics", "out": "./generated/zeroship/analytics",
                 "grant": "readonly" }
}
```

```ts
await env.db.main.users.find({ where: { active: true } });
await env.db.analytics.events.insert({ ... });      // refused at type level when grant is readonly

await env.db.main.transaction(async (tx) => {
  await tx.users.update(...);
  await tx.orders.insert(...);
});
```

**There is no `env.db.users` alias, and adding one would be a bug.** `installSchema` plants
collections directly on the target with `Object.defineProperty`
(`sdks/bootstrap/src/install-schema.ts:1508`), alongside `transaction` (`:1515`) and `live` (`:1521`),
so a binding named `analytics` and a collection named `analytics` are the same key. The only
non-colliding shape is `env.db.<binding>.<collection>` with no single-database shortcut. Pre-launch,
so every call site and every doc example changes in one patch. `RESERVED_ENV_DB_NAMES`
(`install-schema.ts:1136`) becomes the reserved **binding** name set; `transaction` and
`openSubscription` move down a level onto each binding object.

Three databases cannot be three plugins: a second plugin claiming the `db` namespace panics by design
(`crates/zeroship-runtime/src/core/plugin.rs:191-193`). One plugin, N bindings.

`DbBinding` gains a third field, `namespace_id` (`crates/zeroship-plugin-db/src/binding.rs:14-18`). It
is already `Clone + Eq + Hash`, already minted once per isolate from live state, and already the key
for the descriptor store (`crates/zeroship-plugin-db/src/context.rs:616-623`), so threading it is
mechanical. Deciding what goes in the field is section 2; the type change is cheap.

Per-thread resources become maps keyed by `DbResourceKey`. Today `ThreadDbContext` holds one `pool`
(`context.rs:143`), one `db_url` (`:147`), one `resource_key` (`:304`) and one `backend` (`:344`), and
registering a second URL makes `install_db_resources` (`:568`) report a change and the caller call
`clear_pool` (`:487`) - so a second binding today tears down the first. The target shape already
exists one module over as `OPERATOR_POOLS: HashMap<DbResourceKey, Rc<Pool>>`.

---

## 10. Transactions

**`env.db.<binding>.transaction()` covers exactly one binding. A callback that touches a second
binding throws**, and the `tx` handle exposes only that binding's collections, so it is a type error
rather than a runtime surprise.

This is a commitment, not a hedge, for one reason no re-keying fixes. `pending_emits`
(`crates/zeroship-plugin-db/src/context.rs:271`) exists so a subscriber cannot observe a row that is
not yet durable: mutations inside a transaction queue their `ChangeEvent` and the settle path fires
the queue on COMMIT or drops it on ROLLBACK, with `savepoint_emit_marks` (`:251`) extending it to
savepoint frames after a flat buffer once published an event for a row a `ROLLBACK TO SAVEPOINT` had
discarded. **That mechanism is defined relative to one commit point.** Independent per-binding
transactions give N commit points with no ordering: one commits and publishes, the other rolls back,
and a subscriber has observed a half-transaction the creator wrote as one. Real 2PC buys atomicity and
costs a prepared-transaction lifecycle, an orphan reaper and `max_prepared_transactions` capacity
planning on a path that deliberately bounds tenant connection-hold *because* a parked transaction is
an exhaustion vector. A prepared transaction is that vector with the timeout removed.

Consequences, mechanical: `tx_conns` (`:188`), `tx_claims` (`:207`), `tx_waiters` (`:218`),
`savepoint_depths` (`:236`), `savepoint_emit_marks` (`:251`) and `pending_emits` (`:271`) re-key from
`app_id` to `(app_id, namespace_id)`. `TxRoute` carries `namespace_id` and `in_tx` stays a bool
because it is now binding-scoped (`crates/zeroship-plugin-db/src/tx_route.rs:73-78`).

**The continuation slot carries the composite key, and the composite key includes the app id.**
`TxRoute::capture` plants and compares a V8 continuation-preserved value
(`tx_route.rs:83-98`), and its own comment names why: "SEC-1 is structural here rather than
incidental: a co-resident app's callback plants ITS app_id in the continuation slot, so the comparison
below fails and this app routes to its own pool connection". Binding names are unique per app, not
globally - two co-resident apps both using `main` must not compare equal. The planted key is
`(app_id, namespace_id)` and never the binding name. `TxRoute` has one production constructor taking
`&mut v8::PinScope`, so no dispatch site can be missed and the compiler enforces the fix.

---

## 11. Teardown

`DROP SCHEMA IF EXISTS "<app_id>" CASCADE` followed by `drop_per_app_role`
(`crates/zeroship-plugin-db/src/drop_namespace.rs:161`, `:169-174`) becomes two operations.

**Delete an app:**
1. `REVOKE "zs_ns_<N>_*" FROM "zs_app_<A>"` for every grant A holds. For a non-owner this is the
   complete teardown - no data destroyed, instant.
2. `DROP ROLE "zs_app_<A>"`.
3. For each namespace A **owns**: refuse the delete with a 409 naming the holders if any other app
   holds a live grant. **No cascade. No silent destruction of a co-tenant's data.**

**Delete a namespace** (only when its grant set is empty): the existing five-step ordering survives
verbatim per namespace (`drop_namespace.rs:25-26` and the module doc) - subscription gate, broker
drain, consumer cancel, slot teardown, then `DROP SCHEMA ns_<N> CASCADE`, then
`DROP ROLE zs_ns_<N>_{mig,rw,ro}`.

The workflow journal schema `app_<uuid>`
(`crates/zeroship-migrate-server/src/provisioning.rs:216-217`, duplicated deliberately at
`crates/zeroship-plugin-workflow/src/store/pg.rs:112-114`) **stays app-keyed** - a workflow run is app
state, not namespace state. But it lives in a physical database, and under multi-namespace that stops
being a single answer. **The journal lives in the datastore of the app's binding named `main`**, which
is required to exist. Hoisting it to a platform datastore is cleaner but makes every workflow step a
cross-database write, and the two derivations that no compiler keeps in sync would both change anyway.

---

## 12. SQLite dev tier

One file per namespace, `zs-ns-<nsid>.sqlite`, ATTACHed under alias `ns_<nsid>`. The existing
`attach_app_file` (`crates/zeroship-plugin-db/src/backend/sqlite/mod.rs:751`) already *is* a
per-database handle under a different name - `zs-<app_id>.sqlite` attached under the app id, with an
`app_id_cache` dedup set because SQLite errors on a duplicate alias (`:115-117`). The dedup key becomes
the namespace id.

Two fidelity gaps, both real and both written into `docs/reference/sqlite-divergences.md` rather than
discovered:

- **Grants are not enforceable.** SQLite has no roles and no column ACLs. The dev tier's grant fence
  is the Rust resolution layer only; PostgreSQL's is the catalog. This is the same posture masking
  already has on both tiers.
- **One writer per file.** Two apps sharing a namespace contend for a single writer lock that
  PostgreSQL would not impose, so a shared namespace behaves *worse* in dev than in production - the
  inverse of the usual direction, and the one that gets filed as a bug.

The cross-app FK validator in `crates/zeroship-plugin-db/src/cross_app_fk.rs` is dead code - its own
header says "THIS VALIDATOR HAS NO PRODUCTION CALL SITE" and its only caller is
`cfg(any(test, feature = "test-helpers"))` (`:20-30`) - and is **deleted, not updated**. The live rule
is `reject_cross_app_ref` in the engine plus schema-qualified REFERENCES rendering, restated as **"a
foreign key stays inside one namespace"**. That is a different predicate on a different input, and the
current one is wrong in both directions: it blanket-refuses cross-schema refs that a shared datastore
makes legal, and it permits same-app refs that cross a namespace boundary and cannot exist.

---

## 13. What this costs

1. **Revocation is immediate-at-next-transaction on the query path and lagged by one refresh interval
   on the subscription path.** Bought with the 4.32x decode measurement.
2. **Classified columns lose plaintext reactivity for everyone, including the owner.** One published
   column set per table per decode stream is a PostgreSQL constraint, not a choice.
3. **A namespace owner's migration can break a co-tenant's deploy.** The reader's deploy gate checks
   the owner's applied descriptor hash. The owner ships a migration; every reader's next deploy fails
   until it rebuilds. There is no way around it that does not weaken the ordering guarantee the
   masking story rests on.
4. **`env.db.users` dies.** Every creator call site, every generated type, `docs/reference/db.md`,
   `examples/starter/` and `tests/golden_path.sh` change together.
5. **Blanket table grants and prospective default privileges are deleted.** Every apply regenerates
   explicit per-column grants inside the DDL transaction. A migration that fails to regenerate them
   leaves the namespace unreadable rather than over-readable, which is the right failure direction and
   is still a failure.
6. **One app with three namespaces means three applies, three journals, three publications, three
   provisioning runs.** A migration request that is one unit today becomes N, and the deploy blocks on
   all N. Partial success has no representation in `schema_apply_store`'s two terminal states and needs
   a third.
7. **Role count grows roughly 4x** on a shared cluster (apps x 1 + namespaces x 3, versus apps x 1
   today). Roles are cluster-global and per-backend membership caching scales with `pg_auth_members`.
   Unmeasured; not estimated.
8. **The `SET LOCAL ROLE` change is invisible to every existing test.** Nothing today fails if the
   fence is refactored away, because nothing today can be revoked. Two mandatory regression tests:
   (a) grant, query succeeds; REVOKE from a separate connection; **the same warm isolate on the same
   pooled connection** fails with `GRANT_REVOKED`, with no eviction and no restart. (b) a statement
   issued without the session-setup batch fails with `permission denied`, proving the
   `WITH INHERIT FALSE` posture rather than the presence of a call.
9. **Worker boot becomes fatal on any bad datastore.** `db_posture` proves the worker is
   NOSUPERUSER/NOCREATEROLE and cannot write platform tables; it must run per datastore, plus the new
   `inherit_option` arm, and a partial pass would serve some apps and 500 others behind a security
   gate that half-ran. Refuse to boot. An availability regression traded for a boundary.
10. **Metering-core work that looks free and is not.** `dims` is a wire field with no counter behind
    it; adding the namespace dimension re-keys `AppCounters`.
11. **A per-datastore admission control does not exist.** Spend limits throttle an app's requests;
    they cannot protect a shared datastore from an app under its limit. New policy surface, out of
    scope, and a real gap the moment a datastore is shared.

**What sharing buys, stated honestly.** A shared datastore amortizes connections, CDC decode and
provisioning. A shared namespace buys cross-app joins, cross-app foreign keys and shared reads.
**Neither buys shared schema evolution.** One migrating owner per namespace means three sibling apps
that all want to add a column to a shared `users` table have one permanent schema authority and two
permanently downstream readers; the natural workaround (a fourth schema-owner app that ships only
migrations) is a fiction the creator maintains, and it puts three apps' deploy gate behind a fourth
app's release cadence. Lifting that needs adjudicated multi-writer DDL: the migrator would stop being
least-privilege-by-ownership and become a policy-adjudicated writer arbitrating per-table claims
between peer drafts, and escalation-reject has no merge rule for two peers. That puts
creator-influenced policy inside the one service trusted precisely because it does not execute creator
code. If the requirement is shared *evolution*, this is the wrong design and multi-writer DDL is the
actual project.

---

## 14. Open questions, and what each blocks

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
Per-namespace `db_bytes_stored` and per-datastore `db_wal_retained_bytes` need a privileged connection
in a process that does not execute creator code. Today the process holding the replication connection
is the worker (`change_stream_pg.rs:184`), and there is no CDC relay crate. *Blocks:* fair billing on
a shared datastore. Does not block isolation.

**O4. Does `pg_logical_emit_message` need a REVOKE, and what is the signature across versions?**
`proacl` is NULL on 18.4 (both four-argument overloads, verified) so EXECUTE is public, and the WAL
`M` frame carries no emitting role. Under sharing this is a cross-tenant forgery edge. PG 16 has one
three-argument overload; 18.4 has two four-argument ones, so a REVOKE must be written per major.
*Blocks:* trusting any in-WAL epoch marker across tenants.

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

---

## 15. What must be true before implementation starts

Everything below was measured on PostgreSQL **18.4** in a throwaway container created and destroyed
for the purpose - roles are cluster-global objects, so this must never be run against `:5455`,
`:5440` or any shared instance.

**Established. Build on these.**

1. `SET ROLE` resolves membership transitively, so a session narrows directly to
   `zs_ns_<N>_<cap>` without assuming the app principal, and the narrowed role cannot reach a sibling
   namespace (`permission denied for schema`). Per-statement confinement costs one statement.
2. `GRANT <role> TO <login> WITH INHERIT FALSE` makes a bare, unfenced SELECT fail while
   `SET LOCAL ROLE` still works. `ALTER ROLE <login> NOINHERIT` does **not** do this: PG 16+ records
   `inherit_option` per membership at grant time and the existing membership stays inheriting.
3. `REVOKE <ns_role> FROM <app_principal>` on a second connection makes the **next** transaction on
   the same already-established backend fail at `SET LOCAL ROLE` with SQLSTATE **42501**
   `permission denied to set role`. Same pid, no reconnect. Re-granting restores it.
4. An **in-flight** transaction that has already assumed the narrowed role continues to succeed after
   the revoke. When the session instead runs as the inheriting app principal, the very next statement
   fails `42501 permission denied for schema`. The two configurations have different bounds.
5. A table-level `GRANT SELECT` defeats a column list; with the table-level grant revoked, a
   column-list grant denies the withheld column with 42501 and permits the granted ones. A column
   added after the grant is denied - fail-closed on schema evolution by a PostgreSQL property.
6. `BYPASSRLS` does not follow through `SET ROLE`. The same login sees 1 row as itself and 0 rows
   after narrowing to a `NOBYPASSRLS` role.
7. Logical decoding consults no column ACL: a role denied `SELECT ssn` receives the plaintext in the
   decoded stream. A publication **column list does** filter the decoded output, and PostgreSQL
   **refuses** conflicting column lists for one table across publications on one decode stream
   (`cannot use different column lists for table ... in different publications`).
8. `pg_logical_emit_message` has `proacl = NULL` on 18.4, with two four-argument overloads.
9. `AppIncarnationId` has zero occurrences in code. Nothing is built and no code is at stake.

**Must be settled before the first line is written.**

- **Re-run items 1-8 on the PostgreSQL major the platform actually deploys.** Every measurement above
  is 18.4. Item 2 in particular depends on per-membership `inherit_option`, which is 16+; item 5's
  behaviour is stable but the SQLSTATE surface is worth re-confirming. A design whose fence is a
  version-dependent grant option must know its floor.
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
  would-be reset". That reasoning does not change when the role names a namespace instead of an app,
  but the residue it prevents does: a leaked role today is one app's own schema, and under
  many-to-many it is a co-tenant's. Needs a test that checks out, narrows, cancels mid-flight, and
  asserts the next checkout cannot reach the first namespace. Unverified against a narrowed role.
- **Section 3's refusal is a decision, not a finding.** Cross-creator table sharing is out of scope
  for this design. If the operator wants it, O1 must be answered first and the answer changes what
  the migration service owns.
- **Section 13's closing paragraph is the acceptance test for the whole design.** If the real
  requirement is that several sibling apps jointly evolve one schema, this design does not deliver it
  and no amount of grant plumbing will. That must be confirmed before anything is built.
