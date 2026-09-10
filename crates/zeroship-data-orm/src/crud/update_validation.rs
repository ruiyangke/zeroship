//! Check logical update operations before row lookup or storage transforms.
use crate::error::DbError;
use zeroship_data_sql::{update, value::Value};

pub(super) fn validate(schema: &Value, patch: &Value) -> Result<(), DbError> {
    for assignment in update::assignments(patch)? {
        use update::Operator;
        let definition = &schema[assignment.field];
        let kind = definition["type"].as_str();
        let supported = match assignment.operator {
            Operator::Set => true,
            Operator::Increment | Operator::Decrement | Operator::Multiply => matches!(
                kind,
                Some("int" | "integer" | "bigInt" | "number" | "float" | "decimal" | "numeric")
            ),
            Operator::Push | Operator::Pull | Operator::AddToSet => kind == Some("array"),
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
            return Err(DbError::validation(
                code,
                format!(
                    "operation '{}' is not supported for column '{}' with type '{}'",
                    assignment.operator.name(),
                    assignment.field,
                    kind.unwrap_or("undeclared")
                ),
            ));
        }
    }
    Ok(())
}
