//! Canonical authenticated context for column encryption.
//!
//! The wire version, database, collection, column and row identity are
//! length-prefixed. Moving ciphertext to another database, row or column fails
//! authentication.
//!
//! **The database and not the app.** Rows belong to the database, and every app
//! the project binds to it is entitled to the same plaintext, so binding the
//! tenant here would make two co-binding-holders unable to read each other's
//! rows while leaving one app's two databases sharing a context. At-rest
//! encryption is not the fence between apps; a column-level `GRANT` is, and on
//! the subscription path a column the publication does not carry.

use zeroship_core::DatabaseId;

/// Build the authenticated context for a stored field value.
#[must_use]
pub fn canonical_aad(
    database: &DatabaseId,
    collection: &str,
    column: &str,
    row_pk_bytes: &[u8],
) -> Vec<u8> {
    let database = database.as_str();
    let mut out = Vec::with_capacity(
        21 + database.len() + collection.len() + column.len() + row_pk_bytes.len(),
    );
    extend_with_len(&mut out, &[crate::encryption::wire::WIRE_VERSION_V2]);
    extend_with_len(&mut out, database.as_bytes());
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

    fn database() -> DatabaseId {
        DatabaseId::mint()
    }

    /// `(coll="ab", col="c") =/= (coll="a", col="bc")` - pin the
    /// length-prefix collision defence.
    #[test]
    fn length_prefix_blocks_concat_collision() {
        let database = database();
        let a = canonical_aad(&database, "ab", "c", b"row_a");
        let b = canonical_aad(&database, "a", "bc", b"row_a");
        assert_ne!(a, b);
    }

    /// **The second axis this design opens.** One app reaching two databases
    /// derives a key per database, but the AAD is an independent fence: under
    /// ONE key, a ciphertext written in one database does not verify in
    /// another.
    ///
    /// Holding the key constant is the whole point. A wrong key and a wrong
    /// AAD produce the same `encryption_aead_failed`, so an arm that let both
    /// vary would pass over a canonical AAD that had stopped binding the
    /// database at all.
    #[test]
    fn one_key_cannot_replay_a_ciphertext_between_two_databases() {
        let key = crate::encryption::AeadKey { k_enc: [7; 32] };
        let here = database();
        let there = database();
        assert_ne!(here, there, "the control: two mints are two databases");

        let written = canonical_aad(&here, "users", "secret", b"same_row");
        let lifted = canonical_aad(&there, "users", "secret", b"same_row");
        assert_ne!(written, lifted, "the database must reach the bytes");

        let ciphertext = crate::encryption::encrypt(&key, b"secret", &written).unwrap();
        assert_eq!(
            crate::encryption::decrypt(&key, &ciphertext, &written).unwrap(),
            b"secret",
            "the control: the blob is intact and the key is right"
        );
        assert!(crate::encryption::decrypt(&key, &ciphertext, &lifted).is_err());
    }

    /// **The first axis.** Two apps bound to ONE database reconstruct the same
    /// context, so neither is fenced out of rows it is entitled to read. The
    /// tenant is not an argument at all, which is what makes this structural
    /// rather than a convention every call site has to keep.
    #[test]
    fn co_binding_holders_reconstruct_one_context() {
        let shared = database();
        assert_eq!(
            canonical_aad(&shared, "users", "secret", b"same_row"),
            canonical_aad(&shared, "users", "secret", b"same_row")
        );
    }

    /// Different row PKs under the same `(collection, column)`
    /// produce different AAD - the ciphertext-oracle defence
    /// (Camp A binding) hinges on this.
    #[test]
    fn different_row_pks_differ() {
        let database = database();
        let a = canonical_aad(&database, "users", "ssn", b"usr_01");
        let b = canonical_aad(&database, "users", "ssn", b"usr_02");
        assert_ne!(a, b);
    }

    /// Deterministic output: same input always yields the same
    /// bytes. AAD must not carry randomness - that would make
    /// decrypt impossible without storing the AAD beside the
    /// ciphertext.
    #[test]
    fn deterministic_output() {
        let database = database();
        let a = canonical_aad(&database, "c", "f", b"pk");
        let b = canonical_aad(&database, "c", "f", b"pk");
        assert_eq!(a, b);
    }

    /// Spot-check the exact byte layout - locks in the wire
    /// encoding for cross-backend compatibility (the SQLite arm and
    /// the PG arm produce identical AAD bytes from this helper).
    #[test]
    fn explicit_byte_layout() {
        let database =
            DatabaseId::parse("dbs_03cgepu94hyemwpcipafo7264").expect("a canonical database id");
        let aad = canonical_aad(&database, "u", "s", b"p");
        let mut expected = vec![0, 0, 0, 1, 0x02, 0, 0, 0, 29];
        expected.extend_from_slice(b"dbs_03cgepu94hyemwpcipafo7264");
        expected.extend_from_slice(&[0, 0, 0, 1, b'u', 0, 0, 0, 1, b's', 0, 0, 0, 1, b'p']);
        assert_eq!(aad, expected);
    }

    /// DB-14: the wire version is bound into the AAD (first segment).
    #[test]
    fn aad_binds_wire_version_db14() {
        let aad = canonical_aad(&database(), "users", "ssn", b"row_a");
        // First length-prefixed segment is the 1-byte wire version.
        assert_eq!(
            &aad[..5],
            &[0, 0, 0, 1, super::super::wire::WIRE_VERSION_V2]
        );
    }
}
