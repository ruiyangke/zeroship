use zeroship_data_sql::value::Value;

use crate::compile;
use crate::exec::exec_query;
use crate::tx_route::TxRoute;
use zeroship_data_orm::binding::DbBinding;
use zeroship_data_orm::error::DbError;

#[cfg(any(test, feature = "test-helpers"))]
use std::cell::RefCell;

pub enum ApplyMode<'a> {
    Insert {
        actor_id: Option<&'a str>,
    },
    InsertMany {
        actor_id: Option<&'a str>,
    },
    Update {
        row_pk: &'a str,
    },
    /// The deterministic-encryption conflict probe reads the existing row's id
    /// before the write is composed.
    ///
    /// It used to be the ONLY pre-pass issuing SQL of its own, so this variant
    /// carried the dispatch's [`TxRoute`] as a field - which made an upsert site
    /// that had not captured a route unable to construct the mode at all. The
    /// protection-floor fence now issues a catalog read for EVERY mode, so
    /// [`apply`] takes the route as a parameter and the same "no route, no
    /// write" property holds for all four rather than one.
    Upsert {
        actor_id: Option<&'a str>,
        conflict_fields: &'a Value,
    },
}

/// Run the UPDATE-time assignment pass: refuse a patch that rewrites a column
/// the platform fixed at insert, and STRIP the columns it re-assigns on every
/// write so the builder's own bumps are the only assignment to them.
///
/// Takes the patch by `&mut` because the strip is the point. It ran before the
/// strip existed and returned a set of "the creator supplied this, skip your
/// bump" hints; under `assign` there is no such thing as a creator-supplied
/// value for an assigned column.
pub fn inspect_update(app_id: &str, collection: &str, patch: &mut Value) -> Result<(), DbError> {
    super::system_fields_pass::apply_system_fields_on_update(patch, app_id, collection)
}

/// DB-8: validate every top-level field key of a plain write document
/// (insert / insertMany element / upsert) with the same `validate_field_name`
/// fence the read/filter path enforces. Runs on the raw user document before
/// any system/encryption/mask pass adds its own (legitimately reserved) keys.
fn validate_user_doc_keys(doc: &Value) -> Result<(), DbError> {
    if let Some(obj) = doc.as_object() {
        for key in obj.keys() {
            compile::validate_field_name(key)?;
        }
    }
    Ok(())
}

/// `id` is platform-assigned, so a creator-supplied one is refused rather than
/// honoured.
///
/// **Why here and not in the system-fields pass.** That pass mints the id, and
/// it is documented and tested as idempotent - so a check keyed on `id` being
/// PRESENT cannot tell a creator's value from one the pass itself minted on an
/// earlier call. This boundary runs on the raw document before any pass, so
/// presence here means exactly one thing: the creator sent it.
///
/// Refused rather than silently dropped, so a creator cannot believe the id
/// they chose is the id the row has.
fn refuse_platform_assigned_id(doc: &Value) -> Result<(), DbError> {
    let Some(obj) = doc.as_object() else {
        return Ok(());
    };
    if obj.contains_key("id") {
        return Err(DbError::validation(
            "platform_assigned_field",
            "`id` is assigned by the platform; remove it from the document",
        ));
    }
    Ok(())
}

fn validate_upsert_conflict_fields(
    schema: &Value,
    doc: &Value,
    conflict_fields: &Value,
) -> Result<(), DbError> {
    let fields = compile::parse_conflict_fields(conflict_fields)?;
    let assignments = crate::system_shape_charter::plan()?;
    for field in fields {
        if assignments
            .columns()
            .iter()
            .any(|column| column.name == field)
        {
            return Err(DbError::validation(
                "platform_assigned_conflict_field",
                format!(
                    "upsert conflict field '{field}' is platform-assigned; use an application-owned unique key"
                ),
            ));
        }
        if schema.get(field).is_none() || doc.get(field).is_none() {
            return Err(DbError::validation(
                "invalid_upsert_conflict_field",
                format!(
                    "upsert conflict field '{field}' must be declared and supplied in the document"
                ),
            ));
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
                    compile::validate_field_name(nested_key)?;
                }
            }
        } else {
            compile::validate_field_name(key)?;
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
///
/// `keys` is a PARAMETER because the encryption stage needs a key store and
/// nothing more. It used to reach one by resolving a backend inside
/// `super::encryption_pass_dispatch`, through the engine funnel that read
/// ADAPTER state; a key is not a routing decision, so the store is passed in
/// by the caller that already holds the handle this write will run on. It is a
/// borrow rather than an `Option`, so a caller with no key store fails to
/// compile instead of failing mid-write.
///
/// `dialect` is a PARAMETER for exactly the same reason. Stage 5 needs to know
/// which bind form the column takes and nothing more; a dialect is not a
/// routing decision either. It used to be read here as
/// `super::current_sql_dialect()`, the same engine-reads-adapter funnel, four
/// separate times per write. It now rides down from the caller that resolved
/// it - off the dispatch's captured route in production - so one write op
/// lowers under ONE dialect rather than four independent derivations of it.
///
/// `route` is a parameter for the OPPOSITE reason, and the contrast is the
/// point: stage 0 below reads the live catalog, which IS a routing decision. A
/// key store and a dialect are not, so they stay separate arguments rather than
/// being derived from the route here - a write that lowers under `route`'s
/// dialect and one that lowers under a dialect its caller resolved must remain
/// distinguishable in the signature.
pub async fn apply(
    keys: &crate::encryption::KeyStore,
    dialect: compile::SqlDialect,
    route: &TxRoute,
    binding: &DbBinding,
    collection: &str,
    payload: &mut Value,
    mode: ApplyMode<'_>,
) -> Result<(), DbError> {
    let app_id = binding.app_id();
    debug_assert_eq!(
        route.app_id(),
        app_id,
        "the write route must belong to the app being written"
    );
    // DB-8: validate every USER-supplied document field key BEFORE the system /
    // encryption / mask passes below add their own reserved sibling columns. The write SQL builders only `quote_ident`'d these keys —
    // they skipped the `validate_field_name` fence the read/filter path enforces,
    // letting a write smuggle a null-byte key, a >63-byte key (NAMEDATALEN
    // truncation collision), or a reserved name (e.g. `ssn_masked`) straight into
    // a column. Run the same fence here, on the raw user keys, once.
    match &mode {
        ApplyMode::Insert { .. } | ApplyMode::Upsert { .. } => {
            validate_user_doc_keys(payload)?;
            refuse_platform_assigned_id(payload)?;
        }
        ApplyMode::InsertMany { .. } => {
            if let Some(docs) = payload.as_array() {
                for doc in docs {
                    validate_user_doc_keys(doc)?;
                    refuse_platform_assigned_id(doc)?;
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
    if let ApplyMode::Upsert { conflict_fields, .. } = &mode {
        validate_upsert_conflict_fields(&schema, payload, conflict_fields)?;
    }
    match &mode {
        ApplyMode::Insert { .. } | ApplyMode::Upsert { .. } => {
            zeroship_data_sql::codecs::prepare_temporal_document(&schema, payload)?;
        }
        ApplyMode::InsertMany { .. } => {
            if let Some(documents) = payload.as_array_mut() {
                for document in documents {
                    zeroship_data_sql::codecs::prepare_temporal_document(&schema, document)?;
                }
            }
        }
        ApplyMode::Update { .. } => {
            zeroship_data_sql::codecs::prepare_temporal_update(&schema, payload)?;
        }
    }
    // Stage 0. The descriptor decides which protections the stages below APPLY;
    // the live catalog decides which ones this collection is ALLOWED to have
    // lost. Deleting a `mask` or `encrypted` key from a field is otherwise a
    // silent downgrade - the stages simply find nothing to do and the real value
    // is written in the clear. See `super::protection_floor`.
    super::protection_floor::refuse_protection_downgrade(route, binding, collection, &schema)
        .await?;
    let stages = WriteStages::new(&schema);

    match mode {
        ApplyMode::Insert { actor_id } => {
            super::system_fields_pass::apply_system_fields_on_insert(
                payload, &schema, collection, actor_id,
            )?;
            let row_pk = row_pk_from_doc(payload);
            stages
                .apply_to_doc(keys, dialect, app_id, collection, &row_pk, payload)
                .await?;
            Ok(())
        }
        ApplyMode::InsertMany { actor_id } => {
            super::system_fields_pass::apply_system_fields_on_insert_many(
                payload, &schema, collection, actor_id,
            )?;
            let Some(docs) = payload.as_array_mut() else {
                return Ok(());
            };
            for doc in docs.iter_mut() {
                let row_pk = row_pk_from_doc(doc);
                stages
                    .apply_to_doc(keys, dialect, app_id, collection, &row_pk, doc)
                    .await?;
            }
            Ok(())
        }
        ApplyMode::Update { row_pk } => {
            stages
                .apply_to_update(keys, dialect, app_id, collection, row_pk, payload)
                .await?;
            Ok(())
        }
        ApplyMode::Upsert {
            actor_id,
            conflict_fields,
        } => {
            super::system_fields_pass::apply_system_fields_on_insert(
                payload, &schema, collection, actor_id,
            )?;
            rewrite_upsert_doc_id_to_existing_row_id(
                keys,
                dialect,
                payload,
                route,
                collection,
                conflict_fields,
                &schema,
            )
            .await?;
            let row_pk = row_pk_from_doc(payload);
            stages
                .apply_to_doc(keys, dialect, app_id, collection, &row_pk, payload)
                .await?;
            Ok(())
        }
    }
}

struct WriteStages<'a> {
    schema: &'a Value,
    has_encrypted: bool,
    has_masked: bool,
    has_storage_encoding: bool,
    has_plain_bytes: bool,
}

impl<'a> WriteStages<'a> {
    fn new(schema: &'a Value) -> Self {
        Self {
            has_encrypted: super::schema_has_encrypted_columns(schema),
            has_masked: super::schema_has_masked_columns(schema),
            has_storage_encoding: zeroship_data_sql::codecs::has_storage_encoding(schema),
            has_plain_bytes: super::bytes_pass::schema_has_plain_bytes_columns(schema),
            schema,
        }
    }

    /// Does any stage below have work to do for this collection? A schema with
    /// none of these facets skips the whole pipeline.
    fn any(&self) -> bool {
        self.has_encrypted || self.has_masked || self.has_storage_encoding || self.has_plain_bytes
    }

    /// `keys` and `dialect` ride down from [`apply`] rather than being resolved
    /// here: the encryption stage wants a key store, not a backend, the binary
    /// stages want a dialect, not a connection, and this struct makes no
    /// routing decision it could take either from.
    async fn apply_to_doc(
        &self,
        keys: &crate::encryption::KeyStore,
        dialect: compile::SqlDialect,
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
                keys,
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
        if self.has_storage_encoding {
            zeroship_data_sql::codecs::encode_document(dialect, schema, row)?;
        }
        // AFTER encryption: a `t.encrypted({ wraps: t.bytes() })` column is the
        // encryption pass's, and this pass skips it by construction, but the
        // ordering also means the ciphertext it deposits is never re-read as a
        // plain bytes value.
        if self.has_plain_bytes {
            super::bytes_pass::validate_bytes_on_write(schema, row)?;
        }
        // LAST. Every stage above reads and writes a masked field under its
        // LOGICAL key and knows nothing about the flip; this one moves the
        // finished value to the raw column and puts the mask in the logical
        // slot. Exactly one stage owns physical placement, and it is the one
        // that runs after all the producers.
        super::mask_pass::relocate_masked_columns(&masks, row)?;
        Ok(())
    }

    /// `keys` and `dialect` ride down from [`apply`], for the reason on
    /// [`Self::apply_to_doc`].
    async fn apply_to_update(
        &self,
        keys: &crate::encryption::KeyStore,
        dialect: compile::SqlDialect,
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
                keys,
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
        if self.has_storage_encoding {
            zeroship_data_sql::codecs::encode_update(dialect, schema, patch)?;
        }
        if self.has_plain_bytes {
            super::bytes_pass::validate_bytes_on_update(schema, patch)?;
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
pub struct TargetRowId {
    pub id_value: Value,
    pub row_pk: String,
}

/// Resolve the ids the pending write will touch.
///
/// Takes the dispatch's [`TxRoute`] rather than an `app_id`: this read
/// MUST see the rows the same transaction is about to update, so it has
/// to run on the same connection the update will. Reading it off the pool
/// while the update ran in a transaction would resolve pre-transaction
/// ids.
///
/// `schema` is the caller's already-resolved descriptor entry. The probe still
/// selects only `id`; the declared fields are carried solely so its filter can
/// lower SQLite booleans and numeric timestamp binds by field type.
///
/// `dialect` is the caller's too, and passed in even though this function holds
/// a [`TxRoute`] it could read one off - the same shape, and the same reason, as
/// `keys` on [`rewrite_upsert_doc_id_to_existing_row_id`]. The probe and the
/// UPDATE it precedes must be written in ONE dialect, and that is the one the
/// caller resolved for the whole operation, not a second derivation here.
pub async fn resolve_target_row_ids(
    route: &TxRoute,
    dialect: compile::SqlDialect,
    collection: &str,
    filter: &Value,
    limit: i64,
    schema: &Value,
) -> Result<Vec<TargetRowId>, DbError> {
    note_target_row_resolution_for_tests();
    let mut sql_filter = filter.clone();
    zeroship_data_sql::codecs::lower_filter(dialect, schema, &mut sql_filter);
    let built = compile::build_write_target_probe(
        route.schema(),
        collection,
        schema,
        &sql_filter,
        limit,
        dialect,
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
pub fn update_requires_per_row_encryption(schema: &Value, patch: &Value) -> bool {
    update_touches_randomised_encrypted_field(schema, patch)
}

/// The upsert twin of [`update_requires_per_row_encryption`]: a doc that writes
/// a randomised-encrypted column needs the deterministic conflict probe run
/// first, because its ciphertext cannot be compared for ON CONFLICT equality.
pub fn upsert_requires_conflict_probe(schema: &Value, doc: &Value) -> bool {
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
        if field.starts_with('$') {
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
        if value.is_null() {
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

/// `keys` is passed in even though this function holds a [`TxRoute`] it could
/// take a handle off. The route is here to run the probe SELECT on the right
/// connection; the deterministic-encryption step below needs a key and nothing
/// else, and taking it from [`apply`]'s parameter keeps one key store per write
/// op rather than two independent derivations of it.
///
/// `dialect` is passed in for the same reason: the probe's SQL text has to be
/// the dialect the enclosing upsert was planned in, and taking it from
/// [`apply`]'s parameter keeps one dialect per write op rather than re-asking.
async fn rewrite_upsert_doc_id_to_existing_row_id(
    keys: &crate::encryption::KeyStore,
    dialect: compile::SqlDialect,
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

    let mut filter_obj = zeroship_data_sql::value::Map::new();
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
            keys,
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
    zeroship_data_sql::codecs::lower_filter(dialect, schema, &mut filter);
    let built = compile::build_conflict_probe_with_dialect(
        route.schema(),
        collection,
        schema,
        &filter,
        dialect,
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
    let mut out = zeroship_data_sql::value::Map::new();
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
    /// The direct half of the id fence. The prefix validator closed the
    /// DESCRIPTOR vector; this closes the one an attacker reaches without
    /// touching a generated file, by sending `id` on an ordinary insert.
    #[test]
    fn a_supplied_id_is_refused_at_the_document_boundary() {
        let doc = zeroship_data_sql::value!({ "title": "hi", "id": "usr_034HQyaJ0C11GCzHMMrWwz" });
        match super::refuse_platform_assigned_id(&doc) {
            Err(zeroship_data_orm::error::DbError::ValidationFailed { code, .. }) => {
                assert_eq!(code, "platform_assigned_field");
            }
            other => panic!("expected the supplied id to be refused, got {other:?}"),
        }
    }

    /// The control. A fence that refused every document would satisfy the test
    /// above, so prove an ordinary document still passes.
    #[test]
    fn a_document_without_an_id_passes_the_boundary() {
        let doc = zeroship_data_sql::value!({ "title": "hi" });
        super::refuse_platform_assigned_id(&doc)
            .expect("a document that supplies no id must be accepted");
    }

    /// A non-object payload must not panic or refuse - the shape checks belong
    /// to the validators beside this one.
    #[test]
    fn a_non_object_payload_is_not_this_fence_s_business() {
        let doc = zeroship_data_sql::value!("not a document");
        super::refuse_platform_assigned_id(&doc)
            .expect("a non-object payload is another validator's concern");
    }

    use std::path::PathBuf;
    use std::rc::Rc;

    use zeroship_data_sql::value::Value;

    use crate::tx_route::TxRoute;
    use zeroship_data_orm::binding::DbBinding;

    use super::{
        ApplyMode, apply, inspect_update, validate_update_patch_keys, validate_user_doc_keys,
    };
    use crate::backend::sqlite::SqliteBackend;

    #[test]
    fn db8_rejects_reserved_and_malformed_user_doc_keys() {
        use zeroship_data_sql::value;
        // A normal document passes.
        assert!(validate_user_doc_keys(&value!({ "name": "a", "ssn": "x" })).is_ok());
        // The user must not forge the masked sibling suffix the platform emits.
        assert!(validate_user_doc_keys(&value!({ "ssn_masked": "x" })).is_err());
        // Nor a platform-internal `_`-prefixed name (covers `__zsbin__` markers,
        // `__zs_`, synthetic `_rank`/`_score`).
        assert!(validate_user_doc_keys(&value!({ "__zsbin__ssn": true })).is_err());
        assert!(validate_user_doc_keys(&value!({ "_rank": 1 })).is_err());
        // Null-byte and >63-byte keys (NAMEDATALEN truncation collision).
        assert!(validate_user_doc_keys(&value!({ "a\u{0}b": 1 })).is_err());
        let long = "x".repeat(64);
        assert!(validate_user_doc_keys(&value!({ long: 1 })).is_err());
    }

    #[test]
    fn db8_update_patch_validates_field_keys_not_operators() {
        use zeroship_data_sql::value;
        // Plain field keys + a field-scoped operator value pass.
        assert!(
            validate_update_patch_keys(&value!({ "name": "a", "views": { "$inc": 1 } })).is_ok()
        );
        // $set's nested field keys are validated; the operator key itself is skipped.
        assert!(validate_update_patch_keys(&value!({ "$set": { "name": "a" } })).is_ok());
        assert!(validate_update_patch_keys(&value!({ "$set": { "ssn_masked": "x" } })).is_err());
        // A top-level reserved field key is rejected.
        assert!(validate_update_patch_keys(&value!({ "ssn_masked": "x" })).is_err());
    }

    #[test]
    fn write_pipeline_refuses_every_reserved_descriptor_id_prefix() {
        run(async {
            let collection = "people";
            assert!(
                !crate::compile::RESERVED_ID_PREFIXES.is_empty(),
                "the reserved-prefix fence must rule on at least one platform prefix"
            );
            for (index, &prefix) in crate::compile::RESERVED_ID_PREFIXES.iter().enumerate() {
                let app_id = format!("app_reserved_descriptor_id_prefix_{index}");
                let binding = DbBinding::cold_start(&app_id);
                crate::cache_schema_for_tests(
                    &app_id,
                    collection,
                    zeroship_data_sql::value!({
                        "id": { "type": "id", "idPrefix": prefix },
                        "name": { "type": "string" }
                    }),
                );
                let mut doc = zeroship_data_sql::value!({ "name": "Alice" });

                // The dialect is unobservable in this case and stated rather
                // than defaulted: the fixture schema declares no encrypted,
                // masked or binary column, so `WriteStages::any()` is false and
                // no dialect-sensitive stage runs before the refusal.
                let (_dir, route) = empty_backend_route(&app_id);
                let result = apply(
                    &test_key_store(),
                    SqlDialect::Postgres,
                    &route,
                    &binding,
                    collection,
                    &mut doc,
                    ApplyMode::Insert { actor_id: None },
                )
                .await;

                match result {
                    Err(zeroship_data_orm::error::DbError::ValidationFailed { code, .. }) => {
                        assert_eq!(code, "reserved_system_field_name");
                    }
                    other => {
                        panic!("expected descriptor prefix '{prefix}' to be refused, got {other:?}")
                    }
                }
                assert!(
                    doc.get("id").is_none(),
                    "prefix '{prefix}' minted an id before refusal: {doc}"
                );
            }
        });
    }

    #[test]
    fn write_pipeline_accepts_ordinary_descriptor_id_prefix() {
        run(async {
            let app_id = "app_ordinary_descriptor_id_prefix";
            let collection = "people";
            let binding = DbBinding::cold_start(app_id);
            crate::cache_schema_for_tests(
                app_id,
                collection,
                zeroship_data_sql::value!({
                    "id": { "type": "id", "idPrefix": "blog" },
                    "name": { "type": "string" }
                }),
            );
            let mut doc = zeroship_data_sql::value!({ "name": "Alice" });

            let (_dir, route) = empty_backend_route(app_id);
            apply(
                &test_key_store(),
                // Unobservable here for the reason given in the sibling case.
                SqlDialect::Postgres,
                &route,
                &binding,
                collection,
                &mut doc,
                ApplyMode::Insert { actor_id: None },
            )
            .await
            .expect("ordinary descriptor id prefix must be accepted");

            let id = doc
                .get("id")
                .and_then(Value::as_str)
                .expect("accepted descriptor prefix must mint an id");
            assert!(id.starts_with("blog_"), "minted id was {id}");
        });
    }

    use crate::cache_schema_for_tests;
    use crate::compile::{SqlDialect, build_insert_with_dialect};
    use crate::encryption;
    use zeroship_data_orm::fixtures::DatabaseFixture;
    use zeroship_migrate::schema::query::FkEmission;
    fn sqlite_fixture_sql(
        schema: &zeroship_data_sql::SchemaName,
        table: &str,
        fields: &Value,
        fks: &FkEmission<'_>,
        dialect: SqlDialect,
    ) -> Result<String, zeroship_migrate::schema::query::QueryError> {
        assert_eq!(dialect, SqlDialect::Sqlite);
        let policy =
            zeroship_migrate_server::policy::ManagedPolicyConfig::default_confined([7u8; 32], 1)
                .unwrap()
                .current_ceiling_for_app(&uuid::Uuid::nil(), None)
                .unwrap()
                .policy;
        zeroship_migrate::schema::query::build_create_table_with_fks_for_dialect(
            zeroship_migrate::shipping_vendors(),
            schema.as_str(),
            table,
            &serde_json::to_value(fields).expect("migration metadata"),
            fks,
            &zeroship_migrate_sqlite::DIALECT,
            &policy,
        )
    }

    fn run<F: std::future::Future>(f: F) -> F::Output {
        compio::runtime::Runtime::new()
            .expect("compio runtime build")
            .block_on(f)
    }

    /// A key store for the cases that stand no backend up at all.
    ///
    /// `apply` takes the store as a parameter now, so a test that refuses
    /// before the encryption stage still has to name one. It is the real type
    /// resolving from the real source - not a stub - and a test whose schema
    /// DOES declare an encrypted column would exercise it. The cases below use
    /// it only on schemas that declare none.
    ///
    /// The source is named directly rather than read out of the adapter's
    /// per-isolate context: `env_var()` is what that read returns when no
    /// fixture has supplied roots, which is the state every case here is in.
    fn test_key_store() -> encryption::KeyStore {
        encryption::KeyStore::new(encryption::LocalKeySource::env_var())
    }

    /// A real SQLite backend over a fresh directory, and the route bound to it.
    ///
    /// `apply` reads the LIVE catalog before any stage runs (the
    /// protection-floor fence), so a case that stands up no backend at all can
    /// no longer call it. That is the fence working rather than an inconvenience:
    /// a write path that cannot reach the database cannot be told whether the
    /// descriptor dropped a protection, and guessing "no" is the defect.
    ///
    /// The returned `TempDir` must be kept alive - dropping it removes the app
    /// file out from under the backend.
    fn empty_backend_route(app_id: &str) -> (tempfile::TempDir, TxRoute) {
        let dir = tempfile::tempdir().expect("tempdir");
        let backend = Rc::new(
            crate::backend_selection::new_sqlite_backend(
                PathBuf::from(dir.path()),
                encryption::LocalKeySource::env_var(),
            )
            .expect("open sqlite backend"),
        );
        let handle = crate::backend::BackendHandle::new(backend);
        (dir, crate::exec::ambient_route_for_tests(app_id, handle))
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

        let raw_col = crate::compile::raw_column_name("ssn");
        let ciphertext = row
            .get(&raw_col)
            .and_then(Value::as_bytes)
            .expect("native ciphertext");
        assert_ne!(ciphertext, expected_plaintext.as_bytes());
        assert!(row.get(format!("__zsbin__{raw_col}").as_str()).is_none());

        let key = backend
            .key_store()
            .resolve(app_id, key_id)
            .await
            .expect("resolve key");
        let aad = encryption::canonical_aad(collection, "ssn", Some(id.as_bytes()));
        let plaintext = crate::encryption::aead::decrypt(&key, ciphertext, &aad)
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
            // Hand the root key to the BACKEND rather than to the process
            // environment. `new_sqlite_backend` below takes this source, so the
            // encrypt/decrypt legs still run through the real
            // `KeyStore::resolve` + HKDF derivation.
            //
            // It used to go through the adapter's `supply_root_keys_for_tests`,
            // which parked it in the per-isolate context for the constructor to
            // read back. Passing it in is the same correction the constructor's
            // own signature took: the owner of the roots does the handing over.
            let supplied = std::rc::Rc::new(encryption::SuppliedRootKeys::new());
            supplied
                .insert_hex(key_id, &"1".repeat(64))
                .expect("fixture root key must parse");
            let key_source = encryption::LocalKeySource::supplied(supplied);
            let app_id = "app_write_pipeline";
            let binding = DbBinding::cold_start(app_id);
            let collection = "users";
            let schema = zeroship_data_sql::value!({
                "email": { "type": "string", "required": true, "unique": true },
                "name": { "type": "string", "required": true },
                "ssn": {
                    "type": "string",
                    "encrypted": { "mode": "randomised", "keyId": key_id, "wraps": "string" },
                    "mask": { "kind": "last4", "classification": "spi" }
                }
            });
            let ddl_schema = zeroship_data_sql::value!({
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
                crate::backend_selection::new_sqlite_backend(PathBuf::from(dir.path()), key_source)
                    .expect("open sqlite backend"),
            );
            backend
                .attach_app_file(app_id)
                .await
                .expect("ensure schema");
            let handle = crate::backend::BackendHandle::new(Rc::clone(&backend));
            // One route for every stage below. `apply` reads the live catalog
            // through it before any stage runs, so the four write shapes below
            // all exercise the protection-floor fence against a table whose
            // sentinels the DDL further down really wrote - the happy arm, where
            // descriptor and catalog agree.
            let route = crate::exec::ambient_route_for_tests(app_id, handle.clone());
            cache_schema_for_tests(app_id, collection, schema);

            let ddl = sqlite_fixture_sql(
                &zeroship_data_sql::SchemaName::new(app_id).expect("fixture schema name"),
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
                backend
                    .execute_fixture(trimmed, &[])
                    .await
                    .expect("DDL exec");
            }

            let mut insert_doc = zeroship_data_sql::value!({
                "email": "seed@example.com",
                "name": "Seed",
                "ssn": "123-45-6789"
            });
            // SQLite, stated explicitly: the fixture stands up a real
            // `SqliteBackend`, and this is the dialect a captured route would
            // have stamped for it. Passing it in is what lets the case run with
            // no isolate to capture from.
            apply(
                backend.key_store(),
                SqlDialect::Sqlite,
                &route,
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
                &zeroship_data_sql::SchemaName::new(app_id).expect("fixture schema name"),
                collection,
                &ddl_schema,
                &insert_doc,
                SqlDialect::Sqlite,
            )
            .expect("build insert");
            let insert_params = &insert_built.params;
            let client = backend
                .fixture_session(app_id)
                .await
                .expect("acquire client");
            client
                .query_typed(&insert_built.sql, insert_params)
                .await
                .expect("seed insert");
            let seeded_id = insert_doc
                .get("id")
                .and_then(Value::as_str)
                .expect("seeded row id")
                .to_string();

            let mut bulk_docs = zeroship_data_sql::value!([
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
                backend.key_store(),
                SqlDialect::Sqlite,
                &route,
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

            let mut update_patch = zeroship_data_sql::value!({
                "$set": {
                    "ssn": "555-55-5555",
                    "updated_by": "usr_override"
                }
            });
            inspect_update(app_id, collection, &mut update_patch).expect("inspect update patch");
            assert_eq!(
                update_patch
                    .get("$set")
                    .and_then(Value::as_object)
                    .map(|set| set.contains_key("updated_by")),
                Some(false),
                "the update pre-pass must strip a supplied updated_by from the nested $set, \
                 so the builder's own actor bump is the only assignment to it",
            );
            apply(
                backend.key_store(),
                SqlDialect::Sqlite,
                &route,
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
                update_target.get("ssn").and_then(Value::as_str),
                Some(last4_mask("555-55-5555").as_str()),
                "the relocation stage must target the same $set object the \
                 encryption pass wrote to",
            );
            let update_ciphertext = update_target
                .get(crate::compile::raw_column_name("ssn").as_str())
                .and_then(Value::as_bytes)
                .expect("update ssn ciphertext in the raw column");
            let update_key = backend
                .key_store()
                .resolve(app_id, key_id)
                .await
                .expect("resolve update key");
            let update_plaintext = crate::encryption::aead::decrypt(
                &update_key,
                update_ciphertext,
                &encryption::canonical_aad(collection, "ssn", Some(seeded_id.as_bytes())),
            )
            .expect("decrypt update ciphertext");
            assert_eq!(
                update_plaintext, b"555-55-5555",
                "update pipeline must encrypt against the filter row id",
            );

            let conflict_fields = zeroship_data_sql::value!(["email"]);
            let mut upsert_doc = zeroship_data_sql::value!({
                "email": "seed@example.com",
                "name": "Seed Updated",
                "ssn": "222-33-4444"
            });
            apply(
                backend.key_store(),
                // The conflict probe below is the ONE pre-pass that issues SQL
                // of its own, so this is the arm where the dialect is load
                // bearing. `apply` takes it as its own parameter rather than
                // reading the route's, which is why this line exists at all;
                // the two now agree either way, because
                // `ambient_route_for_tests` derives the route's dialect from
                // the SQLite handle below instead of stamping `Postgres` on it
                // (it did until 2026-09-03, and this comment called that inert).
                SqlDialect::Sqlite,
                // No isolate in a unit test: this path is exercised outside any
                // transaction, which is what the pool route means.
                &route,
                &binding,
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

    /// **The protection floor holds on SQLite too, and this is not implied by
    /// the PostgreSQL gate.**
    ///
    /// The two backends recover the mask sentinel by different code: PostgreSQL
    /// reads `pg_description` in `pg_introspect`, SQLite regexes
    /// `sqlite_master.sql` in `parse_mask_sentinels`. Either could stop
    /// populating `ColumnInfo::mask` on its own, and the fence would then wave
    /// the downgrade through on that backend while `plugin-db/tests/mask_flip.rs`
    /// stayed green on the other. This is the SQLite half.
    ///
    /// It also binds the DEV TIER specifically: `pnpm dev` runs SQLite, so a
    /// creator's first encounter with a dropped `mask` key happens here.
    #[test]
    fn a_sqlite_write_is_refused_when_the_descriptor_drops_a_mask_the_file_still_records() {
        run(async {
            let app_id = "app_sqlite_protection_floor";
            let binding = DbBinding::cold_start(app_id);
            let collection = "people";
            let masked = zeroship_data_sql::value!({
                "ssn": { "type": "string", "mask": { "kind": "last4", "classification": "spi" } },
                // The control: same type, no mask. Every refusal below has to be
                // about `ssn` and not about the collection being unwritable.
                "nickname": { "type": "string" },
            });

            let dir = tempfile::tempdir().expect("tempdir");
            let backend = Rc::new(
                crate::backend_selection::new_sqlite_backend(
                    PathBuf::from(dir.path()),
                    encryption::LocalKeySource::env_var(),
                )
                .expect("open sqlite backend"),
            );
            backend.attach_app_file(app_id).await.expect("attach app");
            let handle = crate::backend::BackendHandle::new(Rc::clone(&backend));
            let route = crate::exec::ambient_route_for_tests(app_id, handle);

            // `zeroship-data-sql`'s DDL emitter, so the `zero-migrate:mask:`
            // sentinel and the `__zs_raw__ssn` sibling are built rather than
            // spelled out here.
            //
            // It is NOT the emitter production runs - that is the migration
            // engine's, and this one has no `src` call site anywhere. The two
            // agreed about the raw column's name and disagreed about the
            // sentinel's spelling until 2026-09-04, and this test was green
            // throughout, which is exactly what a fixture sharing an emitter
            // with its reader can be. The oracle that rules on the pair is
            // `zeroship-data-v8`'s `mask_flip.rs`, which builds with the
            // ENGINE's emitter; this case still earns its place as the SQLITE
            // arm of the fence, which that live-PostgreSQL suite cannot reach.
            let ddl = sqlite_fixture_sql(
                &zeroship_data_sql::SchemaName::new(app_id).expect("fixture schema name"),
                collection,
                &masked,
                &FkEmission::Inline,
                SqlDialect::Sqlite,
            )
            .expect("build DDL");
            for stmt in ddl.split(";\n") {
                let trimmed = stmt.trim();
                if trimmed.is_empty() {
                    continue;
                }
                backend
                    .execute_fixture(trimmed, &[])
                    .await
                    .expect("DDL exec");
            }

            // Control: under the mask-declaring descriptor the write prepares,
            // and the mask lands in the field's own column.
            cache_schema_for_tests(app_id, collection, masked);
            let mut ok_doc =
                zeroship_data_sql::value!({ "ssn": "123-45-6789", "nickname": "alice" });
            apply(
                backend.key_store(),
                SqlDialect::Sqlite,
                &route,
                &binding,
                collection,
                &mut ok_doc,
                ApplyMode::Insert { actor_id: None },
            )
            .await
            .expect("the declaring descriptor must still write");
            assert_eq!(
                ok_doc.get("ssn").and_then(Value::as_str),
                Some(last4_mask("123-45-6789").as_str()),
                "control: the mask belongs in the field's own column: {ok_doc}",
            );

            // The one-key deletion, against the same file.
            cache_schema_for_tests(
                app_id,
                collection,
                zeroship_data_sql::value!({
                    "ssn": { "type": "string" },
                    "nickname": { "type": "string" },
                }),
            );
            let mut doc = zeroship_data_sql::value!({ "ssn": "987-65-4321", "nickname": "bob" });
            let err = apply(
                backend.key_store(),
                SqlDialect::Sqlite,
                &route,
                &binding,
                collection,
                &mut doc,
                ApplyMode::Insert { actor_id: None },
            )
            .await
            .expect_err("a descriptor that dropped the mask must not write the plaintext");
            let rendered = format!("{err:?}");
            assert!(
                rendered.contains("protection_removed_from_descriptor") && rendered.contains("ssn"),
                "the refusal must carry the typed code and name the column, got {rendered}",
            );
            assert_eq!(
                doc.get("ssn").and_then(Value::as_str),
                Some("987-65-4321"),
                "the refusal happens BEFORE any stage touches the document, so \
                 the caller's value is handed back untransformed: {doc}",
            );
        });
    }
}
