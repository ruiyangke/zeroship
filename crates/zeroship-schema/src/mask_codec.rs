//! The mask-sentinel CODEC — the contract between the schema layer
//! (which *writes* the sentinel into DDL) and the data plane (which
//! *reads* it back at runtime to drive the mask read-pass).
//!
//! Relocated out of `zeroship_plugin_db::crud::mask_backfill` per the
//! schema-authority split (`docs/proposals/2026-06-18-schema-authority-drizzle-model-design.md`
//! §5): the *codec* (build/parse the `__zsmask:` sentinel string) is a
//! schema-shape concern and lives here; the backfill *runner*
//! (`run_mask_backfill` / `run_mask_rewrite`, which execute UPDATE
//! backfills) stays in plugin-db's data plane.
//!
//! The `(MaskKind, Classification)` types this codec round-trips live in
//! [`crate::diff`] (the schema metadata types). plugin-db re-exports both
//! the codec and the types from their original module paths so existing
//! `crate::crud::mask_backfill::{build,parse}_mask_sentinel` references
//! keep resolving unchanged.

use crate::diff::{Classification, MaskKind};
use crate::error::MaskSentinelError;

/// **P5.5 PR 6** — build the canonical mask-sentinel string for a
/// `(kind, classification)` pair.
///
/// Stored on PG via `COMMENT ON COLUMN "<schema>"."<table>"."<sibling>"
/// IS '<sentinel>'` and on SQLite as a `/* <sentinel> */` inline
/// comment after the sibling column DDL. The parser side
/// ([`parse_mask_sentinel`]) accepts the exact same string.
///
/// Format: `__zsmask:kind=<kind>,classification=<class>`.
#[must_use]
pub fn build_mask_sentinel(kind: MaskKind, classification: Classification) -> String {
    format!(
        "__zsmask:kind={},classification={}",
        kind.as_sql(),
        classification.as_sql(),
    )
}

/// **P5.5 PR 6** — parse a `__zsmask:kind=…,classification=…`
/// sentinel string back into a `(MaskKind, Classification)` pair.
///
/// Returns `Err(MaskSentinelError)` whose `.message` carries the
/// `mask_sentinel_malformed` code-discriminator for any parse failure —
/// unknown kind, unknown classification, missing field, extra trailing
/// junk. plugin-db's `From<MaskSentinelError> for DbError` lifts it back
/// into `DbError::Internal { message }` verbatim, so the typed error the
/// introspector surfaces (with the column name appended) is unchanged.
pub fn parse_mask_sentinel(s: &str) -> Result<(MaskKind, Classification), MaskSentinelError> {
    let body = s.strip_prefix("__zsmask:").ok_or_else(|| {
        MaskSentinelError::new(format!(
            "mask_sentinel_malformed: expected '__zsmask:' prefix, got {s:?}"
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
        MaskSentinelError::new(format!(
            "mask_sentinel_malformed: missing kind= in {s:?}"
        ))
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
        assert_eq!(s, "__zsmask:kind=last4,classification=spi");
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
        let err = parse_mask_sentinel("__zsmask:kind=blink_182,classification=pii").unwrap_err();
        assert!(err.message().contains("mask_sentinel_malformed"));
    }

    #[test]
    fn parse_mask_sentinel_rejects_unknown_classification() {
        let err = parse_mask_sentinel("__zsmask:kind=last4,classification=cosmic").unwrap_err();
        assert!(err.message().contains("mask_sentinel_malformed"));
    }

    #[test]
    fn parse_mask_sentinel_rejects_missing_kind() {
        let err = parse_mask_sentinel("__zsmask:classification=pii").unwrap_err();
        assert!(err.message().contains("mask_sentinel_malformed"));
    }

    #[test]
    fn parse_mask_sentinel_rejects_trailing_junk() {
        let err =
            parse_mask_sentinel("__zsmask:kind=last4,classification=pii,extra=bogus").unwrap_err();
        assert!(err.message().contains("mask_sentinel_malformed"));
    }
}
