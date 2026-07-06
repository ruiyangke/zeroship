# Workflow journal op-DSL migration verification

**Status:** verification report, 2026-07-05

## Summary

The durable-workflows journal migration was authored as a platform op-DSL migration at:

- `db/migrations-ts/20260705000000_durable_workflows_journal.ts`

Platform op-DSL migrations are established in `db/migrations-ts/`. The platform runner applies them with:

```bash
zeroship-migrate --profile platform --dir db/migrations-ts migrate --yes
```

The legacy `db/migrations/*.sql` corpus remains the raw-SQL path. Platform `.ts` migrations are recorded to transient IR at migrate time; committed `.ir.json` is not the platform source of truth.

## Verification

Reference:

- `db/migrations/V0068__durable_workflows_journal.sql`
- `db/migrations/V0068__durable_workflows_journal.down.sql`

Method:

1. Rendered the new op-DSL migration through the zeroship-migrate TS recorder plus PG lower/render path.
2. Applied the op-DSL migration with `zeroship-migrate --profile platform` to a fresh disposable Postgres database on the `appbase-migrate-postgres-1` container.
3. Applied the raw V0068 SQL to a second fresh disposable database.
4. Dumped `--schema-only --schema=zeroship --no-owner` for both.
5. Removed pg_dump session noise and the migration-journal immutability helper from the comparison, because it is runner metadata rather than the target journal schema.
6. Diffed the normalized target schema dumps.
7. Applied the op-DSL down body as a second platform op-DSL migration on a fresh disposable DB.

All disposable DBs and the temporary tablespaces were dropped after verification.

Target-schema diff result:

```diff
@@ -172,7 +172,7 @@
     output_content_type text,
     wake_at timestamp with time zone,
     signal_type text,
-    max_signal_age interval,
+    max_signal_age text,
     consumed_signal_id text,
     child_run_id text,
     batch_id text NOT NULL,
```

The op-DSL down body left only the base fixture table:

```text
DOWN_REMAINING_TABLES=apps,
```

The normal `zeroship-migrate rollback --dir <ts-dir>` path did not run the source `.ts` down migration; see gaps.

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

### 1. No interval column type

Raw SQL:

```sql
max_signal_age INTERVAL
```

Current DSL support:

- `interval("HH:MM:SS")` exists as an expression literal.
- `t.interval()` does not exist as a column type.

Result:

- The op-DSL migration uses `t.text()` for `workflow_steps.max_signal_age`, so the schema diff is not equivalent.

Recommended fix:

- Add a column type, e.g. `t.interval()` or a PG vendor type under `@zeroship/migrate/pg`.
- If cross-dialect support is required, define the portable downgrade explicitly instead of silently using text in PG.

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

### 3. Platform TS render is not exposed by the CLI

Observed:

- `zeroship-migrate plan` only loads `.sql` and `.ir.json`; it reports no artifacts for a `.ts` platform migration dir.
- `zeroship-migrate-js record` records under the confined creator profile, so it rejects platform tables with author-owned `id` columns and vendor `grant` ops.

Result:

- Rendering platform `.ts` before apply required a temporary helper that called public crate APIs to record with `PolicyProfile::platform()` and lower to PG SQL.

Recommended fix:

- Teach `zeroship-migrate plan --profile platform --dir <ts-dir>` to transiently record platform `.ts` migrations and render them without a DB apply.

### 4. Platform TS rollback/down is not wired

Observed:

```text
zeroship-migrate: load migrations: unrecognized migration filename:
'20260705000000_durable_workflows_journal.ts'
```

`migrate --profile platform --dir <ts-dir>` applies `.ts` migrations, but `rollback --dir <ts-dir>` still routes through the raw SQL loader.

Result:

- The source migration includes `export function down()`, and the down operations are expressible.
- The normal platform CLI rollback path cannot invoke that `.ts` down today.

Recommended fix:

- Extend rollback/down to discover platform `.ts` migrations, record their `down()` phase, and apply the resulting down IR/fragments in reverse.
- Alternatively, explicitly declare platform `.ts` migrations forward-only and remove/forbid misleading `down()` exports.

