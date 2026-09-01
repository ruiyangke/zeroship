use serde_json::Value;

use crate::binding::DbBinding;
use crate::error::DbError;
use crate::exec::exec_query;
use crate::query;
use crate::tx_route::TxRoute;

#[cfg(any(test, feature = "test-helpers"))]
use std::cell::RefCell;

pub(crate) enum ApplyMode<'a> {
    Insert {
        actor_id: Option<&'a str>,
    },
    InsertMany {
        actor_id: Option<&'a str>,
    },
    Update {
        row_pk: &'a str,
    },
    /// The only write mode whose PRE-pass issues SQL of its own: the
    /// deterministic-encryption conflict probe reads the existing row's
    /// id. That read has to land on the same connection the upsert
    /// itself will, so the variant carries the dispatch's
    /// [`TxRoute`] — a field, so an upsert site that has not captured a
    /// route cannot construct the mode at all.
    Upsert {
        actor_id: Option<&'a str>,
        conflict_fields: &'a Value,
        route: &'a TxRoute,
    },
}

pub(crate) fn inspect_update(
    app_id: &str,
    collection: &str,
    patch: &Value,
) -> Result<super::system_fields_pass::UpdateAutoBumpHints, DbError> {
    super::system_fields_pass::apply_system_fields_on_update(patch, app_id, collection)
}

/// DB-8: validate every top-level field key of a plain write document
/// (insert / insertMany element / upsert) with the same `validate_field_name`
/// fence the read/filter path enforces. Runs on the raw user document before
/// any system/encryption/mask pass adds its own (legitimately reserved) keys.
fn validate_user_doc_keys(doc: &Value) -> Result<(), DbError> {
    if let Some(obj) = doc.as_object() {
        for key in obj.keys() {
            query::validate_field_name(key)?;
        }
    }
    Ok(())
}

/// DB-8 (update patch): an update patch's top-level keys are either field names
/// (`{ name: "x", views: { $inc: 1 } }`) or the document-level `$set`/`$setOnInsert`
/// operators whose nested keys are field names. Validate the field-name keys
/// (skipping `$`-prefixed operator keys, whose own nested fields are checked).
fn validate_update_patch_keys(patch: &Value) -> Result<(), DbError> {
    let Some(obj) = patch.as_object() else {
        return Ok(());
    };
    for (key, value) in obj {
        if key.starts_with('$') {
            // Document-level operator (e.g. $set): its nested keys are fields.
            if let Some(nested) = value.as_object() {
                for nested_key in nested.keys() {
                    query::validate_field_name(nested_key)?;
                }
            }
        } else {
            query::validate_field_name(key)?;
        }
    }
    Ok(())
}

/// Apply the canonical write-side transform once per write site.
///
/// The ordered stages are fixed:
///
/// 1. system-field pre-pass for the write shape (`insert*`, `update`, `upsert`)
/// 2. any mode-specific row-id rewrite required before encryption (`upsert`)
/// 3. encrypt encrypted columns
/// 4. derive masked sibling columns from plaintext / sidechannel
/// 5. lower binary columns to the dialect's bind: SQLite vector/geoPoint blobs,
///    then plain `t.bytes()` (base64 wire string -> raw bytes) on both dialects
///
/// The SQL builders still own dialect lowering and UPDATE auto-bump
/// emission. This module centralises the transform stages that were
/// previously hand-wired per dispatch site.
pub(crate) async fn apply(
    binding: &DbBinding,
    collection: &str,
    payload: &mut Value,
    mode: ApplyMode<'_>,
) -> Result<(), DbError> {
    let app_id = binding.app_id();
    // DB-8: validate every USER-supplied document field key BEFORE the system /
    // encryption / mask passes below add their own (reserved-suffix / `__zsbin__`)
    // sibling columns. The write SQL builders only `quote_ident`'d these keys —
    // they skipped the `validate_field_name` fence the read/filter path enforces,
    // letting a write smuggle a null-byte key, a >63-byte key (NAMEDATALEN
    // truncation collision), or a reserved name (e.g. `ssn_masked`) straight into
    // a column. Run the same fence here, on the raw user keys, once.
    match &mode {
        ApplyMode::Insert { .. } | ApplyMode::Upsert { .. } => validate_user_doc_keys(payload)?,
        ApplyMode::InsertMany { .. } => {
            if let Some(docs) = payload.as_array() {
                for doc in docs {
                    validate_user_doc_keys(doc)?;
                }
            }
        }
        ApplyMode::Update { .. } => validate_update_patch_keys(payload)?,
    }

    // The encrypt/mask write transforms are driven by THE RUNTIME DESCRIPTOR
    // this isolate was built from, not by a live catalog read. A collection the
    // descriptor does not declare is refused here rather than written with the
    // encryption and mask stages silently skipped - which is what an absent
    // schema used to mean, on a write.
    let schema = crate::descriptor::collection_schema(binding, collection)?;
    let stages = WriteStages::new(&schema);

    match mode {
        ApplyMode::Insert { actor_id } => {
            super::system_fields_pass::apply_system_fields_on_insert(
                payload, &schema, collection, actor_id,
            );
            let row_pk = row_pk_from_doc(payload);
            stages
                .apply_to_doc(app_id, collection, &row_pk, payload)
                .await?;
            Ok(())
        }
        ApplyMode::InsertMany { actor_id } => {
            super::system_fields_pass::apply_system_fields_on_insert_many(
                payload, &schema, collection, actor_id,
            );
            let Some(docs) = payload.as_array_mut() else {
                return Ok(());
            };
            for doc in docs.iter_mut() {
                let row_pk = row_pk_from_doc(doc);
                stages
                    .apply_to_doc(app_id, collection, &row_pk, doc)
                    .await?;
            }
            Ok(())
        }
        ApplyMode::Update { row_pk } => {
            stages
                .apply_to_update(app_id, collection, row_pk, payload)
                .await?;
            Ok(())
        }
        ApplyMode::Upsert {
            actor_id,
            conflict_fields,
            route,
        } => {
            debug_assert_eq!(
                route.app_id(),
                app_id,
                "the upsert route must belong to the app being written"
            );
            super::system_fields_pass::apply_system_fields_on_insert(
                payload, &schema, collection, actor_id,
            );
            rewrite_upsert_doc_id_to_existing_row_id(
                payload,
                route,
                collection,
                conflict_fields,
                &schema,
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
    schema: &'a Value,
    has_encrypted: bool,
    has_masked: bool,
    has_sqlite_binary: bool,
    has_plain_bytes: bool,
}

impl<'a> WriteStages<'a> {
    fn new(schema: &'a Value) -> Self {
        Self {
            has_encrypted: super::schema_has_encrypted_columns(schema),
            has_masked: super::schema_has_masked_columns(schema),
            has_sqlite_binary: super::schema_has_sqlite_binary_columns(schema),
            has_plain_bytes: super::bytes_pass::schema_has_plain_bytes_columns(schema),
            schema,
        }
    }

    /// Does any stage below have work to do for this collection? A schema with
    /// none of these facets skips the whole pipeline.
    fn any(&self) -> bool {
        self.has_encrypted || self.has_masked || self.has_sqlite_binary || self.has_plain_bytes
    }

    async fn apply_to_doc(
        &self,
        app_id: &str,
        collection: &str,
        row_pk: &str,
        row: &mut Value,
    ) -> Result<(), DbError> {
        let schema = self.schema;
        if !self.any() {
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
        // Derive the masks here - from the plaintext, while the row still
        // holds the value under its logical key - but do NOT place them.
        let masks = if self.has_masked {
            super::mask_pass::apply_mask_on_write(schema, &sidechannel, row)?
        } else {
            Vec::new()
        };
        if self.has_sqlite_binary && super::current_sql_dialect() == query::SqlDialect::Sqlite {
            super::encode_sqlite_binary_doc_with_schema(schema, row)?;
        }
        // AFTER encryption: a `t.encrypted({ wraps: t.bytes() })` column is the
        // encryption pass's, and this pass skips it by construction, but the
        // ordering also means the ciphertext it deposits is never re-read as a
        // plain bytes value.
        if self.has_plain_bytes {
            super::bytes_pass::encode_bytes_on_write(schema, super::current_sql_dialect(), row)?;
        }
        // LAST. Every stage above reads and writes a masked field under its
        // LOGICAL key and knows nothing about the flip; this one moves the
        // finished value to the raw column and puts the mask in the logical
        // slot. Exactly one stage owns physical placement, and it is the one
        // that runs after all the producers.
        super::mask_pass::relocate_masked_columns(&masks, row)?;
        Ok(())
    }

    async fn apply_to_update(
        &self,
        app_id: &str,
        collection: &str,
        row_pk: &str,
        patch: &mut Value,
    ) -> Result<(), DbError> {
        let schema = self.schema;
        if !self.any() {
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
        let masks = if self.has_masked {
            super::mask_pass::apply_mask_on_write(schema, &sidechannel, target)?
        } else {
            Vec::new()
        };
        if self.has_sqlite_binary && super::current_sql_dialect() == query::SqlDialect::Sqlite {
            super::encode_sqlite_binary_update_with_schema(schema, patch)?;
        }
        if self.has_plain_bytes {
            super::bytes_pass::encode_bytes_on_update(schema, super::current_sql_dialect(), patch)?;
        }
        // LAST, on the same sub-document the encryption pass wrote to (`$set`
        // when the patch uses one). A field the patch does not mention is
        // absent from `masks`, so it is neither relocated nor re-masked and its
        // stored pair stays consistent.
        super::mask_pass::relocate_masked_columns(&masks, update_target(patch))?;
        Ok(())
    }
}

fn row_pk_from_doc(doc: &Value) -> String {
    row_pk_from_value(doc.get("id"))
}

fn row_pk_from_value(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        _ => String::new(),
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct TargetRowId {
    pub(crate) id_value: Value,
    pub(crate) row_pk: String,
}

/// Resolve the ids the pending write will touch.
///
/// Takes the dispatch's [`TxRoute`] rather than an `app_id`: this read
/// MUST see the rows the same transaction is about to update, so it has
/// to run on the same connection the update will. Reading it off the pool
/// while the update ran in a transaction would resolve pre-transaction
/// ids.
///
/// `schema` is the caller's already-resolved descriptor entry. The probe's own
/// PROJECTION is `query::empty_read_schema()` (it selects `id` alone, a
/// platform system field); this entry is only what the caller's FILTER is
/// lowered against for SQLite booleans.
pub(crate) async fn resolve_target_row_ids(
    route: &TxRoute,
    collection: &str,
    filter: &Value,
    limit: i64,
    schema: &Value,
) -> Result<Vec<TargetRowId>, DbError> {
    let app_id = route.app_id();
    note_target_row_resolution_for_tests();
    let mut sql_filter = filter.clone();
    super::maybe_lower_sqlite_boolean_filter(schema, &mut sql_filter);
    let built = query::build_write_target_probe(
        app_id,
        collection,
        &sql_filter,
        limit,
        super::current_sql_dialect(),
    )
    .map_err(DbError::from)?;
    note_target_row_resolution_sql_for_tests(&built.sql);
    let rows = exec_query(route, built).await?;
    Ok(rows
        .into_iter()
        .filter_map(|row| {
            let id_value = row.get("id")?.clone();
            Some(TargetRowId {
                row_pk: row_pk_from_value(Some(&id_value)),
                id_value,
            })
        })
        .collect())
}

/// Does this UPDATE touch a randomised-encrypted column?
///
/// The answer decides whether the dispatcher fans the update out per row (each
/// row's ciphertext is bound to its own id through the AAD) or issues one
/// multi-row statement. It reads THE DESCRIPTOR, which the caller resolved for
/// the whole operation: a collection with no entry never reaches here, because
/// `collection_schema` already refused it. Returning `false` on a missing
/// schema is what the old `Option` shape did, and it would give every row in a
/// batch the same AAD.
pub(crate) fn update_requires_per_row_encryption(schema: &Value, patch: &Value) -> bool {
    update_touches_randomised_encrypted_field(schema, patch)
}

/// The upsert twin of [`update_requires_per_row_encryption`]: a doc that writes
/// a randomised-encrypted column needs the deterministic conflict probe run
/// first, because its ciphertext cannot be compared for ON CONFLICT equality.
pub(crate) fn upsert_requires_conflict_probe(schema: &Value, doc: &Value) -> bool {
    doc_touches_randomised_encrypted_field(schema, doc)
}

fn update_touches_randomised_encrypted_field(schema: &Value, patch: &Value) -> bool {
    let Some(schema_obj) = schema.as_object() else {
        return false;
    };
    let Some(update_obj) = patch.as_object() else {
        return false;
    };

    if let Some(set_obj) = update_obj.get("$set").and_then(Value::as_object) {
        if set_obj.iter().any(|(field, _)| {
            schema_obj
                .get(field)
                .is_some_and(field_is_randomised_encrypted)
        }) {
            return true;
        }
    }

    update_obj.iter().any(|(field, value)| {
        if field.starts_with('$') || field.starts_with("__zsbin__") {
            return false;
        }
        schema_obj
            .get(field)
            .is_some_and(field_is_randomised_encrypted)
            && field_update_writes_value(value)
    })
}

fn doc_touches_randomised_encrypted_field(schema: &Value, doc: &Value) -> bool {
    let Some(schema_obj) = schema.as_object() else {
        return false;
    };
    let Some(doc_obj) = doc.as_object() else {
        return false;
    };

    doc_obj.iter().any(|(field, value)| {
        if field.starts_with("__zsbin__") || value.is_null() {
            return false;
        }
        schema_obj
            .get(field)
            .is_some_and(field_is_randomised_encrypted)
    })
}

fn field_is_randomised_encrypted(field_def: &Value) -> bool {
    field_def
        .get("encrypted")
        .and_then(Value::as_object)
        .and_then(|enc| enc.get("mode").and_then(Value::as_str))
        .is_some_and(|mode| matches!(mode, "randomised" | "randomized"))
}

fn field_update_writes_value(value: &Value) -> bool {
    match value {
        Value::Object(ops) => ops.keys().any(|key| key.starts_with('$')),
        _ => true,
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
    route: &TxRoute,
    collection: &str,
    conflict_fields: &Value,
    schema: &Value,
) -> Result<(), DbError> {
    let app_id = route.app_id();
    if !upsert_requires_conflict_probe(schema, doc) {
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
    if let Some(probe_schema) = deterministic_conflict_probe_schema(schema, conflict_arr)? {
        let mut sidechannel = super::mask_pass::MaskPlaintextSidechannel::new();
        super::encryption_pass_dispatch(
            app_id,
            collection,
            &probe_schema,
            "",
            &mut filter,
            &mut sidechannel,
        )
        .await?;
    }
    note_upsert_conflict_probe_for_tests();
    super::maybe_lower_sqlite_boolean_filter(schema, &mut filter);
    let built = query::build_conflict_probe_with_dialect(
        app_id,
        collection,
        &filter,
        super::current_sql_dialect(),
    )
    .map_err(DbError::from)?;
    let rows = exec_query(route, built).await?;
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

#[cfg(any(test, feature = "test-helpers"))]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WritePathCounters {
    pub target_row_resolution_calls: usize,
    pub upsert_conflict_probe_calls: usize,
    pub target_row_resolution_sql: Vec<String>,
}

#[cfg(any(test, feature = "test-helpers"))]
thread_local! {
    static WRITE_PATH_COUNTERS: RefCell<WritePathCounters> =
        RefCell::new(WritePathCounters::default());
}

#[cfg(any(test, feature = "test-helpers"))]
#[cfg_attr(test, allow(dead_code))]
pub fn reset_write_path_counters_for_tests() {
    WRITE_PATH_COUNTERS.with(|counters| {
        *counters.borrow_mut() = WritePathCounters::default();
    });
}

#[cfg(any(test, feature = "test-helpers"))]
#[cfg_attr(test, allow(dead_code))]
pub fn write_path_counters_for_tests() -> WritePathCounters {
    WRITE_PATH_COUNTERS.with(|counters| counters.borrow().clone())
}

#[cfg(any(test, feature = "test-helpers"))]
fn note_target_row_resolution_for_tests() {
    WRITE_PATH_COUNTERS.with(|counters| {
        counters.borrow_mut().target_row_resolution_calls += 1;
    });
}

#[cfg(not(any(test, feature = "test-helpers")))]
fn note_target_row_resolution_for_tests() {}

#[cfg(any(test, feature = "test-helpers"))]
fn note_target_row_resolution_sql_for_tests(sql: &str) {
    WRITE_PATH_COUNTERS.with(|counters| {
        counters
            .borrow_mut()
            .target_row_resolution_sql
            .push(sql.to_string());
    });
}

#[cfg(not(any(test, feature = "test-helpers")))]
fn note_target_row_resolution_sql_for_tests(_sql: &str) {}

#[cfg(any(test, feature = "test-helpers"))]
fn note_upsert_conflict_probe_for_tests() {
    WRITE_PATH_COUNTERS.with(|counters| {
        counters.borrow_mut().upsert_conflict_probe_calls += 1;
    });
}

#[cfg(not(any(test, feature = "test-helpers")))]
fn note_upsert_conflict_probe_for_tests() {}

fn deterministic_conflict_probe_schema(
    schema: &Value,
    conflict_fields: &[Value],
) -> Result<Option<Value>, DbError> {
    let Some(schema_obj) = schema.as_object() else {
        return Ok(None);
    };
    let mut out = serde_json::Map::new();
    for field in conflict_fields.iter().filter_map(Value::as_str) {
        let Some(def) = schema_obj.get(field) else {
            continue;
        };
        let Some(enc) = def.get("encrypted").and_then(Value::as_object) else {
            continue;
        };
        let Some(mode) = enc.get("mode").and_then(Value::as_str) else {
            continue;
        };
        match mode {
            "deterministic" => {
                out.insert(field.to_string(), def.clone());
            }
            "randomised" | "randomized" => {
                return Err(DbError::validation(
                    "upsert_conflict_field_requires_deterministic_encryption",
                    format!(
                        "upsert: conflict field `{field}` uses randomised encryption; ON CONFLICT equality requires deterministic ciphertext"
                    ),
                ));
            }
            _ => {}
        }
    }
    if out.is_empty() {
        Ok(None)
    } else {
        Ok(Some(Value::Object(out)))
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::rc::Rc;

    use base64::Engine as _;
    use serde_json::Value;

    use crate::binding::DbBinding;
    use crate::tx_route::TxRoute;

    use super::{
        ApplyMode, apply, inspect_update, validate_update_patch_keys, validate_user_doc_keys,
    };
    use crate::backend::sqlite::SqliteBackend;

    #[test]
    fn db8_rejects_reserved_and_malformed_user_doc_keys() {
        use serde_json::json;
        // A normal document passes.
        assert!(validate_user_doc_keys(&json!({ "name": "a", "ssn": "x" })).is_ok());
        // The user must not forge the masked sibling suffix the platform emits.
        assert!(validate_user_doc_keys(&json!({ "ssn_masked": "x" })).is_err());
        // Nor a platform-internal `_`-prefixed name (covers `__zsbin__` markers,
        // `__zs_`, synthetic `_rank`/`_score`).
        assert!(validate_user_doc_keys(&json!({ "__zsbin__ssn": true })).is_err());
        assert!(validate_user_doc_keys(&json!({ "_rank": 1 })).is_err());
        // Null-byte and >63-byte keys (NAMEDATALEN truncation collision).
        assert!(validate_user_doc_keys(&json!({ "a\u{0}b": 1 })).is_err());
        let long = "x".repeat(64);
        assert!(validate_user_doc_keys(&json!({ long: 1 })).is_err());
    }

    #[test]
    fn db8_update_patch_validates_field_keys_not_operators() {
        use serde_json::json;
        // Plain field keys + a field-scoped operator value pass.
        assert!(
            validate_update_patch_keys(&json!({ "name": "a", "views": { "$inc": 1 } })).is_ok()
        );
        // $set's nested field keys are validated; the operator key itself is skipped.
        assert!(validate_update_patch_keys(&json!({ "$set": { "name": "a" } })).is_ok());
        assert!(validate_update_patch_keys(&json!({ "$set": { "ssn_masked": "x" } })).is_err());
        // A top-level reserved field key is rejected.
        assert!(validate_update_patch_keys(&json!({ "ssn_masked": "x" })).is_err());
    }
    use crate::backend::{EncryptedColumn as _, EncryptionMode, SqlExecutor};
    use crate::encryption;
    use crate::query::{
        FkEmission, SqlDialect, build_create_table_with_fks_for_dialect, build_insert_with_dialect,
    };
    use crate::{cache_schema_for_tests, set_sqlite_backend_for_tests};

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
            row.get("ssn").and_then(Value::as_str),
            Some(last4_mask(expected_plaintext).as_str()),
            "the field's own column must carry the mask after the relocation stage",
        );

        let raw_col = crate::query::raw_column_name("ssn");
        let ciphertext_b64 = row
            .get(&raw_col)
            .and_then(Value::as_str)
            .expect("the raw column should carry the ciphertext base64");
        assert_ne!(
            ciphertext_b64, expected_plaintext,
            "write pipeline must not leave plaintext in the write doc",
        );
        // The binary-bind marker followed the value to the raw column, or the
        // INSERT would bind base64 text into a BYTEA column.
        assert!(
            row.get(format!("__zsbin__{raw_col}").as_str()).is_some()
                && row.get("__zsbin__ssn").is_none(),
            "the binary-bind marker must name the raw column: {row}",
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
            // Hand the root key to the isolate rather than the process
            // environment. `SqliteBackend::new` below reads this source, so
            // the encrypt/decrypt legs still run through the real
            // `KeyStore::resolve` + HKDF derivation.
            let _keys = crate::supply_root_keys_for_tests(&[(key_id, &"1".repeat(64))]);
            let app_id = "app_write_pipeline";
            let binding = DbBinding::cold_start(app_id);
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
                crate::backend_selection::new_sqlite_backend(PathBuf::from(dir.path()))
                    .expect("open sqlite backend"),
            );
            backend
                .attach_app_file(app_id)
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
                &binding,
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

            // `schema` was moved into `cache_schema_for_tests`; `ddl_schema` is
            // its byte-identical twin and is still owned here.
            let insert_built = build_insert_with_dialect(
                app_id,
                collection,
                &ddl_schema,
                &insert_doc,
                SqlDialect::Sqlite,
            )
            .expect("build insert");
            let insert_params: Vec<&str> = insert_built.params.iter().map(String::as_str).collect();
            let client = backend
                .acquire_dedicated_client(app_id)
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
                &binding,
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

            let mut update_patch = serde_json::json!({
                "$set": {
                    "ssn": "555-55-5555",
                    "updated_by": "usr_override"
                }
            });
            let update_hints =
                inspect_update(app_id, collection, &update_patch).expect("inspect update patch");
            assert!(
                update_hints.creator_supplied_updated_by,
                "update system-field pre-pass should surface creator overrides",
            );
            apply(
                &binding,
                collection,
                &mut update_patch,
                ApplyMode::Update {
                    row_pk: seeded_id.as_str(),
                },
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
                update_target.get("ssn").and_then(Value::as_str),
                Some(last4_mask("555-55-5555").as_str()),
                "the relocation stage must target the same $set object the \
                 encryption pass wrote to",
            );
            let update_ciphertext = update_target
                .get(crate::query::raw_column_name("ssn").as_str())
                .and_then(Value::as_str)
                .expect("update ssn ciphertext in the raw column");
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
                update_plaintext, b"555-55-5555",
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
                &binding,
                collection,
                &mut upsert_doc,
                ApplyMode::Upsert {
                    actor_id: Some("usr_upsert"),
                    conflict_fields: &conflict_fields,
                    // No isolate in a unit test: this path is exercised
                    // outside any transaction, which is what the pool
                    // route means.
                    route: &TxRoute::pool_for_tests(app_id),
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
