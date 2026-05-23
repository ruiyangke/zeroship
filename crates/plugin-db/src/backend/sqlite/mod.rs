//! SQLite backend — skeleton.
//!
//! **P1 PR 1**: this module re-introduces the [`SqliteBackend`] type
//! and the five sub-modules (`session`, `dialect`, `lock`, `error`,
//! `fk_parse`) the plan calls out, but every capability-trait method
//! body is a stub. The intent is to lock the field set + the trait
//! impls + the `BackendHandle::Sqlite(Rc<SqliteBackend>)` arm at
//! compile time, so PR 2-5 only edit method bodies — they don't
//! re-shape the surface.
//!
//! - **PR 2**: `SqliteSession` actor + `SqlExecutor` impl + PRAGMA
//!   bootstrap + `error::from_sqlite` switch.
//! - **PR 3**: `NamespaceManager` (ATTACH) + `DialectBuilder` impl
//!   (both PG and SQLite sides, 6 hooks).
//! - **PR 4**: `LockManager` (in-process HashMap) + `SchemaIntrospect`
//!   (PRAGMA walk).
//! - **PR 5**: `IndexBuilder` + `SqliteAuditWriter` capability +
//!   cross-app FK parse-time check + `impl Backend for SqliteBackend`
//!   + the SQLite integration-test mirror.
//!
//! **`impl Backend for SqliteBackend` is intentionally absent in PR 1**
//! — the `Backend` super-trait relaxation (Q-P1-B; see
//! `docs/proposals/p1-sqlite-implementation-plan.md` §10) lands in this
//! same PR, but the marker impl waits until PR 5 ties off every
//! sub-trait. PR 1's surface is the carved capability traits only.

use std::cell::RefCell;
use std::collections::HashSet;
use std::path::PathBuf;
use std::rc::Rc;

use serde_json::Value;

use crate::backend::{
    DialectBuilder, IndexBuilder, LockManager, LockScope, NamespaceManager, SchemaIntrospect,
    SqlExecutor,
};
use crate::error::DbError;
use crate::query::IndexSpec;

pub(crate) mod dialect;
pub(crate) mod error;
pub(crate) mod fk_parse;
pub(crate) mod lock;
pub(crate) mod session;

use dialect::SqliteDialect;
use lock::InProcessLockRegistry;
use session::{SqliteSession, SqliteSessionHandle};

// The `SqliteDialect` ZST carries the canonical hook bodies; the
// backend's `impl DialectBuilder` delegates to the ZST so the
// trait-impl source-of-truth stays in one file (`dialect.rs`). No
// `dialect: SqliteDialect` field on the backend — the ZST has no
// state, so storing it would be a 0-byte field carrying no
// information. The delegation pattern below constructs the ZST
// inline (`SqliteDialect`) per call; rustc inlines the value away.

/// Sentinel returned by every capability-impl method body in PR 1.
/// PR 2-5 replace the body call with the real implementation.
fn pr_stub(method: &'static str) -> DbError {
    DbError::Internal {
        message: format!("SqliteBackend::{method} — P1 PR2+ stub"),
    }
}

/// SQLite backend handle. One instance per worker thread (mirrors
/// [`crate::backend::PostgresBackend`]'s lifecycle).
///
/// **Field set** (`docs/proposals/p1-sqlite-implementation-plan.md` §2.1):
///
/// - `session`: the writer-actor handle. Owns the single
///   `rusqlite::Connection` for this backend and serialises all DDL
///   / DML / DQL through a `flume` mpsc queue. Stubbed in PR 1.
/// - `lock_registry`: in-process advisory-lock map. Empty in PR 1;
///   PR 4 wires the `LockManager` impl through it.
/// - `db_dir`: filesystem directory holding per-app SQLite files
///   (`zs-<app_id>.sqlite`). The URL-scheme dispatcher
///   (`sqlite:///path/to/dir`) lands in P1.5.
/// - `app_id_cache`: dedup set for the `NamespaceManager::ensure_app_schema`
///   path — SQLite errors on a second ATTACH of the same alias, so
///   we filter the second call site in Rust.
///
/// **P1 PR 3 simplification**: the previous PR-1 field set carried a
/// `dialect: SqliteDialect` ZST. The ZST has no state, so the field
/// was 0 bytes carrying no information; the trait-method delegation
/// now constructs the ZST inline. See the impl block below.
#[allow(dead_code)]
pub struct SqliteBackend {
    session: Rc<SqliteSession>,
    lock_registry: Rc<InProcessLockRegistry>,
    db_dir: PathBuf,
    app_id_cache: RefCell<HashSet<String>>,
}

impl std::fmt::Debug for SqliteBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Mirrors `PostgresBackend`'s opaque Debug impl — no field
        // exposure (the `db_dir` path may carry deployment-internal
        // information operators don't want spilled to log lines).
        f.debug_struct("SqliteBackend").finish()
    }
}

impl SqliteBackend {
    /// Construct a backend rooted at `db_dir`.
    ///
    /// **P1 PR 1 stub**: opens a placeholder `SqliteSession` (which
    /// itself returns `Err(DbError::Internal { … "P1 PR2 stub" … })`).
    /// PR 2 wires the real `SqliteSession::open` + PRAGMA bootstrap.
    #[allow(dead_code)]
    pub fn new(db_dir: PathBuf) -> Result<Self, DbError> {
        // Construct a default lock registry + dialect — both are
        // PR1-safe (the lock-registry storage is empty until PR 4
        // wires consumers; the dialect is a ZST). The session is the
        // load-bearing piece — PR 2 makes this constructor actually
        // succeed.
        let session_path = db_dir.join("zs-control.sqlite");
        let session = Rc::new(SqliteSession::open(&session_path)?);
        Ok(Self {
            session,
            lock_registry: Rc::new(InProcessLockRegistry::new()),
            db_dir,
            app_id_cache: RefCell::new(HashSet::new()),
        })
    }
}

// ---------------------------------------------------------------------------
// Capability impls — five carved capability blocks. PR 1 ships
// stubbed bodies; PR 2-5 backfill per `p1-sqlite-implementation-plan.md`
// §9. The order below mirrors `backend/postgres.rs` so a reviewer can
// diff the two files side-by-side as the SQLite side grows.
// ---------------------------------------------------------------------------

impl SqlExecutor for SqliteBackend {
    type Client = SqliteSessionHandle;

    async fn acquire_dedicated_client(&self) -> Result<Self::Client, DbError> {
        // SQLite has no per-client session — the actor IS the only
        // writer — so every "dedicated client" handle multiplexes
        // through the same mpsc queue. Long-lived transactions
        // serialise by construction because every command (BEGIN /
        // INSERT / COMMIT) flows through the same single-threaded
        // worker. This is the documented divergence from PG, where a
        // dedicated client gets its own libpq connection.
        Ok(SqliteSessionHandle::from(self.session.clone()))
    }

    async fn pool_exec(&self, sql: &str, params: &[&str]) -> Result<u64, DbError> {
        // "Pool" is a misnomer on the SQLite arm — there's only one
        // writer. We route directly through the session actor. The
        // PG side uses `pool.query_text_params`; the SQLite side
        // serialises via `session.exec`, which lands on the same
        // worker thread regardless of caller.
        self.session.exec(sql, params).await
    }

    async fn client_exec(
        &self,
        client: &Self::Client,
        sql: &str,
        params: &[&str],
    ) -> Result<u64, DbError> {
        // `client` and `self.session` point at the same actor — the
        // `Client` type is just an Rc handle. We route through the
        // handle so a future where they diverge (e.g. per-app ATTACH
        // scoping inside a transaction) can re-target without
        // touching this method.
        client.exec(sql, params).await
    }
}

impl LockManager for SqliteBackend {
    // The default-impl methods (`acquire`, `try_acquire`, `release`,
    // `try_acquire_with_backoff`) inherit through the trait. The three
    // legacy string-key primitives below route through
    // `InProcessLockRegistry`:
    //
    // - `acquire_advisory_lock`: per plan §3.3, this method is
    //   essentially unused in production — every typed `acquire` call
    //   site routes through `try_acquire_with_backoff` (since [I43]).
    //   We implement it for completeness with a bounded
    //   try-then-sleep loop (20ms tick) that mirrors the PG arm's
    //   "indefinite wait" surface without the PG arm's server-side
    //   `pg_advisory_lock`. The loop has no cap — it polls forever
    //   until acquisition succeeds, matching the legacy contract.
    //
    // - `try_acquire_advisory_lock`: synchronous registry call —
    //   borrow-and-return inside one expression so the `RefCell`
    //   borrow never crosses an `.await`.
    //
    // - `release_advisory_lock`: same shape; the registry handles
    //   "release-on-unheld" as a `tracing::warn` no-op so we always
    //   return `Ok(())`.
    //
    // The `_client` argument is ignored on every primitive — design
    // §7.2: "SQLite ignores the argument". The single-writer actor
    // serialises every lock-state read/write through the same Rust
    // process, so the client identity carries no information at the
    // lock layer.

    async fn acquire_advisory_lock(
        &self,
        _client: &Self::Client,
        key1: &str,
        key2: &str,
    ) -> Result<(), DbError> {
        // 20ms poll cadence — same magnitude as the typed
        // `try_acquire_with_backoff`'s first non-zero retry step. The
        // loop is uncapped to match the legacy `pg_advisory_lock`
        // contract; typed call sites should be using
        // `try_acquire_with_backoff` (bounded at 5 attempts / ~1.75s)
        // instead. No production caller invokes this method.
        let k = (key1.to_string(), key2.to_string());
        loop {
            // Borrow scope confined to a single statement — the
            // `RefCell` is released before the `await` below.
            if self.lock_registry.try_acquire(k.clone()) {
                return Ok(());
            }
            compio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }

    async fn try_acquire_advisory_lock(
        &self,
        _client: &Self::Client,
        key1: &str,
        key2: &str,
    ) -> Result<bool, DbError> {
        // Synchronous registry call. The `RefCell` borrow is contained
        // inside the `try_acquire` body and is released before this
        // function returns — there is no `.await` between borrow and
        // release.
        Ok(self
            .lock_registry
            .try_acquire((key1.to_string(), key2.to_string())))
    }

    async fn release_advisory_lock(
        &self,
        _client: &Self::Client,
        key1: &str,
        key2: &str,
    ) -> Result<(), DbError> {
        // `release` is infallible at the registry layer — unheld /
        // unknown slots emit a `tracing::warn` and no-op. Returning
        // `Ok(())` unconditionally matches the legacy PG-arm contract:
        // a release on a session whose lock has already auto-released
        // (because the connection died) is also benign there.
        self.lock_registry
            .release((key1.to_string(), key2.to_string()));
        Ok(())
    }
}

impl NamespaceManager for SqliteBackend {
    /// Idempotently provision the per-app SQLite namespace.
    ///
    /// Constructs the per-app file path
    /// `<db_dir>/zs-<app_id>.sqlite` and routes an `ATTACH DATABASE
    /// 'file:<path>' AS "<app_id>"` through the session actor. The
    /// `app_id_cache` guards re-entry — SQLite errors on a second
    /// `ATTACH` of the same alias, so we filter the duplicate call
    /// in Rust before reaching the engine.
    ///
    /// **Lossy `PathBuf::to_string_lossy` rationale**: the per-app file
    /// path is built from `db_dir` (operator-controlled, typically a
    /// UTF-8 absolute path) joined with `zs-<app_id>.sqlite`. The
    /// `app_id` is constrained to ASCII alphanumeric + `_` + `-` by
    /// [`crate::audit::validate_app_id`] before any consumer reaches
    /// `ensure_app_schema`, so the suffix is always UTF-8 safe. If
    /// `db_dir` itself contains non-UTF-8 bytes (rare on the Linux
    /// targets we ship to), `to_string_lossy` substitutes U+FFFD —
    /// SQLite then fails to open the resulting path and surfaces a
    /// typed `DbError` on the next call. The lossy conversion is
    /// load-bearing for the actor's `String`-typed `db_path`
    /// parameter; round-tripping through OsStr would mean carrying
    /// raw bytes across an `async` boundary the actor's reply channel
    /// already serialises as `String`.
    async fn ensure_app_schema(&self, app_id: &str) -> Result<(), DbError> {
        // Idempotent guard. The cache must be checked before the
        // ATTACH because SQLite hard-errors on a duplicate ATTACH of
        // the same alias ("database <alias> is already in use"); the
        // PG side gets idempotency for free via `IF NOT EXISTS`.
        if self.app_id_cache.borrow().contains(app_id) {
            return Ok(());
        }

        // Compute the per-app file path. `to_string_lossy` is safe in
        // practice — see the rustdoc note above.
        let file_path = self.db_dir.join(format!("zs-{app_id}.sqlite"));
        let path_str = file_path.to_string_lossy().into_owned();

        // Route through the session actor's `attach` helper. The
        // actor's `run_attach` constructs the formatted ATTACH SQL
        // inline (the alias is double-quote-escaped — matches the
        // dialect's `quote_ident` byte-for-byte — and the path's
        // single quotes are doubled). The dialect's
        // `build_ensure_app_schema` is a template that pairs with
        // this helper; no PR-3 consumer routes through the template
        // path because the actor needs the file_path substituted
        // upstream anyway.
        match self.session.attach(app_id, &path_str).await {
            Ok(()) => {
                self.app_id_cache
                    .borrow_mut()
                    .insert(app_id.to_string());
                Ok(())
            }
            Err(e) => {
                // SQLite surfaces "database <alias> is already in use"
                // when an ATTACH alias collides — possible if a
                // different SqliteBackend instance attached the alias,
                // or if the cache was bypassed (test harness, future
                // pre-warm). Treat as idempotent: insert the alias
                // into the cache so subsequent calls short-circuit,
                // then return Ok. Other errors propagate verbatim.
                let msg = format!("{e}");
                if msg.contains("already in use") || msg.contains("already attached") {
                    self.app_id_cache
                        .borrow_mut()
                        .insert(app_id.to_string());
                    Ok(())
                } else {
                    Err(e)
                }
            }
        }
    }
}

impl SchemaIntrospect for SqliteBackend {
    // Same associated type as the PG impl — the diff engine consumes
    // a uniform `LiveSchema` shape; the SQLite impl populates the
    // PG-style `pg_type` strings with SQLite affinity names
    // (`TEXT`/`INTEGER`/`REAL`/`BLOB`/`NUMERIC`). Classifier
    // teaching about the new vocabulary follows in a later PR.
    type LiveSchema = crate::diff::LiveSchema;

    /// Walk the SQLite catalog for `app_id`'s attached database and
    /// produce a [`crate::diff::LiveSchema`] in the same shape the PG
    /// impl emits — populated via four PRAGMA round-trips per table:
    ///
    /// 1. `SELECT name FROM "<app_id>".sqlite_master WHERE type='table'`
    ///    — table list, filtered to user tables (`sqlite_*` system
    ///    tables and our `__zs_*` bookkeeping tables are excluded).
    /// 2. `PRAGMA "<app_id>".table_info("<collection>")` — columns:
    ///    name, type, notnull (0/1), dflt_value, pk.
    /// 3. `PRAGMA "<app_id>".index_list("<collection>")` — indexes:
    ///    seq, name, unique (0/1), origin, partial. The PG impl
    ///    excludes the primary-key index (`indisprimary`); we mirror
    ///    that by skipping indexes whose `origin = 'pk'`.
    /// 4. For each non-PK index: `PRAGMA "<app_id>".index_info(...)` —
    ///    the index's column list in seqno order.
    /// 5. `PRAGMA "<app_id>".foreign_key_list("<collection>")` —
    ///    FKs: id, seq, table (target), from, to, on_update,
    ///    on_delete, match.
    ///
    /// Per plan §3.4 each PRAGMA flows through the session actor's
    /// `Query` command (one round-trip per call); the totals stay
    /// bounded at `1 + 4N` for N tables, which is fine at dev scale
    /// where this code path runs. The PG impl achieves the same with
    /// 3 SQL statements; folding the SQLite walk into a single SQL
    /// statement isn't possible (PRAGMA is non-composable), but the
    /// per-table count stays well below the orchestrator's budget for
    /// a registration round-trip.
    ///
    /// **System-table filter** (plan §3.4): drop any name beginning
    /// with `sqlite_` (engine-internal) or `__zs_` (our bookkeeping —
    /// migrations / audit / replication). The diff classifier consumes
    /// only user-declared tables; surfacing system tables would
    /// trigger spurious "drop table" classifications.
    async fn introspect_schema(&self, app_id: &str) -> Result<Self::LiveSchema, DbError> {
        let mut out = crate::diff::LiveSchema::default();

        // 1. Table list. The `app_id` is interpolated as a quoted
        //    identifier — the dialect's `quote_ident` doubles embedded
        //    `"`s; PRAGMA / sqlite_master both accept the dotted form
        //    `"app_id".sqlite_master`.
        let q_app = self.quote_ident(app_id);
        let tables_sql = format!(
            "SELECT name FROM {q_app}.sqlite_master WHERE type = 'table' ORDER BY name"
        );
        let table_rows = self.session.query(&tables_sql, &[]).await?;
        let mut user_tables: Vec<String> = Vec::with_capacity(table_rows.len());
        for row in &table_rows {
            let name = row
                .first()
                .and_then(|c| c.clone())
                .unwrap_or_default();
            // Filter out system + bookkeeping tables (plan §3.4).
            if name.starts_with("sqlite_") || name.starts_with("__zs_") {
                continue;
            }
            user_tables.push(name);
        }

        for collection in &user_tables {
            let q_coll = self.quote_ident(collection);

            // 2. Columns via `PRAGMA table_info`.
            //
            //    PRAGMA columns: 0=cid, 1=name, 2=type, 3=notnull,
            //    4=dflt_value, 5=pk. The cell shape is `Option<String>`
            //    uniformly (the session materialises every value as a
            //    stringified `Option<String>`), so we read positionally
            //    and parse the `notnull` "0"/"1" into a bool.
            let table_info_sql = format!("PRAGMA {q_app}.table_info({q_coll})");
            let col_rows = self.session.query(&table_info_sql, &[]).await?;
            let mut col_map = std::collections::HashMap::new();
            for row in &col_rows {
                let name = row
                    .get(1)
                    .and_then(|c| c.clone())
                    .unwrap_or_default();
                let pg_type = row
                    .get(2)
                    .and_then(|c| c.clone())
                    .unwrap_or_default();
                let not_null = row
                    .get(3)
                    .and_then(|c| c.as_deref())
                    .map(|s| s != "0")
                    .unwrap_or(false);
                let default_expr = row.get(4).and_then(|c| c.clone());
                col_map.insert(
                    name,
                    crate::diff::ColumnInfo {
                        pg_type,
                        not_null,
                        default_expr,
                        // SQLite expression defaults are stored as raw
                        // text without a volatility tag (the engine has
                        // no `pg_proc.provolatile` analogue). Leaving
                        // this `None` matches what the PG side sets for
                        // literal defaults; the diff classifier reads
                        // `default_volatility` only when the default
                        // looks like a function call. Future PRs can
                        // pattern-match on common volatile defaults
                        // (`CURRENT_TIMESTAMP`, `(unixepoch())`, etc.).
                        default_volatility: None,
                    },
                );
            }
            if !col_map.is_empty() {
                out.tables.insert(collection.clone(), col_map);
            }

            // 3. Indexes via `PRAGMA index_list` + `PRAGMA index_info`.
            //
            //    `index_list` columns: 0=seq, 1=name, 2=unique,
            //    3=origin, 4=partial. We exclude `origin='pk'` to match
            //    the PG impl's `NOT i.indisprimary` filter.
            let index_list_sql = format!("PRAGMA {q_app}.index_list({q_coll})");
            let idx_rows = self.session.query(&index_list_sql, &[]).await?;
            let mut idx_map = std::collections::HashMap::new();
            for row in &idx_rows {
                let idx_name = row
                    .get(1)
                    .and_then(|c| c.clone())
                    .unwrap_or_default();
                let is_unique = row
                    .get(2)
                    .and_then(|c| c.as_deref())
                    .map(|s| s != "0")
                    .unwrap_or(false);
                let origin = row
                    .get(3)
                    .and_then(|c| c.clone())
                    .unwrap_or_default();
                if origin == "pk" {
                    // PG impl skips primary-key indexes; we mirror.
                    // The auto-generated `sqlite_autoindex_*` names
                    // also appear here, and they all carry origin='pk'
                    // or 'u' (unique constraint). We surface 'u'-origin
                    // indexes because they correspond to declared
                    // UNIQUE columns the diff engine cares about.
                    continue;
                }

                // 4. Columns for this index via `PRAGMA index_info`.
                //    Returns: 0=seqno, 1=cid, 2=name.
                let q_idx = self.quote_ident(&idx_name);
                let index_info_sql = format!("PRAGMA {q_app}.index_info({q_idx})");
                let info_rows = self.session.query(&index_info_sql, &[]).await?;
                let mut columns = Vec::with_capacity(info_rows.len());
                for info_row in &info_rows {
                    let col_name = info_row
                        .get(2)
                        .and_then(|c| c.clone())
                        .unwrap_or_default();
                    columns.push(col_name);
                }

                idx_map.insert(
                    idx_name,
                    crate::diff::IndexInfo {
                        is_unique,
                        columns,
                        // SQLite indexes are always considered valid
                        // once `CREATE INDEX` returns — there is no
                        // analogue to PG's `indisvalid` (which can be
                        // false after a failed `CREATE INDEX
                        // CONCURRENTLY`). Mark every observed index
                        // valid; PR 5's IndexBuilder retry logic does
                        // not need a tri-state.
                        is_valid: true,
                    },
                );
            }
            if !idx_map.is_empty() {
                out.indexes.insert(collection.clone(), idx_map);
            }

            // 5. Foreign keys via `PRAGMA foreign_key_list`.
            //
            //    Columns: 0=id, 1=seq, 2=table (target),
            //    3=from (local column), 4=to (target column),
            //    5=on_update, 6=on_delete, 7=match.
            //
            //    We synthesise a `constraint_name` from the FK id +
            //    local column (SQLite doesn't expose user-given FK
            //    names through PRAGMA — only the implicit auto-name).
            //    The PG impl uses `pg_constraint.conname` directly.
            let fk_sql = format!("PRAGMA {q_app}.foreign_key_list({q_coll})");
            let fk_rows = self.session.query(&fk_sql, &[]).await?;
            let mut fk_map = std::collections::HashMap::new();
            for row in &fk_rows {
                let fk_id = row
                    .first()
                    .and_then(|c| c.clone())
                    .unwrap_or_default();
                let target_table = row
                    .get(2)
                    .and_then(|c| c.clone())
                    .unwrap_or_default();
                let from_col = row
                    .get(3)
                    .and_then(|c| c.clone())
                    .unwrap_or_default();
                let target_column = row
                    .get(4)
                    .and_then(|c| c.clone())
                    .unwrap_or_default();
                let on_update = row
                    .get(5)
                    .and_then(|c| c.clone())
                    .unwrap_or_default();
                let on_delete = row
                    .get(6)
                    .and_then(|c| c.clone())
                    .unwrap_or_default();
                let constraint_name = format!("fk_{fk_id}_{from_col}");
                fk_map.insert(
                    from_col.clone(),
                    crate::diff::ForeignKeyInfo {
                        constraint_name,
                        column: from_col,
                        target_table,
                        target_column,
                        // SQLite's PRAGMA already emits the upper-case
                        // SQL form ("CASCADE", "SET NULL", "NO
                        // ACTION", …); no decode step needed (contrast
                        // PG's single-char code).
                        on_delete,
                        on_update,
                        // SQLite FKs do not surface a deferrable bit
                        // through PRAGMA. The engine supports
                        // `DEFERRABLE INITIALLY DEFERRED` syntax but
                        // doesn't echo it back via foreign_key_list;
                        // default to `false` to match the PG impl's
                        // bool shape.
                        deferrable: false,
                    },
                );
            }
            if !fk_map.is_empty() {
                out.foreign_keys.insert(collection.clone(), fk_map);
            }
        }

        Ok(out)
    }

    /// Cheap row-count probe for the NOT-NULL-on-empty-table
    /// classifier branch.
    ///
    /// SQLite has no `pg_class.reltuples` analogue — every
    /// `SELECT COUNT(*)` is a full scan. The only consumer
    /// (`diff::classify_add_column`) needs the 0 / non-0
    /// distinction, so the scan cost is acceptable at dev scale
    /// (the orchestrator already holds the advisory lock — see plan
    /// §3.4). Future PRs can plug a `MAX(rowid)`-based fast path
    /// here if the dev-scale cost becomes an issue.
    async fn estimate_row_count(&self, app_id: &str, collection: &str) -> Result<i64, DbError> {
        let q_app = self.quote_ident(app_id);
        let q_coll = self.quote_ident(collection);
        let sql = format!("SELECT COUNT(*) FROM {q_app}.{q_coll}");
        let rows = self.session.query(&sql, &[]).await?;
        let n = rows
            .first()
            .and_then(|r| r.first())
            .and_then(|c| c.as_deref())
            .and_then(|s| s.parse::<i64>().ok())
            .unwrap_or(0);
        Ok(n)
    }
}

impl IndexBuilder for SqliteBackend {
    async fn create_index_with_recovery(
        &self,
        _app_id: &str,
        _collection: &str,
        _spec: &IndexSpec,
        _deploy_id: &str,
        _schema_version: i32,
    ) -> Result<(), DbError> {
        // PR 5: `CREATE [UNIQUE] INDEX IF NOT EXISTS` atomic; on
        // `SQLITE_CONSTRAINT_UNIQUE` (extended code 2067) classify
        // through `error::from_sqlite`, write a wire-compatible
        // `unique_violation` audit row, return the canonical
        // `DbError::SchemaRefused` envelope.
        Err(pr_stub("create_index_with_recovery"))
    }
}

impl DialectBuilder for SqliteBackend {
    // The backend forwards every dialect call to the `SqliteDialect`
    // ZST so consumers can hold an `&SqliteBackend` and reach the
    // dialect without naming the inner type. The ZST is instantiated
    // per call — rustc inlines the value away because every method on
    // `SqliteDialect` is `&self` and side-effect-free.

    fn quote_ident(&self, name: &str) -> String {
        SqliteDialect.quote_ident(name)
    }

    fn build_ensure_app_schema(&self, app_id: &str) -> String {
        SqliteDialect.build_ensure_app_schema(app_id)
    }

    fn build_create_index(&self, spec: &IndexSpec, online: bool) -> String {
        SqliteDialect.build_create_index(spec, online)
    }

    fn map_zs_type(&self, zs_type: &str, opts: &Value) -> String {
        SqliteDialect.map_zs_type(zs_type, opts)
    }

    fn now_fn(&self) -> &'static str {
        SqliteDialect.now_fn()
    }

    fn last_insert_rowid_sql(&self) -> Option<&'static str> {
        SqliteDialect.last_insert_rowid_sql()
    }
}

// `impl Backend for SqliteBackend {}` is intentionally absent at PR 1.
// PR 5 ties off every sub-trait + restores the marker impl. Routing
// the field through `BackendHandle::Sqlite(Rc<SqliteBackend>)` does
// NOT depend on the `Backend` marker (the `BackendHandle::with_sqlite`
// / `as_sqlite` accessors hand out `&SqliteBackend` directly).
//
// Why defer the marker? See §10 Q-P1-B in the implementation plan:
// the `Backend` super-trait relaxation lands in PR 1 (this commit)
// but the sub-trait impl set isn't real until PR 5. Adding the
// marker now would type-check (all sub-trait impls are present in
// stub form) but mislead any reader who searches for the impl to
// find a runtime-meaningful backend. Wait until PR 5 lights up the
// behaviour before naming it `: Backend`.

// `LockScope` is re-exported here so PR 4's `LockManager` impl can
// pull it in without crossing module boundaries. PR 1 does not
// reference it directly; the `#[allow(unused_imports)]` documents
// the intent.
#[allow(unused_imports)]
use super::LockScope as _LockScopeForPr4;

// Silence unused-import warnings on the LockScope item until PR 4
// wires the typed-lock primitives. The import above is the load-
// bearing one; this line ensures PR 1 builds clean.
#[allow(dead_code)]
fn _phantom_lock_scope(_s: &LockScope) {}

#[cfg(test)]
mod tests {
    //! Compile-time trait-shape assertions, mirroring the PR-0 set
    //! at `crate::backend::tests` (which target `PostgresBackend`).
    //! These pin the SQLite-side surface so any future drift in the
    //! capability-trait composition trips compilation here rather
    //! than at a distant orchestrator / context call site.
    //!
    //! Body intentionally empty (`fn assert_impl<T: Trait>() {}`) —
    //! the bound itself is the assertion.

    use super::*;
    use crate::backend::{
        DialectBuilder, IndexBuilder, LockManager, NamespaceManager, SchemaIntrospect,
        SqlExecutor,
    };

    fn assert_sqlite_backend_impls_sql_executor() {
        fn assert_impl<T: SqlExecutor>() {}
        assert_impl::<SqliteBackend>();
    }

    fn assert_sqlite_backend_impls_lock_manager() {
        fn assert_impl<T: LockManager>() {}
        assert_impl::<SqliteBackend>();
    }

    fn assert_sqlite_backend_impls_namespace_manager() {
        fn assert_impl<T: NamespaceManager>() {}
        assert_impl::<SqliteBackend>();
    }

    fn assert_sqlite_backend_impls_schema_introspect() {
        fn assert_impl<T: SchemaIntrospect<LiveSchema = crate::diff::LiveSchema>>() {}
        assert_impl::<SqliteBackend>();
    }

    fn assert_sqlite_backend_impls_index_builder() {
        fn assert_impl<T: IndexBuilder>() {}
        assert_impl::<SqliteBackend>();
    }

    fn assert_sqlite_backend_impls_dialect_builder() {
        fn assert_impl<T: DialectBuilder>() {}
        assert_impl::<SqliteBackend>();
    }

    fn assert_sqlite_backend_is_static() {
        fn assert_static<T: 'static>() {}
        assert_static::<SqliteBackend>();
    }

    /// Pin the `SqlExecutor::Client` associated type to the
    /// session-handle shape. A regression that swaps the type (e.g.
    /// accidentally re-pointing it to `rusqlite::Connection` rather
    /// than the actor-handle wrapper) trips here.
    fn assert_sqlite_client_pinned_to_session_handle() {
        fn assert_impl<T: SqlExecutor<Client = SqliteSessionHandle>>() {}
        assert_impl::<SqliteBackend>();
    }

    #[test]
    fn compile_time_trait_assertions_link() {
        // Keep the asserter functions live — same convention as the
        // PG-side `compile_time_assertions_link`.
        let _ = assert_sqlite_backend_impls_sql_executor as fn();
        let _ = assert_sqlite_backend_impls_lock_manager as fn();
        let _ = assert_sqlite_backend_impls_namespace_manager as fn();
        let _ = assert_sqlite_backend_impls_schema_introspect as fn();
        let _ = assert_sqlite_backend_impls_index_builder as fn();
        let _ = assert_sqlite_backend_impls_dialect_builder as fn();
        let _ = assert_sqlite_backend_is_static as fn();
        let _ = assert_sqlite_client_pinned_to_session_handle as fn();
    }
}
