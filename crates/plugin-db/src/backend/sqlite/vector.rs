//! SQLite vector helpers — pure-Rust flat scan over `BLOB` columns.
//!
//! **P4 PR 4** (`docs/proposals/p4-search-implementation-plan.md` §4.1).
//! This module owns the storage round-trip (`&[f32] ↔ &[u8]` via
//! `bytemuck::cast_slice`) and the three distance functions
//! (`cosine_distance`, `l2_distance`, `neg_inner_product`) plus the
//! [`distance`] dispatcher keyed by [`crate::backend::VectorMetric`].
//!
//! The actual `vector_search` flat scan lives in
//! [`crate::backend::sqlite::mod.rs`]'s `impl VectorIndex for
//! SqliteBackend` block — this module is the strict primitive layer so
//! the cryptography-of-math stays unit-testable in isolation.
//!
//! **Pure-Rust over `sqlite-vec` C extension** (plan §10, riskiest
//! decision Q-P4-D): we prefer ~60 LOC of distance math + flat scan
//! over bundling a C extension at compile time. Forking the SQLite
//! amalgamation per platform doubles the CI matrix; loading a runtime
//! `.so` defeats `rusqlite`'s `bundled` feature promise (the "no system
//! libsqlite3" invariant). The flat scan is acceptable at dev scale
//! (≤50k rows, ≤1024 dims, ≤100ms latency); production vector
//! workloads run on pgvector via the PG backend.
//!
//! **Little-endian assumption**: `bytemuck::cast_slice::<f32, u8>` /
//! its inverse use the host's native endianness. Every platform the
//! workspace targets (x86_64, aarch64, riscv64, wasm32) is
//! little-endian, so the on-disk layout matches the PG arm's pgvector
//! binary format byte-for-byte (pgvector also stores LE f32s). A
//! big-endian target would silently swap the byte order on read — out
//! of scope for P4 (no big-endian target ships from this workspace).

use bytemuck::Pod;

use crate::backend::VectorMetric;
use crate::error::DbError;

/// Emit the column-level DDL for a SQLite `t.vector(dims)` field.
///
/// **P4 PR 4** (`docs/proposals/p4-search-implementation-plan.md` §4.1
/// + §4.4): SQLite stores vectors as `BLOB` of native little-endian
/// `[f32]`. The dimension contract is enforced by a `CHECK(length(...)
/// = 4 * dims)` constraint at the column-DDL level so the engine
/// rejects wrong-dim INSERTs before they reach Rust — there is no
/// `vector(N)` parameterised type in SQLite to lean on (contrast PG's
/// pgvector). `NOT NULL` is part of the emission so a vector column
/// has a single canonical shape; a future PR that needs nullable
/// vectors can teach this helper to take a `nullable: bool` flag.
///
/// **Identifier safety**: `name` is `quote_ident`-style double-quoted,
/// doubling any embedded `"`. Callers should pass a name already
/// validated by [`crate::audit::validate_field_name`] for defence in
/// depth — this helper does not re-validate.
///
/// **No production caller yet** in this PR: the SQLite-side
/// `build_create_table_with_fks` (the orchestrator's column-DDL
/// emitter) is PG-flavoured today; a follow-up PR that teaches
/// `register_model::apply` to dispatch by dialect will route through
/// this helper. Until then the helper exists so tests, manual DDL,
/// and the future dialect-aware emitter share one canonical
/// CHECK-constraint shape.
#[allow(dead_code)]
pub(crate) fn sqlite_vector_column_ddl(name: &str, dims: i32) -> String {
    let quoted = format!("\"{}\"", name.replace('"', "\"\""));
    let byte_len = (dims as i64) * 4;
    format!("{quoted} BLOB CHECK(length({quoted}) = {byte_len}) NOT NULL")
}

/// Cosine distance — `1 - (a · b) / (||a|| · ||b||)`.
///
/// The denominator is clamped to `1e-12` to avoid `NaN` on a
/// zero-magnitude vector (which would otherwise produce `0 / 0`). A
/// zero query vector is degenerate input the SDK should reject upstream,
/// but the math here stays total.
pub(crate) fn cosine_distance(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    1.0 - dot / (na * nb).max(1e-12)
}

/// Euclidean (L2) distance — `sqrt(Σ (a_i - b_i)^2)`.
pub(crate) fn l2_distance(a: &[f32], b: &[f32]) -> f32 {
    a.iter()
        .zip(b)
        .map(|(x, y)| (x - y).powi(2))
        .sum::<f32>()
        .sqrt()
}

/// Negative inner product — `-(a · b)`. The "negative" framing makes
/// "smaller is better" hold across all three metrics, so the caller's
/// ORDER BY ASC works uniformly.
pub(crate) fn neg_inner_product(a: &[f32], b: &[f32]) -> f32 {
    -a.iter().zip(b).map(|(x, y)| x * y).sum::<f32>()
}

/// Dispatch a distance computation by [`VectorMetric`]. The SDK pins
/// the metric upstream via `t.vector(dims, { metric: ... })` so the
/// match is total.
pub(crate) fn distance(metric: VectorMetric, a: &[f32], b: &[f32]) -> f32 {
    match metric {
        VectorMetric::Cosine => cosine_distance(a, b),
        VectorMetric::L2 => l2_distance(a, b),
        VectorMetric::InnerProduct => neg_inner_product(a, b),
    }
}

/// Decode a `BLOB` cell back into `Vec<f32>`.
///
/// The blob layout is `dims × 4` little-endian bytes (native-endian
/// `f32`; see module rustdoc on the LE assumption). A length mismatch
/// surfaces as a typed [`DbError::ValidationFailed`] with
/// `code = "dimension_mismatch"` — the SDK already branches on this
/// code for the pgvector equivalent.
pub(crate) fn vec_from_blob(blob: &[u8], expected_dims: i32) -> Result<Vec<f32>, DbError> {
    if expected_dims <= 0 {
        return Err(DbError::validation(
            "dimension_mismatch",
            format!(
                "db: vector column expected_dims must be > 0, got {expected_dims}"
            ),
        ));
    }
    let expected_bytes = (expected_dims as usize)
        .checked_mul(4)
        .ok_or_else(|| DbError::internal(
            "vec_from_blob: dims overflow when computing byte length",
        ))?;
    if blob.len() != expected_bytes {
        return Err(DbError::validation(
            "dimension_mismatch",
            format!(
                "db: vector blob is {} bytes, expected {} (4 × {} dims)",
                blob.len(),
                expected_bytes,
                expected_dims
            ),
        ));
    }
    // `try_cast_slice` requires the source to be a multiple of the
    // target's alignment AND size. SQLite returns BLOB cells as a
    // `&[u8]` slice — alignment isn't guaranteed on `f32` (which
    // requires 4-byte alignment on most ABIs). Copy through a
    // [`Vec<f32>`] via `pod_read_unaligned` per chunk so we tolerate
    // any source alignment.
    let mut out = Vec::with_capacity(expected_dims as usize);
    for chunk in blob.chunks_exact(4) {
        // `chunks_exact(4)` yields 4-byte slices — `try_into` succeeds.
        let arr: [u8; 4] = chunk.try_into().expect("chunks_exact(4) yields 4-byte slices");
        out.push(f32::from_le_bytes(arr));
    }
    Ok(out)
}

/// Encode a `Vec<f32>` into a `BLOB` payload (little-endian, native
/// `f32` layout).
///
/// Used by tests and any caller wanting to write a vector into a SQLite
/// `BLOB` column. The runtime DML path constructs the BLOB upstream
/// (the SDK serialises through the same `bytemuck::cast_slice` shape).
#[allow(dead_code)]
pub(crate) fn blob_from_vec(vec: &[f32]) -> Vec<u8> {
    // `cast_slice` here is alignment-safe: we own the slice and rustc
    // aligns `Vec<f32>` to 4 bytes minimum. The cast is a pure
    // reinterpretation, no copy until `.to_vec()`.
    let bytes: &[u8] = bytemuck::cast_slice::<f32, u8>(vec);
    bytes.to_vec()
}

/// Type assertion shim — kept on a `Pod` bound so a future change
/// to `f32` (which isn't going to happen, but the static check makes
/// it explicit) breaks compilation here rather than at the cast site.
const fn _assert_f32_is_pod() {
    const fn assert_pod<T: Pod>() {}
    assert_pod::<f32>();
}
const _: () = _assert_f32_is_pod();

#[cfg(test)]
mod tests {
    //! Unit tests for the pure-function vector primitives. The
    //! distance functions are pinned against hand-computed values; the
    //! blob round-trip is asserted byte-identical.

    use super::*;

    /// Orthogonal unit vectors have cosine distance 1 (a · b = 0).
    #[test]
    fn cosine_distance_orthogonal_unit_vectors_is_one() {
        let a = [1.0f32, 0.0, 0.0];
        let b = [0.0f32, 1.0, 0.0];
        let d = cosine_distance(&a, &b);
        assert!((d - 1.0).abs() < 1e-6, "expected ~1.0, got {d}");
    }

    /// Identical unit vectors have cosine distance 0 (a · a = ||a||²).
    #[test]
    fn cosine_distance_identical_unit_vectors_is_zero() {
        let a = [1.0f32, 0.0, 0.0];
        let d = cosine_distance(&a, &a);
        assert!(d.abs() < 1e-6, "expected ~0.0, got {d}");
    }

    /// Anti-parallel unit vectors have cosine distance 2 (a · b = -1).
    #[test]
    fn cosine_distance_antiparallel_unit_vectors_is_two() {
        let a = [1.0f32, 0.0, 0.0];
        let b = [-1.0f32, 0.0, 0.0];
        let d = cosine_distance(&a, &b);
        assert!((d - 2.0).abs() < 1e-6, "expected ~2.0, got {d}");
    }

    /// Zero vector denominator is clamped — no NaN.
    #[test]
    fn cosine_distance_zero_vector_does_not_nan() {
        let a = [0.0f32, 0.0, 0.0];
        let b = [1.0f32, 0.0, 0.0];
        let d = cosine_distance(&a, &b);
        assert!(d.is_finite(), "expected finite, got {d}");
    }

    /// L2 distance for known triangle: sqrt((3-0)^2 + (4-0)^2) = 5.
    #[test]
    fn l2_distance_345_triangle() {
        let a = [0.0f32, 0.0];
        let b = [3.0f32, 4.0];
        let d = l2_distance(&a, &b);
        assert!((d - 5.0).abs() < 1e-6, "expected 5.0, got {d}");
    }

    /// L2 distance to self is zero.
    #[test]
    fn l2_distance_self_is_zero() {
        let a = [1.0f32, 2.0, 3.0, 4.0];
        let d = l2_distance(&a, &a);
        assert!(d.abs() < 1e-6, "expected 0.0, got {d}");
    }

    /// Negative inner product on orthogonal vectors is 0.
    #[test]
    fn neg_inner_product_orthogonal_is_zero() {
        let a = [1.0f32, 0.0, 0.0];
        let b = [0.0f32, 1.0, 0.0];
        let d = neg_inner_product(&a, &b);
        assert!(d.abs() < 1e-6, "expected 0.0, got {d}");
    }

    /// Negative inner product on identical unit vectors is -1.
    #[test]
    fn neg_inner_product_identical_unit_vectors_is_minus_one() {
        let a = [1.0f32, 0.0, 0.0];
        let d = neg_inner_product(&a, &a);
        assert!((d + 1.0).abs() < 1e-6, "expected -1.0, got {d}");
    }

    /// For unit vectors, L2² = 2 (1 - cos(θ)) — i.e. the cosine and L2
    /// metrics agree up to a known transform on the unit sphere.
    /// Useful as a sanity check that both distance functions speak the
    /// same geometry.
    #[test]
    fn l2_distance_matches_cosine_for_unit_vectors() {
        let a = [1.0f32, 0.0, 0.0];
        let b = [0.7071068f32, 0.7071068, 0.0];
        let l2 = l2_distance(&a, &b);
        let cos = cosine_distance(&a, &b);
        // ||a-b||^2 = 2 - 2*cos(theta) when ||a|| = ||b|| = 1, and
        // cos_distance = 1 - cos(theta), so ||a-b||^2 = 2 * cos_distance.
        let lhs = l2 * l2;
        let rhs = 2.0 * cos;
        assert!(
            (lhs - rhs).abs() < 1e-4,
            "||a-b||^2 = {lhs}, 2 * cos_distance = {rhs}"
        );
    }

    /// Dispatcher picks the right function per metric.
    #[test]
    fn distance_dispatcher_routes_per_metric() {
        let a = [1.0f32, 0.0, 0.0];
        let b = [0.0f32, 1.0, 0.0];
        let cos = distance(VectorMetric::Cosine, &a, &b);
        let l2 = distance(VectorMetric::L2, &a, &b);
        let ip = distance(VectorMetric::InnerProduct, &a, &b);
        // Orthogonal unit vectors: cosine ~ 1, L2 ~ sqrt(2), -ip ~ 0.
        assert!((cos - 1.0).abs() < 1e-6, "cosine: {cos}");
        assert!((l2 - 2.0_f32.sqrt()).abs() < 1e-6, "l2: {l2}");
        assert!(ip.abs() < 1e-6, "neg_inner_product: {ip}");
    }

    /// Round-trip via blob_from_vec + vec_from_blob is byte-identical.
    #[test]
    fn blob_round_trip_is_byte_identical() {
        let v: Vec<f32> = vec![1.5, -2.25, 3.0, std::f32::consts::PI, -0.0];
        let blob = blob_from_vec(&v);
        assert_eq!(blob.len(), v.len() * 4, "blob is 4 bytes per dim");
        let decoded = vec_from_blob(&blob, v.len() as i32).expect("decode");
        assert_eq!(
            decoded.len(),
            v.len(),
            "round-trip preserves length"
        );
        for (orig, back) in v.iter().zip(decoded.iter()) {
            // bit-exact: same bytes in, same bytes out (no FP rounding).
            assert_eq!(
                orig.to_bits(),
                back.to_bits(),
                "round-trip must be bit-exact: orig={orig:?}, back={back:?}"
            );
        }
    }

    /// `vec_from_blob` rejects a wrong-dim payload with the
    /// `dimension_mismatch` typed code.
    #[test]
    fn vec_from_blob_rejects_wrong_dim() {
        // 8 bytes = 2 f32s; ask for 4-dim → mismatch.
        let blob = vec![0u8; 8];
        let err = vec_from_blob(&blob, 4).expect_err("must reject");
        match err {
            DbError::ValidationFailed { code, message, .. } => {
                assert_eq!(code, "dimension_mismatch", "got message: {message}");
                assert!(
                    message.contains("8") && message.contains("16") && message.contains("4"),
                    "message should mention the byte counts + dim: {message}"
                );
            }
            other => panic!("expected ValidationFailed, got {other:?}"),
        }
    }

    /// `vec_from_blob` rejects non-positive `expected_dims`.
    #[test]
    fn vec_from_blob_rejects_zero_or_negative_dims() {
        let blob = vec![0u8; 16];
        let err = vec_from_blob(&blob, 0).expect_err("zero dims must reject");
        match err {
            DbError::ValidationFailed { code, .. } => assert_eq!(code, "dimension_mismatch"),
            other => panic!("expected ValidationFailed, got {other:?}"),
        }
        let err = vec_from_blob(&blob, -3).expect_err("negative dims must reject");
        match err {
            DbError::ValidationFailed { code, .. } => assert_eq!(code, "dimension_mismatch"),
            other => panic!("expected ValidationFailed, got {other:?}"),
        }
    }

    /// `vec_from_blob` accepts an empty zero-dim case rejected explicitly —
    /// dims must be > 0 (a 0-dim vector is degenerate; the SDK clamps).
    #[test]
    fn vec_from_blob_zero_length_blob_with_zero_dims_rejected() {
        let blob: Vec<u8> = Vec::new();
        let err = vec_from_blob(&blob, 0).expect_err("zero dims rejected");
        match err {
            DbError::ValidationFailed { code, .. } => assert_eq!(code, "dimension_mismatch"),
            other => panic!("expected ValidationFailed, got {other:?}"),
        }
    }

    /// `blob_from_vec` on an empty slice produces an empty blob.
    #[test]
    fn blob_from_vec_empty_slice_yields_empty_blob() {
        let v: Vec<f32> = Vec::new();
        let blob = blob_from_vec(&v);
        assert!(blob.is_empty());
    }

    /// Little-endian byte layout: 1.0f32 = `0x3F800000` LE = `[0, 0, 128, 63]`.
    #[test]
    fn blob_from_vec_uses_little_endian() {
        let v: Vec<f32> = vec![1.0];
        let blob = blob_from_vec(&v);
        assert_eq!(blob, vec![0x00, 0x00, 0x80, 0x3F]);
    }

    /// `sqlite_vector_column_ddl` emits the documented CHECK shape.
    #[test]
    fn sqlite_vector_column_ddl_shape_128() {
        let ddl = sqlite_vector_column_ddl("embedding", 128);
        assert_eq!(
            ddl,
            "\"embedding\" BLOB CHECK(length(\"embedding\") = 512) NOT NULL"
        );
    }

    /// `sqlite_vector_column_ddl` doubles embedded double-quotes.
    #[test]
    fn sqlite_vector_column_ddl_escapes_embedded_quotes() {
        let ddl = sqlite_vector_column_ddl("ev\"il", 4);
        assert_eq!(
            ddl,
            "\"ev\"\"il\" BLOB CHECK(length(\"ev\"\"il\") = 16) NOT NULL"
        );
    }
}
