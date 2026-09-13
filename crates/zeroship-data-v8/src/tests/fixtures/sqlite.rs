//! Inspect persisted SQLite cells independently of ORM decoding.
use zeroship_core::app_id::LOCAL_DEV_APP_ID;

pub(crate) struct Inspector(rusqlite::Connection);
impl Inspector {
    pub(crate) fn open(directory: &std::path::Path) -> Self {
        let connection =
            rusqlite::Connection::open_in_memory().expect("open inspection connection");
        connection
            .execute(
                &format!(r#"ATTACH DATABASE ?1 AS "{LOCAL_DEV_APP_ID}""#),
                [directory
                    .join(format!("zs-{LOCAL_DEV_APP_ID}.sqlite"))
                    .to_str()
                    .expect("fixture path")],
            )
            .expect("attach runtime database");
        Self(connection)
    }
    pub(crate) async fn query(
        &self,
        sql: &str,
        params: &[&str],
    ) -> rusqlite::Result<Vec<Vec<Option<String>>>> {
        let mut statement = self.0.prepare(sql)?;
        let columns = statement.column_count();
        let rows = statement
            .query_map(rusqlite::params_from_iter(params), |row| {
                (0..columns)
                    .map(|index| {
                        Ok(match row.get_ref(index)? {
                            rusqlite::types::ValueRef::Null => None,
                            rusqlite::types::ValueRef::Integer(value) => Some(value.to_string()),
                            rusqlite::types::ValueRef::Real(value) => Some(value.to_string()),
                            rusqlite::types::ValueRef::Text(value) => {
                                Some(String::from_utf8(value.to_vec()).expect("SQLite text"))
                            }
                            rusqlite::types::ValueRef::Blob(_) => {
                                panic!("inspect BLOB values using SQLite hex()")
                            }
                        })
                    })
                    .collect()
            })?
            .collect();
        rows
    }
}
pub(crate) struct Rows {
    pub rows: Vec<Vec<rusqlite::types::Value>>,
}
impl Inspector {
    pub(crate) async fn query_typed(&self, sql: &str, params: &[&str]) -> rusqlite::Result<Rows> {
        let mut statement = self.0.prepare(sql)?;
        let columns = statement.column_count();
        let rows = statement
            .query_map(rusqlite::params_from_iter(params), |row| {
                (0..columns).map(|index| row.get(index)).collect()
            })?
            .collect::<rusqlite::Result<_>>()?;
        Ok(Rows { rows })
    }
}
