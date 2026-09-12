//! ORM-owned statements for protected storage.

use super::resolved::ResolvedTable;
use crate::{
    sql::{
        compiler::CompiledQuery,
        mapping::QueryError,
        predicate::CompareOp,
        registration::SqlRegistration,
        statement::{
            Expression, Insert, InsertParts, ResolvedOperand, ResolvedPredicate,
            ResolvedPredicateValue, SelectParts, SelectStatement, SelectedExpression, Statement,
            StorageType, Table,
        },
        Ident, IdentRole, SchemaName,
    },
    value::Value,
};

pub const AUDIT_UNMASK_TABLE: &str = "__zeroship_audit_unmask";
const RAW_RESULT: &str = "_raw";
const AUDIT_COLUMNS: [&str; 9] = [
    "actor_id",
    "actor_role",
    "claimed_actor",
    "collection",
    "row_pk",
    "column",
    "classification",
    "reason",
    "outcome",
];

pub(crate) fn raw_column(
    namespace: &SchemaName,
    collection: &str,
    raw_column: &str,
    key: Value,
    schema: &Value,
    registration: &SqlRegistration,
) -> Result<CompiledQuery, QueryError> {
    let resolved = ResolvedTable::aliased(namespace, collection, "source", schema, registration)?;
    let raw = resolved
        .inputs
        .get(raw_column)
        .ok_or_else(|| invalid("raw column is not declared by the descriptor"))?;
    let identity = resolved
        .inputs
        .get("id")
        .ok_or_else(|| invalid("descriptor requires an id field"))?;
    let raw = resolved.table.column(&raw.column)?;
    let identity = resolved.table.column(&identity.column)?;
    let key = registration.encode(identity.storage(), key)?;
    let statement = SelectStatement::new(SelectParts {
        table: resolved.table,
        joins: Vec::new(),
        projection: vec![SelectedExpression {
            expression: ResolvedOperand::Column(raw),
            alias: Ident::parse_as(RAW_RESULT, IdentRole::Alias)
                .map_err(crate::sql::compiler::CompileError::from)?,
        }],
        predicate: ResolvedPredicate::Compare {
            lhs: ResolvedOperand::Column(identity.clone()),
            op: CompareOp::Eq,
            rhs: ResolvedPredicateValue::Bind {
                storage: identity.storage(),
                value: key,
            },
        },
        group_by: Vec::new(),
        having: ResolvedPredicate::Const(true),
        order_by: Vec::new(),
        limit: None,
        offset: None,
        distinct: false,
        lock: crate::sql::statement::RowLock::None,
    })?;
    registration
        .compile(Statement::Select(statement))
        .map_err(Into::into)
}

pub(crate) fn unmask_audit(
    namespace: &SchemaName,
    params: Vec<Value>,
    registration: &SqlRegistration,
) -> Result<CompiledQuery, QueryError> {
    if params.len() != AUDIT_COLUMNS.len() {
        return Err(invalid("unmask audit row has the wrong shape"));
    }
    let table_name = Ident::parse_as(AUDIT_UNMASK_TABLE, IdentRole::Collection)
        .map_err(crate::sql::compiler::CompileError::from)?;
    let physical = AUDIT_COLUMNS
        .iter()
        .map(|name| {
            Ident::parse_as(name, IdentRole::StoredColumn)
                .map(|name| (name, StorageType::Text))
                .map_err(crate::sql::compiler::CompileError::from)
        })
        .collect::<Result<Vec<_>, _>>()?;
    let table = Table::new(namespace.clone(), table_name, physical)?;
    let columns = AUDIT_COLUMNS
        .iter()
        .map(|name| table.column(name))
        .collect::<Result<Vec<_>, _>>()?;
    let row = params.into_iter().map(Expression::Bind).collect();
    registration
        .compile(Statement::Insert(Insert::new(InsertParts {
            table,
            columns,
            rows: vec![row],
            returning: Vec::new(),
            insert_generated_identity: false,
        })?))
        .map_err(Into::into)
}

fn invalid(message: &str) -> QueryError {
    QueryError::InvalidFilter(message.into())
}

#[cfg(test)]
mod tests {
    use crate::{
        sql::{registration::SqlRegistration, SchemaName},
        value::Value,
    };

    fn schema() -> Value {
        crate::value!({
            "id": { "type": "string", "primaryKey": true },
            "ssn": {
                "type": "string",
                "mask": { "kind": "last4", "classification": "spi" },
                "storage": { "valueColumn": "ssn", "rawColumn": "__zs_raw__ssn" }
            }
        })
    }

    #[test]
    fn raw_column_reads_compile_through_each_registered_backend() {
        let namespace = SchemaName::new("app").unwrap();
        for registration in [SqlRegistration::postgres(), SqlRegistration::sqlite()] {
            let query = super::raw_column(
                &namespace,
                "people",
                "__zs_raw__ssn",
                Value::from("person-a"),
                &schema(),
                &registration,
            )
            .unwrap();
            assert!(query.sql.contains("$1"));
            assert!(query.sql.contains("AS \"_raw\""));
            assert_eq!(query.params, vec![Value::from("person-a")]);
        }
    }

    #[test]
    fn the_platform_audit_insert_compiles_without_creator_table_admission() {
        let namespace = SchemaName::new("app").unwrap();
        let params: Vec<Value> = vec![
            "actor-a", "member", "member", "people", "person-a", "ssn", "spi", "support", "allowed",
        ]
        .into_iter()
        .map(Value::from)
        .collect();
        for registration in [SqlRegistration::postgres(), SqlRegistration::sqlite()] {
            let query = super::unmask_audit(&namespace, params.clone(), &registration).unwrap();
            assert!(query.sql.contains("\"app\".\"__zeroship_audit_unmask\""));
            assert_eq!(query.params.len(), params.len());
            for value in &params {
                assert!(query.params.contains(value));
            }
        }
    }
}
