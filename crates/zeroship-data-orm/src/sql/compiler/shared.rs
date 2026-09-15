use super::{CompileError, CompiledQuery, SqlWriter};
use crate::sql::{
    CompareOp, MembershipOp, PatternOp,
    statement::{
        ArithmeticOperator, ArrayElement, ArrayOperator, Column, Delete, Expression, Insert,
        MutationScope, ResolvedOperand, ResolvedPredicate, ResolvedPredicateValue, SelectStatement,
        Statement, StorageType, Table, Update, Upsert, VectorSearchStatement,
    },
};
use crate::value::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SqlSupport {
    pub relational_reads: bool,
    pub aggregate_reads: bool,
    pub vector_search: bool,
    pub inner_product_vector_search: bool,
    pub spatial_search: bool,
    pub explicit_conflict_target: bool,
    pub conditional_conflict_update: bool,
    pub returning: bool,
    pub insert_generated_identity: bool,
    pub identity_allocation: bool,
    pub default_expression: bool,
    /// Caller-required exclusive row locks on selected rows.
    pub row_locks: bool,
    /// Advisory locks coordinated by the database server.
    pub advisory_locks: bool,
    /// Custom settings scoped to a transaction.
    pub transaction_settings: bool,
    /// The resolution this backend stores an instant at. A value or offset
    /// finer than the declared resolution is refused, never floored.
    pub timestamp_resolution: crate::sql::temporal::TimestampResolution,
    pub max_bind_parameters: usize,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Requirements {
    pub relational_reads: bool,
    pub aggregate_reads: bool,
    pub vector_search: bool,
    pub inner_product_vector_search: bool,
    pub spatial_search: bool,
    pub explicit_conflict_target: bool,
    pub conditional_conflict_update: bool,
    pub returning: bool,
    pub insert_generated_identity: bool,
    pub identity_allocation: bool,
    pub default_expression: bool,
    pub row_locks: bool,
    pub advisory_locks: bool,
    pub transaction_settings: bool,
    pub bind_parameters: usize,
}

impl Requirements {
    pub fn for_statement(statement: &Statement) -> Self {
        match statement {
            Statement::Select(select) => {
                let parts = select.parts();
                Self {
                    relational_reads: !parts.joins.is_empty(),
                    aggregate_reads: select.summary()
                        == Some(crate::sql::statement::SelectSummary::Count)
                        || parts.projection.iter().any(|selected| {
                            matches!(selected.expression, ResolvedOperand::Aggregate { .. })
                        })
                        || !parts.group_by.is_empty()
                        || predicate_has_aggregate(&parts.having),
                    row_locks: matches!(
                        parts.lock,
                        crate::sql::statement::RowLock::Required { .. }
                    ),
                    bind_parameters: parts
                        .joins
                        .iter()
                        .map(|join| predicate_binds(&join.on))
                        .sum::<usize>()
                        + parts
                            .projection
                            .iter()
                            .map(|selected| operand_binds(&selected.expression))
                            .sum::<usize>()
                        + parts.group_by.iter().map(operand_binds).sum::<usize>()
                        + parts
                            .order_by
                            .iter()
                            .map(|order| operand_binds(&order.expression))
                            .sum::<usize>()
                        + predicate_binds(&parts.predicate)
                        + predicate_binds(&parts.having)
                        + usize::from(parts.limit.is_some())
                        + usize::from(parts.offset.is_some()),
                    ..Self::default()
                }
            }
            Statement::VectorSearch(search) => {
                let parts = search.parts();
                Self {
                    vector_search: true,
                    inner_product_vector_search: parts.metric
                        == crate::sql::descriptors::VectorMetric::InnerProduct,
                    bind_parameters: predicate_binds(&parts.predicate) + 2,
                    ..Self::default()
                }
            }
            Statement::SpatialNear(search) => {
                let parts = search.parts();
                Self {
                    spatial_search: true,
                    bind_parameters: predicate_binds(&parts.predicate) + 3,
                    ..Self::default()
                }
            }
            Statement::Insert(insert) => {
                let parts = insert.parts();
                let values = parts.rows.iter().flatten();
                Self {
                    returning: !parts.returning.is_empty(),
                    insert_generated_identity: parts.insert_generated_identity,
                    identity_allocation: false,
                    default_expression: values
                        .clone()
                        .any(|value| matches!(value, Expression::Default)),
                    bind_parameters: values
                        .filter(|value| matches!(value, Expression::Bind(_)))
                        .count(),
                    ..Self::default()
                }
            }
            Statement::Upsert(upsert) => {
                let parts = upsert.parts();
                let values = parts.insert.iter().chain(&parts.update).map(|a| &a.value);
                Self {
                    relational_reads: false,
                    aggregate_reads: false,
                    vector_search: false,
                    inner_product_vector_search: false,
                    spatial_search: false,
                    explicit_conflict_target: true,
                    conditional_conflict_update: parts.condition.is_some(),
                    returning: !parts.returning.is_empty(),
                    insert_generated_identity: parts.insert_generated_identity,
                    identity_allocation: false,
                    default_expression: values.clone().any(|v| matches!(v, Expression::Default)),
                    row_locks: false,
                    advisory_locks: false,
                    transaction_settings: false,
                    bind_parameters: values.map(expression_binds).sum::<usize>()
                        + usize::from(parts.condition.is_some()),
                }
            }
            Statement::Update(update) => {
                let parts = update.parts();
                Self {
                    returning: !parts.returning.is_empty(),
                    bind_parameters: parts
                        .assignments
                        .iter()
                        .map(|assignment| expression_binds(&assignment.value))
                        .sum::<usize>()
                        + predicate_binds(&parts.predicate),
                    ..Self::default()
                }
            }
            Statement::Delete(delete) => {
                let parts = delete.parts();
                Self {
                    returning: !parts.returning.is_empty(),
                    bind_parameters: predicate_binds(&parts.predicate),
                    ..Self::default()
                }
            }
            Statement::AdvisoryLock(lock) => Self {
                advisory_locks: true,
                bind_parameters: lock.key().bind_parameters(),
                ..Self::default()
            },
            Statement::SetTransactionSetting(_) => Self {
                transaction_settings: true,
                bind_parameters: 2,
                ..Self::default()
            },
        }
    }
}

fn predicate_has_aggregate(predicate: &ResolvedPredicate) -> bool {
    match predicate {
        ResolvedPredicate::And(children) | ResolvedPredicate::Or(children) => {
            children.iter().any(predicate_has_aggregate)
        }
        ResolvedPredicate::Not(child) => predicate_has_aggregate(child),
        ResolvedPredicate::Compare { lhs, rhs, .. } => {
            matches!(lhs, ResolvedOperand::Aggregate { .. })
                || matches!(
                    rhs,
                    ResolvedPredicateValue::Operand(ResolvedOperand::Aggregate { .. })
                )
        }
        ResolvedPredicate::Membership { lhs, .. }
        | ResolvedPredicate::Pattern { lhs, .. }
        | ResolvedPredicate::IsNull { operand: lhs, .. } => {
            matches!(lhs, ResolvedOperand::Aggregate { .. })
        }
        ResolvedPredicate::Const(_) => false,
    }
}

fn expression_binds(expression: &Expression) -> usize {
    usize::from(matches!(
        expression,
        Expression::Bind(_)
            | Expression::Increment { .. }
            | Expression::Arithmetic { .. }
            | Expression::ArrayMutation { .. }
            | Expression::DatabaseTimestamp { .. }
    ))
}

fn operand_binds(operand: &ResolvedOperand) -> usize {
    usize::from(matches!(operand, ResolvedOperand::Comparison(_)))
}

pub(crate) fn predicate_binds(predicate: &ResolvedPredicate) -> usize {
    match predicate {
        ResolvedPredicate::And(children) | ResolvedPredicate::Or(children) => {
            children.iter().map(predicate_binds).sum()
        }
        ResolvedPredicate::Not(child) => predicate_binds(child),
        ResolvedPredicate::Compare { lhs, rhs, .. } => {
            operand_binds(lhs)
                + match rhs {
                    ResolvedPredicateValue::Operand(rhs) => operand_binds(rhs),
                    ResolvedPredicateValue::Bind { .. } => 1,
                }
        }
        ResolvedPredicate::Pattern { lhs, escape, .. } => {
            operand_binds(lhs) + 1 + usize::from(escape.is_some())
        }
        ResolvedPredicate::Membership { lhs, values, .. } => operand_binds(lhs) + values.len(),
        ResolvedPredicate::IsNull { operand, .. } => operand_binds(operand),
        ResolvedPredicate::Const(_) => 0,
    }
}

pub trait SqlCompiler: Send + Sync {
    fn support(&self) -> SqlSupport;
    fn bind_parameters(&self, statement: &Statement) -> usize {
        Requirements::for_statement(statement).bind_parameters
    }
    fn check(
        &self,
        requirements: &Requirements,
        effective: &SqlSupport,
    ) -> Result<(), CompileError>;
    fn compile(
        &self,
        statement: Statement,
        effective: &SqlSupport,
    ) -> Result<CompiledQuery, CompileError>;
    fn compile_identity_allocation(
        &self,
        request: crate::sql::statement::IdentityRequest,
        effective: &SqlSupport,
    ) -> Result<IdentityPlan, CompileError>;
}

pub(crate) fn compiler_requirements(
    compiler: &dyn SqlCompiler,
    statement: &Statement,
) -> Requirements {
    let mut requirements = Requirements::for_statement(statement);
    requirements.bind_parameters = compiler.bind_parameters(statement);
    requirements
}

#[derive(Debug)]
pub struct IdentityPlan {
    pub reservation: Option<CompiledQuery>,
    pub allocation: IdentityReadPlan,
}

#[derive(Debug)]
pub enum IdentityReadPlan {
    Rows(CompiledQuery),
    MaximumAndCounter {
        maximum: CompiledQuery,
        counter_exists: CompiledQuery,
        counter: CompiledQuery,
    },
}

#[derive(Clone, Copy)]
pub(crate) struct Syntax {
    pub(crate) current_timestamp: &'static str,
    /// Renders the database clock shifted by a signed microsecond offset. A
    /// millisecond-resolution backend refuses an offset it cannot render.
    pub(crate) database_timestamp: fn(&mut SqlWriter, i64) -> Result<(), CompileError>,
    pub(crate) generated_identity_override: Option<&'static str>,
    pub(crate) timestamp_cast: &'static str,
    pub(crate) vector_cast: &'static str,
    pub(crate) numeric_cast: &'static str,
    /// Cast for a bound native text array; `None` when the dialect has no array type.
    pub(crate) text_array_cast: Option<&'static str>,
    /// Locking clause for write-target probes and first-row mutation scopes.
    /// Empty when the backend's single writer serializes writes.
    pub(crate) write_target_lock: &'static str,
    /// Exclusive row-lock clause, or `None` when the backend has no row locks.
    pub(crate) required_row_lock: Option<&'static str>,
    pub(crate) insensitive_like: &'static str,
    pub(crate) insensitive_like_suffix: &'static str,
    pub(crate) average_suffix: &'static str,
    pub(crate) offset_without_limit: &'static str,
    pub(crate) structural_json_equality: bool,
    pub(crate) exact_decimal_functions: bool,
    pub(crate) vector_distance: fn(
        &mut SqlWriter,
        &Column,
        crate::sql::descriptors::VectorMetric,
        super::ParameterSlot,
    ) -> Result<(), CompileError>,
    pub(crate) array_mutation: fn(
        &mut SqlWriter,
        &Column,
        ArrayOperator,
        super::ParameterSlot,
    ) -> Result<(), CompileError>,
}

pub(crate) fn check(
    implemented: SqlSupport,
    required: &Requirements,
    effective: &SqlSupport,
) -> Result<(), CompileError> {
    for (requirement, available, implementation, name) in [
        (
            required.relational_reads,
            effective.relational_reads,
            implemented.relational_reads,
            "relational reads",
        ),
        (
            required.aggregate_reads,
            effective.aggregate_reads,
            implemented.aggregate_reads,
            "aggregate reads",
        ),
        (
            required.vector_search,
            effective.vector_search,
            implemented.vector_search,
            "vector search",
        ),
        (
            required.inner_product_vector_search,
            effective.inner_product_vector_search,
            implemented.inner_product_vector_search,
            "inner-product vector search",
        ),
        (
            required.spatial_search,
            effective.spatial_search,
            implemented.spatial_search,
            "spatial search",
        ),
        (
            required.explicit_conflict_target,
            effective.explicit_conflict_target,
            implemented.explicit_conflict_target,
            "explicit conflict targets",
        ),
        (
            required.conditional_conflict_update,
            effective.conditional_conflict_update,
            implemented.conditional_conflict_update,
            "conditional conflict updates",
        ),
        (
            required.returning,
            effective.returning,
            implemented.returning,
            "returning projections",
        ),
        (
            required.insert_generated_identity,
            effective.insert_generated_identity,
            implemented.insert_generated_identity,
            "inserting generated identities",
        ),
        (
            required.identity_allocation,
            effective.identity_allocation,
            implemented.identity_allocation,
            "generated identity allocation",
        ),
        (
            required.default_expression,
            effective.default_expression,
            implemented.default_expression,
            "default expressions",
        ),
        (
            required.row_locks,
            effective.row_locks,
            implemented.row_locks,
            "row locks",
        ),
        (
            required.advisory_locks,
            effective.advisory_locks,
            implemented.advisory_locks,
            "advisory locks",
        ),
        (
            required.transaction_settings,
            effective.transaction_settings,
            implemented.transaction_settings,
            "transaction settings",
        ),
    ] {
        if available && !implementation {
            return Err(CompileError::InvalidStatement(
                "effective SQL support exceeds compiler support".into(),
            ));
        }
        if requirement && !available {
            return Err(CompileError::Unsupported(name));
        }
    }
    if effective.max_bind_parameters > implemented.max_bind_parameters {
        return Err(CompileError::InvalidStatement(
            "effective bind limit exceeds compiler support".into(),
        ));
    }
    if required.bind_parameters > effective.max_bind_parameters {
        return Err(CompileError::BindLimitExceeded {
            limit: effective.max_bind_parameters,
        });
    }
    Ok(())
}

pub(crate) fn compile_insert(
    syntax: Syntax,
    effective: &SqlSupport,
    insert: Insert,
) -> Result<CompiledQuery, CompileError> {
    insert.validate()?;
    let parts = insert.into_parts();
    let mut writer = SqlWriter::new(effective.max_bind_parameters);
    writer.sql.push_str("INSERT INTO ");
    write_table(&mut writer, &parts.table);
    writer.sql.push_str(" (");
    for (index, column) in parts.columns.iter().enumerate() {
        comma(&mut writer, index);
        writer.identifier(column.name().as_str());
    }
    writer.sql.push(')');
    if parts.insert_generated_identity {
        if let Some(clause) = syntax.generated_identity_override {
            writer.sql.push_str(clause);
        }
    }
    writer.sql.push_str(" VALUES ");
    for (row_index, row) in parts.rows.into_iter().enumerate() {
        comma(&mut writer, row_index);
        writer.sql.push('(');
        for (column_index, (column, value)) in parts.columns.iter().zip(row).enumerate() {
            comma(&mut writer, column_index);
            write_expression(&mut writer, syntax, column.storage(), value)?;
        }
        writer.sql.push(')');
    }
    write_returning(&mut writer, &parts.returning);
    Ok(writer.finish())
}

pub(crate) fn compile_upsert(
    syntax: Syntax,
    effective: &SqlSupport,
    upsert: Upsert,
) -> Result<CompiledQuery, CompileError> {
    upsert.validate()?;
    let parts = upsert.into_parts();
    let mut writer = SqlWriter::new(effective.max_bind_parameters);
    writer.sql.push_str("INSERT INTO ");
    write_table(&mut writer, &parts.table);
    writer.sql.push_str(" (");
    for (index, assignment) in parts.insert.iter().enumerate() {
        comma(&mut writer, index);
        writer.identifier(assignment.column.name().as_str());
    }
    writer.sql.push(')');
    if parts.insert_generated_identity {
        if let Some(clause) = syntax.generated_identity_override {
            writer.sql.push_str(clause);
        }
    }
    writer.sql.push_str(" VALUES (");
    for (index, assignment) in parts.insert.into_iter().enumerate() {
        comma(&mut writer, index);
        write_expression(
            &mut writer,
            syntax,
            assignment.column.storage(),
            assignment.value,
        )?;
    }
    writer.sql.push_str(") ON CONFLICT (");
    for (index, column) in parts.conflict.iter().enumerate() {
        comma(&mut writer, index);
        writer.identifier(column.name().as_str());
    }
    writer.sql.push_str(") DO UPDATE SET ");
    for (index, assignment) in parts.update.into_iter().enumerate() {
        comma(&mut writer, index);
        writer.identifier(assignment.column.name().as_str());
        writer.sql.push_str(" = ");
        write_expression(
            &mut writer,
            syntax,
            assignment.column.storage(),
            assignment.value,
        )?;
    }
    if let Some(condition) = parts.condition {
        writer.sql.push_str(" WHERE ");
        write_comparison(
            &mut writer,
            syntax,
            condition.column.storage(),
            condition.op,
            ResolvedPredicateValue::Bind {
                storage: condition.column.storage(),
                value: condition.value,
            },
            |writer| {
                write_current(writer, &condition.column);
                Ok(())
            },
        )?;
    }
    write_returning(&mut writer, &parts.returning);
    Ok(writer.finish())
}

pub(crate) fn compile_update(
    syntax: Syntax,
    effective: &SqlSupport,
    update: Update,
) -> Result<CompiledQuery, CompileError> {
    update.validate()?;
    let parts = update.into_parts();
    let mut writer = SqlWriter::new(effective.max_bind_parameters);
    writer.sql.push_str("UPDATE ");
    write_table(&mut writer, &parts.table);
    writer.sql.push_str(" SET ");
    for (index, assignment) in parts.assignments.into_iter().enumerate() {
        comma(&mut writer, index);
        writer.identifier(assignment.column.name().as_str());
        writer.sql.push_str(" = ");
        write_update_expression(&mut writer, syntax, assignment.column, assignment.value)?;
    }
    write_mutation_predicate(
        &mut writer,
        syntax,
        &parts.table,
        parts.scope,
        parts.predicate,
    )?;
    write_returning(&mut writer, &parts.returning);
    Ok(writer.finish())
}

pub(crate) fn compile_delete(
    syntax: Syntax,
    effective: &SqlSupport,
    delete: Delete,
) -> Result<CompiledQuery, CompileError> {
    delete.validate()?;
    let parts = delete.into_parts();
    let mut writer = SqlWriter::new(effective.max_bind_parameters);
    writer.sql.push_str("DELETE FROM ");
    write_table(&mut writer, &parts.table);
    write_mutation_predicate(
        &mut writer,
        syntax,
        &parts.table,
        parts.scope,
        parts.predicate,
    )?;
    write_returning(&mut writer, &parts.returning);
    Ok(writer.finish())
}

pub(crate) fn compile_vector_search(
    syntax: Syntax,
    effective: &SqlSupport,
    search: VectorSearchStatement,
) -> Result<CompiledQuery, CompileError> {
    search.validate()?;
    let parts = search.into_parts();
    let mut writer = SqlWriter::new(effective.max_bind_parameters);
    let query = writer.bind(parts.query)?;
    writer.sql.push_str("SELECT ");
    write_search_projection(&mut writer, &parts.projection);
    writer.sql.push_str(", ");
    (syntax.vector_distance)(&mut writer, &parts.vector, parts.metric, query)?;
    writer.sql.push_str(" AS ");
    writer.identifier("_distance");
    writer.sql.push_str(" FROM ");
    write_table_reference(&mut writer, &parts.table);
    if !matches!(parts.predicate, ResolvedPredicate::Const(true)) {
        writer.sql.push_str(" WHERE ");
        write_predicate(&mut writer, syntax, parts.predicate)?;
    }
    writer.sql.push_str(" ORDER BY ");
    (syntax.vector_distance)(&mut writer, &parts.vector, parts.metric, query)?;
    writer.sql.push_str(", ");
    write_operand(
        &mut writer,
        syntax,
        &ResolvedOperand::Column(parts.identity),
    )?;
    writer.sql.push_str(" LIMIT ");
    writer.write_param(Value::from(parts.limit))?;
    Ok(writer.finish())
}

pub(crate) fn write_search_projection(
    writer: &mut SqlWriter,
    projection: &[crate::sql::statement::ReturnedColumn],
) {
    for (index, selected) in projection.iter().enumerate() {
        comma(writer, index);
        write_column_reference(writer, &selected.column);
        writer.sql.push_str(" AS ");
        writer.identifier(
            selected
                .alias
                .as_ref()
                .map_or_else(|| selected.column.name().as_str(), |alias| alias.as_str()),
        );
    }
}

pub(crate) fn compile_select(
    syntax: Syntax,
    effective: &SqlSupport,
    select: SelectStatement,
) -> Result<CompiledQuery, CompileError> {
    select.validate()?;
    let summary = select.summary();
    let parts = select.into_parts();
    let mut writer = SqlWriter::new(effective.max_bind_parameters);
    use crate::sql::statement::SelectSummary;
    match summary {
        Some(SelectSummary::Count) => writer.sql.push_str("SELECT COUNT(*) AS \"count\" FROM ("),
        Some(SelectSummary::Exists) => writer.sql.push_str("SELECT CASE WHEN EXISTS ("),
        None => {}
    }
    writer.sql.push_str("SELECT ");
    if parts.distinct {
        writer.sql.push_str("DISTINCT ");
    }
    for (index, selected) in parts.projection.into_iter().enumerate() {
        comma(&mut writer, index);
        write_operand(&mut writer, syntax, &selected.expression)?;
        writer.sql.push_str(" AS ");
        writer.identifier(selected.alias.as_str());
    }
    writer.sql.push_str(" FROM ");
    write_table_reference(&mut writer, &parts.table);
    for join in parts.joins {
        writer.sql.push_str(match join.kind {
            crate::sql::JoinKind::Inner => " INNER JOIN ",
            crate::sql::JoinKind::Left => " LEFT JOIN ",
        });
        write_table_reference(&mut writer, &join.table);
        writer.sql.push_str(" ON ");
        write_predicate(&mut writer, syntax, join.on)?;
    }
    if !matches!(parts.predicate, ResolvedPredicate::Const(true)) {
        writer.sql.push_str(" WHERE ");
        write_predicate(&mut writer, syntax, parts.predicate)?;
    }
    if !parts.group_by.is_empty() {
        writer.sql.push_str(" GROUP BY ");
        for (index, expression) in parts.group_by.iter().enumerate() {
            comma(&mut writer, index);
            write_operand(&mut writer, syntax, expression)?;
        }
    }
    if !matches!(parts.having, ResolvedPredicate::Const(true)) {
        writer.sql.push_str(" HAVING ");
        write_predicate(&mut writer, syntax, parts.having)?;
    }
    if !parts.order_by.is_empty() {
        writer.sql.push_str(" ORDER BY ");
        for (index, order) in parts.order_by.iter().enumerate() {
            comma(&mut writer, index);
            write_operand(&mut writer, syntax, &order.expression)?;
            writer.sql.push_str(match order.direction {
                crate::sql::Direction::Ascending => " ASC",
                crate::sql::Direction::Descending => " DESC",
            });
            writer.sql.push_str(match order.nulls {
                crate::sql::NullOrder::First => " NULLS FIRST",
                crate::sql::NullOrder::Last => " NULLS LAST",
            });
        }
    }
    if let Some(limit) = parts.limit {
        writer.sql.push_str(" LIMIT ");
        writer.write_param(Value::from(limit))?;
    } else if parts.offset.is_some() {
        writer.sql.push_str(syntax.offset_without_limit);
    }
    if let Some(offset) = parts.offset {
        writer.sql.push_str(" OFFSET ");
        writer.write_param(Value::from(offset))?;
    }
    match &parts.lock {
        crate::sql::statement::RowLock::None => {}
        crate::sql::statement::RowLock::WriteTargets => {
            writer.sql.push_str(syntax.write_target_lock);
        }
        crate::sql::statement::RowLock::Required { of } => {
            let clause = syntax
                .required_row_lock
                .ok_or(CompileError::Unsupported("row locks"))?;
            writer.sql.push_str(clause);
            for (index, alias) in of.iter().enumerate() {
                writer.sql.push_str(if index == 0 { " OF " } else { ", " });
                writer.identifier(alias.as_str());
            }
        }
    }
    match summary {
        Some(SelectSummary::Count) => writer.sql.push_str(") AS \"summary\""),
        Some(SelectSummary::Exists) => writer.sql.push_str(") THEN 1 ELSE 0 END AS \"count\""),
        None => {}
    }
    Ok(writer.finish())
}

pub(crate) fn write_table_reference(writer: &mut SqlWriter, table: &Table) {
    write_table(writer, table);
    if let Some(alias) = table.alias() {
        writer.sql.push_str(" AS ");
        writer.identifier(alias.as_str());
    }
}

fn write_mutation_predicate(
    writer: &mut SqlWriter,
    syntax: Syntax,
    table: &Table,
    scope: MutationScope,
    predicate: ResolvedPredicate,
) -> Result<(), CompileError> {
    match scope {
        MutationScope::Matching => {
            if !matches!(predicate, ResolvedPredicate::Const(true)) {
                writer.sql.push_str(" WHERE ");
                write_predicate(writer, syntax, predicate)?;
            }
        }
        MutationScope::First { target } => {
            writer.sql.push_str(" WHERE ");
            writer.identifier(target.name().as_str());
            writer.sql.push_str(" = (SELECT ");
            writer.identifier(target.name().as_str());
            writer.sql.push_str(" FROM ");
            write_table(writer, table);
            if !matches!(predicate, ResolvedPredicate::Const(true)) {
                writer.sql.push_str(" WHERE ");
                write_predicate(writer, syntax, predicate)?;
            }
            writer.sql.push_str(" ORDER BY ");
            writer.identifier(target.name().as_str());
            writer.sql.push_str(" LIMIT 1");
            writer.sql.push_str(syntax.write_target_lock);
            writer.sql.push(')');
        }
    }
    Ok(())
}

pub(crate) fn write_predicate(
    writer: &mut SqlWriter,
    syntax: Syntax,
    predicate: ResolvedPredicate,
) -> Result<(), CompileError> {
    match predicate {
        ResolvedPredicate::Const(value) => {
            writer.sql.push_str(if value { "TRUE" } else { "FALSE" })
        }
        ResolvedPredicate::And(children) => {
            write_connective(writer, syntax, children, true)?;
        }
        ResolvedPredicate::Or(children) => {
            write_connective(writer, syntax, children, false)?;
        }
        ResolvedPredicate::Not(child) => {
            writer.sql.push_str("NOT (");
            write_predicate(writer, syntax, *child)?;
            writer.sql.push(')');
        }
        ResolvedPredicate::Compare { lhs, op, rhs } => {
            write_comparison(writer, syntax, lhs.storage()?, op, rhs, |writer| {
                write_operand(writer, syntax, &lhs)
            })?;
        }
        ResolvedPredicate::Membership { lhs, op, values } => {
            let storage = lhs.storage()?;
            if storage == StorageType::Json && syntax.structural_json_equality {
                writer.sql.push('(');
                for (index, value) in values.into_iter().enumerate() {
                    if index > 0 {
                        writer.sql.push_str(if op == MembershipOp::In {
                            " OR "
                        } else {
                            " AND "
                        });
                    }
                    write_structural_json_equality(
                        writer,
                        syntax,
                        |writer| write_operand(writer, syntax, &lhs),
                        ResolvedPredicateValue::Bind { storage, value },
                        op == MembershipOp::NotIn,
                    )?;
                }
                writer.sql.push(')');
                return Ok(());
            }
            if storage.decimal().is_some() && syntax.exact_decimal_functions {
                writer.sql.push('(');
                for (index, value) in values.into_iter().enumerate() {
                    if index > 0 {
                        writer.sql.push_str(if op == MembershipOp::In {
                            " OR "
                        } else {
                            " AND "
                        });
                    }
                    write_exact_decimal_equality(
                        writer,
                        syntax,
                        |writer| write_operand(writer, syntax, &lhs),
                        ResolvedPredicateValue::Bind { storage, value },
                        op == MembershipOp::NotIn,
                    )?;
                }
                writer.sql.push(')');
                return Ok(());
            }
            write_operand(writer, syntax, &lhs)?;
            writer.sql.push_str(if op == MembershipOp::In {
                " IN ("
            } else {
                " NOT IN ("
            });
            for (index, value) in values.into_iter().enumerate() {
                comma(writer, index);
                write_bind(writer, syntax, storage, value)?;
            }
            writer.sql.push(')');
        }
        ResolvedPredicate::Pattern {
            lhs,
            op,
            value,
            escape,
        } => {
            write_operand(writer, syntax, &lhs)?;
            let insensitive = matches!(op, PatternOp::ILike | PatternOp::NotILike);
            let negated = matches!(op, PatternOp::NotLike | PatternOp::NotILike);
            if negated {
                writer.sql.push_str(" NOT");
            }
            writer.sql.push(' ');
            writer.sql.push_str(if insensitive {
                syntax.insensitive_like
            } else {
                "LIKE"
            });
            writer.sql.push(' ');
            writer.write_param(Value::from(value))?;
            if let Some(escape) = escape {
                writer.sql.push_str(" ESCAPE ");
                writer.write_param(Value::from(escape))?;
            }
            if insensitive {
                writer.sql.push_str(syntax.insensitive_like_suffix);
            }
        }
        ResolvedPredicate::IsNull { operand, negated } => {
            write_operand(writer, syntax, &operand)?;
            writer
                .sql
                .push_str(if negated { " IS NOT NULL" } else { " IS NULL" });
        }
    }
    Ok(())
}

fn write_comparison(
    writer: &mut SqlWriter,
    syntax: Syntax,
    storage: StorageType,
    op: CompareOp,
    rhs: ResolvedPredicateValue,
    write_lhs: impl FnOnce(&mut SqlWriter) -> Result<(), CompileError>,
) -> Result<(), CompileError> {
    if matches!(op, CompareOp::Eq | CompareOp::Ne) {
        if storage == StorageType::Json && syntax.structural_json_equality {
            return write_structural_json_equality(
                writer,
                syntax,
                write_lhs,
                rhs,
                op == CompareOp::Ne,
            );
        }
        if storage.decimal().is_some() && syntax.exact_decimal_functions {
            return write_exact_decimal_equality(
                writer,
                syntax,
                write_lhs,
                rhs,
                op == CompareOp::Ne,
            );
        }
    }
    write_lhs(writer)?;
    writer.sql.push_str(match op {
        CompareOp::Eq => " = ",
        CompareOp::Ne => " != ",
        CompareOp::Lt => " < ",
        CompareOp::Lte => " <= ",
        CompareOp::Gt => " > ",
        CompareOp::Gte => " >= ",
    });
    match rhs {
        ResolvedPredicateValue::Operand(rhs) => write_operand(writer, syntax, &rhs)?,
        ResolvedPredicateValue::Bind { storage, value } => {
            write_bind(writer, syntax, storage, value)?
        }
    }
    Ok(())
}

fn write_exact_decimal_equality(
    writer: &mut SqlWriter,
    syntax: Syntax,
    write_lhs: impl FnOnce(&mut SqlWriter) -> Result<(), CompileError>,
    rhs: ResolvedPredicateValue,
    negated: bool,
) -> Result<(), CompileError> {
    if negated {
        writer.sql.push_str("NOT ");
    }
    writer.sql.push_str("zeroship_decimal_equal(");
    write_lhs(writer)?;
    writer.sql.push_str(", ");
    match rhs {
        ResolvedPredicateValue::Operand(rhs) => write_operand(writer, syntax, &rhs)?,
        ResolvedPredicateValue::Bind { storage, value } => {
            write_bind(writer, syntax, storage, value)?;
        }
    }
    writer.sql.push(')');
    Ok(())
}

fn write_structural_json_equality(
    writer: &mut SqlWriter,
    syntax: Syntax,
    write_lhs: impl FnOnce(&mut SqlWriter) -> Result<(), CompileError>,
    rhs: ResolvedPredicateValue,
    negated: bool,
) -> Result<(), CompileError> {
    if negated {
        writer.sql.push_str("NOT ");
    }
    writer.sql.push_str("zeroship_json_equal(");
    write_lhs(writer)?;
    writer.sql.push_str(", ");
    match rhs {
        ResolvedPredicateValue::Operand(rhs) => write_operand(writer, syntax, &rhs)?,
        ResolvedPredicateValue::Bind { storage, value } => {
            write_bind(writer, syntax, storage, value)?;
        }
    }
    writer.sql.push(')');
    Ok(())
}

pub(crate) fn write_operand(
    writer: &mut SqlWriter,
    syntax: Syntax,
    operand: &ResolvedOperand,
) -> Result<(), CompileError> {
    match operand {
        ResolvedOperand::RowPresence => writer.sql.push('1'),
        ResolvedOperand::Column(column) => write_column_reference(writer, column),
        ResolvedOperand::Comparison(comparison) => {
            writer.sql.push('(');
            write_comparison(
                writer,
                syntax,
                comparison.column.storage(),
                comparison.op,
                ResolvedPredicateValue::Bind {
                    storage: comparison.column.storage(),
                    value: comparison.value.clone(),
                },
                |writer| {
                    write_column_reference(writer, &comparison.column);
                    Ok(())
                },
            )?;
            writer.sql.push(')');
        }
        ResolvedOperand::Aggregate {
            function,
            column,
            distinct,
        } => {
            writer.sql.push_str(function.as_sql());
            writer.sql.push('(');
            if *distinct {
                writer.sql.push_str("DISTINCT ");
            }
            if let Some(column) = column {
                write_operand(writer, syntax, &ResolvedOperand::Column(column.clone()))?;
            } else {
                writer.sql.push('*');
            }
            writer.sql.push(')');
            if *function == crate::sql::AggregateFunc::Avg {
                writer.sql.push_str(syntax.average_suffix);
            }
        }
    }
    Ok(())
}

pub(crate) fn write_column_reference(writer: &mut SqlWriter, column: &Column) {
    if let Some(alias) = column.table().alias() {
        writer.identifier(alias.as_str());
        writer.sql.push('.');
    }
    writer.identifier(column.name().as_str());
}

fn write_connective(
    writer: &mut SqlWriter,
    syntax: Syntax,
    children: Vec<ResolvedPredicate>,
    conjunction: bool,
) -> Result<(), CompileError> {
    writer.sql.push('(');
    for (index, child) in children.into_iter().enumerate() {
        if index > 0 {
            writer
                .sql
                .push_str(if conjunction { " AND " } else { " OR " });
        }
        write_predicate(writer, syntax, child)?;
    }
    writer.sql.push(')');
    Ok(())
}

fn write_update_expression(
    writer: &mut SqlWriter,
    syntax: Syntax,
    column: Column,
    expression: Expression,
) -> Result<(), CompileError> {
    match expression {
        Expression::Arithmetic {
            column,
            operator,
            operand,
        } => {
            if let Some(storage) = column
                .storage()
                .decimal()
                .filter(|_| syntax.exact_decimal_functions)
            {
                writer.sql.push_str(match operator {
                    ArithmeticOperator::Add => "zeroship_decimal_add(",
                    ArithmeticOperator::Subtract => "zeroship_decimal_subtract(",
                    ArithmeticOperator::Multiply => "zeroship_decimal_multiply(",
                });
                writer.identifier(column.name().as_str());
                writer.sql.push_str(", ");
                writer.write_param(operand)?;
                writer.sql.push_str(", ");
                writer.sql.push_str(&storage.precision().to_string());
                writer.sql.push_str(", ");
                writer.sql.push_str(&storage.scale().to_string());
                writer.sql.push(')');
                return Ok(());
            }
            writer.identifier(column.name().as_str());
            writer.sql.push_str(match operator {
                ArithmeticOperator::Add => " + ",
                ArithmeticOperator::Subtract => " - ",
                ArithmeticOperator::Multiply => " * ",
            });
            writer.write_param(operand)?;
            writer.sql.push_str(syntax.numeric_cast);
        }
        Expression::ArrayMutation {
            column,
            operator,
            operand,
        } => {
            let slot = writer.bind(operand)?;
            (syntax.array_mutation)(writer, &column, operator, slot)?;
        }
        expression => write_expression(writer, syntax, column.storage(), expression)?,
    }
    Ok(())
}

fn write_returning(writer: &mut SqlWriter, returning: &[crate::sql::statement::ReturnedColumn]) {
    if returning.is_empty() {
        return;
    }
    writer.sql.push_str(" RETURNING ");
    for (index, field) in returning.iter().enumerate() {
        comma(writer, index);
        writer.identifier(field.column.name().as_str());
        if let Some(alias) = &field.alias {
            writer.sql.push_str(" AS ");
            writer.identifier(alias.as_str());
        }
    }
}

fn comma(writer: &mut SqlWriter, index: usize) {
    if index > 0 {
        writer.sql.push_str(", ");
    }
}

pub(crate) fn write_table(writer: &mut SqlWriter, table: &Table) {
    writer.identifier(table.namespace().as_str());
    writer.sql.push('.');
    writer.identifier(table.name().as_str());
}

fn write_current(writer: &mut SqlWriter, column: &Column) {
    write_table(writer, column.table());
    writer.sql.push('.');
    writer.identifier(column.name().as_str());
}

fn write_bind(
    writer: &mut SqlWriter,
    syntax: Syntax,
    storage: StorageType,
    value: Value,
) -> Result<(), CompileError> {
    let cast = match storage {
        StorageType::Timestamp => syntax.timestamp_cast,
        StorageType::Vector => syntax.vector_cast,
        StorageType::ExactDecimal(_) => syntax.numeric_cast,
        StorageType::Array(ArrayElement::Text) => syntax
            .text_array_cast
            .ok_or(CompileError::Unsupported("native array storage"))?,
        _ => "",
    };
    writer.write_param(value)?;
    writer.sql.push_str(cast);
    Ok(())
}

fn write_expression(
    writer: &mut SqlWriter,
    syntax: Syntax,
    storage: StorageType,
    expression: Expression,
) -> Result<(), CompileError> {
    match expression {
        Expression::Bind(value) => {
            if let Some(decimal) = storage.decimal().filter(|_| syntax.exact_decimal_functions) {
                writer.sql.push_str("zeroship_decimal_quantize(");
                writer.write_param(value)?;
                writer.sql.push_str(", ");
                writer.sql.push_str(&decimal.precision().to_string());
                writer.sql.push_str(", ");
                writer.sql.push_str(&decimal.scale().to_string());
                writer.sql.push(')');
            } else {
                write_bind(writer, syntax, storage, value)?;
            }
        }
        Expression::Null => writer.sql.push_str("NULL"),
        Expression::Default => writer.sql.push_str("DEFAULT"),
        Expression::Current(column) => write_current(writer, &column),
        Expression::Incoming(column) => {
            writer.sql.push_str("EXCLUDED.");
            writer.identifier(column.name().as_str());
        }
        Expression::Increment { column, step } => {
            write_current(writer, &column);
            writer.sql.push_str(" + ");
            writer.write_param(Value::from(step))?;
        }
        Expression::Arithmetic { .. } | Expression::ArrayMutation { .. } => {
            return Err(CompileError::InvalidStatement(
                "update operator reached a non-update expression position".into(),
            ));
        }
        Expression::CurrentTimestamp => writer.sql.push_str(syntax.current_timestamp),
        Expression::DatabaseTimestamp { offset_micros } => {
            (syntax.database_timestamp)(writer, offset_micros)?;
        }
    }
    Ok(())
}
