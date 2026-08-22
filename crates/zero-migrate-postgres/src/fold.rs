use std::collections::BTreeMap;

use zero_migrate_backend::error::IrLowerError;
use zero_migrate_backend::fold::{
    AuthorTypeOverride, CatalogFoldPolicy, CatalogFoldRefusal, FoldCursorColumnContract,
    FoldCursorComparison, FoldCursorScalarType, FoldDatabaseFeature, ReferenceTextStorage,
    SnapshotProvenanceStrength,
};
use zero_migrate_backend::snapshot::{
    ColumnSnapshot, IndexElementSnapshot, PartitionSnapshot, SequenceSnapshot, TableSnapshot,
    ViewSnapshot,
};
use zero_migrate_ir::expr::Expr;
use zero_migrate_ir::ir::{ColType, ValueFormat};
use zero_migrate_ir::precondition::{Precondition, PreconditionCheck};

#[derive(Debug)]
pub(crate) struct PostgresCatalogFoldPolicy;

pub(crate) static POLICY: PostgresCatalogFoldPolicy = PostgresCatalogFoldPolicy;

impl CatalogFoldPolicy for PostgresCatalogFoldPolicy {
    fn snapshot_provenance_strength(
        &self,
        table: &TableSnapshot,
    ) -> Option<SnapshotProvenanceStrength> {
        table
            .columns
            .iter()
            .any(|column| column.ddl_type_override.is_some())
            .then_some(SnapshotProvenanceStrength::TypeOverride)
    }

    fn allocate_implicit_relation_name(
        &self,
        default_name: &str,
        tables: &BTreeMap<String, TableSnapshot>,
        partitions: &BTreeMap<String, PartitionSnapshot>,
        views: &BTreeMap<String, ViewSnapshot>,
        sequences: &BTreeMap<String, SequenceSnapshot>,
    ) -> String {
        let name_is_taken = |candidate: &str| {
            tables.contains_key(candidate)
                || partitions.contains_key(candidate)
                || views.contains_key(candidate)
                || sequences.contains_key(candidate)
                || tables
                    .values()
                    .any(|snapshot| snapshot.indexes.iter().any(|index| index.name == candidate))
        };
        let relation_count = tables.len()
            + partitions.len()
            + views.len()
            + sequences.len()
            + tables
                .values()
                .map(|snapshot| snapshot.indexes.len())
                .sum::<usize>();
        (0..=relation_count)
            .map(|suffix| {
                if suffix == 0 {
                    default_name.to_string()
                } else {
                    format!("{default_name}{suffix}")
                }
            })
            .find(|candidate| !name_is_taken(candidate))
            .expect(
                "one more implicit relation-name candidate than relations must leave a free name",
            )
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
        target_columns.is_some_and(|target| target.iter().any(|candidate| candidate == column))
    }

    fn reusable_primary_index(
        &self,
        snap: &TableSnapshot,
        columns: &[String],
        current_primary_key_name: Option<&str>,
    ) -> Option<String> {
        snap.indexes
            .iter()
            .find(|index| {
                let constraint_owned = snap.constraints.iter().any(|constraint| {
                    constraint.name == index.name
                        && matches!(
                            constraint.kind.as_str(),
                            "PRIMARY KEY" | "UNIQUE" | "EXCLUDE"
                        )
                });
                index.name != current_primary_key_name.unwrap_or_default()
                    && !constraint_owned
                    && index.unique
                    && index.columns == columns
                    && index.access_method == "btree"
                    && index.predicate.is_none()
                    && index.include.is_empty()
                    && !index.only
                    && index.elements.len() == columns.len()
                    && index.elements.iter().all(|element| {
                        matches!(
                            element,
                            IndexElementSnapshot::Column {
                                order: None | Some(zero_migrate_ir::ir::IndexSortOrder::Asc),
                                opclass: None,
                                collation: None,
                                ..
                            }
                        )
                    })
            })
            .map(|index| index.name.clone())
    }

    fn rename_primary_key_after_table_rename(&self, _snapshot: &mut TableSnapshot, _to: &str) {
        // PostgreSQL stores a primary-key constraint name independently of its table.
    }

    fn is_native_uuid_type(&self, data_type: &str) -> bool {
        data_type.eq_ignore_ascii_case("uuid")
    }

    fn materialized_named_type_metadata(
        &self,
        ty: &ColType,
        default_schema: &str,
    ) -> Result<Option<(String, String)>, IrLowerError> {
        let (name, schema) = match ty {
            ColType::Enum { name, schema } | ColType::Domain { name, schema } => {
                (name, schema.as_deref().unwrap_or(default_schema))
            }
            _ => return Ok(None),
        };
        Ok(Some((
            pg_type_data_type(schema, name),
            pg_type_qname(schema, name)?,
        )))
    }

    fn canonical_rename_type_spelling(&self, ty: &str) -> String {
        let compact: String = ty
            .chars()
            .filter(|character| !character.is_ascii_whitespace())
            .flat_map(char::to_lowercase)
            .collect();
        const ALIASES: &[(&str, &str)] = &[
            ("timestampwithtimezone", "timestamptz"),
            ("timestampwithouttimezone", "timestamp"),
            ("timewithtimezone", "timetz"),
            ("timewithouttimezone", "time"),
            ("charactervarying", "varchar"),
            ("character", "char"),
            ("doubleprecision", "float8"),
            ("decimal", "numeric"),
            ("smallserial", "smallint"),
            ("bigserial", "bigint"),
            ("serial", "integer"),
            ("int2", "smallint"),
            ("int4", "integer"),
            ("int8", "bigint"),
            ("int", "integer"),
            ("bool", "boolean"),
            ("float4", "real"),
        ];
        for (alias, canonical) in ALIASES {
            let Some(suffix) = compact.strip_prefix(alias) else {
                continue;
            };
            if suffix.is_empty() || suffix.starts_with('(') || suffix.starts_with('[') {
                return format!("{canonical}{suffix}");
            }
        }
        compact
    }

    fn inline_enum_check(
        &self,
        _column: &str,
        _values: &[String],
    ) -> Result<Option<String>, IrLowerError> {
        Ok(None)
    }

    fn inline_enum_type(&self, _values: &[String]) -> Option<String> {
        None
    }

    fn folds_check_constraint_identity(&self) -> bool {
        true
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
        // PostgreSQL's finalizer is an explicit no-op, so every answer is safe.
        false
    }

    fn reference_catalog_type<'a>(&self, column: &'a ColumnSnapshot) -> &'a str {
        &column.data_type
    }

    fn canonical_reference_catalog_type(
        &self,
        data_type: &str,
        _integer_width_is_logically_proven: bool,
    ) -> String {
        data_type.trim().to_ascii_lowercase()
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
        let scalar_type = if type_is_one_of(
            &snapshot_database_type,
            &["smallint", "integer", "bigint", "int2", "int4", "int8"],
        ) {
            Some(FoldCursorScalarType::Int64)
        } else if type_is_one_of(&snapshot_database_type, &["numeric", "decimal"]) {
            Some(FoldCursorScalarType::Decimal)
        } else if type_is_one_of(
            &snapshot_database_type,
            &[
                "text",
                "citext",
                "character",
                "character varying",
                "char",
                "varchar",
                "uuid",
                "date",
                "time",
                "time without time zone",
                "time with time zone",
                "timestamp",
                "timestamp without time zone",
                "timestamp with time zone",
                "timestamptz",
            ],
        ) {
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
        let database_type = snapshot_database_type;
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

    fn database_requirement_for_value_format(
        &self,
        _value_format: &ValueFormat,
    ) -> Option<FoldDatabaseFeature> {
        None
    }

    fn database_requirement_for_expr(&self, expr: &Expr) -> Option<FoldDatabaseFeature> {
        match expr {
            Expr::UuidV4 => Some(FoldDatabaseFeature::UuidV4Generation),
            Expr::UuidV7 => Some(FoldDatabaseFeature::UuidV7Generation),
            _ => None,
        }
    }

    fn drop_column_precondition(&self, table: &str, column: &str) -> Option<PreconditionCheck> {
        Some(PreconditionCheck::halt(
            Precondition::ColumnHasNoBlockingDependents {
                table: table.to_string(),
                column: column.to_string(),
            },
        ))
    }

    fn restates_column_type_at_apply(&self) -> bool {
        false
    }

    fn column_type_change_precondition(
        &self,
        table: &str,
        column: &str,
    ) -> Option<PreconditionCheck> {
        Some(PreconditionCheck::halt(
            Precondition::ColumnTypeChangeHasNoBlockers {
                table: table.to_string(),
                column: column.to_string(),
            },
        ))
    }

    fn alter_column_refusal(&self, _op: &'static str) -> Result<(), IrLowerError> {
        Ok(())
    }

    fn partition_collapse_mirror_guard(
        &self,
        _table_sql: &str,
        _key_sql: &str,
        _predicate: &str,
    ) -> Result<String, IrLowerError> {
        Err(IrLowerError::UnsupportedOp(
            "partition collapse mirror guard is only for SQLite/MySQL",
        ))
    }

    fn supports_expression_index(&self) -> bool {
        true
    }

    fn author_type_override(&self, ty: &ColType) -> Option<AuthorTypeOverride> {
        match ty {
            ColType::Uuid => Some(AuthorTypeOverride {
                data_type: "uuid".to_string(),
                ddl_type: None,
                quote_literal_default_as_text: false,
            }),
            ColType::Decimal { precision, scale } => Some(AuthorTypeOverride {
                data_type: "numeric".to_string(),
                ddl_type: Some(format!("numeric({precision}, {scale})")),
                quote_literal_default_as_text: false,
            }),
            _ => None,
        }
    }
}

fn type_is_one_of(data_type: &str, candidates: &[&str]) -> bool {
    candidates.iter().any(|candidate| {
        data_type == *candidate
            || data_type
                .strip_prefix(candidate)
                .is_some_and(|rest| rest.starts_with('(') || rest.starts_with(' '))
    })
}

fn quote_engine_ident(what: &'static str, ident: &str) -> Result<String, IrLowerError> {
    zero_migrate_backend::dml::quote_ident_for_backend(what, ident, &crate::dml::RENDERER)
        .map_err(IrLowerError::DmlAssemble)
}

fn pg_type_qname(schema: &str, name: &str) -> Result<String, IrLowerError> {
    Ok(format!(
        "{}.{}",
        quote_engine_ident("schema", schema)?,
        quote_engine_ident("type", name)?
    ))
}

fn pg_type_data_type(schema: &str, name: &str) -> String {
    format!("{schema}.{name}")
}
