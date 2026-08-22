use zero_migrate_backend::validation::{Disposition, ValidationPolicy, ValidationRefusal};
use zero_migrate_ir::capability::VendorCapability;
use zero_migrate_ir::ir::ColType;

#[derive(Debug)]
pub(crate) struct MysqlValidationPolicy;

pub(crate) static POLICY: MysqlValidationPolicy = MysqlValidationPolicy;

#[rustfmt::skip]
const OP_DISPOSITIONS: &[((&str, &str), Disposition)] = &[
    (("addColumn", "base"), Disposition::Portable),
    (("addColumn", "identity"), Disposition::Unsupported),
    (("addColumn", "nextvalDefault"), Disposition::Unsupported),
    (("addConstraint", "check"), Disposition::Unsupported),
    (("addConstraint", "exclusion"), Disposition::Unsupported),
    (("addConstraint", "fkComposite"), Disposition::Portable),
    (("addConstraint", "fkNoLocalColumn"), Disposition::Unsupported),
    (("addConstraint", "fkNonId"), Disposition::Portable),
    (("addConstraint", "fkNotValid"), Disposition::Unsupported),
    (("addConstraint", "fkSimple"), Disposition::Portable),
    (("addConstraint", "unique"), Disposition::Portable),
    (("alterPrimaryKey", "base"), Disposition::Portable),
    (("alterRole", "base"), Disposition::Unsupported),
    (("alterSequence", "base"), Disposition::Unsupported),
    (("attachPartition", "base"), Disposition::Unsupported),
    (("backfill", "base"), Disposition::Portable),
    (("comment", "base"), Disposition::Unsupported),
    (("createDomain", "base"), Disposition::Portable),
    (("createDomain", "nextvalDefault"), Disposition::Unsupported),
    (("createEnum", "base"), Disposition::Portable),
    (("createExtension", "base"), Disposition::Unsupported),
    (("createFunction", "base"), Disposition::Unsupported),
    (("createIndex", "base"), Disposition::Portable),
    (("createIndex", "exprElement"), Disposition::Unsupported),
    (("createIndex", "partialWhere"), Disposition::Unsupported),
    (("createIndex", "pgOnlyMethodOrFeature"), Disposition::Unsupported),
    (("createPartition", "base"), Disposition::TransparentDegradable),
    (("createPolicy", "base"), Disposition::Unsupported),
    (("createRole", "base"), Disposition::Unsupported),
    (("createRole", "superuserIfNotExists"), Disposition::Unsupported),
    (("createSchema", "base"), Disposition::Unsupported),
    (("createSequence", "base"), Disposition::Unsupported),
    (("createTable", "base"), Disposition::Portable),
    (("createTable", "identityAlways"), Disposition::Unsupported),
    (("createTable", "nextvalDefault"), Disposition::Unsupported),
    (("createTable", "nonportableByDefaultIdentity"), Disposition::Unsupported),
    (("createTable", "partitioned"), Disposition::Unsupported),
    (("createTable", "partitionedCollapse"), Disposition::TransparentDegradable),
    (("createTable", "pgOnlyIndexFeature"), Disposition::Unsupported),
    (("createTrigger", "bodyInsteadOf"), Disposition::Unsupported),
    (("createTrigger", "bodyMultipleEvents"), Disposition::Unsupported),
    (("createTrigger", "bodyRaiseIgnore"), Disposition::Unsupported),
    (("createTrigger", "bodySimple"), Disposition::Portable),
    (("createTrigger", "bodyStatementLevel"), Disposition::Unsupported),
    (("createTrigger", "bodyTruncateEvent"), Disposition::Unsupported),
    (("createTrigger", "bodyWhen"), Disposition::Unsupported),
    (("createTrigger", "executeFunction"), Disposition::Unsupported),
    (("createView", "base"), Disposition::Portable),
    (("createView", "materialized"), Disposition::Unsupported),
    (("createView", "materializedReplace"), Disposition::Unsupported),
    (("delete", "base"), Disposition::Portable),
    (("detachPartition", "base"), Disposition::Unsupported),
    (("dialectal", "base"), Disposition::Portable),
    (("dropColumn", "base"), Disposition::Portable),
    (("dropColumnDefault", "base"), Disposition::Portable),
    (("dropColumnNotNull", "base"), Disposition::Unsupported),
    (("dropConstraint", "base"), Disposition::Portable),
    (("dropDomain", "base"), Disposition::Portable),
    (("dropEnum", "base"), Disposition::Portable),
    (("dropExtension", "base"), Disposition::Unsupported),
    (("dropFunction", "base"), Disposition::Unsupported),
    (("dropIndex", "base"), Disposition::Portable),
    (("dropOwnedBy", "base"), Disposition::Unsupported),
    (("dropPartition", "base"), Disposition::Portable),
    (("dropPolicy", "base"), Disposition::Unsupported),
    (("dropRole", "base"), Disposition::Unsupported),
    (("dropSchema", "base"), Disposition::Unsupported),
    (("dropSequence", "base"), Disposition::Unsupported),
    (("dropTable", "base"), Disposition::Portable),
    (("dropTrigger", "base"), Disposition::Portable),
    (("dropView", "base"), Disposition::Portable),
    (("dropView", "materialized"), Disposition::Unsupported),
    (("grant", "base"), Disposition::Unsupported),
    (("insert", "base"), Disposition::Portable),
    (("insert", "onConflictDoNothing"), Disposition::Unsupported),
    (("insert", "onConflictDoUpdate"), Disposition::Portable),
    (("pgRaw", "base"), Disposition::Unsupported),
    (("renameColumn", "base"), Disposition::Unsupported),
    (("renameColumn", "existenceGuard"), Disposition::Unsupported),
    (("renameTable", "base"), Disposition::Portable),
    (("revoke", "base"), Disposition::Unsupported),
    (("setColumnDefault", "base"), Disposition::Portable),
    (("setColumnDefault", "containerOrJson"), Disposition::Portable),
    (("setColumnDefault", "nextval"), Disposition::Unsupported),
    (("setColumnNotNull", "base"), Disposition::Unsupported),
    (("setColumnType", "base"), Disposition::Portable),
    (("setColumnType", "using"), Disposition::Unsupported),
    (("setRls", "base"), Disposition::Unsupported),
    (("setTableOptions", "base"), Disposition::Portable),
    (("synchronizeIdentity", "base"), Disposition::Portable),
    (("update", "base"), Disposition::Portable),
    (("validateConstraint", "base"), Disposition::Unsupported),
];

fn refusal(reason: String, suggested_fix: String) -> ValidationRefusal {
    ValidationRefusal {
        reason,
        suggested_fix,
    }
}

impl ValidationPolicy for MysqlValidationPolicy {
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
        identifier.to_ascii_lowercase()
    }

    fn catalog_proves_uuid_format(
        &self,
        native_uuid: bool,
        catalog_uuid_format_check: bool,
    ) -> bool {
        native_uuid || catalog_uuid_format_check
    }

    fn lowered_reference_storage(&self, ty: &ColType) -> String {
        match ty {
            ColType::String { .. } | ColType::Text | ColType::Ref { .. } => "text".to_string(),
            ColType::SmallInt => "smallint".to_string(),
            ColType::Int => "integer".to_string(),
            ColType::BigInt => "bigint".to_string(),
            ColType::Double => "double".to_string(),
            ColType::Real => "real".to_string(),
            ColType::Boolean => "boolean".to_string(),
            ColType::Json => "json".to_string(),
            ColType::Timestamp => "datetime".to_string(),
            ColType::Date => "date".to_string(),
            ColType::Uuid | ColType::Inet | ColType::TextArray => "text".to_string(),
            ColType::Bytes | ColType::Encrypted { .. } => "blob".to_string(),
            ColType::Char { length } => format!("char({length})"),
            ColType::Vector { vector } => format!("vector({vector})"),
            ColType::GeoPoint => "text".to_string(),
            ColType::Decimal { precision, scale } => format!("decimal({precision},{scale})"),
            ColType::Enum { name, .. } => format!("enum:{name}"),
            ColType::Domain { name, .. } => format!("domain:{name}"),
        }
    }

    fn tracks_constraint_names(&self) -> bool {
        true
    }

    fn tracks_relation_type_namespace(&self) -> bool {
        false
    }

    fn supports_native_partitioning(&self) -> bool {
        false
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
            | VendorCapability::RawSql
            | VendorCapability::RawViewBody
            | VendorCapability::MaterializedView => None,
        }
    }

    fn deferrable_foreign_key_refusal(&self) -> ValidationRefusal {
        refusal(
            "MySQL does not support deferrable foreign-key constraints".to_string(),
            "omit deferrable/initiallyDeferred for MySQL, or use a dialectal PostgreSQL/SQLite leg"
                .to_string(),
        )
    }

    fn inline_trigger_body_refusal(&self) -> ValidationRefusal {
        refusal(
            "MySQL triggers do not accept the closed inline body form".to_string(),
            "use a trigger action supported by this backend".to_string(),
        )
    }

    fn trigger_event_count_refusal(&self, event_count: usize) -> Option<ValidationRefusal> {
        (event_count > 1).then(|| {
            refusal(
                "MySQL CREATE TRIGGER accepts exactly one trigger event".to_string(),
                "split this into one trigger per event when targeting MySQL".to_string(),
            )
        })
    }

    fn sequence_default_refusal(&self, position: &str) -> ValidationRefusal {
        refusal(
            format!(
                "{position} declares a nextval sequence default, but standalone sequences and nextval defaults are PostgreSQL-only"
            ),
            "target PostgreSQL, use an identity/auto-increment shape for this dialect, or remove `.default(nextval(...))`".to_string(),
        )
    }

    fn virtual_generated_column_refusal(&self, column: &str) -> ValidationRefusal {
        refusal(
            format!(
                "column {column:?} requests a VIRTUAL generated column, but backend \"mysql\" does not support virtual generated columns"
            ),
            "use a stored generated column or a backend-specific dialectal leg".to_string(),
        )
    }

    fn identity_placement_refusal(
        &self,
        column: &str,
        always: bool,
        primary_key_columns: Option<&[String]>,
        is_add_column: bool,
    ) -> Option<ValidationRefusal> {
        let fix = "use identity only on the sole integer primary key for this dialect, or remove `.identity(...)`".to_string();
        if always {
            return Some(refusal(
                "identity({ always: true }) is PostgreSQL-only; SQLite/MySQL support only identity({ always: false }) / autoIncrement() on the sole integer primary key".to_string(),
                fix,
            ));
        }
        if is_add_column {
            return Some(refusal(
                "autoIncrement identity: non-PK identity has no sound target-dialect render; SQLite AUTOINCREMENT and MySQL AUTO_INCREMENT are only sound on the sole integer primary key".to_string(),
                fix,
            ));
        }
        let Some(primary_key_columns) = primary_key_columns else {
            return Some(refusal(
                format!(
                    "autoIncrement identity: column {column:?} is not the declared primary key; non-PK identity has no sound target-dialect render"
                ),
                fix,
            ));
        };
        if primary_key_columns.len() == 1 && primary_key_columns[0] == column {
            return None;
        }
        Some(refusal(
            format!(
                "autoIncrement identity: column {column:?} is part of {primary_key_columns:?}, but this dialect's identity is only sound for the sole integer primary key"
            ),
            fix,
        ))
    }
}
