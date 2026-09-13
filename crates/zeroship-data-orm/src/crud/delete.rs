use super::{predicate, resolved::ResolvedTable, update};
use crate::schema::FieldMap;
use crate::sql::{
    lifecycle::WriteAssignments,
    mapping::QueryError,
    registration::SqlRegistration,
    statement::{
        Delete, DeleteParts, MutationScope, ResolvedOperand, ResolvedPredicate, ReturnedColumn,
        Statement, Update, UpdateParts,
    },
    SchemaName,
};

#[derive(Clone, Copy)]
pub(crate) enum Cardinality {
    One,
    Many,
}

pub(crate) struct Builder<'a> {
    namespace: &'a SchemaName,
    collection: &'a str,
    schema: &'a FieldMap,
    registration: &'a SqlRegistration,
}

impl<'a> Builder<'a> {
    pub(crate) const fn new(
        namespace: &'a SchemaName,
        collection: &'a str,
        schema: &'a FieldMap,
        registration: &'a SqlRegistration,
    ) -> Self {
        Self {
            namespace,
            collection,
            schema,
            registration,
        }
    }

    pub(crate) fn hard(
        &self,
        filter: predicate::Input,
        cardinality: Cardinality,
    ) -> Result<crate::sql::compiler::CompiledQuery, QueryError> {
        let resolved = ResolvedTable::new(
            self.namespace,
            self.collection,
            self.schema,
            self.registration,
        )?;
        let predicate = filter.resolve(self.schema, &resolved, self.registration)?;
        let scope = scope(cardinality, self.schema, &resolved)?;
        let returning = returning(cardinality, self.schema, &resolved)?;
        self.registration
            .compile(Statement::Delete(Delete::new(DeleteParts {
                table: resolved.table,
                predicate,
                scope,
                returning,
            })?))
            .map_err(Into::into)
    }

    pub(crate) fn lifecycle(
        &self,
        filter: predicate::Input,
        marker: &str,
        match_deleted: bool,
        generated: &WriteAssignments,
        cardinality: Cardinality,
    ) -> Result<crate::sql::compiler::CompiledQuery, QueryError> {
        let resolved = ResolvedTable::new(
            self.namespace,
            self.collection,
            self.schema,
            self.registration,
        )?;
        let marker = resolved
            .table
            .column(&crate::sql::mapping::value_column_for_field(
                marker,
                self.schema,
            ))?;
        let predicate = ResolvedPredicate::and(vec![
            filter.resolve(self.schema, &resolved, self.registration)?,
            ResolvedPredicate::IsNull {
                operand: ResolvedOperand::Column(marker),
                negated: match_deleted,
            },
        ]);
        let assignments = update::resolve_generated(generated, &resolved, self.registration)?;
        let scope = scope(cardinality, self.schema, &resolved)?;
        let returning = returning(cardinality, self.schema, &resolved)?;
        self.registration
            .compile(Statement::Update(Update::new(UpdateParts {
                table: resolved.table,
                assignments,
                predicate,
                scope,
                returning,
            })?))
            .map_err(Into::into)
    }
}

fn scope(
    cardinality: Cardinality,
    schema: &FieldMap,
    resolved: &ResolvedTable,
) -> Result<MutationScope, QueryError> {
    Ok(match cardinality {
        Cardinality::One => MutationScope::First {
            target: resolved
                .table
                .column(&crate::sql::mapping::value_column_for_field("id", schema))?,
        },
        Cardinality::Many => MutationScope::Matching,
    })
}

fn returning(
    cardinality: Cardinality,
    schema: &FieldMap,
    resolved: &ResolvedTable,
) -> Result<Vec<ReturnedColumn>, QueryError> {
    match cardinality {
        Cardinality::One => resolved.returning(schema),
        Cardinality::Many => Ok(Vec::new()),
    }
}
