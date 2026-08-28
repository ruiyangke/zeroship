# The migration service is one crate named `zeroship-migrate-server`

**Status:** landed (`8f69c7e53`). This page is kept for the one constraint the
change deliberately did not resolve: the service's three database tables still
carry the old `migrated_` prefix, and that mismatch is intentional.

## Shape

`zeroship-migrate-adapter` no longer exists. It carried exactly one thing --
`CompioPgSession`, a newtype over `compio_postgres::Client` implementing the
engine's driver-neutral `SqlSession` seam -- and that now lives in
`crates/zeroship-migrate-server/src/session.rs`.

The orphan rule requires the **newtype** to be local to the crate implementing
the foreign trait; it does not require a crate of its own. The old split was a
leftover from when the adapter also held `platform.rs`, `cluster_lock.rs`, an IR
author, a config parser and a binary, all of which were deleted before the fold.

`zeroship-plugin-db` keeps a dev-dependency on `zeroship-migrate-server`
(`crates/zeroship-plugin-db/Cargo.toml:135`). It is live, not vestigial:
`tests/integration.rs:5439-5495` calls
`zeroship_migrate_server::provisioning::provision_workflow_journal_schema` and
`WORKFLOW_OWNER_ROLE` so the workflow-journal test exercises the production
provisioning statement rather than a `CREATE SCHEMA` of its own, which would
leave the journal owned by whoever the test connected as -- a privilege shape
production never has. The edge is dev-only and `zeroship-migrate-server` does not
depend on plugin-db, so there is no cycle.

## What the name covers, and what it does not

| namespace | value |
| --- | --- |
| crate and binary | `zeroship-migrate-server` |
| config scope | `migrate_server` (`src/config.rs:21`), settings `migrate_server.*` |
| compose service / network DNS | `migrate-server`, port 9091 |
| control setting | `control.migrate_server_url`, flag `--migrate-server-url`, default `http://localhost:9091` |
| **database objects** | **`migrated_migrations`, `migrated_app_policies`, `migrated_migration_audit` -- unchanged** |

The compose service name and `ZEROSHIP_CONTROL_MIGRATE_SERVER_URL` are one fact
in two places (`deploy/compose/docker-compose.yml:340` sets the latter to
`http://migrate-server:9091`). They move together or compose resolves nothing.

There is no `zeroship_migrated` database role, so no grant migration was needed.
`db/migrations-ts/20260816000100_service_assertion_replay.ts:112-113` records the
absence explicitly.

## The `migrated_` table prefix is a deliberate, permanent cost

**Do not rename those three tables in place.** They are created, indexed,
constrained, granted and commented by *applied* migrations
(`20260702000200_control_tables.ts`, `20260702000600_constraints_indexes_fks.ts`,
`20260702000700_functions_triggers_comments.ts`,
`20260702000900_grants.ts`). Editing an applied migration file aborts every
later run against that database, permanently and with no self-healing arm --
see the migration-freeze rule in `AGENTS.md`.

A newer applied migration extends one of them:
`db/migrations-ts/20260828000000_migrated_descriptor_sha256.ts:26` adds
`descriptor_sha256` to `zeroship.migrated_migrations`.

The result is a service called `migrate-server` whose tables are named
`migrated_*`. That reads like an oversight and it is not. **A broad
find-and-replace on the string `migrated` is the single most dangerous edit
anyone can make in this area** -- it will hit those table names and brick the
platform migration runner against the deployed database. Any change to these
tables must be a NEW migration file dated after the last row of
`db/released_migrations.tsv`, never an edit to an existing one.

A handful of comments in `tests/`, `policies/platform.policy.toml` and
`crates/zeroship-config-contract/` still name `zeroship-migrate-adapter`. They
are historical notes recording where deleted code used to live, and are correct
as written.

## Why the name

`zeroship-migrated` read as a past-participle adjective -- "already migrated" --
which `docker-compose.yml` demonstrated by using both senses within a few lines
of each other. The `-d` daemon suffix does not survive being read as English.
`zeroship-migrate-server` says what it is and sorts beside
`zeroship-migrate-core`, `-backend`, `-postgres` and the rest of the family.
