use super::resolved::ResolvedTable;
use crate::{
    sql::{
        compile::{self, QueryError},
        predicate::{CompareOp, MembershipOp, PatternOp},
        registration::SqlRegistration,
        statement::{Column, ResolvedPredicate, StorageType},
    },
    value::Value,
};

pub(crate) fn resolve(
    filter: Value,
    schema: &Value,
    table: &ResolvedTable,
    registration: &SqlRegistration,
) -> Result<ResolvedPredicate, QueryError> {
    compile::validate_filter_budget(&filter)?;
    resolve_inner(filter, schema, table, registration)
}

fn resolve_inner(
    filter: Value,
    schema: &Value,
    table: &ResolvedTable,
    registration: &SqlRegistration,
) -> Result<ResolvedPredicate, QueryError> {
    let fields = match filter {
        Value::Null => return Ok(ResolvedPredicate::Const(true)),
        Value::Object(fields) => fields,
        _ => return Err(invalid("filter must be an object or null")),
    };
    let mut terms = Vec::new();
    for (field, value) in fields {
        if field.starts_with('$') {
            terms.push(match field.as_str() {
                "$and" | "$or" => {
                    let Value::Array(values) = value else {
                        return Err(invalid(format!("{field} must be an array")));
                    };
                    let mut children = values
                        .into_iter()
                        .map(|value| resolve_inner(value, schema, table, registration))
                        .collect::<Result<Vec<_>, _>>()?;
                    sort_predicates(&mut children);
                    if field == "$and" {
                        ResolvedPredicate::and(children)
                    } else {
                        ResolvedPredicate::or(children)
                    }
                }
                "$not" => ResolvedPredicate::Not(Box::new(resolve_inner(
                    value,
                    schema,
                    table,
                    registration,
                )?)),
                _ => return Err(invalid(format!("unsupported top-level operator: {field}"))),
            });
            continue;
        }
        compile::validate_field_name(&field)?;
        compile::validate_value_operation(&field, schema)?;
        let definition = schema
            .get(&field)
            .ok_or_else(|| invalid(format!("unknown filter field: {field}")))?;
        let input = table
            .inputs
            .get(&field)
            .ok_or_else(|| invalid(format!("filter field has no physical column: {field}")))?;
        let column = table.table.column(&input.column)?;
        match value {
            Value::Object(operators) if operators.keys().any(|key| key.starts_with('$')) => {
                for (operator, operand) in operators {
                    terms.push(condition(
                        column.clone(),
                        &field,
                        definition,
                        &operator,
                        operand,
                        registration,
                    )?);
                }
            }
            value => terms.push(condition(
                column,
                &field,
                definition,
                "$eq",
                value,
                registration,
            )?),
        }
    }
    sort_predicates(&mut terms);
    Ok(ResolvedPredicate::and(terms))
}

fn condition(
    column: Column,
    field: &str,
    definition: &Value,
    operator: &str,
    value: Value,
    registration: &SqlRegistration,
) -> Result<ResolvedPredicate, QueryError> {
    let comparison = match operator {
        "$eq" => Some(CompareOp::Eq),
        "$ne" => Some(CompareOp::Ne),
        "$lt" => Some(CompareOp::Lt),
        "$lte" => Some(CompareOp::Lte),
        "$gt" => Some(CompareOp::Gt),
        "$gte" => Some(CompareOp::Gte),
        _ => None,
    };
    if let Some(op) = comparison {
        if value.is_null() {
            if !matches!(op, CompareOp::Eq | CompareOp::Ne) {
                return Err(invalid(
                    "null supports only equality and inequality comparisons",
                ));
            }
            return Ok(ResolvedPredicate::IsNull {
                column,
                negated: op == CompareOp::Ne,
            });
        }
        return Ok(ResolvedPredicate::Compare {
            value: encode(field, definition, column.storage(), value, registration)?,
            column,
            op,
        });
    }
    match operator {
        "$in" | "$nin" => {
            let Value::Array(values) = value else {
                return Err(invalid(format!("{operator} must be an array")));
            };
            if values.len() > crate::sql::MAX_MEMBERSHIP_LIST_LEN {
                return Err(invalid(format!(
                    "{operator} exceeds the maximum of {} values",
                    crate::sql::MAX_MEMBERSHIP_LIST_LEN
                )));
            }
            let negated = operator == "$nin";
            let saw_null = values.iter().any(Value::is_null);
            let values = values
                .into_iter()
                .filter(|value| !value.is_null())
                .map(|value| encode(field, definition, column.storage(), value, registration))
                .collect::<Result<Vec<_>, _>>()?;
            if values.is_empty() {
                return Ok(if saw_null {
                    ResolvedPredicate::IsNull { column, negated }
                } else {
                    ResolvedPredicate::Const(negated)
                });
            }
            let membership = ResolvedPredicate::Membership {
                column: column.clone(),
                op: if negated {
                    MembershipOp::NotIn
                } else {
                    MembershipOp::In
                },
                values,
            };
            if !saw_null {
                return Ok(membership);
            }
            let null = ResolvedPredicate::IsNull { column, negated };
            Ok(if negated {
                ResolvedPredicate::and(vec![membership, null])
            } else {
                ResolvedPredicate::or(vec![membership, null])
            })
        }
        "$exists" => Ok(ResolvedPredicate::IsNull {
            column,
            negated: value
                .as_bool()
                .ok_or_else(|| invalid("$exists must be a boolean"))?,
        }),
        "$like" | "$ilike" => {
            let value = value
                .as_str()
                .ok_or_else(|| invalid(format!("{operator} must be a string")))?
                .to_owned();
            if value.contains('\0') {
                return Err(invalid("a LIKE pattern must not contain a NUL byte"));
            }
            Ok(ResolvedPredicate::Pattern {
                column,
                op: if operator == "$like" {
                    PatternOp::Like
                } else {
                    PatternOp::ILike
                },
                value,
            })
        }
        _ => Err(invalid(format!("unsupported operator: {operator}"))),
    }
}

fn encode(
    field: &str,
    definition: &Value,
    storage: StorageType,
    mut value: Value,
    registration: &SqlRegistration,
) -> Result<Value, QueryError> {
    crate::sql::codecs::prepare_value(field, definition, &mut value)
        .map_err(|error| invalid(error.to_string()))?;
    registration.encode(storage, value).map_err(Into::into)
}

fn sort_predicates(predicates: &mut [ResolvedPredicate]) {
    predicates.sort_by_key(shape);
}

fn shape(predicate: &ResolvedPredicate) -> String {
    match predicate {
        ResolvedPredicate::And(children) => format!(
            "and({})",
            children.iter().map(shape).collect::<Vec<_>>().join(",")
        ),
        ResolvedPredicate::Or(children) => format!(
            "or({})",
            children.iter().map(shape).collect::<Vec<_>>().join(",")
        ),
        ResolvedPredicate::Not(child) => format!("not({})", shape(child)),
        ResolvedPredicate::Compare { column, op, .. } => {
            format!("compare({},{op:?})", column.name().as_str())
        }
        ResolvedPredicate::Membership {
            column, op, values, ..
        } => format!(
            "membership({},{op:?},{})",
            column.name().as_str(),
            values.len()
        ),
        ResolvedPredicate::Pattern { column, op, .. } => {
            format!("pattern({},{op:?})", column.name().as_str())
        }
        ResolvedPredicate::IsNull { column, negated } => {
            format!("null({},{negated})", column.name().as_str())
        }
        ResolvedPredicate::Const(value) => format!("const({value})"),
    }
}

fn invalid(message: impl Into<String>) -> QueryError {
    QueryError::InvalidFilter(message.into())
}
