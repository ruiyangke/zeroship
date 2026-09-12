use super::{CompileError, CompiledQuery, Requirements, SqlCompiler, SqlSupport};
use crate::sql::{BindBudget, statement::Statement};

#[derive(Clone, Copy, Debug)]
pub struct SqliteCompiler;

const SUPPORT: SqlSupport = SqlSupport {
    explicit_conflict_target: true,
    conditional_conflict_update: true,
    returning: true,
    insert_generated_identity: true,
    default_expression: false,
    max_bind_parameters: BindBudget::SQLITE.max(),
};

const SYNTAX: super::upsert::Syntax = super::upsert::Syntax {
    current_timestamp: "(strftime('%Y-%m-%dT%H:%M:%fZ','now'))",
    generated_identity_override: None,
    timestamp_cast: "",
    vector_cast: "",
};

impl SqlCompiler for SqliteCompiler {
    fn support(&self) -> SqlSupport {
        SUPPORT
    }

    fn check(
        &self,
        requirements: &Requirements,
        effective: &SqlSupport,
    ) -> Result<(), CompileError> {
        super::upsert::check(SUPPORT, requirements, effective)
    }

    fn compile(
        &self,
        statement: Statement,
        effective: &SqlSupport,
    ) -> Result<CompiledQuery, CompileError> {
        super::upsert::compile(SYNTAX, SUPPORT, statement, effective)
    }
}
