//! MySQL schema/DDL spelling.

use crate::collation::{mysql_pin_collation, mysql_type_without_collation};
use crate::physical_type::{self, MysqlPhysicalType};
use zeroship_migrate_backend::ddl::ExclusionConstraintRequest;
use zeroship_migrate_backend::renderer::DmlRenderer;
use zeroship_migrate_backend::schema::{
    char_len, decimal_precision_scale, def_case_sensitive, AddColumnIfNotExistsRequest,
    CreateIndexIfNotExistsRequest, KeyStorageEvidence, SchemaRenderer, StorageValidationRefusal,
};
use zeroship_migrate_backend::snapshot::ColumnSnapshot;
use zeroship_migrate_ir::dialect::DialectId;

// This module's vendor identity, read from the crate's ONE declaration of it.
use crate::DIALECT;

#[derive(Debug)]
pub(super) struct MysqlSchemaRenderer;

pub(super) static RENDERER: MysqlSchemaRenderer = MysqlSchemaRenderer;

/// The MySQL storage families whose DDL rules differ from every other column.
///
/// MySQL 8 refuses a bare literal `DEFAULT` on all four, and refuses a key over
/// [`Self::Text`] / [`Self::Blob`] with no prefix length (error 1170). Callers
/// classify the MySQL renderer's output, so this follows physical storage rather
/// than guessing from an authored type name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MysqlStorage {
    /// The `TEXT` family.
    Text,
    /// The `BLOB` family.
    Blob,
    /// `JSON`.
    Json,
    /// The spatial family.
    Geometry,
    /// Everything else: numeric, temporal, `ENUM`, `CHAR(n)`, `VARCHAR(n)`.
    Other,
}

impl MysqlStorage {
    /// Classify a MySQL base type spelling.
    fn of(base: &str) -> Self {
        let upper = base.trim().to_ascii_uppercase();
        let head = upper
            .split(|c: char| c == '(' || c.is_ascii_whitespace())
            .next()
            .unwrap_or("");
        match head {
            "TEXT" | "TINYTEXT" | "MEDIUMTEXT" | "LONGTEXT" => Self::Text,
            "BLOB" | "TINYBLOB" | "MEDIUMBLOB" | "LONGBLOB" => Self::Blob,
            "JSON" => Self::Json,
            "GEOMETRY" | "POINT" | "LINESTRING" | "POLYGON" | "MULTIPOINT" | "MULTILINESTRING"
            | "MULTIPOLYGON" | "GEOMETRYCOLLECTION" => Self::Geometry,
            _ => Self::Other,
        }
    }

    /// Whether MySQL refuses a bare literal `DEFAULT` on this storage.
    const fn refuses_literal_default(self) -> bool {
        matches!(self, Self::Text | Self::Blob | Self::Json | Self::Geometry)
    }

    /// Whether MySQL refuses a key over this storage with no prefix length.
    const fn refuses_key_without_prefix_length(self) -> bool {
        matches!(self, Self::Text | Self::Blob)
    }

    /// The human-facing name used in a refusal.
    const fn label(self) -> &'static str {
        match self {
            Self::Text => "TEXT",
            Self::Blob => "BLOB",
            Self::Json => "JSON",
            Self::Geometry => "GEOMETRY",
            Self::Other => "other",
        }
    }
}

impl SchemaRenderer for MysqlSchemaRenderer {
    fn dialect(&self) -> DialectId {
        DIALECT
    }

    fn quote_ident(&self, ident: &str) -> String {
        crate::dml::RENDERER.quote_ident(ident)
    }

    fn ident_quote_char(&self) -> char {
        '`'
    }

    /// MySQL snapshots expose structured catalog facts rather than retaining a
    /// vendor CREATE statement for surgical rewrites.
    fn stored_ddl(&self) -> Option<&'static dyn zeroship_migrate_backend::stored_ddl::StoredDdl> {
        None
    }

    fn table_rebuild_policy(
        &self,
    ) -> Option<&'static dyn zeroship_migrate_backend::table_rebuild::TableRebuildPolicy> {
        // MySQL's declarative strategy refuses changes that require a complete
        // column/table restatement; it does not borrow SQLite's rebuild grammar.
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
            return mysql_pin_native_enum_collation(ty, c.case_sensitive);
        }

        let rendered = if c.unbounded_text {
            "text".to_string()
        } else if matches!(c.case_sensitive, Some(false))
            && c.data_type.eq_ignore_ascii_case("text")
        {
            // Keep the moved renderer's ordering exact: case-insensitive text
            // selected TEXT before a descriptor type definition was consulted.
            "text".to_string()
        } else if let Some(def) = &c.type_def {
            mysql_base_column_type_for_def(def)
        } else {
            mysql_ddl_type(&c.data_type)
        };

        if c.authored_type || c.type_def.is_some() {
            let case_sensitive = c
                .type_def
                .as_ref()
                .and_then(def_case_sensitive)
                .or(c.case_sensitive);
            self.pin_collation(&rendered, case_sensitive)
        } else {
            mysql_pin_native_enum_collation(&rendered, c.case_sensitive)
        }
    }

    fn snapshot_data_type(&self, c: &ColumnSnapshot) -> String {
        mysql_canonical_type(&self.column_type(c, false))
    }

    /// Stamp this backend's [`physical_type`] leg from the column's FINAL rendered
    /// type. This is the MySQL backend's answer to the neutral
    /// `SchemaRenderer::finalize_column_snapshot` hook, so it is reached only when
    /// MySQL is the registered backend; PostgreSQL and SQLite keep no such projection
    /// and their answers only consume the spent `type_def`.
    ///
    /// Derived from what the renderer DECIDES, not from `data_type`, so it accounts for
    /// `ddl_type_override` and the unbounded-text spelling the same way the emitted DDL
    /// does. Reading the renderer's input instead would describe a column this engine
    /// never creates.
    ///
    /// `inline_pk` is false because it is read only on the SQLite rowid-alias leg
    /// (the SQLite vendor's `sqlite_auto_increment_identity_pk`); the MySQL arm never
    /// consults it.
    ///
    /// The live side parses MySQL's own `COLUMN_TYPE` through the same function. That is
    /// what lets the two sides agree despite spelling apart: the renderer emits
    /// `DECIMAL(65, 30)` and MySQL stores `decimal(65,30)`, and both parse to the same
    /// values.
    ///
    /// # Why this is a FUNCTION rather than three lines at the end of one builder
    ///
    /// The field is a PROJECTION of the finished column, and this builder is not the last
    /// writer of what it projects. `data_type` and `ddl_type_override` are rewritten
    /// AFTER it returns by every facet the builder cannot see from a `FieldDescriptor`
    /// alone - the author type override (`numeric`/`DECIMAL(p, s)`), the UUID column
    /// metadata, the value-format metadata, the bytewise collation override, and the
    /// named-type metadata - in the fold replay and in the lowerer alike. A stamp taken
    /// before those ran describes the type the column briefly had.
    ///
    /// MEASURED on live MySQL 8.4, through the real pipeline: a `createTable` carrying
    /// `decimal(12, 2)` folded `Plain { kind: "double" }` while the server held
    /// `Decimal { precision: 12, scale: 2 }`, and structural drift reported
    /// `column amount data_type expected "numeric" actual "decimal"` against a database
    /// that was exactly what had been deployed. A `uuid` column folded
    /// `Character { length: 191 }` against a live `Character { length: 36 }` at the same
    /// time. So the derivation has ONE spelling and gets applied wherever a column stops
    /// changing - core's `render::fold::finalize_physical_types` hands every
    /// replay-decided column back through this method rather than deriving anything
    /// itself.
    ///
    /// **Only MySQL.** `apply::drift::column_data_types_eq` consults the contract only
    /// when BOTH sides carry one, and PostgreSQL/SQLite introspection leaves it `None`;
    /// filling it on either would compare a contract against an absent one.
    fn finalize_column_snapshot(&self, column: &mut ColumnSnapshot) {
        let rendered = self.column_type(column, false);
        physical_type::record(column, MysqlPhysicalType::parse(&rendered));
        if column.type_def.is_some() {
            // `type_def` is the neutral compiler input, not durable snapshot
            // identity. MySQL needs more than its canonical `data_type` to retain
            // bounds and temporal/decimal parameters, so consume the token into the
            // existing exact DDL-spelling carrier before dropping it. The stored
            // value is the renderer's answer verbatim, including its already-pinned
            // collation; the override path is therefore byte-identical on every
            // later render.
            column.ddl_type_override = Some(rendered);
            column.type_def = None;
        }
    }

    /// MySQL cannot build the derived indexes over these BLOB-backed columns
    /// without additional author choices, so it explicitly omits them.
    fn project_derived_ann_index(
        &self,
        _index: &mut zeroship_migrate_backend::snapshot::IndexSnapshot,
    ) -> bool {
        false
    }

    /// Refuse every desired MySQL key whose rendered column storage is a LOB.
    ///
    /// InnoDB requires an index for BOTH sides of a foreign key and silently
    /// synthesizes the child-side index when the author did not declare one. The
    /// snapshot therefore has two key carriers to inspect: explicit/implicit
    /// primary, unique, and ordinary indexes, plus each FOREIGN KEY's local and
    /// referenced tuples. The IR has no prefix-length element, so none of these
    /// can make a `TEXT`/`BLOB` key legal; letting one reach apply produces MySQL
    /// error 1170 after earlier migration units may already have committed.
    ///
    /// Classification reads this backend's [`physical_type`] leg, never the
    /// neutral `data_type`: MySQL catalog normalization deliberately folds
    /// `VARCHAR(n)` into `"text"`, while the physical contract preserves the
    /// distinction between a bounded character column and a LOB.
    fn validate_key_storage(
        &self,
        desired: &zeroship_migrate_backend::snapshot::SchemaSnapshot,
        live: &zeroship_migrate_backend::snapshot::SchemaSnapshot,
    ) -> Result<(), String> {
        use zeroship_migrate_backend::ddl::{fk_local_columns, fk_referenced_columns, fk_target_table};

        let check = |position: &str, table: &str, columns: &[String]| -> Result<(), String> {
            let snapshot = desired.tables.get(table).or_else(|| live.tables.get(table));
            let Some(snapshot) = snapshot else {
                return Ok(());
            };
            for name in columns {
                let Some(column) = snapshot.columns.iter().find(|column| column.name == *name)
                else {
                    continue;
                };
                let Some(MysqlPhysicalType::Lob { tier }) = physical_type::recorded(column) else {
                    continue;
                };
                return Err(format!(
                    "{position} keys {table}.{name}, which renders as MySQL {} storage; \
                     MySQL refuses a key over a TEXT or BLOB column with no prefix length",
                    tier.to_ascii_uppercase()
                ));
            }
            Ok(())
        };

        for (table, snapshot) in &desired.tables {
            for index in &snapshot.indexes {
                check(
                    &format!("desired index {}", index.name),
                    table,
                    &index.columns,
                )?;
            }
            for constraint in &snapshot.constraints {
                match constraint.kind.as_str() {
                    "PRIMARY KEY" | "UNIQUE" => check(
                        &format!("desired {} constraint {}", constraint.kind, constraint.name),
                        table,
                        &fk_local_columns(&constraint.definition),
                    )?,
                    "FOREIGN KEY" => {
                        check(
                            &format!("desired foreign key {} local key", constraint.name),
                            table,
                            &fk_local_columns(&constraint.definition),
                        )?;
                        if let Some(target) = fk_target_table(&constraint.definition) {
                            check(
                                &format!("desired foreign key {} target key", constraint.name),
                                &target,
                                &fk_referenced_columns(&constraint.definition),
                            )?;
                        }
                    }
                    _ => {}
                }
            }
        }
        Ok(())
    }

    fn unprefixed_key_storage_refusal(
        &self,
        position: &str,
        table: &str,
        column: &str,
        evidence: KeyStorageEvidence<'_>,
    ) -> Option<StorageValidationRefusal> {
        let (storage, witness) = match evidence {
            KeyStorageEvidence::RenderedType(rendered) => {
                (MysqlStorage::of(rendered), "renders as MySQL")
            }
            KeyStorageEvidence::CatalogColumn(column) => {
                let MysqlPhysicalType::Lob { tier } = physical_type::recorded(column)? else {
                    return None;
                };
                (MysqlStorage::of(tier), "the live MySQL catalog reports as")
            }
        };
        if !storage.refuses_key_without_prefix_length() {
            return None;
        }
        Some(StorageValidationRefusal {
            reason: format!(
                "{position} keys {table}.{column}, which {witness} {} storage; \
                 MySQL refuses a key over a TEXT or BLOB column with no prefix length",
                storage.label()
            ),
            suggested_fix:
                "bound the column with t.string({ length }) so it renders VARCHAR, or use a \
                            dialectal PostgreSQL/SQLite leg"
                    .to_string(),
        })
    }

    fn literal_default_storage_refusal(
        &self,
        column: &str,
        rendered_type: &str,
        rendered_default: &str,
    ) -> Option<StorageValidationRefusal> {
        let storage = MysqlStorage::of(rendered_type);
        if !storage.refuses_literal_default() || rendered_default.trim_start().starts_with('(') {
            return None;
        }
        Some(StorageValidationRefusal {
            reason: format!(
                "column {column:?} declares the literal default {rendered_default} but renders as MySQL {} \
                 storage; MySQL refuses a literal DEFAULT on TEXT, BLOB, JSON, and GEOMETRY columns",
                storage.label()
            ),
            suggested_fix: "drop the default for MySQL, bound the column with t.string({ length }) so it renders \
                            VARCHAR, or use a dialectal PostgreSQL/SQLite leg"
                .to_string(),
        })
    }

    fn dual_write_trigger(
        &self,
        _spec: &zeroship_migrate_backend::schema::DualWriteTriggerSpec<'_>,
    ) -> Option<zeroship_migrate_backend::schema::DualWriteTriggerSql> {
        // This backend REFUSES a live column rename (`ColumnRenameStrategy::Refuse`
        // below), so it never reaches an expand-contract sequence at all.
        None
    }

    fn existing_column_change_strategy(
        &self,
    ) -> zeroship_migrate_backend::schema::ExistingColumnChangeStrategy {
        zeroship_migrate_backend::schema::ExistingColumnChangeStrategy::Refuse
    }

    fn column_rename_strategy(&self) -> zeroship_migrate_backend::schema::ColumnRenameStrategy {
        zeroship_migrate_backend::schema::ColumnRenameStrategy::Refuse(
            "renameColumn is render-only for MySQL, not live-rendered",
        )
    }

    fn supports_forward_inline_foreign_key(&self) -> bool {
        false
    }

    fn identity_column_type_allowed(&self, _data_type: &str) -> bool {
        true
    }

    /// Never read while `identity_column_type_allowed` above admits everything.
    ///
    /// DEFERRED DEFECT, recorded here because this is where a reader will look:
    /// this backend's auto-increment column must be an integer type, so the `true`
    /// above is an UNDER-refusal on the declarative lane. It is masked on the IR lane
    /// only because `render::lower` hard-codes one backend's three types instead of
    /// asking this method. Closing it needs a live measurement of what this server
    /// actually rejects, which is a separate change from naming the seam.
    fn identity_column_type_confinement(&self) -> &'static str {
        "any type a column of this target may have"
    }

    fn canonical_type(&self, raw: &str) -> String {
        mysql_canonical_type(raw)
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
            "CREATE {unique_clause}INDEX {} ON {}.{} ({rendered_columns})",
            self.quote_ident(index_name),
            self.quote_ident(app_id),
            self.quote_ident(collection),
        )
    }

    fn schema_string_literal(&self, value: &str) -> String {
        format!("_utf8mb4 X'{}'", hex::encode(value.as_bytes()))
    }

    fn schema_grammar_string_literal(&self, value: &str) -> String {
        crate::dml::grammar_string_literal(value)
    }

    fn empty_json_expr(&self, object: bool) -> &'static str {
        if object {
            "(JSON_OBJECT())"
        } else {
            "(JSON_ARRAY())"
        }
    }

    fn empty_text_array_expr(&self) -> Option<&'static str> {
        Some("(JSON_ARRAY())")
    }

    fn json_value_default_expr(&self, json: &str) -> String {
        format!(
            "(CAST({} AS JSON))",
            crate::dml::RENDERER.inline_string_literal(json)
        )
    }

    fn injected_column_ident(&self, name: &str, _canonical_bare: bool) -> String {
        self.quote_ident(name)
    }

    /// InnoDB has no deferred constraint checks, so its `RESTRICT` and
    /// `NO ACTION` catalog/render forms collapse to the same default.
    fn canonical_fk_action(&self, action: &'static str) -> &'static str {
        if matches!(action, "RESTRICT" | "NO ACTION") {
            "NO ACTION"
        } else {
            action
        }
    }

    fn suppress_string_enum_check(&self, def: &serde_json::Value) -> bool {
        zeroship_migrate_backend::schema::string_enum_values(def).is_some()
    }

    fn pin_collation(&self, rendered: &str, case_sensitive: Option<bool>) -> String {
        mysql_pin_collation(rendered, case_sensitive)
    }

    fn strip_collation<'a>(&self, rendered: &'a str) -> &'a str {
        mysql_type_without_collation(rendered)
    }

    fn json_object_default(&self) -> String {
        "DEFAULT (JSON_OBJECT())".to_string()
    }

    fn json_array_default(&self) -> String {
        "DEFAULT (JSON_ARRAY())".to_string()
    }

    fn current_timestamp_expr(&self) -> &'static str {
        "CURRENT_TIMESTAMP(6)"
    }

    fn column_comment_statements(
        &self,
        _app_id: &str,
        _collection: &str,
        _schema: &serde_json::Value,
    ) -> Vec<String> {
        Vec::new()
    }

    fn add_foreign_key_statement(
        &self,
        _schema: &str,
        _table: &str,
        _clause: &str,
    ) -> Result<String, &'static str> {
        Err("MySQL register-model foreign-key changes are not live-rendered")
    }

    fn drop_foreign_key_if_exists_statement(
        &self,
        _schema: &str,
        _table: &str,
        _name: &str,
    ) -> Result<String, &'static str> {
        Err("MySQL has no DROP FOREIGN KEY IF EXISTS grammar")
    }

    fn add_column_if_not_exists_statements(
        &self,
        _request: AddColumnIfNotExistsRequest<'_>,
    ) -> Result<Vec<String>, &'static str> {
        Err("MySQL register-model column changes are not live-rendered")
    }

    fn create_index_if_not_exists_statement(
        &self,
        _request: CreateIndexIfNotExistsRequest<'_>,
    ) -> Result<String, &'static str> {
        Err("MySQL has no concurrent index-build grammar")
    }

    /// REFUSED: MySQL has no exclusion constraint.
    ///
    /// `None` rather than an approximation. The nearest MySQL shape is a unique
    /// index, which excludes only on equality and would silently accept an authored
    /// overlap constraint while enforcing something narrower. The engine turns this
    /// into its own refusal, naming this dialect.
    fn exclusion_constraint_body(&self, _req: &ExclusionConstraintRequest<'_>) -> Option<String> {
        None
    }
}

fn parse_character_type_len(data_type: &str) -> Option<u64> {
    let lower = data_type.trim().to_ascii_lowercase();
    let inner = lower
        .strip_prefix("character(")
        .or_else(|| lower.strip_prefix("char("))
        .or_else(|| lower.strip_prefix("bpchar("))?
        .strip_suffix(')')?;
    inner.parse::<u64>().ok().filter(|len| *len > 0)
}

/// Canonicalise MySQL `information_schema.COLUMNS.COLUMN_TYPE` / rendered DDL
/// type strings for drift/probe comparison.
#[must_use]
fn mysql_canonical_type(data_type: &str) -> String {
    let lower = data_type.trim().to_ascii_lowercase();
    // Strip an explicit `CHARACTER SET ... COLLATE ...` clause: it is the column's
    // collation, which the base-family canonicalization ignores (charset/collation
    // is compared independently). `VARCHAR(255) CHARACTER SET utf8mb4 COLLATE
    // utf8mb4_0900_as_cs` and a bare `varchar(255)` canonicalize to the same base family.
    let lower = lower
        .split(" character set ")
        .next()
        .and_then(|head| head.split(" collate ").next())
        .unwrap_or(&lower)
        .trim()
        .to_string();
    let no_width = strip_mysql_int_display_width(&lower);
    if no_width.starts_with("enum(") {
        return no_width;
    }
    if no_width == "varchar(43)" || no_width == "inet" {
        return "inet".to_string();
    }
    if let Some(len) = parse_character_type_len(&no_width) {
        return format!("character({len})");
    }
    // `character varying(n)` is the DIALECT-NEUTRAL spelling a bounded `t.string({
    // length })` carries in `data_type`; `varchar(n)` is what MySQL's catalog reports for
    // the same column. Both must fold to the same family or the differ sees a phantom
    // type change on every bounded string and refuses the deploy. Measured: a live
    // MySQL declarative re-deploy of a `character varying(191)` id column was refused
    // with `ExistingColumnChangeRefused { change: "type" }` until this arm existed.
    if no_width.starts_with("varchar(")
        || no_width.starts_with("character varying(")
        || no_width == "character varying"
        || no_width.ends_with("text")
        || no_width == "char"
    {
        return "text".to_string();
    }
    if no_width.starts_with("varbinary(") || no_width.ends_with("blob") || no_width == "bytea" {
        return "blob".to_string();
    }
    if no_width.starts_with("datetime")
        || no_width.starts_with("timestamp")
        || matches!(
            no_width.as_str(),
            "timestamp with time zone" | "timestamptz"
        )
    {
        return "datetime".to_string();
    }
    if no_width.starts_with("decimal") || no_width == "numeric" {
        return "decimal".to_string();
    }
    if no_width.starts_with("double") || matches!(no_width.as_str(), "double precision" | "float8")
    {
        return "double".to_string();
    }
    if matches!(no_width.as_str(), "float" | "real" | "float4") {
        return "real".to_string();
    }
    if no_width.starts_with("tinyint(1)") || no_width == "boolean" {
        return "boolean".to_string();
    }
    match no_width.as_str() {
        "smallint" | "int2" => "smallint".to_string(),
        "int" | "integer" | "int4" => "int".to_string(),
        "bigint" | "int8" => "bigint".to_string(),
        "json" | "jsonb" | "text[]" => "json".to_string(),
        "date" => "date".to_string(),
        "point" | "point srid 4326" | "geography(point, 4326)" | "geography(POINT, 4326)" => {
            "point".to_string()
        }
        other => other.to_string(),
    }
}

fn strip_mysql_int_display_width(input: &str) -> String {
    for ty in [
        "tinyint",
        "smallint",
        "mediumint",
        "int",
        "integer",
        "bigint",
    ] {
        if let Some(rest) = input.strip_prefix(ty) {
            if let Some(after_open) = rest.strip_prefix('(') {
                if let Some((digits, after_close)) = after_open.split_once(')') {
                    if digits.chars().all(|c| c.is_ascii_digit()) {
                        return format!("{ty}{after_close}");
                    }
                }
            }
        }
    }
    input.to_string()
}

/// Pin an explicit collation onto a rendered MySQL `ENUM(...)` spelling.
///
/// Snapshot-native catalog carriers historically pin only native enums here;
/// authored character columns take the broader pass in `column_type`. Keeping the
/// distinction preserves catalog-carrier bytes while the neutral authored marker
/// replaces the vendor-spelled override that core used to precompute.
fn mysql_pin_native_enum_collation(rendered: &str, case_sensitive: Option<bool>) -> String {
    if !rendered.trim().to_ascii_lowercase().starts_with("enum(") {
        return rendered.to_string();
    }
    mysql_pin_collation(rendered, case_sensitive)
}

fn mysql_ddl_type(data_type: &str) -> String {
    let lower = data_type.trim().to_ascii_lowercase();
    if lower.starts_with("enum(") {
        return data_type.to_string();
    }
    if lower.starts_with("vector(") {
        return "BLOB".to_string();
    }
    if let Some(len) = char_len_from_data_type(&lower) {
        return format!("CHAR({len})");
    }
    if let Some(len) = varchar_len_from_data_type(&lower) {
        return format!("VARCHAR({len})");
    }
    match lower.as_str() {
        "text" => "VARCHAR(191)".to_string(),
        "double precision" | "float8" => "DOUBLE".to_string(),
        "real" | "float4" => "FLOAT".to_string(),
        "boolean" => "TINYINT(1)".to_string(),
        "timestamp with time zone" | "timestamptz" => "DATETIME(6)".to_string(),
        "date" => "DATE".to_string(),
        "jsonb" | "json" => "JSON".to_string(),
        "text[]" => "JSON".to_string(),
        "bytea" | "blob" => "LONGBLOB".to_string(),
        "numeric" | "decimal" => "DECIMAL(65, 30)".to_string(),
        "integer" | "int" | "int4" => "INT".to_string(),
        "smallint" | "int2" => "SMALLINT".to_string(),
        "bigint" | "int8" => "BIGINT".to_string(),
        "inet" => "VARCHAR(43)".to_string(),
        "geography(point, 4326)" | "geography(POINT, 4326)" => "POINT SRID 4326".to_string(),
        other => other.to_string(),
    }
}

fn char_len_from_data_type(data_type: &str) -> Option<u32> {
    let lower = data_type.trim().to_ascii_lowercase();
    let inner = lower
        .strip_prefix("character(")
        .or_else(|| lower.strip_prefix("char("))
        .or_else(|| lower.strip_prefix("bpchar("))?
        .strip_suffix(')')?;
    inner.parse::<u32>().ok().filter(|len| *len > 0)
}

fn varchar_len_from_data_type(data_type: &str) -> Option<u32> {
    let lower = data_type.trim().to_ascii_lowercase();
    let inner = lower
        .strip_prefix("character varying(")
        .or_else(|| lower.strip_prefix("varchar("))?
        .strip_suffix(')')?;
    inner.parse::<u32>().ok().filter(|len| *len > 0)
}

fn mysql_native_enum_values(def: &serde_json::Value) -> Option<Vec<String>> {
    let values = def.get("enum")?.as_array()?;
    let mut rendered = Vec::with_capacity(values.len());
    for value in values {
        let s = value.as_str()?;
        // MySQL's ENUM value grammar accepts a bare hex literal but rejects the
        // `_utf8mb4 X'...'` introduced form used in expression positions. The
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

/// Legacy SDK-token-to-MySQL-spelling table, moved out of the neutral contract.
///
/// Snapshot rendering no longer calls this JSON carrier. It remains vendor-owned
/// while the remaining SDK-definition producers lower to neutral snapshots.
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

#[cfg(test)]
mod tests {
    use super::{SchemaRenderer, RENDERER};
    use zeroship_migrate_backend::snapshot::ColumnSnapshot;

    const PIN: &str = "CHARACTER SET utf8mb4 COLLATE utf8mb4_0900_as_cs";

    #[test]
    fn both_unbounded_text_carriers_converge_on_text() {
        let ir = ColumnSnapshot {
            data_type: "text".to_string(),
            unbounded_text: true,
            authored_type: true,
            ..Default::default()
        };
        let descriptor = ColumnSnapshot {
            data_type: "text".to_string(),
            unbounded_text: true,
            type_def: Some(serde_json::json!({ "type": "string" })),
            authored_type: true,
            ..Default::default()
        };

        let expected = format!("text {PIN}");
        assert_eq!(RENDERER.column_type(&ir, false), expected);
        assert_eq!(RENDERER.column_type(&descriptor, false), expected);
    }

    #[test]
    fn a_bounded_string_remains_varchar() {
        let bounded = ColumnSnapshot {
            data_type: "character varying(64)".to_string(),
            authored_type: true,
            ..Default::default()
        };
        assert_eq!(
            RENDERER.column_type(&bounded, false),
            format!("VARCHAR(64) {PIN}")
        );
    }

    #[test]
    fn collation_contract_pins_and_strips_the_vendor_suffix() {
        let case_sensitive = RENDERER.pin_collation("VARCHAR(128)", None);
        assert_eq!(case_sensitive, format!("VARCHAR(128) {PIN}"));
        assert_eq!(RENDERER.strip_collation(&case_sensitive), "VARCHAR(128)");

        let case_insensitive = RENDERER.pin_collation("TEXT", Some(false));
        assert_eq!(
            case_insensitive,
            "TEXT CHARACTER SET utf8mb4 COLLATE utf8mb4_0900_ai_ci"
        );
        assert_eq!(RENDERER.strip_collation(&case_insensitive), "TEXT");

        assert_eq!(RENDERER.pin_collation("JSON", None), "JSON");
        assert_eq!(RENDERER.strip_collation("JSON"), "JSON");
    }

    #[test]
    fn catalog_carrier_preserves_the_old_snapshot_dispatch_domain() {
        let bounded = ColumnSnapshot {
            data_type: "character varying(64)".to_string(),
            ..Default::default()
        };
        assert_eq!(RENDERER.column_type(&bounded, false), "VARCHAR(64)");

        let non_text_case_facet = ColumnSnapshot {
            data_type: "integer".to_string(),
            case_sensitive: Some(false),
            ..Default::default()
        };
        assert_eq!(RENDERER.column_type(&non_text_case_facet, false), "INT");

        let whitespace_override = ColumnSnapshot {
            ddl_type_override: Some("  JSON  ".to_string()),
            authored_type: true,
            ..Default::default()
        };
        assert_eq!(
            RENDERER.column_type(&whitespace_override, false),
            "  JSON  "
        );

        let native_enum = ColumnSnapshot {
            data_type: "ENUM('open', 'closed')".to_string(),
            ..Default::default()
        };
        assert_eq!(
            RENDERER.column_type(&native_enum, false),
            format!("ENUM('open', 'closed') {PIN}")
        );
    }
}
