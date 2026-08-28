//! In-memory subscription broker — foundation for C1 reactive queries.
//!
//! The broker is the routing table between change events (local
//! mutations within any isolate in this process, and pgoutput WAL
//! frames decoded by the streaming consumer) and the subscribers who
//! care about them.
//!
//! ## Scope
//!
//! - **Granularity:** a subscription with no read-set attached matches
//!   coarsely on the collection - `(collection="messages")` fires on
//!   EVERY change to `messages`. When a read-set IS attached, events
//!   are narrowed against its predicates - see `ReadSet` /
//!   `NormalisedPredicate` in the proposal §C1.
//! - **Backpressure:** a bounded per-subscriber queue (default 1024
//!   events) — overflow yields a `kind: "resync"` event and the queue
//!   is cleared. Same semantics as the proposal's "subscriber falls
//!   behind" path.
//! - **Lifecycle:** a subscription is a [`Subscription`] held by the
//!   V8 callback that opened it. Drop = unsubscribe.
//! - **Threading:** one process-wide broker connects every isolate thread
//!   in a worker process. The routing table and each subscription queue
//!   use separate mutexes. Broker operations never await while holding a
//!   lock, and the lock order is always broker then subscription.
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
//! ## Narrowing hooks
//!
//! - `Subscription::read_set` (Option) - `None` until the read-set
//!   capture writes one; `Broker::publish` checks each subscription's
//!   `ReadSet` against the event before pushing.
//! - `ChangeEvent::changed_columns` - populated by the WAL consumer,
//!   and by the local-emit path from the mutation handler's `SET`
//!   clause for INSERT/UPDATE; DELETE reports an empty set.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, PoisonError};
use std::task::Waker;

use serde_json::Value;

use crate::error::DbError;
use crate::read_set::ReadSetEntry;

// ---------------------------------------------------------------------------
// Event shape
// ---------------------------------------------------------------------------

/// One change event flowing through the broker.
///
/// Constructed by the mutation callbacks (INSERT, UPDATE, DELETE) on
/// success, and by the streaming WAL consumer from pgoutput frames, so
/// the broker doesn't care whether it came from a local mutation or
/// replication.
///
/// The shape mirrors the proposal §C1 "broker event":
/// `{app_id, schema, table, op, pk, changed_columns, new_tuple_excerpt}`.
/// `schema` is conflated with `app_id` (every app has its own schema
/// named after `app_id`).
#[derive(Debug, Clone)]
pub struct ChangeEvent {
    /// App that produced the event. Used by the routing table to
    /// isolate tenants.
    pub app_id: String,
    /// Collection (= table inside the app schema).
    pub collection: String,
    /// Operation kind — `"insert"`, `"update"`, `"delete"`.
    pub op: ChangeOp,
    /// Logical row id of the affected row, if known. This is the
    /// public `id` column serialized as text so typed ids and legacy
    /// numeric ids share one wire shape.
    pub pk: Option<String>,
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

/// DB-12: max concurrent (live) subscriptions one app may hold. Each
/// subscription carries a [`DEFAULT_QUEUE_DEPTH`]-slot queue and is iterated on
/// every matching publish, so an unbounded `for (…) db.t.subscribe(…)` loop
/// would grow the isolate's memory and per-event fan-out cost without bound.
/// 256 is far above any legitimate app's working set.
pub(crate) const MAX_SUBSCRIPTIONS_PER_APP: usize = 256;

/// One subscriber's view of the broker.
///
/// Held by the V8 layer (one per `subscribe()` call). Owned via
/// `Arc<Mutex<Inner>>` so a publisher on one isolate thread can wake
/// a subscriber on another isolate thread.
#[derive(Clone)]
pub struct Subscription(Arc<Mutex<SubscriptionInner>>);

impl std::fmt::Debug for Subscription {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let inner = self.lock_inner();
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
    /// Read-set narrowing.
    ///
    /// `None` → coarse-grained: every change on this subscription's
    /// `(app_id, collection)` is delivered. This is what the
    /// subscribe/subscribePoll callers that haven't been routed
    /// through useQuery still get.
    ///
    /// `Some(entries)` → the broker evaluates each entry's predicate
    /// against the event tuple and delivers only if at least one entry
    /// for the matching collection matches.
    read_set: Option<Vec<ReadSetEntry>>,
    /// Unit tests run in parallel while sharing the production-style
    /// process broker. Track ownership so test-only global cleanup and
    /// counts do not interfere with unrelated test threads.
    #[cfg(test)]
    owner_thread: std::thread::ThreadId,
}

/// A single message the iterator yields. `Resync` is special — see
/// the proposal's backpressure section.
///
/// `Change` carries the event behind an `Arc` so the broker can fan
/// the same payload out to N subscribers without deep-cloning the
/// `new_tuple` HashMap (and the rest of the event) per subscriber.
#[derive(Debug, Clone)]
pub enum SubscriptionMessage {
    Change(Arc<ChangeEvent>),
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
        Self(Arc::new(Mutex::new(SubscriptionInner {
            id,
            app_id,
            collection,
            queue: std::collections::VecDeque::new(),
            max_queue,
            waker: None,
            closed: false,
            resync_pending: false,
            read_set: None,
            #[cfg(test)]
            owner_thread: std::thread::current().id(),
        })))
    }

    fn lock_inner(&self) -> MutexGuard<'_, SubscriptionInner> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Attach a read-set to this subscription. Called after the
    /// initial query handler runs and captures its predicate
    /// fingerprint (the useQuery wiring on the SDK side). Subscriptions
    /// opened directly via `db.subscribe(collection)` (no read-set
    /// capture) leave this `None`, and the broker falls back to
    /// coarse-grained delivery.
    ///
    /// The read-set is set once at registration time; today there's no
    /// API to mutate it. If a query handler is re-run and its
    /// predicate set changes, useQuery is expected to close the old
    /// subscription and open a new one with the fresh read-set —
    /// matches the proposal's "fingerprint change ⇒ resubscribe" rule.
    pub fn set_read_set(&self, entries: Vec<ReadSetEntry>) {
        self.lock_inner().read_set = Some(entries);
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
        let inner = self.lock_inner();
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
        self.lock_inner().id
    }
    pub fn app_id(&self) -> String {
        self.lock_inner().app_id.clone()
    }
    pub fn collection(&self) -> String {
        self.lock_inner().collection.clone()
    }

    /// Push an event onto this subscription's queue. Called by
    /// [`Broker::publish`] — not generally by user code.
    ///
    /// If the queue is full, replaces all pending messages with a
    /// single `Resync`. The iterator sees the resync, refetches once,
    /// and starts catching up again.
    pub fn push(&self, msg: SubscriptionMessage) {
        let wake = {
            let mut inner = self.lock_inner();
            if inner.closed {
                return;
            }
            if inner.queue.len() >= inner.max_queue {
                // Overflow - drop everything, emit one Resync. The
                // subscriber's job is then to fetch fresh state.
                if !inner.resync_pending {
                    inner.queue.clear();
                    inner.queue.push_back(SubscriptionMessage::Resync);
                    inner.resync_pending = true;
                }
            } else {
                inner.queue.push_back(msg);
            }
            inner.waker.take()
        };
        if let Some(w) = wake {
            w.wake();
        }
    }

    /// Pop the next pending event, or `None` if the queue is empty.
    /// The iterator stores `waker` via [`Subscription::register_waker`]
    /// before checking `pop` again so it gets woken on the next push.
    pub fn pop(&self) -> Option<SubscriptionMessage> {
        let mut inner = self.lock_inner();
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
        let mut inner = self.lock_inner();
        inner.waker = Some(w);
    }

    /// True if the iterator should terminate (closed + queue empty).
    pub fn is_terminal(&self) -> bool {
        let inner = self.lock_inner();
        inner.closed && inner.queue.is_empty()
    }

    /// Close the subscription. Iterators return `Closed` on next poll
    /// and the broker removes the entry on its next sweep.
    pub fn close(&self) {
        let wake = {
            let mut inner = self.lock_inner();
            if inner.closed {
                return;
            }
            inner.closed = true;
            inner.queue.push_back(SubscriptionMessage::Closed);
            inner.waker.take()
        };
        if let Some(w) = wake {
            w.wake();
        }
    }

    /// True if `close()` has been called. Broker uses this to GC.
    pub fn is_closed(&self) -> bool {
        self.lock_inner().closed
    }

    #[cfg(test)]
    fn is_owned_by_current_thread(&self) -> bool {
        self.lock_inner().owner_thread == std::thread::current().id()
    }
}

// ---------------------------------------------------------------------------
// Broker
// ---------------------------------------------------------------------------

/// The routing table.
///
/// Indexed as a two-level map: `app_id → collection → Vec<Subscription>`.
/// The per-collection list is scanned linearly on each publish
/// (`O(subscribers_on_this_collection)`); a third-level index by
/// `ReadSet` fingerprint is the natural next step if that scan ever
/// becomes hot.
///
/// ## Why two levels (not a single `(String, String)` tuple key)
///
/// The hot path is the WAL-frame fan-out: every replicated mutation
/// asks "are there any subscribers on `(app_id, collection)`?" before
/// doing the more expensive event-shape work. With a tuple key, that
/// question requires allocating a temporary `(String, String)` per
/// call — two String heaps per WAL frame, multiplied by the number of
/// rows per frame, on every multi-tenant worker.
///
/// The two-level layout lets the lookup go through `&str` borrows
/// directly: `self.by_key.get(app)?.get(collection)?` — zero
/// allocations on the read path. Owned Strings are still produced
/// exactly once at subscribe-time (for `HashMap::entry`), which is
/// the rare/cold path.
pub struct Broker {
    /// Counter for [`Subscription::id`].
    next_id: u64,
    /// Subscribers indexed by app, then collection. Insertion-order
    /// `Vec` so publish iterates in subscribe order — keeps test
    /// output deterministic.
    by_key: HashMap<String, HashMap<String, Vec<Subscription>>>,
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
    ///
    /// **Infallible** low-level primitive for internal publishers and unit
    /// tests that construct a broker in isolation. Creator-facing mint sites
    /// must use [`Self::try_subscribe`] so schema-pending and per-app resource
    /// limits reach JavaScript as typed errors.
    pub fn subscribe(&mut self, app_id: &str, collection: &str) -> Subscription {
        self.next_id += 1;
        let sub = Subscription::new(
            self.next_id,
            app_id.to_string(),
            collection.to_string(),
            DEFAULT_QUEUE_DEPTH,
        );
        // `entry` requires owned keys; that's fine — subscribe is the
        // cold path (one call per `db.subscribe(...)`), and the inner
        // HashMap is allocated lazily on first subscription for a
        // given app.
        self.by_key
            .entry(app_id.to_string())
            .or_default()
            .entry(collection.to_string())
            .or_default()
            .push(sub.clone());
        sub
    }

    /// Fallible variant of [`Self::subscribe`] — rejects with
    /// `DbError::Coded { code: "schema_pending" }` while the app is in
    /// the schema-pending window (see [`engage_schema_pending`] /
    /// [`is_schema_pending`]).
    ///
    /// Per design §16.7 ("schema-pending decoder"): when a deploy is
    /// in flight that changes the app's schema, new subscriptions
    /// MUST be refused loudly so the SDK can surface a typed error
    /// rather than open a subscription whose collection may not exist
    /// after the deploy stabilises. The mirror "soft" path is backfill
    /// pause (see [`crate::backend::BrokerPauseGuard`]) which silently
    /// drops events at the publisher and emits one `Resync` per active
    /// subscription on disengage — there is no `subscribe()` rejection
    /// there because backfill is internally driven.
    pub fn try_subscribe(
        &mut self,
        app_id: &str,
        collection: &str,
    ) -> Result<Subscription, DbError> {
        if is_schema_pending(app_id) {
            return Err(DbError::Coded {
                code: "schema_pending".to_string(),
                message: format!(
                    "subscribe refused for app `{app_id}`: a schema-pending \
                     deploy is in flight; the collection set is unstable"
                ),
                hint: Some(
                    "retry after deploy stabilises (the SchemaPendingGuard \
                     window ends + one Resync is pushed per active subscription)"
                        .to_string(),
                ),
            });
        }
        // DB-12: cap the app's concurrent subscriptions and prune closed
        // handles before minting a replacement. Publish also prunes, but an
        // app can close subscriptions without ever publishing again; leaving
        // those entries here would let an open/close loop grow the global
        // routing table without bound even though the live count stays below
        // the cap.
        let mut remove_app = false;
        let live = self.by_key.get_mut(app_id).map_or(0, |collections| {
            collections.retain(|_, subscriptions| {
                subscriptions.retain(|subscription| !subscription.is_closed());
                !subscriptions.is_empty()
            });
            remove_app = collections.is_empty();
            collections.values().map(Vec::len).sum()
        });
        if remove_app {
            self.by_key.remove(app_id);
        }
        if live >= MAX_SUBSCRIPTIONS_PER_APP {
            return Err(DbError::Coded {
                code: "subscription_limit".to_string(),
                message: format!(
                    "subscribe refused for app `{app_id}`: at the maximum of \
                     {MAX_SUBSCRIPTIONS_PER_APP} concurrent subscriptions"
                ),
                hint: Some("close unused subscriptions before opening new ones".to_string()),
            });
        }
        Ok(self.subscribe(app_id, collection))
    }

    /// Fast probe: does any subscriber exist for `(app_id, collection)`?
    ///
    /// Zero allocations on the lookup path — both arguments are `&str`
    /// and the two-level `HashMap` uses native `Borrow<str>` lookups.
    /// Designed for callers on the WAL fan-out path that want to skip
    /// the more expensive event-shape work when nothing is subscribed.
    ///
    /// Returns `false` if the app has no subscribers, or the app has
    /// subscribers on other collections but not this one, or the
    /// `Vec<Subscription>` exists but contains only closed entries.
    /// Closed entries are NOT pruned here — that happens lazily in
    /// [`Self::publish`] — so a probe between `close()` and the next
    /// publish on the same key may still return `true`. This is a
    /// safe over-approximation: callers that act on `true` will fall
    /// through to `publish`, which is the canonical drop point.
    pub fn has_subscribers(&self, app_id: &str, collection: &str) -> bool {
        let Some(by_collection) = self.by_key.get(app_id) else {
            return false;
        };
        let Some(subs) = by_collection.get(collection) else {
            return false;
        };
        !subs.is_empty()
    }

    /// Publish a change event. All subscribers on the matching
    /// `(app_id, collection)` whose read-set accepts the event tuple
    /// receive it; closed subscribers are pruned in the same pass.
    ///
    /// Filtering happens per-subscriber via
    /// `Subscription::accepts`. The bucket index by `(app_id,
    /// collection)` is still the primary fan-in — subscribers on
    /// unrelated collections never enter the predicate-eval path. The
    /// hot inner check is `O(entries_in_read_set)` per event per
    /// matching subscriber and short-circuits on the first match.
    pub fn publish(&mut self, event: &ChangeEvent) {
        let subscribers = self.matching_subscriptions(event);
        deliver_event(event, subscribers);
    }

    /// Select and clone matching subscriber handles while pruning dead
    /// routing entries. The process-wide accessor releases the broker
    /// lock before it pushes messages and wakes tasks.
    fn matching_subscriptions(&mut self, event: &ChangeEvent) -> Vec<Subscription> {
        // Two-level lookup via `&str` — no `(String, String)`
        // allocation per call. Borrow-based `HashMap::get_mut` lookup
        // (`Borrow<str>` impl on the `String` key) keeps the hot path
        // alloc-free.
        let Some(by_collection) = self.by_key.get_mut(event.app_id.as_str()) else {
            return Vec::new();
        };
        let Some(subs) = by_collection.get_mut(event.collection.as_str()) else {
            return Vec::new();
        };
        // Prune dead entries in-place so the bucket stays bounded.
        subs.retain(|s| !s.is_closed());
        let is_empty = subs.is_empty();
        let matching = subs
            .iter()
            .filter(|subscription| subscription.accepts(event))
            .cloned()
            .collect();
        // Drop the bucket if pruning emptied it, so iteration stays
        // bounded. Done after the scan so the `&mut subs` borrow
        // has been released by the time we touch `self.by_key`. If
        // the per-app map empties out as a result, drop it too — keeps
        // `has_subscribers` cheap on apps that churn through ephemeral
        // collections.
        if is_empty {
            by_collection.remove(event.collection.as_str());
            if by_collection.is_empty() {
                self.by_key.remove(event.app_id.as_str());
            }
        }
        matching
    }

    /// Cheap predicate: is there at least one (possibly-still-closed)
    /// subscriber bucket entry for `(app_id, collection)`?
    ///
    /// Used by hot publish callers (notably the WAL consumer, which
    /// otherwise allocates two `HashMap<String, String>` per pgoutput
    /// frame BEFORE the broker would discard the event for lack of
    /// subscribers) to short-circuit tuple construction.
    ///
    /// Conservative-true: an entry whose subscribers have all been
    /// closed but not yet pruned counts as "has subscribers" — the
    /// next `publish` will GC them and the following call will return
    /// `false`. This is the cheaper of the two failure modes: at worst
    /// the caller builds one extra tuple after a subscriber drop, which
    /// `publish` then discards harmlessly.
    /// Number of registered (not-yet-closed) subscriptions across all
    /// keys. Used by tests + the maintenance cron for metrics.
    pub fn subscription_count(&self) -> usize {
        self.by_key
            .values()
            .flat_map(|by_collection| by_collection.values())
            .map(|v| v.iter().filter(|s| !s.is_closed()).count())
            .sum()
    }

    /// Count live (not-yet-closed) subscriptions for a single `app_id`
    /// in this process's broker. Used by the drop-namespace
    /// subscription gate: a non-zero count means a subscriber is still
    /// observable, so the drop defers (or, under `--force`, the broker
    /// is drained first).
    ///
    /// The control plane aggregates across worker processes via the
    /// admin endpoint; this is the process-local source.
    pub fn app_subscription_count(&self, app_id: &str) -> usize {
        let Some(by_collection) = self.by_key.get(app_id) else {
            return 0;
        };
        by_collection
            .values()
            .map(|v| v.iter().filter(|s| !s.is_closed()).count())
            .sum()
    }

    /// Drop subscribers for a given app — used by the per-app slot GC
    /// when the app is deleted. Each affected subscription is sent a
    /// `Closed` message.
    pub fn drop_app(&mut self, app_id: &str) {
        for subscription in self.take_app_subscriptions(app_id) {
            subscription.close();
        }
    }

    fn take_app_subscriptions(&mut self, app_id: &str) -> Vec<Subscription> {
        self.by_key
            .remove(app_id)
            .into_iter()
            .flat_map(|by_collection| by_collection.into_values().flatten())
            .collect()
    }

    #[cfg(not(test))]
    fn take_all_subscriptions(&mut self) -> Vec<Subscription> {
        std::mem::take(&mut self.by_key)
            .into_values()
            .flat_map(|by_collection| by_collection.into_values().flatten())
            .collect()
    }

    #[cfg(test)]
    fn take_current_thread_subscriptions(&mut self) -> Vec<Subscription> {
        let mut taken = Vec::new();
        self.by_key.retain(|_, by_collection| {
            by_collection.retain(|_, subscriptions| {
                subscriptions.retain(|subscription| {
                    if subscription.is_owned_by_current_thread() {
                        taken.push(subscription.clone());
                        false
                    } else {
                        true
                    }
                });
                !subscriptions.is_empty()
            });
            !by_collection.is_empty()
        });
        taken
    }

    #[cfg(test)]
    fn current_thread_subscription_count(&self) -> usize {
        self.by_key
            .values()
            .flat_map(|by_collection| by_collection.values())
            .map(|subscriptions| {
                subscriptions
                    .iter()
                    .filter(|subscription| {
                        subscription.is_owned_by_current_thread() && !subscription.is_closed()
                    })
                    .count()
            })
            .sum()
    }

    fn app_subscriptions(&self, app_id: &str) -> Vec<Subscription> {
        self.by_key
            .get(app_id)
            .into_iter()
            .flat_map(|by_collection| by_collection.values().flatten())
            .cloned()
            .collect()
    }

    /// Push a `Resync` to every active subscription registered for
    /// `app_id`.
    ///
    /// Invoked from [`crate::backend::BrokerPauseGuard::drop`] (after a
    /// backfill window) and
    /// [`crate::backend::SchemaPendingGuard::drop`] (after the
    /// schema-pending decoder window ends) per design §16.7.
    ///
    /// **Idempotent on a per-call basis.** Calling
    /// `resume_app_with_resync` N times pushes N `Resync` messages onto
    /// every active subscription's queue. Subscribers dedup: when
    /// [`Subscription::pop`] yields a `Resync` the iterator refetches
    /// and resets its state, so back-to-back `Resync` messages collapse
    /// at the consumer. Closed subscriptions are skipped; the routing
    /// table is not GC'd here (that happens lazily in
    /// [`Self::publish`]).
    ///
    /// Apps with no active subscriptions are a fast no-op — the
    /// two-level map lookup misses and the function returns immediately.
    pub fn resume_app_with_resync(&mut self, app_id: &str) {
        for subscription in self.app_subscriptions(app_id) {
            subscription.push(SubscriptionMessage::Resync);
        }
    }
}

fn deliver_event(event: &ChangeEvent, subscribers: Vec<Subscription>) {
    if subscribers.is_empty() {
        return;
    }
    // Share the event payload across all matching subscribers without
    // deep-cloning its tuple maps for every queue.
    let shared = Arc::new(event.clone());
    for subscription in subscribers {
        subscription.push(SubscriptionMessage::Change(Arc::clone(&shared)));
    }
}

impl Default for Broker {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for Broker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `buckets` matches the prior single-level meaning: total
        // `(app, collection)` pairs, NOT the number of distinct apps.
        let buckets: usize = self.by_key.values().map(|m| m.len()).sum();
        f.debug_struct("Broker")
            .field("next_id", &self.next_id)
            .field("apps", &self.by_key.len())
            .field("buckets", &buckets)
            .field("subscriptions", &self.subscription_count())
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Process-wide accessors
// ---------------------------------------------------------------------------
//
// A worker process runs many single-threaded compio runtimes, one per
// worker thread. WAL consumption may run on a different thread from a
// V8 subscriber, so the routing table must span every isolate in the
// process. Cross-container delivery remains the replication layer's
// responsibility: each worker process consumes the app's WAL stream,
// then publishes into its own process-wide broker.

static BROKER: LazyLock<Mutex<Broker>> = LazyLock::new(|| Mutex::new(Broker::new()));

/// Schema-pending decoder window (design section 16.7).
///
/// This is process-wide for the same reason as the broker: a deploy on
/// one worker thread must reject a subscription opened concurrently on
/// another worker thread in the same process.
static SCHEMA_PENDING_APPS: LazyLock<Mutex<HashSet<String>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));

fn lock_broker() -> MutexGuard<'static, Broker> {
    BROKER.lock().unwrap_or_else(PoisonError::into_inner)
}

fn lock_schema_pending() -> MutexGuard<'static, HashSet<String>> {
    SCHEMA_PENDING_APPS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
}

/// Mark `app_id` as schema-pending in this process. While engaged,
/// [`Broker::try_subscribe`] returns `DbError::Coded { code:
/// "schema_pending" }` for the app, and the SQLite CDC publisher
/// drops every packet whose `app_id` matches.
///
/// Internal — called from
/// [`crate::backend::SchemaPendingGuard::new`]; production code should
/// reach the guard through `BackendHandle::as_change_stream_*().engage_schema_pending(app_id)`.
pub fn engage_schema_pending(app_id: &str) {
    lock_schema_pending().insert(app_id.to_string());
}

/// Inverse of [`engage_schema_pending`]. Idempotent — calling on an
/// app that is not engaged is a no-op. Called from
/// [`crate::backend::SchemaPendingGuard::drop`] before
/// `resume_app_with_resync` pushes the per-subscription `Resync`.
pub fn disengage_schema_pending(app_id: &str) {
    lock_schema_pending().remove(app_id);
}

/// True if `app_id` is currently in the schema-pending window on
/// this process. Used by [`Broker::try_subscribe`] and by the SQLite
/// CDC publisher's drop-packet check.
pub fn is_schema_pending(app_id: &str) -> bool {
    lock_schema_pending().contains(app_id)
}

/// Convenience accessor — publish without locating the broker manually.
pub fn publish(event: &ChangeEvent) {
    let subscribers = lock_broker().matching_subscriptions(event);
    deliver_event(event, subscribers);
}

/// Convenience accessor - query the process-wide broker for whether
/// any subscriber is registered on `(app_id, collection)`. See
/// [`Broker::has_subscribers`] for the conservative-true semantics.
pub(crate) fn has_subscribers(app_id: &str, collection: &str) -> bool {
    lock_broker().has_subscribers(app_id, collection)
}

/// Convenience accessor — subscribe without locating the broker
/// manually.
pub fn subscribe(app_id: &str, collection: &str) -> Subscription {
    lock_broker().subscribe(app_id, collection)
}

/// Fallible variant of [`subscribe`] — surfaces the schema-pending
/// rejection branch. See [`Broker::try_subscribe`].
pub fn try_subscribe(app_id: &str, collection: &str) -> Result<Subscription, DbError> {
    lock_broker().try_subscribe(app_id, collection)
}

/// Total live (not-yet-closed) subscriptions in this process's broker.
/// Tests use this to verify the [`crate::v8_classes::subscription`]
/// Weak finalizer reclaims broker slots when V8 GCs an orphaned
/// wrapper.
pub fn live_subscription_count() -> usize {
    #[cfg(test)]
    {
        lock_broker().current_thread_subscription_count()
    }
    #[cfg(not(test))]
    {
        lock_broker().subscription_count()
    }
}

/// Live subscription count for a single `app_id` in this process's
/// broker. The §17.7 drop-namespace subscription gate reads this for
/// the in-process / single-worker path. See
/// [`Broker::app_subscription_count`].
pub fn app_subscription_count(app_id: &str) -> usize {
    lock_broker().app_subscription_count(app_id)
}

/// Push a resync marker to every live subscription for `app_id`.
///
/// Kept as a free function so backend guards do not reach through the
/// process-wide synchronization boundary directly.
pub(crate) fn resume_app_with_resync(app_id: &str) {
    let subscriptions = lock_broker().app_subscriptions(app_id);
    for subscription in subscriptions {
        subscription.push(SubscriptionMessage::Resync);
    }
}

/// Drop ALL subscribers (for an app, or globally with `None`). Tests
/// + worker shutdown use this.
pub fn drop_app(app_id: Option<&str>) {
    let subscriptions = {
        let mut broker = lock_broker();
        match app_id {
            Some(id) => broker.take_app_subscriptions(id),
            None => {
                #[cfg(test)]
                {
                    broker.take_current_thread_subscriptions()
                }
                #[cfg(not(test))]
                {
                    broker.take_all_subscriptions()
                }
            }
        }
    };
    for subscription in subscriptions {
        subscription.close();
    }
}

// ---------------------------------------------------------------------------
// JS-facing event JSON
// ---------------------------------------------------------------------------

/// Serialise a [`SubscriptionMessage`] for the V8 bridge.
///
/// The wire shape is:
/// - `{"kind":"change", "op":"insert"|"update"|"delete",
///    "collection":"...", "pk":"usr_..."|null, "columns":[...]}`
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
            "columns": creator_visible_columns(&ev.changed_columns),
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

/// Drop platform-internal column names from a change event's column list.
///
/// A change to a masked field touches two physical columns - the field's own
/// (the mask) and `__zs_raw__<field>` (the real value) - and the second is not
/// a name a creator has ever seen. Only the `__zs_` family is removed, so a
/// system field like `updated_at` still reaches the subscriber.
///
/// The event carries no schema, which is exactly why this is a name test on a
/// prefix the platform owns and `validate_field_name` refuses, rather than a
/// descriptor lookup: the WAL consumer builds these lists in a background task
/// with no isolate and no descriptor in reach.
fn creator_visible_columns(columns: &[String]) -> Vec<String> {
    columns
        .iter()
        .filter(|c| !c.starts_with("__zs_"))
        .cloned()
        .collect()
}

// ---------------------------------------------------------------------------
// WS push-frame format
// ---------------------------------------------------------------------------
//
// Mirrors the shape spelled out in the subscription task contract:
//
// ```json
// { "type": "zs.subscription.event",
//   "handle": "<subscription_id>",
//   "event": { "kind": "insert", "collection": "users", "pk": "usr_42" } }
// ```
//
// The intent is that a WS handler in JS owns a `Map<handle, ws_conn>`
// and pushes the JSON-encoded frame on receipt. The frame is shaped to
// be self-describing — `type` lets the WS handler distinguish broker
// events from app-level WS messages on the same connection.
//
// **There is deliberately no row payload.** `ws_frame_for_change` used to carry
// `ev.new_tuple`, which is every physical column of the row with its value -
// including a masked field's raw column, unmasked, undecrypted and unaudited.
// It had no caller outside this module, and keeping a dead exporter of the raw
// tuple around while the rest of this change removes every other way to reach
// it is how it gets wired up later by someone who reads the function and not
// the design. The frame a subscriber gets names the changed columns; fetching
// the row goes through `find`, which is where the read pipeline runs.

/// Build a WS push frame for a subscription message (Resync / Closed /
/// Change). A `Change` reports its op, collection, pk and creator-visible
/// column names - never values.
pub fn ws_frame(handle: &str, msg: &SubscriptionMessage) -> String {
    let event = match msg {
        SubscriptionMessage::Resync => serde_json::json!({ "kind": "resync" }),
        SubscriptionMessage::Closed => serde_json::json!({ "kind": "closed" }),
        SubscriptionMessage::Change(ev) => serde_json::json!({
            "kind": ev.op.as_str(),
            "collection": ev.collection,
            "pk": ev.pk,
            "columns": creator_visible_columns(&ev.changed_columns),
        }),
    };
    serde_json::json!({
        "type": "zs.subscription.event",
        "handle": handle,
        "event": event,
    })
    .to_string()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(app: &str, col: &str, op: ChangeOp, pk: Option<&str>) -> ChangeEvent {
        ChangeEvent {
            app_id: app.to_string(),
            collection: col.to_string(),
            op,
            pk: pk.map(str::to_string),
            changed_columns: vec![],
            new_tuple: HashMap::new(),
            old_tuple: None,
        }
    }

    fn ev_with_tuple(
        app: &str,
        col: &str,
        op: ChangeOp,
        pk: Option<&str>,
        tuple: &[(&str, &str)],
    ) -> ChangeEvent {
        ChangeEvent {
            app_id: app.to_string(),
            collection: col.to_string(),
            op,
            pk: pk.map(str::to_string),
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
        b.publish(&ev("a", "messages", ChangeOp::Insert, Some("1")));
        let msg = s.pop().expect("expected a message");
        let SubscriptionMessage::Change(c) = msg else {
            panic!("expected Change variant");
        };
        assert_eq!(c.collection, "messages");
        assert_eq!(c.op, ChangeOp::Insert);
        assert_eq!(c.pk.as_deref(), Some("1"));
    }

    #[test]
    fn try_subscribe_prunes_closed_entries_before_minting_a_replacement() {
        let mut broker = Broker::new();
        let attempts = MAX_SUBSCRIPTIONS_PER_APP * 2;
        assert!(attempts > 0, "test must exercise at least one subscription");

        for _ in 0..attempts {
            let subscription = broker
                .try_subscribe("close-loop-app", "users")
                .expect("a closed handle must not consume the live cap");
            subscription.close();
        }

        let stored = broker
            .by_key
            .get("close-loop-app")
            .and_then(|collections| collections.get("users"))
            .map_or(0, Vec::len);
        assert_eq!(
            stored, 1,
            "open-then-close loops must not grow the process-wide routing table"
        );
    }

    #[test]
    fn process_wide_broker_delivers_between_threads() {
        struct WakeFlag(std::sync::atomic::AtomicBool);

        impl std::task::Wake for WakeFlag {
            fn wake(self: Arc<Self>) {
                self.0.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        }

        const APP: &str = "broker_cross_thread_regression_app";
        const COLLECTION: &str = "messages";

        drop_app(Some(APP));
        let (registered_tx, registered_rx) = std::sync::mpsc::channel();
        let (published_tx, published_rx) = std::sync::mpsc::channel();

        let subscriber = std::thread::spawn(move || {
            let sub = subscribe(APP, COLLECTION);
            let wake_flag = Arc::new(WakeFlag(std::sync::atomic::AtomicBool::new(false)));
            sub.register_waker(std::task::Waker::from(Arc::clone(&wake_flag)));
            registered_tx.send(()).expect("signal subscription ready");
            published_rx.recv().expect("wait for cross-thread publish");
            let event_pk = match sub.pop() {
                Some(SubscriptionMessage::Change(event)) => event.pk.clone(),
                _ => None,
            };
            let was_woken = wake_flag.0.load(std::sync::atomic::Ordering::SeqCst);
            (event_pk, was_woken)
        });

        let publisher = std::thread::spawn(move || {
            registered_rx.recv().expect("wait for subscription");
            publish(&ev(
                APP,
                COLLECTION,
                ChangeOp::Insert,
                Some("cross-thread-probe"),
            ));
            published_tx.send(()).expect("signal publish complete");
        });

        publisher.join().expect("publisher thread");
        let (received, was_woken) = subscriber.join().expect("subscriber thread");
        assert_eq!(received.as_deref(), Some("cross-thread-probe"));
        assert!(was_woken, "publisher must wake the subscriber thread");
        drop_app(Some(APP));
    }

    #[test]
    fn subscription_payloads_are_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}

        assert_send_sync::<Subscription>();
        assert_send_sync::<SubscriptionMessage>();
    }

    #[test]
    fn other_app_events_isolated() {
        let mut b = Broker::new();
        let s = b.subscribe("a", "messages");
        b.publish(&ev("b", "messages", ChangeOp::Insert, Some("1")));
        assert!(s.pop().is_none());
    }

    #[test]
    fn other_collection_events_isolated() {
        let mut b = Broker::new();
        let s = b.subscribe("a", "messages");
        b.publish(&ev("a", "channels", ChangeOp::Insert, Some("1")));
        assert!(s.pop().is_none());
    }

    #[test]
    fn multiple_subscribers_receive_event() {
        let mut b = Broker::new();
        let s1 = b.subscribe("a", "messages");
        let s2 = b.subscribe("a", "messages");
        b.publish(&ev("a", "messages", ChangeOp::Update, Some("7")));
        assert!(matches!(s1.pop(), Some(SubscriptionMessage::Change(_))));
        assert!(matches!(s2.pop(), Some(SubscriptionMessage::Change(_))));
    }

    #[test]
    fn delivers_insert_update_delete() {
        let mut b = Broker::new();
        let s = b.subscribe("a", "messages");
        b.publish(&ev("a", "messages", ChangeOp::Insert, Some("1")));
        b.publish(&ev("a", "messages", ChangeOp::Update, Some("1")));
        b.publish(&ev("a", "messages", ChangeOp::Delete, Some("1")));
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
        b.publish(&ev("a", "messages", ChangeOp::Insert, Some("1")));
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
        b.publish(&ev("a", "messages", ChangeOp::Insert, Some("1")));
        assert_eq!(b.subscription_count(), 0);
    }

    #[test]
    fn overflow_collapses_to_resync() {
        // Tiny queue so we can overflow it in the test.
        let s = Subscription::new(1, "a".into(), "m".into(), 2);
        s.push(SubscriptionMessage::Change(Arc::new(ev(
            "a",
            "m",
            ChangeOp::Insert,
            Some("1"),
        ))));
        s.push(SubscriptionMessage::Change(Arc::new(ev(
            "a",
            "m",
            ChangeOp::Insert,
            Some("2"),
        ))));
        s.push(SubscriptionMessage::Change(Arc::new(ev(
            "a",
            "m",
            ChangeOp::Insert,
            Some("3"),
        )))); // overflow
        // Queue now contains a single Resync.
        assert!(matches!(s.pop(), Some(SubscriptionMessage::Resync)));
        assert!(s.pop().is_none());
    }

    #[test]
    fn message_to_json_change_shape() {
        let m = SubscriptionMessage::Change(Arc::new(ChangeEvent {
            app_id: "a".into(),
            collection: "messages".into(),
            op: ChangeOp::Insert,
            pk: Some("7".to_string()),
            changed_columns: vec!["title".into(), "body".into()],
            new_tuple: HashMap::new(),
            old_tuple: None,
        }));
        let v: serde_json::Value = serde_json::from_str(&message_to_json(&m)).unwrap();
        assert_eq!(v["kind"], "change");
        assert_eq!(v["op"], "insert");
        assert_eq!(v["collection"], "messages");
        assert_eq!(v["pk"], "7");
        assert_eq!(v["columns"][0], "title");
    }

    #[test]
    fn message_to_json_resync() {
        let v: serde_json::Value =
            serde_json::from_str(&message_to_json(&SubscriptionMessage::Resync)).unwrap();
        assert_eq!(v["kind"], "resync");
    }

    // ---------- read-set narrowing ----------

    use crate::read_set::{self, ReadSetEntry};

    fn rs_entry(collection: &str, filter: serde_json::Value) -> ReadSetEntry {
        ReadSetEntry {
            collection: collection.to_string(),
            predicate: read_set::normalise_filter(&filter, &serde_json::json!({})),
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
            Some("1"),
            &[("userId", "42"), ("body", "hi")],
        ));
        // Non-matching event → filtered.
        b.publish(&ev_with_tuple(
            "a",
            "messages",
            ChangeOp::Insert,
            Some("2"),
            &[("userId", "99"), ("body", "nope")],
        ));

        match s.pop() {
            Some(SubscriptionMessage::Change(c)) => assert_eq!(c.pk.as_deref(), Some("1")),
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
            pk: Some("1".to_string()),
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
            Some("1"),
            &[("createdAt", "1500")],
        ));
        // Below threshold → filtered.
        b.publish(&ev_with_tuple(
            "a",
            "events",
            ChangeOp::Insert,
            Some("2"),
            &[("createdAt", "500")],
        ));
        match s.pop() {
            Some(SubscriptionMessage::Change(c)) => assert_eq!(c.pk.as_deref(), Some("1")),
            other => panic!("expected pk=\"1\", got {other:?}"),
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
            Some("99"),
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
            pk: Some("1".to_string()),
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
            pk: Some("1".to_string()),
            changed_columns: vec![],
            new_tuple: [("userId".into(), "42".into())].into(),
            old_tuple: None,
        };
        assert!(!s.accepts(&event));
    }

    #[test]
    fn b8b_no_read_set_accepts_every_event() {
        // Subscriptions that don't set a read-set fall back to a
        // coarse-grained collection match.
        let mut b = Broker::new();
        let s = b.subscribe("a", "messages");
        // No set_read_set call.
        b.publish(&ev_with_tuple(
            "a",
            "messages",
            ChangeOp::Insert,
            Some("1"),
            &[("userId", "anything")],
        ));
        assert!(matches!(s.pop(), Some(SubscriptionMessage::Change(_))));
    }

    // ---------- WS push frames ----------

    #[test]
    fn b8b_ws_frame_change_shape() {
        let mut tuple = HashMap::new();
        tuple.insert("userId".into(), "42".into());
        tuple.insert("body".into(), "hi".into());
        let ev = ChangeEvent {
            app_id: "a".into(),
            collection: "messages".into(),
            op: ChangeOp::Insert,
            pk: Some("7".to_string()),
            changed_columns: vec!["userId".into(), "body".into()],
            new_tuple: tuple,
            old_tuple: None,
        };
        let frame = ws_frame("sub_42", &SubscriptionMessage::Change(std::sync::Arc::new(ev.clone())));
        let v: serde_json::Value = serde_json::from_str(&frame).unwrap();
        assert_eq!(v["type"], "zs.subscription.event");
        assert_eq!(v["handle"], "sub_42");
        assert_eq!(v["event"]["kind"], "insert");
        assert_eq!(v["event"]["collection"], "messages");
        assert_eq!(v["event"]["pk"], "7");
        assert_eq!(v["event"]["columns"][0], "userId");
        // The frame names columns and carries NO values. `row` used to be
        // `ev.new_tuple` verbatim - every physical column with its contents.
        assert!(
            v["event"].get("row").is_none(),
            "a change frame must carry no row payload: {frame}",
        );
        assert!(!frame.contains("\"hi\""), "no cell values on the wire: {frame}");
    }

    #[test]
    fn b8b_ws_frame_resync_and_closed() {
        let r = ws_frame("h", &SubscriptionMessage::Resync);
        let c = ws_frame("h", &SubscriptionMessage::Closed);
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
            Some("10"),
            &[("userId", "1"), ("body", "hi alice")],
        ));

        // Drain each subscriber's queue and format as WS frames.
        let alice_frames: Vec<String> = std::iter::from_fn(|| match s_alice.pop() {
            Some(SubscriptionMessage::Change(ev)) => {
                Some(ws_frame("alice", &SubscriptionMessage::Change(ev.clone())))
            }
            _ => None,
        })
        .collect();
        let bob_frames: Vec<String> = std::iter::from_fn(|| match s_bob.pop() {
            Some(SubscriptionMessage::Change(ev)) => {
                Some(ws_frame("bob", &SubscriptionMessage::Change(ev.clone())))
            }
            _ => None,
        })
        .collect();

        assert_eq!(alice_frames.len(), 1, "Alice should receive 1 frame");
        assert_eq!(bob_frames.len(), 0, "Bob should receive 0 frames");
        let f: serde_json::Value = serde_json::from_str(&alice_frames[0]).unwrap();
        assert_eq!(f["handle"], "alice");
        assert_eq!(f["event"]["pk"], "10");
        // The narrowing decision used the tuple; the frame does not carry it.
        assert!(
            !alice_frames[0].contains("hi alice"),
            "a routed frame carries no cell values: {}",
            alice_frames[0],
        );
    }

    #[test]
    fn has_subscribers_lifecycle() {
        // No registration → false.
        let mut b = Broker::new();
        assert!(!b.has_subscribers("a", "messages"));

        // After subscribe → true.
        let s = b.subscribe("a", "messages");
        assert!(b.has_subscribers("a", "messages"));
        // Isolation: other app / other collection still false.
        assert!(!b.has_subscribers("b", "messages"));
        assert!(!b.has_subscribers("a", "channels"));

        // Close the only subscriber → publish prunes the bucket →
        // has_subscribers returns false.
        s.close();
        b.publish(&ev("a", "messages", ChangeOp::Insert, Some("1")));
        assert!(!b.has_subscribers("a", "messages"));
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

    // ---------- resume_app_with_resync ----------

    #[test]
    fn resume_app_with_resync_pushes_one_resync_to_each_subscription() {
        // The pause / schema-pending guards' Drop impls call this
        // primitive after their window closes. Every active
        // subscription on the app should observe a single Resync.
        let mut b = Broker::new();
        let s1 = b.subscribe("a", "messages");
        let s2 = b.subscribe("a", "channels");
        let s3 = b.subscribe("b", "messages"); // other app — unaffected.

        b.resume_app_with_resync("a");

        assert!(matches!(s1.pop(), Some(SubscriptionMessage::Resync)));
        assert!(matches!(s2.pop(), Some(SubscriptionMessage::Resync)));
        // Per-app isolation: app "b" sees nothing.
        assert!(s3.pop().is_none());
    }

    #[test]
    fn resume_app_with_resync_is_idempotent_pushes_multiple_resyncs() {
        // Plan §9: "calling multiple times pushes multiple resyncs;
        // subscribers dedup." Verify the broker side faithfully pushes
        // one Resync per call (consumer-side dedup is a Subscription
        // pop-time concern not exercised here).
        let mut b = Broker::new();
        let s = b.subscribe("a", "messages");
        b.resume_app_with_resync("a");
        b.resume_app_with_resync("a");
        b.resume_app_with_resync("a");
        assert!(matches!(s.pop(), Some(SubscriptionMessage::Resync)));
        assert!(matches!(s.pop(), Some(SubscriptionMessage::Resync)));
        assert!(matches!(s.pop(), Some(SubscriptionMessage::Resync)));
        assert!(s.pop().is_none());
    }

    #[test]
    fn resume_app_with_resync_skips_closed_subscriptions() {
        // `Subscription::push` short-circuits on `inner.closed`, so a
        // closed subscription must NOT observe the Resync. The routing
        // table still contains the closed entry (only `publish` GCs it),
        // but the push path bails out.
        let mut b = Broker::new();
        let s1 = b.subscribe("a", "messages");
        let s2 = b.subscribe("a", "messages");
        s1.close();
        // Drain s1's `Closed` message so the only thing left to observe
        // would be the Resync push (which must be skipped on closed).
        assert!(matches!(s1.pop(), Some(SubscriptionMessage::Closed)));
        b.resume_app_with_resync("a");
        assert!(s1.pop().is_none(), "closed subscription must not receive Resync");
        assert!(matches!(s2.pop(), Some(SubscriptionMessage::Resync)));
    }

    #[test]
    fn resume_app_with_resync_no_op_when_app_unknown() {
        // Apps with no active subscriptions are a fast no-op. The
        // routing table lookup misses and the function returns
        // immediately — no panic, no allocation.
        let mut b = Broker::new();
        let _s = b.subscribe("a", "messages");
        // App "missing" has no entry — must not panic.
        b.resume_app_with_resync("missing");
        // The unrelated app's subscription is untouched.
        let s = _s;
        assert!(s.pop().is_none());
    }

    // ---------- has_subscribers gate (alloc-free WAL fan-out probe) ----------

    #[test]
    fn has_subscribers_false_when_app_unknown() {
        let mut b = Broker::new();
        // Subscribe on a different app so the broker is non-empty —
        // we want to assert the negative result is NOT "broker is empty".
        let _s = b.subscribe("other_app", "messages");
        assert!(!b.has_subscribers("missing_app", "messages"));
    }

    #[test]
    fn has_subscribers_false_when_collection_unknown() {
        let mut b = Broker::new();
        let _s = b.subscribe("a", "messages");
        // Same app, different collection — must not bleed across.
        assert!(!b.has_subscribers("a", "channels"));
    }

    #[test]
    fn has_subscribers_true_for_registered_pair() {
        let mut b = Broker::new();
        let _s = b.subscribe("a", "messages");
        assert!(b.has_subscribers("a", "messages"));
    }

    #[test]
    fn has_subscribers_false_after_publish_prunes_closed() {
        // close() doesn't prune the bucket itself; only publish does.
        // The probe is allowed to return `true` between close() and the
        // next publish (documented behaviour — safe over-approximation).
        // After publish drains the closed entry the bucket is removed
        // and the probe must return false.
        let mut b = Broker::new();
        let s = b.subscribe("a", "messages");
        assert!(b.has_subscribers("a", "messages"));
        s.close();
        b.publish(&ev("a", "messages", ChangeOp::Insert, Some("1")));
        assert!(!b.has_subscribers("a", "messages"));
        // The per-app bucket was emptied — probing other collections on
        // the same app also returns false (no stale inner HashMap).
        assert!(!b.has_subscribers("a", "channels"));
    }

    #[test]
    fn has_subscribers_takes_str_no_string_alloc_at_call_site() {
        // Compile-time check: the API accepts `&str` arguments so
        // callers on the WAL fan-out path can probe without
        // constructing owned `String`s. If this signature ever
        // regresses to `&String` or `String` the test will fail to
        // compile.
        let mut b = Broker::new();
        let _s = b.subscribe("a", "messages");
        let app: &str = "a";
        let collection: &str = "messages";
        assert!(b.has_subscribers(app, collection));
        // Also exercise lookup with literal `&'static str`s through
        // the same borrowed-key shape used by the global accessor.
        assert!(b.has_subscribers("a", "messages"));
    }

    #[test]
    fn has_subscribers_isolated_across_apps_and_collections() {
        let mut b = Broker::new();
        let _s1 = b.subscribe("app_a", "messages");
        let _s2 = b.subscribe("app_b", "channels");
        assert!(b.has_subscribers("app_a", "messages"));
        assert!(b.has_subscribers("app_b", "channels"));
        // Cross-product entries do not exist.
        assert!(!b.has_subscribers("app_a", "channels"));
        assert!(!b.has_subscribers("app_b", "messages"));
        assert!(!b.has_subscribers("app_c", "anything"));
    }

    #[test]
    fn publish_drops_per_app_map_when_last_collection_empties() {
        // Whitebox-ish: confirms the per-app inner HashMap is dropped
        // once it has no live collections — keeps `has_subscribers` /
        // `publish` cheap on apps that churn ephemeral collections.
        let mut b = Broker::new();
        let s = b.subscribe("a", "messages");
        assert_eq!(b.by_key.len(), 1);
        s.close();
        b.publish(&ev("a", "messages", ChangeOp::Insert, Some("1")));
        // Per-app entry collapsed away — no leaked inner HashMap.
        assert_eq!(b.by_key.len(), 0);
    }

    // -----------------------------------------------------------------
    // Schema-pending decoder window unit tests.
    // -----------------------------------------------------------------

    /// Guard that engages schema-pending on construction + clears on
    /// Drop, even on panic-unwind. Tests use this so a panicked
    /// assertion doesn't leak the thread-local into the next test on
    /// the same thread (cargo test runs each `#[test]` on its own
    /// thread by default, but the safety belt costs nothing).
    struct SchemaPendingTestGuard {
        app_id: String,
    }

    impl SchemaPendingTestGuard {
        fn engage(app_id: &str) -> Self {
            engage_schema_pending(app_id);
            Self {
                app_id: app_id.to_string(),
            }
        }
    }

    impl Drop for SchemaPendingTestGuard {
        fn drop(&mut self) {
            disengage_schema_pending(&self.app_id);
        }
    }

    #[test]
    fn engage_then_is_schema_pending_returns_true() {
        let _g = SchemaPendingTestGuard::engage("test_app_engage");
        assert!(is_schema_pending("test_app_engage"));
        assert!(!is_schema_pending("other_app"));
    }

    #[test]
    fn disengage_schema_pending_clears_flag() {
        engage_schema_pending("test_app_disengage");
        assert!(is_schema_pending("test_app_disengage"));
        disengage_schema_pending("test_app_disengage");
        assert!(!is_schema_pending("test_app_disengage"));
    }

    #[test]
    fn disengage_schema_pending_is_idempotent_on_unengaged_app() {
        // Calling disengage on an app never engaged is a no-op — the
        // HashSet::remove returns false, with no panic.
        disengage_schema_pending("never_engaged_app");
        assert!(!is_schema_pending("never_engaged_app"));
    }

    #[test]
    fn try_subscribe_succeeds_when_not_schema_pending() {
        let mut b = Broker::new();
        let result = b.try_subscribe("happy_app", "messages");
        assert!(
            result.is_ok(),
            "try_subscribe should succeed when not schema_pending; got {result:?}"
        );
        let sub = result.unwrap();
        assert_eq!(sub.app_id(), "happy_app");
        assert_eq!(sub.collection(), "messages");
    }

    #[test]
    fn try_subscribe_rejects_with_schema_pending_code_when_engaged() {
        let _g = SchemaPendingTestGuard::engage("pending_app");
        let mut b = Broker::new();
        let result = b.try_subscribe("pending_app", "messages");
        match result {
            Err(DbError::Coded { code, .. }) => {
                assert_eq!(
                    code, "schema_pending",
                    "try_subscribe must reject with code=schema_pending; got code={code}"
                );
            }
            other => panic!("expected Err(Coded {{ code: schema_pending }}); got {other:?}"),
        }
    }

    #[test]
    fn try_subscribe_carries_hint_in_rejection() {
        let _g = SchemaPendingTestGuard::engage("hint_app");
        let mut b = Broker::new();
        let result = b.try_subscribe("hint_app", "messages");
        match result {
            Err(DbError::Coded { hint, .. }) => {
                assert!(
                    hint.is_some(),
                    "schema_pending rejection must carry a non-empty hint"
                );
            }
            other => panic!("expected Err(Coded); got {other:?}"),
        }
    }

    #[test]
    fn try_subscribe_only_blocks_the_engaged_app() {
        let _g = SchemaPendingTestGuard::engage("blocked_app");
        let mut b = Broker::new();
        // Sibling app is unaffected.
        assert!(b.try_subscribe("sibling_app", "messages").is_ok());
        // Engaged app is rejected.
        assert!(matches!(
            b.try_subscribe("blocked_app", "messages"),
            Err(DbError::Coded { .. })
        ));
    }

    // -----------------------------------------------------------------
    // `cdc_event_carries_masked_value_for_masked_columns` (§11)
    //
    // The proposal (Q-MASK-G) asserts that CDC subscribers see the
    // MASKED representation of a masked column — never the plaintext.
    // With sibling-column storage (Path B, docs/archive/sensitive-field-masking.md)
    // the WAL pipeline never
    // decrypts: `tuple_to_map` (in `wal_consumer.rs`) zips pgoutput
    // tuple bytes verbatim into `new_tuple`, so the parent column
    // carries ciphertext text-encoded by PG (e.g. `\xDEADBEEF` for
    // BYTEA) and the sibling carries the pre-computed mask string.
    //
    // The gate below pins the contract by constructing a synthetic
    // `ChangeEvent` mirroring the wal_consumer's output (parent =
    // ciphertext text-encoding, sibling = masked text) and asserts
    // both that the WS frame round-trips both columns AND that the
    // parent column NEVER carries an obvious plaintext "ssn-shape"
    // string. A regression that wires decrypt-on-CDC would land the
    // plaintext in `new_tuple["ssn"]` and flip this assertion.
    // -----------------------------------------------------------------

    /// The CDC tuple after the storage flip, and what a subscriber may see of
    /// it.
    ///
    /// The tuple carries the mask under the field's own name and the real value
    /// (here, ciphertext) under `__zs_raw__ssn`. The two published surfaces -
    /// `message_to_json` and `ws_frame` - must name only creator-visible
    /// columns and must carry no values at all: the raw column's NAME is
    /// internal and its VALUE has exactly one authorized reader.
    #[test]
    fn a_change_event_publishes_no_values_and_no_raw_column_name() {
        // Synthetic encrypted-column ciphertext: PG renders BYTEA
        // as `\xHHHHHH...` in the text protocol.
        let raw_ciphertext_text = "\\x0123456789abcdef0123456789abcdef";
        let masked_text = "***-**-6789";
        let plaintext = "123-45-6789";
        let raw_col = crate::query::raw_column_name("ssn");

        let mut tuple = HashMap::new();
        tuple.insert("id".into(), "42".into());
        tuple.insert("ssn".into(), masked_text.into());
        tuple.insert(raw_col.clone(), raw_ciphertext_text.into());

        let ev = ChangeEvent {
            app_id: "app".into(),
            collection: "users".into(),
            op: ChangeOp::Insert,
            pk: Some("42".to_string()),
            changed_columns: vec!["id".into(), "ssn".into(), raw_col.clone()],
            new_tuple: tuple,
            old_tuple: None,
        };

        // The field's own column holds the mask; the raw column holds the
        // stored (ciphertext) value. Neither is the plaintext.
        assert_eq!(ev.new_tuple.get("ssn").map(String::as_str), Some(masked_text));
        assert_eq!(
            ev.new_tuple.get(&raw_col).map(String::as_str),
            Some(raw_ciphertext_text),
        );
        assert!(
            !ev.new_tuple.values().any(|v| v == plaintext),
            "no value in the CDC tuple may equal the plaintext: {:?}",
            ev.new_tuple,
        );

        for published in [
            message_to_json(&SubscriptionMessage::Change(std::sync::Arc::new(ev.clone()))),
            ws_frame("sub_x", &SubscriptionMessage::Change(std::sync::Arc::new(ev.clone()))),
        ] {
            assert!(
                !published.contains(&raw_col),
                "a published event must not name the raw column: {published}",
            );
            assert!(
                !published.contains(raw_ciphertext_text) && !published.contains(masked_text),
                "a published event carries column NAMES, never values: {published}",
            );
            assert!(
                published.contains("\"ssn\""),
                "the masked field's own name is creator-visible and must survive: {published}",
            );
        }
    }
}
