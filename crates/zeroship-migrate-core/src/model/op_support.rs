//! Engine-side dialect-support + vendor-capability computation for the closed
//! [`Op`] wire type.
//!
//! These were inherent methods on `Op` in `model/ir.rs`. When the wire contract
//! was extracted into the `zero-migrate-ir` leaf crate, the
//! dialect-support / vendor-capability logic could NOT ride along: it reads the
//! engine-owned portability vocabulary ([`crate::model::support`],
//! [`crate::model::capability`]) and the engine's
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
    index_has_element_opclass_or_collation, ForEach, IndexElement, IndexMethod, IrColumn,
    IrConstraintKind, IrDefault, IrIndex, Op, PartitionSpec, RaiseLevel, TriggerAction,
    TriggerEvent, TriggerStmt, TriggerTiming, ViewQuery,
};
use zeroship_migrate_backend::registry::VendorSet;
use zeroship_migrate_ir::attribute::CreateIndexAttributes;

pub fn is_vendor(op: &Op) -> bool {
    !vendor_capabilities(op).is_empty()
}

/// Dynamic support declaration for this concrete op shape across every
/// registered backend.
///
/// This is the support matrix the authoring validator consumes for dialect
/// and feature refusals before lowering.
#[must_use]
pub fn support(vendors: VendorSet, op: &Op) -> crate::model::support::Support {
    use crate::model::support::Support;

    let (kind, variant) = op_kind_and_variant(op);
    let dialects = crate::model::support::DialectSupport::from_cells(
        vendors.as_slice().iter().map(|vendor| {
            let id = &vendor.descriptor.id;
            let disposition = vendor.validation.op_disposition(kind, variant);
            (
                id.clone(),
                support_cell(vendors, op, disposition, id, variant),
            )
        }),
    );
    Support::new(support_tier(op), dialects, support_features(op))
}

/// Support declaration for only the selected registered backend.
///
/// This is the production validation route. Core resolves the open id once and
/// asks that backend's required policy; it never reads the generated three-vendor
/// parity artifact.
#[must_use]
pub fn support_for_target(
    vendors: VendorSet,
    op: &Op,
    target: &zeroship_migrate_ir::dialect::DialectId,
) -> crate::model::support::Support {
    use crate::model::support::Support;

    let (kind, variant) = op_kind_and_variant(op);
    let vendor = vendors
        .get(target)
        .unwrap_or_else(|| panic!("no registered backend vendor for {target}"));
    let disposition = vendor.validation.op_disposition(kind, variant);
    let dialects = crate::model::support::DialectSupport::from_cells([(
        target.clone(),
        support_cell(vendors, op, disposition, target, variant),
    )]);
    Support::new(support_tier(op), dialects, support_features(op))
}

/// The placeholder returned when a cell is declared `unsupported` and NOBODY
/// WROTE THE OPERATOR-FACING REASON. It is not a legitimate diagnosis and must
/// never reach a user.
///
/// It is `pub` on purpose. `dialect-support.toml` and `unsupported_reason` (private,
/// below) are
/// two files that must be edited together - flipping a cell to `unsupported`
/// without adding the matching arm hands the operator this string for an
/// ordinary dialect limit - and that has happened: four rows shipped it on
/// SQLite, three of them also on MySQL. Exporting the constant lets
/// `tests/unsupported_reason_is_operator_facing.rs` (offline, all three
/// dialects) and `tests/dialect_conformance_live.rs` (live, PostgreSQL +
/// SQLite) both detect it WITHOUT re-typing the literal, so the check cannot
/// drift from the thing it checks.
///
/// The arms below are exhaustive over every `unsupported` cell the sidecar
/// declares, so nothing reaches the `_` fallbacks today; the `debug_assert!` in
/// `support_cell` (private, above) fails a debug build the moment that stops being
/// true.
pub const INTERNAL_NO_REFUSAL_REASON: &str = "internal: supported cell has no refusal reason";

/// Short alias for the arms below, which are dense enough already.
const NEVER_REFUSED: &str = INTERNAL_NO_REFUSAL_REASON;

/// Assemble one per-dialect [`crate::model::support::SupportDecision`] from the
/// selected backend's disposition for this (op-kind, variant): an `Unsupported`
/// disposition becomes the engine-internal refusal reason; any supported
/// disposition (portable / vendor / transparent-degradable) becomes the op's
/// render mode on that dialect. The supported/unsupported/vendor GATE is thus
/// owned by the backend that registered the selected id; only the render
/// strategy and diagnostic wording remain in core.
fn support_cell(
    vendors: VendorSet,
    op: &Op,
    disposition: zeroship_migrate_backend::validation::Disposition,
    dialect: &zeroship_migrate_ir::dialect::DialectId,
    variant: &'static str,
) -> crate::model::support::SupportDecision {
    use crate::model::support::{supported, unsupported};
    use zeroship_migrate_backend::validation::Disposition;
    match disposition {
        Disposition::Unsupported => {
            let reason = unsupported_reason(vendors, op, dialect, variant);
            // The backend table and `unsupported_reason` are edited in different files
            // and have already drifted apart once, shipping the placeholder to
            // operators. Debug builds - which is every test run - refuse to
            // return it, so a cell flipped to `unsupported` without its reason
            // arm fails LOUDLY here instead of quietly reaching a user.
            // `tests/unsupported_reason_is_operator_facing.rs` is the release-mode
            // half of the same guard, and covers MySQL, which no live suite does.
            debug_assert_ne!(
                reason,
                INTERNAL_NO_REFUSAL_REASON,
                "the backend policy declares {}/{variant} unsupported on {dialect:?}, but \
                 op_support::unsupported_reason has no arm for it: the operator would be \
                 shown an internal placeholder instead of a reason",
                op_kind_and_variant(op).0,
            );
            unsupported(crate::model::validate::CODE_UNSUPPORTED, reason)
        }
        Disposition::Portable | Disposition::Vendor | Disposition::TransparentDegradable => {
            supported(render_mode(vendors, op, dialect, variant))
        }
    }
}

/// The render mode a SUPPORTED cell of this op reports on `dialect`. This is a
/// render-strategy detail of the current engine (does the op lower fully
/// offline, or only once the live schema is resolved), NOT a dialect-support
/// decision - so it is derived here, not from the generated dialect table.
fn render_mode(
    vendors: VendorSet,
    op: &Op,
    dialect: &zeroship_migrate_ir::dialect::DialectId,
    variant: &'static str,
) -> crate::model::support::RenderMode {
    use crate::model::support::RenderMode;
    let alter_live =
        crate::render::backends::renderer(vendors, dialect).alter_ops_require_live_schema();
    match op {
        // Backfill, column-rename, primary-key lifecycle, and identity
        // synchronization are live-rendered
        // on every supported dialect (they need the resolved live schema).
        Op::Backfill { .. }
        | Op::RenameColumn { .. }
        | Op::AlterPrimaryKey { .. }
        | Op::SynchronizeIdentity { .. } => RenderMode::LiveResolved,
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
            if alter_live {
                RenderMode::LiveResolved
            } else {
                RenderMode::Offline
            }
        }
        _ => RenderMode::Offline,
    }
}

/// The refusal reason an UNSUPPORTED cell of this op reports on `dialect`. Only
/// consulted by `support_cell` where the selected backend records an
/// `Unsupported` disposition, so the fully-portable ops never reach it. The
/// reason wording (and, for `createIndex` / `createTrigger`, its per-dialect
/// divergence on combined payloads) mirrors the previous hand-written arms
/// exactly, preserving the author-facing diagnostics.
fn unsupported_reason(
    vendors: VendorSet,
    op: &Op,
    dialect: &zeroship_migrate_ir::dialect::DialectId,
    variant: &'static str,
) -> &'static str {
    let backend_refusal = || {
        crate::render::backends::renderer(vendors, dialect)
            .op_support_refusal(op, variant)
            .unwrap_or(NEVER_REFUSED)
    };
    match op {
        Op::CreateTable { .. } => match variant {
            "partitioned"
            | "pgOnlyIndexFeature"
            | "nextvalDefault"
            | "identityAlways"
            | "nonportableByDefaultIdentity" => backend_refusal(),
            _ => NEVER_REFUSED,
        },
        Op::CreatePartition { .. }
        | Op::AttachPartition { .. }
        | Op::DetachPartition { .. }
        | Op::DropPartition { .. } => backend_refusal(),
        Op::AddColumn { .. } => match variant {
            "identity" | "nextvalDefault" => backend_refusal(),
            _ => NEVER_REFUSED,
        },
        Op::CreateIndex { .. } | Op::Comment { .. } => backend_refusal(),
        // The ALTER-shaped column ops. All of them refuse on the same two
        // grounds, so they share one helper rather than repeating it five
        // times and letting the copies drift.
        //
        // `using` is a THIRD thing and must be matched first: an expression
        // the engine cannot render is refused on EVERY dialect (PostgreSQL
        // included), which is not a dialect limit at all.
        Op::SetColumnType { .. } => match variant {
            "using" => "setColumnType.using expression rendering is deferred in the current engine",
            _ => backend_refusal(),
        },
        Op::SetColumnDefault { .. } => backend_refusal(),
        Op::SetColumnNotNull { .. }
        | Op::DropColumnNotNull { .. }
        | Op::DropColumnDefault { .. }
        | Op::DropConstraint { .. }
        | Op::ValidateConstraint { .. } => backend_refusal(),
        Op::RenameColumn { .. } => match variant {
            "existenceGuard" => {
                "renameColumn ifExists guards cannot be attributed to a single migration unit today"
            }
            _ => backend_refusal(),
        },
        Op::AlterPrimaryKey { .. } => NEVER_REFUSED,
        Op::SynchronizeIdentity { .. } => NEVER_REFUSED,
        Op::AddConstraint { .. } => match variant {
            "pk" => "addConstraint user PRIMARY KEY is inconsistent today and fold refuses it",
            "fkNoLocalColumn" => "addConstraint(fk) with no local column is unsupported",
            "fkComposite" => "multi-column foreign keys are unsupported on this target",
            "fkNonId" => "foreign keys referencing non-id columns are unsupported on this target",
            // Only the non-FK variants need the rebuild. `fkSimple`,
            // `fkComposite` and `fkNonId` all APPLY on SQLite (measured
            // against a live database), so this is a per-variant gap.
            "check" | "exclusion" | "unique" | "fkNotValid" => backend_refusal(),
            _ => NEVER_REFUSED,
        },
        Op::Insert { .. } => match variant {
            "onConflictDoNothing" => backend_refusal(),
            _ => NEVER_REFUSED,
        },
        Op::CreateView { .. } => match variant {
            "materializedReplace" => {
                "createView replace+materialized is unsupported in the current engine"
            }
            _ => backend_refusal(),
        },
        Op::DropView { .. }
        | Op::CreateDomain { .. }
        | Op::CreateSequence { .. }
        | Op::AlterSequence { .. }
        | Op::DropSequence { .. }
        | Op::CreateSchema { .. }
        | Op::DropSchema { .. }
        | Op::CreateExtension { .. }
        | Op::DropExtension { .. } => backend_refusal(),
        Op::CreateRole { .. } => match variant {
            "superuserIfNotExists" => {
                "createRole cannot combine superuser:true with ifNotExists:true"
            }
            _ => backend_refusal(),
        },
        Op::AlterRole { .. }
        | Op::DropRole { .. }
        | Op::DropOwnedBy { .. }
        | Op::Grant { .. }
        | Op::Revoke { .. }
        | Op::SetRls { .. }
        | Op::CreatePolicy { .. }
        | Op::DropPolicy { .. }
        | Op::CreateFunction { .. }
        | Op::DropFunction { .. }
        | Op::Raw { .. }
        | Op::CreateTrigger { .. } => backend_refusal(),
        // Every remaining op is portable on all three dialects, so no cell is
        // ever `Unsupported` and this reason is never surfaced.
        _ => NEVER_REFUSED,
    }
}

/// The capabilities this op requires **because of the privileged primitive it
/// RENDERS**, as opposed to [`vendor_capabilities`], which is every capability its
/// AUTHOR must hold.
///
/// The two lists answer different questions and have already been conflated once. A
/// `createTrigger` executing a named function requires the FUNCTION capability of its
/// author - conscripting existing code is the same power as writing it - but the
/// primitive it renders is a TRIGGER, which every backend renders. Asking a backend
/// "do you render a function?" about that op produced a refusal claiming the trigger
/// belonged to the privileged catalog-object family and telling the operator to
/// deploy it elsewhere, in place of the accurate facet refusal.
///
/// So the non-renderer gate reads THIS list, and the authority gate reads the other.
///
/// Every op answers with its [`vendor_capabilities`] except where that list carries a
/// capability the op does not render. `createTrigger { executeFunction }` is the only
/// such shape today, and it is spelled out rather than derived so a future addition
/// has to state which of the two lists it is joining.
#[must_use]
pub fn rendered_vendor_capabilities(op: &Op) -> Vec<crate::model::capability::VendorCapability> {
    use crate::model::capability::VendorCapability as C;
    if matches!(
        op,
        Op::CreateTrigger {
            action: TriggerAction::ExecuteFunction { .. },
            ..
        }
    ) {
        return vec![C::Trigger];
    }
    vendor_capabilities(op)
}

/// The support TIER (core vs vendor + its capabilities) for this op shape.
/// Tier cannot be read off the generated table's dispositions - a vendor op
/// can be unsupported on every dialect (e.g. `createRole` superuser+ifNotExists,
/// `createView` materialized+replace) - so it stays a per-op declaration, kept
/// in lock-step with [`vendor_capabilities`] by `op_support_matrix`.
fn support_tier(op: &Op) -> crate::model::support::SupportTier {
    use crate::model::support::{
        SupportTier, CAP_EXTENSION, CAP_FUNCTION, CAP_GRANT, CAP_MATERIALIZED_VIEW, CAP_PARTITION,
        CAP_POLICY, CAP_RAW_MATERIALIZED_VIEW, CAP_RAW_SQL, CAP_RLS, CAP_ROLE, CAP_SCHEMA,
    };
    match op {
        Op::CreateSchema { .. } | Op::DropSchema { .. } => SupportTier::Vendor(CAP_SCHEMA),
        Op::CreateExtension { .. } | Op::DropExtension { .. } => SupportTier::Vendor(CAP_EXTENSION),
        Op::CreateRole { .. }
        | Op::AlterRole { .. }
        | Op::DropRole { .. }
        | Op::DropOwnedBy { .. } => SupportTier::Vendor(CAP_ROLE),
        Op::Grant { .. } | Op::Revoke { .. } => SupportTier::Vendor(CAP_GRANT),
        Op::AttachPartition { .. } => SupportTier::Vendor(CAP_PARTITION),
        Op::SetRls { .. } => SupportTier::Vendor(CAP_RLS),
        Op::CreatePolicy { .. } | Op::DropPolicy { .. } => SupportTier::Vendor(CAP_POLICY),
        Op::CreateFunction { .. } | Op::DropFunction { .. } => SupportTier::Vendor(CAP_FUNCTION),
        Op::Raw { .. } => SupportTier::Vendor(CAP_RAW_SQL),
        Op::CreateView {
            query,
            materialized,
            ..
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

/// Declare the op-to-feature-table registry once, then derive both the live
/// lookup match and the test-only iterable registry used to generate the
/// support-matrix documentation. The declaration order is therefore the
/// canonical documentation order as well as the lookup order.
macro_rules! feature_support_registry {
    (
        $(
            $group:ident {
                label: $label:literal,
                features: $features:path,
                ops: [$($pattern:pat $(if $guard:expr)?),+ $(,)?],
            }
        )+
    ) => {
        #[cfg(test)]
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub(crate) enum FeatureSupportGroup {
            $($group,)+
        }

        #[cfg(test)]
        impl FeatureSupportGroup {
            pub(crate) const fn label(self) -> &'static str {
                match self {
                    $(Self::$group => $label,)+
                }
            }

            pub(crate) const fn features(
                self,
            ) -> &'static [crate::model::support::FeatureSupport] {
                match self {
                    $(Self::$group => $features,)+
                }
            }
        }

        #[cfg(test)]
        pub(crate) const FEATURE_SUPPORT_REGISTRY: &[FeatureSupportGroup] = &[
            $(FeatureSupportGroup::$group,)+
        ];

        /// The static per-op feature-support declarations validation walks after
        /// the op-level dialect decision. Feature gates are a finer, orthogonal
        /// refusal dimension (not the op-level dialect gate the generated table
        /// drives), so they remain hand-declared here.
        fn support_features(op: &Op) -> &'static [crate::model::support::FeatureSupport] {
            match op {
                $($($pattern $(if $guard)? => $features,)+)+
                _ => &[],
            }
        }
    };
}

feature_support_registry! {
    CreateTable {
        label: "Create table",
        features: crate::model::support::CREATE_TABLE_FEATURES,
        ops: [Op::CreateTable { .. }],
    }
    PartitionLifecycle {
        label: "Partition lifecycle",
        features: crate::model::support::PARTITION_FEATURES,
        ops: [
            Op::CreatePartition { .. },
            Op::AttachPartition { .. },
            Op::DetachPartition { .. },
            Op::DropPartition { .. },
        ],
    }
    AddColumn {
        label: "Add column",
        features: crate::model::support::ADD_COLUMN_FEATURES,
        ops: [Op::AddColumn { .. }],
    }
    CreateIndex {
        label: "Create index",
        features: crate::model::support::CREATE_INDEX_FEATURES,
        ops: [Op::CreateIndex { .. }],
    }
    Comment {
        label: "Comment",
        features: crate::model::support::COMMENT_FEATURES,
        ops: [Op::Comment { .. }],
    }
    SetColumnType {
        label: "Set column type",
        features: crate::model::support::ALTER_COLUMN_TYPE_FEATURES,
        ops: [Op::SetColumnType { .. }],
    }
    SetColumnDefault {
        label: "Set column default",
        features: crate::model::support::SET_COLUMN_DEFAULT_FEATURES,
        ops: [Op::SetColumnDefault { .. }],
    }
    RenameColumn {
        label: "Rename column",
        features: crate::model::support::RENAME_COLUMN_FEATURES,
        ops: [Op::RenameColumn { .. }],
    }
    AddConstraint {
        label: "Add constraint",
        features: crate::model::support::ADD_CONSTRAINT_FEATURES,
        ops: [Op::AddConstraint { .. }],
    }
    Insert {
        label: "Insert / DML",
        features: crate::model::support::INSERT_FEATURES,
        ops: [Op::Insert { .. }],
    }
    CreateView {
        label: "Create view / drop materialized view",
        features: crate::model::support::CREATE_VIEW_FEATURES,
        ops: [
            Op::CreateView { .. },
            Op::DropView { materialized, .. } if materialized.unwrap_or(false),
        ],
    }
    SequenceLifecycle {
        label: "Sequence lifecycle",
        features: crate::model::support::SEQUENCE_FEATURES,
        ops: [
            Op::CreateSequence { .. },
            Op::AlterSequence { .. },
            Op::DropSequence { .. },
        ],
    }
    CreateTrigger {
        label: "Create trigger",
        features: crate::model::support::CREATE_TRIGGER_FEATURES,
        ops: [Op::CreateTrigger { .. }],
    }
    Raw {
        label: "Raw SQL",
        features: crate::model::support::RAW_SQL_FEATURES,
        ops: [Op::Raw { .. }],
    }
}

/// The generated-dialect-table lookup key for this op: its wire kind token and
/// the variant that selects the payload-dependent support branch. The variant
/// derivation is the SINGLE source shared with `dialect_table_faithfulness`'s
/// corpus (via [`op_variant`]) - the branch-selection that used to live in
/// the hand-written `support()` dialect arms.
pub(crate) fn op_kind_and_variant(op: &Op) -> (&'static str, &'static str) {
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
            attributes,
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
                attributes,
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
        Op::AlterPrimaryKey { .. } => ("alterPrimaryKey", "base"),
        Op::SynchronizeIdentity { .. } => ("synchronizeIdentity", "base"),
        Op::SetTableOptions { .. } => ("setTableOptions", "base"),
        Op::AddConstraint { constraint, .. } => {
            ("addConstraint", add_constraint_variant(&constraint.kind))
        }
        Op::DropConstraint { .. } => ("dropConstraint", "base"),
        Op::ValidateConstraint { .. } => ("validateConstraint", "base"),
        Op::Insert { on_conflict, .. } => {
            let variant = match on_conflict {
                None => "base",
                Some(conflict)
                    if conflict
                        .do_update
                        .as_ref()
                        .is_some_and(|set| !set.is_empty()) =>
                {
                    "onConflictDoUpdate"
                }
                Some(_) => "onConflictDoNothing",
            };
            ("insert", variant)
        }
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
        Op::Raw { .. } => ("raw", "base"),
    }
}

/// The generated-dialect-table variant token for this op shape (the payload
/// branch its support decision turns on; `"base"` for payload-independent ops).
/// This is the ONE variant derivation, shared by `support()` (which looks the
/// disposition up in the table) and the `dialect_table_faithfulness` corpus
/// (which pins each representative op to its labelled variant) - so the two can
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
    let has_nonportable_index_feature = indexes.iter().any(|index| {
        matches!(index.using, Some(IndexMethod::Brin))
            || !index.include.is_empty()
            || !index.attributes.is_empty()
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
    } else if has_nonportable_index_feature {
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
    attributes: &CreateIndexAttributes,
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
        )
    ) || !include.is_empty()
        || !attributes.is_empty()
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
        // `notValid` FK reports the single PG-only `fkNotValid` variant - keeping
        // the op-level `Support::decision()` PG-only (and thus == validate, like
        // `fkComposite`), robust regardless of corpus sampling order.
        IrConstraintKind::Fk {
            not_valid: Some(true),
            ..
        } => "fkNotValid",
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

/// All VENDOR capabilities this op's AUTHOR must hold. Most ops require at most
/// one; a raw materialized view requires both the raw-view-body and
/// materialized-view capabilities, and a trigger executing a named function requires
/// both the trigger and the function capabilities.
///
/// This is not the list of primitives the op RENDERS - see
/// [`rendered_vendor_capabilities`], which is the one a backend can answer for.
#[must_use]
pub fn vendor_capabilities(op: &Op) -> Vec<crate::model::capability::VendorCapability> {
    use crate::model::capability::VendorCapability as C;
    match op {
        // Portable core - no capability required.
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
        | Op::AlterPrimaryKey { .. }
        | Op::SynchronizeIdentity { .. }
        | Op::AddConstraint { .. }
        | Op::DropConstraint { .. }
        | Op::ValidateConstraint { .. }
        | Op::Insert { .. }
        | Op::Update { .. }
        | Op::Delete { .. }
        | Op::Backfill { .. }
        | Op::Dialectal { .. }
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
        // Vendor - each maps to its capability flag.
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
        // Capability-gated but NOT tier-vendor. Every registered backend renders
        // triggers, so the op keeps a portable reach and a `SupportTier::Core`
        // declaration; what the grant governs is the authority to arrange for work to
        // fire on every affected row without a later statement naming it. The same
        // core-tier-plus-capability shape the raw view body already has.
        //
        // A trigger whose action EXECUTES A NAMED FUNCTION requires the FUNCTION
        // capability as well. `C::Function`'s meaning is "this charter may introduce
        // code into the database", and arranging for existing code to run on every
        // affected row - with no later statement naming it - is that same power
        // reached by a different door. An author who may not write a function may not
        // conscript one either. A trigger carrying a closed inline BODY introduces no
        // pre-existing code and keeps the trigger capability alone.
        Op::CreateTrigger {
            action: TriggerAction::ExecuteFunction { .. },
            ..
        } => vec![C::Trigger, C::Function],
        Op::CreateTrigger { .. } | Op::DropTrigger { .. } => vec![C::Trigger],
        Op::Raw { .. } => vec![C::RawSql],
    }
}

#[cfg(test)]
mod alter_primary_key_tests {
    use super::*;
    use crate::model::ir::AlterPrimaryKeyAction;
    use crate::model::support::RenderMode;
    use crate::test_fixtures::{MYSQL, POSTGRES, SQLITE};

    #[test]
    fn lifecycle_operation_is_portable_live_resolved_core() {
        let op = Op::AlterPrimaryKey {
            table: "orders".to_string(),
            action: AlterPrimaryKeyAction::Replace {
                expected_columns: vec!["id".to_string()],
                columns: vec!["tenant_id".to_string(), "order_id".to_string()],
                drop_identity_from: Some(vec!["id".to_string()]),
            },
            schema: None,
        };

        assert_eq!(op_variant(&op), "base");
        assert!(!is_vendor(&op));
        assert!(vendor_capabilities(&op).is_empty());
        let support = support(crate::test_fixtures::VENDORS, &op);
        for dialect in [&POSTGRES, &SQLITE, &MYSQL] {
            let decision = support.decision(crate::test_fixtures::VENDORS, dialect);
            assert!(decision.is_supported(), "{dialect:?}: {decision:?}");
            assert_eq!(decision.render_mode(), Some(RenderMode::LiveResolved));
        }
    }
}
