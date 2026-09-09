//! Bind native values without formatting ordinary columns as text.
use rusqlite::types::{ToSql, ToSqlOutput};
use zeroship_data_sql::value::Value;

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
            Value::Timestamp(v) => ToSqlOutput::Owned(SqliteValue::Integer(*v)),
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
    use zeroship_data_sql::{SchemaName, compile, value};

    #[test]
    fn json_scalars_keep_their_json_type_and_text_remains_text() {
        let db = rusqlite::Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE documents (id TEXT, created_at TEXT, updated_at TEXT, created_by TEXT, updated_by TEXT, version INTEGER, deleted_at TEXT, payload TEXT, label TEXT)")
            .unwrap();
        let schema = value!({"payload":{"type":"json"}, "label":{"type":"string"}});
        let query = compile::build_insert_with_dialect(
            &SchemaName::new("main").unwrap(),
            "documents",
            &schema,
            &value!({"payload":"true", "label":"true"}),
            compile::SqlDialect::Sqlite,
        )
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
