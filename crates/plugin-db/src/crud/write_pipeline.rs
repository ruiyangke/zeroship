use serde_json::Value;

use crate::error::DbError;
use crate::exec::exec_query;
use crate::query;

pub(crate) enum ApplyMode<'a> {
    Insert { actor_id: Option<&'a str> },
    InsertMany { actor_id: Option<&'a str> },
    Update { filter: &'a Value },
    Upsert {
        actor_id: Option<&'a str>,
        conflict_fields: &'a Value,
    },
}

pub(crate) fn inspect_update(
    app_id: &str,
    collection: &str,
    patch: &Value,
) -> Result<super::system_fields_pass::UpdateAutoBumpHints, DbError> {
    super::system_fields_pass::apply_system_fields_on_update(patch, app_id, collection)
}

/// Apply the canonical write-side transform once per write site.
///
/// The ordered stages are fixed:
///
/// 1. system-field pre-pass for the write shape (`insert*`, `update`, `upsert`)
/// 2. any mode-specific row-id rewrite required before encryption (`upsert`)
/// 3. encrypt encrypted columns
/// 4. derive masked sibling columns from plaintext / sidechannel
///
/// The SQL builders still own dialect lowering and UPDATE auto-bump
/// emission. This module centralises the transform stages that were
/// previously hand-wired per dispatch site.
pub(crate) async fn apply(
    app_id: &str,
    collection: &str,
    payload: &mut Value,
    mode: ApplyMode<'_>,
) -> Result<(), DbError> {
    let schema = crate::context::with(|c| c.schema_for(app_id, collection));
    let stages = WriteStages::new(schema.as_ref());

    match mode {
        ApplyMode::Insert { actor_id } => {
            super::system_fields_pass::apply_system_fields_on_insert(
                payload,
                app_id,
                collection,
                actor_id,
            );
            let row_pk = row_pk_from_doc(payload);
            stages
                .apply_to_doc(app_id, collection, &row_pk, payload)
                .await?;
            Ok(())
        }
        ApplyMode::InsertMany { actor_id } => {
            super::system_fields_pass::apply_system_fields_on_insert_many(
                payload,
                app_id,
                collection,
                actor_id,
            );
            let Some(docs) = payload.as_array_mut() else {
                return Ok(());
            };
            for doc in docs.iter_mut() {
                let row_pk = row_pk_from_doc(doc);
                stages.apply_to_doc(app_id, collection, &row_pk, doc).await?;
            }
            Ok(())
        }
        ApplyMode::Update { filter } => {
            let row_pk = row_pk_from_filter(filter);
            stages
                .apply_to_update(app_id, collection, &row_pk, payload)
                .await?;
            Ok(())
        }
        ApplyMode::Upsert {
            actor_id,
            conflict_fields,
        } => {
            super::system_fields_pass::apply_system_fields_on_insert(
                payload,
                app_id,
                collection,
                actor_id,
            );
            rewrite_upsert_doc_id_to_existing_row_id(
                payload,
                app_id,
                collection,
                conflict_fields,
                schema.as_ref(),
            )
            .await?;
            let row_pk = row_pk_from_doc(payload);
            stages
                .apply_to_doc(app_id, collection, &row_pk, payload)
                .await?;
            Ok(())
        }
    }
}

struct WriteStages<'a> {
    schema: Option<&'a Value>,
    has_encrypted: bool,
    has_masked: bool,
    has_sqlite_binary: bool,
}

impl<'a> WriteStages<'a> {
    fn new(schema: Option<&'a Value>) -> Self {
        Self {
            has_encrypted: schema.is_some_and(super::schema_has_encrypted_columns),
            has_masked: schema.is_some_and(super::schema_has_masked_columns),
            has_sqlite_binary: schema.is_some_and(super::schema_has_sqlite_binary_columns),
            schema,
        }
    }

    async fn apply_to_doc(
        &self,
        app_id: &str,
        collection: &str,
        row_pk: &str,
        row: &mut Value,
    ) -> Result<(), DbError> {
        let Some(schema) = self.schema else {
            return Ok(());
        };
        if !self.has_encrypted && !self.has_masked && !self.has_sqlite_binary {
            return Ok(());
        }

        let mut sidechannel = super::mask_pass::MaskPlaintextSidechannel::new();
        if self.has_encrypted {
            super::encryption_pass_dispatch(
                app_id,
                collection,
                schema,
                row_pk,
                row,
                &mut sidechannel,
            )
            .await?;
        }
        if self.has_masked {
            super::mask_pass::apply_mask_on_write(schema, &sidechannel, row)?;
        }
        if self.has_sqlite_binary && super::current_sql_dialect() == query::SqlDialect::Sqlite {
            super::encode_sqlite_binary_doc_with_schema(schema, row)?;
        }
        Ok(())
    }

    async fn apply_to_update(
        &self,
        app_id: &str,
        collection: &str,
        row_pk: &str,
        patch: &mut Value,
    ) -> Result<(), DbError> {
        let Some(schema) = self.schema else {
            return Ok(());
        };
        if !self.has_encrypted && !self.has_masked && !self.has_sqlite_binary {
            return Ok(());
        }

        let target = update_target(patch);
        let mut sidechannel = super::mask_pass::MaskPlaintextSidechannel::new();
        if self.has_encrypted {
            super::encryption_pass_dispatch(
                app_id,
                collection,
                schema,
                row_pk,
                target,
                &mut sidechannel,
            )
            .await?;
        }
        if self.has_masked {
            super::mask_pass::apply_mask_on_write(schema, &sidechannel, target)?;
        }
        if self.has_sqlite_binary && super::current_sql_dialect() == query::SqlDialect::Sqlite {
            super::encode_sqlite_binary_update_with_schema(schema, patch)?;
        }
        Ok(())
    }
}

fn row_pk_from_doc(doc: &Value) -> String {
    row_pk_from_value(doc.get("id"))
}

fn row_pk_from_filter(filter: &Value) -> String {
    row_pk_from_value(filter.get("id"))
}

fn row_pk_from_value(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        _ => String::new(),
    }
}

fn update_target(patch: &mut Value) -> &mut Value {
    if patch.get("$set").is_some() {
        patch.get_mut("$set").expect("checked above")
    } else {
        patch
    }
}

async fn rewrite_upsert_doc_id_to_existing_row_id(
    doc: &mut Value,
    app_id: &str,
    collection: &str,
    conflict_fields: &Value,
    schema: Option<&Value>,
) -> Result<(), DbError> {
    let Some(schema) = schema else {
        return Ok(());
    };
    if !super::schema_has_encrypted_columns(schema) {
        return Ok(());
    }
    let Some(obj) = doc.as_object_mut() else {
        return Ok(());
    };
    let Some(conflict_arr) = conflict_fields.as_array() else {
        return Ok(());
    };
    if conflict_arr.is_empty() {
        return Ok(());
    }

    let mut filter_obj = serde_json::Map::with_capacity(conflict_arr.len());
    for field in conflict_arr.iter().filter_map(Value::as_str) {
        let Some(value) = obj.get(field).cloned() else {
            return Ok(());
        };
        filter_obj.insert(field.to_string(), value);
    }
    if filter_obj.len() != conflict_arr.len() {
        return Ok(());
    }

    let mut filter = Value::Object(filter_obj);
    super::maybe_lower_sqlite_boolean_filter(app_id, collection, &mut filter);
    let select = serde_json::json!(["id"]);
    let built = query::build_find(
        app_id,
        collection,
        &filter,
        Some(1),
        None,
        None,
        Some(&select),
    )
    .map_err(DbError::from)?;
    let rows = exec_query(app_id, built).await?;
    let Some(existing_id) = rows.first().and_then(|row| match row.get("id") {
        Some(Value::String(id)) => Some(id.clone()),
        Some(Value::Number(n)) => Some(n.to_string()),
        _ => None,
    }) else {
        return Ok(());
    };
    obj.insert("id".to_string(), Value::String(existing_id));
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::rc::Rc;

    use base64::Engine as _;
    use serde_json::Value;

    use super::{apply, inspect_update, ApplyMode};
    use crate::backend::sqlite::SqliteBackend;
    use crate::backend::{EncryptedColumn as _, EncryptionMode, NamespaceManager, SqlExecutor};
    use crate::encryption;
    use crate::query::{
        build_create_table_with_fks_for_dialect, build_insert_with_dialect, FkEmission,
        SqlDialect,
    };
    use crate::{cache_schema_for_tests, set_sqlite_backend_for_tests};

    struct ScopedEnvVar {
        name: String,
        prior: Option<String>,
    }

    impl ScopedEnvVar {
        fn set(name: &str, value: &str) -> Self {
            let prior = std::env::var(name).ok();
            std::env::set_var(name, value);
            Self {
                name: name.to_string(),
                prior,
            }
        }
    }

    impl Drop for ScopedEnvVar {
        fn drop(&mut self) {
            if let Some(prior) = self.prior.as_deref() {
                std::env::set_var(&self.name, prior);
            } else {
                std::env::remove_var(&self.name);
            }
        }
    }

    fn run<F: std::future::Future>(f: F) -> F::Output {
        compio::runtime::Runtime::new()
            .expect("compio runtime build")
            .block_on(f)
    }

    fn last4_mask(plaintext: &str) -> String {
        let suffix: String = plaintext
            .chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .rev()
            .take(4)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        format!("***-**-{suffix}")
    }

    async fn assert_encrypted_masked_row(
        backend: &SqliteBackend,
        app_id: &str,
        collection: &str,
        key_id: &str,
        row: &Value,
        expected_plaintext: &str,
        expected_actor: Option<&str>,
    ) {
        let id = row
            .get("id")
            .and_then(Value::as_str)
            .expect("row must carry id");
        if let Some(actor) = expected_actor {
            assert_eq!(
                row.get("created_by").and_then(Value::as_str),
                Some(actor),
                "created_by should be populated by the system-field stage",
            );
            assert_eq!(
                row.get("updated_by").and_then(Value::as_str),
                Some(actor),
                "updated_by should be populated by the system-field stage",
            );
        }
        assert_eq!(
            row.get("ssn_masked").and_then(Value::as_str),
            Some(last4_mask(expected_plaintext).as_str()),
            "mask stage must derive the sibling from plaintext",
        );

        let ciphertext_b64 = row
            .get("ssn")
            .and_then(Value::as_str)
            .expect("ssn should be ciphertext base64");
        assert_ne!(
            ciphertext_b64, expected_plaintext,
            "write pipeline must not leave plaintext in the write doc",
        );
        let ciphertext = base64::engine::general_purpose::STANDARD
            .decode(ciphertext_b64)
            .expect("ciphertext base64 must decode");
        let key = backend
            .resolve_key(app_id, key_id)
            .await
            .expect("resolve key");
        let aad = encryption::canonical_aad(collection, "ssn", Some(id.as_bytes()));
        let plaintext = backend
            .decrypt(&key, EncryptionMode::Randomised, &ciphertext, &aad)
            .expect("decrypt prepared ciphertext");
        assert_eq!(
            plaintext,
            expected_plaintext.as_bytes(),
            "prepared ciphertext must round-trip through the backend decryptor",
        );
    }

    #[test]
    fn write_pipeline_applies_every_stage_across_insert_many_update_and_upsert() {
        run(async {
            let key_id = "write_pipeline_uniform";
            let _env = ScopedEnvVar::set(
                "ZEROSHIP_COLUMN_KEY_WRITE_PIPELINE_UNIFORM",
                &"1".repeat(64),
            );
            let app_id = "app_write_pipeline";
            let collection = "users";
            let schema = serde_json::json!({
                "email": { "type": "string", "required": true, "unique": true },
                "name": { "type": "string", "required": true },
                "ssn": {
                    "type": "string",
                    "encrypted": { "mode": "randomised", "keyId": key_id, "wraps": "string" },
                    "mask": { "kind": "last4", "classification": "spi" }
                }
            });
            let ddl_schema = serde_json::json!({
                "email": { "type": "string", "required": true, "unique": true },
                "name": { "type": "string", "required": true },
                "ssn": {
                    "type": "string",
                    "encrypted": { "mode": "randomised", "keyId": key_id, "wraps": "string" },
                    "mask": { "kind": "last4", "classification": "spi" }
                }
            });

            let dir = tempfile::tempdir().expect("tempdir");
            let backend = Rc::new(
                SqliteBackend::new(PathBuf::from(dir.path())).expect("open sqlite backend"),
            );
            backend
                .ensure_app_schema(app_id)
                .await
                .expect("ensure schema");
            set_sqlite_backend_for_tests(Rc::clone(&backend));
            cache_schema_for_tests(app_id, collection, schema);

            let ddl = build_create_table_with_fks_for_dialect(
                app_id,
                collection,
                &ddl_schema,
                &FkEmission::Inline,
                SqlDialect::Sqlite,
            )
            .expect("build DDL");
            for stmt in ddl.split(";\n") {
                let trimmed = stmt.trim();
                if trimmed.is_empty() {
                    continue;
                }
                backend.pool_exec(trimmed, &[]).await.expect("DDL exec");
            }

            let mut insert_doc = serde_json::json!({
                "email": "seed@example.com",
                "name": "Seed",
                "ssn": "123-45-6789"
            });
            apply(
                app_id,
                collection,
                &mut insert_doc,
                ApplyMode::Insert {
                    actor_id: Some("usr_insert"),
                },
            )
            .await
            .expect("prepare insert doc");
            assert_encrypted_masked_row(
                backend.as_ref(),
                app_id,
                collection,
                key_id,
                &insert_doc,
                "123-45-6789",
                Some("usr_insert"),
            )
            .await;

            let insert_built =
                build_insert_with_dialect(app_id, collection, &insert_doc, SqlDialect::Sqlite)
                    .expect("build insert");
            let insert_params: Vec<&str> = insert_built.params.iter().map(String::as_str).collect();
            let client = backend
                .acquire_dedicated_client()
                .await
                .expect("acquire client");
            client
                .query_typed_internal(&insert_built.sql, &insert_params)
                .await
                .expect("seed insert");
            let seeded_id = insert_doc
                .get("id")
                .and_then(Value::as_str)
                .expect("seeded row id")
                .to_string();

            let mut bulk_docs = serde_json::json!([
                {
                    "email": "bulk-a@example.com",
                    "name": "Bulk A",
                    "ssn": "987-65-4321"
                },
                {
                    "email": "bulk-b@example.com",
                    "name": "Bulk B",
                    "ssn": "111-22-3333"
                }
            ]);
            apply(
                app_id,
                collection,
                &mut bulk_docs,
                ApplyMode::InsertMany {
                    actor_id: Some("usr_bulk"),
                },
            )
            .await
            .expect("prepare insertMany docs");
            let bulk = bulk_docs.as_array().expect("bulk docs array");
            assert_eq!(bulk.len(), 2, "insertMany fixture must keep both docs");
            assert_encrypted_masked_row(
                backend.as_ref(),
                app_id,
                collection,
                key_id,
                &bulk[0],
                "987-65-4321",
                Some("usr_bulk"),
            )
            .await;
            assert_encrypted_masked_row(
                backend.as_ref(),
                app_id,
                collection,
                key_id,
                &bulk[1],
                "111-22-3333",
                Some("usr_bulk"),
            )
            .await;

            let filter = serde_json::json!({ "id": seeded_id.clone() });
            let mut update_patch = serde_json::json!({
                "$set": {
                    "ssn": "555-55-5555",
                    "updated_by": "usr_override"
                }
            });
            let update_hints = inspect_update(app_id, collection, &update_patch)
                .expect("inspect update patch");
            assert!(
                update_hints.creator_supplied_updated_by,
                "update system-field pre-pass should surface creator overrides",
            );
            apply(
                app_id,
                collection,
                &mut update_patch,
                ApplyMode::Update { filter: &filter },
            )
            .await
            .expect("prepare update patch");
            let update_target = update_patch
                .get("$set")
                .expect("update target should stay nested under $set");
            assert_eq!(
                update_target.get("updated_by").and_then(Value::as_str),
                Some("usr_override"),
                "update pipeline must preserve explicit updated_by override",
            );
            assert_eq!(
                update_target.get("ssn_masked").and_then(Value::as_str),
                Some(last4_mask("555-55-5555").as_str()),
                "update mask stage must target the same $set object",
            );
            let update_ciphertext = update_target
                .get("ssn")
                .and_then(Value::as_str)
                .expect("update ssn ciphertext");
            let update_key = backend
                .resolve_key(app_id, key_id)
                .await
                .expect("resolve update key");
            let update_plaintext = backend
                .decrypt(
                    &update_key,
                    EncryptionMode::Randomised,
                    &base64::engine::general_purpose::STANDARD
                        .decode(update_ciphertext)
                        .expect("decode update ciphertext"),
                    &encryption::canonical_aad(collection, "ssn", Some(seeded_id.as_bytes())),
                )
                .expect("decrypt update ciphertext");
            assert_eq!(
                update_plaintext,
                b"555-55-5555",
                "update pipeline must encrypt against the filter row id",
            );

            let conflict_fields = serde_json::json!(["email"]);
            let mut upsert_doc = serde_json::json!({
                "id": "user_new",
                "email": "seed@example.com",
                "name": "Seed Updated",
                "ssn": "222-33-4444"
            });
            apply(
                app_id,
                collection,
                &mut upsert_doc,
                ApplyMode::Upsert {
                    actor_id: Some("usr_upsert"),
                    conflict_fields: &conflict_fields,
                },
            )
            .await
            .expect("prepare upsert doc");
            assert_eq!(
                upsert_doc.get("id").and_then(Value::as_str),
                Some(seeded_id.as_str()),
                "upsert pipeline must rewrite id to the existing conflict row before encryption",
            );
            assert_encrypted_masked_row(
                backend.as_ref(),
                app_id,
                collection,
                key_id,
                &upsert_doc,
                "222-33-4444",
                Some("usr_upsert"),
            )
            .await;
        });
    }
}
