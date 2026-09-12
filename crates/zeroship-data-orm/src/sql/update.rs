//! Shared update grammar. Assignments remain native values throughout parsing.
use crate::sql::codecs::CodecError;
use crate::value::{Record, Value};
use std::collections::HashSet;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Operator {
    Set,
    Increment,
    Decrement,
    Multiply,
    Push,
    Pull,
    AddToSet,
}

impl Operator {
    pub(crate) fn parse(name: &str) -> Result<Self, CodecError> {
        match name {
            "$set" => Ok(Self::Set),
            "$inc" => Ok(Self::Increment),
            "$dec" => Ok(Self::Decrement),
            "$mul" => Ok(Self::Multiply),
            "$push" => Ok(Self::Push),
            "$pull" => Ok(Self::Pull),
            "$addToSet" => Ok(Self::AddToSet),
            _ => Err(invalid(&format!("unsupported update operator: {name}"))),
        }
    }

    /// Check the logical field type independently of storage protection.
    ///
    /// # Errors
    /// Refuses an operator unsupported by the declared field type.
    pub fn validate_type(self, field: &str, definition: &Value) -> Result<(), CodecError> {
        use Operator::*;
        let kind = definition["type"].as_str();
        let supported = match self {
            Set => true,
            Increment | Decrement | Multiply => matches!(
                kind,
                Some("int" | "integer" | "bigInt" | "number" | "float" | "decimal" | "numeric")
            ),
            Push | Pull | AddToSet => kind == Some("array"),
        };
        if !supported {
            let code = match kind {
                Some("calendarDate") => "invalid_calendar_date_operation",
                Some("date" | "timestamp") => "invalid_timestamp_operation",
                Some("array")
                    if matches!(
                        definition["items"].as_str(),
                        Some("calendarDate" | "date" | "timestamp")
                    ) =>
                {
                    "invalid_temporal_operation"
                }
                _ => "invalid_update_operation",
            };
            return Err(CodecError::validation(
                code,
                format!(
                    "operation '{}' is not supported for column '{}' with type '{}'",
                    self.name(),
                    field,
                    kind.unwrap_or("undeclared")
                ),
            ));
        }
        Ok(())
    }

    pub const fn name(self) -> &'static str {
        match self {
            Self::Set => "$set",
            Self::Increment => "$inc",
            Self::Decrement => "$dec",
            Self::Multiply => "$mul",
            Self::Push => "$push",
            Self::Pull => "$pull",
            Self::AddToSet => "$addToSet",
        }
    }
}

#[derive(Debug)]
pub struct Assignment<'a> {
    pub field: &'a str,
    pub operator: Operator,
    pub operand: &'a Value,
}

#[derive(Debug)]
pub(crate) struct OwnedAssignment {
    pub(crate) field: String,
    pub(crate) operator: Operator,
    pub(crate) operand: Value,
}

fn invalid(message: &str) -> CodecError {
    CodecError::validation("invalid_update", message)
}

fn field_operator(value: &Value) -> Result<Operator, CodecError> {
    if let Some(fields) = value.as_object() {
        if let Some(name) = fields.keys().find(|name| name.starts_with('$')) {
            if fields.len() != 1 {
                return Err(invalid("a field update must contain exactly one operator"));
            }
            return Operator::parse(name);
        }
    }
    Ok(Operator::Set)
}

fn numeric_operand(value: &Value) -> bool {
    match value {
        Value::Number(_) => true,
        Value::Decimal(text) => serde_json::from_str::<&serde_json::value::RawValue>(text)
            .is_ok_and(|raw| {
                raw.get()
                    .starts_with(|c: char| c == '-' || c.is_ascii_digit())
            }),
        _ => false,
    }
}

/// Parse both document operators and per-field operators without copying values.
/// Explicit `$set` operands are literal data, including objects with `$` keys.
///
/// # Errors
/// Refuses malformed patches, duplicate fields, and nonnumeric arithmetic operands.
pub fn assignments(patch: &Value) -> Result<Vec<Assignment<'_>>, CodecError> {
    let fields = patch
        .as_object()
        .ok_or_else(|| invalid("update must be an object"))?;
    let mut seen = HashSet::new();
    let mut result = Vec::new();
    let mut add = |field, operator, operand| {
        if !seen.insert(field) {
            return Err(invalid("a field may be assigned only once per update"));
        }
        if matches!(
            operator,
            Operator::Increment | Operator::Decrement | Operator::Multiply
        ) && !numeric_operand(operand)
        {
            return Err(CodecError::validation(
                "invalid_arithmetic_operand",
                format!("arithmetic update for '{field}' requires a native number or decimal"),
            ));
        }
        result.push(Assignment {
            field,
            operator,
            operand,
        });
        Ok(())
    };
    for (field, value) in fields {
        if field.starts_with('$') {
            let operator = Operator::parse(field)?;
            let operands = value
                .as_object()
                .ok_or_else(|| invalid(&format!("{field} requires an object")))?;
            for (field, operand) in operands {
                if field.starts_with('$') {
                    return Err(invalid("update field names cannot begin with '$'"));
                }
                add(field.as_str(), operator, operand)?;
            }
        } else {
            let operator = field_operator(value)?;
            let operand = value
                .as_object()
                .filter(|object| object.keys().any(|key| key.starts_with('$')))
                .map_or(value, |object| &object[operator.name()]);
            add(field.as_str(), operator, operand)?;
        }
    }
    if result.is_empty() {
        return Err(invalid("update fields cannot be empty"));
    }
    Ok(result)
}

/// Gather literal assignments under `$set` before protection transforms.
/// Moves operands into the canonical patch without serializing or cloning them.
///
/// # Errors
/// Refuses invalid updates without modifying the supplied patch.
pub fn normalize(patch: &mut Value) -> Result<(), CodecError> {
    assignments(patch)?;
    let Value::Object(fields) = patch.take() else {
        unreachable!()
    };
    let mut sets = Record::new();
    let mut operations = Record::new();
    let mut add = |field, operator: Operator, operand| {
        if operator == Operator::Set {
            sets.insert(field, operand);
        } else {
            operations.insert(
                field,
                Value::Object([(operator.name().into(), operand)].into()),
            );
        }
    };
    for (field, value) in fields {
        if field.starts_with('$') {
            let operator = Operator::parse(&field).expect("validated operator");
            let Value::Object(operands) = value else {
                unreachable!()
            };
            for (field, operand) in operands {
                add(field, operator, operand);
            }
        } else if value
            .as_object()
            .is_some_and(|object| object.keys().any(|key| key.starts_with('$')))
        {
            let Value::Object(mut object) = value else {
                unreachable!()
            };
            let (name, operand) = object.pop().expect("validated operator");
            add(
                field,
                Operator::parse(&name).expect("validated operator"),
                operand,
            );
        } else {
            add(field, Operator::Set, value);
        }
    }
    if !sets.is_empty() {
        operations.insert("$set".into(), Value::Object(sets));
    }
    *patch = Value::Object(operations);
    Ok(())
}

pub(crate) fn into_assignments(mut patch: Value) -> Result<Vec<OwnedAssignment>, CodecError> {
    normalize(&mut patch)?;
    let Value::Object(fields) = patch else {
        unreachable!("normalized update is an object")
    };
    let mut assignments = Vec::new();
    for (field, value) in fields {
        if field == "$set" {
            let Value::Object(values) = value else {
                unreachable!("normalized $set is an object")
            };
            assignments.extend(values.into_iter().map(|(field, operand)| OwnedAssignment {
                field,
                operator: Operator::Set,
                operand,
            }));
            continue;
        }
        let Value::Object(mut operation) = value else {
            unreachable!("normalized field operation is an object")
        };
        let (operator, operand) = operation.pop().expect("normalized field operation");
        assignments.push(OwnedAssignment {
            field,
            operator: Operator::parse(&operator).expect("normalized operator"),
            operand,
        });
    }
    Ok(assignments)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value;

    #[test]
    fn normalization_moves_buffers_and_keeps_explicit_set_data_literal() {
        let bytes = vec![1, 2, 3];
        let address = bytes.as_ptr();
        let mut patch = value!({"payload":{"$set":{"$inc":2}},"$mul":{"balance":3}});
        patch
            .as_object_mut()
            .unwrap()
            .insert("bytes".into(), Value::Bytes(bytes));
        normalize(&mut patch).unwrap();
        assert_eq!(patch["$set"]["bytes"].as_bytes().unwrap().as_ptr(), address);
        assert_eq!(patch["$set"]["payload"], value!({"$inc":2}));
        assert_eq!(patch["balance"], value!({"$mul":3}));
        let expected = patch.clone();
        normalize(&mut patch).unwrap();
        assert_eq!(patch, expected);
        assert_eq!(patch["$set"]["bytes"].as_bytes().unwrap().as_ptr(), address);
    }

    #[test]
    fn invalid_updates_do_not_change_the_supplied_patch() {
        for mut patch in [
            value!(null),
            value!([]),
            value!({}),
            value!({"$set":{}}),
            value!({"x":{"$inc":1,"$mul":2}}),
            value!({"x":{"$inc":1,"data":2}}),
            value!({"x":1,"$set":{"x":2}}),
            value!({"$mul":{"x":1},"$inc":{"x":2}}),
            value!({"$unknown":{"x":1}}),
            value!({"$set":{"$inc":1}}),
        ] {
            let before = patch.clone();
            assert!(normalize(&mut patch).is_err(), "{patch:?}");
            assert_eq!(patch, before);
        }
    }
}
