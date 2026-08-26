use crate::guard::{check_raw_view_body, RawViewBodyDefect};
use zero_migrate_backend::validation::{Disposition, ValidationPolicy, ValidationRefusal};
use zero_migrate_ir::capability::VendorCapability;
use zero_migrate_ir::ir::ColType;
use zero_migrate_ir::policy::SchemaScope;

#[derive(Debug)]
pub(crate) struct PostgresValidationPolicy;

pub(crate) static POLICY: PostgresValidationPolicy = PostgresValidationPolicy;

#[rustfmt::skip]
const OP_DISPOSITIONS: &[((&str, &str), Disposition)] = &[
    (("addColumn", "base"), Disposition::Portable),
    (("addColumn", "identity"), Disposition::Portable),
    (("addColumn", "nextvalDefault"), Disposition::Portable),
    (("addConstraint", "check"), Disposition::Portable),
    (("addConstraint", "exclusion"), Disposition::Portable),
    (("addConstraint", "fkComposite"), Disposition::Portable),
    (("addConstraint", "fkNoLocalColumn"), Disposition::Unsupported),
    (("addConstraint", "fkNonId"), Disposition::Portable),
    (("addConstraint", "fkNotValid"), Disposition::Portable),
    (("addConstraint", "fkSimple"), Disposition::Portable),
    (("addConstraint", "unique"), Disposition::Portable),
    (("alterPrimaryKey", "base"), Disposition::Portable),
    (("alterRole", "base"), Disposition::Vendor),
    (("alterSequence", "base"), Disposition::Portable),
    (("attachPartition", "base"), Disposition::Vendor),
    (("backfill", "base"), Disposition::Portable),
    (("comment", "base"), Disposition::Portable),
    (("createDomain", "base"), Disposition::Portable),
    (("createDomain", "nextvalDefault"), Disposition::Portable),
    (("createEnum", "base"), Disposition::Portable),
    (("createExtension", "base"), Disposition::Vendor),
    (("createFunction", "base"), Disposition::Vendor),
    (("createIndex", "base"), Disposition::Portable),
    (("createIndex", "exprElement"), Disposition::Portable),
    (("createIndex", "partialWhere"), Disposition::Portable),
    (("createIndex", "pgOnlyMethodOrFeature"), Disposition::Portable),
    (("createPartition", "base"), Disposition::Portable),
    (("createPolicy", "base"), Disposition::Vendor),
    (("createRole", "base"), Disposition::Vendor),
    (("createRole", "superuserIfNotExists"), Disposition::Unsupported),
    (("createSchema", "base"), Disposition::Vendor),
    (("createSequence", "base"), Disposition::Portable),
    (("createTable", "base"), Disposition::Portable),
    (("createTable", "identityAlways"), Disposition::Portable),
    (("createTable", "nextvalDefault"), Disposition::Portable),
    (("createTable", "nonportableByDefaultIdentity"), Disposition::Portable),
    (("createTable", "partitioned"), Disposition::Portable),
    (("createTable", "partitionedCollapse"), Disposition::Portable),
    (("createTable", "pgOnlyIndexFeature"), Disposition::Portable),
    (("createTrigger", "bodyInsteadOf"), Disposition::Unsupported),
    (("createTrigger", "bodyMultipleEvents"), Disposition::Unsupported),
    (("createTrigger", "bodyRaiseIgnore"), Disposition::Unsupported),
    (("createTrigger", "bodySimple"), Disposition::Unsupported),
    (("createTrigger", "bodyStatementLevel"), Disposition::Unsupported),
    (("createTrigger", "bodyTruncateEvent"), Disposition::Unsupported),
    (("createTrigger", "bodyWhen"), Disposition::Unsupported),
    (("createTrigger", "executeFunction"), Disposition::Portable),
    (("createView", "base"), Disposition::Portable),
    (("createView", "materialized"), Disposition::Vendor),
    (("createView", "materializedReplace"), Disposition::Unsupported),
    (("delete", "base"), Disposition::Portable),
    (("detachPartition", "base"), Disposition::Portable),
    (("dialectal", "base"), Disposition::Portable),
    (("dropColumn", "base"), Disposition::Portable),
    (("dropColumnDefault", "base"), Disposition::Portable),
    (("dropColumnNotNull", "base"), Disposition::Portable),
    (("dropConstraint", "base"), Disposition::Portable),
    (("dropDomain", "base"), Disposition::Portable),
    (("dropEnum", "base"), Disposition::Portable),
    (("dropExtension", "base"), Disposition::Vendor),
    (("dropFunction", "base"), Disposition::Vendor),
    (("dropIndex", "base"), Disposition::Portable),
    (("dropOwnedBy", "base"), Disposition::Vendor),
    (("dropPartition", "base"), Disposition::Portable),
    (("dropPolicy", "base"), Disposition::Vendor),
    (("dropRole", "base"), Disposition::Vendor),
    (("dropSchema", "base"), Disposition::Vendor),
    (("dropSequence", "base"), Disposition::Portable),
    (("dropTable", "base"), Disposition::Portable),
    (("dropTrigger", "base"), Disposition::Portable),
    (("dropView", "base"), Disposition::Portable),
    (("dropView", "materialized"), Disposition::Vendor),
    (("grant", "base"), Disposition::Vendor),
    (("insert", "base"), Disposition::Portable),
    (("insert", "onConflictDoNothing"), Disposition::Portable),
    (("insert", "onConflictDoUpdate"), Disposition::Portable),
    (("raw", "base"), Disposition::Vendor),
    (("renameColumn", "base"), Disposition::Portable),
    (("renameColumn", "existenceGuard"), Disposition::Unsupported),
    (("renameTable", "base"), Disposition::Portable),
    (("revoke", "base"), Disposition::Vendor),
    (("setColumnDefault", "base"), Disposition::Portable),
    (("setColumnDefault", "containerOrJson"), Disposition::Portable),
    (("setColumnDefault", "nextval"), Disposition::Portable),
    (("setColumnNotNull", "base"), Disposition::Portable),
    (("setColumnType", "base"), Disposition::Portable),
    (("setColumnType", "using"), Disposition::Unsupported),
    (("setRls", "base"), Disposition::Vendor),
    (("setTableOptions", "base"), Disposition::Portable),
    (("synchronizeIdentity", "base"), Disposition::Portable),
    (("update", "base"), Disposition::Portable),
    (("validateConstraint", "base"), Disposition::Portable),
];

fn refusal(reason: String, suggested_fix: String) -> ValidationRefusal {
    ValidationRefusal {
        reason,
        suggested_fix,
    }
}

impl ValidationPolicy for PostgresValidationPolicy {
    fn op_disposition(&self, kind: &str, variant: &str) -> Disposition {
        OP_DISPOSITIONS
            .binary_search_by(|((row_kind, row_variant), _)| {
                (*row_kind, *row_variant).cmp(&(kind, variant))
            })
            .ok()
            .map(|index| OP_DISPOSITIONS[index].1)
            .unwrap_or(Disposition::Unsupported)
    }

    fn canonical_identifier(&self, identifier: &str) -> String {
        identifier.to_string()
    }

    fn catalog_proves_uuid_format(
        &self,
        native_uuid: bool,
        _catalog_uuid_format_check: bool,
    ) -> bool {
        native_uuid
    }

    fn lowered_reference_storage(&self, ty: &ColType) -> String {
        match ty {
            ColType::String { .. } | ColType::Text | ColType::Ref { .. } => "text".to_string(),
            ColType::SmallInt => "smallint".to_string(),
            ColType::Int => "integer".to_string(),
            ColType::BigInt => "bigint".to_string(),
            ColType::Double => "double precision".to_string(),
            ColType::Real => "real".to_string(),
            ColType::Boolean => "boolean".to_string(),
            ColType::Json => "jsonb".to_string(),
            ColType::Timestamp => "timestamp with time zone".to_string(),
            ColType::Date => "date".to_string(),
            ColType::Uuid => "uuid".to_string(),
            ColType::Inet => "inet".to_string(),
            ColType::TextArray => "text[]".to_string(),
            ColType::Bytes | ColType::Encrypted { .. } => "bytea".to_string(),
            ColType::Char { length } => format!("char({length})"),
            ColType::Vector { vector } => format!("vector({vector})"),
            ColType::GeoPoint => "geography(point,4326)".to_string(),
            ColType::Decimal { precision, scale } => format!("numeric({precision},{scale})"),
            ColType::Enum { name, schema } | ColType::Domain { name, schema } => schema
                .as_deref()
                .map_or_else(|| name.clone(), |schema| format!("{schema}.{name}")),
        }
    }

    fn tracks_constraint_names(&self) -> bool {
        true
    }

    fn tracks_relation_type_namespace(&self) -> bool {
        true
    }

    fn vendor_capability_refusal(&self, capability: VendorCapability) -> Option<ValidationRefusal> {
        match capability {
            VendorCapability::Extension
            | VendorCapability::Schema
            | VendorCapability::Role
            | VendorCapability::Grant
            | VendorCapability::Rls
            | VendorCapability::Partition
            | VendorCapability::Policy
            | VendorCapability::Function
            | VendorCapability::Trigger
            | VendorCapability::RawSql
            | VendorCapability::RawViewBody
            | VendorCapability::MaterializedView => None,
        }
    }

    fn deferrable_foreign_key_refusal(&self) -> ValidationRefusal {
        refusal(
            "backend \"postgres\" does not support deferrable foreign-key constraints".to_string(),
            "omit deferrable/initiallyDeferred or provide a backend-specific dialectal leg"
                .to_string(),
        )
    }

    fn inline_trigger_body_refusal(&self) -> ValidationRefusal {
        refusal(
            "Postgres triggers must execute a named trigger function; the closed inline body form renders only on SQLite".to_string(),
            "use action: { kind: \"executeFunction\", name: \"...\" } and create the trigger function separately".to_string(),
        )
    }

    fn trigger_event_count_refusal(&self, _event_count: usize) -> Option<ValidationRefusal> {
        None
    }

    fn sequence_default_refusal(&self, position: &str) -> ValidationRefusal {
        refusal(
            format!(
                "{position} declares a nextval sequence default, but backend \"postgres\" does not support standalone sequences"
            ),
            "use a backend-supported identity/default shape or remove `.default(nextval(...))`"
                .to_string(),
        )
    }

    fn virtual_generated_column_refusal(&self, column: &str) -> ValidationRefusal {
        refusal(
            format!(
                "column {column:?} requests a VIRTUAL generated column, but Postgres supports generated columns only as STORED"
            ),
            "use `.generated(expr)` / `{ virtual: false }` for Postgres, or target SQLite"
                .to_string(),
        )
    }

    fn identity_placement_refusal(
        &self,
        _column: &str,
        _always: bool,
        _primary_key_columns: Option<&[String]>,
        _is_add_column: bool,
    ) -> Option<ValidationRefusal> {
        None
    }

    /// PostgreSQL keeps the full gate: `libpg_query` proves the body is exactly one
    /// top-level `SELECT` with no `INTO`, then the read-only body scanner runs the
    /// deny-list over it.
    ///
    /// The five messages below are the ones the engine's `validate_raw_view_body_sql`
    /// emitted inline before this seam existed, moved verbatim. They live here now
    /// because they are all PARSER-derived facts, and the parser is this vendor's.
    fn raw_view_body_refusal(
        &self,
        sql: &str,
        scope: Option<&SchemaScope>,
    ) -> Option<ValidationRefusal> {
        let defect = check_raw_view_body(sql, scope).err()?;
        Some(match defect {
            RawViewBodyDefect::Unparseable(error) => refusal(
                format!("raw viewBody SQL must parse as exactly one top-level SELECT: {error}"),
                "rewrite the view body as a single SELECT, or use the structured SelectAst builder"
                    .to_string(),
            ),
            RawViewBodyDefect::NotExactlyOneStatement(count) => refusal(
                format!(
                    "raw viewBody SQL must contain exactly one top-level SELECT statement; parsed {count} statements"
                ),
                "remove semicolon-chained statements from the view body".to_string(),
            ),
            RawViewBodyDefect::NotASelect => refusal(
                "raw viewBody SQL must be a single top-level SELECT; DDL, DML, COPY, and utility statements are refused".to_string(),
                "rewrite the view body as a SELECT, or use the structured SelectAst builder"
                    .to_string(),
            ),
            RawViewBodyDefect::SelectInto => refusal(
                "raw viewBody SQL uses SELECT INTO, which creates a table and is not a read-only view body".to_string(),
                "drop the INTO clause; a view body must be read-only".to_string(),
            ),
            RawViewBodyDefect::BodyScanner(error) => refusal(
                format!("raw viewBody SQL failed the read-only body scanner: {error}"),
                "remove host/file/network/dynamic-SQL escape tokens from the view body".to_string(),
            ),
        })
    }
}
