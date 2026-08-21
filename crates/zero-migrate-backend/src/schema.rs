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
//! spelling. What moved is exactly the transitive closure of what the three
//! `SchemaRenderer` impls CALL:
//!
//! * the trait itself, and `quote_ident_for_dialect` / `pg_quote_ident` — the
//!   ONE forwarder from this stack into [`crate::dml`], described below;
//! * the JSON field-shape readers ([`char_len`], [`max_length`],
//!   [`decimal_precision_scale`], [`def_case_sensitive`],
//!   [`is_schema_metadata_key`]);
//! * the two per-vendor type maps a vendor asks for by name ([`def_to_pg_type`],
//!   [`mysql_base_column_type_for_def`]);
//! * the sentinel builders PostgreSQL's `column_comment_statements` spells
//!   ([`build_encryption_sentinel_comments`], [`build_mask_sentinel_comments`] and
//!   the three field-level readers under them).
//!
//! Everything else — the CREATE TABLE composer, the FK/index/constraint builders,
//! the identifier and reserved-name validators, `canonical_type_for_dialect`,
//! `def_to_column_type_for_dialect` — stayed in the engine. Every one of them
//! DECIDES something about a vendor rather than ASKING a vendor how to write
//! something, which is the boundary rule stated at length in
//! `zero_migrate::render::backends`.
//!
//! # The cross-stack edge, unchanged and still ONE forwarder
//!
//! These vendors spell identifiers through `quote_ident_for_dialect`, which
//! forwards to [`crate::dml::escape_quote_ident_for_backend`], which resolves
//! through the DML registry. That was already true inside the engine and the
//! crate split did NOT change it: it is one forwarder, and it must stay one. The
//! alternative — each `SchemaRenderer` spelling its own identifiers — would put a
//! second physical home of the quoting bytes back in the tree, which is exactly the
//! defect `render::backends`'s header measured and removed.

use zero_migrate_ir::dialect::DialectId;

/// Dialect-specific schema/DDL spelling.
///
/// This trait deliberately has no default methods: adding a third dialect must
/// provide every spelling explicitly. The single exhaustive dispatch match lives
/// in `renderer`, so a new `SqlDialect` variant breaks there at compile time
/// and forces the missing renderer to be wired before the crate can build.
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
    fn column_type(&self, def: &serde_json::Value) -> String;
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

pub fn mysql_native_enum_values(def: &serde_json::Value) -> Option<Vec<String>> {
    let values = def.get("enum")?.as_array()?;
    let mut rendered = Vec::with_capacity(values.len());
    for value in values {
        let s = value.as_str()?;
        // MySQL's ENUM value grammar accepts a bare hex literal but rejects the
        // `_utf8mb4 X'…'` introduced form used in expression positions. The
        // column's utf8mb4 character set consumes these UTF-8 bytes while the hex
        // spelling remains independent of `NO_BACKSLASH_ESCAPES`.
        rendered.push(format!("X'{}'", hex::encode(s.as_bytes())));
    }
    if rendered.is_empty() {
        None
    } else {
        Some(rendered)
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

/// The MySQL type spelling for a field def, BEFORE any collation suffix.
///
/// Split out of `MysqlSchemaRenderer::column_type` — now over in
/// `schema::backends::mysql` — so the collation is pinned in
/// exactly ONE place rather than on each of the nine arms that produce a character
/// type. Which arms those are is what the split makes checkable:
///
/// * CHARACTER, and therefore collated: `string` (`VARCHAR(n)` / `LONGTEXT` /
///   `VARCHAR(191)`), `char` (`CHAR(n)` / `CHAR(1)`), `ref` and `inet`
///   (`VARCHAR(191)` / `VARCHAR(43)`), a string-or-null `literal` (`VARCHAR(191)`),
///   the unknown-token fallback (`VARCHAR(191)`), and the native `ENUM(...)`.
/// * NOT character, and therefore bare: `LONGBLOB` (encrypted / `bytes`), `BLOB`
///   (`vector`), `POINT SRID 4326` (`geoPoint`), `JSON` (`json`/`object`/`array`/
///   `union`/`textArray`), and every numeric and temporal spelling. `JSON COLLATE
///   ...` is not redundant but a parse error, so the predicate matters.
///
/// The classification is `zero_migrate::render::declarative::mysql_spelling_takes_collation`
/// reading this function's OUTPUT; nothing here decides it a second time.
pub fn mysql_base_column_type_for_def(def: &serde_json::Value) -> String {
    if def.get("encrypted").is_some() {
        return "LONGBLOB".to_string();
    }

    if let Some(values) = mysql_native_enum_values(def) {
        return format!("ENUM({})", values.join(", "));
    }

    let zs_type = def.get("type").and_then(|t| t.as_str());

    if zs_type == Some("vector") {
        return "BLOB".to_string();
    }

    if zs_type == Some("geoPoint") {
        return "POINT SRID 4326".to_string();
    }

    // The decimal half of the shared `number` token. `DOUBLE` is right for the
    // float and wrong for `t.numeric({ precision, scale })`; the MySQL arm of
    // `render::lower::author_type_override` already spells this column
    // `DECIMAL(p, s)` on the snapshot carrier, so this is the field-def carrier
    // catching up rather than a second opinion. Note that a BARE `DECIMAL` would
    // not do: MySQL reads it as `DECIMAL(10, 0)` and silently truncates the
    // scale, which is why the parameters have to reach this emitter at all.
    if zs_type == Some("number") {
        if let Some((precision, scale)) = decimal_precision_scale(def) {
            return format!("DECIMAL({precision}, {scale})");
        }
    }

    match zs_type {
        Some("string") => {
            let max = def
                .get("maxLength")
                .or_else(|| def.get("max"))
                .and_then(serde_json::Value::as_u64)
                .filter(|n| *n > 0 && *n <= 65_535);
            match max {
                Some(n) if n <= 16_383 => format!("VARCHAR({n})"),
                Some(_) => "LONGTEXT".to_string(),
                None => "VARCHAR(191)".to_string(),
            }
        }
        Some("char") => match char_len(def) {
            Some(len) => format!("CHAR({len})"),
            None => "CHAR(1)".to_string(),
        },
        Some("number") => "DOUBLE".to_string(),
        Some("real") => "FLOAT".to_string(),
        Some("boolean") => "TINYINT(1)".to_string(),
        Some("date") => "DATETIME(6)".to_string(),
        Some("calendarDate") => "DATE".to_string(),
        Some("json") | Some("object") | Some("array") | Some("union") => "JSON".to_string(),
        Some("textArray") => "JSON".to_string(),
        Some("ref") => "VARCHAR(191)".to_string(),
        Some("bytes") => "LONGBLOB".to_string(),
        Some("literal") => match def.get("literalValue") {
            Some(serde_json::Value::Number(_)) => "DECIMAL(65, 30)".to_string(),
            Some(serde_json::Value::Bool(_)) => "TINYINT(1)".to_string(),
            _ => "VARCHAR(191)".to_string(),
        },
        Some("bigInt") | Some("bigint") | Some("int8") => "BIGINT".to_string(),
        Some("integer") | Some("int") | Some("int4") => "INT".to_string(),
        Some("smallInt") => "SMALLINT".to_string(),
        Some("inet") => "VARCHAR(43)".to_string(),
        _ => "VARCHAR(191)".to_string(),
    }
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

/// Map schema type to PostgreSQL type.
///
/// `ref` columns emit `TEXT`, matching the SDK's typed-id wire format. An
/// `INTEGER` FK would fail with `column type mismatch` when the referenced key is
/// text on Postgres. SQLite treats declared types as advisory, but typed-id values
/// are still TEXT-shaped strings at storage time.
pub fn def_to_pg_type(def: &serde_json::Value) -> &'static str {
    match def.get("type").and_then(|t| t.as_str()) {
        Some("string") => "TEXT",
        Some("char") => "TEXT",
        // `t.vector(dims)` maps to pgvector's `vector(N)`.
        // Returning the bare `"vector"` token would lose the dims, so
        // this arm is unused; column DDL composes the dims back in via
        // [`def_to_pg_type_with_dims`]. Kept here to keep the
        // enumeration exhaustive at the type-vocabulary level — a
        // future caller that ignores dims (e.g. a generic introspection
        // path) gets the un-parameterised type.
        Some("vector") => "vector",
        // `t.number()` maps to DOUBLE PRECISION (FLOAT8). JS `number`
        // is an IEEE-754 double, so this is the exact 1:1 mapping.
        // NUMERIC would be more precise but compio-postgres' text-out
        // path doesn't decode it back to a JS value cleanly;
        // `t.bigInteger()` exists for callers who need exact 64-bit
        // ints.
        Some("number") => "DOUBLE PRECISION",
        Some("real") => "REAL",
        // `int`/`integer` are first-class integer tokens (the SQLite arm of
        // `def_to_column_type_for_dialect` already maps them to `INTEGER`; the dev
        // `registerModel` JSON declares `{ type: "int" }`). Before this arm the PG
        // map degraded them to the `_ => TEXT` fallback, so the engine's
        // dialect-agnostic `desired_snapshot` (which spells types via the PG map)
        // recorded `integer` while this emitter would have written TEXT — a
        // permanent drift. Mapping to `INTEGER` here makes the snapshot and the
        // emitter agree on BOTH dialects. PG stays byte-identical for every
        // existing column: the SDK's `t.*` surface never emits a bare `int` on PG
        // (`t.number()` → DOUBLE PRECISION, `t.bigInteger()` → BIGINT), so no
        // previously-emitted PG column changes type. The PG type *names*
        // (`bigint`/`int4`/`int8`) are deliberately NOT accepted — they are not DSL
        // tokens and stay on the TEXT fallback so they remain typo-rejected.
        Some("int") | Some("integer") => "INTEGER",
        Some("smallInt") => "SMALLINT",
        Some("bigInt") => "BIGINT",
        Some("boolean") => "BOOLEAN",
        Some("date") => "TIMESTAMPTZ",
        // `t.calendarDate()` is a `YYYY-MM-DD` value with no time
        // and no timezone, distinct from `t.date()` (TIMESTAMPTZ stored
        // as Unix-ms numbers at the SDK layer).
        Some("calendarDate") => "DATE",
        Some("json") => "JSONB",
        // `t.object({...})` declares a JSONB column. The nested
        // shape is enforced application-side by `validate.ts`; no
        // CHECK constraint is emitted (Postgres JSONB CHECKs are
        // expressible but expensive at write time).
        Some("object") => "JSONB",
        Some("array") => "JSONB",
        Some("textArray") => "text[]",
        // Cascades to TEXT so FK column type matches the
        // `id TEXT PRIMARY KEY`. See doc-comment on
        // [`def_to_pg_type`] for the rationale.
        Some("ref") => "TEXT",
        Some("inet") => "INET",
        // A top-level `t.union(...)` is flattened to discrete
        // columns by the SDK before it reaches the DDL emitter, so this
        // path should never fire for the discriminator column itself
        // (it has the discriminator's primitive type, not "union").
        // A *nested* `t.union(...)` (inside `t.object`) falls through
        // to JSONB storage; per-variant integrity is application-side.
        Some("union") => "JSONB",
        // A top-level `t.literal()` field outside a union would
        // store as TEXT/NUMERIC/BOOLEAN based on its literal type, but
        // by the time the DDL emitter sees it the SDK normaliser keeps
        // the `literal` tag. We pick the primitive type from the
        // literal value so a `t.literal("login")` column becomes TEXT
        // with a CHECK constraint elsewhere.
        Some("literal") => match def.get("literalValue") {
            Some(serde_json::Value::Number(_)) => "NUMERIC",
            Some(serde_json::Value::Bool(_)) => "BOOLEAN",
            _ => "TEXT",
        },
        _ => "TEXT",
    }
}
