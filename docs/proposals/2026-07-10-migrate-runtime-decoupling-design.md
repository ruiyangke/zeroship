# V8 decoupling of the `zeroship-migrate` core — design

**Status:** proposed 2026-07-10. Structural refactor of `crates/zeroship-migrate` (branch `refactor/migrate-decouple-core`). No engine-logic change; no wire/IR change; commit-only.

**Goal:** two-part, and both parts ship in this milestone (the first without the second delivers no V8 removal — see the callout below):

1. **Make the V8-free core compilable.** `cargo build -p zeroship-migrate --no-default-features` compiles a **V8-free core** — guard + IR + render + PG/SQLite apply + journal + executor + `.ir.json` load/sealed-apply — with `zeroship-runtime`, `v8`, `seccompiler`, `landlock` **absent from the dependency tree**.
2. **Make a real consumer use it.** Flip the two production library dependents — `plugin-db` and `migrated` — to `zeroship-migrate = { workspace = true, default-features = false }` so that V8 (`v8` + `zeroship-runtime`) actually leaves *their* resolved dependency trees, and therefore leaves the platform build graph.

The **default build (`default = ["js-cli"]`) is byte-for-byte behaviorally unchanged**: it still builds the JS authoring front-end, the recorder child, and the live-MySQL backend.

> <!-- Added in round 2: addressing BLOCKER #1 (V8-free core unreachable) + BLOCKER-verification #9 -->
> **Why step 2 is not optional.** With the workspace resolver v3, feature unification means a normal `cargo build` / `cargo test` unifies the feature sets of every path to `zeroship-migrate`. Both `plugin-db` (`crates/plugin-db/Cargo.toml:34`, `zeroship-migrate = { workspace = true }`) and `migrated` (`crates/migrated/Cargo.toml:27`, same) currently depend on it *with default features on*. Since `default = ["js-cli"] ⊇ zsv8`, any ordinary workspace build unifies `zsv8` **on** for the whole graph — V8 is pulled back into `zeroship-migrate` for the platform binaries regardless of the `--no-default-features` invocation existing. The isolated `cargo build -p zeroship-migrate --no-default-features` proves the *core compiles* V8-free, but the product never runs it, so on its own it removes zero V8 from the platform. The removal only becomes real once `plugin-db` and `migrated` opt out of default features (they consume only the V8-free surface per §7, so this is safe). This design commits to both.

---

## 1. Objective + non-goals

**Objective.** Make the in-Rust V8 host an *optional* subsystem of `zeroship-migrate`, gated behind a single feature. V8 (`zeroship-runtime` + `v8`) is coupled in exactly two **library** subsystems — the JS/TS schema-authoring front-end (`src/frontend/`) and the live-MySQL backend (`src/apply/backend/mysql/`, which drives `mysql2` in a V8 isolate over `node:net`) — plus **three V8-touching bins**: `zeroship-migrate-js`, `zeroship-migrate` (standalone, whose runner can reach MySQL), and `zeroship-migrate-recorder-child` (`src/bin/recorder-child.rs`, itself 12 `v8::`/`zeroship_runtime` hits + 41 `libc::` hits, grep-verified). Everything else is already V8-free. This milestone converts that latent separation into a compiled one.

**Non-goals (explicitly out of scope for this milestone):**
- **No engine-logic changes.** Guard (`pg_query` deny-list), model/IR, render (SQL gen), analysis, journal, executor, PG apply, SQLite apply — untouched. Zero behavior delta.
- **compio STAYS.** `compio-postgres` + `rusqlite` remain in the core; the core owns its DB I/O (Temporal-style). The one `compio::runtime::Runtime` in `sqlite/dump_sql.rs` is the *event-loop* runtime, not the V8 `zeroship_runtime::Runtime` — it stays.
- **No wire-format / IR-version change.** No `CURRENT_IR_VERSION` bump; `.zship`/`.ir.json` bytes unchanged.
- **No new mechanism / no redesign of the `MigrationBackend` seam.** The seam is already the right cut line (see §6.4); we gate along it, we do not rebuild it.
- **No Node/napi shell, no compio DB-I/O seam** — those are FUTURE WORK (§8), positioned but not built here.

---

## 2. Feature model

One new feature, `zsv8`, gates the entire in-Rust V8 host. It is the *sole* switch that pulls the four V8-adjacent crates.

```toml
[features]
default = ["js-cli"]

# The in-Rust V8 host: JS/TS schema authoring front-end + live-MySQL backend
# (mysql2 in a V8 isolate over node:net) + the kernel-sandboxed recorder child.
# This is the ONLY feature that pulls zeroship-runtime / v8 into the tree.
zsv8 = ["dep:v8", "dep:zeroship-runtime", "dep:seccompiler", "dep:landlock", "dep:libc"]

# The JS authoring CLI bin. Authoring needs the V8 host, so js-cli ⊇ zsv8.
js-cli = ["dep:clap", "zsv8"]

# The operator standalone apply/runner CLI. Its live path can reach the MySQL
# backend, so it too rides zsv8.
standalone-cli = ["dep:clap", "zsv8"]
```

Dependency declarations flip to optional:

```toml
zeroship-runtime = { workspace = true, optional = true }
v8               = { workspace = true, optional = true }

[target.'cfg(target_os = "linux")'.dependencies]
seccompiler = { workspace = true, optional = true }
landlock    = { workspace = true, optional = true }
libc        = { workspace = true, optional = true }
```

<!-- Added in round 2: addressing MAJOR #5 (seccompiler/landlock footprint mis-stated) -->
**Verified footprint of the folded Linux deps** (`grep -rln` over `src/`):
- `seccompiler` — used **only** in `frontend/sandbox.rs`.
- `landlock` — used across `frontend/{sandbox,record,build,recorder_protocol,recorder_service}.rs` **and** `bin/recorder-child.rs`.
- `libc` — used in `frontend/{sandbox,recorder_service}.rs` **and** `bin/recorder-child.rs` (41 hits).

Every one of those sites is already `zsv8`-gated: the `frontend/` files transitively (whole directory gated, §4) and `bin/recorder-child.rs` via `required-features = ["zsv8"]` (§3E). No non-frontend, non-bin module references any of the three (grep-confirmed clean). Folding all three into `dep:` of `zsv8` is therefore sound.

<!-- Added in round 2: addressing MINOR #8 (libc left unconditional with no in-core users) -->
**`libc` is made optional and folded into `zsv8`**, not left unconditional. Its only users are the V8-gated sandbox / recorder-child paths above, so leaving it unconditional would ship a leaf dependency with **zero in-core consumers** — inconsistent with this design's own lean-tree principle (the same principle that motivates relocating `js_driver_module_graph` in §4). Making it optional keeps the V8-free tree free of dead deps and makes the `cargo tree` absence assertion (§10) complete.

**Consequences of the model:**
- `default = ["js-cli"]` ⇒ `zsv8` on ⇒ **today's full behavior preserved exactly**. The platform's `zeroship-migrate-js` binary still builds.
- `--no-default-features` ⇒ `zsv8` off ⇒ **V8-free core**: no `v8`, no `zeroship-runtime`, no `seccompiler`, no `landlock` in the tree; no `frontend/`, no MySQL backend, no recorder-child bin.
- <!-- Added in round 2: addressing BLOCKER #1 --> The two real library dependents (`plugin-db`, `migrated`) consume only the V8-free surface (§7). **This milestone flips both to `default-features = false`** (§7.1) — that is the step that actually removes V8 from the platform build graph; leaving them on default features would keep `zsv8` unified on for the whole workspace and remove nothing. The dev-dep-only `schema-authority-e2e` keeps default features on because it genuinely imports `frontend::*`.

**Why a single `zsv8` and not two features (one per subsystem).** The two subsystems are *not* independent: `apply/backend/mysql/transport.rs` imports `crate::frontend::embedding::js_driver_module_graph` — the MySQL backend structurally depends on the frontend `embedding` module. A `mysql`-without-`frontend` build is not a real configuration; splitting the feature would invite a broken combination. One host, one gate. (We do relocate that shared helper — §4 — so the *code* dependency is clean even though the *feature* is unified.)

---

## 3. The exact cfg-gate map

Every site below carries `#[cfg(feature = "zsv8")]` unless noted. Line numbers are from the coupling map; treat as anchors, not literals.

### A. Module declarations
| File:line | Change |
|---|---|
| `src/lib.rs:76` — `pub mod frontend;` | gate `#[cfg(feature = "zsv8")]` |
| `src/apply/backend/mod.rs:40` — `pub mod mysql;` | gate `#[cfg(feature = "zsv8")]` |
| `src/apply/backend/mod.rs:41,42` — `pub mod postgres; pub mod sqlite;` | **unchanged** (V8-free) |

### B. `lib.rs` + `backend/mod.rs` re-exports (split MySQL names out of the shared blocks)

<!-- Revised in round 3: addressing MAJOR #1 — the split boundary is now an explicit KEEP/MOVE checklist per block, and the earlier line ranges were corrected against the actual source. -->
The one block that genuinely **interleaves** V8-free and MySQL names is the crate-root re-export in `lib.rs` (verified `src/lib.rs:114–124`). `backend/mod.rs` is *already* cleanly partitioned — its V8-free names come from a `pub use capability::{…}` block and inline trait definitions, and its MySQL names sit in a *separate* `pub use mysql::{…}` block — so mod.rs only needs the mysql block re-gated, no interleaving to untangle. The verdict below is a **literal keep-list / move-list per block** so the split is mechanical, not judgment.

**Verified source anchors (grep-confirmed, not the coupling-map estimates):**
- `src/lib.rs:113–114` — `#[cfg(test)] pub use apply::backend::{MysqlFragmentDecision, MysqlFragmentEvent, MysqlFragmentHookAction};`
- `src/lib.rs:114–124` — the single interleaved `pub use apply::backend::{ … };` block.
- `src/apply/backend/mod.rs:43–46` — `pub use capability::{ BackfillError, BackfillOutcome, BackfillSpec, DryRunError, DryRunReport, MigrationResult, OnlineSchemaChange, SeedError, ShadowConfig, ShadowDryRun };` — **already V8-free-only; unchanged.**
- `src/apply/backend/mod.rs:47` — `pub use postgres::PostgresBackend;` — **already V8-free-only; unchanged.**
- `src/apply/backend/mod.rs:48–49` — `#[cfg(test)] pub use mysql::{MysqlFragmentDecision, MysqlFragmentEvent, MysqlFragmentHookAction};`
- `src/apply/backend/mod.rs:50–55` — the `pub use mysql::{ … };` block — **already MySQL-only.**
- `MigrationBackend`, `CrossDeployObligations`, `PgSessionSnapshot` are **defined inline** in `backend/mod.rs` (`mod.rs:78`, `:97`, `:166`), not re-exported — so they are V8-free by construction and need no gate.

#### B.1 — `src/lib.rs:114–124` interleaved block (the only interleaving to untangle)
Split into two `pub use apply::backend::{…}` blocks. Every symbol below is accounted for exactly once.

| KEEP UNGATED (V8-free — consumed by `plugin-db`/`migrated` per §7) | MOVE to `#[cfg(feature = "zsv8")] pub use apply::backend::{…}` (MySQL) |
|---|---|
| `BackfillError` | `JsDriverConn` |
| `BackfillOutcome` | `JsDriverError` |
| `CrossDeployObligations` | `MysqlBackend` |
| `DryRunError` | `MysqlGuardedFragment` |
| `DryRunReport` | `MysqlMigratorAccount` |
| `MigrationBackend` | `MysqlMigratorAccountError` |
| `MigrationResult` | `MysqlSessionSnapshot` |
| `OnlineSchemaChange` | `RowSet` |
| `PgSessionSnapshot` | `deprovision_mysql_migrator_account` |
| `PostgresBackend` | `mysql_migration_lock_name` |
| `SeedError` | `provision_mysql_migrator_account` |
| `ShadowConfig` | `provision_mysql_migrator_account_with_password` |
| `ShadowDryRun` | |

The KEEP set is the exact §7 dependent surface (`MigrationBackend`, `PostgresBackend`, `ShadowConfig`, …) — mechanically moving the whole block behind `zsv8` (the risk-register row-4 over-gating regression) would delete these from the V8-free core and break `plugin-db`/`migrated`. This table forbids that.

#### B.2 — `src/lib.rs:113–114` (`#[cfg(test)]` MysqlFragment* re-export)
Re-gate `#[cfg(test)]` → `#[cfg(all(test, feature = "zsv8"))]`. All three (`MysqlFragmentDecision`, `MysqlFragmentEvent`, `MysqlFragmentHookAction`) are MySQL; whole-block gate.

#### B.3 — `src/apply/backend/mod.rs` (already partitioned — gate the mysql blocks only)

| Block (mod.rs) | Verdict |
|---|---|
| `:43–46` `pub use capability::{BackfillError, BackfillOutcome, BackfillSpec, DryRunError, DryRunReport, MigrationResult, OnlineSchemaChange, SeedError, ShadowConfig, ShadowDryRun}` | **KEEP UNGATED** — V8-free-only, no change |
| `:47` `pub use postgres::PostgresBackend` | **KEEP UNGATED** — V8-free-only, no change |
| `:48–49` `#[cfg(test)] pub use mysql::{MysqlFragmentDecision, MysqlFragmentEvent, MysqlFragmentHookAction}` | re-gate → `#[cfg(all(test, feature = "zsv8"))]` |
| `:50–55` `pub use mysql::{deprovision_mysql_migrator_account, mysql_migration_lock_name, provision_mysql_migrator_account, provision_mysql_migrator_account_with_password, JsDriverConn, JsDriverError, MysqlBackend, MysqlGuardedFragment, MysqlMigratorAccount, MysqlMigratorAccountError, MysqlSessionSnapshot, RowSet}` | gate the whole block `#[cfg(feature = "zsv8")]` (already MySQL-only — no interleaving) |
| inline defs `MigrationBackend` (`:166`), `CrossDeployObligations` (`:97`), `PgSessionSnapshot` (`:78`) | **KEEP UNGATED** — inline items, V8-free |

<!-- Added in round 2: addressing MINOR #6 (compile_fail doctest vs new feature lattice) -->
**Re-examine the existing `compile_fail` doctests against the new lattice.** `lib.rs:88–103` carries three `#[cfg(all(doctest, not(feature = "standalone-cli")))]`-guarded `compile_fail` doctests asserting that `apply_standalone`, `RunProfile::Trusted`, and `GuardConfig::trusted` are *absent* from default/server builds. The new lattice makes `standalone-cli ⊇ zsv8`, so a doctest gated on `not(standalone-cli)` now *also* runs only when the extra `zsv8`-dependent surface may be present. Confirm at impl time: (a) none of the three referenced symbols (`apply_standalone`, `RunProfile::Trusted`, `GuardConfig::trusted`) becomes `zsv8`-gated — they are standalone-runner symbols, not V8-host symbols, so the intended "absent under default/server" proof is preserved; and (b) no *new* `compile_fail` doctest is added that references a now-`zsv8`-gated symbol under the wrong guard. The three MySQL `#[cfg(test)]` re-exports (`MysqlFragment*`) are separately re-gated to `#[cfg(all(test, feature = "zsv8"))]` above and are not doctest-visible. This is a cheap read-and-confirm, not a code change unless (a) or (b) surprises.

### C. The `command/` layer — the `.ts`-record path (transitively V8 via `crate::frontend`)
| File:line | Change |
|---|---|
| `src/command/ir_apply.rs:63` — `use crate::frontend::{record_migration_transient, BuildError, DiscoveredMigration, RecordVia};` | gate the `use` + its callers (`apply_platform_ts_postgres`, the `discover_migrations` call at `:456`) behind `#[cfg(feature = "zsv8")]` |
| `src/command/runner.rs:823` — `async fn run_migrate_pg_platform_ts` (contains `crate::frontend::RecordVia::local()` at `:833`) | gate the whole `.ts`-record function `#[cfg(feature = "zsv8")]` |
| `src/command/runner.rs:797–799` — the `MigrationCorpusFormat::Ts => return run_migrate_pg_platform_ts(cfg).await` arm **inside the V8-free `run_migrate_pg`** | <!-- Added in round 2: addressing BLOCKER #3 (dangling caller) --> the `Ts` arm is the **sole caller** of the gated function; it must be cfg-split — see the callout below |

<!-- Added in round 2: addressing BLOCKER #3 (dangling call site) -->
**Dangling-caller resolution (`run_migrate_pg`, runner.rs:795–819).** `run_migrate_pg` is itself V8-free and **stays in the core** — it also handles the `Sql` arm (`:799`) and the flat-load PG apply path (`:802–818`). But its `Ts` arm at `:798` calls the now-gated `run_migrate_pg_platform_ts`; under `--no-default-features` that call would reference a function that no longer exists → compile error. Resolution — split the `match platform_corpus_format(...)` block by cfg:

```rust
if cfg.profile == RunProfile::Platform {
    match platform_corpus_format(&cfg.dir)? {
        #[cfg(feature = "zsv8")]
        MigrationCorpusFormat::Ts => return run_migrate_pg_platform_ts(cfg).await,
        #[cfg(not(feature = "zsv8"))]
        MigrationCorpusFormat::Ts => return Err(RunError::TsRecordRequiresZsv8),
        MigrationCorpusFormat::Sql => {}
    }
}
```

This mirrors the `MysqlRequiresZsv8` pattern (§3D): a **build-time absence** surfaced as a typed error rather than a link failure. Add a new variant **`RunError::TsRecordRequiresZsv8`** — message: *"Authoring `.ts` migration records requires the `zsv8` runtime host; this binary was compiled V8-free (`--no-default-features`). Rebuild with `--features zsv8` (the default build), or supply committed `.ir.json` / `.sql` corpus instead."* The `Sql` arm and everything below `:800` are untouched and V8-free.

**Audit of all callers of the gated `.ts`-record functions:** `run_migrate_pg_platform_ts` has exactly one caller (runner.rs:798, handled above); `apply_platform_ts_postgres` (`command/ir_apply`) is called only from `run_migrate_pg_platform_ts` (gated) — no other production caller constructs the `.ts`-record path. No other dangling site exists.

<!-- Added in round 3: addressing MINOR #3 — confirm the two new RunError variants compile clean under BOTH feature states and break no exhaustive match. -->
**Both new variants are declared UNCONDITIONALLY on `RunError` (no `#[cfg]` on the variant or its message).** `RunError` (`runner.rs:161`) is `#[derive(Debug, thiserror::Error)]` and is **not** `#[non_exhaustive]`. `TsRecordRequiresZsv8` and `MysqlRequiresZsv8` are added as plain unit variants, each with an unconditional `#[error("…")]` attribute (the messages in §3C/§3D). They must exist in both feature states because the `#[cfg(not(feature = "zsv8"))]` arms *construct* them and their `Display` impl (via `thiserror`) must resolve unconditionally — gating the variant would break the `zsv8`-on build's `Display` derive and the `not(zsv8)` build's construction site respectively. **No exhaustive `match`/`matches!` over `RunError` needs a new arm:** every `matches!` site in the crate is a *partial* match (`matches!(err, RunError::X)` / `RunError::X | RunError::Y`), verified at `runner.rs:2368, 2386, 2556, 2562, 2567, 2571, 2588, 2594, 2747` — including the MySQL fail-closed assertion at `runner.rs:2386` (`matches!(err, RunError::MysqlLiveExecUnimplemented)`, itself inside a `zsv8`-gated test) — none of which is exhaustive, and there is no full `match err { … }` mapper over `RunError` (the `to_string()` at `runner.rs:2243` calls the variant's own `Display`, it does not re-match). Adding two unit variants is therefore additive under both feature states.

**Critical invariant for module B/C:** `command/ir_apply.rs` co-locates a V8-coupled `.ts`-record arm with the **V8-free** `.ir.json` load / sealed-apply arm. The functions dependents actually call — `apply_sealed`, `discover_ir_files`, `postgres_ir_apply_state`, `apply_*_ir_postgres`/`_sqlite` — have V8-free bodies and **must remain exported** in the V8-free core. Only the `.ts`-record functions and the module-level `use crate::frontend::{…}` are gated. The gate is per-function, not per-module — which is precisely why the `run_migrate_pg` `Ts` arm above needs the explicit cfg-split; a per-function gate leaves the caller behind unless the call site is handled too.

### D. The `mysql://` dispatch arm (graceful fallback)
The CLI `mysql://` path is *already* render-only and fail-closed: `classify_engine` (`runner.rs:464–469`) maps `mysql://`/`mysqlx://`/`mariadb://` → `Engine::Mysql`, and every live command arm returns `RunError::MysqlLiveExecUnimplemented` (`runner.rs:773,874,952,1183,1524,1812,2056,2243`; `bin/zeroship-migrate.rs:1024`). No production `command/` caller constructs `MysqlBackend` — the live MySQL backend is reached only via the library API `MysqlBackend::open_mysql_dsn_json*`, exercised by the e2e tests.

So the fallback is **honest, not new behavior**:
- `classify_engine`'s `Engine::Mysql` arm stays structurally intact (it does not touch V8).
- Add a typed error variant **`RunError::MysqlRequiresZsv8`** — message: *"MySQL execution requires the `zsv8` runtime host. This binary was compiled V8-free (`--no-default-features`); rebuild with `--features zsv8` (the default build)."* Under `#[cfg(not(feature = "zsv8"))]`, the live-MySQL match arms return `MysqlRequiresZsv8`; under `zsv8` they keep returning today's `MysqlLiveExecUnimplemented` (CLI still refuses MySQL DSNs — unchanged). The distinction is intentional: `MysqlRequiresZsv8` names a *build-time* absence, `MysqlLiveExecUnimplemented` names a *CLI-path* policy.
- **Compile-time fail-closed for direct embedders.** Because `MysqlBackend` and `open_mysql_dsn_json*` are `#[cfg(feature = "zsv8")]`, any embedder calling them without `zsv8` simply fails to compile — the correct fail-closed posture (compile error, not a runtime surprise).

### E. Bins (`[[bin]]` `required-features`)
| Bin (Cargo.toml) | Change |
|---|---|
| `zeroship-migrate` (`required-features = ["standalone-cli"]`) | unchanged; `standalone-cli ⊇ zsv8` (§2). The bin body is V8-free but its runner can reach MySQL, so riding `zsv8` is correct. |
| `zeroship-migrate-js` (`required-features = ["js-cli"]`) | unchanged; `js-cli ⊇ zsv8`. |
| `zeroship-migrate-recorder-child` (path `src/bin/recorder-child.rs`, **currently no `required-features`**) | **add `required-features = ["zsv8"]`** — otherwise it fails to compile under `--no-default-features`. This is the single most likely default-build breaker if missed. |

### F. Tests / dev-deps

<!-- Added in round 2: addressing BLOCKER #2 (test-gating map drastically under-scoped) -->
`cargo test -p zeroship-migrate --no-default-features` compiles **every** `tests/*.rs` target. So the gating map must cover every integration test that references a `zsv8`-gated symbol, not just the two MySQL-backend files. Two disjoint sets:

**(i) MySQL-backend tests — construct `MysqlBackend` (2 files):** `tests/mysql_jsdriver_e2e.rs`, `tests/extract_equivalence.rs`. `rcgen` dev-dep backs the MySQL e2e; not a build-graph blocker but the tests must be gated.

**(ii) Authoring / front-end tests — reference `zeroship_migrate::frontend::*` (22 files, grep-verified `grep -rln 'frontend::' tests/`):**
`build_generate_constraints_indexes.rs`, `build_new_generate_pg.rs`, `build_new_generate_sqlite.rs`, `build_one_migration.rs`, `build_record_paths.rs`, `cli_recorder_url_fallback.rs`, `eval_ir.rs`, `full_surface.rs`, `generate_all_types_parity.rs`, `generate_pg.rs`, `gen_types_cli.rs`, `gen_types_dts_golden.rs`, `gen_types_dts_tsc_gate.rs`, `gen_types_keystone_parity.rs`, `ir_dml_pg.rs`, `ir_dml_sqlite.rs`, `op_round_trip.rs`, `platform_ts_apply_pg.rs`, `recorder_http_contract.rs`, `recorder_sandbox_e2e.rs`, `recorder_service_contract.rs`, `split_part_lint.rs`.

**Gate for both sets:** prepend a **crate-level `#![cfg(feature = "zsv8")]`** to each file whose assertions are *entirely* V8-coupled, so under `--no-default-features` the whole target compiles to an empty crate and is skipped. **Two of the 22 authoring files are NOT entirely V8-coupled — `ir_dml_pg.rs` and `ir_dml_sqlite.rs` are SPLIT, not whole-gated** (each carries ~13–14 pure-core DML assertions plus a single V8-recorder test); see the DECIDED split-rule table below. Net gating: the 2 MySQL files + the 20 remaining authoring files are whole-gated; the 2 DML files stay ungated with their one recorder test extracted to a new gated `*_recorded.rs` sibling.

**Split rule for mixed files — DECIDED (not deferred).**
<!-- Revised in round 3: addressing MAJOR #2 — the three candidates were grepped and the split-vs-whole-gate verdict is now recorded per file, so §9 step 7 is a mechanical apply, not an investigation. -->
The three candidates were inspected (grep of each file's `frontend::` line against its test bodies). Verdict per file:

| File | `frontend::` usage | Core assertions? | Verdict |
|---|---|---|---|
| `tests/ir_dml_pg.rs` | module-scope import `frontend::record_migration_to_ir_unsandboxed` (`:23`), called by **exactly one** test — `recorded_fnsynth_symbol_insert_applies_db_evaluated_values_on_pg` (`:416`, uses it at `:456`/`:458`). The other ~13 `#[compio::test]` fns drive a hand-written `.ir.json` → `load_ir_document` → `IrAuthor::lower_plan` → `apply_plan` (pure core; `author_and_apply` `:110`, `load_recorded_ir_and_apply` `:143` — neither touches the V8 recorder). | **Yes — ~13 pure-core DML-render/apply assertions.** | **SPLIT.** Move the single `recorded_fnsynth_symbol_*` test **and** the `record_migration_to_ir_unsandboxed` import into a new gated sibling `tests/ir_dml_pg_recorded.rs` (`#![cfg(feature = "zsv8")]`). Keep `tests/ir_dml_pg.rs` ungated (drop the now-unused `frontend::` import). |
| `tests/ir_dml_sqlite.rs` | module-scope import `frontend::record_migration_to_ir_unsandboxed` (`:29`), called by **exactly one** test — `recorded_fnsynth_symbol_insert_applies_db_evaluated_values_on_sqlite` (`:317`, at `:353`/`:355`). The other ~14 `#[compio::test]` fns drive `.ir.json` via `lower_and_apply` (`:106`) / `lower_plan_and_apply` (`:147`) — pure core (`IrAuthor::load_and_lower` / `lower_plan` + `apply_plan`, no recorder). | **Yes — ~14 pure-core DML-render/apply assertions.** | **SPLIT.** Move the single `recorded_fnsynth_symbol_*` test **and** the `record_migration_to_ir_unsandboxed` import into a new gated sibling `tests/ir_dml_sqlite_recorded.rs` (`#![cfg(feature = "zsv8")]`). Keep `tests/ir_dml_sqlite.rs` ungated. |
| `tests/split_part_lint.rs` | module-scope import `frontend::record_migration_to_json_unsandboxed` (`:13`); **all 4** `#[test]` fns call it via the `record()` helper (`:17`). | **No pure-core assertion** — it exercises the JS `op.*` recorder grammar lint end-to-end; the authoritative *Rust* `validate::check_split_part` is covered elsewhere (this file's own header says so). | **WHOLE-GATE.** Prepend `#![cfg(feature = "zsv8")]`. |

**Resulting file inventory** (the 24 candidates → 24 files touched + 2 new siblings):
- **22 whole-gated** (`#![cfg(feature = "zsv8")]`): the 2 MySQL-backend files + `split_part_lint.rs` + the other 19 `frontend::` authoring files.
- **2 SPLIT**: `ir_dml_pg.rs` and `ir_dml_sqlite.rs` stay **ungated** (their ~13–14 pure-core DML assertions survive `--no-default-features`, in the *original* file — no `_core.rs` rename needed), with each file's single `recorded_fnsynth_symbol_*` test + `record_migration_to_ir_unsandboxed` import extracted into a **new gated sibling** (`ir_dml_pg_recorded.rs` / `ir_dml_sqlite_recorded.rs`, `#![cfg(feature = "zsv8")]`).

§9 step 7 applies this inventory verbatim.

**Recorder-child is a third V8 bin, not a test.** `src/bin/recorder-child.rs` is handled in §3E (required-features), not here.

<!-- Added in round 3: addressing MINOR #5 — the test-gating exhaustiveness argument must also clear doctests, benches, and examples, not just tests/. -->
**Beyond `tests/`: doctests, benches, examples are all clear.**
- **Benches / examples:** the crate has **no** `[[bench]]` or `[[example]]` targets (`grep -n '\[\[bench\]\]\|\[\[example\]\]' Cargo.toml` → 0; no `benches/` or `examples/` dir). Nothing to gate.
- **Doctests:** `cargo test --no-default-features --doc` runs runnable doctests on public items in ungated modules. A `///` example on a now-ungated core item (e.g. `MigrationBackend`) that referenced a `zsv8`-gated symbol (`MysqlBackend`, `JsDriverConn`, `frontend::…`) would fail the core doctest run. **Verified clear:** `grep -rn --include='*.rs' -e 'MysqlBackend' -e 'JsDriver' -e 'frontend::' src | grep -F '///'` returns **zero** doc-comment lines — no ungated public item carries a runnable doctest mentioning a gated symbol. The only `compile_fail` doctests are the three at `lib.rs:88–103` (audited in §3B tail; they reference standalone-runner symbols, not V8-host symbols). So `cargo test --no-default-features --doc` compiles clean; no `no_run`/`ignore` retrofit is needed. Should a future edit add such an example on an ungated item, gate it or mark it `no_run`.

---

## 4. Can `frontend/` be gated wholesale? — Almost; one helper must move first.

`frontend/` is a droppable leaf for the V8-free core (no production library embedder needs authoring — `control` has no dep, `plugin-db` uses apply/IR/differ, `migrated` uses IR-load + sealed-apply; only the dev-dep-only `schema-authority-e2e` imports `frontend::*`). But **`frontend/embedding.rs` is not purely authoring**: it hosts `js_driver_module_graph` (`embedding.rs:123`) — the **mysql2 driver-isolate** module graph + vendored `mysql2/promise` bundle const — which the MySQL backend imports at `transport.rs:9`. It sits beside the genuine authoring glue (`module_graph`, `install_frontend_globals`, `FrontendProgram`, `FrontendGlobals`).

**Decision: relocate `js_driver_module_graph` (and the vendored `mysql2-*.bundle.mjs` asset it embeds) into `apply/backend/mysql/` — its sole caller — before gating.** After the move, `frontend/embedding.rs` is authoring-only, and the whole `frontend/` directory gates cleanly under `zsv8`. Because both the frontend and the MySQL backend ride the *same* `zsv8` feature, the two compile-or-neither together — but the *code* dependency (`mysql → frontend::embedding`) is severed, which is the correct architecture (a MySQL driver-isolate helper does not belong in the schema-authoring module). This is a pure code move (no logic change).

<!-- Added in round 3: addressing MINOR #6 — pin the exact new asset path + corrected include_str! so the move doesn't silently break the bundle load, and add a DB-free validation. -->
**Exact relocation mechanics (asset + const + fn, no dangling `include_str!`):**
- **The vendored asset file MOVES.** `git mv crates/zeroship-migrate/src/frontend/vendor/mysql2-3.14.1.bundle.mjs crates/zeroship-migrate/src/apply/backend/mysql/vendor/mysql2-3.14.1.bundle.mjs` — it is not left orphaned under `frontend/vendor/`.
- **The const moves and its `include_str!` literal is rewritten to the new module-relative path.** Today `embedding.rs:32` reads `const MYSQL2_PROMISE_BUNDLE_JS: &str = include_str!("vendor/mysql2-3.14.1.bundle.mjs");`. After the move, in the new `apply/backend/mysql/embedding.rs` (or an inline block in `transport.rs`), it reads `include_str!("vendor/mysql2-3.14.1.bundle.mjs")` — the literal is unchanged **only because the `vendor/` subdir travels with it**; if the const lands in `transport.rs` directly (no `vendor/` subdir beside it), the literal becomes `include_str!("vendor/mysql2-3.14.1.bundle.mjs")` relative to `apply/backend/mysql/`. Either way the resolved path is `apply/backend/mysql/vendor/mysql2-3.14.1.bundle.mjs`; keep the `vendor/` subdir so the literal text needs no change.
- **`js_driver_module_graph` (`embedding.rs:123`) moves with it.** It depends on `MYSQL2_PROMISE_BUNDLE_JS`, the `module()` helper (`embedding.rs:73`), and `zeroship_runtime::ModuleEntry` (`embedding.rs:8`). The new home re-imports `use zeroship_runtime::ModuleEntry;` and carries a private `module()` helper (or `pub(crate)`-re-uses the one that stays in `frontend/embedding.rs` — but since `frontend/` is `zsv8`-gated and so is `mysql/`, the cleanest cut is a small private copy in the mysql module so it has no `frontend::` edge at all). Fix `transport.rs:9` `use crate::frontend::embedding::js_driver_module_graph;` → `use crate::apply::backend::mysql::embedding::js_driver_module_graph;` (or drop the `use` if colocated in the mysql module).
- **DB-free validation of the move.** Add a `#[cfg(feature = "zsv8")]` unit test in the mysql module asserting the module graph is well-formed without a live MySQL: e.g. `let g = js_driver_module_graph("export default 1;"); assert!(g.iter().any(|m| m.specifier == "mysql2/promise")); assert!(!MYSQL2_PROMISE_BUNDLE_JS.is_empty());`. This proves the `include_str!` still resolves (a broken path fails to *compile*) and the vendored bundle is non-empty — validated on the `zsv8` build even when the live-MySQL e2e can't run (DB down). It does not need a MySQL server.

V8 coupling inside `frontend/` is confined to 4 of 16 files (`record.rs`, `embedding.rs`, `sandbox.rs`, `recorder_http.rs`); the rest lower to IR and drive the differ. Gating the whole directory is correct — the V8-free core has no authoring path at all (author `.ts` under the default build, ingest committed `.ir.json` under the core).

---

## 5. `command/ir_apply` split (the load-bearing separation)

The one module that mixes V8-coupled and V8-free code. Concretely:

- **Stays in the core (ungated):** `apply_sealed`, `discover_ir_files`, `postgres_ir_apply_state`, `PostgresIrApplyError`, `PostgresIrApplyOutcome`, `SealedApplyError`, `apply_*_ir_postgres`/`_sqlite`. These are what `migrated` (`apply.rs`) and `plugin-db` consume; all have V8-free bodies.
- **Gated behind `zsv8`:** the module-level `use crate::frontend::{record_migration_transient, BuildError, DiscoveredMigration, RecordVia}`, the `.ts`-record functions (`apply_platform_ts_postgres` and its `discover_migrations`/`record_migration_transient` calls), and the `runner.rs` `run_migrate_pg_platform_ts` entry.

Implementation: split `ir_apply.rs` so the `frontend`-importing functions live in a `#[cfg(feature = "zsv8")]` submodule (e.g. `command/ir_apply/ts_record.rs`), leaving the IR-load/sealed-apply functions in the ungated parent. This keeps the module-level `use crate::frontend` off the V8-free compile path entirely (a gated `use` at module scope is fragile — a gated submodule is clean).

---

## 6. Why the seam is the right cut line (confirmation, not new work)

1. **`MigrationBackend` (`apply/backend/mod.rs:166`) never names a V8 type.** Static dispatch (`apply_with_lock_backend<B: MigrationBackend>`, `executor.rs:826`); every method takes dialect-neutral owned types (`ExecutorConfig`, `Migration`, `AppliedEntry`, `BindValue`, `SchemaSnapshot`, typed errors). No `JsDriverConn`, no `v8::` handle crosses the trait surface.
2. **`MysqlBackend` (`mysql/mod.rs:45`) is the only V8-coupled impl.** It wraps a `RefCell<JsDriverConn>`; the isolate is built in `JsDriverConn::open_with_entry` (`transport.rs:502–525`) via `Runtime::builder()…build()`. It cannot exist without the isolate — hence the compile gate.
3. **`PostgresBackend` / `SqliteBackend` are genuinely V8-free** (grep-confirmed: zero `zeroship_runtime`/`v8`/`JsDriver` hits; the only `Runtime` match in `sqlite/dump_sql.rs:169` is compio). `PostgresBackend::new` takes a `compio_postgres::Client`; `SqliteBackend::open` opens a rusqlite file.
4. Nothing V8-free references `MysqlBackend`. The generic executor reaches MySQL only when a caller *hands* it a `MysqlBackend` — which, gated, no V8-free caller can construct.

---

## 7. Dependent sufficiency (does the V8-free core satisfy the real consumers?)

| Dependent | Surface used | V8-free-core sufficient? |
|---|---|---|
| `plugin-db` (`register_model/sqlite_engine.rs`) | `MigrationBackend`, `sqlite::SqliteBackend`, `render::declarative::*`, `desired_snapshot`, `MigrationEngine`, `GuardConfig`, `ExecutorConfig`, `Migration`, `Checksum`, … | **Yes** — every symbol in a V8-free module; never MySQL, never `frontend`. |
| `migrated` (`apply.rs`, `policy.rs`, `*_store.rs`) | `apply_sealed`, `discover_ir_files`, `postgres_ir_apply_state`, `PostgresBackend`, `IrAuthor`, `PolicyProfile`, `seal_effective_profile`, `analysis::…::DATA_SECURITY_UNCLASSIFIED_OPS_WARN`, … | **Yes** — all V8-free, once §5 separation lands. IR-load + sealed-apply; no `frontend::` import. |
| `schema-authority-e2e` (**dev-dep only**) | above + `frontend::generate_migration`, `frontend::eval_schema_to_ir` | **Needs `zsv8`** — and that is correct. Depends with default features on; not part of the "core must build" milestone. |
| `control` | none (comment references only) | Not a dependent. |

**No production/library dependent regresses** against `--no-default-features`.

<!-- Added in round 3: addressing MINOR #4 — prove the inbound-edge set is complete, not sampled. -->
**The inbound-edge set is exhaustive, not sampled.** `grep -rn 'zeroship-migrate' crates/*/Cargo.toml` returns **exactly three** dependency edges into `zeroship-migrate` from other workspace members (all other hits are `zeroship-migrate`'s *own* `Cargo.toml` bin/feature lines):
- `crates/plugin-db/Cargo.toml:34` — `zeroship-migrate = { workspace = true }` (production lib dep → flip to `default-features = false`)
- `crates/migrated/Cargo.toml:27` — `zeroship-migrate = { workspace = true }` (production lib dep → flip)
- `crates/schema-authority-e2e/Cargo.toml:18` — `zeroship-migrate = { workspace = true }` (**dev-dep-only** e2e harness — keeps default features; imports `frontend::*`)

**No other workspace member (`control`, `gateway`, `worker`, `runtime`, …) has a direct `zeroship-migrate` edge** (grep-confirmed zero hits outside the three above). So the unification argument closes: the only two default-features edges that could re-unify `zsv8` onto a shared platform build are `plugin-db` and `migrated`, and this milestone flips both. Any transitive dependant of `plugin-db`/`migrated` (e.g. `control` → `plugin-db`) inherits the already-flipped `default-features = false` edge, so no third flip is required. The §10 `cargo tree -p zeroship-plugin-db | grep -c v8 == 0` assertion is the empirical backstop confirming this holds after resolution.

<!-- Added in round 2: addressing BLOCKER #1 + MAJOR #6 (dependent flip + stale Cargo comment) -->
### 7.1 The dependent flip (the step that actually removes V8)

Once the core compiles V8-free (steps 1–6), flip the two production library dependents to opt out of the default `zsv8`-pulling features. This is the payoff step; without it the workspace still unifies `zsv8` on and nothing changes for the platform.

```toml
# crates/plugin-db/Cargo.toml   (currently line 34: `zeroship-migrate = { workspace = true }`)
zeroship-migrate = { workspace = true, default-features = false }

# crates/migrated/Cargo.toml    (currently line 27: `zeroship-migrate = { workspace = true }`)
zeroship-migrate = { workspace = true, default-features = false }
```

Both are safe: §7 shows every symbol they use lives in a V8-free module. `plugin-db` reaches `SqliteBackend` / `MigrationEngine` / `render::declarative` / `desired_snapshot`; `migrated` reaches `apply_sealed` / `discover_ir_files` / `postgres_ir_apply_state` / `PostgresBackend` — none behind `zsv8`.

**Fix the stale `plugin-db` Cargo comment in the same change.** `crates/plugin-db/Cargo.toml:32–33` currently asserts *"zeroship-migrate depends only on core + schema (no runtime/plugin-db), so no dependency cycle."* That is **already false today** — `zeroship-migrate` depends on `zeroship-runtime` unconditionally — and it is the exact misconception that hid BLOCKER #1. Under pre-launch no-back-compat discipline, correct it in this PR to read: *"zeroship-migrate pulls the V8 host (`zeroship-runtime` + `v8`) under its default features; plugin-db opts out via `default-features = false` and consumes only the V8-free apply/IR/render surface — so neither the V8 host nor a dependency cycle enters plugin-db's tree."*

**Update the `zeroship-migrate` package `description`.** If the crate `description` advertises the "V8-backed JS authoring front-end" as core identity, amend it to note the V8 host is now **optional behind `zsv8`** and the default library surface for embedders can be built V8-free — so the manifest metadata matches the new feature reality.

> **Note on `schema-authority-e2e`.** It stays on default features (it imports `frontend::*`); it is a dev-dep-only e2e harness, not part of the "core must build V8-free" milestone. Its presence does **not** re-unify `zsv8` onto `plugin-db`/`migrated`, because feature unification is per-package-per-resolved-features: `schema-authority-e2e` pulling `zsv8`-on `zeroship-migrate` does not force `plugin-db`'s *own* `default-features = false` edge to gain `zsv8`. A `cargo tree -p zeroship-plugin-db` (§10) confirms this empirically.

---

## 8. FUTURE WORK (not this milestone)

### 8.1 Node/napi shell
The eventual direction is a **Node shell** (napi-rs) that embeds the V8-free core and supplies *authoring* and *mysql2* **from the JS side** — the JS host already has V8, `mysql2`, and the `@zeroship/db` DSL, so the in-Rust `zsv8` host becomes redundant there. This milestone's gate is the enabling precondition: once the core builds without `zeroship-runtime`, the napi shell links the V8-free core and re-supplies the two gated subsystems in JS (authoring via the existing recorder protocol over the napi boundary; MySQL via `mysql2` in the host Node process). `zsv8` then becomes the "in-Rust host" build; the napi shell is the "JS host" build. **No code for this now** — the gate just makes it reachable.

### 8.2 compio DB-I/O seam
Later, the core's DB I/O (`compio-postgres`/`rusqlite`) could itself become a seam so a host can inject its own driver (mirroring the Temporal model). **Explicitly deferred** — compio stays in the core for this milestone (§1 non-goal). Noting it only so the `MigrationBackend` seam and the eventual I/O seam are understood as two independent axes, not conflated.

---

## 9. Impl steps (in order)

1. **Relocate `js_driver_module_graph` + the `MYSQL2_PROMISE_BUNDLE_JS` const + the vendored asset file** from `frontend/{embedding.rs, vendor/}` into `apply/backend/mysql/{embedding.rs, vendor/}` per §4's exact mechanics: `git mv` the `.mjs` asset (keep the `vendor/` subdir so the `include_str!` literal is unchanged), move the const + fn, fix `transport.rs:9` import. Add the DB-free `#[cfg(feature = "zsv8")]` module-graph unit test (§4). Build default — green.
2. **Split `command/ir_apply.rs`**: move the `frontend`-importing `.ts`-record functions into a `#[cfg(feature = "zsv8")]` submodule; keep IR-load/sealed-apply ungated. Gate `runner.rs:823`'s `run_migrate_pg_platform_ts`. <!-- Added in round 2: BLOCKER #3 --> Cfg-split the `run_migrate_pg` `Ts` arm (`runner.rs:797–799`) and add `RunError::TsRecordRequiresZsv8` for the `#[cfg(not(feature = "zsv8"))]` fallback (§3C). Build default — green.
3. **Add `RunError::MysqlRequiresZsv8`**; wire the `#[cfg(not(feature = "zsv8"))]` live-MySQL arms to it. Build default — green (arm is dead under `zsv8`).
4. **Split the re-export blocks** per the §3B KEEP/MOVE checklist: `lib.rs:114–124` split into an ungated `pub use` (the 13 KEEP names — `BackfillError`, `BackfillOutcome`, `CrossDeployObligations`, `DryRunError`, `DryRunReport`, `MigrationBackend`, `MigrationResult`, `OnlineSchemaChange`, `PgSessionSnapshot`, `PostgresBackend`, `SeedError`, `ShadowConfig`, `ShadowDryRun`) + a `#[cfg(feature = "zsv8")]` `pub use` (the 12 MOVE MySQL names); re-gate `lib.rs:113–114` to `#[cfg(all(test, feature = "zsv8"))]`. In `backend/mod.rs`: `:43–47` unchanged (already V8-free), gate `:50–55` mysql block `#[cfg(feature = "zsv8")]`, re-gate `:48–49` to `#[cfg(all(test, feature = "zsv8"))]`.
5. **Gate module decls** (`lib.rs:76` `mod frontend`; `backend/mod.rs:40` `mod mysql`).
6. **Cargo.toml (this crate)**: flip `zeroship-runtime`/`v8`/`seccompiler`/`landlock`/`libc` to `optional = true`; add `zsv8` (incl. `dep:libc`); make `js-cli`/`standalone-cli` pull `zsv8`; add `required-features = ["zsv8"]` to the recorder-child `[[bin]]`; amend the crate `description` (§7.1). Turn on the V8-free build here.
7. **Gate the V8-coupled test files per the §3F DECIDED table** — whole-gate (`#![cfg(feature = "zsv8")]`) the 2 MySQL-backend files + 20 authoring files (incl. `split_part_lint.rs`); **SPLIT** `ir_dml_pg.rs` and `ir_dml_sqlite.rs` — extract their single `recorded_fnsynth_symbol_*` test + the `record_migration_to_ir_unsandboxed` import into new gated siblings `ir_dml_pg_recorded.rs` / `ir_dml_sqlite_recorded.rs` (`#![cfg(feature = "zsv8")]`), leaving the ~13–14 pure-core DML assertions ungated. Mechanical apply of the §3F table — no fresh investigation.
8. <!-- Added in round 2: BLOCKER #1 --> **Flip the dependents** (§7.1): set `default-features = false` on the `zeroship-migrate` edge in `crates/plugin-db/Cargo.toml:34` and `crates/migrated/Cargo.toml:27`; fix the stale plugin-db Cargo comment (lines 32–33). This is the step that removes V8 from the platform tree.
9. <!-- Added in round 2: MINOR #6 --> **Confirm the `lib.rs:88–103` `compile_fail` doctests** still assert absence under the new feature lattice (§3B tail). No symbol they reference should become `zsv8`-gated in a way that changes what the doctest proves.

Steps 1–5 keep the **default build green**; the V8-free build turns on at step 6; steps 8–9 make the removal real and prove it.

---

## 10. Verification plan

- **Core-only builds V8-free:** `cargo build -p zeroship-migrate --no-default-features` succeeds, and `cargo tree -p zeroship-migrate --no-default-features` shows **no** `v8`, `zeroship-runtime`, `seccompiler`, `landlock`, `libc`.
- **Default build unchanged:** `cargo build -p zeroship-migrate` (⇒ `js-cli` ⇒ `zsv8`) builds all three bins; `cargo build -p zeroship-migrate --features standalone-cli` builds the standalone bin.
- **Full suite (default):** `cargo test -p zeroship-migrate` — all targets, per the full-suite discipline (not `--lib` only). DB-backed tests need Postgres on :5440 (docker `appbase-migrate-postgres-1`); render/unit tests are the DB-free clean signal if the DB is down.
- <!-- Added in round 2: addressing BLOCKER #2 (core-only test claim was false) --> **Core-only tests compile and run.** `cargo test -p zeroship-migrate --no-default-features` compiles **every** `tests/*.rs` target, so this only passes once the 22 whole-gated files carry `#![cfg(feature = "zsv8")]` **and** the 2 DML files are SPLIT with their recorder tests moved to the gated `*_recorded.rs` siblings (§3F inventory) — otherwise the ungated `ir_dml_*.rs` and the ~20 other authoring targets fail to resolve `zeroship_migrate::frontend::*` and the invocation never links. Assert: (a) the command compiles clean, and (b) the two SPLIT core targets — the now-ungated `ir_dml_pg.rs` / `ir_dml_sqlite.rs` (which still hold their ~13–14 pure-core DML assertions) — actually run and pass under `--no-default-features`, proving core DML coverage survived the split. If a full `--no-default-features` test compile is undesirable, the fallback is to run only explicitly-selected core targets: `cargo test -p zeroship-migrate --no-default-features --test ir_dml_pg --test ir_dml_sqlite` (and any other pure-core suite) — but the whole-invocation form is preferred as the exhaustive gate.
- <!-- Added in round 2: addressing BLOCKER #1 + MINOR #9 (workspace-level outcome never asserted) --> **The real removal, at the workspace level.** After the §7.1 flip, assert V8 is gone from the dependents' *resolved* trees:
  - `cargo tree -p zeroship-plugin-db | grep -c -E 'v8|zeroship-runtime'` **== 0**
  - `cargo tree -p zeroship-migrated  | grep -c -E 'v8|zeroship-runtime'` **== 0**
  - A whole-workspace `cargo build` still succeeds (the platform binaries link the V8-free `plugin-db`/`migrated` while `zeroship-migrate-js` / the standalone bin still get `zsv8` via their own edges). Without these two assertions the isolated `-p ... --no-default-features` check can be green while the platform still links V8 everywhere it matters — a checklist that proves nothing about the goal.
- **Dependents build:** `cargo build -p zeroship-plugin-db` and `cargo build -p zeroship-migrated` (now `default-features = false` on the migrate edge, both must stay green); `cargo build -p schema-authority-e2e --tests` (dev-dep, keeps default features — needs `zsv8`).
- **No wire/IR drift:** no `CURRENT_IR_VERSION` change; serde byte-parity of existing `.ir.json` fixtures.

---

## 11. Risk register

| Risk | Likelihood | Mitigation |
|---|---|---|
| **cfg-cascade breaks the *default* build** (a `#[cfg]` typo or an over-gated symbol drops a name the default path needs) | Med | Every impl step (§9) ends with "build default — green"; the split-not-delete rule for re-export blocks (§3B) keeps V8-free names ungated; CI runs both `--default` and `--no-default-features`. |
| **Recorder-child bin fails under `--no-default-features`** (it has no `required-features` today) | High if missed | §3E: add `required-features = ["zsv8"]`. Called out as the single most likely breaker; the core-only build gate (§10) catches it immediately. |
| **`command/ir_apply` module-level `use crate::frontend` leaks V8 into the core** | Med | §5: move the `.ts`-record functions into a gated *submodule* so no gated `use` sits at the ungated module scope. |
| **Over-gating drops a V8-free symbol a dependent needs** (`MigrationBackend`, `PostgresBackend`, `ShadowConfig`, `apply_sealed`, `discover_ir_files`, `postgres_ir_apply_state`) | Med | The **§3B.1 explicit KEEP/MOVE table** is the literal checklist — the 13 KEEP names stay ungated by construction, forbidding a mechanical whole-block move behind `zsv8`; §7 dependent matrix cross-checks; the dependents-build verification (§10) is the empirical backstop. |
| **Hidden V8 use the map missed** | Low | <!-- Corrected in round 2: addressing MAJOR #4 (false grep claim) --> Grep sweep, scoped correctly: `grep -rn "v8::" src/` and `grep -rn "zeroship_runtime" src/` land **only** in (a) `src/frontend/` and `src/apply/backend/mysql/` (the two gated library subsystems) and (b) the three V8 bins — of which `src/bin/recorder-child.rs` alone carries 12 `v8::`/`zeroship_runtime` hits (it is a *third* V8 bin, gated via `required-features` per §3E, and must be excluded from the "library modules" grep). Restricting to library modules outside `frontend/` and `apply/backend/mysql/` → **0 hits**. The earlier claim of "outside the two subsystems + two V8 bins → 0 hits" was wrong: it silently miscounted recorder-child. The `--no-default-features` build is itself the exhaustive proof — any missed use fails to compile. |
| **`seccompiler`/`landlock`/`libc` linger in the core tree** (Linux-target block) | Low | All three made `optional` + folded into `zsv8` (§2, verified footprint entirely inside `frontend/*` + `bin/recorder-child.rs`); `cargo tree --no-default-features` asserts absence. |
| **`js_driver_module_graph` relocation breaks the `include_str!` or orphans the vendored `.mjs`** | Low | §4/§9-step-1: `git mv` the asset **with** its `vendor/` subdir so the `include_str!` literal is unchanged; a mis-resolved path fails to *compile* (not a runtime surprise). Backstopped by a **DB-free** `#[cfg(feature="zsv8")]` unit test asserting the module graph is well-formed and the bundle const is non-empty (§4) — validated even when the live-MySQL e2e (`mysql_jsdriver_e2e.rs`, DB-dependent) can't run. |
| **A `#[cfg(test)]` re-export interacts badly with `zsv8`** (`MysqlFragment*` at `mod.rs:49`, `lib.rs:113`) | Low | Gate as `#[cfg(all(test, feature = "zsv8"))]` (§3B). |

---

## Implementation addendum (2026-07-10, post-impl corrections)

Two claims in the body were falsified during implementation and are corrected here (the body above is left intact as the reviewed record):

1. **Flipping the consumer edges is not sufficient — the *workspace-dependencies* entry must also set `default-features = false`.** §7.1/§9-step-8 assumed setting `default-features = false` on `plugin-db`/`migrated`'s `zeroship-migrate = { workspace = true, … }` edges would drop V8 from their trees. Cargo **ignores** a consumer's `default-features = false` on a `workspace = true` dependency unless the root `[workspace.dependencies]` entry itself specifies `default-features` (Cargo emits a warning to this effect). The load-bearing change is therefore in the **root `Cargo.toml`**: `zeroship-migrate = { path = "…", default-features = false }`. Consequence: every `workspace = true` consumer now defaults to features-off and must **opt in** explicitly — `schema-authority-e2e` was set to `features = ["zsv8"]`; the CLI bins still get `zsv8` via the package's own `default = ["js-cli"]`.

2. **The `cargo tree -p zeroship-plugin-db | grep -c v8 == 0` assertion (§7 line ~282, §10) is unachievable and wrong.** `plugin-db` depends on `zeroship-runtime` + `v8` **directly** (`plugin-db/Cargo.toml`) — it is itself a native-V8 `env.db` plugin — so its count stays nonzero regardless of the migrate edge. What this milestone actually severs is the **migrate → plugin-db** V8-injection path; the correct empirical checks are: `cargo tree -p zeroship-migrated | grep -c -E 'v8|zeroship-runtime'` **== 0** (achieved — `migrated` has no intrinsic V8 edge), and a reverse-tree (`cargo tree -i v8`) confirming `plugin-db`'s remaining V8 comes only from its own direct edges, not from `zeroship-migrate`.
