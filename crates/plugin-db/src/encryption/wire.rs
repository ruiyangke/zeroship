//! Wire format for column-encrypted blobs.
//!
//! ```text
//! [version_flag (1 B) | nonce (12 B) | ciphertext + GCM tag (N B)]
//! ```
//!
//! ## Why the leading version flag
//!
//! P5 baseline ships `version_flag = 0x01` (AAD = `(collection, column,
//! row_pk_bytes?)`). The post-system-fields proposal (see
//! `docs/proposals/platform-system-fields.md` §8) introduces a
//! `version_flag = 0x02` shape where AAD additionally binds the
//! row-version bytes. The leading byte is reserved from P5 day one so
//! the upgrade requires no in-place data migration of P5-era
//! ciphertext — `re-encrypt-on-write` produces v2 blobs naturally,
//! and decryption inspects the flag to pick the right AAD
//! reconstruction.
//!
//! ## Why not split nonce from ciphertext+tag at the API boundary
//!
//! The Rust-Crypto `Aead::encrypt` / `Aead::decrypt` impls return /
//! accept the ciphertext-with-appended-tag as a single byte slice. We
//! keep that shape on the wire too rather than splitting tag off, so
//! the unpack path can hand the slice straight to `Aes256Gcm::decrypt`
//! without re-stitching.

use crate::error::DbError;

/// Reserved version flag for the P5-baseline AAD shape.
///
/// PR 1 emits only this version; the post-system-fields phase will
/// introduce `0x02` alongside the version-bytes AAD extension.
pub const WIRE_VERSION_V1: u8 = 0x01;

/// AES-GCM nonce length (RFC 5116 §5.3). The aes-gcm crate's
/// `Nonce::from_slice` panics on anything else, so this is a
/// hard-coded invariant.
const NONCE_LEN: usize = 12;

/// `[version_flag (1) | nonce (12)]` = 13 bytes.
const HEADER_LEN: usize = 1 + NONCE_LEN;

/// AES-GCM authentication tag length (RFC 5116 §5.3).
const GCM_TAG_LEN: usize = 16;

/// Pack a freshly-encrypted blob: prefix the v1 version flag, then the
/// nonce, then the ciphertext-with-appended-tag.
///
/// Returns `Err(Internal)` if the cipher output is shorter than the
/// minimum 16-byte tag — that's an invariant breach inside aes-gcm,
/// not user input, so the variant choice is internal-error rather
/// than validation.
pub fn pack(nonce: &[u8; NONCE_LEN], ct_and_tag: &[u8]) -> Result<Vec<u8>, DbError> {
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
pub fn unpack(blob: &[u8]) -> Result<(&[u8; NONCE_LEN], &[u8]), DbError> {
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

    /// Unpack rejects any version flag other than `0x01` in PR 1.
    /// `0x02` is reserved for the post-system-fields shape but is not
    /// yet emitted; treat as unknown.
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

        // 0x02 also rejected today (post-system-fields shape not yet
        // emitted; this test pins the reservation).
        let mut blob_v2 = vec![0x02u8];
        blob_v2.extend_from_slice(&[0u8; NONCE_LEN]);
        blob_v2.extend_from_slice(&[0u8; GCM_TAG_LEN]);
        assert!(unpack(&blob_v2).is_err());
    }

    /// Wire layout is exactly `1 + 12 + len(ct+tag)` bytes — pin the
    /// invariant so a future change that introduces extra framing
    /// (sentinel prefix, alignment padding, …) trips here.
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
