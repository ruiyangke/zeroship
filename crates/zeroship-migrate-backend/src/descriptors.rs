//! Schema-shape descriptor enums shared between the DDL/diff layer and
//! the data plane.
//!
//! These were relocated verbatim out of the original data-plane `backend`
//! module (the `VectorMetric` / `GeoPoint` types): they are
//! pure *shape* descriptors - no DB round-trip, no crypto, no runtime -
//! and the DDL builders in `crate::schema::query` consume them. The data plane's
//! `backend` module re-exports them so existing `crate::backend::...`
//! references keep resolving (the data-plane crypto/spatial impls that
//! name them are unchanged).

/// Distance metric for a vector index. The three metrics map 1:1 to
/// pgvector's operator class set (`vector_cosine_ops`,
/// `vector_l2_ops`, `vector_ip_ops`) and the SQLite Rust-side distance
/// functions (`cosine_distance`, `l2_distance`, `neg_inner_product`).
///
/// **Why an enum, not a string**: the SDK validates against
/// a closed three-element set; carrying it through the Rust surface
/// as an enum trips the rustc exhaustiveness checker if a future change
/// adds a fourth metric - every match arm in the impl flags rather
/// than the new metric silently routing to a default branch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VectorMetric {
    /// Cosine distance: one minus the dot product of `a` and `b` divided by the
    /// product of their magnitudes. PG operator
    /// `<=>`, opclass `vector_cosine_ops`. The default for embedding
    /// models that produce L2-normalised vectors.
    Cosine,
    /// Euclidean (L2) distance: the square root of the sum over each dimension `i`
    /// of `(a_i - b_i)` squared. PG operator
    /// `<->`, opclass `vector_l2_ops`.
    L2,
    /// Negative inner product: the negated dot product of `a` and `b`. PG operator `<#>`,
    /// opclass `vector_ip_ops`. The "negative" framing makes "smaller
    /// is better" hold across all three metrics, so a single ORDER BY
    /// clause works.
    InnerProduct,
}

/// A geographic point in WGS84 (EPSG:4326). Used by the spatial index
/// surface for query input and by the `geoPoint` DDL emitter.
///
/// **Field order**: `lat` then `lng` - matches the SDK shape
/// (`{ lat: number, lng: number }`) and the GeoJSON convention.
/// Note that PostGIS `ST_MakePoint` takes `(lng, lat)`; the PG impl
/// reorders at the SQL boundary.
///
/// `Copy` because it's two `f64`s - passing by value is cheaper than
/// borrowing.
#[derive(Debug, Clone, Copy)]
pub struct GeoPoint {
    /// Latitude in degrees, range `[-90, 90]`. SDK validate rejects
    /// out-of-range values before the trait method is called.
    pub lat: f64,
    /// Longitude in degrees, range `[-180, 180]`. SDK validate
    /// rejects out-of-range values before the trait method is called.
    pub lng: f64,
}
