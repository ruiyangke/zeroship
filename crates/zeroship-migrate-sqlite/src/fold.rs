use std::collections::BTreeMap;

use zeroship_migrate_backend::error::IrLowerError;
use zeroship_migrate_backend::fold::{
    AuthorTypeOverride, CatalogFoldPolicy, CatalogFoldRefusal, FoldCursorColumnContract,
    FoldCursorComparison, FoldCursorScalarType, FoldDatabaseFeature, ReferenceTextStorage,
    SnapshotProvenanceStrength,
};
use zeroship_migrate_backend::schema::SchemaRenderer;
use zeroship_migrate_backend::snapshot::{
    ColumnSnapshot, PartitionSnapshot, SequenceSnapshot, TableSnapshot, ViewSnapshot,
};
use zeroship_migrate_backend::stored_ddl::StoredDdl;
use zeroship_migrate_ir::expr::Expr;
use zeroship_migrate_ir::ir::ColType;
use zeroship_migrate_ir::precondition::PreconditionCheck;

#[derive(Debug)]
pub(crate) struct SqliteCatalogFoldPolicy;

pub(crate) static POLICY: SqliteCatalogFoldPolicy = SqliteCatalogFoldPolicy;

impl CatalogFoldPolicy for SqliteCatalogFoldPolicy {
    fn snapshot_provenance_strength(
        &self,
        table: &TableSnapshot,
    ) -> Option<SnapshotProvenanceStrength> {
        table
            .stored_create_sql
            .is_some()
            .then_some(SnapshotProvenanceStrength::StoredTableDefinition)
    }

    /// SQLite has no name for a primary key to report. There is no constraint
    /// catalog at all, and the PK's auto-index - when one exists - is named by the
    /// engine (`sqlite_autoindex_*`), which is an internal that must not leak.
    ///
    /// So this backend does not report a name it was given; it CHOOSES one, and the
    /// choice is `<table>_pkey`. The spelling is the one this backend has always
    /// used, moved here from the neutral contract crate unchanged. What is new is
    /// that it is now this backend's stated answer rather than an assumption made on
    /// its behalf, so changing it is a one-line change in this file.
    ///
    /// Known divergence, pre-existing and NOT introduced here: this crate's catalog
    /// reader synthesizes `pk_<table>` for the constraint and emits no index at all
    /// for the primary key, so the folded and introspected sides disagree on both
    /// counts. The engine's differential corpus records that pair as by-design.
    /// Converging them is a change to the reader and to those recorded rows, not to
    /// this answer.
    fn implicit_primary_key_name(&self, table: &str) -> String {
        format!("{table}_pkey")
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

    fn rowid_storage_generates(&self, stored_create_sql: Option<&str>, data_type: &str) -> bool {
        if stored_create_sql.is_some() {
            data_type.trim().eq_ignore_ascii_case("INTEGER")
        } else {
            matches!(
                data_type.trim().to_ascii_lowercase().as_str(),
                "integer" | "int" | "bigint" | "smallint" | "boolean"
            )
        }
    }

    fn stored_table_allows_rowid(&self, stored_create_sql: Option<&str>) -> bool {
        stored_create_sql
            .is_none_or(|stored| !crate::stored_ddl::PARSER.create_is_without_rowid(stored))
    }

    fn stored_primary_key_allows_rowid(
        &self,
        stored_create_sql: Option<&str>,
        column: &str,
    ) -> bool {
        stored_create_sql.is_none_or(|stored| {
            !crate::stored_ddl::PARSER.create_is_without_rowid(stored)
                && !crate::stored_ddl::PARSER.inline_primary_key_is_desc(stored, column)
        })
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

    fn rename_primary_key_after_table_rename(&self, _snapshot: &mut TableSnapshot, _to: &str) {
        // SQLite retains the authored primary-key name across a table rename.
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
        // SQLite rebuilds renames and compares its own affinity spelling; it
        // does not inherit PostgreSQL's built-in alias table.
        crate::schema::RENDERER.canonical_type(ty)
    }

    fn inline_enum_check(
        &self,
        column: &str,
        values: &[String],
    ) -> Result<Option<String>, IrLowerError> {
        let col = zeroship_migrate_backend::dml::quote_ident_for_backend(
            "column",
            column,
            &crate::dml::RENDERER,
        )
        .map_err(IrLowerError::DmlAssemble)?;
        Ok(Some(format!(
            "CHECK ({col} IN ({}))",
            render_enum_values(values)
        )))
    }

    fn inline_enum_type(&self, _values: &[String]) -> Option<String> {
        None
    }

    fn folds_check_constraint_identity(&self) -> bool {
        // SQLite retains CHECK text in stored CREATE DDL, but the current fold
        // scope does not reconcile arbitrary authored CHECK identity.
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

    fn physical_type_inputs_equal(&self, _left: &ColumnSnapshot, _right: &ColumnSnapshot) -> bool {
        // SQLite's finalizer is an explicit no-op, so every answer is safe.
        false
    }

    fn reference_catalog_type<'a>(&self, column: &'a ColumnSnapshot) -> &'a str {
        // SQLite emits every managed integer width as INTEGER, but authored
        // reference validation must still distinguish int/smallInt/bigInt when
        // the other side is an unmanaged declaration retained by PRAGMA. The
        // neutral token is the only non-vendor carrier that preserves that fact.
        if let Some(integer_token) = column
            .type_def
            .as_ref()
            .and_then(|def| def.get("type"))
            .and_then(serde_json::Value::as_str)
            .filter(|ty| matches!(*ty, "smallInt" | "int" | "integer" | "bigInt"))
        {
            return integer_token;
        }
        column
            .ddl_type_override
            .as_deref()
            .unwrap_or(&column.data_type)
    }

    fn canonical_reference_catalog_type(
        &self,
        data_type: &str,
        integer_width_is_logically_proven: bool,
    ) -> String {
        // Reference compatibility must retain the authored integer width.
        // SQLite gives all three spellings INTEGER affinity, but PRAGMA
        // `table_info` preserves an unmanaged target's declared type. Do
        // not let the general drift-affinity canonicalizer make `int` and
        // `bigInt` look interchangeable here. A project-declared target is
        // different: the logical pass has already proved its exact authored
        // width, while this engine deliberately renders every managed integer
        // spelling as SQLite INTEGER. Compare that known physical form without
        // weakening the unmanaged-catalog check.
        let normalized = data_type.trim().to_ascii_lowercase();
        match normalized.as_str() {
            "smallint" | "int2" | "integer" | "int" | "int4" | "bigint" | "int8"
                if integer_width_is_logically_proven =>
            {
                "integer".to_string()
            }
            "smallint" | "int2" => "smallint".to_string(),
            "integer" | "int" | "int4" => "int".to_string(),
            "bigint" | "int8" => "bigint".to_string(),
            _ => crate::schema::RENDERER.canonical_type(data_type),
        }
    }

    fn explicit_reference_text_storage(&self, _ddl_type: &str) -> Option<ReferenceTextStorage> {
        None
    }

    fn catalog_reference_text_storage(
        &self,
        _column: &ColumnSnapshot,
    ) -> Option<ReferenceTextStorage> {
        None
    }

    fn compares_reference_named_collation(&self) -> bool {
        true
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
        let upper = snapshot_database_type.to_ascii_uppercase();
        let scalar_type = if upper.contains("INT") {
            Some(FoldCursorScalarType::Int64)
        } else if ["CHAR", "CLOB", "TEXT"]
            .iter()
            .any(|fragment| upper.contains(fragment))
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
        let database_type = match scalar_type {
            FoldCursorScalarType::Int64 => "integer".to_string(),
            FoldCursorScalarType::String => "text".to_string(),
            FoldCursorScalarType::Decimal => {
                unreachable!("SQLite cursor scalar inference does not admit decimal")
            }
        };
        let comparison = if let Some(collation) = &column.collation {
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

    fn wrap_default_expr(&self, _expr: &Expr, rendered: String) -> String {
        rendered
    }

    fn database_requirement_for_column(
        &self,
        _ty: &ColType,
        _is_reference: bool,
    ) -> Option<FoldDatabaseFeature> {
        None
    }

    fn database_requirement_for_expr(&self, _expr: &Expr) -> Option<FoldDatabaseFeature> {
        None
    }

    fn drop_column_precondition(&self, _table: &str, _column: &str) -> Option<PreconditionCheck> {
        None
    }

    fn restates_column_type_at_apply(&self) -> bool {
        false
    }

    fn column_type_change_precondition(
        &self,
        _table: &str,
        _column: &str,
    ) -> Option<PreconditionCheck> {
        None
    }

    fn alter_column_refusal(&self, _op: &'static str) -> Result<(), IrLowerError> {
        Ok(())
    }

    fn partition_collapse_mirror_guard(
        &self,
        table_sql: &str,
        key_sql: &str,
        predicate: &str,
    ) -> Result<String, IrLowerError> {
        // SQLite can use INSERT...SELECT NULL into the NOT NULL partition key:
        // the constraint is checked only for selected rows. MySQL's guard uses
        // a row-dependent JSON parse instead, because a constant invalid JSON
        // expression can be folded by the optimizer before WHERE filters.
        Ok(format!(
            "/* zero-migrate: partition collapse populated-default mirror guard */\n\
             INSERT INTO {table_sql} ({key_sql}) \
             SELECT NULL FROM {table_sql} WHERE {predicate} LIMIT 1"
        ))
    }

    fn supports_expression_index(&self) -> bool {
        true
    }

    fn author_type_override(&self, ty: &ColType) -> Option<AuthorTypeOverride> {
        match ty {
            ColType::Decimal { .. } => Some(AuthorTypeOverride {
                // SQLite has no fixed-precision decimal storage class. NUMERIC/REAL
                // affinity converts a sufficiently wide decimal string through a
                // binary float, so retain authored decimal text byte-for-byte.
                data_type: "text".to_string(),
                ddl_type: Some("TEXT".to_string()),
                quote_literal_default_as_text: true,
            }),
            _ => None,
        }
    }
}

fn render_enum_values(values: &[String]) -> String {
    values
        .iter()
        .map(|v| zeroship_migrate_backend::dml::sql_string_literal(v))
        .collect::<Vec<_>>()
        .join(", ")
}
