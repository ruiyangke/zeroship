//! Collect lifecycle generators from a collection's immutable schema.

use crate::schema::{AssignmentEvent, AssignmentGenerator, FieldMap};

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
    pub fn from_schema(schema: &FieldMap) -> Self {
        let columns = schema
            .iter()
            .filter_map(|(name, definition)| {
                definition
                    .assignment
                    .as_ref()
                    .map(|assignment| AssignedColumn {
                        name: name.clone(),
                        by: assignment.by,
                        on: assignment.on,
                    })
            })
            .collect();
        Self { columns }
    }

    pub fn write_assignments(
        &self,
        schema: &FieldMap,
        actor: Option<&str>,
        deleting: bool,
        restoring: bool,
    ) -> crate::sql::lifecycle::WriteAssignments {
        use crate::sql::lifecycle::{AssignedValue, ColumnAssignment, WriteAssignments};
        use crate::value::Value;
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
                let physical = schema[&column.name]
                    .storage
                    .value_column
                    .as_deref()
                    .unwrap_or(&column.name);
                Some(ColumnAssignment {
                    column: physical.into(),
                    value,
                })
            })
            .collect();
        WriteAssignments { columns }
    }

    /// Assignments in declared field order.
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{Assignment, CollectionSchema, ColumnSchema, LogicalType};
    use crate::sql::lifecycle::AssignedValue;

    #[test]
    fn native_and_decoded_assignments_keep_generators_and_physical_columns() {
        let mut revision = ColumnSchema::new(LogicalType::Integer);
        revision.assignment = Some(Assignment {
            by: AssignmentGenerator::Increment(2),
            on: AssignmentEvent::Write,
        });
        revision.storage.value_column = Some("stored_revision".into());
        let native = CollectionSchema::new([("revision".into(), revision)]).into_fields();
        let decoded = CollectionSchema::from_fields(&crate::value!({
            "revision": {
                "type": "int", "required": true,
                "assign": {"by": "increment(2)", "on": "write"},
                "storage": {"valueColumn": "stored_revision"}
            }
        }))
        .unwrap()
        .into_fields();
        assert_eq!(native, decoded);
        for schema in [native, decoded] {
            let plan = AssignmentPlan::from_schema(&schema);
            assert_eq!(
                plan.columns(),
                &[AssignedColumn {
                    name: "revision".into(),
                    by: AssignmentGenerator::Increment(2),
                    on: AssignmentEvent::Write,
                }]
            );
            let assignments = plan.write_assignments(&schema, None, false, false);
            assert_eq!(assignments.columns.len(), 1);
            assert_eq!(assignments.columns[0].column, "stored_revision");
            assert!(matches!(
                assignments.columns[0].value,
                AssignedValue::Increment(2)
            ));
        }
        assert!(
            AssignmentPlan::from_schema(&FieldMap::new())
                .columns()
                .is_empty()
        );
    }
}
