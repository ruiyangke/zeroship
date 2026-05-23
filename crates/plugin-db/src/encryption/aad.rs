//! Canonical AAD (additional authenticated data) construction for the
//! column-encryption AEAD.
//!
//! AAD binds a ciphertext to its logical context. AES-GCM verifies the
//! AAD as part of decryption: any mismatch produces a tag-verification
//! failure (the SDK-visible `encryption_aead_failed` error). Per the
//! Camp-A resolution in `docs/proposals/p5-encryption-backup-implementation-plan.md`
//! §13 (2026-05-24), plugin-db binds:
//!
//! - **Randomised mode**: `(collection, column, row_pk_bytes)`.
//!   row_pk binding is what defeats the ciphertext-oracle attack: an
//!   attacker with UPDATE-only access who copies row A's ciphertext
//!   into row B's column slot now triggers AAD mismatch on read,
//!   surfacing the tamper instead of silently leaking row A's
//!   plaintext through row B's API surface.
//! - **Deterministic mode**: `(collection, column)` only.
//!   row_pk is intentionally omitted because deterministic mode's
//!   defining property — "same plaintext under the same `(collection,
//!   column)` produces the same ciphertext" — is what makes the
//!   B-tree-on-ciphertext equality lookup work. Binding row_pk would
//!   produce a different ciphertext per row and break that lookup
//!   (the entire reason deterministic mode exists).
//!
//! ## Length-prefix encoding
//!
//! Each segment is encoded as `[u32 big-endian length][bytes]`. This
//! removes the obvious collision class where two different
//! `(collection, column)` pairs concat to the same byte string:
//!
//! ```text
//! ("ab",  "c") =/= ("a",  "bc")
//! ```
//!
//! Without the length prefix both encode to `b"abc"`. With it the
//! first encodes `00 00 00 02 'a' 'b' 00 00 00 01 'c'`, the second
//! `00 00 00 01 'a' 00 00 00 02 'b' 'c'`.
//!
//! The trailing `row_pk_bytes` block is always emitted with its own
//! length prefix; `None` (deterministic mode) and `Some(b"")` (empty
//! pk — an unsupported caller mistake we still want to be
//! deterministic about) both encode `00 00 00 00` and therefore
//! produce byte-identical AAD. The aad_pk_none_and_empty_equivalent
//! test pins that property; the docs comment on the public function
//! flags it explicitly so callers know not to rely on the distinction.

/// Build the canonical AAD bytes for an AEAD encrypt / decrypt
/// operation on an encrypted column.
///
/// `row_pk_bytes` is `Some(bytes)` in
/// [`crate::backend::EncryptionMode::Randomised`] (Camp A binding),
/// `None` in [`crate::backend::EncryptionMode::Deterministic`]. The
/// caller is responsible for choosing the mode; this function just
/// serialises the result. See the module-level docs for the
/// rationale.
///
/// **Edge case**: `Some(b"")` and `None` produce byte-identical AAD
/// (both encode `len=0`). Callers must not rely on the distinction —
/// "empty pk" is not a supported state in plugin-db (typed_id PKs are
/// minted SDK-side and always non-empty).
#[must_use]
pub fn canonical_aad(
    collection: &str,
    column: &str,
    row_pk_bytes: Option<&[u8]>,
) -> Vec<u8> {
    // Pre-allocate enough for typical names (collection ~16B, column
    // ~16B, pk ~32B + three length prefixes). Over-allocation is
    // cheap; the helper runs once per encrypt/decrypt of a column
    // value.
    let mut out = Vec::with_capacity(
        12 + collection.len() + column.len() + row_pk_bytes.map_or(0, <[u8]>::len),
    );
    extend_with_len(&mut out, collection.as_bytes());
    extend_with_len(&mut out, column.as_bytes());
    extend_with_len(&mut out, row_pk_bytes.unwrap_or(&[]));
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
        let a = canonical_aad("ab", "c", None);
        let b = canonical_aad("a", "bc", None);
        assert_ne!(a, b);
    }

    /// row_pk binding flips the AAD — same `(collection, column)`
    /// with a `Some(pk)` block differs from `None`.
    #[test]
    fn row_pk_changes_aad() {
        let with_pk = canonical_aad("users", "ssn", Some(b"usr_01HX"));
        let without_pk = canonical_aad("users", "ssn", None);
        assert_ne!(with_pk, without_pk);
    }

    /// Different row PKs under the same `(collection, column)`
    /// produce different AAD — the ciphertext-oracle defence
    /// (Camp A binding) hinges on this.
    #[test]
    fn different_row_pks_differ() {
        let a = canonical_aad("users", "ssn", Some(b"usr_01"));
        let b = canonical_aad("users", "ssn", Some(b"usr_02"));
        assert_ne!(a, b);
    }

    /// Deterministic output: same input always yields the same
    /// bytes. AAD must not carry randomness — that would make
    /// decrypt impossible without storing the AAD beside the
    /// ciphertext.
    #[test]
    fn deterministic_output() {
        let a = canonical_aad("c", "f", Some(b"pk"));
        let b = canonical_aad("c", "f", Some(b"pk"));
        assert_eq!(a, b);
    }

    /// `Some(b"")` and `None` map to byte-identical AAD because both
    /// encode `len = 0` for the row-pk segment. Documented in the
    /// public function's doc-comment so callers know not to rely on
    /// the distinction.
    #[test]
    fn aad_pk_none_and_empty_equivalent() {
        let none = canonical_aad("c", "f", None);
        let empty = canonical_aad("c", "f", Some(&[]));
        assert_eq!(none, empty);
    }

    /// Spot-check the exact byte layout — locks in the wire
    /// encoding for cross-backend compatibility (PR 3 SQLite arm and
    /// PR 2 PG arm produce identical AAD bytes from this helper).
    #[test]
    fn explicit_byte_layout() {
        // collection = "u" (1B), column = "s" (1B), pk = "p" (1B).
        // Encoding: 00 00 00 01 75 00 00 00 01 73 00 00 00 01 70
        let aad = canonical_aad("u", "s", Some(b"p"));
        assert_eq!(
            aad,
            vec![0, 0, 0, 1, b'u', 0, 0, 0, 1, b's', 0, 0, 0, 1, b'p']
        );

        // None pk → final segment length 0, no bytes.
        let aad_none = canonical_aad("u", "s", None);
        assert_eq!(aad_none, vec![0, 0, 0, 1, b'u', 0, 0, 0, 1, b's', 0, 0, 0, 0]);
    }
}
