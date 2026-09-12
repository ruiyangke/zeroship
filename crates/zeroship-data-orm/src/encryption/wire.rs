//! Column-ciphertext framing: version flag, nonce, ciphertext and authentication tag.
//!
//! Only `WIRE_VERSION_V1` is accepted. Ciphertext and its appended tag stay together
//! to match the AEAD API; AAD construction belongs to `super::aad`.

use crate::error::DbError;

/// Version emitted by `pack` and accepted by `unpack`.
pub(crate) const WIRE_VERSION_V1: u8 = 0x01;

/// Nonce length required by the AES-GCM implementation.
const NONCE_LEN: usize = 12;

/// Version and nonce prefix length.
const HEADER_LEN: usize = 1 + NONCE_LEN;

/// AES-GCM authentication tag length (RFC 5116 §5.3).
const GCM_TAG_LEN: usize = 16;

/// Prefix the version and nonce to authenticated ciphertext.
/// Cipher output shorter than `GCM_TAG_LEN` indicates an internal invariant failure.
pub(crate) fn pack(nonce: &[u8; NONCE_LEN], ct_and_tag: &[u8]) -> Result<Vec<u8>, DbError> {
    if ct_and_tag.len() < GCM_TAG_LEN {
        return Err(DbError::internal(format!(
            "wire::pack: ciphertext+tag too short ({} bytes, tag length is {})",
            ct_and_tag.len(),
            GCM_TAG_LEN
        )));
    }
    let mut out = Vec::with_capacity(HEADER_LEN + ct_and_tag.len());
    out.push(WIRE_VERSION_V1);
    out.extend_from_slice(nonce);
    out.extend_from_slice(ct_and_tag);
    Ok(out)
}

/// Unpack a stored blob into `(nonce, ciphertext+tag)`. Rejects
/// blobs shorter than `header + tag` and blobs with an unknown
/// version flag, both as `encryption_aead_failed` validation errors
/// (operator-visible at the SDK boundary).
pub(crate) fn unpack(blob: &[u8]) -> Result<(&[u8; NONCE_LEN], &[u8]), DbError> {
    if blob.len() < HEADER_LEN + GCM_TAG_LEN {
        return Err(DbError::validation(
            "encryption_aead_failed",
            format!(
                "wire::unpack: blob too short ({} bytes, need at least {})",
                blob.len(),
                HEADER_LEN + GCM_TAG_LEN
            ),
        ));
    }
    if blob[0] != WIRE_VERSION_V1 {
        return Err(DbError::validation(
            "encryption_aead_failed",
            format!("wire::unpack: unknown version flag 0x{:02x}", blob[0]),
        ));
    }
    let nonce: &[u8; NONCE_LEN] = blob[1..HEADER_LEN]
        .try_into()
        .map_err(|_| DbError::internal("wire::unpack: nonce slice size mismatch"))?;
    Ok((nonce, &blob[HEADER_LEN..]))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Round-trip: pack then unpack yields the same nonce + payload.
    #[test]
    fn pack_unpack_round_trip() {
        let nonce = [7u8; NONCE_LEN];
        // 32 bytes of payload + 16-byte tag = 48-byte ct+tag.
        let mut ct_and_tag = vec![0xAB; 32];
        ct_and_tag.extend_from_slice(&[0xCD; GCM_TAG_LEN]);
        let packed = pack(&nonce, &ct_and_tag).expect("pack");
        // [1 version | 12 nonce | 32 ct | 16 tag] = 61 bytes.
        assert_eq!(packed.len(), HEADER_LEN + ct_and_tag.len());
        assert_eq!(packed[0], WIRE_VERSION_V1);

        let (out_nonce, out_payload) = unpack(&packed).expect("unpack");
        assert_eq!(out_nonce, &nonce);
        assert_eq!(out_payload, ct_and_tag.as_slice());
    }

    /// Unpack rejects a blob whose length is below `header + tag`.
    #[test]
    fn unpack_rejects_too_short() {
        // 1-byte blob — clearly too short.
        let too_short = vec![WIRE_VERSION_V1];
        let err = unpack(&too_short).expect_err("too-short blob must error");
        match err {
            DbError::ValidationFailed { code, .. } => {
                assert_eq!(code, "encryption_aead_failed");
            }
            other => panic!("expected ValidationFailed, got {other:?}"),
        }

        // header-only blob (no tag yet) — also rejected.
        let header_only = vec![WIRE_VERSION_V1; HEADER_LEN];
        assert!(unpack(&header_only).is_err());

        // exactly header + tag - 1 — boundary.
        let boundary = vec![WIRE_VERSION_V1; HEADER_LEN + GCM_TAG_LEN - 1];
        assert!(unpack(&boundary).is_err());
    }

    /// Reject any version other than `WIRE_VERSION_V1`.
    #[test]
    fn unpack_rejects_unknown_version() {
        let mut blob = vec![0xFFu8]; // unknown version
        blob.extend_from_slice(&[0u8; NONCE_LEN]);
        blob.extend_from_slice(&[0u8; GCM_TAG_LEN]);
        let err = unpack(&blob).expect_err("unknown version must error");
        match err {
            DbError::ValidationFailed { code, .. } => {
                assert_eq!(code, "encryption_aead_failed");
            }
            other => panic!("expected ValidationFailed, got {other:?}"),
        }

        // A nearby version is unknown too; do not accept it as a compatible variant.
        let mut blob_v2 = vec![0x02u8];
        blob_v2.extend_from_slice(&[0u8; NONCE_LEN]);
        blob_v2.extend_from_slice(&[0u8; GCM_TAG_LEN]);
        assert!(unpack(&blob_v2).is_err());
    }

    /// Packing adds only the declared header to the authenticated payload.
    #[test]
    fn wire_size_is_header_plus_payload() {
        let nonce = [0u8; NONCE_LEN];
        for payload_len in [16, 32, 64, 1024] {
            let ct = vec![0u8; payload_len];
            let packed = pack(&nonce, &ct).expect("pack");
            assert_eq!(
                packed.len(),
                1 + NONCE_LEN + payload_len,
                "payload_len={payload_len}"
            );
        }
    }

    /// `pack` rejects a ciphertext+tag slice shorter than the GCM tag
    /// length — that's an aes-gcm invariant breach, surfaced as an
    /// internal error.
    #[test]
    fn pack_rejects_payload_shorter_than_tag() {
        let nonce = [0u8; NONCE_LEN];
        let bad = vec![0u8; GCM_TAG_LEN - 1];
        let err = pack(&nonce, &bad).expect_err("under-tag pack must error");
        assert!(matches!(err, DbError::Internal { .. }));
    }
}
