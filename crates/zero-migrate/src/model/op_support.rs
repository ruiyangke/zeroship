//! Engine-side dialect-support + vendor-capability computation for the closed
//! [`Op`](crate::model::ir::Op) wire type.
//!
//! These were inherent methods on `Op` in `model/ir.rs`. When the wire contract
//! was extracted into the `zero-migrate-ir` leaf crate, the
//! dialect-support / vendor-capability logic could NOT ride along: it reads the
//! engine-owned portability tables ([`crate::model::support`],
//! [`crate::model::dialect_table`], [`crate::model::capability`]) and the engine's
//! authoring error codes ([`crate::model::validate`]). Those are dialect/policy
//! facts of THIS engine, not wire data, so they stay engine-side and reach the
//! foreign `Op` type through free functions taking `&Op`.
//!
//! Call sites use `op_support::support(op)` / `op_support::vendor_capabilities(op)`
//! / `op_support::op_variant(op)` exactly where they previously wrote
//! `op.support()` etc.
#![allow(clippy::too_many_lines, clippy::match_same_arms)]

use crate::model::expr::Expr;
use crate::model::ir::{
    index_has_element_opclass_or_collation, ForEach, IndexElement, IndexMethod, IndexStorageParams,
    IrColumn, IrConstraintKind, IrDefault, IrIndex, Op, PartitionSpec, RaiseLevel, TriggerAction,
    TriggerEvent, TriggerStmt, TriggerTiming, ViewQuery,
};

    pub fn is_vendor(op: &Op) -> bool {
        !vendor_capabilities(op).is_empty()
    }

    /// Static dialect support declaration for this concrete op shape.
    ///
    /// This is the support matrix the authoring validator consumes for dialect
    /// and feature refusals before lowering.
    #[must_use]
    pub fn support(op: &Op) -> crate::model::support::Support {
        use crate::model::support::Support;

        let (kind, variant) = op_kind_and_variant(op);
        let row = crate::model::dialect_table::lookup(kind, variant).unwrap_or_else(|| {
            panic!("dialect table has no row for op {kind}/{variant}")
        });
        let dialects = crate::model::support::DialectSupport::new(
            support_cell(op, row.postgres, crate::model::support::Dialect::Postgres, variant),
            support_cell(op, row.sqlite, crate::model::support::Dialect::Sqlite, variant),
            support_cell(op, row.mysql, crate::model::support::Dialect::Mysql, variant),
        );
        Support::new(support_tier(op), dialects, support_features(op))
    }

    /// The single per-op reason used where a supported cell has no refusal — it is
    /// never surfaced (`support_cell` only asks for a reason on an `Unsupported`
    /// disposition), but a non-empty placeholder keeps the well-formed invariant.
    const NEVER_REFUSED: &'static str = "internal: supported cell has no refusal reason";
    const NEXTVAL_PG_ONLY: &'static str =
        "nextval sequence defaults are PostgreSQL-only; SQLite/MySQL have no standalone sequences";
    const IDENTITY_ALWAYS_PG_ONLY: &'static str =
        "identity({ always: true }) is PostgreSQL-only; SQLite/MySQL support only \
         identity({ always: false }) / autoIncrement() on the sole integer primary key";
    const BY_DEFAULT_IDENTITY_SINGLE_PK: &'static str =
        "identity({ always: false }) / autoIncrement() must be the sole primary-key \
         column on SQLite/MySQL";

    /// Assemble one per-dialect [`SupportDecision`] from the generated dialect
    /// table's disposition for this (op-kind, variant): an `Unsupported`
    /// disposition becomes the engine-internal refusal reason; any supported
    /// disposition (portable / vendor / transparent-degradable) becomes the op's
    /// render mode on that dialect. The supported/unsupported/vendor GATE is thus
    /// single-sourced from the generated table (`dialect_table.rs`, emitted from
    /// the human-reviewed `dialect-support.toml`); only the render strategy and
    /// diagnostic wording — which are NOT dialect truth — remain in Rust.
    fn support_cell(
        op: &Op,
        disposition: crate::model::dialect_table::Disposition,
        dialect: crate::model::support::Dialect,
        variant: &'static str,
    ) -> crate::model::support::SupportDecision {
        use crate::model::dialect_table::Disposition;
        use crate::model::support::{supported, unsupported};
        match disposition {
            Disposition::Unsupported => unsupported(
                crate::model::validate::CODE_UNSUPPORTED,
                unsupported_reason(op, dialect, variant),
            ),
            Disposition::Portable
            | Disposition::Vendor
            | Disposition::TransparentDegradable => supported(render_mode(op, dialect, variant)),
        }
    }

    /// The render mode a SUPPORTED cell of this op reports on `dialect`. This is a
    /// render-strategy detail of the current engine (does the op lower fully
    /// offline, or only once the live schema is resolved), NOT a dialect-support
    /// decision — so it is derived here, not from the generated dialect table.
    fn render_mode(
        op: &Op,
        dialect: crate::model::support::Dialect,
        variant: &'static str,
    ) -> crate::model::support::RenderMode {
        use crate::model::support::{Dialect, RenderMode};
        let sqlite_live = matches!(dialect, Dialect::Sqlite);
        match op {
            // Backfill and column-rename are live-rendered on every supported
            // dialect (they need the resolved live schema).
            Op::Backfill { .. } | Op::RenameColumn { .. } => RenderMode::LiveResolved,
            // The wrapper itself is expanded before per-op lower; the selected
            // inner ops report their own render modes.
            Op::Dialectal { .. } => RenderMode::Offline,
            // Container / JSON column defaults are live-resolved on every dialect.
            Op::SetColumnDefault { .. } if variant == "containerOrJson" => RenderMode::LiveResolved,
            // ALTER-shaped column/constraint ops are live-resolved only on SQLite
            // (which rebuilds the table from the live schema); offline elsewhere.
            Op::SetColumnType { .. }
            | Op::SetColumnNotNull { .. }
            | Op::DropColumnNotNull { .. }
            | Op::DropColumnDefault { .. }
            | Op::DropConstraint { .. }
            | Op::ValidateConstraint { .. }
            | Op::AddConstraint { .. }
            | Op::SetColumnDefault { .. } => {
                if sqlite_live {
                    RenderMode::LiveResolved
                } else {
                    RenderMode::Offline
                }
            }
            _ => RenderMode::Offline,
        }
    }

    /// The refusal reason an UNSUPPORTED cell of this op reports on `dialect`. Only
    /// consulted by `support_cell` where the generated table records an
    /// `Unsupported` disposition, so the fully-portable ops never reach it. The
    /// reason wording (and, for `createIndex` / `createTrigger`, its per-dialect
    /// divergence on combined payloads) mirrors the previous hand-written arms
    /// exactly, preserving the author-facing diagnostics.
    fn unsupported_reason(
        op: &Op,
        dialect: crate::model::support::Dialect,
        variant: &'static str,
    ) -> &'static str {
        use crate::model::support::Dialect;
        match op {
            Op::CreateTable { .. } => match variant {
                "partitioned" => "partitioned tables are PostgreSQL-only",
                "pgOnlyIndexFeature" => {
                    "createTable BRIN/INCLUDE/WITH/ONLY index features are PostgreSQL-only"
                }
                "nextvalDefault" => NEXTVAL_PG_ONLY,
                "identityAlways" => IDENTITY_ALWAYS_PG_ONLY,
                "nonportableByDefaultIdentity" => BY_DEFAULT_IDENTITY_SINGLE_PK,
                _ => NEVER_REFUSED,
            },
            Op::CreatePartition { .. }
            | Op::AttachPartition { .. }
            | Op::DetachPartition { .. }
            | Op::DropPartition { .. } => "partition lifecycle operations are PostgreSQL-only",
            Op::AddColumn { .. } => match variant {
                "identity" => {
                    "addColumn identity is PostgreSQL-only; SQLite/MySQL auto-increment \
                     identity requires a createTable sole primary key"
                }
                "nextvalDefault" => NEXTVAL_PG_ONLY,
                _ => NEVER_REFUSED,
            },
            Op::CreateIndex {
                columns, r#where, ..
            } => match dialect {
                Dialect::Sqlite => {
                    "createIndex BRIN/INCLUDE/WITH/ONLY features are unsupported on SQLite"
                }
                Dialect::Mysql => {
                    if columns
                        .iter()
                        .any(|element| matches!(element, IndexElement::Expr { .. }))
                    {
                        "createIndex expression elements are not supported on MySQL"
                    } else if r#where.is_some() {
                        "createIndex partial predicates require partial-index support; MySQL does not support partial indexes"
                    } else {
                        "createIndex BRIN/INCLUDE/WITH/ONLY features are unsupported on MySQL"
                    }
                }
                Dialect::Postgres => NEVER_REFUSED,
            },
            Op::Comment { .. } => "COMMENT ON is PostgreSQL-only in the current engine",
            Op::SetColumnType { .. } => {
                "setColumnType.using expression rendering is deferred in the current engine"
            }
            Op::SetColumnDefault { .. } => match variant {
                "nextval" => NEXTVAL_PG_ONLY,
                _ => NEVER_REFUSED,
            },
            Op::RenameColumn { .. } => match variant {
                "existenceGuard" => {
                    "renameColumn ifExists guards cannot be attributed to a single migration unit today"
                }
                _ => "renameColumn is render-only for MySQL, not live-rendered",
            },
            Op::AddConstraint { .. } => match variant {
                "check" => {
                    "addConstraint(check) expression rendering is PostgreSQL-only in the current engine"
                }
                "pk" => "addConstraint user PRIMARY KEY is inconsistent today and fold refuses it",
                "fkNoLocalColumn" => "addConstraint(fk) with no local column is unsupported",
                "exclusion" => "exclusion constraints are PostgreSQL-only in the current engine",
                "fkComposite" => "multi-column foreign keys are PostgreSQL-only in the current engine",
                "fkNonId" => {
                    "foreign keys referencing non-id columns are PostgreSQL-only in the current engine"
                }
                "fkNotValid" => {
                    "NOT VALID online constraint adoption (addForeignKey { notValid }) is PostgreSQL-only in the current engine"
                }
                _ => NEVER_REFUSED,
            },
            Op::Insert { .. } => "insert onConflict is PostgreSQL-only in the current engine",
            Op::CreateView { .. } => match variant {
                "materializedReplace" => {
                    "createView replace+materialized is unsupported in the current engine"
                }
                _ => "materialized views are PostgreSQL-only in the current engine",
            },
            Op::DropView { .. } => "materialized views are PostgreSQL-only in the current engine",
            Op::CreateDomain { .. } => NEXTVAL_PG_ONLY,
            Op::CreateSequence { .. } | Op::AlterSequence { .. } | Op::DropSequence { .. } => {
                "standalone sequence objects are PostgreSQL-only in the current engine"
            }
            Op::CreateSchema { .. } | Op::DropSchema { .. } => {
                "schema vendor primitives are PostgreSQL-only"
            }
            Op::CreateExtension { .. } | Op::DropExtension { .. } => {
                "extension vendor primitives are PostgreSQL-only"
            }
            Op::CreateRole { .. } => match variant {
                "superuserIfNotExists" => {
                    "createRole cannot combine superuser:true with ifNotExists:true"
                }
                _ => "role vendor primitives are PostgreSQL-only",
            },
            Op::AlterRole { .. } | Op::DropRole { .. } | Op::DropOwnedBy { .. } => {
                "role vendor primitives are PostgreSQL-only"
            }
            Op::Grant { .. } | Op::Revoke { .. } => "grant vendor primitives are PostgreSQL-only",
            Op::SetRls { .. } => "RLS vendor primitives are PostgreSQL-only",
            Op::CreatePolicy { .. } | Op::DropPolicy { .. } => {
                "policy vendor primitives are PostgreSQL-only"
            }
            Op::CreateFunction { .. } | Op::DropFunction { .. } => {
                "function vendor primitives are PostgreSQL-only"
            }
            Op::PgRaw { .. } => "pgRaw statements are PostgreSQL-only",
            Op::CreateTrigger {
                timing,
                events,
                for_each,
                action,
                when,
                ..
            } => trigger_reason(dialect, timing, events, for_each, action, when),
            // Every remaining op is portable on all three dialects, so no cell is
            // ever `Unsupported` and this reason is never surfaced.
            _ => NEVER_REFUSED,
        }
    }

    /// The per-dialect trigger refusal reason, computed from the payload with the
    /// same priority the previous op-level trigger arms used (so a combined
    /// trigger reports each dialect's own first-hit facet).
    fn trigger_reason(
        dialect: crate::model::support::Dialect,
        timing: &TriggerTiming,
        events: &[TriggerEvent],
        for_each: &ForEach,
        action: &TriggerAction,
        when: &Option<Expr>,
    ) -> &'static str {
        use crate::model::support::Dialect;
        match dialect {
            Dialect::Postgres => {
                if matches!(action, TriggerAction::Body { .. }) {
                    "Postgres triggers must execute a named trigger function"
                } else {
                    NEVER_REFUSED
                }
            }
            Dialect::Sqlite => {
                if matches!(action, TriggerAction::ExecuteFunction { .. }) {
                    "SQLite has no CREATE TRIGGER EXECUTE FUNCTION form"
                } else if events.iter().any(|e| matches!(e, TriggerEvent::Truncate)) {
                    "SQLite has no TRUNCATE trigger event"
                } else if matches!(for_each, ForEach::Statement) {
                    "SQLite triggers are row-level only"
                } else {
                    NEVER_REFUSED
                }
            }
            Dialect::Mysql => {
                if matches!(action, TriggerAction::ExecuteFunction { .. }) {
                    "MySQL has no CREATE TRIGGER EXECUTE FUNCTION form"
                } else if events.len() != 1 {
                    "MySQL CREATE TRIGGER accepts exactly one trigger event"
                } else if events.iter().any(|e| matches!(e, TriggerEvent::Truncate)) {
                    "MySQL has no TRUNCATE trigger event"
                } else if matches!(timing, TriggerTiming::InsteadOf) {
                    "MySQL does not support INSTEAD OF triggers"
                } else if matches!(for_each, ForEach::Statement) {
                    "MySQL triggers are row-level only"
                } else if when.is_some() {
                    "MySQL triggers do not support WHEN predicates"
                } else if let TriggerAction::Body { statements } = action {
                    if statements.iter().any(|stmt| {
                        matches!(
                            stmt,
                            TriggerStmt::Raise {
                                level: RaiseLevel::Ignore,
                                ..
                            }
                        )
                    }) {
                        "MySQL cannot render RAISE IGNORE"
                    } else {
                        NEVER_REFUSED
                    }
                } else {
                    NEVER_REFUSED
                }
            }
        }
    }

    /// The support TIER (core vs vendor + its capabilities) for this op shape.
    /// Tier cannot be read off the generated table's dispositions — a vendor op
    /// can be unsupported on every dialect (e.g. `createRole` superuser+ifNotExists,
    /// `createView` materialized+replace) — so it stays a per-op declaration, kept
    /// in lock-step with [`Op::vendor_capabilities`] by `op_support_matrix`.
    fn support_tier(op: &Op) -> crate::model::support::SupportTier {
        use crate::model::support::{
            SupportTier, CAP_EXTENSION, CAP_FUNCTION, CAP_GRANT, CAP_MATERIALIZED_VIEW, CAP_POLICY,
            CAP_PARTITION, CAP_RAW_MATERIALIZED_VIEW, CAP_RAW_SQL, CAP_RLS, CAP_ROLE, CAP_SCHEMA,
        };
        match op {
            Op::CreateSchema { .. } | Op::DropSchema { .. } => SupportTier::Vendor(CAP_SCHEMA),
            Op::CreateExtension { .. } | Op::DropExtension { .. } => {
                SupportTier::Vendor(CAP_EXTENSION)
            }
            Op::CreateRole { .. }
            | Op::AlterRole { .. }
            | Op::DropRole { .. }
            | Op::DropOwnedBy { .. } => SupportTier::Vendor(CAP_ROLE),
            Op::Grant { .. } | Op::Revoke { .. } => SupportTier::Vendor(CAP_GRANT),
            Op::AttachPartition { .. } => SupportTier::Vendor(CAP_PARTITION),
            Op::SetRls { .. } => SupportTier::Vendor(CAP_RLS),
            Op::CreatePolicy { .. } | Op::DropPolicy { .. } => SupportTier::Vendor(CAP_POLICY),
            Op::CreateFunction { .. } | Op::DropFunction { .. } => SupportTier::Vendor(CAP_FUNCTION),
            Op::PgRaw { .. } => SupportTier::Vendor(CAP_RAW_SQL),
            Op::CreateView {
                query, materialized, ..
            } if materialized.unwrap_or(false) => {
                if matches!(query, ViewQuery::Raw { .. }) {
                    SupportTier::Vendor(CAP_RAW_MATERIALIZED_VIEW)
                } else {
                    SupportTier::Vendor(CAP_MATERIALIZED_VIEW)
                }
            }
            Op::DropView { materialized, .. } if materialized.unwrap_or(false) => {
                SupportTier::Vendor(CAP_MATERIALIZED_VIEW)
            }
            _ => SupportTier::Core,
        }
    }

    /// The static per-op feature-support declarations validation walks after the
    /// op-level dialect decision. Feature gates are a finer, orthogonal refusal
    /// dimension (not the op-level dialect gate the generated table drives), so
    /// they remain hand-declared here.
    fn support_features(op: &Op) -> &'static [crate::model::support::FeatureSupport] {
        use crate::model::support::{
            ADD_COLUMN_FEATURES, ADD_CONSTRAINT_FEATURES, ALTER_COLUMN_TYPE_FEATURES,
            COMMENT_FEATURES, CREATE_INDEX_FEATURES, CREATE_TABLE_FEATURES, CREATE_TRIGGER_FEATURES,
            CREATE_VIEW_FEATURES, INSERT_FEATURES, PARTITION_FEATURES, PG_RAW_FEATURES,
            RENAME_COLUMN_FEATURES, SEQUENCE_FEATURES, SET_COLUMN_DEFAULT_FEATURES,
        };
        match op {
            Op::CreateTable { .. } => CREATE_TABLE_FEATURES,
            Op::CreatePartition { .. }
            | Op::AttachPartition { .. }
            | Op::DetachPartition { .. }
            | Op::DropPartition { .. } => PARTITION_FEATURES,
            Op::AddColumn { .. } => ADD_COLUMN_FEATURES,
            Op::CreateIndex { .. } => CREATE_INDEX_FEATURES,
            Op::Comment { .. } => COMMENT_FEATURES,
            Op::SetColumnType { .. } => ALTER_COLUMN_TYPE_FEATURES,
            Op::SetColumnDefault { .. } => SET_COLUMN_DEFAULT_FEATURES,
            Op::RenameColumn { .. } => RENAME_COLUMN_FEATURES,
            Op::AddConstraint { .. } => ADD_CONSTRAINT_FEATURES,
            Op::Insert { .. } => INSERT_FEATURES,
            Op::CreateView { .. } => CREATE_VIEW_FEATURES,
            Op::DropView { materialized, .. } if materialized.unwrap_or(false) => {
                CREATE_VIEW_FEATURES
            }
            Op::CreateSequence { .. } | Op::AlterSequence { .. } | Op::DropSequence { .. } => {
                SEQUENCE_FEATURES
            }
            Op::CreateTrigger { .. } => CREATE_TRIGGER_FEATURES,
            Op::PgRaw { .. } => PG_RAW_FEATURES,
            _ => &[],
        }
    }

    /// The generated-dialect-table lookup key for this op: its wire kind token and
    /// the variant that selects the payload-dependent support branch. The variant
    /// derivation is the SINGLE source shared with `dialect_table_faithfulness`'s
    /// corpus (via [`Op::op_variant`]) — the branch-selection that used to live in
    /// the hand-written `support()` dialect arms.
    fn op_kind_and_variant(op: &Op) -> (&'static str, &'static str) {
        match op {
            Op::CreateTable {
                columns,
                primary_key,
                partition_by,
                indexes,
                ..
            } => (
                "createTable",
                create_table_variant(columns, primary_key.as_deref(), partition_by, indexes),
            ),
            Op::CreatePartition { .. } => ("createPartition", "base"),
            Op::AttachPartition { .. } => ("attachPartition", "base"),
            Op::DetachPartition { .. } => ("detachPartition", "base"),
            Op::DropPartition { .. } => ("dropPartition", "base"),
            Op::DropTable { .. } => ("dropTable", "base"),
            Op::RenameTable { .. } => ("renameTable", "base"),
            Op::AddColumn {
                default, identity, ..
            } => {
                let variant = if identity.is_some() {
                    "identity"
                } else if matches!(default, Some(IrDefault::Nextval { .. })) {
                    "nextvalDefault"
                } else {
                    "base"
                };
                ("addColumn", variant)
            }
            Op::DropColumn { .. } => ("dropColumn", "base"),
            Op::CreateIndex {
                columns,
                using,
                r#where,
                include,
                with,
                only,
                nulls_not_distinct,
                ..
            } => (
                "createIndex",
                create_index_variant(
                    columns,
                    using,
                    r#where,
                    include,
                    with,
                    *only,
                    *nulls_not_distinct,
                ),
            ),
            Op::DropIndex { .. } => ("dropIndex", "base"),
            Op::SetColumnType { using, .. } => (
                "setColumnType",
                if using.is_some() { "using" } else { "base" },
            ),
            Op::SetColumnNotNull { .. } => ("setColumnNotNull", "base"),
            Op::DropColumnNotNull { .. } => ("dropColumnNotNull", "base"),
            Op::SetColumnDefault { value, .. } => {
                let variant = match value {
                    IrDefault::Container { .. } | IrDefault::Json { .. } => "containerOrJson",
                    IrDefault::Nextval { .. } => "nextval",
                    _ => "base",
                };
                ("setColumnDefault", variant)
            }
            Op::DropColumnDefault { .. } => ("dropColumnDefault", "base"),
            Op::RenameColumn {
                existence_guard, ..
            } => (
                "renameColumn",
                if existence_guard.is_some() {
                    "existenceGuard"
                } else {
                    "base"
                },
            ),
            Op::SetTableOptions { .. } => ("setTableOptions", "base"),
            Op::AddConstraint { constraint, .. } => {
                ("addConstraint", add_constraint_variant(&constraint.kind))
            }
            Op::DropConstraint { .. } => ("dropConstraint", "base"),
            Op::ValidateConstraint { .. } => ("validateConstraint", "base"),
            Op::Insert { on_conflict, .. } => (
                "insert",
                if on_conflict.is_some() {
                    "onConflict"
                } else {
                    "base"
                },
            ),
            Op::Update { .. } => ("update", "base"),
            Op::Delete { .. } => ("delete", "base"),
            Op::Backfill { .. } => ("backfill", "base"),
            Op::Dialectal { .. } => ("dialectal", "base"),
            Op::Comment { .. } => ("comment", "base"),
            Op::CreateView {
                materialized,
                replace,
                ..
            } => {
                let materialized = materialized.unwrap_or(false);
                let variant = if materialized && replace.unwrap_or(false) {
                    "materializedReplace"
                } else if materialized {
                    "materialized"
                } else {
                    "base"
                };
                ("createView", variant)
            }
            Op::DropView { materialized, .. } => (
                "dropView",
                if materialized.unwrap_or(false) {
                    "materialized"
                } else {
                    "base"
                },
            ),
            Op::CreateEnum { .. } => ("createEnum", "base"),
            Op::DropEnum { .. } => ("dropEnum", "base"),
            Op::CreateDomain { default, .. } => (
                "createDomain",
                if matches!(default, Some(IrDefault::Nextval { .. })) {
                    "nextvalDefault"
                } else {
                    "base"
                },
            ),
            Op::DropDomain { .. } => ("dropDomain", "base"),
            Op::CreateSequence { .. } => ("createSequence", "base"),
            Op::AlterSequence { .. } => ("alterSequence", "base"),
            Op::DropSequence { .. } => ("dropSequence", "base"),
            Op::CreateTrigger {
                timing,
                events,
                for_each,
                action,
                when,
                ..
            } => (
                "createTrigger",
                create_trigger_variant(timing, events, for_each, action, when),
            ),
            Op::DropTrigger { .. } => ("dropTrigger", "base"),
            Op::CreateSchema { .. } => ("createSchema", "base"),
            Op::DropSchema { .. } => ("dropSchema", "base"),
            Op::CreateExtension { .. } => ("createExtension", "base"),
            Op::DropExtension { .. } => ("dropExtension", "base"),
            Op::CreateRole {
                superuser,
                if_not_exists,
                ..
            } => (
                "createRole",
                if superuser.unwrap_or(false) && if_not_exists.unwrap_or(false) {
                    "superuserIfNotExists"
                } else {
                    "base"
                },
            ),
            Op::AlterRole { .. } => ("alterRole", "base"),
            Op::DropRole { .. } => ("dropRole", "base"),
            Op::DropOwnedBy { .. } => ("dropOwnedBy", "base"),
            Op::Grant { .. } => ("grant", "base"),
            Op::Revoke { .. } => ("revoke", "base"),
            Op::SetRls { .. } => ("setRls", "base"),
            Op::CreatePolicy { .. } => ("createPolicy", "base"),
            Op::DropPolicy { .. } => ("dropPolicy", "base"),
            Op::CreateFunction { .. } => ("createFunction", "base"),
            Op::DropFunction { .. } => ("dropFunction", "base"),
            Op::PgRaw { .. } => ("pgRaw", "base"),
        }
    }

    /// The generated-dialect-table variant token for this op shape (the payload
    /// branch its support decision turns on; `"base"` for payload-independent ops).
    /// This is the ONE variant derivation, shared by `support()` (which looks the
    /// disposition up in the table) and the `dialect_table_faithfulness` corpus
    /// (which pins each representative op to its labelled variant) — so the two can
    /// never drift.
    #[must_use]
    pub fn op_variant(op: &Op) -> &'static str {
        op_kind_and_variant(op).1
    }

    fn create_table_variant(
        columns: &[IrColumn],
        primary_key: Option<&[String]>,
        partition_by: &Option<PartitionSpec>,
        indexes: &[IrIndex],
    ) -> &'static str {
        let has_pg_only_index_feature = indexes.iter().any(|index| {
            matches!(index.using, Some(IndexMethod::Brin))
                || !index.include.is_empty()
                || index.with.as_ref().is_some_and(|params| !params.is_empty())
                || index.only.unwrap_or(false)
                || index.nulls_not_distinct.unwrap_or(false)
                || index_has_element_opclass_or_collation(&index.columns)
        });
        let has_nextval_default = columns
            .iter()
            .any(|column| matches!(column.default, Some(IrDefault::Nextval { .. })));
        let has_identity_always = columns
            .iter()
            .any(|column| matches!(column.identity, Some(identity) if identity.always));
        let pk_cols = primary_key;
        let has_nonportable_by_default_identity = columns.iter().any(|column| {
            matches!(column.identity, Some(identity) if !identity.always)
                && !matches!(pk_cols, Some(cols) if cols.len() == 1 && cols[0] == column.name)
        });
        if let Some(spec) = partition_by {
            if spec.collapse() {
                "partitionedCollapse"
            } else {
                "partitioned"
            }
        } else if has_pg_only_index_feature {
            "pgOnlyIndexFeature"
        } else if has_nextval_default {
            "nextvalDefault"
        } else if has_identity_always {
            "identityAlways"
        } else if has_nonportable_by_default_identity {
            "nonportableByDefaultIdentity"
        } else {
            "base"
        }
    }

    fn create_index_variant(
        columns: &[IndexElement],
        using: &Option<IndexMethod>,
        r#where: &Option<Expr>,
        include: &[String],
        with: &Option<IndexStorageParams>,
        only: Option<bool>,
        nulls_not_distinct: Option<bool>,
    ) -> &'static str {
        let method_or_feature = matches!(
            using,
            Some(
                IndexMethod::Brin
                    | IndexMethod::Gin
                    | IndexMethod::Gist
                    | IndexMethod::Ivfflat
                    | IndexMethod::Hnsw
                    | IndexMethod::Fts5
            )
        ) || !include.is_empty()
            || with.as_ref().is_some_and(|params| !params.is_empty())
            || only.unwrap_or(false)
            || nulls_not_distinct.unwrap_or(false)
            || index_has_element_opclass_or_collation(columns);
        if method_or_feature {
            "pgOnlyMethodOrFeature"
        } else if columns
            .iter()
            .any(|element| matches!(element, IndexElement::Expr { .. }))
        {
            "exprElement"
        } else if r#where.is_some() {
            "partialWhere"
        } else {
            "base"
        }
    }

    fn add_constraint_variant(kind: &IrConstraintKind) -> &'static str {
        match kind {
            IrConstraintKind::Check { .. } => "check",
            IrConstraintKind::Fk { columns, .. } if columns.is_empty() => "fkNoLocalColumn",
            IrConstraintKind::Exclusion { .. } => "exclusion",
            // `NOT VALID` online adoption is PostgreSQL-only. It takes precedence
            // over the composite/non-id FK sub-shapes (all likewise PG-only), so a
            // `notValid` FK reports the single PG-only `fkNotValid` variant — keeping
            // the op-level `Support::decision()` PG-only (and thus == validate, like
            // `fkComposite`), robust regardless of corpus sampling order.
            IrConstraintKind::Fk { not_valid: Some(true), .. } => "fkNotValid",
            IrConstraintKind::Fk { columns, .. } if columns.len() != 1 => "fkComposite",
            IrConstraintKind::Fk {
                references_columns, ..
            } if !(references_columns.is_empty()
                || (references_columns.len() == 1 && references_columns[0] == "id")) =>
            {
                "fkNonId"
            }
            IrConstraintKind::Fk { .. } => "fkSimple",
            IrConstraintKind::Unique { .. } => "unique",
        }
    }

    fn create_trigger_variant(
        timing: &TriggerTiming,
        events: &[TriggerEvent],
        for_each: &ForEach,
        action: &TriggerAction,
        when: &Option<Expr>,
    ) -> &'static str {
        if matches!(action, TriggerAction::ExecuteFunction { .. }) {
            return "executeFunction";
        }
        // The remaining variants are trigger BODIES. A truncate event or a
        // statement-level body is refused on every dialect; otherwise the body is
        // portable on SQLite and classified by MySQL's own refusal priority (which
        // is the strictest), so the chosen variant's disposition matches the op.
        if events.iter().any(|e| matches!(e, TriggerEvent::Truncate)) {
            return "bodyTruncateEvent";
        }
        if matches!(for_each, ForEach::Statement) {
            return "bodyStatementLevel";
        }
        if events.len() != 1 {
            return "bodyMultipleEvents";
        }
        if matches!(timing, TriggerTiming::InsteadOf) {
            return "bodyInsteadOf";
        }
        if when.is_some() {
            return "bodyWhen";
        }
        if let TriggerAction::Body { statements } = action {
            if statements.iter().any(|stmt| {
                matches!(
                    stmt,
                    TriggerStmt::Raise {
                        level: RaiseLevel::Ignore,
                        ..
                    }
                )
            }) {
                return "bodyRaiseIgnore";
            }
        }
        "bodySimple"
    }

    /// All VENDOR capabilities this op requires. Most ops require at most one; a
    /// raw materialized view requires both the raw-view-body and materialized-view
    /// capabilities.
    #[must_use]
    pub fn vendor_capabilities(op: &Op) -> Vec<crate::model::capability::VendorCapability> {
        use crate::model::capability::VendorCapability as C;
        match op {
            // Portable core — no capability required.
            Op::CreateTable { .. }
            | Op::CreatePartition { .. }
            | Op::DetachPartition { .. }
            | Op::DropPartition { .. }
            | Op::SetTableOptions { .. }
            | Op::DropTable { .. }
            | Op::RenameTable { .. }
            | Op::AddColumn { .. }
            | Op::DropColumn { .. }
            | Op::CreateIndex { .. }
            | Op::DropIndex { .. }
            | Op::SetColumnType { .. }
            | Op::SetColumnNotNull { .. }
            | Op::DropColumnNotNull { .. }
            | Op::SetColumnDefault { .. }
            | Op::DropColumnDefault { .. }
            | Op::RenameColumn { .. }
            | Op::AddConstraint { .. }
            | Op::DropConstraint { .. }
            | Op::ValidateConstraint { .. }
            | Op::Insert { .. }
            | Op::Update { .. }
            | Op::Delete { .. }
            | Op::Backfill { .. }
            | Op::Dialectal { .. }
            | Op::CreateTrigger { .. }
            | Op::DropTrigger { .. }
            | Op::CreateEnum { .. }
            | Op::DropEnum { .. }
            | Op::CreateDomain { .. }
            | Op::DropDomain { .. }
            | Op::CreateSequence { .. }
            | Op::AlterSequence { .. }
            | Op::DropSequence { .. }
            | Op::Comment { .. } => Vec::new(),
            Op::CreateView {
                query: ViewQuery::Structured { .. },
                materialized,
                ..
            } if !materialized.unwrap_or(false) => Vec::new(),
            Op::CreateView {
                query,
                materialized,
                ..
            } => {
                let mut caps = Vec::new();
                if matches!(query, ViewQuery::Raw { .. }) {
                    caps.push(C::RawViewBody);
                }
                if materialized.unwrap_or(false) {
                    caps.push(C::MaterializedView);
                }
                caps
            }
            Op::DropView { materialized, .. } => {
                if materialized.unwrap_or(false) {
                    vec![C::MaterializedView]
                } else {
                    Vec::new()
                }
            }
            // Vendor — each maps to its capability flag.
            Op::CreateSchema { .. } | Op::DropSchema { .. } => vec![C::Schema],
            Op::CreateExtension { .. } | Op::DropExtension { .. } => vec![C::Extension],
            Op::CreateRole { .. }
            | Op::AlterRole { .. }
            | Op::DropRole { .. }
            | Op::DropOwnedBy { .. } => vec![C::Role],
            Op::Grant { .. } | Op::Revoke { .. } => vec![C::Grant],
            Op::AttachPartition { .. } => vec![C::Partition],
            Op::SetRls { .. } => vec![C::Rls],
            Op::CreatePolicy { .. } | Op::DropPolicy { .. } => vec![C::Policy],
            Op::CreateFunction { .. } | Op::DropFunction { .. } => vec![C::Function],
            Op::PgRaw { .. } => vec![C::RawSql],
        }
    }
