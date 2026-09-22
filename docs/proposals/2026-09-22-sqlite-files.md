# The SQLite files: one per database, journal inside, paths from data

**Status.** PROPOSED, nothing built. Three decisions about the files the dev tier keeps on disk:
one file per database stays, a database's migration journal moves into that database's own file,
and every SQLite path becomes a value the host carries rather than a string each reader composes.
The two workflow schemas are a separate concern and are left alone.

---

## What exists today

Measured by opening each site, not by reading a doc about it. The census is grouped by what
DECLARES the name, because the interesting split is not which files exist but which of them a
reader invents.

| file | what declares the name | code or data |
|---|---|---|
| the session file (`main`) | `SqliteBackend::open_blocking` in `crates/zeroship-data-orm/src/backend/sqlite/mod.rs` takes the `DATABASE_URL` path when it names a FILE, and joins `zs-control.sqlite` when it names a DIRECTORY | directory and filename from data; the directory fallback name is a literal |
| the dev default for that URL | `cmd_serve` in `crates/zeroship-cli/src/main.rs`: `.unwrap_or_else(\|\| "sqlite:.zeroship/dev.sqlite".into())`, and `DEV_DB_DIR`/`DEV_DB_FILE` in `packages/vite-plugin/src/dev-db.ts` | literal, in two places, one Rust and one TypeScript |
| one file per database | `SqliteBackend::attach_alias_file` in the same `mod.rs`: `let file_path = self.db_dir.join(format!("zs-{alias}.sqlite"));` | prefix and extension are literals; `alias` is data |
| the alias inside that name | `SqliteBackend::database_alias` returns `binding.schema().as_str()`; `DbBinding::to_database` sets it from `database_derivation::schema_name`, which is `format!("db_{}", database.as_str())` | a derivation over a `DatabaseId` the host resolved |
| the workflow journal's file | the same `attach_alias_file`, under the app schema: `cmd_serve` builds the workflow store's binding with `DbBinding::platform(.., app_derivation::schema_name(&dev_app_id))`, and `app_derivation::schema_name` is `app.as_str().to_owned()` | a derivation over an `AppId` |
| the migration journal | `devSqlitePaths` in `packages/vite-plugin/src/gen-types/dev-apply.ts`: `journalPath: join(dir, \`zs-${appId}.migrations.sqlite\`)`; and `sqliteJournalPath` in `packages/zero-migrate-cli/src/cli.ts`, reached from `driverFor` as `journalOverride ?? sqliteJournalPath(appPath)` | composed in TypeScript, from an app id; the CLI accepts a `--journal` override, which is the one place it is data |
| the local platform file | `AppDeployment::new` in `crates/zeroship-cli/src/deployment.rs`: `platform: state.join("platform/metadata.sqlite")` | literal |
| the restore temp file | the restore path in `crates/zeroship-data-orm/src/backend/sqlite/snapshot_fixture.rs` recomposes `format!("zs-{app_id}.sqlite")` and a `.restore-tmp` sibling | recomposed from an identity the attachment already knows |
| the engine's project lock | `project_lock_path` in `crates/zeroship-migrate-sqlite/src/backend/mod.rs` names a sidecar after the app file's `(dev, ino)` | derived from the inode, deliberately: two hard links to one database name one lock |

Three things the census turns up that are worth stating before any decision rests on them.

**The session file holds nothing.** No production path issues DDL into `main`. The positive
control for that absence: the same search over `crates/zeroship-data-orm/src/backend/sqlite/`
does match `CREATE TABLE {alias}.leased`, inside the `#[cfg(test)]` module of `driver.rs`, so the
pattern finds `CREATE TABLE` where it exists and the empty production result is the tree's
content rather than a mis-scoped search. `main` is database zero and the anchor every `ATTACH`
hangs off, and
`BEGIN_TRANSACTION` in `crates/zeroship-data-orm/src/backend/sqlite/executor.rs` says what that
costs: `IMMEDIATE` "takes a write lock on every database the connection has open, `main` included".

**The engine takes both paths as parameters.** `SqliteBackend::open(app_path, journal_path)` in
`crates/zeroship-migrate-sqlite/src/backend/mod.rs` receives them; it composes neither. Every
composition is in a caller, and every caller composes its own.

**Two callers now compose different names for one file.** `devSqlitePaths` names
`zs-<app_id>.sqlite`. `attach_alias_file` names `zs-<alias>.sqlite`, where the alias is the
binding's schema, and a creator binding's schema is `db_<database_id>` with the database id read
from `.zeroship/private/dev-database-binding.json`, which `load_material` in
`crates/zeroship-cli/src/dev_binding.rs` mints when the file is absent:

```rust
database_id: DatabaseId::mint().into_string(),
```

So the dev apply writes one file and the dev runtime's `env.db` opens another. The header of
`dev-apply.ts` names exactly this as the failure worth guarding against: "`applyIrSqlite` would
report `applied: [...]` against a file nobody opens, and the app would still be broken with a
success line in the log." Two derivations over two different identities cannot be kept in
agreement by being careful, which is the whole of decision 3 below.

**Not SQLite, and partitioned out rather than filtered out.**
`crates/zeroship-metering/src/outbox.rs` composes
`format!(".zeroship/usage-outbox-{}.redb", wal.as_str())` and the KV store keeps
`kv.redb`. These are redb, single-writer by their own design, and nothing here applies to them.

---

## Decision 1: one file per database, and not one file for everything

**What this refuses: a single shared SQLite file holding every database.** Three measurements
say that is worse, and one of them says it plainly enough to settle the question on its own.

**The alias is the schema, and it has to be.** `SqliteBackend::database_alias` in
`crates/zeroship-data-orm/src/backend/sqlite/mod.rs` carries the argument in its own rustdoc:
"**It is the SCHEMA, and it has to be.** SQLite's ATTACH alias occupies the schema-name position
of a qualified table and every query builder qualifies with `binding.schema()`."
`binding_for_isolate` in `crates/zeroship-data-v8/src/v8_classes/db.rs` states the consequence:
"the dev tier addresses one file per database and PostgreSQL addresses one schema per database."
A single file has no alias to qualify with. Two databases that both declare a collection called
`users` would collide in one namespace, and the qualification that every compiled statement
carries would have nowhere to point. One file per database is not a storage layout; it is the dev
tier's expression of the boundary production expresses as a schema.

**SQLite admits one writer per database file.**
`crates/zeroship-data-orm/src/backend/sqlite/session.rs`
opens with it: "Separate connections prevent unrelated work from joining a creator transaction,
but SQLite writes still contend for the database's writer lock." The divergence register says the
same in creator-facing words: "SQLite has **one writer per database**". Folding N databases into
one file makes N writer locks into one.

State the limit of that argument honestly, because it is smaller than it looks. `BEGIN_TRANSACTION`
in `crates/zeroship-data-orm/src/backend/sqlite/executor.rs` is `BEGIN IMMEDIATE`, and its rustdoc
records the cost: "two apps on one backend now serialize their explicit transactions through
`main` instead of overlapping." So explicit creator transactions ALREADY serialize. What a single
file would additionally serialise is autocommit writes to different databases. That is real, and
it is the smaller half.

**`ATTACH` already gives an atomic transaction across files, so a single file buys no atomicity.**
`open_hardened` in `crates/zeroship-migrate-sqlite/src/backend/actor.rs` states the mechanism:

```
// A transaction that touches `main` and the attached `_mig` journal is
// crash-atomic only when SQLite can use its super-journal protocol. WAL,
// MEMORY, and OFF journal modes do not provide that guarantee across attached
// databases.
```

and `enforce_atomic_profile_for_schema` beside it pins and verifies the settings that buy it.
Cross-file atomicity is available today, on terms the engine already meets. The one thing a
single file would have bought is therefore already in hand.

**Whole-file operations stay whole-file.** The restore path in
`crates/zeroship-data-orm/src/backend/sqlite/snapshot_fixture.rs` copies a snapshot to a sibling
temp and renames it over the live file, then re-`ATTACH`es the alias. Restoring one database out
of a shared file would stop being a rename and start being a schema-scoped copy.

---

## Decision 2: a database's migration journal lives in that database's file

**Today's placement, from the code rather than from the register.** `MIG_ALIAS` in
`crates/zeroship-migrate-sqlite/src/backend/authorizer.rs` is `"_mig"`, and `open_hardened` in
`actor.rs` runs `&format!("ATTACH DATABASE ?1 AS \"{MIG_ALIAS}\"")` against a second file whose
path the caller supplied. The register's row agrees: `docs/reference/sqlite-divergences.md`,
"Migration journal placement", says the journal's tables sit in "a **separate attached database**,
`<app>.migrations.<ext>`, with the **unprefixed** names". The doc and the code say the same thing,
which is worth recording because only one of them is the fact.

**What the separate file costs.** Cross-file crash atomicity needs the super-journal protocol, so
`enforce_atomic_profile_for_schema` pins BOTH files to DELETE and FULL and refuses to proceed
otherwise:

```rust
"{schema}.journal_mode remained {actual:?}; DELETE rollback journaling is required for atomic app+journal commits"
```

That pin is the whole reason a creator's own tables are not in WAL on the dev tier.
`an_app_files_write_upgrade_is_plain_busy_because_it_is_not_in_wal`
(`crates/zeroship-data-orm/src/tests/sqlite/transactions.rs`) records the pair as it stands:
an attached app file answers `delete` and `main` answers `wal`.

**And it puts two hosts in disagreement about one file.** `initialize_sqlite` in
`crates/zeroship-workflow/src/service/schema.rs` opens the app file and runs

```rust
conn.pragma_update(None, "journal_mode", "WAL")
```

on it, while the migration engine pins the same file to DELETE and refuses to run otherwise.
`journal_mode = WAL` is persistent, so the two hosts are writing opposite settings to one file,
and which one a later open finds depends on which ran last. Neither is wrong on its own terms.
The conflict exists because the migration engine needs a mode the runtime does not, and it needs
it only because the journal is in a second file.

**So: fold it in.** A database's journal describes that database. With the journal inside the file
it describes, a migration is one transaction against one database, the super-journal requirement
disappears, the DELETE and FULL pin goes with it, app files are WAL like everything else, and both
of the register's rows above collapse rather than being maintained.

**What this refuses: keeping the journal in a sibling file because the engine already accepts a
path for it.** That the engine takes `journal_path` as a parameter is what makes this change
cheap. It is not an argument that the placement is right.

The journal's table names then need the fence PostgreSQL already applies. The register's own
reasoning for the `__zeroship_` prefix is that "the journal lives inside the tenant's own schema
and its table names are literals: without a fenced prefix, a creator declaring a table called
`schema_migrations` would have it silently adopted as the journal." Once SQLite's journal shares
the file, that reasoning applies unchanged and the prefix follows. The two tiers converge on one
spelling instead of branching on dialect.

---

## Decision 3: a SQLite file path is data the host carries, not a rule each reader recomputes

This is the same change `docs/proposals/2026-09-22-role-names-as-data.md` argues for cluster role
names, applied to file paths, and for the same reason: a derivation is not one copy of a fact, it
is one rule recomputed at every reader, correct only while every reader agrees on the rule AND on
the inputs.

**The readers do not agree on the inputs.** `devSqlitePaths` derives from an `AppId`.
`attach_alias_file` derives from a schema, which `DbBinding::to_database` derives from a
`DatabaseId`, which the dev host mints in `dev_binding.rs`. Both rules are internally consistent.
They name different files, and nothing compiles differently because of it. That is what a
derivation over a contested input does: it fails silently, at a reader, with a success line
upstream.

**The configuration already carries the identity.** `zeroship.jsonc` declares
`databases.<label>.id` as a `dbs_` id; `ProjectConfig::database_id` in
`crates/zeroship-cli/src/project_config/mod.rs` dereferences a label to it, and `declaredDatabases`
in `packages/vite-plugin/src/project-config/index.ts` reads the same map on the TypeScript side.
Both processes already read one file that names the database. Neither uses it to decide which
SQLite file to open.

**The tree already has the shape this wants.** `initialize_local` in
`crates/zeroship-workflow/src/service/schema.rs` needs the file behind a namespace and does not
compose one: it runs `PRAGMA database_list` and reads the `file` column for its namespace. The
path is a fact you can ask for. Every other reader recomputes it instead.

**What this refuses: a second derivation kept in sync with the first.** A fallback that composes a
path when the carried value is absent makes the value optional and the convention load-bearing
again, which is the defect being removed.

---

## What changes

Concretely, and naming the functions rather than the files:

- `SqliteBackend::attach_alias_file` (`crates/zeroship-data-orm/src/backend/sqlite/mod.rs`) attaches
  the path the binding carries instead of composing `format!("zs-{alias}.sqlite")`. The alias it
  attaches UNDER stays the schema, because decision 1 says that is what a qualified statement
  points at.
- `SqliteBackend::open_blocking` and `SqliteBackend::new` (same file) stop naming
  `zs-control.sqlite`. The session file is a configured path like any other.
- The restore path in `crates/zeroship-data-orm/src/backend/sqlite/snapshot_fixture.rs` reads the
  live path from the attachment it is about to replace rather than recomposing it.
- `devSqlitePaths` and `devSqliteDir` (`packages/vite-plugin/src/gen-types/dev-apply.ts`) retire.
  `applyMigrationsToDevSqlite` receives the same path the runtime will open.
- `load_material` (`crates/zeroship-cli/src/dev_binding.rs`) reads the configured `dbs_` id rather
  than minting one, so the two processes derive from one recorded identity or from none.
- `sqliteJournalPath` and the `--journal` flag (`packages/zero-migrate-cli/src/cli.ts`) retire with
  the journal fold, along with `journalPath` on the three napi verbs in
  `crates/zeroship-migrate-node/src/bridge.rs` and the second parameter of
  `SqliteBackend::open` in `crates/zeroship-migrate-sqlite/src/backend/mod.rs`.
- `MIG_ALIAS` and its authorizer arms (`crates/zeroship-migrate-sqlite/src/backend/authorizer.rs`),
  the `ATTACH ... AS "_mig"` in `open_hardened`, and the `journal_attached` arm of
  `enforce_atomic_profile` (`crates/zeroship-migrate-sqlite/src/backend/actor.rs`) go with it.
- `docs/reference/sqlite-divergences.md` loses the migration-journal-placement row and the
  journal-mode-of-app-data row. Both describe consequences of the separate journal file.

---

## What does not change

- **The two workflow schemas.** `zeroship-workflow-schema`'s `SQLITE`, installed by
  `initialize_sqlite` in `crates/zeroship-workflow/src/service/schema.rs`, and the manager's
  `SQLITE_SCHEMA`, installed by `initialize` in `crates/zeroship-workflow-manager/src/local.rs`
  into `.zeroship/platform/metadata.sqlite`. They have their own reasons, including a stamped
  version with a fingerprint and a bootstrap comparison against compiler output, and nothing here
  proposes anything for either. Named so they are out of scope by decision rather than by
  oversight.
- **The session file.** It keeps no tables and it is still database zero and the anchor every
  `ATTACH` hangs off. Only the way its path is chosen changes.
- **The engine's project lock.** `project_lock_path` names a sidecar after the app file's inode
  identity on purpose, so that two hard links to one database name one lock. That is not a file
  name pattern and it is not in scope.
- **The redb stores.** The metering outbox WAL and the KV store are a different engine with a
  different concurrency model.
- **The migration service is still PostgreSQL-only.** Folding the SQLite journal changes the
  embedded engine's file layout, not which dialects the hosted apply service accepts.

---

## Open

1. **Where does the path live once it is data?** Two candidates, and they are not the same answer.
   On the binding, beside the schema, so `attach_alias_file` reads it the way `to_database`
   reads a role today. Or in the project configuration the CLI and the vite plugin both already
   read, with the host resolving it once at boot. Production has no SQLite, so the only producer
   is the dev host; but `DbBinding` is the shared type and a SQLite-only field on it is a
   dialect leaking into a dialect-neutral shape.

2. **Does the journal's move put its tables inside the namespace the catalog walks?**
   `introspect_schema` in `crates/zeroship-data-orm/src/backend/sqlite/protection.rs` lists user
   tables with `format!("SELECT name FROM {q_app}.sqlite_master WHERE type = 'table' ORDER BY
   name")` and skips a name only when `name.starts_with("sqlite_")`. Journal tables landing in
   that file become tables the catalog reports as the creator's unless the fence prefix is also a
   skip rule there. The PostgreSQL side has the same shape and is the place to check what it does
   about it.

3. **Does the dev host read a configured database id, or does the configuration read the host's?**
   SETTLED: the dev host reads the configuration, and the mint is not needed at all. `id` is a
   REQUIRED key on every declared database - `DATABASE_REQUIRED_KEYS` is `["id", "migrations",
   "out"]` in both `crates/zeroship-cli/src/project_config/generated.rs` and
   `packages/vite-plugin/src/project-config/generated.ts`, which come from one generator, so a
   config that parses has declared one. The reader already exists:
   `ProjectConfig::database_id` (`crates/zeroship-cli/src/project_config/mod.rs`) resolves
   `databases.<label>.id` and names the declared labels when it cannot.

   So `load_material` (`crates/zeroship-cli/src/dev_binding.rs`) minting a `DatabaseId` is
   generating a fact the file it is reading beside already states, and that mint is the root of
   the naming disagreement above: it is the third identity for one database, after the app id the
   apply composes with and the declared id everything else reads. `.zeroship/private/dev-database-
   binding.json` survives only for the BINDING id, which nothing declares and which no other
   reader needs to predict.

4. **Is `zs-` still earning its place?** Once a path is data, the prefix is not distinguishing
   anything a directory does not already distinguish. Worth deciding with the rest rather than
   inheriting.

---

## Acceptance

Each arm names what makes it fail, because an arm that cannot fail measures nothing.

(a) **Two databases on one dev host are two files, and a write to one does not take the other's
writer lock.** Bind two databases, issue an autocommit write to each, and assert both land.
Control differing in one variable: hold an explicit transaction open on the first and assert the
second's autocommit write still lands, which separates the per-file lock from the connection-wide
one `BEGIN IMMEDIATE` takes. Fails if the files are folded, and fails if the control cannot
distinguish the two locks.

(b) **A migration commits its journal entry and its DDL in one transaction against one file, and a
crash between them is not observable.** Apply a migration, interrupt between the DDL and the
journal write, reopen, and assert the journal and the schema agree. Fails if the two can disagree,
which is the property the super-journal protocol currently buys across two files.

(c) **The app file is in WAL, and nothing pins it to DELETE.** Read `PRAGMA <alias>.journal_mode`
after an apply and after a runtime boot, in either order, and assert both answer `wal`. Fails if
any host still writes the other mode, which is the two-host conflict this is removing. Nonempty
input: the apply must have committed a delta, asserted from the journal, so the check is not
passing over a no-op.

(d) **The apply and the runtime open the same file, and the test can tell.** Apply migrations
through the dev path, boot the dev runtime, and read a collection through `env.db`. Control: with
the configured identity changed and nothing else, the runtime reaches a different file and the
read fails. Fails if any reader still composes a path, because a second composition would agree
with the first only until an input moved.

(e) **A second composition does not COMPILE.** Not a scanner: this repository's rule is that
tests assert on behavior, compiler contracts, parsed artifacts or structured metadata, and never
on source spelling, so "nobody composes a path" has to be a type rather than a grep. The path
becomes a newtype whose only constructor is private to the resolver that owns it, and every
opener takes that type instead of a `Path`. A reader that wants to build its own filename then
has nothing to build it with, and the arm is the compile failure itself - exercised the way the
other compiler contracts here are, with a `trybuild`-style case or an equivalent that fails when
the constructor is made public again. Fails if any opener still accepts a bare path, because that
signature is the hole the convention would leak back through.

(f) **The journal's tables are not reported as the creator's.** Install the journal into the app
file, then assert `introspect_schema` returns the creator's collections and none of the journal's.
Control: a creator collection whose name begins with the fence prefix is still reported, so the
skip rule is a prefix fence and not a substring filter. Fails if the catalog adopts the journal.
