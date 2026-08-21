//! The SCHEMA/DDL half of the backend contract: the [`SchemaRenderer`] trait plus
//! the spelling primitives its three implementations call.
//!
//! The sibling of [`crate::renderer`], for the OTHER renderer registry. There are
//! two, because there are two things a vendor is asked to spell: DML/trigger/view
//! text ([`crate::renderer::DmlRenderer`]) and column/DDL text (this). This trait
//! also selects the vendor-owned parser for catalog-stored table DDL when that
//! backend exposes such DDL to the engine.
//!
//! # What is here and what stayed in the engine
//!
//! `zero_migrate::schema::query` is 5973 lines and almost none of it is a vendor
//! spelling. What lives here is exactly the neutral contract and the shared
//! codecs its methods name:
//!
//! * the trait itself, including the required identifier primitives each vendor
//!   supplies;
//! * the neutral [`crate::stored_ddl::StoredDdl`] contract used to reach a
//!   vendor-owned stored-DDL parser;
//! * the sentinel builders PostgreSQL's `column_comment_statements` spells
//!   ([`build_encryption_sentinel_comments`], [`build_mask_sentinel_comments`] and
//!   the three field-level readers under them).
//!
//! Composition and validation stay in the engine. Physical type and identifier
//! spellings, including the vendor's catalog-type canonicalization, live in the
//! backend that owns them. This is the boundary rule stated at length in
//! `zero_migrate::render::backends`.
//!
use crate::snapshot::ColumnSnapshot;
use zero_migrate_ir::dialect::DialectId;

/// Dialect-specific schema/DDL spelling.
///
/// This trait deliberately has no default methods: every registered backend must
/// provide every spelling explicitly. Registration associates the renderer with
/// its open [`DialectId`], so a backend cannot inherit another vendor's answer.
///
/// IT LOST TWO METHODS, AND THE MEASUREMENT IS WHY. `encrypted_column_bind_placeholder`
/// and `wrap_encrypted_param` had ZERO call sites anywhere in the workspace — four
/// and five mentions respectively, every one of them the declaration, an impl, or a
/// doc line. `wrap_encrypted_param`'s SQLite arm was the sole reader of a `pub const
/// SQLITE_ENC_BLOB_PREFIX` whose own doc claimed "the SQLite session strips the
/// prefix and base64-decodes the remainder"; the sentinel string appeared exactly
/// once in the whole repository, in its own definition, so nothing stripped it and
/// nothing ever had. A dead encryption seam that DOCUMENTS a decode step it does not
/// perform is worse than no seam, because the next reader budgets for it. All three
/// are deleted rather than kept warm: 10 methods to 8.
///
/// It then gained TWO required collation spellings. Pinning an engine-selected
/// collation when a column is created and stripping that pin when a retype must
/// preserve the live column's collation are inverse-looking but distinct vendor
/// operations. Keeping both required makes a new backend state each position in
/// its own crate instead of inheriting another engine's answer.
///
/// It also gained THREE required schema-emission operations. Catalog-type
/// canonicalization, CREATE TABLE target qualification, and injected-index syntax
/// all depend on vendor grammar. Keeping their bodies in the registering backend
/// means core never dispatches on a closed vendor enum to answer them.
pub trait SchemaRenderer: std::fmt::Debug + Sync {
    /// Which vendor this is, as the OPEN [`DialectId`] rather than the closed
    /// [`SqlDialect`](zero_migrate_ir::dialect::SqlDialect).
    ///
    /// The same signature change, and for the same reason, as
    /// [`DmlRenderer::dialect`](crate::renderer::DmlRenderer::dialect): a backend
    /// crate cannot construct a variant of an enum it does not own, so a closed
    /// return type left `todo!()` as the only body a fourth backend could write.
    /// `DialectId::new` is `const`, so an outsider declares its identity at item
    /// scope and this method hands it back.
    fn dialect(&self) -> DialectId;

    /// Quote one identifier in this backend's emitted SQL spelling.
    ///
    /// Required, with no shared default: a backend cannot silently inherit another
    /// vendor's quoting rules.
    fn quote_ident(&self, ident: &str) -> String;

    /// The delimiter this vendor's [`quote_ident`](Self::quote_ident) uses.
    ///
    /// The rename rewriter needs to recognize quoted identifier runs without
    /// deriving a vendor spelling in core. Required for the same reason as
    /// [`Self::quote_ident`].
    fn ident_quote_char(&self) -> char;

    /// This vendor's parser for catalog-stored table DDL, or an explicit `None`
    /// when its snapshots do not retain a grammar that the engine may rewrite.
    ///
    /// Required with no default so a new backend states that boundary itself.
    fn stored_ddl(&self) -> Option<&'static dyn crate::stored_ddl::StoredDdl>;

    fn foreign_key_target(&self, app_id: &str, target: &str) -> String;
    fn column_type(&self, c: &ColumnSnapshot, inline_pk: bool) -> String;

    /// Fold a raw catalog/DDL type spelling to this backend's drift-comparison
    /// token.
    fn canonical_type(&self, raw: &str) -> String;

    /// Spell the target of a CREATE TABLE statement. `unqualified` is the
    /// caller's primitive request for the engine's main-database mode; backends
    /// whose table namespace is always qualified state that explicitly by
    /// ignoring it.
    fn create_table_target(&self, app_id: &str, collection: &str, unqualified: bool) -> String;

    /// Spell one already-validated policy-injected index statement.
    fn injected_index_statement(
        &self,
        app_id: &str,
        collection: &str,
        index_name: &str,
        unique: bool,
        columns: &[&str],
        unqualified: bool,
    ) -> String;

    /// Render an author-controlled string in a schema expression position.
    ///
    /// Required because string-escape modes and literal carriers are vendor
    /// grammar, even when two vendors currently share quote doubling.
    fn schema_string_literal(&self, value: &str) -> String;

    /// Render one policy-owned injected column identifier.
    ///
    /// `canonical_bare` is the caller's already-computed answer to the neutral
    /// policy-name question: may this canonical identifier remain bare in the
    /// established constraint-definition spelling? The backend still owns the
    /// emitted spelling and must explicitly choose whether to use that fact.
    fn injected_column_ident(&self, name: &str, canonical_bare: bool) -> String;

    /// Canonicalize one already-tokenized foreign-key action for this backend's
    /// catalog/render comparison form.
    fn canonical_fk_action(&self, action: &'static str) -> &'static str;

    /// Whether this field's string enum is represented by a native type and must
    /// therefore omit the otherwise portable membership CHECK.
    fn suppress_string_enum_check(&self, def: &serde_json::Value) -> bool;

    /// Pin this vendor's explicit collation spelling onto a rendered type when the
    /// type can carry one.
    fn pin_collation(&self, rendered: &str, case_sensitive: Option<bool>) -> String;

    /// Remove only the explicit collation spelling this vendor's renderer pins.
    ///
    /// The borrowed result makes this a primitive spelling operation: a backend
    /// may return either the whole input or a prefix of it without inventing a
    /// shared collation carrier in the contract crate.
    fn strip_collation<'a>(&self, rendered: &'a str) -> &'a str;

    fn json_object_default(&self) -> String;
    fn json_array_default(&self) -> String;
    fn current_timestamp_expr(&self) -> &'static str;
    fn column_comment_statements(
        &self,
        app_id: &str,
        collection: &str,
        schema: &serde_json::Value,
    ) -> Vec<String>;
}

/// True for top-level schema keys that carry
/// metadata rather than a field declaration (e.g. `"_meta"`,
/// `"_indexes"`). These keys are produced by the SDK normaliser
/// or appear in test schemas; they MUST be skipped before the
/// schema-iteration loop reaches `validate_field_name` (otherwise
/// the leading `_` would trip the reserved-prefix rule).
///
/// The list is intentionally narrow — only keys the runtime
/// actually reads. Adding a new metadata key here is a deliberate
/// platform extension, not a creator-driven decision.
pub fn is_schema_metadata_key(key: &str) -> bool {
    matches!(key, "_meta" | "_indexes")
}

/// String-valued enum members from a neutral SDK field definition.
pub fn string_enum_values(def: &serde_json::Value) -> Option<Vec<String>> {
    let values = def.get("enum")?.as_array()?;
    let mut members = Vec::with_capacity(values.len());
    for value in values {
        let s = value.as_str()?;
        members.push(s.to_string());
    }
    if members.is_empty() {
        None
    } else {
        Some(members)
    }
}

/// Render the `COMMENT ON COLUMN … 'zero-migrate:enc:<mode>:<keyId>:<wraps>'`
/// statements for every `t.encrypted(...)` column in `schema` (PG only). The
/// comment BODY is built by the shared codec
/// ([`crate::mask_codec::build_encryption_sentinel`]) so it is byte-identical to
/// what the migration engine emits and what the runtime parser
/// ([`crate::mask_codec::parse_encryption_sentinel`], via `read_live_schema`)
/// expects. Returns the empty vector when no column is encrypted.
#[must_use]
pub fn build_encryption_sentinel_comments(
    app_id: &str,
    collection: &str,
    schema: &serde_json::Value,
    backend: &dyn SchemaRenderer,
) -> Vec<String> {
    let mut out = Vec::new();
    let Some(obj) = schema.as_object() else {
        return out;
    };
    for (field, def) in obj {
        if is_schema_metadata_key(field) {
            continue;
        }
        // Reuse the single-source-of-truth body builder — no re-spelling.
        let Some(body) = encryption_sentinel_body_for_field(def) else {
            continue;
        };
        let escaped = body.replace('\'', "''");
        out.push(format!(
            "COMMENT ON COLUMN {}.{}.{} IS '{}'",
            backend.quote_ident(app_id),
            backend.quote_ident(collection),
            backend.quote_ident(field),
            escaped,
        ));
    }
    out
}

/// Return the sibling column name `<field>_masked` IFF
/// the field's schema entry carries a `.mask({...})` declaration with
/// `kind != "none"`. Returns `None` for non-masked columns and for
/// columns that explicitly opt out via `.mask({ kind: "none" })`.
///
/// The platform reserves the `_masked` suffix at the field-name level
/// (`validate_field_name`'s `ReservedName::Suffix`) so a creator cannot
/// shadow a sibling. Called by both `build_create_table_with_fks_for_dialect`
/// (DDL emission) and `build_insert` / `build_set_clauses` (atomic
/// dual-write).
pub fn mask_sibling_column_for_field(field: &str, def: &serde_json::Value) -> Option<String> {
    let mask_meta = def.get("mask").and_then(|v| v.as_object())?;
    let kind = mask_meta
        .get("kind")
        .and_then(|v| v.as_str())
        .unwrap_or("full");
    if kind == "none" {
        return None;
    }
    Some(format!("{field}_masked"))
}

/// Render the canonical mask-sentinel comment payload
/// for a field's `.mask({...})` declaration, IFF the declaration is
/// present AND `kind != "none"`. Returns `None` when there's no
/// sibling to attach a sentinel to.
///
/// Reused by both backend introspectors (PG `COMMENT ON COLUMN` write
/// + SQLite inline-comment parse on read) — keeps the wire shape
/// consistent. The parser side lives in
/// [`crate::mask_codec::parse_mask_sentinel`].
pub fn mask_sentinel_for_field(def: &serde_json::Value) -> Option<String> {
    let mask_meta = def.get("mask").and_then(|v| v.as_object())?;
    let kind_str = mask_meta
        .get("kind")
        .and_then(|v| v.as_str())
        .unwrap_or("full");
    if kind_str == "none" {
        return None;
    }
    let kind = crate::mask_meta::MaskKind::from_sql(kind_str)?;
    let class_str = mask_meta
        .get("classification")
        .and_then(|v| v.as_str())
        .unwrap_or("pii");
    let classification = crate::mask_meta::Classification::from_sql(class_str)?;
    Some(crate::mask_codec::build_mask_sentinel(kind, classification))
}

/// Render the `COMMENT ON COLUMN` statements that
/// attach the mask sentinel to every sibling column. Returns one
/// statement per masked field in `schema` (in declared order); the
/// caller joins them onto the CREATE TABLE / ALTER TABLE SQL via
/// `;` so they apply atomically.
///
/// Only the PG arm executes these statements — SQLite doesn't support
/// `COMMENT ON COLUMN`. The SQLite arm relies on the inline
/// `/* zero-migrate:mask:... */` comment emitted by
/// `build_create_table_with_fks_for_dialect`,
/// preserved verbatim in `sqlite_master.sql`.
///
/// Returns the empty vector when the schema declares no masked
/// columns — the caller then emits no extra DDL.
#[must_use]
pub fn build_mask_sentinel_comments(
    app_id: &str,
    collection: &str,
    schema: &serde_json::Value,
    backend: &dyn SchemaRenderer,
) -> Vec<String> {
    let mut out = Vec::new();
    let Some(obj) = schema.as_object() else {
        return out;
    };
    for (field, def) in obj {
        if is_schema_metadata_key(field) {
            continue;
        }
        let Some(sibling) = mask_sibling_column_for_field(field, def) else {
            continue;
        };
        let Some(sentinel) = mask_sentinel_for_field(def) else {
            continue;
        };
        // Escape single quotes in the sentinel body for the SQL string
        // literal. The kind+classification alphabet contains none, but
        // be defensive against a future kind that does.
        let escaped = sentinel.replace('\'', "''");
        out.push(format!(
            "COMMENT ON COLUMN {}.{}.{} IS '{}'",
            backend.quote_ident(app_id),
            backend.quote_ident(collection),
            backend.quote_ident(&sibling),
            escaped,
        ));
    }
    out
}

/// The bare `zero-migrate:enc:<mode>:<keyId>:<wraps>` sentinel BODY for a field's
/// `t.encrypted({...})` declaration (no `/* */` wrapper, no comment statement),
/// or `None` for a plain column. The SINGLE source of truth for the `zero-migrate:enc` wire
/// grammar: `encryption_sentinel_for_field` wraps it in `/* */` for the inline
/// DDL form, and [`build_encryption_sentinel_comments`] wraps it in a
/// `COMMENT ON COLUMN … '…'` statement for the PG-recoverable form. The runtime
/// parser is [`crate::mask_codec::parse_encryption_sentinel`].
#[must_use]
pub fn encryption_sentinel_body_for_field(def: &serde_json::Value) -> Option<String> {
    let enc = def.get("encrypted").and_then(|v| v.as_object())?;
    let mode = enc
        .get("mode")
        .and_then(|v| v.as_str())
        .unwrap_or("randomised");
    // Normalise legacy `"randomized"` (US spelling) to the canonical
    // `randomised` so the introspector parser (which accepts both but the
    // emit side normalises to one) round-trips cleanly.
    let mode_norm = if mode == "randomized" {
        "randomised"
    } else {
        mode
    };
    let key_id = enc
        .get("keyId")
        .and_then(|v| v.as_str())
        .unwrap_or("default");
    let wraps = enc
        .get("wraps")
        .and_then(|v| v.as_str())
        .unwrap_or("string");
    Some(format!("zero-migrate:enc:{mode_norm}:{key_id}:{wraps}"))
}

/// The portable `caseSensitive` intent a field def carries, in the shape
/// `zero_migrate::render::declarative::mysql_collation_clause` reads.
///
/// The SDK def only ever carries the key when it is FALSE (see
/// `render::declarative::field_to_sdk_def`), so an absent key is the default
/// case-SENSITIVE intent and maps to `None` - the same canonical spelling
/// `ColumnSnapshot::case_sensitive` uses, which is why the two carriers can share one
/// clause builder.
pub fn def_case_sensitive(def: &serde_json::Value) -> Option<bool> {
    def.get("caseSensitive")
        .and_then(serde_json::Value::as_bool)
}

pub fn char_len(def: &serde_json::Value) -> Option<u64> {
    def.get("charLen")
        .and_then(serde_json::Value::as_u64)
        .filter(|len| *len > 0)
}

pub fn max_length(def: &serde_json::Value) -> Option<u64> {
    def.get("maxLength")
        .and_then(serde_json::Value::as_u64)
        .filter(|len| *len > 0)
}

/// The `(precision, scale)` of a FIXED-PRECISION decimal column, or `None` for a
/// float.
///
/// The `number` token is shared: `render::lower::col_type_to_token` spells BOTH
/// `ColType::Double` and `ColType::Decimal { .. }` as `"number"`, because that is the
/// vocabulary the shared `FieldDef` kernel speaks. So no arm of any `column_type`
/// match below can tell the two apart from the token; the parameters ride beside it,
/// the way `charLen` and `maxLength` ride beside `char` and `string`.
///
/// This is the ONE reader that decides which of the two a `number` field is, so the
/// three dialect emitters cannot drift from each other - and reading it makes them
/// agree with `render::lower::author_type_override`, which answers `numeric(p, s)` /
/// `DECIMAL(p, s)` / SQLite `TEXT` for the same `ColType` on the snapshot carrier.
/// The disagreement it closes was measured, not inferred: a SQLite rebuild reading the
/// bare token re-declared a `t.numeric(20, 4)` column `REAL` and copied
/// `12345678901234.5678` across as `12345678901234.6`
/// (`tests/fold_live/sqlite_decimal_rebuild_live.rs`).
///
/// A zero or absent `precision` is NOT a decimal: `DECIMAL(0, …)` is not a type any
/// dialect accepts, so a malformed facet falls back to the float spelling the column
/// had before rather than emitting DDL no server will take.
pub fn decimal_precision_scale(def: &serde_json::Value) -> Option<(u64, u64)> {
    let precision = def
        .get("precision")
        .and_then(serde_json::Value::as_u64)
        .filter(|precision| *precision > 0)?;
    let scale = def
        .get("scale")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    Some((precision, scale))
}
