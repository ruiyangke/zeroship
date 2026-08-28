# The migration service is one crate named `zeroship-migrate-server`

**Status:** landed (`8f69c7e53`). Kept for two decisions a later tidy-up would
otherwise undo: why the service is one crate rather than two, and why
plugin-db's dev-dependency on it is load-bearing.

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
| database objects | `zeroship.app_schema_applies`, plus the per-app engine journals |

The compose service name and `ZEROSHIP_CONTROL_MIGRATE_SERVER_URL` are one fact
in two places (`deploy/compose/docker-compose.yml:340` sets the latter to
`http://migrate-server:9091`). They move together or compose resolves nothing.

There is no `zeroship_migrated` database role, so no grant migration was needed.
`db/migrations-ts/20260816000100_service_assertion_replay.ts:112-113` records the
absence explicitly.

## What the service owns in the database

`zeroship.app_schema_applies`
(`db/migrations-ts/20260702000200_control_tables.ts:82`) is the platform's own
record of what an app's schema corresponds to. The engine journal cannot serve
as one: it lives in the app's own schema, whose migrator role owns it and can
drop it (`2026-08-28-migration-record-consolidation.md`).

**The `migrated_` table prefix is gone, and this page used to exist to defend
it.** `migrated_migrations`, `migrated_app_policies` and
`migrated_migration_audit` were removed from the corpus outright rather than
renamed, so the fossil resolved itself: crate, binary, config scope, DNS name
and table names now all agree. The old warning against a find-and-replace on
the string `migrated` no longer has a subject.

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
