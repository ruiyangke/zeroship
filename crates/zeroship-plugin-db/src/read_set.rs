//! Read-set capture + predicate evaluation.
//!
//! # NOTHING BELOW RUNS TODAY. Measured 2026-09-02.
//!
//! Everything after this block describes the feature as designed, in the
//! present tense, and it is worth reading that way - but the narrowing does not
//! happen in any shipped path, and delivery is coarse-grained. **BOTH ends are
//! disconnected, and each was checked separately because "inert on both ends"
//! is two claims:**
//!
//! * **Producer.** [`record_if_active`] has exactly one call site,
//!   `crud/mod.rs:242`, and it is real production code - which is why a caller
//!   grep alone reads as "live". But its first line is `if !is_active()`, and
//!   `is_active()` is true only inside an [`Active`] capture. **`Active::begin`
//!   has no caller anywhere outside this module**, so `CURRENT_BUFFER` is
//!   always `None` and the function returns immediately. No entry is ever
//!   recorded.
//! * **Consumer.** `broker::Subscription::set_read_set` has ten call sites and
//!   every one is inside `broker.rs`'s own `#[cfg(test)]` module (it begins at
//!   line 1106; the calls run 1374-1612). So `Subscription::read_set` is `None`
//!   on every production subscription, and `Subscription::accepts` opens with
//!   `let Some(rs) = ... else { return true }` - it accepts every event.
//!
//! **The consequence is a delivery-semantics fact, not just dead code.** Every
//! subscriber on `(app_id, collection)` receives every change to that
//! collection, including rows its filter excludes. That is the coarse-grained
//! default the paragraph below says this module removes.
//!
//! Wiring it is two connections, not a rewrite: open a capture around the
//! `query()` handler dispatch (adapter-side - #117 moved the kind gate INTO
//! the capture precisely so this module need not ask), and hand
//! [`Active::take`]'s entries to the subscription the handler opened. That is
//! a change to what subscribers receive, so it is a decision, not a cleanup.
//!
//! Without a read-set, the broker and the cross-worker WAL consumer
//! deliver **coarse-grained**: every subscription on
//! `(app_id, collection)` receives every change to that collection.
//! This module narrows the delivery to "only events whose row actually
//! matches the subscription's filter".
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
//! default. Subscribers that use `$or`, `$nor`, JSON path operators,
//! or any other shape we don't normalise fall back to this safe default.
//!
//! ## Capture site
//!
//! Capture happens inside `ctx.db.find / count / aggregate`, and is enabled
//! **only** when the active procedure kind is `Query`. Mutations and actions
//! never participate.
//!
//! **The kind is resolved by whoever opens the scope, not read here.** This
//! module consulted the B3 capability layer's thread-local `current_kind`
//! marker on every record until 2026-09-02, which made a data-plane module
//! depend on the V8 runtime crate for a fact its own installer already had.
//! `Active::begin` now takes the answer as a boolean; see `Capture::recording`.
//!
//! ## What this module does NOT do
//!
//! - It doesn't open a subscription. The capture buffer is a passive
//!   thread-local; the layer that opens subscriptions
//!   (`@zeroship/db`'s reactive-query helper) snapshots it into a
//!   `Subscription` when the handler returns.
//! - It doesn't talk to V8. All the V8-facing surface lives in
//!   `crud.rs` (capture sites: dispatch_find / dispatch_count) and
//!   the broker's v8_class wrapper.

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
    /// scalar's canonical text form. See `Conjunct::matches_text`.
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
///
/// # Masked columns
///
/// `schema` is the collection's descriptor entry, and it is a `&Value` rather
/// than an `Option` so "no schema" is unspellable: a caller without one cannot
/// build a predicate at all, rather than building one that silently compares
/// against the wrong thing.
///
/// The WAL tuple carries what the row physically holds, and for a masked field
/// that is the MASK under the field's own name. So a conjunct on a masked
/// column is lowered rather than compared as written:
///
/// - `Eq` → the operand is masked with the column's own mask kind, and the
///   comparison becomes mask-against-mask. That holds whenever the underlying
///   values were equal. It also holds for values that merely share a mask,
///   which is a false positive - a wider fanout, the side this module already
///   declares acceptable.
/// - a range (`$gt` / `$gte` / `$lt` / `$lte`) → `None`, coarse-grained. A
///   range over a mask is not a range over the value and no rewriting makes it
///   one. Coarse-grained wakes the subscription on every change to the
///   collection: correct and slow, which is the declared bias.
///
/// Without this, `find({ssn: "123-45-6789"})` would compare a plaintext operand
/// against a stored mask, never match, and the subscription would stop firing
/// with no error anywhere - the failure mode this module's own contract calls
/// unacceptable.
pub fn normalise_filter(filter: &Value, schema: &Value) -> Option<Predicate> {
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
            // Top-level operator. This does not decompose these. $and on
            // a flat list of bare conjuncts could in principle be
            // unrolled, but it's a rare shape from real apps; fall
            // back coarse-grained to keep the code small.
            return None;
        }
        let mask_kind = mask_kind_for_column(schema, key);
        match value {
            // Bare scalar: { col: scalar } → Eq.
            Value::String(_) | Value::Number(_) | Value::Bool(_) => {
                conjuncts.push(Conjunct {
                    column: key.clone(),
                    op: PredicateOp::Eq,
                    value: lower_operand(mask_kind, value),
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
                    // A range over a mask is not a range over the value.
                    if mask_kind.is_some() && predicate_op != PredicateOp::Eq {
                        return None;
                    }
                    conjuncts.push(Conjunct {
                        column: key.clone(),
                        op: predicate_op,
                        value: lower_operand(mask_kind, val),
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

/// The mask kind declared for `column`, or `None` when it is unmasked or opted
/// out with `kind: "none"`.
fn mask_kind_for_column(schema: &Value, column: &str) -> Option<crate::diff::MaskKind> {
    let kind = schema
        .as_object()?
        .get(column)?
        .get("mask")?
        .as_object()?
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or("full");
    match kind {
        "full" => Some(crate::diff::MaskKind::Full),
        "last4" => Some(crate::diff::MaskKind::Last4),
        "first4" => Some(crate::diff::MaskKind::First4),
        "email" => Some(crate::diff::MaskKind::Email),
        "name" => Some(crate::diff::MaskKind::Name),
        "dateYear" | "date-year" => Some(crate::diff::MaskKind::DateYear),
        "dateDecade" | "date-decade" => Some(crate::diff::MaskKind::DateDecade),
        // "none", and anything the parser does not know: treat as unmasked so
        // an unrecognised kind widens the fanout rather than silently
        // rewriting the operand with the wrong transform.
        _ => None,
    }
}

/// Mask the operand when the column is masked, so the comparison runs
/// mask-against-mask against what the WAL tuple actually carries.
fn lower_operand(mask_kind: Option<crate::diff::MaskKind>, value: &Value) -> Value {
    let Some(kind) = mask_kind else {
        return value.clone();
    };
    Value::String(crate::crud::mask_pass::apply_mask_kind(
        kind,
        &value_to_text(value),
    ))
}

// ---------------------------------------------------------------------------
// Thread-local capture buffer
// ---------------------------------------------------------------------------
//
// The capture buffer is keyed by the active procedure kind: capture is
// enabled only when an [`Active`] scope guard is in scope (set up by
// the runtime's query-dispatch entry path). The API is split so:
//
// 1. find / count / aggregate callbacks can call
//    [`record_if_active`] without checking themselves.
// 2. Tests and the SDK layer can set the active scope explicitly.

/// One thread's in-flight capture.
///
/// **`recording` is the caller's answer to "is this a query?", not a lookup.**
/// This module used to ask `zeroship_runtime::rpc::current_kind()` on every
/// record, which made a data-plane module depend on the V8 runtime crate for a
/// fact its own installer already knows. The tier is the reason: the procedure
/// kind is ambient adapter state, and an engine that reads it directly is
/// reaching up. The installer decides once; this records.
struct Capture {
    /// `false` installs an inert capture: entries are dropped rather than
    /// buffered. Kept representable rather than refusing to install, because
    /// the caller learns the kind and opens the scope at the same moment and
    /// should not have to branch.
    recording: bool,
    entries: Vec<ReadSetEntry>,
}

thread_local! {
    /// `Some(capture)` when read-set capture is active on this thread.
    /// `None` outside any query handler (the default) — `record_if_active`
    /// is a no-op then.
    static CURRENT_BUFFER: RefCell<Option<Capture>> = const { RefCell::new(None) };
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
#[cfg(test)]
pub struct Active {
    _no_send: std::marker::PhantomData<*const ()>,
}

#[cfg(test)]
impl Active {
    /// Begin capturing. Subsequent [`record_if_active`] calls append to
    /// this guard's buffer until [`Active::take`] or drop.
    ///
    /// `recording` is the caller's answer to "is the active procedure a
    /// `Query`?" - read-set capture is a `query()`-only feature, and the layer
    /// that opens the scope is the layer that knows the kind. Passing `false`
    /// installs an inert scope: calls are dropped, `take` yields nothing.
    pub fn begin(recording: bool) -> Self {
        CURRENT_BUFFER.with(|c| {
            let mut slot = c.borrow_mut();
            assert!(
                slot.is_none(),
                "read-set capture already active on this thread"
            );
            *slot = Some(Capture {
                recording,
                entries: Vec::new(),
            });
        });
        Self {
            _no_send: std::marker::PhantomData,
        }
    }

    /// End capture and return the buffer. After this the guard is
    /// inert; dropping it is a no-op.
    #[must_use]
    pub fn take(self) -> Vec<ReadSetEntry> {
        CURRENT_BUFFER
            .with(|c| c.borrow_mut().take())
            .map(|capture| capture.entries)
            .unwrap_or_default()
    }
}

#[cfg(test)]
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
///
/// **That gate is carried by the capture, not looked up here.** The kind is
/// ambient adapter state; whoever opened the scope resolved it once, at the one
/// moment it is unambiguous. See [`Capture::recording`].
///
/// `schema` is the collection's descriptor entry; every call site already holds
/// one because the read builder it just called takes the same value. It is
/// needed to lower a predicate on a masked column - see [`normalise_filter`].
pub fn record_if_active(collection: &str, filter: &Value, schema: &Value) {
    if !is_active() {
        return;
    }
    let entry = ReadSetEntry {
        collection: collection.to_string(),
        predicate: normalise_filter(filter, schema),
    };
    CURRENT_BUFFER.with(|c| {
        if let Some(capture) = c.borrow_mut().as_mut() {
            if capture.recording {
                capture.entries.push(entry);
            }
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
        let p = normalise_filter(&json!({}), &json!({})).expect("empty filter normalises");
        assert!(p.matches(&row(&[])));
    }

    #[test]
    fn normalise_bare_equality() {
        let p = normalise_filter(&json!({ "userId": 42 }), &json!({})).expect("scalar normalises");
        assert!(p.matches(&row(&[("userId", "42")])));
        assert!(!p.matches(&row(&[("userId", "99")])));
    }

    #[test]
    fn normalise_multi_field_and() {
        let p = normalise_filter(&json!({ "userId": 42, "status": "active" }), &json!({}))
            .expect("conjunction normalises");
        assert!(p.matches(&row(&[("userId", "42"), ("status", "active")])));
        assert!(!p.matches(&row(&[("userId", "42"), ("status", "archived")])));
        assert!(!p.matches(&row(&[("userId", "1"), ("status", "active")])));
    }

    #[test]
    fn normalise_explicit_eq_operator() {
        let p = normalise_filter(&json!({ "userId": { "$eq": 42 } }), &json!({})).expect("op normalises");
        assert!(p.matches(&row(&[("userId", "42")])));
    }

    #[test]
    fn normalise_gt_range_query() {
        let p = normalise_filter(&json!({ "createdAt": { "$gt": 1000 } }), &json!({})).unwrap();
        assert!(p.matches(&row(&[("createdAt", "1500")])));
        assert!(!p.matches(&row(&[("createdAt", "500")])));
        assert!(!p.matches(&row(&[("createdAt", "1000")])));
    }

    #[test]
    fn normalise_range_combination() {
        let p =
            normalise_filter(&json!({ "createdAt": { "$gte": 1000, "$lt": 2000 } }), &json!({})).unwrap();
        assert!(p.matches(&row(&[("createdAt", "1000")])));
        assert!(p.matches(&row(&[("createdAt", "1500")])));
        assert!(!p.matches(&row(&[("createdAt", "2000")])));
        assert!(!p.matches(&row(&[("createdAt", "999")])));
    }

    #[test]
    fn normalise_or_falls_back_coarse() {
        let p = normalise_filter(&json!({ "$or": [ { "a": 1 }, { "b": 2 } ] }), &json!({}));
        assert!(p.is_none(), "$or should collapse to coarse-grained");
    }

    #[test]
    fn normalise_in_falls_back_coarse() {
        let p = normalise_filter(&json!({ "userId": { "$in": [1, 2, 3] } }), &json!({}));
        assert!(p.is_none(), "$in should collapse to coarse-grained");
    }

    #[test]
    fn normalise_like_falls_back_coarse() {
        let p = normalise_filter(&json!({ "name": { "$like": "j%" } }), &json!({}));
        assert!(p.is_none(), "$like should collapse to coarse-grained");
    }

    #[test]
    fn normalise_null_value_falls_back_coarse() {
        // Null/IS NULL semantics need a NULL-aware tuple representation;
        // the WAL consumer's text-encoded tuple can't distinguish "absent"
        // from "explicit NULL" so we conservatively bail out.
        let p = normalise_filter(&json!({ "deletedAt": null }), &json!({}));
        assert!(p.is_none());
    }

    #[test]
    fn normalise_array_filter_falls_back_coarse() {
        let p = normalise_filter(&json!({ "tags": ["x", "y"] }), &json!({}));
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
        let p = normalise_filter(&json!({ "active": true }), &json!({})).unwrap();
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
            predicate: normalise_filter(&json!({ "userId": 42 }), &json!({})),
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
        record_if_active("messages", &json!({ "userId": 42 }), &json!({}));
        assert!(!is_active());
    }

    #[test]
    fn record_no_op_outside_query_kind() {
        // No kind set on the thread, so the installer resolves `recording` to
        // false and the scope is inert.
        let guard = Active::begin(recording_for_current_kind());
        record_if_active("messages", &json!({ "userId": 42 }), &json!({}));
        let entries = guard.take();
        assert!(entries.is_empty());
    }

    /// The kind gate, driven through the REAL runtime marker.
    ///
    /// `record_if_active` no longer reads `current_kind()` itself - the
    /// installer does, once - so these three arms go through
    /// [`recording_for_current_kind`], which is the decision the adapter makes
    /// when it opens the scope. Keeping `KindGuard` here is deliberate: a test
    /// that passed the boolean literally would still pass if the mapping from
    /// kind to boolean were inverted.
    fn recording_for_current_kind() -> bool {
        matches!(
            zeroship_runtime::rpc::current_kind(),
            Some(zeroship_runtime::rpc::ProcedureKind::Query)
        )
    }

    #[test]
    fn capture_in_query_kind_only() {
        use zeroship_runtime::rpc::{KindGuard, ProcedureKind};
        let _kg = KindGuard::enter(ProcedureKind::Query);
        let guard = Active::begin(recording_for_current_kind());
        record_if_active("messages", &json!({ "userId": 42 }), &json!({}));
        record_if_active("messages", &json!({}), &json!({}));
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
        let guard = Active::begin(recording_for_current_kind());
        record_if_active("messages", &json!({ "userId": 42 }), &json!({}));
        let entries = guard.take();
        assert!(entries.is_empty(), "mutations must not record read-set");
    }

    /// **A recording scope still ends at its guard.** Distinct from the kind
    /// gate: an inert scope drops entries, this one has none left to drop.
    #[test]
    fn drop_clears_buffer_even_without_take() {
        {
            let _g = Active::begin(true);
            assert!(is_active());
        }
        assert!(!is_active(), "drop must clear the buffer");
    }
}
