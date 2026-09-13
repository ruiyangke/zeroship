//! Batched reference reads shared by native Rust and adapter queries.
use super::*;
use crate::sql::{descriptors, registration::SqlRegistration};
use crate::tx_route::TxRoute;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

fn invalid(message: impl Into<String>) -> DbError {
    DbError::validation("invalid_relation", message.into())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum KeyKind {
    Text,
    Integer,
}

impl KeyKind {
    fn for_field(field: &Value) -> Result<Self, DbError> {
        match field.get("type").and_then(Value::as_str) {
            Some("string" | "text" | "id" | "ref") => Ok(Self::Text),
            Some("integer" | "int" | "bigint" | "bigInt") => Ok(Self::Integer),
            _ => Err(invalid("reference keys must use text or integer storage")),
        }
    }

    fn key(self, value: &Value) -> Result<Key, DbError> {
        match (self, value) {
            (Self::Text, Value::String(value)) => Ok(Key::Text(value.clone())),
            (Self::Integer, Value::Number(value)) => value
                .as_i64()
                .map(Key::Integer)
                .ok_or_else(|| invalid("reference integer is outside the signed database range")),
            _ => Err(DbError::validation(
                "WITH_FK_NOT_ID_SHAPED",
                "reference value does not match its declared storage type",
            )),
        }
    }
}

#[derive(Debug, PartialEq, Eq, Hash)]
enum Key {
    Text(String),
    Integer(i64),
}

fn validate_key(schema: &Value, field: &str) -> Result<KeyKind, DbError> {
    let definition = schema
        .get(field)
        .ok_or_else(|| invalid("reference key is not declared"))?;
    if !descriptors::readable_fields(schema).contains(field)
        || definition.get("filterable").and_then(Value::as_bool) == Some(false)
        || descriptors::is_encrypted(definition)
        || descriptors::effective_mask(definition).is_some()
    {
        return Err(invalid(format!(
            "reference key '{field}' must be readable, filterable and unprotected"
        )));
    }
    KeyKind::for_field(definition)
}

#[derive(Debug)]
struct Reference {
    field: String,
    collection: String,
    column: String,
    schema: Arc<Value>,
    kind: KeyKind,
    batch_size: usize,
}

#[derive(Debug)]
pub(super) struct PreparedRelations {
    source: String,
    schema: Arc<Value>,
    references: Vec<Reference>,
}

pub(super) struct LoadedRelation {
    pub field: String,
    pub rows: Vec<Option<Value>>,
    pub has_masked: bool,
}

impl PreparedRelations {
    pub(super) fn new(
        binding: &DbBinding,
        registration: &SqlRegistration,
        source: &str,
        fields: &[String],
    ) -> Result<Self, DbError> {
        let schema = crate::descriptor::collection_schema(binding, source)?;
        let mut references = Vec::new();
        let mut seen = HashSet::new();
        for field in fields {
            if !seen.insert(field.clone()) {
                continue;
            }
            let collection = schema
                .get(field)
                .and_then(|definition| definition.get("refTarget"))
                .and_then(Value::as_str)
                .filter(|name| !name.is_empty())
                .ok_or_else(|| {
                    DbError::validation(
                        "WITH_NOT_A_REF_FIELD",
                        format!("'{source}.{field}' is not a declared reference"),
                    )
                })?;
            let kind = validate_key(&schema, field)?;
            let column = match schema[field].get("refColumn") {
                Some(Value::String(name)) if !name.is_empty() => name,
                _ => return Err(invalid("reference target column must be a nonempty string")),
            };
            let target =
                crate::descriptor::collection_schema(binding, collection).map_err(|_| {
                    DbError::validation(
                        "WITH_TARGET_NOT_FOUND",
                        format!("reference target '{collection}' is not declared"),
                    )
                })?;
            if kind != validate_key(&target, column)? {
                return Err(invalid("reference and target key storage types differ"));
            }
            if target[column].get("primaryKey").and_then(Value::as_bool) != Some(true)
                && target[column].get("unique").and_then(Value::as_bool) != Some(true)
            {
                return Err(invalid("a forward reference must target a unique column"));
            }
            // Validate target projection and comparison before an empty parent result can hide errors.
            crate::crud::read::find(
                binding.schema(),
                collection,
                &target,
                Value::Object(
                    [(
                        column.into(),
                        Value::Object([("$in".into(), Value::Array(Vec::new()))].into()),
                    )]
                    .into(),
                ),
                Some(crate::sql::MAX_ROW_LIMIT),
                None,
                None,
                None,
                &[],
                true,
                registration,
            )?;
            crate::cdc::read_set::record_if_active(
                collection,
                &Value::Object(Record::new()),
                &target,
            );
            references.push(Reference {
                field: field.clone(),
                collection: collection.into(),
                column: column.into(),
                batch_size: crate::crud::read::relation_match_capacity(&target, registration)?,
                schema: target,
                kind,
            });
        }
        Ok(Self {
            source: source.into(),
            schema,
            references,
        })
    }

    fn validate_schemas(&self, binding: &DbBinding) -> Result<(), DbError> {
        for (collection, schema) in std::iter::once((&self.source, &self.schema)).chain(
            self.references
                .iter()
                .map(|reference| (&reference.collection, &reference.schema)),
        ) {
            if crate::descriptor::collection_schema(binding, collection)?.as_ref()
                != schema.as_ref()
            {
                return Err(schema_mismatch_for(collection));
            }
        }
        Ok(())
    }

    pub(super) async fn load(
        &self,
        binding: &DbBinding,
        route: &TxRoute,
        rows: &[Value],
    ) -> Result<Vec<LoadedRelation>, DbError> {
        self.validate_schemas(binding)?;
        let mut output_budget = read::MAX_READ_RESULT_BYTES;
        for row in rows {
            read::consume_budget(row, &mut output_budget)?;
        }
        let mut loaded = Vec::new();
        for reference in &self.references {
            let mut ids = Vec::new();
            let mut seen = HashSet::new();
            for row in rows {
                let value = row
                    .get(&reference.field)
                    .ok_or_else(|| invalid("parent projection omitted its reference key"))?;
                if !value.is_null() && seen.insert(reference.kind.key(value)?) {
                    ids.push(value.clone());
                }
            }
            let mut targets = HashMap::new();
            let mut has_masked = false;
            let mut target_budget = read::MAX_READ_RESULT_BYTES;
            for chunk in ids.chunks(reference.batch_size) {
                self.validate_schemas(binding)?;
                let (query, match_slots) = crate::crud::read::find_with_key_matches(
                    binding.schema(),
                    &reference.collection,
                    &reference.schema,
                    &reference.column,
                    chunk,
                    route.sql_registration(),
                )?;
                let mut target_rows = crate::exec::exec_query(route, query).await?;
                for row in &target_rows {
                    read::consume_budget(row, &mut target_budget)?;
                }
                let matches = target_rows
                    .iter_mut()
                    .map(|row| {
                        let row = row
                            .as_object_mut()
                            .ok_or_else(|| DbError::internal("expected reference target record"))?;
                        match_slots
                            .iter()
                            .enumerate()
                            .filter_map(|(index, slot)| match row.swap_remove(slot) {
                                Some(Value::Bool(true)) => Some(Ok(index)),
                                Some(Value::Bool(false)) => None,
                                Some(Value::Number(number)) if number.as_i64() == Some(1) => {
                                    Some(Ok(index))
                                }
                                Some(Value::Number(number)) if number.as_i64() == Some(0) => None,
                                _ => Some(Err(DbError::internal(
                                    "reference match projection is not boolean",
                                ))),
                            })
                            .collect::<Result<Vec<_>, DbError>>()
                    })
                    .collect::<Result<Vec<_>, DbError>>()?;
                self.validate_schemas(binding)?;
                let result = crate::crud::read_pipeline::apply(
                    route,
                    binding,
                    &reference.collection,
                    target_rows,
                    crate::crud::read_pipeline::ApplyOptions::default(),
                )
                .await?;
                self.validate_schemas(binding)?;
                has_masked |= result.has_masked;
                if result.rows.len() != matches.len() {
                    return Err(DbError::internal(
                        "reference protection changed row alignment",
                    ));
                }
                for (row, matches) in result.rows.into_iter().zip(matches) {
                    if matches.is_empty() {
                        return Err(DbError::internal(
                            "reference target has no matching lookup key",
                        ));
                    }
                    let row = Rc::new(row);
                    for index in matches {
                        let key = reference.kind.key(&chunk[index])?;
                        if targets.insert(key, row.clone()).is_some() {
                            return Err(invalid("reference target key is not unique"));
                        }
                    }
                }
            }
            let rows = rows
                .iter()
                .map(|row| {
                    let value = &row[reference.field.as_str()];
                    if value.is_null() {
                        Ok(None)
                    } else {
                        let target = targets.get(&reference.kind.key(value)?);
                        if let Some(target) = target {
                            read::consume_budget(target, &mut output_budget)?;
                        }
                        Ok(target.map(|row| row.as_ref().clone()))
                    }
                })
                .collect::<Result<Vec<_>, DbError>>()?;
            loaded.push(LoadedRelation {
                field: reference.field.clone(),
                rows,
                has_masked,
            });
        }
        Ok(loaded)
    }
}

#[derive(Debug)]
pub(super) struct FindRelations {
    prepared: PreparedRelations,
    aliases: Vec<(String, String)>,
    hidden_keys: Vec<String>,
}

impl FindRelations {
    pub(super) fn validate_schemas(&self, binding: &DbBinding) -> Result<(), DbError> {
        self.prepared.validate_schemas(binding)
    }

    pub(super) fn new(
        binding: &DbBinding,
        registration: &SqlRegistration,
        collection: &str,
        options: &mut Value,
    ) -> Result<Option<Self>, DbError> {
        let Some(spec) = options.get("with") else {
            return Ok(None);
        };
        let spec = spec
            .as_object()
            .ok_or_else(|| invalid("with must select named schema relations"))?;
        if spec.len() > crate::sql::MAX_READ_SOURCES {
            return Err(invalid("with exceeds the relation budget"));
        }
        let schema = crate::descriptor::collection_schema(binding, collection)?;
        let declared = descriptors::relation_fields(&schema).map_err(invalid)?;
        let mut aliases = Vec::new();
        for (alias, spec) in spec {
            if spec.as_bool() != Some(true) {
                return Err(DbError::validation(
                    "WITH_UNSUPPORTED_VALUE",
                    "select a relation with true",
                ));
            }
            let field = declared.get(alias.as_str()).ok_or_else(|| {
                DbError::validation(
                    "unknown_relation",
                    format!("'{collection}.{alias}' is not a named schema relation"),
                )
            })?;
            aliases.push((alias.clone(), (*field).to_owned()));
        }
        if aliases.is_empty() {
            return Ok(None);
        }
        let fields = aliases
            .iter()
            .map(|(_, field)| field.clone())
            .collect::<Vec<_>>();
        let prepared = PreparedRelations::new(binding, registration, collection, &fields)?;
        let mut hidden_keys = Vec::new();
        if let Some(Value::Array(select)) = options
            .as_object_mut()
            .and_then(|options| options.get_mut("select"))
        {
            if !select.is_empty() {
                for field in fields {
                    if !select.iter().any(|value| value.as_str() == Some(&field)) {
                        select.push(Value::String(field.clone()));
                        hidden_keys.push(field);
                    }
                }
            }
        }
        Ok(Some(Self {
            prepared,
            aliases,
            hidden_keys,
        }))
    }

    pub(super) async fn apply(
        self,
        binding: &DbBinding,
        route: &TxRoute,
        result: &mut crate::crud::read_pipeline::ApplyResult,
    ) -> Result<(), DbError> {
        let loaded = self.prepared.load(binding, route, &result.rows).await?;
        for relation in loaded {
            result.has_masked |= relation.has_masked;
            for (row, target) in result.rows.iter_mut().zip(relation.rows) {
                let row = row
                    .as_object_mut()
                    .ok_or_else(|| DbError::internal("expected relation parent record"))?;
                for (alias, _) in self
                    .aliases
                    .iter()
                    .filter(|(_, field)| field == &relation.field)
                {
                    row.insert(alias.clone(), target.clone().unwrap_or(Value::Null));
                }
            }
        }
        for row in &mut result.rows {
            let row = row
                .as_object_mut()
                .ok_or_else(|| DbError::internal("expected relation parent record"))?;
            for key in &self.hidden_keys {
                row.swap_remove(key);
            }
        }
        let mut output_budget = read::MAX_READ_RESULT_BYTES;
        for row in &result.rows {
            read::consume_budget(row, &mut output_budget)?;
        }
        Ok(())
    }
}
