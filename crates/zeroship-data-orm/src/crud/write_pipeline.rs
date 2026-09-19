use crate::schema::{ColumnSchema, FieldMap};
use crate::value::Value;

use crate::exec::exec_query;
use crate::sql::mapping;
use crate::tx_route::TxRoute;
use zeroship_data_orm::binding::DbBinding;
use zeroship_data_orm::error::DbError;

#[cfg(test)]
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
    /// Resolve the existing row identity before encrypting an upsert.
    Upsert {
        actor_id: Option<&'a str>,
        conflict_fields: &'a Value,
    },
}

/// Normalize the update and enforce descriptor-declared assignments. Insert-fixed
/// fields are refused; write-assigned fields are removed for the SQL builder.
pub fn inspect_update(schema: &FieldMap, patch: &mut Value) -> Result<(), DbError> {
    crate::sql::update::normalize(patch)?;
    super::assignment_pass::apply_assignments_on_update(patch, schema)
}

/// DB-8: validate every top-level field key of a plain write document
/// (insert / insertMany element / upsert) with the same `validate_field_name`
/// fence the read/filter path enforces. Runs on the raw user document before
/// protection and assignment passes add reserved storage keys.
fn validate_user_doc_keys(doc: &Value, schema: &FieldMap) -> Result<(), DbError> {
    if let Some(obj) = doc.as_object() {
        for key in obj.keys() {
            mapping::validate_field_name(key)?;
            if schema.get(key).is_none() {
                return Err(DbError::validation(
                    "unknown_field",
                    format!("field '{key}' is not declared"),
                ));
            }
        }
    }
    Ok(())
}

/// Reject a caller-supplied generated identifier before the assignment pass.
fn refuse_generated_identifier(doc: &Value, schema: &FieldMap) -> Result<(), DbError> {
    let Some(obj) = doc.as_object() else {
        return Ok(());
    };
    if crate::assignments::AssignmentPlan::from_schema(schema)
        .columns()
        .iter()
        .any(|column| {
            matches!(
                column.by,
                zeroship_migrate_policy::AssignmentGenerator::TypedId
                    | zeroship_migrate_policy::AssignmentGenerator::Identity
            ) && obj.contains_key(&column.name)
        })
    {
        return Err(DbError::validation(
            "platform_assigned_field",
            "the identifier is generated; remove it from the document",
        ));
    }
    Ok(())
}

fn validate_upsert_conflict_fields(
    schema: &FieldMap,
    doc: &Value,
    conflict_fields: &Value,
) -> Result<(), DbError> {
    let fields = mapping::parse_conflict_fields(conflict_fields)?;
    let assignments = crate::assignments::AssignmentPlan::from_schema(schema);
    let protected = upsert_requires_conflict_probe(schema, doc);
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
        if schema.get(field).is_some_and(field_is_encrypted) {
            return Err(DbError::validation(
                "encrypted_conflict_field",
                format!("encrypted field '{field}' cannot be a conflict target"),
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
        if protected && doc.get(field).is_some_and(Value::is_null) {
            return Err(DbError::validation(
                "protected_upsert_nullable_conflict",
                "a protected upsert cannot use a null conflict value",
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
                    mapping::validate_field_name(nested_key)?;
                }
            }
        } else {
            mapping::validate_field_name(key)?;
        }
    }
    Ok(())
}

/// Validate and protect a write using its runtime descriptor. Catalog markers
/// prevent protection downgrades; row identity is established before encryption.
/// Mask inputs survive encryption in a sidechannel, and the statement builder
/// performs storage encoding after physical columns are resolved.
pub async fn apply(
    keys: &crate::encryption::KeyStore,
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
    let schema = crate::descriptor::collection_schema(binding, collection)?;
    // Validate creator keys before protection and assignment passes add storage keys.
    match &mode {
        ApplyMode::Insert { .. } | ApplyMode::Upsert { .. } => {
            validate_user_doc_keys(payload, &schema)?;
            refuse_generated_identifier(payload, &schema)?;
        }
        ApplyMode::InsertMany { .. } => {
            if let Some(docs) = payload.as_array() {
                for doc in docs {
                    validate_user_doc_keys(doc, &schema)?;
                    refuse_generated_identifier(doc, &schema)?;
                }
            }
        }
        ApplyMode::Update { .. } => validate_update_patch_keys(payload)?,
    }

    if let ApplyMode::Upsert {
        conflict_fields, ..
    } = &mode
    {
        validate_upsert_conflict_fields(&schema, payload, conflict_fields)?;
    }
    match &mode {
        ApplyMode::Insert { .. } | ApplyMode::Upsert { .. } => {
            crate::sql::codecs::prepare_document(&schema, payload)?;
        }
        ApplyMode::InsertMany { .. } => {
            if let Some(documents) = payload.as_array_mut() {
                for document in documents {
                    crate::sql::codecs::prepare_document(&schema, document)?;
                }
            }
        }
        // Updates are prepared before encrypted target lookup, even when no
        // row matches. Per-row protection must not repeat that traversal.
        ApplyMode::Update { .. } => {}
    }
    // Reject descriptors that remove protection recorded in the live catalog.
    super::protection_floor::refuse_protection_downgrade(route, binding, collection, &schema)
        .await?;
    let stages = WriteStages::new(&schema);

    match mode {
        ApplyMode::Insert { actor_id } => {
            super::assignment_pass::apply_assignments_on_insert(
                payload, &schema, collection, actor_id,
            )?;
            if super::identity::requires_allocation(&schema, payload) {
                let request = super::identity::request(
                    route.schema(),
                    collection,
                    &schema,
                    1,
                    route.sql_registration(),
                )?;
                payload["id"] = crate::backend::identity::reserve(route, request)
                    .await?
                    .allocate()
                    .await?
                    .remove(0);
            }
            let row_pk = row_pk_from_doc(payload);
            stages
                .apply_to_doc(keys, app_id, collection, &row_pk, payload)
                .await?;
            Ok(())
        }
        ApplyMode::InsertMany { actor_id } => {
            super::assignment_pass::apply_assignments_on_insert_many(
                payload, &schema, collection, actor_id,
            )?;
            let identities = if super::identity::requires_allocation(&schema, payload) {
                let request = super::identity::request(
                    route.schema(),
                    collection,
                    &schema,
                    payload.as_array().expect("batch").len(),
                    route.sql_registration(),
                )?;
                Some(
                    crate::backend::identity::reserve(route, request)
                        .await?
                        .allocate()
                        .await?,
                )
            } else {
                None
            };
            let Some(docs) = payload.as_array_mut() else {
                return Ok(());
            };
            if let Some(identities) = identities {
                for (doc, id) in docs.iter_mut().zip(identities) {
                    doc["id"] = id;
                }
            }
            for doc in docs.iter_mut() {
                let row_pk = row_pk_from_doc(doc);
                stages
                    .apply_to_doc(keys, app_id, collection, &row_pk, doc)
                    .await?;
            }
            Ok(())
        }
        ApplyMode::Update { row_pk } => {
            stages
                .apply_to_update(keys, app_id, collection, row_pk, payload)
                .await?;
            Ok(())
        }
        ApplyMode::Upsert {
            actor_id,
            conflict_fields,
        } => {
            super::assignment_pass::apply_assignments_on_insert(
                payload, &schema, collection, actor_id,
            )?;
            let allocate_identity = super::identity::requires_allocation(&schema, payload);
            let allocation = if allocate_identity {
                let request = super::identity::request(
                    route.schema(),
                    collection,
                    &schema,
                    1,
                    route.sql_registration(),
                )?;
                Some(crate::backend::identity::reserve(route, request).await?)
            } else {
                None
            };
            rewrite_upsert_doc_id_to_existing_row_id(
                payload,
                route,
                collection,
                conflict_fields,
                &schema,
            )
            .await?;
            if allocate_identity && payload.get("id").is_none_or(Value::is_null) {
                payload["id"] = allocation
                    .expect("generated identity reservation")
                    .allocate()
                    .await?
                    .remove(0);
            }
            let row_pk = row_pk_from_doc(payload);
            stages
                .apply_to_doc(keys, app_id, collection, &row_pk, payload)
                .await?;
            Ok(())
        }
    }
}

struct WriteStages<'a> {
    schema: &'a FieldMap,
    has_encrypted: bool,
    has_masked: bool,
    has_plain_bytes: bool,
}

impl<'a> WriteStages<'a> {
    fn new(schema: &'a FieldMap) -> Self {
        Self {
            has_encrypted: super::schema_has_encrypted_columns(schema),
            has_masked: super::schema_has_masked_columns(schema),
            has_plain_bytes: super::bytes_pass::schema_has_plain_bytes_columns(schema),
            schema,
        }
    }

    /// Does any stage below have work to do for this collection? A schema with
    /// none of these facets skips the whole pipeline.
    fn any(&self) -> bool {
        self.has_encrypted || self.has_masked || self.has_plain_bytes
    }

    async fn apply_to_doc(
        &self,
        keys: &crate::encryption::KeyStore,
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
        // Validate plain bytes after encryption; encrypted fields already hold ciphertext.
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

    async fn apply_to_update(
        &self,
        keys: &crate::encryption::KeyStore,
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

fn compile_target_probe(
    namespace: &crate::sql::SchemaName,
    collection: &str,
    schema: &FieldMap,
    filter: super::predicate::Input,
    limit: i64,
    registration: &crate::sql::registration::SqlRegistration,
) -> Result<crate::sql::compiler::CompiledQuery, crate::sql::mapping::QueryError> {
    use crate::sql::statement::{
        ResolvedOperand, ResolvedPredicate, RowLock, SelectParts, SelectStatement,
        SelectedExpression, Statement,
    };

    if limit <= 0 {
        return Err(crate::sql::mapping::QueryError::InvalidFilter(
            "target probe limit must be positive".into(),
        ));
    }
    let resolved = super::resolved::ResolvedTable::aliased(
        namespace,
        collection,
        "target",
        schema,
        registration,
    )?;
    let identity = resolved.inputs.get("id").ok_or_else(|| {
        crate::sql::mapping::QueryError::InvalidFilter("descriptor requires an id field".into())
    })?;
    let identity = resolved.table.column(&identity.column)?;
    let predicate = filter.resolve(schema, &resolved, registration)?;
    let statement = SelectStatement::new(SelectParts {
        table: resolved.table,
        joins: Vec::new(),
        projection: vec![SelectedExpression {
            expression: ResolvedOperand::Column(identity),
            alias: crate::sql::Ident::parse_as("id", crate::sql::IdentRole::Alias)
                .map_err(crate::sql::compiler::CompileError::from)?,
        }],
        predicate,
        group_by: Vec::new(),
        having: ResolvedPredicate::Const(true),
        order_by: Vec::new(),
        limit: Some(limit),
        offset: None,
        distinct: false,
        lock: RowLock::WriteTargets,
    })?;
    registration
        .compile(Statement::select(statement))
        .map_err(Into::into)
}

/// Resolve and lock the identities a protected write will touch. The captured
/// route keeps this read on the same transaction connection as the write.
pub async fn resolve_target_row_ids(
    route: &TxRoute,
    collection: &str,
    filter: super::predicate::Input,
    limit: i64,
    schema: &FieldMap,
) -> Result<Vec<TargetRowId>, DbError> {
    note_target_row_resolution_for_tests();
    let built = compile_target_probe(
        route.schema(),
        collection,
        schema,
        filter,
        limit,
        route.sql_registration(),
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
pub fn update_requires_per_row_encryption(schema: &FieldMap, patch: &Value) -> bool {
    update_touches_encrypted_field(schema, patch)
}

/// The upsert twin of [`update_requires_per_row_encryption`]: a doc that writes
/// a randomised-encrypted column needs the conflict probe run
/// first, because its ciphertext cannot be compared for ON CONFLICT equality.
pub fn upsert_requires_conflict_probe(schema: &FieldMap, doc: &Value) -> bool {
    doc_touches_encrypted_field(schema, doc)
}

fn update_touches_encrypted_field(schema: &FieldMap, patch: &Value) -> bool {
    let schema_obj = schema;
    let Some(update_obj) = patch.as_object() else {
        return false;
    };

    if let Some(set_obj) = update_obj.get("$set").and_then(Value::as_object) {
        if set_obj
            .iter()
            .any(|(field, _)| schema_obj.get(field).is_some_and(field_is_encrypted))
        {
            return true;
        }
    }

    update_obj.iter().any(|(field, value)| {
        if field.starts_with('$') {
            return false;
        }
        schema_obj.get(field).is_some_and(field_is_encrypted) && field_update_writes_value(value)
    })
}

fn doc_touches_encrypted_field(schema: &FieldMap, doc: &Value) -> bool {
    let schema_obj = schema;
    let Some(doc_obj) = doc.as_object() else {
        return false;
    };

    doc_obj.iter().any(|(field, value)| {
        if value.is_null() {
            return false;
        }
        schema_obj.get(field).is_some_and(field_is_encrypted)
    })
}

fn field_is_encrypted(field_def: &ColumnSchema) -> bool {
    crate::sql::descriptors::is_encrypted(field_def)
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

/// Resolve a candidate identity before encryption. The upsert's SQL guard
/// handles conflicts that become visible after this probe.
async fn rewrite_upsert_doc_id_to_existing_row_id(
    doc: &mut Value,
    route: &TxRoute,
    collection: &str,
    conflict_fields: &Value,
    schema: &FieldMap,
) -> Result<(), DbError> {
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

    let mut filter_obj = crate::value::Map::new();
    for field in conflict_arr.iter().filter_map(Value::as_str) {
        let Some(value) = obj.get(field).cloned() else {
            return Ok(());
        };
        filter_obj.insert(field.to_string(), value);
    }
    if filter_obj.len() != conflict_arr.len() {
        return Ok(());
    }

    let filter = Value::Object(filter_obj);
    note_upsert_conflict_probe_for_tests();
    let built = compile_target_probe(
        route.schema(),
        collection,
        schema,
        super::predicate::Input::Dynamic(filter),
        1,
        route.sql_registration(),
    )
    .map_err(DbError::from)?;
    let rows = exec_query(route, built).await?;
    let key = "id";
    if let Some(existing_id) = rows
        .first()
        .and_then(|row| row.get(key))
        .filter(|value| !value.is_null())
    {
        obj.insert(key.to_owned(), existing_id.clone());
    }
    Ok(())
}

#[cfg(test)]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WritePathCounters {
    pub target_row_resolution_calls: usize,
    pub upsert_conflict_probe_calls: usize,
    pub target_row_resolution_sql: Vec<String>,
}

#[cfg(test)]
thread_local! {
    static WRITE_PATH_COUNTERS: RefCell<WritePathCounters> =
        RefCell::new(WritePathCounters::default());
}

#[cfg(test)]
#[cfg_attr(test, allow(dead_code))]
pub fn reset_write_path_counters_for_tests() {
    WRITE_PATH_COUNTERS.with(|counters| {
        *counters.borrow_mut() = WritePathCounters::default();
    });
}

#[cfg(test)]
#[cfg_attr(test, allow(dead_code))]
pub fn write_path_counters_for_tests() -> WritePathCounters {
    WRITE_PATH_COUNTERS.with(|counters| counters.borrow().clone())
}

#[cfg(test)]
fn note_target_row_resolution_for_tests() {
    WRITE_PATH_COUNTERS.with(|counters| {
        counters.borrow_mut().target_row_resolution_calls += 1;
    });
}

#[cfg(not(test))]
fn note_target_row_resolution_for_tests() {}

#[cfg(test)]
fn note_target_row_resolution_sql_for_tests(sql: &str) {
    WRITE_PATH_COUNTERS.with(|counters| {
        counters
            .borrow_mut()
            .target_row_resolution_sql
            .push(sql.to_string());
    });
}

#[cfg(not(test))]
fn note_target_row_resolution_sql_for_tests(_sql: &str) {}

#[cfg(test)]
fn note_upsert_conflict_probe_for_tests() {
    WRITE_PATH_COUNTERS.with(|counters| {
        counters.borrow_mut().upsert_conflict_probe_calls += 1;
    });
}

#[cfg(not(test))]
fn note_upsert_conflict_probe_for_tests() {}

#[cfg(test)]
mod tests {
    #[test]
    fn target_probes_compile_dynamic_and_model_predicates_with_backend_locking() {
        use crate::{
            crud::predicate::Input,
            orm::ModelPredicate,
            sql::{predicate::CompareOp, registration::SqlRegistration, SchemaName},
        };

        let namespace = SchemaName::new("app").unwrap();
        let schema = crate::tests::fixtures::native_fields(crate::value!({
            "id": { "type": "string", "primaryKey": true },
            "name": { "type": "string" }
        }));
        let dynamic = Input::Dynamic(crate::value!({ "name": "Ada" }));
        let model = Input::Model(ModelPredicate::Compare {
            field: "name",
            op: CompareOp::Eq,
            value: Value::from("Ada"),
        });

        for (registration, expects_lock) in [
            (SqlRegistration::postgres(), true),
            (SqlRegistration::sqlite(), false),
        ] {
            let dynamic = super::compile_target_probe(
                &namespace,
                "people",
                &schema,
                dynamic.clone(),
                1,
                &registration,
            )
            .unwrap();
            let model = super::compile_target_probe(
                &namespace,
                "people",
                &schema,
                model.clone(),
                1,
                &registration,
            )
            .unwrap();
            assert_eq!(dynamic, model);
            assert!(dynamic
                .sql
                .starts_with("SELECT \"target\".\"id\" AS \"id\" FROM "));
            assert_eq!(dynamic.sql.contains("FOR UPDATE"), expects_lock);
            assert_eq!(dynamic.params, vec![Value::from("Ada"), Value::from(1_i64)]);
        }
    }

    #[test]
    fn target_probes_reject_storage_shaped_boolean_filters_on_every_backend() {
        use crate::{
            crud::predicate::Input,
            sql::{registration::SqlRegistration, SchemaName},
        };

        let namespace = SchemaName::new("app").unwrap();
        let schema = crate::tests::fixtures::native_fields(crate::value!({
            "id": { "type": "string", "primaryKey": true },
            "active": { "type": "boolean" }
        }));

        for registration in [SqlRegistration::postgres(), SqlRegistration::sqlite()] {
            let result = super::compile_target_probe(
                &namespace,
                "people",
                &schema,
                Input::Dynamic(crate::value!({ "active": 1 })),
                1,
                &registration,
            );
            assert!(result.is_err(), "boolean filters require logical booleans");
        }
    }

    /// The direct half of the id fence. The prefix validator closed the
    /// DESCRIPTOR vector; this closes the one an attacker reaches without
    /// touching a generated file, by sending `id` on an ordinary insert.
    #[test]
    fn a_supplied_id_is_refused_at_the_document_boundary() {
        let doc = crate::value!({ "title": "hi", "id": "usr_034HQyaJ0C11GCzHMMrWwz" });
        match super::refuse_generated_identifier(
            &doc,
            &crate::tests::fixtures::generated_schema(crate::value!({})),
        ) {
            Err(zeroship_data_orm::error::DbError::ValidationFailed { code, .. }) => {
                assert_eq!(code, "platform_assigned_field");
            }
            other => panic!("expected the supplied id to be refused, got {other:?}"),
        }
    }

    #[test]
    fn a_supplied_database_identity_is_refused_at_the_document_boundary() {
        let doc = crate::value!({ "id": 42, "title": "hi" });
        let schema = crate::tests::fixtures::native_fields(crate::value!({
            "id": {
                "type": "bigInt",
                "primaryKey": true,
                "required": true,
                "assign": { "by": "identity", "on": "insert" }
            },
            "title": { "type": "string" }
        }));
        match super::refuse_generated_identifier(&doc, &schema) {
            Err(zeroship_data_orm::error::DbError::ValidationFailed { code, .. }) => {
                assert_eq!(code, "platform_assigned_field");
            }
            other => panic!("expected the supplied identity to be refused, got {other:?}"),
        }
    }

    /// The control. A fence that refused every document would satisfy the test
    /// above, so prove an ordinary document still passes.
    #[test]
    fn a_document_without_an_id_passes_the_boundary() {
        let doc = crate::value!({ "title": "hi" });
        super::refuse_generated_identifier(
            &doc,
            &crate::tests::fixtures::generated_schema(crate::value!({})),
        )
        .expect("a document that supplies no id must be accepted");
    }

    /// A non-object payload must not panic or refuse - the shape checks belong
    /// to the validators beside this one.
    #[test]
    fn a_non_object_payload_is_not_this_fence_s_business() {
        let doc = crate::value!("not a document");
        super::refuse_generated_identifier(
            &doc,
            &crate::tests::fixtures::generated_schema(crate::value!({})),
        )
        .expect("a non-object payload is another validator's concern");
    }

    use std::path::PathBuf;
    use std::rc::Rc;

    use crate::value::Value;

    use crate::tx_route::TxRoute;

    use super::{
        apply, inspect_update, validate_update_patch_keys, validate_user_doc_keys, ApplyMode,
    };
    use crate::backend::sqlite::SqliteBackend;

    #[test]
    fn db8_rejects_reserved_and_malformed_user_doc_keys() {
        use crate::value;
        // A normal document passes.
        assert!(validate_user_doc_keys(
            &value!({ "name": "a", "ssn": "x" }),
            &crate::tests::fixtures::native_fields(
                value!({"name":{"type":"string"}, "ssn":{"type":"string"}})
            )
        )
        .is_ok());
        // Nor a platform-internal `_`-prefixed name (covers `__zsbin__` markers,
        // `__zs_`, synthetic `_rank`/`_score`).
        assert!(validate_user_doc_keys(
            &value!({ "__zsbin__ssn": true }),
            &crate::tests::fixtures::native_fields(
                value!({"name":{"type":"string"}, "ssn":{"type":"string"}})
            )
        )
        .is_err());
        assert!(validate_user_doc_keys(
            &value!({ "_rank": 1 }),
            &crate::tests::fixtures::native_fields(
                value!({"name":{"type":"string"}, "ssn":{"type":"string"}})
            )
        )
        .is_err());
        // Null-byte and >63-byte keys (NAMEDATALEN truncation collision).
        assert!(validate_user_doc_keys(
            &value!({ "a\u{0}b": 1 }),
            &crate::tests::fixtures::native_fields(
                value!({"name":{"type":"string"}, "ssn":{"type":"string"}})
            )
        )
        .is_err());
        let long = "x".repeat(64);
        assert!(validate_user_doc_keys(
            &value!({ long: 1 }),
            &crate::tests::fixtures::native_fields(
                value!({"name":{"type":"string"}, "ssn":{"type":"string"}})
            )
        )
        .is_err());
    }

    #[test]
    fn db8_update_patch_validates_field_keys_not_operators() {
        use crate::value;
        // Plain field keys + a field-scoped operator value pass.
        assert!(
            validate_update_patch_keys(&value!({ "name": "a", "views": { "$inc": 1 } })).is_ok()
        );
        // $set's nested field keys are validated; the operator key itself is skipped.
        assert!(validate_update_patch_keys(&value!({ "$set": { "name": "a" } })).is_ok());
        // A top-level reserved field key is rejected.
        assert!(validate_update_patch_keys(&value!({ "_rank": 1 })).is_err());
    }

    #[test]
    fn write_pipeline_refuses_every_reserved_descriptor_id_prefix() {
        run(async {
            let collection = "people";
            assert!(
                !crate::sql::mapping::RESERVED_ID_PREFIXES.is_empty(),
                "the reserved-prefix fence must rule on at least one platform prefix"
            );
            for (index, &prefix) in crate::sql::mapping::RESERVED_ID_PREFIXES.iter().enumerate() {
                let app_id = format!("app_reserved_descriptor_id_prefix_{index}");
                let binding = crate::tests::fixtures::harness_binding(&app_id);
                crate::tests::fixtures::cache_schema(
                    &app_id,
                    collection,
                    crate::value!({
                        "id": { "type": "id", "idPrefix": prefix },
                        "name": { "type": "string" }
                    }),
                );
                let mut doc = crate::value!({ "name": "Alice" });

                let (_dir, route) = empty_backend_route(&app_id);
                let result = apply(
                    &test_key_store(),
                    &route,
                    &binding,
                    collection,
                    &mut doc,
                    ApplyMode::Insert { actor_id: None },
                )
                .await;

                match result {
                    Err(zeroship_data_orm::error::DbError::ValidationFailed { code, .. }) => {
                        assert_eq!(code, "reserved_id_prefix");
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
            let binding = crate::tests::fixtures::harness_binding(app_id);
            crate::tests::fixtures::cache_schema(
                app_id,
                collection,
                crate::value!({
                    "id": { "type": "id", "idPrefix": "blog" },
                    "name": { "type": "string" }
                }),
            );
            let mut doc = crate::value!({ "name": "Alice" });

            let (_dir, route) = empty_backend_route(app_id);
            apply(
                &test_key_store(),
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

    use crate::encryption;
    use crate::sql::registration::SqlRegistration;
    use crate::tests::fixtures::cache_schema;
    use crate::tests::fixtures::DatabaseFixture;
    use zeroship_migrate::schema::query::FkEmission;
    fn sqlite_fixture_sql(
        schema: &crate::sql::SchemaName,
        table: &str,
        fields: &Value,
        fks: &FkEmission<'_>,
    ) -> Result<String, zeroship_migrate::schema::query::QueryError> {
        let policy =
            zeroship_migrate_server::policy::ManagedPolicyConfig::default_confined([7u8; 32], 1)
                .unwrap()
                .current_ceiling_for_app(&zeroship_core::AppId::mint(), None)
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
        encryption::KeyStore::new(encryption::ProjectKeySource::unavailable())
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
                encryption::ProjectKeySource::unavailable(),
            )
            .expect("open sqlite backend"),
        );
        let handle = crate::backend::BackendHandle::new(backend);
        (dir, crate::exec::ambient_route_for_tests(&crate::tests::fixtures::harness_binding(app_id), handle))
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
                "the declared insert actor field should be populated",
            );
            assert_eq!(
                row.get("updated_by").and_then(Value::as_str),
                Some(actor),
                "the declared update actor field should be populated",
            );
        }
        assert_eq!(
            row.get("ssn").and_then(Value::as_str),
            Some(last4_mask(expected_plaintext).as_str()),
            "the field's own column must carry the mask after the relocation stage",
        );

        let raw_col = crate::sql::mapping::raw_column_name("ssn");
        let ciphertext = row
            .get(&raw_col)
            .and_then(Value::as_bytes)
            .expect("native ciphertext");
        assert_ne!(ciphertext, expected_plaintext.as_bytes());
        assert!(row.get(format!("__zsbin__{raw_col}").as_str()).is_none());

        let key = backend
            .key_store()
            .resolve(app_id)
            .await
            .expect("resolve key");
        let aad = encryption::canonical_aad(app_id, collection, "ssn", id.as_bytes());
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
            let project_id = "project_write_pipeline";
            // Bind this fixture app to its supplied project key.
            let supplied = std::sync::Arc::new(encryption::SuppliedProjectKeys::new());
            supplied
                .insert_hex(project_id, &"1".repeat(64))
                .expect("fixture root key must parse");
            let app_id = "app_write_pipeline";
            supplied.bind_app(app_id, project_id).unwrap();
            let key_source = encryption::ProjectKeySource::supplied(supplied);
            let binding = crate::tests::fixtures::harness_binding(app_id);
            let collection = "users";
            let schema = crate::tests::fixtures::schema::generated_fields(crate::value!({
                "email": { "type": "string", "required": true, "unique": true },
                "name": { "type": "string", "required": true },
                "ssn": {
                    "type": "string",
                    "encrypted": true,
                    "mask": { "kind": "last4", "classification": "spi" }
                }
            }));
            let ddl_schema = crate::value!({
                "email": { "type": "string", "required": true, "unique": true },
                "name": { "type": "string", "required": true },
                "ssn": {
                    "type": "string",
                    "encrypted": true,
                    "mask": { "kind": "last4", "classification": "spi" }
                }
            });

            let dir = tempfile::tempdir().expect("tempdir");
            let backend = Rc::new(
                crate::backend_selection::new_sqlite_backend(PathBuf::from(dir.path()), key_source)
                    .expect("open sqlite backend"),
            );
            backend
                .attach_binding(&crate::tests::fixtures::harness_binding(app_id))
                .await
                .expect("ensure schema");
            let handle = crate::backend::BackendHandle::new(Rc::clone(&backend));
            // One route for every stage below. `apply` reads the live catalog
            // through it before any stage runs, so the four write shapes below
            // all exercise the protection-floor fence against a table whose
            // sentinels the DDL further down really wrote - the happy arm, where
            // descriptor and catalog agree.
            let route = crate::exec::ambient_route_for_tests(&crate::tests::fixtures::harness_binding(app_id), handle.clone());
            cache_schema(app_id, collection, schema.clone());
            let schema = crate::tests::fixtures::native_fields(schema);

            let ddl = sqlite_fixture_sql(
                crate::tests::fixtures::harness_binding(app_id).schema(),
                collection,
                &ddl_schema,
                &FkEmission::Inline,
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

            let mut insert_doc = crate::value!({
                "email": "seed@example.com",
                "name": "Seed",
                "ssn": "123-45-6789"
            });
            apply(
                backend.key_store(),
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
                &insert_doc,
                "123-45-6789",
                Some("usr_insert"),
            )
            .await;

            let insert_built = crate::crud::insert::build_one(
                binding.schema(),
                collection,
                &schema,
                insert_doc.clone(),
                &SqlRegistration::sqlite(),
            )
            .expect("build insert");
            let insert_params = &insert_built.params;
            let client = backend
                .fixture_session(&crate::tests::fixtures::harness_alias(app_id))
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

            let mut bulk_docs = crate::value!([
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
                &bulk[0],
                "987-65-4321",
                Some("usr_bulk"),
            )
            .await;
            assert_encrypted_masked_row(
                backend.as_ref(),
                app_id,
                collection,
                &bulk[1],
                "111-22-3333",
                Some("usr_bulk"),
            )
            .await;

            let mut update_patch = crate::value!({
                "$set": {
                    "name": "Canonical update",
                    "updated_by": "usr_override"
                },
                "ssn": { "$set": "555-55-5555" }
            });
            inspect_update(&schema, &mut update_patch).expect("inspect update patch");
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
                .get(crate::sql::mapping::raw_column_name("ssn").as_str())
                .and_then(Value::as_bytes)
                .expect("update ssn ciphertext in the raw column");
            let update_key = backend
                .key_store()
                .resolve(app_id)
                .await
                .expect("resolve update key");
            let update_plaintext = crate::encryption::aead::decrypt(
                &update_key,
                update_ciphertext,
                &encryption::canonical_aad(app_id, collection, "ssn", seeded_id.as_bytes()),
            )
            .expect("decrypt update ciphertext");
            assert_eq!(
                update_plaintext, b"555-55-5555",
                "update pipeline must encrypt against the filter row id",
            );

            let conflict_fields = crate::value!(["email"]);
            let mut upsert_doc = crate::value!({
                "email": "seed@example.com",
                "name": "Seed Updated",
                "ssn": "222-33-4444"
            });
            apply(
                backend.key_store(),
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
                &upsert_doc,
                "222-33-4444",
                Some("usr_upsert"),
            )
            .await;
        });
    }

    /// SQLite introspection must retain the protection floor recorded in the file.
    #[test]
    fn a_sqlite_write_is_refused_when_the_descriptor_drops_a_mask_the_file_still_records() {
        run(async {
            let app_id = "app_sqlite_protection_floor";
            let binding = crate::tests::fixtures::harness_binding(app_id);
            let collection = "people";
            let masked = crate::value!({
                "ssn": { "type": "string", "mask": { "kind": "last4", "classification": "spi" } },
                // The control: same type, no mask. Every refusal below has to be
                // about `ssn` and not about the collection being unwritable.
                "nickname": { "type": "string" },
            });

            let dir = tempfile::tempdir().expect("tempdir");
            let backend = Rc::new(
                crate::backend_selection::new_sqlite_backend(
                    PathBuf::from(dir.path()),
                    encryption::ProjectKeySource::unavailable(),
                )
                .expect("open sqlite backend"),
            );
            backend.attach_binding(&crate::tests::fixtures::harness_binding(app_id)).await.expect("attach app");
            let handle = crate::backend::BackendHandle::new(Rc::clone(&backend));
            let route = crate::exec::ambient_route_for_tests(&crate::tests::fixtures::harness_binding(app_id), handle);

            // Build the file-backed fixture from its schema so the raw storage
            // column and masking sentinel match the reader contract.
            let ddl = sqlite_fixture_sql(
                crate::tests::fixtures::harness_binding(app_id).schema(),
                collection,
                &masked,
                &FkEmission::Inline,
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
            cache_schema(app_id, collection, masked);
            let mut ok_doc = crate::value!({ "ssn": "123-45-6789", "nickname": "alice" });
            apply(
                backend.key_store(),
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
            cache_schema(
                app_id,
                collection,
                crate::value!({
                    "ssn": { "type": "string" },
                    "nickname": { "type": "string" },
                }),
            );
            let mut doc = crate::value!({ "ssn": "987-65-4321", "nickname": "bob" });
            let err = apply(
                backend.key_store(),
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
