//! Value descriptors shared by runtime catalog readers and storage backends.

/// Projectable fields explicitly declared by the generated schema.
pub fn readable_fields(schema: &crate::value::Value) -> std::collections::BTreeSet<String> {
    schema
        .as_object()
        .into_iter()
        .flat_map(|fields| fields.iter())
        .filter(|(name, definition)| {
            !crate::compile::is_schema_metadata_key(name)
                && definition.is_object()
                && definition
                    .get("readable")
                    .and_then(crate::value::Value::as_bool)
                    != Some(false)
                && definition
                    .get("projectable")
                    .and_then(crate::value::Value::as_bool)
                    != Some(false)
        })
        .map(|(name, _)| name.clone())
        .collect()
}

/// Whether the field uses encrypted binary storage.
pub fn is_encrypted(field: &crate::value::Value) -> bool {
    field
        .get("encrypted")
        .and_then(crate::value::Value::as_bool)
        == Some(true)
}

/// Effective masking metadata from an installed field descriptor.
#[derive(Debug, Clone, Copy)]
pub struct EffectiveMask<'a> {
    pub kind: &'a str,
    pub classification: &'a str,
}

/// An absent mask or explicit `kind: "none"` leaves the field unmasked.
pub fn effective_mask(field: &crate::value::Value) -> Option<EffectiveMask<'_>> {
    let metadata = field.get("mask")?.as_object()?;
    let kind = metadata
        .get("kind")
        .and_then(crate::value::Value::as_str)
        .unwrap_or("full");
    (kind != "none").then(|| EffectiveMask {
        kind,
        classification: metadata
            .get("classification")
            .and_then(crate::value::Value::as_str)
            .unwrap_or("pii"),
    })
}

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
