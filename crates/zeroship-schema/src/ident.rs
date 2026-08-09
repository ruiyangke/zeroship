//! The single source of truth for capping a **derived** SQL identifier to the
//! Postgres `NAMEDATALEN` budget.
//!
//! # Why derived names get capped and authored names get refused
//!
//! `query::validate_collection` / `query::validate_field_name` REFUSE a name
//! over [`PG_MAX_IDENT_BYTES`]. That is right for names the creator chose: they
//! can shorten them, and a refusal tells them so.
//!
//! It is wrong for names the platform DERIVES from those — `<coll>_<col>_idx`,
//! `<coll>__<col>_masked_idx`, `<coll>__fts_idx`, `<field>_fkey`, … . A
//! collection at exactly the 63-byte ceiling passes validation, yet every name
//! derived from it overflows. Refusing there would fail the creator for
//! something they did not do, so the derived name is capped instead.
//!
//! # Why capping is not optional
//!
//! Postgres does not error on an over-long identifier. It truncates to
//! `NAMEDATALEN - 1` bytes and emits a NOTICE. Two derived names that share
//! their first 63 bytes therefore become ONE identifier — and because almost
//! every `CREATE INDEX` this crate emits carries `IF NOT EXISTS`, the second
//! one is a SILENT no-op: both statements report success, the truncation and
//! the skip are both NOTICEs, and only one index exists.
//!
//! For a `CREATE UNIQUE INDEX` that means the uniqueness the schema declares
//! does not exist in the database, with nothing in the catalog to notice it.
//! For `<coll>__fts_idx` on a 63-byte collection the truncation lands on
//! exactly the collection name, and since Postgres indexes share the `pg_class`
//! namespace with tables, `IF NOT EXISTS` finds the TABLE and skips.
//!
//! # One function, no second implementation
//!
//! Every derivation site in this crate (and plugin-db's Postgres FTS builder)
//! routes through [`cap_ident_name`]. A second copy is the actual failure mode
//! here: this crate previously carried FOUR inlined copies of the cap, and the
//! vendored migration engine carries a fifth with a DIFFERENT budget and hash
//! encoding — so for a natural name in the 61..=63-byte window the engine
//! emitted it verbatim while the runtime hashed it, and the two disagreed about
//! what the index is called. [`cap_ident_name`] is byte-for-byte the engine's
//! `zero_migrate::plan::author::cap_ident_name`, so the two now agree.
//!
//! `ident_cap_has_exactly_one_implementation` (below) fails the build if any
//! other file in this crate starts hashing identifiers on its own.

/// The Postgres identifier length limit (`NAMEDATALEN - 1`), in bytes.
///
/// An identifier longer than this is silently truncated *by the server*, which
/// is why nothing downstream may emit a longer one. Postgres can be compiled
/// with a larger `NAMEDATALEN`, but 63 is the default and the only value we
/// target; capping below the server's real limit is always safe.
pub const PG_MAX_IDENT_BYTES: usize = 63;

/// Number of hex characters of the full-name digest appended on overflow.
///
/// 10 hex chars = 40 bits. Distinct inputs that share a truncated prefix would
/// have to collide in 40 bits of SHA-256 to collapse onto one identifier.
const HASH_HEX_CHARS: usize = 10;

/// Cap a DERIVED identifier to at most [`PG_MAX_IDENT_BYTES`] bytes,
/// deterministically and collision-safely.
///
/// * A name that already fits is returned **unchanged** — capping must not
///   churn the identifier of every index that was never at risk.
/// * A name that overflows becomes `<readable prefix>_<10 hex of SHA-256 of the
///   FULL natural name>`. The hash covers the whole input, not the truncated
///   remainder, so two inputs that diverge only after byte 63 still produce two
///   different identifiers.
/// * Same input, same output, always: `CREATE ... IF NOT EXISTS` and the
///   matching `DROP` must agree across processes and releases.
///
/// The prefix is truncated on a UTF-8 char boundary. Derived identifiers are
/// ASCII in practice (both validators enforce an ASCII allowlist), but slicing
/// by raw byte offset would be a panic waiting for the first caller that is
/// not.
///
/// Byte-identical to the vendored migration engine's
/// `zero_migrate::plan::author::cap_ident_name` — keep them that way, or the
/// engine and the runtime will disagree about what an index is called.
#[must_use]
pub fn cap_ident_name(natural: &str) -> String {
    use sha2::{Digest, Sha256};

    if natural.len() <= PG_MAX_IDENT_BYTES {
        return natural.to_string();
    }

    // Lowercase hex, hand-rolled: the crate does not depend on `hex` and a new
    // dependency for ten characters is not worth it. The table lookup is
    // masked to 0..=15, so it is total — no panic path to document.
    const HEX: [char; 16] = [
        '0', '1', '2', '3', '4', '5', '6', '7', '8', '9', 'a', 'b', 'c', 'd', 'e', 'f',
    ];
    let digest = Sha256::digest(natural.as_bytes());
    let mut suffix = String::with_capacity(HASH_HEX_CHARS);
    for byte in digest.iter().take(HASH_HEX_CHARS / 2) {
        suffix.push(HEX[usize::from(byte >> 4)]);
        suffix.push(HEX[usize::from(byte & 0x0f)]);
    }

    // Reserve `_<suffix>` on the tail; the rest is a readable prefix of the
    // natural name.
    let budget = PG_MAX_IDENT_BYTES - (1 + suffix.len());
    let mut prefix = String::with_capacity(budget);
    for ch in natural.chars() {
        if prefix.len() + ch.len_utf8() > budget {
            break;
        }
        prefix.push(ch);
    }
    format!("{prefix}_{suffix}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A name inside the budget must come back byte-for-byte. A cap that
    /// rewrote short names would rename every index that was never at risk.
    ///
    /// What this does NOT catch: it says nothing about the overflow branch, and
    /// nothing about which names the derivation sites feed in.
    #[test]
    fn short_names_are_returned_unchanged() {
        for n in ["users_email_idx", "a", "", &"x".repeat(62), &"y".repeat(63)] {
            assert_eq!(cap_ident_name(n), n, "name within budget must not be rewritten");
        }
    }

    /// One byte over the budget is where the hash must start.
    #[test]
    fn the_boundary_is_inclusive_at_63_and_caps_at_64() {
        assert_eq!(cap_ident_name(&"z".repeat(63)).len(), 63);
        let capped = cap_ident_name(&"z".repeat(64));
        assert_ne!(capped, "z".repeat(64));
        assert_eq!(capped.len(), PG_MAX_IDENT_BYTES);
    }

    /// `CREATE ... IF NOT EXISTS` and its `DROP` run in different processes and
    /// different releases; they must derive the same name.
    ///
    /// What this does NOT catch: determinism within one build only — it cannot
    /// detect a future change to the digest or the budget, which would be
    /// self-consistently deterministic and still wrong.
    #[test]
    fn capping_is_deterministic() {
        let long = format!("{}__{}_idx", "c".repeat(63), "alpha_masked");
        assert_eq!(cap_ident_name(&long), cap_ident_name(&long));
    }

    /// The point of the whole module: inputs that are identical for the first
    /// 63 bytes and diverge only afterwards must NOT collapse onto one
    /// identifier. Truncation alone would; the hash covers the full name.
    #[test]
    fn inputs_diverging_after_the_truncation_point_stay_distinct() {
        let coll = "c".repeat(63);
        let a = cap_ident_name(&format!("{coll}__alpha_masked_idx"));
        let b = cap_ident_name(&format!("{coll}__beta_masked_idx"));
        assert_ne!(a, b, "distinct derived names collapsed: {a} / {b}");
        assert!(a.len() <= PG_MAX_IDENT_BYTES && b.len() <= PG_MAX_IDENT_BYTES);
    }

    /// Every output must fit, whatever the input length or shape.
    #[test]
    fn every_output_fits_the_budget() {
        for len in 0..200 {
            let out = cap_ident_name(&"n".repeat(len));
            assert!(out.len() <= PG_MAX_IDENT_BYTES, "len {len} produced {} bytes", out.len());
        }
    }

    /// A multi-byte input must not be sliced mid-character. Both identifier
    /// validators reject non-ASCII today, so this guards the function against a
    /// future caller rather than a current one.
    #[test]
    fn multibyte_input_is_truncated_on_a_char_boundary() {
        // 3 bytes per char, 40 chars = 120 bytes.
        let out = cap_ident_name(&"日".repeat(40));
        assert!(out.len() <= PG_MAX_IDENT_BYTES);
        assert!(std::str::from_utf8(out.as_bytes()).is_ok());
    }

    /// Structural guard for the "one function" rule: the identifier cap is
    /// implemented here and nowhere else in the crate. Four inlined copies is
    /// what this file replaced; a fifth would drift the same way.
    ///
    /// What this does NOT catch: a second implementation that hashes with
    /// something other than `sha2`, or one that lives in a different crate.
    #[test]
    fn ident_cap_has_exactly_one_implementation() {
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut hashing_files: Vec<String> = Vec::new();
        for entry in std::fs::read_dir(&src).expect("src/ is readable") {
            let path = entry.expect("dir entry").path();
            if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                continue;
            }
            let body = std::fs::read_to_string(&path).expect("source is utf-8");
            if body.contains("Sha256") {
                hashing_files
                    .push(path.file_name().and_then(|n| n.to_str()).unwrap_or("?").to_string());
            }
        }
        assert_eq!(
            hashing_files,
            vec!["ident.rs".to_string()],
            "identifier hashing must live only in ident.rs; found it in {hashing_files:?}"
        );
    }
}
