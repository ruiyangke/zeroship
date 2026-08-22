use std::collections::BTreeMap;

use zero_migrate_backend::error::IrLowerError;
use zero_migrate_backend::fold::{
    AuthorTypeOverride, CatalogFoldPolicy, CatalogFoldRefusal, FoldCursorColumnContract,
    FoldCursorComparison, FoldCursorScalarType, FoldDatabaseFeature, ReferenceTextStorage,
    SnapshotProvenanceStrength,
};
use zero_migrate_backend::schema::SchemaRenderer;
use zero_migrate_backend::snapshot::{
    ColumnSnapshot, PartitionSnapshot, SequenceSnapshot, TableSnapshot, ViewSnapshot,
};
use zero_migrate_ir::expr::{Expr, SynthFn};
use zero_migrate_ir::ir::{ColType, ValueFormat};
use zero_migrate_ir::precondition::PreconditionCheck;

#[derive(Debug)]
pub(crate) struct MysqlCatalogFoldPolicy;

pub(crate) static POLICY: MysqlCatalogFoldPolicy = MysqlCatalogFoldPolicy;

impl CatalogFoldPolicy for MysqlCatalogFoldPolicy {
    fn snapshot_provenance_strength(
        &self,
        table: &TableSnapshot,
    ) -> Option<SnapshotProvenanceStrength> {
        table
            .columns
            .iter()
            .any(|column| column.mysql_text_storage.is_some())
            .then_some(SnapshotProvenanceStrength::ExactTextStorage)
    }

    fn allocate_implicit_relation_name(
        &self,
        default_name: &str,
        _tables: &BTreeMap<String, TableSnapshot>,
        _partitions: &BTreeMap<String, PartitionSnapshot>,
        _views: &BTreeMap<String, ViewSnapshot>,
        _sequences: &BTreeMap<String, SequenceSnapshot>,
    ) -> String {
        default_name.to_string()
    }

    fn rowid_storage_generates(&self, _stored_create_sql: Option<&str>, _data_type: &str) -> bool {
        false
    }

    fn stored_table_allows_rowid(&self, _stored_create_sql: Option<&str>) -> bool {
        false
    }

    fn stored_primary_key_allows_rowid(
        &self,
        _stored_create_sql: Option<&str>,
        _column: &str,
    ) -> bool {
        false
    }

    fn primary_key_keeps_identity(&self, target_columns: Option<&[String]>, column: &str) -> bool {
        target_columns.is_some_and(|target| target == [column])
    }

    fn reusable_primary_index(
        &self,
        _snapshot: &TableSnapshot,
        _columns: &[String],
        _current_primary_key_name: Option<&str>,
    ) -> Option<String> {
        None
    }

    fn rename_primary_key_after_table_rename(&self, snap: &mut TableSnapshot, to: &str) {
        let renamed_pk = format!("{to}_pkey");
        // The index is matched by the constraint's OLD name rather than by
        // a second `format!`, so a primary key the author named something
        // else still moves together with its implicit index instead of
        // splitting into a renamed constraint and an orphaned index.
        let previous: Vec<String> = snap
            .constraints
            .iter()
            .filter(|constraint| constraint.kind == "PRIMARY KEY")
            .map(|constraint| constraint.name.clone())
            .collect();
        for constraint in &mut snap.constraints {
            if constraint.kind == "PRIMARY KEY" {
                constraint.name.clone_from(&renamed_pk);
            }
        }
        for index in &mut snap.indexes {
            if previous.contains(&index.name) {
                index.name.clone_from(&renamed_pk);
            }
        }
    }

    fn is_native_uuid_type(&self, _data_type: &str) -> bool {
        false
    }

    fn materialized_named_type_metadata(
        &self,
        _ty: &ColType,
        _default_schema: &str,
    ) -> Result<Option<(String, String)>, IrLowerError> {
        Ok(None)
    }

    fn canonical_rename_type_spelling(&self, ty: &str) -> String {
        // MySQL's rename strategy never uses PostgreSQL alias equivalence. Its
        // own schema canonicalizer owns the physical spellings it does compare.
        crate::schema::RENDERER.canonical_type(ty)
    }

    fn inline_enum_check(
        &self,
        _column: &str,
        _values: &[String],
    ) -> Result<Option<String>, IrLowerError> {
        Ok(None)
    }

    fn inline_enum_type(&self, values: &[String]) -> Option<String> {
        Some(format!("ENUM({})", render_enum_values(values)))
    }

    fn folds_check_constraint_identity(&self) -> bool {
        // MySQL exposes CHECK names, clauses, and ENFORCED state. The current fold
        // scope nevertheless does not reconcile arbitrary authored CHECK identity.
        false
    }

    fn refusal_message(&self, refusal: CatalogFoldRefusal) -> &'static str {
        match refusal {
            CatalogFoldRefusal::AlterPrimaryKeyRowidGeneration => {
                "alterPrimaryKey cannot introduce SQLite INTEGER PRIMARY KEY rowid generation"
            }
            CatalogFoldRefusal::AddColumnIdentity => {
                "addColumn identity on SQLite (non-PK identity has no sound SQLite emulation)"
            }
            CatalogFoldRefusal::CreateTableCheckConstraint => {
                "createTable table-level CHECK is PostgreSQL-only"
            }
            CatalogFoldRefusal::CreateTableUniqueConstraint => {
                "createTable table-level UNIQUE on SQLite (the SQLite CREATE \
                 renders from the descriptor; a table-level UNIQUE is not \
                 threaded into the emitter)"
            }
            CatalogFoldRefusal::CreateTableExclusionConstraint => {
                "createTable exclusion constraint is PostgreSQL-only"
            }
            CatalogFoldRefusal::CreateTableNonBtreeIndex => {
                "createTable non-btree index `using` on SQLite (not yet supported)"
            }
            CatalogFoldRefusal::AddCheckConstraint => "addConstraint(check) is PostgreSQL-only",
            CatalogFoldRefusal::AddExclusionConstraint => {
                "addConstraint exclusion constraint is PostgreSQL-only"
            }
        }
    }

    fn physical_type_inputs_equal(&self, left: &ColumnSnapshot, right: &ColumnSnapshot) -> bool {
        left.data_type == right.data_type
            && left.ddl_type_override == right.ddl_type_override
            && left.case_sensitive == right.case_sensitive
            && left.unbounded_text == right.unbounded_text
            && left.type_def == right.type_def
            && left.authored_type == right.authored_type
    }

    fn reference_catalog_type<'a>(&self, column: &'a ColumnSnapshot) -> &'a str {
        column
            .ddl_type_override
            .as_deref()
            .unwrap_or(&column.data_type)
    }

    fn canonical_reference_catalog_type(
        &self,
        data_type: &str,
        _integer_width_is_logically_proven: bool,
    ) -> String {
        crate::schema::RENDERER.canonical_type(data_type)
    }

    fn explicit_reference_text_storage(&self, ddl_type: &str) -> Option<ReferenceTextStorage> {
        let tokens = ddl_type
            .split_ascii_whitespace()
            .map(str::to_ascii_lowercase)
            .collect::<Vec<_>>();
        let character_set = tokens.windows(3).find_map(|window| {
            (window[0] == "character" && window[1] == "set").then(|| window[2].clone())
        });
        let collation = tokens
            .windows(2)
            .find_map(|window| (window[0] == "collate").then(|| window[1].clone()));
        match (character_set, collation) {
            // `utf8mb4` is the platform-default charset, and the only collations the
            // renderer emits on it (`utf8mb4_0900_as_cs` case-sensitive, `utf8mb4_0900_ai_ci`
            // case-insensitive) map 1:1 to the `caseSensitive` intent — which is compared
            // separately. So a `utf8mb4` column is NOT "explicit storage" that requires
            // exact target metadata; only a non-default charset (a typed-id's `ascii`)
            // does, because the charset itself must match for a foreign key.
            (Some(character_set), Some(collation)) if character_set != "utf8mb4" => {
                Some(ReferenceTextStorage {
                    character_set,
                    collation,
                })
            }
            _ => None,
        }
    }

    fn catalog_reference_text_storage(
        &self,
        column: &ColumnSnapshot,
    ) -> Option<ReferenceTextStorage> {
        column
            .mysql_text_storage
            .as_ref()
            .map(|storage| ReferenceTextStorage {
                character_set: storage.character_set.clone(),
                collation: storage.collation.clone(),
            })
    }

    fn compares_reference_named_collation(&self) -> bool {
        false
    }

    fn cursor_column_contract(
        &self,
        column: &ColumnSnapshot,
    ) -> Result<FoldCursorColumnContract, String> {
        let snapshot_database_type = column
            .data_type
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_ascii_lowercase();
        let scalar_type = if type_is_one_of(
            &snapshot_database_type,
            &[
                "tinyint",
                "smallint",
                "mediumint",
                "int",
                "integer",
                "bigint",
                "year",
            ],
        ) {
            // This backend's unsigned integer domain reaches 2^64-1, which cannot
            // fit the signed `int64` tagged scalar. Keep one exact codec for
            // the whole column domain by using the arbitrary-precision
            // decimal tag whenever the catalog type is unsigned.
            if snapshot_database_type
                .split_ascii_whitespace()
                .any(|part| part == "unsigned")
            {
                Some(FoldCursorScalarType::Decimal)
            } else {
                Some(FoldCursorScalarType::Int64)
            }
        } else if type_is_one_of(&snapshot_database_type, &["decimal", "numeric"]) {
            Some(FoldCursorScalarType::Decimal)
        } else if cursor_type_is_character(&snapshot_database_type)
            || type_is_one_of(
                &snapshot_database_type,
                &["date", "datetime", "timestamp", "time"],
            )
        {
            Some(FoldCursorScalarType::String)
        } else {
            None
        }
        .ok_or_else(|| {
            format!(
                "cursor component {:?} has unsupported ordered type {:?}; its scalar/checkpoint comparison semantics cannot be proven",
                column.name, column.data_type
            )
        })?;
        let database_type = crate::schema::RENDERER.canonical_type(&snapshot_database_type);
        let comparison = if scalar_type == FoldCursorScalarType::String
            // Classification must use the original type. Canonical CHAR(N) is
            // `character(N)`, which would otherwise lose the mandatory exact
            // character-set/collation proof.
            && cursor_type_is_character(&snapshot_database_type)
        {
            let storage = column.mysql_text_storage.as_ref().ok_or_else(|| {
                format!(
                    "cursor component {:?} is a MySQL character column but its exact character set and collation are unavailable",
                    column.name
                )
            })?;
            FoldCursorComparison::ExactText {
                character_set: storage.character_set.clone(),
                collation: storage.collation.clone(),
            }
        } else if let Some(collation) = &column.collation {
            FoldCursorComparison::NamedCollation {
                schema: collation.schema.clone(),
                name: collation.name.clone(),
            }
        } else if column.case_sensitive == Some(false) || database_type == "citext" {
            FoldCursorComparison::CaseInsensitive
        } else {
            FoldCursorComparison::Default
        };
        Ok(FoldCursorColumnContract {
            scalar_type,
            database_type,
            comparison,
        })
    }

    fn wrap_default_expr(&self, expr: &Expr, rendered: String) -> String {
        if default_needs_parens(expr) {
            format!("({rendered})")
        } else {
            rendered
        }
    }

    fn database_requirement_for_column(
        &self,
        ty: &ColType,
        is_reference: bool,
    ) -> Option<FoldDatabaseFeature> {
        (!is_reference && matches!(ty, ColType::Uuid))
            .then_some(FoldDatabaseFeature::UuidValidation)
    }

    fn database_requirement_for_value_format(
        &self,
        value_format: &ValueFormat,
    ) -> Option<FoldDatabaseFeature> {
        Some(match value_format {
            ValueFormat::TypeId { .. } => FoldDatabaseFeature::TypeIdValidation,
            ValueFormat::Ulid => FoldDatabaseFeature::UlidValidation,
        })
    }

    fn database_requirement_for_expr(&self, expr: &Expr) -> Option<FoldDatabaseFeature> {
        matches!(expr, Expr::UuidV4).then_some(FoldDatabaseFeature::UuidV4Generation)
    }

    fn drop_column_precondition(&self, _table: &str, _column: &str) -> Option<PreconditionCheck> {
        None
    }

    fn restates_column_type_at_apply(&self) -> bool {
        true
    }

    fn column_type_change_precondition(
        &self,
        _table: &str,
        _column: &str,
    ) -> Option<PreconditionCheck> {
        None
    }

    fn alter_column_refusal(&self, op: &'static str) -> Result<(), IrLowerError> {
        Err(IrLowerError::MysqlAlterColumnUnsupported(op))
    }

    fn partition_collapse_mirror_guard(
        &self,
        table_sql: &str,
        key_sql: &str,
        predicate: &str,
    ) -> Result<String, IrLowerError> {
        Ok(format!(
            "/* zero-migrate: partition collapse populated-default mirror guard */\n\
             SELECT JSON_EXTRACT(CONCAT('!', {key_sql}), '$') \
               FROM {table_sql} WHERE {predicate} LIMIT 1"
        ))
    }

    fn supports_expression_index(&self) -> bool {
        false
    }

    fn author_type_override(&self, ty: &ColType) -> Option<AuthorTypeOverride> {
        match ty {
            ColType::Decimal { precision, scale } => Some(AuthorTypeOverride {
                data_type: "numeric".to_string(),
                ddl_type: Some(format!("DECIMAL({precision}, {scale})")),
                quote_literal_default_as_text: false,
            }),
            _ => None,
        }
    }
}

fn cursor_type_is_character(data_type: &str) -> bool {
    type_is_one_of(
        data_type,
        &[
            "char",
            "varchar",
            "tinytext",
            "text",
            "mediumtext",
            "longtext",
        ],
    )
}

fn type_is_one_of(data_type: &str, candidates: &[&str]) -> bool {
    candidates.iter().any(|candidate| {
        data_type == *candidate
            || data_type
                .strip_prefix(candidate)
                .is_some_and(|rest| rest.starts_with('(') || rest.starts_with(' '))
    })
}

/// Whether a `DEFAULT` clause must wrap this expression in parentheses.
fn default_needs_parens(expr: &Expr) -> bool {
    !matches!(
        expr,
        Expr::Literal { .. }
            | Expr::UuidV4
            | Expr::FnSynth {
                r#fn: SynthFn::Now,
                ..
            }
    )
}

fn render_enum_values(values: &[String]) -> String {
    values
        .iter()
        .map(|v| zero_migrate_backend::dml::mysql_grammar_string_literal(v))
        .collect::<Vec<_>>()
        .join(", ")
}
