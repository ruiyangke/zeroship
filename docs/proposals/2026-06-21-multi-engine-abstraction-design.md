# Multi-engine abstraction for zeroship-migrate

**Status:** proposal / design audit
**Date:** 2026-06-21
**Branch context:** `feat/db-migration-engine` (worktree `appbase-migrate`)
**Directive:** "support sqlite first, abstract the zeroship migrate, support multiple db engine."

## Goal

Turn `zeroship-migrate` into a genuinely **engine-agnostic** migration engine:

- **SQLite as a first-class backend**, not bolted on — the *reference* clean impl.
- The core abstraction clean/general enough that adding a **new engine (e.g. MySQL)** is "implement the backend trait(s)", **not** "touch the core".
- Reconcile with the public dbmate-like CLI (#55) so it can target any engine.

The bar: the **Postgres suite is the byte-identical regression bar** — every PG behavior must survive the refactor unchanged. SQLite proves generality by being a clean impl with **no special-casing in the core**.

---

## 1. Audit — where the engine is still PG-coupled / where the abstraction leaks

P1 introduced `MigrationBackend` (`backend.rs`); P6a genericized `MigrationEngine::apply*` over it; P6b wired SQLite into the dev tier. An abstraction **exists**, but it was designed PG-first and leaks. The leaks fall into four severities: **(A) hard PG escape hatches in the generic body**, **(B) PG types/semantics on the trait surface**, **(C) PG-only subsystems not behind the trait at all**, **(D) the CLI / config / DDL dialect set**.

### 1.1 The `MigrationBackend` trait surface (`backend.rs`)

Clean (dialect-neutral) methods — these are the model to follow:
- `dialect()`, `acquire/release_project_lock`, `apply_up_transactional`, `rollback_one_transactional`, `validate_non_txn`, `ensure_journal`, `applied`, `superseded_versions`, `latest_completed_checksums`, `check_checksum_drift`, `snapshot_schema`, `rebuild_one` — all return dialect-neutral owned types (`AppliedEntry`, `SchemaSnapshot`, `ChecksumDriftReport`). Errors map onto `ApplyError::Backend(String)` / `DriftError::Backend(String)` / `JournalError::Backend(String)`. Good.

> **CORRECTION (post-critic, see Revision §R.C3):** `evaluate_preconditions` was listed here as clean — it is **not**. The trait *return type* (`PreconditionVerdict`) is neutral, but the only impl, `precondition.rs`, is **wholly PG**: `validate_single_select` calls `pg_query::parse` (`precondition.rs:424-425`) and `table_exists`/`column_exists` query `information_schema` over a PG `&Client` (`precondition.rs:348/368`). It is a per-engine subsystem behind a neutral return type, the same shape as `snapshot_schema` — it belongs in the **per-engine** column, not the "clean core" list. See the leak inventory (L12) and Revision §R.C3.

Leaks **on the trait**:

1. **`fn expand_conn(&self) -> Option<&Client>`** — `backend.rs:245`. A raw `compio_postgres::Client` on the trait. This is *the* canonical leak: the trait's contract says "I am dialect-neutral" but one method hands back a concrete PG connection so the engine can drive PG-shaped expand-contract directly. `PostgresBackend` returns `Some` (`backend.rs:410-412`); `SqliteBackend` returns `None` (`backend_sqlite/mod.rs:421-427`). A third engine has nothing sane to return.

2. **`SessionSnapshot`** — `backend.rs:58-67`. The struct is *documented* as dialect-neutral (the engine round-trips it opaquely), but its three fields are literally `statement_timeout` / `lock_timeout` / `search_path` GUC strings — PG vocabulary baked into a "neutral" type. SQLite returns `default()` (empty). It works, but the shape is PG's, so a new engine inherits three meaningless fields. Minor leak (the engine never inspects it), but it is PG vocabulary in the shared type.

3. **`configure_session_non_txn` / `apply_up_non_transactional`** — `backend.rs:119-136`. The whole non-txn two-phase concept is a PG-ism (`CREATE INDEX CONCURRENTLY`, `ALTER TYPE … ADD VALUE`). SQLite implements both as hard errors (`backend_sqlite/mod.rs:266-291`) and relies on `validate_non_txn` rejecting `transaction:false` upstream so they are never called. This is *acceptable* (the trait method exists; SQLite fails closed) but it bakes "every engine has a non-txn pass" into the trait. A cleaner model makes the non-txn pass an **optional capability**, not two mandatory methods every engine must stub.

4. **`import compio_postgres::Client`** at the top of `backend.rs:40` — the trait module itself depends on the PG driver purely for `expand_conn`. Removing `expand_conn` removes the only reason the trait file imports PG.

### 1.2 `run_expand` / `run_backfill` — the expand-contract online path (NOT abstracted)

This is the **single worst leak**. The online expand-contract drive is PG-shaped and reached through `expand_conn()`:

- `engine.rs:929-964` `run_expand_with_lock` takes `conn: &Client` and calls `executor::apply_with_lock(conn, …)` + `backfill::run_backfill(conn, …)` directly.
- `engine.rs:550-570` (`apply_declarative_locked`) pulls the PG `Client` out via `backend.expand_conn()` and fails closed if a non-PG backend has renames.
- `backfill.rs` is **entirely PG** — `pg_advisory_xact_lock` (`backfill.rs:899`), `pg_query::parse` (`backfill.rs:602`), paged `UPDATE` SQL, `compio_postgres::Client` throughout.
- `expand_contract.rs` authors PG-specific DDL (PL/pgSQL dual-write trigger functions, `CREATE TRIGGER`).

So expand-contract is **structurally PG-only** and the trait's `expand_conn` is the escape hatch. SQLite "handles" renames by routing them to a `rebuild_one` (offline 12-step table rebuild) and keeping `plan.renames` empty, so `expand_conn() == None` is never reached with work to do. That works for SQLite but it does **not generalize**: a third engine that *does* support online rename (MySQL via `ALTER TABLE … ALGORITHM=INPLACE` / pt-online-schema-change-style) has no seam to plug into — it would have to return a PG `Client` from `expand_conn`, which is impossible.

### 1.3 The guard / lint / parse-time validation (the HARDEST concern)

`guard.rs` / `classify.rs` / `analyze.rs` are **`pg_query` (libpg_query) — PG grammar only**:

- `guard.rs:24-25` imports `pg_query::protobuf`; `SqlGuard::check` (`guard.rs:466`) calls `pg_query::parse` (`guard.rs:502`); `classify.rs` and `analyze.rs` likewise parse with `pg_query`.
- The line-1 defense is **fundamentally per-engine**: PG = libpg_query parse + deny-list walk; SQLite = the runtime authorizer (`backend_sqlite/authorizer.rs`) + descriptor-only-DDL (no raw SQL accepted); MySQL = ? (a MySQL parser, or sqlparser-rs, or a deny-by-allowlist over its own grammar).

The current handling is the **second-worst leak after expand-contract**, because it leaks into *three* places in the generic core:

1. **`engine.rs:150-151`** `plan()` branches on `cfg.dialect() == SqlDialect::Sqlite` and calls a bespoke `plan_sqlite_trusted` that skips the guard entirely. The core engine **knows about SQLite by name**.
2. **`executor.rs:1008`** the generic `apply_locked` body computes `run_string_guard = backend.dialect() != SqlDialect::Sqlite` and constructs `SqlGuard::new(cfg.guard_config())` **inline** (`executor.rs:1009`), gating the PG-only string guard behind a dialect check. The guard is **not behind the trait** — the core builds a PG guard and decides per-dialect whether to run it.
3. **`guard.rs:466-478`** `SqlGuard::check` itself fail-closes (`GuardError::SqliteRawSqlRejected`) when handed a SQLite config — a PG construct that has to know SQLite exists to refuse it.

So "the guard" is modeled as **a PG thing the core conditionally skips for SQLite**, rather than **a per-engine capability the core always invokes through one seam**. This is the abstraction's biggest conceptual gap: there is no `Guard` trait. Each engine's line-1 defense is wired by `if dialect == Sqlite` scattered across `engine.rs` and `executor.rs`.

### 1.4 `ExecutorConfig` (`db.rs`) — PG-shaped

`ExecutorConfig` (`db.rs:26-93`) is steeped in PG:
- `meta_schema` (`db.rs:37`) — PG schema namespacing; SQLite's journal is an attached `_mig` database (no schema concept), so this field is meaningless on SQLite.
- `migrator_role` (`db.rs:49`) — PG `SET ROLE`; SQLite has no roles (the authorizer is the confinement).
- `extension_schemas` (`db.rs:84`), `platform_schemas`, `platform_exts` — PG `CREATE EXTENSION` / multi-schema; no SQLite analog.
- `search_path_clause()` (`db.rs:248`), `statement_timeout` / `lock_timeout` mapped to PG `SET` GUCs (`db.rs:38-42`, `statement_timeout_ms`, `lock_timeout_ms`).
- `connect()` (`db.rs:298`) is `compio_postgres::connect`.

SQLite's `SqliteBackend` ignores almost all of this (it takes `app_path` + `journal_path` at `open()`, not from `ExecutorConfig`). So the config is a **PG config the SQLite backend mostly ignores** — the confinement model (journal location, role/authorizer, timeouts) does not generalize; it is PG-specific fields plus a SQLite backend that side-channels its own paths.

### 1.5 Drift snapshot / introspection — per-engine, but the SHAPE is clean

This is **mostly right already**:
- `SchemaSnapshot` (`drift.rs:429`) is engine-neutral (a `BTreeMap` of `TableSnapshot`), and `check_checksum_drift` / `compare_applied_to_set` / `diff_snapshots` (`drift.rs:149`, `746`) are **pure, dialect-agnostic** comparisons. Both backends call `compare_applied_to_set` with their own `applied()` read (`backend_sqlite/mod.rs:353-366`). Good — this is the model the rest should follow.
- The introspection *queries* are per-engine: PG `drift::snapshot_schema` over `information_schema`/`pg_catalog` (`drift.rs:538`); SQLite `drift_sql::snapshot_schema` over `sqlite_master` + PRAGMAs. Both correctly sit behind `MigrationBackend::snapshot_schema`. Good.

Minor leak: `DriftError::Db(#[from] compio_postgres::Error)` (`drift.rs:73`) — a PG-specific arm on a "shared" error. SQLite uses `DriftError::Backend(String)`. The PG `from` is convenient but it's PG vocabulary in the shared error enum (same pattern as `ApplyError::Db`). Acceptable but noted.

Some `ColumnSnapshot` / `IndexSnapshot` fields are PG-flavored emission metadata (`access_method: "btree"`/`ivfflat`, `opclass`, `geography(POINT,4326)` canonicalization at `drift.rs:522`). These are *emission-only* (excluded from equality), so they don't break SQLite drift comparison — but a third engine's DDL emitter must understand them or leave them defaulted.

### 1.6 DDL emission — `zeroship-schema` `query.rs` is dialect-parametrized but a CLOSED 2-set

`query.rs` threads `SqlDialect` (`query.rs:96-106`, only `Postgres | Sqlite`) and `SqliteEmitScope` (`query.rs:593`) through every emit site (`def_to_column_type_for_dialect` `query.rs:2010`, `build_system_field_columns` `query.rs:980`, FK clauses, constraints). The dialect is a **`match` over a 2-variant enum** — every `match dialect { Postgres => …, Sqlite => … }` is exhaustive, so adding `MySql` is a compile-time fan-out across ~20 match sites in `query.rs` (and `SqliteEmitScope` is SQLite-specific bleed in the shared emitter). It is **parametrized, not extensible**: a new engine touches every match arm rather than implementing a `Dialect` trait. This is a real (but mechanical) refactor cost, distinct from the migrate-core leaks.

### 1.7 Journal / locking / baseline — logical shape shared, SQL per-engine, baseline NOT behind the trait

- **Journal logical model is shared** (shared `event_seq`, append-only, net-state via window function) — `AppliedEntry` is dialect-neutral and `applied`/`superseded_versions`/`latest_completed_checksums`/`ensure_journal` are all behind the trait. PG SQL in `journal.rs`; SQLite SQL in `backend_sqlite/journal_sql.rs`. **Good.**
- **Locking is behind the trait** (`acquire/release_project_lock`): PG `pg_advisory_lock`; SQLite no-op (single-actor serialization). **Good.** *But* the **backfill** path takes a *second*, per-batch `pg_advisory_xact_lock` (`backfill.rs:899`) outside the trait — part of the expand-contract leak (§1.2).
- **Baseline is NOT behind the trait.** `baseline::baseline` (`baseline.rs:116`) is PG-`&Client`-typed; SQLite added a *parallel* inherent method `SqliteBackend::baseline_sqlite` (`backend_sqlite/mod.rs:122`) called directly by the dev tier (`plugin-db/src/register_model/sqlite_engine.rs:313`). So baseline is **two divergent code paths** keyed by the caller knowing the engine — not one trait method. A third engine would add a *third* `baseline_*`. **Leak.**

### 1.8 Confinement / TrustProfile / role model — PG role vs SQLite authorizer, not unified

The security model is split:
- PG: `TrustProfile` (`guard.rs:57`) drives the deny-list width; the least-priv `migrator` role (`role.rs`, `SET ROLE`) is line-2; `OperatorCapability` token gates `Platform`/`Trusted`.
- SQLite: the `prepare`-time authorizer (`backend_sqlite/authorizer.rs`) + per-file isolation is line-2; there is no role, and `Platform` "fail-closes to Confined" (`guard.rs:221-235`, `for_dialect`).

`TrustProfile` is **PG-centric** (`Platform` is "the zeroship platform schemas" — a multi-schema PG concept with no SQLite meaning). The model "works" for two engines via fail-close, but there is no **per-engine confinement abstraction** — line-2 is "PG role OR SQLite authorizer" hard-coded by dialect, not "the backend's confinement strategy". MySQL (which *does* have roles/users and `DEFINER`) would need a third hand-wired confinement story.

### 1.9 The public CLI (#55) — PG-only

`bin/zeroship-migrate.rs` + `guard/platform_runner.rs` are **entirely PG**: `connect(&cfg.database_url)` + `PostgresBackend::new(&conn)` (`platform_runner.rs:306-317`), `pg_dump` for `dump` (`bin:393`), "Postgres DSN" CLI help (`bin:48,61`). The CLI cannot target SQLite at all today, even though the trait it ultimately calls (`run_migrate` → `apply_verified` over `MigrationBackend`) is generic. The CLI is the natural place the multi-engine seam pays off.

### Leak inventory (file:line summary)

| # | Leak | Location | Severity |
|---|------|----------|----------|
| L1 | `expand_conn() -> Option<&Client>` raw PG connection on the trait | `backend.rs:245`, `:410`; `backend_sqlite/mod.rs:421` | **Critical** |
| L2 | Online expand-contract / backfill entirely PG, not behind the trait | `engine.rs:929-964,550-570`; `backfill.rs:*`; `expand_contract.rs:*` | **Critical** |
| L3 | No `Guard` trait — core branches `if dialect == Sqlite` to skip/build the PG `SqlGuard` | `engine.rs:150`; `executor.rs:1008-1009`; `guard.rs:466-478` | **High** |
| L4 | `ExecutorConfig` PG-shaped (`meta_schema`, `migrator_role`, `search_path`, GUC timeouts, ext schemas) | `db.rs:26-93,248-287,298` | **High** |
| L5 | Baseline not behind the trait — parallel `baseline()` (PG) vs `baseline_sqlite()` | `baseline.rs:116`; `backend_sqlite/mod.rs:122` | **High** |
| L6 | `query.rs` dialect is a closed 2-variant `match`, not an extensible `Dialect` | `query.rs:96-106` + ~20 match sites; `SqliteEmitScope` | **Medium** (mechanical) |
| L7 | `SessionSnapshot` fields are PG GUC strings on a "neutral" type | `backend.rs:58-67` | Low |
| L8 | non-txn pass is two mandatory trait methods (a PG-ism) every engine must stub | `backend.rs:119-136` | Low |
| L9 | `DriftError::Db(compio_postgres::Error)` / `ApplyError::Db(…)` PG arms on shared errors | `drift.rs:73`; `executor.rs:121` | Low |
| L10 | `TrustProfile::Platform` is a PG multi-schema concept; confinement is dialect-hardwired, not a backend strategy | `guard.rs:57,221`; `role.rs` vs `authorizer.rs` | Medium |
| L11 | Public CLI is PG-only (`connect`, `PostgresBackend`, `pg_dump`, "Postgres DSN") | `bin/zeroship-migrate.rs`; `platform_runner.rs:306-317` | Medium |
| L12 | **Shadow-DB dry-run is entirely PG** (admin `&Client`, `CREATE DATABASE`, second PG session, `provision_migrator`) — and it is a CORE SECURITY mechanism (the pre-apply confinement check for untrusted/AI-authored DDL). Exposed neutrally via `MigrationEngine::dry_run` (`engine.rs:801-822`). A non-PG deploy with **no** dry-run is a **confinement regression**, not a cosmetic leak. | `shadow.rs:*`; `engine.rs:801-822` | **Critical** |
| L13 | `squash()` (PG `Client` + `pg_advisory`) is a **public API** (`lib.rs:148`); §2.1 (now corrected) wrongly listed it CORE. It is per-engine journal/baseline record-keeping, not core. | `squash.rs:*`; `lib.rs:148` | High |
| L14 | `evaluate_preconditions` impl is PG-only (`pg_query::parse` + `information_schema` over `&Client`); §1.1 (now corrected) wrongly listed it clean. Neutral return type, PG-only body — same shape as `snapshot_schema`. | `precondition.rs:348,368,424-425` | High |
| L15 | **MySQL has no atomic DDL+journal txn** (`backend.rs:103-105` contract); the §3.3 "`non_txn()→None`" sketch leaves MySQL with **no valid apply path**. The abstraction is PG+SQLite-shaped here — this is the true multi-engine crux. | `backend.rs:103-105`; `journal.rs:11-15` (two-phase recovery) | **Critical** |
| L16 | `GuardReport.classes: Vec<StatementClass>` carries PG `DdlKind`/libpg_query node names (`classify.rs:50`) into the **public** `PlanItem.report` (`engine.rs:46`); the core only reads `destructive` + advisories (`engine.rs:171`). | `classify.rs:50`; `engine.rs:46,171` | High |
| L17 | `OnlineSchemaChange` (`expand_conn`) seam leaks PG-DDL via its payload: the **neutral** declarative differ constructs `Vec<ExpandContractPlan>` (PG-authored migs) for every dialect (`declarative.rs:1861,2225`; `engine.rs:289`). | `declarative.rs:1861,2225`; `engine.rs:289` | High |
| L18 | The **declarative differ** (`DeclarativeAuthor`) is the *largest* SQLite coupling: ~15 `is_sqlite`/`SqlDialect::Sqlite` branches drive DDL emission (`dialect` at `declarative.rs:2022`; branches `:2251–:3602`). Its **comparison** (`diff_snapshots`/`SchemaSnapshot`) is neutral; its **DDL emission** is per-engine. Bigger than the guard phase (L3). | `declarative.rs:2022,2251-3602` | **Critical** |
| L19 | SQLite **authorizer is a runtime two-mode `Arc<AtomicU8>`** flipped between phases (`authorizer.rs:11-14,60-62`), not static config — so confinement cannot be a flat `Confinement` enum the core constructs; it is a strategy object owning its own lifecycle. | `backend_sqlite/authorizer.rs:11-14,60-62` | Medium |

---

## 2. The clean multi-engine architecture

### 2.1 Engine-agnostic CORE vs per-BACKEND surface

The split — what stays **dialect-free** in the core vs what each engine implements:

**CORE (no dialect, no `if engine ==`):**
- Migration ids / checksums / `MigrationFlags` (`migration.rs`).
- The journal **logical** model: net-state, ordering (`order_pending`), the *supersession-gating computation* (`executor::compute_superseded` — pure, neutral), repeatable gating (`executor.rs` orchestration; `journal::AppliedEntry`).
  - **CORRECTION (post-critic):** the supersession *gating computation* is neutral, but `squash.rs` itself — the **public** `squash()` API (`lib.rs:148`) that records the supersession-by-baseline — is **PG-coupled**: a PG `Client` + `pg_advisory` lock, no trait seam. So the *journal record-keeping behind squash* is per-engine, like baseline (squash is "baseline a contiguous prefix as superseded"). Do NOT list `squash` as CORE; list it under per-backend journal/baseline. See L13 + Revision §R.C3.
- The drift **comparison**: `compare_applied_to_set`, `diff_snapshots`, `SchemaSnapshot` shape (`drift.rs`).
- Approval / manifest integrity (`approval.rs`, `manifest.rs`).
- The apply/rollback **orchestration shell**: lock → ensure-journal → pending diff → checksum pre-check → first-pass static validation → second-pass execute → repeatable phase. This already lives generically in `apply_locked` / `apply_with_lock_backend` and **must shed its two `if dialect == Sqlite` branches** (L3).

**Per-BACKEND (each engine implements):**
- apply (txn; optional non-txn), rollback `down`.
- journal I/O (its own SQL, returning the neutral `AppliedEntry`).
- lock (advisory / in-process / none).
- drift introspection (`snapshot_schema`).
- **the line-1 + line-2 guard** (its own parser/authorizer/allowlist).
- DDL emission (its own dialect emitter, or a dialected `zeroship-schema`).
- baseline.
- expand-contract (or "unsupported → route to offline rebuild").
- confinement (role / authorizer / DEFINER strategy).

### 2.2 The trait set a new engine implements

Replace the single fat `MigrationBackend` (with its `expand_conn` escape hatch) with a **small set of focused traits**, all dialect-neutral, no raw `Client` anywhere:

```
trait MigrationBackend {                 // execution + journal + introspection
    fn dialect(&self) -> EngineId;       // opaque id, not a closed enum the core matches on
    fn guard(&self) -> &dyn MigrationGuard;       // line-1; replaces the inline SqlGuard
    // session/lock
    async fn acquire_project_lock / release_project_lock(...);
    async fn snapshot_session / restore_session / reset_confinement_best_effort(...);
    // confined apply
    async fn apply_up_transactional(...);
    fn non_txn(&self) -> Option<&dyn NonTxnApply>;   // OPTIONAL capability (PG: Some, SQLite/MySQL: None)
    async fn rollback_one_transactional(...);
    // journal (neutral owned rows)
    async fn ensure_journal / applied / superseded_versions / latest_completed_checksums(...);
    async fn baseline_one(&self, m, applied_by) -> Result<BaselineOutcome, ApplyError>;  // L5 fixed
    // drift
    async fn check_checksum_drift / snapshot_schema(...);
    // preconditions
    async fn evaluate_preconditions(...);
    // structured ops
    async fn rebuild_one(...);  // engines without native ALTER; PG/MySQL may reject
    fn online(&self) -> Option<&dyn OnlineSchemaChange>;  // OPTIONAL; replaces expand_conn (L1/L2)
}

trait MigrationGuard {                   // line-1, per-engine (L3)
    fn check(&self, up: &str) -> Result<GuardReport, GuardError>;
    // PG: libpg_query deny-list. SQLite: descriptor-only (reject raw SQL) — the
    // authorizer is line-2 inside apply. MySQL: its own parser/allowlist.
}

trait OnlineSchemaChange {               // the expand-contract seam (L2), dialect-neutral
    async fn run_expand(&self, plan, approval, cfg, applied_by, lock_mode)
        -> Result<ApplyOutcome, OnlineError>;
    // PG: ADD COLUMN + dual-write trigger + paged backfill + journal E3.
    // SQLite: None (renames route to rebuild_one). MySQL: INPLACE/INSTANT or copy.
}

trait NonTxnApply {                      // optional PG-style non-txn two-phase (L8)
    fn validate_non_txn(&self, m) -> Result<(), ApplyError>;
    async fn configure_session_non_txn(...);
    async fn apply_up_non_transactional(...);
}
```

Key moves:
- **`guard()` on the backend** dissolves L3: the core *always* calls `backend.guard().check(up)` in the first pass and in `plan()` — no `if dialect == Sqlite`, no inline `SqlGuard`. PG's guard is `PgGuard(SqlGuard)`; SQLite's is `SqliteDescriptorGuard` (accepts the empty-report descriptor-diff path, rejects raw SQL); MySQL's is its own. The `GuardReport` / `GuardError` types stay shared and neutral.
- **`online()` Optional capability** dissolves L1/L2: no `Client` on the trait. `apply_declarative_locked` asks `backend.online()`; `Some` drives expand-contract through the **neutral** `OnlineSchemaChange::run_expand` (PG impl owns the `Client` internally), `None` means "this engine has no online rename — renames must already be routed to `rebuild_one`" (the existing SQLite invariant, now expressed as a capability instead of a `None`-`Client` sentinel).
- **`baseline_one` on the trait** dissolves L5: one method, two impls (PG wraps `baseline::baseline`, SQLite wraps `baseline_sqlite`). The dev tier stops calling `baseline_sqlite` directly.
- **`non_txn()` Optional** dissolves L8: PG returns `Some`; SQLite/MySQL return `None` and the core skips the non-txn pass entirely (instead of every engine stubbing two error-returning methods).
- **`EngineId` opaque** (not a closed enum the core matches): the core never `match`es on the engine. The only legitimate "what engine am I" consumers are the backend's own impls. (`SqlDialect` stays as the *emission* dialect inside `zeroship-schema`, see §2.3.)

### 2.3 DDL emission: dialected `zeroship-schema` (L6)

Two viable shapes:
- **(A) `Dialect` trait** in `zeroship-schema`: replace the `match dialect { … }` fan-out with `trait Dialect { fn column_type(&self, def) -> String; fn timestamp_default(&self) -> &str; fn fk_clause(...); … }` and `struct PgDialect`, `struct SqliteDialect`, (future) `struct MySqlDialect`. Adding an engine is one impl, not 20 match arms. `SqliteEmitScope` becomes a SQLite-dialect-internal concern, off the shared signature.
- **(B) keep the enum, add `MySql`** — cheaper now, but every future engine re-pays the fan-out. Rejected for the stated goal ("adding an engine is implement-the-trait").

Recommend **(A)**, but note it is the **largest mechanical refactor** and is *separable* from the migrate-core trait work (it can land independently — the migrate engine consumes whatever `zeroship-schema` emits). Phase it after the core trait split so the two don't entangle.

### 2.4 `ExecutorConfig` generalization (L4)

Split the PG-specific knobs off the shared config into a **backend-owned confinement bundle**:

```
struct ExecutorConfig {            // engine-neutral
    project_id, project_schema,    // logical names (project_schema = the SQLite app, the PG schema)
    timeouts: Timeouts,            // neutral { statement, lock } — each backend maps to its mechanism
    confinement: Confinement,      // opaque, backend-interpreted
}
enum Confinement {
    None,                          // Trusted / dev
    PgRole { migrator_role, meta_schema, search_path: …, extension_schemas, platform_* },
    SqliteAuthorizer { app_path, journal_path },   // folds today's side-channel open() args in
    // MySqlDefiner { … }          // future
}
```

The core reads only `project_id` / `project_schema` / `timeouts`; everything PG-specific moves into the `PgRole` arm the PG backend interprets. SQLite stops ignoring an ExecutorConfig-with-PG-fields and instead carries its real inputs (`app_path`/`journal_path`) in `SqliteAuthorizer` — unifying the today-split "config for PG, open() args for SQLite". `connect()` (`db.rs:298`) moves to a PG-backend constructor, not the shared `db` module.

### 2.5 Confinement / trust generalization (L10)

`TrustProfile` (Confined/Platform/Trusted) is genuinely a **PG concept** (multi-schema platform allowlist). Keep it as the **PG backend's** confinement policy, not a core type. The core's only trust question is the **gate** (destructive → approval), which is already engine-neutral (`MigrationPlan::requires_approval`). Line-2 confinement becomes "whatever `Confinement` the backend was built with" — PG role, SQLite authorizer, MySQL DEFINER — each enforced *inside* that backend's `apply_up_transactional`. The `OperatorCapability` token stays a PG/CLI concern.

---

## 3. SQLite-first, PG byte-identical, MySQL-as-sketch

### 3.1 SQLite is the reference clean impl

The acid test of the abstraction: **the core contains zero `if dialect == Sqlite`**. Today there are three (`engine.rs:150`, `executor.rs:1008`, `guard.rs:476`). After the refactor:
- `plan()` calls `backend.guard().check()` uniformly; `plan_sqlite_trusted` is deleted (its behavior becomes `SqliteDescriptorGuard::check` returning the empty report).
- `apply_locked` deletes `run_string_guard` and `SqlGuard::new(...)`; it calls `backend.guard().check()` for every engine.
- `SqlGuard` no longer needs a `Sqlite` dialect arm or `SqliteRawSqlRejected` — `SqlGuard` becomes purely the PG guard; SQLite-raw-rejection lives in `SqliteDescriptorGuard`.

When those three branches are gone and the SQLite suite still passes, the abstraction is proven clean.

### 3.2 PG fits the SAME trait, byte-identical

Every PG leak is refactored **behind** the new seams with **no behavior change**:
- `PgGuard` wraps the existing `SqlGuard` verbatim → same denials.
- `PgOnlineSchemaChange` owns the `Client` and calls today's `run_expand_with_lock` body verbatim → same expand-contract.
- `PgBackend::baseline_one` calls `baseline::baseline` verbatim.
- `Confinement::PgRole` carries today's fields → same `search_path`/`SET ROLE`/timeouts.
The **entire `tests/*_pg.rs` suite is the regression bar** and must pass unchanged at every phase.

### 3.3 MySQL as a validation sketch (do NOT build)

Confirm the trait suffices by sketching what MySQL would implement:
- `MySqlGuard` — a MySQL-grammar deny-list (sqlparser-rs or a MySQL parser) or descriptor-only like SQLite. Plugs into `MigrationGuard`. ✔ (this is *why* the guard must be a trait, not PG-libpg_query in the core.)
- `non_txn()` → `None` (MySQL DDL mostly auto-commits; no PG-style CONCURRENTLY two-phase). ✔ optional capability.
- `online()` → `Some(MySqlOnline)` driving `ALTER TABLE … ALGORITHM=INPLACE/INSTANT` (or a copy-and-swap) — **and this is the payoff**: MySQL *has* online rename, so it needs the `OnlineSchemaChange` seam that `expand_conn() -> Option<&Client>` could never provide. ✔ proves L1/L2's fix is necessary, not cosmetic.
- `snapshot_schema` over `information_schema` (MySQL flavor) → neutral `SchemaSnapshot`. ✔
- journal SQL over an InnoDB table; lock via `GET_LOCK()`; confinement via a least-priv MySQL user / `DEFINER`. ✔
- DDL via `MySqlDialect` in `zeroship-schema`. ✔ (needs §2.3 (A).)

No core change in any of the above — that is the goal met.

---

## 4. Reconcile with the public CLI (#55) and P6c

### 4.1 CLI (L11)

Generalize `RunConfig` to carry an **engine selector** (derive from the DSN scheme: `postgres://` vs `sqlite:`/a file path — the same "select by DB URL" model the dev tier already uses per the production-wiring design). `platform_runner::run_migrate` builds the right backend (`PgBackend` vs `SqliteBackend`) and the rest (`apply_verified` over `MigrationBackend`) is already generic. `dump` dispatches to `pg_dump` vs a `sqlite_master` dump per engine. The CLI's `--profile trusted/platform/confined` stays a **PG-only** flag (Platform/Trusted are PG postures); SQLite is implicitly its own confinement. This makes the dbmate-like CLI genuinely multi-DB with the engine's own guard/DDL/introspection.

### 4.2 P6c — does multi-engine subsume or reorder it?

P6c (per the production-wiring design §9) bundles **two orthogonal things**:
1. **SQLite online expand-contract** — this is **exactly L2 / `OnlineSchemaChange`**. The multi-engine abstraction **subsumes** it: once `online()` is a capability trait, "SQLite online expand-contract" is just "implement `OnlineSchemaChange` for SQLite" (if ever wanted) — but SQLite's *correct* answer is likely to **stay `None`** (route renames to offline rebuild; SQLite has no cheap online rename). So the abstraction **reframes** P6c-part-1 from "add a PG-shaped path to SQLite" to "SQLite opts out of the online capability, cleanly." The *seam* (this design) is the prerequisite; the SQLite *online impl* may never be needed.
2. **Build-side `generate`** (schema.ts → versioned migration files in the `.zship`) — this is **orthogonal DX**, untouched by the engine abstraction. It neither subsumes nor is subsumed; sequence it independently.

**Recommendation:** do the **multi-engine abstraction FIRST** (it is the foundation that makes both the CLI and any future online path engine-agnostic), which **retires P6c-part-1 as a standalone item** (it becomes "SQLite returns `None` from `online()`", already the behavior). Keep **build-side `generate`** as a separate DX track, unaffected.

---

## 5. Phase plan (each step independently landable + green)

The PG suite is the byte-identical bar at **every** phase.

- **Phase 0 — extract `MigrationGuard` trait (L3).** Add `MigrationGuard`; `PgGuard(SqlGuard)` + `SqliteDescriptorGuard`. Route `plan()` and `apply_locked` through `backend.guard()`. Delete the three `if dialect == Sqlite` branches and `plan_sqlite_trusted`. **This is the highest-value, cleanest extraction** — it removes the core's by-name knowledge of SQLite. Clean-extraction (no redesign): the two guard behaviors already exist; this just puts them behind a seam.

- **Phase 1 — `baseline_one` on the trait (L5).** One method; PG/SQLite wrap their existing impls; dev tier calls the trait. Clean extraction.

- **Phase 2 — `non_txn()` + `online()` optional capabilities (L1, L2, L8).** Remove `expand_conn`/`Client` from the trait; introduce `OnlineSchemaChange` (PG impl = today's `run_expand_with_lock` body verbatim, owning the `Client`) and `NonTxnApply`. `apply_declarative_locked` asks `backend.online()`. **Part clean-extraction (move PG code behind a trait), part real design** (defining the neutral `run_expand` signature so a non-PG engine could implement it). The SQLite `None` arm is unchanged behavior.

- **Phase 3 — `ExecutorConfig` / `Confinement` split (L4, L10).** Move PG knobs into `Confinement::PgRole`; fold SQLite's `app_path`/`journal_path` into `Confinement::SqliteAuthorizer`; neutral `Timeouts`. Moderate refactor (touches every config call site) but mechanical. `connect()` → PG backend constructor.

- **Phase 4 — `zeroship-schema` `Dialect` trait (L6).** Replace the `match dialect` fan-out with `trait Dialect` + `PgDialect`/`SqliteDialect`. Largest mechanical change; **separable** and can land in parallel with 0–3 (it's a different crate). Byte-identical PG/SQLite emission is the bar.

- **Phase 5 — CLI multi-engine (L11).** DSN-scheme engine selection; SQLite `dump`. Depends on 0–2.

- **Phase 6 (validation, not code) — MySQL sketch review.** Walk the trait set against the §3.3 MySQL sketch in design review; fix any seam that the sketch reveals as still-PG-shaped. No MySQL impl ships.

**Clean-extraction vs real-redesign:**
- *Clean extraction* (low risk, behavior already exists, just re-seamed): Phases 0, 1, the PG-side of 2, most of 4.
- *Real redesign* (needs design judgment): the **neutral `OnlineSchemaChange::run_expand` signature** (Phase 2 — getting it engine-agnostic without a `Client`, while PG keeps a `Client` internally and SQLite/MySQL differ structurally) and the **`Confinement` enum** (Phase 3 — modeling three confinement strategies without leaking any one engine's vocabulary).

---

## 6. The two hardest problems + how to solve them

### 6.1 The guard (#3) — line-1 is fundamentally per-engine

**Problem:** PG's line-1 is libpg_query (a real PG parser, chosen precisely so a deny-list can't be bypassed by exotic syntax). SQLite has *no* equivalent line-1 parser in the engine — its line-1 is "accept ONLY descriptor-diff-generated DDL (no raw SQL)" + the runtime authorizer as line-2. MySQL would need its own. There is no shared parser.

**Solution:** Do **not** try to unify the *implementation* — unify the *seam*. `MigrationGuard::check(up) -> Result<GuardReport, GuardError>` is the contract; `GuardReport`/`GuardError` are neutral; each engine brings its own line-1:
- `PgGuard` = libpg_query deny-list (today's `SqlGuard`, verbatim).
- `SqliteDescriptorGuard` = "raw SQL → reject; descriptor-diff DDL → empty report" (today's `plan_sqlite_trusted` + `SqliteRawSqlRejected`, now a real impl instead of a core `if`).
- Line-2 stays inside `apply_up_transactional` (PG role / SQLite authorizer) — already per-backend.

The insight: the core's job is "**always run line-1 through the backend's guard, then run line-2 inside the backend's confined apply**" — it must **never** assume *which* parser. Removing the inline `SqlGuard::new` from `executor.rs:1009` and the `dialect == Sqlite` skip is the concrete fix. This is mostly clean extraction; the only judgment is making `GuardReport` rich enough that a non-PG guard can populate it (it already is — `classes`/`destructive`/`advisories`, and SQLite already returns an empty one).

### 6.2 Expand-contract (#2) — the PG-shaped online path

**Problem:** `run_expand`/`run_backfill` are deeply PG (PL/pgSQL dual-write triggers, `pg_advisory_xact_lock`, paged `UPDATE`, `pg_query::parse`), reached via the `expand_conn() -> Option<&Client>` escape hatch. It is *structurally* PG-only, and the trait leaks a raw `Client` to admit it.

**Solution:** Model online schema change as an **optional capability trait** `OnlineSchemaChange`, dialect-neutral at the seam, engine-owned internally:
- `MigrationBackend::online(&self) -> Option<&dyn OnlineSchemaChange>`.
- `PgOnlineSchemaChange` owns its `Client` and runs today's expand-contract **verbatim** (byte-identical). The `Client` never appears on a shared trait — it's a private field of the PG impl.
- `SqliteBackend::online()` → `None` (renames already route to `rebuild_one`; `plan.renames` is empty). The current `None`-`Client` sentinel becomes an honest `None`-capability.
- `apply_declarative_locked` (`engine.rs:550`) replaces the `backend.expand_conn()` pull + fail-close with `match backend.online() { Some(o) => o.run_expand(...), None if !renames.is_empty() => Err(routing bug), None => Ok }` — same control flow, no `Client`.
- `backfill.rs` / `expand_contract.rs` move under the PG backend (or a `pg/online` module) — they are PG impl detail, not engine core.

The hard part is **the neutral `run_expand` signature**: it must take `(plan: &ExpandContractPlan, approval, cfg, applied_by, lock_mode)` and return `ApplyOutcome`/`OnlineError` without a `Client`. PG threads its own `Client`; a future MySQL impl threads its own connection and a *different* mechanism (INPLACE alter). `ExpandContractPlan` today carries PG-authored DDL migrations + a `BackfillSpec` — to be truly neutral it should carry the **intent** (rename `from`→`to` on `table`, type `ty`) and let each engine author its own expand/backfill, OR stay PG-authored and simply be PG-only (SQLite/MySQL author their own plans). **Recommendation:** keep `ExpandContractPlan` as the PG intent today (don't over-generalize a plan only PG produces), make `OnlineSchemaChange` the seam, and revisit a neutral online-intent only if/when MySQL online lands. This is the **real-redesign** kernel; everything around it is extraction.

---

## 7. Top risks / open questions (for a design-critic)

1. **`OnlineSchemaChange` signature neutrality.** Is keeping `ExpandContractPlan` PG-authored (and making SQLite/MySQL author their own) the right call, or should the *plan* be a neutral online-intent the backend lowers? Over-generalizing now risks a wrong abstraction with only one producer (PG); under-generalizing risks a second leak when MySQL online lands. **Recommend the conservative seam-only fix; flag the plan shape as deferred.**

2. **Does `EngineId` need to be opaque, or is a closed `enum` fine?** The core must not `match` on it, but the CLI/dev-tier legitimately select a backend by DSN scheme. An opaque id + a registry is cleanest; a closed enum is simpler but re-introduces the "core knows every engine by name" smell the moment someone `match`es it. **Lean opaque, but it's a judgment call.**

3. **`zeroship-schema` `Dialect` trait scope (Phase 4).** It's the biggest mechanical change and lives in a *shared* crate (plugin-db, runtime, migrate all consume `query.rs`). Risk: the refactor ripples beyond migrate. Mitigation: it's behavior-preserving and separable — but is it worth doing *now* (for a hypothetical MySQL) vs deferring until a 3rd engine is real? **Open: do we build the `Dialect` trait speculatively, or add `MySql` to the enum only when MySQL is actually scheduled?** The "implement-the-trait" goal argues for now; YAGNI argues for defer.

4. **Confinement enum vs trait.** `Confinement` as a closed enum (`PgRole`/`SqliteAuthorizer`/`MySqlDefiner`) re-introduces a closed set the way `SqlDialect` did. A `dyn ConfinementStrategy` is more open but heavier. Which?

5. **`SchemaSnapshot` PG-flavored emission fields** (`access_method`, `opclass`, `geography(...)`). They're emission-only and excluded from equality, so they don't break cross-engine *drift*, but a MySQL DDL emitter must understand or ignore them. Is the snapshot truly engine-neutral, or PG-neutral-plus-SQLite-tolerated? Likely fine, but worth a critic's eye on whether a MySQL spatial/index type would round-trip.

6. **Test bar honesty.** The regression bar is `tests/*_pg.rs` + the SQLite suites. The *real* proof of "a new engine is just a trait impl" can't be tested without building one. The MySQL sketch (§3.3) is a paper proof — a critic should pressure-test whether any sketch step secretly needs a core change. **This is the single biggest unknown: we are asserting generality we can't execute until engine #3 exists.**

7. **Scope realism.** This is a refactor of a heavily-tested security engine. Phases 0–1 are genuinely small (clean extraction). Phase 2 (online capability) and Phase 4 (`Dialect` trait) are the large ones. The honest framing: **most of the leak count is tidy-up; two items (guard-seam done in P0, online-seam in P2) are the architecture, and one (Dialect trait) is a big mechanical sweep in a shared crate.** Don't let the leak *count* imply the *effort* is uniform.

---

## Revision (2026-06-21, post-critic)

A design-critic scored the first draft **54/100**. The verdict was precise and correct: *the named leaks are real, but the draft (a) over-claims cleanliness in three places, (b) mis-scopes which leak is the largest, and (c) mis-models true multi-engine generality — it describes a clean **PG+SQLite** abstraction and then hand-waves "MySQL is just a trait impl," when MySQL's auto-committing DDL breaks the core atomicity contract.* This section supersedes the relevant parts of §1–§6 wherever it conflicts. The inline `> CORRECTION` notes above and inventory rows **L12–L19** are part of this revision.

The through-line of the fix: **stop pretending the abstraction is already general.** It is *not* — it is PG-shaped with SQLite tolerated. The honest, buildable claim is: **"a clean PG+SQLite abstraction now; a 3rd engine (MySQL) becomes *pluggable* via two new capabilities — `JournalAtomicity` and per-engine DDL emission — neither of which exists today."** That is a real, defensible goal; "MySQL is a trait impl" was not.

### R.C1 — Re-scope: the declarative differ is the real SQLite coupling (CRITICAL)

<!-- Added post-critic: addressing C1 — the false "delete 3 guard branches → clean" success criterion -->

The draft's headline success criterion (§3.1: "the core contains zero `if dialect == Sqlite`; today there are three; delete them and the abstraction is proven clean") is **false**, and dangerously so — it would let the refactor declare victory with the largest coupling untouched.

`DeclarativeAuthor` (`declarative.rs`) carries a `dialect: SqlDialect` field (`:2022`) and **~15** `is_sqlite` / `matches!(self.dialect, SqlDialect::Sqlite)` branches across `:2251–:3602` (confirmed: `:2251`, `:2276`, `:2292`, `:2329`, `:2387`, `:2459`, `:3296`, `:3310`, `:3328`, `:3350`, `:3503`, `:3541`, `:3562`, `:3602`). These drive **DDL emission shape**, not comparison: FK inline-vs-defer, `ADD COLUMN` shape, index emission (system-field index skip), and table-reference qualification (schema-qualified PG vs unqualified SQLite `main`). This is **bigger than the guard phase** and the draft never gave it a phase.

**Reclassification of the declarative/diff path (correcting §1.5 and §2.1):**
- **`diff_snapshots` / `SchemaSnapshot` / `compare_applied_to_set` — NEUTRAL.** The *comparison* (what changed) is dialect-free. Stays CORE. (Unchanged.)
- **`DeclarativeAuthor`'s DDL *emission* — PER-ENGINE.** Turning a diff into `up`/`down` SQL is dialect-specific and must move behind the engine's DDL author.

**Decision — keep `DeclarativeAuthor` parametrized on the (closed) dialect; do NOT split into a per-backend DDL author *yet*.** Rationale:
- The differ already routes its SQLite new-table CREATE *through the shared `zeroship_schema::query` emitter* (`:2017–2021`), i.e. the real per-dialect DDL knowledge already lives in `zeroship-schema` (L6/L18). The `DeclarativeAuthor` branches are mostly *which shared-emitter scope to call* + structural routing (defer FK vs inline), not a second DDL dialect.
- Per **H3 (below) we keep `SqlDialect` a closed enum.** Given that, an exhaustive `match self.dialect` inside `DeclarativeAuthor` is **correct and idiomatic** — it is compile-time-checked completeness, not a leak. (This directly resolves the draft's M1 tension: the draft wanted both an *opaque* `EngineId` *and* exhaustive `match self.dialect` — incompatible. We drop the opaque goal.)

**New explicit phase (P1 in the revised plan):** *de-branch the declarative differ.* Concretely: (a) push every emission decision that is "spell this DDL fragment" down into the `zeroship-schema` dialect (so the differ holds *structural* routing only); (b) the residual structural branches (defer-FK, route-rename-to-rebuild) become a small, named, **closed** `match self.dialect` with one helper per arm, reviewed as the engine's emission policy — not scattered `is_sqlite` booleans. The bar: PG emission byte-identical; the SQLite branch count drops to the genuinely-structural minimum and each is justified in-comment. **This phase is sequenced before the JournalAtomicity work because it is the largest mechanical surface and gates a clean read of what's truly core.**

### R.C2 — The `JournalAtomicity` capability: the actual multi-engine crux (CRITICAL)

<!-- Added post-critic: addressing C2 — MySQL auto-commits DDL; the core "atomic DDL+journal" contract is impossible for it -->

This is **the** finding. The core contract at `backend.rs:103-105` is: *"`apply_up_transactional`: BEGIN; …; <up>; INSERT journal; COMMIT — DDL + journal row commit **atomically** in one txn."* **MySQL DDL auto-commits per statement** (every `CREATE/ALTER/DROP TABLE` issues an implicit commit), so there is **no** transaction in which `<up>` and the journal `INSERT` are atomic. The §3.3 sketch said "MySQL `non_txn()` → `None`" — but `non_txn` is the *only* non-atomic apply path, so that sketch leaves MySQL with **no valid apply path at all**: `apply_up_transactional` needs an atomicity MySQL cannot provide, and the only alternative was just set to `None`. The abstraction, as drafted, **cannot admit MySQL.** The "MySQL is a trait impl" claim was unfounded.

**The fix — model atomicity as a backend capability, and reuse the existing two-phase recovery as the generic non-atomic path.** Note the journal *already has* the mechanism: the two-phase `started → completed` protocol with crash recovery (`journal.rs:11-15`), built for PG's non-txn DDL (`CREATE INDEX CONCURRENTLY`). That same `started`-marker-then-`completed` shape is **exactly** what a non-transactional-DDL engine needs for *every* migration, not just concurrent-index ones. So the capability already exists in the codebase; we generalize *when it is used*.

```rust
/// How a backend can bind a migration's DDL to its journal row.
/// Reported by the backend; the executor selects the apply path from it.
enum JournalAtomicity {
    /// DDL and the journal INSERT commit in ONE transaction (PG default, SQLite).
    /// Executor path: `apply_up_transactional` (BEGIN…COMMIT).
    Transactional,
    /// DDL auto-commits and CANNOT share a txn with the journal write (MySQL;
    /// also PG's `CREATE INDEX CONCURRENTLY` per-migration). Executor path:
    /// the GENERIC two-phase `started → completed` protocol with idempotent
    /// crash recovery — `journal.rs`'s existing inflight machinery, promoted
    /// from "the non-txn special case" to "the apply path for this engine".
    InflightMarker,
}

trait MigrationBackend {
    /// Per-migration: most engines answer statically (MySQL: always
    /// `InflightMarker`; SQLite: always `Transactional`); PG answers
    /// per-migration (`Transactional` by default, `InflightMarker` for a
    /// `transaction:false` / CONCURRENTLY migration).
    fn journal_atomicity(&self, m: &Migration) -> JournalAtomicity;
    // …
}
```

**What this does to the executor.** The two-phase `started → completed` recovery (`journal.rs:11-15`) becomes the **generic apply path whenever the backend reports `InflightMarker`** — *not* a PG-only branch. The executor's apply loop becomes:

```
match backend.journal_atomicity(m) {
    Transactional  => backend.apply_up_transactional(...),      // BEGIN…<up>…INSERT…COMMIT
    InflightMarker => backend.apply_up_with_inflight(...),      // started-marker → <up> → completed (+recovery)
}
```

A non-transactional-DDL engine (MySQL) then **plugs in by *reporting* `InflightMarker`**, not by special-casing: it implements `apply_up_with_inflight` (write `started`; run the auto-committing `<up>`; write the immutable `completed`; clear marker) and inherits the *exact* crash-recovery semantics PG's non-txn path already has and is already tested for. This is **the** mechanism that makes the abstraction genuinely multi-engine rather than PG+SQLite-shaped: atomicity stops being an assumed property of the core and becomes a *declared capability* with two executor paths.

**This subsumes and replaces the draft's `non_txn()` capability (L8).** "Non-transactional apply" is no longer a PG-special-case pair of stub methods every engine must implement; it is the `InflightMarker` arm of `JournalAtomicity`, which PG selects per-migration and MySQL selects always. `validate_non_txn` becomes "does this backend accept a `transaction:false` migration," answered by whether it can report `InflightMarker`.

**Honesty (carried into the closing assessment):** `JournalAtomicity` is sound *as a capability shape*, but its **deeper correctness for MySQL needs its own design pass before we commit to MySQL-readiness** — specifically: (1) MySQL's implicit-commit boundaries mean a multi-statement `<up>` is *partially* committable, so the recovery model must define "is a half-applied `<up>` re-runnable?" (idempotent-DDL requirement, or a per-statement sub-journal); (2) `GET_LOCK()` session-lock semantics under the inflight protocol; (3) whether the `started` marker itself can be written transactionally on MySQL (it can — it's a single-row DML INSERT, which *is* transactional; only DDL auto-commits). We design the **capability** here; we do **not** claim the MySQL impl is shovel-ready. See the closing assessment.

### R.C3 — Three omitted PG-coupled subsystems, with seams (CRITICAL)

<!-- Added post-critic: addressing C3 — shadow.rs, squash.rs, precondition.rs were missing from the leak inventory -->

Added to the inventory as **L12 (shadow), L13 (squash), L14 (precondition)**; the inline `> CORRECTION` notes fix the §1.1 and §2.1 misclassifications. Seams:

**Shadow dry-run (L12) — a confinement mechanism, not a convenience.** `shadow.rs` runs untrusted/AI-authored DDL against a throwaway clone under the *exact* guard + migrator role before it ever touches the live project DB (`engine.rs:801-822`). It is entirely PG: admin `&Client`, `CREATE DATABASE`, a second PG session, `provision_migrator`. **A non-PG deploy path with no dry-run is a security regression**, not a missing nicety — the AI-authored-DDL confinement story *depends* on it. Seam:

```rust
trait ShadowDryRun {                 // per-engine; OPTIONAL but security-load-bearing
    async fn dry_run(&self, migrations, exec_cfg, applied_by) -> Result<DryRunReport, DryRunError>;
    async fn dry_run_declarative(&self, plan, desired, exec_cfg, applied_by) -> Result<DryRunReport, DryRunError>;
}
// PG: today's CREATE DATABASE clone + provision_migrator + UNMODIFIED executor::apply.
// SQLite: a throwaway temp app-file + the same authorizer + the same apply path
//         (cheaper than PG — copy the file, open with the hardened authorizer).
// MySQL: a throwaway schema/database under the least-priv user.
```
**Invariant we now state:** *no engine may expose an apply path for untrusted/AI-authored DDL without a `ShadowDryRun`.* A backend that returns `None` here is restricted to **trusted** (descriptor-diff-only) migrations — the dev SQLite tier qualifies (its DDL is descriptor-generated, never raw), so SQLite *may* defer `ShadowDryRun` initially **only** because its guard already forbids raw SQL. Any engine that accepts raw creator/AI SQL **must** provide it. Document this as a gating requirement on the confinement model.

**Squash (L13).** `squash()` (`squash.rs`, public `lib.rs:148`) is PG `Client` + `pg_advisory`. It is *journal record-keeping* ("baseline a contiguous prefix as superseded; do not run its `up`"), so it folds under the **per-engine journal/baseline** surface — same treatment as `baseline_one` (L5). Add `fn squash_one(&self, s, supersedes) -> Result<SquashOutcome, …>` to the backend (or to a `JournalAdmin` sub-trait alongside `baseline_one`). The neutral *supersession-gating computation* (`compute_superseded`) stays CORE; only the record-write is per-engine.

**Preconditions (L14).** `evaluate_preconditions`' return type is neutral but the impl is PG-only (`pg_query::parse` at `:424-425`; `information_schema` over `&Client` at `:348/:368`). It belongs in the per-engine column with `snapshot_schema`. The trait method stays; each backend implements its own existence checks (PG `information_schema`; SQLite `sqlite_master`/PRAGMA; MySQL `information_schema`) and its own single-`SELECT` validation (PG `pg_query`; SQLite the authorizer-on-a-prepared-statement; MySQL its parser). No core change — same shape as drift introspection.

### R.H1 — Lower the online seam to the neutral `OnlineIntent` (HIGH)

<!-- Added post-critic: addressing H1 — ExpandContractPlan is NOT PG-only-produced -->

§6.2 claimed `ExpandContractPlan` "is a plan only PG produces." **False.** The *neutral* declarative differ constructs `Vec<ExpandContractPlan>` for **every** dialect (`declarative.rs:1861`, `:2225`; surfaced at `engine.rs:289`). Since `ExpandContractPlan` carries **PG-authored DDL migrations** (`expand: Vec<Migration>`, PL/pgSQL trigger bodies, `BackfillSpec`), passing it through a "neutral" `OnlineSchemaChange::run_expand` seam means **PG DDL flows through the neutral seam** — the leak the seam was supposed to close.

**Fix — lower the seam to the *intent*, which already exists.** `expand_contract.rs:71` already defines a neutral `OnlineIntent::RenameColumn { table, from, to, ty }`. Make that the seam payload:

```rust
trait OnlineSchemaChange {
    async fn run_online(&self, intent: &OnlineIntent, approval, cfg, applied_by, lock_mode)
        -> Result<ApplyOutcome, OnlineError>;
    // PG: lowers OnlineIntent → today's ExpandContractPlan (ADD COLUMN + dual-write
    //     trigger + paged backfill + journal E3) INTERNALLY, owning its Client.
    // MySQL: lowers the SAME intent → ALTER TABLE … ALGORITHM=INPLACE/INSTANT.
}
```
The PG-authored `ExpandContractPlan` becomes a **PG-internal lowering** of `OnlineIntent`, never crossing the seam. The neutral differ emits `OnlineIntent`s; the PG backend lowers them to its DDL plan; a future MySQL backend lowers the *same* intent to `INPLACE` DDL.

**Invariant we now state explicitly (the SQLite safety the draft relied on but never asserted):** *SQLite never populates `renames`* — the declarative differ routes every SQLite existing-table change (including column rename) to `rebuilds` (the 12-step offline rebuild; `declarative.rs:2225` comment, `engine.rs:544-555`), so on SQLite `renames` is **always empty** and `online() == None` is **never reached with work to do.** This is a *structural* invariant of the differ, not an accident; the executor's fail-closed check at `engine.rs:550-555` (non-empty `renames` + `online()==None` ⇒ routing bug) enforces it. State it; test it.

### R.H2 — Narrow `GuardReport` to neutral (HIGH)

<!-- Added post-critic: addressing H2 — GuardReport.classes carries PG DdlKind/libpg_query node names into the public surface -->

`GuardReport.classes: Vec<StatementClass>` carries `DdlKind` — including `Other(String)`, the **raw libpg_query node name** (`classify.rs:49-51`) — into the **public** `PlannedMigration.report` (`engine.rs:46`). The core reads only `report.destructive` and the advisories (`engine.rs:171`). So PG parser vocabulary is exposed on a cross-engine type that consumers see.

**Fix — the public, cross-engine `GuardReport` exposes only `{ destructive: bool, advisories: Vec<Advisory> }`.** `classes` stays a **PG-guard-internal** detail (the PG guard uses it for its deny-list walk; it is never surfaced). If a consumer needs PG statement classes, it asks the PG guard, not the neutral report.

**Decision on advisories (the explicit call the critic demanded):** advisories are **a PG-only diagnostic, not a cross-engine contract.** `analyze.rs` is `pg_query`-only and the advisories (lock-escalation, full-table-rewrite, etc.) are PG operational hazards. We do **not** require every guard to populate them. The neutral `GuardReport.advisories` is therefore `Vec<Advisory>` that is **PG-populated, empty elsewhere** — and we **document the gap**: SQLite/MySQL guards return no advisories today; a MySQL advisory analyzer (e.g. "this `ALTER` copies the table") is a *future per-engine enhancement*, not a contract the core enforces. This is the honest position: the *seam* is cross-engine; the *advisory content* is PG-only until each engine grows its own analyzer. (Consistent with R.H2's principle: share the neutral shape, keep engine-specific richness inside the engine.)

### R.H3 — Decision: KEEP the closed `SqlDialect` enum; DROP the opaque-`EngineId`/`Dialect`-trait goal (HIGH)

<!-- Added post-critic: addressing H3 + M1 — trait-ifying SqlDialect ripples into the runtime data plane -->

The draft wanted an **opaque `EngineId`** (core never matches) *and* a **`Dialect` trait** in `zeroship-schema` (§2.3 option A). Both are now **rejected.** `SqlDialect` (zeroship-schema `query.rs:96`, **155 references**) is **not** a migrate-only emission detail — it is consumed by **plugin-db's data plane**: `write_pipeline.rs:214/250/754/792`, `register_model/sqlite_engine.rs:131`, `backend/sqlite/dialect.rs:31`. Trait-ifying it ripples into the **runtime write path**, contradicting the draft's "byte-identical for consumers" claim, and it conflicts with `DeclarativeAuthor`'s exhaustive `match self.dialect` (the draft's own M1 tension).

**Decision: keep `SqlDialect` a closed enum. Add a `MySql` variant *when MySQL is real* (YAGNI).** The exhaustive `match dialect { Postgres => …, Sqlite => … }` fan-out across `query.rs` (~20 sites) and `DeclarativeAuthor` is then *correct, compile-checked* code — adding `MySql` makes the compiler enumerate every site that needs a MySQL spelling, which is *exactly the review surface you want* for a new engine's DDL, not a smell to hide behind a trait. This **supersedes §2.3 (recommend A)** and **§2.2's `EngineId` opaque** — the core does not need an opaque id; it needs to *not branch on the dialect for control flow* (which the guard/online/journal seams already deliver), while DDL *emission* legitimately stays an exhaustive closed match. **L6 is therefore downgraded** from "build a `Dialect` trait" to "add the `MySql` enum arm when scheduled."

### R.M2 — Confinement as a per-backend strategy object, not a core-constructed enum (MEDIUM)

<!-- Added post-critic: addressing M2 — the SQLite authorizer is a runtime two-mode atomic, not static config -->

The draft modeled confinement as a flat `Confinement` enum (§2.4/§2.5) the **core constructs** (`PgRole{…}` / `SqliteAuthorizer{app_path,journal_path}`). That mis-models SQLite: its line-2 confinement is a **runtime two-mode `Arc<AtomicU8>`** (`authorizer.rs:11-14`) installed once at connection-open and **flipped between `CreatorUp` and `EngineJournal` phases during a single apply** (`authorizer.rs:60-62`) — the creator `<up>` runs under `CreatorUp` (writes to `_mig` denied), then the engine flips to `EngineJournal` to write its own journal row. This is **lifecycle the core cannot own**; a flat enum the core constructs can't express "flip my mode after the creator `<up>`, before the journal write."

**Fix — confinement is a per-backend strategy object that owns its own lifecycle**, constructed *by the backend*, not the core:

```rust
trait Confinement {
    async fn enter_creator(&self);   // PG: SET LOCAL ROLE migrator.  SQLite: mode.store(CreatorUp).
    async fn enter_journal(&self);   // PG: RESET ROLE (engine writes as connector).
                                     // SQLite: mode.store(EngineJournal) — the atomic flip.
    async fn reset_best_effort(&self);
}
```
The backend builds its own `Confinement` and calls `enter_creator()` / `enter_journal()` *inside* its `apply_up_transactional` (and the `InflightMarker` path), straddling the creator-`<up>` / journal-write boundary. **How the mode-flip survives the abstraction:** it is *inside* the SQLite backend's `apply_*`, owned by the SQLite `Confinement` impl that captured the `Arc<AtomicU8>` — never surfaced to or constructed by the core. The core only knows "the backend confines itself across the up/journal boundary." `ExecutorConfig` (L4) keeps *neutral* fields (`project_id`, `project_schema`, `timeouts`); each backend's confinement strategy carries its own inputs (PG role/schemas; SQLite paths + the atomic; MySQL DEFINER user). This **supersedes the §2.4 `Confinement` enum and §2.5**: strategy object, not core-constructed enum. (Resolves draft open-question #4 — *trait, not enum* — for the stated reason: only a strategy object can own the SQLite mode-flip lifecycle.)

### R.L-a + BUNDLE-VS-GENERATE — migration SOURCE is per-TIER, not per-engine (new §)

<!-- Added post-critic: addressing L-a and the user's explicit bundle-vs-generate question -->

This was the user's explicit question and the draft never addressed it head-on. **Migration *source* — versioned-bundled vs declarative-generated — is a per-TIER decision, orthogonal to the engine abstraction.** Today the two are accidentally **coupled** to engine:
- **prod:** control plane `apply_bundle_migrations` → `load_dir` → `engine.apply` — **versioned-bundled, PG** (`control/.../deploy_migrate.rs:133/172`).
- **dev:** plugin-db `plan_declarative` → `engine.apply_declarative` — **declarative-generated-at-boot, SQLite** (`register_model/sqlite_engine.rs:196`).

That coupling (prod⇒bundled⇒PG, dev⇒generated⇒SQLite) is incidental, not designed. The deliberate decision:

| Tier | Source | Why |
|------|--------|-----|
| **prod** | **versioned-bundled** (explicit, reviewed, reproducible; content-addressed in the `.zship`) | A *reviewed* migration carries **intent a schema-diff cannot reproduce**: rename (vs drop+add → data loss), data backfills, the destructive/approval flags. You do **not** want a deploy-time auto-diff *guessing* a plan against live creator data. Bundled = the bytes that ran in CI are the bytes that run in prod (checksum-identical), reviewed, and reproducible. |
| **dev** | **declarative-generated-at-boot** | Low-stakes, recoverable (throwaway SQLite app file), fast iteration — the cost of a wrong guess is `rm` the dev DB. Generation latency must be ~zero; review is the creator's own loop. |

**The GATED alternative — declarative-at-deploy (Atlas-style).** Generating the plan at *deploy* time (diff desired-vs-live, apply the delta) is viable **only behind plan → review → approve** — never an unattended deploy-time auto-diff against live creator data. We note it as a *future, gated* option, not the default. (This is precisely why §1.2/§6.2's expand-contract *intent* matters: a reviewed rename carries `OnlineIntent`, which a deploy-time diff would have to re-infer.)

**Missing quadrants — explicitly out of scope:** *SQLite-bundled* (a prod-SQLite deploy reading bundled migrations) and *PG-declarative-at-boot* (a PG dev tier generating at boot) are both **coherent but unbuilt**; flag as future, not designed here. The engine abstraction (this doc) is what *unblocks* them — once source and engine are decoupled by the trait seams, any (tier × engine × source) cell is wireable.

**Build-side `generate` (P6c-2) is the PRODUCER of the bundled migrations**, and is **orthogonal** to the engine abstraction: `schema.ts` → a *reviewed* migration file → packed content-addressed into the `.zship`. It feeds the *prod=bundled* tier; it does not touch the engine traits. (Correcting the draft §4.2's framing: `generate` is not just "DX untouched by the abstraction" — it is the *upstream producer* of the artifact the prod tier consumes, and the reason prod can be bundled-and-reviewed at all.) Sequence it independently.

### R — Revised phase plan

The PG suite + SQLite suites are the **byte-identical regression bar at every phase**. This is a **large, multi-phase refactor of a security-critical engine** — stated honestly, not minimized.

- **P0 — `MigrationGuard` trait (BUILDING NOW).** `PgGuard(SqlGuard)` + `SqliteDescriptorGuard`; route `plan()`/`apply_locked` through `backend.guard()`; delete the three `if dialect==Sqlite` branches + `plan_sqlite_trusted`. **Narrow `GuardReport` to `{destructive, advisories}` (R.H2)**; keep `classes` PG-internal. Clean extraction.
- **P1 — De-branch the declarative differ (R.C1 / L18).** Push DDL-fragment spelling into `zeroship-schema`; reduce `DeclarativeAuthor`'s ~15 `is_sqlite` branches to a small, named, **closed** `match self.dialect` of *structural* routing (defer-FK, route-rename-to-rebuild). **The largest single surface; sequenced early.** PG emission byte-identical.
- **P2 — `JournalAtomicity` capability (R.C2 — the generality crux).** Introduce `journal_atomicity(m) -> {Transactional | InflightMarker}`; promote the existing two-phase `started→completed` recovery to the **generic** apply path for `InflightMarker`. Subsumes the draft's `non_txn()` (L8). PG selects per-migration (default `Transactional`, CONCURRENTLY ⇒ `InflightMarker`), SQLite always `Transactional`. **This is the phase that makes the abstraction multi-engine vs PG+SQLite-shaped.** *Real redesign — see closing assessment on whether it needs its own pre-impl pass.*
- **P3 — Shadow / squash / precondition seams (R.C3 / L12–L14).** `ShadowDryRun` (with the "no raw-SQL apply path without a shadow" confinement invariant), `squash_one` on the backend, per-engine `evaluate_preconditions`. `online()` seam lowered to `OnlineIntent` (R.H1) — PG lowers internally; assert the SQLite `renames`-always-empty invariant.
- **P4 — Baseline + `Client`-removal capabilities (L5, L1/L2).** `baseline_one` on the trait; remove `expand_conn`/raw `Client` from the trait surface entirely; **this removes the SQLite backend's PG-driver import** (the SQLite backend stops importing `compio_postgres` once nothing on the shared trait is PG-typed). `ExecutorConfig` neutral fields only (L4).
- **P5 — `Confinement` strategy object (R.M2).** Per-backend strategy owning its lifecycle (PG `SET ROLE`; SQLite the `Arc<AtomicU8>` mode-flip; future MySQL DEFINER), constructed by the backend, called inside `apply_*` across the up/journal boundary.
- **P6 — CLI multi-engine (L11).** DSN-scheme engine selection; per-engine `dump`; `--profile` stays a PG-only posture.
- **P7 — MySQL as a COMPILE-TIME stub (L-c, type-check only — no real impl).** A `MySqlGuard`/`MySqlBackend` skeleton whose methods `unimplemented!()` but whose *signatures compile* against the full trait set — including `journal_atomicity ⇒ InflightMarker`. **Purpose: prove the trait set type-checks for a non-transactional-DDL engine.** It ships nothing; it is the compile-time validation that replaces the draft's paper-only §3.3 sketch. (Keep `SqlDialect` closed; add `MySql` arm only here, behind `#[cfg]` or feature, to force the `query.rs` fan-out to enumerate.)

Each phase is independently landable and PG-byte-identical. The leak *count* does not imply uniform effort: **P0 is clean extraction; P1 and P2 are the architecture (P1 the largest surface, P2 the generality crux); the rest are mostly re-seaming.**

### R — Summary by finding-id

| Finding | Resolution |
|---|---|
| **C1** (declarative differ is the real coupling) | New **P1** phase; reclassify differ comparison=neutral / emission=per-engine; keep `DeclarativeAuthor` on the closed dialect; **delete the false "3 branches → clean" criterion.** |
| **C2** (MySQL atomicity) | New **`JournalAtomicity {Transactional\|InflightMarker}`** capability; generic two-phase `started→completed` becomes the non-atomic apply path; subsumes `non_txn()`. **The multi-engine crux.** |
| **C3** (omitted PG subsystems) | Added **L12 shadow / L13 squash / L14 precondition** with seams (`ShadowDryRun` + confinement invariant; `squash_one`; per-engine preconditions); corrected §1.1 + §2.1. |
| **H1** (online seam leaks via payload) | Lower seam to neutral **`OnlineIntent`** (already exists, `expand_contract.rs:71`); PG lowers `ExpandContractPlan` internally; assert SQLite-`renames`-always-empty invariant. |
| **H2** (`GuardReport` carries PG vocab) | Public `GuardReport = {destructive, advisories}`; `classes` PG-internal. Advisories **declared PG-only diagnostic**, documented gap for SQLite/MySQL. |
| **H3** (Dialect question) | **Keep closed `SqlDialect`**, add `MySql` when real (YAGNI). **Drop opaque-`EngineId` + `Dialect`-trait goals** (ripple into plugin-db data plane; conflict with M1). |
| **M1** (exhaustive `match self.dialect`) | Resolved by H3: the exhaustive match is *correct* given a closed enum; not a leak. |
| **M2** (authorizer runtime model) | Confinement = per-backend **strategy object** owning its lifecycle (incl. the SQLite `Arc<AtomicU8>` mode-flip), not a core-constructed enum. |
| **L-a + bundle-vs-generate** | New §: source is **per-tier** (prod=bundled/reviewed/reproducible; dev=generated-at-boot); Atlas-style deploy-time-declarative is gated; missing quadrants out-of-scope; build-side `generate` is the bundled-migration *producer*. |

### R — Closing assessment (honest)

**Are the post-fix phases sound to build?**

- **P0, P1, P3–P7: yes.** They are extraction + re-seaming + one mechanical de-branch (P1). The behaviors already exist and are tested; the work is moving them behind neutral seams with the PG suite as the byte-identical bar. The corrected scope (P1 is the largest surface, not the guard) makes the effort estimate honest.

- **P2 (`JournalAtomicity`): the capability *shape* is sound, but it needs its own deeper design pass before committing to MySQL-readiness.** The *abstraction* is right — atomicity must be a declared capability with two executor paths, and reusing the existing two-phase recovery is the correct mechanism (it's already in the tree, already tested for PG's CONCURRENTLY path). What is **not** yet designed to implementation depth is MySQL's *partial-commit* semantics inside the `InflightMarker` path: because MySQL implicit-commits at each DDL statement, a multi-statement `<up>` can be **half-applied** on crash, and the recovery model must define re-runnability (idempotent-DDL requirement, or a per-statement sub-journal, or a "MySQL migrations are single-DDL-statement" constraint). PG's existing non-txn path sidesteps this (its non-txn migrations are *single* statements like `CREATE INDEX CONCURRENTLY`), so the existing recovery code does **not** yet cover multi-statement partial commits.

  **Verdict:** build P0–P1 and the *seam* for P2 now (they are foundational and engine-honest), but **gate the MySQL `InflightMarker` impl behind a dedicated `JournalAtomicity` design pass** that resolves multi-statement partial-commit recovery. The compile-time MySQL stub (P7) is the right place to *surface* the unresolved method (`apply_up_with_inflight` left `unimplemented!()`), making the open question explicit in the type system rather than hidden in prose.

**Bottom line:** after these fixes the design is **sound to build the post-P0 phases through the seams**, and honest about being a large refactor of a security engine. It is **not** yet sound to claim "MySQL is shovel-ready" — `JournalAtomicity` (C2) is the one capability whose *implementation* (not its shape) needs a further design pass, and the revised plan now says so explicitly rather than asserting a generality it cannot yet execute.
