//! Resolved physical statements. Application policy is applied before this boundary.

use super::{Ident, IdentRole, SchemaName, compiler::CompileError, predicate::CompareOp};
use crate::value::Value;
use std::{
    collections::{BTreeMap, HashSet},
    sync::Arc,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StorageType {
    Boolean,
    Integer,
    Real,
    Text,
    Bytes,
    Timestamp,
    Decimal,
    Json,
    Vector,
    GeoPoint,
}

impl StorageType {
    fn accepts(self, value: &Value) -> bool {
        if value.is_null() {
            return true;
        }
        match self {
            Self::Boolean => matches!(value, Value::Bool(_)),
            Self::Integer => value.as_i64().is_some() || value.as_u64().is_some(),
            Self::Real => matches!(value, Value::Number(_)),
            Self::Text => matches!(value, Value::String(_)),
            Self::Bytes => matches!(value, Value::Bytes(_)),
            Self::Timestamp => {
                matches!(value, Value::Timestamp(_))
                    || matches!(value, Value::String(_))
                        && crate::sql::temporal::timestamp_millis(value).is_some()
            }
            Self::Decimal => matches!(value, Value::Decimal(_)),
            Self::Json => matches!(value, Value::Json(_) | Value::Array(_) | Value::Object(_)),
            Self::Vector => matches!(value, Value::Array(_)),
            Self::GeoPoint => matches!(value, Value::Object(_)),
        }
    }

    fn numeric(self) -> bool {
        matches!(self, Self::Integer | Self::Real | Self::Decimal)
    }
}

#[derive(Debug)]
struct Source {
    namespace: SchemaName,
    table: Ident,
    columns: Vec<(Ident, StorageType)>,
    indices: BTreeMap<String, usize>,
}

#[derive(Clone, Debug)]
pub struct Table(Arc<Source>);

impl Table {
    pub fn new(
        namespace: SchemaName,
        table: Ident,
        columns: impl IntoIterator<Item = (Ident, StorageType)>,
    ) -> Result<Self, CompileError> {
        Ident::parse_as(table.as_str(), IdentRole::Collection)?;
        let mut indices = BTreeMap::new();
        let mut resolved = Vec::new();
        for (name, storage) in columns {
            Ident::parse_as(name.as_str(), IdentRole::StoredColumn)?;
            if indices
                .insert(name.as_str().to_owned(), resolved.len())
                .is_some()
            {
                return Err(invalid("duplicate physical column"));
            }
            resolved.push((name, storage));
        }
        Ok(Self(Arc::new(Source {
            namespace,
            table,
            columns: resolved,
            indices,
        })))
    }

    pub fn namespace(&self) -> &SchemaName {
        &self.0.namespace
    }
    pub fn name(&self) -> &Ident {
        &self.0.table
    }

    pub fn column(&self, name: &str) -> Result<Column, CompileError> {
        let index = self
            .0
            .indices
            .get(name)
            .copied()
            .ok_or_else(|| invalid("column does not belong to the resolved table"))?;
        Ok(Column {
            source: self.clone(),
            index,
        })
    }

    fn check_column(&self, column: &Column) -> Result<(), CompileError> {
        if !Arc::ptr_eq(&self.0, &column.source.0) {
            return Err(invalid("column belongs to a different source"));
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct Column {
    source: Table,
    index: usize,
}

impl Column {
    pub fn table(&self) -> &Table {
        &self.source
    }
    pub fn name(&self) -> &Ident {
        &self.source.0.columns[self.index].0
    }
    pub fn storage(&self) -> StorageType {
        self.source.0.columns[self.index].1
    }
}

pub enum Expression {
    Bind(Value),
    Null,
    Default,
    Current(Column),
    Incoming(Column),
    Increment { column: Column, step: i64 },
    CurrentTimestamp,
}

#[derive(Debug)]
pub struct Assignment {
    pub column: Column,
    pub value: Expression,
}

pub struct Comparison {
    pub column: Column,
    pub op: CompareOp,
    pub value: Value,
}

#[derive(Debug)]
pub struct ReturnedColumn {
    pub column: Column,
    pub alias: Option<Ident>,
}

#[derive(Debug)]
pub struct UpsertParts {
    pub table: Table,
    pub insert: Vec<Assignment>,
    pub conflict: Vec<Column>,
    pub update: Vec<Assignment>,
    pub conditions: Vec<Comparison>,
    pub returning: Vec<ReturnedColumn>,
    pub insert_generated_identity: bool,
}

#[derive(Debug)]
pub struct Upsert(UpsertParts);

impl Upsert {
    pub fn new(mut parts: UpsertParts) -> Result<Self, CompileError> {
        validate(&parts)?;
        // Input records are unordered; projection and conflict order are retained.
        parts
            .insert
            .sort_by(|a, b| a.column.name().cmp(b.column.name()));
        Ok(Self(parts))
    }

    pub fn parts(&self) -> &UpsertParts {
        &self.0
    }
    pub fn into_parts(self) -> UpsertParts {
        self.0
    }
    pub fn validate(&self) -> Result<(), CompileError> {
        validate(&self.0)
    }
}

#[derive(Debug)]
pub enum Statement {
    Upsert(Upsert),
}

fn invalid(message: &'static str) -> CompileError {
    CompileError::InvalidStatement(message.into())
}

fn validate(parts: &UpsertParts) -> Result<(), CompileError> {
    if parts.insert.is_empty() || parts.conflict.is_empty() || parts.update.is_empty() {
        return Err(invalid(
            "upsert requires insert values, a conflict target, and an update",
        ));
    }
    for (assignments, inserting) in [(&parts.insert, true), (&parts.update, false)] {
        let mut columns = HashSet::new();
        for assignment in assignments {
            parts.table.check_column(&assignment.column)?;
            if !columns.insert(assignment.column.index) {
                return Err(invalid("column is assigned more than once"));
            }
            let storage = assignment.column.storage();
            match &assignment.value {
                Expression::Bind(value) if !storage.accepts(value) => {
                    return Err(invalid(
                        "bound value does not match its physical storage type",
                    ));
                }
                Expression::Bind(_) | Expression::Null | Expression::Default => {}
                _ if inserting => {
                    return Err(invalid("insert values cannot reference a conflict row"));
                }
                Expression::Current(column) | Expression::Incoming(column) => {
                    parts.table.check_column(column)?;
                    if column.storage() != storage {
                        return Err(invalid("assignment column types differ"));
                    }
                }
                Expression::Increment { column, .. } => {
                    parts.table.check_column(column)?;
                    if column.index != assignment.column.index || !storage.numeric() {
                        return Err(invalid("increment requires its assigned numeric column"));
                    }
                }
                Expression::CurrentTimestamp => {
                    if storage != StorageType::Timestamp {
                        return Err(invalid("current timestamp requires timestamp storage"));
                    }
                }
            }
        }
    }
    let mut conflict = HashSet::new();
    for column in &parts.conflict {
        parts.table.check_column(column)?;
        if !conflict.insert(column.index) {
            return Err(invalid("duplicate conflict target column"));
        }
    }
    for condition in &parts.conditions {
        parts.table.check_column(&condition.column)?;
        if condition.value.is_null() || !condition.column.storage().accepts(&condition.value) {
            return Err(invalid(
                "comparison requires a non-null value with matching storage type",
            ));
        }
    }
    for field in &parts.returning {
        parts.table.check_column(&field.column)?;
        if let Some(alias) = &field.alias {
            Ident::parse_as(alias.as_str(), IdentRole::Alias)?;
        }
    }
    Ok(())
}

impl std::fmt::Debug for Expression {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Bind(value) => f
                .debug_tuple("Bind")
                .field(&super::compiler::ParameterType::of(value))
                .finish(),
            Self::Null => f.write_str("Null"),
            Self::Default => f.write_str("Default"),
            Self::Current(column) => f.debug_tuple("Current").field(column).finish(),
            Self::Incoming(column) => f.debug_tuple("Incoming").field(column).finish(),
            Self::Increment { column, .. } => f
                .debug_struct("Increment")
                .field("column", column)
                .finish_non_exhaustive(),
            Self::CurrentTimestamp => f.write_str("CurrentTimestamp"),
        }
    }
}

impl std::fmt::Debug for Comparison {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Comparison")
            .field("column", &self.column)
            .field("op", &self.op)
            .field(
                "parameter_type",
                &super::compiler::ParameterType::of(&self.value),
            )
            .finish()
    }
}
