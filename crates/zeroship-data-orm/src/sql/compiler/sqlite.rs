use super::{
    CompileError, CompiledQuery, IdentityPlan, IdentityReadPlan, Requirements, SqlCompiler,
    SqlSupport, SqlWriter,
};
use crate::{
    sql::statement::{ArrayOperator, Column, IdentityRequest, Statement},
    value::Value,
};

#[derive(Clone, Copy, Debug)]
pub struct SqliteCompiler;

const SUPPORT: SqlSupport = SqlSupport {
    relational_reads: true,
    aggregate_reads: true,
    vector_search: true,
    inner_product_vector_search: false,
    spatial_search: true,
    explicit_conflict_target: true,
    conditional_conflict_update: true,
    returning: true,
    insert_generated_identity: true,
    identity_allocation: true,
    default_expression: false,
    max_bind_parameters: super::SQLITE_BIND_LIMIT,
};

const SYNTAX: super::shared::Syntax = super::shared::Syntax {
    current_timestamp: "(strftime('%Y-%m-%dT%H:%M:%fZ','now'))",
    generated_identity_override: None,
    timestamp_cast: "",
    vector_cast: "",
    numeric_cast: "",
    first_row_lock: "",
    insensitive_like: "LIKE",
    insensitive_like_suffix: " COLLATE NOCASE",
    average_suffix: "",
    structural_json_equality: true,
    vector_distance: write_vector_distance,
    array_mutation: write_array_mutation,
};

fn write_vector_distance(
    writer: &mut SqlWriter,
    column: &Column,
    metric: crate::sql::descriptors::VectorMetric,
    query: super::ParameterSlot,
) -> Result<(), CompileError> {
    if metric == crate::sql::descriptors::VectorMetric::InnerProduct {
        return Err(CompileError::Unsupported("inner-product vector search"));
    }
    writer.sql.push_str(match metric {
        crate::sql::descriptors::VectorMetric::Cosine => "vec_distance_cosine(",
        crate::sql::descriptors::VectorMetric::L2 => "vec_distance_l2(",
        crate::sql::descriptors::VectorMetric::InnerProduct => unreachable!("refused above"),
    });
    super::shared::write_column_reference(writer, column);
    writer.sql.push_str(", ");
    writer.write_bound(query);
    writer.sql.push(')');
    Ok(())
}

fn compile_spatial_near(
    search: crate::sql::statement::SpatialNearStatement,
    effective: &SqlSupport,
) -> Result<CompiledQuery, CompileError> {
    search.validate()?;
    let parts = search.into_parts();
    let mut writer = SqlWriter::new(effective.max_bind_parameters);
    writer.sql.push_str("SELECT ");
    super::shared::write_search_projection(&mut writer, &parts.projection);
    writer.sql.push_str(" FROM ");
    super::shared::write_table_reference(&mut writer, &parts.table);
    if !matches!(
        &parts.predicate,
        crate::sql::statement::ResolvedPredicate::Const(true)
    ) {
        writer.sql.push_str(" WHERE ");
        super::shared::write_predicate(&mut writer, SYNTAX, parts.predicate)?;
    }
    Ok(writer.finish())
}

fn write_column(writer: &mut SqlWriter, column: &Column) {
    writer.identifier(column.name().as_str());
}

fn write_array_mutation(
    writer: &mut SqlWriter,
    column: &Column,
    operator: ArrayOperator,
    operand: super::ParameterSlot,
) -> Result<(), CompileError> {
    writer.sql.push_str("CASE WHEN ");
    write_column(writer, column);
    writer.sql.push_str(" IS NULL OR json_type(");
    write_column(writer, column);
    writer.sql.push_str(") = 'null' THEN ");
    write_column(writer, column);
    writer.sql.push_str(" WHEN json_type(");
    write_column(writer, column);
    writer.sql.push_str(") = 'array' THEN ");
    match operator {
        ArrayOperator::Push => write_append(writer, column, operand),
        ArrayOperator::Pull => {
            writer.sql.push('(');
            write_elements(writer, column);
            writer.sql.push_str(" SELECT json_group_array(json(element) ORDER BY position) FROM __zs_elements WHERE NOT zeroship_json_equal(element, ");
            writer.write_bound(operand);
            writer.sql.push_str("))");
        }
        ArrayOperator::AddToSet => {
            writer.sql.push_str("CASE WHEN EXISTS (");
            write_elements(writer, column);
            writer
                .sql
                .push_str(" SELECT 1 FROM __zs_elements WHERE zeroship_json_equal(element, ");
            writer.write_bound(operand);
            writer.sql.push_str(")) THEN ");
            write_column(writer, column);
            writer.sql.push_str(" ELSE ");
            write_append(writer, column, operand);
            writer.sql.push_str(" END");
        }
    }
    writer.sql.push_str(" ELSE json('') END");
    Ok(())
}

fn write_elements(writer: &mut SqlWriter, column: &Column) {
    writer.sql.push_str("WITH __zs_input(document) AS (SELECT ");
    write_column(writer, column);
    writer.sql.push_str("), __zs_elements(position, element) AS (SELECT __zs_each.key, __zs_input.document -> __zs_each.key FROM __zs_input, json_each(__zs_input.document) AS __zs_each)");
}

fn write_append(writer: &mut SqlWriter, column: &Column, operand: super::ParameterSlot) {
    writer.sql.push_str("json_insert(");
    write_column(writer, column);
    writer.sql.push_str(", '$[#]', json(");
    writer.write_bound(operand);
    writer.sql.push_str("))");
}

impl SqlCompiler for SqliteCompiler {
    fn support(&self) -> SqlSupport {
        SUPPORT
    }

    fn requirements(&self, statement: &Statement) -> Requirements {
        let mut requirements = Requirements::for_statement(statement);
        if let Statement::SpatialNear(search) = statement {
            requirements.bind_parameters =
                super::shared::predicate_binds(&search.parts().predicate);
        }
        requirements
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
        self.check(&self.requirements(&statement), effective)?;
        match statement {
            Statement::Select(statement) => {
                super::shared::compile_select(SYNTAX, effective, statement)
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
