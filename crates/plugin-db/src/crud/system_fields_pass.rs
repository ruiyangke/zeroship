//! **P7 PR 3** — INSERT-time auto-population pass for the seven
//! platform-managed system fields.
//!
//! Sits between [`crate::crud::dispatch_insert`] (and `dispatch_insert_many`)
//! and the `query::build_insert*` SQL builders. Before validation, the
//! pass inspects the inbound row JSON and:
//!
//! 1. **`id`** — if absent, mints a fresh typed_id via
//!    [`zeroship_core::typed_id::generate`] using the
//!    [`prefix_for_collection`]-derived prefix. Creator-supplied `id`
//!    is honoured as-is (Q-SF-B: "Allow user-supplied IF the value
//!    passes typed_id format validation" — PR 3 ships the allow path;
//!    format validation can layer on later without a wire-format
//!    change).
//! 2. **`created_by` / `updated_by`** — if absent, populated from the
//!    request's authenticated user (the gateway-injected
//!    `ZeroShip-User` header; surfaced via
//!    `RuntimeState::per_request_user`). If no user is bound (system-
//!    initiated writes — migrations, background jobs, raw `serve` mode),
//!    the columns are left absent so the DB's `NULL` default fires.
//! 3. **`created_at` / `updated_at` / `version` / `deleted_at`** — NOT
//!    injected here. The PR 2 DDL emits `DEFAULT NOW()` /
//!    `DEFAULT CURRENT_TIMESTAMP` / `DEFAULT 1` / nullable, so a row
//!    INSERT that omits the column lets the engine populate the
//!    canonical value. Creator-supplied overrides for these fields
//!    flow through untouched (rare but legitimate for migration code
//!    that pre-seeds historical timestamps or a specific version).
//!
//! Rust-side minting (rather than SDK-side) is the design choice so
//! non-SDK deploys (raw `default = { fetch }` apps that call
//! `zeroship.db.*` directly) also receive auto-mint. The SDK reads the
//! minted id back from the INSERT's `RETURNING *` row.

use serde_json::{Map, Value};
use zeroship_runtime::state::SharedState;

use crate::error::DbError;
use crate::query::SYSTEM_FIELD_NAMES;

/// **P7 PR 4** — write-once system field names that the UPDATE pass
/// refuses to accept on the patch side. Even though all 7 names live in
/// [`SYSTEM_FIELD_NAMES`], only these 3 are immutable post-INSERT
/// (`updated_at` / `updated_by` / `version` are auto-bumped each
/// UPDATE — see `apply_system_fields_on_update`; `deleted_at` is
/// owned by `delete()` / `restore()` — PR 5).
pub(crate) const IMMUTABLE_SYSTEM_FIELDS: &[&str] = &["id", "created_at", "created_by"];

/// Maximum typed_id prefix length. Matches the convention used by
/// `crates/core/src/typed_id.rs` for well-known prefixes
/// (`usr`, `app`, `ses` — all 3 chars; we cap at 4 to allow `post`-
/// style collection-derived prefixes while keeping the typed_id
/// "<prefix>_<22 base62 chars>" shape compact).
const MAX_AUTO_PREFIX_LEN: usize = 4;

/// Derive a typed_id prefix from a collection name when the schema
/// does not declare one explicitly.
///
/// Algorithm (deterministic, no DB lookup):
///
/// 1. If the collection name ends in `s` AND has length > 1, strip the
///    trailing `s` — `posts` → `post`, `users` → `user`. (Common
///    English convention; matches what creators intuitively expect.)
/// 2. Truncate to the first [`MAX_AUTO_PREFIX_LEN`] ASCII characters.
/// 3. Lowercase (typed_id prefixes are conventionally lowercase
///    alphanumeric — matches the validator in
///    `crates/core/src/typed_id.rs`).
///
/// Empty / non-ASCII / pathological collection names fall back to
/// `"row"` so the prefix is always a valid typed_id segment.
///
/// PR 1 added the `t.id(prefix)` schema declaration (carried on the
/// field as `idPrefix`); when that's present the orchestrator-cached
/// schema can supply it. PR 3 reads that prefix when available and
/// falls back to this derivation otherwise — see
/// [`prefix_for_collection`].
pub(crate) fn derive_prefix_from_collection_name(collection: &str) -> String {
    let stem = if collection.len() > 1 && collection.ends_with('s') {
        &collection[..collection.len() - 1]
    } else {
        collection
    };
    let truncated: String = stem
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(MAX_AUTO_PREFIX_LEN)
        .collect::<String>()
        .to_ascii_lowercase();
    if truncated.is_empty() {
        "row".to_string()
    } else {
        truncated
    }
}

/// Resolve the typed_id prefix for a given `(app_id, collection)`.
///
/// 1. Consult the [`crate::context::IsolateDbContext`] schema cache for
///    a `t.id(prefix)`-declared `idPrefix` on the `id` field.
/// 2. Fall back to [`derive_prefix_from_collection_name`].
///
/// The cache lookup is cheap (a `HashMap` keyed by `"{app_id}:{collection}"`)
/// and the schema is the same one [`crate::crud::encryption_pass`]
/// reads — no extra wire calls.
pub(crate) fn prefix_for_collection(app_id: &str, collection: &str) -> String {
    let from_schema = crate::context::with(|ctx| {
        ctx.schema_for(app_id, collection).and_then(|schema| {
            schema
                .get("id")
                .and_then(|id_def| id_def.get("idPrefix"))
                .and_then(|p| p.as_str())
                .map(|s| s.to_string())
        })
    });
    from_schema.unwrap_or_else(|| derive_prefix_from_collection_name(collection))
}

/// Look up the current request's authenticated actor id (typed_id
/// string), if any.
///
/// Reads the runtime's `per_request_user` slot using the request id
/// currently bound by the pump (see `crates/runtime/src/auth.rs` for
/// the wire contract). The user JSON shape is gateway-defined and
/// carries at minimum `{ "id": "usr_..." }` for an authenticated
/// user; we extract the `id` field and discard the rest (Q-SF-A:
/// "typed_id only" for `created_by` — only the id flows to the row,
/// not the role / display name / etc.).
///
/// Returns `None` when no request is bound (module init, raw
/// background dispatch), when no user is attached to the request
/// (anonymous), or when the user JSON is malformed. NULL is the
/// design choice for `created_by` in that case (§2.3 of the
/// proposal); the column is nullable so the INSERT succeeds.
pub(crate) fn current_actor_id(state: &SharedState) -> Option<String> {
    let s = state.borrow();
    let rid = s.executing_request_id?;
    let user_json = s.per_request_user.get(&rid)?;
    let parsed: Value = serde_json::from_str(user_json).ok()?;
    parsed
        .get("id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

/// Run the auto-population pass over a single insert document.
///
/// Mutates `doc` in place when it is a JSON object. Non-object docs
/// pass through untouched — the downstream `build_insert` will return
/// a typed `InvalidFilter` error for those.
///
/// Idempotent: calling this twice on the same doc is a no-op the
/// second time (every check is "field absent → inject").
///
/// Visibility: `pub(crate)` in release builds; `pub` under
/// `test-helpers` so the integration tests can drive the helper
/// directly without standing up V8.
#[cfg(not(feature = "test-helpers"))]
pub(crate) fn apply_system_fields_on_insert(
    doc: &mut Value,
    app_id: &str,
    collection: &str,
    actor_id: Option<&str>,
) {
    apply_system_fields_on_insert_impl(doc, app_id, collection, actor_id);
}

#[cfg(feature = "test-helpers")]
pub fn apply_system_fields_on_insert(
    doc: &mut Value,
    app_id: &str,
    collection: &str,
    actor_id: Option<&str>,
) {
    apply_system_fields_on_insert_impl(doc, app_id, collection, actor_id);
}

fn apply_system_fields_on_insert_impl(
    doc: &mut Value,
    app_id: &str,
    collection: &str,
    actor_id: Option<&str>,
) {
    let Some(obj) = doc.as_object_mut() else {
        return;
    };
    inject_into_object(obj, app_id, collection, actor_id);
}

/// Run the auto-population pass over every doc in an `insertMany`
/// array. Each doc gets its own freshly-minted `id`; the actor stamp
/// is the same for every doc in the batch (the batch is issued under
/// one request context).
///
/// Same visibility rationale as [`apply_system_fields_on_insert`].
#[cfg(not(feature = "test-helpers"))]
pub(crate) fn apply_system_fields_on_insert_many(
    docs: &mut Value,
    app_id: &str,
    collection: &str,
    actor_id: Option<&str>,
) {
    apply_system_fields_on_insert_many_impl(docs, app_id, collection, actor_id);
}

#[cfg(feature = "test-helpers")]
pub fn apply_system_fields_on_insert_many(
    docs: &mut Value,
    app_id: &str,
    collection: &str,
    actor_id: Option<&str>,
) {
    apply_system_fields_on_insert_many_impl(docs, app_id, collection, actor_id);
}

fn apply_system_fields_on_insert_many_impl(
    docs: &mut Value,
    app_id: &str,
    collection: &str,
    actor_id: Option<&str>,
) {
    let Some(arr) = docs.as_array_mut() else {
        return;
    };
    for doc in arr.iter_mut() {
        if let Some(obj) = doc.as_object_mut() {
            inject_into_object(obj, app_id, collection, actor_id);
        }
    }
}

fn inject_into_object(
    obj: &mut Map<String, Value>,
    app_id: &str,
    collection: &str,
    actor_id: Option<&str>,
) {
    // `id` — auto-mint when absent.
    if !obj.contains_key("id") {
        let prefix = prefix_for_collection(app_id, collection);
        let minted = zeroship_core::typed_id::generate(&prefix);
        obj.insert("id".to_string(), Value::String(minted));
    }

    // `created_by` / `updated_by` — only inject when an actor is in
    // scope AND the column isn't already set. No actor → leave absent
    // so the DDL's `NULL` default fires.
    if let Some(actor) = actor_id {
        if !obj.contains_key("created_by") {
            obj.insert("created_by".to_string(), Value::String(actor.to_string()));
        }
        if !obj.contains_key("updated_by") {
            obj.insert("updated_by".to_string(), Value::String(actor.to_string()));
        }
    }

    // `created_at` / `updated_at` / `version` / `deleted_at` are
    // intentionally NOT injected — the DB default fires when absent,
    // creator overrides flow through when present. See
    // `build_system_field_columns` in `query.rs` for the DDL defaults.
    //
    // `SYSTEM_FIELD_NAMES` is referenced here only via the
    // `debug_assert!` below — every value we touch must be in the
    // canonical list, otherwise a future emitter regression silently
    // writes to a creator-shaped column.
    debug_assert!(
        ["id", "created_by", "updated_by"]
            .iter()
            .all(|n| SYSTEM_FIELD_NAMES.contains(n)),
        "apply_system_fields_on_insert touched a name not in SYSTEM_FIELD_NAMES",
    );
}

// ---------------------------------------------------------------------------
// **P7 PR 4** — UPDATE-time validation pass + CAS-version extraction.
// ---------------------------------------------------------------------------

/// Result of running [`apply_system_fields_on_update`] against an
/// UPDATE patch. Carries the post-validation knobs the SQL builder
/// needs to compose the auto-bump SET clauses correctly.
///
/// `creator_supplied_version` / `creator_supplied_updated_at` /
/// `creator_supplied_updated_by` cover Q-SF-B's "creator can override"
/// rule for the three auto-bumped columns: when set, the builder must
/// emit the creator's value verbatim AND skip the corresponding
/// auto-bump SET clause (the explicit value wins).
#[derive(Debug, Clone, Default)]
pub struct UpdateAutoBumpHints {
    /// `true` when the patch carries an explicit `version` key. The
    /// builder must NOT append `"version" = "version" + 1` (the
    /// creator's value flows through the standard SET clause).
    pub creator_supplied_version: bool,
    /// `true` when the patch carries an explicit `updated_at`. Same
    /// rationale — the builder skips the dialect-appropriate
    /// `NOW()` / `CURRENT_TIMESTAMP` auto-bump.
    pub creator_supplied_updated_at: bool,
    /// `true` when the patch carries an explicit `updated_by`. Builder
    /// skips appending the actor-bound `$N` placeholder.
    pub creator_supplied_updated_by: bool,
}

/// Run the UPDATE-time validation pass over an UPDATE patch.
///
/// Two rejections, one inspection:
///
/// 1. **Immutable fields** — the patch must not carry `id`,
///    `created_at`, or `created_by`. Those are auto-populated at
///    INSERT (PR 3) and write-once. Returns a typed
///    `DbError::ValidationFailed { code: "immutable_system_field" }`
///    via `QueryError::ImmutableSystemField`.
/// 2. **Creator-supplied overrides** — `version` / `updated_at` /
///    `updated_by` are inspected (not refused). The returned
///    [`UpdateAutoBumpHints`] tells the SQL builder whether to skip
///    each auto-bump.
///
/// The pass also strips `$set`-flattened patches so the immutable-field
/// check covers both top-level keys and nested `$set` entries.
///
/// Visibility: `pub(crate)` in release builds; `pub` under
/// `test-helpers` so integration tests can drive the helper directly.
#[cfg(not(feature = "test-helpers"))]
pub(crate) fn apply_system_fields_on_update(
    patch: &Value,
    app_id: &str,
    collection: &str,
) -> Result<UpdateAutoBumpHints, DbError> {
    apply_system_fields_on_update_impl(patch, app_id, collection)
}

#[cfg(feature = "test-helpers")]
pub fn apply_system_fields_on_update(
    patch: &Value,
    app_id: &str,
    collection: &str,
) -> Result<UpdateAutoBumpHints, DbError> {
    apply_system_fields_on_update_impl(patch, app_id, collection)
}

fn apply_system_fields_on_update_impl(
    patch: &Value,
    _app_id: &str,
    _collection: &str,
) -> Result<UpdateAutoBumpHints, DbError> {
    let Some(obj) = patch.as_object() else {
        // Non-object patches are the SQL builder's problem (they get a
        // typed `InvalidFilter` there). The pass has nothing to do.
        return Ok(UpdateAutoBumpHints::default());
    };

    // The patch can carry top-level keys AND a nested `$set`. Inspect
    // both so creator overrides + immutability checks cover either
    // shape uniformly with the SQL builder's `build_set_clauses`
    // flattening.
    let mut hints = UpdateAutoBumpHints::default();
    check_keys_for_immutable_and_overrides(obj, &mut hints)?;
    if let Some(set_obj) = obj.get("$set").and_then(|v| v.as_object()) {
        check_keys_for_immutable_and_overrides(set_obj, &mut hints)?;
    }
    // Defensive: pre-PR-4 SDK versions augmented the update with
    // `$inc: { version: 1 }`. After PR 4 the runtime auto-bumps too —
    // a naive double-bump would advance version by 2 instead of 1.
    // Detect the legacy operator-nested version key on `$inc` / `$dec`
    // / `$mul` and suppress our own bump so the creator's explicit
    // operator wins (matching the `Q-SF-B` "creator override" rule).
    for op_key in &["$inc", "$dec", "$mul"] {
        if let Some(op_obj) = obj.get(*op_key).and_then(|v| v.as_object()) {
            if op_obj.contains_key("version") {
                hints.creator_supplied_version = true;
            }
            if op_obj.contains_key("updated_at") {
                hints.creator_supplied_updated_at = true;
            }
            if op_obj.contains_key("updated_by") {
                hints.creator_supplied_updated_by = true;
            }
            // Immutable fields under an operator must also be refused.
            for imm in IMMUTABLE_SYSTEM_FIELDS {
                if op_obj.contains_key(*imm) {
                    return Err(crate::query::QueryError::ImmutableSystemField(
                        format!(
                            "UPDATE patch attempted to overwrite immutable system field `{imm}` \
                             under `{op_key}` (write-once on INSERT)"
                        ),
                    )
                    .into());
                }
            }
        }
    }
    Ok(hints)
}

/// Run the immutable-field + creator-override inspection over a single
/// key/value map (called once for the top-level patch and again for any
/// `$set` nesting).
fn check_keys_for_immutable_and_overrides(
    obj: &Map<String, Value>,
    hints: &mut UpdateAutoBumpHints,
) -> Result<(), DbError> {
    for (key, _value) in obj.iter() {
        if key.starts_with('$') {
            // Skip operator keys like `$set` / `$inc` / `$push` — the
            // SQL builder unpacks them. The nested-`$set` branch in
            // the caller covers `$set`-nested immutable-field attempts.
            continue;
        }
        if IMMUTABLE_SYSTEM_FIELDS.contains(&key.as_str()) {
            return Err(crate::query::QueryError::ImmutableSystemField(
                format!(
                    "UPDATE patch attempted to overwrite immutable system field `{key}` \
                     (write-once on INSERT)"
                ),
            )
            .into());
        }
        match key.as_str() {
            "version" => hints.creator_supplied_version = true,
            "updated_at" => hints.creator_supplied_updated_at = true,
            "updated_by" => hints.creator_supplied_updated_by = true,
            _ => {}
        }
    }
    Ok(())
}

/// Extract a creator-supplied top-level `version: N` predicate from a
/// filter for the optimistic-concurrency check. Returns `Ok(None)` when:
///
/// - the filter is not a JSON object (the SQL builder will reject it),
/// - the filter has no `version` key,
/// - the `version` value is not a finite integer (operator objects
///   like `{ $gt: 5 }` short-circuit to `None` — CAS only honours a
///   plain equality predicate).
///
/// Returns `Err(version_filter_must_be_top_level)` when a `version`
/// predicate appears under a top-level `$and` / `$or` combinator. The
/// dispatch layer would otherwise silently treat that shape as "no CAS
/// filter", degrading the write to last-writer-wins.
///
/// Used by both `dispatch_update_one` and `dispatch_update_many` to
/// decide whether to surface a `version_mismatch` typed error when the
/// affected-rows count comes back zero.
pub(crate) fn extract_cas_version(
    filter: &Value,
    collection: &str,
) -> Result<Option<i64>, DbError> {
    if filter_has_nested_version_predicate(filter) {
        return Err(DbError::version_filter_must_be_top_level(collection));
    }
    let Some(obj) = filter.as_object() else {
        return Ok(None);
    };
    let Some(v) = obj.get("version") else {
        return Ok(None);
    };
    // Reject operator objects ({ $gt, $in, ... }) — only a plain
    // equality predicate carries CAS semantics. `as_i64` also rejects
    // floats and strings, which is the desired strictness.
    Ok(v.as_i64())
}

/// Detect whether a filter carries an `id` predicate. Used to refuse
/// the unsupported "multi-row UPDATE with `version` filter but no `id`"
/// case eagerly with a typed `multi_row_version_filter_unsupported`
/// error — the CAS semantics don't generalise to multi-row UPDATEs.
///
/// "Has an `id`" means the filter object contains a top-level `id` key
/// (either a scalar equality, an `$in`, or an operator object — any of
/// these narrow the UPDATE to a per-PK lookup). We don't try to walk
/// `$and` / `$or` combinators; the SDK's typical CAS update is
/// `{ id, version }` and that's what we optimise for.
pub(crate) fn filter_has_id_predicate(filter: &Value) -> bool {
    filter
        .as_object()
        .map(|o| o.contains_key("id"))
        .unwrap_or(false)
}

fn filter_has_nested_version_predicate(filter: &Value) -> bool {
    fn combinator_contains_field(value: &Value, field: &str) -> bool {
        match value {
            Value::Array(items) => items.iter().any(|item| object_contains_field(item, field)),
            Value::Object(_) => object_contains_field(value, field),
            _ => false,
        }
    }

    fn object_contains_field(value: &Value, field: &str) -> bool {
        let Some(obj) = value.as_object() else {
            return false;
        };
        if obj.contains_key(field) {
            return true;
        }
        obj.iter().any(|(key, nested)| {
            matches!(key.as_str(), "$and" | "$or")
                && combinator_contains_field(nested, field)
        })
    }

    filter
        .as_object()
        .map(|obj| {
            obj.iter().any(|(key, nested)| {
                matches!(key.as_str(), "$and" | "$or")
                    && combinator_contains_field(nested, "version")
            })
        })
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// **P7 PR 5** — soft-delete dispatch helpers.
//
// `delete()` is implemented as a soft-delete `UPDATE ... SET deleted_at
// = NOW()`. `purge()` remains the explicit hard-delete escape hatch,
// and `restore()` clears `deleted_at`.
//
// Two new methods land in this PR — `purge` (explicit hard-delete) and
// `restore` (clear `deleted_at`). Both are direct verbs in the SDK and
// reach the dispatch layer through their own helpers below.
// ---------------------------------------------------------------------------

/// **P7 PR 5** — should this collection's SELECTs auto-append
/// `AND deleted_at IS NULL`?
///
/// When `include_deleted: true` is set, the caller explicitly opts out
/// of the platform's default soft-delete filter.
pub(crate) fn should_filter_soft_deleted(
    include_deleted: bool,
) -> bool {
    !include_deleted
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ---- prefix derivation -----------------------------------------

    #[test]
    fn derive_prefix_strips_trailing_s() {
        assert_eq!(derive_prefix_from_collection_name("posts"), "post");
        assert_eq!(derive_prefix_from_collection_name("users"), "user");
        assert_eq!(derive_prefix_from_collection_name("orders"), "orde");
    }

    #[test]
    fn derive_prefix_keeps_short_names() {
        assert_eq!(derive_prefix_from_collection_name("s"), "s");
        assert_eq!(derive_prefix_from_collection_name(""), "row");
    }

    #[test]
    fn derive_prefix_truncates_to_four() {
        assert_eq!(derive_prefix_from_collection_name("invoices"), "invo");
        assert_eq!(derive_prefix_from_collection_name("authentications"), "auth");
    }

    #[test]
    fn derive_prefix_falls_back_to_row_on_non_ascii() {
        // "café" → after strip-s (no change) → ASCII-filter strips
        // the é → "caf" — still valid. A purely-non-ASCII name
        // collapses to "row".
        assert_eq!(derive_prefix_from_collection_name("café"), "caf");
        assert_eq!(derive_prefix_from_collection_name("日本語"), "row");
    }

    #[test]
    fn derive_prefix_lowercases() {
        assert_eq!(derive_prefix_from_collection_name("Posts"), "post");
        assert_eq!(derive_prefix_from_collection_name("USERS"), "user");
    }

    // ---- single-doc injection --------------------------------------

    #[test]
    fn insert_auto_mints_id_when_absent() {
        let mut doc = json!({ "title": "hi" });
        apply_system_fields_on_insert(&mut doc, "app1", "posts", None);
        let id = doc
            .get("id")
            .and_then(|v| v.as_str())
            .expect("id must be minted");
        assert!(
            id.starts_with("post_"),
            "expected 'post_' prefix for posts collection, got {id}"
        );
        // typed_id is `<prefix>_<22 base62 chars>` — total 5 + 22.
        assert_eq!(id.len(), "post_".len() + 22);
    }

    #[test]
    fn insert_respects_creator_supplied_id() {
        let mut doc = json!({ "id": "post_abc123", "title": "hi" });
        apply_system_fields_on_insert(&mut doc, "app1", "posts", None);
        assert_eq!(
            doc.get("id").and_then(|v| v.as_str()),
            Some("post_abc123"),
            "creator-supplied id must pass through untouched"
        );
    }

    #[test]
    fn insert_minted_id_has_correct_prefix_for_collection_name() {
        let mut doc = json!({});
        apply_system_fields_on_insert(&mut doc, "app1", "users", None);
        assert!(doc.get("id").unwrap().as_str().unwrap().starts_with("user_"));

        let mut doc2 = json!({});
        apply_system_fields_on_insert(&mut doc2, "app1", "tasks", None);
        assert!(doc2.get("id").unwrap().as_str().unwrap().starts_with("task_"));
    }

    #[test]
    fn insert_populates_created_by_from_actor() {
        let mut doc = json!({ "title": "hi" });
        apply_system_fields_on_insert(&mut doc, "app1", "posts", Some("usr_actor1"));
        assert_eq!(
            doc.get("created_by").and_then(|v| v.as_str()),
            Some("usr_actor1")
        );
        assert_eq!(
            doc.get("updated_by").and_then(|v| v.as_str()),
            Some("usr_actor1"),
            "updated_by must equal created_by on INSERT"
        );
    }

    #[test]
    fn insert_leaves_created_by_absent_when_no_actor() {
        let mut doc = json!({ "title": "hi" });
        apply_system_fields_on_insert(&mut doc, "app1", "posts", None);
        assert!(
            !doc.as_object().unwrap().contains_key("created_by"),
            "no actor → no created_by injection (DB default NULL fires)"
        );
        assert!(
            !doc.as_object().unwrap().contains_key("updated_by"),
            "no actor → no updated_by injection (DB default NULL fires)"
        );
    }

    #[test]
    fn insert_respects_creator_supplied_created_by() {
        let mut doc = json!({
            "title": "hi",
            "created_by": "usr_override",
        });
        apply_system_fields_on_insert(&mut doc, "app1", "posts", Some("usr_session_actor"));
        // Creator's explicit value wins over the session actor — same
        // pattern as `id` above (Q-SF-B).
        assert_eq!(
            doc.get("created_by").and_then(|v| v.as_str()),
            Some("usr_override")
        );
    }

    #[test]
    fn insert_does_not_inject_created_at_or_updated_at_or_version() {
        let mut doc = json!({ "title": "hi" });
        apply_system_fields_on_insert(&mut doc, "app1", "posts", Some("usr_x"));
        let obj = doc.as_object().unwrap();
        assert!(!obj.contains_key("created_at"), "DB default must fire");
        assert!(!obj.contains_key("updated_at"), "DB default must fire");
        assert!(!obj.contains_key("version"), "DB default must fire");
        assert!(!obj.contains_key("deleted_at"), "DB default must fire");
    }

    #[test]
    fn insert_respects_creator_supplied_version() {
        let mut doc = json!({
            "title": "hi",
            "version": 5,
            "created_at": 1700000000000_i64,
        });
        apply_system_fields_on_insert(&mut doc, "app1", "posts", None);
        // Creator-supplied overrides for the DB-defaulted columns flow
        // through untouched — migration code uses this to pre-seed
        // historical timestamps + version pointers.
        assert_eq!(doc.get("version").and_then(|v| v.as_i64()), Some(5));
        assert_eq!(
            doc.get("created_at").and_then(|v| v.as_i64()),
            Some(1700000000000)
        );
    }

    #[test]
    fn insert_pass_is_idempotent() {
        let mut doc = json!({ "title": "hi" });
        apply_system_fields_on_insert(&mut doc, "app1", "posts", Some("usr_x"));
        let id_after_first = doc.get("id").unwrap().as_str().unwrap().to_string();
        apply_system_fields_on_insert(&mut doc, "app1", "posts", Some("usr_y"));
        // Second pass must NOT re-mint id and must NOT overwrite
        // created_by — pass is "inject when absent".
        assert_eq!(
            doc.get("id").unwrap().as_str(),
            Some(id_after_first.as_str())
        );
        assert_eq!(
            doc.get("created_by").and_then(|v| v.as_str()),
            Some("usr_x")
        );
    }

    #[test]
    fn insert_pass_non_object_doc_is_no_op() {
        let mut doc = json!("not an object");
        apply_system_fields_on_insert(&mut doc, "app1", "posts", Some("usr_x"));
        // Non-object docs pass through unchanged — the downstream
        // build_insert will reject them with a typed error.
        assert_eq!(doc, json!("not an object"));
    }

    // ---- insertMany batch ------------------------------------------

    #[test]
    fn insert_many_pass_mints_id_per_row() {
        let mut docs = json!([
            { "title": "a" },
            { "title": "b" },
        ]);
        apply_system_fields_on_insert_many(&mut docs, "app1", "posts", Some("usr_x"));
        let arr = docs.as_array().unwrap();
        let id_a = arr[0].get("id").and_then(|v| v.as_str()).unwrap();
        let id_b = arr[1].get("id").and_then(|v| v.as_str()).unwrap();
        assert!(id_a.starts_with("post_"));
        assert!(id_b.starts_with("post_"));
        assert_ne!(id_a, id_b, "every row in the batch gets its own id");
        // All rows share the same actor stamp.
        assert_eq!(
            arr[0].get("created_by").and_then(|v| v.as_str()),
            Some("usr_x")
        );
        assert_eq!(
            arr[1].get("created_by").and_then(|v| v.as_str()),
            Some("usr_x")
        );
    }

    #[test]
    fn insert_many_pass_respects_per_row_supplied_id() {
        let mut docs = json!([
            { "id": "post_keepme", "title": "a" },
            { "title": "b" },
        ]);
        apply_system_fields_on_insert_many(&mut docs, "app1", "posts", None);
        let arr = docs.as_array().unwrap();
        assert_eq!(
            arr[0].get("id").and_then(|v| v.as_str()),
            Some("post_keepme")
        );
        assert!(arr[1]
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap()
            .starts_with("post_"));
    }

    // ---- INSERT SQL builder integration -----------------------------

    /// **Brief-required**: every system field reaches the INSERT VALUES
    /// when populated (id always; created_by / updated_by when actor
    /// present). The DB DEFAULTs handle the rest.
    #[test]
    fn insert_pass_emits_three_extra_columns_when_actor_present() {
        let mut doc = json!({ "title": "hi" });
        apply_system_fields_on_insert(&mut doc, "app1", "posts", Some("usr_x"));
        let obj = doc.as_object().unwrap();
        // The 4 columns reaching INSERT: user-declared title + the 3
        // auto-injected system fields (id + created_by + updated_by).
        assert_eq!(obj.len(), 4, "doc keys: {:?}", obj.keys().collect::<Vec<_>>());
        for required in &["id", "title", "created_by", "updated_by"] {
            assert!(
                obj.contains_key(*required),
                "expected {required} in doc, got: {:?}",
                obj.keys().collect::<Vec<_>>()
            );
        }
    }

    /// **Brief-required**: when no actor is bound the INSERT VALUES
    /// must NOT carry created_by / updated_by so the DDL's
    /// `created_by TEXT NULL` default fires.
    #[test]
    fn insert_pass_omits_actor_columns_when_no_actor() {
        let mut doc = json!({ "title": "hi" });
        apply_system_fields_on_insert(&mut doc, "app1", "posts", None);
        let obj = doc.as_object().unwrap();
        // Only id is injected (auto-mint always runs).
        assert_eq!(obj.len(), 2, "doc keys: {:?}", obj.keys().collect::<Vec<_>>());
        assert!(obj.contains_key("id"));
        assert!(obj.contains_key("title"));
        assert!(!obj.contains_key("created_by"));
        assert!(!obj.contains_key("updated_by"));
    }

    /// **Brief-required**: confirm the SQL builder integration — the
    /// INSERT statement carries all 4 columns and uses RETURNING * so
    /// the SDK gets every system field back (DDL DEFAULTs included).
    #[test]
    fn insert_pass_followed_by_build_insert_emits_returning_star() {
        use crate::query::build_insert;
        let mut doc = json!({ "title": "hi" });
        apply_system_fields_on_insert(&mut doc, "app1", "posts", Some("usr_x"));
        let built = build_insert("app1", "posts", &doc).expect("build_insert");
        // RETURNING * pulls every column back — that's the contract the
        // SDK relies on to populate `Row<S>` with the DB-defaulted
        // timestamps + version.
        assert!(
            built.sql.contains("RETURNING *"),
            "INSERT must use RETURNING *, got: {}",
            built.sql
        );
        // Parameter count: 4 columns (id, title, created_by, updated_by).
        assert_eq!(built.params.len(), 4, "params: {:?}", built.params);
    }

    // ---- P7 PR 4 — UPDATE pass: immutable fields, CAS extraction --

    #[test]
    fn update_refuses_creator_supplied_id_change() {
        let patch = json!({ "id": "post_other", "title": "x" });
        let err = apply_system_fields_on_update(&patch, "app1", "posts_imid")
            .expect_err("UPDATE must refuse id overwrite");
        match err {
            DbError::ValidationFailed { code, .. } => {
                assert_eq!(code, "immutable_system_field");
            }
            other => panic!("expected ValidationFailed, got {other:?}"),
        }
    }

    #[test]
    fn update_refuses_creator_supplied_created_at_change() {
        let patch = json!({ "created_at": 1700000000000_i64 });
        let err = apply_system_fields_on_update(&patch, "app1", "posts_imca")
            .expect_err("UPDATE must refuse created_at overwrite");
        match err {
            DbError::ValidationFailed { code, .. } => {
                assert_eq!(code, "immutable_system_field");
            }
            other => panic!("expected ValidationFailed, got {other:?}"),
        }
    }

    #[test]
    fn update_refuses_creator_supplied_created_by_change() {
        let patch = json!({ "created_by": "usr_other" });
        let err = apply_system_fields_on_update(&patch, "app1", "posts_imcb")
            .expect_err("UPDATE must refuse created_by overwrite");
        match err {
            DbError::ValidationFailed { code, .. } => {
                assert_eq!(code, "immutable_system_field");
            }
            other => panic!("expected ValidationFailed, got {other:?}"),
        }
    }

    #[test]
    fn update_refuses_immutable_fields_under_dollar_set() {
        // Nested $set form must be caught too — the SDK can produce
        // either shape.
        let patch = json!({ "$set": { "id": "post_other" } });
        let err = apply_system_fields_on_update(&patch, "app1", "posts_imset")
            .expect_err("UPDATE must refuse id overwrite inside $set");
        match err {
            DbError::ValidationFailed { code, .. } => {
                assert_eq!(code, "immutable_system_field");
            }
            other => panic!("expected ValidationFailed, got {other:?}"),
        }
    }

    #[test]
    fn update_respects_creator_supplied_version() {
        let patch = json!({ "title": "x", "version": 42 });
        let hints = apply_system_fields_on_update(&patch, "app1", "posts_csv")
            .expect("passes immutable check");
        assert!(
            hints.creator_supplied_version,
            "explicit version on patch must set the hint"
        );
    }

    #[test]
    fn update_respects_creator_supplied_updated_at() {
        let patch = json!({ "title": "x", "updated_at": "2026-01-01T00:00:00Z" });
        let hints = apply_system_fields_on_update(&patch, "app1", "posts_csua")
            .expect("passes immutable check");
        assert!(hints.creator_supplied_updated_at);
    }

    #[test]
    fn update_respects_creator_supplied_updated_by() {
        let patch = json!({ "title": "x", "updated_by": "usr_explicit" });
        let hints = apply_system_fields_on_update(&patch, "app1", "posts_csub")
            .expect("passes immutable check");
        assert!(hints.creator_supplied_updated_by);
    }

    #[test]
    fn update_detects_legacy_dollar_inc_version_as_creator_supplied() {
        // Pre-PR-4 SDK shape: `{ $inc: { version: 1 } }`. The pass must
        // set `creator_supplied_version` so the SQL builder skips its
        // own auto-bump (otherwise a double-bump lands version at +2).
        let patch = json!({ "$inc": { "version": 1 } });
        let hints = apply_system_fields_on_update(&patch, "app1", "posts_legacyinc")
            .expect("passes");
        assert!(
            hints.creator_supplied_version,
            "legacy $inc.version must mark creator-supplied to avoid double-bump"
        );
    }

    #[test]
    fn update_refuses_immutable_field_under_dollar_inc() {
        // Defence-in-depth: $inc.id / $inc.created_at / $inc.created_by
        // should be refused for the same reason as top-level overwrites.
        let patch = json!({ "$inc": { "created_by": 1 } });
        let err = apply_system_fields_on_update(&patch, "app1", "posts_immut_inc")
            .expect_err("immutable field under $inc must refuse");
        match err {
            DbError::ValidationFailed { code, .. } => {
                assert_eq!(code, "immutable_system_field");
            }
            other => panic!("expected ValidationFailed, got {other:?}"),
        }
    }

    #[test]
    fn update_creator_hints_default_to_false() {
        let patch = json!({ "title": "x" });
        let hints = apply_system_fields_on_update(&patch, "app1", "posts_defaults")
            .expect("passes");
        assert!(!hints.creator_supplied_version);
        assert!(!hints.creator_supplied_updated_at);
        assert!(!hints.creator_supplied_updated_by);
    }

    #[test]
    fn extract_cas_version_returns_plain_number() {
        let f = json!({ "id": "post_x", "version": 7 });
        assert_eq!(extract_cas_version(&f, "posts").unwrap(), Some(7));
    }

    #[test]
    fn extract_cas_version_returns_none_for_missing_version() {
        let f = json!({ "id": "post_x" });
        assert_eq!(extract_cas_version(&f, "posts").unwrap(), None);
    }

    #[test]
    fn extract_cas_version_returns_none_for_operator_object() {
        // `{ $gt: 5 }` is not a CAS predicate.
        let f = json!({ "version": { "$gt": 5 } });
        assert_eq!(extract_cas_version(&f, "posts").unwrap(), None);
    }

    #[test]
    fn extract_cas_version_returns_none_for_string_value() {
        let f = json!({ "version": "7" });
        assert_eq!(extract_cas_version(&f, "posts").unwrap(), None);
    }

    #[test]
    fn extract_cas_version_returns_none_for_non_object_filter() {
        let f = json!("scalar");
        assert_eq!(extract_cas_version(&f, "posts").unwrap(), None);
    }

    #[test]
    fn extract_cas_version_rejects_nested_and_version() {
        let f = json!({
            "$and": [
                { "id": "post_x" },
                { "version": 7 }
            ]
        });
        let err = extract_cas_version(&f, "posts").expect_err("nested CAS must refuse");
        match err {
            DbError::ValidationFailed { code, .. } => {
                assert_eq!(code, "version_filter_must_be_top_level");
            }
            other => panic!("expected ValidationFailed, got {other:?}"),
        }
    }

    #[test]
    fn extract_cas_version_rejects_deeply_nested_or_version() {
        let f = json!({
            "$and": [
                {
                    "$or": [
                        { "version": 7 },
                        { "title": "x" }
                    ]
                },
                { "id": "post_x" }
            ]
        });
        let err = extract_cas_version(&f, "posts").expect_err("nested CAS must refuse");
        match err {
            DbError::ValidationFailed { code, .. } => {
                assert_eq!(code, "version_filter_must_be_top_level");
            }
            other => panic!("expected ValidationFailed, got {other:?}"),
        }
    }

    #[test]
    fn filter_has_id_predicate_detects_scalar() {
        assert!(filter_has_id_predicate(&json!({ "id": "post_x" })));
    }

    #[test]
    fn filter_has_id_predicate_detects_operator() {
        // `id: { $in: [...] }` still narrows to id-keyed lookups.
        assert!(filter_has_id_predicate(&json!({ "id": { "$in": ["a", "b"] } })));
    }

    #[test]
    fn filter_has_id_predicate_false_when_absent() {
        assert!(!filter_has_id_predicate(&json!({ "title": "x" })));
        assert!(!filter_has_id_predicate(&json!({})));
        assert!(!filter_has_id_predicate(&json!("scalar")));
    }

    // ---- P7 PR 5 — soft-delete filter gate -----

    #[test]
    fn should_filter_soft_deleted_by_default() {
        assert!(should_filter_soft_deleted(false));
    }

    #[test]
    fn should_filter_soft_deleted_false_when_include_deleted_true() {
        assert!(!should_filter_soft_deleted(true));
    }
}
