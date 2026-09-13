//! Capture query dependencies and conservatively match change events.
//!
//! The host enables capture for query dispatches and attaches a collection's
//! nonempty read set when opening its subscription. Subscribing without a captured
//! read stays coarse; an explicitly empty read set matches nothing.
//!
//! Supported predicates can narrow events with row images. Unsupported filters or
//! missing row data fall back to collection invalidation to avoid dropping changes.
//! This module owns neither V8 callbacks nor subscription lifecycle.

use std::collections::HashMap;

use crate::schema::FieldMap;
use crate::sql::catalog::MaskKind;
use crate::value::Value;

use crate::masking::apply_mask_kind;

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
/// The collection's native schema determines how protected filter values
/// compare with the stored row image.
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
pub fn normalise_filter(filter: &Value, schema: &FieldMap) -> Option<Predicate> {
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
                    if !matches!(val, Value::String(_) | Value::Number(_) | Value::Bool(_)) {
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
            Value::Json(_)
            | Value::Array(_)
            | Value::Bytes(_)
            | Value::Timestamp(_)
            | Value::Decimal(_) => return None,
        }
    }
    Some(Predicate::All(conjuncts))
}

/// The mask kind declared for `column`, or `None` when it is unmasked or opted
/// out with `kind: "none"`.
fn mask_kind_for_column(schema: &FieldMap, column: &str) -> Option<MaskKind> {
    let mask = crate::sql::descriptors::effective_mask(schema.get(column)?)?;
    // Unknown kinds widen fanout instead of applying the wrong transform.
    MaskKind::from_sql(mask.kind)
}

/// Mask the operand when the column is masked, so the comparison runs
/// mask-against-mask against what the WAL tuple actually carries.
fn lower_operand(mask_kind: Option<MaskKind>, value: &Value) -> Value {
    let Some(kind) = mask_kind else {
        return value.clone();
    };
    Value::String(apply_mask_kind(kind, &value_to_text(value)))
}

mod capture;
pub use capture::{Capture, is_active};

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
/// moment it is unambiguous. See [`Capture::new`].
///
/// `schema` is the collection's descriptor entry; every call site already holds
/// one because the read builder it just called takes the same value. It is
/// needed to lower a predicate on a masked column - see [`normalise_filter`].
pub fn record_if_active(collection: &str, filter: &Value, schema: &FieldMap) {
    if !capture::is_recording() {
        return;
    }
    let entry = ReadSetEntry {
        collection: collection.to_string(),
        predicate: normalise_filter(filter, schema),
    };
    capture::record(entry);
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value;

    fn row(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    // -------- normalise_filter --------

    #[test]
    fn normalise_empty_filter_is_always_true() {
        let p = normalise_filter(&value!({}), &FieldMap::new()).expect("empty filter normalises");
        assert!(p.matches(&row(&[])));
    }

    #[test]
    fn native_and_decoded_masks_normalise_the_same_read_dependency() {
        use crate::schema::{CollectionSchema, ColumnSchema, LogicalType, MaskSchema};
        let mut column = ColumnSchema::new(LogicalType::Text);
        column.mask = Some(MaskSchema {
            kind: "last4".into(),
            classification: "spi".into(),
        });
        let native = CollectionSchema::new([("ssn".into(), column)]).into_fields();
        let decoded = CollectionSchema::from_fields(&value!({
            "ssn": {
                "type": "string", "required": true,
                "mask": {"kind": "last4", "classification": "spi"}
            }
        }))
        .unwrap()
        .into_fields();
        assert_eq!(native, decoded);
        for schema in [native, decoded] {
            let predicate = normalise_filter(&value!({"ssn": "123-45-6789"}), &schema)
                .expect("masked equality retains the dependency");
            assert!(predicate.matches(&row(&[("ssn", "***-**-6789")])));
            assert!(!predicate.matches(&row(&[("ssn", "***-**-4321")])));
            assert!(normalise_filter(&value!({"ssn": {"$gt": "123-45-6789"}}), &schema).is_none());
        }
    }

    #[test]
    fn normalise_bare_equality() {
        let p = normalise_filter(&value!({ "userId": 42 }), &FieldMap::new())
            .expect("scalar normalises");
        assert!(p.matches(&row(&[("userId", "42")])));
        assert!(!p.matches(&row(&[("userId", "99")])));
    }

    #[test]
    fn normalise_multi_field_and() {
        let p = normalise_filter(
            &value!({ "userId": 42, "status": "active" }),
            &FieldMap::new(),
        )
        .expect("conjunction normalises");
        assert!(p.matches(&row(&[("userId", "42"), ("status", "active")])));
        assert!(!p.matches(&row(&[("userId", "42"), ("status", "archived")])));
        assert!(!p.matches(&row(&[("userId", "1"), ("status", "active")])));
    }

    #[test]
    fn normalise_explicit_eq_operator() {
        let p = normalise_filter(&value!({ "userId": { "$eq": 42 } }), &FieldMap::new())
            .expect("op normalises");
        assert!(p.matches(&row(&[("userId", "42")])));
    }

    #[test]
    fn normalise_gt_range_query() {
        let p =
            normalise_filter(&value!({ "createdAt": { "$gt": 1000 } }), &FieldMap::new()).unwrap();
        assert!(p.matches(&row(&[("createdAt", "1500")])));
        assert!(!p.matches(&row(&[("createdAt", "500")])));
        assert!(!p.matches(&row(&[("createdAt", "1000")])));
    }

    #[test]
    fn normalise_range_combination() {
        let p = normalise_filter(
            &value!({ "createdAt": { "$gte": 1000, "$lt": 2000 } }),
            &FieldMap::new(),
        )
        .unwrap();
        assert!(p.matches(&row(&[("createdAt", "1000")])));
        assert!(p.matches(&row(&[("createdAt", "1500")])));
        assert!(!p.matches(&row(&[("createdAt", "2000")])));
        assert!(!p.matches(&row(&[("createdAt", "999")])));
    }

    #[test]
    fn normalise_or_falls_back_coarse() {
        let p = normalise_filter(
            &value!({ "$or": [ { "a": 1 }, { "b": 2 } ] }),
            &FieldMap::new(),
        );
        assert!(p.is_none(), "$or should collapse to coarse-grained");
    }

    #[test]
    fn normalise_in_falls_back_coarse() {
        let p = normalise_filter(
            &value!({ "userId": { "$in": [1, 2, 3] } }),
            &FieldMap::new(),
        );
        assert!(p.is_none(), "$in should collapse to coarse-grained");
    }

    #[test]
    fn normalise_like_falls_back_coarse() {
        let p = normalise_filter(&value!({ "name": { "$like": "j%" } }), &FieldMap::new());
        assert!(p.is_none(), "$like should collapse to coarse-grained");
    }

    #[test]
    fn normalise_null_value_falls_back_coarse() {
        // Null/IS NULL semantics need a NULL-aware tuple representation;
        // the WAL consumer's text-encoded tuple can't distinguish "absent"
        // from "explicit NULL" so we conservatively bail out.
        let p = normalise_filter(&value!({ "deletedAt": null }), &FieldMap::new());
        assert!(p.is_none());
    }

    #[test]
    fn normalise_array_filter_falls_back_coarse() {
        let p = normalise_filter(&value!({ "tags": ["x", "y"] }), &FieldMap::new());
        assert!(p.is_none());
    }

    // -------- Predicate::matches --------

    #[test]
    fn predicate_with_missing_column_does_not_match() {
        let p = Predicate::All(vec![Conjunct {
            column: "userId".into(),
            op: PredicateOp::Eq,
            value: value!(42),
        }]);
        assert!(!p.matches(&row(&[("otherCol", "42")])));
    }

    #[test]
    fn predicate_bool_eq() {
        let p = normalise_filter(&value!({ "active": true }), &FieldMap::new()).unwrap();
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
            predicate: normalise_filter(&value!({ "userId": 42 }), &FieldMap::new()),
        };
        assert!(entry.matches(&row(&[("userId", "42")])));
        assert!(!entry.matches(&row(&[("userId", "99")])));
    }

}
