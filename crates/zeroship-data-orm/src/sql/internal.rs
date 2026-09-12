//! SQL for ORM-owned protection operations. Caller values remain parameters.
use crate::value::Value;
use crate::sql::{compile::{BuiltQuery, SqlDialect, quote_ident_for_dialect}};

pub const AUDIT_UNMASK_TABLE: &str = "__zeroship_audit_unmask";

fn placeholder(dialect: SqlDialect, index: usize) -> String {
    match dialect {
        SqlDialect::Postgres => format!("${index}"),
        SqlDialect::Sqlite => format!("?{index}"),
        SqlDialect::Mysql => "?".into(),
    }
}

pub fn raw_column(
    namespace: &str,
    collection: &str,
    column: &str,
    key_column: &str,
    key: Value,
    dialect: SqlDialect,
) -> BuiltQuery {
    BuiltQuery {
        sql: format!(
            "SELECT {} FROM {}.{} WHERE {} = {}",
            quote_ident_for_dialect(column, dialect),
            quote_ident_for_dialect(namespace, dialect),
            quote_ident_for_dialect(collection, dialect),
            quote_ident_for_dialect(key_column, dialect),
            placeholder(dialect, 1)
        ),
        params: vec![key],
    }
}

pub fn unmask_audit(namespace: &str, dialect: SqlDialect, params: Vec<Value>) -> BuiltQuery {
    let columns = [
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
    let placeholders = (1..=columns.len())
        .map(|index| placeholder(dialect, index))
        .collect::<Vec<_>>()
        .join(", ");
    BuiltQuery {
        sql: format!(
            "INSERT INTO {}.{} ({}) VALUES ({placeholders})",
            quote_ident_for_dialect(namespace, dialect),
            quote_ident_for_dialect(AUDIT_UNMASK_TABLE, dialect),
            columns
                .map(|column| quote_ident_for_dialect(column, dialect))
                .join(", ")
        ),
        params,
    }
}
