use super::{CompileError, CompiledQuery, SqlWriter};
use crate::sql::{
    statement::{
        ArithmeticOperator, ArrayOperator, Column, Expression, MutationScope, ResolvedOperand,
        ResolvedPredicate, ResolvedPredicateValue, Statement, StorageType, Table,
    },
    CompareOp, MembershipOp, PatternOp,
};
use crate::value::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SqlSupport {
    pub relational_reads: bool,
    pub aggregate_reads: bool,
    pub explicit_conflict_target: bool,
    pub conditional_conflict_update: bool,
    pub returning: bool,
    pub insert_generated_identity: bool,
    pub identity_allocation: bool,
    pub default_expression: bool,
    pub max_bind_parameters: usize,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Requirements {
    pub relational_reads: bool,
    pub aggregate_reads: bool,
    pub explicit_conflict_target: bool,
    pub conditional_conflict_update: bool,
    pub returning: bool,
    pub insert_generated_identity: bool,
    pub identity_allocation: bool,
    pub default_expression: bool,
    pub bind_parameters: usize,
}

impl Requirements {
    pub fn for_statement(statement: &Statement) -> Self {
        match statement {
            Statement::Select(select) => {
                let parts = select.parts();
                Self {
                    relational_reads: !parts.joins.is_empty(),
                    aggregate_reads: parts.projection.iter().any(|selected| {
                        matches!(selected.expression, ResolvedOperand::Aggregate { .. })
                    }) || !parts.group_by.is_empty()
                        || predicate_has_aggregate(&parts.having),
                    bind_parameters: parts
                        .joins
                        .iter()
                        .map(|join| predicate_binds(&join.on))
                        .sum::<usize>()
                        + predicate_binds(&parts.predicate)
                        + predicate_binds(&parts.having)
                        + usize::from(parts.limit.is_some())
                        + usize::from(parts.offset.is_some()),
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
                    explicit_conflict_target: true,
                    conditional_conflict_update: parts.condition.is_some(),
                    returning: !parts.returning.is_empty(),
                    insert_generated_identity: parts.insert_generated_identity,
                    identity_allocation: false,
                    default_expression: values.clone().any(|v| matches!(v, Expression::Default)),
                    bind_parameters: values
                        .filter(|v| matches!(v, Expression::Bind(_) | Expression::Increment { .. }))
                        .count()
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
    ))
}

fn predicate_binds(predicate: &ResolvedPredicate) -> usize {
    match predicate {
        ResolvedPredicate::And(children) | ResolvedPredicate::Or(children) => {
            children.iter().map(predicate_binds).sum()
        }
        ResolvedPredicate::Not(child) => predicate_binds(child),
        ResolvedPredicate::Compare { rhs, .. } => {
            usize::from(matches!(rhs, ResolvedPredicateValue::Bind { .. }))
        }
        ResolvedPredicate::Pattern { escape, .. } => 1 + usize::from(escape.is_some()),
        ResolvedPredicate::Membership { values, .. } => values.len(),
        ResolvedPredicate::IsNull { .. } | ResolvedPredicate::Const(_) => 0,
    }
}

pub trait SqlCompiler: Send + Sync {
    fn support(&self) -> SqlSupport;
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
    pub(crate) generated_identity_override: Option<&'static str>,
    pub(crate) timestamp_cast: &'static str,
    pub(crate) vector_cast: &'static str,
    pub(crate) numeric_cast: &'static str,
    pub(crate) first_row_lock: &'static str,
    pub(crate) insensitive_like: &'static str,
    pub(crate) insensitive_like_suffix: &'static str,
    pub(crate) average_suffix: &'static str,
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

pub(crate) fn compile(
    syntax: Syntax,
    implemented: SqlSupport,
    statement: Statement,
    effective: &SqlSupport,
) -> Result<CompiledQuery, CompileError> {
    check(
        implemented,
        &Requirements::for_statement(&statement),
        effective,
    )?;
    match statement {
        Statement::Select(select) => compile_select(syntax, effective, select),
        Statement::Insert(insert) => {
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
        Statement::Upsert(upsert) => {
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
                write_current(&mut writer, &condition.column);
                writer.sql.push_str(match condition.op {
                    CompareOp::Eq => " = ",
                    CompareOp::Ne => " <> ",
                    CompareOp::Lt => " < ",
                    CompareOp::Lte => " <= ",
                    CompareOp::Gt => " > ",
                    CompareOp::Gte => " >= ",
                });
                write_bind(
                    &mut writer,
                    syntax,
                    condition.column.storage(),
                    condition.value,
                )?;
            }
            write_returning(&mut writer, &parts.returning);
            Ok(writer.finish())
        }
        Statement::Update(update) => {
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
        Statement::Delete(delete) => {
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
    }
}

fn compile_select(
    syntax: Syntax,
    effective: &SqlSupport,
    select: crate::sql::statement::SelectStatement,
) -> Result<CompiledQuery, CompileError> {
    select.validate()?;
    let parts = select.into_parts();
    let mut writer = SqlWriter::new(effective.max_bind_parameters);
    writer.sql.push_str("SELECT ");
    if parts.distinct {
        writer.sql.push_str("DISTINCT ");
    }
    for (index, selected) in parts.projection.into_iter().enumerate() {
        comma(&mut writer, index);
        write_operand(&mut writer, syntax, &selected.expression);
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
            write_operand(&mut writer, syntax, expression);
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
            write_operand(&mut writer, syntax, &order.expression);
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
    }
    if let Some(offset) = parts.offset {
        writer.sql.push_str(" OFFSET ");
        writer.write_param(Value::from(offset))?;
    }
    Ok(writer.finish())
}

fn write_table_reference(writer: &mut SqlWriter, table: &Table) {
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
            writer.sql.push_str(" LIMIT 1");
            writer.sql.push_str(syntax.first_row_lock);
            writer.sql.push(')');
        }
    }
    Ok(())
}

fn write_predicate(
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
            write_operand(writer, syntax, &lhs);
            writer.sql.push_str(match op {
                CompareOp::Eq => " = ",
                CompareOp::Ne => " != ",
                CompareOp::Lt => " < ",
                CompareOp::Lte => " <= ",
                CompareOp::Gt => " > ",
                CompareOp::Gte => " >= ",
            });
            match rhs {
                ResolvedPredicateValue::Operand(rhs) => write_operand(writer, syntax, &rhs),
                ResolvedPredicateValue::Bind { storage, value } => {
                    write_bind(writer, syntax, storage, value)?
                }
            }
        }
        ResolvedPredicate::Membership { lhs, op, values } => {
            let storage = lhs.storage()?;
            write_operand(writer, syntax, &lhs);
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
            write_operand(writer, syntax, &lhs);
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
            write_operand(writer, syntax, &operand);
            writer
                .sql
                .push_str(if negated { " IS NOT NULL" } else { " IS NULL" });
        }
    }
    Ok(())
}

fn write_operand(writer: &mut SqlWriter, syntax: Syntax, operand: &ResolvedOperand) {
    match operand {
        ResolvedOperand::Column(column) => {
            if let Some(alias) = column.table().alias() {
                writer.identifier(alias.as_str());
                writer.sql.push('.');
            }
            writer.identifier(column.name().as_str());
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
                write_operand(writer, syntax, &ResolvedOperand::Column(column.clone()));
            } else {
                writer.sql.push('*');
            }
            writer.sql.push(')');
            if *function == crate::sql::AggregateFunc::Avg {
                writer.sql.push_str(syntax.average_suffix);
            }
        }
    }
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
    writer.write_param(value)?;
    match storage {
        StorageType::Timestamp => writer.sql.push_str(syntax.timestamp_cast),
        StorageType::Vector => writer.sql.push_str(syntax.vector_cast),
        _ => {}
    }
    Ok(())
}

fn write_expression(
    writer: &mut SqlWriter,
    syntax: Syntax,
    storage: StorageType,
    expression: Expression,
) -> Result<(), CompileError> {
    match expression {
        Expression::Bind(value) => write_bind(writer, syntax, storage, value)?,
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
    }
    Ok(())
}
