use super::{CompileError, CompiledQuery, SqlWriter};
use crate::sql::{
    statement::{Column, Expression, Statement, StorageType, Table},
    CompareOp,
};
use crate::value::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SqlSupport {
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
        }
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
}

pub(crate) fn check(
    implemented: SqlSupport,
    required: &Requirements,
    effective: &SqlSupport,
) -> Result<(), CompileError> {
    for (requirement, available, implementation, name) in [
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
    }
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
        Expression::CurrentTimestamp => writer.sql.push_str(syntax.current_timestamp),
    }
    Ok(())
}
