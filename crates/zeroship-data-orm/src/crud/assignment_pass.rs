//! Apply the collection descriptor's assignments before SQL compilation.

use crate::value::Map;
use crate::value::Value;
use zeroship_migrate_policy::{AssignmentEvent, AssignmentGenerator};

use crate::assignments::AssignmentPlan;
use zeroship_data_orm::error::DbError;

/// Write assignments also fire on insert.
const fn fires_on_insert(event: AssignmentEvent) -> bool {
    match event {
        AssignmentEvent::Insert | AssignmentEvent::Write => true,
        AssignmentEvent::Delete => false,
    }
}

/// Bound collection-derived prefixes; explicit prefixes use the shared validator.
const MAX_AUTO_PREFIX_LEN: usize = 4;

/// Derive a lowercase prefix when a field has no explicit `idPrefix`.
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

/// Resolve and validate the prefix declared for this assigned column.
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
    crate::sql::compile::validate_id_prefix(&prefix)?;
    Ok(prefix)
}

pub fn apply_assignments_on_insert(
    doc: &mut Value,
    schema: &Value,
    collection: &str,
    actor_id: Option<&str>,
) -> Result<(), DbError> {
    apply_assignments_on_insert_impl(doc, schema, collection, actor_id)
}

fn apply_assignments_on_insert_impl(
    doc: &mut Value,
    schema: &Value,
    collection: &str,
    actor_id: Option<&str>,
) -> Result<(), DbError> {
    let plan = AssignmentPlan::from_schema(schema)?;
    let Some(obj) = doc.as_object_mut() else {
        return Ok(());
    };
    inject_into_object(obj, &plan, schema, collection, actor_id)
}

/// Apply the same assignment plan to every row in a batch.
pub fn apply_assignments_on_insert_many(
    docs: &mut Value,
    schema: &Value,
    collection: &str,
    actor_id: Option<&str>,
) -> Result<(), DbError> {
    apply_assignments_on_insert_many_impl(docs, schema, collection, actor_id)
}

fn apply_assignments_on_insert_many_impl(
    docs: &mut Value,
    schema: &Value,
    collection: &str,
    actor_id: Option<&str>,
) -> Result<(), DbError> {
    // Remove generated defaults consistently before the insert compiler unions columns.
    let plan = AssignmentPlan::from_schema(schema)?;
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
            // Delete assignments cannot be supplied during insertion.
            obj.shift_remove(name);
            continue;
        }

        match column.by {
            // The input boundary rejects supplied IDs; retain generated IDs on re-entry.
            AssignmentGenerator::TypedId => {
                if !obj.contains_key(name) {
                    let prefix = prefix_for_collection(schema, collection, name)?;
                    obj.insert(
                        name.to_string(),
                        Value::String(zeroship_core::typed_id::generate(&prefix)),
                    );
                }
            }
            // An anonymous write assigns NULL instead of retaining a supplied actor.
            AssignmentGenerator::Actor => {
                obj.insert(
                    name.to_string(),
                    actor_id.map_or(Value::Null, |actor| Value::String(actor.to_string())),
                );
            }
            // Omit these columns so the migration-generated database defaults run.
            AssignmentGenerator::Now
            | AssignmentGenerator::Increment(_)
            | AssignmentGenerator::Identity => {
                obj.shift_remove(name);
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// UPDATE-time validation pass + CAS-version extraction.
// ---------------------------------------------------------------------------

pub fn apply_assignments_on_update(patch: &mut Value, schema: &Value) -> Result<(), DbError> {
    if patch.get("id").is_some()
        || ["$set", "$inc", "$dec", "$mul"]
            .iter()
            .any(|op| patch.get(*op).is_some_and(|fields| fields.get("id").is_some()))
    {
        return Err(DbError::validation(
            "immutable_primary_key",
            "the collection identity cannot be changed after insertion",
        ));
    }
    let plan = AssignmentPlan::from_schema(schema)?;
    let immutable: Vec<String> = plan.immutable_after_insert().map(str::to_string).collect();
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

/// Reject protected assignment fields and strip values that write generators replace.
fn refuse_and_strip(
    obj: &mut Map<String, Value>,
    immutable: &[String],
    reassigned: &[String],
    under: Option<&str>,
) -> Result<(), DbError> {
    for name in immutable {
        if obj.contains_key(name) {
            let where_ = under.map_or_else(String::new, |op| format!(" under `{op}`"));
            return Err(crate::sql::compile::QueryError::ImmutableAssignedField(format!(
                "UPDATE patch attempted to overwrite immutable assigned field `{name}`{where_}"
            ))
            .into());
        }
    }
    for name in reassigned {
        obj.shift_remove(name);
    }
    Ok(())
}

/// Resolve a direct equality guard on the declared concurrency field; reject nested guards.
pub fn extract_cas_version(
    filter: &Value,
    collection: &str,
    schema: &Value,
) -> Result<Option<i64>, DbError> {
    let Some(column) = crate::sql::lifecycle::concurrency_column(schema)? else {
        return Ok(None);
    };
    if filter_has_nested_version_predicate(filter, column) {
        return Err(DbError::version_filter_must_be_top_level(collection));
    }
    let Some(obj) = filter.as_object() else {
        return Ok(None);
    };
    let Some(v) = obj.get(column) else {
        return Ok(None);
    };
    // Reject operator objects ({ $gt, $in, ... }) — only a plain
    // equality predicate carries CAS semantics. `as_i64` also rejects
    // floats and strings, which is the desired strictness.
    Ok(v.as_i64())
}

fn filter_has_nested_version_predicate(filter: &Value, column: &str) -> bool {
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
            matches!(key.as_str(), "$and" | "$or") && combinator_contains_field(nested, field)
        })
    }

    filter
        .as_object()
        .map(|obj| {
            obj.iter().any(|(key, nested)| {
                matches!(key.as_str(), "$and" | "$or") && combinator_contains_field(nested, column)
            })
        })
        .unwrap_or(false)
}

/// Whether reads should apply a declared soft-delete visibility marker.
pub fn should_filter_soft_deleted(include_deleted: bool) -> bool {
    !include_deleted
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value;
    use crate::sql::{SchemaName, compile::{SqlDialect, build_insert_many_with_dialect}};

    fn schema() -> Value {
        value!({
            "key": {"type":"string", "primaryKey":true, "idPrefix":"note", "assign":{"by":"typedId", "on":"insert"}},
            "born": {"type":"date", "assign":{"by":"now", "on":"insert"}},
            "touched": {"type":"date", "assign":{"by":"now", "on":"write"}},
            "author": {"type":"string", "assign":{"by":"actor", "on":"insert"}},
            "editor": {"type":"string", "assign":{"by":"actor", "on":"write"}},
            "revision": {"type":"int", "concurrency":true, "default":1, "assign":{"by":"increment(1)", "on":"write"}},
            "removed": {"type":"date", "softDelete":true, "assign":{"by":"now", "on":"delete"}},
            "title":{"type":"string"}
        })
    }

    #[test]
    fn insert_uses_declared_generators_and_preserves_ordinary_names() {
        let mut fields = schema();
        for name in [
            "id",
            "created_at",
            "updated_at",
            "created_by",
            "updated_by",
            "version",
            "deleted_at",
        ] {
            fields[name] = value!({"type":"string"});
        }
        for actor in [None, Some("usr_editor")] {
            let mut document = value!({"title":"hello", "born":0, "touched":0, "revision":999, "removed":0,
                "author":"forged", "editor":"forged", "id":"ordinary", "created_at":"ordinary", "version":"ordinary", "deleted_at":"ordinary"});
            apply_assignments_on_insert(&mut document, &fields, "notes", actor).unwrap();
            assert!(document["key"].as_str().unwrap().starts_with("note_"));
            assert_eq!(document["author"], actor.map_or(Value::Null, Value::from));
            assert_eq!(document["editor"], actor.map_or(Value::Null, Value::from));
            for name in ["born", "touched", "revision", "removed"] {
                assert!(document.get(name).is_none());
            }
            for name in ["id", "created_at", "version", "deleted_at"] {
                assert_eq!(document[name], value!("ordinary"));
            }
            let before = document.clone();
            apply_assignments_on_insert(&mut document, &fields, "notes", actor).unwrap();
            assert_eq!(document, before);
        }
    }

    #[test]
    fn batches_mint_distinct_keys_and_leave_database_defaults_absent() {
        let fields = schema();
        let mut docs = value!([{"title":"first", "born":0}, {"title":"second", "revision":99}]);
        apply_assignments_on_insert_many(&mut docs, &fields, "notes", None).unwrap();
        assert_ne!(docs[0]["key"], docs[1]["key"]);
        for dialect in [SqlDialect::Postgres, SqlDialect::Sqlite] {
            let query = build_insert_many_with_dialect(
                &SchemaName::new("app").unwrap(),
                "notes",
                &fields,
                &docs,
                dialect,
            )
            .unwrap();
            let insert = query.sql.split("RETURNING").next().unwrap();
            for name in ["born", "touched", "revision", "removed"] {
                assert!(!insert.contains(&format!("\"{name}\"")), "{}", query.sql);
            }
        }
    }

    #[test]
    fn identity_is_supplied_by_the_database() {
        let fields = value!({"sequence":{"type":"int", "assign":{"by":"identity", "on":"insert"}}});
        let mut doc = value!({"sequence":999});
        apply_assignments_on_insert(&mut doc, &fields, "items", None).unwrap();
        assert!(doc.as_object().unwrap().is_empty());
    }

    #[test]
    fn prefix_validation_applies_to_the_assigned_column() {
        let mut fields = schema();
        fields["key"]["idPrefix"] = value!("usr");
        assert!(apply_assignments_on_insert(&mut value!({}), &fields, "notes", None).is_err());
        assert_eq!(derive_prefix_from_collection_name("posts"), "post");
        assert_eq!(derive_prefix_from_collection_name(""), "row");
        assert_eq!(
            prefix_for_collection(&schema(), "items", "key").unwrap(),
            "note"
        );
    }

    #[test]
    fn updates_refuse_insert_and_delete_assignments_and_remove_write_assignments() {
        let fields = schema();
        for name in ["key", "born", "author", "removed"] {
            for op in [None, Some("$set"), Some("$inc"), Some("$dec"), Some("$mul")] {
                let mut patch = op.map_or_else(|| value!({name:1}), |op| value!({op:{name:1}}));
                assert!(
                    apply_assignments_on_update(&mut patch, &fields).is_err(),
                    "{patch}"
                );
            }
        }
        for op in [None, Some("$set"), Some("$inc"), Some("$dec"), Some("$mul")] {
            let values = value!({"touched":1, "editor":"forged", "revision":999});
            let mut patch = op.map_or_else(|| values.clone(), |op| value!({op:values.clone()}));
            apply_assignments_on_update(&mut patch, &fields).unwrap();
            let remaining = op.map_or(&patch, |op| &patch[op]);
            assert!(remaining.as_object().unwrap().is_empty());
        }
        let mut ordinary = value!({"version":"custom", "created_at":"custom"});
        apply_assignments_on_update(&mut ordinary, &value!({})).unwrap();
        assert_eq!(
            ordinary,
            value!({"version":"custom", "created_at":"custom"})
        );
    }

    #[test]
    fn concurrency_predicates_follow_roles_and_identity_uses_id() {
        let fields = schema();
        assert_eq!(
            extract_cas_version(&value!({"revision":7}), "notes", &fields).unwrap(),
            Some(7)
        );
        for filter in [
            value!({"version":7}),
            value!({"revision":"7"}),
            value!({"revision":{"$gt":7}}),
            Value::Null,
        ] {
            assert_eq!(
                extract_cas_version(&filter, "notes", &fields).unwrap(),
                None
            );
        }
        assert!(extract_cas_version(&value!({"$and":[{"revision":7}]}), "notes", &fields).is_err());
        assert!(
            extract_cas_version(
                &value!({"$or":[{"$and":[{"revision":7}]}]}),
                "notes",
                &fields
            )
            .is_err()
        );
        assert_eq!(
            extract_cas_version(&value!({"version":7}), "notes", &value!({})).unwrap(),
            None
        );
    }

    #[test]
    fn delete_restore_and_write_expressions_follow_assignment_events() {
        let fields = schema();
        let plan = AssignmentPlan::from_schema(&fields).unwrap();
        for dialect in [SqlDialect::Postgres, SqlDialect::Sqlite] {
            for (deleting, restoring) in [(false, false), (true, false), (false, true)] {
                let mut params = Vec::new();
                let expressions = plan
                    .write_assignments(&fields, Some("usr_actor"), deleting, restoring)
                    .render(dialect, &mut params, None)
                    .unwrap()
                    .join(", ");
                assert!(expressions.contains("\"revision\" = \"revision\" + $"));
                assert!(expressions.contains(&format!(
                    "\"touched\" = {}",
                    dialect.current_timestamp_expr()
                )));
                assert!(params.contains(&value!("usr_actor")));
                assert_eq!(expressions.contains("\"removed\" ="), deleting || restoring);
                if restoring {
                    assert!(params.contains(&Value::Null));
                }
                assert!(!expressions.contains("\"born\""));
            }
        }
    }
}
