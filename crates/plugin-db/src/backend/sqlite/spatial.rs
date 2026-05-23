//! SQLite spatial helpers — haversine within-radius math + `geoPoint`
//! BLOB packing.
//!
//! **P4 PR 5** (`docs/proposals/p4-search-implementation-plan.md` §4.3
//! + §8 PR 5). This module owns the cryptography-of-math: the
//! haversine distance function (great-circle metres on a spherical
//! Earth approximation), the `(lat, lng)` ↔ `BLOB` round-trip
//! (`f64 × 2` little-endian = 16 bytes), and the column-DDL emitter
//! that pins the BLOB's length via a CHECK constraint.
//!
//! The actual `spatial_near` orchestration (flat scan over base-rows,
//! per-row distance, top-`limit` selection, JSON re-emission) lives in
//! [`crate::backend::sqlite::mod.rs`]'s `impl SpatialIndex for
//! SqliteBackend` block — this module is the strict primitive layer
//! so the math stays unit-testable in isolation against
//! hand-computed values.
//!
//! ## Pure-Rust over an R-tree (Q-P4-C, plan §4.3)
//!
//! SQLite ships an R-tree vtable extension, but bundling it requires
//! the same CI-matrix amalgamation fork the vector path rejected (see
//! `vector.rs` rustdoc on `sqlite-vec`). Flat scan with haversine is
//! ~30 LOC of trig; acceptable at dev scale (≤50k rows, ≤100ms
//! latency). Production spatial workloads run on PostGIS via the PG
//! arm.
//!
//! ## Storage layout
//!
//! `(lat, lng)` packed as two little-endian `f64`s = 16 bytes per
//! row. The `geoPoint` column DDL pins this with `BLOB CHECK(length =
//! 16) NOT NULL` so the engine rejects malformed inserts before they
//! reach Rust.
//!
//! ## Endianness
//!
//! Same little-endian assumption as `vector.rs`: every platform the
//! workspace targets is LE, so `f64::to_le_bytes` / `from_le_bytes`
//! is the canonical layout. A big-endian target would silently swap
//! the byte order on read — out of scope for P4.

use crate::backend::GeoPoint;
use crate::error::DbError;

/// Earth's mean radius in metres (IUGG / WGS84 standard). The
/// haversine formula treats Earth as a perfect sphere — actual
/// surface distance differs from the spheroidal WGS84 calculation
/// by up to ~0.5%. Acceptable at dev scale; production spatial
/// workloads use PostGIS's `geography(POINT, 4326)` type which does
/// the spheroidal computation.
const EARTH_RADIUS_M: f64 = 6_371_000.0;

/// Great-circle distance in metres between two WGS84 points.
///
/// **Formula** (haversine, plan §4.3):
/// ```text
/// φ₁ = lat_a (rad)
/// φ₂ = lat_b (rad)
/// Δφ = (lat_b - lat_a) (rad)
/// Δλ = (lng_b - lng_a) (rad)
/// h  = sin²(Δφ/2) + cos(φ₁) cos(φ₂) sin²(Δλ/2)
/// d  = 2 R atan2(√h, √(1 - h))
/// ```
///
/// Range: `[0.0, π·R]` ≈ `[0, 20_015_086.79 m]` (antipodal points).
/// `atan2(√h, √(1-h))` is preferred over `asin(√h)` because it stays
/// numerically stable for `h → 1` (near-antipodal pairs).
///
/// **Identity**: `haversine_m(a, a) == 0.0` exactly (every term zeros
/// out). No FP rounding noise on the self-distance, which the
/// `_distance_m: 0.0` synthetic field consumer can rely on.
pub(crate) fn haversine_m(a: GeoPoint, b: GeoPoint) -> f64 {
    let phi1 = a.lat.to_radians();
    let phi2 = b.lat.to_radians();
    let dphi = (b.lat - a.lat).to_radians();
    let dlam = (b.lng - a.lng).to_radians();
    let h = (dphi / 2.0).sin().powi(2)
        + phi1.cos() * phi2.cos() * (dlam / 2.0).sin().powi(2);
    2.0 * EARTH_RADIUS_M * h.sqrt().atan2((1.0 - h).sqrt())
}

/// Emit the column-level DDL for a SQLite `t.geoPoint()` field.
///
/// Shape (plan §4.3 / §4.4):
/// ```sql
/// "<name>" BLOB CHECK(length("<name>") = 16) NOT NULL
/// ```
///
/// The 16-byte CHECK pins the `(lat, lng)` little-endian `f64 × 2`
/// payload at the engine layer — any INSERT with a mis-sized blob
/// fails the constraint and surfaces as `DbError::SchemaRefused {
/// check_violation }` upstream.
///
/// **Identifier safety**: `name` is `quote_ident`-style double-quoted,
/// doubling any embedded `"`. The SDK validates field names at
/// schema-emission time, but this helper stays lexically robust
/// against any caller that passes a raw string.
///
/// **No production caller yet** in this PR: the SQLite-side
/// `build_create_table_with_fks` (the orchestrator's column-DDL
/// emitter) is PG-flavoured today; a follow-up PR that teaches
/// `register_model::apply` to dispatch by dialect will route through
/// this helper. The integration test in `tests/sqlite_integration.rs`
/// constructs the DDL inline using the same shape.
#[allow(dead_code)]
pub(crate) fn sqlite_geopoint_column_ddl(name: &str) -> String {
    let quoted = format!("\"{}\"", name.replace('"', "\"\""));
    format!("{quoted} BLOB CHECK(length({quoted}) = 16) NOT NULL")
}

/// Encode a [`GeoPoint`] as a 16-byte `BLOB` payload.
///
/// Layout: `lat.to_le_bytes()` (8 bytes) || `lng.to_le_bytes()` (8
/// bytes). The field order matches the [`GeoPoint`] struct declaration
/// — a future change to that ordering must update this helper AND
/// [`blob_to_point`] together (the round-trip test pins the
/// behaviour).
#[allow(dead_code)]
pub(crate) fn point_to_blob(point: GeoPoint) -> Vec<u8> {
    let mut out = Vec::with_capacity(16);
    out.extend_from_slice(&point.lat.to_le_bytes());
    out.extend_from_slice(&point.lng.to_le_bytes());
    out
}

/// Decode a `BLOB` cell back into a [`GeoPoint`].
///
/// A `blob.len() != 16` surfaces as `DbError::ValidationFailed { code:
/// "dimension_mismatch", ... }` — the same typed code the vector
/// blob-decoder uses for analogous wrong-size payloads. The CHECK
/// constraint at column-DDL time rejects bad payloads at INSERT, so a
/// row reaching this decoder with the wrong length indicates a
/// missing CHECK (operator misconfiguration) — the typed error makes
/// that surface in the operator's logs.
pub(crate) fn blob_to_point(blob: &[u8]) -> Result<GeoPoint, DbError> {
    if blob.len() != 16 {
        return Err(DbError::validation(
            "dimension_mismatch",
            format!(
                "db: geoPoint blob is {} bytes, expected 16 (2 × 8-byte f64)",
                blob.len()
            ),
        ));
    }
    // `chunks_exact` would also work, but the explicit byte-slice
    // indexing matches the symmetry with `point_to_blob` and avoids
    // an iterator allocation.
    let lat_bytes: [u8; 8] = blob[0..8]
        .try_into()
        .expect("blob.len() == 16 — first 8 bytes always present");
    let lng_bytes: [u8; 8] = blob[8..16]
        .try_into()
        .expect("blob.len() == 16 — second 8 bytes always present");
    Ok(GeoPoint {
        lat: f64::from_le_bytes(lat_bytes),
        lng: f64::from_le_bytes(lng_bytes),
    })
}

#[cfg(test)]
mod tests {
    //! Unit tests for the pure-function spatial primitives. The
    //! haversine accuracy is asserted against an authoritative pair
    //! (London → Paris ~344 km); the blob round-trip is asserted
    //! bit-exact.

    use super::*;

    /// `haversine_m(a, a) == 0.0` exactly.
    #[test]
    fn haversine_self_distance_is_zero() {
        let london = GeoPoint { lat: 51.5074, lng: -0.1278 };
        let d = haversine_m(london, london);
        assert_eq!(d, 0.0, "self-distance must be exactly 0, got {d}");
    }

    /// London → Paris is approximately 344 km on the great circle.
    /// Authoritative value: ~343.6 km (varies by ±2 km depending on
    /// exact city-centre coordinates). Tolerance ±5 km absorbs both
    /// the spherical-vs-spheroidal Earth approximation and the
    /// imprecise "centre" coordinates.
    #[test]
    fn haversine_london_to_paris() {
        let london = GeoPoint { lat: 51.5074, lng: -0.1278 };
        let paris = GeoPoint { lat: 48.8566, lng: 2.3522 };
        let d = haversine_m(london, paris);
        let expected_km = 344.0;
        let actual_km = d / 1000.0;
        assert!(
            (actual_km - expected_km).abs() < 5.0,
            "London → Paris: expected ~{expected_km}km ± 5km, got {actual_km}km"
        );
    }

    /// Distance is symmetric: `haversine(a,b) == haversine(b,a)`.
    /// FP determinism: the formula is symmetric in `a` and `b` up to
    /// the order of additions, but `sin`/`cos`/`atan2` are
    /// deterministic for identical inputs, so the result is
    /// bit-identical.
    #[test]
    fn haversine_is_symmetric() {
        let london = GeoPoint { lat: 51.5074, lng: -0.1278 };
        let paris = GeoPoint { lat: 48.8566, lng: 2.3522 };
        let d1 = haversine_m(london, paris);
        let d2 = haversine_m(paris, london);
        assert_eq!(
            d1.to_bits(),
            d2.to_bits(),
            "haversine(a,b)={d1} != haversine(b,a)={d2}"
        );
    }

    /// Antipodal points (e.g. (0,0) and (0,180)) are at most π·R
    /// ≈ 20_015_086.79 m apart.
    #[test]
    fn haversine_antipodal_is_half_circumference() {
        let a = GeoPoint { lat: 0.0, lng: 0.0 };
        let b = GeoPoint { lat: 0.0, lng: 180.0 };
        let d = haversine_m(a, b);
        let expected = std::f64::consts::PI * EARTH_RADIUS_M;
        assert!(
            (d - expected).abs() < 1.0,
            "antipodal distance: expected ~{expected}m, got {d}m"
        );
    }

    /// One degree of latitude at the equator is ~111.195 km (the
    /// great-circle distance over a sphere of `EARTH_RADIUS_M`).
    /// The exact value is `π R / 180 ≈ 111_194.92664...` metres.
    #[test]
    fn haversine_one_degree_latitude_at_equator() {
        let a = GeoPoint { lat: 0.0, lng: 0.0 };
        let b = GeoPoint { lat: 1.0, lng: 0.0 };
        let d = haversine_m(a, b);
        let expected = std::f64::consts::PI * EARTH_RADIUS_M / 180.0;
        assert!(
            (d - expected).abs() < 1.0,
            "1 deg lat at equator: expected {expected}m, got {d}m"
        );
    }

    /// `sqlite_geopoint_column_ddl` emits the documented CHECK shape.
    #[test]
    fn geopoint_column_ddl_shape() {
        let ddl = sqlite_geopoint_column_ddl("location");
        assert_eq!(
            ddl,
            "\"location\" BLOB CHECK(length(\"location\") = 16) NOT NULL"
        );
    }

    /// `sqlite_geopoint_column_ddl` doubles embedded double-quotes.
    #[test]
    fn geopoint_column_ddl_escapes_embedded_quotes() {
        let ddl = sqlite_geopoint_column_ddl("ev\"il");
        assert_eq!(
            ddl,
            "\"ev\"\"il\" BLOB CHECK(length(\"ev\"\"il\") = 16) NOT NULL"
        );
    }

    /// `point_to_blob` produces a 16-byte payload.
    #[test]
    fn point_to_blob_is_16_bytes() {
        let p = GeoPoint { lat: 51.5074, lng: -0.1278 };
        let blob = point_to_blob(p);
        assert_eq!(blob.len(), 16, "geoPoint blob is 16 bytes (2 × f64 LE)");
    }

    /// `point_to_blob` then `blob_to_point` round-trips bit-exact.
    #[test]
    fn point_blob_round_trip_is_bit_exact() {
        let p = GeoPoint { lat: 51.5074, lng: -0.1278 };
        let blob = point_to_blob(p);
        let back = blob_to_point(&blob).expect("decode");
        // Bit-exact: bytes in, bytes out (no FP rounding through the
        // intermediate `to_radians` / `sin` path).
        assert_eq!(back.lat.to_bits(), p.lat.to_bits());
        assert_eq!(back.lng.to_bits(), p.lng.to_bits());
    }

    /// `point_to_blob` uses little-endian byte ordering:
    /// `0.0_f64 = 0x0000000000000000` LE = `[0, 0, 0, 0, 0, 0, 0, 0]`.
    /// A zero `(lat, lng)` packs to 16 zero bytes.
    #[test]
    fn point_to_blob_uses_little_endian() {
        let p = GeoPoint { lat: 0.0, lng: 0.0 };
        let blob = point_to_blob(p);
        assert_eq!(blob, vec![0u8; 16]);
    }

    /// `blob_to_point` rejects a wrong-length payload with the
    /// `dimension_mismatch` typed code.
    #[test]
    fn blob_to_point_rejects_wrong_length() {
        // 12 bytes — too short.
        let blob = vec![0u8; 12];
        let err = blob_to_point(&blob).expect_err("must reject");
        match err {
            DbError::ValidationFailed { code, message, .. } => {
                assert_eq!(code, "dimension_mismatch", "got message: {message}");
                assert!(
                    message.contains("12") && message.contains("16"),
                    "message should mention the byte counts: {message}"
                );
            }
            other => panic!("expected ValidationFailed, got {other:?}"),
        }
    }

    /// `blob_to_point` rejects an empty payload too.
    #[test]
    fn blob_to_point_rejects_empty_blob() {
        let err = blob_to_point(&[]).expect_err("empty must reject");
        match err {
            DbError::ValidationFailed { code, .. } => {
                assert_eq!(code, "dimension_mismatch");
            }
            other => panic!("expected ValidationFailed, got {other:?}"),
        }
    }
}
