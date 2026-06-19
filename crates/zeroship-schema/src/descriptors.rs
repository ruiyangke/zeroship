//! Schema-shape descriptor enums shared between the DDL/diff layer and
//! plugin-db's data plane.
//!
//! These were relocated verbatim out of `zeroship_plugin_db::backend`
//! (the `VectorMetric` / `EncryptionMode` / `GeoPoint` triple): they are
//! pure *shape* descriptors — no DB round-trip, no crypto, no runtime —
//! and the DDL builders in [`crate::query`] consume them. plugin-db's
//! `backend` module re-exports them so existing `crate::backend::…`
//! references keep resolving (the data-plane crypto/spatial impls that
//! name them are unchanged).

/// Distance metric for a vector index. The three metrics map 1:1 to
/// pgvector's operator class set (`vector_cosine_ops`,
/// `vector_l2_ops`, `vector_ip_ops`) and the SQLite Rust-side distance
/// functions (`cosine_distance`, `l2_distance`, `neg_inner_product`).
///
/// **Why an enum, not a string** (plan §2): the SDK validates against
/// a closed three-element set; carrying it through the Rust surface
/// as an enum trips the rustc exhaustiveness checker if a future PR
/// adds a fourth metric — every match arm in the impl flags rather
/// than the new metric silently routing to a default branch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VectorMetric {
    /// Cosine distance: `1 - (a · b) / (||a|| · ||b||)`. PG operator
    /// `<=>`, opclass `vector_cosine_ops`. The default for embedding
    /// models that produce L2-normalised vectors.
    Cosine,
    /// Euclidean (L2) distance: `sqrt(Σ (a_i - b_i)^2)`. PG operator
    /// `<->`, opclass `vector_l2_ops`.
    L2,
    /// Negative inner product: `- (a · b)`. PG operator `<#>`,
    /// opclass `vector_ip_ops`. The "negative" framing makes "smaller
    /// is better" hold across all three metrics, so a single ORDER BY
    /// clause works.
    InnerProduct,
}

/// A geographic point in WGS84 (EPSG:4326). Used by the spatial index
/// surface for query input and by the `geoPoint` DDL emitter.
///
/// **Field order**: `lat` then `lng` — matches the SDK shape
/// (`{ lat: number, lng: number }`) and the GeoJSON convention.
/// Note that PostGIS `ST_MakePoint` takes `(lng, lat)`; the PG impl
/// reorders at the SQL boundary.
///
/// `Copy` because it's two `f64`s — passing by value is cheaper than
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

/// Encryption mode — chooses nonce derivation + AAD shape.
///
/// Two-mode design from `docs/proposals/db-system-design.md` §7.2.
/// The on-wire blob layout is identical between modes (the synthetic
/// vs random distinction is fully internal to the encrypt side); the
/// caller has to track the mode to reconstruct the right AAD on
/// decrypt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncryptionMode {
    /// Per-row random nonce. AAD =
    /// `(collection, column, row_pk_bytes)` — binds ciphertext to its
    /// row position. Per the Camp A architecture
    /// (`docs/proposals/p5-encryption-backup-implementation-plan.md`
    /// §13): plugin-db mints typed_id PKs **SDK-side** before INSERT,
    /// so `row_pk` is always available when `encrypt()` is called.
    /// Single-phase INSERT — no chicken-and-egg vs Microsoft Always
    /// Encrypted / MongoDB CSFLE. Defeats the ciphertext-oracle
    /// attack on randomised columns. Default (fail-safe).
    Randomised,

    /// Synthetic nonce = HMAC-SHA256(k_siv, plaintext)[..12]. AAD =
    /// `(collection, column)` only — `row_pk_bytes` intentionally
    /// omitted because deterministic mode's defining property is
    /// "same plaintext → same ciphertext under (collection, column)",
    /// which the B-tree-on-ciphertext equality index depends on.
    /// Inherits the standard deterministic-mode leak (equality
    /// across rows is observable to anyone with column read access).
    /// The SDK filter pre-flight refuses range / regex / `LIKE`
    /// queries on deterministic columns regardless.
    Deterministic,
}
