//! Session-bound execution for compiler-provided identity allocation plans.

use crate::{
    error::DbError,
    exec::{exec_mutation, exec_query},
    sql::{
        compiler::{IdentityPlan, IdentityReadPlan},
        statement::IdentityRequest,
    },
    tx_route::TxRoute,
    value::Value,
};

pub(crate) struct ReservedIdentityAllocation<'a> {
    route: &'a TxRoute,
    count: usize,
    plan: IdentityReadPlan,
}

pub(crate) async fn reserve(
    route: &TxRoute,
    request: IdentityRequest,
) -> Result<ReservedIdentityAllocation<'_>, DbError> {
    if !route.in_tx() {
        return Err(DbError::internal(
            "identity allocation requires a transaction",
        ));
    }
    let count = request.count();
    let IdentityPlan {
        reservation,
        allocation,
    } = route
        .sql_registration()
        .compile_identity_allocation(request)
        .map_err(crate::sql::mapping::QueryError::from)?;
    if let Some(reservation) = reservation {
        exec_mutation(route, reservation).await?;
    }
    Ok(ReservedIdentityAllocation {
        route,
        count,
        plan: allocation,
    })
}

impl ReservedIdentityAllocation<'_> {
    pub(crate) async fn allocate(self) -> Result<Vec<Value>, DbError> {
        match self.plan {
            IdentityReadPlan::Rows(query) => {
                let rows = exec_query(self.route, query).await?;
                if rows.len() != self.count {
                    return Err(DbError::internal(
                        "identity sequence returned an incomplete batch",
                    ));
                }
                rows.into_iter().map(row_identity).collect()
            }
            IdentityReadPlan::MaximumAndCounter {
                maximum,
                counter_exists,
                counter,
            } => {
                let maximum = exec_query(self.route, maximum).await?;
                let mut last = maximum
                    .first()
                    .and_then(|row| row["id"].as_i64())
                    .ok_or_else(|| DbError::internal("identity maximum did not return an integer"))?
                    .max(0);
                if !exec_query(self.route, counter_exists).await?.is_empty() {
                    if let Some(row) = exec_query(self.route, counter).await?.first() {
                        last = last.max(row["id"].as_i64().ok_or_else(|| {
                            DbError::internal("identity counter is not an integer")
                        })?);
                    }
                }
                (1..=self.count)
                    .map(|offset| {
                        let offset = i64::try_from(offset).map_err(|_| exhausted())?;
                        last.checked_add(offset)
                            .map(Value::from)
                            .ok_or_else(exhausted)
                    })
                    .collect()
            }
        }
    }
}

fn row_identity(row: Value) -> Result<Value, DbError> {
    row["id"]
        .as_i64()
        .map(Value::from)
        .ok_or_else(|| DbError::internal("identity sequence did not return an integer"))
}

fn exhausted() -> DbError {
    DbError::validation(
        "generated_identity_exhausted",
        "the collection identity counter is exhausted",
    )
}
