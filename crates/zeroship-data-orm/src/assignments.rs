//! Resolve lifecycle generators from a collection's immutable descriptor.

use crate::error::DbError;
use crate::value::Value;
use zeroship_migrate_policy::{AssignmentEvent, AssignmentGenerator};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AssignedColumn {
    pub name: String,
    pub by: AssignmentGenerator,
    pub on: AssignmentEvent,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AssignmentPlan {
    columns: Vec<AssignedColumn>,
}

impl AssignmentPlan {
    /// Resolve the generators declared by one collection's immutable descriptor.
    pub fn from_schema(schema: &crate::value::Value) -> Result<Self, DbError> {
        let mut columns = Vec::new();
        for (name, definition) in schema
            .as_object()
            .ok_or_else(|| DbError::internal("field map must be an object"))?
        {
            let Some(assignment) = definition.get("assign") else {
                continue;
            };
            let by = assignment
                .get("by")
                .and_then(Value::as_str)
                .ok_or_else(|| DbError::internal("assignment requires a generator"))?
                .parse::<AssignmentGenerator>()
                .map_err(|e| DbError::validation("invalid_assignment", e.to_string()))?;
            let on = match assignment.get("on").and_then(Value::as_str) {
                Some("insert") => AssignmentEvent::Insert,
                Some("write") => AssignmentEvent::Write,
                Some("delete") => AssignmentEvent::Delete,
                _ => {
                    return Err(DbError::validation(
                        "invalid_assignment",
                        "unknown assignment event",
                    ));
                }
            };
            let kind = definition.get("type").and_then(Value::as_str).unwrap_or("");
            let valid = match by {
                AssignmentGenerator::Now => matches!(kind, "date" | "timestamp" | "timestamptz"),
                AssignmentGenerator::Actor => matches!(kind, "string" | "text" | "id"),
                AssignmentGenerator::TypedId => {
                    on == AssignmentEvent::Insert && matches!(kind, "string" | "text" | "id")
                }
                AssignmentGenerator::Identity => {
                    on == AssignmentEvent::Insert
                        && matches!(kind, "integer" | "int" | "bigint" | "bigInt")
                }
                AssignmentGenerator::Increment(_) => {
                    on == AssignmentEvent::Write
                        && matches!(kind, "integer" | "int" | "bigint" | "bigInt")
                }
            };
            if !valid {
                return Err(DbError::validation(
                    "invalid_assignment",
                    format!("generator does not match the type or event of '{name}'"),
                ));
            }
            columns.push(AssignedColumn {
                name: name.clone(),
                by,
                on,
            });
        }
        Ok(Self { columns })
    }

    pub fn write_assignments(
        &self,
        schema: &crate::value::Value,
        actor: Option<&str>,
        deleting: bool,
        restoring: bool,
    ) -> crate::sql::lifecycle::WriteAssignments {
        use crate::value::Value;
        use crate::sql::{lifecycle::{AssignedValue, ColumnAssignment, WriteAssignments}};
        let columns = self
            .columns
            .iter()
            .filter_map(|column| {
                let value = if column.on == AssignmentEvent::Delete && restoring {
                    AssignedValue::Bound(Value::Null)
                } else if column.on == AssignmentEvent::Write
                    || (column.on == AssignmentEvent::Delete && deleting)
                {
                    match column.by {
                        AssignmentGenerator::Now => AssignedValue::CurrentTimestamp,
                        AssignmentGenerator::Actor => {
                            AssignedValue::Bound(actor.map_or(Value::Null, |actor| actor.into()))
                        }
                        AssignmentGenerator::Increment(step) => AssignedValue::Increment(step),
                        AssignmentGenerator::TypedId | AssignmentGenerator::Identity => {
                            return None;
                        }
                    }
                } else {
                    return None;
                };
                let physical = schema[&column.name]["storage"]["valueColumn"]
                    .as_str()
                    .unwrap_or(&column.name);
                Some(ColumnAssignment {
                    column: physical.into(),
                    value,
                })
            })
            .collect();
        WriteAssignments { columns }
    }

    /// Assignments in descriptor order.
    pub fn columns(&self) -> &[AssignedColumn] {
        &self.columns
    }

    /// Fields that callers may not change after insertion.
    pub fn immutable_after_insert(&self) -> impl Iterator<Item = &str> {
        self.columns
            .iter()
            .filter(|column| column.on != AssignmentEvent::Write)
            .map(|column| column.name.as_str())
    }

    /// Columns the platform re-assigns on every write: `on = "write"`.
    pub fn reassigned_on_write(&self) -> impl Iterator<Item = &str> {
        self.columns
            .iter()
            .filter(|column| column.on == AssignmentEvent::Write)
            .map(|column| column.name.as_str())
    }
}
