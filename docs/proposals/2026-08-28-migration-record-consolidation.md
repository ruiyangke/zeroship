# Where migration records live

**Date:** 2026-08-28
**Status:** decided, not implemented. Blocked on the
`zeroship-migrated` -> `zeroship-migrate-server` rename landing, which touches
the same crates.

## The decision

**Creator migrations belong to the creator.** Their journal lives in their own
schema, under the platform prefix:

```
<app_id>.__zeroship_schema_migrations
<app_id>.__zeroship_schema_migrations_supersedes
<app_id>.__zeroship_schema_migrations_inflight
<app_id>.__zeroship_schema_pending_contracts
<app_id>.__zeroship_schema_deploy_recovery
<app_id>.__zeroship_schema_backfills
```

**Platform migrations keep the schema they already have:**

```
zeroship_migrations.<table>       (unchanged)
```

No new schema for creator apps, and no rename on the platform side. The creator
is responsible for their own migrations.

**The asymmetry is deliberate.** The platform keeps a separate meta schema
because it has no tenant: nothing owns `zeroship` the way a migrator role owns
an app schema, so the engine's default derivation (`<project_schema>` +
`_migrations`) costs nothing and stays correct. The creator case had to move
in-schema precisely because a tenant DOES own theirs - see "What this costs"
below.

## Why `__zeroship_`

It is already fenced. `validate_collection` refuses the `__zeroship` prefix for
table names (`zeroship-schema/src/query.rs`, beside `pg_`), so co-habitation is
protected with no new reservation. The prefix means one thing everywhere: **the
platform owns this name.**

**That protection is load-bearing, not decoration.** Without a fenced prefix,
co-habitation is a live collision:

- the engine creates its journal with `CREATE TABLE IF NOT EXISTS`;
- its table names are **literals** - only `{meta}` is interpolated
  (`zeroship-migrate-postgres/src/backend/journal_sql.rs:128`, `:178`, `:224`,
  `:327`, `:342`);
- `validate_collection` does **not** fence `schema_migrations`.

So a creator declaring a table named `schema_migrations` in their own schema
would have it silently **adopted as the journal**.

**The pattern is already in use.** `__zeroship_workflow_{runs,steps,signals,blobs,subscriptions}`
live in the `app_<uuid>` workflow schema (`plugin-workflow/src/store/pg.rs`).
That schema is a different service's journal and is **not** folded in here.

## What this costs, accepted deliberately

The migrator role **owns** the app's schema
(`migrate-server/src/provisioning.rs:140-160`: `ALTER SCHEMA {proj} OWNER TO
{role}`, `GRANT CREATE, USAGE`, `ALTER DEFAULT PRIVILEGES ... GRANT ALL ON
TABLES`). A schema owner can `DROP` and `TRUNCATE` anything in it, and owner
privileges are **implicit - they cannot be REVOKE'd away.**

So a creator can destroy their own journal. That is the point: it is their
database and their app, and corrupting it breaks only them.

**Two consequences that follow, and must not be forgotten:**

1. **The platform cannot treat the creator journal as a trust anchor.**
   `zeroship.migrated_migrations` remains the platform's answer to "did this
   app's migrations apply". The deploy precondition keeps reading the ledger.
2. **The rollback hole closes platform-side, not by reading the journal.**
   Record what the engine **reported as applied** (`outcome.applied`) on the
   ledger row beside the declared descriptor. A re-submitted old IR yields
   `applied: []`, so the row shows nothing advanced, while the per-request write
   that the engine-upgrade case needs is preserved. Detail:
   `2026-08-28-deploy-schema-precondition.md`.

## The work

1. **Engine:** prefix the six journal tables `__zeroship_`. Our copy is
   in-sourced at `crates/zeroship-migrate-*`, so this is an ordinary change.
2. **Config:** `meta_schema` becomes the app's own schema. `conn.rs` already
   exposes `meta_schema` and nothing else, so this is the one knob that exists.
3. **Platform: nothing.** `zeroship_migrations` stays as it is.
4. **Delete** `provisioning.rs` step 5's `REVOKE ALL ON ... SCHEMA {meta}`. With
   no separate meta schema it names nothing, and a revoke that names nothing
   reads as protection.
5. **A migration** moving existing objects, dated after the last applied file.

**No rename is needed on either side.** `__zeroship_migrations` - the runtime
DDL audit table that would have collided - disappears under operator decision
10 (no DDL in the data plane), because its only writers are the two lazy index
paths that decision removes. See the index, "Decision 10".

## The platform half: two shadows can go

`zeroship_migrations.schema_migrations` is append-only, enforced by a
`schema_migrations_immutable` trigger. Two other records duplicate slices of it
and are both already broken:

| record | state |
| --- | --- |
| `zeroship_migrations.platform_migration_files` | **no writer** since `platform.rs` was deleted (register L32) |
| `db/released_migrations.tsv` | all 34 checksums stale; its pre-flight check was demoted from `fail` to a printed report |

Both hold *filename + checksum*, which the journal already stores under an
immutability trigger. They exist only because the code that needed "what
actually ran" could not reach the journal: its public reader
`journal_sql::applied`
(`zeroship-migrate-postgres/src/backend/journal_sql.rs:506`) is called by
**nothing**.

**Fix:** point the freeze check at the platform journal and delete both shadows.
This is a platform schema with no tenant, so no grant question arises.

## The one thing a reviewer must check

If any future change grants access to a **creator** journal, check the
**grantee**, not just the privilege. The tenant owning its own journal is
accepted here; a *third party* gaining write access to someone else's is not.

## A constraint on any future consolidation

The engine's journal has **no tenant column** - `event_seq`, `event_kind`,
`version`, `name`, `checksum`, `at`, `by`, `exec_ms`, `down`, `phase`,
`outcome`, `kind`. Its per-app isolation is carried entirely by the schema name.
Any move to a single shared journal table must first add an owner column
upstream in the standalone engine, which changes a published contract.
