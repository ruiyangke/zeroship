# db-v2-migrations-playground

Focused demo of `@zeroship/migrations` (B1, commit `7ba2869`) — the
batched + resumable + online + dry-runnable data backfill component.

## What this demonstrates

Three migrations covering the common patterns:

1. **`backfillSeverity`** — additive backfill (NULL → "info"). Simplest
   case. New rows pick up the default automatically; old rows need
   this migration.
2. **`expandKind`** — copy `event_type` → `kind` for the
   **expand-migrate-contract** pattern. Old field stays valid;
   migration backfills the new field; future deploy drops the old.
3. **`addUserHash`** — derive a hash from `user_id` for analytics.
   The "derived field" pattern: new column's value depends on
   existing data, so a backfill (not a default) is required.

Lifecycle exercised:

  - `defineMigration` with `migrateOne` per-row transform
  - `migrations.run(m, { dryRun: true })` — preview without committing
  - `migrations.run(m)` — real execution; state persists to
    `__zeroship_migrations`
  - `migrations.status(m)` — read state snapshot
  - `migrations.cancel(m)` — abort a running migration
  - `migrations.reset(m)` — wipe state for re-run

State lives in `__zeroship_migrations` (A3 audit table). A crash
mid-migration is recoverable from the persisted `validate_cursor`
on the next `run()` (default behaviour). `reset: true` discards
state and starts from cursor=0.

## The expand-migrate-contract pattern

Breaking schema changes ship as three deploys:

1. **EXPAND** — schema accepts both old AND new shape (`event_type` AND
   `kind` both present, both nullable). Writes set both.
2. **MIGRATE** — run `expandKind` to backfill `kind` from `event_type`
   for old rows.
3. **CONTRACT** — once the audit log confirms the migration applied
   cleanly, drop `event_type` from the schema. The diff engine (A2)
   would refuse this deploy if any row still had `kind` null; it's
   the safety net.

App stays live throughout — new writes are valid in both shapes.

## Run locally

```bash
npm install
npm run typecheck
npm run dev           # vite-plugin bootstraps the dev runtime
npm run smoke         # in another shell — exercises the 5 checks
```

## Smoke test checks

1. Seed 1000 events with mixed NULL severity / empty kind / empty hash
2. Dry-run `backfillSeverity` → verifies no mutation, processed count
3. Real run `backfillSeverity` → verifies 0 NULL-severity rows after
4. `expandKind` → verifies 0 empty-kind rows
5. `addUserHash` → verifies 0 empty-hash rows
6. Status check + audit log accessibility

## Pointer

Each migration's status row in `__zeroship_migrations` includes:
  - phase ('backfill')
  - change_kind ('migration_batch' per batch)
  - status state machine (pending → running → applied)
  - validate_cursor (last PK processed; used for resume)
  - processed count
  - dead_letter_pks (rows that failed; under failureBudget → status
    becomes `applied_with_dead_letter`; over budget → status=failed)

Query:
```sql
SELECT name, status, processed, validate_cursor, applied_at
FROM __zeroship_migrations
WHERE phase = 'backfill'
ORDER BY id;
```
