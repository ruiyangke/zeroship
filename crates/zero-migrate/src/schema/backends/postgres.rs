//! PostgreSQL schema/DDL spelling. The future `zero-migrate-postgres`.

use crate::schema::query::{
    build_encryption_sentinel_comments, build_mask_sentinel_comments, char_len,
    decimal_precision_scale, def_to_pg_type, max_length, quote_ident_for_dialect, SchemaRenderer,
    SqlDialect,
};

/// This module's own vendor identity — the ONE dialect literal it is allowed to
/// name. See `backends/mod.rs` for why. Deleting this const is the whole of the
/// edit this module needs when it becomes its own crate.
const DIALECT: SqlDialect = SqlDialect::Postgres;

pub(super) struct PostgresSchemaRenderer;

pub(super) static RENDERER: PostgresSchemaRenderer = PostgresSchemaRenderer;

impl SchemaRenderer for PostgresSchemaRenderer {
    fn dialect(&self) -> SqlDialect {
        DIALECT
    }

    fn foreign_key_target(&self, app_id: &str, target: &str) -> String {
        format!(
            "{}.{}",
            quote_ident_for_dialect(app_id, self.dialect()),
            quote_ident_for_dialect(target, self.dialect())
        )
    }

    fn column_type(&self, def: &serde_json::Value) -> String {
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

        // `t.string({ length })` — bounded VARCHAR. A `string` token carrying an
        // explicit `maxLength` renders `character varying(N)` (SQL-standard spelling
        // for `VARCHAR(N)`); a `string` without one keeps the unbounded `TEXT`
        // spelling from `def_to_pg_type` (uuid / typed-id / ref land there).
        if zs_type == Some("string") {
            if let Some(len) = max_length(def) {
                return format!("character varying({len})");
            }
        }

        // A `number` carrying `precision` is `t.numeric({ precision, scale })`, not
        // `t.number()`. `def_to_pg_type`'s `DOUBLE PRECISION` is the right answer for
        // the float and the wrong one for the decimal - and `author_type_override`
        // already spells this column `numeric(p, s)` on the snapshot carrier, so
        // reading the facet here is what stops the two carriers describing the same
        // column differently. The doc on `def_to_pg_type`'s `number` arm explains why
        // a BARE `number` must stay `DOUBLE PRECISION`: this branch narrows only the
        // columns that asked for exactness.
        if zs_type == Some("number") {
            if let Some((precision, scale)) = decimal_precision_scale(def) {
                return format!("numeric({precision}, {scale})");
            }
        }

        def_to_pg_type(def).to_string()
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
        let mut statements = build_mask_sentinel_comments(app_id, collection, schema);
        statements.extend(build_encryption_sentinel_comments(
            app_id, collection, schema,
        ));
        statements
    }
}
