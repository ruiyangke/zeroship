//! Resolved column roles and SQL assignments supplied by the ORM.

use crate::schema::{ColumnSchema, FieldMap};
use crate::sql::mapping::QueryError;
use crate::value::Value;

fn invalid(message: &str) -> QueryError {
    QueryError::InvalidFilter(message.into())
}

pub fn soft_delete_column(schema: &FieldMap) -> Result<Option<&str>, QueryError> {
    role(schema, |column| column.soft_delete)
}

pub fn concurrency_column(schema: &FieldMap) -> Result<Option<&str>, QueryError> {
    role(schema, |column| column.concurrency)
}

fn role(
    schema: &FieldMap,
    matches: impl Fn(&ColumnSchema) -> bool,
) -> Result<Option<&str>, QueryError> {
    let mut columns = schema
        .iter()
        .filter(|(_, column)| matches(column))
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
        let mut removed = ColumnSchema::new(crate::schema::LogicalType::Timestamp);
        removed.soft_delete = true;
        let mut revision = ColumnSchema::new(crate::schema::LogicalType::Integer);
        revision.concurrency = true;
        let schema: FieldMap = [
            ("removed_on".into(), removed),
            ("revision".into(), revision),
            (
                "id".into(),
                ColumnSchema::new(crate::schema::LogicalType::Text),
            ),
            (
                "deleted_at".into(),
                ColumnSchema::new(crate::schema::LogicalType::Text),
            ),
            (
                "version".into(),
                ColumnSchema::new(crate::schema::LogicalType::Text),
            ),
        ]
        .into();
        assert_eq!(soft_delete_column(&schema).unwrap(), Some("removed_on"));
        assert_eq!(concurrency_column(&schema).unwrap(), Some("revision"));
        assert_eq!(
            soft_delete_column(
                &[(
                    "deleted_at".into(),
                    ColumnSchema::new(crate::schema::LogicalType::Text)
                )]
                .into()
            )
            .unwrap(),
            None
        );
    }

    #[test]
    fn ambiguous_roles_are_refused() {
        let mut assigned = ColumnSchema::new(crate::schema::LogicalType::Timestamp);
        assigned.soft_delete = true;
        let fields = [
            ("first".into(), assigned.clone()),
            ("second".into(), assigned),
        ]
        .into();
        assert!(soft_delete_column(&fields).is_err());
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
