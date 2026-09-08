Plan written to `/home/ruiyang/Projects/appbase/.scratch/kill-test-helpers-plan.md`. Full text:

---

# Execution plan: kill `test-helpers`, and remove `__zeroship_admin`

Every claim is marked CONFIRMED (I ran the command in this tree), PLAUSIBLE (read the
source, did not compile), or NEEDS-BUILD (cannot be settled without cargo). I ran no
`cargo build`, `check`, `test` or `clippy` while writing this; `cargo metadata --no-deps`
and greps only. Paths and symbol names are cited; line numbers deliberately are not.

---

## 1. The answer in one paragraph

**This is a flag deletion, not a test relocation, and your instinct is right.** The single
fact that settles it: `#[cfg(test)]` never fires for an integration target, and all fifteen
of `zeroship-plugin-db`'s test targets are integration targets (CONFIRMED from `cargo
metadata`). So moving a test file from one crate's `tests/` to another's retires exactly
zero gates - the moved file is still a separate crate seeing only `pub` items - and moving
it into `src/` under `#[cfg(test)]` would trade a compiler-enforced live-database target
guard for nothing. The flag comes off by fixing what it was compensating for, with every
test file staying where it is. What it compensates for is three unrelated things wearing one
name: (a) a re-export ladder that widens nothing, because nine of the modules it aliases are
already `pub` in the crate below and all four owning crates are already plugin-db
dev-dependencies (CONFIRMED); (b) three genuine *behaviour* forks, where the shipped build
and every tested build differ - the broker's thread isolation, the SQLite session's
`next_command_gate` field, and the write-pipeline counters - which no amount of relocation
touches and which are the real "make the code testable" work; (c) one gate that is not a
hack at all. **Two things must survive under honest names, and I will defend both.** First,
`required-features` is the workspace's live-database *target suppressor*, and four
`[[test]]` blocks in `crates/zeroship-plugin-db/Cargo.toml` say so in their own comments
("it FAILS rather than skips without a database"); cargo offers no other mechanism, so
`live-db-tests` survives as a self-contained `= []` selector - the same shape
`zeroship-control`, `zeroship-worker` and `zeroship-migrate-server` already run. Second, the
two `impl Backup` blocks are gated because `restore` issues `DROP SCHEMA ... CASCADE` and
shells out to `pg_dump`/`pg_restore`; keeping that out of every worker, gateway and CLI
binary is real safety work with nothing to do with tests, and it becomes a `backup` feature.
Calling both of those `test-helpers` is what let two vendor manifests keep a justification
that is now false, and it is why the flag reads as a hack even where it is doing a job.

---

## 2. Directive 2 first: `__zeroship_admin`

Small, independent, and it discharges one open question that directive 1 would otherwise
have to defer.

### 2.1 The complete disposition list

**DELETE (code) - exactly one executable statement in the whole tree.** CONFIRMED by
`grep -rn "__zeroship_admin" crates libs sdks db deploy tests | grep -vE ':[0-9]+:[[:space:]]*(//|///|//!|\*|#)'`,
which returns five lines: four refusal-test fixtures (below) and

- `crates/zeroship-data-postgres/src/postgres.rs`, `backup_pg::pitr_replay_impl` - the
  `INSERT INTO __zeroship_admin.pitr_targets` string.

It is unreachable in every shipped configuration: both `impl zeroship_data_core::storage::Backup
for PostgresBackend` and `mod backup_pg` carry `#[cfg(feature = "test-helpers")]` - feature
only, no `any(test, ...)` arm (CONFIRMED by reading the attributes above each item). It has
no caller and no test; `pitr_pg_records_target` was deleted with the schema and is itemised
in `tests/run_plugin_db_live_suite.sh`'s floor ledger.

Deleted with it, as one coherent unit:

- `Backup::pitr_replay` (`crates/zeroship-data-core/src/storage.rs`)
- `backup_pg::pitr_replay_impl` (`crates/zeroship-data-postgres/src/postgres.rs`)
- `SqliteBackend`'s `pitr_pg_only` refusal arm (`crates/zeroship-data-sqlite/src/lib.rs`)
- `PitrTarget` (`crates/zeroship-data-core/src/capability.rs`) and the engine's re-export
  in `crates/zeroship-data-engine/src/backend/mod.rs`
- one test, `pitr_pg_only_returns_configuration_on_sqlite` in
  `crates/zeroship-plugin-db/tests/sqlite_integration.rs`

**The design constraint must land in the same commit or the deletion loses it.** Add one
paragraph to the `Backup` trait rustdoc: PostgreSQL WAL replay needs server-level
configuration (`recovery.conf`, `archive_command`) that no client connection can initiate,
so the PG arm only ever recorded a requested target for an operator to act on; SQLite refused
outright for want of a WAL-archive substrate; the recording table went with the platform
system schema and PITR targets were never rehomed. Without that paragraph the next author
re-adds the method.

**KEEP - the prefix reservation, in full, and every refusal fixture.** There are five
reservation surfaces, not the two the brief names (CONFIRMED):

| Surface | Rows that must survive |
| --- | --- |
| `crates/zeroship-data-query-builder/src/ident.rs` `NAMESPACE_RESERVATIONS` | `Reservation::Prefix("__zeroship")` |
| `crates/zeroship-data-query-builder/src/ident.rs` column table | `Reservation::Prefix("__zeroship_")` |
| `crates/zeroship-data-query-builder/src/ident.rs` `ALIAS_RESERVATIONS` | `Reservation::Prefix("__zeroship")` |
| `crates/zeroship-schema/src/query.rs` `RESERVED_NAMES` | `ReservedName::Prefix("__zeroship_")` |
| `PLATFORM_RESERVED_COLLECTION_PREFIXES` in `zeroship-data-query-builder/src/ident.rs`, `zeroship-schema/src/query.rs` and `zeroship-migrate-core/src/schema/query.rs` | `"__zeroship"`, pinned three-way by a live agreement test inside `zeroship-schema/src/query.rs` |

The four `__zeroship_admin` fixture sites all stay:
`crates/zeroship-data-query-builder/src/ident.rs`
(`every_role_has_deliberate_reservation_behavior`),
`crates/zeroship-data-query-builder/tests/ident_refusals.rs`
(`the_namespace_fence_refuses_the_platform_schema`),
`crates/zeroship-migrate-node/tests/gen_artifacts_reserved_identifiers.rs`
(`RESERVED_TABLE_NAMES`), and `crates/zeroship-migrate-core/src/model/validate.rs`.

**Two corrections to the reason both censuses gave for keeping them.**

1. *"`__zeroship_admin` is the only longer-than-prefix witness in the corpus"* is **FALSE**
   (CONFIRMED). `ident.rs` also carries `(IdentRole::Alias, "__zeroship_internal", false)`
   and `Reservation::Prefix("pg_").matches("pg_class")`; `ident_refusals.rs` carries
   `Ident::parse_as("__zeroship_leak", IdentRole::Alias)` and `"__zeroship_migrations"` in
   both the table-name and column-name corpora. A `starts_with` to `==` refactor is caught
   four other ways. Publishing the false reason is how the rows get deleted later by
   somebody who checks it.
2. The **real** reason to keep them is that both are inside counting assertions:
   `assert_eq!(cases.len(), 6, "a new role must be given reservation behavior deliberately")`
   and `assert_eq!(ruled_on, 5)` (CONFIRMED). Drop a row and the test fails. The fence is
   *bound*, not merely spelled.

**State this explicitly: the prefix reservation survives, and it is not guarding an empty
namespace.** This is the largest factual correction in the whole exercise. `__zeroship_` is
the live prefix of tables that exist in **every app schema today** (CONFIRMED):
`__zeroship_schema_migrations` (named in `deploy/scripts/deploy-remote.sh`),
`__zeroship_audit_unmask` (`crates/zeroship-migrate-server/src/provisioning.rs`
`AUDIT_UNMASK_TABLE`), and the five `__zeroship_workflow_*` journal tables
(`crates/zeroship-plugin-workflow/src/store/pg.rs`). The fence stops creator code from
declaring a collection or column that collides with any of them. It *additionally* holds the
namespace open for the schema epoch that
`docs/proposals/2026-08-28-app-database-decoupling.md` specifies. Both censuses framed it as
a fence over a dead name; a reader who believes that will eventually delete it.

**REWRITE, do not delete - the Rust prose.** Roughly twenty comment sites across
`crates/zeroship-data-engine/src/{auth/mod.rs,auth/bootstrap.rs,crud/unmask.rs,
crud/mask_policy.rs,backend/mod.rs}`, `crates/zeroship-data-core/src/{storage.rs,
encryption/keys.rs}`, `crates/zeroship-plugin-db/src/lib.rs`,
`crates/zeroship-migrate-server/src/provisioning.rs`, and
`crates/zeroship-plugin-db/tests/integration.rs`. Substitute "a platform-owned system
schema" for the identifier and keep every date, count and argument. Three of these are bug
postmortems and not history:

- `crud/unmask.rs` `ensure_mask_policy_cached` records that the PG arm used to fail with an
  undefined-schema error *instead of default-denying*, and names its regression fence
  (`pg_declared_mask_policy_authorizes_unmask_without_durable_store`).
- `crud/mask_policy.rs` records why there is no durable PG policy store.
- `data-core/src/encryption/keys.rs` records why there is no database-backed key source.

Delete those and the next author re-derives the bug. Add to
`crates/zeroship-data-engine/src/auth/mod.rs` the one sentence neither file currently
carries: the name is **reserved, not retired**, AGENTS.md's privilege invariant permits
exactly one use for it, and the five reservation surfaces hold the prefix open for that use.

**KEEP UNTOUCHED.** `docs/archive/` (five files, each banner-marked as frozen);
`docs/reviews/dbbind-2026-08-26/snapshot-*/` (tracked in git; editing a frozen snapshot voids
the review it belongs to); and - most importantly - the two **live** proposals
`docs/proposals/2026-08-28-app-database-decoupling.md` and
`docs/proposals/2026-08-28-cdc-service.md`. Those two *specify creating* the schema, and one
of them carries the grant posture ("Zero worker-callable functions. No `SECURITY DEFINER`,
no `EXECUTE ... TO PUBLIC`, no `GRANT USAGE ON SCHEMA` ...") that is the entire difference
between the schema deleted for being a vulnerability and the one the invariant permits. A
literal "remove it everywhere" sweep run against them would delete a security specification
and leave the identifier's disappearance as the only trace. Scope directive 2 as: **no code
names it; no prose asserts it exists; the design that reserves it is untouched.**

`db/migrations-ts/` and `deploy/` contain zero occurrences, in any form. CONFIRMED as
established absence, not a failed instrument: `grep -rn "__zeroship_admin" db deploy` exits
1, while the positive controls `grep -rn "__zeroship_" db` returns 2 and
`grep -rn "__zeroship" deploy` returns 3.

### 2.2 The AGENTS.md correction

The invariant's paragraph currently says: *"One live statement still names it and therefore
fails on every database: the PITR placeholder at `crates/zeroship-data-postgres/src/postgres.rs:1333`,
whose own comment at `:839-844` says the schema 'NO LONGER EXISTS' ..."*

Both halves are wrong and both line citations have drifted (CONFIRMED: the statement and the
comment are both roughly a hundred lines below their cited positions; AGENTS.md is not
line-checked by `tests/doc_citation_gate.sh` at all, because arm 1's character class
excludes `:`). Replace with, and cite the module and function rather than new numbers, per
AGENTS.md's own rule:

> `db/migrations-ts/` provisions no such schema - measured against every file in that
> directory with a positive control proving the search would have found `__zeroship_` had it
> been there. No statement in any shipped binary named it either. The last SQL that did, an
> `INSERT INTO __zeroship_admin.pitr_targets` inside `zeroship-data-postgres`'s `backup_pg`
> module, carried `#[cfg(feature = "test-helpers")]` on both the module and the
> `impl Backup for PostgresBackend` block, a feature enabled only through
> `[dev-dependencies]`; it had no caller and no test, and it was deleted along with the
> `Backup::pitr_replay` placeholder it belonged to.

Do **not** delete the paragraph above it. "A system schema (`__zeroship_admin`) is therefore
reserved for exactly one thing: state a separate service WRITES and the worker only READS"
is the reservation the five fences serve and the shape the two live proposals build.

---

## 3. Directive 1: the end state

### 3.1 Manifests

- **No crate declares `test-helpers`.** The five declarations
  (`zeroship-data-{core,postgres,sqlite,engine}`, `zeroship-plugin-db`) and the nine
  forwarding routes are gone. CONFIRMED that those nine plus one `live-db-tests` route are
  ten of the twenty-two `dep/feat` routes in the workspace.
- **`live-db-tests` survives, self-contained.** `= []` in `zeroship-plugin-db` and in
  `zeroship-data-engine` (the engine's declaration is not optional: its
  `auth/bootstrap.rs` carries `#[cfg(all(test, feature = "live-db-tests"))] mod
  live_reserved_sweep_tests`, and dropping the declaration while that `#[cfg]` survives
  trips rustc's `unexpected_cfgs`. NEEDS-BUILD to settle whether that is warn or deny here:
  `./tests/clippy_gate.sh`). It gates targets via `required-features`, in-crate
  `#[cfg(all(test, feature = "live-db-tests"))]` modules, and one module that must not exist
  in a shipped binary (`drop_namespace`, below). It never gates a `pub` that a test reaches
  across a crate boundary.
- **A new `backup` feature** in `zeroship-data-postgres` and `zeroship-data-sqlite`, carrying
  `dep:sha2`, gating `impl Backup` x2, `mod backup_pg`, `mod backup_sqlite`, `pub mod
  lock_guard`, `trait PgLockManager` and its impl, and the `_assert_*_backend_impls_backup`
  witnesses in `crates/zeroship-data-engine/src/backend/mod.rs`. `sha2` stays a normal
  optional dependency in both vendors - a dev-dependency cannot serve it, because
  `sha256_file` lives in the LIB. This commit makes **no** ship-or-delete decision about
  backup; it renames a gate to describe what it actually protects.
- **A new dev-only crate `crates/zeroship-data-testkit`** (`publish = false`), the third
  member of a family `zeroship-test-support` and `zeroship-testkit` already establish. It is
  consumed only from `[dev-dependencies]`.

### 3.2 Which symbols became real API

Every one of these is unconditional and `#[doc(hidden)] pub` unless stated. The house
precedent is `crates/zeroship-runtime/src/core/runtime.rs`'s `into_inner_probe_for_test`.

| Symbol | Crate | Why it is API and not a hack |
| --- | --- | --- |
| `crud::encryption_pass::{encrypt_row_on_write, decrypt_row_on_read}` (plain `pub use`, module stays `pub(crate)`) | data-engine | The engine's CRUD passes are its contract. Three test references resolve through them. |
| `crud::system_fields_pass::apply_system_fields_on_insert` (plain `pub use`) | data-engine | Same. Two references. |
| `transaction::probe` module | data-engine | An injectable SC-1 protocol probe. One reference. |
| `pg_error::classify_pg_per_app_session_setup` (renamed off `_for_tests`) | data-postgres | A thin public wrapper over a disposition production transaction code already consumes (its own rustdoc says so). |
| `SqliteBackend::arm_next_command_gate` (renamed off `_for_tests`), plus `NextCommandGate` and `NextCommandGate::release` | data-sqlite | "Park this actor's next command" is a deterministic-scheduling seam a future graceful-drain would want. **Three items, not one** - the return type and its method are gated today and must be promoted with it or `private_interfaces` fires. |
| `Broker::drain_all()`, `current_thread_subscription_count()`, `drop_current_thread_subscriptions()` | data-core | See 3.4. |
| `write_path_counters()` / `reset_write_path_counters()` / `arm_write_path_counters()` | data-engine | See 3.4. |
| `reset_context_for_tests`, `set_db_url_for_tests`, `set_postgres_pool_for_tests`, `set_sqlite_backend_for_tests` | plugin-db | The residue. These set process-global statics and are the four that **cannot** move (their consumers span six integration targets). They are not good API; they are honestly-marked API. See section 7. |

Moved out rather than promoted: **`DbBinding::cold_start` leaves `zeroship-data-core`
entirely** and becomes `cold_start_binding(app_id)` in the testkit. Its body is
`SchemaName::new` plus `COLD_START_DEPLOY_TOKEN` plus `DbBinding::new`, all already `pub`
(PLAUSIBLE from reading `crates/zeroship-data-core/src/binding.rs`), so nothing in data-core
changes except the deletion. This converts its safety property - "nothing in a shipped binary
constructs a deploy identity from an app id alone" - from a feature-resolution accident into
a dependency-graph fact, and it **narrows** data-core. Promoting it to unconditional `pub`,
as one of the two source designs proposed, would ship a constructor whose own rustdoc says it
panics on an illegal app id into every worker binary.

Deleted outright: `PgSqlExecutor` and its impl and both witnesses (its own rustdoc names
deletion as the follow-up and its one method has no behavioural caller);
`strip_encryption_markers` made private; the stale `#[cfg(feature = "test-helpers")]` on the
sqlite `SchemaIntrospect` witness activation line; and the engine's duplicate role
provisioner (3.5).

### 3.3 Which tests moved where

**None across a crate boundary.** That is the plan's shape and it is what makes it
executable. Two files move *in-crate*, both of which narrow the shipped surface:

- `crates/zeroship-plugin-db/tests/db_v8_class.rs` and `tests/subscription_finalizer.rs`
  become `#[cfg(test)]` modules under `crates/zeroship-plugin-db/src/`, beside
  `v8_classes/cold_open.rs`, whose header already states the rule and applies it to three of
  the five mints. `mint_db` and `mint_subscription` go back to `pub(crate)`, joining
  `mint_collection`, `mint_db_platform` and `mint_masked_value`.
- Three tests are **deleted**: `cold_unmask_open_comes_from_ensure_backend_not_the_fixture`
  and its two siblings in `tests/sqlite_integration.rs`, which `v8_classes/cold_open.rs`
  binds strictly more of (its header says so and names the mutation that separates them).
  The sentence in `sqlite_integration.rs`'s header pointing at them is repointed at
  `cold_open.rs`.

Everything else stays exactly where it is. `crates/zeroship-plugin-db/tests/audit_table_parity.rs`
in particular stays: its subject is a three-way constant agreement across three crates that
no single crate can own, and dispersing it would delete the one guard demonstrating the
failure mode it exists for.

### 3.4 Behaviour forks: gone, and how

These are the "make the code testable" half. Relocation does nothing for them.

- **Broker** (`crates/zeroship-data-core/src/broker.rs`). `owner_thread`,
  `is_owned_by_current_thread`, `take_current_thread_subscriptions` and
  `current_thread_subscription_count` become unconditional. Free `drop_app` narrows from
  `Option<&str>` to `&str` - no production caller passes `None`. The global drain survives as
  an ungated inherent `Broker::drain_all()` **with no caller**, because `drop_app`'s own
  rustdoc names worker shutdown as a consumer: an absent caller today is evidence about the
  calendar, not about the design, and the substitute a later author would reach for
  (`drop_current_thread_subscriptions` on a signal thread) is silently wrong in a
  V8-per-thread process. Free `live_subscription_count` is renamed
  `current_thread_subscription_count`, so the semantics are in the name rather than in
  whether a dev-dependency edge unified a feature.
- **SQLite session** (`crates/zeroship-data-sqlite/src/session.rs`). `next_command_gate`
  becomes an unconditional field, with an `AtomicBool` armed flag checked by a relaxed load
  in the actor loop and the mutex taken only when set. One struct layout, one hot path.
- **Write pipeline** (`crates/zeroship-data-engine/src/crud/write_pipeline.rs`). **This is
  where I depart from both source designs and from one critic.** One design left the fork
  and moved it to `cfg(test)`, which is dead on arrival: twelve of its consumers are in
  plugin-db integration targets, where `cfg(test)` never fires (CONFIRMED: seven
  `reset_write_path_counters_for_tests` and five `write_path_counters_for_tests` references
  from `tests/`). The other design added a `WritePathObserver` public trait, which is a
  heavier permanent commitment to the engine's published surface than the problem warrants.
  Use the mechanism the SQLite fix already invents: **unconditional counters behind a relaxed
  armed atomic**, exposed `#[doc(hidden)] pub`. One write path in every build, one relaxed
  load per pass against a database round trip, no new trait, and the two sites end up with
  the same shape - which is worth more than either local optimum.

### 3.5 The engine's duplicate role provisioner

`crates/zeroship-data-engine/src/auth/bootstrap.rs` holds `ensure_per_app_role`,
`create_role_if_missing`, `set_local_role_sql`, `drop_per_app_role` and a private
`coded_sql`, all gated. **Zero production callers** (CONFIRMED: outside `bootstrap.rs`, the
only non-test caller of any of them is `drop_per_app_role` from
`crates/zeroship-plugin-db/src/drop_namespace.rs`; every other site is a test - 28 in
`integration.rs`, 5 in `native_transaction.rs`, 3 in `mask_flip.rs`, 2 in `unmask_tx_lane.rs`,
1 each in `parity/mod.rs` and `search_tx_lane.rs`). Production role creation is
`zeroship-migrate-server`'s `provisioning.rs` / `apply.rs`; production `SET LOCAL ROLE` is
`zeroship-data-postgres`'s `pg_session_sql.rs`.

It must be **deleted**, and the deletion is forced, not optional:
`tests/decision_four_gate.sh` treats `test-helpers`-gated items as non-production, so
removing the feature promotes real `CREATE ROLE` / `GRANT` / `ALTER DEFAULT PRIVILEGES` text
into the engine's production region and reddens its `statement_sites` arm - correctly.
`cfg(test)` cannot save it, because 41 consumers are cross-crate. Baselining it would be the
gate becoming a rubber stamp on the tier that runs creator code.

The fixtures call the real provisioner instead. `zeroship-migrate-server` is already a
plugin-db dev-dependency for exactly this reason and exposes `provision_database`,
`provision_migrator` and `runtime_role_provisioning_sql` (CONFIRMED they are `pub`).
NEEDS-BUILD whether any of them has `ensure_per_app_role`'s create-if-missing/idempotent
contract or whether a thin fixture wrapper in the testkit is needed on top.

`drop_namespace.rs`'s single call is satisfied by inlining the `DROP ROLE` statement there.
That module is already the tree's `DROP SCHEMA` / `DROP ROLE` home and is itself gated, so
the statement does not move tier. `revoke_reserved_system_table_privileges` and
`set_worker_unmask_audit_append_privileges` are a separate question: they are about the
`__zeroship_` reserved-table prefix rather than about role provisioning, and their
`live_reserved_sweep_tests` module has **never run** (see 3.6). Wire it first, then decide;
do not delete seven tests that have never been observed.

### 3.6 Live-database coverage: the state before anything moves

Four gated targets are named by **no runner anywhere** - CONFIRMED with
`for t in mask_flip unmask_tx_lane search_tx_lane search_ir_live; do grep -rn -- "$t" tests/ .github/ deploy/ | grep -v sync_claim_gate | wc -l; done`,
which returns 0 0 0 0 against the control `column_grants`, which returns 3. `mask_flip.rs` is
the DB-3 security suite. Separately, `zeroship-data-engine`'s
`#[cfg(all(test, feature = "live-db-tests"))] mod live_reserved_sweep_tests` is unreachable
from CI's only invocation of that package (a bare `cargo test -p zeroship-data-engine`).

**Fix this before touching code.** It is the instrument that has already failed twice in this
exact area, and every later "the counts match" claim rests on it.

### 3.7 Gates: dissolved, rewritten, added

**No gate file is deleted.** `tests/gate_arm_census.sh`'s `GATE_FILE_FLOOR=48` stands
against 49 files, and `tests/ci_wiring_gate.sh` requires every gate to appear in `ci.yml`;
deleting a file would force both to move.

**Dissolved (deleted as vacuous), one item:** `tests/lib/pub_fence_census.sh`'s
`ALSO PUB UNDER test-helpers` column. Its value comes from a grep for `^pub mod` in
`crates/zeroship-plugin-db/src/lib.rs`; once the four own-module ladders collapse it is empty
and the census prints a blank column on every row and exits 0. It is not a gate, so no floor
catches it. Delete the column, not the file. Note also that this census's four named controls
`report()` `MISSING <name>` and **return 0** - so `DbBinding::cold_start` leaving data-core
degrades a control into a printed word unless the census is edited in the same commit.

**Rewritten, with the property preserved:**

| Gate | Edit | Property that survives |
| --- | --- | --- |
| `tests/lib/widening_check.sh` | Key the diff comparison on the declared identifier, skip `^#\[` lines, and additionally catch a **new** `pub fn` and a **private-to-`pub`** promotion - both invisible today because its awk requires a `pub(crate)` on the removal side | "nothing went `pub(crate)` to `pub`" |
| `tests/shipped_config_gate.sh` arm 3 | Generalise from "no member resolves `test-helpers` ON" to a **denylist keyed on a declared dev-only marker** | "no member whose lib ships resolves a dev-only feature ON" - the only thing in the tree refusing the wrong repair of adding a test feature to `default` |
| `tests/private_interface_gate.sh` | Drop `--features test-helpers`, keep `--all-targets`; re-derive both floors from the run | `private_interfaces` / `private_bounds` over the widest resolution |
| `tests/contract_feature_invariance_gate.sh` arm 3 | Retarget at the `backup` split (build each vendor with and without) | "a capability contract must not desynchronise from its impl across a feature split" - arms 1 and 2 are feature-blind and untouched |
| `tests/noop_cfg_pair_gate.sh` | Fix two blind spots (its `is_not()` regex has **no** `any(` alternative - CONFIRMED, `grep -n 'any(' tests/noop_cfg_pair_gate.sh` is empty; its inter-item scan skips blanks but not doc comments), then lower `PAIR_FLOOR` to a re-measured value with the measurement written beside it and a note that the enumeration is correct and the defect class genuinely shrank | "no `cfg`/`cfg(not)` pair declares the identical item twice" |
| `tests/decision_four_gate.sh` | Delete the `test-helpers` alternative from the extractor; re-derive the baseline; repoint three self-test fixtures onto a surviving feature | "no production SQL text in the engine"; strengthened, because the SQL is gone rather than excused |
| `tests/dev_dep_feature_route_gate.sh` | Re-derive the `routes` floor. **CONFIRMED it lands exactly on its floor**: 22 routes today, 10 of them ours, floor 12; `gate_arm` compares `-lt`, so it passes with zero margin and the next feature-table tidy turns it red for an unrelated reason | "no feature routes onto a dev-only dependency" |
| `tests/lib/module_gating.sh`, `tests/lib/tier_direction_census_prod_filter_control.sh` | Repoint synthetic fixture labels onto a surviving feature so they stay controls for something real | the shared "compiled out of every shipped binary" definition |
| `tests/lib/tier_direction_census.sh` | Delete the `test-helpers` alternative in `declared_test_only()` - already dead by enumeration, not by a search returning nothing: all three singly-declared gated modules use the `any(test, ...)` spelling its first alternative matches | tier direction |
| `tests/run_plugin_db_live_suite.sh` | Whole-package run **plus** a built-target arm (below); re-derive `PLUGIN_DB_MIN_PASSED` from a real run | live coverage |
| `tests/vendor_embedding_gate.sh`, `tests/sync_claim_gate.sh`, `tests/worker_replication_privilege_gate.sh` | Re-derive baselines / ledger rows / a structural header sentence in the commit that moves their population | as today |

**Added, one arm, to `tests/test_target_census_gate.sh`:** enumerate every `[[test]]` target
from `cargo metadata` and refuse any that is **absent from the set cargo actually built**
under the live runner's command (`--no-run --message-format=json`). This is not "is the
target named by a runner": a whole-package runner names no target, so that predicate is
vacuously satisfied and would hide exactly the failure a whole-package runner introduces - a
target whose `required-features` become unsatisfiable is skipped silently, and a clean pass
prints identically. This arm is the only instrument in the plan that catches the `backup`
required-features hazard, and it goes red on today's tree, which is how you know it works.

---

## 4. The ordered commits

Sixteen commits. The shape is Design 2's (no test crosses a crate boundary), the ordering is
Design 1's Phase A (fix the instruments and light the dark targets before any code moves) -
which both critics independently identified as the better half of the weaker design - and the
built-target arm in commit 2 is the build-reality critic's repair of my own preferred
ordering, not from either design.

Live-DB coverage is **DARK** through commit 2 and lit from commit 3 onward; every commit says
so.

### Phase A - instruments, before touching the code they watch

**1. `fix(gate): key the widening check on the declared identifier`** - SAFE

Changes `tests/lib/widening_check.sh` from positional `-U0` diff pairing to identifier-keyed
declaration sets, skipping `^#\[` lines, and adds the two cases its awk cannot express today
(a brand-new `pub fn` has no removal line at all; a private-to-`pub` promotion has a `fn`
removal, not a `pub(crate)` one). Same commit: `tests/noop_cfg_pair_gate.sh`'s `is_not()`
gains an `any(` alternative and its inter-item scan skips `//`, `///` and `//!` as well as
blanks, with two new `--self-test` fixtures so `self_test_scenarios` rises with them.

Verified by: `tests/noop_cfg_pair_gate.sh --self-test` (shell only) and
`tests/lib/widening_check.sh --self-test`, plus a mutation - collapse one ladder to `pub` in
a scratch copy and confirm the check **fails**, then restore. A self-test proving only the
pass direction is the failure mode this repo keeps hitting; restore with a fresh mtime.

Live DB: dark (pre-existing).

**2. `test(gate): refuse a test target cargo silently declined to build`** - NEEDS-BUILD

Adds the arm described in 3.7 to `tests/test_target_census_gate.sh`. Verified by running it:
red before commit 3, green after.

Live DB: dark (pre-existing).

**3. `test(db): run every gated db target and the engine's live arms`** - NEEDS-BUILD, live PG

Replaces `tests/run_plugin_db_live_suite.sh`'s name list with
`cargo test -p zeroship-plugin-db --features live-db-tests` (the shape
`crates/zeroship-control/Cargo.toml` documents as having "no second list to keep in step"),
guarded by commit 2's arm so a whole-package run cannot hide a skipped target. Adds
`cargo test -p zeroship-data-engine --features live-db-tests`, recovering
`live_reserved_sweep_tests`. Provisions PostGIS beside pgvector, which makes the suite's
postgis skip allowlist stop matching nothing - handle that deliberately; it is the allowlist
working. Re-derive `PLUGIN_DB_MIN_PASSED` from a real run, never by subtraction; the
script's own header records the last time somebody did the arithmetic and left an
unexplained residual.

Verified by: `tests/run_plugin_db_live_suite.sh` against Postgres carrying pgvector **and**
postgis with `wal_level=logical`, then `tests/test_target_census_gate.sh`.

Live DB: **lit from here.** Four dark targets and seven dark engine tests run for the first
time.

### Phase B - directive 2

**4. `refactor(db)!: delete the pitr replay placeholder and its admin-schema statement`** - NEEDS-BUILD

The deletion list in 2.1, plus the replacement paragraph in the `Backup` trait rustdoc in the
same commit.

Verified by: `cargo check -p zeroship-data-postgres -p zeroship-data-sqlite --features
test-helpers --all-targets`, then `tests/shipped_config_gate.sh` - the `--lib --bins` arm is
the one that catches an ungated caller of a gated impl, and `--all-targets` is blind to it.
Then the live suite, for the one deleted SQLite test.

Live DB: lit.

**5. `docs(db): stop asserting the admin schema exists`** - SAFE

The prose rewrites in 2.1, the AGENTS.md correction in 2.2, and three stale claims that will
otherwise misdirect Phase C: both vendor manifests say the capability traits are
feature-gated in data-core (false since 2026-09-04 - `crates/zeroship-data-core/src/storage.rs`
says so per item), and `crates/zeroship-data-sqlite/src/lib.rs` says the `Backup` trait
carries the gate and lives in `backend/mod.rs`, wrong on both counts.

Verified by: `tests/doc_citation_gate.sh`, `tests/run_doc_gate.sh`.

Live DB: lit.

### Phase C - remove the visibility job

**6. `refactor(db)!: gate the backup impls on a backup feature, not a test flag`** - NEEDS-BUILD

Declares `backup = ["dep:sha2"]` in the two vendors and moves the items listed in 3.1 onto
it; drops `dep:sha2` from `test-helpers`. Retargets
`tests/contract_feature_invariance_gate.sh` arm 3 at the `backup` split **in the same
commit** - its jq selects members forwarding `zeroship-data-core/test-helpers` and would
otherwise hit its explicit `REFUSED: no workspace member forwards ...`.

**This is the one-commit-or-two step.** Ten `Backup` call sites live in
`crates/zeroship-plugin-db/tests/{integration,sqlite_integration}.rs`, and neither source
design forwarded `backup` anywhere, so both would have left those targets failing to resolve
an impl while their stated verifications (`--lib --bins`, and `--all-targets` on two
lib-only crates) stayed green. The intended fix is the one
`tests/dev_dep_feature_route_gate.sh` prescribes in its own header - declare the feature on
the dev-dependency entry (`zeroship-data-postgres = { workspace = true, features = ["backup"] }`
in plugin-db's and data-engine's `[dev-dependencies]`), so plugin-db declares no `backup`
feature and `required-features` never names it. If that does not resolve, plugin-db must
declare and forward `backup`, `required-features` must name it, and this becomes two
commits. Settle it before starting - see section 6.

Verified by: `cargo check --workspace --lib --bins`; `cargo check -p zeroship-data-postgres
--features backup`; `cargo check -p zeroship-data-sqlite --features backup`;
**`cargo check -p zeroship-plugin-db --features test-helpers --all-targets`** (the command
both designs omitted and the only one that builds a target holding a `Backup` call site);
`cargo tree -i sha2 -p zeroship-data-postgres`; `tests/shipped_config_gate.sh`;
`tests/contract_feature_invariance_gate.sh`.

Live DB: lit.

**7. `refactor(db)!: delete the adapter's gated re-export ladder`** - NEEDS-BUILD

Collapses the nine foreign ladders in `crates/zeroship-plugin-db/src/lib.rs` to one
unconditional `pub(crate) use` each and repoints the test imports at the owning crate. Zero
widening for those nine: each target module is already unconditionally `pub` in a
default build (CONFIRMED).

**In the same commit, and this is the piece both designs missed:** six test references
resolve through a **second-level** gated module, and repointing them at
`zeroship_data_engine::...` does not compile in a default build. CONFIRMED by
`grep -rn -oE 'zeroship_plugin_db::(crud|transaction|...)::[a-z_]+' crates/zeroship-plugin-db/tests/*.rs`:
`crud::encryption_pass` x3, `crud::system_fields_pass` x2, `transaction::probe` x1, against
three ladders in `crates/zeroship-data-engine/src/crud/mod.rs` and one in
`transaction/mod.rs`. The answer is neither widening those modules (which scores zero) nor
moving the tests: **promote the three named functions** with a plain `pub use` while the
modules stay `pub(crate)`, and make `transaction::probe` `#[doc(hidden)] pub`. Same commit:
`pg_error::classify_pg_per_app_session_setup_for_tests` becomes an unconditional
`#[doc(hidden)] pub` under its `_for_tests`-free name - it is feature-only today (CONFIRMED),
so `cfg(test)` cannot reach its one consumer in `missing_role.rs`.

Verified by: `cargo check -p zeroship-plugin-db --features test-helpers --all-targets`, then
`tests/lib/widening_check.sh HEAD -- crates` - which only became capable of ruling on this in
commit 1, and is the settling instrument here, not a courtesy.

Live DB: lit.

**8. `fix(db)!: drop the broker global-drain arm and its behaviour fork`** - NEEDS-BUILD

As 3.4. Regression test: an in-crate case asserting two subscriptions on two threads are
counted separately and that `Broker::drain_all()` takes both - it fails before this commit
because the production arm counts process-wide.

Verified by: `cargo test -p zeroship-data-core`;
`cargo test -p zeroship-plugin-db --test subscription_finalizer`;
`cargo check -p zeroship-data-engine --all-targets` for the narrowed `drop_app` signature;
and the mutation that restores the fork and must turn the new test red.

Live DB: lit.

**9. `refactor(sqlite): make the next-command gate a first-class seam`** - NEEDS-BUILD

As 3.4, promoting three items (the method, `NextCommandGate`, `NextCommandGate::release`).
Regression test: one asserting the unarmed path takes no lock - poison the mutex and assert
an unarmed command still runs.

Verified by: `cargo test -p zeroship-data-sqlite`; `cargo test -p zeroship-data-engine`; and
the sqlite integration target compared by **group count and test-name set**, never by pass
tally (`--list` before and after).

Live DB: lit. This is the step most able to change behaviour silently, because it alters the
actor's hot loop.

**10. `refactor(db): arm the write-path counters instead of forking them`** - NEEDS-BUILD

As 3.4: unconditional counters behind a relaxed armed atomic, `#[doc(hidden)] pub`.
Regression test: an in-crate case arming the counters and asserting target-row resolution and
the upsert conflict probe are each hit exactly once - it fails before this commit in a
non-feature build because the notes compile to `{}`.

Verified by: `cargo test -p zeroship-data-engine --features live-db-tests` (the bare `--lib`
run is feature-blind here), plus the mutation that empties one `note_*` call and must turn a
counter assertion red.

Live DB: lit.

**11. `refactor(db): move the cold-start binding fixture to a dev-only crate`** - NEEDS-BUILD

Creates `crates/zeroship-data-testkit` and moves `DbBinding::cold_start` verbatim as
`cold_start_binding`. Adds it to `[dev-dependencies]` of `zeroship-plugin-db` **and**
`zeroship-data-engine`. **Ordering matters and one source design got it wrong:**
`cold_start` is called today from three LIB items behind a feature -
`zeroship-data-engine`'s `cache_schema_for_tests` and plugin-db's
`prepare_insert_many_docs_for_tests` and `finalize_rows_on_read_for_tests` - and a
dev-dependency cannot serve feature-gated lib code (the proof is in
`crates/zeroship-data-engine/src/lib.rs`'s note about `tracing-subscriber`). So either this
commit follows commit 15, or those three items lose their feature gate here. Take the second:
this commit makes all three unconditional `#[doc(hidden)] pub` and reads the testkit from
their **callers**, which are tests.

Verified by: `cargo check -p zeroship-data-core --all-targets` (the fixture must be gone);
`cargo check -p zeroship-plugin-db --features test-helpers --all-targets`;
`cargo check -p zeroship-data-engine --features test-helpers --all-targets`;
`tests/clippy_gate.sh --preflight-only` and `tests/gate_arm_census.sh tests`, because a new
workspace member changes the clippy gate's package expectation and its declared-feature arm;
and `tests/lib/pub_fence_census.sh`, whose `DbBinding::cold_start` control must be repointed
or it prints `MISSING` and exits 0.

Live DB: lit.

**12. `refactor(db)!: delete the engine's duplicate per-app role provisioner`** - NEEDS-BUILD, live PG

As 3.5, including the inlined `DROP ROLE` in `crates/zeroship-plugin-db/src/drop_namespace.rs`
and the 41 fixture call sites repointed at `zeroship-migrate-server`. Must precede commit 16.

Verified by: `tests/decision_four_gate.sh --self-test` then `tests/decision_four_gate.sh` -
the baseline edit and the deletion must land together or its `baseline_rows` arm refuses a
row describing a dead site; `cargo check -p zeroship-data-engine --all-targets` **and**
`cargo check -p zeroship-plugin-db --features test-helpers --all-targets` (the second is the
one that sees all 41 breakages; the design this comes from named only the first);
`tests/run_plugin_db_live_suite.sh`.

Live DB: lit, and load-bearing - the fixtures now provision through the real code path.

**13. `refactor(db): delete the dead gated symbols`** - NEEDS-BUILD

`PgSqlExecutor`, its impl, both `assert_impl` witnesses and the `use` lines feeding it;
`strip_encryption_markers` made private; the stale `#[cfg]` on the sqlite
`SchemaIntrospect` witness activation.

Verified by: `cargo check -p zeroship-data-postgres --features test-helpers --all-targets`;
`tests/shipped_config_gate.sh`.

Live DB: lit.

**14. `test(db): move the v8 class suites in-crate and narrow the mints`** - NEEDS-BUILD

As 3.3, including deleting the three redundant `cold_*` tests and repointing the header
sentence at `v8_classes/cold_open.rs`.

Verified by: `cargo test -p zeroship-plugin-db --lib`;
`tests/private_interface_gate.sh`, because narrowing `mint_db` and `mint_subscription`
changes its `fenced_pub_items` population; `tests/lib/widening_check.sh`.

Live DB: lit.

**15. `refactor(db): settle the last gated symbols`** - NEEDS-BUILD

The residue, one symbol at a time, three outcomes only. Move to the testkit what is pure
composition over public API (the pending-emit trio, `supply_root_keys_for_tests` and its
guard, `install/uninstall_tx_marker_for_tests`, the exec/query wrappers) - **but read each
body first**: any that touches `crate::context::` or `crate::tx_scope::` cannot move, and if
it can move, the testkit must not acquire a dependency on `zeroship-plugin-db`, which would
drag V8 into data-core's and data-engine's test builds. Take `reset_engine_for_tests` to
`#[cfg(test)]` (27 sites, all inside `crates/zeroship-data-engine/src`, zero cross-crate
consumers). Take the four process-global setters in `crates/zeroship-plugin-db/src/lib.rs` to
unconditional `#[doc(hidden)] pub` - they are the honest residue, not good API, and section 7
says what dissolves them. Re-gate `pub mod drop_namespace` from `test-helpers` to
`live-db-tests`, preserving the "no `DROP SCHEMA` path in any shipped binary" property that
`cfg(test)` would have destroyed by making its only exerciser unreachable. Collapse the four
own-module ladders to unconditional `pub(crate) mod`, keeping
`tests/private_interface_gate.sh`'s adapter-half population intact.

Verified by: `cargo check -p zeroship-plugin-db --all-targets`;
`tests/lib/widening_check.sh HEAD~1`; `tests/lib/pub_fence_census.sh` (read its output - the
`ALSO PUB UNDER test-helpers` column goes vacuous here, which is commit 16's deletion).

Live DB: lit.

**16. `refactor(db)!: delete the test-helpers feature`** - NEEDS-BUILD

The five manifest declarations, the nine forwarding routes, and **every** gate edit in 3.7,
all in this commit. `live-db-tests` is redeclared `= []` in `zeroship-plugin-db` and
`zeroship-data-engine`. Floors that must be **re-derived from a run and not by arithmetic**,
with the measurement written beside each: `private_interface_gate.sh`'s `compiled_units` and
`fenced_pub_items` (the latter will drop); `noop_cfg_pair_gate.sh`'s `PAIR_FLOOR` (its
scanner is item-boundary-based and stricter than any window heuristic - take the number from
the gate); `dev_dep_feature_route_gate.sh`'s `routes` (CONFIRMED it lands on exactly 12
against a floor of 12); `shipped_config_gate.sh`'s generalised arm 3;
`vendor_embedding_gate.sh`'s baseline; `run_plugin_db_live_suite.sh`'s
`PLUGIN_DB_MIN_PASSED`.

Verified by: `./tests/clippy_gate.sh` (needs `pnpm build` and `setup-wpt.sh` first, and
refuses rather than linting a smaller workspace); `tests/shipped_config_gate.sh`;
`tests/private_interface_gate.sh`; `tests/contract_feature_invariance_gate.sh`;
`tests/noop_cfg_pair_gate.sh --self-test` then a plain run;
`tests/decision_four_gate.sh --self-test` then a plain run;
`tests/dev_dep_feature_route_gate.sh`; `tests/vendor_embedding_gate.sh`;
`tests/worker_replication_privilege_gate.sh`; `tests/sync_claim_gate.sh`;
`tests/test_target_census_gate.sh`; `tests/gate_arm_census.sh tests --run <each changed gate>`;
`tests/ci_wiring_gate.sh`; `cargo test --workspace --no-fail-fast`; and
`tests/run_plugin_db_live_suite.sh`.

Live DB: lit.

---

## 5. What the critics caught

Ten things that would have been silently lost, and how the plan preserves each. This is the
section that justifies the shape.

**1. The ladder census was one level too shallow.** Both designs enumerated the nine
top-level ladders in `crates/zeroship-plugin-db/src/lib.rs` and concluded "zero widening,
every target is already `pub`". True of the nine; false of the six test references that go
through `crud::encryption_pass`, `crud::system_fields_pass` and `transaction::probe`, which
are `pub(crate)` in a default build (CONFIRMED). Neither design had a step for them, and the
two exits are widening three engine modules permanently or moving tests. **Preserved by**
commit 7 promoting three named *functions* and one probe module instead - a bounded, argued
widening rather than an accidental one.

**2. `cfg(test)` never fires for a consumer, and one design walked into it three times.** Its
commit 17 collapsed the write-path counters to `cfg(test)` on the strength of having moved
their consumers into `zeroship-data-engine/tests/` - an integration target, a separate crate.
Twelve call sites either break or silently observe a permanently-zero counter. Its commit 11
did the same to `classify_pg_per_app_session_setup_for_tests`, which is feature-only
(CONFIRMED). Its `ensure_per_app_role` disposition ("must go to `#[cfg(test)]`") is dead for
41 sites. **Preserved by** moving no test across a crate boundary at all, and by every
residue terminating at an unconditional item (commits 7, 9, 10, 15) or being deleted
(commit 12).

**3. One design's whole relocation phase cannot compile.** `crates/zeroship-data-engine/Cargo.toml`
states in its own header that a dev-dependency cycle back onto `zeroship-plugin-db` would
link two copies of the engine into the test binary, giving every `thread_local!` there two
independent instances - and the engine has a dozen of them, all fixture state the moved
suites reset between phases. Six moved files all name the adapter, plus `rusqlite`,
`zeroship-runtime`, `zeroship-test-support`, `zeroship-plugin-workflow`,
`zeroship-migrate-{sqlite,postgres}` - none of which the engine's dev-dependencies carry.
The silent-failure direction is the bad one: `reset_context_for_tests` clears copy A's cache
while the code under test reads copy B's, and "the cache was cleared" passes over stale
state. **Preserved by** not attempting it.

**4. Promoting `DbBinding::cold_start` would ship a panicking identity constructor.** Its own
rustdoc states the property the gate exists for and, immediately below, that it panics if the
app id is not a legal schema name - while production mints through `SchemaName::new` and
refuses. **Preserved by** commit 11 moving it out to a dev-only crate, which is strictly
stronger than the gate: a feature-resolution property becomes a dependency-graph fact.

**5. The `backup` split breaks ten call sites, invisibly to both designs' verifications.**
`Backup as _` plus `.snapshot(` / `.restore(` / `.pitr_replay(` appear in
`crates/zeroship-plugin-db/tests/{integration,sqlite_integration}.rs`. The trait is ungated
in data-core, so it resolves; the impl would not exist. One design verified with
`--lib --bins` and `cargo tree`; the other added `--all-targets` on two crates `cargo
metadata` reports as **lib-only**, a no-op. **Preserved by** commit 6 naming
`cargo check -p zeroship-plugin-db --features test-helpers --all-targets` as its settling
command, and by commit 2's arm catching an unsatisfiable `required-features` target if the
forwarding route is taken instead.

**6. The proposed replacement for `shipped_config_gate.sh` arm 3 is unpassable.** Both
designs generalised it to "refuse any declared non-default feature of a shipped member that
resolved ON". CONFIRMED that normal, non-dev dependency edges legitimately enable several:
`zeroship-cli` and `zeroship-worker` both enable `zeroship-plugin-storage/s3`, and six
workspace members enable `compio-postgres` codec features. Both are workspace members whose
libs are in the shipped build. The arm goes red on day one for reasons that are right, and
the predictable repair is an allowlist - which is exactly the mechanism arm 3 exists to
refuse. **Preserved by** phrasing the arm as a **denylist keyed on a declared dev-only
marker**, in commit 16.

**7. `tests/dev_dep_feature_route_gate.sh` lands exactly on its floor.** CONFIRMED: 22
`dep/feat` routes today, ten of them `test-helpers`/`live-db-tests`, floor 12. `gate_arm`
compares `-lt`, so 12 against 12 passes with zero margin, and the property the floor exists
for - stated in its own header, that a renamed metadata field or a broken regex flags nothing
and prints what a clean tree prints - is gone. Neither design named this gate at all.
**Preserved by** re-deriving the floor in commit 16.

**8. Five instruments go quiet over an empty set rather than refusing.**
`tests/lib/pub_fence_census.sh`'s `ALSO PUB UNDER test-helpers` column and its four
`MISSING`-but-exit-0 controls; three `--self-test` fixtures in `decision_four_gate.sh`; the
`test-helpers` case in `tests/lib/module_gating.sh` (consumed by four instruments); two
fixture lines in `tests/lib/tier_direction_census_prod_filter_control.sh`; and
`tests/lib/widening_check.sh`, which is blind to a ladder collapse in **both** directions and
is precisely the instrument an operator would reach for to check they had not committed the
obvious wrong answer. **Preserved by** commit 1 fixing widening_check first, and commit 16
deleting the vacuous column and repointing every fixture onto a surviving feature.

**9. Four gated targets and one gated engine module are dark right now.** CONFIRMED: zero
runner references to `mask_flip`, `unmask_tx_lane`, `search_tx_lane`, `search_ir_live`
against a control that returns three for `column_grants`; and CI's only invocation of
`zeroship-data-engine` passes no features. `mask_flip.rs` is the DB-3 security suite, whose
own manifest comment says it fails rather than skips because "a skipping run of a security
suite is indistinguishable from a passing one" - it fails loudly in a job that does not
exist. One design deferred this to "a runner problem"; the other fixed it but then replaced
the name list with a whole-package run, which is the exact command shape under which a
newly-unsatisfiable `required-features` target vanishes with no change to the output.
**Preserved by** commit 3 doing the wiring and commit 2 adding a built-target arm first, so
the whole-package run cannot hide a skip.

**10. Two deletions that look identical and are not.** `drop_per_app_role` has a real caller
in `crates/zeroship-plugin-db/src/drop_namespace.rs` that one design's step never mentions
(CONFIRMED); `live_reserved_sweep_tests` sits directly below the block that step deletes and
has never run once. And on the other side, `Broker`'s global drain has no caller but has a
rustdoc naming worker shutdown as one - a no-caller proof is strong evidence about a leaf
accessor and weak evidence about the only correct implementation of an operation the system
will need, where its absence pushes the next author toward a racy per-app loop over a
process-wide `Mutex<Broker>`. **Preserved by** commit 12 inlining the `DROP ROLE`, commit 3
lighting the sweep tests before anyone rules on them, and commit 8 keeping
`Broker::drain_all()` ungated and documented with no caller.

---

## 6. What must be settled by a build before starting

Run these first, in this order. Each is cheap next to the commit it unblocks.

```sh
# 0. Can this machine lint and build at all.
pnpm build && ./crates/zeroship-runtime/tests/setup-wpt.sh
./tests/clippy_gate.sh --preflight-only

# 1. THE dep/feat QUESTION, and it decides whether commit 6 is one commit or two.
#    Add `backup = ["dep:sha2"]` to both vendors, move the impls onto it, and add
#    `features = ["backup"]` to the plugin-db and data-engine [dev-dependencies]
#    entries for those two crates - and to NOTHING else. Then:
cargo check -p zeroship-plugin-db --features test-helpers --all-targets
cargo tree -p zeroship-data-postgres -e features --all-targets | grep -n sha2
#    GREEN + sha2 present  -> a dep/feat feature resolves through a [dev-dependencies]
#                             entry alone; plugin-db declares no `backup` feature,
#                             `required-features` never names it, ONE commit.
#    RED / sha2 absent     -> plugin-db must declare and forward `backup`, the nine
#                             [[test]] blocks must name it in required-features, and
#                             the generalised shipped_config arm must tolerate a
#                             dev-only-marked feature. TWO commits, and commit 2's
#                             built-target arm becomes mandatory rather than prudent.
#    Note: tests/dev_dep_feature_route_gate.sh's header records that routing a
#    `dep/feat` entry THROUGH a [features] table onto a dev-only dependency is
#    broken in cargo 1.94 (feature name propagates, optional-dep activation does
#    not). Declaring on the dev-dep entry is that gate's own prescribed fix, which
#    is why it is the shape to try first - but it has not been exercised for
#    `dep:sha2` here.

# 2. Does a [[test]] block whose required-features names an undeclared feature
#    error, or silently never build? Decides whether commit 2's arm is the only
#    thing standing between you and a silent skip.
cargo metadata --no-deps --format-version 1 \
  | jq '.packages[]|select(.name=="zeroship-plugin-db")|.targets[]|select(.kind[0]=="test")'
cargo test -p zeroship-plugin-db --features live-db-tests -- --list | tail -20

# 3. Does dropping data-engine's `live-db-tests` declaration warn or deny.
./tests/clippy_gate.sh

# 4. Baseline every floor this plan will move, BEFORE moving it. Record the
#    zsgate-arm lines, do not derive them later by subtraction.
./tests/private_interface_gate.sh
./tests/noop_cfg_pair_gate.sh
./tests/dev_dep_feature_route_gate.sh
./tests/shipped_config_gate.sh
./tests/decision_four_gate.sh
./tests/gate_arm_census.sh tests

# 5. Does the live suite pass today, on a server carrying BOTH extensions, and what
#    is the real PLUGIN_DB_MIN_PASSED. Everything after commit 3 is measured
#    against this run.
#    (Postgres with pgvector AND postgis, wal_level=logical.)
./tests/run_plugin_db_live_suite.sh
cargo test -p zeroship-data-engine --features live-db-tests

# 6. Read, do not grep: the eight fixture bodies commit 15 proposes to move, for
#    `crate::context::` / `crate::tx_scope::`. If any touches adapter state it
#    cannot move, and if the testkit would need to depend on zeroship-plugin-db,
#    the move is off - that edge drags V8 into data-core's test build.

# 7. Does zeroship-migrate-server expose a create-if-missing, idempotent role
#    provisioner with ensure_per_app_role's contract? Decides whether commit 12
#    is a bounded edit or needs a fixture wrapper in the testkit.
grep -n 'pub .*fn ' crates/zeroship-migrate-server/src/{apply,provisioning}.rs \
  | grep -iE 'role|provision'
```

---

## 7. What this does not fix, and how it meets the crate split

### 7.1 Not fixed

- **The process-global statics are still there.** Commit 15 leaves four
  `#[doc(hidden)] pub` setters in `crates/zeroship-plugin-db/src/lib.rs` and a `reset_*`
  family in the engine, because the broker, the tx lanes, the schema cache, the mask-policy
  cache and the metrics counters are process-wide singletons and test isolation has to be
  bought somewhere. The plan removes the *fork* (the two builds become identical) but not the
  *singleton*. The real fix is a handle the isolate owns, which is a design change of its
  own size.
- **No cross-dialect conformance suite exists.** `integration.rs` and `sqlite_integration.rs`
  hold paired assertions about the same behaviour on two dialects, kept together only by
  living in two files one author edits. The right eventual home is a suite parameterised over
  `BackendHandle`'s variants, so a new backend is billed for every arm by construction. This
  plan does not build it; it also does not disperse the pairs, which is why nothing here
  moves a test.
- **Four data crates still have zero test targets.** `zeroship-data-{core,engine,postgres,sqlite}`
  keep their oracle in the adapter. `zeroship-data-query-builder` - ten test targets, no
  feature - remains the anomaly rather than becoming the pattern.
- **DB-3's audit gap.** `sanitize_app_actor` strips the whole actor including the id, so a
  denied audit row is byte-identical to routine anonymous traffic. The strip is right; the
  missing piece is auditing the rejected claim in its own column. Untouched here.
- **`snapshot` / `restore` still have no consumer.** Commit 6 renames their gate honestly and
  deliberately makes no ship-or-delete decision. That decision stays open and should be taken
  on its own evidence, not as a side effect of a flag removal.

### 7.2 The crate split, and this is your call to make

You have said separately that you dislike the current split and naming and that boundaries
are in the wrong places. Some of these commits are indifferent to a re-cut and some are keyed
to today's boundaries. I am framing it, not deciding it.

**Robust to a reshape - do these regardless of when the crates move:**

- Commits 1, 2, 3 (instruments and dark coverage). They rule on `cargo metadata` and on
  runner wiring; a re-cut changes the package names in their output, not the properties.
  Commit 3 in particular is worth more *before* a reshape than after, because a reshape you
  cannot measure is a reshape you cannot verify.
- Commits 4 and 5 (directive 2). The PITR deletion and the prose corrections are about a
  schema and a capability, not about a tier.
- Commit 6 (`backup`). It names a capability, not a crate. If the vendors merge or split, the
  feature moves with the impls.
- Commits 8, 9, 10 (the three behaviour forks). A fork between the shipped build and the
  tested build is wrong wherever the code lands, and each fix is local to one file.
- Commit 12 (delete the duplicate provisioner) and 13 (dead symbols). Deleting something that
  should not exist is never invalidated by moving what remains.

**Wasted if the crates are re-cut first:**

- Commit 7. Every collapsed ladder and every repointed import names a crate by today's name
  and today's contents. If `zeroship-data-engine` and `zeroship-plugin-db` are re-cut, the
  nine ladders and the six second-level references are re-derived from scratch, and the three
  function promotions may not be needed at all if the modules end up in the same crate as
  their tests. This is the single largest wasted-motion risk in the plan.
- Commits 11 and 15 (the testkit and the fixture split). The testkit's whole value is which
  crates may and may not see it; those edges are drawn against today's tiers, and a re-cut
  redraws them. If `zeroship-data-core` merges into the engine, `cold_start_binding`'s
  dependency-graph guarantee changes shape.
- Commit 14 (the v8 suites in-crate). It assumes `zeroship-plugin-db` keeps the V8 classes.
- Commit 16's floor re-derivations. Every one of them counts items per crate, and every one
  is re-measured after a re-cut anyway.

**The two orderings, stated plainly so you can pick:**

*Flag first.* Commits 1-16 as written, then reshape. You get the dark coverage lit and the
behaviour forks gone before anything moves, which means the reshape has a working oracle to
land against - and the reshape is the change most in need of one. Cost: commits 7, 11, 14, 15
and half of 16 are redone.

*Reshape first.* Do commits 1-6, 8-10, 12-13 (the reshape-robust set, which includes both
directive-2 commits and all three behaviour forks), stop, re-cut the crates, then take the
placement work (7, 11, 14, 15, 16) against the new boundaries in one pass. Cost:
`test-helpers` survives through the reshape, and it is the reshape's own verification vehicle
- `docs/proposals/2026-08-31-data-crate-shape.md` prescribes
`cargo check -p <crate> --features test-helpers --all-targets` as step one of verifying every
move. Keeping it is not free but it is not absurd either; it is the tool the job uses.

I lean toward *reshape first* on the strength of that last point - the flag's remaining honest
job is verifying moves, and there are moves coming - but the split is yours and the argument
turns on how soon the re-cut actually happens. If it is more than a few weeks out, do the
flag first; a feature whose stated justification is "a migration is in progress" stops being
true the moment the migration stalls.
