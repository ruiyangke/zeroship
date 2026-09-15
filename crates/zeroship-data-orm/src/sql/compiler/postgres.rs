use super::{
    CompileError, CompiledQuery, IdentityPlan, IdentityReadPlan, Requirements, SqlCompiler,
    SqlSupport, SqlWriter,
};
use crate::{
    sql::statement::{ArrayOperator, Column, IdentityRequest, Statement},
    value::Value,
};

#[derive(Clone, Copy, Debug)]
pub struct PostgresCompiler;

const SUPPORT: SqlSupport = SqlSupport {
    relational_reads: true,
    aggregate_reads: true,
    vector_search: true,
    inner_product_vector_search: true,
    spatial_search: true,
    explicit_conflict_target: true,
    conditional_conflict_update: true,
    returning: true,
    insert_generated_identity: true,
    identity_allocation: true,
    default_expression: true,
    row_locks: true,
    advisory_locks: true,
    transaction_settings: true,
    max_bind_parameters: super::POSTGRES_BIND_LIMIT,
};

const SYNTAX: super::shared::Syntax = super::shared::Syntax {
    current_timestamp: "NOW()",
    database_timestamp: write_database_timestamp,
    generated_identity_override: Some(" OVERRIDING SYSTEM VALUE"),
    timestamp_cast: "::timestamptz",
    vector_cast: "::vector",
    numeric_cast: "::numeric",
    write_target_lock: " FOR UPDATE",
    required_row_lock: Some(" FOR UPDATE"),
    insensitive_like: "ILIKE",
    insensitive_like_suffix: "",
    average_suffix: "::double precision",
    offset_without_limit: "",
    structural_json_equality: false,
    exact_decimal_functions: false,
    vector_distance: write_vector_distance,
    array_mutation: write_array_mutation,
};

fn write_database_timestamp(
    writer: &mut SqlWriter,
    offset_millis: i64,
) -> Result<(), CompileError> {
    writer.sql.push_str("(clock_timestamp() + ");
    let seconds = offset_millis / 1000;
    let fraction = (offset_millis % 1000).unsigned_abs();
    let sign = if offset_millis < 0 && seconds == 0 {
        "-"
    } else {
        ""
    };
    writer.write_param(Value::from(format!(
        "{sign}{seconds}.{fraction:03} seconds"
    )))?;
    writer.sql.push_str("::interval)");
    Ok(())
}

fn write_vector_distance(
    writer: &mut SqlWriter,
    column: &Column,
    metric: crate::sql::descriptors::VectorMetric,
    query: super::ParameterSlot,
) -> Result<(), CompileError> {
    super::shared::write_column_reference(writer, column);
    writer.sql.push_str(match metric {
        crate::sql::descriptors::VectorMetric::Cosine => " <=> ",
        crate::sql::descriptors::VectorMetric::L2 => " <-> ",
        crate::sql::descriptors::VectorMetric::InnerProduct => " <#> ",
    });
    writer.write_bound(query);
    writer.sql.push_str("::vector");
    Ok(())
}

fn compile_spatial_near(
    search: crate::sql::statement::SpatialNearStatement,
    effective: &SqlSupport,
) -> Result<CompiledQuery, CompileError> {
    search.validate()?;
    let parts = search.into_parts();
    let mut writer = SqlWriter::new(effective.max_bind_parameters);
    let point = writer.bind(parts.point)?;
    let radius = writer
        .bind(Value::try_from(parts.radius_m).map_err(|_| {
            CompileError::InvalidStatement("spatial radius must be finite".into())
        })?)?;
    writer.sql.push_str("SELECT ");
    super::shared::write_search_projection(&mut writer, &parts.projection);
    writer.sql.push_str(", ST_Distance(");
    super::shared::write_column_reference(&mut writer, &parts.spatial);
    writer.sql.push_str(", ");
    writer.write_bound(point);
    writer.sql.push_str("::geography) AS ");
    writer.identifier("_distance_m");
    writer.sql.push_str(" FROM ");
    super::shared::write_table_reference(&mut writer, &parts.table);
    writer.sql.push_str(" WHERE ST_DWithin(");
    super::shared::write_column_reference(&mut writer, &parts.spatial);
    writer.sql.push_str(", ");
    writer.write_bound(point);
    writer.sql.push_str("::geography, ");
    writer.write_bound(radius);
    writer.sql.push(')');
    if !matches!(
        &parts.predicate,
        crate::sql::statement::ResolvedPredicate::Const(true)
    ) {
        writer.sql.push_str(" AND ");
        super::shared::write_predicate(&mut writer, SYNTAX, parts.predicate)?;
    }
    writer.sql.push_str(" ORDER BY ");
    writer.identifier("_distance_m");
    writer.sql.push_str(", ");
    super::shared::write_column_reference(&mut writer, &parts.identity);
    writer.sql.push_str(" LIMIT ");
    writer.write_param(Value::from(parts.limit))?;
    Ok(writer.finish())
}

fn write_column(writer: &mut SqlWriter, column: &Column) {
    writer.identifier(column.name().as_str());
}

/// Render an advisory lock request. The database hashes and folds case, so a
/// caller spelling the same form in SQL contends on the identical lock.
fn compile_advisory_lock(
    lock: crate::sql::coordination::AdvisoryLock,
    effective: &SqlSupport,
) -> Result<CompiledQuery, CompileError> {
    use crate::sql::coordination::{AdvisoryKey, AdvisoryLockAction, AdvisoryLockScope};
    lock.validate()?;
    let mut writer = SqlWriter::new(effective.max_bind_parameters);
    writer.sql.push_str(match (lock.scope(), lock.action()) {
        (AdvisoryLockScope::Transaction, AdvisoryLockAction::Wait) => {
            "SELECT pg_advisory_xact_lock("
        }
        (AdvisoryLockScope::Transaction, AdvisoryLockAction::Try) => {
            "SELECT pg_try_advisory_xact_lock("
        }
        (AdvisoryLockScope::Session, AdvisoryLockAction::Wait) => "SELECT pg_advisory_lock(",
        (AdvisoryLockScope::Session, AdvisoryLockAction::Try) => "SELECT pg_try_advisory_lock(",
        (AdvisoryLockScope::Session, AdvisoryLockAction::Release) => "SELECT pg_advisory_unlock(",
        (AdvisoryLockScope::Transaction, AdvisoryLockAction::Release) => {
            return Err(CompileError::InvalidStatement(
                "transaction advisory locks are released when the transaction settles".into(),
            ));
        }
    });
    match lock.key() {
        AdvisoryKey::Single(key) => {
            writer.write_param(Value::from(*key))?;
            writer.sql.push_str("::int8");
        }
        AdvisoryKey::Pair(high, low) => {
            writer.write_param(Value::from(i64::from(*high)))?;
            writer.sql.push_str("::int4, ");
            writer.write_param(Value::from(i64::from(*low)))?;
            writer.sql.push_str("::int4");
        }
        AdvisoryKey::HashedPair { namespace, text } => {
            writer.write_param(Value::from(i64::from(*namespace)))?;
            writer.sql.push_str("::int4, hashtext(");
            writer.write_param(Value::from(text.as_str()))?;
            writer.sql.push_str("::text)");
        }
        AdvisoryKey::Hashed(text) => {
            writer.sql.push_str("hashtext(");
            writer.write_param(Value::from(text.as_str()))?;
            writer.sql.push_str("::text)::int8");
        }
        AdvisoryKey::HashedLowercase(text) => {
            writer.sql.push_str("hashtext(lower(");
            writer.write_param(Value::from(text.as_str()))?;
            writer.sql.push_str("::text))::int8");
        }
    }
    writer.sql.push(')');
    match lock.action() {
        AdvisoryLockAction::Wait => {}
        AdvisoryLockAction::Try => {
            writer.sql.push_str(" AS ");
            writer.identifier(crate::sql::coordination::ADVISORY_ACQUIRED);
        }
        AdvisoryLockAction::Release => {
            writer.sql.push_str(" AS ");
            writer.identifier(crate::sql::coordination::ADVISORY_RELEASED);
        }
    }
    Ok(writer.finish())
}

/// Render a transaction-local setting. Name and value are both bound.
fn compile_transaction_setting(
    setting: crate::sql::coordination::SetTransactionSetting,
    effective: &SqlSupport,
) -> Result<CompiledQuery, CompileError> {
    setting.validate()?;
    let (name, value) = setting.into_parts();
    let mut writer = SqlWriter::new(effective.max_bind_parameters);
    writer.sql.push_str("SELECT set_config(");
    writer.write_param(Value::from(name.as_str()))?;
    writer.sql.push_str("::text, ");
    writer.write_param(Value::from(value))?;
    writer.sql.push_str("::text, true)");
    Ok(writer.finish())
}

fn write_array_mutation(
    writer: &mut SqlWriter,
    column: &Column,
    operator: ArrayOperator,
    operand: super::ParameterSlot,
) -> Result<(), CompileError> {
    writer.sql.push_str("CASE WHEN ");
    write_column(writer, column);
    writer.sql.push_str(" IS NULL OR jsonb_typeof(");
    write_column(writer, column);
    writer.sql.push_str(") = 'null' THEN ");
    write_column(writer, column);
    writer.sql.push_str(" ELSE ");
    match operator {
        ArrayOperator::Push => write_append(writer, column, operand),
        ArrayOperator::Pull => {
            writer.sql.push_str("(SELECT COALESCE(jsonb_agg(__zs_array.element ORDER BY __zs_array.position), '[]'::jsonb) FROM jsonb_array_elements(");
            write_column(writer, column);
            writer.sql.push_str(
                ") WITH ORDINALITY AS __zs_array(element, position) WHERE __zs_array.element != ",
            );
            writer.write_bound(operand);
            writer.sql.push_str("::jsonb)");
        }
        ArrayOperator::AddToSet => {
            writer
                .sql
                .push_str("CASE WHEN EXISTS (SELECT 1 FROM jsonb_array_elements(");
            write_column(writer, column);
            writer
                .sql
                .push_str(") AS __zs_array(element) WHERE __zs_array.element = ");
            writer.write_bound(operand);
            writer.sql.push_str("::jsonb) THEN ");
            write_column(writer, column);
            writer.sql.push_str(" ELSE ");
            write_append(writer, column, operand);
            writer.sql.push_str(" END");
        }
    }
    writer.sql.push_str(" END");
    Ok(())
}

fn write_append(writer: &mut SqlWriter, column: &Column, operand: super::ParameterSlot) {
    writer.sql.push_str("jsonb_insert(");
    write_column(writer, column);
    writer.sql.push_str(", ARRAY[jsonb_array_length(");
    write_column(writer, column);
    writer.sql.push_str(")::text], ");
    writer.write_bound(operand);
    writer.sql.push_str("::jsonb)");
}

impl SqlCompiler for PostgresCompiler {
    fn support(&self) -> SqlSupport {
        SUPPORT
    }

    fn check(
        &self,
        requirements: &Requirements,
        effective: &SqlSupport,
    ) -> Result<(), CompileError> {
        super::shared::check(SUPPORT, requirements, effective)
    }

    fn compile(
        &self,
        statement: Statement,
        effective: &SqlSupport,
    ) -> Result<CompiledQuery, CompileError> {
        self.check(
            &super::shared::compiler_requirements(self, &statement),
            effective,
        )?;
        match statement {
            Statement::Select(statement) => {
                super::shared::compile_select(SYNTAX, effective, *statement)
            }
            Statement::VectorSearch(statement) => {
                super::shared::compile_vector_search(SYNTAX, effective, statement)
            }
            Statement::SpatialNear(statement) => compile_spatial_near(statement, effective),
            Statement::Insert(statement) => {
                super::shared::compile_insert(SYNTAX, effective, statement)
            }
            Statement::Upsert(statement) => {
                super::shared::compile_upsert(SYNTAX, effective, statement)
            }
            Statement::Update(statement) => {
                super::shared::compile_update(SYNTAX, effective, statement)
            }
            Statement::Delete(statement) => {
                super::shared::compile_delete(SYNTAX, effective, statement)
            }
            Statement::AdvisoryLock(statement) => compile_advisory_lock(statement, effective),
            Statement::SetTransactionSetting(statement) => {
                compile_transaction_setting(statement, effective)
            }
        }
    }

    fn compile_identity_allocation(
        &self,
        request: IdentityRequest,
        effective: &SqlSupport,
    ) -> Result<IdentityPlan, CompileError> {
        super::shared::check(
            SUPPORT,
            &Requirements {
                identity_allocation: true,
                ..Requirements::default()
            },
            effective,
        )?;
        let mut table = SqlWriter::new(effective.max_bind_parameters);
        super::shared::write_table(&mut table, request.table());
        let table = table.finish().into_parts().0;
        let count = i64::try_from(request.count()).map_err(|_| {
            CompileError::InvalidStatement("generated identity batch is too large".into())
        })?;
        let mut writer = SqlWriter::new(effective.max_bind_parameters);
        writer
            .sql
            .push_str("SELECT nextval(pg_get_serial_sequence(");
        writer.write_param(Value::from(table))?;
        writer.sql.push_str(", ");
        writer.write_param(Value::from(request.column().name().as_str()))?;
        writer.sql.push_str(")) AS ");
        writer.identifier("id");
        writer.sql.push_str(" FROM generate_series(1, ");
        writer.write_param(Value::from(count))?;
        writer.sql.push_str("::integer)");
        Ok(IdentityPlan {
            reservation: None,
            allocation: IdentityReadPlan::Rows(writer.finish()),
        })
    }
}
