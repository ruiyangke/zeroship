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
//! 2. [`emit_local`] gains an [`EmitMode`] selector. When the
//!    consumer is active on this thread the mode is
//!    [`EmitMode::Suppressed`] and local-emit becomes a no-op; the
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
//!   tested, but the V8 callback that spawns it ([`spawn`]) is opt-in:
//!   apps call `db.replicationConsumerStart()` to enable cross-worker
//!   propagation. Spawning automatically on isolate boot is one
//!   `r.add("replicationConsumerStart", …)` + a callback away in
//!   `callbacks.rs`; left out so the first ship of this code doesn't
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

use std::cell::Cell;
use std::collections::HashMap;

use compio_postgres::replication::{
    self as repl, IdentifySystem, ReplicationMessage, ReplicationStream, StartReplicationOptions,
    pgoutput::{self, PgOutputMessage, TupleColumn, TupleData},
};

use crate::broker::{publish, ChangeEvent, ChangeOp};

// ---------------------------------------------------------------------------
// Emit-mode toggle
// ---------------------------------------------------------------------------

thread_local! {
    /// When `true` on this thread, [`emit_local`] is a no-op — the
    /// streaming WAL consumer is the sole source of broker events.
    ///
    /// Set by [`WalConsumer::run`] for the lifetime of the consumer
    /// loop; cleared on exit (success or error).
    ///
    /// We track this per-thread (not per-app) because each worker
    /// runs one compio runtime + at most one consumer per app at a
    /// time. If a worker hosts apps A and B and only A has a
    /// consumer, we still want B's local-emit path to function — the
    /// toggle would need to become per-app. P8a.2 ships the
    /// coarse-grained version; the per-app refinement is one line in
    /// [`emit_local`] (replace the bool with a `HashSet<String>` of
    /// suppressed app_ids) when a real multi-app worker shows up.
    static EMIT_SUPPRESSED: Cell<bool> = const { Cell::new(false) };
}

/// Drive the suppression toggle from tests + internal callers.
#[doc(hidden)]
pub fn set_local_emit_suppressed(v: bool) {
    EMIT_SUPPRESSED.with(|c| c.set(v));
}

/// True when the WAL consumer is active on this thread.
pub fn local_emit_suppressed() -> bool {
    EMIT_SUPPRESSED.with(|c| c.get())
}

/// Emit a local change event into the in-process broker.
///
/// Called from the mutation callbacks (`insert`, `update_one`,
/// `delete_one`, ...) after a successful SQL run.
///
/// When [`local_emit_suppressed`] is `true` this is a no-op — the WAL
/// consumer is publishing the same event on the cross-worker path and
/// emitting locally too would double-deliver.
pub fn emit_local(
    app_id: &str,
    collection: &str,
    op: ChangeOp,
    pk: Option<i64>,
    changed_columns: Vec<String>,
) {
    if local_emit_suppressed() {
        return;
    }
    publish(&ChangeEvent {
        app_id: app_id.to_string(),
        collection: collection.to_string(),
        op,
        pk,
        changed_columns,
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

#[derive(Debug)]
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
    /// happens in [`run`].
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

    /// Resume from a specific LSN on next [`run`]. Pass the value
    /// returned by [`crate::replication::ensure_publication_and_slot`].
    pub fn with_start_lsn(mut self, lsn: impl Into<String>) -> Self {
        self.start_lsn = lsn.into();
        self
    }

    /// Run the consumer loop. Returns on the first wire error (the
    /// caller restarts with backoff).
    ///
    /// While running, this thread's [`local_emit_suppressed`] flag is
    /// `true` — see the module docs for why.
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

        set_local_emit_suppressed(true);
        let result = self.consume(stream).await;
        set_local_emit_suppressed(false);
        result
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
                self.emit_for_tuple(relations, *rel_id, ChangeOp::Insert, new_tuple);
            }
            PgOutputMessage::Update {
                rel_id, new_tuple, ..
            } => {
                self.emit_for_tuple(relations, *rel_id, ChangeOp::Update, new_tuple);
            }
            PgOutputMessage::Delete { rel_id, old_tuple } => {
                self.emit_for_tuple(relations, *rel_id, ChangeOp::Delete, old_tuple);
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
    fn emit_for_tuple(
        &self,
        relations: &HashMap<u32, RelationEntry>,
        rel_id: u32,
        op: ChangeOp,
        tuple: &TupleData,
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

        publish(&ChangeEvent {
            app_id: self.app_id.clone(),
            collection: rel.table.clone(),
            op,
            pk,
            changed_columns,
        });
    }
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
        emit_local("xapp", "messages", ChangeOp::Insert, Some(99), vec!["title".into()]);
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
        emit_local("nobody", "ghosts", ChangeOp::Delete, None, vec![]);
    }

    #[test]
    fn emit_local_suppressed_when_consumer_active() {
        crate::broker::drop_app(None);
        let sub = crate::broker::subscribe("xapp", "messages");

        set_local_emit_suppressed(true);
        emit_local("xapp", "messages", ChangeOp::Insert, Some(1), vec!["title".into()]);
        // Suppressed — no event in the queue.
        assert!(sub.pop().is_none());

        set_local_emit_suppressed(false);
        emit_local("xapp", "messages", ChangeOp::Insert, Some(2), vec!["title".into()]);
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
}
