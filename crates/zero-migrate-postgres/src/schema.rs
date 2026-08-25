//! PostgreSQL schema/DDL spelling. The future `zero-migrate-postgres`.

use zero_migrate_backend::ddl::ExclusionConstraintRequest;
use zero_migrate_backend::renderer::DmlRenderer;
use zero_migrate_backend::schema::{
    build_encryption_sentinel_comments, build_mask_sentinel_comments, char_len,
    decimal_precision_scale, max_length, AddColumnDefinition, AddColumnIfNotExistsRequest,
    CreateIndexIfNotExistsRequest, SchemaRenderer,
};
use zero_migrate_backend::snapshot::ColumnSnapshot;
use zero_migrate_ir::dialect::DialectId;
use zero_migrate_ir::ir::{ExclusionMethod, ExclusionOperator};

// This module's vendor identity, read from the crate's ONE declaration of it.
use crate::DIALECT;

#[derive(Debug)]
pub(super) struct PostgresSchemaRenderer;

pub(super) static RENDERER: PostgresSchemaRenderer = PostgresSchemaRenderer;

impl SchemaRenderer for PostgresSchemaRenderer {
    fn dialect(&self) -> DialectId {
        DIALECT
    }

    fn quote_ident(&self, ident: &str) -> String {
        crate::dml::RENDERER.quote_ident(ident)
    }

    fn ident_quote_char(&self) -> char {
        '"'
    }

    /// PostgreSQL snapshots expose structured catalog facts rather than retaining
    /// a vendor CREATE statement for surgical rewrites.
    fn stored_ddl(&self) -> Option<&'static dyn zero_migrate_backend::stored_ddl::StoredDdl> {
        None
    }

    fn table_rebuild_policy(
        &self,
    ) -> Option<&'static dyn zero_migrate_backend::table_rebuild::TableRebuildPolicy> {
        // PostgreSQL reconciles the supported existing-table changes natively.
        None
    }

    fn foreign_key_target(&self, app_id: &str, target: &str) -> String {
        format!("{}.{}", self.quote_ident(app_id), self.quote_ident(target))
    }

    fn canonical_fk_target(&self, schema: &str, target: &str) -> String {
        format!("{schema}.{target}")
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

    fn snapshot_data_type(&self, c: &ColumnSnapshot) -> String {
        // Preserve the established first-class engine token even though the
        // legacy SDK spelling table still has no `bytes` arm.
        if c.type_def.as_ref().is_some_and(|def| {
            def.get("type").and_then(serde_json::Value::as_str) == Some("bytes")
                && def.get("encrypted").is_none()
        }) {
            return "bytea".to_string();
        }

        let ddl = self.column_type(c, false);
        match ddl.to_ascii_uppercase().as_str() {
            "TEXT" => "text".into(),
            "DOUBLE PRECISION" => "double precision".into(),
            "REAL" => "real".into(),
            "BOOLEAN" => "boolean".into(),
            "TIMESTAMPTZ" => "timestamp with time zone".into(),
            "DATE" => "date".into(),
            "JSONB" => "jsonb".into(),
            "BYTEA" => "bytea".into(),
            "NUMERIC" => "numeric".into(),
            "INTEGER" => "integer".into(),
            "SMALLINT" => "smallint".into(),
            "BIGINT" => "bigint".into(),
            "INET" => "inet".into(),
            "TEXT[]" => "text[]".into(),
            // Parameterised / extension types keep their DDL spelling.
            _ => ddl,
        }
    }

    /// PostgreSQL has no separate vendor-only physical-type projection on the
    /// neutral snapshot; its retained `data_type`/override fields are complete.
    /// Consume the neutral definition after deriving that catalog spelling so
    /// later DDL uses the established snapshot-native casing and aliases.
    fn finalize_column_snapshot(&self, column: &mut ColumnSnapshot) {
        column.type_def = None;
    }

    /// PostgreSQL owns the derived `ivfflat`/GiST shape, so the index is already
    /// in its final catalog form.
    fn project_derived_ann_index(
        &self,
        _index: &mut zero_migrate_backend::snapshot::IndexSnapshot,
    ) -> bool {
        true
    }

    /// PostgreSQL has no additional key-storage restriction at this seam.
    fn validate_key_storage(
        &self,
        _desired: &zero_migrate_backend::snapshot::SchemaSnapshot,
        _live: &zero_migrate_backend::snapshot::SchemaSnapshot,
    ) -> Result<(), String> {
        Ok(())
    }

    fn unprefixed_key_storage_refusal(
        &self,
        _position: &str,
        _table: &str,
        _column: &str,
        _evidence: zero_migrate_backend::schema::KeyStorageEvidence<'_>,
    ) -> Option<zero_migrate_backend::schema::StorageValidationRefusal> {
        // PostgreSQL accepts the physical storage families this engine exposes as
        // key columns without a MySQL-style prefix-length requirement.
        None
    }

    fn literal_default_storage_refusal(
        &self,
        _column: &str,
        _rendered_type: &str,
        _rendered_default: &str,
    ) -> Option<zero_migrate_backend::schema::StorageValidationRefusal> {
        // PostgreSQL accepts these literal defaults; it has no storage-family
        // exception corresponding to MySQL's LOB/JSON/spatial rule.
        None
    }

    fn dual_write_trigger(
        &self,
        spec: &zero_migrate_backend::schema::DualWriteTriggerSpec<'_>,
    ) -> Option<zero_migrate_backend::schema::DualWriteTriggerSql> {
        // This backend answers `ColumnRenameStrategy::ExpandContract` below, so it
        // MUST answer here; the two are one decision stated twice, and a `None`
        // paired with `ExpandContract` would leave the engine with a rename it
        // selected and cannot install.
        Some(crate::dual_write::dual_write_trigger(spec))
    }

    fn existing_column_change_strategy(
        &self,
    ) -> zero_migrate_backend::schema::ExistingColumnChangeStrategy {
        zero_migrate_backend::schema::ExistingColumnChangeStrategy::Native
    }

    fn column_rename_strategy(&self) -> zero_migrate_backend::schema::ColumnRenameStrategy {
        zero_migrate_backend::schema::ColumnRenameStrategy::ExpandContract
    }

    fn supports_forward_inline_foreign_key(&self) -> bool {
        false
    }

    /// Whether `data_type` is one of the three types PostgreSQL lets an IDENTITY
    /// column have.
    ///
    /// MEASURED on PostgreSQL 18.4, and the set is exactly three: `numeric(10,0)` is
    /// refused, and so is a DOMAIN over `integer` — the server checks the type itself,
    /// not what it is built on. So this compares the catalog spelling the snapshot
    /// carries rather than trying to reason about a type's underlying family.
    fn identity_column_type_allowed(&self, data_type: &str) -> bool {
        matches!(
            data_type.trim().to_ascii_lowercase().as_str(),
            "smallint" | "integer" | "bigint" | "int2" | "int4" | "int8"
        )
    }

    /// MEASURED, and the server's own words are carried because they are what an
    /// operator will find in the log if the refusal is bypassed.
    fn identity_column_type_confinement(&self) -> &'static str {
        "smallInt, int or bigInt (`identity column type must be smallint, integer, or bigint`)"
    }

    /// PostgreSQL's desired and catalog spellings are already compared in their
    /// retained form, so its canonicalizer is an explicit identity.
    fn canonical_type(&self, raw: &str) -> String {
        raw.to_string()
    }

    fn create_table_target(&self, app_id: &str, collection: &str, _unqualified: bool) -> String {
        format!(
            "{}.{}",
            self.quote_ident(app_id),
            self.quote_ident(collection)
        )
    }

    fn injected_index_statement(
        &self,
        app_id: &str,
        collection: &str,
        index_name: &str,
        unique: bool,
        columns: &[&str],
        _unqualified: bool,
    ) -> String {
        let unique_clause = if unique { "UNIQUE " } else { "" };
        let rendered_columns = columns
            .iter()
            .map(|column| self.quote_ident(column))
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "CREATE {unique_clause}INDEX IF NOT EXISTS {} ON {}.{} ({rendered_columns})",
            self.quote_ident(index_name),
            self.quote_ident(app_id),
            self.quote_ident(collection),
        )
    }

    fn schema_string_literal(&self, value: &str) -> String {
        format!("'{}'", value.replace('\'', "''"))
    }

    fn schema_grammar_string_literal(&self, value: &str) -> String {
        zero_migrate_backend::dml::sql_string_literal(value)
    }

    fn empty_json_expr(&self, object: bool) -> &'static str {
        if object {
            "'{}'::jsonb"
        } else {
            "'[]'::jsonb"
        }
    }

    fn empty_text_array_expr(&self) -> Option<&'static str> {
        Some("'{}'::text[]")
    }

    fn json_value_default_expr(&self, json: &str) -> String {
        format!(
            "{}::jsonb",
            zero_migrate_backend::dml::sql_string_literal(json)
        )
    }

    fn injected_column_ident(&self, name: &str, canonical_bare: bool) -> String {
        if canonical_bare {
            name.to_string()
        } else {
            self.quote_ident(name)
        }
    }

    /// PostgreSQL preserves `RESTRICT` and `NO ACTION` as distinct catalog
    /// spellings, so its explicit canonicalizer is an identity.
    fn canonical_fk_action(&self, action: &'static str) -> &'static str {
        action
    }

    /// PostgreSQL string enums remain ordinary columns plus membership CHECKs.
    fn suppress_string_enum_check(&self, _def: &serde_json::Value) -> bool {
        false
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

    fn add_foreign_key_statement(
        &self,
        schema: &str,
        table: &str,
        clause: &str,
    ) -> Result<String, &'static str> {
        let table = format!("{}.{}", self.quote_ident(schema), self.quote_ident(table));
        Ok(format!("ALTER TABLE {table} ADD {clause}"))
    }

    fn drop_foreign_key_if_exists_statement(
        &self,
        schema: &str,
        table: &str,
        name: &str,
    ) -> Result<String, &'static str> {
        let table = format!("{}.{}", self.quote_ident(schema), self.quote_ident(table));
        Ok(format!(
            "ALTER TABLE {} DROP CONSTRAINT IF EXISTS {}",
            table,
            self.quote_ident(name)
        ))
    }

    fn add_column_if_not_exists_statements(
        &self,
        request: AddColumnIfNotExistsRequest<'_>,
    ) -> Result<Vec<String>, &'static str> {
        let (data_type, constraints) = match request.definition {
            AddColumnDefinition::Rendered {
                data_type,
                constraints,
            } => (data_type, constraints),
            AddColumnDefinition::NullableUnboundedText => ("TEXT", "NULL"),
        };
        let table = format!(
            "{}.{}",
            self.quote_ident(request.schema),
            self.quote_ident(request.table)
        );
        let mut statements = vec![format!(
            "ALTER TABLE {} ADD COLUMN IF NOT EXISTS {} {} {}",
            table,
            self.quote_ident(request.column),
            data_type,
            constraints
        )
        .trim()
        .to_string()];
        if let Some(sentinel) = request.comment_sentinel {
            let escaped = sentinel.replace('\'', "''");
            statements.push(format!(
                "COMMENT ON COLUMN {}.{}.{} IS '{}'",
                self.quote_ident(request.schema),
                self.quote_ident(request.table),
                self.quote_ident(request.column),
                escaped,
            ));
        }
        Ok(statements)
    }

    fn create_index_if_not_exists_statement(
        &self,
        request: CreateIndexIfNotExistsRequest<'_>,
    ) -> Result<String, &'static str> {
        let table = format!(
            "{}.{}",
            self.quote_ident(request.schema),
            self.quote_ident(request.table)
        );
        let columns = request
            .columns
            .iter()
            .map(|column| self.quote_ident(column))
            .collect::<Vec<_>>()
            .join(", ");
        let kind = if request.unique {
            "UNIQUE INDEX"
        } else {
            "INDEX"
        };
        Ok(format!(
            "CREATE {kind} CONCURRENTLY IF NOT EXISTS {} ON {} ({})",
            self.quote_ident(request.name),
            table,
            columns,
        ))
    }

    /// PostgreSQL is the only shipping vendor with exclusion constraints, so this is the
    /// only implementation that returns `Some`.
    ///
    /// Moved here from the engine's lowering, unchanged in what it emits. `gist` and
    /// `spgist` are PostgreSQL index access methods and `&&` is its overlap operator;
    /// none of that named a vendor while it sat in neutral code, which is why a
    /// product-name census never saw it.
    fn exclusion_constraint_body(&self, req: &ExclusionConstraintRequest<'_>) -> Option<String> {
        let elements = req
            .elements
            .iter()
            .map(|element| {
                format!(
                    "{} WITH {}",
                    element.target,
                    exclusion_operator_sql(element.operator)
                )
            })
            .collect::<Vec<_>>()
            .join(", ");

        let mut body = format!(
            "EXCLUDE USING {} ({elements})",
            exclusion_method_sql(req.method)
        );
        if let Some(predicate) = req.where_predicate {
            body.push_str(" WHERE (");
            body.push_str(predicate);
            body.push(')');
        }
        if let Some(deferrable) = req.deferrable {
            if deferrable {
                body.push_str(" DEFERRABLE");
                if let Some(initially_deferred) = req.initially_deferred {
                    body.push_str(if initially_deferred {
                        " INITIALLY DEFERRED"
                    } else {
                        " INITIALLY IMMEDIATE"
                    });
                }
            } else {
                body.push_str(" NOT DEFERRABLE");
            }
        }
        Some(body)
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
        // `column_type_for_def`. Kept here to keep the
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

/// The index access method token for an exclusion constraint.
fn exclusion_method_sql(method: ExclusionMethod) -> &'static str {
    match method {
        ExclusionMethod::Gist => "gist",
        ExclusionMethod::Spgist => "spgist",
        ExclusionMethod::Btree => "btree",
    }
}

/// The operator token one exclusion element compares with.
fn exclusion_operator_sql(operator: ExclusionOperator) -> &'static str {
    match operator {
        ExclusionOperator::Overlaps => "&&",
        ExclusionOperator::Equal => "=",
        ExclusionOperator::NotEqual => "<>",
        ExclusionOperator::Less => "<",
        ExclusionOperator::Greater => ">",
        ExclusionOperator::LessEqual => "<=",
        ExclusionOperator::GreaterEqual => ">=",
    }
}

#[cfg(test)]
mod tests {
    use super::{AddColumnDefinition, AddColumnIfNotExistsRequest, SchemaRenderer, RENDERER};

    #[test]
    fn nullable_unbounded_text_addition_keeps_exact_postgres_bytes() {
        let statements = RENDERER
            .add_column_if_not_exists_statements(AddColumnIfNotExistsRequest {
                schema: "app1",
                table: "users",
                column: "secret_masked",
                definition: AddColumnDefinition::NullableUnboundedText,
                comment_sentinel: None,
            })
            .expect("PostgreSQL renders the semantic mask-sibling request");
        assert_eq!(
            statements,
            ["ALTER TABLE \"app1\".\"users\" ADD COLUMN IF NOT EXISTS \
              \"secret_masked\" TEXT NULL"]
        );
    }

    #[test]
    fn collation_hooks_are_explicit_postgres_pass_throughs() {
        let rendered = "  public.citext COLLATE custom  ";
        assert_eq!(RENDERER.pin_collation(rendered, Some(false)), rendered);
        assert_eq!(RENDERER.strip_collation(rendered), rendered);
    }

    #[test]
    fn canonical_type_is_explicit_postgres_identity() {
        assert_eq!(
            RENDERER.canonical_type("timestamp with time zone"),
            "timestamp with time zone"
        );
    }
}
