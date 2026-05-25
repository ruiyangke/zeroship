> Archived 2026-05-25: shipped (DB P-series). Live reference: docs/reference/db.md.

# P0 Implementation Plan — Capability Trait Split + PostgresBackend Migration

**Source design**: `docs/proposals/db-system-design.md` §5, §7, §19 P0
**Working crate**: `crates/plugin-db/`
**Closes**: `docs/reviews/plugin-db-deferred.md` CRITICAL [C1]
**Per-PR budget**: builds + `cargo test -p zeroship-plugin-db` green on its own; keeps 364 lib + 73 integration tests passing; ≤~800 LOC diff; one-line purpose.

---

## 0. Measured baseline — sites that must be migrated

Counted at HEAD across `crates/plugin-db/src/**`:

| concrete reference | site | count |
|---|---|---|
| `&PostgresBackend` parameter | `migrations.rs` (7 fns: `exec_begin`, `exec_fetch_batch`, `exec_commit_batch` + inner `rollback_and_return`, `exec_status`, `exec_cancel`, `exec_reset`) | 7 |
| `&PostgresBackend` parameter | `orchestrator/register_model/mod.rs::run_pipeline`, `bootstrap::bootstrap`, `bootstrap::build_ctx` | 3 |
| `Rc<PostgresBackend>` field | `context.rs::IsolateDbContext::backend` + getter | 2 |
| `BackendHandle` alias | `backend/mod.rs` | 1 |
| `<B: Backend<Client = compio_postgres::Client>>` | `orchestrator/lock_guard.rs` | 1 |
| `<B: Backend>` generic bounds | `orchestrator/register_model/{apply, validate, plan}.rs` | 3 |
| `backend.pool().get()` escape hatch | `orchestrator/register_model/bootstrap.rs` | **1 (load-bearing)** |
| `backend.as_ref()` call sites (v8_classes) | `migration.rs` (7), `migrations.rs` (3) | 10 |

**`Backend` trait method count**: 23 async methods + 2 associated types. (The doc's "26" was round-off.)

**Out of scope for P0**: `replication.rs`, `wal_consumer.rs`, `replication_ops.rs` — §7 explicitly keeps them Postgres-only.

---

## 1. Six-PR sequence

### PR 1 — Carve `SqlExecutor` + `LockManager` traits

**Goal**: get the two most-used capabilities into their own traits via `Backend: SqlExecutor + LockManager + ...` super-trait. Zero behaviour / call-site change.

**Files**:
- `crates/plugin-db/src/backend/mod.rs` — split trait; `Backend` becomes a super-trait carrying remaining 16 methods.
- `crates/plugin-db/src/backend/postgres.rs` — add `impl SqlExecutor` + `impl LockManager` blocks.
- Add `assert_impl::<PostgresBackend, SqlExecutor>()` compile-time tests.

**Trait shapes**:
```rust
pub trait SqlExecutor: 'static {
    type Client;
    async fn acquire_dedicated_client(&self) -> Result<Self::Client, DbError>;
    async fn pool_exec(&self, sql: &str, params: &[&str]) -> Result<u64, DbError>;
    async fn client_exec(&self, c: &Self::Client, sql: &str, params: &[&str]) -> Result<u64, DbError>;
}

pub trait LockManager: SqlExecutor {
    async fn acquire_advisory_lock(&self, c: &Self::Client, k1: &str, k2: &str) -> Result<(), DbError>;
    async fn try_acquire_advisory_lock(&self, c: &Self::Client, k1: &str, k2: &str) -> Result<bool, DbError>;
    async fn release_advisory_lock(&self, c: &Self::Client, k1: &str, k2: &str) -> Result<(), DbError>;
}

pub trait Backend: SqlExecutor<Client = compio_postgres::Client> + LockManager + 'static {
    type LiveSchema;
    // remaining 16 methods stay here for PR 1
}
```

**Note on `LockScope`**: NOT introduced in PR 1 — the existing string-key shape is preserved. `LockScope` enum + RAII `LockGuard` rename lands in PR 6.

**LOC**: ~+80 net.

---

### PR 2 — Carve remaining capability traits; promote register_model bounds

**Goal**: every old `Backend` method lives in a focused sub-trait. `register_model/{plan,validate,apply}` declare the narrowest bound they need.

**Files**:
- `backend/mod.rs`: carve `NamespaceManager` (1 method), `SchemaIntrospect` (3 methods + `LiveSchema` associated type), `IndexBuilder` (1 method).
- **Audit-helper resolution (Open Q1)**: take the **free-function path** — drop 16 audit trait methods; the helpers stay free functions in `crate::audit::*` taking `&Pool` or `&Client`. `SqlExecutor` exposes a `pool_handle()` PG-side accessor for audit lookups.
- `register_model/plan.rs` — `<B: SchemaIntrospect<LiveSchema = LiveSchema>>`.
- `register_model/validate.rs` — `<B: SqlExecutor<Client = compio_postgres::Client>>`.
- `register_model/apply.rs` — `<'p, B: SqlExecutor<Client = compio_postgres::Client> + IndexBuilder>`.
- `orchestrator/lock_guard.rs` — `<B: LockManager<Client = compio_postgres::Client>>`.

**LOC**: ~+100 net (free-function path); ~+300 if `AuditWriter` trait path chosen.

---

### PR 3 — Migrate `register_model/{bootstrap,mod}.rs`; close `pool().get()` escape hatch

**Goal**: kill `&PostgresBackend` parameters in register_model. The blocker is `bootstrap.rs:103`'s `backend.pool().get()` — produces `PooledClient<'p>` whose `'p` threads through `OrchestratorLockGuard<'p>`.

**Resolution (Open Q5)**: introduce `PgLockManager: LockManager` extension trait carrying `acquire_pooled_client_for_lock<'p>(&'p self) -> Result<compio_postgres::PooledClient<'p>, DbError>`. PG impl only. Avoids the GAT-with-async-fn-in-trait friction.

**Files**:
- `backend/mod.rs` — add `PgLockManager` extension trait + PG impl in `backend/postgres.rs`.
- `register_model/bootstrap.rs` — bound = `SqlExecutor<Client = compio_postgres::Client> + LockManager + NamespaceManager + SchemaIntrospect<LiveSchema = LiveSchema> + PgLockManager`; replace `backend.pool().get()` with `backend.acquire_pooled_client_for_lock()`.
- `register_model/mod.rs::run_pipeline` — generic over `B: <compound bound>`. Introduce `RegisterBackend` marker super-trait for ergonomics.

**LOC**: ~+100 net.

---

### PR 4 — Migrate `migrations.rs` off `&PostgresBackend`

**Files**:
- `migrations.rs` — 7 fn signatures (`exec_begin`, `exec_fetch_batch`, `exec_commit_batch` + `rollback_and_return`, `exec_status`, `exec_cancel`, `exec_reset`) each take a narrow capability bound.
- `LockClient` type alias reified to `compio_postgres::Client` (the alias's purpose was to hide concrete type from consumers; now the only consumer is `migrations.rs` itself, which parks PG-shaped state in `IsolateDbContext::mig_lock`).
- Test helpers `exec_*_with_pool` stay as-is — PG-only by design.

**LOC**: ~+50 net.

---

### PR 5 — Introduce `BackendHandle` enum; swap `IsolateDbContext::backend`

**Goal**: close §5.5 / round-3 critic CRITICAL #3. The per-isolate context stores a `BackendHandle` enum that **today has only a Postgres arm** but is shaped for the SQLite arm P1 will add. **No `Rc<dyn Backend>` ever.**

**Files**:
- `backend/mod.rs`:
  ```rust
  #[derive(Clone)]
  pub enum BackendHandle {
      #[cfg(feature = "pg")]
      Postgres(Rc<PostgresBackend>),
      #[cfg(feature = "sqlite")]
      Sqlite(Rc<crate::backend::sqlite::SqliteBackend>),
  }
  impl BackendHandle {
      pub fn with_postgres<R>(&self, f: impl FnOnce(&PostgresBackend) -> R) -> R { ... }
  }
  ```
- `Cargo.toml` — `[features] default = ["pg"]; pg = []; sqlite = []`.
- `context.rs` — field type swap; accessor returns `BackendHandle` (not `Rc`).
- v8_classes consumers — wrap in `backend.with_postgres(|b| ...)`.

**LOC**: ~+150 net.

---

### PR 6 — `LockScope` enum + `LockGuard` rename; classify advisory-lock sites

**Goal**: close §19 P0's "classify every advisory-lock site under `LockScope`" deliverable.

**Files**:
- `backend/mod.rs`:
  ```rust
  pub enum LockScope {
      GlobalApp { app_id: String, name: String },
      LocalApp { app_id: String, name: String },
  }
  ```
- `orchestrator/lock_guard.rs` → renamed `backend/lock_guard.rs`; struct `OrchestratorLockGuard` → `LockGuard`.
- Call-site classification:
  - `bootstrap.rs` → `LockScope::GlobalApp { app_id, name: "register_model".into() }`
  - `migrations.rs` → `LockScope::GlobalApp { app_id, name: format!("mig:{name}") }`
- Legacy 3 string-key methods stay `pub(crate)` as impl detail of the typed API. Eager removal deferred to P1.

**LOC**: ~+60 net.

---

## 2. Seam invariants

| After PR | What must hold |
|---|---|
| PR 1 | `Backend` methods reachable via sub-traits; method resolution unchanged. |
| PR 2 | `&PostgresBackend` count: 13 → 10 (only in `migrations.rs` + `register_model/{mod,bootstrap}` + `context.rs` field). |
| PR 3 | `&PostgresBackend` count: 10 → 7 (all in `migrations.rs`). |
| PR 4 | `&PostgresBackend` count: 7 → 1 (only `context.rs` field). |
| PR 5 | `Rc<PostgresBackend>` gone from `context.rs`; **no `dyn Backend` anywhere** (grep verified). |
| PR 6 | Every advisory-lock site uses `LockScope`; `LockGuard` is the canonical RAII guard name. |

**§19 P0 gate tests** (must pass after each PR + the existing 364 lib + 73 integration tests):
- `register_model_idempotent`
- `tx_savepoint_rollback`
- `subscription_fanout_basic`

---

## 3. Open questions — resolutions

| # | Question | Resolution |
|---|---|---|
| Q1 | `AuditWriter` trait vs free functions? | **Free-function path** in PR 2; helpers stay in `crate::audit::*`. |
| Q2 | Where do capability traits live? | **`backend/capabilities.rs`** — single file, populated across PRs 1–2. |
| Q3 | `BackendHandle` enum vs single concrete? | **Enum** per §5.5 (settled by round-3 reviser). |
| Q4 | `Send + Sync` on traits? | **No** — single-threaded compio runtime per worker; document the absence in PR 1. |
| Q5 | `PooledClient<'p>` GAT vs PG extension trait? | **PG extension trait `PgLockManager`** in PR 3; defer cross-backend lifetime threading to P1. |
| Q6 | `sqlite` feature flag — P0 or P1? | **Add in PR 5** (gated arms only; enables nothing until P1). |

---

## 4. Sequencing constraints

- PR 1 → PR 2 (sub-traits must exist before bounds can use them).
- PR 2 → PR 3 (`PgLockManager` lives alongside other capabilities).
- PR 3 ↔ PR 4 (independent; recommend PR 3 first because it validates the extension-trait choice).
- PR 4 → PR 5 (`with_postgres` ceremony cleaner when consumers are already generic).
- PR 5 ↔ PR 6 (independent; recommend PR 5 first per §19 P0 order).

---

## 5. Critical files

- `crates/plugin-db/src/backend/mod.rs` — trait declarations land here; becomes the capability-trait registry.
- `crates/plugin-db/src/backend/capabilities.rs` (new) — populated across PRs 1–2.
- `crates/plugin-db/src/backend/postgres.rs` — split monolithic `impl Backend` into per-capability impl blocks.
- `crates/plugin-db/src/migrations.rs` — largest concrete-type holdout; PR 4's budget.
- `crates/plugin-db/src/context.rs` — `IsolateDbContext::backend` field is the API boundary v8_classes sees; PR 5 swaps it.
- `crates/plugin-db/src/orchestrator/register_model/bootstrap.rs` — the `backend.pool().get()` escape hatch lives here; PR 3 closes it.
