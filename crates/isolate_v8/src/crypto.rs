//! Crypto APIs for V8 apps — backed by aws-lc-rs.

use appbase_ops::appbase_op;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;

/// `crypto.randomUUID() → string`
///
/// Generates a RFC 4122 v4 UUID.
#[appbase_op]
fn crypto_random_uuid() -> String {
    let mut bytes = [0u8; 16];
    aws_lc_rs::rand::fill(&mut bytes).unwrap();
    bytes[6] = (bytes[6] & 0x0f) | 0x40; // version 4
    bytes[8] = (bytes[8] & 0x3f) | 0x80; // variant 10xx

    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0], bytes[1], bytes[2], bytes[3],
        bytes[4], bytes[5], bytes[6], bytes[7],
        bytes[8], bytes[9], bytes[10], bytes[11],
        bytes[12], bytes[13], bytes[14], bytes[15]
    )
}

/// `__cryptoGetRandomValues(len) → base64 string of random bytes`
#[appbase_op]
fn crypto_get_random_values(len: u32) -> Result<String, crate::ops::OpError> {
    if len > 65536 {
        return Err(crate::ops::OpError::type_error(
            "getRandomValues: quota exceeded (max 65536 bytes)",
        ));
    }
    let mut buf = vec![0u8; len as usize];
    aws_lc_rs::rand::fill(&mut buf)
        .map_err(|e| crate::ops::OpError::error(format!("RNG failed: {e}")))?;
    Ok(B64.encode(&buf))
}

/// `__cryptoDigest(algo, data_b64) → base64 hash`
#[appbase_op]
fn crypto_digest(algo: String, data_b64: String) -> Result<String, crate::ops::OpError> {
    let algorithm = match algo.as_str() {
        "SHA-1" => &aws_lc_rs::digest::SHA1_FOR_LEGACY_USE_ONLY,
        "SHA-256" => &aws_lc_rs::digest::SHA256,
        "SHA-384" => &aws_lc_rs::digest::SHA384,
        "SHA-512" => &aws_lc_rs::digest::SHA512,
        _ => {
            return Err(crate::ops::OpError::type_error(format!(
                "Unsupported digest: {algo}"
            )))
        }
    };
    let data = B64
        .decode(&data_b64)
        .map_err(|e| crate::ops::OpError::type_error(format!("Invalid base64: {e}")))?;
    let digest = aws_lc_rs::digest::digest(algorithm, &data);
    Ok(B64.encode(digest.as_ref()))
}
