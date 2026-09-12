use super::resolved::ResolvedTable;
use crate::{
    sql::{
        mapping::{self, QueryError},
        predicate::{CompareOp, MembershipOp, PatternOp},
        registration::SqlRegistration,
        statement::{
            Column, ResolvedOperand, ResolvedPredicate, ResolvedPredicateValue, StorageType,
        },
    },
    value::Value,
};

#[derive(Clone, Debug)]
pub(crate) enum Input {
    Dynamic(Value),
    Model(crate::orm::ModelPredicate),
}

impl From<Value> for Input {
    fn from(value: Value) -> Self {
        Self::Dynamic(value)
    }
}

impl From<crate::orm::ModelPredicate> for Input {
    fn from(value: crate::orm::ModelPredicate) -> Self {
        Self::Model(value)
    }
}

impl Input {
    pub(crate) fn resolve(
        self,
        schema: &Value,
        table: &ResolvedTable,
        registration: &SqlRegistration,
    ) -> Result<ResolvedPredicate, QueryError> {
        match self {
            Self::Dynamic(value) => resolve(value, schema, table, registration),
            Self::Model(value) => resolve_model(value, schema, table, registration),
        }
    }

    pub(crate) fn dynamic(&self) -> Option<&Value> {
        match self {
            Self::Dynamic(value) => Some(value),
            Self::Model(_) => None,
        }
    }

    pub(crate) fn conjunctive_value(&self, field: &str) -> Option<&Value> {
        match self {
            Self::Dynamic(value) => dynamic_equality(value, field),
            Self::Model(value) => model_equality(value, field),
        }
    }

    pub(crate) fn has_non_null_equality(&self, field: &str) -> bool {
        self.conjunctive_value(field)
            .is_some_and(|value| !value.is_null())
    }
}

fn dynamic_equality<'a>(filter: &'a Value, field: &str) -> Option<&'a Value> {
    let mut value = filter.as_object()?.get(field)?;
    if let Some(operators) = value.as_object() {
        if operators.len() != 1 {
            return None;
        }
        value = operators.get("$eq")?;
    }
    Some(value)
}

fn model_equality<'a>(predicate: &'a crate::orm::ModelPredicate, field: &str) -> Option<&'a Value> {
    match predicate {
        crate::orm::ModelPredicate::Compare {
            field: candidate,
            op: CompareOp::Eq,
            value,
        } if *candidate == field => Some(value),
        crate::orm::ModelPredicate::And(children) => children
            .iter()
            .find_map(|child| model_equality(child, field)),
        _ => None,
    }
}

pub(crate) fn resolve(
    filter: Value,
    schema: &Value,
    table: &ResolvedTable,
    registration: &SqlRegistration,
) -> Result<ResolvedPredicate, QueryError> {
    mapping::validate_filter_budget(&filter)?;
    resolve_inner(filter, schema, table, registration)
}

pub(crate) fn resolve_model(
    predicate: crate::orm::ModelPredicate,
    schema: &Value,
    table: &ResolvedTable,
    registration: &SqlRegistration,
) -> Result<ResolvedPredicate, QueryError> {
    use crate::orm::ModelPredicate;
    Ok(match predicate {
        ModelPredicate::And(children) => ResolvedPredicate::and(
            children
                .into_iter()
                .map(|child| resolve_model(child, schema, table, registration))
                .collect::<Result<_, _>>()?,
        ),
        ModelPredicate::Or(children) => ResolvedPredicate::or(
            children
                .into_iter()
                .map(|child| resolve_model(child, schema, table, registration))
                .collect::<Result<_, _>>()?,
        ),
        ModelPredicate::Const(value) => ResolvedPredicate::Const(value),
        ModelPredicate::Compare {
            field,
            op,
            mut value,
        } => {
            mapping::validate_field_name(field)?;
            mapping::validate_value_operation(field, schema)?;
            let definition = schema
                .get(field)
                .ok_or_else(|| invalid(format!("unknown filter field: {field}")))?;
            if definition["filterable"].as_bool() == Some(false) {
                return Err(invalid(format!("field '{field}' is not filterable")));
            }
            let input = table
                .inputs
                .get(field)
                .ok_or_else(|| invalid(format!("filter field has no physical column: {field}")))?;
            let column = table.table.column(&input.column)?;
            if value.is_null() {
                if !matches!(op, CompareOp::Eq | CompareOp::Ne) {
                    return Err(invalid("null supports only equality comparisons"));
                }
                ResolvedPredicate::IsNull {
                    operand: ResolvedOperand::Column(column),
                    negated: op == CompareOp::Ne,
                }
            } else {
                validate_comparison(definition, op)?;
                crate::sql::codecs::prepare_value(field, definition, &mut value)
                    .map_err(|error| invalid(error.to_string()))?;
                let storage = column.storage();
                let value = registration.encode(storage, value)?;
                ResolvedPredicate::Compare {
                    lhs: ResolvedOperand::Column(column),
                    op,
                    rhs: ResolvedPredicateValue::Bind { storage, value },
                }
            }
        }
    })
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
        mapping::validate_field_name(&field)?;
        mapping::validate_value_operation(&field, schema)?;
        let definition = schema
            .get(&field)
            .ok_or_else(|| invalid(format!("unknown filter field: {field}")))?;
        if definition["filterable"].as_bool() == Some(false) {
            return Err(invalid(format!("field '{field}' is not filterable")));
        }
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
                operand: ResolvedOperand::Column(column),
                negated: op == CompareOp::Ne,
            });
        }
        validate_comparison(definition, op)?;
        return Ok(ResolvedPredicate::Compare {
            rhs: ResolvedPredicateValue::Bind {
                storage: column.storage(),
                value: encode(field, definition, column.storage(), value, registration)?,
            },
            lhs: ResolvedOperand::Column(column),
            op,
        });
    }
    match operator {
        "$in" | "$nin" => {
            validate_equality(definition)?;
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
                    ResolvedPredicate::IsNull {
                        operand: ResolvedOperand::Column(column),
                        negated,
                    }
                } else {
                    ResolvedPredicate::Const(negated)
                });
            }
            let membership = ResolvedPredicate::Membership {
                lhs: ResolvedOperand::Column(column.clone()),
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
            let null = ResolvedPredicate::IsNull {
                operand: ResolvedOperand::Column(column),
                negated,
            };
            Ok(if negated {
                ResolvedPredicate::and(vec![membership, null])
            } else {
                ResolvedPredicate::or(vec![membership, null])
            })
        }
        "$exists" => Ok(ResolvedPredicate::IsNull {
            operand: ResolvedOperand::Column(column),
            negated: value
                .as_bool()
                .ok_or_else(|| invalid("$exists must be a boolean"))?,
        }),
        "$like" | "$ilike" => {
            validate_pattern(definition)?;
            let value = value
                .as_str()
                .ok_or_else(|| invalid(format!("{operator} must be a string")))?
                .to_owned();
            if value.contains('\0') {
                return Err(invalid("a LIKE pattern must not contain a NUL byte"));
            }
            Ok(ResolvedPredicate::Pattern {
                lhs: ResolvedOperand::Column(column),
                op: if operator == "$like" {
                    PatternOp::Like
                } else {
                    PatternOp::ILike
                },
                value,
                escape: None,
            })
        }
        _ => Err(invalid(format!("unsupported operator: {operator}"))),
    }
}

fn validate_comparison(definition: &Value, op: CompareOp) -> Result<(), QueryError> {
    use crate::sql::descriptors::{supports_predicate_operator, PredicateOperator};
    let operator = if matches!(op, CompareOp::Eq | CompareOp::Ne) {
        PredicateOperator::Equality
    } else {
        PredicateOperator::Ordering
    };
    if supports_predicate_operator(definition, operator) {
        Ok(())
    } else {
        Err(invalid(
            "comparison operator is not supported for this field type",
        ))
    }
}

fn validate_equality(definition: &Value) -> Result<(), QueryError> {
    use crate::sql::descriptors::{supports_predicate_operator, PredicateOperator};
    supports_predicate_operator(definition, PredicateOperator::Equality)
        .then_some(())
        .ok_or_else(|| invalid("ordinary equality is not supported for this field type"))
}

fn validate_pattern(definition: &Value) -> Result<(), QueryError> {
    use crate::sql::descriptors::{supports_predicate_operator, PredicateOperator};
    supports_predicate_operator(definition, PredicateOperator::Pattern)
        .then_some(())
        .ok_or_else(|| invalid("pattern operator requires a text field"))
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
    predicates.sort_by_cached_key(shape);
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
        ResolvedPredicate::Compare { lhs, op, .. } => {
            format!("compare({},{op:?})", operand_shape(lhs))
        }
        ResolvedPredicate::Membership {
            lhs, op, values, ..
        } => format!("membership({},{op:?},{})", operand_shape(lhs), values.len()),
        ResolvedPredicate::Pattern { lhs, op, .. } => {
            format!("pattern({},{op:?})", operand_shape(lhs))
        }
        ResolvedPredicate::IsNull { operand, negated } => {
            format!("null({},{negated})", operand_shape(operand))
        }
        ResolvedPredicate::Const(value) => format!("const({value})"),
    }
}

fn operand_shape(operand: &ResolvedOperand) -> String {
    match operand {
        ResolvedOperand::Column(column) => column.name().as_str().to_owned(),
        ResolvedOperand::Aggregate {
            function,
            column,
            distinct,
        } => format!(
            "aggregate({function:?},{},{distinct})",
            column.as_ref().map_or("*", |column| column.name().as_str())
        ),
    }
}

fn invalid(message: impl Into<String>) -> QueryError {
    QueryError::InvalidFilter(message.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::SchemaName;

    fn resolves(definition: Value, filter: Value) -> Result<ResolvedPredicate, QueryError> {
        let schema = crate::value!({
            "id": {"type":"string", "required":true, "primaryKey":true},
            "field": definition,
        });
        let registration = SqlRegistration::sqlite();
        let table = ResolvedTable::new(
            &SchemaName::new("main").unwrap(),
            "records",
            &schema,
            &registration,
        )?;
        resolve(filter, &schema, &table, &registration)
    }

    #[test]
    fn dynamic_filters_enforce_the_portable_operator_matrix() {
        for (definition, filter) in [
            (
                crate::value!({"type":"boolean"}),
                crate::value!({"field":{"$gt":false}}),
            ),
            (
                crate::value!({"type":"number", "precision":18, "scale":2}),
                crate::value!({"field":{"$lt":"10.5"}}),
            ),
            (
                crate::value!({"type":"json"}),
                crate::value!({"field":{"$gte":{"key":true}}}),
            ),
            (
                crate::value!({"type":"bytes"}),
                crate::value!({"field":{"$lt":[1,2]}}),
            ),
            (
                crate::value!({"type":"vector", "vectorDims":2}),
                crate::value!({"field":{"$eq":[1,2]}}),
            ),
            (
                crate::value!({"type":"geoPoint"}),
                crate::value!({"field":{"$in":[{"lat":1,"lng":2}]}}),
            ),
            (
                crate::value!({"type":"number", "mask":{"kind":"full"}}),
                crate::value!({"field":{"$gt":10}}),
            ),
        ] {
            assert!(resolves(definition, filter).is_err());
        }

        assert!(resolves(
            crate::value!({"type":"json"}),
            crate::value!({"field":{"$eq":{"key":true}}}),
        )
        .is_ok());
        assert!(resolves(
            crate::value!({"type":"calendarDate"}),
            crate::value!({"field":{"$gte":"2026-01-01"}}),
        )
        .is_ok());
        assert!(resolves(
            crate::value!({"type":"string"}),
            crate::value!({"field":{"$like":"prefix%"}}),
        )
        .is_ok());
    }
}
