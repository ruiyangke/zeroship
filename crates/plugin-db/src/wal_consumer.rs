//! Streaming WAL consumer — P8a.2.
//!
//! This module owns the long-running task that bridges Postgres
//! logical-decoding output (`pgoutput` over the streaming-replication
//! protocol) into the in-process [`crate::broker`]. It is the
//! cross-worker leg of reactive queries: a row written on worker A
//! reaches a subscriber on worker B because both workers consume the
//! same WAL slot.
//!
//! ## What changed from P8a
//!
//! P8a shipped only the local-emit fast path: mutation callbacks
//! published directly to the same-isolate broker on success. That
//! works for the single-worker case (the platform routes per-app
//! traffic via CHWBL so it's the common case) but offers nothing when
//! the writer and subscriber happen to land on different workers.
//!
//! P8a.2 adds:
//!
//! 1. [`WalConsumer`] — a compio task that opens a
//!    `replication=database` connection (via
//!    [`compio_postgres::replication::connect_replication`]), issues
//!    `START_REPLICATION SLOT ... LOGICAL ...`, decodes pgoutput
//!    frames, and publishes [`ChangeEvent`]s into the broker.
//! 2. [`emit_local`] gains an `EmitMode` selector. When the
//!    consumer is active on this thread the mode is
//!    `EmitMode::Suppressed` and local-emit becomes a no-op; the
//!    WAL path is the sole source of truth. Otherwise (no consumer
//!    yet started, or the consumer crashed and the watchdog hasn't
//!    re-spawned) local-emit fires as before.
//! 3. A relation cache (`rel_id -> (namespace, table, columns)`)
//!    populated from pgoutput `Relation` messages. Required to map a
//!    `Insert { rel_id, tuple }` back to a `(collection, pk,
//!    changed_columns)` broker event.
//!
//! ## What's NOT in this commit
//!
//! - **Per-subscription LSN tracking** — when the consumer is
//!   suppressed (i.e. fast-path local-emit is firing) AND the
//!   consumer is also active, a single mutation produces TWO events.
//!   The current implementation chooses one OR the other via the
//!   thread-local mode toggle. A future change can add per-event
//!   `wal_lsn` + per-subscription `seen_local_lsn` dedup so that BOTH
//!   paths can fire concurrently with the broker filtering duplicates.
//!   The proposal accepts the cleaner-but-slower (WAL-only) semantics
//!   for P8a.2; the dual-path optimisation is P8a.3.
//! - **Boot-time auto-spawn** — the consumer struct is exposed and
//!   tested, but the V8 callback that spawns it is opt-in:
//!   apps call `db.replicationConsumerStart()` to enable cross-worker
//!   propagation. Spawning automatically on isolate boot is one
//!   `r.add("replicationConsumerStart", …)` + a callback away in
//!   `replication_ops.rs`; left out so the first ship of this code doesn't
//!   change the boot path for apps that have never enabled C1.
//! - **Reconnection / fault-tolerance** — the consumer's `run` loop
//!   returns on first I/O error. A supervising task (`watchdog.rs` in
//!   a future commit) restarts it with exponential backoff. For P8a.2
//!   the caller is responsible for re-spawning. The slot's WAL
//!   retention is the safety net: even if the consumer is offline for
//!   minutes, no events are lost.
//!
//! ## Why the thread-local mode toggle
//!
//! The mutation callbacks already call [`emit_local`] from the same
//! thread as the WAL consumer (compio runs both on one OS thread per
//! worker). A thread-local cell is the cheapest signal — no Mutex, no
//! atomic. The toggle is set when the consumer starts and cleared
//! when it stops; if a callback fires between those events with the
//! WAL consumer briefly down, local-emit takes over with one-off
//! "same-worker delivery only" semantics that the watchdog will fix
//! on the next consumer reconnect.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::time::Duration;

use compio_postgres::replication::{
    self as repl, IdentifySystem, ReplicationMessage, ReplicationStream, StartReplicationOptions,
    pgoutput::{self, PgOutputMessage, TupleColumn, TupleData},
};

use crate::broker::{has_subscribers, publish, ChangeEvent, ChangeOp};

// ---------------------------------------------------------------------------
// Per-app emit-suppression
// ---------------------------------------------------------------------------
//
// When a WAL consumer is active for app A on this thread, local-emit
// for app A must become a no-op — the consumer publishes the same
// event on the cross-worker path and emitting locally too would
// double-deliver. Other apps on the same thread must continue to use
// local-emit; a coarse thread-wide flag would silence their events as
// well.
//
// The set is tracked per-thread (the compio runtime is single-thread
// per worker, every callback that emits runs on the same isolate
// thread). Insert/remove is cheap: small set, no locking.

thread_local! {
    /// App ids whose local-emit path is suppressed because a consumer
    /// is running. Populated by [`WalConsumer::run`] for the lifetime
    /// of the consumer loop via a Drop guard so panics also clean up.
    static SUPPRESSED_APPS: RefCell<HashSet<String>> = RefCell::new(HashSet::new());
}

/// Suppress local-emit for `app_id` on this thread. Mutation callbacks
/// that produce events for this app will become no-ops until
/// [`unsuppress_app`] is called (typically via the Drop guard returned
/// by [`SuppressGuard::activate`]).
pub fn suppress_app(app_id: &str) {
    SUPPRESSED_APPS.with(|s| {
        s.borrow_mut().insert(app_id.to_string());
    });
}

/// Inverse of [`suppress_app`]. Idempotent.
pub fn unsuppress_app(app_id: &str) {
    SUPPRESSED_APPS.with(|s| {
        s.borrow_mut().remove(app_id);
    });
}

/// True when the given app's local-emit path is suppressed on this
/// thread (i.e. a [`WalConsumer`] is running for that app).
pub fn is_app_suppressed(app_id: &str) -> bool {
    SUPPRESSED_APPS.with(|s| s.borrow().contains(app_id))
}

/// True when ANY app on this thread is suppressed. Diagnostic helper —
/// the production code path always checks a specific app.
#[doc(hidden)]
pub(crate) fn any_app_suppressed() -> bool {
    SUPPRESSED_APPS.with(|s| !s.borrow().is_empty())
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

// --- Back-compat shims for the pre-P8a.2-finish API. The single-app
//     case used a thread-wide bool; tests against that surface keep
//     working by mapping it onto the per-app set under a stable
//     "sentinel" key. New callers should use the per-app API above.

#[doc(hidden)]
const LEGACY_SUPPRESSION_KEY: &str = "__legacy_thread_wide__";

/// Legacy compatibility: set/clear the thread-wide suppression flag.
/// Internally maps to a sentinel entry in [`SUPPRESSED_APPS`] so the
/// new per-app check still covers callers that drive this surface.
#[doc(hidden)]
pub(crate) fn set_local_emit_suppressed(v: bool) {
    if v {
        suppress_app(LEGACY_SUPPRESSION_KEY);
    } else {
        unsuppress_app(LEGACY_SUPPRESSION_KEY);
    }
}

/// Legacy compatibility: true when the thread-wide flag was set via
/// [`set_local_emit_suppressed`]. Production code should use
/// [`is_app_suppressed`] with a concrete app id.
#[doc(hidden)]
pub(crate) fn local_emit_suppressed() -> bool {
    is_app_suppressed(LEGACY_SUPPRESSION_KEY)
}

/// Emit a local change event into the in-process broker.
///
/// Called from the mutation callbacks (`insert`, `update_one`,
/// `delete_one`, ...) after a successful SQL run.
///
/// When [`local_emit_suppressed`] is `true` this is a no-op — the WAL
/// consumer is publishing the same event on the cross-worker path and
/// emitting locally too would double-deliver.
///
/// P8b: the `new_tuple` is the row's post-image (or pre-image for
/// DELETE) — used by the broker's read-set narrowing to test each
/// subscriber's predicate. May be empty when the caller doesn't have a
/// tuple snapshot to hand; predicate evaluation treats missing columns
/// as non-matching (the conservative direction).
pub fn emit_local(
    app_id: &str,
    collection: &str,
    op: ChangeOp,
    pk: Option<i64>,
    changed_columns: Vec<String>,
    new_tuple: std::collections::HashMap<String, String>,
) {
    // Per-app suppression: only suppress when THIS app has an active
    // consumer. The legacy thread-wide flag is also honoured (it maps
    // onto a sentinel entry in the suppressed set) so older callers
    // keep their semantics. Tests that rely on the multi-app case must
    // call `suppress_app(app_id)` directly.
    if is_app_suppressed(app_id) || local_emit_suppressed() {
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

/// Errors raised by [`WalConsumer::run`].
#[derive(Debug)]
pub enum ConsumerError {
    /// Establishing the replication connection failed.
    Connect(String),
    /// The publication or slot was missing — the caller should run
    /// [`crate::replication::ensure_publication_and_slot`] first.
    NotProvisioned(String),
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
            ConsumerError::NotProvisioned(s) => write!(f, "wal consumer: not provisioned: {s}"),
            ConsumerError::Io(s) => write!(f, "wal consumer: io: {s}"),
            ConsumerError::Decode(s) => write!(f, "wal consumer: decode: {s}"),
        }
    }
}

impl std::error::Error for ConsumerError {}

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
    /// 2. Surface `changed_columns` to the broker event (P8b will
    ///    use this for read-set filtering).
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
pub struct WalConsumer {
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
    /// Build a consumer descriptor. Spinning up the connection
    /// happens in [`Self::run`].
    pub fn new(app_id: &str, db_url: &str) -> Result<Self, ConsumerError> {
        let slot_name = crate::replication::slot_name(app_id)
            .map_err(ConsumerError::NotProvisioned)?;
        let publication_name = crate::replication::publication_name(app_id)
            .map_err(ConsumerError::NotProvisioned)?;
        Ok(Self {
            app_id: app_id.to_string(),
            db_url: db_url.to_string(),
            slot_name,
            publication_name,
            start_lsn: "0/0".to_string(),
        })
    }

    /// Resume from a specific LSN on next [`Self::run`]. Pass the value
    /// returned by [`crate::replication::ensure_publication_and_slot`].
    pub fn with_start_lsn(mut self, lsn: impl Into<String>) -> Self {
        self.start_lsn = lsn.into();
        self
    }

    /// App id this consumer is bound to. Exposed so the supervisor can
    /// log it without cloning the whole consumer.
    pub fn app_id(&self) -> &str {
        &self.app_id
    }

    /// Run the consumer loop. Returns on the first wire error (the
    /// caller restarts with backoff via [`run_supervised`]).
    ///
    /// While running, the per-app suppression entry for this consumer's
    /// `app_id` is set on the current thread — local-emit becomes a
    /// no-op for that app (and only that app). The Drop guard ensures
    /// the entry is cleared even if the connection task panics.
    pub async fn run(self) -> Result<(), ConsumerError> {
        let url = ensure_replication_param(&self.db_url);
        let config = url
            .parse::<compio_postgres::Config>()
            .map_err(|e| ConsumerError::Connect(e.to_string()))?;

        let mut conn = repl::connect_replication(compio_postgres::NoTls, &config)
            .await
            .map_err(|e| ConsumerError::Connect(e.to_string()))?;

        // IDENTIFY_SYSTEM is a useful sanity-check (we don't actually
        // need its result for logical replication — the slot already
        // tracks resume position — but a successful response confirms
        // the connection entered walsender mode).
        let _identify: IdentifySystem = conn
            .identify_system()
            .await
            .map_err(|e| ConsumerError::Io(e.to_string()))?;

        let opts = StartReplicationOptions {
            slot_name: &self.slot_name,
            start_lsn: &self.start_lsn,
            proto_version: 1,
            publication_names: &self.publication_name,
        };
        let stream = conn
            .start_logical_replication(opts)
            .await
            .map_err(|e| ConsumerError::Io(e.to_string()))?;

        // RAII suppression — cleared on Drop, including panic-unwind.
        let _guard = SuppressGuard::activate(&self.app_id);
        self.consume(stream).await
    }

    /// The actual decode + publish loop. Separated from `run` so
    /// tests can drive it with a synthetic stream-source.
    async fn consume<S, T>(
        self,
        mut stream: ReplicationStream<S, T>,
    ) -> Result<(), ConsumerError>
    where
        S: compio::io::AsyncRead + compio::io::AsyncWrite + Unpin,
        T: compio::io::AsyncRead + compio::io::AsyncWrite + Unpin,
    {
        let mut relations: HashMap<u32, RelationEntry> = HashMap::new();

        loop {
            let msg = stream
                .next()
                .await
                .map_err(|e| ConsumerError::Io(e.to_string()))?;
            let Some(msg) = msg else {
                // Server sent CopyDone. The consumer terminates.
                return Ok(());
            };

            match msg {
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

                    // On Commit, advance the slot.
                    if let PgOutputMessage::Commit { end_lsn, .. } = &decoded {
                        stream.advance_lsn(*end_lsn);
                        stream
                            .send_standby_status_update(false)
                            .await
                            .map_err(|e| ConsumerError::Io(e.to_string()))?;
                    } else {
                        // Track wal_end for inter-commit progress so a
                        // keepalive reply still reflects the truth.
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
                self.emit_for_tuple(
                    relations,
                    *rel_id,
                    ChangeOp::Update,
                    new_tuple,
                    old_tuple.as_ref(),
                );
            }
            PgOutputMessage::Delete { rel_id, old_tuple } => {
                self.emit_for_tuple(relations, *rel_id, ChangeOp::Delete, old_tuple, None);
            }
            // Begin/Commit/Origin/Type/Truncate/Message: not surfaced
            // to subscribers in P8a.2. Truncate could fan out to all
            // subscribers as Resync — left for P8a.3 alongside per-
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
    /// P8b: the relation's column declarations are zipped with the
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

        // Perf CRITICAL N-C1: short-circuit before the
        // (column-name-clone) `changed_columns` Vec and the two
        // `tuple_to_map` HashMaps. On a table with no reactive
        // subscribers — the majority of tables in typical apps — the
        // event would otherwise be built only for `broker::publish` to
        // immediately discard it. The check is a single
        // `HashMap::get` on the thread-local broker; safe because the
        // broker and this consumer run on the same compio thread, so a
        // subscriber registered after the check would only see FUTURE
        // events anyway.
        if !has_subscribers(&self.app_id, &rel.table) {
            return;
        }

        let pk = rel.primary_key_index().and_then(|idx| {
            tuple.columns.get(idx).and_then(|col| match col {
                TupleColumn::Text(s) => s.parse::<i64>().ok(),
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
///   - `NotProvisioned`: the slot/publication name is malformed (an
///     `app_id` validation failure). Restarting won't fix that — the
///     deploy needs to drop+recreate with a valid id.
///   - `Decode` errors carrying SQLSTATE 58P01 ("undefined_object" =
///     slot dropped). That's a watchdog signal — the slot was
///     externally invalidated and the supervisor should let the
///     reconciler reprovision before a fresh run.
pub fn is_fatal(err: &ConsumerError) -> bool {
    match err {
        ConsumerError::NotProvisioned(_) => true,
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

/// Run a [`WalConsumer`] forever, reconnecting with exponential backoff
/// on transient failure.
///
/// Schedule: 1s → 2s → 4s → 8s → 16s → 30s (cap). The cap holds for
/// every subsequent attempt until the consumer stays connected for
/// `STABILITY_THRESHOLD` — then the next failure resets the backoff
/// to `INITIAL_BACKOFF`.
///
/// Exits when:
///   - The consumer returns `Ok(())` (graceful CopyDone — server
///     terminated streaming intentionally, e.g. shutdown).
///   - The consumer returns an error that [`is_fatal`] classifies as
///     non-retryable (slot invalidated, malformed app id, etc.).
///
/// `tracing::warn!` is used for transient failures; `tracing::error!`
/// for fatal exits. Both carry the `app_id` field for log correlation.
pub async fn run_supervised(consumer: WalConsumer) {
    let app_id = consumer.app_id().to_string();
    let mut backoff = INITIAL_BACKOFF;

    loop {
        let attempt_started = std::time::Instant::now();
        let attempt = consumer.clone();
        match attempt.run().await {
            Ok(()) => {
                tracing::info!(
                    app_id = %app_id,
                    "wal consumer: graceful shutdown (CopyDone), supervisor exits"
                );
                return;
            }
            Err(e) if is_fatal(&e) => {
                tracing::error!(
                    app_id = %app_id,
                    error = %e,
                    "wal consumer: fatal error, supervisor exits — \
                     slot/publication likely invalidated; rerun \
                     replicationSetup() to reprovision"
                );
                return;
            }
            Err(e) => {
                let ran_for = attempt_started.elapsed();
                let stable = ran_for >= STABILITY_THRESHOLD;
                tracing::warn!(
                    app_id = %app_id,
                    ran_for_ms = ran_for.as_millis() as u64,
                    backoff_ms = backoff.as_millis() as u64,
                    error = %e,
                    "wal consumer: exited, reconnecting"
                );
                compio::time::sleep(backoff).await;
                backoff = if stable {
                    // The previous attempt streamed long enough to
                    // count as "healthy". Reset the schedule so a
                    // single later blip doesn't start at the cap.
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
    fn emit_local_reaches_thread_local_broker() {
        // The thread-local broker is shared across tests on the same
        // thread; clean it before observing.
        crate::broker::drop_app(None);
        let sub = crate::broker::subscribe("xapp", "messages");
        emit_local(
            "xapp",
            "messages",
            ChangeOp::Insert,
            Some(99),
            vec!["title".into()],
            HashMap::new(),
        );
        match sub.pop() {
            Some(SubscriptionMessage::Change(ev)) => {
                assert_eq!(ev.app_id, "xapp");
                assert_eq!(ev.collection, "messages");
                assert_eq!(ev.op, ChangeOp::Insert);
                assert_eq!(ev.pk, Some(99));
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
            Some(1),
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
            Some(2),
            vec!["title".into()],
            HashMap::new(),
        );
        // Unsuppressed — event delivered.
        assert!(matches!(sub.pop(), Some(SubscriptionMessage::Change(_))));
        crate::broker::drop_app(None);
    }

    #[test]
    fn emit_local_legacy_thread_wide_flag_still_works() {
        // Back-compat shim: callers that drive the old
        // `set_local_emit_suppressed(true/false)` surface still suppress
        // every emit on the thread, regardless of app id.
        crate::broker::drop_app(None);
        let sub = crate::broker::subscribe("legacy_app", "m");
        set_local_emit_suppressed(true);
        emit_local("legacy_app", "m", ChangeOp::Insert, Some(1), vec![], HashMap::new());
        assert!(sub.pop().is_none(), "legacy thread-wide flag must suppress");
        set_local_emit_suppressed(false);
        // Drain the sentinel and confirm following emits go through.
        emit_local("legacy_app", "m", ChangeOp::Insert, Some(2), vec![], HashMap::new());
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
        let c = WalConsumer::new("alpha", "postgres://localhost/db").unwrap();
        assert_eq!(c.app_id, "alpha");
        assert_eq!(c.slot_name, "__zs_slot_alpha");
        assert_eq!(c.publication_name, "__zs_pub_alpha");
        assert_eq!(c.start_lsn, "0/0");
    }

    #[test]
    fn wal_consumer_with_start_lsn() {
        let c = WalConsumer::new("alpha", "postgres://localhost/db")
            .unwrap()
            .with_start_lsn("0/16B3750");
        assert_eq!(c.start_lsn, "0/16B3750");
    }

    #[test]
    fn wal_consumer_rejects_invalid_app_id() {
        let err = WalConsumer::new("has space", "postgres://localhost/db").unwrap_err();
        assert!(matches!(err, ConsumerError::NotProvisioned(_)));
    }

    // -------- dispatch (pure logic, no I/O) --------

    fn make_consumer(app_id: &str) -> WalConsumer {
        WalConsumer::new(app_id, "postgres://localhost/db").unwrap()
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
                assert_eq!(ev.pk, Some(42));
                assert_eq!(ev.changed_columns, vec!["id".to_string(), "title".into()]);
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
                old_tuple: TupleData {
                    columns: vec![TupleColumn::Text("7".into()), TupleColumn::Null],
                },
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
                assert_eq!(u.pk, Some(7));
                assert_eq!(d.op, ChangeOp::Delete);
                assert_eq!(d.pk, Some(7));
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

    // -------- Per-app emit suppression (P8a.2 finish-up) --------

    /// A consumer suppresses local-emit ONLY for the app it's bound to.
    /// A different app on the same thread is unaffected.
    #[test]
    fn p8a2_per_app_emit_suppression_app_a_only() {
        crate::broker::drop_app(None);
        // Clean any sentinel left by other tests.
        unsuppress_app(LEGACY_SUPPRESSION_KEY);
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
            Some(1),
            vec!["title".into()],
            HashMap::new(),
        );
        assert!(sub_a.pop().is_none(), "app_a must be suppressed");

        // Emit on app_b — MUST be delivered.
        emit_local(
            "app_b",
            "messages",
            ChangeOp::Insert,
            Some(2),
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

    // -------- is_fatal classification --------

    #[test]
    fn is_fatal_not_provisioned_is_fatal() {
        assert!(is_fatal(&ConsumerError::NotProvisioned(
            "app_id rejected".into()
        )));
    }

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
    // We can't easily drive `run_supervised` against a real WalConsumer
    // without Postgres, so these tests target the constants + the
    // backoff math. The integration tests (`p8a2_supervised_consumer_*`)
    // in `tests/integration.rs` exercise the full reconnect path.

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
