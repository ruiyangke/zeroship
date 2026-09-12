//! Resolved column roles and SQL assignments supplied by the ORM.

use crate::sql::compile::QueryError;
use crate::value::Value;

fn invalid(message: &str) -> QueryError {
    QueryError::InvalidFilter(message.into())
}

pub fn soft_delete_column(schema: &Value) -> Result<Option<&str>, QueryError> {
    role(schema, "softDelete")
}

pub fn concurrency_column(schema: &Value) -> Result<Option<&str>, QueryError> {
    role(schema, "concurrency")
}

fn role<'a>(schema: &'a Value, name: &str) -> Result<Option<&'a str>, QueryError> {
    let mut columns = schema
        .as_object()
        .into_iter()
        .flat_map(|fields| fields.iter())
        .filter(|(_, def)| def.get(name).and_then(Value::as_bool) == Some(true))
        .map(|(name, _)| name.as_str());
    let column = columns.next();
    if columns.next().is_some() {
        return Err(invalid("operation requires an unambiguous column role"));
    }
    Ok(column)
}

#[derive(Debug, Clone)]
pub enum AssignedValue {
    CurrentTimestamp,
    Increment(i64),
    Bound(Value),
}

#[derive(Debug, Clone)]
pub struct ColumnAssignment {
    pub column: String,
    pub value: AssignedValue,
}

#[derive(Debug, Clone, Default)]
pub struct WriteAssignments {
    pub columns: Vec<ColumnAssignment>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value;

    #[test]
    fn roles_follow_metadata_and_never_column_names() {
        let schema = value!({
            "removed_on":{"softDelete":true},
            "revision":{"concurrency":true}, "id":{}, "deleted_at":{}, "version":{}
        });
        assert_eq!(soft_delete_column(&schema).unwrap(), Some("removed_on"));
        assert_eq!(concurrency_column(&schema).unwrap(), Some("revision"));
        assert_eq!(
            soft_delete_column(&value!({"deleted_at":{}})).unwrap(),
            None
        );
    }

    #[test]
    fn assignments_preserve_their_resolved_values() {
        let assignments = WriteAssignments {
            columns: vec![
                ColumnAssignment {
                    column: "revision".into(),
                    value: AssignedValue::Increment(2),
                },
                ColumnAssignment {
                    column: "editor".into(),
                    value: AssignedValue::Bound(value!("actor'")),
                },
            ],
        };
        assert_eq!(assignments.columns[0].column, "revision");
        assert!(matches!(
            assignments.columns[0].value,
            AssignedValue::Increment(2)
        ));
        assert_eq!(assignments.columns[1].column, "editor");
        assert!(matches!(
            &assignments.columns[1].value,
            AssignedValue::Bound(value) if value == &value!("actor'")
        ));
    }
}
