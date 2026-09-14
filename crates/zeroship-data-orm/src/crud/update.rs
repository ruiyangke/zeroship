use super::{predicate, resolved::ResolvedTable};
use crate::schema::FieldMap;
use crate::{
    sql::{
        SchemaName,
        lifecycle::{AssignedValue, WriteAssignments},
        mapping::QueryError,
        registration::SqlRegistration,
        statement::{
            ArithmeticOperator, ArrayOperator, Assignment, Expression, MutationScope, Statement,
            Update, UpdateParts,
        },
        update::Operator,
    },
    value::Value,
};

#[derive(Debug, Clone)]
pub(crate) struct Input {
    pub(crate) values: Value,
    pub(crate) expressions: indexmap::IndexMap<String, crate::orm::TimestampExpr>,
    /// Whether the value patch assigns a field. It starts from what the caller
    /// supplied, where a patch made only of expressions carries an empty value
    /// object. [`Input::inspect`] clears it when removing the fields the
    /// descriptor reassigns on write empties the value patch; validation and
    /// statement building then skip the value patch.
    assigns_values: bool,
}

impl From<Value> for Input {
    fn from(values: Value) -> Self {
        Self::new(values, indexmap::IndexMap::new())
    }
}

impl Input {
    pub(crate) fn new(
        values: Value,
        expressions: indexmap::IndexMap<String, crate::orm::TimestampExpr>,
    ) -> Self {
        let assigns_values =
            expressions.is_empty() || !values.as_object().is_some_and(indexmap::IndexMap::is_empty);
        Self {
            values,
            expressions,
            assigns_values,
        }
    }

    pub(crate) fn result_checks(
        &self,
        schema: &FieldMap,
        many: bool,
    ) -> Result<Vec<ResultCheck>, QueryError> {
        let visible = if many {
            Vec::new()
        } else {
            crate::sql::mapping::implicit_read_fields(schema)?
        };
        let mut alias_index = 0;
        Ok(self
            .expressions
            .keys()
            .map(|field| {
                let hidden = many || !visible.contains(&field.as_str());
                let output = if hidden {
                    loop {
                        let candidate = format!("_timestamp_expr_{alias_index}");
                        alias_index += 1;
                        if !schema.contains_key(&candidate) {
                            break candidate;
                        }
                    }
                } else {
                    field.clone()
                };
                ResultCheck {
                    field: field.clone(),
                    output,
                    hidden,
                }
            })
            .collect())
    }

    pub(crate) fn inspect(&mut self, schema: &FieldMap) -> Result<(), crate::error::DbError> {
        use crate::error::DbError;
        use crate::schema::LogicalType;
        let assignments = crate::assignments::AssignmentPlan::from_schema(schema);
        for name in self.expressions.keys() {
            crate::sql::mapping::validate_field_name(name)?;
            let column = schema.get(name).ok_or_else(|| {
                DbError::validation("unknown_field", format!("field '{name}' is not declared"))
            })?;
            if name == "id" {
                return Err(DbError::validation(
                    "immutable_primary_key",
                    "collection identity cannot be changed",
                ));
            }
            if assignments
                .immutable_after_insert()
                .any(|field| field == name)
            {
                return Err(super::assignment_pass::immutable_assigned_field(name, None));
            }
            if column.logical_type != LogicalType::Timestamp {
                return Err(DbError::validation(
                    "invalid_timestamp_operation",
                    "timestamp expression requires a timestamp column",
                ));
            }
            if column.encrypted || column.is_masked() {
                return Err(DbError::validation(
                    "protected_update_operation",
                    "protected columns require literal assignments",
                ));
            }
            if !column.writable && column.assignment.is_none() {
                return Err(DbError::validation(
                    "field_not_writable",
                    "column does not permit assignments",
                ));
            }
        }
        if self.assigns_values {
            for assignment in crate::sql::update::assignments(&self.values)? {
                if self.expressions.contains_key(assignment.field) {
                    return Err(DbError::validation(
                        "invalid_update",
                        "a field may be assigned only once per update",
                    ));
                }
            }
            super::write_pipeline::inspect_update(schema, &mut self.values)?;
            self.assigns_values = assigns_a_field(&self.values);
        }
        self.expressions.retain(|field, _| {
            !assignments
                .reassigned_on_write()
                .any(|assigned| assigned == field)
        });
        // Removing the fields the descriptor reassigns on write can empty either
        // part of the patch. A patch left with neither a value assignment nor an
        // expression would write only the generated assignments.
        if !self.assigns_values && self.expressions.is_empty() {
            return Err(DbError::validation(
                "invalid_update",
                "update fields cannot be empty",
            ));
        }
        Ok(())
    }

    pub(crate) fn validate_values(&self, schema: &FieldMap) -> Result<(), crate::error::DbError> {
        if !self.assigns_values {
            return Ok(());
        }
        super::update_validation::validate(schema, &self.values)
    }
}

/// Whether a normalized patch assigns a field. Normalization keeps literal
/// values under `$set` and every other operator under its field name.
fn assigns_a_field(patch: &Value) -> bool {
    patch.as_object().is_some_and(|fields| {
        fields.iter().any(|(name, operand)| {
            name != "$set" || operand.as_object().is_some_and(|sets| !sets.is_empty())
        })
    })
}

pub(crate) struct ResultCheck {
    field: String,
    output: String,
    hidden: bool,
}

pub(crate) fn validate_results(
    rows: &mut [Value],
    checks: &[ResultCheck],
) -> Result<(), crate::error::DbError> {
    for row in rows {
        for check in checks {
            if row
                .get(&check.output)
                .and_then(crate::sql::temporal::timestamp_millis)
                .is_none()
            {
                return Err(crate::error::DbError::timestamp_expression_outside_calendar());
            }
            if check.hidden {
                row.as_object_mut()
                    .expect("checked result row")
                    .shift_remove(&check.output);
            }
        }
    }
    Ok(())
}

pub(crate) fn build_one(
    namespace: &SchemaName,
    collection: &str,
    schema: &FieldMap,
    filter: predicate::Input,
    update: Input,
    generated: &WriteAssignments,
    registration: &SqlRegistration,
) -> Result<crate::sql::compiler::CompiledQuery, QueryError> {
    build(
        BuildInput {
            namespace,
            collection,
            schema,
            filter,
            update,
            generated,
        },
        UpdateCardinality::One,
        registration,
    )
}

pub(crate) fn build_many(
    namespace: &SchemaName,
    collection: &str,
    schema: &FieldMap,
    filter: predicate::Input,
    update: Input,
    generated: &WriteAssignments,
    registration: &SqlRegistration,
) -> Result<crate::sql::compiler::CompiledQuery, QueryError> {
    build(
        BuildInput {
            namespace,
            collection,
            schema,
            filter,
            update,
            generated,
        },
        UpdateCardinality::Many,
        registration,
    )
}

struct BuildInput<'a> {
    namespace: &'a SchemaName,
    collection: &'a str,
    schema: &'a FieldMap,
    filter: predicate::Input,
    update: Input,
    generated: &'a WriteAssignments,
}

#[derive(Clone, Copy)]
enum UpdateCardinality {
    One,
    Many,
}

fn build(
    input: BuildInput<'_>,
    cardinality: UpdateCardinality,
    registration: &SqlRegistration,
) -> Result<crate::sql::compiler::CompiledQuery, QueryError> {
    let resolved = ResolvedTable::new(
        input.namespace,
        input.collection,
        input.schema,
        registration,
    )?;
    let predicate = input
        .filter
        .resolve(input.schema, &resolved, registration)?;
    let checks = input
        .update
        .result_checks(input.schema, matches!(cardinality, UpdateCardinality::Many))?;
    let assignments = resolve_assignments(input.update, input.generated, &resolved, registration)?;
    let scope = if matches!(cardinality, UpdateCardinality::One) {
        MutationScope::First {
            target: resolved
                .table
                .column(&crate::sql::mapping::value_column_for_field(
                    "id",
                    input.schema,
                ))?,
        }
    } else {
        MutationScope::Matching
    };
    let mut returning = if matches!(cardinality, UpdateCardinality::One) {
        resolved.returning(input.schema)?
    } else {
        Vec::new()
    };
    for check in checks.into_iter().filter(|check| check.hidden) {
        let physical = &resolved.inputs[&check.field].column;
        returning.push(crate::sql::statement::ReturnedColumn {
            column: resolved.table.column(physical)?,
            alias: Some(
                crate::sql::Ident::parse_as(&check.output, crate::sql::IdentRole::Alias)
                    .map_err(crate::sql::compiler::CompileError::from)?,
            ),
        });
    }
    registration
        .compile(Statement::Update(Update::new(UpdateParts {
            table: resolved.table,
            assignments,
            predicate,
            scope,
            returning,
        })?))
        .map_err(Into::into)
}

pub(crate) fn resolve_assignments(
    update: Input,
    generated: &WriteAssignments,
    resolved: &ResolvedTable,
    registration: &SqlRegistration,
) -> Result<Vec<Assignment>, QueryError> {
    let values = if update.assigns_values {
        crate::sql::update::into_assignments(update.values)
            .map_err(|error| invalid(error.to_string()))?
    } else {
        Vec::new()
    };
    let mut assignments = values
        .into_iter()
        .map(|assignment| {
            let input = resolved.inputs.get(&assignment.field).ok_or_else(|| {
                invalid(format!(
                    "update column is not declared by the descriptor: {}",
                    assignment.field
                ))
            })?;
            let column = resolved.table.column(&input.column)?;
            let value = match assignment.operator {
                Operator::Set if assignment.operand.is_null() => Expression::Null,
                Operator::Set => {
                    Expression::Bind(registration.encode(input.storage, assignment.operand)?)
                }
                Operator::Increment | Operator::Decrement | Operator::Multiply => {
                    let operator = match assignment.operator {
                        Operator::Increment => ArithmeticOperator::Add,
                        Operator::Decrement => ArithmeticOperator::Subtract,
                        Operator::Multiply => ArithmeticOperator::Multiply,
                        _ => unreachable!(),
                    };
                    Expression::Arithmetic {
                        column: column.clone(),
                        operator,
                        operand: registration.encode(input.storage, assignment.operand)?,
                    }
                }
                Operator::Push | Operator::Pull | Operator::AddToSet => {
                    let operator = match assignment.operator {
                        Operator::Push => ArrayOperator::Push,
                        Operator::Pull => ArrayOperator::Pull,
                        Operator::AddToSet => ArrayOperator::AddToSet,
                        _ => unreachable!(),
                    };
                    Expression::ArrayMutation {
                        column: column.clone(),
                        operator,
                        operand: registration.encode(input.storage, assignment.operand)?,
                    }
                }
            };
            Ok(Assignment { column, value })
        })
        .collect::<Result<Vec<_>, QueryError>>()?;
    for (name, expression) in update.expressions {
        let input = resolved
            .inputs
            .get(&name)
            .ok_or_else(|| invalid("expression column is not declared"))?;
        assignments.push(Assignment {
            column: resolved.table.column(&input.column)?,
            value: Expression::DatabaseTimestamp {
                offset_millis: expression.offset_millis(),
            },
        });
    }
    assignments.extend(resolve_generated(generated, resolved, registration)?);
    Ok(assignments)
}

pub(crate) fn resolve_generated(
    generated: &WriteAssignments,
    resolved: &ResolvedTable,
    registration: &SqlRegistration,
) -> Result<Vec<Assignment>, QueryError> {
    let mut assignments = Vec::with_capacity(generated.columns.len());
    for assignment in &generated.columns {
        let column = resolved.table.column(&assignment.column)?;
        let value = match &assignment.value {
            AssignedValue::CurrentTimestamp => Expression::CurrentTimestamp,
            AssignedValue::Increment(step) => Expression::Increment {
                column: column.clone(),
                step: *step,
            },
            AssignedValue::Bound(value) if value.is_null() => Expression::Null,
            AssignedValue::Bound(value) => {
                Expression::Bind(registration.encode(column.storage(), value.clone())?)
            }
        };
        assignments.push(Assignment { column, value });
    }
    Ok(assignments)
}

fn invalid(message: impl Into<String>) -> QueryError {
    QueryError::InvalidFilter(message.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::ColumnSchema;
    use crate::sql::{
        compiler::{CompileError, SqlCompiler, SqliteCompiler},
        registration::{SQLITE_FAMILY, SqlStorageCodecs},
        statement::StorageType,
    };
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    #[test]
    fn typed_timestamp_result_aliases_preserve_real_columns() {
        use crate::schema::LogicalType;
        let mut timestamp = ColumnSchema::new(LogicalType::Timestamp);
        timestamp.projectable = false;
        timestamp.readable = false;
        let mut id = ColumnSchema::new(LogicalType::Text);
        id.primary_key = true;
        let schema: FieldMap = [
            ("id".into(), id),
            ("private_stamp".into(), timestamp),
            (
                "_timestamp_expr_0".into(),
                ColumnSchema::new(LogicalType::Text),
            ),
        ]
        .into();
        let input = Input::new(
            crate::value!({}),
            [(
                "private_stamp".into(),
                crate::orm::TimestampExpr::database_now(),
            )]
            .into(),
        );
        let checks = input.result_checks(&schema, false).unwrap();
        assert_ne!(checks[0].output, "_timestamp_expr_0");
        let mut row = crate::value!({"id":"row", "_timestamp_expr_0":"public"});
        row.as_object_mut()
            .unwrap()
            .insert(checks[0].output.clone(), Value::from(0));
        let mut rows = vec![row];
        validate_results(&mut rows, &checks).unwrap();
        assert_eq!(
            rows,
            vec![crate::value!({"id":"row", "_timestamp_expr_0":"public"})]
        );
    }

    #[test]
    fn typed_timestamp_validation_refuses_protected_and_invalid_columns() {
        use crate::schema::LogicalType;
        for (name, definition) in [
            ("missing", None),
            ("text", Some(ColumnSchema::new(LogicalType::Text))),
            (
                "encrypted",
                Some({
                    let mut column = ColumnSchema::new(LogicalType::Timestamp);
                    column.encrypted = true;
                    column
                }),
            ),
        ] {
            let fields = definition
                .map(|definition| [(name.to_owned(), definition)].into())
                .unwrap_or_default();
            let mut input = Input::new(
                crate::value!({}),
                [(name.into(), crate::orm::TimestampExpr::database_now())].into(),
            );
            assert!(input.inspect(&fields).is_err());
        }
    }

    fn assigned_timestamp_fields() -> FieldMap {
        use crate::schema::{Assignment, AssignmentEvent, AssignmentGenerator, LogicalType};
        let mut id = ColumnSchema::new(LogicalType::Text);
        id.primary_key = true;
        let mut touched = ColumnSchema::new(LogicalType::Timestamp);
        touched.writable = false;
        touched.assignment = Some(Assignment {
            by: AssignmentGenerator::Now,
            on: AssignmentEvent::Write,
        });
        let mut revision = ColumnSchema::new(LogicalType::Integer);
        revision.writable = false;
        revision.assignment = Some(Assignment {
            by: AssignmentGenerator::Increment(1),
            on: AssignmentEvent::Write,
        });
        [
            ("id".into(), id),
            ("stamp".into(), ColumnSchema::new(LogicalType::Timestamp)),
            ("touched".into(), touched),
            ("revision".into(), revision),
        ]
        .into()
    }

    fn expressions(fields: &[&str]) -> indexmap::IndexMap<String, crate::orm::TimestampExpr> {
        fields
            .iter()
            .map(|field| {
                (
                    (*field).to_owned(),
                    crate::orm::TimestampExpr::database_now(),
                )
            })
            .collect()
    }

    #[test]
    fn patches_left_without_caller_fields_after_write_assignments_are_refused() {
        let fields = assigned_timestamp_fields();
        for (values, supplied) in [
            (crate::value!({"touched":0}), &[][..]),
            (crate::value!({"$set":{"touched":0}}), &[]),
            (crate::value!({"revision":{"$inc":1}}), &[]),
            (crate::value!({}), &["touched"]),
            (crate::value!({"revision":5}), &["touched"]),
            (crate::value!({"touched":0}), &["touched"]),
        ] {
            let mut input = Input::new(values.clone(), expressions(supplied));
            let error = input.inspect(&fields).unwrap_err();
            assert!(
                matches!(
                    error,
                    crate::error::DbError::ValidationFailed {
                        code: "invalid_update",
                        ..
                    }
                ),
                "{values:?} with {supplied:?}: {error:?}"
            );
        }

        let mut expression_only = Input::new(crate::value!({}), expressions(&["stamp"]));
        expression_only.inspect(&fields).unwrap();
        expression_only.validate_values(&fields).unwrap();
        assert_eq!(
            expression_only.expressions.keys().collect::<Vec<_>>(),
            ["stamp"]
        );

        let mut stripped = Input::new(
            crate::value!({"stamp":0,"revision":3}),
            expressions(&["touched"]),
        );
        stripped.inspect(&fields).unwrap();
        stripped.validate_values(&fields).unwrap();
        assert_eq!(stripped.values, crate::value!({"$set":{"stamp":0}}));
        assert!(stripped.expressions.is_empty());
    }

    /// Removing reassigned fields keeps whatever else the caller assigns, as a
    /// literal or as an expression, so an expression beside a removed value
    /// still writes and only the expression reaches the statement.
    #[test]
    fn expressions_left_beside_removed_values_still_write() {
        let fields = assigned_timestamp_fields();
        let registration = SqlRegistration::sqlite();
        let resolved = ResolvedTable::new(
            &SchemaName::new("app").unwrap(),
            "moments",
            &fields,
            &registration,
        )
        .unwrap();
        for values in [
            crate::value!({"touched":0}),
            crate::value!({"revision":{"$inc":1}}),
        ] {
            let mut input = Input::new(values.clone(), expressions(&["stamp"]));
            input.inspect(&fields).unwrap();
            input.validate_values(&fields).unwrap();
            let assignments = resolve_assignments(
                input,
                &WriteAssignments::default(),
                &resolved,
                &registration,
            )
            .unwrap();
            assert_eq!(assignments.len(), 1, "{values:?}");
            assert!(
                matches!(
                    assignments[0].value,
                    Expression::DatabaseTimestamp { offset_millis: 0 }
                ),
                "{values:?}"
            );
        }
    }

    #[derive(Clone)]
    struct CountingCodecs(Arc<AtomicUsize>);

    impl SqlStorageCodecs for CountingCodecs {
        fn storage_type(&self, definition: &ColumnSchema) -> Result<StorageType, CompileError> {
            SqlRegistration::sqlite().storage_type(definition)
        }

        fn encode(&self, storage: StorageType, value: Value) -> Result<Value, CompileError> {
            self.0.fetch_add(1, Ordering::Relaxed);
            SqlRegistration::sqlite().encode(storage, value)
        }

        fn decode(&self, storage: StorageType, value: Value) -> Result<Value, CompileError> {
            SqlRegistration::sqlite().decode(storage, value)
        }
    }

    #[test]
    fn array_mutation_operands_cross_the_registered_codec_once() {
        let calls = Arc::new(AtomicUsize::new(0));
        let compiler = SqliteCompiler;
        let registration = SqlRegistration::new(
            "array-codec",
            SQLITE_FAMILY,
            compiler,
            CountingCodecs(calls.clone()),
            compiler.support(),
        )
        .unwrap();
        let schema = crate::tests::fixtures::native_fields(crate::value!({
            "id":{"type":"string","primaryKey":true},
            "items":{"type":"array","items":"json"}
        }));
        let resolved = ResolvedTable::new(
            &SchemaName::new("app").unwrap(),
            "entries",
            &schema,
            &registration,
        )
        .unwrap();

        resolve_assignments(
            crate::value!({"items":{"$push":{"key":"value"}}}).into(),
            &WriteAssignments::default(),
            &resolved,
            &registration,
        )
        .unwrap();

        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }
}
