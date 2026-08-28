# Fold the adapter into the migration service, and rename it

**Date:** 2026-08-28
**Status:** planned, not started. Blocked only on the deploy-precondition work
finishing, because that agent is editing `crates/zeroship-migrated/src/apply.rs`
and `migration_store.rs` - the exact files this moves.

Two operator instructions, 2026-08-28:

1. Merge `zeroship-migrate-adapter` into `zeroship-migrated`.
2. Rename `zeroship-migrated` to `zeroship-migrate-server`.

## 1. The merge is unblocked - the orphan rule does not require a separate crate

`zeroship-migrate-adapter` exists **only** to carry `CompioPgSession`, a newtype
over `compio_postgres::Client` implementing the engine's driver-neutral
`SqlSession` seam. Its own module doc says it is "the session newtype and
nothing else since the platform one-shot was deleted on 2026-08-28".

The orphan rule requires the **newtype** to be local to the crate that
implements the foreign trait - not that it live in a crate of its own. So
`zeroship-migrate-server` can define `CompioPgSession` directly. There is no
technical reason for the split; the split is a leftover from when the crate also
held `platform.rs`, `cluster_lock.rs`, an IR author, a config parser and a
binary, all deleted earlier today.

**Consumers, both verified:**

| consumer | uses it? |
| --- | --- |
| `zeroship-migrated/src/apply.rs:23` | **yes** - `use zeroship_migrate_adapter::CompioPgSession;` |
| `zeroship-plugin-db/Cargo.toml:134` (dev-dep) | **no** - zero `zeroship_migrate_adapter` identifiers anywhere in the crate |

The plugin-db dev-dep is **vestigial and should be deleted with the merge.** Its
justifying comment (`Cargo.toml:128-133`) says the parity tests need a
`SqlSession` producer because the orphan rule blocks plugin-db writing its own.
Those tests exist and are compiled - `tests/parity/mod.rs` is 676 lines,
included by `integration.rs:48` and `sqlite_integration.rs:30` - but they no
longer reference the adapter. The comment describes an intent the code stopped
realizing.

`zeroship-config-contract/Cargo.toml:30` mentions the adapter only in a comment
recording that its entry used to sit there.

## 2. The rename: six namespaces, five free, one FROZEN

The name appears in ~90 files: 24 Rust, 6 `Cargo.toml`, 15 shell, 3 deploy, 47
docs.

| # | namespace | current | action |
| --- | --- | --- | --- |
| 1 | crate name | `zeroship-migrated` (`Cargo.toml:2`) | rename |
| 2 | binary name | `zeroship-migrated` (`Cargo.toml:13`) | rename - referenced as `"$BIN/zeroship-migrated"` in test harnesses, plus `deploy/Dockerfile` |
| 3 | compose service / network DNS | `migrated` (`docker-compose.yml`, `http://migrated:9091`) | rename **in lockstep with 5** |
| 4 | control CLI flag | `--migrated-url` | rename |
| 5 | env vars | `ZEROSHIP_CONTROL_MIGRATED_URL`, `MIGRATED_PORT`, `MIGRATED_TEST_DB` | rename; `ZEROSHIP_CONTROL_MIGRATED_URL`'s default is the compose DNS name, so 3 and 5 must move together or compose resolves nothing |
| 6 | **database objects** | `migrated_migrations`, `migrated_app_policies`, `migrated_migration_audit` | **DO NOT RENAME** |

### Why 6 is a hard stop, twice over

Those tables are created and granted by **applied, frozen** migrations
(`db/migrations-ts/20260702000900_grants.ts:25,27`,
`20260702000700_functions_triggers_comments.ts:46`). `AGENTS.md`: editing an
applied file aborts every later run against that database, permanently.

And independently: the deploy-precondition work in flight **adds
`descriptor_sha256` to `migrated_migrations`**. Renaming that table would
collide with it head-on.

An agent doing a broad find-and-replace on "migrated" will rename these unless
told not to. That is the single most dangerous edit in this task.

### One thing that does NOT need renaming, verified

There is **no `zeroship_migrated` database role.**
`db/migrations-ts/20260816000100_service_assertion_replay.ts:112-113` records it
explicitly: *"there is no `zeroship_migrated` role in
`20260702000100_schema_roles_extensions.ts`, so migrated is absent here."* So the
role namespace is untouched and no grant migration is needed.

## 3. Sequencing

Do NOT start while the deploy-precondition change is in flight. It edits
`crates/zeroship-migrated/src/apply.rs`, `src/migration_store.rs` and
`tests/apply_api_test.rs`; this task moves the whole directory **and** edits
`apply.rs` (to drop the `use zeroship_migrate_adapter::...` line). That is a
rename-plus-edit conflict on the same files, which git resolves badly.

## 4. Acceptance

- `cargo build --workspace` green.
- `cargo test -p zeroship-migrate-server` green, count stated before and after.
- **`grep -rn 'zeroship-migrated\|zeroship_migrated' --include='*.rs' --include='*.toml' --include='*.sh' .` returns zero** outside `target/`, EXCEPT the frozen `db/migrations-ts/` table names and any historical note that deliberately records the old name.
- `tests/gate_arm_census.sh tests` still passes - several harnesses invoke the
  binary by path.
- The compose stack still resolves the service: `deploy/compose/docker-compose.yml`
  and `ZEROSHIP_CONTROL_MIGRATED_URL`'s default agree on one name.
- Docs updated (47 files), including `AGENTS.md`'s crate index, which names
  `migrated/` in its layout table.

## 5. Why the name is better

`zeroship-migrated` reads as a past-participle adjective ("already migrated"),
which is what the comment at `docker-compose.yml:159` accidentally demonstrates
when it says services "boot against a fully-migrated schema" three lines from a
reference to the service itself. The `-d` daemon suffix is a Unix convention
that does not survive being read as English. `zeroship-migrate-server` says what
it is, and it sorts beside `zeroship-migrate-core`, `-backend`, `-postgres` and
the rest of the family it belongs to.
