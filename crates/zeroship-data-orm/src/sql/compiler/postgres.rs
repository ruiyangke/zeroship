use super::{
    CompileError, CompiledQuery, IdentityPlan, IdentityReadPlan, Requirements, SqlCompiler,
    SqlSupport, SqlWriter,
};
use crate::{
    sql::{
        statement::{IdentityRequest, Statement},
        BindBudget,
    },
    value::Value,
};

#[derive(Clone, Copy, Debug)]
pub struct PostgresCompiler;

const SUPPORT: SqlSupport = SqlSupport {
    explicit_conflict_target: true,
    conditional_conflict_update: true,
    returning: true,
    insert_generated_identity: true,
    identity_allocation: true,
    default_expression: true,
    max_bind_parameters: BindBudget::POSTGRES.max(),
};

const SYNTAX: super::shared::Syntax = super::shared::Syntax {
    current_timestamp: "NOW()",
    generated_identity_override: Some(" OVERRIDING SYSTEM VALUE"),
    timestamp_cast: "::timestamptz",
    vector_cast: "::vector",
};

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
        super::shared::compile(SYNTAX, SUPPORT, statement, effective)
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
