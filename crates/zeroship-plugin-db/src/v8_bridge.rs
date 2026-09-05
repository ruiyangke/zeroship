//! V8 ↔ Rust marshaling layer for `zeroship.db.*` callbacks.
//!
//! This module is the seam between V8 and the rest of the plugin —
//! nothing here knows about SQL or schema. Callers above
//! (`crud`, `transaction`, `drop_namespace`,
//! `replication_ops`) parse args via these helpers, mint promises, and
//! hand the work to the async layer.
//!
//! Contents:
//!
//! - **Promise plumbing**: `setup_promise` for the `OpResult::Completed`
//!   path (string-typed value), `setup_js_promise` for the
//!   `OpResult::JsValue` path (real JS values via `ResolveValue::Json` /
//!   `ResolveValue::F64` / `ResolveValue::JsGlobal`).
//! - **Argument decoders**: `read_json_arg`, `v8_value_to_serde_json`
//!   (the hot-path walker that avoids a `JSON.stringify` round-trip).
//! - **State accessors**: `runtime_state` (read the `SharedState` off
//!   the isolate slot). `get_app_id` and its `get_app_id_pub` wrapper
//!   were deleted on 2026-09-04: the wrapper existed for
//!   `v8_classes::migration`, that consumer is gone, and a two-link
//!   chain like this only reads as dead once the public half goes -
//!   the private half looks called right up until then.
//! - **Capability gate**: `refuse_if_query_capability` — the B3 gate
//!   that rejects writes from inside a `query()` handler.
//! - **Row decoding**: `row_to_json`, `column_to_json`,
//!   `rows_to_json_value` (the typed `Vec<Value>` intermediate the
//!   CRUD chain threads end-to-end) — Postgres OID → JSON conversion,
//!   used by every exec path. The `fmt_db_err` shim that used to sit
//!   beside them is gone with its last caller (the deleted
//!   `create_index_with_recovery_audited`); reach for
//!   [`crate::backend::pg_error::classify`] directly so the SQLSTATE
//!   classification survives to the V8 boundary.

use serde_json::Value;
use zeroship_runtime::state::{ResolveValue, SharedState};

// ---------------------------------------------------------------------------
// State accessors
// ---------------------------------------------------------------------------

/// Get the runtime state slot off the isolate. Shared by every callback
/// and dispatch helper; consolidated here to avoid copy-pasting the
/// expect.
pub(crate) fn runtime_state(scope: &mut v8::PinScope<'_, '_>) -> SharedState {
    scope
        .get_slot::<SharedState>()
        .expect("RuntimeState not in isolate slot")
        .clone()
}

// ---------------------------------------------------------------------------
// Capability gate
// ---------------------------------------------------------------------------

/// B3 capability gate. Returns `true` if the caller refused the write
/// because the active procedure kind is `query()` — in which case the
/// callback has already set a rejected promise on `rv` and the caller
/// must return immediately.
///
/// Refuse a write op from inside a `query()` handler. Returns
/// `Some(rejected_promise)` to the caller (which returns it as the JS
/// value); `None` when the write is allowed.
///
/// The error envelope matches the `capability_violation` shape
/// (`code`, `wrapper`, `violated`, `remediation`) so the dispatch
/// path can render a structured 500 instead of a generic exception.
pub(crate) fn refuse_if_query_capability<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    op: &str,
) -> Option<v8::Local<'s, v8::Promise>> {
    if !matches!(
        zeroship_runtime::rpc::current_kind(),
        Some(zeroship_runtime::rpc::ProcedureKind::Query)
    ) {
        return None;
    }
    let resolver = v8::PromiseResolver::new(scope).unwrap();
    let promise = resolver.get_promise(scope);
    let exc = zeroship_runtime::rpc::build_capability_violation(
        scope,
        "query",
        op,
        "Use mutation() if you need to write to the database. Queries are read-only.",
    );
    resolver.reject(scope, exc.into());
    Some(promise)
}

// ---------------------------------------------------------------------------
// Read-set capture
// ---------------------------------------------------------------------------

/// Open (or keep) the read-set capture for the dispatch frame this read runs
/// in. Call from the adapter's read entry points, before the engine plans the
/// query - `crate::crud`'s `record_read_set` runs inside `plan_find` and needs
/// a capture already installed.
///
/// **This function exists so that `read_set` and `crud` need not.** Both are
/// engine-tier and name no runtime type (#96, #117); the kind and the dispatch
/// generation are ambient ADAPTER state, so the adapter resolves them once and
/// passes them down. `read_set::ensure_capture` takes them as plain values.
///
/// Cheap enough for the read hot path: two thread-local reads plus, on the
/// first read of a frame, one small allocation.
pub(crate) fn ensure_read_set_capture() {
    crate::read_set::ensure_capture(
        zeroship_runtime::rpc::dispatch_generation(),
        matches!(
            zeroship_runtime::rpc::current_kind(),
            Some(zeroship_runtime::rpc::ProcedureKind::Query)
        ),
    );
}

// ---------------------------------------------------------------------------
// V8 value walker
// ---------------------------------------------------------------------------

/// Walk a `v8::Local<v8::Value>` directly into a `serde_json::Value`,
/// skipping the JSON.stringify / serde_json::from_str round trip used by
/// `read_json_arg`. Used by the v8_class `Collection` methods on the
/// hot path so we don't pay two parse costs per CRUD call.
///
/// Mirrors the small walker in `runtime/src/rpc/superjson.rs`. We
/// duplicate rather than re-export because plugin-db must not pull in
/// the entire `rpc` subtree (cyclic dep risk).
///
/// The mirror is NOT faithful on non-finite numbers, and the divergence is
/// undecided rather than intended. `superjson`'s `encode_number` tags NaN and
/// +/-Infinity and sends them as sentinel strings, so RPC round-trips them
/// losslessly - that is a documented contract in its module table and is covered
/// by `runtime/tests/rpc_superjson.rs::special_floats_round_trip`. This walker
/// now REFUSES them (see `non_finite_numbers_are_refused_not_coerced`).
///
/// The two boundaries still differ, but deliberately and in the safe direction:
/// RPC preserves non-finite values via sentinels, `env.db` REFUSES them. What is
/// no longer true is the old third option - silently coercing to `Value::Null` -
/// which turned a filter value into `IS NULL` and stored NULL for a number the
/// app supplied.
///
/// Mapping:
/// - `undefined` / `null` → `Value::Null`
/// - boolean → `Value::Bool`
/// - number → `Value::Number` (lossless integer when representable,
///   otherwise f64)
/// - string → `Value::String`
/// - array → `Value::Array` (recurse on each element)
/// - object → `Value::Object` (recurse on each enumerable own property)
/// - anything else (functions, symbols) → REFUSED, never coerced
///
/// The decode is TOTAL: it yields the whole value or an error. It never returns
/// a strict subset of its input, because callers build SQL predicates from the
/// result.
///
/// DB-6: max nesting depth the V8→serde walker will descend. A malicious
/// deeply-nested argument (tens of thousands of `[[[…]]]` / `{a:{a:…}}` levels)
/// would otherwise overflow the worker thread's native stack inside the
/// recursion — an **end-user-reachable** abort, since apps routinely forward
/// untrusted end-user JSON straight into a filter/document. 256 is far above
/// any legitimate document nesting yet far below the stack limit. Structure past
/// the cap is REFUSED: truncating it to `Value::Null` would hand the caller a
/// document that is not the one they passed.
pub(crate) const MAX_DECODE_DEPTH: usize = 256;

/// Total number of decoded nodes one argument may produce.
pub(crate) const MAX_DECODE_NODES: usize = 200_000;
/// Total decoded string bytes one argument may produce.
pub(crate) const MAX_DECODE_BYTES: usize = 8 * 1024 * 1024;

/// Why a decode refused. The decode is **total**: it either produces the whole
/// value the caller declared, or it refuses. It never returns a value that is a
/// strict subset of the input, because callers build SQL predicates from the
/// result and a silently-narrowed predicate is a tenant-isolation defect, not a
/// robustness one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DecodeError {
    /// A getter, `Proxy` trap or `toISOString` threw. V8 already holds the
    /// pending exception, so the caller returns without throwing its own and
    /// V8 rethrows the creator's original error.
    PendingException,
    /// A structural budget was exceeded before allocation.
    Budget(&'static str),
    /// A value that cannot be represented, and must not be coerced. A filter
    /// value silently becoming `null` changes the operator to `IS NULL`.
    Unsupported(&'static str),
}

/// Per-argument allocation budget, enforced **before** allocating.
pub(crate) struct DecodeBudget {
    nodes: usize,
    bytes: usize,
}

impl DecodeBudget {
    fn new() -> Self {
        Self {
            nodes: MAX_DECODE_NODES,
            bytes: MAX_DECODE_BYTES,
        }
    }
    fn take_node(&mut self) -> Result<(), DecodeError> {
        self.nodes = self
            .nodes
            .checked_sub(1)
            .ok_or(DecodeError::Budget("node count"))?;
        Ok(())
    }
    fn take_nodes(&mut self, n: usize) -> Result<(), DecodeError> {
        self.nodes = self
            .nodes
            .checked_sub(n)
            .ok_or(DecodeError::Budget("node count"))?;
        Ok(())
    }
    fn take_bytes(&mut self, n: usize) -> Result<(), DecodeError> {
        self.bytes = self
            .bytes
            .checked_sub(n)
            .ok_or(DecodeError::Budget("decoded bytes"))?;
        Ok(())
    }
}

pub(crate) fn v8_value_to_serde_json(
    scope: &mut v8::PinScope<'_, '_>,
    v: v8::Local<v8::Value>,
) -> Result<Value, DecodeError> {
    let mut budget = DecodeBudget::new();
    v8_value_to_serde_json_depth(scope, v, 0, &mut budget)
}

fn v8_value_to_serde_json_depth(
    scope: &mut v8::PinScope<'_, '_>,
    v: v8::Local<v8::Value>,
    depth: usize,
    budget: &mut DecodeBudget,
) -> Result<Value, DecodeError> {
    budget.take_node()?;
    // DB-6: stop descending past the cap (before recursing into the
    // array/object branches below) so a pathological nesting can't overflow
    // the native stack.
    if depth > MAX_DECODE_DEPTH {
        return Err(DecodeError::Budget("nesting depth"));
    }
    if v.is_null_or_undefined() {
        return Ok(Value::Null);
    }
    if v.is_boolean() {
        return Ok(Value::Bool(v.is_true()));
    }
    if v.is_number() {
        let n = v.number_value(scope).ok_or(DecodeError::PendingException)?;
        if n.fract() == 0.0 && n >= i64::MIN as f64 && n <= i64::MAX as f64 {
            let i = n as i64;
            if (i as f64) == n {
                return Ok(Value::Number(serde_json::Number::from(i)));
            }
        }
        if let Some(num) = serde_json::Number::from_f64(n) {
            return Ok(Value::Number(num));
        }
        // Non-finite. Coercing to null would change a filter's operator to
        // `IS NULL` and would store NULL for a number the app supplied.
        return Err(DecodeError::Unsupported("non-finite number"));
    }
    if v.is_string() {
        let s = v.to_rust_string_lossy(scope);
        budget.take_bytes(s.len())?;
        return Ok(Value::String(s));
    }
    // Date — `JSON.stringify(new Date())` calls `Date.prototype.toJSON`
    // which returns an ISO string. Mirror that here so date fields in
    // filters/docs round-trip the same way they did under the legacy
    // `JSON.stringify` boundary. Without this branch the object walk
    // below sees `new Date()` as a plain object with no own properties
    // and produces `{}` — silently losing the value.
    if v.is_date() {
        if let Ok(obj) = v8::Local::<v8::Object>::try_from(v) {
            let to_iso_key = v8::String::new(scope, "toISOString").unwrap();
            // `get` runs JS: a replaced `toISOString` may throw. A throw here
            // leaves a pending exception, which we propagate rather than
            // falling through to the numeric branch with a live exception set.
            let fn_v = obj
                .get(scope, to_iso_key.into())
                .ok_or(DecodeError::PendingException)?;
            if let Ok(to_iso) = v8::Local::<v8::Function>::try_from(fn_v) {
                let result = to_iso
                    .call(scope, v, &[])
                    .ok_or(DecodeError::PendingException)?;
                if result.is_string() {
                    let s = result.to_rust_string_lossy(scope);
                    budget.take_bytes(s.len())?;
                    return Ok(Value::String(s));
                }
            }
        }
        // Fallback: produce the Unix-ms number (no ISO formatter
        // reachable). The SDK accepts numeric dates anyway.
        if let Ok(date) = v8::Local::<v8::Date>::try_from(v) {
            let ms = date.value_of();
            if ms.is_finite() {
                if let Some(n) = serde_json::Number::from_f64(ms) {
                    return Ok(Value::Number(n));
                }
            }
        }
        return Err(DecodeError::Unsupported("date with no representable value"));
    }
    if v.is_array() {
        let arr: v8::Local<v8::Array> = v
            .try_into()
            .map_err(|_| DecodeError::Unsupported("array"))?;
        let n = arr.length() as usize;
        // Charge the whole breadth BEFORE reserving. `new Array(4294967295)` is
        // cheap and sparse in V8 but would otherwise ask Rust to reserve
        // billions of slots and then walk them.
        budget.take_nodes(n)?;
        let mut out = Vec::with_capacity(n);
        for i in 0..arr.length() {
            // An element that cannot be read is NOT `null`. Defaulting it would
            // silently alter the value the caller passed.
            let elem = arr
                .get_index(scope, i)
                .ok_or(DecodeError::PendingException)?;
            out.push(v8_value_to_serde_json_depth(
                scope,
                elem,
                depth + 1,
                budget,
            )?);
        }
        return Ok(Value::Array(out));
    }
    if v.is_object() {
        let obj: v8::Local<v8::Object> = v
            .try_into()
            .map_err(|_| DecodeError::Unsupported("object"))?;
        let names = obj
            .get_own_property_names(scope, v8::GetPropertyNamesArgs::default())
            .ok_or(DecodeError::PendingException)?;
        budget.take_nodes(names.length() as usize)?;
        let mut map = serde_json::Map::new();
        for i in 0..names.length() {
            let key_v = names
                .get_index(scope, i)
                .ok_or(DecodeError::PendingException)?;
            let key = key_v.to_rust_string_lossy(scope);
            budget.take_bytes(key.len())?;
            // THE DEFECT THIS REPLACES: `obj.get` runs JS (accessor property,
            // `Proxy` trap). The old arm did `continue` here, treating "I could
            // not read this" as "this was not there" - so a two-clause filter
            // decoded to one clause and the mutation ran against a strict
            // subset of the declared predicate. Read exactly once, and refuse.
            let val_v = obj.get(scope, key_v).ok_or(DecodeError::PendingException)?;
            map.insert(
                key,
                v8_value_to_serde_json_depth(scope, val_v, depth + 1, budget)?,
            );
        }
        return Ok(Value::Object(map));
    }
    // Functions and symbols reach here. Coercing them to null is a silent
    // substitution of the caller's value.
    Err(DecodeError::Unsupported("value is not JSON-representable"))
}

/// Read a CRUD method's object/array argument directly from V8 into a
/// `serde_json::Value`. `undefined`/missing → empty object (matches
/// `read_json_arg`'s default).
pub(crate) fn read_json_arg(
    scope: &mut v8::PinScope<'_, '_>,
    v: Option<v8::Local<v8::Value>>,
) -> Result<Value, DecodeError> {
    match v {
        Some(val) if !val.is_null_or_undefined() => v8_value_to_serde_json(scope, val),
        _ => Ok(Value::Object(serde_json::Map::new())),
    }
}

/// Turn a refused decode into the value a `#[v8_method]` returns.
///
/// [`DecodeError::PendingException`] means V8 already holds the creator's own
/// exception (their getter threw); we return `undefined` and V8 rethrows it, so
/// the developer sees their error rather than one we invented. Every other
/// variant is ours, and throws a stable `INVALID_ARGUMENT`.
pub(crate) fn throw_decode_error<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    err: &DecodeError,
) -> v8::Local<'s, v8::Value> {
    match err {
        DecodeError::PendingException => {}
        DecodeError::Budget(what) | DecodeError::Unsupported(what) => {
            let msg = v8::String::new(
                scope,
                &format!("INVALID_ARGUMENT: argument could not be decoded ({what})"),
            )
            .unwrap();
            let exc = v8::Exception::type_error(scope, msg);
            scope.throw_exception(exc);
        }
    }
    v8::undefined(scope).into()
}

// ---------------------------------------------------------------------------
// Promise plumbing
// ---------------------------------------------------------------------------

/// Create a promise for the `OpResult::JsValue` resolution path —
/// returns `(resolver_global, request_id, promise)`. Use when the
/// dispatch helper resolves with a real JS value (number, object,
/// `null`, `undefined`) rather than a JSON string.
pub(crate) fn setup_js_promise<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    state: &SharedState,
) -> (
    v8::Global<v8::PromiseResolver>,
    Option<u64>,
    v8::Local<'s, v8::Promise>,
) {
    let resolver = v8::PromiseResolver::new(scope).unwrap();
    let promise = resolver.get_promise(scope);
    let global_resolver = v8::Global::new(scope, resolver);
    let request_id = state.borrow().executing_request_id;
    (global_resolver, request_id, promise)
}

// ---------------------------------------------------------------------------
// Result lowering: engine value -> runtime ResolveValue
// ---------------------------------------------------------------------------
//
// Moved out of `crud/mod.rs` on 2026-08-31. Every one of these returns a
// `zeroship_runtime::state::ResolveValue`, so they were already adapter work
// filed in the engine - but `maybe_rehydrate` was the one that mattered: it
// selected `crate::v8_classes::masked_value::rehydrate_masked_values`, an
// ADAPTER function pointer whose type is
// `fn(&mut v8::PinScope, v8::Local<Value>) -> Option<v8::Local<Value>>`, on a
// data predicate (`has_masked`). An engine module was choosing V8 behaviour
// while spelling no `v8::` token, which is invisible to every marker-based
// instrument in this repository.
//
// It sits on the masked read path - reached from twelve dispatch bodies - so
// every masked read routed through it. The adapter now picks its own callback,
// and the engine says only whether the result carries masked columns.

/// `first_row_or_null` variant that, when `has_masked` is
/// set, resolves via [`ResolveValue::JsonWithRehydration`] so the pump
/// walks the parsed value and replaces `__zsmask__` sentinels with
/// native `MaskedValue` instances. When `has_masked` is `false` this is
/// identical to `first_row_or_null` (plain `JSON.parse`, no walk).
pub(crate) fn first_row_or_null_masked(rows: Vec<Value>, has_masked: bool) -> ResolveValue {
    let value = rows.into_iter().next().unwrap_or(Value::Null).to_string();
    maybe_rehydrate(value, has_masked)
}

/// Lower a `Vec<Value>` result to the row count, as a JS `number`.
/// Used by `updateMany` / `deleteMany` (resolves to the affected-row
/// count).
#[allow(clippy::cast_precision_loss)]
pub(crate) fn row_count_as_f64(rows: Vec<Value>) -> ResolveValue {
    ResolveValue::F64(rows.len() as f64)
}

#[allow(clippy::cast_precision_loss)]
pub(crate) fn usize_count_as_f64(count: usize) -> ResolveValue {
    ResolveValue::F64(count as f64)
}

/// `rows_as_json_array` variant that resolves via
/// [`ResolveValue::JsonWithRehydration`] when `has_masked` is set. See
/// [`first_row_or_null_masked`].
pub(crate) fn rows_as_json_array_masked(rows: Vec<Value>, has_masked: bool) -> ResolveValue {
    let value = Value::Array(rows).to_string();
    maybe_rehydrate(value, has_masked)
}

/// Pick `ResolveValue::JsonWithRehydration` (walk the
/// parsed value, mint `MaskedValue` for `__zsmask__` sentinels) when the
/// result is known to carry masked columns; otherwise the plain
/// `ResolveValue::Json` fast path (bulk `JSON.parse`, no walk).
pub(crate) fn maybe_rehydrate(json: String, has_masked: bool) -> ResolveValue {
    if has_masked {
        ResolveValue::JsonWithRehydration {
            json,
            transform: crate::v8_classes::masked_value::rehydrate_masked_values,
        }
    } else {
        ResolveValue::Json(json)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeroship_runtime::init_v8;

    // -----------------------------------------------------------------
    // The read-set kind gate.
    //
    // These three arms came from `read_set.rs`'s own test module on
    // 2026-09-03, when that module moved to `zeroship-data-core` and could no
    // longer name `zeroship_runtime`. They belong here on the merits, not just
    // by exclusion: the mapping from procedure kind to `recording` is made
    // HERE, in `ensure_read_set_capture`, and nowhere else. The versions they
    // replace called `Active::begin(recording_for_current_kind())` - a
    // test-local copy of that mapping, which would still have passed with the
    // production mapping inverted.
    //
    // `libtest` runs each test on its own thread, so the thread-local capture
    // buffer starts empty in every arm and needs no teardown.
    // -----------------------------------------------------------------

    #[test]
    fn capture_records_in_query_kind() {
        use zeroship_runtime::rpc::{KindGuard, ProcedureKind};
        use crate::read_set::Predicate;

        let _kg = KindGuard::enter(ProcedureKind::Query);
        ensure_read_set_capture();
        crate::read_set::record_if_active(
            "messages",
            &serde_json::json!({ "userId": 42 }),
            &serde_json::json!({}),
        );
        crate::read_set::record_if_active(
            "messages",
            &serde_json::json!({}),
            &serde_json::json!({}),
        );

        let entries = crate::read_set::snapshot_for("messages");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].collection, "messages");
        assert!(entries[0].predicate.is_some());
        assert_eq!(entries[1].predicate, Some(Predicate::All(vec![])));
    }

    #[test]
    fn capture_skipped_in_mutation_kind() {
        use zeroship_runtime::rpc::{KindGuard, ProcedureKind};

        let _kg = KindGuard::enter(ProcedureKind::Mutation);
        ensure_read_set_capture();
        crate::read_set::record_if_active(
            "messages",
            &serde_json::json!({ "userId": 42 }),
            &serde_json::json!({}),
        );
        assert!(
            crate::read_set::snapshot_for("messages").is_empty(),
            "mutations must not record read-set"
        );
    }

    #[test]
    fn capture_skipped_outside_any_procedure_kind() {
        // No `KindGuard`, so `current_kind()` is `None` and the capture is
        // installed inert rather than not installed at all.
        ensure_read_set_capture();
        assert!(crate::read_set::is_active(), "the capture is still opened");
        crate::read_set::record_if_active(
            "messages",
            &serde_json::json!({ "userId": 42 }),
            &serde_json::json!({}),
        );
        assert!(crate::read_set::snapshot_for("messages").is_empty());
    }

    #[test]
    fn decode_caps_recursion_depth_db6() {
        // DB-6: a deeply-nested arg must not overflow the worker thread's
        // native stack inside the recursive decoder. Build an array nested far
        // past MAX_DECODE_DEPTH (via a loop, not a literal, to avoid V8's own
        // parser depth limit), decode it, and assert (a) the process does NOT
        // crash — the test completing is the proof — and (b) the structure is
        // terminated at the cap with Null rather than descending forever.
        init_v8();
        let mut isolate = v8::Isolate::new(v8::CreateParams::default());
        v8::scope!(let handle_scope, &mut isolate);
        let context = v8::Context::new(handle_scope, Default::default());
        let scope = &mut v8::ContextScope::new(handle_scope, context);

        let depth = MAX_DECODE_DEPTH + 50;
        let src = format!(
            "(() => {{ let root = [], cur = root; for (let i = 0; i < {depth}; i++) \
             {{ const n = []; cur.push(n); cur = n; }} return root; }})()"
        );
        let code = v8::String::new(scope, &src).unwrap();
        let script = v8::Script::compile(scope, code, None).unwrap();
        let val = script.run(scope).unwrap();

        let decoded = v8_value_to_serde_json(scope, val);

        // The cap now REFUSES. v2 pinned `Value::Null` past the cap, which is
        // the same absence-means-safe shape the total decode removes: a
        // truncated document is not the document the caller passed.
        assert_eq!(
            decoded,
            Err(DecodeError::Budget("nesting depth")),
            "nesting past the cap must refuse, not truncate to Null"
        );
    }

    /// Non-finite JS numbers decode to `Value::Null`, SILENTLY.
    ///
    /// This pins current behaviour rather than endorsing it. `Infinity`,
    /// `-Infinity` and `NaN` all reach the number arm as real V8 numbers, skip the
    /// lossless-integer branch (their `fract()` is NaN, so `fract() == 0.0` is
    /// false), and then fail `serde_json::Number::from_f64`, which returns `None`
    /// for anything non-finite. The arm falls through to `Value::Null`.
    ///
    /// So a creator value of `Infinity` is not rejected here and does not error -
    /// it becomes NULL, and on a nullable column the row stores NULL for a number
    /// the app supplied. What prevents that on the ordinary path is the SDK guard
    /// in `sdks/db/src/validate.ts` (`Number.isFinite`), which runs before the op.
    /// A raw native op call does not go through it and still nulls silently.
    ///
    /// The coercion is worth pinning because it is invisible: no error, no log,
    /// and the same `Value::Null` an explicit `null` produces, so nothing
    /// downstream can tell the two apart.
    #[test]
    fn non_finite_numbers_are_refused_not_coerced() {
        init_v8();
        let mut isolate = v8::Isolate::new(v8::CreateParams::default());
        v8::scope!(let handle_scope, &mut isolate);
        let context = v8::Context::new(handle_scope, Default::default());
        let scope = &mut v8::ContextScope::new(handle_scope, context);

        macro_rules! decode {
            ($src:expr) => {{
                let code = v8::String::new(scope, $src).unwrap();
                let script = v8::Script::compile(scope, code, None).unwrap();
                let val = script.run(scope).unwrap();
                v8_value_to_serde_json(scope, val)
            }};
        }

        for src in ["Infinity", "-Infinity", "NaN", "1/0", "-1/0", "0/0"] {
            assert_eq!(
                decode!(src),
                Err(DecodeError::Unsupported("non-finite number")),
                "{src} must be REFUSED, not coerced to Null: coercion turns a
                 filter value into IS NULL and stores NULL for a supplied number"
            );
        }

        // POSITIVE CONTROL. The loop above is satisfied by a decoder that returns
        // Null for every number, which would be a far worse bug and would leave
        // this test green. Finite values must survive, including the f64 extreme
        // adjacent to the ones that do not.
        assert_eq!(
            decode!("1.5"),
            Ok(Value::Number(serde_json::Number::from_f64(1.5).unwrap()))
        );
        assert_eq!(
            decode!("0"),
            Ok(Value::Number(serde_json::Number::from(0i64)))
        );
        assert_eq!(
            decode!("-42"),
            Ok(Value::Number(serde_json::Number::from(-42i64)))
        );
        assert!(
            matches!(decode!("Number.MAX_VALUE"), Ok(Value::Number(_))),
            "the largest finite double must decode as a number, not Null"
        );
    }

    /// REVEAL TEST for L5 - a filter key whose getter throws is SILENTLY
    /// DROPPED, so a mutation can execute against a strict subset of the
    /// predicate the caller declared.
    ///
    /// `obj.get(scope, key_v)` runs JS: an accessor property, a `Proxy` trap,
    /// or a `Date`'s `toISOString`. When that throws, `get` returns `None` and
    /// the object arm does `continue`, which treats "I could not read this" as
    /// "this was not there".
    ///
    /// Concretely: `updateMany({ tenantId: <throwing getter>, status: "x" })`
    /// is validated by the SDK as a two-clause filter, then decoded here into
    /// a ONE-clause filter, and the UPDATE runs as `WHERE status = 'x'` across
    /// every tenant's rows. That is a tenant-isolation defect, not a
    /// robustness one, which is why this is a security test.
    ///
    /// This test asserts the REQUIRED behaviour and therefore FAILS on the
    /// current decoder. The fix is the total, fallible decode: a failed
    /// `Object::get` aborts the whole operation with `INVALID_ARGUMENT`
    /// instead of skipping the key.
    #[test]
    fn decode_must_not_silently_drop_a_key_whose_getter_throws_l5() {
        init_v8();
        let mut isolate = v8::Isolate::new(v8::CreateParams::default());
        v8::scope!(let handle_scope, &mut isolate);
        let context = v8::Context::new(handle_scope, Default::default());
        let scope = &mut v8::ContextScope::new(handle_scope, context);

        // A two-clause filter whose first clause throws on read.
        let src = r#"(() => {
            const f = { };
            Object.defineProperty(f, "tenantId", {
                enumerable: true,
                get() { throw new Error("boom"); },
            });
            f.status = "x";
            return f;
        })()"#;
        let code = v8::String::new(scope, src).unwrap();
        let script = v8::Script::compile(scope, code, None).unwrap();
        let val = script.run(scope).unwrap();

        let decoded = v8_value_to_serde_json(scope, val);

        // Both clauses were declared. The decode must REFUSE rather than yield
        // a filter that silently lost one of them.
        assert_eq!(
            decoded,
            Err(DecodeError::PendingException),
            "a key whose getter throws must refuse the whole decode"
        );

        // The point of the refusal, stated as an assertion so a future change
        // that reintroduces `continue` fails here and not in production: no
        // decoded value may be a strict subset of the declared filter.
        if let Ok(Value::Object(m)) = &decoded {
            assert!(
                m.contains_key("tenantId"),
                "decode returned a NARROWED filter {m:?} - a mutation built \
                 from it would run against every tenant's rows"
            );
        }
    }

    /// The array arm had the same shape as the object arm: an element that
    /// could not be read was defaulted to `null`, silently substituting the
    /// caller's value. It must refuse instead.
    #[test]
    fn decode_must_not_default_an_unreadable_array_element_to_null() {
        init_v8();
        let mut isolate = v8::Isolate::new(v8::CreateParams::default());
        v8::scope!(let handle_scope, &mut isolate);
        let context = v8::Context::new(handle_scope, Default::default());
        let scope = &mut v8::ContextScope::new(handle_scope, context);

        let src = r#"(() => {
            const a = [1, 2, 3];
            Object.defineProperty(a, "1", { get() { throw new Error("boom"); } });
            return a;
        })()"#;
        let code = v8::String::new(scope, src).unwrap();
        let script = v8::Script::compile(scope, code, None).unwrap();
        let val = script.run(scope).unwrap();

        assert_eq!(
            v8_value_to_serde_json(scope, val),
            Err(DecodeError::PendingException),
            "an unreadable element must refuse, not become null"
        );
    }

    /// DBR-06: `new Array(4294967295)` is cheap and sparse in V8. The old array
    /// arm called `Vec::with_capacity(len)` on that attacker-controlled length
    /// and then walked every index. The breadth is now charged to the budget
    /// BEFORE reserving, so this returns promptly instead of trying to reserve
    /// billions of slots.
    #[test]
    fn decode_refuses_a_huge_sparse_array_without_allocating_it() {
        init_v8();
        let mut isolate = v8::Isolate::new(v8::CreateParams::default());
        v8::scope!(let handle_scope, &mut isolate);
        let context = v8::Context::new(handle_scope, Default::default());
        let scope = &mut v8::ContextScope::new(handle_scope, context);

        let code = v8::String::new(scope, "new Array(4294967295)").unwrap();
        let script = v8::Script::compile(scope, code, None).unwrap();
        let val = script.run(scope).unwrap();

        assert_eq!(
            v8_value_to_serde_json(scope, val),
            Err(DecodeError::Budget("node count")),
            "a sparse array past the node budget must refuse before allocating"
        );
    }
}
