# Database migrations (Liquibase)

The platform's Postgres schema — the `control`, `auth`, and `platform`
schemas — is managed by **[Liquibase](https://www.liquibase.com/)**. Migrations
are hand-authored SQL changesets in `db/changelog/`, version-controlled,
reviewable, and rollback-able. Liquibase tracks what's applied in its
`DATABASECHANGELOG` table (in `public`), so `update` only ever runs pending
changesets and is safe to re-run.

> **Hydra owns its own schema** via `hydra migrate` (the `hydra-migrate` compose
> service). Do not put hydra tables in `db/changelog/`.

Services **do not migrate themselves.** `zeroship-control`/`zeroship-auth`
connect to an already-migrated database — exactly as in production, where the
`migrate` step runs once before anything boots. There is no inline migration
code in the Rust crates.

## Layout

```
db/changelog/
  db.changelog-master.yaml      # entry point: includeAll of changesets/ (lexicographic)
  changesets/
    0001_extensions_schemas.sql # citext + CREATE SCHEMA control/auth/platform
    0002_auth.sql               # auth.* tables, append-only guards, cron tables
    0003_platform.sql           # platform.roles
    0004_control.sql            # control.* tables, append-only guards
```

Each changeset is **formatted SQL**: a `--changeset author:id` header, the SQL,
and a `--rollback`. Changesets that contain `DO $$ … $$` / `CREATE FUNCTION …
$$ … $$` carry `splitStatements:false` so the `;` inside the dollar-quoted body
is not split. Every object reference is fully schema-qualified.

## Running migrations

**In the compose stack** — automatic. The one-shot `migrate` service runs
`liquibase update` after Postgres is healthy and before control/auth start
(they `depends_on` it with `service_completed_successfully`):

```bash
docker compose up -d            # migrate runs, then control/auth boot
docker compose logs migrate     # see what was applied
```

**By hand** against a running dev DB — `ops/db-migrate.sh` wraps the
`liquibase/liquibase` image (targets the compose Postgres on `localhost:5440`;
override with `ZEROSHIP_DB_JDBC`/`ZEROSHIP_DB_USER`/`ZEROSHIP_DB_PASS`):

```bash
ops/db-migrate.sh status          # pending vs applied
ops/db-migrate.sh update          # apply pending changesets
ops/db-migrate.sh update-sql      # print the SQL update WOULD run (dry run)
ops/db-migrate.sh history         # deployment history
ops/db-migrate.sh validate        # checksum / structural validation
ops/db-migrate.sh rollback-count 1  # roll back the last applied changeset
ops/db-migrate.sh tag v1          # tag current state for later rollback-to
```

## Adding a migration

1. Create the next-numbered file `db/changelog/changesets/NNNN_<name>.sql`.
2. Add one `--changeset <author>:<id>` per logical change, the SQL, and a
   matching `--rollback`. Schema-qualify everything. Use `splitStatements:false`
   on any changeset with a `$$` body.
3. `ops/db-migrate.sh update-sql` to preview, then `update` to apply.

`includeAll` picks the file up by filename order — no master-changelog edit
needed. **Never edit an already-applied changeset** (Liquibase validates
checksums); add a new one instead.

## Adopting onto an existing database

A DB that already has the schema (e.g. one an older build created) won't have a
`DATABASECHANGELOG`. Mark the changesets applied without re-running them:

```bash
ops/db-migrate.sh changelog-sync
```

Fresh databases just run `update`.

## Tests

The control/auth integration tests connect to a **pre-migrated** database
(`AUTH_DB_URL` / `CONTROL_TEST_DB`) — they no longer self-migrate. Bring the
schema up once before running them: the compose `migrate` service does this for
the compose DB, or run `ops/db-migrate.sh update` against your test DB.
