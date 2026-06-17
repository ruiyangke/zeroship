# Database migrations (zeroship-migrate)

The platform's Postgres schema — the single `zeroship` schema that holds every
platform/system table, plus the dedicated `oauth_hydra` schema — is managed by
**`zeroship-migrate`**, zeroship's own versioned migration engine, run under its
**Platform** trust profile. Migrations are hand-authored Flyway-style SQL files
in `db/migrations/`, version-controlled, reviewable, and rollback-able. The
engine tracks what's applied in an append-only journal (in a meta schema,
default `zeroship_migrations`), so `migrate` only ever runs pending files and is
safe to re-run (idempotent no-op when everything is applied).

> **Hydra owns its own schema** via `hydra migrate` (the `hydra-migrate` compose
> service). Do not put hydra tables in `db/migrations/`. The `oauth_hydra`
> *schema + role + grants + uuid-ossp pre-create* live in `V0027`; Hydra's own
> `hydra_*` tables are migrated by Hydra.

Services **do not migrate themselves.** `zeroship-control`/`zeroship-auth`
connect to an already-migrated database — exactly as in production, where the
`migrate` step runs once before anything boots. There is no inline migration
code in the Rust crates.

## Why our own engine (not Liquibase)

The platform schema used to run on Liquibase; it now runs on `zeroship-migrate`
under the **Platform** profile. The trust separation that mattered (the creator
migration path must never reach `control`/`auth`/`billing`) is preserved as a
**call-site invariant**: the Platform profile is constructible only at the
operator call site (gated by a crate-private `PlatformCapability` token), and
the creator submission ingress is hard-wired to the Confined profile with no API
path to Platform. The win over Liquibase is a **parse-time deny-list backstop**
(every statement — including DO-block / EXECUTE-literal bodies — is parsed with
the real `pg_query` parser and the RCE / host-escape / file / network surface is
hard-denied even under Platform) plus per-migration checksum tamper-evidence.
See `docs/proposals/2026-06-17-platform-migrations-flyway-mode-design.md`.

## Layout

```
db/migrations/
  V0001__extensions_schemas.sql        # citext + CREATE SCHEMA zeroship (the one platform schema)
  V0001__extensions_schemas.down.sql   # OPTIONAL reverse for the same version
  …                                    # 56 versioned files today (V0001–V0057, 0045 is a gap)
  V0004__control.sql                   # the control-plane app/usage/env tables
  V0027__oauth_hydra_schema.sql        # the separate oauth_hydra schema + least-priv role for Hydra
```

The filename encodes everything — there is **no** `--changeset`/`--rollback`
header parsing. The grammar is `V<NNNN>__<description>.sql` (versioned "up"),
`V<NNNN>__<description>.down.sql` (optional reverse for that version), and
`R__<description>.sql` (repeatable, re-applies on checksum change). Files apply
in numeric `V<NNNN>` order. **One file = one migration**, multi-statement,
whole-file transaction-atomic. Any `--rollback` / `--liquibase formatted sql`
comment surviving from the port is just a comment — the reverse lives in the
sibling `.down.sql`. Every object reference is fully schema-qualified; the
Platform guard permits the `zeroship`/`oauth_hydra`/`public` schema allowlist.

## Running migrations

**In the compose stack** — automatic. The one-shot `migrate` service runs
`zeroship-migrate migrate --dir /db/migrations --profile platform` after
Postgres is healthy and before control/auth start (they `depends_on` it with
`service_completed_successfully`):

```bash
docker compose up -d            # migrate runs, then control/auth boot
docker compose logs migrate     # see what was applied
```

**By hand** against a running dev DB — `ops/db-migrate.sh` shells into the
`zeroship-migrate` bin (via `cargo run`, or set `ZEROSHIP_MIGRATE_BIN` to a
prebuilt binary). It targets the compose Postgres on `localhost:5440` by default
(override with `ZEROSHIP_MIGRATE_DSN`):

```bash
ops/db-migrate.sh status            # pending vs applied
ops/db-migrate.sh migrate           # apply pending migrations
ops/db-migrate.sh validate          # dry-run on a shadow DB + guard-check + drift + destructive advisories
ops/db-migrate.sh rollback --steps 1  # roll back the last applied migration (requires --yes for the bin)
ops/db-migrate.sh rollback --to 0024  # roll back everything strictly after V0024
```

(There is no `changelog-sync` / adoption verb — pre-launch zeroship has no
deployed DB whose history must be honoured; fresh DBs re-migrate from scratch.)

## Adding a migration

1. Create the next-numbered file `db/migrations/V<NNNN>__<name>.sql` (the whole
   file is the "up", multi-statement). Schema-qualify everything.
2. If the change is reversible, add a sibling `V<NNNN>__<name>.down.sql` with the
   reverse SQL. (A multi-statement file's `.down.sql` should undo its statements
   in reverse order.)
3. `ops/db-migrate.sh validate` to dry-run + guard-check, then `migrate` to apply.

The loader picks the file up by its numeric `V<NNNN>` version order — no master
file to edit. **Never edit an already-applied migration** (the engine validates
per-migration checksums and aborts on drift); add a new versioned file instead.

## Tests

The control/auth integration tests connect to a **pre-migrated** database
(`AUTH_DB_URL` / `CONTROL_TEST_DB`) — they no longer self-migrate. Bring the
schema up once before running them: the compose `migrate` service does this for
the compose DB, or run `ops/db-migrate.sh migrate` against your test DB.
