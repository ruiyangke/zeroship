//! Descriptor-resolved collection reads.

use super::{predicate, resolved::ResolvedTable};
use crate::{
    sql::{
        compile::{self, QueryError},
        registration::SqlRegistration,
        statement::{
            ResolvedOperand, ResolvedOrder, ResolvedPredicate, SelectParts, SelectStatement,
            SelectedExpression, Statement,
        },
        Direction, Ident, IdentRole, NullOrder, RowLimit, RowOffset, SchemaName,
    },
    value::Value,
};
use std::collections::BTreeSet;

const SOURCE_ALIAS: &str = "source";

#[allow(clippy::too_many_arguments)]
pub(super) fn find(
    namespace: &SchemaName,
    collection: &str,
    schema: &Value,
    filter: Value,
    limit: Option<i64>,
    offset: Option<i64>,
    order_by: Option<&Value>,
    select: Option<&Value>,
    unmask_columns: &[String],
    filter_soft_deleted: bool,
    registration: &SqlRegistration,
) -> Result<crate::sql::compiler::CompiledQuery, QueryError> {
    let table = ResolvedTable::aliased(namespace, collection, SOURCE_ALIAS, schema, registration)?;
    let fields = projection_fields(select, schema, unmask_columns)?;
    let projection = fields
        .into_iter()
        .map(|field| selected(&table, &field))
        .collect::<Result<Vec<_>, _>>()?;
    let predicate = visible_predicate(
        predicate::resolve(filter, schema, &table, registration)?,
        schema,
        &table,
        filter_soft_deleted,
    )?;
    let order_by = parse_order(order_by, schema, &table)?;
    let limit = limit
        .map(RowLimit::new)
        .transpose()
        .map_err(|error| invalid(error.to_string()))?
        .map(RowLimit::get);
    let offset = offset
        .map(RowOffset::new)
        .transpose()
        .map_err(|error| invalid(error.to_string()))?
        .map(RowOffset::get);
    compile_select(
        table,
        projection,
        predicate,
        order_by,
        limit,
        offset,
        false,
        registration,
    )
}

pub(super) fn count(
    namespace: &SchemaName,
    collection: &str,
    schema: &Value,
    filter: Value,
    filter_soft_deleted: bool,
    registration: &SqlRegistration,
) -> Result<crate::sql::compiler::CompiledQuery, QueryError> {
    let table = ResolvedTable::aliased(namespace, collection, SOURCE_ALIAS, schema, registration)?;
    let predicate = visible_predicate(
        predicate::resolve(filter, schema, &table, registration)?,
        schema,
        &table,
        filter_soft_deleted,
    )?;
    let projection = vec![SelectedExpression {
        expression: ResolvedOperand::Aggregate {
            function: crate::sql::AggregateFunc::Count,
            column: None,
            distinct: false,
        },
        alias: alias("count")?,
    }];
    compile_select(
        table,
        projection,
        predicate,
        Vec::new(),
        None,
        None,
        false,
        registration,
    )
}

pub(super) fn distinct(
    namespace: &SchemaName,
    collection: &str,
    schema: &Value,
    field: &str,
    filter: Value,
    filter_soft_deleted: bool,
    registration: &SqlRegistration,
) -> Result<crate::sql::compiler::CompiledQuery, QueryError> {
    compile::validate_field_name(field)?;
    if !crate::sql::descriptors::readable_fields(schema).contains(field) {
        return Err(QueryError::InvalidIdent(format!(
            "field '{field}' is not a readable schema field; readable fields must be declared in the schema"
        )));
    }
    compile::validate_value_operation(field, schema)?;
    let table = ResolvedTable::aliased(namespace, collection, SOURCE_ALIAS, schema, registration)?;
    let selected = selected(&table, field)?;
    let order_by = vec![ResolvedOrder {
        expression: selected.expression.clone(),
        direction: Direction::Ascending,
        nulls: NullOrder::Last,
    }];
    let predicate = visible_predicate(
        predicate::resolve(filter, schema, &table, registration)?,
        schema,
        &table,
        filter_soft_deleted,
    )?;
    compile_select(
        table,
        vec![selected],
        predicate,
        order_by,
        None,
        None,
        true,
        registration,
    )
}

#[allow(clippy::too_many_arguments)]
fn compile_select(
    table: ResolvedTable,
    projection: Vec<SelectedExpression>,
    predicate: ResolvedPredicate,
    order_by: Vec<ResolvedOrder>,
    limit: Option<i64>,
    offset: Option<i64>,
    distinct: bool,
    registration: &SqlRegistration,
) -> Result<crate::sql::compiler::CompiledQuery, QueryError> {
    let statement = SelectStatement::new(SelectParts {
        table: table.table,
        joins: Vec::new(),
        projection,
        predicate,
        group_by: Vec::new(),
        having: ResolvedPredicate::Const(true),
        order_by,
        limit,
        offset,
        distinct,
        lock: crate::sql::statement::RowLock::None,
    })?;
    registration
        .compile(Statement::Select(statement))
        .map_err(Into::into)
}

fn projection_fields(
    select: Option<&Value>,
    schema: &Value,
    unmask_columns: &[String],
) -> Result<Vec<String>, QueryError> {
    let Some(fields) = select
        .and_then(Value::as_array)
        .filter(|fields| !fields.is_empty())
    else {
        return Ok(compile::implicit_read_fields(schema)?
            .into_iter()
            .map(str::to_owned)
            .collect());
    };
    let readable = crate::sql::descriptors::readable_fields(schema);
    let mut selected = Vec::with_capacity(fields.len());
    let mut seen = BTreeSet::new();
    for field in fields {
        let field = field
            .as_str()
            .ok_or_else(|| invalid("select entries must be strings"))?;
        compile::validate_field_name(field)?;
        if !readable.contains(field) {
            return Err(QueryError::InvalidIdent(format!(
                "field '{field}' is not a readable schema field; readable fields must be declared in the schema"
            )));
        }
        if !seen.insert(field.to_owned()) {
            return Err(invalid("select contains a duplicate field"));
        }
        selected.push(field.to_owned());
    }
    let needs_identity = !unmask_columns.is_empty()
        || selected.iter().any(|field| {
            schema.get(field).is_some_and(|definition| {
                crate::sql::descriptors::is_encrypted(definition)
                    || crate::sql::descriptors::effective_mask(definition).is_some()
            })
        });
    if needs_identity && seen.insert("id".into()) {
        selected.push("id".into());
    }
    Ok(selected)
}

pub(super) fn selected(
    table: &ResolvedTable,
    field: &str,
) -> Result<SelectedExpression, QueryError> {
    let input = table
        .inputs
        .get(field)
        .ok_or_else(|| invalid(format!("projection field has no physical column: {field}")))?;
    Ok(SelectedExpression {
        expression: ResolvedOperand::Column(table.table.column(&input.column)?),
        alias: alias(field)?,
    })
}

pub(super) fn visible_predicate(
    predicate: ResolvedPredicate,
    schema: &Value,
    table: &ResolvedTable,
    enabled: bool,
) -> Result<ResolvedPredicate, QueryError> {
    let marker = if enabled {
        crate::sql::lifecycle::soft_delete_column(schema)?
    } else {
        None
    };
    let Some(marker) = marker else {
        return Ok(predicate);
    };
    let input = table
        .inputs
        .get(marker)
        .ok_or_else(|| invalid("soft-delete field has no physical column"))?;
    Ok(ResolvedPredicate::and(vec![
        predicate,
        ResolvedPredicate::IsNull {
            operand: ResolvedOperand::Column(table.table.column(&input.column)?),
            negated: false,
        },
    ]))
}

fn parse_order(
    order: Option<&Value>,
    schema: &Value,
    table: &ResolvedTable,
) -> Result<Vec<ResolvedOrder>, QueryError> {
    let Some(order) = order else {
        return Ok(Vec::new());
    };
    let entries: Vec<(&str, &Value)> = match order {
        Value::Object(fields) => fields
            .iter()
            .map(|(field, value)| (field.as_str(), value))
            .collect(),
        Value::Array(entries) => entries
            .iter()
            .map(|entry| {
                let pair = entry
                    .as_array()
                    .filter(|pair| pair.len() == 2)
                    .ok_or_else(|| invalid("orderBy array entries must be [field, dir]"))?;
                let field = pair[0]
                    .as_str()
                    .ok_or_else(|| invalid("orderBy field must be a string"))?;
                Ok((field, &pair[1]))
            })
            .collect::<Result<_, QueryError>>()?,
        _ => return Err(invalid("orderBy must be an object or array")),
    };
    let readable = crate::sql::descriptors::readable_fields(schema);
    entries
        .into_iter()
        .map(|(field, direction)| {
            compile::validate_field_name(field)?;
            if !readable.contains(field) {
                return Err(QueryError::InvalidIdent(format!(
                    "field '{field}' is not a readable schema field; readable fields must be declared in the schema"
                )));
            }
            compile::validate_value_operation(field, schema)?;
            if schema[field]["sortable"].as_bool() == Some(false) {
                return Err(invalid(format!("field '{field}' is not sortable")));
            }
            let input = table
                .inputs
                .get(field)
                .ok_or_else(|| invalid(format!("sort field has no physical column: {field}")))?;
            let descending = direction.as_i64().is_some_and(|value| value < 0);
            Ok(ResolvedOrder {
                expression: ResolvedOperand::Column(table.table.column(&input.column)?),
                direction: if descending {
                    Direction::Descending
                } else {
                    Direction::Ascending
                },
                nulls: if descending {
                    NullOrder::First
                } else {
                    NullOrder::Last
                },
            })
        })
        .collect()
}

fn alias(name: &str) -> Result<Ident, QueryError> {
    Ident::parse_as(name, IdentRole::Alias)
        .map_err(crate::sql::compiler::CompileError::from)
        .map_err(Into::into)
}

fn invalid(message: impl Into<String>) -> QueryError {
    QueryError::InvalidFilter(message.into())
}
