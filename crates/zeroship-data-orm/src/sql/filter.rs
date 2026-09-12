//! Decode the SDK filter vocabulary into the typed predicate grammar.

use crate::sql::compile::QueryError;
use crate::sql::{
    CompareOp, Ident, IdentRole, Literal, MembershipOp, Operand, PatternOp, Predicate, TextPattern,
};
use crate::value::Value;

fn invalid(message: impl Into<String>) -> QueryError {
    QueryError::InvalidFilter(message.into())
}

/// Parse a filter before any SQL is emitted. The budget walk runs before the
/// recursive decoder, including for filters supplied directly by native callers.
pub fn decode(value: &Value) -> Result<Predicate, QueryError> {
    crate::sql::compile::validate_filter_budget(value)?;
    decode_inner(value)
}

fn decode_inner(value: &Value) -> Result<Predicate, QueryError> {
    let fields = match value {
        Value::Null => return Ok(Predicate::always()),
        Value::Object(fields) => fields,
        _ => return Err(invalid("filter must be an object or null")),
    };
    let mut terms = Vec::new();
    for (field, value) in fields {
        if field.starts_with('$') {
            terms.push(match field.as_str() {
                "$and" | "$or" => {
                    let values = value
                        .as_array()
                        .ok_or_else(|| invalid(format!("{field} must be an array")))?;
                    let children = values
                        .iter()
                        .map(decode_inner)
                        .collect::<Result<Vec<_>, _>>()?;
                    if field == "$and" {
                        Predicate::And(children)
                    } else {
                        Predicate::Or(children)
                    }
                }
                "$not" => Predicate::Not(Box::new(decode_inner(value)?)),
                _ => return Err(invalid(format!("unsupported top-level operator: {field}"))),
            });
            continue;
        }
        // Keep the SDK's error vocabulary at the decoding boundary.
        crate::sql::compile::validate_field_name(field)?;
        let field =
            Ident::parse_as(field, IdentRole::Column).map_err(|e| invalid(e.to_string()))?;
        let operand = Operand::column(field);
        match value {
            Value::Object(operators) if operators.keys().any(|k| k.starts_with('$')) => {
                for (operator, value) in operators {
                    terms.push(condition(operand.clone(), operator, value)?);
                }
            }
            _ => terms.push(condition(operand, "$eq", value)?),
        }
    }
    Ok(Predicate::And(terms))
}

fn condition(lhs: Operand, operator: &str, value: &Value) -> Result<Predicate, QueryError> {
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
        return match literal(value)? {
            Some(value) => Ok(Predicate::Compare {
                lhs,
                op,
                rhs: Operand::Lit(value),
            }),
            None if matches!(op, CompareOp::Eq | CompareOp::Ne) => Ok(Predicate::IsNull {
                operand: lhs,
                negated: op == CompareOp::Ne,
            }),
            None => Err(invalid(
                "null supports only equality and inequality comparisons",
            )),
        };
    }
    match operator {
        "$in" | "$nin" => {
            let values = value
                .as_array()
                .ok_or_else(|| invalid(format!("{operator} must be an array")))?;
            if values.len() > crate::sql::MAX_MEMBERSHIP_LIST_LEN {
                return Err(invalid(format!(
                    "{operator} exceeds the maximum of {} values",
                    crate::sql::MAX_MEMBERSHIP_LIST_LEN
                )));
            }
            let values = values.iter().map(literal).collect::<Result<Vec<_>, _>>()?;
            Predicate::membership(
                lhs,
                if operator == "$in" {
                    MembershipOp::In
                } else {
                    MembershipOp::NotIn
                },
                values,
            )
            .map_err(|e| invalid(e.to_string()))
        }
        "$exists" => Ok(Predicate::IsNull {
            operand: lhs,
            negated: value
                .as_bool()
                .ok_or_else(|| invalid("$exists must be a boolean"))?,
        }),
        "$like" | "$ilike" => Ok(Predicate::Pattern {
            lhs,
            op: if operator == "$like" {
                PatternOp::Like
            } else {
                PatternOp::ILike
            },
            pattern: TextPattern::new(
                value
                    .as_str()
                    .ok_or_else(|| invalid(format!("{operator} must be a string")))?,
            )
            .map_err(|e| invalid(e.to_string()))?,
            escape: None,
        }),
        _ => Err(invalid(format!("unsupported operator: {operator}"))),
    }
}

/// JSON containers remain typed text binds, preserving PostgreSQL's contextual
/// JSON conversion. A null has no literal representation.
fn literal(value: &Value) -> Result<Option<Literal>, QueryError> {
    Literal::try_from_value(value.clone()).map_err(|error| invalid(error.to_string()))
}
