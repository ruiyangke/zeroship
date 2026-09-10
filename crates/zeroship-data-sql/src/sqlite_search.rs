//! SQLite search statement compilation. Extension availability is a host concern.
use crate::{compile::*, sqlite_values::vec_to_le_bytes, value::Value};

fn vec_table_name(collection: &str, column: &str) -> String {
    format!("{collection}__vec_{column}")
}

/// Compile a vec0 query with native vector, limit and filter parameters.
/// The descriptor determines the protected projection of the base table.
pub fn build_vector_search(
    namespace: &str,
    collection: &str,
    column: &str,
    query: &[f32],
    k: usize,
    filter: &Value,
    schema: &Value,
) -> Result<BuiltQuery, QueryError> {
    let k = i64::try_from(k).map_err(|_| {
        QueryError::InvalidFilter("vector search limit exceeds the integer range".into())
    })?;
    let mut params = vec![Value::Bytes(vec_to_le_bytes(query)), Value::from(k)];
    let where_expr = build_where_with_dialect(filter, &mut params, schema, SqlDialect::Sqlite)?;
    let select = build_masked_aware_select_expr_for_table_alias(schema, "t")?;
    let namespace = quote_ident(namespace);
    let table = quote_ident(collection);
    let vector_table = quote_ident(&vec_table_name(collection, column));
    let column = quote_ident(column);
    let filter = if where_expr.is_empty() {
        String::new()
    } else {
        format!(" AND {where_expr}")
    };
    Ok(BuiltQuery {
        sql: format!(
            "SELECT {select}, v.distance AS _distance \
            FROM {namespace}.{table} t \
            JOIN {namespace}.{vector_table} v ON t.rowid = v.rowid \
            WHERE v.{column} MATCH $1 AND k = $2{filter} ORDER BY v.distance"
        ),
        params,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protects_projection_and_binds_vector_before_filter() {
        let schema = crate::value!({
            "ssn": {"type": "string", "mask": {"kind": "last4", "classification": "spi"}},
            "embedding": {"type": "vector"}
        });
        let query = build_vector_search(
            "app1",
            "users",
            "embedding",
            &[1.0, 2.0],
            5,
            &crate::value!({"ssn": "***1234"}),
            &schema,
        )
        .unwrap();
        assert!(!query.sql.contains("t.*"));
        assert!(query.sql.contains(r#""t"."ssn" AS "ssn""#));
        assert!(!query.sql.contains(&raw_column_name("ssn")));
        assert!(query.sql.contains("MATCH $1 AND k = $2"));
        assert!(query.sql.contains("$3"));
        assert_eq!(
            query.params,
            vec![
                Value::Bytes(vec_to_le_bytes(&[1.0, 2.0])),
                5.into(),
                "***1234".into()
            ]
        );
        assert_eq!(vec_table_name("docs", "embedding"), "docs__vec_embedding");
    }
}
