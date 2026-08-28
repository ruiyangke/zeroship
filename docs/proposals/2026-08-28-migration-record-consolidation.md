# Five records of one fact: consolidate onto the engine journal

**Date:** 2026-08-28
**Status:** finding + recommendation. Not started.

Raised by the operator asking a one-line question - *"where do we have two places
to manage the migrations?"* - which turned out to have a five-part answer, three
of which are shadow copies and two of which are already broken.

## The count

### Creator app migrations: two, and the overlap is documented as hazardous

| record | holds |
| --- | --- |
| `<app_uuid>_migrations.schema_migrations` | **what ran.** Engine journal: `event_kind` in `applied`/`rolled_back`, `version`, `name`, `checksum`, `at`, `by`, `exec_ms`, `phase`, `kind`. Append-only |
| `zeroship.migrated_migrations` | **what was requested.** `status`, `submitted_by`, `approved_by`, `ceiling_id`/`ceiling_version`, `request_body`, `approved_checksum` |

These are genuinely different concerns and the split is right: the engine is a
standalone publishable library and knows nothing about operator ceilings or
approval, correctly.

**But they overlap on the single fact everything depends on - "applied" - and
the code already documents that they can contradict** (`migrate-server/src/apply.rs:784-788`):

> *"The DDL is committed and cannot be taken back, but a concurrent path
> rejected this migration while the engine was applying it, so the row reads
> `rejected`. The record now contradicts the database it describes and only an
> operator can reconcile them."*

That is an accepted divergence on the write path. **The rollback hole
(`2026-08-28-deploy-schema-precondition.md`) is the same seam from the read
side:** the ledger can move backwards because it records *requests*; the journal
cannot, because it records *events*.

### Platform migrations: three, and two are dead

| record | state |
| --- | --- |
| `zeroship_migrations.schema_migrations` | the engine journal for the platform's own schema, protected by a `schema_migrations_immutable` trigger. **Alive and authoritative** |
| `zeroship_migrations.platform_migration_files` | filename + checksum. **No writer** - its only producer, `zeroship-migrate-adapter/src/platform.rs`, was deleted 2026-08-28 (register L32) |
| `db/released_migrations.tsv` | a git-tracked snapshot of the above. **All 34 checksums stale**; the pre-flight check reading it was demoted from `fail` to a printed report |

The platform grew two bookkeeping layers *beside* the engine's own journal, and
today's removal killed both. Neither death was noticed until someone went
looking.

## The root cause: the journal has no platform reader

Every shadow exists because the code that needed "what actually ran" could not
reach the record of it.

- The journal's public reader is `journal_sql::applied`
  (`zeroship-migrate-postgres/src/backend/journal_sql.rs:506`). **No platform
  service calls it.**
- The meta schema is created on the superuser provisioning DSN, and
  `provisioning.rs:169` runs `REVOKE ALL ON ALL TABLES IN SCHEMA {meta} FROM
  {role}`.

**That REVOKE is deliberate and must not be undone.** `{role}` is the **app's own
migrator role**, and the comment beside it (`:163`) gives the reason:
*"unforgeable by deny-by-absence"*. A tenant that could write its own journal
could claim a migration ran that never did. The fence is a security property,
not an oversight.

**But it fences the TENANT, not the platform.** `zeroship_control` is a
different role and granting it SELECT on the meta schema does not weaken the
unforgeability property at all - the tenant still cannot write, and now cannot
write *or* be the only source the platform trusts.

## The recommendation

**Grant a platform role read on the journal, and delete the shadows.**

One change collapses three open problems:

1. **The rollback hole closes.** The deploy precondition currently compares
   against `migrated_migrations`, which records requests and is therefore
   mutable in both directions. Comparing against the journal - or against a
   descriptor `migrate-server` derives *from* the journal - removes the
   backwards move entirely, because re-submitting an old IR journals nothing.
2. **L32 stops needing a fix.** `platform_migration_files` and
   `db/released_migrations.tsv` exist to answer "was an applied file edited?",
   which is `version` + `checksum` - exactly two columns `schema_migrations`
   already stores, under an immutability trigger. Neither shadow needs a
   producer if the check reads the journal.
3. **The freeze guard gets a source that cannot drift.** Today it reads a table
   nothing writes, compared against a TSV maintained by a deploy script. The
   journal is append-only by trigger.

**What `migrated_migrations` legitimately keeps:** approval state, submitter and
approver identity, the operator ceiling a migration was approved under, the
request body, and `approved_checksum`'s TOCTOU pin. None of that is in the
journal and none of it should be. The recommendation is not to delete the
ledger - it is to stop using the ledger as the answer to *"what ran?"*, which is
the one question it cannot answer correctly.

## FINAL DECISION 2026-08-28: everything in the creator-accessible schema

**Operator, on being shown the ownership objection below and reaffirming:**
*"place everything inside the creator accessible schema, no new schema, the
creator is responsible for the migration."*

**This supersedes the sibling-schema compromise recorded further down.** The
reasoning is a position, not an oversight: if the creator owns their database
and their migrations, the journal is **their** bookkeeping. A creator who
corrupts it breaks their own app, which is their problem, not a platform
integrity failure.

**What that costs, stated so it is not rediscovered.** The platform can no
longer treat the creator journal as a trust anchor - a tenant owning the schema
can `DROP` or rewrite it, and owner privileges cannot be revoked. So:

- `zeroship.migrated_migrations` **remains** the platform's answer to "did this
  app's migrations apply", and the consolidation recommended above applies to
  **platform** migrations only.
- **The rollback hole must be closed platform-side**, not by reading the
  journal. The workable fix without the journal: record what the engine
  **reported as applied** (`outcome.applied`) on the ledger row alongside the
  declared descriptor. A re-submitted old IR produces `applied: []`, so the row
  records that nothing advanced and the precondition can compare against the
  newest row that actually applied something. That keeps the per-request write
  the engine-upgrade case needs while removing the backwards move.
- `provisioning.rs` step 5's `REVOKE ALL ON ... SCHEMA {meta}` becomes moot -
  there is no separate meta schema to revoke. Delete it rather than leave a
  revoke that names nothing.

### The prefix is load-bearing, not cosmetic - and it needs an engine change

Verified 2026-08-28, and this is the part that decides the work:

1. **The engine's journal table names are LITERALS.** Only `{meta}` is
   interpolated (`zeroship-migrate-postgres/src/backend/journal_sql.rs:128`,
   `:178`, `:224`, `:327`, `:342`, plus `schema_backfills`):
   `schema_migrations`, `schema_migrations_supersedes`,
   `schema_migrations_inflight`, `schema_pending_contracts`,
   `schema_deploy_recovery`, `schema_backfills`. `conn.rs` exposes `meta_schema`
   and no table-name knob.
2. **`validate_collection` fences only `pg_` and `__zeroship`** for table names.
   **So a creator CAN declare a table named `schema_migrations`.**
3. The engine creates its journal with `CREATE TABLE IF NOT EXISTS`. A
   creator-declared `schema_migrations` in the same schema would therefore be
   silently **adopted as the journal**, or collide on shape.

So co-habitation without a prefix is a live collision, and the operator's
`zeroship_migrate_` prefix is exactly what prevents it.

**The work this implies, in dependency order:**

1. **Engine:** prefix the six journal tables to `zeroship_migrate_*`, or add a
   table-prefix config. Our copy is in-sourced at `crates/zeroship-migrate-*`,
   so this is an ordinary change - not a vendored-submodule edit.
2. **Reserve the prefix in `validate_collection`**, beside `pg_` and
   `__zeroship`. **Without this the prefix is decoration** - a creator can still
   declare `zeroship_migrate_schema_migrations` and collide deliberately. This
   step is what converts a naming convention into a guarantee.
3. **Config:** `meta_schema` becomes the app's own schema.
4. **Platform:** `zeroship_migrations` schema becomes `zeroship_migrate`.
5. **Delete** `provisioning.rs` step 5's now-meaningless revoke.
6. **A migration** moving existing journal objects, remembering that
   `db/migrations-ts` files already applied are frozen.

## SUPERSEDED: the naming, with one correction

Operator instruction: place creator migration records under
`<app_schema>.zeroship_migrate_<table>`, and platform records under
`zeroship_migrate.<table>`.

**The platform half lands as instructed.** `zeroship_migrations` becomes
`zeroship_migrate`, matching the renamed crate family.

**The creator half cannot be inside `<app_schema>`, and the reason is
ownership rather than grants.** `provisioning.rs:140-160` does three things to
the app's project schema:

```
ALTER SCHEMA {proj} OWNER TO {role}                       -- step 3
GRANT CREATE, USAGE ON SCHEMA {proj} TO {role}            -- step 4
ALTER DEFAULT PRIVILEGES ... GRANT ALL ON TABLES TO {role} -- step 4b
```

The migrator role **owns** that schema. A PostgreSQL schema owner can `DROP` and
`TRUNCATE` any table in it, and owner privileges are **implicit - they cannot be
REVOKE'd away.** So a journal inside `<app_schema>` is a journal the tenant can
delete or rewrite, which destroys exactly the property step 5 exists to hold:

```
REVOKE ALL ON ALL TABLES IN SCHEMA {meta} FROM {role}
REVOKE ALL ON SCHEMA {meta} FROM {role}
-- "journal is unforgeable by deny-by-absence"
```

**The separateness of the meta schema IS the mechanism.** It is not incidental
placement.

**Adopted instead - a sibling schema, same naming family:**

| | before | after |
| --- | --- | --- |
| creator | `<app_uuid>_migrations.<table>` | **`<app_schema>_zeroship_migrate.<table>`** |
| platform | `zeroship_migrations.<table>` | **`zeroship_migrate.<table>`** |

One word different from the instruction, the same consistency gained, and the
revoke keeps working untouched.

### The shape that would have been better, and why it is blocked

One shared `zeroship_migrate` schema holding **every** app's journal, rows
partitioned by app, would be maximally consistent and would enable the
consolidation above directly. It does not work today: **`schema_migrations` has
no tenant column.** Its columns are `event_seq`, `event_kind`, `version`,
`name`, `checksum`, `at`, `by`, `exec_ms`, `down`, `phase`, `outcome`, `kind`.
A shared table would mix tenants with no way to separate them, and `version`
would collide across apps.

**So the engine's per-app isolation is carried by the schema name itself.** That
is worth stating plainly because it constrains every future consolidation: any
move to a shared journal must first add an owner column upstream, in the
standalone engine, and that changes a published contract.

## The one thing to get right

The grant must go to a **platform** role and must be **SELECT only**. Any grant
that lets a tenant role reach its own meta schema re-opens forgeability, which
is the property `provisioning.rs:163-169` exists to hold. A reviewer should
check the grantee, not just the privilege.
