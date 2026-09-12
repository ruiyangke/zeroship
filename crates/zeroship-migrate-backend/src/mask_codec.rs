//! The mask-sentinel CODEC - the contract between the schema layer
//! (which *writes* the sentinel into DDL) and the data plane (which
//! *reads* it back at runtime to drive the mask read-pass).
//!
//! Split out of the original data-plane `crud::mask_backfill` module because
//! the *codec* (build/parse the `zero-migrate:mask:` sentinel string) is a
//! schema-shape concern and lives here.
//!
//! **This block said the backfill RUNNER - `run_mask_backfill` /
//! `run_mask_rewrite` - "stays in the data plane". Neither function ever
//! existed there**; `crud::mask_backfill`'s own header said so in its first
//! four lines, and three crates repeated the claim anyway. The runner is the
//! engine's (#30), and that module was deleted on 2026-09-02 with zero
//! callers.
//!
//! The `(MaskKind, Classification)` types this codec round-trips live in
//! `crate::schema::diff` (the schema metadata types).

use crate::mask_meta::{Classification, EncryptionMeta, MaskKind, WrappedType};
use crate::schema_error::MaskSentinelError;

/// The encryption-sentinel prefix this engine persists.
///
/// # Why this is a constant and not a knob
///
/// It was a knob until 2026-09-04: a `SentinelPrefix` struct plus
/// `build_*_with` / `parse_*_with` pairs, so "a host that must interoperate with
/// a legacy writer" could inject that writer's prefix. Nothing ever injected
/// one - `SentinelPrefix` occurred ten times in the whole tree, all inside its
/// own defining file - and the reader that was supposed to be interoperated
/// with (`zeroship-data-sql`'s copy of this codec, which the data plane uses to
/// read the live catalog) simply spelled the sentinel differently and never
/// learned this one.
///
/// That is what the knob cost. `zeroship_data_orm::protection::protection_floor`
/// refuses a write whose descriptor dropped a protection the catalog still
/// records; on every table THIS engine created it introspected, matched no
/// sentinel, concluded nothing was protected, and permitted the downgrade. The
/// spellings are converged now, and the knob is gone rather than wired, because
/// a per-host prefix is precisely the shape that lets them diverge again -
/// silently, and in the fail-open direction.
pub const ENC_SENTINEL_PREFIX: &str = "zero-migrate:enc:";

/// The mask-sentinel prefix this engine persists. See [`ENC_SENTINEL_PREFIX`].
pub const MASK_SENTINEL_PREFIX: &str = "zero-migrate:mask:";

/// The canonical wire string for a [`WrappedType`].
#[must_use]
fn wrapped_type_as_sql(w: WrappedType) -> &'static str {
    match w {
        WrappedType::String => "string",
        WrappedType::Number => "number",
        WrappedType::Bytes => "bytes",
    }
}

/// Parse a `zero-migrate:enc:` wraps token. `None` for an unknown token.
#[must_use]
fn wrapped_type_from_sql(s: &str) -> Option<WrappedType> {
    match s {
        "string" => Some(WrappedType::String),
        "number" => Some(WrappedType::Number),
        "bytes" => Some(WrappedType::Bytes),
        _ => None,
    }
}

/// Build the canonical encryption-sentinel BODY for an
/// [`EncryptionMeta`]: `zero-migrate:enc:<wraps>`.
///
/// This is the COMMENT-body form (no surrounding `/* */`): on PG it is stored
/// via `COMMENT ON COLUMN "<schema>"."<table>"."<col>" IS '<body>'` on the
/// ENCRYPTED column itself, so PG (which discards the inline `/* zero-migrate:enc */`
/// comment at parse time) can still recover the metadata from `pg_description`.
/// On SQLite the inline form (`query::encryption_sentinel_for_field`, which
/// wraps this same `zero-migrate:enc:...` body in `/* */`) survives in `sqlite_master.sql`.
///
/// The two emitters share the SAME `zero-migrate:enc:<wraps>` body, so the
/// metadata a `generate`d migration carries is byte-identical to the one
/// schema application writes - the verify-bricking guard. The parser side is
/// [`parse_encryption_sentinel`].
#[must_use]
pub fn build_encryption_sentinel(meta: &EncryptionMeta) -> String {
    format!(
        "{ENC_SENTINEL_PREFIX}{}",
        wrapped_type_as_sql(meta.wraps),
    )
}

/// Parse a `zero-migrate:enc:<wraps>` sentinel body back
/// into an [`EncryptionMeta`].
///
/// Accepts either the bare comment body (`zero-migrate:enc:string`, the
/// PG `pg_description` form) or the inline-comment form wrapping it
/// (`/* zero-migrate:enc:string */`, the SQLite `sqlite_master.sql`
/// form) - the leading/trailing `/* */` and whitespace are stripped first, so
/// both introspectors feed the SAME parser.
///
/// Returns `Err(MaskSentinelError)` (the shared sentinel-error type) carrying an
/// `enc_sentinel_malformed` discriminator for any parse failure - wrong prefix,
/// unknown wraps or extra metadata - so a hand-edited or
/// future-version sentinel produces a typed error rather than silently routing
/// through a default codec (the fail-closed contract).
pub fn parse_encryption_sentinel(s: &str) -> Result<EncryptionMeta, MaskSentinelError> {
    // Strip an optional inline `/* ... */` wrapper (the SQLite form) so both
    // the PG comment body and the SQLite inline comment parse identically.
    let trimmed = s.trim();
    let body = trimmed
        .strip_prefix("/*")
        .and_then(|rest| rest.strip_suffix("*/"))
        .map_or(trimmed, str::trim);

    let rest = body.strip_prefix(ENC_SENTINEL_PREFIX).ok_or_else(|| {
        MaskSentinelError::new(format!(
            "enc_sentinel_malformed: expected {ENC_SENTINEL_PREFIX:?} prefix, got {s:?}"
        ))
    })?;
    let wraps = wrapped_type_from_sql(rest).ok_or_else(|| {
        MaskSentinelError::new(format!(
            "enc_sentinel_malformed: expected {ENC_SENTINEL_PREFIX}<wraps> with string, number, or bytes, got {s:?}"
        ))
    })?;
    Ok(EncryptionMeta { wraps })
}

/// Build the canonical mask-sentinel string for a
/// `(kind, classification)` pair.
///
/// Stored on PG via `COMMENT ON COLUMN "<schema>"."<table>"."<sibling>"
/// IS '<sentinel>'` and on SQLite as a `/* <sentinel> */` inline
/// comment after the sibling column DDL. The parser side
/// ([`parse_mask_sentinel`]) accepts the exact same string.
///
/// Format: `zero-migrate:mask:kind=<kind>,classification=<class>`.
#[must_use]
pub fn build_mask_sentinel(kind: MaskKind, classification: Classification) -> String {
    format!(
        "{MASK_SENTINEL_PREFIX}kind={},classification={}",
        kind.as_sql(),
        classification.as_sql(),
    )
}

/// Parse a `zero-migrate:mask:kind=...,classification=...`
/// sentinel string back into a `(MaskKind, Classification)` pair.
///
/// Returns `Err(MaskSentinelError)` whose `.message` carries the
/// `mask_sentinel_malformed` code-discriminator for any parse failure -
/// unknown kind, unknown classification, missing field, extra trailing
/// junk. plugin-db's `From<MaskSentinelError> for DbError` lifts it back
/// into `DbError::Internal { message }` verbatim, so the typed error the
/// introspector surfaces (with the column name appended) is unchanged.
pub fn parse_mask_sentinel(s: &str) -> Result<(MaskKind, Classification), MaskSentinelError> {
    let body = s.strip_prefix(MASK_SENTINEL_PREFIX).ok_or_else(|| {
        MaskSentinelError::new(format!(
            "mask_sentinel_malformed: expected {MASK_SENTINEL_PREFIX:?} prefix, got {s:?}"
        ))
    })?;
    let mut kind_str: Option<&str> = None;
    let mut class_str: Option<&str> = None;
    for piece in body.split(',') {
        let trimmed = piece.trim();
        if let Some(v) = trimmed.strip_prefix("kind=") {
            kind_str = Some(v);
        } else if let Some(v) = trimmed.strip_prefix("classification=") {
            class_str = Some(v);
        } else {
            return Err(MaskSentinelError::new(format!(
                "mask_sentinel_malformed: unrecognised key in {s:?}"
            )));
        }
    }
    let kind_str = kind_str.ok_or_else(|| {
        MaskSentinelError::new(format!("mask_sentinel_malformed: missing kind= in {s:?}"))
    })?;
    let class_str = class_str.ok_or_else(|| {
        MaskSentinelError::new(format!(
            "mask_sentinel_malformed: missing classification= in {s:?}"
        ))
    })?;
    let kind = MaskKind::from_sql(kind_str).ok_or_else(|| {
        MaskSentinelError::new(format!(
            "mask_sentinel_malformed: unknown kind {kind_str:?} in {s:?}"
        ))
    })?;
    let classification = Classification::from_sql(class_str).ok_or_else(|| {
        MaskSentinelError::new(format!(
            "mask_sentinel_malformed: unknown classification {class_str:?} in {s:?}"
        ))
    })?;
    Ok((kind, classification))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The persisted prefixes, pinned as literals.
    ///
    /// Not a tautology over the constants: every OTHER assertion in this module
    /// spells the sentinel out, so renaming a constant alone would go red there
    /// too - but only here does the failure message say what the wire is. The
    /// peer that must agree is `zeroship_data_sql::mask_codec`'s pair of the same
    /// names, which the data plane reads the live catalog with. Nothing in the
    /// type system relates them (their `MaskKind`/`Classification` types are
    /// separate), so the binding is behavioural, and it lives in the peer rather
    /// than here: `cross_codec_parity`, at the bottom of
    /// `crates/zeroship-data-sql/src/mask_codec.rs`, builds with THIS emitter and
    /// parses with that codec and vice versa, over both sentinel families and
    /// both backends' dispatch sites. It sits on that side because
    /// `zeroship-data-sql` already carries the test-only `zeroship-migrate-core`
    /// dev-dependency; this crate has no edge back and must not grow one.
    ///
    /// **This doc named `zeroship-data-v8`'s `mask_flip.rs` until
    /// 2026-09-04.** Those catalog tests now live in
    /// `crates/zeroship-data-orm/src/tests/postgres/protection.rs` and run in
    /// ordinary ORM tests. This crate's suite still needs its own sentinel guard.
    #[test]
    fn the_persisted_sentinel_prefixes_are_the_zero_migrate_brand() {
        assert_eq!(ENC_SENTINEL_PREFIX, "zero-migrate:enc:");
        assert_eq!(MASK_SENTINEL_PREFIX, "zero-migrate:mask:");
    }

    #[test]
    fn build_mask_sentinel_round_trips() {
        let s = build_mask_sentinel(MaskKind::Last4, Classification::Spi);
        assert_eq!(s, "zero-migrate:mask:kind=last4,classification=spi");
        let (kind, class) = parse_mask_sentinel(&s).unwrap();
        assert_eq!(kind, MaskKind::Last4);
        assert_eq!(class, Classification::Spi);
    }

    #[test]
    fn build_mask_sentinel_for_every_kind_classification_pair() {
        let kinds = [
            MaskKind::Full,
            MaskKind::Last4,
            MaskKind::First4,
            MaskKind::Email,
            MaskKind::Name,
            MaskKind::DateYear,
            MaskKind::DateDecade,
            MaskKind::None,
        ];
        let classes = [
            Classification::Public,
            Classification::Pii,
            Classification::Spi,
            Classification::Phi,
            Classification::Pci,
            Classification::Internal,
        ];
        for kind in kinds {
            for class in classes {
                let s = build_mask_sentinel(kind, class);
                let parsed = parse_mask_sentinel(&s).unwrap();
                assert_eq!(parsed, (kind, class));
            }
        }
    }

    #[test]
    fn parse_mask_sentinel_rejects_missing_prefix() {
        let err = parse_mask_sentinel("kind=last4,classification=spi").unwrap_err();
        assert!(err.message().contains("mask_sentinel_malformed"));
    }

    #[test]
    fn parse_mask_sentinel_rejects_unknown_kind() {
        let err =
            parse_mask_sentinel("zero-migrate:mask:kind=blink_182,classification=pii").unwrap_err();
        assert!(err.message().contains("mask_sentinel_malformed"));
    }

    #[test]
    fn parse_mask_sentinel_rejects_unknown_classification() {
        let err =
            parse_mask_sentinel("zero-migrate:mask:kind=last4,classification=cosmic").unwrap_err();
        assert!(err.message().contains("mask_sentinel_malformed"));
    }

    #[test]
    fn parse_mask_sentinel_rejects_missing_kind() {
        let err = parse_mask_sentinel("zero-migrate:mask:classification=pii").unwrap_err();
        assert!(err.message().contains("mask_sentinel_malformed"));
    }

    #[test]
    fn parse_mask_sentinel_rejects_trailing_junk() {
        let err =
            parse_mask_sentinel("zero-migrate:mask:kind=last4,classification=pii,extra=bogus")
                .unwrap_err();
        assert!(err.message().contains("mask_sentinel_malformed"));
    }

    // ----- encryption sentinel codec -----

    fn enc(wraps: WrappedType) -> EncryptionMeta {
        EncryptionMeta {
            wraps,
        }
    }

    #[test]
    fn build_encryption_sentinel_canonical_shape() {
        let s = build_encryption_sentinel(&enc(WrappedType::String));
        assert_eq!(s, "zero-migrate:enc:string");
        let d = build_encryption_sentinel(&enc(WrappedType::Number));
        assert_eq!(d, "zero-migrate:enc:number");
    }

    #[test]
    fn encryption_sentinel_round_trips_every_combination() {
        {
            for wraps in [WrappedType::String, WrappedType::Number, WrappedType::Bytes] {
                    let meta = enc(wraps);
                    let s = build_encryption_sentinel(&meta);
                    assert_eq!(parse_encryption_sentinel(&s).unwrap(), meta);
            }
        }
    }

    #[test]
    fn parse_encryption_sentinel_accepts_inline_comment_form() {
        // The SQLite-surviving inline form parses to the same meta as the bare
        // PG comment body - both introspectors feed one parser.
        let bare = parse_encryption_sentinel("zero-migrate:enc:string").unwrap();
        let inline = parse_encryption_sentinel("/* zero-migrate:enc:string */").unwrap();
        assert_eq!(bare, inline);
        assert_eq!(bare, enc(WrappedType::String));
    }

    #[test]
    fn parse_encryption_sentinel_rejects_missing_prefix() {
        assert!(parse_encryption_sentinel("default:string")
            .unwrap_err()
            .message()
            .contains("enc_sentinel_malformed"));
    }

    #[test]
    fn parse_encryption_sentinel_rejects_wrong_arity() {
        assert!(
            parse_encryption_sentinel("zero-migrate:enc:default")
                .unwrap_err()
                .message()
                .contains("enc_sentinel_malformed")
        );
        assert!(
            parse_encryption_sentinel("zero-migrate:enc:string:extra")
                .unwrap_err()
                .message()
                .contains("enc_sentinel_malformed")
        );
    }

    #[test]
    fn parse_encryption_sentinel_rejects_unknown_wraps() {
        assert!(
            parse_encryption_sentinel("zero-migrate:enc:json")
                .unwrap_err()
                .message()
                .contains("enc_sentinel_malformed")
        );
        assert!(
            parse_encryption_sentinel("zero-migrate:enc:default:blob")
                .unwrap_err()
                .message()
                .contains("enc_sentinel_malformed")
        );
    }

    #[test]
    fn parse_encryption_sentinel_rejects_extra_metadata() {
        assert!(
            parse_encryption_sentinel("zero-migrate:enc::string")
                .unwrap_err()
                .message()
                .contains("enc_sentinel_malformed")
        );
    }
}
