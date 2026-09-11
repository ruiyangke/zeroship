//! Canonical authenticated context for column encryption.
//!
//! The wire version, collection, column and row identity are length-prefixed.
//! Moving ciphertext to another row or column fails authentication.

/// Build the authenticated context for a stored field value.
#[must_use]
pub fn canonical_aad(collection: &str, column: &str, row_pk_bytes: &[u8]) -> Vec<u8> {
    // Pre-allocate enough for typical names (collection ~16B, column
    // ~16B, pk ~32B + three length prefixes). Over-allocation is
    // cheap; the helper runs once per encrypt/decrypt of a column
    // value.
    let mut out = Vec::with_capacity(17 + collection.len() + column.len() + row_pk_bytes.len());
    // DB-14: bind the wire version FIRST, so the version byte (unauthenticated
    // framing in the wire envelope) is authenticated by the AEAD tag. A future
    // `0x02` (row-version-binding) shape that downgraded a stored `0x02` → `0x01`
    // would force the weaker AAD reconstruction; binding the version here makes
    // that mismatch fail the tag. When `0x02` ships, the version MUST be threaded
    // through this function as a parameter rather than the constant below.
    extend_with_len(&mut out, &[crate::encryption::wire::WIRE_VERSION_V1]);
    extend_with_len(&mut out, collection.as_bytes());
    extend_with_len(&mut out, column.as_bytes());
    extend_with_len(&mut out, row_pk_bytes);
    out
}

/// Append `[u32-BE length][bytes]` to `out`.
fn extend_with_len(out: &mut Vec<u8>, bytes: &[u8]) {
    // `u32::try_from(bytes.len()).unwrap_or(u32::MAX)` would silently
    // truncate; collection / column names are TEXT-bounded by the
    // schema layer well below 4 GiB, and row PKs are typed_ids
    // (~26 bytes), so a real-world overflow is impossible. We assert
    // here so a future caller passing a giant blob (e.g. a row PK
    // mistakenly = the whole row) fails loud rather than producing
    // truncated AAD.
    debug_assert!(
        bytes.len() <= u32::MAX as usize,
        "canonical_aad segment exceeds u32 length cap ({} bytes)",
        bytes.len()
    );
    #[allow(clippy::cast_possible_truncation)]
    let len = bytes.len() as u32;
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(bytes);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `(coll="ab", col="c") =/= (coll="a", col="bc")` — pin the
    /// length-prefix collision defence.
    #[test]
    fn length_prefix_blocks_concat_collision() {
        let a = canonical_aad("ab", "c", b"row_a");
        let b = canonical_aad("a", "bc", b"row_a");
        assert_ne!(a, b);
    }

    /// Different row PKs under the same `(collection, column)`
    /// produce different AAD — the ciphertext-oracle defence
    /// (Camp A binding) hinges on this.
    #[test]
    fn different_row_pks_differ() {
        let a = canonical_aad("users", "ssn", b"usr_01");
        let b = canonical_aad("users", "ssn", b"usr_02");
        assert_ne!(a, b);
    }

    /// Deterministic output: same input always yields the same
    /// bytes. AAD must not carry randomness — that would make
    /// decrypt impossible without storing the AAD beside the
    /// ciphertext.
    #[test]
    fn deterministic_output() {
        let a = canonical_aad("c", "f", b"pk");
        let b = canonical_aad("c", "f", b"pk");
        assert_eq!(a, b);
    }

    /// Spot-check the exact byte layout — locks in the wire
    /// encoding for cross-backend compatibility (the SQLite arm and
    /// the PG arm produce identical AAD bytes from this helper).
    #[test]
    fn explicit_byte_layout() {
        // version = 0x01 (DB-14, bound first), collection = "u", column = "s",
        // pk = "p". Encoding: 00000001 01 | 00000001 75 | 00000001 73 | 00000001 70
        let aad = canonical_aad("u", "s", b"p");
        assert_eq!(
            aad,
            vec![
                0, 0, 0, 1, 0x01, 0, 0, 0, 1, b'u', 0, 0, 0, 1, b's', 0, 0, 0, 1, b'p'
            ]
        );
    }

    /// DB-14: the wire version is bound into the AAD (first segment).
    #[test]
    fn aad_binds_wire_version_db14() {
        let aad = canonical_aad("users", "ssn", b"row_a");
        // First length-prefixed segment is the 1-byte wire version.
        assert_eq!(
            &aad[..5],
            &[0, 0, 0, 1, super::super::wire::WIRE_VERSION_V1]
        );
    }
}
