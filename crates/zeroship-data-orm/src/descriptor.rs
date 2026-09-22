//! Host-installed native metadata is the model-shape authority.

use crate::{
    binding::DbBinding,
    error::DbError,
    schema::{FieldMap, Schema},
};
use std::sync::Arc;

/// Validate the complete schema before publishing any collection.
pub fn install_collections(binding: &DbBinding, schema: Schema) -> Result<(), DbError> {
    schema.validate()?;
    let collections = schema
        .into_collections()
        .into_iter()
        .map(|(name, schema)| (name, schema.into_fields()))
        .collect();
    crate::schema_cache::with_mut(|cache| cache.replace_for_binding(binding, collections));
    Ok(())
}

pub fn collection_schema(binding: &DbBinding, collection: &str) -> Result<Arc<FieldMap>, DbError> {
    crate::schema_cache::with(|cache| cache.require(binding, collection))
}

pub fn declared_collections(binding: &DbBinding) -> Vec<(String, Arc<FieldMap>)> {
    crate::schema_cache::with(|cache| cache.entries_for_binding(binding))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{schema::CollectionSchema, value, value::Value};

    fn install_artifact(
        binding: &DbBinding,
        collections: Vec<(String, Value)>,
    ) -> Result<(), DbError> {
        install_collections(binding, Schema::from_collections(collections)?)
    }

    fn fields(value: &Value) -> FieldMap {
        CollectionSchema::from_fields(value).unwrap().into_fields()
    }

    fn identity_corpus() -> serde_json::Value {
        serde_json::from_str(include_str!(
            "../../../tests/fixtures/data/collection-identity.json"
        ))
        .unwrap()
    }

    #[test]
    fn invalid_identity_cannot_replace_installed_collections() {
        crate::tests::fixtures::reset_engine();
        let binding = crate::tests::fixtures::harness_binding("app_identity_contract");
        let valid = value!({"id":{"type":"string", "required":true, "primaryKey":true}});
        install_artifact(&binding, vec![("entries".into(), valid.clone())]).unwrap();
        let corpus = identity_corpus();
        let cases = corpus["invalid"].as_object().unwrap();
        assert!(!cases.is_empty(), "invalid fixtures must not be empty");
        for (name, case) in cases {
            let err = install_artifact(
                &binding,
                vec![
                    ("replacement".into(), valid.clone()),
                    ("invalid".into(), Value::from(case["fields"].clone())),
                ],
            )
            .unwrap_err();
            if matches!(
                name.as_str(),
                "null_id" | "string_primary_key" | "string_required"
            ) {
                assert!(
                    matches!(
                        err,
                        DbError::ValidationFailed {
                            code: "invalid_schema",
                            ..
                        }
                    ),
                    "{name}: {err:?}"
                );
            } else {
                match err {
                    DbError::ValidationFailed {
                        code: "invalid_collection_identity",
                        message,
                        ..
                    } => assert_eq!(
                        message,
                        format!("invalid: {}", case["error"].as_str().unwrap()),
                        "{name}"
                    ),
                    error => panic!("{name}: expected invalid_collection_identity, got {error:?}"),
                }
            }
            assert_eq!(
                collection_schema(&binding, "entries").unwrap().as_ref(),
                &fields(&valid)
            );
            assert!(collection_schema(&binding, "replacement").is_err());
            assert!(collection_schema(&binding, "invalid").is_err());
        }
    }

    #[test]
    fn invalid_relation_metadata_cannot_replace_installed_collections() {
        crate::tests::fixtures::reset_engine();
        let binding = crate::tests::fixtures::harness_binding("app_relation_contract");
        let valid = value!({
            "id":{"type":"string", "required":true, "primaryKey":true},
            "owner_id":{"type":"string", "refTarget":"people", "refColumn":"id", "relation":"owner"}
        });
        let people = value!({"id":{"type":"string", "required":true, "primaryKey":true}});
        install_artifact(
            &binding,
            vec![
                ("entries".into(), valid.clone()),
                ("people".into(), people.clone()),
            ],
        )
        .unwrap();
        for relation in [
            value!(null),
            value!(true),
            value!(""),
            value!("owner.name"),
            value!("__proto__"),
            value!("constructor"),
            value!("prototype"),
            value!("_meta"),
            value!("_custom"),
            value!("__zeroship_internal"),
            value!("id"),
            value!("owner_id"),
        ] {
            let mut candidate = valid.clone();
            candidate["owner_id"]["relation"] = relation;
            assert!(install_artifact(
                &binding,
                vec![
                    ("entries".into(), candidate),
                    ("people".into(), people.clone())
                ]
            )
            .is_err());
            assert_eq!(
                collection_schema(&binding, "entries").unwrap().as_ref(),
                &fields(&valid)
            );
        }
        let mut duplicate = valid.clone();
        duplicate["editor_id"] = valid["owner_id"].clone();
        assert!(install_artifact(
            &binding,
            vec![
                ("entries".into(), duplicate),
                ("people".into(), people.clone())
            ]
        )
        .is_err());
        for required in ["refTarget", "refColumn"] {
            let mut candidate = valid.clone();
            candidate["owner_id"]
                .as_object_mut()
                .unwrap()
                .swap_remove(required);
            assert!(install_artifact(
                &binding,
                vec![
                    ("entries".into(), candidate),
                    ("people".into(), people.clone())
                ]
            )
            .is_err());
        }
        assert_eq!(
            collection_schema(&binding, "entries").unwrap().as_ref(),
            &fields(&valid)
        );
    }

    #[test]
    fn valid_identity_descriptors_are_installed_as_native_contracts() {
        crate::tests::fixtures::reset_engine();
        let binding = crate::tests::fixtures::harness_binding("app_explicit_identity");
        let corpus = identity_corpus();
        let cases = corpus["valid"].as_object().unwrap();
        assert!(!cases.is_empty(), "valid fixtures must not be empty");
        for (name, case) in cases {
            let fields = Value::from(case["fields"].clone());
            install_artifact(&binding, vec![("entries".into(), fields.clone())])
                .unwrap_or_else(|error| panic!("{name}: {error}"));
            assert_eq!(
                collection_schema(&binding, "entries").unwrap().as_ref(),
                &CollectionSchema::from_fields(&fields)
                    .unwrap()
                    .into_fields(),
                "{name}"
            );
        }
    }

    #[test]
    fn an_undeclared_collection_is_a_typed_error_not_a_missing_schema() {
        crate::tests::fixtures::reset_engine();
        let binding = crate::tests::fixtures::harness_binding("app_descriptor_miss");
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
        let base = crate::tests::fixtures::harness_binding("app_two_deploys");
        let edge = base.edge().expect("a harness binding addresses a database");
        let pinned = DbBinding::to_database(
            base.app_id(),
            "deploy_pinned",
            edge.database().clone(),
            edge.binding().clone(),
            edge.database_capability(),
        )
        .expect("the harness ids compose");
        let current = DbBinding::to_database(
            base.app_id(),
            "deploy_current",
            edge.database().clone(),
            edge.binding().clone(),
            edge.database_capability(),
        )
        .expect("the harness ids compose");
        zeroship_data_orm::schema_cache::with_mut(|c| {
            c.insert_one(
                &pinned,
                "secrets",
                fields(&value!({ "marker": { "type": "string" } })),
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
                fields(&value!({ "other": { "type": "string" } })),
            );
        });
        assert_eq!(
            collection_schema(&pinned, "secrets").unwrap().as_ref(),
            &fields(&value!({ "marker": { "type": "string" } })),
            "installing the current deploy redirected the pinned binding",
        );
    }
}
