//! SQLite search statement compilation. Extension availability is a host concern.
use crate::{compile::*, descriptors::VectorMetric, sqlite_values::vec_to_le_bytes, value::Value};

/// Rank stored vectors after filtering, without requiring a shadow index.
/// The descriptor determines physical columns and the protected projection.
#[allow(clippy::too_many_arguments)]
pub fn build_vector_search(
    namespace: &str,
    collection: &str,
    column: &str,
    query: &[f32],
    k: usize,
    metric: VectorMetric,
    filter: &Value,
    schema: &Value,
) -> Result<BuiltQuery, QueryError> {
    let distance = match metric {
        VectorMetric::Cosine => "vec_distance_cosine",
        VectorMetric::L2 => "vec_distance_l2",
        VectorMetric::InnerProduct => {
            return Err(QueryError::InvalidFilter(
                "SQLite does not support inner-product vector search".into(),
            ))
        }
    };
    let k = i64::try_from(k).map_err(|_| {
        QueryError::InvalidFilter("vector search limit exceeds the integer range".into())
    })?;
    let mut params = vec![Value::Bytes(vec_to_le_bytes(query))];
    let where_expr = build_where_with_dialect(filter, &mut params, schema, SqlDialect::Sqlite)?;
    // SQLite numbers named parameters by first appearance in the statement.
    // LIMIT occurs after the filter, so its binding must follow filter values.
    params.push(Value::from(k));
    let limit = params.len();
    let select = build_masked_aware_select_expr_for_table_alias(schema, "t")?;
    let namespace = quote_ident(namespace);
    let table = quote_ident(collection);
    let column = quote_ident(&value_column_for_field(column, schema));
    let filter = if where_expr.is_empty() {
        String::new()
    } else {
        format!(" AND {where_expr}")
    };
    Ok(BuiltQuery {
        sql: format!(
            "SELECT {select}, {distance}(t.{column}, $1) AS _distance \
            FROM {namespace}.{table} t \
            WHERE t.{column} IS NOT NULL{filter} \
            ORDER BY _distance, t.\"id\" LIMIT ${limit}"
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
            "embedding": {"type": "vector", "storage": {"valueColumn": "stored_embedding"}}
        });
        let query = build_vector_search(
            "app1",
            "users",
            "embedding",
            &[1.0, 2.0],
            5,
            VectorMetric::Cosine,
            &crate::value!({"ssn": "***1234"}),
            &schema,
        )
        .unwrap();
        assert!(!query.sql.contains("t.*"));
        assert!(query.sql.contains(r#""t"."ssn" AS "ssn""#));
        assert!(!query.sql.contains(&raw_column_name("ssn")));
        assert!(query
            .sql
            .contains("vec_distance_cosine(t.\"stored_embedding\", $1)"));
        assert!(query.sql.contains("LIMIT $3"));
        assert!(query.sql.contains("$2"));
        assert_eq!(
            query.params,
            vec![
                Value::Bytes(vec_to_le_bytes(&[1.0, 2.0])),
                "***1234".into(),
                5.into()
            ]
        );
    }
}
