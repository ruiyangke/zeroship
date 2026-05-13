//! In-memory subscription broker — foundation for C1 reactive queries.
//!
//! The broker is the routing table between change events (today: local
//! mutations within the same isolate; in P8a.2: pgoutput WAL frames
//! decoded by the streaming consumer) and the subscribers who care
//! about them.
//!
//! ## P8a scope vs. proposal
//!
//! - **Granularity:** coarse-grained collection match only. A
//!   subscription `(collection="messages")` fires on EVERY change to
//!   `messages`. The proposal's predicate-filtered fingerprint
//!   matching (`channelId=42 | *`) is deferred to P8b — see
//!   `ReadSet` / `NormalisedPredicate` in the proposal §C1.
//! - **Backpressure:** a bounded per-subscriber queue (default 1024
//!   events) — overflow yields a `kind: "resync"` event and the queue
//!   is cleared. Same semantics as the proposal's "subscriber falls
//!   behind" path.
//! - **Lifecycle:** a subscription is a [`Subscription`] held by the
//!   V8 callback that opened it. Drop = unsubscribe.
//! - **Threading:** the broker is thread-local. The compio runtime is
//!   single-threaded per worker, and every callback that writes events
//!   (`insert`, `updateOne`, `deleteOne`, ...) runs in the same isolate
//!   thread. A future cross-thread broker would replace `RefCell` with
//!   a `Mutex` and add a per-worker dispatcher; we don't do that yet
//!   because there's nothing to dispatch across threads.
//!
//! ## How an event reaches a subscriber
//!
//! ```text
//!   JS calls db.insert("messages", {...})
//!         └→ Rust mutation callback
//!               └→ exec_mutation runs the INSERT
//!                     └→ on success, BROKER.publish(event)
//!                           └→ event pushed onto every matching
//!                              Subscription's bounded queue
//!                                 └→ next subscribe-iterator poll
//!                                    drains the queue → resolves the
//!                                    JS Promise for the AsyncIterable
//! ```
//!
//! ## Why two layers (`Broker` + `Subscription`)
//!
//! The `Broker` is the routing table — keyed by (app_id, collection).
//! A `Subscription` is one consumer's view. Decoupling them lets the
//! same broker serve multiple subscribers without each subscriber
//! holding a lock on the routing table.
//!
//! ## P8b/P8c integration points (intentionally hooked in here)
//!
//! - `Subscription::read_set` (Option) — left as `None` in P8a. P8b's
//!   read-set capture writes a `ReadSet` here; `Broker::publish`
//!   checks each subscription's `ReadSet` against the event before
//!   pushing.
//! - `ChangeEvent::changed_columns` — populated by the WAL consumer
//!   when it lands. The local-emit path (P8a) populates it from the
//!   mutation handler's `SET` clause for INSERT/UPDATE; DELETE
//!   reports an empty set.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::task::Waker;

use serde_json::Value;

use crate::read_set::ReadSetEntry;

// ---------------------------------------------------------------------------
// Event shape
// ---------------------------------------------------------------------------

/// One change event flowing through the broker.
///
/// Today this is constructed by the mutation callbacks (INSERT,
/// UPDATE, DELETE) on success. In P8a.2 the streaming WAL consumer
/// builds the same shape from pgoutput frames so the broker doesn't
/// care whether it came from a local mutation or replication.
///
/// The shape mirrors the proposal §C1 "broker event":
/// `{app_id, schema, table, op, pk, changed_columns, new_tuple_excerpt}`.
/// For P8a we conflate `schema` with `app_id` (every app has its own
/// schema named after `app_id`).
#[derive(Debug, Clone)]
pub struct ChangeEvent {
    /// App that produced the event. Used by the routing table to
    /// isolate tenants.
    pub app_id: String,
    /// Collection (= table inside the app schema).
    pub collection: String,
    /// Operation kind — `"insert"`, `"update"`, `"delete"`.
    pub op: ChangeOp,
    /// Primary key of the affected row, if known. Today this is the
    /// integer surrogate id (BIGINT IDENTITY); typed_id support is
    /// deferred (open question 3 in the proposal).
    pub pk: Option<i64>,
    /// Columns the mutation touched. For INSERT this is "every
    /// declared column" — we only track the SET-side of UPDATE here.
    /// Empty for DELETE.
    pub changed_columns: Vec<String>,
    /// Text-encoded column values for the affected row.
    ///
    /// - INSERT / UPDATE: the new tuple's column values.
    /// - DELETE: the old tuple's column values (so a subscriber whose
    ///   filter matched the now-deleted row still gets a `delete`
    ///   event).
    ///
    /// Populated by the WAL consumer from pgoutput Insert/Update/Delete
    /// frames; the local-emit fast path populates it from the mutation
    /// handler's SET / WHERE clauses where available. May be empty when
    /// neither path can produce a tuple (e.g. local-emit DELETE with no
    /// row snapshot) — readers that find a missing column treat the
    /// predicate as non-matching (see [`crate::read_set::Predicate::matches`]).
    pub new_tuple: HashMap<String, String>,
    /// Optional old-tuple snapshot for UPDATE events. When present, a
    /// subscriber's predicate fires if EITHER `new_tuple` OR
    /// `old_tuple` matches — captures "row left the view" semantics
    /// alongside "row entered the view".
    ///
    /// `None` for INSERT (no old tuple exists) and DELETE (the old
    /// tuple is already in `new_tuple` — see field doc above).
    pub old_tuple: Option<HashMap<String, String>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeOp {
    Insert,
    Update,
    Delete,
}

impl ChangeOp {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Insert => "insert",
            Self::Update => "update",
            Self::Delete => "delete",
        }
    }
}

// ---------------------------------------------------------------------------
// Subscription
// ---------------------------------------------------------------------------

/// Default bounded-queue depth per subscription. Picked to match the
/// proposal's stated default. Overflow triggers a `resync` event.
pub const DEFAULT_QUEUE_DEPTH: usize = 1024;

/// One subscriber's view of the broker.
///
/// Held by the V8 layer (one per `subscribe()` call). Owned via
/// `Rc<RefCell<Inner>>` so the broker can push events into it without
/// holding a borrow across `.await`.
#[derive(Clone)]
pub struct Subscription(Rc<RefCell<SubscriptionInner>>);

impl std::fmt::Debug for Subscription {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let inner = self.0.borrow();
        f.debug_struct("Subscription")
            .field("id", &inner.id)
            .field("app_id", &inner.app_id)
            .field("collection", &inner.collection)
            .field("queue_len", &inner.queue.len())
            .field("closed", &inner.closed)
            .finish()
    }
}

struct SubscriptionInner {
    /// Monotonic id within the broker's per-(app, collection) bucket.
    /// Used for fast removal from the routing table.
    id: u64,
    app_id: String,
    collection: String,
    /// Pending events not yet drained by the iterator.
    queue: std::collections::VecDeque<SubscriptionMessage>,
    /// Max items in `queue`. Overflow yields a single
    /// `SubscriptionMessage::Resync` and clears.
    max_queue: usize,
    /// Set by the iterator when waiting for the next event. Woken by
    /// `publish` and `close`.
    waker: Option<Waker>,
    /// Set when `Subscription::close` is called (or the holder is
    /// dropped) — the iterator returns `Closed` and terminates.
    closed: bool,
    /// True if we've already emitted a `Resync` since the last drain.
    /// Avoids spamming the iterator with multiple Resync events for
    /// successive overflows.
    resync_pending: bool,
    /// P8b read-set narrowing.
    ///
    /// `None` → coarse-grained: every change on this subscription's
    /// `(app_id, collection)` is delivered (the P8a behaviour kept for
    /// back-compat with the existing subscribe/subscribePoll callers
    /// that haven't been routed through useQuery yet).
    ///
    /// `Some(entries)` → the broker evaluates each entry's predicate
    /// against the event tuple and delivers only if at least one entry
    /// for the matching collection matches.
    read_set: Option<Vec<ReadSetEntry>>,
}

/// A single message the iterator yields. `Resync` is special — see
/// the proposal's backpressure section.
#[derive(Debug, Clone)]
pub enum SubscriptionMessage {
    Change(ChangeEvent),
    /// Bounded queue overflowed; client should refetch and drop
    /// any cached results.
    Resync,
    /// Subscription was closed by the consumer or the broker (e.g.
    /// connection drop). The iterator terminates after this.
    Closed,
}

impl Subscription {
    /// Construct an empty subscription. The caller is expected to
    /// register it with the broker via [`Broker::subscribe`] — this
    /// constructor doesn't take the broker because tests construct
    /// subscriptions standalone.
    pub fn new(id: u64, app_id: String, collection: String, max_queue: usize) -> Self {
        Self(Rc::new(RefCell::new(SubscriptionInner {
            id,
            app_id,
            collection,
            queue: std::collections::VecDeque::new(),
            max_queue,
            waker: None,
            closed: false,
            resync_pending: false,
            read_set: None,
        })))
    }

    /// Attach a read-set to this subscription. Called by Stage 4
    /// (useQuery) after the initial query handler runs and captures
    /// its predicate fingerprint. Subscriptions opened directly via
    /// `db.subscribe(collection)` (no useQuery) leave this `None`, and
    /// the broker falls back to coarse-grained delivery.
    ///
    /// The read-set is set once at registration time; today there's no
    /// API to mutate it. If a query handler is re-run and its
    /// predicate set changes, useQuery is expected to close the old
    /// subscription and open a new one with the fresh read-set —
    /// matches the proposal's "fingerprint change ⇒ resubscribe" rule.
    pub fn set_read_set(&self, entries: Vec<ReadSetEntry>) {
        self.0.borrow_mut().read_set = Some(entries);
    }

    /// Evaluate this subscription's read-set against an event tuple.
    /// True if the subscription should receive the event.
    ///
    /// - `None` read-set: every event for the matching collection
    ///   passes (coarse-grained).
    /// - `Some(entries)`: at least one entry whose collection matches
    ///   the event AND whose predicate matches the row.
    ///
    /// For UPDATE events the broker also checks `old_tuple` so a row
    /// "leaving the view" still fires.
    pub(crate) fn accepts(&self, event: &ChangeEvent) -> bool {
        let inner = self.0.borrow();
        let Some(rs) = inner.read_set.as_ref() else {
            return true;
        };
        for entry in rs {
            if entry.collection != event.collection {
                continue;
            }
            if entry.matches(&event.new_tuple) {
                return true;
            }
            if let Some(old) = event.old_tuple.as_ref() {
                if entry.matches(old) {
                    return true;
                }
            }
        }
        false
    }

    pub fn id(&self) -> u64 {
        self.0.borrow().id
    }
    pub fn app_id(&self) -> String {
        self.0.borrow().app_id.clone()
    }
    pub fn collection(&self) -> String {
        self.0.borrow().collection.clone()
    }

    /// Push an event onto this subscription's queue. Called by
    /// [`Broker::publish`] — not generally by user code.
    ///
    /// If the queue is full, replaces all pending messages with a
    /// single `Resync`. The iterator sees the resync, refetches once,
    /// and starts catching up again.
    pub fn push(&self, msg: SubscriptionMessage) {
        let mut inner = self.0.borrow_mut();
        if inner.closed {
            return;
        }
        if inner.queue.len() >= inner.max_queue {
            // Overflow — drop everything, emit one Resync. The
            // subscriber's job is then to fetch fresh state.
            if !inner.resync_pending {
                inner.queue.clear();
                inner.queue.push_back(SubscriptionMessage::Resync);
                inner.resync_pending = true;
            }
            if let Some(w) = inner.waker.take() {
                w.wake();
            }
            return;
        }
        inner.queue.push_back(msg);
        if let Some(w) = inner.waker.take() {
            w.wake();
        }
    }

    /// Pop the next pending event, or `None` if the queue is empty.
    /// The iterator stores `waker` via [`Subscription::register_waker`]
    /// before checking `pop` again so it gets woken on the next push.
    pub fn pop(&self) -> Option<SubscriptionMessage> {
        let mut inner = self.0.borrow_mut();
        let msg = inner.queue.pop_front();
        if matches!(msg, Some(SubscriptionMessage::Resync)) {
            // Reset so a future overflow can re-emit.
            inner.resync_pending = false;
        }
        msg
    }

    /// Register a Waker to be notified on the next `push` / `close`.
    /// Overwrites any prior waker — only the latest iterator poll
    /// is observed.
    pub fn register_waker(&self, w: Waker) {
        let mut inner = self.0.borrow_mut();
        inner.waker = Some(w);
    }

    /// True if the iterator should terminate (closed + queue empty).
    pub fn is_terminal(&self) -> bool {
        let inner = self.0.borrow();
        inner.closed && inner.queue.is_empty()
    }

    /// Close the subscription. Iterators return `Closed` on next poll
    /// and the broker removes the entry on its next sweep.
    pub fn close(&self) {
        let mut inner = self.0.borrow_mut();
        if inner.closed {
            return;
        }
        inner.closed = true;
        inner.queue.push_back(SubscriptionMessage::Closed);
        if let Some(w) = inner.waker.take() {
            w.wake();
        }
    }

    /// True if `close()` has been called. Broker uses this to GC.
    pub fn is_closed(&self) -> bool {
        self.0.borrow().closed
    }
}

// ---------------------------------------------------------------------------
// Broker
// ---------------------------------------------------------------------------

/// The routing table.
///
/// Indexed by `(app_id, collection)`. P8b will add a second-level
/// index by `ReadSet` fingerprint; for P8a the per-collection list is
/// scanned linearly on each publish (`O(subscribers_on_this_collection)`).
pub struct Broker {
    /// Counter for [`Subscription::id`].
    next_id: u64,
    /// Subscribers per (app_id, collection). Insertion-order list so
    /// publish iterates in subscribe order — keeps test output
    /// deterministic.
    by_key: HashMap<(String, String), Vec<Subscription>>,
}

impl Broker {
    pub fn new() -> Self {
        Self {
            next_id: 0,
            by_key: HashMap::new(),
        }
    }

    /// Register a fresh subscription on `(app_id, collection)`.
    ///
    /// Returns the subscription handle. Drop the handle (or call
    /// [`Subscription::close`]) to unsubscribe; the broker GC's its
    /// reference on next publish.
    pub fn subscribe(&mut self, app_id: &str, collection: &str) -> Subscription {
        self.next_id += 1;
        let sub = Subscription::new(
            self.next_id,
            app_id.to_string(),
            collection.to_string(),
            DEFAULT_QUEUE_DEPTH,
        );
        self.by_key
            .entry((app_id.to_string(), collection.to_string()))
            .or_default()
            .push(sub.clone());
        sub
    }

    /// Publish a change event. All subscribers on the matching
    /// `(app_id, collection)` whose read-set accepts the event tuple
    /// receive it; closed subscribers are pruned in the same pass.
    ///
    /// P8b: filtering happens per-subscriber via
    /// [`Subscription::accepts`]. The bucket index by `(app_id,
    /// collection)` is still the primary fan-in — subscribers on
    /// unrelated collections never enter the predicate-eval path. The
    /// hot inner check is `O(entries_in_read_set)` per event per
    /// matching subscriber and short-circuits on the first match.
    pub fn publish(&mut self, event: &ChangeEvent) {
        let key = (event.app_id.clone(), event.collection.clone());
        let Some(subs) = self.by_key.get_mut(&key) else {
            return;
        };
        // Two-pass: clone live subs (cheap — Rc), then push outside
        // the &mut borrow so a panic-on-push can't poison the table.
        let alive: Vec<Subscription> = subs.iter().filter(|s| !s.is_closed()).cloned().collect();
        // Prune dead entries.
        if alive.len() != subs.len() {
            subs.retain(|s| !s.is_closed());
        }
        // Empty key — drop the bucket so iteration stays bounded.
        if subs.is_empty() {
            self.by_key.remove(&key);
        }
        for s in alive {
            if !s.accepts(event) {
                continue;
            }
            s.push(SubscriptionMessage::Change(event.clone()));
        }
    }

    /// Number of registered (not-yet-closed) subscriptions across all
    /// keys. Used by tests + the maintenance cron for metrics.
    pub fn subscription_count(&self) -> usize {
        self.by_key
            .values()
            .map(|v| v.iter().filter(|s| !s.is_closed()).count())
            .sum()
    }

    /// Drop subscribers for a given app — used by the per-app slot GC
    /// when the app is deleted. Each affected subscription is sent a
    /// `Closed` message.
    pub fn drop_app(&mut self, app_id: &str) {
        let keys: Vec<_> = self
            .by_key
            .keys()
            .filter(|(a, _)| a == app_id)
            .cloned()
            .collect();
        for k in keys {
            if let Some(subs) = self.by_key.remove(&k) {
                for s in subs {
                    s.close();
                }
            }
        }
    }
}

impl Default for Broker {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for Broker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Broker")
            .field("next_id", &self.next_id)
            .field("buckets", &self.by_key.len())
            .field("subscriptions", &self.subscription_count())
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Thread-local accessor
// ---------------------------------------------------------------------------
//
// The broker lives in a thread-local cell mirroring `DB_POOL` /
// `TX_CONN`: the compio runtime is single-threaded per worker and
// every callback that needs the broker (mutation publish, subscribe,
// unsubscribe) runs on the same isolate thread.

thread_local! {
    pub(crate) static BROKER: RefCell<Broker> = RefCell::new(Broker::new());
}

/// Convenience accessor — publish without locating the broker manually.
pub fn publish(event: &ChangeEvent) {
    BROKER.with(|b| b.borrow_mut().publish(event));
}

/// Convenience accessor — subscribe without locating the broker
/// manually.
pub fn subscribe(app_id: &str, collection: &str) -> Subscription {
    BROKER.with(|b| b.borrow_mut().subscribe(app_id, collection))
}

/// Total live (not-yet-closed) subscriptions on this thread's broker.
/// Tests use this to verify the [`crate::v8_classes::subscription`]
/// Weak finalizer reclaims broker slots when V8 GCs an orphaned
/// wrapper.
pub fn live_subscription_count() -> usize {
    BROKER.with(|b| b.borrow().subscription_count())
}

/// Drop ALL subscribers (for an app, or globally with `None`). Tests
/// + worker shutdown use this.
pub fn drop_app(app_id: Option<&str>) {
    BROKER.with(|b| {
        let mut br = b.borrow_mut();
        if let Some(id) = app_id {
            br.drop_app(id);
        } else {
            // Drop everything — used by the "test cleanup" path.
            let keys: Vec<_> = br.by_key.keys().cloned().collect();
            for k in keys {
                if let Some(subs) = br.by_key.remove(&k) {
                    for s in subs {
                        s.close();
                    }
                }
            }
        }
    });
}

// ---------------------------------------------------------------------------
// JS-facing event JSON
// ---------------------------------------------------------------------------

/// Serialise a [`SubscriptionMessage`] for the V8 bridge.
///
/// The wire shape is:
/// - `{"kind":"change", "op":"insert"|"update"|"delete",
///    "collection":"...", "pk": 1|null, "columns":[...]}`
/// - `{"kind":"resync"}`
/// - `{"kind":"closed"}`
///
/// Distinct from `db.find` / `db.insert` JSON: those are arrays of
/// rows; this is a single event per `next()` of the AsyncIterable.
pub fn message_to_json(msg: &SubscriptionMessage) -> String {
    match msg {
        SubscriptionMessage::Change(ev) => serde_json::json!({
            "kind": "change",
            "op": ev.op.as_str(),
            "collection": ev.collection,
            "pk": ev.pk,
            "columns": ev.changed_columns,
        })
        .to_string(),
        SubscriptionMessage::Resync => Value::Object({
            let mut m = serde_json::Map::new();
            m.insert("kind".into(), Value::String("resync".into()));
            m
        })
        .to_string(),
        SubscriptionMessage::Closed => Value::Object({
            let mut m = serde_json::Map::new();
            m.insert("kind".into(), Value::String("closed".into()));
            m
        })
        .to_string(),
    }
}

// ---------------------------------------------------------------------------
// WS push-frame format (Stage 3)
// ---------------------------------------------------------------------------
//
// Mirrors the shape spelled out in the P8b task contract:
//
// ```json
// { "type": "zs.subscription.event",
//   "handle": "<subscription_id>",
//   "event": { "kind": "insert", "collection": "users",
//              "pk": 42, "row": {...} } }
// ```
//
// The intent is that a WS handler in JS owns a `Map<handle, ws_conn>`
// and pushes the JSON-encoded frame on receipt. The frame is shaped to
// be self-describing — `type` lets the WS handler distinguish broker
// events from app-level WS messages on the same connection.

/// Build a WS push frame for a change event.
///
/// The `handle` parameter is the per-WS-connection subscription handle
/// (stringified — protocol stays stable if the handle type ever
/// widens). The `row` carries the new tuple values so client code can
/// avoid an extra fetch on the common "patch in place" path.
pub fn ws_frame_for_change(handle: &str, ev: &ChangeEvent) -> String {
    serde_json::json!({
        "type": "zs.subscription.event",
        "handle": handle,
        "event": {
            "kind": ev.op.as_str(),
            "collection": ev.collection,
            "pk": ev.pk,
            "columns": ev.changed_columns,
            "row": ev.new_tuple,
        }
    })
    .to_string()
}

/// Build a WS push frame for a non-Change subscription message
/// (Resync / Closed). `Change` should go through
/// [`ws_frame_for_change`] which carries the row payload.
pub fn ws_frame_for_control(handle: &str, msg: &SubscriptionMessage) -> Option<String> {
    let kind = match msg {
        SubscriptionMessage::Resync => "resync",
        SubscriptionMessage::Closed => "closed",
        SubscriptionMessage::Change(_) => return None,
    };
    Some(
        serde_json::json!({
            "type": "zs.subscription.event",
            "handle": handle,
            "event": { "kind": kind },
        })
        .to_string(),
    )
}

/// Convenience: dispatch a [`SubscriptionMessage`] into the right
/// WS-frame shape. Returns `None` only if the message is `Change` and
/// the caller passed it through this path by mistake (shouldn't happen
/// in practice — kept defensive).
pub fn ws_frame(handle: &str, msg: &SubscriptionMessage) -> String {
    match msg {
        SubscriptionMessage::Change(ev) => ws_frame_for_change(handle, ev),
        other => ws_frame_for_control(handle, other).unwrap_or_default(),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(app: &str, col: &str, op: ChangeOp, pk: Option<i64>) -> ChangeEvent {
        ChangeEvent {
            app_id: app.to_string(),
            collection: col.to_string(),
            op,
            pk,
            changed_columns: vec![],
            new_tuple: HashMap::new(),
            old_tuple: None,
        }
    }

    fn ev_with_tuple(
        app: &str,
        col: &str,
        op: ChangeOp,
        pk: Option<i64>,
        tuple: &[(&str, &str)],
    ) -> ChangeEvent {
        ChangeEvent {
            app_id: app.to_string(),
            collection: col.to_string(),
            op,
            pk,
            changed_columns: vec![],
            new_tuple: tuple
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect(),
            old_tuple: None,
        }
    }

    #[test]
    fn subscribe_and_publish_delivers_event() {
        let mut b = Broker::new();
        let s = b.subscribe("a", "messages");
        b.publish(&ev("a", "messages", ChangeOp::Insert, Some(1)));
        let msg = s.pop().expect("expected a message");
        let SubscriptionMessage::Change(c) = msg else {
            panic!("expected Change variant");
        };
        assert_eq!(c.collection, "messages");
        assert_eq!(c.op, ChangeOp::Insert);
        assert_eq!(c.pk, Some(1));
    }

    #[test]
    fn other_app_events_isolated() {
        let mut b = Broker::new();
        let s = b.subscribe("a", "messages");
        b.publish(&ev("b", "messages", ChangeOp::Insert, Some(1)));
        assert!(s.pop().is_none());
    }

    #[test]
    fn other_collection_events_isolated() {
        let mut b = Broker::new();
        let s = b.subscribe("a", "messages");
        b.publish(&ev("a", "channels", ChangeOp::Insert, Some(1)));
        assert!(s.pop().is_none());
    }

    #[test]
    fn multiple_subscribers_receive_event() {
        let mut b = Broker::new();
        let s1 = b.subscribe("a", "messages");
        let s2 = b.subscribe("a", "messages");
        b.publish(&ev("a", "messages", ChangeOp::Update, Some(7)));
        assert!(matches!(s1.pop(), Some(SubscriptionMessage::Change(_))));
        assert!(matches!(s2.pop(), Some(SubscriptionMessage::Change(_))));
    }

    #[test]
    fn delivers_insert_update_delete() {
        let mut b = Broker::new();
        let s = b.subscribe("a", "messages");
        b.publish(&ev("a", "messages", ChangeOp::Insert, Some(1)));
        b.publish(&ev("a", "messages", ChangeOp::Update, Some(1)));
        b.publish(&ev("a", "messages", ChangeOp::Delete, Some(1)));
        let ops: Vec<_> = (0..3)
            .filter_map(|_| match s.pop() {
                Some(SubscriptionMessage::Change(c)) => Some(c.op),
                _ => None,
            })
            .collect();
        assert_eq!(ops, vec![ChangeOp::Insert, ChangeOp::Update, ChangeOp::Delete]);
    }

    #[test]
    fn close_terminates_iterator() {
        let mut b = Broker::new();
        let s = b.subscribe("a", "messages");
        b.publish(&ev("a", "messages", ChangeOp::Insert, Some(1)));
        s.close();
        // Pending change is preserved until drained.
        assert!(matches!(s.pop(), Some(SubscriptionMessage::Change(_))));
        // Then Closed.
        assert!(matches!(s.pop(), Some(SubscriptionMessage::Closed)));
        assert!(s.is_terminal());
    }

    #[test]
    fn dropped_subscription_pruned_on_next_publish() {
        let mut b = Broker::new();
        let s = b.subscribe("a", "messages");
        assert_eq!(b.subscription_count(), 1);
        s.close();
        b.publish(&ev("a", "messages", ChangeOp::Insert, Some(1)));
        assert_eq!(b.subscription_count(), 0);
    }

    #[test]
    fn overflow_collapses_to_resync() {
        // Tiny queue so we can overflow it in the test.
        let s = Subscription::new(1, "a".into(), "m".into(), 2);
        s.push(SubscriptionMessage::Change(ev(
            "a",
            "m",
            ChangeOp::Insert,
            Some(1),
        )));
        s.push(SubscriptionMessage::Change(ev(
            "a",
            "m",
            ChangeOp::Insert,
            Some(2),
        )));
        s.push(SubscriptionMessage::Change(ev(
            "a",
            "m",
            ChangeOp::Insert,
            Some(3),
        ))); // overflow
        // Queue now contains a single Resync.
        assert!(matches!(s.pop(), Some(SubscriptionMessage::Resync)));
        assert!(s.pop().is_none());
    }

    #[test]
    fn message_to_json_change_shape() {
        let m = SubscriptionMessage::Change(ChangeEvent {
            app_id: "a".into(),
            collection: "messages".into(),
            op: ChangeOp::Insert,
            pk: Some(7),
            changed_columns: vec!["title".into(), "body".into()],
            new_tuple: HashMap::new(),
            old_tuple: None,
        });
        let v: serde_json::Value = serde_json::from_str(&message_to_json(&m)).unwrap();
        assert_eq!(v["kind"], "change");
        assert_eq!(v["op"], "insert");
        assert_eq!(v["collection"], "messages");
        assert_eq!(v["pk"], 7);
        assert_eq!(v["columns"][0], "title");
    }

    #[test]
    fn message_to_json_resync() {
        let v: serde_json::Value =
            serde_json::from_str(&message_to_json(&SubscriptionMessage::Resync)).unwrap();
        assert_eq!(v["kind"], "resync");
    }

    // ---------- P8b read-set narrowing ----------

    use crate::read_set::{self, ReadSetEntry};

    fn rs_entry(collection: &str, filter: serde_json::Value) -> ReadSetEntry {
        ReadSetEntry {
            collection: collection.to_string(),
            predicate: read_set::normalise_filter(&filter),
        }
    }

    #[test]
    fn b8b_read_set_narrowing_filters_irrelevant_events() {
        let mut b = Broker::new();
        let s = b.subscribe("a", "messages");
        s.set_read_set(vec![rs_entry("messages", serde_json::json!({ "userId": 42 }))]);

        // Matching event → delivered.
        b.publish(&ev_with_tuple(
            "a",
            "messages",
            ChangeOp::Insert,
            Some(1),
            &[("userId", "42"), ("body", "hi")],
        ));
        // Non-matching event → filtered.
        b.publish(&ev_with_tuple(
            "a",
            "messages",
            ChangeOp::Insert,
            Some(2),
            &[("userId", "99"), ("body", "nope")],
        ));

        match s.pop() {
            Some(SubscriptionMessage::Change(c)) => assert_eq!(c.pk, Some(1)),
            other => panic!("expected delivered Change, got {other:?}"),
        }
        assert!(
            s.pop().is_none(),
            "userId=99 event must not reach this subscriber"
        );
    }

    #[test]
    fn b8b_read_set_update_old_or_new_matches() {
        // Row updates userId from 42 to 99. Subscriber on {userId: 42}
        // should still see the event — the row "left the view".
        let mut b = Broker::new();
        let s = b.subscribe("a", "messages");
        s.set_read_set(vec![rs_entry("messages", serde_json::json!({ "userId": 42 }))]);

        let ev = ChangeEvent {
            app_id: "a".into(),
            collection: "messages".into(),
            op: ChangeOp::Update,
            pk: Some(1),
            changed_columns: vec!["userId".into()],
            new_tuple: [("userId".to_string(), "99".to_string())].into(),
            old_tuple: Some([("userId".to_string(), "42".to_string())].into()),
        };
        b.publish(&ev);
        assert!(matches!(s.pop(), Some(SubscriptionMessage::Change(_))));

        // A subscriber on {userId: 7} sees neither side of the update.
        let s2 = b.subscribe("a", "messages");
        s2.set_read_set(vec![rs_entry("messages", serde_json::json!({ "userId": 7 }))]);
        b.publish(&ev);
        assert!(s2.pop().is_none());
    }

    #[test]
    fn b8b_read_set_range_query() {
        let mut b = Broker::new();
        let s = b.subscribe("a", "events");
        s.set_read_set(vec![rs_entry(
            "events",
            serde_json::json!({ "createdAt": { "$gt": 1000 } }),
        )]);

        // Past threshold → delivered.
        b.publish(&ev_with_tuple(
            "a",
            "events",
            ChangeOp::Insert,
            Some(1),
            &[("createdAt", "1500")],
        ));
        // Below threshold → filtered.
        b.publish(&ev_with_tuple(
            "a",
            "events",
            ChangeOp::Insert,
            Some(2),
            &[("createdAt", "500")],
        ));
        match s.pop() {
            Some(SubscriptionMessage::Change(c)) => assert_eq!(c.pk, Some(1)),
            other => panic!("expected pk=1, got {other:?}"),
        }
        assert!(s.pop().is_none());
    }

    #[test]
    fn b8b_read_set_complex_falls_back_to_coarse() {
        // $or normalises to None ⇒ coarse-grained ⇒ every collection event fires.
        let mut b = Broker::new();
        let s = b.subscribe("a", "messages");
        s.set_read_set(vec![rs_entry(
            "messages",
            serde_json::json!({ "$or": [ { "a": 1 }, { "b": 2 } ] }),
        )]);
        // Without any read-set match, predicate is None (coarse).
        b.publish(&ev_with_tuple(
            "a",
            "messages",
            ChangeOp::Insert,
            Some(99),
            &[("a", "99"), ("b", "99")],
        ));
        assert!(matches!(s.pop(), Some(SubscriptionMessage::Change(_))));
    }

    #[test]
    fn b8b_empty_read_set_filters_everything_on_collection() {
        // A read-set that is `Some(empty_vec)` — explicitly attached
        // but with zero entries on the matching collection — should
        // accept nothing. (Distinct from `None` which means coarse.)
        let s = Subscription::new(1, "a".into(), "messages".into(), 8);
        s.set_read_set(vec![]);
        let event = ChangeEvent {
            app_id: "a".into(),
            collection: "messages".into(),
            op: ChangeOp::Insert,
            pk: Some(1),
            changed_columns: vec![],
            new_tuple: [("userId".into(), "42".into())].into(),
            old_tuple: None,
        };
        assert!(!s.accepts(&event));
    }

    #[test]
    fn b8b_read_set_skips_different_collection_entries() {
        // Subscription bucket is keyed by collection so this case
        // is mostly redundant with the bucket index, but we exercise
        // the entry.collection != event.collection branch explicitly.
        let s = Subscription::new(1, "a".into(), "messages".into(), 8);
        s.set_read_set(vec![rs_entry("channels", serde_json::json!({ "userId": 42 }))]);
        let event = ChangeEvent {
            app_id: "a".into(),
            collection: "messages".into(),
            op: ChangeOp::Insert,
            pk: Some(1),
            changed_columns: vec![],
            new_tuple: [("userId".into(), "42".into())].into(),
            old_tuple: None,
        };
        assert!(!s.accepts(&event));
    }

    #[test]
    fn b8b_no_read_set_accepts_every_event() {
        // Back-compat: subscriptions that don't set a read-set must
        // continue to behave as P8a (coarse-grained collection match).
        let mut b = Broker::new();
        let s = b.subscribe("a", "messages");
        // No set_read_set call.
        b.publish(&ev_with_tuple(
            "a",
            "messages",
            ChangeOp::Insert,
            Some(1),
            &[("userId", "anything")],
        ));
        assert!(matches!(s.pop(), Some(SubscriptionMessage::Change(_))));
    }

    // ---------- P8b Stage 3: WS push frames ----------

    #[test]
    fn b8b_ws_frame_change_shape() {
        let mut tuple = HashMap::new();
        tuple.insert("userId".into(), "42".into());
        tuple.insert("body".into(), "hi".into());
        let ev = ChangeEvent {
            app_id: "a".into(),
            collection: "messages".into(),
            op: ChangeOp::Insert,
            pk: Some(7),
            changed_columns: vec!["userId".into(), "body".into()],
            new_tuple: tuple,
            old_tuple: None,
        };
        let frame = ws_frame_for_change("sub_42", &ev);
        let v: serde_json::Value = serde_json::from_str(&frame).unwrap();
        assert_eq!(v["type"], "zs.subscription.event");
        assert_eq!(v["handle"], "sub_42");
        assert_eq!(v["event"]["kind"], "insert");
        assert_eq!(v["event"]["collection"], "messages");
        assert_eq!(v["event"]["pk"], 7);
        assert_eq!(v["event"]["row"]["userId"], "42");
    }

    #[test]
    fn b8b_ws_frame_resync_and_closed() {
        let r = ws_frame_for_control("h", &SubscriptionMessage::Resync).unwrap();
        let c = ws_frame_for_control("h", &SubscriptionMessage::Closed).unwrap();
        let rv: serde_json::Value = serde_json::from_str(&r).unwrap();
        let cv: serde_json::Value = serde_json::from_str(&c).unwrap();
        assert_eq!(rv["event"]["kind"], "resync");
        assert_eq!(cv["event"]["kind"], "closed");
    }

    #[test]
    fn b8b_ws_pushes_event_to_correct_connection() {
        // Two "WS clients" represented as broker subscriptions with
        // distinct read-sets. We simulate WS routing by stringifying
        // each event into its frame ONLY IF the subscription delivered
        // it (i.e. the broker's accept gate fired). The test asserts
        // that only the matching client's frame buffer has an event.
        let mut b = Broker::new();
        let s_alice = b.subscribe("app", "messages");
        s_alice.set_read_set(vec![rs_entry("messages", serde_json::json!({ "userId": 1 }))]);
        let s_bob = b.subscribe("app", "messages");
        s_bob.set_read_set(vec![rs_entry("messages", serde_json::json!({ "userId": 2 }))]);

        // Event for Alice only.
        b.publish(&ev_with_tuple(
            "app",
            "messages",
            ChangeOp::Insert,
            Some(10),
            &[("userId", "1"), ("body", "hi alice")],
        ));

        // Drain each subscriber's queue and format as WS frames.
        let alice_frames: Vec<String> = std::iter::from_fn(|| match s_alice.pop() {
            Some(SubscriptionMessage::Change(ev)) => {
                Some(ws_frame_for_change("alice", &ev))
            }
            _ => None,
        })
        .collect();
        let bob_frames: Vec<String> = std::iter::from_fn(|| match s_bob.pop() {
            Some(SubscriptionMessage::Change(ev)) => {
                Some(ws_frame_for_change("bob", &ev))
            }
            _ => None,
        })
        .collect();

        assert_eq!(alice_frames.len(), 1, "Alice should receive 1 frame");
        assert_eq!(bob_frames.len(), 0, "Bob should receive 0 frames");
        let f: serde_json::Value = serde_json::from_str(&alice_frames[0]).unwrap();
        assert_eq!(f["handle"], "alice");
        assert_eq!(f["event"]["row"]["userId"], "1");
    }

    #[test]
    fn drop_app_closes_all_subscribers() {
        let mut b = Broker::new();
        let s1 = b.subscribe("a", "messages");
        let s2 = b.subscribe("a", "channels");
        let s3 = b.subscribe("b", "messages");
        b.drop_app("a");
        assert!(s1.is_closed());
        assert!(s2.is_closed());
        assert!(!s3.is_closed());
    }
}
