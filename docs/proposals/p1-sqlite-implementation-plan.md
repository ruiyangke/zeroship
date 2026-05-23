# P1 — SQLite Backend Core: Implementation Blueprint

**Source design**: `docs/proposals/db-system-design.md` §1, §5.5, §6.2.1, §7, §8, §10.5, §11.5 (context), §17.6/§17.7, §18 Q1+Q7+Q8, §19 P1
**Precedent shape**: `docs/proposals/p0-implementation-plan.md`
**Working crate**: `crates/plugin-db/`
**Head**: `eb3c0270` on `main`
**Per-PR budget**: builds + `cargo test -p zeroship-plugin-db` green on its own with **both** `--features pg` (default) and `--features sqlite` (P1 introduces the latter).

> **Constraint reminder.** Per `feedback_never_estimate.md`, LOC cells not anchored in an existing PG impl line count are tagged "unknown — needs measurement"; anchored cells are written as `≈<count> (PG: …)`.

---

## 0. Pre-flight inventory — what P0 left for P1 to wire up

| Reference | Site(s) | Status |
|---|---|---|
| `BackendHandle::Sqlite` arm | `backend/mod.rs:787-794` | **Removed** in P0 mop-up (`724a64ca`); to be re-added under `#[cfg(feature = "sqlite")]` |
| `sqlite = []` Cargo feature | `Cargo.toml:69-80` | Removed in P0 mop-up; to be re-added |
| `BackendHandle::with_sqlite` / `as_sqlite` | none | New — symmetric to `with_postgres`/`as_postgres` |
| `LockGuard<'p>` generalisation | `backend/lock_guard.rs:100-256` | **PG-only**: hard-codes `compio_postgres::PooledClient` + `LockManager<Client = compio_postgres::Client>`. **Not refactored in P1** — see §3.3. |
| `OwnedLockGuard<'b, B>` | `backend/owned_lock_guard.rs:116-340` | Generic over `B: LockManager<Client = compio_postgres::Client>`; PG-only by Client= bound. SQLite-side migration backfill path lands in a later PR. |
| `DialectBuilder` trait | none | New — design §7.2 lists it; P0 did not introduce; **decision in §5: introduce in P1** |
| `RegisterBackend` super-trait | `backend/mod.rs:700-718` | Composes PG-specific bounds. SQLite does **not** implement; orchestrator pipeline migration deferred. |
| `IsolateDbContext::backend` | `context.rs:175` | Already `Option<BackendHandle>`; field shape unchanged. |
| Test scaffolding | `tests/integration.rs` PG-shaped | P1 needs sibling `tests/sqlite_integration.rs`. Existing PG tests stay as-is. |

**Critical observation.** P0 PR 5's "every consumer site previously holding `Rc<PostgresBackend>` migrates one-to-one" works today because only one arm exists. The instant P1 lands `Sqlite(Rc<SqliteBackend>)`, every `BackendHandle::with_postgres` call site must either (a) continue functioning under the SQLite arm by returning a typed `backend_unsupported` error, or (b) be rerouted via a backend-agnostic capability bound. **P1 keeps (a)** — orchestrator/replication/auth stay PG-arm-only at runtime; SQLite-arm equivalents land in P2 (CDC) / P3 (auth) / future PRs. **P1's job is the capability-trait surface only — no consumer-side rewires beyond exposing the SQLite arm.**

---

## 1. File structure

**Recommendation**: **module folder**, not flat file. PG's `backend/postgres.rs` is 685 lines — the ceiling for a minimum-viable backend. SQLite carries more moving parts at parity (session actor, dialect, ATTACH bookkeeping, PRAGMA wiring, parser hook); design §19 P1 names three sub-files explicitly.

```
crates/plugin-db/src/backend/sqlite/
├── mod.rs           SqliteBackend struct + all six capability impl blocks + #[cfg(test)] compile-time asserts.
├── session.rs       SqliteSession actor — owns rusqlite::Connection, mpsc command queue, ATTACH bookkeeping, PRAGMA bootstrap (journal_mode=WAL, synchronous=NORMAL, busy_timeout=5000, foreign_keys=ON).
├── dialect.rs       SqliteDialect impl of DialectBuilder; ships only the P1-essential hooks.
├── lock.rs          InProcessLockRegistry — HashMap<(String,String), …> per design §8.5.
├── error.rs         SqliteError → DbError mapping (extended result codes → §15.7 .code taxonomy).
└── fk_parse.rs      Cross-app FK parse-time check (§18 Q1). Pure-Rust validator over schema JSON.
```

**Module wiring in `backend/mod.rs`**:

```rust
#[cfg(feature = "sqlite")]
pub(crate) mod sqlite;
#[cfg(feature = "sqlite")]
pub use sqlite::SqliteBackend;
```

---

## 2. Type definitions — file by file

> **LOC anchoring**. PG impl: `backend/postgres.rs` is 685 LOC; six capability impl blocks sum to ~257 LOC of method bodies. SQLite is in the same ballpark but with different mix. "Unknown" otherwise.

### 2.1 `backend/sqlite/mod.rs`

| Item | Shape | LOC |
|---|---|---|
| `pub struct SqliteBackend` | `{ session: Rc<session::SqliteSession>, dialect: SqliteDialect, lock_registry: Rc<InProcessLockRegistry>, db_dir: PathBuf, app_id_cache: RefCell<HashSet<String>> }` | ~25 |
| `impl SqliteBackend::new` | `pub fn new(db_dir: PathBuf) -> Result<Self, DbError>` — opens session, runs boot PRAGMAs inside `spawn_blocking` | ~40 |
| `impl Debug` | opaque (mirrors PG `postgres.rs:37-41`) | 5 |
| `impl SqlExecutor for SqliteBackend` | `type Client = SqliteSessionHandle;` + 3 methods | ≈60 (PG: 39) |
| `impl LockManager for SqliteBackend` | 3 legacy string-key methods routing to `InProcessLockRegistry`; default acquire/try_acquire/release/try_acquire_with_backoff inherit | ≈45 (PG: 63) |
| `impl NamespaceManager` | `ensure_app_schema` — `ATTACH DATABASE 'file:{db_dir}/zs-{app_id}.sqlite' AS "<app_id>"`, idempotent via `app_id_cache` | ≈25 (PG: 11) |
| `impl SchemaIntrospect` | `type LiveSchema = crate::diff::LiveSchema;` + 2 methods; PRAGMA-based catalog walk | unknown (PG: 220 in diff.rs) |
| `impl IndexBuilder` | `CREATE [UNIQUE] INDEX IF NOT EXISTS` atomic; audit-row via `SqliteAuditWriter` trait | unknown |
| `impl DialectBuilder` | 6 P1-essential hooks (see §5) | ~40 |
| `impl Backend for SqliteBackend {}` | composition marker | 1 |
| `#[cfg(test)] mod tests` | compile-time asserts mirror `backend/mod.rs:852-1311` | ≈80 |

**Crucial design note on `Backend` super-trait.** P0's `Backend` is:

```rust
pub trait Backend:
    SqlExecutor<Client = compio_postgres::Client>  // <-- PG-only Client pin
    + LockManager + NamespaceManager
    + SchemaIntrospect<LiveSchema = crate::diff::LiveSchema>
    + IndexBuilder + 'static
{}
```

The `Client = compio_postgres::Client` bound prevents `SqliteBackend: Backend`. Two options:

- **(B-1) Relax the bound** to `SqlExecutor + LockManager + …` (drop `Client = …`). 4-line edit. **Recommended.**
- **(B-2) Don't impl `Backend` for `SqliteBackend`** — treat `Backend` as PG-only deprecated marker.

**Riskiest P1 decision** — see §10.

### 2.2 `backend/sqlite/session.rs`

| Item | Shape | LOC |
|---|---|---|
| `pub struct SqliteSession` | `{ tx: flume::Sender<Command>, _worker: compio::runtime::Task<()> }` | ~20 |
| `enum Command` | `Exec`, `Query`, `Attach`, `Detach`, `Shutdown` | ~25 |
| `pub struct SqliteSessionHandle` | clone-cheap `Rc<SqliteSession>` wrapper — the `SqlExecutor::Client` assoc type | ~15 |
| `impl SqliteSession::open` | `pub fn open(db_path: &Path) -> Result<Self, DbError>` — spawns blocking actor | ≈80 |
| `async fn SqliteSession::exec / query / attach / detach` | helpers: oneshot reply, cancel-safe | ~25 each × 4 = ~100 |
| `impl Drop for SqliteSession` | sends `Shutdown` (best-effort), drops worker handle | ~10 |

**Compio API**: `compio::runtime::spawn_blocking<T>(f: impl FnOnce() -> T + Send + 'static) -> Task<Result<T, …>>`. Closure captures `flume::Receiver<Command>` (Send) + `rusqlite::Connection` (Send + !Sync) — both satisfy `Send + 'static`.

**Cancellation safety**: spawn_blocking task persists if the future is dropped. Cancelled `pool_exec` drops the oneshot receiver; worker still completes the SQL (durable to WAL or rolled back); next `rx.recv()` returns next command. No leaked locks/rows.

### 2.3 `backend/sqlite/dialect.rs`

| Item | Shape | LOC |
|---|---|---|
| `pub trait DialectBuilder` | declared here; 6-8 P1 hooks (see §5) | ~50 |
| `pub struct SqliteDialect;` | ZST | 2 |
| `impl DialectBuilder for SqliteDialect` | 6-8 methods, each 1-3 lines | ~60 |
| Deferred to P2-P5 | RETURNING/upsert/JSON/vector/FTS hooks | ~10 placeholder |

### 2.4 `backend/sqlite/lock.rs`

| Item | Shape | LOC |
|---|---|---|
| `pub struct InProcessLockRegistry` | `RefCell<HashMap<(String, String), Rc<Cell<bool>>>>` per §8.5 | ~30 |
| `impl InProcessLockRegistry::{try_acquire, release}` | matches `LockManager` legacy primitive signature | ~25 |

### 2.5 `backend/sqlite/error.rs`

| Item | Shape | LOC |
|---|---|---|
| `pub fn from_sqlite(e: rusqlite::Error) -> DbError` | mirrors `DbError::from_pg`; switch on extended result code | ~70 (PG anchor: ~50) |

### 2.6 `backend/sqlite/fk_parse.rs`

| Item | Shape | LOC |
|---|---|---|
| `pub fn reject_cross_app_fk(schema: &serde_json::Value, app_id: &str) -> Result<(), DbError>` | walks `t.ref(target)` entries; rejects cross-app `Configuration { code: "cross_app_fk_forbidden", …}` | ~50 |

**Hook point**: `orchestrator/register_model/bootstrap.rs::build_ctx`, after strictness read, before lock acquire.

---

## 3. Trait-by-trait mapping — PG behaviour ↔ SQLite divergence

### 3.1 `SqlExecutor`

| Method | PG | SQLite | Divergence |
|---|---|---|---|
| `type Client` | `compio_postgres::Client` | `SqliteSessionHandle` | Different concrete type. |
| `acquire_dedicated_client` | `compio_postgres::connect(url, NoTls)` + spawn connection task | `Rc::clone(&self.session)` wrapped in handle — single writer actor per backend (§8) | Documented — SQLite has no per-client session; actor IS the only writer. Long-lived tx multiplexes through mpsc queue, which serialises by construction. |
| `pool_exec` | `pool.query_text_params` | `session.exec(Command::Exec)` over mpsc | Both `u64` row count. SQLite returns `Connection::changes() as u64`. |
| `client_exec` | `client.query_text_params` over `&Client` | `(&Self::Client).exec(...)` routes to same actor | No trait-API divergence. |

### 3.2 `NamespaceManager`

| Method | PG | SQLite | Divergence |
|---|---|---|---|
| `ensure_app_schema(app_id)` | `CREATE SCHEMA IF NOT EXISTS "app_id"` | `ATTACH DATABASE 'file:{db_dir}/zs-{app_id}.sqlite' AS "app_id"` via session, guarded by `app_id_cache: HashSet<String>` (SQLite errors on double-ATTACH; PG's IF NOT EXISTS is idempotent natively) | Per-app file, not per-app-schema-within-one-file. |

### 3.3 `LockManager` — subtle divergence

PG-side `acquire_advisory_lock`/`try_acquire_advisory_lock`/`release_advisory_lock` use `pg_advisory_lock` SQL. SQLite-side use the in-process HashMap.

**`LockGuard<'p>` generalisation**: keep PG-only (decision **LG-1**). The current bound `<B: LockManager<Client = compio_postgres::Client>>` is correct — SQLite has zero P1 consumers needing the guard. Defer to a future PR with `SqliteLockGuard` sibling.

**`OwnedLockGuard<'b, B>`**: same — PG-only by `Client = …` bound. P1 doesn't wire SQLite-side register-model.

### 3.4 `SchemaIntrospect`

| Method | PG | SQLite |
|---|---|---|
| `type LiveSchema` | `crate::diff::LiveSchema` | **same**. Diff engine reads `pg_type` stringly; SQLite populates with `"TEXT"`/`"INTEGER"`/`"REAL"`/`"BLOB"`/`"NUMERIC"`. Classifier teaching follows in a separate PR. |
| `introspect_schema(app_id)` | One large SQL over `pg_catalog` | Multiple PRAGMA queries: `sqlite_master`, `PRAGMA table_info`, `PRAGMA index_list`/`index_info`, `PRAGMA foreign_key_list`. Inside one `spawn_blocking`. |
| `estimate_row_count` | `reltuples`-style | `SELECT COUNT(*)` (only consumer needs 0/non-0 distinction) |

### 3.5 `IndexBuilder`

| Method | PG | SQLite |
|---|---|---|
| `create_index_with_recovery` | `CREATE INDEX CONCURRENTLY` + audited retry loop | `CREATE [UNIQUE] INDEX IF NOT EXISTS` atomic; on `SQLITE_CONSTRAINT_UNIQUE` (2067) classify via `error::from_sqlite`, write `unique_violation` audit row, return wire-compatible `DbError::SchemaRefused` envelope. |

**`SqliteAuditWriter` capability trait** (decision **AW-1**): parallel to `PgSqlExecutor::pool_handle`. PG impl forwards to existing `crate::audit::write_audit_row(pool, ...)`; SQLite impl dispatches via session actor. ~10 LOC trait + ~50 at impls. Preserves PG observability parity.

### 3.6 `DialectBuilder` — §5 (introduce now)

---

## 4. compio integration — exact `spawn_blocking` usage

```rust
let worker_task = compio::runtime::spawn_blocking(move || {
    // conn: rusqlite::Connection (Send + !Sync); rx: flume::Receiver<Command> (Send).
    while let Ok(cmd) = rx.recv() {
        match cmd {
            Command::Exec { sql, params, reply } => {
                let result = run_exec(&conn, &sql, &params);
                let _ = reply.send(result);
            }
            Command::Query { .. } => { /* ... */ }
            Command::Attach { .. } => { /* ... */ }
            Command::Detach { .. } => { /* ... */ }
            Command::Shutdown => break,
        }
    }
});
```

**One worker actor per `SqliteBackend` instance** (per §8). Pool size tunable at runtime level (`ZEROSHIP_WORKER_BLOCKING_THREADS`; §18A). Reader concurrency (§18 Q8: "1 writer + 4 readers") deferred — P1 ships 1 writer; reads serialise through it.

---

## 5. `DialectBuilder` decision

**Introduce in P1 with a minimum-viable hook set; flesh out in P2-P5.**

P1's six essential hooks:

```rust
pub trait DialectBuilder: 'static {
    fn quote_ident(&self, name: &str) -> String;
    fn build_ensure_app_schema(&self, app_id: &str) -> String;   // PG: CREATE SCHEMA; SQLite: ATTACH
    fn build_create_index(&self, spec: &IndexSpec, online: bool) -> String;  // PG online=true → CONCURRENTLY
    fn map_zs_type(&self, zs_type: &str, opts: &serde_json::Value) -> String;
    fn now_fn(&self) -> &'static str;                            // PG: NOW(); SQLite: CURRENT_TIMESTAMP
    fn last_insert_rowid_sql(&self) -> Option<&'static str>;     // PG: None; SQLite: Some("SELECT last_insert_rowid()")
}
```

PG side: `query.rs`'s free fns become wrappers routing through `dialect`. New ZST `PgDialect: DialectBuilder` lives beside the existing capability impls. Orchestrator's `RegisterBackend` bound gains `+ DialectBuilder`.

---

## 6. Cross-app FK parse-time check — hook point

**`orchestrator/register_model/bootstrap.rs::build_ctx`**, after strictness read, before lock acquire.

```rust
crate::backend::sqlite::fk_parse::reject_cross_app_fk(schema, app_id)?;
```

Pure-Rust validator (no SQL). Rejects `Configuration { code: "cross_app_fk_forbidden", message: "FK target \"other_app.users\" crosses app boundary; only same-app FKs allowed", hint: Some("Drop the \"other_app.\" prefix from refTarget") }`. Runs on **both backends**.

---

## 7. Test gate

| File | P1 SQLite peer |
|---|---|
| `tests/integration.rs` | **NEW**: `tests/sqlite_integration.rs` — `required-features = ["sqlite", "test-helpers"]`. Mirrors `register_model_idempotent`, `tx_savepoint_rollback`, basic CRUD, schema introspection. Subscription/replication/WAL tests stay PG-only (P2 territory). |
| `tests/db_v8_class.rs` | No peer (V8 only, no DB). |
| `tests/auto_tx.rs` | No peer. |
| `tests/capability.rs` | No peer. |
| `tests/subscription_finalizer.rs` | No peer (P2). |
| In-crate `cargo test --lib` (368 tests) | Most build green under `--features sqlite` too; new compile-time asserts in `backend/sqlite/mod.rs::tests`. |

**New test-helper**: `set_db_dir_for_tests(path)` mirroring `set_db_url_for_tests(url)`. **New cross-backend test**: `cross_app_fk_rejected_at_parse` — asserts `err.code === "cross_app_fk_forbidden"`.

---

## 8. Cargo + dependency

```diff
 [features]
 default = ["pg"]
 pg = []
+sqlite = ["dep:rusqlite", "dep:flume"]
 test-helpers = []
 hardening = []

 [dependencies]
+rusqlite = { version = "0.39", features = ["bundled", "preupdate_hook"], optional = true }
+flume = { workspace = true, optional = true }

+[[test]]
+name = "sqlite_integration"
+path = "tests/sqlite_integration.rs"
+required-features = ["sqlite", "test-helpers"]
```

Workspace `Cargo.toml`: add `rusqlite = "0.39"` at `[workspace.dependencies]`. Verified versions: rusqlite 0.39.0, embedded SQLite amalgamation 3.51.3, above the 3.16 `sqlite3_preupdate_hook` minimum.

---

## 9. Commit sequence — 5 logical PRs

Each PR builds + tests green on its own under both `--features pg` and `--features sqlite`.

**PR 1** — Cargo + module skeleton + `BackendHandle::Sqlite` arm + `DialectBuilder` trait + `Backend` super-trait relaxation. Stubs return `unimplemented!()` or `Err(DbError::Internal { message: "P1 PRn+".into() })`.

**PR 2** — `SqliteSession` actor + `SqlExecutor` impl + PRAGMA bootstrap + `error::from_sqlite`.

**PR 3** — `NamespaceManager` (ATTACH) + `DialectBuilder` impl (PgDialect + SqliteDialect, 6 hooks).

**PR 4** — `LockManager` (in-process HashMap) + `SchemaIntrospect` (PRAGMA walk).

**PR 5** — `IndexBuilder` + `SqliteAuditWriter` capability trait + cross-app FK parse-time check + `impl Backend for SqliteBackend {}` + test mirror.

---

## 10. Open questions

| # | Question | Default recommendation |
|---|---|---|
| Q-P1-A | Audit-row writing on SQLite arm — (AW-1) trait vs (AW-2) free-fn refactor vs (AW-3) audit-less | **(AW-1)** trait; ~10 LOC; preserves PG observability parity |
| Q-P1-B | **`Backend` super-trait Client-pin relaxation (B-1)** — riskiest. Need grep audit at PR 1 confirming no consumer assumes `B::Client = compio_postgres::Client` via the super-trait. | **(B-1)** with audit. Only path to `SqliteBackend: Backend`. |
| Q-P1-C | `LockGuard<'p>` generalisation — (LG-1) PG-only / (LG-2) generalise / (LG-3) sibling | **(LG-1)**. P1 has zero SQLite Guard consumers. |
| Q-P1-D | `RegisterBackend` super-trait composition — keep PG extension-trait composition? | **Yes**. P1 doesn't migrate orchestrator to dual-backend; deferred to P3-ish PR. |
| Q-P1-E | `DialectBuilder` scope — six P1 hooks now or all ~30 upfront? | **Six** now; others land alongside their phase. |
| Q-P1-F | SQLite `db_dir` config — runtime URL-scheme dispatcher (`sqlite:///path/to/dir`) | URL-prefix dispatcher per design §7.4. Lands separate PR after P1 capability surface green ("P1.5"). |
| Q-P1-G | `db.replication` against SQLite arm | PG-arm-only at runtime; SQLite returns `Configuration { code: "backend_unsupported", hint: "replication is PG-only in P1" }`. Consumer-side, outside P1 scope. |

---

## Riskiest decision (explicit flag)

**Q-P1-B / decision (B-1)**: relaxing `Backend`'s super-trait by dropping the `SqlExecutor<Client = compio_postgres::Client>` pin.

**Why risky**: any consumer with `fn foo<B: Backend>(b: &B, ...)` that calls `b.acquire_dedicated_client().await?.query_text_params(...)` relies on `B::Client = compio_postgres::Client` by inference through `Backend`'s super-bound. P0 PR 2 narrowed register_model's bounds onto sub-traits (`<B: SqlExecutor<Client = compio_postgres::Client> + ...>`) — the audit should come back clean, but it's load-bearing.

**Mitigation**: PR 1 ships with a fresh `grep -rn '<.*Backend.*>'` audit in the PR description showing every call site. If any depends on the pin, narrow it to `<B: PgSqlExecutor>` in PR 1.
