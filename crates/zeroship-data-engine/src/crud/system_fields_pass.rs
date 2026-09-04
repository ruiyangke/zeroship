//! The write-time assignment pass: the platform's own columns, computed by the
//! platform.
//!
//! **This module names no platform column on the write side.** It iterates the
//! operator charter (`policies/confined-system-shape.inject.toml`, compiled into
//! the worker and projected by [`crate::system_shape_charter::AssignmentPlan`])
//! and invokes the generator each column declares.
//!
//! # What that does NOT mean, measured 2026-09-01
//!
//! An eighth charter column is a charter line for THIS pass, and for the DDL
//! (the migration engine resolves the same fragment). It is **not** a charter
//! line for the whole system, and the boundary is worth stating exactly because
//! the next person to add one will trust this comment.
//!
//! The READ projection still holds a second, hand-maintained list:
//! `zeroship_schema::query::SYSTEM_FIELD_NAMES` (`crates/zeroship-schema/src/query.rs:756`),
//! walked by `implicit_read_projection_parts` (`:3644`) - which builds the
//! SELECT list for every read builder AND the `RETURNING` list for every write
//! (`build_returning_expr`, `:3685`) - and by `read_surface_columns` (`:3746`),
//! the predicate that narrows every row reaching a creator. Nothing binds that
//! list to this charter; the two agree because both were written to the same
//! seven names.
//!
//! What that costs is NOT that an eighth column becomes unreadable. Measured on
//! `build_find_with_schema`: a descriptor declaring `tenant_id` projects
//! `..., "version", "deleted_at", "title", "tenant_id"`, because the same
//! function's second loop projects every readable descriptor key. Real
//! descriptors carry the injected columns as ordinary fields
//! (`examples/db-todos/generated/zeroship/schema.runtime.json:6-45`, all
//! `readable: true`), and they are generated from this same charter fragment
//! (`sdks/vite-plugin/src/gen-types/index.ts:322`).
//!
//! What it costs is that the seven are projected UNCONDITIONALLY from the const
//! while an eighth would be projected only through the descriptor - and the
//! descriptor is creator-authored (`zeroship-migrate-server`'s `apply.rs:61-65`).
//! So `created_at` cannot be hidden by a hand-edited descriptor and an eighth
//! column could be, by dropping the key or marking it `readable: false`
//! (measured: `readable: false` removes it from the SELECT while the seven stay).
//! An eighth charter column would not inherit that unhideability until the read
//! projection reads the charter too.
//!
//! # The shape it reads
//!
//! Every charter column carries one property, `assign = { by, on }`: WHO
//! computes the value and WHEN. `on` is `insert` | `write` | `delete`, and
//! `write` covers insert - `updated_by` is `on = "write"` and must still be
//! stamped when the row is created.
//!
//! # What the pass emits, and what it leaves to the database
//!
//! The runtime emits a value only for the generators the DATABASE cannot
//! compute:
//!
//! * `typedId` - minted here via [`zeroship_core::typed_id::generate`] with the
//!   [`prefix_for_collection`]-resolved prefix, because the prefix is
//!   per-collection creator data the charter deliberately does not carry.
//! * `actor` - the request's authenticated user, or **`null`** when there is
//!   none. The generator runs on every write; stale attribution is a false
//!   claim about who touched a row.
//!
//! `now`, `increment(n)` and `identity` emit NOTHING. Their value is produced by
//! the column's own DDL - `DEFAULT NOW()` / `DEFAULT CURRENT_TIMESTAMP`,
//! `DEFAULT 1`, and the identity sequence - which is the same authority
//! rendering the same expression the UPDATE auto-bump uses. That is not a gap:
//! it is what keeps ONE writer and ONE spelling per dialect. A Rust-side
//! timestamp formatter would be a third spelling on SQLite, where the DDL
//! default is space-separated `CURRENT_TIMESTAMP` and a creator value arrives
//! `T`-separated, and TEXT comparison is bytewise.
//!
//! # A supplied value is REMOVED, not refused
//!
//! `assign` is not overridable, so a value the creator supplied for an assigned
//! column is dropped before the SQL builder sees it. Removal, not refusal,
//! because this pass is documented and tested as IDEMPOTENT: a check keyed on a
//! field being PRESENT cannot tell a creator's value from one an earlier call
//! minted. The refusal that needs provenance lives at the document boundary
//! (`write_pipeline::refuse_platform_assigned_id`), which runs on the raw
//! document before any pass.
//!
//! `id` is the one column this pass does not remove: it MINTS when absent and
//! leaves a present value alone, precisely so a second call cannot re-mint. Its
//! creator-supplied case is the boundary's to refuse.
//!
//! Rust-side assignment (rather than SDK-side) is the design choice so non-SDK
//! deploys (raw `default = { fetch }` apps that call `zeroship.db.*` directly)
//! are assigned too. The SDK reads the values back from the INSERT's
//! `RETURNING` row.

use std::rc::Rc;

use serde_json::{Map, Value};
use zeroship_migrate_policy::{AssignmentEvent, AssignmentGenerator};

use zeroship_data_core::error::DbError;
use crate::system_shape_charter::AssignmentPlan;

/// Does `event` fire while a row is being created?
///
/// `write` covers insert, so `updated_at` / `updated_by` / `version` are
/// assigned when the row is born as well as when it changes. `delete` does not:
/// a row is not born soft-deleted.
const fn fires_on_insert(event: AssignmentEvent) -> bool {
    match event {
        AssignmentEvent::Insert | AssignmentEvent::Write => true,
        AssignmentEvent::Delete => false,
    }
}

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
/// Empty / non-ASCII collection names fall back to `"row"`. The candidate is
/// validated together with descriptor-declared prefixes by
/// [`prefix_for_collection`].
///
/// The `t.id(prefix)` schema declaration (carried on the
/// field as `idPrefix`) is read from the orchestrator-cached
/// schema when present; this derivation is the fallback otherwise — see
/// [`prefix_for_collection`].
pub fn derive_prefix_from_collection_name(collection: &str) -> String {
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
        return "row".to_string();
    }
    truncated
}

/// Resolve the typed_id prefix for one `typedId`-assigned column.
///
/// 1. Read the `t.id(prefix)`-declared `idPrefix` off THAT COLUMN's entry in
///    the descriptor the caller resolved.
/// 2. Fall back to [`derive_prefix_from_collection_name`] when the descriptor
///    declares no explicit prefix.
/// 3. Validate either source through [`crate::query::validate_id_prefix`].
///
/// `column` is a parameter rather than the literal `"id"` because the charter
/// is what decides which column a typed id is minted for. It happens to be
/// `id` today; a charter naming a second `typedId` column, or renaming that
/// one, would otherwise look up a descriptor key that does not exist and fall
/// silently back to the collection-derived prefix - a defect that cannot occur
/// while the pass hardcodes the name, and appears the moment it stops.
///
/// `schema` is passed in rather than looked up. The write pipeline resolves the
/// collection's entry once through [`crate::descriptor::collection_schema`] and
/// hands it to every stage, so an undeclared collection is refused BEFORE any
/// id is minted — a lookup here could only re-derive the same answer, or
/// silently fall back to the derived prefix for a collection the write was
/// about to be refused for anyway.
///
/// # Errors
///
/// Returns a validation error when either the descriptor-declared or derived
/// prefix is malformed or reserved for platform ids.
pub fn prefix_for_collection(
    schema: &Value,
    collection: &str,
    column: &str,
) -> Result<String, DbError> {
    let prefix = schema
        .get(column)
        .and_then(|id_def| id_def.get("idPrefix"))
        .and_then(|p| p.as_str())
        .map(str::to_string)
        .unwrap_or_else(|| derive_prefix_from_collection_name(collection));
    crate::query::validate_id_prefix(&prefix)?;
    Ok(prefix)
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
/// Unconditionally `pub`: this crate's real release-vs-`test-helpers`
/// visibility gate is on the ENCLOSING `system_fields_pass` module in
/// `crud/mod.rs` (`pub(crate)` in release, `pub` under `test-helpers`), the
/// same mechanism `encryption_pass` uses. This function used to be declared
/// twice, once per feature arm, both times as `pub fn` with an identical
/// body - a second, redundant no-op split one level below the module gate
/// that already does the narrowing. Collapsed 2026-09-04.
///
/// # Errors
///
/// Returns a validation error before minting when the resolved typed-id prefix
/// is malformed or reserved for platform ids.
pub fn apply_system_fields_on_insert(
    doc: &mut Value,
    schema: &Value,
    collection: &str,
    actor_id: Option<&str>,
) -> Result<(), DbError> {
    apply_system_fields_on_insert_impl(doc, schema, collection, actor_id)
}

fn apply_system_fields_on_insert_impl(
    doc: &mut Value,
    schema: &Value,
    collection: &str,
    actor_id: Option<&str>,
) -> Result<(), DbError> {
    let plan = assignment_plan()?;
    let Some(obj) = doc.as_object_mut() else {
        return Ok(());
    };
    inject_into_object(obj, &plan, schema, collection, actor_id)
}

/// This thread's charter projection - the authority every arm of this pass
/// reads instead of naming a column.
fn assignment_plan() -> Result<Rc<AssignmentPlan>, DbError> {
    crate::system_shape_charter::plan()
}

/// Run the auto-population pass over every doc in an `insertMany`
/// array. Each doc gets its own freshly-minted `id`; the actor stamp
/// is the same for every doc in the batch (the batch is issued under
/// one request context).
///
/// Same visibility rationale as [`apply_system_fields_on_insert`]: the module
/// gate does the narrowing, so this is unconditionally `pub` rather than
/// duplicated per feature arm.
///
/// # Errors
///
/// Returns a validation error before minting when the resolved typed-id prefix
/// is malformed or reserved for platform ids.
pub fn apply_system_fields_on_insert_many(
    docs: &mut Value,
    schema: &Value,
    collection: &str,
    actor_id: Option<&str>,
) -> Result<(), DbError> {
    apply_system_fields_on_insert_many_impl(docs, schema, collection, actor_id)
}

fn apply_system_fields_on_insert_many_impl(
    docs: &mut Value,
    schema: &Value,
    collection: &str,
    actor_id: Option<&str>,
) -> Result<(), DbError> {
    // ONE plan for the whole batch, and the removal is keyed on the COLUMN
    // rather than on what a row happened to supply. That is what survives
    // `build_insert_many`'s column union (`query.rs`): a per-row decision would
    // let one row's supplied `created_at` pull the name into the union and bind
    // an explicit NULL into `created_at NOT NULL` for every other row.
    let plan = assignment_plan()?;
    let Some(arr) = docs.as_array_mut() else {
        return Ok(());
    };
    for doc in arr.iter_mut() {
        if let Some(obj) = doc.as_object_mut() {
            inject_into_object(obj, &plan, schema, collection, actor_id)?;
        }
    }
    Ok(())
}

fn inject_into_object(
    obj: &mut Map<String, Value>,
    plan: &AssignmentPlan,
    schema: &Value,
    collection: &str,
    actor_id: Option<&str>,
) -> Result<(), DbError> {
    for column in plan.columns() {
        let name = column.name.as_str();

        if !fires_on_insert(column.on) {
            // The charter assigns this column on another event, so its value is
            // still not the creator's to supply. Drop it and let the row be
            // born without it - `deleted_at` is the live instance, and a row
            // born soft-deleted is invisible to every read.
            obj.remove(name);
            continue;
        }

        match column.by {
            // MINT WHEN ABSENT, deliberately not remove-then-mint. That is what
            // keeps this pass idempotent, and idempotence is why the refusal of
            // a creator-supplied id lives at the document boundary instead.
            AssignmentGenerator::TypedId => {
                if !obj.contains_key(name) {
                    let prefix = prefix_for_collection(schema, collection, name)?;
                    obj.insert(
                        name.to_string(),
                        Value::String(zeroship_core::typed_id::generate(&prefix)),
                    );
                }
            }
            // The generator RUNS on every write and yields NULL when there is
            // no actor. Injecting rather than skipping is what stops a supplied
            // `created_by` from standing in for one - the anonymous arm is
            // exactly where the naive patch leaves a passthrough.
            AssignmentGenerator::Actor => {
                obj.insert(
                    name.to_string(),
                    actor_id.map_or(Value::Null, |actor| Value::String(actor.to_string())),
                );
            }
            // The DATABASE computes these: `DEFAULT NOW()` /
            // `DEFAULT CURRENT_TIMESTAMP`, `DEFAULT 1`, the identity sequence.
            // Emit nothing so its expression fires - one writer, one spelling
            // per dialect. Removing any supplied value is the whole of this
            // pass's job for them.
            AssignmentGenerator::Now
            | AssignmentGenerator::Increment(_)
            | AssignmentGenerator::Identity => {
                obj.remove(name);
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// UPDATE-time validation pass + CAS-version extraction.
// ---------------------------------------------------------------------------

/// Run the UPDATE-time assignment pass over an UPDATE patch, in place.
///
/// The charter decides both arms; this function names no column.
///
/// 1. **Fixed at insert** (`on = "insert"`) — the patch may not carry the
///    column at all. Returns a typed
///    `DbError::ValidationFailed { code: "immutable_system_field" }` via
///    `QueryError::ImmutableSystemField`. Refusal rather than removal is right
///    HERE and only here: an update patch is entirely creator-authored, so
///    "the creator sent it" is knowable, and silently discarding an attempt to
///    rewrite `created_by` would let the caller believe it landed.
/// 2. **Re-assigned on every write** (`on = "write"`) — the key is REMOVED.
///    The SQL builder then appends its own `version` / `updated_at` /
///    `updated_by` clauses unconditionally, because the patch can no longer
///    carry a competing assignment to the same column.
/// 3. **Assigned on delete** (`on = "delete"`) — left exactly as the caller
///    sent it, and that is a DEPENDENCY, not an exemption. The SDK's own soft
///    delete is an UPDATE carrying `deleted_at`
///    (`sdks/db/src/collection/crud.ts:519-523` and `:562-566`; restore is
///    `:745-748`), so touching it here would refuse the platform's own
///    `delete()`. Soft delete has to route through the native op first.
///
/// Both arms cover the top-level keys, the nested `$set`, and the arithmetic
/// operators `$inc` / `$dec` / `$mul`, matching the flattening
/// `build_set_clauses` performs.
///
/// A patch whose ONLY key was a re-assigned column comes out empty and the SQL
/// builder rejects it with `update fields cannot be empty`. That is a true
/// statement about the request - nothing remained to write - and it is the
/// builder's own message rather than a second refusal here.
///
/// Unconditionally `pub`: same rationale as
/// [`apply_system_fields_on_insert`] - the module gate in `crud/mod.rs`
/// does the release-vs-`test-helpers` narrowing, so this is not duplicated
/// per feature arm.
pub fn apply_system_fields_on_update(
    patch: &mut Value,
    app_id: &str,
    collection: &str,
) -> Result<(), DbError> {
    apply_system_fields_on_update_impl(patch, app_id, collection)
}

fn apply_system_fields_on_update_impl(
    patch: &mut Value,
    _app_id: &str,
    _collection: &str,
) -> Result<(), DbError> {
    let plan = assignment_plan()?;
    let immutable: Vec<String> = plan
        .immutable_after_insert()
        .map(str::to_string)
        .collect();
    let reassigned: Vec<String> = plan.reassigned_on_write().map(str::to_string).collect();

    let Some(obj) = patch.as_object_mut() else {
        // Non-object patches are the SQL builder's problem (they get a
        // typed `InvalidFilter` there). The pass has nothing to do.
        return Ok(());
    };

    refuse_and_strip(obj, &immutable, &reassigned, None)?;
    // `$set` and the arithmetic operators nest one level; the builder flattens
    // them into the same SET list, so an assignment hidden under one is the
    // same assignment.
    for op_key in ["$set", "$inc", "$dec", "$mul"] {
        let Some(nested) = obj.get_mut(op_key).and_then(Value::as_object_mut) else {
            continue;
        };
        refuse_and_strip(nested, &immutable, &reassigned, Some(op_key))?;
    }
    Ok(())
}

/// Refuse every insert-fixed column and strip every write-re-assigned one from
/// a single key/value map. `under` names the operator this map hangs off, for
/// the error message.
fn refuse_and_strip(
    obj: &mut Map<String, Value>,
    immutable: &[String],
    reassigned: &[String],
    under: Option<&str>,
) -> Result<(), DbError> {
    for name in immutable {
        if obj.contains_key(name) {
            let where_ = under.map_or_else(String::new, |op| format!(" under `{op}`"));
            return Err(crate::query::QueryError::ImmutableSystemField(format!(
                "UPDATE patch attempted to overwrite immutable system field `{name}`{where_} \
                 (assigned by the platform when the row was created)"
            ))
            .into());
        }
    }
    for name in reassigned {
        obj.remove(name);
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
pub fn extract_cas_version(
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
pub fn filter_has_id_predicate(filter: &Value) -> bool {
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
// Soft-delete dispatch helpers.
//
// `delete()` is implemented as a soft-delete `UPDATE ... SET deleted_at
// = NOW()`. `purge()` remains the explicit hard-delete escape hatch,
// and `restore()` clears `deleted_at`.
//
// `purge` (explicit hard-delete) and `restore` (clear `deleted_at`) are
// both direct verbs in the SDK and reach the dispatch layer through
// their own helpers below.
// ---------------------------------------------------------------------------

/// Should this collection's SELECTs auto-append
/// `AND deleted_at IS NULL`?
///
/// When `include_deleted: true` is set, the caller explicitly opts out
/// of the platform's default soft-delete filter.
pub fn should_filter_soft_deleted(
    include_deleted: bool,
) -> bool {
    !include_deleted
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use zeroship_migrate_policy::{PolicyRegistry, RootCharter};

    // ---- the charter drives the pass -------------------------------

    /// Stamp a charter of the test's own making onto this thread, so a test can
    /// ask what the pass does with an authority the operator has not shipped.
    ///
    /// The fragment is spelled the way `policies/confined-system-shape.inject.toml`
    /// spells it, and takes the same synthetic header
    /// `crate::system_shape_charter` prepends.
    fn stamp_charter(columns_toml: &str) {
        let toml = format!(
            "policy_version = 1\n\n[[inject]]\nscope = \"all\"\nmandatory = true\n\
             primary_key = [\"id\"]\nauthor_primary_key = \"forbid\"\ncolumns = [\n{columns_toml}\n]\n"
        );
        let charter = RootCharter::parse_toml(&toml, &PolicyRegistry::empty())
            .expect("the test charter must parse");
        let plan = std::rc::Rc::new(
            crate::system_shape_charter::AssignmentPlan::from_charter(&charter),
        );
        crate::system_shape_charter::stamp(plan);
    }

    /// The deliverable, stated as a test: an eighth platform column is a
    /// CHARTER LINE, not a code change. Nothing in `system_fields_pass.rs`
    /// names `tenant_id`, and the pass assigns it anyway.
    #[test]
    fn an_eighth_charter_column_is_assigned_without_a_code_change() {
        crate::reset_engine_for_tests();
        stamp_charter(
            "  { name = \"id\", type = \"text\", nullable = false, assign = { by = \"typedId\", on = \"insert\" } },\n\
             \x20 { name = \"tenant_id\", type = \"text\", nullable = true, assign = { by = \"actor\", on = \"insert\" } },",
        );
        let mut doc = json!({ "title": "hi" });
        apply_system_fields_on_insert(
            &mut doc,
            &schema_without_id_prefix(),
            "posts",
            Some("usr_tenant"),
        )
        .expect("the charter's own columns must be assignable");
        assert_eq!(
            doc.get("tenant_id").and_then(Value::as_str),
            Some("usr_tenant"),
            "a column only the charter names must still be assigned: {doc}",
        );
        crate::reset_engine_for_tests();
    }

    /// `by = "identity"` means the column's own DDL identity supplies the
    /// value. The runtime must emit NOTHING for it - including no minted
    /// typed id - so the INSERT default fires.
    ///
    /// The charter ships no `identity` column today, so this is the only place
    /// that generator has a caller. It exists because the resolver rewrites
    /// `typedId` to `identity` when a creator declares an integer identity
    /// `id`, and the pass must already be correct on the day that arrives.
    #[test]
    fn an_identity_id_is_left_to_the_database() {
        crate::reset_engine_for_tests();
        stamp_charter(
            "  { name = \"id\", type = \"integer\", nullable = false, assign = { by = \"identity\", on = \"insert\" } },",
        );
        let mut doc = json!({ "title": "hi" });
        apply_system_fields_on_insert(&mut doc, &schema_without_id_prefix(), "posts", None)
            .expect("an identity id must not be refused");
        assert!(
            doc.get("id").is_none(),
            "an identity-assigned id must not be minted by the runtime: {doc}",
        );
        crate::reset_engine_for_tests();
    }

    // ---- a supplied value for an assigned column is removed ---------

    #[test]
    fn insert_removes_a_supplied_created_at_and_version() {
        crate::reset_engine_for_tests();
        let mut doc = json!({
            "title": "hi",
            "version": 5,
            "created_at": 1_700_000_000_000_i64,
            "updated_at": 1_700_000_000_000_i64,
        });
        apply_system_fields_on_insert(&mut doc, &schema_without_id_prefix(), "posts", None)
            .expect("derived prefix must be accepted");
        let obj = doc.as_object().expect("object");
        assert!(!obj.contains_key("version"), "supplied version survived: {doc}");
        assert!(
            !obj.contains_key("created_at"),
            "supplied created_at survived: {doc}",
        );
        assert!(
            !obj.contains_key("updated_at"),
            "supplied updated_at survived: {doc}",
        );
    }

    #[test]
    fn insert_removes_a_supplied_deleted_at() {
        crate::reset_engine_for_tests();
        let mut doc = json!({ "title": "hi", "deleted_at": 1_700_000_000_000_i64 });
        apply_system_fields_on_insert(&mut doc, &schema_without_id_prefix(), "posts", None)
            .expect("derived prefix must be accepted");
        assert!(
            !doc.as_object().expect("object").contains_key("deleted_at"),
            "a row must not be born soft-deleted by a supplied value: {doc}",
        );
    }

    /// Acceptance 7: a supplied `created_by` does not survive, and the bound
    /// actor is what lands.
    #[test]
    fn insert_overwrites_a_supplied_created_by_with_the_bound_actor() {
        crate::reset_engine_for_tests();
        let mut doc = json!({ "title": "hi", "created_by": "usr_SOMEONE_ELSE" });
        apply_system_fields_on_insert(
            &mut doc,
            &schema_without_id_prefix(),
            "posts",
            Some("usr_session_actor"),
        )
        .expect("derived prefix must be accepted");
        assert_eq!(
            doc.get("created_by").and_then(Value::as_str),
            Some("usr_session_actor"),
            "a supplied created_by must not outrank the bound actor: {doc}",
        );
    }

    /// The same, on an ANONYMOUS request - the arm the naive patch leaves as a
    /// passthrough because it flips the guards inside `if let Some(actor)`.
    #[test]
    fn insert_overwrites_a_supplied_created_by_on_an_anonymous_write() {
        crate::reset_engine_for_tests();
        let mut doc = json!({ "title": "hi", "created_by": "usr_SOMEONE_ELSE" });
        apply_system_fields_on_insert(&mut doc, &schema_without_id_prefix(), "posts", None)
            .expect("derived prefix must be accepted");
        assert_eq!(
            doc.get("created_by"),
            Some(&Value::Null),
            "an anonymous write must yield NULL, never the supplied actor: {doc}",
        );
    }

    /// The actor generator runs on every write and yields NULL when there is no
    /// actor. Absent and NULL store the same thing on an INSERT; what this pins
    /// is that the generator RAN, which is what makes the UPDATE arm's
    /// staleness fix the same rule rather than a special case.
    #[test]
    fn insert_stamps_null_actor_columns_when_anonymous() {
        crate::reset_engine_for_tests();
        let mut doc = json!({ "title": "hi" });
        apply_system_fields_on_insert(&mut doc, &schema_without_id_prefix(), "posts", None)
            .expect("derived prefix must be accepted");
        assert_eq!(doc.get("created_by"), Some(&Value::Null), "doc: {doc}");
        assert_eq!(doc.get("updated_by"), Some(&Value::Null), "doc: {doc}");
    }

    /// `insertMany` unions the column set across documents and binds NULL for
    /// every missing cell (`query.rs`'s `build_insert_many_with_dialect`), so a
    /// per-document removal that depended on what the row supplied would drive
    /// an explicit NULL into `created_at NOT NULL`.
    ///
    /// The pass removes by COLUMN, not by value, so the union never sees the
    /// name at all. This asserts against the built SQL rather than the
    /// documents, because the union is the builder's behaviour and not the
    /// pass's.
    #[test]
    fn insert_many_keeps_a_supplied_value_out_of_the_batch_union() {
        crate::reset_engine_for_tests();
        let mut docs = json!([
            { "title": "a", "created_at": 1_700_000_000_000_i64 },
            { "title": "b" },
        ]);
        apply_system_fields_on_insert_many(
            &mut docs,
            &schema_without_id_prefix(),
            "posts",
            Some("usr_x"),
        )
        .expect("derived prefix must be accepted");
        let built =
            crate::query::build_insert_many("app1", "posts", &schema_without_id_prefix(), &docs)
                .expect("build_insert_many");
        let columns = built
            .sql
            .split_once(" VALUES ")
            .expect("the INSERT names its columns")
            .0;
        assert!(
            !columns.contains("\"created_at\""),
            "an assigned column reached the batch union: {}",
            built.sql,
        );
        assert!(
            !built.sql.contains("NULL"),
            "the batch bound a NULL cell: {}",
            built.sql,
        );
    }

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

    #[test]
    fn insert_refuses_reserved_derived_prefix() {
        // The same validator that fences creator-declared prefixes also
        // fences a collection name whose derived prefix is reserved.
        let mut doc = json!({ "title": "hi" });
        let result = apply_system_fields_on_insert(
            &mut doc,
            &schema_without_id_prefix(),
            "usrs",
            None,
        );

        match result {
            Err(DbError::ValidationFailed { code, .. }) => {
                assert_eq!(code, "reserved_system_field_name");
            }
            other => panic!("expected reserved-prefix refusal, got {other:?}"),
        }
        assert!(doc.get("id").is_none(), "refusal must happen before minting");
    }

    /// The other half of the id fence. The prefix validator closed the
    /// DESCRIPTOR vector - a descriptor can no longer declare `idPrefix: "usr"`
    /// and have the worker mint a platform-shaped id. It does not close the
    /// DIRECT one, because minting is conditional on `id` being absent, so a
    /// supplied value is written verbatim.
    ///
    /// This is the vector an attacker reaches without touching a generated
    /// file, so the refusal is security behaviour rather than ergonomics.
    #[test]
    #[ignore = "the refusal cannot live in this pass: it is documented and tested as idempotent, \
                so a check keyed on `id` being present cannot tell a creator's value from one an \
                earlier call minted. Move the refusal to the caller boundary, then un-ignore."]
    fn insert_refuses_a_creator_supplied_id() {
        let mut doc = json!({ "title": "hi", "id": "usr_034HQyaJ0C11GCzHMMrWwz" });
        let result =
            apply_system_fields_on_insert(&mut doc, &schema_without_id_prefix(), "posts", None);

        match result {
            Err(DbError::ValidationFailed { code, .. }) => {
                assert_eq!(code, "platform_assigned_field");
            }
            other => panic!("expected a refusal of the supplied id, got {other:?}"),
        }
    }

    /// The control. Refusing every supplied id would also pass the test above,
    /// so prove an ordinary insert still mints rather than erroring.
    #[test]
    fn insert_without_an_id_still_mints_one() {
        let mut doc = json!({ "title": "hi" });
        apply_system_fields_on_insert(&mut doc, &schema_without_id_prefix(), "posts", None)
            .expect("an insert that supplies no id must be accepted");

        let minted = doc
            .get("id")
            .and_then(Value::as_str)
            .expect("the pass must mint an id when none was supplied");
        assert!(minted.starts_with("post_"), "minted id was {minted}");
    }

    // ---- declared idPrefix wins over derivation --------------------

    /// A descriptor entry that declares no `t.id(prefix)`, so the auto-mint
    /// pass falls through to [`derive_prefix_from_collection_name`]. The
    /// pass takes the entry the write pipeline resolved, so a test that is
    /// about derivation states "no declared prefix" as data rather than by
    /// leaving a store empty.
    fn schema_without_id_prefix() -> Value {
        json!({ "title": { "type": "string" } })
    }

    /// A descriptor entry declaring `id: t.id("blog")`.
    fn schema_with_blog_id_prefix() -> Value {
        json!({
            "id": { "type": "id", "idPrefix": "blog" },
            "title": { "type": "string" },
        })
    }

    #[test]
    fn prefix_for_collection_reads_declared_id_prefix() {
        // When the descriptor entry carries `id: t.id("blog")`,
        // `prefix_for_collection` returns the declared prefix instead of
        // deriving from the collection name.
        assert_eq!(
            prefix_for_collection(&schema_with_blog_id_prefix(), "posts", "id")
                .expect("ordinary declared prefix must be accepted"),
            "blog"
        );
    }

    /// The prefix is looked up under the CHARTER'S column name, not `"id"`.
    ///
    /// A charter that minted a typed id into `row_key` would otherwise read
    /// `schema["id"]["idPrefix"]`, miss, and fall back to the collection-derived
    /// prefix while the descriptor plainly declared one.
    #[test]
    fn prefix_is_read_under_the_charter_column_not_the_literal_id() {
        crate::reset_engine_for_tests();
        stamp_charter(
            "  { name = \"row_key\", type = \"text\", nullable = false, assign = { by = \"typedId\", on = \"insert\" } },",
        );
        let schema = json!({
            "row_key": { "type": "id", "idPrefix": "blog" },
            "title": { "type": "string" },
        });
        let mut doc = json!({ "title": "hi" });
        apply_system_fields_on_insert(&mut doc, &schema, "posts", None)
            .expect("the charter's typed-id column must mint");
        let minted = doc
            .get("row_key")
            .and_then(Value::as_str)
            .expect("the charter's typed-id column must be minted");
        assert!(
            minted.starts_with("blog_"),
            "the declared prefix must be read under the charter's column name, got {minted}",
        );
        crate::reset_engine_for_tests();
    }

    #[test]
    fn insert_auto_mints_id_with_declared_prefix() {
        // The auto-mint pass honours the declared `idPrefix`
        // from the descriptor entry: a `posts` collection declaring
        // `id: t.id("blog")` mints `blog_...` ids, not `post_...`.
        let mut doc = json!({ "title": "hi" });
        apply_system_fields_on_insert(&mut doc, &schema_with_blog_id_prefix(), "posts", None)
            .expect("ordinary declared prefix must be accepted");
        let id = doc
            .get("id")
            .and_then(|v| v.as_str())
            .expect("id must be minted");
        assert!(
            id.starts_with("blog_"),
            "expected declared 'blog_' prefix, got {id}"
        );
        assert_eq!(id.len(), "blog_".len() + 22);
    }

    // ---- single-doc injection --------------------------------------

    #[test]
    fn insert_auto_mints_id_when_absent() {
        let mut doc = json!({ "title": "hi" });
        apply_system_fields_on_insert(&mut doc, &schema_without_id_prefix(), "posts", None)
            .expect("derived prefix must be accepted");
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
        apply_system_fields_on_insert(&mut doc, &schema_without_id_prefix(), "posts", None)
            .expect("derived prefix must be accepted");
        assert_eq!(
            doc.get("id").and_then(|v| v.as_str()),
            Some("post_abc123"),
            "creator-supplied id must pass through untouched"
        );
    }

    #[test]
    fn insert_minted_id_has_correct_prefix_for_collection_name() {
        let mut doc = json!({});
        apply_system_fields_on_insert(&mut doc, &schema_without_id_prefix(), "users", None)
            .expect("derived prefix must be accepted");
        assert!(doc.get("id").unwrap().as_str().unwrap().starts_with("user_"));

        let mut doc2 = json!({});
        apply_system_fields_on_insert(&mut doc2, &schema_without_id_prefix(), "tasks", None)
            .expect("derived prefix must be accepted");
        assert!(doc2.get("id").unwrap().as_str().unwrap().starts_with("task_"));
    }

    #[test]
    fn insert_populates_created_by_from_actor() {
        let mut doc = json!({ "title": "hi" });
        apply_system_fields_on_insert(
            &mut doc,
            &schema_without_id_prefix(),
            "posts",
            Some("usr_actor1"),
        )
        .expect("derived prefix must be accepted");
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
    fn insert_does_not_inject_created_at_or_updated_at_or_version() {
        let mut doc = json!({ "title": "hi" });
        apply_system_fields_on_insert(
            &mut doc,
            &schema_without_id_prefix(),
            "posts",
            Some("usr_x"),
        )
        .expect("derived prefix must be accepted");
        let obj = doc.as_object().unwrap();
        assert!(!obj.contains_key("created_at"), "DB default must fire");
        assert!(!obj.contains_key("updated_at"), "DB default must fire");
        assert!(!obj.contains_key("version"), "DB default must fire");
        assert!(!obj.contains_key("deleted_at"), "DB default must fire");
    }

    /// Idempotence, stated correctly: the SAME inputs applied twice produce the
    /// same document.
    ///
    /// This test used to run the second call with a DIFFERENT actor and assert
    /// the first actor survived. That is not idempotence - `f(f(x))` and
    /// `f(x)` are only comparable when the arguments are the same - and the
    /// property it actually pinned ("a later actor cannot restamp the row") is
    /// what let a supplied `created_by` stand in for one. The load-bearing half
    /// is the id: a second call must not re-mint, which is why the `typedId`
    /// arm mints-when-absent instead of removing and re-minting, and why the
    /// refusal of a creator-supplied id lives at the document boundary.
    #[test]
    fn insert_pass_is_idempotent() {
        crate::reset_engine_for_tests();
        let mut doc = json!({ "title": "hi" });
        apply_system_fields_on_insert(
            &mut doc,
            &schema_without_id_prefix(),
            "posts",
            Some("usr_x"),
        )
        .expect("derived prefix must be accepted");
        let after_first = doc.clone();
        apply_system_fields_on_insert(
            &mut doc,
            &schema_without_id_prefix(),
            "posts",
            Some("usr_x"),
        )
        .expect("derived prefix must be accepted");
        assert_eq!(
            doc, after_first,
            "a second pass over the same document with the same actor must change nothing",
        );
    }

    /// The anonymous batch is idempotent too, and it is the arm where the
    /// `actor` generator writes rather than skips.
    #[test]
    fn insert_pass_is_idempotent_without_an_actor() {
        crate::reset_engine_for_tests();
        let mut doc = json!({ "title": "hi" });
        apply_system_fields_on_insert(&mut doc, &schema_without_id_prefix(), "posts", None)
            .expect("derived prefix must be accepted");
        let after_first = doc.clone();
        apply_system_fields_on_insert(&mut doc, &schema_without_id_prefix(), "posts", None)
            .expect("derived prefix must be accepted");
        assert_eq!(doc, after_first);
    }

    #[test]
    fn insert_pass_non_object_doc_is_no_op() {
        let mut doc = json!("not an object");
        apply_system_fields_on_insert(
            &mut doc,
            &schema_without_id_prefix(),
            "posts",
            Some("usr_x"),
        )
        .expect("derived prefix must be accepted");
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
        apply_system_fields_on_insert_many(
            &mut docs,
            &schema_without_id_prefix(),
            "posts",
            Some("usr_x"),
        )
        .expect("derived prefix must be accepted");
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
        apply_system_fields_on_insert_many(
            &mut docs,
            &schema_without_id_prefix(),
            "posts",
            None,
        )
        .expect("derived prefix must be accepted");
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

    /// Every system field reaches the INSERT VALUES when populated
    /// (id always; created_by / updated_by when actor present). The
    /// DB DEFAULTs handle the rest.
    #[test]
    fn insert_pass_emits_three_extra_columns_when_actor_present() {
        let mut doc = json!({ "title": "hi" });
        apply_system_fields_on_insert(
            &mut doc,
            &schema_without_id_prefix(),
            "posts",
            Some("usr_x"),
        )
        .expect("derived prefix must be accepted");
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

    /// When no actor is bound the INSERT still names `created_by` /
    /// `updated_by`, bound to SQL NULL. Absent and NULL store the same value
    /// here; what the explicit NULL buys is that the batch shape does not
    /// depend on who was signed in, and that the `actor` generator has one
    /// behaviour rather than two.
    #[test]
    fn insert_pass_binds_null_actor_columns_when_no_actor() {
        crate::reset_engine_for_tests();
        let mut doc = json!({ "title": "hi" });
        apply_system_fields_on_insert(&mut doc, &schema_without_id_prefix(), "posts", None)
            .expect("derived prefix must be accepted");
        let obj = doc.as_object().unwrap();
        assert_eq!(obj.len(), 4, "doc keys: {:?}", obj.keys().collect::<Vec<_>>());
        assert!(obj.contains_key("id"));
        assert!(obj.contains_key("title"));
        assert_eq!(obj.get("created_by"), Some(&Value::Null));
        assert_eq!(obj.get("updated_by"), Some(&Value::Null));

        let built = crate::query::build_insert("app1", "posts", &schema_without_id_prefix(), &doc)
            .expect("build_insert");
        assert!(
            built.sql.contains("\"created_by\""),
            "the NULL actor column must still be named: {}",
            built.sql,
        );
    }

    /// Confirms the SQL builder integration — the INSERT statement carries all
    /// 4 columns and NAMES every system field in its `RETURNING` clause, so the
    /// SDK gets the DB-defaulted values back.
    ///
    /// This asserted `RETURNING *` until the projection landed. The star was
    /// never the property: what the SDK relies on is that the DDL-defaulted
    /// timestamps and `version` come back, and naming the seven system fields
    /// says that where `*` only implied it.
    #[test]
    fn insert_pass_followed_by_build_insert_returns_every_system_field() {
        use crate::query::{build_insert, SYSTEM_FIELD_NAMES};
        let mut doc = json!({ "title": "hi" });
        apply_system_fields_on_insert(
            &mut doc,
            &schema_without_id_prefix(),
            "posts",
            Some("usr_x"),
        )
        .expect("derived prefix must be accepted");
        let built = build_insert("app1", "posts", &schema_without_id_prefix(), &doc)
            .expect("build_insert");
        let returning = built
            .sql
            .split_once(" RETURNING ")
            .expect("the INSERT carries a RETURNING clause")
            .1;
        assert!(
            !returning.contains('*'),
            "the RETURNING clause must name columns, not star: {}",
            built.sql
        );
        for field in SYSTEM_FIELD_NAMES {
            assert!(
                returning.contains(&format!("\"{field}\"")),
                "RETURNING must name the system field {field}: {}",
                built.sql
            );
        }
        // Parameter count: 4 columns (id, title, created_by, updated_by).
        assert_eq!(built.params.len(), 4, "params: {:?}", built.params);
    }

    // ---- UPDATE pass: insert-fixed refusal, write-assigned strip --

    /// Refuse the column, then prove the refusal is charter-DERIVED rather
    /// than a coincidence with the three names that used to be hardcoded: the
    /// same helper reads `immutable_after_insert()` off the plan.
    fn expect_immutable_refusal(mut patch: Value, collection: &str) {
        let err = apply_system_fields_on_update(&mut patch, "app1", collection)
            .expect_err("UPDATE must refuse a column the platform fixed at insert");
        match err {
            DbError::ValidationFailed { code, .. } => {
                assert_eq!(code, "immutable_system_field");
            }
            other => panic!("expected ValidationFailed, got {other:?}"),
        }
    }

    #[test]
    fn update_refuses_creator_supplied_id_change() {
        expect_immutable_refusal(json!({ "id": "post_other", "title": "x" }), "posts_imid");
    }

    #[test]
    fn update_refuses_creator_supplied_created_at_change() {
        expect_immutable_refusal(json!({ "created_at": 1700000000000_i64 }), "posts_imca");
    }

    #[test]
    fn update_refuses_creator_supplied_created_by_change() {
        expect_immutable_refusal(json!({ "created_by": "usr_other" }), "posts_imcb");
    }

    #[test]
    fn update_refuses_immutable_fields_under_dollar_set() {
        // Nested $set form must be caught too - the SDK can produce
        // either shape.
        expect_immutable_refusal(json!({ "$set": { "id": "post_other" } }), "posts_imset");
    }

    #[test]
    fn update_refuses_immutable_field_under_dollar_inc() {
        // Defence-in-depth: $inc.id / $inc.created_at / $inc.created_by
        // are refused for the same reason as top-level overwrites.
        expect_immutable_refusal(json!({ "$inc": { "created_by": 1 } }), "posts_immut_inc");
    }

    /// Every column the charter fixes at insert is refused, whatever it is
    /// named. This is the arm that would have to change if the charter gained
    /// an eighth `on = "insert"` column, and it does not name one.
    #[test]
    fn update_refuses_every_column_the_charter_fixes_at_insert() {
        crate::reset_engine_for_tests();
        let plan = crate::system_shape_charter::plan().expect("the compiled charter must project");
        let fixed: Vec<String> = plan.immutable_after_insert().map(str::to_string).collect();
        assert!(!fixed.is_empty(), "the charter must fix at least one column at insert");
        for name in fixed {
            expect_immutable_refusal(json!({ name.clone(): "x" }), "posts_charter_immutable");
        }
    }

    #[test]
    fn update_strips_a_supplied_version() {
        crate::reset_engine_for_tests();
        let mut patch = json!({ "title": "x", "version": 42 });
        apply_system_fields_on_update(&mut patch, "app1", "posts_csv").expect("passes");
        assert!(
            !patch.as_object().expect("object").contains_key("version"),
            "a supplied version must not reach the builder beside its own bump: {patch}",
        );
        assert_eq!(patch.get("title").and_then(Value::as_str), Some("x"));
    }

    #[test]
    fn update_strips_a_supplied_updated_at_and_updated_by() {
        crate::reset_engine_for_tests();
        let mut patch = json!({
            "title": "x",
            "updated_at": "2026-01-01T00:00:00Z",
            "updated_by": "usr_explicit",
        });
        apply_system_fields_on_update(&mut patch, "app1", "posts_csua").expect("passes");
        let obj = patch.as_object().expect("object");
        assert!(!obj.contains_key("updated_at"), "patch: {patch}");
        assert!(!obj.contains_key("updated_by"), "patch: {patch}");
    }

    #[test]
    fn update_strips_write_assigned_columns_under_dollar_set() {
        crate::reset_engine_for_tests();
        let mut patch = json!({ "$set": { "title": "x", "updated_by": "usr_explicit" } });
        apply_system_fields_on_update(&mut patch, "app1", "posts_setstrip").expect("passes");
        let set_obj = patch
            .get("$set")
            .and_then(Value::as_object)
            .expect("$set survives");
        assert!(!set_obj.contains_key("updated_by"), "patch: {patch}");
        assert!(set_obj.contains_key("title"), "patch: {patch}");
    }

    /// The legacy `{ $inc: { version: 1 } }` shape used to be DETECTED so the
    /// runtime could stand down and let it win. Under `assign` there is no
    /// competing assignment to stand down for: the operator is stripped and the
    /// platform's own bump is the only one emitted, which is what keeps a
    /// double bump impossible rather than merely unlikely.
    #[test]
    fn update_strips_the_legacy_dollar_inc_version() {
        crate::reset_engine_for_tests();
        let mut patch = json!({ "$inc": { "version": 1, "views": 1 } });
        apply_system_fields_on_update(&mut patch, "app1", "posts_legacyinc").expect("passes");
        let inc = patch
            .get("$inc")
            .and_then(Value::as_object)
            .expect("$inc survives");
        assert!(!inc.contains_key("version"), "patch: {patch}");
        assert!(inc.contains_key("views"), "patch: {patch}");
    }

    /// `deleted_at` is `on = "delete"`, and the pass must leave it exactly
    /// where it found it. This is a DEPENDENCY on soft delete still routing
    /// through an UPDATE (`sdks/db/src/collection/crud.ts:519-523`): strip or
    /// refuse it here and the platform's own `delete()` stops working.
    #[test]
    fn update_leaves_the_delete_assigned_column_alone() {
        crate::reset_engine_for_tests();
        let mut patch = json!({ "deleted_at": 1_700_000_000_000_i64 });
        apply_system_fields_on_update(&mut patch, "app1", "posts_softdelete")
            .expect("the platform's own soft delete must not be refused");
        assert_eq!(
            patch.get("deleted_at").and_then(Value::as_i64),
            Some(1_700_000_000_000),
            "patch: {patch}",
        );
    }

    #[test]
    fn update_leaves_an_ordinary_patch_untouched() {
        crate::reset_engine_for_tests();
        let mut patch = json!({ "title": "x" });
        apply_system_fields_on_update(&mut patch, "app1", "posts_defaults").expect("passes");
        assert_eq!(patch, json!({ "title": "x" }));
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

    // ---- soft-delete filter gate -----

    #[test]
    fn should_filter_soft_deleted_by_default() {
        assert!(should_filter_soft_deleted(false));
    }

    #[test]
    fn should_filter_soft_deleted_false_when_include_deleted_true() {
        assert!(!should_filter_soft_deleted(true));
    }
}
