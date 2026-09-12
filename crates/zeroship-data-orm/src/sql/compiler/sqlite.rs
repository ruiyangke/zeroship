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
pub struct SqliteCompiler;

const SUPPORT: SqlSupport = SqlSupport {
    explicit_conflict_target: true,
    conditional_conflict_update: true,
    returning: true,
    insert_generated_identity: true,
    identity_allocation: true,
    default_expression: false,
    max_bind_parameters: BindBudget::SQLITE.max(),
};

const SYNTAX: super::shared::Syntax = super::shared::Syntax {
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
        let writer = || SqlWriter::new(effective.max_bind_parameters);

        let mut reservation = writer();
        reservation.sql.push_str("UPDATE ");
        super::shared::write_table(&mut reservation, request.table());
        reservation.sql.push_str(" SET ");
        reservation.identifier(request.column().name().as_str());
        reservation.sql.push_str(" = ");
        reservation.identifier(request.column().name().as_str());
        reservation.sql.push_str(" WHERE FALSE");

        let mut maximum = writer();
        maximum.sql.push_str("SELECT COALESCE(MAX(");
        maximum.identifier(request.column().name().as_str());
        maximum.sql.push_str("), 0) AS ");
        maximum.identifier("id");
        maximum.sql.push_str(" FROM ");
        super::shared::write_table(&mut maximum, request.table());

        let mut counter_exists = writer();
        counter_exists.sql.push_str("SELECT ");
        counter_exists.identifier("name");
        counter_exists.sql.push_str(" FROM ");
        counter_exists.identifier(request.table().namespace().as_str());
        counter_exists
            .sql
            .push_str(".sqlite_schema WHERE type = 'table' AND name = 'sqlite_sequence'");

        let mut counter = writer();
        counter.sql.push_str("SELECT ");
        counter.identifier("seq");
        counter.sql.push_str(" AS ");
        counter.identifier("id");
        counter.sql.push_str(" FROM ");
        counter.identifier(request.table().namespace().as_str());
        counter.sql.push_str(".sqlite_sequence WHERE name = ");
        counter.write_param(Value::from(request.table().name().as_str()))?;

        Ok(IdentityPlan {
            reservation: Some(reservation.finish()),
            allocation: IdentityReadPlan::MaximumAndCounter {
                maximum: maximum.finish(),
                counter_exists: counter_exists.finish(),
                counter: counter.finish(),
            },
        })
    }
}
