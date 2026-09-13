//! Catalog protection markers consumed by ORM introspection.
//!
//! The migration engine writes the catalog text. Runtime introspection decodes
//! mask policy and treats encryption as a boolean protection fact.

use crate::sql::catalog::{Classification, MaskKind};
use crate::sql::schema_error::MaskSentinelError;

/// The encryption-sentinel prefix, byte-identical to
/// `zeroship_migrate_backend::mask_codec::ENC_SENTINEL_PREFIX`.
///
/// A constant rather than a literal at each site because the two backend
/// introspectors dispatch on it BEFORE calling the parser
/// (`zeroship_data_orm::backend::postgres::pg_introspect` on the `pg_description` body,
/// `zeroship_data_orm::backend::sqlite` on the inline `sqlite_master.sql` comment), and a
/// dispatch that recognises a different string than the parser accepts is a
/// silent drop, not a parse error.
pub const ENC_SENTINEL_PREFIX: &str = "zero-migrate:enc:";

/// The mask-sentinel prefix, byte-identical to
/// `zeroship_migrate_backend::mask_codec::MASK_SENTINEL_PREFIX`. See
/// [`ENC_SENTINEL_PREFIX`] for why this is a constant.
pub const MASK_SENTINEL_PREFIX: &str = "zero-migrate:mask:";

/// Whether catalog text carries the encryption marker.
///
/// The migration engine owns any detail after the prefix. Runtime type and
/// storage behavior come from the installed descriptor.
#[must_use]
pub fn is_encryption_sentinel(s: &str) -> bool {
    let trimmed = s.trim();
    let body = trimmed
        .strip_prefix("/*")
        .and_then(|rest| rest.strip_suffix("*/"))
        .map_or(trimmed, str::trim);
    body.starts_with(ENC_SENTINEL_PREFIX)
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

/// Parse a mask sentinel. Malformed or unknown fields return a
/// `mask_sentinel_malformed` error for the caller to contextualize.
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

    #[test]
    fn encryption_sentinel_is_only_a_protection_marker() {
        assert!(is_encryption_sentinel("zero-migrate:enc:string"));
        assert!(is_encryption_sentinel(
            "/* zero-migrate:enc:opaque-catalog-detail */"
        ));
        assert!(!is_encryption_sentinel("zero-migrate:mask:kind=full"));
    }
}

/// Cross-check the mask sentinel against the migration engine that writes it.
#[cfg(test)]
mod cross_codec_parity {
    use super::*;

    use zeroship_migrate_core::schema::diff::{
        Classification as EngineClassification, MaskKind as EngineMaskKind,
    };
    use zeroship_migrate_core::schema::mask_codec as engine;

    fn mask_kind_to_engine(k: MaskKind) -> EngineMaskKind {
        match k {
            MaskKind::Full => EngineMaskKind::Full,
            MaskKind::Last4 => EngineMaskKind::Last4,
            MaskKind::First4 => EngineMaskKind::First4,
            MaskKind::Email => EngineMaskKind::Email,
            MaskKind::Name => EngineMaskKind::Name,
            MaskKind::DateYear => EngineMaskKind::DateYear,
            MaskKind::DateDecade => EngineMaskKind::DateDecade,
            MaskKind::None => EngineMaskKind::None,
        }
    }

    fn mask_kind_from_engine(k: EngineMaskKind) -> MaskKind {
        match k {
            EngineMaskKind::Full => MaskKind::Full,
            EngineMaskKind::Last4 => MaskKind::Last4,
            EngineMaskKind::First4 => MaskKind::First4,
            EngineMaskKind::Email => MaskKind::Email,
            EngineMaskKind::Name => MaskKind::Name,
            EngineMaskKind::DateYear => MaskKind::DateYear,
            EngineMaskKind::DateDecade => MaskKind::DateDecade,
            EngineMaskKind::None => MaskKind::None,
        }
    }

    fn classification_to_engine(c: Classification) -> EngineClassification {
        match c {
            Classification::Public => EngineClassification::Public,
            Classification::Pii => EngineClassification::Pii,
            Classification::Spi => EngineClassification::Spi,
            Classification::Phi => EngineClassification::Phi,
            Classification::Pci => EngineClassification::Pci,
            Classification::Internal => EngineClassification::Internal,
        }
    }

    fn classification_from_engine(c: EngineClassification) -> Classification {
        match c {
            EngineClassification::Public => Classification::Public,
            EngineClassification::Pii => Classification::Pii,
            EngineClassification::Spi => Classification::Spi,
            EngineClassification::Phi => Classification::Phi,
            EngineClassification::Pci => Classification::Pci,
            EngineClassification::Internal => Classification::Internal,
        }
    }

    const KINDS: [MaskKind; 8] = [
        MaskKind::Full,
        MaskKind::Last4,
        MaskKind::First4,
        MaskKind::Email,
        MaskKind::Name,
        MaskKind::DateYear,
        MaskKind::DateDecade,
        MaskKind::None,
    ];

    const CLASSES: [Classification; 6] = [
        Classification::Public,
        Classification::Pii,
        Classification::Spi,
        Classification::Phi,
        Classification::Pci,
        Classification::Internal,
    ];

    /// The engine writes every creator table's mask sentinel; this crate reads
    /// it back out of `pg_description`. A spelling only the writer knows is a
    /// protection record the protection floor cannot see.
    #[test]
    fn every_engine_built_mask_sentinel_parses_with_the_data_plane_codec() {
        for kind in KINDS {
            for class in CLASSES {
                let written = engine::build_mask_sentinel(
                    mask_kind_to_engine(kind),
                    classification_to_engine(class),
                );
                let (read_kind, read_class) = parse_mask_sentinel(&written).unwrap_or_else(|e| {
                    panic!(
                        "the data plane cannot read the mask sentinel the migration engine \
                         writes for ({kind:?}, {class:?}): engine wrote {written:?}, \
                         this crate refused it with {}",
                        e.message()
                    )
                });
                assert_eq!(
                    (read_kind, read_class),
                    (kind, class),
                    "cross-codec mask round-trip changed the value: engine wrote {written:?}",
                );
            }
        }
    }

    /// The reverse direction. This crate's emitter has no `src` call site today,
    /// but the diff classifier compares against what it builds, so a one-way
    /// agreement is not enough.
    #[test]
    fn every_data_plane_built_mask_sentinel_parses_with_the_engine_codec() {
        for kind in KINDS {
            for class in CLASSES {
                let written = build_mask_sentinel(kind, class);
                let (read_kind, read_class) =
                    engine::parse_mask_sentinel(&written).unwrap_or_else(|e| {
                        panic!(
                            "the migration engine cannot read the mask sentinel the data plane \
                             builds for ({kind:?}, {class:?}): data plane wrote {written:?}, \
                             the engine refused it with {}",
                            e.message()
                        )
                    });
                assert_eq!(
                    (
                        mask_kind_from_engine(read_kind),
                        classification_from_engine(read_class)
                    ),
                    (kind, class),
                    "cross-codec mask round-trip changed the value: data plane wrote {written:?}",
                );
            }
        }
    }

    #[test]
    fn the_runtime_mask_prefix_matches_what_the_engine_writes() {
        let mask = engine::build_mask_sentinel(EngineMaskKind::Last4, EngineClassification::Pci);
        assert!(
            mask.starts_with(MASK_SENTINEL_PREFIX),
            "pg_introspect dispatches on {MASK_SENTINEL_PREFIX:?}; the engine wrote {mask:?}, \
             which it would classify as an ordinary comment and drop in silence",
        );
        assert!(
            !mask.starts_with(ENC_SENTINEL_PREFIX),
            "the two sentinel families must stay distinguishable: {mask:?}",
        );
    }

    #[test]
    fn the_sqlite_inline_mask_walk_reads_what_the_engine_emitted() {
        let mask = engine::build_mask_sentinel(EngineMaskKind::Email, EngineClassification::Phi);
        let ddl = format!("CREATE TABLE t (\n  \"email\" TEXT /* {mask} */\n)");

        let marker = format!("/* {MASK_SENTINEL_PREFIX}");
        let found = ddl.find(&marker).unwrap_or_else(|| {
            panic!(
                "the SQLite mask walker composes its marker as {marker:?} and found nothing in \
                 engine-emitted DDL {ddl:?}; a column carrying a mask reads back unmasked",
            )
        });
        let body_start = found + "/* ".len();
        let end = ddl[body_start..].find("*/").expect("terminated comment");
        let body = ddl[body_start..body_start + end].trim();
        assert_eq!(
            parse_mask_sentinel(body).unwrap(),
            (MaskKind::Email, Classification::Phi),
        );
    }
}
