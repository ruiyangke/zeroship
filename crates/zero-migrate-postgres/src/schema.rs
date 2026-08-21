//! PostgreSQL schema/DDL spelling. The future `zero-migrate-postgres`.

use zero_migrate_backend::renderer::DmlRenderer;
use zero_migrate_backend::schema::{
    build_encryption_sentinel_comments, build_mask_sentinel_comments, char_len,
    decimal_precision_scale, max_length, SchemaRenderer,
};
use zero_migrate_backend::snapshot::ColumnSnapshot;
use zero_migrate_ir::dialect::{DialectId, SqlDialect};

/// This module's own vendor identity — the ONE dialect literal it is allowed to
/// name. See `backends/mod.rs` for why. Deleting this const is the whole of the
/// edit this module needs when it becomes its own crate.
const DIALECT: SqlDialect = SqlDialect::Postgres;

#[derive(Debug)]
pub(super) struct PostgresSchemaRenderer;

pub(super) static RENDERER: PostgresSchemaRenderer = PostgresSchemaRenderer;

impl SchemaRenderer for PostgresSchemaRenderer {
    fn dialect(&self) -> DialectId {
        DIALECT.id()
    }

    fn quote_ident(&self, ident: &str) -> String {
        crate::dml::RENDERER.quote_ident(ident)
    }

    fn ident_quote_char(&self) -> char {
        '"'
    }

    fn foreign_key_target(&self, app_id: &str, target: &str) -> String {
        format!("{}.{}", self.quote_ident(app_id), self.quote_ident(target))
    }

    fn column_type(&self, c: &ColumnSnapshot, _inline_pk: bool) -> String {
        if let Some(ty) = &c.ddl_type_override {
            ty.clone()
        } else if let Some(def) = &c.type_def {
            column_type_for_def(def)
        } else if matches!(c.case_sensitive, Some(false))
            && c.data_type.eq_ignore_ascii_case("text")
        {
            "public.citext".to_string()
        } else {
            ddl_type(&c.data_type).to_string()
        }
    }

    /// PostgreSQL represents the portable case-insensitive choice as the `citext`
    /// TYPE in `column_type`; named catalog collations are separate column facets.
    /// There is therefore no engine-added type suffix to pin here.
    fn pin_collation(&self, rendered: &str, _case_sensitive: Option<bool>) -> String {
        rendered.to_string()
    }

    /// For the same reason, a PostgreSQL retype has no renderer-added collation
    /// suffix to remove from its type spelling.
    fn strip_collation<'a>(&self, rendered: &'a str) -> &'a str {
        rendered
    }

    fn json_object_default(&self) -> String {
        "DEFAULT '{}'::jsonb".to_string()
    }

    fn json_array_default(&self) -> String {
        "DEFAULT '[]'::jsonb".to_string()
    }

    fn current_timestamp_expr(&self) -> &'static str {
        "NOW()"
    }

    fn column_comment_statements(
        &self,
        app_id: &str,
        collection: &str,
        schema: &serde_json::Value,
    ) -> Vec<String> {
        let mut statements = build_mask_sentinel_comments(app_id, collection, schema, self);
        statements.extend(build_encryption_sentinel_comments(
            app_id, collection, schema, self,
        ));
        statements
    }
}

fn column_type_for_def(def: &serde_json::Value) -> String {
    if def.get("encrypted").is_some() {
        return "BYTEA".to_string();
    }

    let zs_type = def.get("type").and_then(|t| t.as_str());

    if zs_type == Some("vector") {
        let dims = def
            .get("vectorDims")
            .and_then(serde_json::Value::as_i64)
            .filter(|d| *d > 0 && *d <= 16000)
            .unwrap_or(0);
        if dims > 0 {
            return format!("vector({dims})");
        }
        return "vector".to_string();
    }

    if zs_type == Some("geoPoint") {
        return "geography(POINT, 4326)".to_string();
    }

    if zs_type == Some("char") {
        if let Some(len) = char_len(def) {
            return format!("character({len})");
        }
    }

    if zs_type == Some("string") {
        if let Some(len) = max_length(def) {
            return format!("character varying({len})");
        }
    }

    if zs_type == Some("number") {
        if let Some((precision, scale)) = decimal_precision_scale(def) {
            return format!("numeric({precision}, {scale})");
        }
    }

    def_to_pg_type(def).to_string()
}

fn ddl_type(data_type: &str) -> &str {
    match data_type {
        "timestamp with time zone" => "timestamptz",
        "double precision" => "double precision",
        other => other,
    }
}

/// Legacy SDK-token-to-PostgreSQL-spelling table, moved out of the neutral
/// contract. Snapshot rendering no longer calls this JSON carrier.
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

#[cfg(test)]
mod tests {
    use super::{SchemaRenderer, RENDERER};

    #[test]
    fn collation_hooks_are_explicit_postgres_pass_throughs() {
        let rendered = "  public.citext COLLATE custom  ";
        assert_eq!(RENDERER.pin_collation(rendered, Some(false)), rendered);
        assert_eq!(RENDERER.strip_collation(rendered), rendered);
    }
}
