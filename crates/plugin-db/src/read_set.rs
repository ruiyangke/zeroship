//! Read-set capture + predicate evaluation — Phase 8b (P8b).
//!
//! P8a + P8a.2 shipped the broker and the cross-worker WAL consumer
//! with **coarse-grained** delivery: every subscription on
//! `(app_id, collection)` receives every change to that collection.
//! P8b narrows the delivery to "only events whose row actually matches
//! the subscription's filter".
//!
//! ## What a read-set is
//!
//! When a `query()` handler runs `ctx.db.find({ userId: 42 })` against
//! `messages`, the runtime records a [`ReadSetEntry`]:
//!
//! ```text
//! ReadSetEntry {
//!     collection: "messages",
//!     predicate: Some(Predicate::All(vec![
//!         Conjunct { column: "userId", op: Eq, value: Number(42) },
//!     ])),
//! }
//! ```
//!
//! That entry attaches to the [`Subscription`](crate::broker::Subscription)
//! the handler opens. On the next WAL event the broker evaluates the
//! event's row against every subscriber's predicate; non-matching
//! subscribers are skipped.
//!
//! `predicate == None` (a missing or unsupported filter shape) means
//! "match every row in the collection" — equivalent to the coarse-grained
//! P8a behaviour. Subscribers that use `$or`, `$nor`, JSON path operators,
//! or any other shape we don't normalise fall back to this safe default.
//!
//! ## Capture site
//!
//! Capture happens inside `ctx.db.find / findOne / count / aggregate`.
//! The B3 capability layer's thread-local
//! [`current_kind`](zeroship_runtime::rpc::current_kind) marker gates
//! it: read-set capture is enabled **only** when the active procedure
//! kind is `Query`. Mutations and actions never participate.
//!
//! ## What this module does NOT do
//!
//! - It doesn't open a subscription. The capture buffer is a passive
//!   thread-local; the layer that opens subscriptions
//!   (`@zeroship/db`'s reactive-query helper) snapshots it into a
//!   [`Subscription`] when the handler returns.
//! - It doesn't talk to V8. All the V8-facing surface lives in
//!   `callbacks.rs` (capture sites: dispatch_find / dispatch_find_one /
//!   dispatch_count) and the broker's v8_class wrapper.

use std::cell::RefCell;
use std::collections::HashMap;

use serde_json::Value;

// ---------------------------------------------------------------------------
// NormalizedFilter shape
// ---------------------------------------------------------------------------

/// One comparison inside a predicate conjunction.
///
/// Equality is the most common shape (`{userId: 42}`); range comparisons
/// (`$gt` / `$gte` / `$lt` / `$lte`) cover time-window filters. Anything
/// else (set membership, JSON-path operators, regex) collapses the
/// whole filter to coarse-grained — we'd rather over-deliver than
/// silently drop events the subscriber should see.
#[derive(Debug, Clone, PartialEq)]
pub enum PredicateOp {
    Eq,
    Gt,
    Gte,
    Lt,
    Lte,
}

/// One conjunct in a [`Predicate::All`] — `column <op> value`.
#[derive(Debug, Clone, PartialEq)]
pub struct Conjunct {
    pub column: String,
    pub op: PredicateOp,
    /// Serialised value. The WAL consumer emits column values as text
    /// (pgoutput proto v1 default), so we compare against the JSON
    /// scalar's canonical text form. See [`Conjunct::matches_text`].
    pub value: Value,
}

/// A normalised filter expression.
///
/// `All(vec![])` is "no predicates" — equivalent to matching every row.
/// Callers should prefer `None` at the [`ReadSetEntry::predicate`]
/// level for the same semantics; `All(vec![])` exists so a future
/// normaliser can distinguish "explicitly empty `{}`" from "couldn't
/// parse".
#[derive(Debug, Clone, PartialEq)]
pub enum Predicate {
    /// AND of zero or more conjuncts. Empty list = always true.
    All(Vec<Conjunct>),
}

impl Predicate {
    /// Evaluate this predicate against a row's column→text map.
    ///
    /// The pgoutput stream gives us text representations of every
    /// column; we render the JSON value to canonical text and string-
    /// compare for `Eq`, parse both sides as `f64` for range ops. This
    /// matches Postgres's loose comparison semantics for the small set
    /// of shapes we support and avoids dragging type-coercion rules
    /// into the broker hot path.
    pub fn matches(&self, row: &HashMap<String, String>) -> bool {
        let Predicate::All(conjuncts) = self;
        if conjuncts.is_empty() {
            return true;
        }
        conjuncts.iter().all(|c| c.matches_text(row))
    }
}

impl Conjunct {
    fn matches_text(&self, row: &HashMap<String, String>) -> bool {
        let Some(actual) = row.get(&self.column) else {
            // Column absent from the event tuple (TOASTed, or relation
            // schema missing it). Conservative: treat as non-match for
            // Eq, non-match for range — the subscriber won't see this
            // event but it'll see the next one that DOES carry the
            // column. Cheaper than coarse-grained fallback per-event.
            return false;
        };
        match self.op {
            PredicateOp::Eq => value_to_text(&self.value) == *actual,
            PredicateOp::Gt | PredicateOp::Gte | PredicateOp::Lt | PredicateOp::Lte => {
                let (Some(a), Some(b)) = (actual.parse::<f64>().ok(), value_to_f64(&self.value))
                else {
                    return false;
                };
                match self.op {
                    PredicateOp::Gt => a > b,
                    PredicateOp::Gte => a >= b,
                    PredicateOp::Lt => a < b,
                    PredicateOp::Lte => a <= b,
                    PredicateOp::Eq => unreachable!(),
                }
            }
        }
    }
}

fn value_to_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Null => "NULL".to_string(),
        other => other.to_string(),
    }
}

fn value_to_f64(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.parse::<f64>().ok(),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// ReadSetEntry
// ---------------------------------------------------------------------------

/// One `(collection, normalised filter)` pair captured during a query
/// handler's execution.
///
/// A handler may produce multiple entries — e.g. a query that joins
/// `messages` with `users` would emit two. The subscription matches an
/// event if ANY entry matches (i.e. union of collections).
#[derive(Debug, Clone, PartialEq)]
pub struct ReadSetEntry {
    pub collection: String,
    /// `None` → coarse-grained (every row in `collection` matches).
    /// `Some(p)` → only rows for which `p` evaluates to true match.
    pub predicate: Option<Predicate>,
}

impl ReadSetEntry {
    /// True if this entry's filter accepts an event row.
    ///
    /// Hot path: called once per (subscriber, WAL event) pair on the
    /// broker. The `None` branch is the most common in the conservative
    /// fallback path so it short-circuits first.
    pub fn matches(&self, row: &HashMap<String, String>) -> bool {
        match &self.predicate {
            None => true,
            Some(p) => p.matches(row),
        }
    }
}

// ---------------------------------------------------------------------------
// Normaliser — MongoDB-style filter → Predicate
// ---------------------------------------------------------------------------

/// Convert a filter JSON into a normalised predicate.
///
/// Returns `None` for any shape we can't decompose cleanly:
/// - `$or`, `$nor`, `$not` at any depth → coarse-grained.
/// - `$in` / `$nin` with anything other than a small set of scalars.
/// - JSON-path operators.
///
/// Inputs we DO handle:
/// - `{}` → empty conjunction (always true).
/// - `{userId: 42}` → `userId = 42`.
/// - `{userId: 42, status: "active"}` → AND of two conjuncts.
/// - `{userId: {$eq: 42}}` → same as the bare form.
/// - `{createdAt: {$gt: 1000, $lte: 2000}}` → two conjuncts on same column.
///
/// Conservative bias: false negatives (returning `None` for a shape we
/// could in principle decompose) cost a wider fanout but never miss
/// events; false positives (matching when we shouldn't) would silently
/// drop events. The latter is unacceptable so we round trip every
/// supported shape through the unit tests in this module.
pub fn normalise_filter(filter: &Value) -> Option<Predicate> {
    let Value::Object(map) = filter else {
        // null / array / scalar / etc. — caller policy: treat as
        // coarse-grained. The query builder rejects these too, so
        // hitting this branch in practice means we already failed
        // upstream. Be conservative regardless.
        return None;
    };
    if map.is_empty() {
        return Some(Predicate::All(vec![]));
    }

    let mut conjuncts: Vec<Conjunct> = Vec::with_capacity(map.len());
    for (key, value) in map {
        if key.starts_with('$') {
            // Top-level operator. P8b doesn't decompose these. $and on
            // a flat list of bare conjuncts could in principle be
            // unrolled, but it's a rare shape from real apps; fall
            // back coarse-grained to keep the code small.
            return None;
        }
        match value {
            // Bare scalar: { col: scalar } → Eq.
            Value::String(_) | Value::Number(_) | Value::Bool(_) => {
                conjuncts.push(Conjunct {
                    column: key.clone(),
                    op: PredicateOp::Eq,
                    value: value.clone(),
                });
            }
            // Null on the lhs of equality is `IS NULL` in SQL — we
            // don't track NULL events through the broker's
            // text-encoded tuple, so collapse to coarse-grained.
            Value::Null => return None,
            // { col: { $op: val, ... } } — operator object.
            Value::Object(ops) => {
                for (op, val) in ops {
                    let predicate_op = match op.as_str() {
                        "$eq" => PredicateOp::Eq,
                        "$gt" => PredicateOp::Gt,
                        "$gte" => PredicateOp::Gte,
                        "$lt" => PredicateOp::Lt,
                        "$lte" => PredicateOp::Lte,
                        // $in / $nin / $exists / $like / regex / etc.
                        _ => return None,
                    };
                    if !matches!(
                        val,
                        Value::String(_) | Value::Number(_) | Value::Bool(_)
                    ) {
                        return None;
                    }
                    conjuncts.push(Conjunct {
                        column: key.clone(),
                        op: predicate_op,
                        value: val.clone(),
                    });
                }
            }
            // Arrays as rhs are typically the $in shape but bare —
            // unsupported.
            Value::Array(_) => return None,
        }
    }
    Some(Predicate::All(conjuncts))
}

// ---------------------------------------------------------------------------
// Thread-local capture buffer
// ---------------------------------------------------------------------------
//
// The capture buffer is keyed by the active procedure kind: capture is
// enabled only when an [`Active`] scope guard is in scope (set up by
// the runtime's query-dispatch entry path). The API is split so:
//
// 1. find / findOne / count / aggregate callbacks can call
//    [`record_if_active`] without checking themselves.
// 2. Tests and the SDK layer can set the active scope explicitly.

thread_local! {
    /// `Some(buffer)` when read-set capture is active on this thread.
    /// `None` outside any query handler (the default) — `record_if_active`
    /// is a no-op then.
    static CURRENT_BUFFER: RefCell<Option<Vec<ReadSetEntry>>> = const { RefCell::new(None) };
}

/// RAII guard that activates read-set capture on construction and
/// returns the captured buffer on drop via [`Active::take`].
///
/// Construction is intentionally side-effecting: if a guard already
/// exists on this thread we panic — nested query handlers don't make
/// sense in the current dispatch model (every procedure runs to
/// completion before another starts on the same isolate). The panic is
/// a loud failure mode for the (currently impossible) case so future
/// refactors that introduce nesting must explicitly engage with the
/// capture-merge semantics.
#[derive(Debug)]
pub struct Active {
    _no_send: std::marker::PhantomData<*const ()>,
}

impl Active {
    /// Begin capturing. Subsequent [`record_if_active`] calls append to
    /// this guard's buffer until [`Active::take`] or drop.
    pub fn begin() -> Self {
        CURRENT_BUFFER.with(|c| {
            let mut slot = c.borrow_mut();
            assert!(
                slot.is_none(),
                "read-set capture already active on this thread"
            );
            *slot = Some(Vec::new());
        });
        Self {
            _no_send: std::marker::PhantomData,
        }
    }

    /// End capture and return the buffer. After this the guard is
    /// inert; dropping it is a no-op.
    #[must_use]
    pub fn take(self) -> Vec<ReadSetEntry> {
        CURRENT_BUFFER.with(|c| c.borrow_mut().take()).unwrap_or_default()
    }
}

impl Drop for Active {
    fn drop(&mut self) {
        // Defensive: if `take` wasn't called, the buffer would leak
        // into the next handler. Clear it.
        CURRENT_BUFFER.with(|c| {
            let _ = c.borrow_mut().take();
        });
    }
}

/// True if a read-set capture is active on this thread.
pub fn is_active() -> bool {
    CURRENT_BUFFER.with(|c| c.borrow().is_some())
}

/// If a capture is active AND the active procedure kind is `Query`,
/// append an entry built from `(collection, filter)`. No-op otherwise.
///
/// The capability-kind gate is the contract from the spec: read-set
/// capture is a `query()`-only feature. Inside a `mutation` or
/// `action` the call is silently dropped — those callbacks read from
/// the DB freely but their reads should never invalidate any
/// subscription.
pub fn record_if_active(collection: &str, filter: &Value) {
    if !is_active() {
        return;
    }
    if !matches!(
        zeroship_runtime::rpc::current_kind(),
        Some(zeroship_runtime::rpc::ProcedureKind::Query)
    ) {
        return;
    }
    let entry = ReadSetEntry {
        collection: collection.to_string(),
        predicate: normalise_filter(filter),
    };
    CURRENT_BUFFER.with(|c| {
        if let Some(buf) = c.borrow_mut().as_mut() {
            buf.push(entry);
        }
    });
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn row(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    // -------- normalise_filter --------

    #[test]
    fn normalise_empty_filter_is_always_true() {
        let p = normalise_filter(&json!({})).expect("empty filter normalises");
        assert!(p.matches(&row(&[])));
    }

    #[test]
    fn normalise_bare_equality() {
        let p = normalise_filter(&json!({ "userId": 42 })).expect("scalar normalises");
        assert!(p.matches(&row(&[("userId", "42")])));
        assert!(!p.matches(&row(&[("userId", "99")])));
    }

    #[test]
    fn normalise_multi_field_and() {
        let p = normalise_filter(&json!({ "userId": 42, "status": "active" }))
            .expect("conjunction normalises");
        assert!(p.matches(&row(&[("userId", "42"), ("status", "active")])));
        assert!(!p.matches(&row(&[("userId", "42"), ("status", "archived")])));
        assert!(!p.matches(&row(&[("userId", "1"), ("status", "active")])));
    }

    #[test]
    fn normalise_explicit_eq_operator() {
        let p = normalise_filter(&json!({ "userId": { "$eq": 42 } })).expect("op normalises");
        assert!(p.matches(&row(&[("userId", "42")])));
    }

    #[test]
    fn normalise_gt_range_query() {
        let p = normalise_filter(&json!({ "createdAt": { "$gt": 1000 } })).unwrap();
        assert!(p.matches(&row(&[("createdAt", "1500")])));
        assert!(!p.matches(&row(&[("createdAt", "500")])));
        assert!(!p.matches(&row(&[("createdAt", "1000")])));
    }

    #[test]
    fn normalise_range_combination() {
        let p =
            normalise_filter(&json!({ "createdAt": { "$gte": 1000, "$lt": 2000 } })).unwrap();
        assert!(p.matches(&row(&[("createdAt", "1000")])));
        assert!(p.matches(&row(&[("createdAt", "1500")])));
        assert!(!p.matches(&row(&[("createdAt", "2000")])));
        assert!(!p.matches(&row(&[("createdAt", "999")])));
    }

    #[test]
    fn normalise_or_falls_back_coarse() {
        let p = normalise_filter(&json!({ "$or": [ { "a": 1 }, { "b": 2 } ] }));
        assert!(p.is_none(), "$or should collapse to coarse-grained");
    }

    #[test]
    fn normalise_in_falls_back_coarse() {
        let p = normalise_filter(&json!({ "userId": { "$in": [1, 2, 3] } }));
        assert!(p.is_none(), "$in should collapse to coarse-grained");
    }

    #[test]
    fn normalise_like_falls_back_coarse() {
        let p = normalise_filter(&json!({ "name": { "$like": "j%" } }));
        assert!(p.is_none(), "$like should collapse to coarse-grained");
    }

    #[test]
    fn normalise_null_value_falls_back_coarse() {
        // Null/IS NULL semantics need a NULL-aware tuple representation;
        // the WAL consumer's text-encoded tuple can't distinguish "absent"
        // from "explicit NULL" so we conservatively bail out.
        let p = normalise_filter(&json!({ "deletedAt": null }));
        assert!(p.is_none());
    }

    #[test]
    fn normalise_array_filter_falls_back_coarse() {
        let p = normalise_filter(&json!({ "tags": ["x", "y"] }));
        assert!(p.is_none());
    }

    // -------- Predicate::matches --------

    #[test]
    fn predicate_with_missing_column_does_not_match() {
        let p = Predicate::All(vec![Conjunct {
            column: "userId".into(),
            op: PredicateOp::Eq,
            value: json!(42),
        }]);
        assert!(!p.matches(&row(&[("otherCol", "42")])));
    }

    #[test]
    fn predicate_bool_eq() {
        let p = normalise_filter(&json!({ "active": true })).unwrap();
        assert!(p.matches(&row(&[("active", "true")])));
        assert!(!p.matches(&row(&[("active", "false")])));
    }

    // -------- ReadSetEntry --------

    #[test]
    fn read_set_entry_coarse_matches_everything() {
        let entry = ReadSetEntry {
            collection: "messages".into(),
            predicate: None,
        };
        assert!(entry.matches(&row(&[("userId", "99")])));
        assert!(entry.matches(&row(&[])));
    }

    #[test]
    fn read_set_entry_filters_by_predicate() {
        let entry = ReadSetEntry {
            collection: "messages".into(),
            predicate: normalise_filter(&json!({ "userId": 42 })),
        };
        assert!(entry.matches(&row(&[("userId", "42")])));
        assert!(!entry.matches(&row(&[("userId", "99")])));
    }

    // -------- Active guard / record_if_active --------
    //
    // These tests intentionally do NOT set the procedure kind — they
    // verify the guard mechanics. The kind gate is exercised by the
    // higher-level callback tests (and indirectly by the
    // capture_in_query_kind_only smoke test below).

    #[test]
    fn record_no_op_when_inactive() {
        // No `Active::begin` — record should be a no-op.
        record_if_active("messages", &json!({ "userId": 42 }));
        assert!(!is_active());
    }

    #[test]
    fn record_no_op_outside_query_kind() {
        let guard = Active::begin();
        // No kind set on the thread — record_if_active should skip.
        record_if_active("messages", &json!({ "userId": 42 }));
        let entries = guard.take();
        assert!(entries.is_empty());
    }

    #[test]
    fn capture_in_query_kind_only() {
        use zeroship_runtime::rpc::{KindGuard, ProcedureKind};
        let _kg = KindGuard::enter(ProcedureKind::Query);
        let guard = Active::begin();
        record_if_active("messages", &json!({ "userId": 42 }));
        record_if_active("messages", &json!({}));
        let entries = guard.take();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].collection, "messages");
        assert!(entries[0].predicate.is_some());
        assert_eq!(entries[1].predicate, Some(Predicate::All(vec![])));
    }

    #[test]
    fn capture_skipped_in_mutation_kind() {
        use zeroship_runtime::rpc::{KindGuard, ProcedureKind};
        let _kg = KindGuard::enter(ProcedureKind::Mutation);
        let guard = Active::begin();
        record_if_active("messages", &json!({ "userId": 42 }));
        let entries = guard.take();
        assert!(entries.is_empty(), "mutations must not record read-set");
    }

    #[test]
    fn drop_clears_buffer_even_without_take() {
        {
            let _g = Active::begin();
            assert!(is_active());
        }
        assert!(!is_active(), "drop must clear the buffer");
    }
}
