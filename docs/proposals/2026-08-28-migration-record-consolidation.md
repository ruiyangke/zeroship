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

## DECIDED 2026-08-28: the whole `zeroship.migrated_*` family goes

Five records removed, and with `migrated_app_policies` included the **entire
`migrated_*` prefix disappears from the platform schema.** That also dissolves
the naming fossil the crate rename created - `zeroship-migrate-server` writing
tables called `migrated_*` - by removing the tables rather than renaming frozen
objects.

**What survives as the record of migrations: the engine journal, and nothing
else.** Append-only, checksummed, immutability-triggered on the platform side,
tenant-owned in the creator's own schema.



**Operator: remove `zeroship.migrated_migrations`,
`zeroship_migrations.platform_migration_files`, and
`db/released_migrations.tsv`.**

That leaves **one** record of what ran, per schema: the engine journal. It is
append-only, checksummed, and enforced by an immutability trigger on the
platform side.

### Why each one goes

| record | why |
| --- | --- |
| `platform_migration_files` | **no writer** since `platform.rs` was deleted today. Holds filename + checksum, which `schema_migrations` already stores under a trigger |
| `db/released_migrations.tsv` | a snapshot of the above. All 34 checksums stale; its pre-flight check was already demoted from `fail` to a printed report |
| `zeroship.migrated_migrations` | see below - its two halves die for different reasons |

### `migrated_migrations` has two halves and both are dispensable

**The approval workflow is unreachable in production.** `apply.rs:491` computes
`requires_approval = !gated_versions.is_empty()`, driven by
`migration_requires_approval` and the `safety.require_approval` obligation.
`ApprovalLevel::Never` is the **default** (`migrate-ir/src/policy_approval.rs:32-38`),
and measured 2026-08-28: **no policy artifact anywhere in the tree sets
`require_approval`** - not in `policies/`, not in any `.toml`, `.json` or `.ts`
outside tests. So `gated_versions` is always empty, `insert_pending` is never
called, and the whole `pending_approval -> approved -> applied` state machine,
with `approved_by`, `approved_at`, `approved_checksum` and `ceiling_version`, is
configured-but-unused.

**This is a capability removal and should be explicit rather than incidental:**
operator approval of destructive creator migrations goes away. Under "the
creator is responsible for the migration" that is the right answer - a creator
approves their own destructive ops - but it is a decision, not a cleanup.

**The other half - `descriptor_sha256` - moves to the journal.** The deploy
precondition (landed today, `5d27e71c5`) reads it to refuse a deploy whose
migrations have not applied. Re-pointing that read at the creator's own journal
**closes the rollback hole for free**: the hole exists because the ledger
records *requests* and can be moved backwards by re-submitting an old IR, while
the journal records *events* and cannot. Re-submitting an old IR journals
nothing.

**And the tenant-ownership objection does not apply here.** The creator's
journal is tenant-owned and destructible - but the precondition exists to
protect the creator from their own ordering mistake. A creator who forges their
journal to bypass it harms only themselves, which is the same reasoning that
made tenant-owned journals acceptable in the first place.

### What this deletes

- the three named tables, **plus `zeroship.migrated_migration_audit`** -
  measured 2026-08-28: **one writer (`migration_store.rs:302`), zero readers**
  anywhere in the tree. It is append-only by trigger
  (`reject_migrated_migration_audit_mutation`) and its own comment scopes it to
  *"submit, approval, pending rejection, and apply outcomes"* - three of those
  four actions belong to the approval flow that is unreachable, and the fourth
  duplicates the journal;
- **`zeroship.migrated_app_policies`, and `policy_store.rs` with it.** Operator,
  2026-08-28: *"we should not persist policy in the database ... even so, we
  should not make it editable in the runtime, everything should be in the
  crafting scope."*

  **This is decision 3's principle applied to migration policy.** Decision 3
  made the mask policy code-managed, folded at build time, delivered in the
  artifact and fixed for the isolate's life. The same rule now holds for what a
  creator's migrations may do: **no runtime-mutable policy anywhere.**

  **The delivery path already exists and is the right one.**
  `ApplyMigrationsRequest.policy: Option<PolicyDraftDocument>` carries the draft
  **with the request** - i.e. from the creator's repository, at apply time.
  `resolve_apply_policy` (`apply.rs:1208-1214`) already prefers it and never
  touches the store when it is present. The table was a second, mutable copy of
  something that should only ever arrive in the artifact.

  **Removing it changes no behaviour today**, verified by tracing the whole
  chain: the CLI sends no policy (zero policy references in
  `crates/zeroship-cli/src/`), control proxies no policy route
  (`migrations_api.rs`), and the only writer is `migrate-server`'s own endpoint
  (`api.rs:211`) that no client reaches. So the table is empty, `get_current`
  returns `None`, and every migration already composes against **the operator
  ceiling alone**.

  *An earlier revision of this list said "keep - four live readers". That was
  wrong in an instructive way: those are live CODE PATHS reading an EMPTY table.
  Grep found the callers and could not tell me the callers find nothing.*

  Also delete the two read endpoints (`api.rs:245` `get`, `:262`
  `list_versions`) and the write endpoint (`:211`).
- `migration_store.rs`'s approval transitions (`insert_pending`, approve,
  reject) and the `status` state machine;
- the `zeroship_control` grant on `migrated_migrations`
  (`db/migrations-ts/20260702000900_grants.ts:25`);
- the freeze check's dependence on a table nothing writes - it reads
  `zeroship_migrations.schema_migrations` instead.

**Constraint:** the three tables are created by an **applied, frozen** migration
(`20260702000200_control_tables.ts`, row 28 of the released ledger). Removing
them needs a NEW migration dated after the last applied file, not an edit. And
`db/released_migrations.tsv` is itself being deleted, so the ordering guard that
reads it (`released_ledger_misordered`) must move to the journal in the same
change or it silently stops guarding.

## Background: the shadows this replaces

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
