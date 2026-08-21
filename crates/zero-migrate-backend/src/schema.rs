//! The SCHEMA/DDL half of the backend contract: the [`SchemaRenderer`] trait plus
//! the spelling primitives its three implementations call.
//!
//! The sibling of [`crate::renderer`], for the OTHER renderer registry. There are
//! two, because there are two things a vendor is asked to spell: DML/trigger/view
//! text ([`crate::renderer::DmlRenderer`]) and column/DDL text (this).
//!
//! # What is here and what stayed in the engine
//!
//! `zero_migrate::schema::query` is 5973 lines and almost none of it is a vendor
//! spelling. What lives here is exactly the neutral contract and the shared
//! codecs its methods name:
//!
//! * the trait itself and the identifier forwarders into [`crate::dml`],
//!   described below;
//! * the sentinel builders PostgreSQL's `column_comment_statements` spells
//!   ([`build_encryption_sentinel_comments`], [`build_mask_sentinel_comments`] and
//!   the three field-level readers under them).
//!
//! Composition, validation, and semantic comparison stay in the engine. Physical
//! type and identifier spellings live in the backend that owns them, which is the
//! boundary rule stated at length in `zero_migrate::render::backends`.
//!
//! # The cross-stack edge, unchanged and still ONE forwarder
//!
//! These vendors spell identifiers through `quote_ident_for_backend`, which
//! forwards to [`crate::dml::escape_quote_ident_for_backend`], which resolves
//! through the DML registry. That was already true inside the engine and the
//! crate split did NOT change it: it is one forwarder, and it must stay one. The
//! alternative — each `SchemaRenderer` spelling its own identifiers — would put a
//! second physical home of the quoting bytes back in the tree, which is exactly the
//! defect `render::backends`'s header measured and removed.

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
/// IT THEN LOST A NINTH, AND FOR THE OPPOSITE REASON — `canonical_type` was ALIVE,
/// with a real caller. It was never a SPELLING. PostgreSQL's arm was the IDENTITY
/// (`raw.to_string()`), SQLite's folded to storage affinity and MySQL's folded
/// `varchar(n)` to `text`, so the question it answered was "do these two type
/// spellings MEAN the same" — a drift COMPARISON, not "how does this vendor write
/// a type". By this crate's own backend-boundary rule (stated at length in
/// `render::backends`'s header) comparison and normalization stay in core,
/// dialect-PARAMETERIZED, even when the answer depends on the vendor; only
/// spelling is core ASKING a vendor something. It is now the free function
/// `canonical_type_for_dialect`, sitting beside the two core folds its arms
/// already delegated to, and that is what let `render::existence_probe` — the only
/// caller of this trait outside this module — stop resolving a renderer at all.
/// 8 methods to 7.
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
    fn foreign_key_target(&self, app_id: &str, target: &str) -> String;
    fn column_type(&self, c: &ColumnSnapshot, inline_pk: bool) -> String;
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

/// EMIT a schema-layer identifier in `dialect`'s own spelling.
///
/// THE schema kernel's only identifier-quoting door, and a thin forward to the
/// crate's single dispatch ([`crate::dml::escape_quote_ident_for_backend`],
/// which resolves through `render::backends::renderer`). It used to hold its own
/// three-arm `match` over `SqlDialect` and its own `pub fn quote_ident`, which made
/// this module a SECOND physical home for the ANSI double-quote spelling that
/// `render::backends` declares must have exactly one. Both are gone; the bytes are
/// unchanged and the vendor is now decided by the vendor's own module.
///
/// There is deliberately no un-dialected sibling. `quote_ident(name)` existed here,
/// was `pub`, and spelled `"x"` for a vendor it never named — correct bytes for two
/// of the three shipping dialects and therefore invisible to every assertion about
/// emitted SQL. Its call sites are now split between this function (where a
/// `dialect` is in scope) and `pg_quote_ident` (where the surrounding statement is
/// PostgreSQL-only syntax).
///
/// THE CENSUS, and it is worth stating how it was counted, because the obvious count
/// is wrong. An anchored `quote_ident\(` scan of this file matched 52; a naive
/// `grep -o 'quote_ident('` returns 63 because it also matches inside
/// `mysql_quote_ident(`. Of the 52: one was the definition, one an internal call from
/// the three-arm dispatch this function used to be, one a prose comment, and 49 were
/// real call sites (45 emission, 4 in-src tests). `schema::diff` held 6 more through
/// the `pub` name, and three integration probes imported it as their expected-value
/// oracle. All 49 + 6 now name a dialect; the probes grew their own local spelling,
/// which is what an oracle should have been in the first place.
///
/// MEASURED at `0b45ea46`, on the 1231-test `--lib` binary, by neutering
/// `render::backends::ansi_double_quote_ident` — the one home — with a single
/// appended token:
///
/// | tree | red | note |
/// |------|-----|------|
/// | before | 125 | exactly ONE of them under `schema::` |
/// | before, neutering `schema::query::quote_ident` instead | 39 | DISJOINT from the 125 |
/// | after this change | 164 | 125 + 39, the whole formerly-blind set |
///
/// The two before-sets being disjoint is the measurement that matters: those 39
/// schema-kernel tests could not see the crate's single quoting home at all, which
/// is what "second physical home" means operationally. No test was lost and no
/// emitted byte changed — only who decided it.
pub fn quote_ident_for_backend(name: &str, backend: &dyn crate::renderer::DmlRenderer) -> String {
    crate::dml::escape_quote_ident_for_backend(name, backend)
}

/// EMIT an identifier for a statement whose SYNTAX is PostgreSQL-only.
///
/// The schema kernel's PG-only builders (`CREATE INDEX CONCURRENTLY`,
/// `COMMENT ON COLUMN`, `ALTER TABLE … DROP CONSTRAINT IF EXISTS`,
/// `ADD COLUMN IF NOT EXISTS`, `CREATE SCHEMA`) have no `dialect` parameter because
/// they have no other dialect to be. They still must not spell an identifier for a
/// vendor they never named, so the vendor is in this function's NAME — the same
/// technique as `crate::dml::pg_canonical_ident`, and for the same reason:
/// a red count cannot tell a deliberate PostgreSQL spelling apart from an unrouted
/// one, so the door has to carry the intent.
///
/// This is EMISSION, not the `pg_get_constraintdef` normal form. Nothing in this
/// module builds comparison text — `information_schema` appears here only in two doc
/// comments, and `pg_get_constraintdef` not at all — so
/// `crate::dml::pg_canonical_ident` is deliberately NOT the door used
/// here, even though it would produce identical bytes.
/// THE PIN MOVED, IT DID NOT GO. This used to write `SqlDialect::Postgres` into its
/// own body and resolve a renderer from it; this crate is below the vendors and has
/// no PostgreSQL renderer to resolve. The vendor is now supplied by the caller, and
/// the one place that still writes the literal is
/// `zero_migrate::schema::query::pg_quote_ident`, one line in the engine, which every
/// existing engine caller still names.
pub fn pg_quote_ident_for_backend(
    name: &str,
    backend: &dyn crate::renderer::DmlRenderer,
) -> String {
    quote_ident_for_backend(name, backend)
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
    backend: &dyn crate::renderer::DmlRenderer,
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
            pg_quote_ident_for_backend(app_id, backend),
            pg_quote_ident_for_backend(collection, backend),
            pg_quote_ident_for_backend(field, backend),
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
    backend: &dyn crate::renderer::DmlRenderer,
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
            pg_quote_ident_for_backend(app_id, backend),
            pg_quote_ident_for_backend(collection, backend),
            pg_quote_ident_for_backend(&sibling, backend),
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
