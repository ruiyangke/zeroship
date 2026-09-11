//! Resolved column roles and SQL assignments supplied by the ORM.

use crate::{
    compile::{QueryError, SqlDialect, quote_ident},
    value::Value,
};

fn invalid(message: &str) -> QueryError {
    QueryError::InvalidFilter(message.into())
}

pub fn primary_key(schema: &Value) -> Result<&str, QueryError> {
    primary_key_column(schema)?.ok_or_else(|| invalid("operation requires a declared primary key"))
}

pub fn primary_key_column(schema: &Value) -> Result<Option<&str>, QueryError> {
    role(schema, "primaryKey")
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

impl WriteAssignments {
    pub fn render(
        &self,
        dialect: SqlDialect,
        params: &mut Vec<Value>,
        source: Option<&str>,
    ) -> Result<Vec<String>, QueryError> {
        self.columns
            .iter()
            .map(|assignment| {
                crate::compile::validate_field_name(&assignment.column)?;
                let column = quote_ident(&assignment.column);
                let value = match &assignment.value {
                    AssignedValue::CurrentTimestamp => dialect.current_timestamp_expr().into(),
                    AssignedValue::Increment(step) => {
                        let operand = source
                            .map_or_else(|| column.clone(), |source| format!("{source}.{column}"));
                        params.push(Value::from(*step));
                        format!("{operand} + ${}", params.len())
                    }
                    AssignedValue::Bound(value) => {
                        params.push(value.clone());
                        format!("${}", params.len())
                    }
                };
                Ok(format!("{column} = {value}"))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value;

    #[test]
    fn roles_follow_metadata_and_never_column_names() {
        let schema = value!({
            "row_key":{"primaryKey":true}, "removed_on":{"softDelete":true},
            "revision":{"concurrency":true}, "id":{}, "deleted_at":{}, "version":{}
        });
        assert_eq!(primary_key(&schema).unwrap(), "row_key");
        assert_eq!(soft_delete_column(&schema).unwrap(), Some("removed_on"));
        assert_eq!(concurrency_column(&schema).unwrap(), Some("revision"));
        assert!(primary_key(&value!({"id":{}})).is_err());
        assert_eq!(
            soft_delete_column(&value!({"deleted_at":{}})).unwrap(),
            None
        );
    }

    #[test]
    fn assignments_keep_values_bound_and_qualify_conflict_operands() {
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
        for dialect in [SqlDialect::Postgres, SqlDialect::Sqlite] {
            let mut params = Vec::new();
            let sql = assignments
                .render(dialect, &mut params, Some("\"records\""))
                .unwrap();
            assert_eq!(
                sql,
                [
                    "\"revision\" = \"records\".\"revision\" + $1",
                    "\"editor\" = $2"
                ]
            );
            assert_eq!(params, [value!(2), value!("actor'")]);
        }
    }
}
