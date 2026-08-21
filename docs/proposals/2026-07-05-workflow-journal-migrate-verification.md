# Workflow journal op-DSL migration verification

**Status:** historical verification report, 2026-07-05; current runner guidance updated 2026-07-15

## Summary

The durable-workflows journal migration was authored as a platform op-DSL migration at:

- `db/migrations-ts/20260705000000_durable_workflows_journal.ts`

Platform op-DSL migrations are established in `db/migrations-ts/`. The current
platform runner is `zeroship-platform-migrate`, built from
`crates/zeroship-migrate-adapter` on the published migration engine. Apply the
platform corpus with:

```bash
zeroship-platform-migrate \
  --migrations-dir db/migrations-ts \
  --database-url-file ./migrate-dsn \
  --project-schema zeroship \
  --project-id zeroship
```

The DSN is a path, not a value; the file must be owner-only (0600).

Platform `.ts` migrations are recorded to transient IR at migrate time;
committed `.ir.json` and the historical raw-SQL fixtures are not the platform
schema source of truth.

## Verification

Reference:

- `db/migrations/V0068__durable_workflows_journal.sql`
- `db/migrations/V0068__durable_workflows_journal.down.sql`

Historical method:

The steps below record what was run on 2026-07-05 with the now-retired in-tree
runner. Its command names are evidence for that verification, not current
runnable guidance.

1. Rendered the new op-DSL migration through the zeroship-migrate TS recorder plus PG lower/render path.
2. Applied the op-DSL migration with `zeroship-migrate --profile platform` to a fresh disposable Postgres database on the `appbase-migrate-postgres-1` container.
3. Applied the raw V0068 SQL to a second fresh disposable database.
4. Dumped `--schema-only --schema=zeroship --no-owner` for both.
5. Removed pg_dump session noise and the migration-journal immutability helper from the comparison, because it is runner metadata rather than the target journal schema.
6. Diffed the normalized target schema dumps.
7. Applied the op-DSL down body as a second platform op-DSL migration on a fresh disposable DB.

All disposable DBs and the temporary tablespaces were dropped after verification.

The historical target-schema comparison found one duration-representation
mismatch: the raw-SQL fixture used a database-native duration while the first
op-DSL draft used text. Both shapes are superseded. The implemented journal
stores `max_signal_age_ms` as `bigint`; the engine computes and binds the
timestamp cutoff used by the signal-freshness query.

The op-DSL down body left only the base fixture table:

```text
DOWN_REMAINING_TABLES=apps,
```

At the time, the retired runner's normal rollback path did not run the source
`.ts` down migration; see the historical gap below.

## Construct Coverage

Expressed faithfully:

- Tables, primary keys, regular unique constraints, and named check constraints.
- Cross-column checks, including `(parent_run_id IS NULL) = (parent_wait_step_key IS NULL)` and `(delivery = 'topic') = (topic IS NOT NULL)`.
- CHECK domains expressed with `membership(...)` / `notMembership(...)`.
- Regex checks with `.matches("^[0-9a-f]{64}$")`.
- Partial indexes with `where: (c) => ...`, including boolean predicates and `NOT paused`.
- Descending index elements, e.g. `{ kind: "column", name: "ordinal", order: "desc" }`.
- Composite primary keys and composite unique indexes.
- Foreign keys, including self-FK and `ON DELETE CASCADE` / `RESTRICT`.
- PostgreSQL grants and revokes as structured `@zeroship/migrate/pg` `grant` / `revoke` ops.

## Gaps

### 1. Duration representation mismatch (resolved)

The historical reference and the first op-DSL draft disagreed on the storage
shape. The durable-workflow contract now uses one portable representation:

- `workflow_steps.max_signal_age_ms` is a `bigint` millisecond count.
- `wake_at` and the signal-freshness cutoff are bound timestamps.
- Workflow SQL does not store a database-native duration or cast a duration
  string at query time.

No migration-engine duration column type is required for this feature.

### 2. No guarded role-grant primitive

Raw SQL uses a DO block to conditionally grant/revoke only if roles exist:

```sql
IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'zeroship_control') THEN
  EXECUTE 'GRANT SELECT, INSERT, UPDATE, DELETE ON TABLE ... TO zeroship_control';
END IF;
```

Current DSL support:

- Unconditional `grant(...)` and `revoke(...)` are expressible.
- There is no structured `ifRoleExists` guard and no managed least-privilege grant profile.

Result:

- The final ACLs match when the expected platform roles exist.
- The guarded no-op behavior for missing roles cannot be represented without raw SQL.

Recommended fix:

- Add `ifRoleExists: true` to role-targeting `grant` / `revoke`, or add a higher-level platform grant profile primitive for standard platform roles.

### 3. Platform TS render is not exposed by the current runner

Historical observation:

- The retired plan command did not record a platform `.ts` directory, and its
  creator-facing recorder used the confined policy rather than the platform
  policy.

Current result:

- `zeroship-platform-migrate` authors, lowers, and applies the platform corpus,
  but does not expose an offline preview flag.

Recommended fix:

- Add a preview mode to `zeroship-platform-migrate` backed by the adapter's same
  platform authoring and guarded-lowering path, without opening or applying to a
  database.

### 4. Platform TS rollback/down is not exposed by the current runner

Historical observation:

- The retired rollback path sent `.ts` files through its raw-SQL loader and
  rejected the migration filename.

Current result:

- `zeroship-platform-migrate` is an apply-only runner. Although the source
  migration can describe `down()` operations, the platform runner has no
  rollback command.

Recommended fix:

- Either add a platform-policy rollback mode to `zeroship-platform-migrate`
  that records and applies `down()` operations in reverse, or explicitly make
  platform migrations forward-only and reject misleading `down()` exports.
