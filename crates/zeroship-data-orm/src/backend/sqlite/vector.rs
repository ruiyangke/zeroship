//! SQLite vector capability validation; SQL compilation lives in data-sql.
use zeroship_data_orm::error::DbError;
use crate::sql::descriptors::VectorMetric;

/// Reject [`VectorMetric::InnerProduct`] with a typed `DbError` on
/// SQLite. The PG arm continues to support all three metrics via
/// pgvector opclasses (`vector_ip_ops`).
pub(crate) fn reject_inner_product(metric: VectorMetric) -> Result<(), DbError> {
    if matches!(metric, VectorMetric::InnerProduct) {
        return Err(DbError::Configuration {
            code: "vector_unsupported_metric",
            message: "db: SQLite vector backend does not support inner-product distance \
                 (vec0 supports cosine and l2 only). Use cosine for normalised \
                 embeddings; production inner-product workloads run on pgvector."
                .to_string(),
            hint: Some(
                "switch the column to { metric: 'cosine' } or run the app on the \
                 Postgres backend (pgvector)"
                    .to_string(),
            ),
        });
    }
    Ok(())
}

#[cfg(test)]
use crate::sql::sqlite_values::vec_to_le_bytes;

#[cfg(test)]
mod tests {
    //! Unit tests for the SQL primitives. The shapes are pinned
    //! against the canonical forms documented in this module's
    //! rustdoc + the upstream `sqlite-vec` examples.

    use super::*;

    #[test]
    fn reject_inner_product_returns_typed_error() {
        let err = reject_inner_product(VectorMetric::InnerProduct)
            .expect_err("inner product must reject");
        match err {
            DbError::Configuration { code, .. } => {
                assert_eq!(code, "vector_unsupported_metric");
            }
            other => panic!("expected Configuration error, got {other:?}"),
        }
        assert!(reject_inner_product(VectorMetric::Cosine).is_ok());
        assert!(reject_inner_product(VectorMetric::L2).is_ok());
    }

    #[test]
    fn vec_to_le_bytes_is_native_f32_layout() {
        // 1.0f32 = 0x3F800000 → LE bytes [0x00, 0x00, 0x80, 0x3F].
        let bytes = vec_to_le_bytes(&[1.0]);
        assert_eq!(bytes, vec![0x00, 0x00, 0x80, 0x3F]);
        // Empty input → empty bytes.
        assert!(vec_to_le_bytes(&[]).is_empty());
        // Length is 4 × dims.
        assert_eq!(vec_to_le_bytes(&[1.0, 2.0, 3.0]).len(), 12);
    }
}
