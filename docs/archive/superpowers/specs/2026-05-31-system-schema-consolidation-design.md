# System-schema consolidation → a single `zeroship` schema

**Status:** approved (interactive, 2026-05-31). Pre-launch, no back-compat: rewrite
the changesets in place, no migration shim; dev DBs reset.

## Decision

All PLATFORM/SYSTEM Postgres tables move into **one `zeroship` schema**. Today they
live in 4 schemas in the single `zeroship` database: `auth` (17 tables), `control`
(15), `platform` (1 = `roles`), and `sandbox` (its own service). The sandbox is
folded in **fully** — its 14 migrations move into the Liquibase changelog and its
embedded migration runner is removed (one schema, one migrator).

**Out of scope — must NOT be touched:** per-app data schemas (named by app_id),
`__zeroship_admin` (plugin-db crypto), `public` (Liquibase DATABASECHANGELOG),
`pg_catalog`/`information_schema`. **Role names stay** (`zeroship_auth`,
`zeroship_control`, …, `sandbox_admin/app/audit/gdpr`) — they are roles, not
schemas; only schema-qualified table refs move.

## Why safe

- **No table-name collisions:** all 34 platform tables + the sandbox tables
  (`hosts/sandboxes/shares/events(+monthly partitions)/deleted_sandboxes/wake_jobs`)
  have globally-unique names.
- **search_path is never relied on** — every query is fully schema-qualified, so
  this is a precise `auth.`/`control.`/`platform.`/`sandbox.` → `zeroship.` rename.
- **16 cross-schema FKs** (mostly → `auth.users`) become same-schema + simpler.

## Phase 1 — platform consolidation (`auth`/`control`/`platform` → `zeroship`)

- Changelog: `0001` creates ONE `zeroship` schema (+ citext); `0002–0010` rewrite
  `auth.`/`control.`/`platform.` → `zeroship.`. Append-only-guard `REVOKE`s keep
  their role names; only the table refs move.
- Source + tests: rename the qualified refs in `crates/control`, `crates/authz`,
  `crates/core`, `crates/auth`, `crates/gateway` (~582 refs). The control crate's
  `auth_pg` connection stays (same DB); only its query strings change.
- Verify: `cargo check --workspace`; apply the changelog to a FRESH dev DB
  (`ops/db-migrate.sh`); run the DB-gated auth/control suites. Commit.

## Phase 2 — sandbox fold-in (separate commit, on top of Phase 1)

- Transcribe the 14 `crates/sandbox/migrations/*.sql` into Liquibase changesets in
  the `zeroship` schema (faithful 1:1 — preserve the reviewed DDL, partitions, role
  grants, append-only guards, CHECK domains; `sandbox.`→`zeroship.`). Drop the
  sandbox's own `schema_migrations` bookkeeping (Liquibase tracks via
  DATABASECHANGELOG).
- Rip out the embedded runner in `crates/sandbox/src/db.rs`: the `MIGRATIONS`
  array, `ensure_schema_at_version`, `run_pending_migrations`,
  `current_schema_version`, the designated-migrator gate
  (`SANDBOX_PG_RUN_MIGRATIONS`). The controller now assumes the schema exists
  (migrate service ran) — connect + `ping`, no version gate.
- Rename `sandbox.` → `zeroship.` across the sandbox crate (db.rs + handlers +
  persist/restore/wake_machine + tests); delete `crates/sandbox/migrations/`.
- Verify: `cargo check --workspace`; fresh-DB migrate applies the sandbox tables;
  the sandbox controller boots against the migrated schema; sandbox DB-gated suites
  pass. Commit.

## Invariants
- One `zeroship` schema holds every platform + sandbox table; no others created.
- Per-app schemas, `__zeroship_admin`, `public`, `information_schema` untouched.
- Role names unchanged; all FKs intact (now same-schema); append-only guards intact.
- After Phase 2, the sandbox controller has NO self-migration; Liquibase is the one
  migrator; `crates/sandbox/migrations/` is gone.

## Testing
Each phase: a fresh-DB Liquibase apply must succeed, and the affected DB-gated
suites must pass against the unified schema (regression: a query against the old
schema name would now fail).
