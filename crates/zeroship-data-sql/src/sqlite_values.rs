//! SQLite binary representations shared by query parameters and search adapters.
use crate::descriptors::GeoPoint;

/// Pack a vector using the little-endian float layout consumed by sqlite-vec.
pub fn vec_to_le_bytes(values: &[f32]) -> Vec<u8> {
    values
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect()
}

/// Pack latitude followed by longitude as little-endian floats.
pub fn point_to_blob(point: GeoPoint) -> Vec<u8> {
    point
        .lat
        .to_le_bytes()
        .into_iter()
        .chain(point.lng.to_le_bytes())
        .collect()
}
