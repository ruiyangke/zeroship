# Role names are data, not a convention

**Status.** PROPOSED, nothing built. The change is to stop deriving cluster role names from
entity ids at every reader, and to store each name on the row whose object it is. The payoff is
that a role the platform did not mint becomes expressible, which is what a bring-your-own-database
tier needs. The cost is that one parser stops being able to attribute a stray role by its shape.

---

## What is derived today, and where

`crates/zeroship-core/src/database_role.rs` is the single codec. Four shapes come out of it:

```
zs_db_<database_id>_mig      database_migrator_role_name
zs_db_<database_id>_rw       database_capability_role_name(ReadWrite)
zs_db_<database_id>_ro       database_capability_role_name(ReadOnly)
zs_bind_<binding_id>_e<E>    binding_role_name
```

`crates/zeroship-core/src/database_derivation.rs` wraps them in typed-id signatures, and the
production readers are few:

- `crates/zeroship-data-orm/src/binding.rs`, `DbBinding::to_database` composes the session role the
  setup batch sends.
- `crates/zeroship-migrate-server/src/apply.rs` composes the migrator role it narrows to and the
  two capability roles it grants from.
- `crates/zeroship-migrate-server/src/datastore/cluster.rs` composes all four when the reconciler
  mints them.

The codec exists to stop two spellings disagreeing. `DbBinding` derives rather than accepts
"so no call site can hand it a name that disagrees with the cluster", and `as_wire` on
`DatabaseCapability` says a second spelling "would compose a role nothing created".

## Why data is fewer copies than a codec

A derivation is not one copy of a fact. It is one RULE, recomputed at every reader, and it is
correct only while every reader agrees on the rule and on the inputs. The rule is what the readers
share; the NAME is what the cluster actually has.

Storing the name collapses that. The reconciler writes the name in the same transaction that
creates the role, so the row and the object are one statement. Every other reader reads it. There
is no second computation to disagree with, because there is no second computation.

This is the shape the rest of this design already prefers: `LIVE_BINDINGS_FROM_WHERE` spells the
liveness predicate once and every projection is generated from it; `internal.rs` serves
`capability.as_wire()` so the response carries one codec's spelling rather than whatever a column
holds. The role name is the remaining fact that is recomputed rather than carried.

## The change

`zeroship.database_bindings` gains `role_name`, written when the reconciler mints the role.
`zeroship.databases` gains `migrator_role`, `readwrite_role` and `readonly_role`, written when
`converge_database` mints them.

Control serves `role_name` on `GET /internal/apps/{app_id}/bindings` in place of the binding id
and epoch the worker composes from. `DbBinding::to_database` takes the name rather than deriving
it. `apply.rs` reads the three database roles from the row it already fetches.

The epoch stays a column. It is already one on `zeroship.databases`, and
`AppVersionInfo::binding_epochs` already carries it to the worker for the reload comparison, so
nothing needs the name to be parseable to learn it.

## Three things move rather than disappear

**Truncation moves from compose time to write time.** `refuse_truncation`
(`crates/zeroship-core/src/database_role.rs`) refuses a name over the identifier limit, and today
every reader can hit it. As data the check happens once, where the role is minted, and an operator
sees it at provisioning rather than a creator meeting it at a first query. The guard does not
weaken; its site improves.

**Rotation becomes an explicit write.** Advancing the epoch currently changes the name implicitly,
because the name contains the epoch. With the name stored, `rotation::rotate_if_owed`
(`crates/zeroship-migrate-server/src/rotation.rs`) updates `role_name` in the same transaction
that mints `E+1` and advances the head. That transaction already exists and is already atomic, so
this is a column in a statement that is already there.

**The reaper loses its shape test, and this is the real cost.** `classify_role_name`
(`crates/zeroship-migrate-server/src/datastore/cluster.rs`) attributes a role found on the cluster
by parsing its name and then RE-COMPOSING it to confirm the parse - a round trip that is exact, and
that works on a role whose row is already gone. Reading the table instead cannot attribute a role
the control plane has no row for, which is precisely the orphan the reaper exists to find.

The honest options are to keep `classify_role_name` as a fallback for orphans while the stored name
is authoritative for live rows, or to accept that an orphan is unattributable and reap by absence
from the table instead of by shape. The first keeps a convention nothing enforces; the second
changes what "unattributed" means. This is the open question below, and it should be settled before
the columns land rather than after.

## What this unblocks: a database the platform did not provision

The role apparatus exists to fence CO-TENANTS on a shared database. Every role in the list above
answers "how does app A not reach app B". A creator's own database has no app B.

Today the platform cannot use one anyway. `apply_bootstrap_corpus` and `converge_database` need
`CREATEROLE` on the target cluster, held continuously rather than once, because the reconciler
re-asserts on every pass. `require_no_direct_database_memberships` refuses to converge at all when
it finds a membership it did not create - which a creator's existing roles would trip on the first
pass. The blocker is not the number of roles. It is a standing claim on the cluster's whole
authorization surface, re-asserted on a timer.

With names as data, a creator-supplied role is a value in a column. The platform reads it and
narrows to it; it mints nothing and needs no `CREATEROLE`. That is the whole of the mechanism
change. What it does NOT do is carry the rest of the tier:

- **No epoch fence.** The fence works because the platform rotates role names on an apply. Nothing
  rotates a creator's role, so an isolate built against an older shape is not refused - it meets
  `42703 undefined_column` at query time, the direction the fence already cannot catch. On a
  single-tenant database this is the creator's own code against the creator's own schema.
- **Masking becomes advisory.** Column-level masking is enforced by capability-role grants. A
  creator-supplied role has whatever grants the creator gave it.
- **Revocation is credential rotation**, not a dropped membership, and so is not instant.

`docs/reference/sqlite-divergences.md` already carries this shape for the dev tier, which has no
Datastore entity and no control plane and enforces in process what production enforces in the
catalog. A second tier whose enforcement differs is not a new concept here; it is the second
instance, and it owes the same divergences row.

## What this does not change

The privilege invariant is untouched. The worker still narrows to a role it did not compose, the
migration service still holds the only `CREATEROLE` credential for platform-provisioned clusters,
and `SET LOCAL ROLE` is still the fence for those. Reading a name from a row the control plane
serves is not the worker choosing its own authority: the name arrives over the same authenticated
channel the binding id arrives on now, and PostgreSQL still decides what it opens.

## Open

1. **What attributes an orphan role after the shape test goes?** Stated above. Until it is settled
   the columns should not land, because the answer decides whether `classify_role_name` survives as
   a fallback or is retired.

2. **Is a BYOD database a creation-time choice?** Open 1 of
   `docs/proposals/2026-08-28-app-database-decoupling.md` rejected a dedicated-cluster tier for a
   reason that applies here unchanged: without relocation it is an irreversible choice presented to
   a creator who cannot yet evaluate it. A creator who starts on BYOD and wants managed, or the
   reverse, is stuck. Relocation looks like the prerequisite rather than a follow-up.

3. **Does the per-app role (`per_app_role_name`) move too?** It is derived from a physical schema
   name rather than an entity id, so it has no obvious row to sit on. Left out of this proposal
   deliberately; it should be named rather than assumed to follow.

## Acceptance

Each arm names what makes it fail, because an arm that cannot fail measures nothing.

(a) **A minted role's name is what the row says.** Converge a database and a binding, then read the
row and the cluster: the stored name is the role that exists. Control differing in one variable: a
second binding on the same database stores a different name. Fails if the writer and the minter
disagree.

(b) **A rotation moves the stored name and the role together.** Apply a schema delta, then assert
the row's `role_name` names a role that exists and the previous one does not. Fails if the column
update and the mint fall out of the same transaction.

(c) **A supplied name is narrowed to without minting.** Provision a database whose role the test
creates out of band, store its name, and assert a session narrows to it and reaches the schema -
with the control that the platform issued no `CREATE ROLE` for it. Fails if any path still derives
rather than reads.

(d) **An orphan is still attributable**, under whichever answer Open 1 takes. Fails if a role whose
row is gone becomes invisible to the reaper.

## Do not

- Do not keep the derivation "as a fallback" for live rows. Two sources for one name is the defect
  this removes; a fallback that is consulted when the column is empty makes the column optional and
  the convention load-bearing again.
- Do not let a BYOD tier ship without settling Open 2. A tier that is an irreversible creation-time
  choice is the artifact Open 1 of the decoupling proposal rejects, and shipping one here would
  reject it again by accident.
- Do not present the BYOD tier's in-process checks as the fence. On a platform-provisioned cluster
  PostgreSQL refuses; on a creator's cluster the creator's grants refuse. Anything the worker
  checks is ergonomics, because the worker runs creator code.
