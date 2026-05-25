//! SQLite-side [`crate::backend::ChangeStream`] adapter — the
//! `preupdate_hook` / `commit_hook` / `rollback_hook` integration.
//!
//! **PR 2** lights up the dispatcher: this file now installs the three
//! hooks on the writer-actor's `rusqlite::Connection`, buffers
//! per-transaction change events, and ships a `CommitPacket` over a
//! `flume` channel to a publisher task that calls
//! [`crate::broker::publish`] on the compio thread.
//!
//! Source plan: `docs/proposals/p2-sqlite-cdc-implementation-plan.md`
//! §2.2-2.5, §4-5, §11.
//!
//! ## Hook → publisher data flow
//!
//! ```text
//!   writer thread (std::thread)              compio thread
//!   ----------------------------             -------------
//!   INSERT INTO "app".t VALUES(...)
//!       │
//!       ▼
//!   sqlite3_preupdate_hook fires
//!       │
//!       ▼ preupdate_callback
//!   buffer.events.push(PendingEvent { … })
//!       │
//!       ▼ (transaction commits)
//!   sqlite3_commit_hook fires
//!       │
//!       ▼ commit_callback
//!   try_send(CommitPacket { events, commit_id })  ─────────►  publisher_task
//!                                                                  │
//!                                                                  ▼ (per event)
//!                                                            resolve column names via
//!                                                            session.query("PRAGMA …")
//!                                                                  │
//!                                                                  ▼
//!                                                            broker::publish(&ChangeEvent)
//! ```
//!
//! ## Cross-thread invariants
//!
//! - Hook closures captured by `preupdate_hook`/`commit_hook`/`rollback_hook`
//!   must be `Send + 'static`. All shared state uses `Arc<Mutex<…>>`,
//!   never `Rc<RefCell<…>>`.
//! - `ValueRef<'_>` borrows from SQLite scratch — copy to owned `String`
//!   inside the callback before any return path.
//! - `commit_hook` returns `bool` where `true = veto/rollback`. We
//!   always return `false`.
//! - SQLite forbids calling `prepare`/`step`/`execute` on the same
//!   `sqlite3*` connection from within the preupdate hook. Column-name
//!   resolution therefore happens lazily on the publisher side (compio
//!   thread, post-COMMIT) via the session actor's `Query` command.
//! - The broker is thread-local to the compio thread. The hook
//!   **never** calls `broker::publish` directly; the only path is via
//!   the `flume` channel and the publisher task.
//!
//! ## Column-name cache strategy
//!
//! The preupdate hook only receives column **values by index** (via
//! `PreUpdate{Old,New}ValueAccessor::get_*_column_value(i)`). It does
//! NOT receive column names — and we can't query `PRAGMA table_info`
//! from inside the hook because that would re-enter the connection
//! while the engine is mid-statement.
//!
//! The dispatcher buffers positional values
//! (`Vec<Option<String>>`) keyed by `(db_name, table)`, and the
//! publisher task — running on the compio thread, post-COMMIT —
//! resolves column names lazily via
//! `session.query("PRAGMA \"<db_name>\".table_info(\"<table>\")")`.
//! Names are memoised inside the publisher task in a local
//! `HashMap<(db, table), Arc<Vec<String>>>` so the second touch on the
//! same table doesn't pay another round-trip. Cache invalidation on
//! DDL is deferred to PR 4 (per plan §10 Q-P2-B); the engaged
//! schema-pending guard clears the cache at that point.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use base64::Engine;
use rusqlite::Connection;
use rusqlite::hooks::{Action, PreUpdateCase};
use rusqlite::types::ValueRef;

use crate::backend::sqlite::SqliteBackend;
use crate::backend::sqlite::session::SqliteSession;
use crate::backend::{BrokerPauseGuard, ChangeStream, SchemaPendingGuard};
use crate::broker::{ChangeEvent, ChangeOp};
use crate::error::DbError;

// ---------------------------------------------------------------------------
// Dispatcher state — captured by the hook closures
// ---------------------------------------------------------------------------

/// Per-transaction scratch buffer captured by the three hooks.
///
/// Lives behind an `Arc<Mutex<…>>` because the hook closures must be
/// `Send + 'static` (the writer thread is a `std::thread::spawn` worker;
/// the session actor's closures move into that thread's frame and
/// hold the only writer reference for the rest of the process). The
/// mutex is uncontended in steady state — the writer thread is the
/// only producer; the commit hook drains under the same lock.
pub(crate) struct CdcTxBuffer {
    /// Events queued since the last COMMIT / ROLLBACK boundary.
    pub(crate) events: Vec<PendingEvent>,
}

impl CdcTxBuffer {
    fn new() -> Self {
        Self { events: Vec::new() }
    }
}

/// One buffered event prior to column-name resolution.
///
/// The preupdate hook can't ask SQLite for column names (reentrancy
/// hazard — see module rustdoc). It stores positional values here; the
/// publisher task resolves names via `PRAGMA table_info` on the compio
/// thread after the transaction commits.
#[derive(Debug, Clone)]
pub(crate) struct PendingEvent {
    pub(crate) op: ChangeOp,
    /// ATTACH alias the preupdate hook reported — typically the
    /// per-app schema name (since `NamespaceManager::ensure_app_schema`
    /// ATTACHes each app file as its own alias). The publisher uses
    /// this as the `ChangeEvent::app_id` and as the schema-qualifier
    /// for the column-name PRAGMA.
    pub(crate) db_name: String,
    pub(crate) table: String,
    /// SQLite's stable per-row identifier. For an UPDATE we capture the
    /// new rowid (the post-image — same convention as the WAL consumer
    /// emits in [`crate::wal_consumer::emit_local`]).
    pub(crate) pk: Option<String>,
    /// Positional values for the new tuple (INSERT / UPDATE). `None`
    /// for DELETE.
    pub(crate) new_values: Option<Vec<Option<String>>>,
    /// Positional values for the old tuple (UPDATE / DELETE). `None`
    /// for INSERT.
    pub(crate) old_values: Option<Vec<Option<String>>>,
}

/// Cross-thread payload shipped over the flume channel at COMMIT time.
///
/// `commit_id` is the monotonic per-dispatcher sequence number. The
/// `ChangeEvent` shape (`crate::broker::ChangeEvent`) does NOT carry
/// `commit_id` today — surfacing it requires a broker-schema change
/// (plan §10 Q-P2-E) deferred until a subscriber consumes it.
#[derive(Debug)]
pub(crate) struct CommitPacket {
    pub(crate) events: Vec<PendingEvent>,
    #[allow(dead_code)] // PR 2: stamped but no downstream consumer yet.
    pub(crate) commit_id: u64,
}

/// Hook-captured dispatcher state. **Not exported across crate
/// boundaries** — the session actor owns one of these (kept alive on
/// the worker stack frame so the hooks' captures outlive the
/// `Connection`), and the publisher task owns the matching
/// `flume::Receiver<CommitPacket>` end.
#[allow(dead_code)]
pub(crate) struct SqliteCdcDispatcher {
    /// Per-tx buffer shared with all three hook closures.
    buffer: Arc<Mutex<CdcTxBuffer>>,
    /// Monotonic commit-id source — stamped on each `CommitPacket`.
    commit_id: Arc<AtomicU64>,
    /// Sender half of the worker→compio channel.
    packet_tx: flume::Sender<CommitPacket>,
}

/// Install the CDC hook triplet on a freshly opened `rusqlite::Connection`.
///
/// Called from the session worker thread before [`SqliteSession`]
/// enters its `Command` receive loop, so the hook closures capture
/// owned `Arc<Mutex<…>>` clones that outlive the connection. Dropping
/// the connection at thread exit unregisters all three hooks
/// automatically (rusqlite stores the boxed closures in
/// `InnerConnection` and frees them in `Drop`).
///
/// `app_id` is currently unused — the per-event app_id is derived
/// from the `db_name` argument the preupdate hook reports (the ATTACH
/// alias, which by convention equals the app id). The parameter is
/// retained so future PRs can override the dispatcher's "default"
/// label (e.g. for tests opening a connection without an ATTACH).
pub(crate) fn install(
    conn: &Connection,
    _app_id: Option<String>,
    packet_tx: flume::Sender<CommitPacket>,
) -> Result<SqliteCdcDispatcher, DbError> {
    let buffer = Arc::new(Mutex::new(CdcTxBuffer::new()));
    let commit_id = Arc::new(AtomicU64::new(0));

    let dispatcher = SqliteCdcDispatcher {
        buffer: buffer.clone(),
        commit_id: commit_id.clone(),
        packet_tx: packet_tx.clone(),
    };

    // preupdate hook: capture buffer.
    let buffer_pre = buffer.clone();
    conn.preupdate_hook(Some(
        move |action: Action, db_name: &str, table: &str, case: &PreUpdateCase| {
            preupdate_callback(action, db_name, table, case, &buffer_pre);
        },
    ))
    .map_err(crate::backend::sqlite::error::from_sqlite)?;

    // commit hook: drains buffer, ships packet. Returns `false` to
    // never veto the commit.
    let buffer_commit = buffer.clone();
    let commit_id_commit = commit_id.clone();
    let packet_tx_commit = packet_tx.clone();
    conn.commit_hook(Some(move || -> bool {
        commit_callback(&buffer_commit, &commit_id_commit, &packet_tx_commit)
    }))
    .map_err(crate::backend::sqlite::error::from_sqlite)?;

    // rollback hook: clears buffer; no channel send.
    let buffer_rb = buffer.clone();
    conn.rollback_hook(Some(move || {
        rollback_callback(&buffer_rb);
    }))
    .map_err(crate::backend::sqlite::error::from_sqlite)?;

    Ok(dispatcher)
}

// ---------------------------------------------------------------------------
// Hook bodies — each runs on the SQLite writer thread.
// ---------------------------------------------------------------------------

fn preupdate_callback(
    action: Action,
    db_name: &str,
    table: &str,
    case: &PreUpdateCase,
    buffer: &Arc<Mutex<CdcTxBuffer>>,
) {
    // Filter system / bookkeeping relations. PR 2 wired the full filter
    // set already (per plan §6: MV shadow, audit, migrations,
    // `__zs_*`, `sqlite_*`); PR 3 only adds the integration coverage +
    // the SDK-boundary refusal that keeps subscribers from opening on
    // `__zeroship_mv_*` names. This early-return is FIRST after the
    // action discriminant on purpose: filtered relations must never
    // build a `PendingEvent`, never touch the buffer mutex, never
    // increment any per-tx counters.
    if is_filtered_relation(table) {
        return;
    }

    // The hook also fires for writes to the "main" attached database
    // (the control session SqliteBackend opened with). For PR 2 we
    // suppress those — every app-side write lands against an ATTACHed
    // alias, and "main" only carries the control session's own
    // bookkeeping (none in PR 2 — the control session is empty).
    // Filtering here keeps the publisher's per-event app_id derivation
    // straightforward (`app_id = db_name`).
    if db_name == "main" {
        return;
    }

    // Build a PendingEvent based on the action variant. All
    // materialisation happens BEFORE this function returns — the
    // `ValueRef<'_>` references the engine's scratch storage and is
    // invalidated as soon as the hook unwinds.
    let pending = match case {
        PreUpdateCase::Insert(new_acc) => PendingEvent {
            op: ChangeOp::Insert,
            db_name: db_name.to_string(),
            table: table.to_string(),
            pk: Some(new_acc.get_new_row_id().to_string()),
            new_values: Some(materialise_new(new_acc)),
            old_values: None,
        },
        PreUpdateCase::Delete(old_acc) => PendingEvent {
            op: ChangeOp::Delete,
            db_name: db_name.to_string(),
            table: table.to_string(),
            pk: Some(old_acc.get_old_row_id().to_string()),
            new_values: None,
            old_values: Some(materialise_old(old_acc)),
        },
        PreUpdateCase::Update {
            old_value_accessor,
            new_value_accessor,
        } => PendingEvent {
            op: ChangeOp::Update,
            db_name: db_name.to_string(),
            table: table.to_string(),
            pk: Some(new_value_accessor.get_new_row_id().to_string()),
            new_values: Some(materialise_new(new_value_accessor)),
            old_values: Some(materialise_old(old_value_accessor)),
        },
        PreUpdateCase::Unknown => {
            // Plan §10 Q-P2-C — drop silently + tracing::warn once per
            // session. The "once" gate would require a session-level
            // flag; for PR 2 a per-fire warn is acceptable noise (the
            // variant only appears with engine/binding version skew).
            tracing::warn!(
                action = ?action,
                db_name = %db_name,
                table = %table,
                "SqliteCdcDispatcher: PreUpdateCase::Unknown observed; \
                 dropping event (rusqlite/sqlite3 version skew suspected)"
            );
            return;
        }
    };

    // Mutex contention: the writer thread is the only producer; the
    // commit hook is the only consumer; both run on the same thread,
    // serialised by the engine's statement evaluator. A poisoned lock
    // means a previous hook fire panicked — we log + drop the event
    // rather than re-panic inside an FFI callback (rusqlite catches
    // panics but the engine state is already iffy).
    match buffer.lock() {
        Ok(mut buf) => buf.events.push(pending),
        Err(e) => {
            tracing::error!(
                err = %e,
                "SqliteCdcDispatcher::preupdate_callback: buffer mutex poisoned; \
                 event dropped"
            );
        }
    }
}

fn commit_callback(
    buffer: &Arc<Mutex<CdcTxBuffer>>,
    commit_id: &Arc<AtomicU64>,
    packet_tx: &flume::Sender<CommitPacket>,
) -> bool {
    // `mem::take` swaps in a fresh empty Vec so subsequent hook fires
    // (in a new transaction) start with a clean buffer.
    let events = match buffer.lock() {
        Ok(mut buf) => std::mem::take(&mut buf.events),
        Err(e) => {
            tracing::error!(
                err = %e,
                "SqliteCdcDispatcher::commit_callback: buffer mutex poisoned; \
                 no packet shipped"
            );
            // `false` = never veto the commit — durability stays intact
            // even when CDC degrades.
            return false;
        }
    };

    if events.is_empty() {
        // No CDC-relevant changes in this tx — DDL-only, filtered
        // relations, or rollback-then-commit-empty. Skip the channel
        // send entirely.
        return false;
    }

    // Stamp the commit id BEFORE the channel send so a recipient that
    // sees the packet observes the commit_id the dispatcher would
    // observe next.
    let id = commit_id.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
    let packet = CommitPacket { events, commit_id: id };

    // With `flume::unbounded()`, `try_send` is structurally equivalent
    // to `send` — it never fails unless the receiver has dropped (a
    // disconnect at shutdown). We log + drop the packet on disconnect;
    // there's no useful recovery from inside a commit hook.
    match packet_tx.try_send(packet) {
        Ok(()) => {}
        Err(flume::TrySendError::Disconnected(_)) => {
            tracing::warn!(
                "SqliteCdcDispatcher::commit_callback: publisher channel disconnected; \
                 packet dropped (likely a teardown race)"
            );
        }
        Err(flume::TrySendError::Full(_)) => {
            // Unreachable with `flume::unbounded()` — included for
            // exhaustive match coverage so a future switch to a bounded
            // channel surfaces the overflow path explicitly here.
            tracing::warn!(
                "SqliteCdcDispatcher::commit_callback: publisher channel full; \
                 packet dropped (bounded-channel cutover surfaced)"
            );
        }
    }

    // commit_hook returns `bool` where `true = veto`. We never veto.
    false
}

fn rollback_callback(buffer: &Arc<Mutex<CdcTxBuffer>>) {
    match buffer.lock() {
        Ok(mut buf) => buf.events.clear(),
        Err(e) => {
            tracing::error!(
                err = %e,
                "SqliteCdcDispatcher::rollback_callback: buffer mutex poisoned; \
                 buffer may carry stale events into next tx"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Value materialisation — turn `ValueRef<'_>` into owned `Option<String>`
// before the callback returns (the engine reclaims the scratch on
// return).
// ---------------------------------------------------------------------------

fn materialise_new(acc: &rusqlite::hooks::PreUpdateNewValueAccessor) -> Vec<Option<String>> {
    let count = acc.get_column_count();
    let mut out = Vec::with_capacity(count as usize);
    for i in 0..count {
        // The accessor errors for out-of-range or column-not-modified
        // (`SQLITE_ERROR`) — we coerce to `None` so a partial UPDATE
        // surfaces a sentinel rather than crashing the publisher.
        let cell = acc
            .get_new_column_value(i)
            .ok()
            .and_then(|v| value_to_string(v));
        out.push(cell);
    }
    out
}

fn materialise_old(acc: &rusqlite::hooks::PreUpdateOldValueAccessor) -> Vec<Option<String>> {
    let count = acc.get_column_count();
    let mut out = Vec::with_capacity(count as usize);
    for i in 0..count {
        let cell = acc
            .get_old_column_value(i)
            .ok()
            .and_then(|v| value_to_string(v));
        out.push(cell);
    }
    out
}

fn value_to_string(v: ValueRef<'_>) -> Option<String> {
    // Mirror the JSON read-path shapes so broker predicate evaluation
    // sees the same scalar encoding a fetch would expose.
    match v {
        ValueRef::Null => None,
        ValueRef::Integer(n) => Some(n.to_string()),
        ValueRef::Real(f) => Some(f.to_string()),
        ValueRef::Text(bytes) => {
            // Lossy UTF-8 — a non-UTF-8 TEXT cell (rare; SQLite stores
            // arbitrary bytes in TEXT) shouldn't kill the publisher.
            // The PG side's pgoutput decoder makes the same trade-off.
            Some(String::from_utf8_lossy(bytes).into_owned())
        }
        ValueRef::Blob(bytes) => Some(base64::engine::general_purpose::STANDARD.encode(bytes)),
    }
}

/// Drop CDC for system / bookkeeping relations. Per plan §6 the
/// filter is:
///
/// - `__zeroship_mv_*`  — materialised-view shadow tables (§13.5)
/// - `__zeroship_audit_*` — audit trail (§10.7)
/// - `__zeroship_migrations` — migration audit table
/// - `__zs_*` — P1 SQLite bookkeeping (e.g. `__zs_migrations`)
/// - `sqlite_*` — engine-internal (`sqlite_master`, `sqlite_sequence`,
///   `sqlite_autoindex_*`)
fn is_filtered_relation(table: &str) -> bool {
    table.starts_with("__zeroship_mv_")
        || table.starts_with("__zeroship_audit_")
        || table == "__zeroship_migrations"
        || table.starts_with("__zs_")
        || table.starts_with("sqlite_")
}

// ---------------------------------------------------------------------------
// Publisher task — runs on the compio thread, owns the broker side.
// ---------------------------------------------------------------------------

/// Spawn the worker→compio publisher task.
///
/// Captures the session handle so it can resolve column names lazily
/// via `PRAGMA table_info` (which can't run from inside the preupdate
/// hook — see module rustdoc). Returns the `compio::runtime::JoinHandle`
/// the caller stores on [`SqliteBackend`] so dropping the backend
/// cancels the task (the task body is a `while let Ok(packet) =
/// rx.recv_async().await` loop; cancellation simply stops polling).
///
/// The future returned is `!Send` (it captures `Rc<SqliteSession>`);
/// compio is single-threaded per worker so spawning on the current
/// runtime is sound. The task never holds a borrow across `.await`
/// other than through the session actor's mpsc reply channel, which is
/// thread-safe by construction.
pub(crate) fn spawn_publisher(
    session: Rc<SqliteSession>,
    invalidations: Rc<RefCell<HashSet<(String, String)>>>,
    rx: flume::Receiver<CommitPacket>,
) -> compio::runtime::JoinHandle<()> {
    compio::runtime::spawn(publisher_loop(session, invalidations, rx))
}

async fn publisher_loop(
    session: Rc<SqliteSession>,
    invalidations: Rc<RefCell<HashSet<(String, String)>>>,
    rx: flume::Receiver<CommitPacket>,
) {
    // Per-task local cache: `(db_name, table) → Arc<Vec<String>>`. The
    // dispatcher's `column_cache` field is reserved for a future PR 3
    // pre-cache strategy that runs inside the writer thread. For PR 2
    // every name resolution flows through this map so the publisher
    // owns the lookup end-to-end (one PRAGMA round-trip per (db,
    // table) per process lifetime; tens of microseconds at dev scale).
    let mut name_cache: HashMap<(String, String), Vec<String>> = HashMap::new();

    while let Ok(packet) = rx.recv_async().await {
        // P2 PR 4 — backfill pause + schema-pending decoder fence
        // (plan §5 + §7). The publisher runs on the compio thread and
        // owns the broker-side; it is THE chokepoint where suppression
        // applies for the SQLite arm (the PG arm uses the legacy
        // `wal_consumer::is_app_suppressed` rail inside `emit_local`).
        //
        // Suppression is keyed by `app_id` and the dispatcher derives
        // the per-event `app_id` from the preupdate hook's `db_name`
        // parameter (the ATTACH alias by convention equals the app
        // id; see `cdc::install` + `preupdate_callback` for the
        // contract). A single packet may carry events for one app_id
        // — the writer is single-threaded and the buffer flushes
        // per-commit — but we group by `app_id` defensively so a
        // future multi-app commit (unlikely under the current actor
        // shape) still pulls the right flag set.
        //
        // The check is debug-only (NOT warn): both the backfill
        // window and the schema-pending window are normal lifecycle
        // events (`migrations.run` is the dominant caller of the
        // former; `bundle_invalidated` of the latter), and a stream
        // of warns during a deploy would spam operators.
        let mut suppressed_count: usize = 0;
        let mut schema_pending_count: usize = 0;
        let mut delivered: Vec<PendingEvent> = Vec::with_capacity(packet.events.len());
        for ev in packet.events {
            let app_id = ev.db_name.as_str();
            if crate::wal_consumer::is_app_suppressed(app_id) {
                suppressed_count += 1;
                continue;
            }
            if crate::broker::is_schema_pending(app_id) {
                schema_pending_count += 1;
                continue;
            }
            delivered.push(ev);
        }
        if suppressed_count > 0 {
            tracing::debug!(
                count = suppressed_count,
                commit_id = packet.commit_id,
                "SqliteCdcDispatcher publisher: dropped events under \
                 BrokerPauseGuard (backfill window)"
            );
        }
        if schema_pending_count > 0 {
            tracing::debug!(
                count = schema_pending_count,
                commit_id = packet.commit_id,
                "SqliteCdcDispatcher publisher: dropped events under \
                 SchemaPendingGuard (schema-pending decoder)"
            );
        }
        if delivered.is_empty() {
            continue;
        }

        for pending in delivered {
            // Look up column names for this (db, table). Cache miss
            // routes through the session actor's `Query` command —
            // safe to await here because we're on the compio thread
            // post-COMMIT, NOT inside a hook.
            let key = (pending.db_name.clone(), pending.table.clone());
            if invalidations.borrow_mut().remove(&key) {
                name_cache.remove(&key);
            }
            if !name_cache.contains_key(&key) {
                match fetch_column_names(&session, &pending.db_name, &pending.table).await {
                    Ok(names) => {
                        name_cache.insert(key.clone(), names);
                    }
                    Err(e) => {
                        tracing::warn!(
                            app_id = %pending.db_name,
                            table = %pending.table,
                            err = %e,
                            "SqliteCdcDispatcher publisher: PRAGMA table_info failed; \
                             publishing with positional column names"
                        );
                        // Fall through with empty names — `tuple_from_positional`
                        // synthesises `c0`/`c1`/… placeholders so the
                        // subscriber still sees a populated map.
                        name_cache.insert(key.clone(), Vec::new());
                    }
                }
            }
            let names = name_cache.get(&key).expect("just inserted");

            let new_tuple = pending
                .new_values
                .as_deref()
                .map(|vs| tuple_from_positional(names, vs))
                .unwrap_or_default();
            let old_tuple = pending
                .old_values
                .as_deref()
                .map(|vs| tuple_from_positional(names, vs));

            // `changed_columns` — per `ChangeEvent` rustdoc, INSERT
            // reports every column; UPDATE reports the SET-side; DELETE
            // is empty. The preupdate hook doesn't tell us WHICH
            // columns changed on an UPDATE (`get_new_column_value(i)`
            // returns `SQLITE_ERROR` for unchanged columns, which we
            // coerce to `None`), so we approximate "changed" as
            // "non-None in new_tuple" — matches what the WAL consumer
            // emits today.
            let changed_columns: Vec<String> = match pending.op {
                ChangeOp::Insert | ChangeOp::Update => new_tuple.keys().cloned().collect(),
                ChangeOp::Delete => Vec::new(),
            };

            let pk: Option<String> = new_tuple
                .get("id")
                .cloned()
                .or_else(|| old_tuple.as_ref().and_then(|tuple| tuple.get("id").cloned()))
                .or(pending.pk);

            let event = ChangeEvent {
                app_id: pending.db_name,
                collection: pending.table,
                op: pending.op,
                pk,
                changed_columns,
                new_tuple,
                old_tuple,
            };

            // The broker is thread-local to THIS thread — safe to call
            // directly. Suppression / schema-pending filters are
            // deferred to PR 4 (plan §9).
            crate::broker::publish(&event);
        }
    }
    // rx error = sender dropped (backend torn down). Exit cleanly.
}

async fn fetch_column_names(
    session: &SqliteSession,
    db_name: &str,
    table: &str,
) -> Result<Vec<String>, DbError> {
    // PRAGMA table_info returns rows of (cid, name, type, notnull,
    // dflt_value, pk). We only need column 1 (name) in cid order. The
    // identifier escaping mirrors `SqliteDialect::quote_ident`: double
    // any embedded `"`s and wrap in double-quotes.
    let q_db = quote_ident(db_name);
    let q_tbl = quote_ident(table);
    let sql = format!("PRAGMA {q_db}.table_info({q_tbl})");
    let rows = session.query(&sql, &[]).await?;
    let mut names = Vec::with_capacity(rows.len());
    for row in rows {
        // `row.get(1)` is the column-name cell, materialised as
        // `Option<String>` by the session's `run_query`. NULL or
        // missing → empty string (the engine never emits NULL here in
        // practice).
        let name = row.get(1).and_then(|c| c.clone()).unwrap_or_default();
        names.push(name);
    }
    Ok(names)
}

/// Build a `HashMap<column_name, value_string>` from positional
/// values + a resolved column-name list.
///
/// When the name list is short (cache lookup failed or returned
/// nothing), synthesise `c{i}` placeholders so the subscriber still
/// sees a populated map. NULL cells (`None` in the positional vec) are
/// elided — matches the PG side, where pgoutput omits NULL columns
/// from the tuple.
fn tuple_from_positional(
    names: &[String],
    values: &[Option<String>],
) -> HashMap<String, String> {
    let mut out = HashMap::with_capacity(values.len());
    for (i, cell) in values.iter().enumerate() {
        let Some(v) = cell else { continue };
        let key = names
            .get(i)
            .cloned()
            .unwrap_or_else(|| format!("c{i}"));
        out.insert(key, v.clone());
    }
    out
}

fn quote_ident(name: &str) -> String {
    let escaped = name.replace('"', "\"\"");
    format!("\"{escaped}\"")
}

// ---------------------------------------------------------------------------
// PR 1 stub — `SqliteChangeStream` adapter (kept; PR 4 wires the real
// `pause_broker` / `engage_schema_pending` bodies).
// ---------------------------------------------------------------------------

/// Handle returned by [`SqliteChangeStream::spawn_consumer`].
///
/// PR 2 keeps the unit-struct shape from PR 1. PR 4 grows it to carry
/// the dispatcher's commit-id watermark for `Resync` correlation; PR 2
/// has no consumer that observes the handle.
#[derive(Debug)]
pub struct SqliteConsumerHandle;

/// SQLite arm of the [`ChangeStream`] capability.
///
/// Constructed via [`crate::backend::BackendHandle::as_change_stream_sqlite`].
/// Owns an `Rc<SqliteBackend>` (Rc-cloned from the
/// [`crate::backend::BackendHandle::Sqlite`] arm) — same ownership
/// shape as the PG-arm adapter for the same `'static` reason
/// (`async fn`-in-trait futures don't compose with borrowed-reference
/// self).
///
/// **PR 2**: the hook triplet is installed automatically at
/// [`SqliteBackend::new`] time (see `backend/sqlite/mod.rs`) rather
/// than via this adapter — the writer-actor's lifetime IS the
/// dispatcher's lifetime, so deferred `provision` would just be a
/// noop. PR 4 may revisit if `provision`/`deprovision` grow per-app
/// state.
#[allow(dead_code, reason = "concrete adapter stays available for the sqlite change-stream capability surface")]
#[derive(Debug)]
pub struct SqliteChangeStream {
    backend: Rc<SqliteBackend>,
}

impl SqliteChangeStream {
    /// Construct an adapter holding an Rc-clone of `backend`.
    /// Crate-private — the
    /// [`crate::backend::BackendHandle::as_change_stream_sqlite`] accessor
    /// is the public entry point.
    pub(crate) fn new(backend: Rc<SqliteBackend>) -> Self {
        Self { backend }
    }
}

impl ChangeStream for SqliteChangeStream {
    type ConsumerHandle = SqliteConsumerHandle;

    /// **PR 2**: no-op. The dispatcher is installed at backend
    /// construction; `provision` would re-arm an already-armed
    /// connection, which is harmless on the engine side but adds no
    /// value. PR 4 may grow per-app `Command::CdcSuppress` admin
    /// commands here.
    async fn provision(&self, _app_id: &str) -> Result<(), DbError> {
        Ok(())
    }

    /// **PR 2**: no-op. PR 4 disarms hooks for a session whose app is
    /// being torn down.
    async fn deprovision(&self, _app_id: &str) -> Result<(), DbError> {
        Ok(())
    }

    /// **PR 2**: returns a unit handle. The publisher task is already
    /// running (spawned at `SqliteBackend::new`); there is no per-app
    /// consumer to spawn on the SQLite arm.
    async fn spawn_consumer(&self, _app_id: &str) -> Result<Self::ConsumerHandle, DbError> {
        Ok(SqliteConsumerHandle)
    }

    /// **PR 2**: returns a no-op [`BrokerPauseGuard`]. PR 4 wires the
    /// guard's `Drop` to clear the per-session `Command::CdcSuppress
    /// { on: false }` admin flag + emit `Broker::resume_app_with_resync`.
    fn pause_broker(&self, app_id: &str) -> BrokerPauseGuard {
        BrokerPauseGuard::new(app_id.to_string())
    }

    /// **PR 2**: returns a no-op [`SchemaPendingGuard`]. PR 4 wires the
    /// broker's thread-local `schema_pending_apps` set + `subscribe`
    /// rejection.
    fn engage_schema_pending(&self, app_id: &str) -> SchemaPendingGuard {
        SchemaPendingGuard::new(app_id.to_string())
    }
}

#[cfg(test)]
mod tests {
    //! Unit-level checks for the static helpers. End-to-end behaviour
    //! (hook → publisher → broker) is covered by the
    //! `tests/sqlite_integration.rs` mirror.

    use super::*;

    #[test]
    fn is_filtered_relation_excludes_system_tables() {
        assert!(is_filtered_relation("sqlite_master"));
        assert!(is_filtered_relation("sqlite_sequence"));
        assert!(is_filtered_relation("sqlite_autoindex_users_1"));
        assert!(is_filtered_relation("__zs_migrations"));
        assert!(is_filtered_relation("__zeroship_migrations"));
        assert!(is_filtered_relation("__zeroship_audit_users"));
        assert!(is_filtered_relation("__zeroship_mv_orders_shadow"));
    }

    #[test]
    fn is_filtered_relation_includes_user_tables() {
        assert!(!is_filtered_relation("users"));
        assert!(!is_filtered_relation("orders"));
        // Edge case: a user table whose name happens to start with
        // `__zs` (not `__zs_`) must NOT be filtered — the underscore
        // is the discriminator.
        assert!(!is_filtered_relation("__zsales"));
    }

    #[test]
    fn tuple_from_positional_with_full_names_maps_each_index() {
        let names = vec!["id".to_string(), "name".to_string()];
        let values = vec![Some("1".to_string()), Some("alice".to_string())];
        let m = tuple_from_positional(&names, &values);
        assert_eq!(m.len(), 2);
        assert_eq!(m.get("id"), Some(&"1".to_string()));
        assert_eq!(m.get("name"), Some(&"alice".to_string()));
    }

    #[test]
    fn tuple_from_positional_elides_null_cells() {
        let names = vec!["id".to_string(), "name".to_string()];
        let values = vec![Some("1".to_string()), None];
        let m = tuple_from_positional(&names, &values);
        // NULL → elided; matches PG pgoutput shape.
        assert_eq!(m.len(), 1);
        assert!(m.contains_key("id"));
        assert!(!m.contains_key("name"));
    }

    #[test]
    fn tuple_from_positional_synthesises_placeholder_when_names_missing() {
        // Empty names list = PRAGMA fetch failed; the publisher
        // synthesises `c{i}` placeholders so the subscriber still sees
        // a populated map.
        let names: Vec<String> = Vec::new();
        let values = vec![Some("1".to_string()), Some("alice".to_string())];
        let m = tuple_from_positional(&names, &values);
        assert_eq!(m.len(), 2);
        assert_eq!(m.get("c0"), Some(&"1".to_string()));
        assert_eq!(m.get("c1"), Some(&"alice".to_string()));
    }

    #[test]
    fn value_to_string_base64_encodes_blob_cells() {
        let blob = ValueRef::Blob(&[0x01, 0x02, 0xFF]);
        assert_eq!(
            value_to_string(blob),
            Some("AQL/".to_string()),
            "CDC tuple blobs must preserve byte content, not length-only placeholders"
        );
    }
}
