use super::{CompileError, CompiledQuery, SqlWriter};
use crate::sql::{
    BindBudget, CompareOp,
    statement::{Column, Expression, Statement, StorageType, Table},
};
use crate::value::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SqlSupport {
    pub explicit_conflict_target: bool,
    pub conditional_conflict_update: bool,
    pub returning: bool,
    pub insert_generated_identity: bool,
    pub default_expression: bool,
    pub max_bind_parameters: usize,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Requirements {
    pub explicit_conflict_target: bool,
    pub conditional_conflict_update: bool,
    pub returning: bool,
    pub insert_generated_identity: bool,
    pub default_expression: bool,
    pub bind_parameters: usize,
}

impl Requirements {
    pub fn for_statement(statement: &Statement) -> Self {
        match statement {
            Statement::Upsert(upsert) => {
                let parts = upsert.parts();
                let values = parts.insert.iter().chain(&parts.update).map(|a| &a.value);
                Self {
                    explicit_conflict_target: true,
                    conditional_conflict_update: !parts.conditions.is_empty(),
                    returning: !parts.returning.is_empty(),
                    insert_generated_identity: parts.insert_generated_identity,
                    default_expression: values.clone().any(|v| matches!(v, Expression::Default)),
                    bind_parameters: values
                        .filter(|v| matches!(v, Expression::Bind(_) | Expression::Increment { .. }))
                        .count()
                        + parts.conditions.len(),
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
}

#[derive(Debug)]
pub struct PostgresCompiler;
#[derive(Debug)]
pub struct SqliteCompiler;

#[derive(Clone, Copy)]
enum Syntax {
    Postgres,
    Sqlite,
}

fn support(syntax: Syntax) -> SqlSupport {
    SqlSupport {
        explicit_conflict_target: true,
        conditional_conflict_update: true,
        returning: true,
        insert_generated_identity: true,
        default_expression: matches!(syntax, Syntax::Postgres),
        max_bind_parameters: match syntax {
            Syntax::Postgres => BindBudget::POSTGRES.max(),
            Syntax::Sqlite => BindBudget::SQLITE.max(),
        },
    }
}

fn check(
    syntax: Syntax,
    required: &Requirements,
    effective: &SqlSupport,
) -> Result<(), CompileError> {
    let implemented = support(syntax);
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

impl SqlCompiler for PostgresCompiler {
    fn support(&self) -> SqlSupport {
        support(Syntax::Postgres)
    }
    fn check(
        &self,
        requirements: &Requirements,
        effective: &SqlSupport,
    ) -> Result<(), CompileError> {
        check(Syntax::Postgres, requirements, effective)
    }
    fn compile(
        &self,
        statement: Statement,
        effective: &SqlSupport,
    ) -> Result<CompiledQuery, CompileError> {
        compile(Syntax::Postgres, statement, effective)
    }
}

impl SqlCompiler for SqliteCompiler {
    fn support(&self) -> SqlSupport {
        support(Syntax::Sqlite)
    }
    fn check(
        &self,
        requirements: &Requirements,
        effective: &SqlSupport,
    ) -> Result<(), CompileError> {
        check(Syntax::Sqlite, requirements, effective)
    }
    fn compile(
        &self,
        statement: Statement,
        effective: &SqlSupport,
    ) -> Result<CompiledQuery, CompileError> {
        compile(Syntax::Sqlite, statement, effective)
    }
}

fn compile(
    syntax: Syntax,
    statement: Statement,
    effective: &SqlSupport,
) -> Result<CompiledQuery, CompileError> {
    check(syntax, &Requirements::for_statement(&statement), effective)?;
    match statement {
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
            if parts.insert_generated_identity && matches!(syntax, Syntax::Postgres) {
                writer.sql.push_str(" OVERRIDING SYSTEM VALUE");
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
            for (index, condition) in parts.conditions.into_iter().enumerate() {
                writer.sql.push_str(if index == 0 { " WHERE " } else { " AND " });
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
            if !parts.returning.is_empty() {
                writer.sql.push_str(" RETURNING ");
                for (index, field) in parts.returning.iter().enumerate() {
                    comma(&mut writer, index);
                    writer.identifier(field.column.name().as_str());
                    if let Some(alias) = &field.alias {
                        writer.sql.push_str(" AS ");
                        writer.identifier(alias.as_str());
                    }
                }
            }
            Ok(writer.finish())
        }
    }
}

fn comma(writer: &mut SqlWriter, index: usize) {
    if index > 0 {
        writer.sql.push_str(", ");
    }
}

fn write_table(writer: &mut SqlWriter, table: &Table) {
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
    if storage == StorageType::Timestamp && matches!(syntax, Syntax::Postgres) {
        writer.sql.push_str("::timestamptz");
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
        Expression::CurrentTimestamp => writer.sql.push_str(match syntax {
            Syntax::Postgres => "NOW()",
            Syntax::Sqlite => "(strftime('%Y-%m-%dT%H:%M:%fZ','now'))",
        }),
    }
    Ok(())
}
