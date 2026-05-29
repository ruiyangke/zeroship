# Platform DB Migrations — Liquibase, hand-authored, code-verified

**Goal:** Replace the ad-hoc inline migration code (control's `Registry::new`
DDL + auth's `store::migrations::migrate`) with a real migration tool —
**Liquibase** — driven by hand-authored, reviewable, rollback-able changesets
that are the single source of truth for the platform's Postgres schema.

**Status:** design + build, worktree `.worktrees/full-stack-compose`, commit-only.

---

## Decisions (from the user)

1. **Liquibase**, run as a one-shot compose service (like `hydra-migrate`) and
   via `ops/db-migrate.sh` for dev.
2. **Hand-author the changesets** — do NOT dump/generateChangeLog from a DB. The
   inline migration code is the source of truth to transcribe.
3. **Full cutover** — delete the inline migrations from control and auth.
4. **Verify with code** — tests run the real Liquibase to set up their schema;
   a dedicated parity test proves the changesets reproduce the inline schema.

## Scope of ownership

Liquibase owns the `control`, `auth`, and `platform` schemas (+ the `public`
trigger function `app_audit_block_tamper`). **Hydra owns its own schema** via
`hydra migrate` (the `hydra-migrate` compose service) — out of scope here.

---

## Changeset layout

`db/changelog/db.changelog-master.yaml` → `includeAll: changesets/` (lexicographic).
Each file is **formatted SQL** with a `--changeset author:id` header and a
matching `--rollback`. PL/pgSQL `DO $$ … $$` blocks, `CREATE FUNCTION … $$ … $$`,
and trigger creation need `splitStatements:false` on their changeset (the `;`
inside `$$` bodies must not be split).

Dependency order (cross-schema FKs all point into `auth.users`; `oauth_grants`→
`oauth_clients`; `payouts`→`creator_accounts`):

| file | contents |
|---|---|
| `0001_extensions_schemas.sql` | `CREATE EXTENSION IF NOT EXISTS citext;` + `CREATE SCHEMA IF NOT EXISTS {control,auth,platform}`. (Drop `uuid-ossp` — unused; all UUID defaults use `gen_random_uuid()`.) |
| `0002_auth.sql` | all `auth.*` tables (users first), the `auth.audit_events_block_tamper()` fn + triggers + role-revokes, **plus the 2 missing tables** `auth.wrapper_revoked_subjects` and `auth.jwk_key_state`. |
| `0003_platform.sql` | `platform.roles` (FK → `auth.users`). |
| `0004_control.sql` | all 16 `control.*` tables; `public.app_audit_block_tamper()` + `control.authz_decisions_block_tamper()` fns + triggers + role-revokes. oauth_clients before oauth_grants; creator_accounts before payouts. |

All object references **fully schema-qualified**. Every changeset reversible
(`--rollback DROP …`); `0001` rollback drops the schemas (CASCADE).

## Collapse / drop rules

- Fold every `ALTER TABLE … ADD COLUMN IF NOT EXISTS` into the table's final
  column list (the inventory lists net columns). Drop duplicate re-adds.
- `control.payouts` CHECK constraints: keep the inline named CHECKs; **drop** the
  idempotent `DO $$ … pg_constraint` re-add block (registry.rs:376–433).
- `control.oauth_clients.created_by`: declare nullable (the `ALTER … DROP NOT
  NULL` collapses in).
- `control.app_audit`: net = base columns (the `DROP COLUMN actor` removes a
  column never in the final shape).
- **Drop** `control_migrations` (table + keyed mechanism) entirely.
- **Drop** both DATA migrations (no production data, per AGENTS.md no-backfill):
  `app_secrets_aad_v1` wipe (registry.rs:223–246) and the
  `creator_account_history` open-row backfill (registry.rs:316–334) — just
  create the table + the `idx_…_one_open` unique partial index.
- Role-revoke `DO $$` loops (registry.rs:557–583; migrations.rs:158–179,346–367):
  keep verbatim (they guard on role existence); they harden the append-only
  tables alongside the triggers.

## The 2 missing tables (cron bug fix)

Inferred from `crates/core/src/wrapper_revocation.rs` and
`crates/auth/src/cron/{token_sweep,jwk_rotation}.rs`:

```sql
CREATE TABLE IF NOT EXISTS auth.wrapper_revoked_subjects (
    subject    UUID PRIMARY KEY,
    revoked_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE TABLE IF NOT EXISTS auth.jwk_key_state (
    set_name   TEXT NOT NULL,
    kid        TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (set_name, kid)
);
```

## Cutover

Production entry points to replace (Liquibase now owns schema creation):
- `crates/control/src/main.rs:657` — `Registry::new(&db_url)` → connect-only.
- `crates/auth/src/main.rs:166` — `store::migrations::migrate(&client)` → remove.
- `crates/control/src/main.rs:742` — conditional `zeroship_auth::store::
  migrations::migrate(&auth_pg)` (gated on `bootstrap_builder_client`) → remove.

Delete the migration DDL bodies: `Registry::new` keeps only the connect; delete
`crates/auth/src/store/migrations.rs`'s statement array + `migrate`.

**Tests** (~38 `Registry::new` + ~37 `migrate` callers, almost all in
`crates/{control,auth}/tests/` + the two `#[cfg(test)]` `pg()` helpers in
`control/src/http_util.rs` and `auth/src/identity/linker.rs`): replace the
self-migrate call with a shared helper that applies the **real Liquibase**
changelog to the throwaway DB once per process (faithful — the test exercises
the actual migrator). `Registry::new` then just connects.

## Verification (with code)

1. **Parity test**: spin a fresh DB, run the inline migrations (reference) on
   one and the Liquibase changesets on another; assert the schemas are
   equivalent except the intended deltas: `+auth.wrapper_revoked_subjects`,
   `+auth.jwk_key_state`, `−control_migrations` (dropped). Use `liquibase diff`
   (DB-to-DB over JDBC) or a table/column/constraint introspection query — no
   schema file dump.
2. **Suites**: `cargo test -p zeroship-control` and `-p zeroship-auth` green
   against Liquibase-migrated test DBs.
3. **Integration**: the full compose stack boots with the `migrate` service
   gating the platform services; auth cron no longer errors on the 2 tables.
