use crate::{
    error::DbError,
    exec::{exec_mutation, exec_query},
    tx_route::TxRoute,
};
use crate::value::Value;
use crate::sql::{compile::SqlDialect, identity::{self, Allocation}};

pub(super) fn requires_allocation(schema: &Value, payload: &Value) -> bool {
    identity::is_generated(schema)
        && payload.as_array().map_or_else(
            || super::write_pipeline::upsert_requires_conflict_probe(schema, payload),
            |documents| {
                documents
                    .iter()
                    .any(|doc| super::write_pipeline::upsert_requires_conflict_probe(schema, doc))
            },
        )
}

pub(super) async fn reserve_writer(
    route: &TxRoute,
    collection: &str,
    schema: &Value,
) -> Result<(), DbError> {
    if !route.in_tx() {
        return Err(DbError::internal(
            "identity allocation requires a transaction",
        ));
    }
    if route.dialect() == SqlDialect::Sqlite {
        exec_mutation(
            route,
            identity::reserve_sqlite_writer(route.schema(), collection, schema)?,
        )
        .await?;
    }
    Ok(())
}

pub(super) async fn allocate(
    route: &TxRoute,
    collection: &str,
    schema: &Value,
    count: usize,
) -> Result<Vec<Value>, DbError> {
    if !route.in_tx() {
        return Err(DbError::internal(
            "identity allocation requires a transaction",
        ));
    }
    match identity::allocation(route.schema(), collection, schema, route.dialect(), count)? {
        Allocation::Sequence(query) => {
            let rows = exec_query(route, query).await?;
            if rows.len() != count {
                return Err(DbError::internal(
                    "identity sequence returned an incomplete batch",
                ));
            }
            rows.into_iter()
                .map(|row| {
                    row["id"].as_i64().map(Value::from).ok_or_else(|| {
                        DbError::internal("identity sequence did not return an integer")
                    })
                })
                .collect()
        }
        Allocation::RowId {
            maximum,
            has_sequence,
            sequence,
        } => {
            let maximum = exec_query(route, maximum).await?;
            let mut last = maximum
                .first()
                .and_then(|row| row["id"].as_i64())
                .ok_or_else(|| DbError::internal("identity maximum did not return an integer"))?
                .max(0);
            if !exec_query(route, has_sequence).await?.is_empty() {
                let rows = exec_query(route, sequence).await?;
                if let Some(row) = rows.first() {
                    last =
                        last.max(row["id"].as_i64().ok_or_else(|| {
                            DbError::internal("identity counter is not an integer")
                        })?);
                }
            }
            (1..=count)
                .map(|offset| {
                    last.checked_add(offset as i64)
                        .map(Value::from)
                        .ok_or_else(|| {
                            DbError::validation(
                                "generated_identity_exhausted",
                                "the collection identity counter is exhausted",
                            )
                        })
                })
                .collect()
        }
    }
}
