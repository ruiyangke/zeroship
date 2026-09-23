# Retire the schema epoch fence

**Status.** BUILT on `main`, in `2bf158f04`. The platform stops promising that code built against
an older schema shape is refused before it runs. Creators own migration/deploy sequencing, by
expand-and-contract, the way every system with migrations does. The binding role and its two
membership edges stay exactly as they are: that is the tenant boundary and it is not what this
retires.

The three membership arms that distinguish this from weakening isolation pass by name -
`a_worker_login_reaches_a_shared_database_only_through_a_live_binding_role`,
`only_a_noninheriting_membership_keeps_a_binding_out_of_ambient_login_privileges` and
`set_role_separates_a_missing_role_from_a_role_the_session_may_not_assume`, in
`crates/zeroship-data-orm/tests/postgres_tenant_fence.rs`.

Removing the rotation also removed the trigger for re-resolving an app's binding SET, which was
never part of what this retires. That was restored separately, keyed on the live binding set
rather than on any schema shape, in `7064b2448`.

---

## What is being retired

The epoch is a version number for a database's schema SHAPE, carried as the last component of a
binding role name (`zs_bind_<binding>_e<E>`). An apply that commits a schema delta rotates it:
retire `E-1`, mint `E+1`, advance the head. An isolate holding a retired epoch fails
`SET LOCAL ROLE` and is refused before any statement runs.

That refusal is the promise. It is the promise being withdrawn.

## Why

**Old code meeting a new schema is universal, and the answer is the creator's.** Every system with
migrations has this. The industry answer is expand-and-contract: add the column, deploy code that
tolerates both shapes, backfill, then drop. It is a sequencing discipline, and sequencing a
creator's own migrations against their own deploys is theirs.

**The platform hands over the gun.** `DropTable` and `DropColumn` are first-class operations in
`crates/zeroship-migrate-ir/src/ir.rs`. The migration service executes them on request. So the
fence protects code from a destructive change THE PLATFORM ITSELF PERFORMED because the creator
asked for it. It is not guarding against an outside hazard.

**It is not a property of the product, only of one deployment mode.** The fence works because the
platform owns role creation on the cluster and rotates names on an apply. On a database the
platform did not provision, nothing rotates and there is no fence. A guarantee that evaporates when
the creator brings their own database was never a guarantee; it was an artifact of who happened to
hold `CREATEROLE`.

**It is half a guarantee where it does work.** It catches the schema moving forward under code that
is behind. It cannot catch code moving forward against a schema that is behind - the proposal that
introduced it says so plainly - and that direction surfaces as `42703 undefined_column` at query
time. So `42703` is the fallback either way; the fence only decides which half gets a tidier error.

**It amplifies across co-tenants.** `schema_name` is `db_<database_id>` and the epoch sits on the
database, not the app. Many bindings point at one database. So one creator's migration advances the
epoch for EVERY app bound to that database, and with the reload comparison in place every
co-tenant rebuilds its isolate because someone else changed a shape. The apps being reloaded did
nothing and, in the common case, are unaffected by the change.

## What survives, and why it is a different thing

The tenant boundary is the binding role's MEMBERSHIP, not the epoch in its name:

```
GRANT <capability> TO <binding> WITH SET FALSE      -- what the binding may reach
GRANT <binding> TO <worker>     WITH INHERIT FALSE  -- the worker cannot inherit it ambiently
```

(`crates/zeroship-migrate-server/src/datastore/cluster.rs`, `grant_binding_statements`.)

That is "app A cannot read app B's data". It is cross-tenant, the creator cannot enforce it for
themselves, and PostgreSQL enforces it regardless of what a creator does to their own tables.
Revocation stays a dropped membership and stays instant. None of this depends on the epoch.

A binding keeps exactly one role. The name needs to be unique per binding, and nothing needs to
decode it - which also settles Open 1 of `docs/proposals/2026-09-22-role-names-as-data.md` in the
"unique, not decodable" direction for the binding roles at least.

## What comes out

| what | where |
|---|---|
| the rotation itself | `crates/zeroship-migrate-server/src/rotation.rs`, whole module |
| T1/T4/E wiring in the apply | `crates/zeroship-migrate-server/src/apply.rs` |
| the cluster's epoch head and its table | `crates/zeroship-migrate-server/src/datastore/cluster.rs` |
| the epoch projection | `crates/zeroship-control/src/{databases,internal,registry}.rs` |
| `databases.schema_epoch` and its CHECK | `db/migrations-ts/20260919000200_database_entities.ts` |
| the epoch in the role name | `crates/zeroship-core/src/database_role.rs`, `binding_role_name` |
| `AppVersionInfo::binding_epochs` | `crates/zeroship-core/src/types.rs` |
| the reload comparison and re-supply | `crates/zeroship-worker/src/{sync,cache,handler}.rs` |
| `SCHEMA_EPOCH_STALE` and its classification | `crates/zeroship-data-orm/src/{error.rs,backend/postgres/pg_error.rs}` |
| the epoch arm of the role reaper | `crates/zeroship-migrate-server/src/datastore/cluster.rs`, `classify_role_name` |

Most of this landed today. That is not a reason to keep it.

## What creators get instead

Documentation, not machinery. `docs/reference/db.md` should say what every migration guide says:
a deploy and a migration are two events, a running build is not replaced atomically by either, and
the way to change a shape without breaking a live build is to expand, migrate, then contract. The
platform applies what it is given and does not adjudicate whether the sequence was safe.

`42703 undefined_column` remains the observable failure when a creator gets it wrong, and it names
the missing column, which is a better diagnostic than a role that does not exist.

## What this does NOT license

- **Do not weaken the membership edges.** `WITH SET FALSE` and `WITH INHERIT FALSE` are the tenant
  boundary. Nothing here touches them, and an argument that starts "since we dropped the epoch"
  and ends at either of those has changed subject.
- **Do not drop the binding role.** One role per binding stays. It is the object membership hangs
  off and the thing a session narrows to.
- **Do not read this as "the platform makes no promises about data".** Isolation between tenants is
  promised and enforced. What is withdrawn is a promise about a creator's own code meeting a
  creator's own schema change.

## What this moots rather than fixes

Two items recorded in `docs/proposals/2026-08-28-app-database-decoupling.md` stop being open. Both
stop for the same reason, and it is not that anyone repaired them. Saying which is which matters:
"we fixed it" and "we stopped promising it" leave very different traces for the next reader, and
only one of them is true here.

**The availability defect.** A second schema-changing apply retired the role a live binding named,
so that app's sessions were refused until the process dropped the binding it was holding. Nothing
fixes this. The rotation that produced it is gone, so the defect has no mechanism left - which is
also the cleanest statement of what the fence cost: it could take a serving app off the air for a
schema change the app did not make and, on a shared database, did not ask for.

**The workflow-replay exposure.** A pinned deployment replayed against a store that follows the
schema forward meant old code reaching a shape it was not built against, with no role to refuse it.
The remedy needed a fact nobody recorded - which schema shape a deployment was built against -
and it was blocked on that. With no epoch there is nothing to record and nothing to compare, but
the honest reading is not that the item dissolved. It is that its behaviour became the contract:
every build now meets the current schema, replay and live dispatch alike, and a mismatch surfaces
at query time as `42703 undefined_column` naming the column. That is the same answer this proposal
gives everywhere else, applied to a replay.

## Open

1. **Does the reaper still need the epoch arm?** `classify_role_name` attributes a stray role by
   parsing and re-composing its name. With no epoch in the name the binding arm simplifies rather
   than disappears, but it interacts with Open 1 of the role-names-as-data proposal and the two
   should be settled together.

2. **Does anything else read `schema_epoch` that is not the fence?** SETTLED: no. Both hits in the
   CDC crates are fixtures, not reads - `crates/zeroship-data-cdc-server/src/source.rs` declares
   the column inside a `#[cfg(test)]` helper that stands up control-table stand-ins, and
   `crates/zeroship-data-cdc-server/tests/relay.rs` grants `SELECT (id, status, schema_epoch)` in
   the same spirit. The relay resolves a subscriber's schema from the binding rows and never reads
   the value. The fixtures move with the column; nothing depends on it.

3. **Is the dev-tier divergence row now shorter?** SETTLED: the divergence is gone rather than
   owed, and it is the only item here that this change makes SIMPLER rather than smaller. The dev
   tier never carried an epoch - the only `epoch` in `crates/zeroship-data-orm/src/backend/sqlite/`
   is `UNIX_EPOCH` in `snapshot_fixture.rs`, which is a clock - and
   `docs/reference/sqlite-divergences.md` carries no row for an epoch, a role or a grant. Both tiers
   now agree by having the same nothing, so the register needs no row and the dev tier owes no
   equivalent. Control for the absence: `sqlite` matches throughout that same file, so the empty
   result is the register's content and not a mis-scoped search.

## Acceptance

(a) **A migration that changes a shape does not rotate anything.** Apply a schema delta, assert the
binding role that existed before still exists and no new one was minted. Control: the apply did
commit a delta, asserted from the engine journal, so the test is not passing over a no-op.

(b) **A co-tenant is not reloaded by a neighbour's migration.** Two apps on one shared database; A
applies; assert B's isolate is not replaced. This is the amplification going away and it should be
measured, not assumed.

(c) **Revocation still fences immediately.** Revoke a binding, assert the next session is refused
`42501` and the role still exists in `pg_roles`. This must pass unchanged - it is the boundary that
survives, and its passing is what distinguishes this change from weakening isolation.

(d) **Stale code meets `42703`, and the message names the column.** A build expecting a dropped
column gets `undefined_column` naming it, rather than a role-does-not-exist error. This is the
documented contract now, so it needs a test rather than a paragraph.
