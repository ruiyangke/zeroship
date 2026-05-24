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

use crate::query::SYSTEM_FIELD_NAMES;

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
}
