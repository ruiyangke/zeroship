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
use crate::snapshot::{ColumnSnapshot, IndexSnapshot, SchemaSnapshot};
use zero_migrate_ir::dialect::DialectId;

/// How a backend reconciles an existing column whose type/nullability changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExistingColumnChangeStrategy {
    Native,
    TableRebuild,
    Refuse,
}

/// How a backend lowers a column rename.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColumnRenameStrategy {
    ExpandContract,
    TableRebuild,
    Refuse(&'static str),
}

/// The already-quoted names one dual-write trigger is built from.
///
/// Every field arrives QUOTED, by the same renderer that is being asked for the SQL —
/// the engine composes the names (the generated function and trigger names are its
/// own, bounded by `GENERATED_IDENT_MAX_BYTES`) and the backend spells them. Passing
/// the quoted forms rather than the bare ones keeps the one identifier-quoting seam
/// the tree already has instead of opening a second one inside this method.
#[derive(Debug, Clone, Copy)]
pub struct DualWriteTriggerSpec<'a> {
    /// Quoted, schema-qualified name of the generated trigger function.
    pub function: &'a str,
    /// Quoted name of the generated trigger.
    pub trigger: &'a str,
    /// Quoted, schema-qualified name of the table being renamed on.
    pub table: &'a str,
    /// Quoted name of the legacy column.
    pub from: &'a str,
    /// Quoted name of the new column.
    pub to: &'a str,
}

/// The two statements a managed dual-write trigger needs over its lifetime.
///
/// Both are returned together because the engine uses `remove` in three places — the
/// structural rollback of the install step, the contract step that tears the trigger
/// down, and that contract step's own `down` — and a backend that spelled the install
/// without the matching removal would strand a trigger the contract's
/// `DROP COLUMN <from>` then runs beside.
#[derive(Debug, Clone)]
pub struct DualWriteTriggerSql {
    /// Installs the function and the trigger. Must be re-runnable.
    pub install: String,
    /// Removes the trigger and the function. Must be idempotent, so a partly
    /// applied install can still be torn down.
    pub remove: String,
}

/// The physical evidence available while validating whether a column may be
/// used as a key without a prefix length.
///
/// The engine owns the neutral declaration/catalog traversal. The selected
/// backend owns the meaning of the physical spelling that traversal reaches.
#[derive(Debug, Clone, Copy)]
pub enum KeyStorageEvidence<'a> {
    /// The exact type spelling this backend renders for an authored column.
    RenderedType(&'a str),
    /// The live catalog column, including any backend-owned physical metadata.
    CatalogColumn(&'a ColumnSnapshot),
}

/// A backend-owned schema-validation refusal.
///
/// Core supplies only the op index and dialect provenance when projecting this
/// into its public authoring error. The reason and remedy remain in the backend
/// that owns the physical storage rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorageValidationRefusal {
    /// Operator-facing explanation of the backend's physical storage rule.
    pub reason: String,
    /// Operator-facing remedy written by the backend that owns the rule.
    pub suggested_fix: String,
}

/// The already-validated neutral pieces of one additive column change.
///
/// The selected backend owns the surrounding `ALTER TABLE` grammar and the
/// placement (or refusal) of a catalog comment sentinel. Core deliberately
/// supplies primitives rather than a vendor-shaped column-definition type.
#[derive(Debug, Clone, Copy)]
pub struct AddColumnIfNotExistsRequest<'a> {
    pub schema: &'a str,
    pub table: &'a str,
    pub column: &'a str,
    pub definition: AddColumnDefinition<'a>,
    pub comment_sentinel: Option<&'a str>,
}

/// The neutral definition input for an additive column request.
///
/// `Rendered` carries fragments produced for this same selected backend. The
/// synthetic mask sibling instead uses `NullableUnboundedText`, leaving both
/// its physical type and nullability spellings entirely to that backend.
#[derive(Debug, Clone, Copy)]
pub enum AddColumnDefinition<'a> {
    Rendered {
        data_type: &'a str,
        constraints: &'a str,
    },
    NullableUnboundedText,
}

/// The already-validated neutral pieces of one additive index change.
///
/// `if_not_exists` is part of the requested recovery semantics. A backend that
/// cannot express those semantics must return its own refusal rather than inherit
/// another vendor's syntax.
#[derive(Debug, Clone, Copy)]
pub struct CreateIndexIfNotExistsRequest<'a> {
    pub schema: &'a str,
    pub table: &'a str,
    pub name: &'a str,
    pub columns: &'a [&'a str],
    pub unique: bool,
}

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
    /// Which vendor this is, as the OPEN [`DialectId`] rather than the former
    /// closed dialect enum.
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

    /// This backend's table-rebuild policy, or an explicit `None` when its
    /// structural strategy never rebuilds a table.
    ///
    /// Required with no default: a new backend must either register its own
    /// rebuild decisions and stored-DDL rewrites or state that it has none.
    fn table_rebuild_policy(&self)
        -> Option<&'static dyn crate::table_rebuild::TableRebuildPolicy>;

    fn foreign_key_target(&self, app_id: &str, target: &str) -> String;

    /// Compose a snapshot-normalized foreign-key target from already canonical
    /// identifier tokens. Some catalogs retain a namespace qualifier and some
    /// grammars prohibit one.
    fn canonical_fk_target(&self, schema: &str, target: &str) -> String;

    fn column_type(&self, c: &ColumnSnapshot, inline_pk: bool) -> String;

    /// Project a completed neutral column token into the spelling this backend's
    /// catalog snapshot retains in [`ColumnSnapshot::data_type`].
    ///
    /// Required separately from [`Self::column_type`]: emitted DDL and catalog
    /// introspection are not the same vocabulary (PostgreSQL emits `TIMESTAMPTZ`
    /// but reports `timestamp with time zone`; SQLite reports a lowercased declared
    /// type; MySQL canonicalizes `COLUMN_TYPE`). A new backend must state both.
    fn snapshot_data_type(&self, c: &ColumnSnapshot) -> String;

    /// Finalize vendor-owned physical metadata after every neutral column facet
    /// has been applied.
    ///
    /// Required even for backends with no extra physical carrier: a new backend
    /// must state whether the completed column needs a vendor-specific projection
    /// instead of inheriting another engine's answer.
    fn finalize_column_snapshot(&self, column: &mut ColumnSnapshot);

    /// Project a derived vector/geospatial index into this backend's desired
    /// catalog shape. Return false when this backend cannot build the derived
    /// index at all.
    fn project_derived_ann_index(&self, index: &mut IndexSnapshot) -> bool;

    /// Validate vendor-specific physical key-storage restrictions after the
    /// neutral desired shape is complete.
    fn validate_key_storage(
        &self,
        desired: &SchemaSnapshot,
        live: &SchemaSnapshot,
    ) -> Result<(), String>;

    /// Refuse one authored/catalog column as an unprefixed key target when this
    /// backend's physical storage requires a prefix length.
    ///
    /// Required with no default: a backend that has no such restriction must
    /// state that itself by returning `None`.
    fn unprefixed_key_storage_refusal(
        &self,
        position: &str,
        table: &str,
        column: &str,
        evidence: KeyStorageEvidence<'_>,
    ) -> Option<StorageValidationRefusal>;

    /// Refuse one bare literal default when this backend's physical storage
    /// requires an expression form instead.
    ///
    /// Required with no default for the same fail-closed registration reason as
    /// [`Self::unprefixed_key_storage_refusal`].
    fn literal_default_storage_refusal(
        &self,
        column: &str,
        rendered_type: &str,
        rendered_default: &str,
    ) -> Option<StorageValidationRefusal>;

    /// The MANAGED DUAL-WRITE TRIGGER for an expand-contract online rename: the SQL
    /// that installs it and the SQL that removes it, or an explicit `None` from a
    /// backend that does not resolve a rename that way.
    ///
    /// Required with no default, for the same fail-closed reason as
    /// [`Self::stored_ddl`]: a backend that returns [`ColumnRenameStrategy::ExpandContract`]
    /// and no trigger has contradicted itself, and that should be a decision it makes
    /// at its own definition site.
    ///
    /// # Why this is a method and not four `format!`s in the engine
    ///
    /// It was four `format!`s in the engine. `render::expand_contract` spelled
    /// `CREATE OR REPLACE FUNCTION … LANGUAGE plpgsql`, `CREATE TRIGGER … BEFORE
    /// INSERT OR UPDATE … EXECUTE FUNCTION`, and twice `DROP TRIGGER … ON … ; DROP
    /// FUNCTION …` — one vendor's procedural language and one vendor's trigger
    /// grammar, emitted from the neutral engine. Only the `plpgsql` token was visible
    /// to a vendor-name census; the other three statements are just as much one
    /// backend's SQL, and `DROP TRIGGER <t> ON <table>` is not even syntactically
    /// portable.
    ///
    /// The trigger BODY had the same problem one crate lower. It was
    /// `zero_migrate_backend::capability::dual_write_function_body`, twenty lines of
    /// PL/pgSQL — `TG_OP`, `NEW`, `OLD`, `IS DISTINCT FROM`, `RETURN NEW` — in the
    /// crate whose rule is that nothing in it spells a vendor's grammar. It passed
    /// that crate's neutrality census because the census looks for vendor NAMES and
    /// PL/pgSQL contains none. It lives with its backend now, next to the backfill
    /// guard that compares a live trigger against it.
    fn dual_write_trigger(&self, spec: &DualWriteTriggerSpec<'_>) -> Option<DualWriteTriggerSql>;

    /// Required structural strategies used by the neutral declarative planner.
    fn existing_column_change_strategy(&self) -> ExistingColumnChangeStrategy;
    fn column_rename_strategy(&self) -> ColumnRenameStrategy;
    fn supports_forward_inline_foreign_key(&self) -> bool;

    /// Whether this backend's identity-column implementation accepts the target
    /// catalog type. Backends without this restriction explicitly return true.
    fn identity_column_type_allowed(&self, data_type: &str) -> bool;

    /// The types this backend DOES admit for an identity column, in its own
    /// operator-facing words, for the refusal
    /// [`identity_column_type_allowed`](Self::identity_column_type_allowed) produces.
    ///
    /// A refusal that says only "not that one" is not actionable, and
    /// `tests/column_shapes/set_column_type_generation_contracts.rs` pins that the
    /// message must name the legal set. That set is this backend's, so this backend
    /// spells it: the string used to be written into
    /// [`IrLowerError::IdentityColumnTypeUnsupported`](crate::error::IrLowerError)'s
    /// message in the neutral contract, where it was one vendor's rule printed at
    /// every target that reached the arm.
    ///
    /// Required rather than defaulted, like every other method here: a backend that
    /// confines nothing answers so deliberately, and its string is never read because
    /// its `identity_column_type_allowed` never says no.
    fn identity_column_type_confinement(&self) -> &'static str;

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

    /// Render an author-controlled string where the vendor grammar requires a
    /// quoted string token rather than a general string-valued expression.
    fn schema_grammar_string_literal(&self, value: &str) -> String;

    /// Spell an empty JSON object/array expression. `object` distinguishes the
    /// two neutral container values without importing a core enum.
    fn empty_json_expr(&self, object: bool) -> &'static str;

    /// Spell an empty text-array expression, or explicitly refuse when this
    /// backend has no text-array storage type.
    fn empty_text_array_expr(&self) -> Option<&'static str>;

    /// Spell one already-serialized JSON value as a column-default expression.
    fn json_value_default_expr(&self, json: &str) -> String;

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

    /// Render an additive foreign-key change from a clause this same backend
    /// already rendered, or return this backend's explicit refusal.
    fn add_foreign_key_statement(
        &self,
        schema: &str,
        table: &str,
        clause: &str,
    ) -> Result<String, &'static str>;

    /// Render an idempotent named foreign-key removal, or return this backend's
    /// explicit refusal when its grammar requires a structural rebuild.
    fn drop_foreign_key_if_exists_statement(
        &self,
        schema: &str,
        table: &str,
        name: &str,
    ) -> Result<String, &'static str>;

    /// Render one idempotent additive-column payload as structural statements.
    fn add_column_if_not_exists_statements(
        &self,
        request: AddColumnIfNotExistsRequest<'_>,
    ) -> Result<Vec<String>, &'static str>;

    /// Render one idempotent additive-index statement.
    fn create_index_if_not_exists_statement(
        &self,
        request: CreateIndexIfNotExistsRequest<'_>,
    ) -> Result<String, &'static str>;
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
