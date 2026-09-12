//! SQLite spatial distance and `geoPoint` decoding.
//!
//! SQLite stores points as little-endian latitude and longitude values. Search
//! scans decoded rows and orders matching points in Rust. Migration code owns
//! the corresponding column definition.

use crate::sql::descriptors::GeoPoint;
use zeroship_data_orm::error::DbError;

/// Mean Earth radius used by the spherical haversine calculation.
const EARTH_RADIUS_M: f64 = 6_371_000.0;

/// Great-circle distance in metres between two WGS84 points.
pub(crate) fn haversine_m(a: GeoPoint, b: GeoPoint) -> f64 {
    let phi1 = a.lat.to_radians();
    let phi2 = b.lat.to_radians();
    let dphi = (b.lat - a.lat).to_radians();
    let dlam = (b.lng - a.lng).to_radians();
    let h = (dphi / 2.0).sin().powi(2) + phi1.cos() * phi2.cos() * (dlam / 2.0).sin().powi(2);
    2.0 * EARTH_RADIUS_M * h.sqrt().atan2((1.0 - h).sqrt())
}

#[cfg(test)]
use crate::sql::sqlite_values::point_to_blob;

/// Decode a `BLOB` cell back into a [`GeoPoint`].
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
    use super::*;

    #[test]
    fn haversine_self_distance_is_zero() {
        let london = GeoPoint {
            lat: 51.5074,
            lng: -0.1278,
        };
        let d = haversine_m(london, london);
        assert_eq!(d, 0.0, "self-distance must be exactly 0, got {d}");
    }

    #[test]
    fn haversine_london_to_paris() {
        let london = GeoPoint {
            lat: 51.5074,
            lng: -0.1278,
        };
        let paris = GeoPoint {
            lat: 48.8566,
            lng: 2.3522,
        };
        let d = haversine_m(london, paris);
        let expected_km = 344.0;
        let actual_km = d / 1000.0;
        assert!(
            (actual_km - expected_km).abs() < 5.0,
            "London → Paris: expected ~{expected_km}km ± 5km, got {actual_km}km"
        );
    }

    #[test]
    fn haversine_is_symmetric() {
        let london = GeoPoint {
            lat: 51.5074,
            lng: -0.1278,
        };
        let paris = GeoPoint {
            lat: 48.8566,
            lng: 2.3522,
        };
        let d1 = haversine_m(london, paris);
        let d2 = haversine_m(paris, london);
        assert_eq!(
            d1.to_bits(),
            d2.to_bits(),
            "haversine(a,b)={d1} != haversine(b,a)={d2}"
        );
    }

    #[test]
    fn haversine_antipodal_is_half_circumference() {
        let a = GeoPoint { lat: 0.0, lng: 0.0 };
        let b = GeoPoint {
            lat: 0.0,
            lng: 180.0,
        };
        let d = haversine_m(a, b);
        let expected = std::f64::consts::PI * EARTH_RADIUS_M;
        assert!(
            (d - expected).abs() < 1.0,
            "antipodal distance: expected ~{expected}m, got {d}m"
        );
    }

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

    #[test]
    fn point_to_blob_is_16_bytes() {
        let p = GeoPoint {
            lat: 51.5074,
            lng: -0.1278,
        };
        let blob = point_to_blob(p);
        assert_eq!(blob.len(), 16, "geoPoint blob is 16 bytes (2 × f64 LE)");
    }

    #[test]
    fn point_blob_round_trip_is_bit_exact() {
        let p = GeoPoint {
            lat: 51.5074,
            lng: -0.1278,
        };
        let blob = point_to_blob(p);
        let back = blob_to_point(&blob).expect("decode");
        assert_eq!(back.lat.to_bits(), p.lat.to_bits());
        assert_eq!(back.lng.to_bits(), p.lng.to_bits());
    }

    #[test]
    fn point_to_blob_uses_little_endian() {
        let p = GeoPoint { lat: 0.0, lng: 0.0 };
        let blob = point_to_blob(p);
        assert_eq!(blob, vec![0u8; 16]);
    }

    #[test]
    fn blob_to_point_rejects_wrong_length() {
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
