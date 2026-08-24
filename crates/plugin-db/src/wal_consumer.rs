//! Streaming WAL consumer.
//!
//! This module owns the long-running task that bridges Postgres
//! logical-decoding output (`pgoutput` over the streaming-replication
//! protocol) into the in-process [`crate::broker`]. It is the
//! cross-worker leg of reactive queries: a row written on worker A
//! reaches a subscriber on worker B because every worker consumes the
//! same app publication through its own logical slot.
//!
//! ## Design
//!
//! The local-emit fast path publishes directly to the process-wide
//! broker on success. That works for the single-worker case (the
//! platform routes per-app traffic via CHWBL so it's the common case)
//! but offers nothing when the writer and subscriber happen to land on
//! different workers.
//!
//! This module adds:
//!
//! 1. [`WalConsumer`] — a compio task that opens a
//!    `replication=database` connection (via
//!    [`compio_postgres::replication::connect_replication`]), issues
//!    `START_REPLICATION SLOT ... LOGICAL ...`, decodes pgoutput
//!    frames, and publishes [`ChangeEvent`]s into the broker.
//! 2. [`emit_local`] consults a process-wide, per-app suppression
//!    count. While a consumer is active, WAL is the sole source of
//!    truth in every isolate thread in the worker process.
//! 3. A relation cache (`rel_id -> (namespace, table, columns)`)
//!    populated from pgoutput `Relation` messages. Required to map a
//!    `Insert { rel_id, tuple }` back to a `(collection, pk,
//!    changed_columns)` broker event.
//!
//!
//! [`run_supervised_controlled`] adds a readiness boundary, explicit
//! shutdown, and reconnect backoff. It reports ready only after
//! `START_REPLICATION` succeeds.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use futures::FutureExt;

use compio_postgres::replication::{
    self as repl, ReplicationMessage, ReplicationStream, StartReplicationOptions,
    pgoutput::{self, OldTuple, PgOutputMessage, TupleColumn, TupleData},
};

use crate::broker::{has_subscribers, publish, ChangeEvent, ChangeOp};
use crate::error::DbError;

// ---------------------------------------------------------------------------
// Per-app emit-suppression
// ---------------------------------------------------------------------------
//
// When a WAL consumer is active for app A in this process, local-emit
// for app A must become a no-op — the consumer publishes the same
// event on the cross-worker path and emitting locally too would
// double-deliver. Other apps on the same thread must continue to use
// local-emit; a coarse thread-wide flag would silence their events as
// well.
//
// Counts are process-wide because consumer and mutation tasks can run
// on different isolate threads.

/// Process-wide suppression counts keyed by app id.
///
/// A WAL consumer and mutations for the same app can run on different
/// compio threads. Process scope is therefore required for the local
/// fast path to see that WAL is authoritative. Counts, rather than a
/// set, prevent one overlapping guard from unsuppressing another.
static SUPPRESSED_APPS: LazyLock<Mutex<HashMap<String, usize>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn suppressed_apps() -> std::sync::MutexGuard<'static, HashMap<String, usize>> {
    SUPPRESSED_APPS.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Suppress local-emit for `app_id` in this process. Mutation callbacks
/// that produce events for this app will become no-ops until
/// [`unsuppress_app`] is called (typically via the Drop guard returned
/// by [`SuppressGuard::activate`]).
pub fn suppress_app(app_id: &str) {
    *suppressed_apps().entry(app_id.to_string()).or_default() += 1;
}

/// Inverse of [`suppress_app`]. Idempotent.
pub fn unsuppress_app(app_id: &str) {
    let mut apps = suppressed_apps();
    if let Some(count) = apps.get_mut(app_id) {
        *count -= 1;
        if *count == 0 {
            apps.remove(app_id);
        }
    }
}

/// True when the given app's local-emit path is suppressed on this
/// thread (i.e. a [`WalConsumer`] is running for that app).
pub fn is_app_suppressed(app_id: &str) -> bool {
    suppressed_apps().contains_key(app_id)
}

/// RAII guard: suppresses local-emit for one app on construction,
/// unsuppresses on drop (including panic-unwind). The consumer's run
/// loop holds one of these for the duration of its decode loop.
#[derive(Debug)]
pub struct SuppressGuard {
    app_id: String,
}

impl SuppressGuard {
    /// Activate suppression for `app_id`. The guard's `Drop` impl
    /// removes the app from the suppressed set, so even a panic inside
    /// the consumer leaves local-emit re-enabled for that app.
    pub fn activate(app_id: &str) -> Self {
        suppress_app(app_id);
        Self {
            app_id: app_id.to_string(),
        }
    }
}

impl Drop for SuppressGuard {
    fn drop(&mut self) {
        unsuppress_app(&self.app_id);
    }
}

/// Emit a local change event into the in-process broker.
///
/// Called from the mutation callbacks (`insert`, `update_one`,
/// `delete_one`, ...) after a successful SQL run.
///
/// When this app is suppressed, this is a no-op. The WAL consumer is
/// publishing the same event on the cross-worker path and emitting
/// locally too would double-deliver.
///
/// The `new_tuple` is the row's post-image (or pre-image for
/// DELETE) — used by the broker's read-set narrowing to test each
/// subscriber's predicate. May be empty when the caller doesn't have a
/// tuple snapshot to hand; predicate evaluation treats missing columns
/// as non-matching (the conservative direction).
pub fn emit_local(
    app_id: &str,
    collection: &str,
    op: ChangeOp,
    pk: Option<String>,
    changed_columns: Vec<String>,
    new_tuple: std::collections::HashMap<String, String>,
) {
    if is_app_suppressed(app_id) {
        return;
    }
    publish(&ChangeEvent {
        app_id: app_id.to_string(),
        collection: collection.to_string(),
        op,
        pk,
        changed_columns,
        new_tuple,
        old_tuple: None,
    });
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Errors raised by one controlled WAL-consumer attempt.
///
/// Note: pre-flight failures from [`WalConsumer::new`] do NOT flow
/// through this enum — they are surfaced as [`crate::error::DbError`]
/// directly so the SDK can branch on `.code` (e.g. `invalid_app_id`
/// vs `not_provisioned`). See the doc comment on `WalConsumer::new`.
#[derive(Debug)]
enum ConsumerError {
    /// Establishing the replication connection failed.
    Connect(String),
    /// An I/O error on the wire (the supervising task should
    /// reconnect with backoff).
    Io(String),
    /// pgoutput decode failure — usually a sign of a protocol-version
    /// mismatch (we use v1; PG 17+ defaults to higher).
    Decode(String),
}

impl std::fmt::Display for ConsumerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConsumerError::Connect(s) => write!(f, "wal consumer: connect: {s}"),
            ConsumerError::Io(s) => write!(f, "wal consumer: io: {s}"),
            ConsumerError::Decode(s) => write!(f, "wal consumer: decode: {s}"),
        }
    }
}

impl std::error::Error for ConsumerError {}

#[derive(Debug)]
enum ControlledAttempt {
    Shutdown,
    StreamEnd,
    StartupFailed(DbError),
    RuntimeFailed(ConsumerError),
}

fn consumer_error_to_db(error: &ConsumerError) -> DbError {
    match error {
        ConsumerError::Connect(message) | ConsumerError::Io(message) => DbError::Transient {
            message: format!("wal consumer: {message}"),
        },
        ConsumerError::Decode(message) => DbError::Internal {
            message: format!("wal consumer: pgoutput decode: {message}"),
        },
    }
}

fn startup_failure(
    startup: Option<&flume::Sender<Result<(), DbError>>>,
    error: ConsumerError,
) -> ControlledAttempt {
    if let Some(startup) = startup {
        let db_error = consumer_error_to_db(&error);
        let _ = startup.send(Err(db_error.clone()));
        ControlledAttempt::StartupFailed(db_error)
    } else {
        ControlledAttempt::RuntimeFailed(error)
    }
}

// ---------------------------------------------------------------------------
// Relation cache
// ---------------------------------------------------------------------------

/// One entry in the relation cache. Populated by pgoutput `Relation`
/// messages; referenced by subsequent `Insert` / `Update` / `Delete`
/// messages via `rel_id`.
#[derive(Debug, Clone)]
struct RelationEntry {
    /// Postgres namespace (= schema). We use this to map an app's
    /// per-schema tables back to the app's `(app_id, collection)`
    /// pair: every event whose namespace equals the consumer's
    /// `app_id` belongs to this app.
    namespace: String,
    /// Table name within the namespace.
    table: String,
    /// Column metadata in declaration order. Used to:
    /// 1. Extract `pk` — we look for the first column flagged as
    ///    replica-identity-key.
    /// 2. Surface `changed_columns` to the broker event (used for
    ///    read-set filtering).
    columns: Vec<pgoutput::RelationColumn>,
}

impl RelationEntry {
    fn primary_key_index(&self) -> Option<usize> {
        self.columns.iter().position(|c| (c.flags & 0x01) != 0)
    }
}

// ---------------------------------------------------------------------------
// WalConsumer
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
/// Configuration + state for the per-app WAL consumer.
///
/// One instance per app per worker. Holds:
///
/// - The replication connection URL (regular DB URL with
///   `replication=database` appended internally).
/// - The slot + publication names ([`crate::replication`] computes
///   them deterministically from the app_id).
/// - The relation cache (populated lazily as pgoutput Relation
///   messages arrive).
///
/// `Clone` is cheap (three short strings) — the supervisor clones one
/// descriptor per reconnect attempt because [`WalConsumer::run`]
/// consumes `self`.
pub(crate) struct WalConsumer {
    app_id: String,
    db_url: String,
    slot_name: String,
    publication_name: String,
    /// Resume LSN — `"0/0"` means "use the slot's
    /// `confirmed_flush_lsn`". Set explicitly by callers that
    /// remember the last advanced LSN across restarts.
    start_lsn: String,
}

impl WalConsumer {
    /// Build a consumer descriptor. The controlled supervisor opens
    /// the connection after provisioning finishes.
    ///
    /// # Errors
    ///
    /// Returns a typed [`crate::error::DbError`] so the SDK can
    /// distinguish failure classes by `.code`:
    ///
    /// - [`DbError::ValidationFailed`] with `code = "invalid_app_id"`:
    ///   the `app_id` is empty or contains NUL. Restarting will not
    ///   help; the deploy needs a valid id.
    /// - [`DbError::Configuration`] with `code = "not_provisioned"` —
    ///   the runtime context has no `db_url` configured. Operator
    ///   must set `DB_URL` (or equivalent) before replication can run.
    ///
    /// The two classes are kept distinct on purpose: an
    /// `invalid_app_id` is a developer/deploy error; a missing
    /// `db_url` is an operator/configuration error. The SDK branches
    /// on `.code` to surface the right remediation.
    pub(crate) fn new(app_id: &str, worker_id: &str, db_url: &str) -> Result<Self, DbError> {
        if db_url.is_empty() {
            return Err(DbError::Configuration {
                code: "not_provisioned",
                message: "wal consumer: db_url not configured".to_string(),
                hint: Some(
                    "replication requires a connected runtime context; set the worker's \
                     ZEROSHIP_WORKER_DATABASE_URL (or --database-url-file) so the \
                     runtime can mint a replication=database connection"
                        .to_string(),
                ),
            });
        }
        // slot_name / publication_name already return Result<String,
        // DbError> — propagate the typed error so the SDK sees
        // `.code = "invalid_app_id"` for validation failures and not
        // an opaque `"not_provisioned"` re-stamp.
        let slot_name = crate::replication::worker_slot_name(app_id, worker_id)?;
        let publication_name = crate::replication::publication_name(app_id)?;
        Ok(Self {
            app_id: app_id.to_string(),
            db_url: db_url.to_string(),
            slot_name,
            publication_name,
            start_lsn: "0/0".to_string(),
        })
    }

    /// Resume from a specific LSN on the controlled consumer. Pass the value
    /// returned by [`crate::replication::ensure_worker_slot`].
    pub(crate) fn with_start_lsn(mut self, lsn: impl Into<String>) -> Self {
        self.start_lsn = lsn.into();
        self
    }

    /// App id this consumer is bound to. Exposed so the supervisor can
    /// log it without cloning the whole consumer.
    pub(crate) fn app_id(&self) -> &str {
        &self.app_id
    }

    async fn run_controlled_once(
        self,
        shutdown: &flume::Receiver<()>,
        startup: Option<&flume::Sender<Result<(), DbError>>>,
    ) -> ControlledAttempt {
        let url = ensure_replication_param(&self.db_url);
        let config = match url.parse::<compio_postgres::Config>() {
            Ok(config) => config,
            Err(e) => {
                return startup_failure(
                    startup,
                    ConsumerError::Connect(e.to_string()),
                );
            }
        };
        let mut conn = match repl::connect_replication(compio_postgres::NoTls, &config).await {
            Ok(conn) => conn,
            Err(e) => {
                return startup_failure(
                    startup,
                    ConsumerError::Connect(e.to_string()),
                );
            }
        };
        if let Err(e) = conn.identify_system().await {
            return startup_failure(startup, ConsumerError::Io(e.to_string()));
        }
        let opts = StartReplicationOptions {
            slot_name: &self.slot_name,
            start_lsn: &self.start_lsn,
            proto_version: 1,
            publication_names: &[&self.publication_name],
            ..Default::default()
        };
        let stream = match conn.start_logical_replication(opts).await {
            Ok(stream) => stream,
            Err(e) => {
                return startup_failure(startup, ConsumerError::Io(e.to_string()));
            }
        };

        if let Some(startup) = startup {
            if startup.send(Ok(())).is_err() {
                return ControlledAttempt::Shutdown;
            }
        }

        match self.consume_until_shutdown(stream, shutdown).await {
            Ok(ControlledAttempt::Shutdown) => ControlledAttempt::Shutdown,
            Ok(ControlledAttempt::StreamEnd) => ControlledAttempt::StreamEnd,
            Ok(other) => other,
            Err(error) => ControlledAttempt::RuntimeFailed(error),
        }
    }

    async fn consume_until_shutdown<S, T>(
        self,
        mut stream: ReplicationStream<S, T>,
        shutdown: &flume::Receiver<()>,
    ) -> Result<ControlledAttempt, ConsumerError>
    where
        S: compio::io::AsyncRead + compio::io::AsyncWrite + Unpin,
        T: compio::io::AsyncRead + compio::io::AsyncWrite + Unpin,
    {
        let mut relations: HashMap<u32, RelationEntry> = HashMap::new();
        loop {
            let message = {
                let next = stream.next().fuse();
                let stop = shutdown.recv_async().fuse();
                futures::pin_mut!(next, stop);
                futures::select! {
                    message = next => Some(message),
                    _ = stop => None,
                }
            };
            let Some(message) = message else {
                return Ok(ControlledAttempt::Shutdown);
            };
            let Some(message) = message.map_err(|e| ConsumerError::Io(e.to_string()))? else {
                return Ok(ControlledAttempt::StreamEnd);
            };

            match message {
                ReplicationMessage::PrimaryKeepalive {
                    wal_end,
                    reply_requested,
                    ..
                } => {
                    stream.advance_lsn(wal_end);
                    if reply_requested {
                        stream
                            .send_standby_status_update(false)
                            .await
                            .map_err(|e| ConsumerError::Io(e.to_string()))?;
                    }
                }
                ReplicationMessage::XLogData { wal_end, body, .. } => {
                    let decoded = pgoutput::decode(&body)
                        .map_err(|e| ConsumerError::Decode(e.to_string()))?;
                    self.dispatch(&mut relations, &decoded);
                    if let PgOutputMessage::Commit { end_lsn, .. } = &decoded {
                        stream.advance_lsn(*end_lsn);
                        stream
                            .send_standby_status_update(false)
                            .await
                            .map_err(|e| ConsumerError::Io(e.to_string()))?;
                    } else {
                        // Mid-transaction. This reports `wal_end` as flushed
                        // for records that have only been DISPATCHED into the
                        // broker, and `advance_lsn` feeds `flush_lsn`, which
                        // compio-postgres documents as "a durability promise
                        // that lets the server recycle WAL". That reads like a
                        // premature promise; it is not, for two reasons that
                        // are worth writing down because neither is local.
                        //
                        // Replay is decided by COMMIT lsn, not by this one. The
                        // server re-sends any transaction whose commit record
                        // sorts after `confirmed_flush_lsn`. A commit record is
                        // written after every data record of its transaction,
                        // so a mid-transaction `wal_end` is strictly below the
                        // commit lsn of the very transaction it belongs to --
                        // this position can never suppress replay of the
                        // transaction in progress. That is an ordering property
                        // of WAL, not an accident of pgoutput. It would stop
                        // holding under protocol-v2 `streaming=on`, where
                        // in-progress transactions interleave; we do not enable
                        // it, and turning it on means revisiting this branch.
                        //
                        // WAL retention is governed by `restart_lsn`, which the
                        // client cannot move. MEASURED 2026-08-23 against
                        // PostgreSQL in `zs-cpg-review-5455`: with a write
                        // transaction verifiably open (`backend_xid IS NOT
                        // NULL`, checked before AND after), forcing the slot's
                        // `confirmed_flush_lsn` forward past that transaction's
                        // uncommitted records left `restart_lsn` pinned at
                        // 1/1D4D2A30 -- at or before the position preceding the
                        // transaction -- with `catalog_xmin` held. The server
                        // pins retention itself, independently of what the
                        // client reports.
                        //
                        // So the position is safe to report here, and `Commit`
                        // above is what actually promises durability.
                        stream.advance_lsn(wal_end);
                    }
                }
            }
        }
    }

    /// Apply one pgoutput message: maintain the relation cache and
    /// emit broker events for Insert/Update/Delete on this app's
    /// schema.
    fn dispatch(
        &self,
        relations: &mut HashMap<u32, RelationEntry>,
        msg: &PgOutputMessage,
    ) {
        match msg {
            PgOutputMessage::Relation {
                rel_id,
                namespace,
                name,
                columns,
                ..
            } => {
                relations.insert(
                    *rel_id,
                    RelationEntry {
                        namespace: namespace.clone(),
                        table: name.clone(),
                        columns: columns.clone(),
                    },
                );
            }
            PgOutputMessage::Insert { rel_id, new_tuple } => {
                self.emit_for_tuple(relations, *rel_id, ChangeOp::Insert, new_tuple, None);
            }
            PgOutputMessage::Update {
                rel_id,
                new_tuple,
                old_tuple,
                ..
            } => {
                // Only a FULL old tuple carries the row's prior VALUES. A
                // `Key` tuple names the row and pads every other column with
                // NULL, so handing it to the broker as a before-image would
                // have it evaluate subscription predicates against NULLs the
                // server never claimed the row held.
                self.emit_for_tuple(
                    relations,
                    *rel_id,
                    ChangeOp::Update,
                    new_tuple,
                    old_tuple.as_ref().and_then(OldTuple::full),
                );
            }
            PgOutputMessage::Delete { rel_id, old_tuple } => {
                // A DELETE has no after-image, so the old tuple IS the event's
                // tuple. Under the default replica identity that is a `Key`,
                // whose non-key columns are placeholders - which is why a
                // subscription filtered on a non-key column can miss a delete.
                // Fixing that needs REPLICA IDENTITY FULL on published tables,
                // not a change here; this at least no longer pretends the
                // placeholders are values it verified.
                self.emit_for_tuple(
                    relations,
                    *rel_id,
                    ChangeOp::Delete,
                    old_tuple.tuple(),
                    None,
                );
            }
            // Begin/Commit/Origin/Type/Truncate/Message: not surfaced
            // to subscribers. Truncate could fan out to all
            // subscribers as Resync — left as future work alongside per-
            // subscription LSN tracking.
            _ => {}
        }
    }

    /// Build a [`ChangeEvent`] from a tuple and publish it through
    /// the broker.
    ///
    /// Only events whose relation's namespace equals the consumer's
    /// `app_id` are surfaced — the slot may carry events from any
    /// publication that happens to be in scope, but we explicitly
    /// scope to this app for tenant isolation.
    ///
    /// The relation's column declarations are zipped with the
    /// tuple's text values to populate `new_tuple` (and `old_tuple`
    /// for UPDATE) — these maps drive the broker's predicate
    /// evaluation on the subscriber-narrowing path.
    fn emit_for_tuple(
        &self,
        relations: &HashMap<u32, RelationEntry>,
        rel_id: u32,
        op: ChangeOp,
        tuple: &TupleData,
        old_tuple: Option<&TupleData>,
    ) {
        let Some(rel) = relations.get(&rel_id) else {
            // Relation cache miss. pgoutput contracts that Relation
            // is sent before the first DML referencing it, so this
            // is a protocol violation — but it's also possible if
            // the consumer started mid-transaction. Drop the event;
            // the next Commit's snapshot is consistent.
            return;
        };
        if rel.namespace != self.app_id {
            return;
        }

        // Performance-critical: short-circuit before the
        // (column-name-clone) `changed_columns` Vec and the two
        // `tuple_to_map` HashMaps. On a table with no reactive
        // subscribers — the majority of tables in typical apps — the
        // event would otherwise be built only for `broker::publish` to
        // immediately discard it. The check is a single
        // `HashMap::get` under the process-wide broker's short lock.
        // A subscriber registered after the check only needs future
        // events; its initial snapshot covers earlier state.
        if !has_subscribers(&self.app_id, &rel.table) {
            return;
        }

        let pk = rel.primary_key_index().and_then(|idx| {
            tuple.columns.get(idx).and_then(|col| match col {
                TupleColumn::Text(s) => Some(s.clone()),
                _ => None,
            })
        });

        let changed_columns: Vec<String> = rel
            .columns
            .iter()
            .map(|c| c.name.clone())
            .collect();

        let new_tuple_map = tuple_to_map(&rel.columns, tuple);
        let old_tuple_map = old_tuple.map(|t| tuple_to_map(&rel.columns, t));

        publish(&ChangeEvent {
            app_id: self.app_id.clone(),
            collection: rel.table.clone(),
            op,
            pk,
            changed_columns,
            new_tuple: new_tuple_map,
            old_tuple: old_tuple_map,
        });
    }
}

/// Zip a pgoutput tuple with its relation's column declarations into a
/// `column_name → text_value` map.
///
/// Only `TupleColumn::Text` values are surfaced; `Null` produces the
/// sentinel `"NULL"` so the broker's predicate evaluation can treat
/// it as a non-match against any scalar (the proposal explicitly
/// punts on NULL-aware filters until a future phase). `Toasted` and
/// `Binary` columns are skipped — the column simply doesn't appear in
/// the map and predicate evaluation treats it as non-matching, which
/// is the conservative choice (the subscriber sees fewer events, not
/// more; the next event WITH the column populated re-triggers
/// matching).
fn tuple_to_map(
    columns: &[pgoutput::RelationColumn],
    tuple: &TupleData,
) -> HashMap<String, String> {
    let mut out = HashMap::with_capacity(columns.len());
    for (col, val) in columns.iter().zip(tuple.columns.iter()) {
        match val {
            TupleColumn::Text(s) => {
                out.insert(col.name.clone(), s.clone());
            }
            TupleColumn::Null => {
                out.insert(col.name.clone(), "NULL".to_string());
            }
            // Toasted (unchanged TOAST values, not transmitted) and
            // Binary (publication-options-dependent) skip the map.
            TupleColumn::Toasted | TupleColumn::Binary(_) => {}
        }
    }
    out
}

/// Add `replication=database` to a libpq-style URL if not already set.
///
/// Conservative: if the URL already has any `replication=` value we
/// leave it (allows callers to force `=true` for a physical-replication
/// connection from the same code path).
fn ensure_replication_param(url: &str) -> String {
    if url.contains("replication=") {
        return url.to_string();
    }
    // Crude but adequate: append as a query param if the URL is a URI,
    // else as a key=value pair if it's keyword/value.
    if url.contains("://") {
        if url.contains('?') {
            format!("{url}&replication=database")
        } else {
            format!("{url}?replication=database")
        }
    } else {
        format!("{url} replication=database")
    }
}

// ---------------------------------------------------------------------------
// Supervisor (auto-reconnect with exponential backoff)
// ---------------------------------------------------------------------------

/// Initial backoff between reconnect attempts.
pub(crate) const INITIAL_BACKOFF: Duration = Duration::from_millis(1_000);
/// Cap on the backoff schedule.
pub(crate) const MAX_BACKOFF: Duration = Duration::from_millis(30_000);
/// Minimum elapsed streaming time that "resets" the backoff schedule.
///
/// If the consumer ran for at least this long before failing, the next
/// retry uses [`INITIAL_BACKOFF`] again — i.e. only thrashing reconnects
/// keep escalating. Picked at 30 s so transient blips don't pin backoff
/// to its cap.
pub(crate) const STABILITY_THRESHOLD: Duration = Duration::from_secs(30);

/// Classifies an error as "do not retry" (slot/publication invalidated,
/// config error). The supervisor exits cleanly when this returns true.
///
/// Conservative on purpose: we retry by default. Only known-fatal
/// kinds short-circuit. Picking the wrong direction on this gate is
/// asymmetric:
///   - over-eager retry on truly fatal errors: tight busy loop,
///     hammers Postgres and the log. The backoff cap mitigates but
///     does not eliminate.
///   - under-eager retry on transient errors: a brief outage breaks
///     subscriptions until the operator restarts the worker.
///
/// The kinds we treat as fatal are the ones where retrying is
/// guaranteed not to help:
///   - `Decode` errors carrying SQLSTATE 58P01 ("undefined_object" =
///     slot dropped). That's a watchdog signal — the slot was
///     externally invalidated and the supervisor should let the
///     reconciler reprovision before a fresh run.
///
/// (Pre-flight `invalid_app_id` validation failures do not flow
/// through this function — they are caught at construction time in
/// [`WalConsumer::new`] as a typed [`DbError`].)
fn is_fatal(err: &ConsumerError) -> bool {
    match err {
        ConsumerError::Io(s) | ConsumerError::Connect(s) | ConsumerError::Decode(s) => {
            // SQLSTATE 58P01 (undefined_object) — slot/pub dropped.
            // Postgres surfaces this from START_REPLICATION when the
            // slot was deleted (manual operator action, or the
            // replication watchdog's drop_abandoned_slots reaper).
            // We bail so the next supervisor iteration doesn't keep
            // hammering a slot that the watchdog hasn't yet
            // reprovisioned.
            let lc = s.to_ascii_lowercase();
            lc.contains("58p01")
                || lc.contains("does not exist")
                    && (lc.contains("replication slot") || lc.contains("publication"))
                || lc.contains("invalid slot name")
        }
    }
}

/// Run a supervised consumer with an explicit startup and shutdown
/// contract.
///
/// `startup` receives success only after Postgres accepts
/// `START_REPLICATION`. A connection, slot, or publication failure is
/// returned immediately instead of leaving a subscription looking
/// healthy with only its initial snapshot. After startup, transient
/// failures reconnect with the standard backoff schedule. `shutdown`
/// interrupts both streaming and backoff waits.
pub(crate) async fn run_supervised_controlled(
    consumer: WalConsumer,
    startup: flume::Sender<Result<(), DbError>>,
    shutdown: flume::Receiver<()>,
) -> Result<(), DbError> {
    let app_id = consumer.app_id().to_string();
    // Hold suppression for the supervisor's full lifetime, including
    // reconnect backoff. The slot retains changes while disconnected;
    // allowing local emit during the gap would deliver once locally
    // and again when WAL replay catches up.
    let _suppression = SuppressGuard::activate(&app_id);
    let mut first_attempt = true;
    let mut backoff = INITIAL_BACKOFF;

    loop {
        let attempt_started = std::time::Instant::now();
        let attempt = consumer.clone();
        let outcome = attempt
            .run_controlled_once(
                &shutdown,
                if first_attempt { Some(&startup) } else { None },
            )
            .await;
        first_attempt = false;

        match outcome {
            ControlledAttempt::Shutdown => return Ok(()),
            ControlledAttempt::StreamEnd => {
                return Err(DbError::Transient {
                    message: format!(
                        "wal consumer for app {app_id} stopped before shutdown was requested"
                    ),
                });
            }
            ControlledAttempt::StartupFailed(error) => return Err(error),
            ControlledAttempt::RuntimeFailed(error) if is_fatal(&error) => {
                tracing::error!(
                    app_id = %app_id,
                    error = %error,
                    "wal consumer: fatal error, controlled supervisor exits"
                );
                return Err(consumer_error_to_db(&error));
            }
            ControlledAttempt::RuntimeFailed(error) => {
                let ran_for = attempt_started.elapsed();
                let stable = ran_for >= STABILITY_THRESHOLD;
                tracing::warn!(
                    app_id = %app_id,
                    ran_for_ms = ran_for.as_millis() as u64,
                    backoff_ms = backoff.as_millis() as u64,
                    error = %error,
                    "wal consumer: exited, reconnecting"
                );

                let sleep = compio::time::sleep(backoff).fuse();
                let stop = shutdown.recv_async().fuse();
                futures::pin_mut!(sleep, stop);
                let stopped = futures::select! {
                    _ = sleep => false,
                    _ = stop => true,
                };
                if stopped {
                    return Ok(());
                }
                backoff = if stable {
                    INITIAL_BACKOFF
                } else {
                    (backoff * 2).min(MAX_BACKOFF)
                };
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::broker::{Broker, SubscriptionMessage};
    use compio_postgres::replication::pgoutput;

    // -------- emit_local --------

    #[test]
    fn emit_local_reaches_process_broker() {
        // Clean the process-wide broker before observing.
        crate::broker::drop_app(None);
        let sub = crate::broker::subscribe("xapp", "messages");
        emit_local(
            "xapp",
            "messages",
            ChangeOp::Insert,
            Some("99".to_string()),
            vec!["title".into()],
            HashMap::new(),
        );
        match sub.pop() {
            Some(SubscriptionMessage::Change(ev)) => {
                assert_eq!(ev.app_id, "xapp");
                assert_eq!(ev.collection, "messages");
                assert_eq!(ev.op, ChangeOp::Insert);
                assert_eq!(ev.pk.as_deref(), Some("99"));
                assert_eq!(ev.changed_columns, vec!["title".to_string()]);
            }
            other => panic!("expected Change variant, got {other:?}"),
        }
        crate::broker::drop_app(None);
    }

    #[test]
    fn emit_local_with_no_subscriber_is_noop() {
        let _ = Broker::new();
        emit_local(
            "nobody",
            "ghosts",
            ChangeOp::Delete,
            None,
            vec![],
            HashMap::new(),
        );
    }

    #[test]
    fn emit_local_suppressed_when_consumer_active() {
        crate::broker::drop_app(None);
        let sub = crate::broker::subscribe("xapp", "messages");

        // Per-app suppression: the consumer for "xapp" is active, so
        // local-emit for "xapp" must be a no-op.
        suppress_app("xapp");
        emit_local(
            "xapp",
            "messages",
            ChangeOp::Insert,
            Some("1".to_string()),
            vec!["title".into()],
            HashMap::new(),
        );
        // Suppressed — no event in the queue.
        assert!(sub.pop().is_none());

        unsuppress_app("xapp");
        emit_local(
            "xapp",
            "messages",
            ChangeOp::Insert,
            Some("2".to_string()),
            vec!["title".into()],
            HashMap::new(),
        );
        // Unsuppressed — event delivered.
        assert!(matches!(sub.pop(), Some(SubscriptionMessage::Change(_))));
        crate::broker::drop_app(None);
    }

    // -------- ensure_replication_param --------

    #[test]
    fn ensure_replication_param_url_no_query() {
        let out = ensure_replication_param("postgres://u@h/db");
        assert_eq!(out, "postgres://u@h/db?replication=database");
    }

    #[test]
    fn ensure_replication_param_url_with_query() {
        let out = ensure_replication_param("postgres://u@h/db?sslmode=disable");
        assert_eq!(out, "postgres://u@h/db?sslmode=disable&replication=database");
    }

    #[test]
    fn ensure_replication_param_keyword_value() {
        let out = ensure_replication_param("host=h user=u dbname=db");
        assert_eq!(out, "host=h user=u dbname=db replication=database");
    }

    #[test]
    fn ensure_replication_param_passthrough() {
        let out = ensure_replication_param("host=h replication=true");
        assert_eq!(out, "host=h replication=true");
    }

    // -------- WalConsumer::new --------

    #[test]
    fn wal_consumer_new_computes_slot_and_publication() {
        let c = WalConsumer::new("alpha", "worker-a", "postgres://localhost/db").unwrap();
        assert_eq!(c.app_id, "alpha");
        assert_eq!(
            c.slot_name,
            crate::replication::worker_slot_name("alpha", "worker-a").unwrap()
        );
        assert_eq!(
            c.publication_name,
            crate::replication::publication_name("alpha").unwrap()
        );
        assert_eq!(c.start_lsn, "0/0");
    }

    #[test]
    fn wal_consumer_with_start_lsn() {
        let c = WalConsumer::new("alpha", "worker-a", "postgres://localhost/db")
            .unwrap()
            .with_start_lsn("0/16B3750");
        assert_eq!(c.start_lsn, "0/16B3750");
    }

    #[test]
    fn wal_consumer_rejects_invalid_app_id() {
        let err = WalConsumer::new("has\0nul", "worker-a", "postgres://localhost/db").unwrap_err();
        assert!(matches!(
            err,
            DbError::ValidationFailed { code: "invalid_app_id", .. }
        ));
    }

    /// Invalid app ids must surface a typed
    /// `ValidationFailed { code: "invalid_app_id" }` so the SDK can
    /// distinguish "developer passed a bad app_id" from "operator
    /// hasn't configured the database". Prior code collapsed both
    /// into a single opaque `Configuration { code: "not_provisioned" }`.
    #[test]
    fn wal_consumer_new_invalid_app_id_returns_typed_error() {
        // NUL is the sole disallowed non-empty app-id character.
        let err = WalConsumer::new("bad\0id", "worker-a", "postgres://localhost/db").unwrap_err();
        match err {
            DbError::ValidationFailed { code, message, hint } => {
                assert_eq!(code, "invalid_app_id");
                assert!(
                    message.contains("must not contain NUL"),
                    "message must explain the rejection: {message}"
                );
                assert!(hint.is_none(), "app-id validation errors carry no hint");
            }
            other => panic!("expected ValidationFailed/invalid_app_id, got {other:?}"),
        }
    }

    /// The genuine "operator forgot to set DB_URL" path must still
    /// surface as `Configuration { code: "not_provisioned" }` — that
    /// `.code` is what the SDK branches on to surface the right
    /// remediation. This pins the distinction between the two error
    /// classes.
    #[test]
    fn wal_consumer_new_missing_db_url_returns_configuration() {
        let err = WalConsumer::new("alpha", "worker-a", "").unwrap_err();
        match err {
            DbError::Configuration { code, message, hint } => {
                assert_eq!(code, "not_provisioned");
                assert!(hint.is_some(), "configuration error should carry a remediation hint");
                assert!(
                    message.contains("db_url"),
                    "message must mention db_url: {message}"
                );
            }
            other => panic!("expected Configuration/not_provisioned, got {other:?}"),
        }
    }

    // -------- dispatch (pure logic, no I/O) --------

    fn make_consumer(app_id: &str) -> WalConsumer {
        WalConsumer::new(app_id, "worker-a", "postgres://localhost/db").unwrap()
    }

    fn make_relation_msg(
        rel_id: u32,
        ns: &str,
        name: &str,
        cols: &[(u8, &str)],
    ) -> PgOutputMessage {
        PgOutputMessage::Relation {
            rel_id,
            namespace: ns.into(),
            name: name.into(),
            replica_identity: b'd',
            columns: cols
                .iter()
                .map(|(flags, name)| pgoutput::RelationColumn {
                    flags: *flags,
                    name: (*name).into(),
                    type_oid: 20, // BIGINT
                    type_modifier: -1,
                })
                .collect(),
        }
    }

    fn make_insert_msg(rel_id: u32, values: &[Option<&str>]) -> PgOutputMessage {
        PgOutputMessage::Insert {
            rel_id,
            new_tuple: TupleData {
                columns: values
                    .iter()
                    .map(|v| match v {
                        None => TupleColumn::Null,
                        Some(s) => TupleColumn::Text((*s).into()),
                    })
                    .collect(),
            },
        }
    }

    #[test]
    fn dispatch_caches_relation_and_emits_insert() {
        crate::broker::drop_app(None);
        let sub = crate::broker::subscribe("myapp", "messages");
        let c = make_consumer("myapp");
        let mut rels = HashMap::new();

        // Pretend a Relation arrived first.
        c.dispatch(
            &mut rels,
            &make_relation_msg(16384, "myapp", "messages", &[(1, "id"), (0, "title")]),
        );
        assert!(rels.contains_key(&16384));

        // Then an Insert.
        c.dispatch(
            &mut rels,
            &make_insert_msg(16384, &[Some("42"), Some("hello")]),
        );

        match sub.pop() {
            Some(SubscriptionMessage::Change(ev)) => {
                assert_eq!(ev.app_id, "myapp");
                assert_eq!(ev.collection, "messages");
                assert_eq!(ev.op, ChangeOp::Insert);
                assert_eq!(ev.pk.as_deref(), Some("42"));
                assert_eq!(ev.changed_columns, vec!["id".to_string(), "title".into()]);
            }
            other => panic!("expected Change, got {other:?}"),
        }
        crate::broker::drop_app(None);
    }

    #[test]
    fn dispatch_emits_typed_id_pk_for_text_primary_key() {
        crate::broker::drop_app(None);
        let sub = crate::broker::subscribe("myapp", "messages");
        let c = make_consumer("myapp");
        let mut rels = HashMap::new();

        c.dispatch(
            &mut rels,
            &make_relation_msg(16384, "myapp", "messages", &[(1, "id"), (0, "title")]),
        );
        c.dispatch(
            &mut rels,
            &make_insert_msg(
                16384,
                &[Some("usr_02HXWALSUBSCRIPTIONPK"), Some("hello")],
            ),
        );

        match sub.pop() {
            Some(SubscriptionMessage::Change(ev)) => {
                assert_eq!(ev.pk.as_deref(), Some("usr_02HXWALSUBSCRIPTIONPK"));
                assert_eq!(
                    ev.new_tuple.get("id").map(String::as_str),
                    Some("usr_02HXWALSUBSCRIPTIONPK")
                );
            }
            other => panic!("expected Change, got {other:?}"),
        }
        crate::broker::drop_app(None);
    }

    #[test]
    fn dispatch_filters_other_app_schemas() {
        crate::broker::drop_app(None);
        let sub = crate::broker::subscribe("myapp", "messages");
        let c = make_consumer("myapp");
        let mut rels = HashMap::new();

        // Relation in a DIFFERENT schema — should not produce events
        // for this consumer.
        c.dispatch(
            &mut rels,
            &make_relation_msg(16385, "otherapp", "messages", &[(1, "id"), (0, "title")]),
        );
        c.dispatch(
            &mut rels,
            &make_insert_msg(16385, &[Some("1"), Some("nope")]),
        );

        assert!(sub.pop().is_none());
        crate::broker::drop_app(None);
    }

    #[test]
    fn dispatch_handles_relation_cache_miss() {
        crate::broker::drop_app(None);
        let sub = crate::broker::subscribe("myapp", "messages");
        let c = make_consumer("myapp");
        let mut rels = HashMap::new();

        // Insert with no prior Relation. Drop silently.
        c.dispatch(
            &mut rels,
            &make_insert_msg(16384, &[Some("1"), Some("hello")]),
        );
        assert!(sub.pop().is_none());
        crate::broker::drop_app(None);
    }

    #[test]
    fn dispatch_emits_update_and_delete() {
        crate::broker::drop_app(None);
        let sub = crate::broker::subscribe("myapp", "messages");
        let c = make_consumer("myapp");
        let mut rels = HashMap::new();

        c.dispatch(
            &mut rels,
            &make_relation_msg(16384, "myapp", "messages", &[(1, "id"), (0, "title")]),
        );
        c.dispatch(
            &mut rels,
            &PgOutputMessage::Update {
                rel_id: 16384,
                old_tuple: None,
                new_tuple: TupleData {
                    columns: vec![
                        TupleColumn::Text("7".into()),
                        TupleColumn::Text("updated".into()),
                    ],
                },
            },
        );
        c.dispatch(
            &mut rels,
            &PgOutputMessage::Delete {
                rel_id: 16384,
                old_tuple: OldTuple::Key(TupleData {
                    columns: vec![TupleColumn::Text("7".into()), TupleColumn::Null],
                }),
            },
        );

        let m1 = sub.pop().expect("first event");
        let m2 = sub.pop().expect("second event");
        match (m1, m2) {
            (
                SubscriptionMessage::Change(u),
                SubscriptionMessage::Change(d),
            ) => {
                assert_eq!(u.op, ChangeOp::Update);
                assert_eq!(u.pk.as_deref(), Some("7"));
                assert_eq!(d.op, ChangeOp::Delete);
                assert_eq!(d.pk.as_deref(), Some("7"));
            }
            other => panic!("expected two Change events, got {other:?}"),
        }
        crate::broker::drop_app(None);
    }

    #[test]
    fn dispatch_ignores_begin_commit() {
        crate::broker::drop_app(None);
        let sub = crate::broker::subscribe("myapp", "messages");
        let c = make_consumer("myapp");
        let mut rels = HashMap::new();

        c.dispatch(
            &mut rels,
            &PgOutputMessage::Begin {
                final_lsn: 0x100,
                commit_timestamp: 0,
                xid: 42,
            },
        );
        c.dispatch(
            &mut rels,
            &PgOutputMessage::Commit {
                flags: 0,
                commit_lsn: 0x100,
                end_lsn: 0x200,
                commit_timestamp: 0,
            },
        );
        assert!(sub.pop().is_none());
        crate::broker::drop_app(None);
    }

    // -------- Per-app emit suppression --------

    /// A consumer suppresses local-emit ONLY for the app it's bound to.
    /// A different app on the same thread is unaffected.
    #[test]
    fn p8a2_per_app_emit_suppression_app_a_only() {
        crate::broker::drop_app(None);
        unsuppress_app("app_a");
        unsuppress_app("app_b");

        let sub_a = crate::broker::subscribe("app_a", "messages");
        let sub_b = crate::broker::subscribe("app_b", "messages");

        // Activate suppression for app_a only.
        let _guard = SuppressGuard::activate("app_a");
        assert!(is_app_suppressed("app_a"));
        assert!(!is_app_suppressed("app_b"));

        // Emit on app_a — must be a no-op.
        emit_local(
            "app_a",
            "messages",
            ChangeOp::Insert,
            Some("1".to_string()),
            vec!["title".into()],
            HashMap::new(),
        );
        assert!(sub_a.pop().is_none(), "app_a must be suppressed");

        // Emit on app_b — MUST be delivered.
        emit_local(
            "app_b",
            "messages",
            ChangeOp::Insert,
            Some("2".to_string()),
            vec!["title".into()],
            HashMap::new(),
        );
        assert!(
            matches!(sub_b.pop(), Some(SubscriptionMessage::Change(_))),
            "app_b must NOT be suppressed by app_a's consumer"
        );

        drop(_guard);
        assert!(!is_app_suppressed("app_a"));
        crate::broker::drop_app(None);
    }

    /// The Drop guard restores suppression state on panic-unwind. We
    /// exercise this by panicking inside a closure that holds a guard
    /// and asserting the suppression entry is cleared afterwards.
    #[test]
    fn p8a2_per_app_emit_suppression_drop_guard_restores_on_panic() {
        unsuppress_app("panic_app");
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = SuppressGuard::activate("panic_app");
            assert!(is_app_suppressed("panic_app"));
            panic!("forced");
        }));
        assert!(outcome.is_err(), "expected the inner panic to surface");
        assert!(
            !is_app_suppressed("panic_app"),
            "Drop guard must clear suppression on panic-unwind"
        );
    }

    /// Two consumers in flight for different apps; dropping one
    /// guard leaves the other's suppression intact.
    #[test]
    fn p8a2_per_app_independent_guards() {
        unsuppress_app("ga");
        unsuppress_app("gb");
        let g_a = SuppressGuard::activate("ga");
        let g_b = SuppressGuard::activate("gb");
        assert!(is_app_suppressed("ga"));
        assert!(is_app_suppressed("gb"));
        drop(g_a);
        assert!(!is_app_suppressed("ga"));
        assert!(is_app_suppressed("gb"));
        drop(g_b);
        assert!(!is_app_suppressed("gb"));
    }

    #[test]
    fn suppression_is_visible_across_threads_and_reference_counted() {
        const APP: &str = "suppression_cross_thread_refcount";
        unsuppress_app(APP);
        let first = SuppressGuard::activate(APP);
        let second = SuppressGuard::activate(APP);
        std::thread::spawn(|| assert!(is_app_suppressed(APP)))
            .join()
            .unwrap();
        drop(first);
        assert!(is_app_suppressed(APP));
        drop(second);
        assert!(!is_app_suppressed(APP));
    }

    // -------- is_fatal classification --------
    //
    // (Pre-flight `invalid_app_id` errors no longer flow through
    // `ConsumerError::is_fatal` — they're caught at construction time
    // as typed `DbError::ValidationFailed`. See
    // `wal_consumer_new_invalid_app_id_returns_typed_error`.)

    #[test]
    fn is_fatal_io_error_default_is_retryable() {
        // A bare connection-refused IO error must be retryable —
        // restarting Postgres is the canonical case.
        assert!(!is_fatal(&ConsumerError::Io(
            "broken pipe".into()
        )));
        assert!(!is_fatal(&ConsumerError::Connect(
            "connection refused".into()
        )));
    }

    #[test]
    fn is_fatal_slot_invalidated_is_fatal() {
        // 58P01 (undefined_object) and the specific
        // "replication slot ... does not exist" message are both fatal.
        assert!(is_fatal(&ConsumerError::Io(
            "ERROR: SQLSTATE 58P01: foo".into()
        )));
        assert!(is_fatal(&ConsumerError::Io(
            "replication slot \"__zs_slot_x\" does not exist".into()
        )));
        assert!(is_fatal(&ConsumerError::Io(
            "publication \"__zs_pub_x\" does not exist".into()
        )));
        assert!(is_fatal(&ConsumerError::Connect(
            "invalid slot name: too long".into()
        )));
    }

    #[test]
    fn is_fatal_decode_protocol_violation_is_retryable() {
        // Plain decode error — could be a transient framing glitch.
        assert!(!is_fatal(&ConsumerError::Decode("unknown tag 0xff".into())));
    }

    // -------- Supervisor behaviour (no real wire) --------
    //
    // Unit tests target constants and backoff math. The integration
    // test `p8a2_supervised_consumer_reconnects_after_kill` exercises
    // the production controlled owner against Postgres.

    #[test]
    fn supervisor_backoff_constants_sane() {
        assert_eq!(INITIAL_BACKOFF, Duration::from_secs(1));
        assert_eq!(MAX_BACKOFF, Duration::from_secs(30));
        assert!(STABILITY_THRESHOLD >= Duration::from_secs(10));
    }

    #[test]
    fn supervisor_backoff_doubles_until_cap() {
        let mut b = INITIAL_BACKOFF;
        let schedule: Vec<Duration> = (0..8)
            .map(|_| {
                let cur = b;
                b = (b * 2).min(MAX_BACKOFF);
                cur
            })
            .collect();
        assert_eq!(
            schedule,
            vec![
                Duration::from_secs(1),
                Duration::from_secs(2),
                Duration::from_secs(4),
                Duration::from_secs(8),
                Duration::from_secs(16),
                Duration::from_secs(30),
                Duration::from_secs(30),
                Duration::from_secs(30),
            ]
        );
    }
}
