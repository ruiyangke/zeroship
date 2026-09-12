use super::{CompileError, CompiledQuery, Requirements, SqlCompiler, SqlSupport};
use crate::sql::{statement::Statement, BindBudget};

#[derive(Clone, Copy, Debug)]
pub struct PostgresCompiler;

const SUPPORT: SqlSupport = SqlSupport {
    explicit_conflict_target: true,
    conditional_conflict_update: true,
    returning: true,
    insert_generated_identity: true,
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
}
