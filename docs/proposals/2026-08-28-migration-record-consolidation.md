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

## The one thing to get right

The grant must go to a **platform** role and must be **SELECT only**. Any grant
that lets a tenant role reach its own meta schema re-opens forgeability, which
is the property `provisioning.rs:163-169` exists to hold. A reviewer should
check the grantee, not just the privilege.
