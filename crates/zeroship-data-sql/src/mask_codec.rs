//! Catalog sentinel codec consumed by the ORM's masking and encryption passes.
//!
//! The migration engine writes these sentinels into table definitions. Runtime
//! introspection reads them to establish storage protection. Cross-codec tests
//! compare the runtime reader with the migration writer in both directions;
//! migration crates are test dependencies only.

use crate::catalog::{Classification, EncryptionMeta, MaskKind, WrappedType};
use crate::descriptors::EncryptionMode;
use crate::schema_error::MaskSentinelError;

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

/// The CROSS-CODEC parity suite: the migration engine WRITES the sentinel, this
/// crate READS it, and nothing in the type system relates the two.
///
/// # Why this module exists
///
/// `zeroship-migrate-backend::mask_codec` and this module are two independent
/// implementations of one wire. Their prefixes diverged before 2026-09-04, and
/// the failure was silent and fail-open:
/// `zeroship_data_orm::crud::protection_floor` refuses a write whose
/// descriptor drops a protection the live catalog records, and on every
/// migration-engine-built table it introspected it matched no sentinel,
/// concluded nothing was protected, and permitted the downgrade. Both files then
/// carried a doc block claiming the re-divergence guard lived in
/// `zeroship-plugin-db`'s `mask_flip.rs`. That test does bind them - it renders
/// DDL through `zeroship_migrate::schema::query`, executes it, and compares the
/// stored comment against this crate's builder - but it needs a live
/// `PostgreSQL` and `--features test-helpers`, so it runs in no ordinary
/// `cargo test`, it compares two BUILDERS rather than crossing a builder into
/// the other parser, and it covers one `(kind, classification)` pair, one
/// `EncryptionMeta` and neither `SQLite` form.
///
/// This suite is the always-run half. It reaches the engine codec through the
/// existing test-only `zeroship-migrate-core` dev-dependency (which re-exports
/// `zeroship_migrate_backend::mask_codec` at `schema::mask_codec`), so it adds
/// no production dependency edge; the crates stay separated by the migration
/// engine boundary exactly as `crate::compile`'s identifier parity suite leaves
/// them.
///
/// # What it pins, and what it deliberately does not
///
/// Every case builds with ONE crate's emitter and parses with the OTHER's, in
/// both directions and for both sentinel families, over the full
/// `MaskKind x Classification` and `EncryptionMode x WrappedType` corpora. The
/// enum bridges below are exhaustive `match`es rather than
/// `from_sql(as_sql(..))` round-trips on purpose: laundering a variant through
/// its own crate's wire string would make a divergence in `as_sql` invisible,
/// and an exhaustive match additionally fails to COMPILE when a variant is added
/// to one side only.
///
/// The last two tests cross the DISPATCH side, not just the parser. Both backend
/// introspectors decide a comment is a sentinel before handing it to the codec
/// (`zeroship_data_orm::backend::postgres::pg_introspect` on the `pg_description` body,
/// `zeroship_data_orm::backend::sqlite` on the `/* ... */` marker in `sqlite_master.sql`), and
/// a dispatch that recognises a different string than the parser accepts drops
/// the column in silence instead of erroring - the fail-open shape again. Those
/// tests therefore compose the marker the way production composes it and assert
/// it finds what the other crate wrote.
#[cfg(test)]
mod cross_codec_parity {
    use super::*;

    use zeroship_migrate_core::schema::descriptors::EncryptionMode as EngineEncryptionMode;
    use zeroship_migrate_core::schema::diff::{
        Classification as EngineClassification, EncryptionMeta as EngineEncryptionMeta,
        MaskKind as EngineMaskKind, WrappedType as EngineWrappedType,
    };
    use zeroship_migrate_core::schema::mask_codec as engine;

    // ----- the enum bridges, exhaustive in both directions -----

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

    fn mode_to_engine(m: EncryptionMode) -> EngineEncryptionMode {
        match m {
            EncryptionMode::Randomised => EngineEncryptionMode::Randomised,
            EncryptionMode::Deterministic => EngineEncryptionMode::Deterministic,
        }
    }

    fn mode_from_engine(m: EngineEncryptionMode) -> EncryptionMode {
        match m {
            EngineEncryptionMode::Randomised => EncryptionMode::Randomised,
            EngineEncryptionMode::Deterministic => EncryptionMode::Deterministic,
        }
    }

    fn wraps_to_engine(w: WrappedType) -> EngineWrappedType {
        match w {
            WrappedType::String => EngineWrappedType::String,
            WrappedType::Number => EngineWrappedType::Number,
            WrappedType::Bytes => EngineWrappedType::Bytes,
        }
    }

    fn wraps_from_engine(w: EngineWrappedType) -> WrappedType {
        match w {
            EngineWrappedType::String => WrappedType::String,
            EngineWrappedType::Number => WrappedType::Number,
            EngineWrappedType::Bytes => WrappedType::Bytes,
        }
    }

    fn meta_to_engine(m: &EncryptionMeta) -> EngineEncryptionMeta {
        EngineEncryptionMeta {
            mode: mode_to_engine(m.mode),
            key_id: m.key_id.clone(),
            wraps: wraps_to_engine(m.wraps),
        }
    }

    fn meta_from_engine(m: &EngineEncryptionMeta) -> EncryptionMeta {
        EncryptionMeta {
            mode: mode_from_engine(m.mode),
            key_id: m.key_id.clone(),
            wraps: wraps_from_engine(m.wraps),
        }
    }

    // ----- the corpora -----

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

    fn encryption_corpus() -> Vec<EncryptionMeta> {
        let mut out = Vec::new();
        for mode in [EncryptionMode::Randomised, EncryptionMode::Deterministic] {
            for wraps in [WrappedType::String, WrappedType::Number, WrappedType::Bytes] {
                for key in ["default", "k7", "tenant_42_root"] {
                    out.push(EncryptionMeta {
                        mode,
                        key_id: key.to_string(),
                        wraps,
                    });
                }
            }
        }
        out
    }

    // ----- mask sentinel, both directions -----

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

    // ----- encryption sentinel, both directions, both wire forms -----

    /// The bare `pg_description` body and the `SQLite` `/* ... */` inline form are
    /// the same wire with one wrapper; the parser strips the wrapper, so both
    /// backends feed one codec and both must cross.
    #[test]
    fn every_engine_built_encryption_sentinel_parses_with_the_data_plane_codec() {
        for meta in encryption_corpus() {
            let written = engine::build_encryption_sentinel(&meta_to_engine(&meta));
            for form in [written.clone(), format!("/* {written} */")] {
                let read = parse_encryption_sentinel(&form).unwrap_or_else(|e| {
                    panic!(
                        "the data plane cannot read the encryption sentinel the migration \
                         engine writes for {meta:?}: engine wrote {form:?}, this crate \
                         refused it with {}",
                        e.message()
                    )
                });
                assert_eq!(
                    read, meta,
                    "cross-codec encryption round-trip changed the value: engine wrote {form:?}",
                );
            }
        }
    }

    #[test]
    fn every_data_plane_built_encryption_sentinel_parses_with_the_engine_codec() {
        for meta in encryption_corpus() {
            let written = build_encryption_sentinel(&meta);
            for form in [written.clone(), format!("/* {written} */")] {
                let read = engine::parse_encryption_sentinel(&form).unwrap_or_else(|e| {
                    panic!(
                        "the migration engine cannot read the encryption sentinel the data \
                         plane builds for {meta:?}: data plane wrote {form:?}, the engine \
                         refused it with {}",
                        e.message()
                    )
                });
                assert_eq!(
                    meta_from_engine(&read),
                    meta,
                    "cross-codec encryption round-trip changed the value: \
                     data plane wrote {form:?}",
                );
            }
        }
    }

    // ----- the dispatch side -----

    /// `zeroship_data_orm::backend::postgres::pg_introspect` decides which sentinel family a
    /// `pg_description` body belongs to with `starts_with(<PREFIX>)` before it
    /// calls any parser. A prefix that drifts does not produce a parse error
    /// there: it produces NO MATCH, the column reads back unprotected, and the
    /// protection floor is handed an empty catalog.
    #[test]
    fn the_postgres_dispatch_prefixes_match_what_the_engine_writes() {
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

        let enc = engine::build_encryption_sentinel(&EngineEncryptionMeta {
            mode: EngineEncryptionMode::Randomised,
            key_id: "default".to_string(),
            wraps: EngineWrappedType::String,
        });
        assert!(
            enc.starts_with(ENC_SENTINEL_PREFIX),
            "pg_introspect dispatches on {ENC_SENTINEL_PREFIX:?}; the engine wrote {enc:?}, \
             which it would classify as an ordinary comment and drop in silence",
        );
        assert!(
            !enc.starts_with(MASK_SENTINEL_PREFIX),
            "the two sentinel families must stay distinguishable: {enc:?}",
        );
    }

    /// `zeroship_data_orm::backend::sqlite` walks `sqlite_master.sql` for a `/* <PREFIX>`
    /// marker composed from these constants, then hands the body to the codec.
    /// This reproduces that walk verbatim over DDL the ENGINE emitted, so the
    /// marker, the wrapper stripping and the parse are pinned as one chain
    /// rather than three independent facts.
    ///
    /// Note the asymmetry it encodes: `parse_encryption_sentinel` strips the
    /// `/* */` wrapper itself, `parse_mask_sentinel` does not - the mask walker
    /// strips it before calling in. Both are reproduced as production does them.
    #[test]
    fn the_sqlite_inline_walk_reads_what_the_engine_emitted() {
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

        let enc = engine::build_encryption_sentinel(&EngineEncryptionMeta {
            mode: EngineEncryptionMode::Deterministic,
            key_id: "payroll_v2".to_string(),
            wraps: EngineWrappedType::Number,
        });
        let ddl = format!("CREATE TABLE t (\n  \"salary\" BYTEA /* {enc} */\n)");

        let marker = format!("/* {ENC_SENTINEL_PREFIX}");
        let found = ddl.find(&marker).unwrap_or_else(|| {
            panic!(
                "the SQLite encryption walker composes its marker as {marker:?} and found \
                 nothing in engine-emitted DDL {ddl:?}; a column carrying ciphertext reads \
                 back as plaintext",
            )
        });
        let body_start = found + marker.len();
        let end = ddl[body_start..].find("*/").expect("terminated comment");
        let body = ddl[body_start..body_start + end].trim();
        assert_eq!(
            body.split(':').collect::<Vec<_>>(),
            vec!["deterministic", "payroll_v2", "number"],
            "the SQLite walker hand-parses this body rather than calling the codec; \
             its field order must match what the engine emitted",
        );
    }
}
