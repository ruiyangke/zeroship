//! Host-installed runtime descriptors are the model-shape authority.
//! Physical catalog evidence is read separately by protection-floor checks.

use std::sync::Arc;

use zeroship_data_sql::value::Value;

use zeroship_data_orm::binding::DbBinding;
use zeroship_data_orm::error::DbError;

/// Validate and atomically install the host's runtime field maps.
pub fn install_collections(
    binding: &DbBinding,
    collections: Vec<(String, Value)>,
) -> Result<(), DbError> {
    let mut names = std::collections::HashSet::new();
    for (name, schema) in &collections {
        crate::compile::validate_collection(name)?;
        if !names.insert(name) {
            return Err(DbError::internal(
                "duplicate collection in the runtime descriptor",
            ));
        }
        let fields = schema
            .as_object()
            .ok_or_else(|| DbError::internal("collection descriptor must be an object"))?;
        zeroship_data_sql::descriptors::validate_collection_identity(schema).map_err(
            |message| {
                DbError::validation("invalid_collection_identity", format!("{name}: {message}"))
            },
        )?;
        let assignments = crate::assignments::AssignmentPlan::from_schema(schema)?;
        zeroship_data_sql::lifecycle::soft_delete_column(schema)?;
        zeroship_data_sql::lifecycle::concurrency_column(schema)?;
        for (name, definition) in fields {
            for (role, event, generator) in [
                (
                    "softDelete",
                    zeroship_migrate_policy::AssignmentEvent::Delete,
                    "now",
                ),
                (
                    "concurrency",
                    zeroship_migrate_policy::AssignmentEvent::Write,
                    "increment",
                ),
            ] {
                if definition.get(role).and_then(Value::as_bool) == Some(true)
                    && !assignments.columns().iter().any(|column| {
                        column.name == *name
                            && column.on == event
                            && match generator {
                                "now" => {
                                    column.by == zeroship_migrate_policy::AssignmentGenerator::Now
                                }
                                _ => matches!(
                                    column.by,
                                    zeroship_migrate_policy::AssignmentGenerator::Increment(_)
                                ),
                            }
                    })
                {
                    return Err(DbError::validation(
                        "invalid_column_role",
                        format!("'{name}' requires a matching generator for {role}"),
                    ));
                }
            }
            if crate::compile::is_schema_metadata_key(name) {
                continue;
            }
            crate::compile::validate_field_name(name)?;
            if crate::encryption::plaintext::PlaintextType::from_field(definition)?.is_some()
                && definition.get("unique").and_then(Value::as_bool) == Some(true)
            {
                return Err(DbError::validation(
                    "encrypted_unique_unsupported",
                    "encrypted fields cannot be unique",
                ));
            }
            if !definition.is_object() {
                return Err(DbError::internal("field descriptor must be an object"));
            }
        }
    }
    zeroship_data_orm::schema_cache::with_mut(|cache| {
        cache.replace_for_binding(binding, collections)
    });
    Ok(())
}

/// The descriptor entry for one collection, or a typed error.
///
/// **There is no `Option` here on purpose.** The read path used to treat an
/// absent schema as "carry on", which is how L24 happened: the projection
/// allowlist stopped applying and the read-identifier check silently passed, so
/// a read served before the schema arrived returned every physical column and
/// accepted any field name. A collection the descriptor does not declare is not
/// a collection this isolate can serve, and saying so is the whole fix.
pub fn collection_schema(binding: &DbBinding, collection: &str) -> Result<Arc<Value>, DbError> {
    zeroship_data_orm::schema_cache::with(|c| c.require(binding, collection))
}

/// Every collection this isolate's descriptor declares, with its field map.
pub fn declared_collections(binding: &DbBinding) -> Vec<(String, Arc<Value>)> {
    zeroship_data_orm::schema_cache::with(|c| c.entries_for_binding(binding))
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeroship_data_sql::value;

    #[test]
    fn invalid_identity_cannot_replace_installed_collections() {
        crate::tests::fixtures::reset_engine();
        let binding = DbBinding::cold_start("app_identity_contract");
        let valid = value!({"id":{"type":"string", "required":true, "primaryKey":true}});
        install_collections(&binding, vec![("entries".into(), valid.clone())]).unwrap();
        for invalid in [
            value!({}),
            value!({"key":{"type":"string", "required":true, "primaryKey":true}}),
            value!({"id":{"type":"string", "required":true}}),
            value!({"id":{"type":"string", "primaryKey":true}}),
            value!({"id":{"type":"string", "required":false, "primaryKey":true}}),
            value!({
                "id":{"type":"string", "required":true, "primaryKey":true},
                "tenant":{"type":"string", "required":true, "primaryKey":true}
            }),
        ] {
            let err = install_collections(
                &binding,
                vec![
                    ("replacement".into(), valid.clone()),
                    ("invalid".into(), invalid),
                ],
            )
            .unwrap_err();
            assert!(matches!(
                err,
                DbError::ValidationFailed {
                    code: "invalid_collection_identity",
                    ..
                }
            ));
            assert_eq!(
                collection_schema(&binding, "entries").unwrap().as_ref(),
                &valid
            );
            assert!(collection_schema(&binding, "replacement").is_err());
            assert!(collection_schema(&binding, "invalid").is_err());
        }
    }

    #[test]
    fn declared_id_does_not_require_or_invent_a_generator() {
        crate::tests::fixtures::reset_engine();
        let binding = DbBinding::cold_start("app_explicit_identity");
        for id in [
            value!({"type":"string", "required":true, "primaryKey":true}),
            value!({"type":"string", "required":true, "primaryKey":true,
                "assign":{"by":"typedId", "on":"insert"}}),
        ] {
            let fields = value!({"id":id, "slug":{"type":"string", "unique":true}});
            install_collections(&binding, vec![("entries".into(), fields.clone())]).unwrap();
            assert_eq!(
                collection_schema(&binding, "entries").unwrap().as_ref(),
                &fields
            );
        }
    }

    #[test]
    fn an_undeclared_collection_is_a_typed_error_not_a_missing_schema() {
        crate::tests::fixtures::reset_engine();
        let binding = DbBinding::cold_start("app_descriptor_miss");
        let err = collection_schema(&binding, "users").expect_err("must not resolve");
        assert!(
            format!("{err:?}").contains("collection_not_declared"),
            "an undeclared collection must carry the typed code; got {err:?}",
        );
    }

    /// The store is keyed by the DEPLOY. A worker thread holding a pinned and a
    /// current isolate of one app must not serve one deploy's schema to the
    /// other.
    ///
    /// This is the store-level half of the property.
    /// `v8_classes::db::tests::co_resident_deploy_bindings_keep_tokens_and_schema_entries_isolated`
    /// is the receiver-level half: it mints two real `Collection` wrappers off
    /// two real isolates and asks what each RESOLVES. Both are needed - this one
    /// would still pass if `mint_db` stopped capturing the deploy token, and
    /// that one would still pass if the key were right for the wrong reason.
    #[test]
    fn two_deploys_of_one_app_hold_separate_descriptor_entries() {
        crate::tests::fixtures::reset_engine();
        let pinned = DbBinding::new(
            "app_two_deploys",
            "deploy_pinned",
            zeroship_data_sql::SchemaName::new("app_two_deploys").unwrap(),
        );
        let current = DbBinding::new(
            "app_two_deploys",
            "deploy_current",
            zeroship_data_sql::SchemaName::new("app_two_deploys").unwrap(),
        );
        zeroship_data_orm::schema_cache::with_mut(|c| {
            c.insert_one(
                &pinned,
                "secrets",
                value!({ "marker": { "type": "string" } }),
            );
        });
        assert!(
            collection_schema(&current, "secrets").is_err(),
            "the current deploy must not read the pinned deploy's descriptor entry",
        );
        zeroship_data_orm::schema_cache::with_mut(|c| {
            c.insert_one(
                &current,
                "secrets",
                value!({ "other": { "type": "string" } }),
            );
        });
        assert_eq!(
            collection_schema(&pinned, "secrets").unwrap().as_ref(),
            &value!({ "marker": { "type": "string" } }),
            "installing the current deploy redirected the pinned binding",
        );
    }
}
