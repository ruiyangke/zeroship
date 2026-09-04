//! The mask-sentinel CODEC — the contract between the schema layer
//! (which *writes* the sentinel into DDL) and the data plane (which
//! *reads* it back at runtime to drive the mask read-pass).
//!
//! Relocated out of `zeroship_plugin_db::crud::mask_backfill` per the
//! schema-authority split (`docs/archive/proposals/2026-06-18-schema-authority-drizzle-model-design.md`
//! §5): the *codec* (build/parse the `zero-migrate:mask:` sentinel string) is a
//! schema-shape concern and lives here; the backfill *runner*
//! (`run_mask_backfill` / `run_mask_rewrite`, which execute UPDATE
//! backfills) stays in plugin-db's data plane.
//!
//! The `(MaskKind, Classification)` types this codec round-trips live in
//! [`crate::diff`] (the schema metadata types). plugin-db's two consumers -
//! `backend::pg_introspect` and `backend::sqlite` - call this module directly.
//!
//! They were once reached through a delegating wrapper at
//! `zeroship_plugin_db::crud::mask_backfill::parse_mask_sentinel`; that module
//! was deleted on 2026-09-02 with zero callers, and neither consumer went
//! through it even then.
//!
//! Written `crate::...` until 2026-08-08, which was wrong in two ways once
//! this module was extracted: `crate` is zeroship-schema, a leaf crate that
//! does not depend on plugin-db, and `build_mask_sentinel` is not in
//! plugin-db at all - it is defined below, in this file.
//!
//! # Why the prefixes are the migration engine's and not this crate's
//!
//! This codec is a READER of a wire the MIGRATION ENGINE writes. Every creator
//! table on the platform is created by `zeroship-migrate-server` (PostgreSQL) or
//! the dev-tier SQLite apply host, both of which render their DDL through
//! `zeroship_migrate_backend::mask_codec` - so `zero-migrate:mask:` /
//! `zero-migrate:enc:` is what is physically on disk. This crate's own DDL
//! emitter (`crate::query::build_create_table_with_fks`) has no `src` call site
//! anywhere in the workspace; it is reached only from tests.
//!
//! It spelled the two sentinels with a `zs`-branded prefix of its own until
//! 2026-09-04, and the divergence was not cosmetic: the protection floor
//! (`zeroship_data_engine::crud::protection_floor`) refuses a write whose
//! descriptor dropped a protection the live catalog records, and on every
//! migration-engine-built table it introspected, found no sentinel it
//! recognised, concluded nothing was protected, and PERMITTED the downgrade it
//! exists to refuse. Its tests passed because their fixtures built tables with
//! the emitter above - the suite and production disagreed about how a masked
//! column is spelled on disk, and the suite was the unrepresentative one.
//!
//! The two prefixes are now one wire with one spelling. The engine's parse side
//! keeps its own copy of this codec (`zeroship-migrate-backend`); the crates
//! cannot share one because their `MaskKind`/`Classification`/`EncryptionMeta`
//! types are separate. What binds them is behavioural, not textual:
//! `zeroship-plugin-db`'s `mask_flip.rs` builds a table with the ENGINE's
//! emitter and reads it back with THIS one, so a spelling that drifts again
//! fails there rather than in production.

use crate::descriptors::EncryptionMode;
use crate::diff::{Classification, EncryptionMeta, MaskKind, WrappedType};
use crate::error::MaskSentinelError;

/// The encryption-sentinel prefix, byte-identical to
/// `zeroship_migrate_backend::mask_codec::ENC_SENTINEL_PREFIX`.
///
/// A constant rather than a literal at each site because the two backend
/// introspectors dispatch on it BEFORE calling the parser
/// (`zeroship_data_postgres::pg_introspect` on the `pg_description` body,
/// `zeroship_data_sqlite` on the inline `sqlite_master.sql` comment), and a
/// dispatch that recognises a different string than the parser accepts is a
/// silent drop, not a parse error.
pub const ENC_SENTINEL_PREFIX: &str = "zero-migrate:enc:";

/// The mask-sentinel prefix, byte-identical to
/// `zeroship_migrate_backend::mask_codec::MASK_SENTINEL_PREFIX`. See
/// [`ENC_SENTINEL_PREFIX`] for why this is a constant.
pub const MASK_SENTINEL_PREFIX: &str = "zero-migrate:mask:";

/// The canonical wire string for an [`EncryptionMode`] in a
/// `zero-migrate:enc:` sentinel. `randomised` is the canonical spelling (the US
/// `randomized` is normalised to it at emit time so the parser only needs the
/// one form). Kept here next to the codec rather than on `EncryptionMode` so the
/// descriptor enum stays a pure data type with no wire-format opinions.
#[must_use]
fn encryption_mode_as_sql(mode: EncryptionMode) -> &'static str {
    match mode {
        EncryptionMode::Randomised => "randomised",
        EncryptionMode::Deterministic => "deterministic",
    }
}

/// Parse a `zero-migrate:enc:` mode token. Accepts the canonical
/// `randomised` plus the legacy US `randomized` spelling (the SDK historically
/// emitted it; the emit path normalises to `randomised`, but a hand-written
/// migration may carry either). `None` for any other token.
#[must_use]
fn encryption_mode_from_sql(s: &str) -> Option<EncryptionMode> {
    match s {
        "randomised" | "randomized" => Some(EncryptionMode::Randomised),
        "deterministic" => Some(EncryptionMode::Deterministic),
        _ => None,
    }
}

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
/// [`EncryptionMeta`]: `zero-migrate:enc:<mode>:<keyId>:<wraps>`.
///
/// This is the COMMENT-body form (no surrounding `/* */`): on PG it is stored
/// via `COMMENT ON COLUMN "<schema>"."<table>"."<col>" IS '<body>'` on the
/// ENCRYPTED column itself, so PG (which discards the inline `/* zero-migrate:enc */`
/// comment at parse time) can still recover the metadata from `pg_description`.
/// On SQLite the inline form (`query::encryption_sentinel_for_field`, which
/// wraps this same `zero-migrate:enc:…` body in `/* */`) survives in `sqlite_master.sql`.
///
/// The two emitters share the SAME `zero-migrate:enc:<mode>:<keyId>:<wraps>` body, so the
/// metadata generated for either dialect is byte-identical - the
/// verify-bricking guard. The parser side is
/// [`parse_encryption_sentinel`].
#[must_use]
pub fn build_encryption_sentinel(meta: &EncryptionMeta) -> String {
    format!(
        "{ENC_SENTINEL_PREFIX}{}:{}:{}",
        encryption_mode_as_sql(meta.mode),
        meta.key_id,
        wrapped_type_as_sql(meta.wraps),
    )
}

/// Parse a `zero-migrate:enc:<mode>:<keyId>:<wraps>` sentinel body back
/// into an [`EncryptionMeta`].
///
/// Accepts either the bare comment body (`zero-migrate:enc:randomised:default:string`, the
/// PG `pg_description` form) or the inline-comment form wrapping it
/// (`/* zero-migrate:enc:randomised:default:string */`, the SQLite `sqlite_master.sql`
/// form) — the leading/trailing `/* */` and whitespace are stripped first, so
/// both introspectors feed the SAME parser.
///
/// Returns `Err(MaskSentinelError)` (the shared sentinel-error type) carrying an
/// `enc_sentinel_malformed` discriminator for any parse failure — wrong prefix,
/// wrong arity, unknown mode/wraps, empty keyId — so a hand-edited or
/// future-version sentinel produces a typed error rather than silently routing
/// through a default codec (the fail-closed contract).
pub fn parse_encryption_sentinel(s: &str) -> Result<EncryptionMeta, MaskSentinelError> {
    // Strip an optional inline `/* … */` wrapper (the SQLite form) so both
    // the PG comment body and the SQLite inline comment parse identically.
    let trimmed = s.trim();
    let body = trimmed
        .strip_prefix("/*")
        .and_then(|rest| rest.strip_suffix("*/"))
        .map_or(trimmed, str::trim);

    let rest = body.strip_prefix(ENC_SENTINEL_PREFIX).ok_or_else(|| {
        MaskSentinelError::new(format!(
            "enc_sentinel_malformed: expected '{ENC_SENTINEL_PREFIX}' prefix, got {s:?}"
        ))
    })?;
    let parts: Vec<&str> = rest.split(':').collect();
    if parts.len() != 3 {
        return Err(MaskSentinelError::new(format!(
            "enc_sentinel_malformed: expected {ENC_SENTINEL_PREFIX}<mode>:<keyId>:<wraps>, \
             got {s:?}"
        )));
    }
    let mode = encryption_mode_from_sql(parts[0]).ok_or_else(|| {
        MaskSentinelError::new(format!(
            "enc_sentinel_malformed: unknown mode {:?} in {s:?}",
            parts[0]
        ))
    })?;
    let key_id = parts[1];
    if key_id.is_empty() {
        return Err(MaskSentinelError::new(format!(
            "enc_sentinel_malformed: empty keyId in {s:?}"
        )));
    }
    let wraps = wrapped_type_from_sql(parts[2]).ok_or_else(|| {
        MaskSentinelError::new(format!(
            "enc_sentinel_malformed: unknown wraps {:?} in {s:?}",
            parts[2]
        ))
    })?;
    Ok(EncryptionMeta {
        mode,
        key_id: key_id.to_string(),
        wraps,
    })
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

/// Parse a `zero-migrate:mask:kind=…,classification=…`
/// sentinel string back into a `(MaskKind, Classification)` pair.
///
/// Returns `Err(MaskSentinelError)` whose `.message` carries the
/// `mask_sentinel_malformed` code-discriminator for any parse failure —
/// unknown kind, unknown classification, missing field, extra trailing
/// junk. plugin-db's `From<MaskSentinelError> for DbError` lifts it back
/// into `DbError::Internal { message }` verbatim, so the typed error the
/// introspector surfaces (with the column name appended) is unchanged.
pub fn parse_mask_sentinel(s: &str) -> Result<(MaskKind, Classification), MaskSentinelError> {
    let body = s.strip_prefix(MASK_SENTINEL_PREFIX).ok_or_else(|| {
        MaskSentinelError::new(format!(
            "mask_sentinel_malformed: expected '{MASK_SENTINEL_PREFIX}' prefix, got {s:?}"
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

    fn enc(mode: EncryptionMode, key: &str, wraps: WrappedType) -> EncryptionMeta {
        EncryptionMeta {
            mode,
            key_id: key.to_string(),
            wraps,
        }
    }

    #[test]
    fn build_encryption_sentinel_canonical_shape() {
        let s = build_encryption_sentinel(&enc(
            EncryptionMode::Randomised,
            "default",
            WrappedType::String,
        ));
        assert_eq!(s, "zero-migrate:enc:randomised:default:string");
        let d = build_encryption_sentinel(&enc(
            EncryptionMode::Deterministic,
            "k7",
            WrappedType::Number,
        ));
        assert_eq!(d, "zero-migrate:enc:deterministic:k7:number");
    }

    #[test]
    fn encryption_sentinel_round_trips_every_combination() {
        for mode in [EncryptionMode::Randomised, EncryptionMode::Deterministic] {
            for wraps in [WrappedType::String, WrappedType::Number, WrappedType::Bytes] {
                for key in ["default", "k7", "tenant_42_root"] {
                    let meta = enc(mode, key, wraps);
                    let s = build_encryption_sentinel(&meta);
                    assert_eq!(parse_encryption_sentinel(&s).unwrap(), meta);
                }
            }
        }
    }

    #[test]
    fn parse_encryption_sentinel_accepts_inline_comment_form() {
        // The SQLite-surviving inline form parses to the same meta as the bare
        // PG comment body — both introspectors feed one parser.
        let bare = parse_encryption_sentinel("zero-migrate:enc:randomised:default:string").unwrap();
        let inline =
            parse_encryption_sentinel("/* zero-migrate:enc:randomised:default:string */").unwrap();
        assert_eq!(bare, inline);
        assert_eq!(
            bare,
            enc(EncryptionMode::Randomised, "default", WrappedType::String)
        );
    }

    #[test]
    fn parse_encryption_sentinel_normalises_us_spelling() {
        let m = parse_encryption_sentinel("zero-migrate:enc:randomized:default:string").unwrap();
        assert_eq!(m.mode, EncryptionMode::Randomised);
    }

    #[test]
    fn parse_encryption_sentinel_rejects_missing_prefix() {
        assert!(
            parse_encryption_sentinel("randomised:default:string")
                .unwrap_err()
                .message()
                .contains("enc_sentinel_malformed")
        );
    }

    #[test]
    fn parse_encryption_sentinel_rejects_wrong_arity() {
        assert!(
            parse_encryption_sentinel("zero-migrate:enc:randomised:default")
                .unwrap_err()
                .message()
                .contains("enc_sentinel_malformed")
        );
        assert!(
            parse_encryption_sentinel("zero-migrate:enc:randomised:default:string:extra")
                .unwrap_err()
                .message()
                .contains("enc_sentinel_malformed")
        );
    }

    #[test]
    fn parse_encryption_sentinel_rejects_unknown_mode_and_wraps() {
        assert!(
            parse_encryption_sentinel("zero-migrate:enc:rot13:default:string")
                .unwrap_err()
                .message()
                .contains("enc_sentinel_malformed")
        );
        assert!(
            parse_encryption_sentinel("zero-migrate:enc:randomised:default:blob")
                .unwrap_err()
                .message()
                .contains("enc_sentinel_malformed")
        );
    }

    #[test]
    fn parse_encryption_sentinel_rejects_empty_key_id() {
        assert!(
            parse_encryption_sentinel("zero-migrate:enc:randomised::string")
                .unwrap_err()
                .message()
                .contains("enc_sentinel_malformed")
        );
    }
}
