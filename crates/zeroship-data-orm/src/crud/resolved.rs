//! Descriptor resolution shared by collection statement builders.

use crate::{
    sql::{
        compile::{self, QueryError},
        registration::SqlRegistration,
        statement::{ReturnedColumn, StorageType, Table},
        Ident, IdentRole, SchemaName,
    },
    value::Value,
};
use std::collections::BTreeMap;

#[derive(Debug)]
pub(crate) struct PhysicalInput {
    pub(crate) column: String,
    pub(crate) storage: StorageType,
    pub(crate) insert_only: bool,
}

#[derive(Debug)]
pub(crate) struct ResolvedTable {
    pub(crate) table: Table,
    pub(crate) inputs: BTreeMap<String, PhysicalInput>,
}

impl ResolvedTable {
    pub(crate) fn new(
        namespace: &SchemaName,
        collection: &str,
        schema: &Value,
        registration: &SqlRegistration,
    ) -> Result<Self, QueryError> {
        Self::build(namespace, collection, None, schema, registration)
    }

    pub(crate) fn aliased(
        namespace: &SchemaName,
        collection: &str,
        alias: &str,
        schema: &Value,
        registration: &SqlRegistration,
    ) -> Result<Self, QueryError> {
        Self::build(namespace, collection, Some(alias), schema, registration)
    }

    fn build(
        namespace: &SchemaName,
        collection: &str,
        alias: Option<&str>,
        schema: &Value,
        registration: &SqlRegistration,
    ) -> Result<Self, QueryError> {
        compile::validate_collection(collection)?;
        let fields = schema
            .as_object()
            .ok_or_else(|| invalid("statement requires a field-map schema"))?;
        let mut physical = Vec::new();
        let mut inputs = BTreeMap::new();
        for (name, definition) in fields {
            if compile::is_schema_metadata_key(name) {
                continue;
            }
            let stored = compile::value_column_for_field(name, schema);
            let storage = if crate::sql::descriptors::effective_mask(definition).is_some() {
                StorageType::Text
            } else {
                registration.storage_type(definition)?
            };
            physical.push((stored_ident(&stored)?, storage));
            let insert_only = name == "id" || definition["assign"]["on"].as_str() == Some("insert");
            inputs.insert(
                name.clone(),
                PhysicalInput {
                    column: stored.clone(),
                    storage,
                    insert_only,
                },
            );
            if let Some(raw) = compile::declared_raw_column(name, definition)? {
                let raw_storage = registration.storage_type(definition)?;
                physical.push((stored_ident(&raw)?, raw_storage));
                inputs.insert(
                    raw.clone(),
                    PhysicalInput {
                        column: raw,
                        storage: raw_storage,
                        insert_only,
                    },
                );
            }
        }
        let collection = Ident::parse_as(collection, IdentRole::Collection)
            .map_err(crate::sql::compiler::CompileError::from)?;
        let table = match alias {
            Some(alias) => Table::aliased(
                namespace.clone(),
                collection,
                Ident::parse_as(alias, IdentRole::Alias)
                    .map_err(crate::sql::compiler::CompileError::from)?,
                physical,
            )?,
            None => Table::new(namespace.clone(), collection, physical)?,
        };
        Ok(Self { table, inputs })
    }

    pub(crate) fn returning(&self, schema: &Value) -> Result<Vec<ReturnedColumn>, QueryError> {
        compile::implicit_read_fields(schema)?
            .into_iter()
            .map(|name| {
                let physical = compile::value_column_for_field(name, schema);
                let alias = if physical == name {
                    None
                } else {
                    Some(
                        Ident::parse_as(name, IdentRole::Alias)
                            .map_err(crate::sql::compiler::CompileError::from)?,
                    )
                };
                Ok(ReturnedColumn {
                    column: self.table.column(&physical)?,
                    alias,
                })
            })
            .collect()
    }
}

fn stored_ident(name: &str) -> Result<Ident, QueryError> {
    Ident::parse_as(name, IdentRole::StoredColumn)
        .map_err(|error| crate::sql::compiler::CompileError::from(error).into())
}

fn invalid(message: &str) -> QueryError {
    QueryError::InvalidFilter(message.into())
}
