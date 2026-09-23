//! Capture SQLite changes on the actor thread and publish committed events.
//!
//! Preupdate hooks copy positional values into a transaction buffer. Commit hooks
//! stamp delivery disposition and enqueue packets; rollback hooks discard the
//! buffer. Sampling disposition at commit time keeps suppression independent of
//! publisher scheduling.
//!
//! The compio publisher resolves and caches column names through the session actor
//! before calling the change sink. Hooks cannot query the connection they run on,
//! and borrowed SQLite values must be copied before returning.

use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use base64::Engine;
use rusqlite::hooks::{Action, PreUpdateCase};
use rusqlite::types::ValueRef;
use rusqlite::Connection;
use zeroship_data_orm::cdc::{ChangeEvent, ChangeOp};

use crate::backend::sqlite::session::SqliteSession;
use crate::binding::DbRoute;
use crate::cdc::{ChangeSink, DeliveryDisposition};
use zeroship_data_orm::error::DbError;

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
    /// ATTACH alias the preupdate hook reported: the physical schema the
    /// binding addresses, which on this tier is one file per DATABASE.
    ///
    /// It is the schema-qualifier for the column-name PRAGMA, and it is not
    /// the routing key: the broker routes on the tenant AND the database, and
    /// one database may be bound by several apps.
    pub(crate) db_name: String,
    /// The route this event is published under: the tenant and the database.
    ///
    /// Filled at the commit boundary by expanding one physical change into one
    /// event per route bound to that alias, which is the dev tier's form of the
    /// fan-out the relay does from a datastore.
    pub(crate) route: DbRoute,
    pub(crate) table: String,
    /// Positional values for the new tuple (INSERT / UPDATE). `None`
    /// for DELETE.
    pub(crate) new_values: Option<Vec<Option<String>>>,
    /// Positional values for the old tuple (UPDATE / DELETE). `None`
    /// for INSERT.
    pub(crate) old_values: Option<Vec<Option<String>>>,
}

/// Cross-thread payload shipped over the flume channel at COMMIT time.
///
/// The commit identifier is local to one dispatcher and is used only for logs.
#[derive(Debug)]
pub(crate) struct CommitPacket {
    pub(crate) events: Vec<DispositionedEvent>,
    #[allow(dead_code)] // Stamped but no downstream consumer yet.
    pub(crate) commit_id: u64,
}

/// One buffered event plus the delivery decision taken for it at COMMIT time.
///
/// The pairing is the whole point: carrying the disposition on the packet is
/// what makes a suppression guard cover a *commit window* rather than a drain
/// window. See the module rustdoc.
#[derive(Debug)]
pub(crate) struct DispositionedEvent {
    pub(crate) event: PendingEvent,
    pub(crate) disposition: DeliveryDisposition,
}

/// Writer-thread half of the CDC wire: the packet channel plus the delivery
/// policy the `commit_hook` samples before enqueueing.
///
/// The two travel together deliberately. They were a bare
/// `flume::Sender<CommitPacket>` threaded through the session actor until
/// 2026-09-03; pairing them means every place that can enqueue a packet can
/// also stamp it, so there is no path that ships an unstamped commit.
#[derive(Clone)]
pub(crate) struct CommitSender {
    tx: flume::Sender<CommitPacket>,
    sink: Arc<dyn ChangeSink>,
}

impl std::fmt::Debug for CommitSender {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `dyn ChangeSink` is not `Debug` — the port stays minimal, and the
        // channel's disconnected/len state is the only thing worth printing.
        f.debug_struct("CommitSender")
            .field("disconnected", &self.tx.is_disconnected())
            .finish_non_exhaustive()
    }
}

impl CommitSender {
    pub(crate) fn new(tx: flume::Sender<CommitPacket>, sink: Arc<dyn ChangeSink>) -> Self {
        Self { tx, sink }
    }
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
    /// Sender half of the worker→compio channel, paired with the delivery
    /// policy the commit hook samples.
    packet_tx: CommitSender,
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
pub(crate) fn install(
    conn: &Connection,
    packet_tx: CommitSender,
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
    // Bookkeeping relations do not enter the change stream.
    if is_filtered_relation(table) {
        return;
    }

    // The hook also fires for writes to the "main" attached database
    // (the control session SqliteBackend opened with). Those are
    // suppressed - every app-side write lands against an ATTACHed
    // alias, and "main" only carries the control session's own
    // bookkeeping (none today - the control session is empty).
    // Filtering here keeps the commit boundary's fan-out to one lookup per
    // alias rather than one per row.
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
            // The commit boundary fills this by expanding the change into one
            // event per route bound to this alias; the hook knows no tenant.
            route: DbRoute::platform(""),
            table: table.to_string(),
            new_values: Some(materialise_new(new_acc)),
            old_values: None,
        },
        PreUpdateCase::Delete(old_acc) => PendingEvent {
            op: ChangeOp::Delete,
            db_name: db_name.to_string(),
            // The commit boundary fills this by expanding the change into one
            // event per route bound to this alias; the hook knows no tenant.
            route: DbRoute::platform(""),
            table: table.to_string(),
            new_values: None,
            old_values: Some(materialise_old(old_acc)),
        },
        PreUpdateCase::Update {
            old_value_accessor,
            new_value_accessor,
        } => PendingEvent {
            op: ChangeOp::Update,
            db_name: db_name.to_string(),
            // The commit boundary fills this by expanding the change into one
            // event per route bound to this alias; the hook knows no tenant.
            route: DbRoute::platform(""),
            table: table.to_string(),
            new_values: Some(materialise_new(new_value_accessor)),
            old_values: Some(materialise_old(old_value_accessor)),
        },
        PreUpdateCase::Unknown => {
            // The binding cannot describe this change safely, so drop it.
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
    packet_tx: &CommitSender,
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

    // Sample the delivery disposition HERE — this is the commit boundary, and
    // sampling it here is what makes a suppression guard cover the commits made
    // in its scope rather than the packets the publisher has yet to drain (see
    // the module rustdoc's "Delivery-window semantics"). Memoised by route:
    // the writer is single-threaded and a commit is one route in practice, so
    // this is one `disposition` call per commit, not per row.
    let mut sampled: HashMap<DbRoute, DeliveryDisposition> = HashMap::new();
    let events: Vec<DispositionedEvent> = events
        .into_iter()
        .flat_map(|event| {
            // One physical change becomes one event per route bound to that
            // alias. A database with no recorded binding publishes nothing:
            // the broker routes on the tenant AND the database, and stamping
            // the alias as a route would deliver to a subscription nobody
            // holds.
            super::routes_for_alias(&event.db_name)
                .into_iter()
                .map(|route| {
                    let disposition = match sampled.get(&route) {
                        Some(d) => *d,
                        None => {
                            let d = packet_tx.sink.disposition(&route);
                            sampled.insert(route.clone(), d);
                            d
                        }
                    };
                    DispositionedEvent {
                        event: PendingEvent {
                            route,
                            ..event.clone()
                        },
                        disposition,
                    }
                })
                .collect::<Vec<_>>()
        })
        .collect();

    // Stamp the commit id BEFORE the channel send so a recipient that
    // sees the packet observes the commit_id the dispatcher would
    // observe next.
    let id = commit_id.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
    let packet = CommitPacket {
        events,
        commit_id: id,
    };

    // With `flume::unbounded()`, `try_send` is structurally equivalent
    // to `send` — it never fails unless the receiver has dropped (a
    // disconnect at shutdown). We log + drop the packet on disconnect;
    // there's no useful recovery from inside a commit hook.
    match packet_tx.tx.try_send(packet) {
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

/// SQLite owns its catalog relations. Every other table in an attached creator
/// schema participates in CDC regardless of its name - including the
/// `__zeroship_` ones the platform writes there (materialized-view shadows, the
/// unmask audit ledger), which subscribers are meant to see.
///
/// The migration journal shares that database too and is NOT an exception here,
/// because it never reaches this hook: the hook is installed per CONNECTION by
/// `cdc::install`, from `open_lane_connection`, and the migration engine owns a
/// connection of its own with no hooks on it. A name fence here would therefore
/// filter nothing that arrives and would silence the platform tables that do.
fn is_filtered_relation(table: &str) -> bool {
    table.starts_with("sqlite_")
}

// ---------------------------------------------------------------------------
// Publisher task — runs on the compio thread, owns the broker side.
// ---------------------------------------------------------------------------

/// Spawn the worker→compio publisher task.
///
/// Captures the session handle so it can resolve column names lazily
/// via `PRAGMA table_info` (which can't run from inside the preupdate
/// hook — see module rustdoc). Returns the `compio::runtime::JoinHandle`
/// the caller stores on [`super::SqliteBackend`] so dropping the backend
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
    rx: flume::Receiver<CommitPacket>,
    sink: Arc<dyn ChangeSink>,
) -> compio::runtime::JoinHandle<()> {
    compio::runtime::spawn(publisher_loop(session, rx, sink))
}

async fn publisher_loop(
    session: Rc<SqliteSession>,
    rx: flume::Receiver<CommitPacket>,
    sink: Arc<dyn ChangeSink>,
) {
    let mut name_cache: HashMap<(String, String), (Vec<String>, Option<String>)> = HashMap::new();
    let mut schema_versions: HashMap<String, String> = HashMap::new();

    while let Ok(packet) = rx.recv_async().await {
        // Delivery state is stamped at commit so later scheduling cannot change it.
        let mut suppressed_count: usize = 0;
        let mut schema_pending_count: usize = 0;
        let mut delivered: Vec<PendingEvent> = Vec::with_capacity(packet.events.len());
        for ev in packet.events {
            match ev.disposition {
                DeliveryDisposition::Deliver => delivered.push(ev.event),
                DeliveryDisposition::Suppressed => suppressed_count += 1,
                DeliveryDisposition::SchemaPending => schema_pending_count += 1,
            }
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

        let mut checked_databases = HashSet::new();
        for pending in delivered {
            if checked_databases.insert(pending.db_name.clone()) {
                match fetch_schema_version(&session, &pending.db_name).await {
                    Ok(version) => {
                        if schema_versions
                            .insert(pending.db_name.clone(), version.clone())
                            .is_some_and(|previous| previous != version)
                        {
                            name_cache.retain(|(db_name, _), _| db_name != &pending.db_name);
                        }
                    }
                    Err(error) => {
                        tracing::warn!(
                            app_id = %pending.db_name,
                            err = %error,
                            "SQLite CDC schema version lookup failed"
                        );
                    }
                }
            }

            let key = (pending.db_name.clone(), pending.table.clone());
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
                        name_cache.insert(key.clone(), (Vec::new(), None));
                    }
                }
            }
            let (names, identity) = name_cache.get(&key).expect("just inserted");

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

            let pk = identity.as_deref().and_then(|key| {
                new_tuple
                    .get(key)
                    .cloned()
                    .or_else(|| old_tuple.as_ref().and_then(|tuple| tuple.get(key).cloned()))
            });

            let event = ChangeEvent {
                route: pending.route,
                collection: pending.table,
                op: pending.op,
                pk,
                changed_columns,
                new_tuple,
                old_tuple,
            };

            sink.publish(&event);
        }
    }
    // rx error = sender dropped (backend torn down). Exit cleanly.
}

async fn fetch_schema_version(session: &SqliteSession, db_name: &str) -> Result<String, DbError> {
    let sql = format!("PRAGMA {}.schema_version", quote_ident(db_name));
    session
        .query(&sql, &[])
        .await?
        .first()
        .and_then(|row| row.first())
        .and_then(Clone::clone)
        .ok_or_else(|| DbError::internal("SQLite did not return a schema version"))
}

async fn fetch_column_names(
    session: &SqliteSession,
    db_name: &str,
    table: &str,
) -> Result<(Vec<String>, Option<String>), DbError> {
    // Read column names in storage order and discover the declared primary key.
    let q_db = quote_ident(db_name);
    let q_tbl = quote_ident(table);
    let sql = format!("PRAGMA {q_db}.table_info({q_tbl})");
    let rows = session.query(&sql, &[]).await?;
    let mut names = Vec::with_capacity(rows.len());
    let mut keys = Vec::new();
    for row in rows {
        let name = row.get(1).and_then(|c| c.clone()).unwrap_or_default();
        if row
            .get(5)
            .and_then(|cell| cell.as_deref())
            .is_some_and(|value| value != "0")
        {
            keys.push(name.clone());
        }
        names.push(name);
    }
    let identity = if keys.len() == 1 { keys.pop() } else { None };
    Ok((names, identity))
}

/// Build a `HashMap<column_name, value_string>` from positional
/// values + a resolved column-name list.
///
/// When the name list is short (cache lookup failed or returned
/// nothing), synthesise `c{i}` placeholders so the subscriber still
/// sees a populated map. NULL cells (`None` in the positional vec) are
/// elided — matches the PG side, where pgoutput omits NULL columns
/// from the tuple.
fn tuple_from_positional(names: &[String], values: &[Option<String>]) -> HashMap<String, String> {
    let mut out = HashMap::with_capacity(values.len());
    for (i, cell) in values.iter().enumerate() {
        let Some(v) = cell else { continue };
        let key = names.get(i).cloned().unwrap_or_else(|| format!("c{i}"));
        out.insert(key, v.clone());
    }
    out
}

fn quote_ident(name: &str) -> String {
    let escaped = name.replace('"', "\"\"");
    format!("\"{escaped}\"")
}

// ---------------------------------------------------------------------------
// `SqliteChangeStream` adapter.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    //! Unit-level checks for the static helpers, plus the delivery-window
    //! fences. End-to-end behaviour (hook → publisher → broker) is covered by
    //! the `crates/zeroship-data-orm/src/tests/sqlite/cdc.rs` mirror; what lives here is the part
    //! that mirror CANNOT state deterministically — the interleaving of a guard
    //! drop with an undrained channel.

    use super::*;
    use std::sync::atomic::AtomicUsize;

    /// A [`ChangeSink`] whose answer the test flips, standing in for engaging
    /// and dropping a `BrokerPauseGuard` / `SchemaPendingGuard`.
    ///
    /// `Mutex`, not `Cell`: the commit hook samples `disposition` from the
    /// SQLite writer thread while the test drives the publisher on the compio
    /// thread, which is the whole reason [`ChangeSink`] is `Send + Sync`.
    struct GuardSink {
        disposition: Mutex<DeliveryDisposition>,
        published: AtomicUsize,
    }

    impl GuardSink {
        fn new(initial: DeliveryDisposition) -> Self {
            Self {
                disposition: Mutex::new(initial),
                published: AtomicUsize::new(0),
            }
        }

        fn set(&self, next: DeliveryDisposition) {
            *self.disposition.lock().expect("GuardSink mutex") = next;
        }

        fn published(&self) -> usize {
            self.published.load(Ordering::SeqCst)
        }
    }

    impl ChangeSink for GuardSink {
        fn disposition(&self, _route: &DbRoute) -> DeliveryDisposition {
            *self.disposition.lock().expect("GuardSink mutex")
        }

        fn publish(&self, _event: &ChangeEvent) {
            self.published.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// The ONE route the window fixture records against its alias.
    ///
    /// **Process-wide and minted once**, because `ALIAS_ROUTES` is a set that
    /// accumulates: a route minted per call would leave the alias carrying one
    /// more route on each arm, the commit boundary would fan one physical
    /// change out to all of them, and the delivery counts below would grow with
    /// the number of arms that ran before them.
    fn window_route() -> &'static DbRoute {
        static ROUTE: std::sync::OnceLock<DbRoute> = std::sync::OnceLock::new();
        ROUTE.get_or_init(|| {
            DbRoute::new("app_window_tenant", Some(zeroship_core::DatabaseId::mint()))
        })
    }

    /// Drive one commit through the real hooks with `at_commit` in force, then
    /// switch to `at_drain` BEFORE the publisher exists, then drain. Returns
    /// how many events reached [`ChangeSink::publish`].
    ///
    /// **The determinism is structural, not a sleep.** `publisher_loop` is not
    /// called until after the disposition has been switched, so the packet is
    /// provably still in the channel when the switch happens — the assertion
    /// below checks exactly that with `rx.len()`. There is no ordering left for
    /// a scheduler to decide.
    async fn drive_window(at_commit: DeliveryDisposition, at_drain: DeliveryDisposition) -> usize {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("cdc-window.sqlite");
        // The preupdate hook drops writes to `main` (that is the control
        // session's own file), so the fixture writes through an ATTACHed alias
        // exactly as a binding does. The commit boundary publishes one event
        // per ROUTE recorded against that alias, so the fixture records one.
        let app_path = dir.path().join("zs-app_window.sqlite");
        let app_path = app_path.to_string_lossy().into_owned();
        super::super::record_alias_route("app_window", window_route().clone());

        let (tx, rx) = flume::unbounded::<CommitPacket>();
        let sink = Arc::new(GuardSink::new(at_commit));

        // Writer session: the real hook triplet, the real `commit_callback`.
        let writer = SqliteSession::open(
            &db_path,
            Some(CommitSender::new(tx, sink.clone() as Arc<dyn ChangeSink>)),
        )
        .expect("open writer session");
        writer
            .attach("app_window", &app_path)
            .await
            .expect("ATTACH app file");
        writer
            .exec(
                "CREATE TABLE \"app_window\".\"items\" \
                 (id INTEGER PRIMARY KEY, name TEXT NOT NULL)",
                &[],
            )
            .await
            .expect("CREATE TABLE");
        writer
            .exec(
                "INSERT INTO \"app_window\".\"items\" (id, name) VALUES (1, 'in-window')",
                &[],
            )
            .await
            .expect("INSERT");

        // The commit has happened and NOTHING has drained it: no publisher task
        // exists yet. The DDL above contributes no packet (`sqlite_master` is a
        // filtered relation), so this is the INSERT's packet and only it.
        assert_eq!(
            rx.len(),
            1,
            "the fixture is only meaningful while the packet is still queued"
        );

        // The guard's scope ends here — after the commit, before any drainage.
        sink.set(at_drain);

        // A second session serves the publisher's `PRAGMA table_info` lookups,
        // so dropping the writer can disconnect the channel and let
        // `publisher_loop` return. One session cannot do both: the publisher
        // borrows it for the lifetime of the loop.
        let reader = SqliteSession::open(&db_path, None).expect("open reader");
        reader
            .attach("app_window", &app_path)
            .await
            .expect("ATTACH on the reader");
        let reader = Rc::new(reader);
        drop(writer);

        publisher_loop(reader, rx, sink.clone() as Arc<dyn ChangeSink>).await;

        sink.published()
    }

    #[compio::test]
    async fn a_backfill_guard_dropped_before_drainage_still_suppresses_its_commit() {
        // The delivery disposition is captured when the commit is queued, so a
        // later guard drop cannot publish a suppressed event.
        assert_eq!(
            drive_window(
                DeliveryDisposition::Suppressed,
                DeliveryDisposition::Deliver
            )
            .await,
            0,
            "a commit made inside a backfill window must stay suppressed even when \
             the guard drops before the publisher drains the channel"
        );
    }

    #[compio::test]
    async fn a_schema_pending_guard_dropped_before_drainage_still_drops_its_commit() {
        // Same fence on the other rail. This one is not merely noise-control:
        // the positional values were captured against the pre-DDL column order,
        // and the publisher re-resolves names AFTER the invalidation lands, so
        // delivering it would decode an old tuple against a new schema.
        assert_eq!(
            drive_window(
                DeliveryDisposition::SchemaPending,
                DeliveryDisposition::Deliver
            )
            .await,
            0,
            "a commit made inside a schema-pending window must stay dropped even when \
             the guard drops before the publisher drains the channel"
        );
    }

    #[compio::test]
    async fn an_unguarded_commit_is_delivered() {
        // The control, differing from the two cases above in exactly one
        // variable: the disposition in force AT COMMIT. Without it, "0
        // published" would also be satisfied by a fixture that never publishes
        // anything at all.
        assert_eq!(
            drive_window(DeliveryDisposition::Deliver, DeliveryDisposition::Deliver).await,
            1,
            "the fixture must be able to observe a publish"
        );
    }

    #[compio::test]
    async fn a_commit_made_before_a_guard_is_engaged_is_still_delivered() {
        // The converse direction, and it is a consequence of the contract
        // rather than a gap in it: a commit that landed before the guard was
        // engaged is outside the window, so a guard engaged while it sits in
        // the channel must not swallow it. This is also why the publisher's
        // `.await` on the column-name PRAGMA needs no re-check.
        assert_eq!(
            drive_window(
                DeliveryDisposition::Deliver,
                DeliveryDisposition::Suppressed
            )
            .await,
            1,
            "a commit made outside any window must be delivered even if a guard is \
             engaged before the publisher drains it"
        );
    }

    #[test]
    fn only_sqlite_catalog_relations_are_filtered() {
        assert!(is_filtered_relation("sqlite_master"));
        assert!(is_filtered_relation("sqlite_sequence"));
        assert!(is_filtered_relation("sqlite_autoindex_users_1"));
        for table in [
            "users",
            "__zs_migrations",
            "__zeroship_migrations",
            "__zeroship_audit_users",
            "__zeroship_mv_orders_shadow",
        ] {
            assert!(
                !is_filtered_relation(table),
                "creator-schema table {table} must reach CDC"
            );
        }
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
