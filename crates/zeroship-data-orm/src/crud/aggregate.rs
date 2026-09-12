//! JSON aggregate pipeline lowering into resolved select statements.

use super::{predicate, read, resolved::ResolvedTable};
use crate::{
    sql::{
        mapping::{self, QueryError},
        registration::SqlRegistration,
        statement::{
            ResolvedOperand, ResolvedOrder, ResolvedPredicate, ResolvedPredicateValue, SelectParts,
            SelectStatement, SelectedExpression, Statement, StorageType,
        },
        AggregateFunc, CompareOp, Direction, Ident, IdentRole, NullOrder, RowLimit, SchemaName,
    },
    value::Value,
};
use std::collections::{BTreeMap, BTreeSet};

const SOURCE_ALIAS: &str = "source";

#[derive(Clone)]
struct AggregateOutput {
    operand: ResolvedOperand,
    source_field: Option<String>,
}

pub(crate) fn build(
    namespace: &SchemaName,
    collection: &str,
    pipeline: &Value,
    filter_soft_deleted: bool,
    schema: &Value,
    registration: &SqlRegistration,
) -> Result<(crate::sql::compiler::CompiledQuery, Option<Vec<String>>), QueryError> {
    let stages = pipeline
        .as_array()
        .ok_or_else(|| invalid("aggregate: pipeline must be an array"))?;
    let table = ResolvedTable::aliased(namespace, collection, SOURCE_ALIAS, schema, registration)?;
    let mut matches = Vec::new();
    let mut group = None;
    let mut having = None;
    let mut order = None;
    let mut limit = None;
    let mut after_group = false;
    let mut after_sort = false;
    for stage in stages {
        let stage = stage
            .as_object()
            .filter(|stage| stage.len() == 1)
            .ok_or_else(|| invalid("aggregate: each stage must contain one operator"))?;
        let (operator, value) = stage.iter().next().expect("one aggregate stage");
        match operator.as_str() {
            "$match" if !after_group && !after_sort && limit.is_none() => {
                matches.push(predicate::resolve(
                    value.clone(),
                    schema,
                    &table,
                    registration,
                )?);
            }
            "$group" if group.is_none() && !after_sort && limit.is_none() => {
                group = Some(value);
                after_group = true;
            }
            "$having" if after_group && having.is_none() && !after_sort && limit.is_none() => {
                having = Some(value);
            }
            "$sort" if order.is_none() && limit.is_none() => {
                order = Some(value);
                after_sort = true;
            }
            "$limit" if limit.is_none() => {
                let value = value
                    .as_i64()
                    .ok_or_else(|| invalid("aggregate: $limit must be an integer"))?;
                limit = Some(
                    RowLimit::new(value)
                        .map_err(|error| invalid(error.to_string()))?
                        .get(),
                );
            }
            "$match" | "$group" | "$having" | "$sort" | "$limit" => {
                return Err(invalid("aggregate stages are duplicated or out of order"));
            }
            _ => return Err(invalid(format!("aggregate: unsupported stage: {operator}"))),
        }
    }
    let predicate = read::visible_predicate(
        ResolvedPredicate::and(matches),
        schema,
        &table,
        filter_soft_deleted,
    )?;

    let (projection, group_by, outputs, result_columns) = match group {
        Some(group) => grouped_projection(group, schema, &table)?,
        None => {
            if having.is_some() {
                return Err(invalid("aggregate: $having requires $group"));
            }
            let projection = mapping::implicit_read_fields(schema)?
                .into_iter()
                .map(|field| read::selected(&table, field))
                .collect::<Result<Vec<_>, _>>()?;
            (projection, Vec::new(), BTreeMap::new(), None)
        }
    };
    let having = match having {
        Some(value) => resolve_having(value, schema, &table, &outputs, registration)?,
        None => ResolvedPredicate::Const(true),
    };
    let order_by = match order {
        Some(value) => resolve_order(value, schema, &table, &outputs)?,
        None => Vec::new(),
    };
    let statement = SelectStatement::new(SelectParts {
        table: table.table,
        joins: Vec::new(),
        projection,
        predicate,
        group_by,
        having,
        order_by,
        limit,
        offset: None,
        distinct: false,
        lock: crate::sql::statement::RowLock::None,
    })?;
    Ok((
        registration.compile(Statement::Select(statement))?,
        result_columns,
    ))
}

type GroupedProjection = (
    Vec<SelectedExpression>,
    Vec<ResolvedOperand>,
    BTreeMap<String, AggregateOutput>,
    Option<Vec<String>>,
);

fn grouped_projection(
    group: &Value,
    schema: &Value,
    table: &ResolvedTable,
) -> Result<GroupedProjection, QueryError> {
    let fields = group
        .as_object()
        .ok_or_else(|| invalid("aggregate: $group must be an object"))?;
    let mut projection = Vec::new();
    let mut group_by = Vec::new();
    let mut outputs = BTreeMap::new();
    let mut result_columns = Vec::new();
    let mut names = BTreeSet::new();
    if let Some(by) = fields.get("by") {
        let names_in = match by {
            Value::String(field) => vec![field.as_str()],
            Value::Array(fields) => fields
                .iter()
                .map(|field| {
                    field
                        .as_str()
                        .ok_or_else(|| invalid("aggregate: $group.by entries must be strings"))
                })
                .collect::<Result<Vec<_>, _>>()?,
            _ => return Err(invalid("aggregate: $group.by must be a string or array")),
        };
        for field in names_in {
            validate_value_field(field, schema, "groupable")?;
            if !names.insert(field.to_owned()) {
                return Err(invalid("aggregate: duplicate group field"));
            }
            let selected = read::selected(table, field)?;
            group_by.push(selected.expression.clone());
            projection.push(selected);
            result_columns.push(field.to_owned());
        }
    }
    for (name, expression) in fields {
        if name == "by" {
            continue;
        }
        if !names.insert(name.clone()) {
            return Err(invalid("aggregate: duplicate result name"));
        }
        let alias = alias(name)?;
        let expression = expression
            .as_object()
            .filter(|expression| expression.len() == 1)
            .ok_or_else(|| invalid(format!("aggregate: $group.{name} requires one accumulator")))?;
        let (operator, argument) = expression.iter().next().expect("one accumulator");
        let (function, source_field) = match operator.as_str() {
            "$count" => (AggregateFunc::Count, None),
            "$sum" => (
                AggregateFunc::Sum,
                Some(field_argument(operator, argument)?),
            ),
            "$avg" => (
                AggregateFunc::Avg,
                Some(field_argument(operator, argument)?),
            ),
            "$min" => (
                AggregateFunc::Min,
                Some(field_argument(operator, argument)?),
            ),
            "$max" => (
                AggregateFunc::Max,
                Some(field_argument(operator, argument)?),
            ),
            "$first" => {
                return Err(invalid(
                    "aggregate: $first has no portable backend semantics",
                ));
            }
            _ => {
                return Err(invalid(format!(
                    "aggregate: unsupported accumulator: {operator}"
                )));
            }
        };
        let column = source_field
            .as_deref()
            .map(|field| {
                validate_value_field(field, schema, "aggregateable")?;
                let input = table.inputs.get(field).ok_or_else(|| {
                    invalid(format!("aggregate field has no physical column: {field}"))
                })?;
                table.table.column(&input.column).map_err(QueryError::from)
            })
            .transpose()?;
        if let Some(column) = &column {
            validate_aggregate_storage(function, column.storage())?;
        }
        let operand = ResolvedOperand::Aggregate {
            function,
            column,
            distinct: false,
        };
        projection.push(SelectedExpression {
            expression: operand.clone(),
            alias,
        });
        outputs.insert(
            name.clone(),
            AggregateOutput {
                operand,
                source_field,
            },
        );
        result_columns.push(name.clone());
    }
    if projection.is_empty() {
        return Err(invalid("aggregate: $group requires a field or accumulator"));
    }
    Ok((projection, group_by, outputs, Some(result_columns)))
}

fn resolve_having(
    value: &Value,
    schema: &Value,
    table: &ResolvedTable,
    outputs: &BTreeMap<String, AggregateOutput>,
    registration: &SqlRegistration,
) -> Result<ResolvedPredicate, QueryError> {
    mapping::validate_filter_budget(value)?;
    let fields = value
        .as_object()
        .ok_or_else(|| invalid("aggregate: $having must be an object"))?;
    let mut predicates = Vec::new();
    for (field, value) in fields {
        if field.starts_with('$') {
            let values = value
                .as_array()
                .ok_or_else(|| invalid(format!("aggregate: {field} must be an array")))?;
            let children = values
                .iter()
                .map(|value| resolve_having(value, schema, table, outputs, registration))
                .collect::<Result<Vec<_>, _>>()?;
            predicates.push(match field.as_str() {
                "$and" => ResolvedPredicate::and(children),
                "$or" => ResolvedPredicate::or(children),
                _ => return Err(invalid(format!("unsupported $having operator: {field}"))),
            });
            continue;
        }
        let output = if let Some(output) = outputs.get(field) {
            output.clone()
        } else {
            validate_value_field(field, schema, "groupable")?;
            let selected = read::selected(table, field)?;
            AggregateOutput {
                operand: selected.expression,
                source_field: Some(field.clone()),
            }
        };
        let conditions: Vec<(&str, &Value)> = match value {
            Value::Object(operators) if operators.keys().any(|key| key.starts_with('$')) => {
                operators
                    .iter()
                    .map(|(operator, value)| (operator.as_str(), value))
                    .collect()
            }
            value => vec![("$eq", value)],
        };
        for (operator, value) in conditions {
            let op = match operator {
                "$eq" => CompareOp::Eq,
                "$ne" => CompareOp::Ne,
                "$lt" => CompareOp::Lt,
                "$lte" => CompareOp::Lte,
                "$gt" => CompareOp::Gt,
                "$gte" => CompareOp::Gte,
                _ => return Err(invalid(format!("unsupported $having operator: {operator}"))),
            };
            if value.is_null() {
                if !matches!(op, CompareOp::Eq | CompareOp::Ne) {
                    return Err(invalid("null supports only equality in $having"));
                }
                predicates.push(ResolvedPredicate::IsNull {
                    operand: output.operand.clone(),
                    negated: op == CompareOp::Ne,
                });
                continue;
            }
            let storage = output.operand.storage()?;
            let mut value = value.clone();
            if let Some(field) = output.source_field.as_deref() {
                crate::sql::codecs::prepare_value(field, &schema[field], &mut value)
                    .map_err(|error| invalid(error.to_string()))?;
            }
            let value = registration.encode(storage, value)?;
            predicates.push(ResolvedPredicate::Compare {
                lhs: output.operand.clone(),
                op,
                rhs: ResolvedPredicateValue::Bind { storage, value },
            });
        }
    }
    Ok(ResolvedPredicate::and(predicates))
}

fn resolve_order(
    value: &Value,
    schema: &Value,
    table: &ResolvedTable,
    outputs: &BTreeMap<String, AggregateOutput>,
) -> Result<Vec<ResolvedOrder>, QueryError> {
    let entries: Vec<(&str, &Value)> = match value {
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
                    .ok_or_else(|| invalid("aggregate: $sort entries must be [field, dir]"))?;
                Ok((
                    pair[0]
                        .as_str()
                        .ok_or_else(|| invalid("aggregate: $sort field must be a string"))?,
                    &pair[1],
                ))
            })
            .collect::<Result<_, QueryError>>()?,
        _ => return Err(invalid("aggregate: $sort must be an object or array")),
    };
    entries
        .into_iter()
        .map(|(field, direction)| {
            let operand = if let Some(output) = outputs.get(field) {
                output.operand.clone()
            } else {
                validate_value_field(field, schema, "sortable")?;
                read::selected(table, field)?.expression
            };
            let descending = direction.as_i64().is_some_and(|value| value < 0);
            Ok(ResolvedOrder {
                expression: operand,
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

fn validate_value_field(field: &str, schema: &Value, capability: &str) -> Result<(), QueryError> {
    mapping::validate_field_name(field)?;
    if !crate::sql::descriptors::readable_fields(schema).contains(field) {
        return Err(QueryError::InvalidIdent(format!(
            "field '{field}' is not a readable schema field; readable fields must be declared in the schema"
        )));
    }
    mapping::validate_value_operation(field, schema)?;
    if schema[field][capability].as_bool() == Some(false) {
        return Err(invalid(format!("field '{field}' is not {capability}")));
    }
    Ok(())
}

fn validate_aggregate_storage(
    function: AggregateFunc,
    storage: StorageType,
) -> Result<(), QueryError> {
    let supported = match function {
        AggregateFunc::Sum | AggregateFunc::Avg => {
            matches!(storage, StorageType::Integer | StorageType::Real)
        }
        AggregateFunc::Min | AggregateFunc::Max => matches!(
            storage,
            StorageType::Integer | StorageType::Real | StorageType::Text | StorageType::Timestamp
        ),
        AggregateFunc::Count => true,
    };
    if supported {
        Ok(())
    } else {
        Err(invalid(
            "aggregate: accumulator has no portable column type",
        ))
    }
}

fn field_argument(operator: &str, value: &Value) -> Result<String, QueryError> {
    value
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| invalid(format!("aggregate: {operator} requires a field name")))
}

fn alias(name: &str) -> Result<Ident, QueryError> {
    Ident::parse_as(name, IdentRole::Alias)
        .map_err(crate::sql::compiler::CompileError::from)
        .map_err(Into::into)
}

fn invalid(message: impl Into<String>) -> QueryError {
    QueryError::InvalidFilter(message.into())
}
