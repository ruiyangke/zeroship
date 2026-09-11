//! Canonical authenticated context for column encryption.
//!
//! The wire version, app, collection, column and row identity are length-prefixed.
//! Moving ciphertext to another app, row or column fails authentication.

/// Build the authenticated context for a stored field value.
#[must_use]
pub fn canonical_aad(app_id: &str, collection: &str, column: &str, row_pk_bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(
        21 + app_id.len() + collection.len() + column.len() + row_pk_bytes.len(),
    );
    extend_with_len(&mut out, &[crate::encryption::wire::WIRE_VERSION_V1]);
    extend_with_len(&mut out, app_id.as_bytes());
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
        let a = canonical_aad("app", "ab", "c", b"row_a");
        let b = canonical_aad("app", "a", "bc", b"row_a");
        assert_ne!(a, b);
    }

    #[test]
    fn shared_project_key_cannot_replay_between_apps() {
        let key = crate::encryption::AeadKey { k_enc: [7; 32] };
        let a = canonical_aad("app_a", "users", "secret", b"same_row");
        let b = canonical_aad("app_b", "users", "secret", b"same_row");
        let ciphertext = crate::encryption::encrypt(&key, b"secret", &a).unwrap();
        assert!(crate::encryption::decrypt(&key, &ciphertext, &b).is_err());
    }

    /// Different row PKs under the same `(collection, column)`
    /// produce different AAD — the ciphertext-oracle defence
    /// (Camp A binding) hinges on this.
    #[test]
    fn different_row_pks_differ() {
        let a = canonical_aad("app", "users", "ssn", b"usr_01");
        let b = canonical_aad("app", "users", "ssn", b"usr_02");
        assert_ne!(a, b);
    }

    /// Deterministic output: same input always yields the same
    /// bytes. AAD must not carry randomness — that would make
    /// decrypt impossible without storing the AAD beside the
    /// ciphertext.
    #[test]
    fn deterministic_output() {
        let a = canonical_aad("app", "c", "f", b"pk");
        let b = canonical_aad("app", "c", "f", b"pk");
        assert_eq!(a, b);
    }

    /// Spot-check the exact byte layout — locks in the wire
    /// encoding for cross-backend compatibility (the SQLite arm and
    /// the PG arm produce identical AAD bytes from this helper).
    #[test]
    fn explicit_byte_layout() {
        let aad = canonical_aad("a", "u", "s", b"p");
        assert_eq!(
            aad,
            vec![
                0, 0, 0, 1, 0x01, 0, 0, 0, 1, b'a', 0, 0, 0, 1, b'u', 0, 0, 0, 1, b's', 0, 0, 0, 1,
                b'p'
            ]
        );
    }

    /// DB-14: the wire version is bound into the AAD (first segment).
    #[test]
    fn aad_binds_wire_version_db14() {
        let aad = canonical_aad("app", "users", "ssn", b"row_a");
        // First length-prefixed segment is the 1-byte wire version.
        assert_eq!(
            &aad[..5],
            &[0, 0, 0, 1, super::super::wire::WIRE_VERSION_V1]
        );
    }
}
