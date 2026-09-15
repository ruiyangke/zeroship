//! Bind native values without formatting ordinary columns as text.
use crate::value::Value;
use rusqlite::types::{ToSql, ToSqlOutput};

pub(crate) struct Parameter<'a>(pub &'a Value);
impl ToSql for Parameter<'_> {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        use rusqlite::types::Value as SqliteValue;
        Ok(match self.0 {
            Value::Null => ToSqlOutput::Owned(SqliteValue::Null),
            Value::Bool(v) => ToSqlOutput::Owned(SqliteValue::Integer(i64::from(*v))),
            Value::Number(v) if v.as_i64().is_some() => {
                ToSqlOutput::Owned(SqliteValue::Integer(v.as_i64().unwrap()))
            }
            Value::Number(v) if v.as_u64().is_some() => {
                return Err(rusqlite::Error::ToSqlConversionFailure(
                    "unsigned integer exceeds SQLite integer range".into(),
                ));
            }
            Value::Number(v) => {
                ToSqlOutput::Owned(SqliteValue::Real(v.as_f64().ok_or_else(|| {
                    rusqlite::Error::ToSqlConversionFailure("invalid database number".into())
                })?))
            }
            // SQLite stores an instant as canonical fixed-width text, which is
            // what the registration's codec produces before a value reaches
            // here. A native timestamp that arrives unencoded is bound in the
            // same form, and a sub-millisecond one is refused rather than
            // silently bound as something the column cannot match.
            Value::TimestampMicros(v) => ToSqlOutput::Owned(SqliteValue::Text(
                crate::sql::temporal::exact_timestamp_millis(*v)
                    .and_then(crate::sql::temporal::format_timestamp_millis)
                    .ok_or_else(|| {
                        rusqlite::Error::ToSqlConversionFailure(
                            "SQLite stores whole milliseconds within the portable calendar".into(),
                        )
                    })?,
            )),
            Value::Json(v) => {
                serde_json::from_str::<&serde_json::value::RawValue>(v)
                    .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
                ToSqlOutput::Borrowed(v.as_str().into())
            }
            Value::String(v) | Value::Decimal(v) => ToSqlOutput::Borrowed(v.as_str().into()),
            Value::Bytes(v) => ToSqlOutput::Borrowed(v.as_slice().into()),
            Value::Array(_) | Value::Object(_) => ToSqlOutput::Owned(SqliteValue::Text(
                serde_json::to_string(self.0)
                    .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?,
            )),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        sql::{
            registration::SqlRegistration,
            statement::{
                Expression, Insert, InsertParts, ReturnedColumn, Statement, StorageType, Table,
            },
            Ident, IdentRole, SchemaName,
        },
        value,
    };

    #[test]
    fn json_scalars_keep_their_json_type_and_text_remains_text() {
        let db_file = tempfile::NamedTempFile::new().unwrap();
        let db = rusqlite::Connection::open(db_file.path()).unwrap();
        db.execute_batch("CREATE TABLE documents (id TEXT, created_at TEXT, updated_at TEXT, created_by TEXT, updated_by TEXT, version INTEGER, deleted_at TEXT, payload TEXT, label TEXT)")
            .unwrap();
        let registration = SqlRegistration::sqlite();
        let table = Table::new(
            SchemaName::new("main").unwrap(),
            Ident::parse_as("documents", IdentRole::Collection).unwrap(),
            [
                (
                    Ident::parse_as("payload", IdentRole::StoredColumn).unwrap(),
                    StorageType::Json,
                ),
                (
                    Ident::parse_as("label", IdentRole::StoredColumn).unwrap(),
                    StorageType::Text,
                ),
            ],
        )
        .unwrap();
        let payload = table.column("payload").unwrap();
        let label = table.column("label").unwrap();
        let query = registration
            .compile(Statement::Insert(
                Insert::new(InsertParts {
                    table,
                    columns: vec![payload.clone(), label.clone()],
                    rows: vec![vec![
                        Expression::Bind(
                            registration
                                .encode(StorageType::Json, value!("true"))
                                .unwrap(),
                        ),
                        Expression::Bind(value!("true")),
                    ]],
                    returning: vec![
                        ReturnedColumn {
                            column: payload,
                            alias: None,
                        },
                        ReturnedColumn {
                            column: label,
                            alias: None,
                        },
                    ],
                    insert_generated_identity: false,
                })
                .unwrap(),
            ))
            .unwrap();
        let parameters: Vec<_> = query.params.iter().map(Parameter).collect();
        let result: (String, String) = db
            .query_row(&query.sql, rusqlite::params_from_iter(parameters), |row| {
                Ok((row.get("payload")?, row.get("label")?))
            })
            .unwrap();
        assert_eq!(result, ("\"true\"".into(), "true".into()));
        let kind: String = db
            .query_row("SELECT json_type(payload) FROM documents", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(kind, "text");
        assert!(Parameter(&Value::from(u64::MAX)).to_sql().is_err());
    }
}
