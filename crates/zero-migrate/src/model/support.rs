//! Static support declarations for the migration op DSL.
//!
//! Validation consumes these declarations before expression walking. Lowering
//! still keeps defensive guards for invalid direct callers, but author-facing
//! dialect/refusal diagnostics are sourced from this support matrix.

use std::borrow::Cow;

use crate::model::capability::VendorCapability;
use zero_migrate_backend::renderer::FeatureSupportKey;
use zero_migrate_ir::dialect::DialectId;

/// The set of dialects an op/feature is supported on.
///
/// Defined in the leaf contract (`zero_migrate_ir::dialect`) and re-exported
/// here unchanged: the backend registry and the support matrix key on the SAME
/// set type, and the registry lives below the engine.
pub use zero_migrate_ir::dialect::DialectSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenderMode {
    Offline,
    LiveResolved,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SupportDecision {
    Supported {
        render: RenderMode,
    },
    Unsupported {
        code: &'static str,
        reason: &'static str,
    },
}

impl SupportDecision {
    #[must_use]
    pub const fn is_supported(self) -> bool {
        matches!(self, Self::Supported { .. })
    }

    #[must_use]
    pub const fn render_mode(self) -> Option<RenderMode> {
        match self {
            Self::Supported { render } => Some(render),
            Self::Unsupported { .. } => None,
        }
    }
}

/// The support decision for an op or feature, PER DIALECT.
///
/// An ASSOCIATION LIST keyed by [`DialectId`], not one field per vendor. The
/// previous shape put every dialect this engine ships in the type itself, so a
/// fourth backend could not declare its support without editing a struct in a
/// crate it does not own, and every one of the ~40 construction sites passed its
/// three cells in vendor order. It is the same closed-set problem [`DialectId`]
/// exists to remove, in a shape that is not an enum and so survived deleting one.
///
/// Cells are sorted by id and deduplicated, the same discipline
/// [`DialectSet`] uses, so lookup is a binary search.
///
/// `Cow`, not `&'static [_]`, because the op-level declaration is assembled at
/// RUNTIME from the registered backend policies (`op_support::support`). Feature
/// declarations carry a backend-policy key instead of copying the shipping registry
/// into core. That costs `Copy`; the readers all take `&self`, so no call site changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DialectSupport {
    decisions: Cow<'static, [(DialectId, SupportDecision)]>,
    backend_feature: Option<BackendFeatureSupport>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BackendFeatureSupport {
    feature: FeatureSupportKey,
    render: RenderMode,
}

impl DialectSupport {
    /// Declare a feature whose support/refusal is owned exhaustively by every
    /// registered backend rather than by a core-owned vendor id set.
    #[must_use]
    pub const fn backend_feature(feature: FeatureSupportKey, render: RenderMode) -> Self {
        Self {
            decisions: Cow::Borrowed(&[]),
            backend_feature: Some(BackendFeatureSupport { feature, render }),
        }
    }

    /// Declare from cells in any order; they are sorted and deduplicated here.
    ///
    /// The runtime door, used by `op_support::support` to assemble an op's cells
    /// from registered backend policy answers.
    #[must_use]
    pub fn from_cells(cells: impl IntoIterator<Item = (DialectId, SupportDecision)>) -> Self {
        let mut decisions: Vec<(DialectId, SupportDecision)> = cells.into_iter().collect();
        decisions.sort_by(|(left, _), (right, _)| left.cmp(right));
        decisions.dedup_by(|(left, _), (right, _)| left == right);
        Self {
            decisions: Cow::Owned(decisions),
            backend_feature: None,
        }
    }

    /// The decision this declaration states for `id`, or `None` if it states none.
    ///
    /// `None` means THIS DECLARATION MAKES NO CLAIM, which is not the same as
    /// `Unsupported` (an authored refusal, with a reason an operator can act on).
    /// Callers needing a verdict must choose which they mean; [`Self::decision`]
    /// panics rather than pick one silently.
    #[must_use]
    pub fn decision_for(&self, id: &DialectId) -> Option<SupportDecision> {
        if let Some(policy) = self.backend_feature {
            let vendor = crate::render::backends::VENDORS.get(id)?;
            return Some(match vendor.dml.feature_support_refusal(policy.feature) {
                Some(reason) => unsupported(crate::model::validate::CODE_UNSUPPORTED, reason),
                None => supported(policy.render),
            });
        }
        self.decisions
            .binary_search_by(|(dialect, _)| dialect.cmp(id))
            .ok()
            .map(|i| self.decisions[i].1)
    }

    /// The decision for one of the dialects this engine ships.
    ///
    /// Panics if this declaration states no cell for it. A missing cell is a
    /// declaration defect, and both of the other answers hide it — treating it as
    /// supported fails open into a render that was never declared, and treating it
    /// as unsupported invents a refusal with no reason to show anyone.
    #[must_use]
    pub fn decision(&self, dialect: &DialectId) -> SupportDecision {
        self.decision_for(dialect)
            .unwrap_or_else(|| panic!("support declaration states no decision for {dialect}"))
    }

    /// The dialects this declaration states a decision for.
    ///
    /// **The order differs by branch, and is NOT ascending in both.** A
    /// `backend_feature` declaration yields the REGISTRY's order (shipping order:
    /// `postgres, sqlite, mysql`); a decision-list declaration yields its own
    /// `decisions` order, which IS ascending because `decision_for`
    /// binary-searches it. Callers must not treat this iterator as sorted: this
    /// doc previously claimed ascending order for both, and an ordering assertion
    /// written against that claim failed on a correct tree.
    pub fn dialects(&self) -> Box<dyn Iterator<Item = DialectId> + '_> {
        if self.backend_feature.is_some() {
            Box::new(crate::render::backends::VENDORS.dialects())
        } else {
            Box::new(self.decisions.iter().map(|(id, _)| id.clone()))
        }
    }

    /// The subset of the stated dialects whose decision is supported.
    ///
    /// ITS ONLY ASSERTION CHECKS THE EMPTINESS BIT, NOT THE MEMBERS.
    /// `op_support_matrix::support_declarations_cover_every_op_and_dialect` reads
    /// this and says "DialectSet must agree with per-dialect decisions", which
    /// reads like a cross-check of the membership. It is not, and that was
    /// MEASURED, not inferred: returning every registered dialect for each op that
    /// supports anything — wrong members for `raw` and every other PG-only op,
    /// right emptiness bit — leaves the lib suite and the whole `dialect_matrix`
    /// suite reporting clean. Predates this shape (the same hole existed when
    /// this was `from_bools` over three fields); recorded so the next reader does
    /// not take the message for the guarantee.
    #[must_use]
    pub fn supported_dialects(&self) -> DialectSet {
        self.dialects()
            .filter(|id| self.decision(id).is_supported())
            .collect()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SupportTier {
    Core,
    Vendor(&'static [VendorCapability]),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Feature {
    PartialIndex,
    IndexInclude,
    IndexStorageParams,
    IndexOnly,
    IndexNullsNotDistinct,
    IndexOpclass,
    IndexCollation,
    ExpressionIndex,
    NonBtreeIndexMethod,
    TableLevelForeignKey,
    TableLevelUnique,
    TableLevelCheck,
    CompositeForeignKey,
    ForeignKeyNoLocalColumn,
    NonIdForeignKey,
    ConstraintNotValid,
    ExclusionConstraint,
    AlterColumnUsing,
    SequenceDefault,
    RenameColumnGuard,
    ExistenceGuardProbe,
    InsertOnConflict,
    MaterializedView,
    CreateOrReplaceMaterializedView,
    TriggerMultipleEvents,
    TriggerTruncateEvent,
    TriggerInsteadOfTiming,
    TriggerStatementForEach,
    TriggerExecuteFunction,
    TriggerBody,
    TriggerWhen,
    TriggerRaiseIgnore,
    Comment,
    Sequence,
    RawViewBody,
    RawSql,
    PartitionDdl,
}

impl Feature {
    const fn support_key(self) -> FeatureSupportKey {
        match self {
            Self::PartialIndex => FeatureSupportKey::PartialIndex,
            Self::IndexInclude => FeatureSupportKey::IndexInclude,
            Self::IndexStorageParams => FeatureSupportKey::IndexStorageParams,
            Self::IndexOnly => FeatureSupportKey::IndexOnly,
            Self::IndexNullsNotDistinct => FeatureSupportKey::IndexNullsNotDistinct,
            Self::IndexOpclass => FeatureSupportKey::IndexOpclass,
            Self::IndexCollation => FeatureSupportKey::IndexCollation,
            Self::ExpressionIndex => FeatureSupportKey::ExpressionIndex,
            Self::NonBtreeIndexMethod => FeatureSupportKey::NonBtreeIndexMethod,
            Self::TableLevelForeignKey => FeatureSupportKey::TableLevelForeignKey,
            Self::TableLevelUnique => FeatureSupportKey::TableLevelUnique,
            Self::TableLevelCheck => FeatureSupportKey::TableLevelCheckExpression,
            Self::CompositeForeignKey => FeatureSupportKey::CompositeForeignKey,
            Self::ForeignKeyNoLocalColumn => FeatureSupportKey::ForeignKeyNoLocalColumn,
            Self::NonIdForeignKey => FeatureSupportKey::NonIdForeignKey,
            Self::ConstraintNotValid => FeatureSupportKey::ConstraintNotValid,
            Self::ExclusionConstraint => FeatureSupportKey::ExclusionConstraint,
            Self::AlterColumnUsing => FeatureSupportKey::AlterColumnUsing,
            Self::SequenceDefault => FeatureSupportKey::SequenceDefault,
            Self::RenameColumnGuard => FeatureSupportKey::RenameColumnGuard,
            Self::ExistenceGuardProbe => FeatureSupportKey::ExistenceGuardProbe,
            Self::InsertOnConflict => FeatureSupportKey::InsertOnConflict,
            Self::MaterializedView => FeatureSupportKey::MaterializedView,
            Self::CreateOrReplaceMaterializedView => {
                FeatureSupportKey::CreateOrReplaceMaterializedView
            }
            Self::TriggerMultipleEvents => FeatureSupportKey::TriggerMultipleEvents,
            Self::TriggerTruncateEvent => FeatureSupportKey::TriggerTruncateEvent,
            Self::TriggerInsteadOfTiming => FeatureSupportKey::TriggerInsteadOfTiming,
            Self::TriggerStatementForEach => FeatureSupportKey::TriggerStatementForEach,
            Self::TriggerExecuteFunction => FeatureSupportKey::TriggerExecuteFunction,
            Self::TriggerBody => FeatureSupportKey::TriggerBody,
            Self::TriggerWhen => FeatureSupportKey::TriggerWhen,
            Self::TriggerRaiseIgnore => FeatureSupportKey::TriggerRaiseIgnore,
            Self::Comment => FeatureSupportKey::Comment,
            Self::Sequence => FeatureSupportKey::Sequence,
            Self::RawViewBody => FeatureSupportKey::RawViewBody,
            Self::RawSql => FeatureSupportKey::RawSql,
            Self::PartitionDdl => FeatureSupportKey::PartitionDdl,
        }
    }
}

// Not `Copy`: `DialectSupport` owns a `Cow` so an op's cells can be assembled at
// runtime. Every reader takes `&self`, so no call site changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeatureSupport {
    pub feature: Feature,
    pub dialects: DialectSupport,
}

impl FeatureSupport {
    #[must_use]
    pub const fn new(feature: Feature, render: RenderMode) -> Self {
        Self {
            feature,
            dialects: DialectSupport::backend_feature(feature.support_key(), render),
        }
    }

    #[must_use]
    pub fn decision(&self, dialect: &DialectId) -> SupportDecision {
        self.dialects.decision(dialect)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Support {
    pub tier: SupportTier,
    pub dialects: DialectSupport,
    pub features: &'static [FeatureSupport],
}

impl Support {
    #[must_use]
    pub const fn new(
        tier: SupportTier,
        dialects: DialectSupport,
        features: &'static [FeatureSupport],
    ) -> Self {
        Self {
            tier,
            dialects,
            features,
        }
    }

    #[must_use]
    pub const fn core(dialects: DialectSupport, features: &'static [FeatureSupport]) -> Self {
        Self::new(SupportTier::Core, dialects, features)
    }

    #[must_use]
    pub const fn vendor(
        capabilities: &'static [VendorCapability],
        dialects: DialectSupport,
        features: &'static [FeatureSupport],
    ) -> Self {
        Self::new(SupportTier::Vendor(capabilities), dialects, features)
    }

    #[must_use]
    pub fn decision(&self, dialect: &DialectId) -> SupportDecision {
        self.dialects.decision(dialect)
    }

    #[must_use]
    pub fn supported_dialects(&self) -> DialectSet {
        self.dialects.supported_dialects()
    }
}

#[must_use]
pub const fn supported(render: RenderMode) -> SupportDecision {
    SupportDecision::Supported { render }
}

#[must_use]
pub const fn unsupported(code: &'static str, reason: &'static str) -> SupportDecision {
    SupportDecision::Unsupported { code, reason }
}

pub(crate) const CAP_EXTENSION: &[VendorCapability] = &[VendorCapability::Extension];
pub(crate) const CAP_SCHEMA: &[VendorCapability] = &[VendorCapability::Schema];
pub(crate) const CAP_ROLE: &[VendorCapability] = &[VendorCapability::Role];
pub(crate) const CAP_GRANT: &[VendorCapability] = &[VendorCapability::Grant];
pub(crate) const CAP_RLS: &[VendorCapability] = &[VendorCapability::Rls];
pub(crate) const CAP_PARTITION: &[VendorCapability] = &[VendorCapability::Partition];
pub(crate) const CAP_POLICY: &[VendorCapability] = &[VendorCapability::Policy];
pub(crate) const CAP_FUNCTION: &[VendorCapability] = &[VendorCapability::Function];
pub(crate) const CAP_RAW_SQL: &[VendorCapability] = &[VendorCapability::RawSql];
pub(crate) const CAP_MATERIALIZED_VIEW: &[VendorCapability] = &[VendorCapability::MaterializedView];
pub(crate) const CAP_RAW_MATERIALIZED_VIEW: &[VendorCapability] = &[
    VendorCapability::RawViewBody,
    VendorCapability::MaterializedView,
];

pub(crate) const CREATE_TABLE_FEATURES: &[FeatureSupport] = &[
    FeatureSupport::new(Feature::ExistenceGuardProbe, RenderMode::LiveResolved),
    FeatureSupport::new(Feature::SequenceDefault, RenderMode::Offline),
    FeatureSupport::new(Feature::TableLevelCheck, RenderMode::Offline),
    FeatureSupport::new(Feature::TableLevelForeignKey, RenderMode::Offline),
    FeatureSupport::new(Feature::ForeignKeyNoLocalColumn, RenderMode::Offline),
    FeatureSupport::new(Feature::CompositeForeignKey, RenderMode::Offline),
    FeatureSupport::new(Feature::NonIdForeignKey, RenderMode::Offline),
    FeatureSupport::new(Feature::TableLevelUnique, RenderMode::Offline),
    FeatureSupport::new(Feature::ExclusionConstraint, RenderMode::Offline),
    FeatureSupport::new(Feature::ExpressionIndex, RenderMode::Offline),
    FeatureSupport::new(Feature::PartialIndex, RenderMode::Offline),
    FeatureSupport::new(Feature::IndexInclude, RenderMode::Offline),
    FeatureSupport::new(Feature::IndexStorageParams, RenderMode::Offline),
    FeatureSupport::new(Feature::IndexOnly, RenderMode::Offline),
    FeatureSupport::new(Feature::IndexNullsNotDistinct, RenderMode::Offline),
    FeatureSupport::new(Feature::IndexOpclass, RenderMode::Offline),
    FeatureSupport::new(Feature::IndexCollation, RenderMode::Offline),
    FeatureSupport::new(Feature::NonBtreeIndexMethod, RenderMode::Offline),
];

pub(crate) const ADD_COLUMN_FEATURES: &[FeatureSupport] = &[
    FeatureSupport::new(Feature::ExistenceGuardProbe, RenderMode::LiveResolved),
    FeatureSupport::new(Feature::SequenceDefault, RenderMode::Offline),
];

pub(crate) const CREATE_INDEX_FEATURES: &[FeatureSupport] = &[
    FeatureSupport::new(Feature::ExistenceGuardProbe, RenderMode::LiveResolved),
    FeatureSupport::new(Feature::ExpressionIndex, RenderMode::Offline),
    FeatureSupport::new(Feature::PartialIndex, RenderMode::Offline),
    FeatureSupport::new(Feature::IndexInclude, RenderMode::Offline),
    FeatureSupport::new(Feature::IndexStorageParams, RenderMode::Offline),
    FeatureSupport::new(Feature::IndexOnly, RenderMode::Offline),
    FeatureSupport::new(Feature::IndexNullsNotDistinct, RenderMode::Offline),
    FeatureSupport::new(Feature::IndexOpclass, RenderMode::Offline),
    FeatureSupport::new(Feature::IndexCollation, RenderMode::Offline),
    FeatureSupport::new(Feature::NonBtreeIndexMethod, RenderMode::Offline),
];

pub(crate) const PARTITION_FEATURES: &[FeatureSupport] = &[
    FeatureSupport::new(Feature::ExistenceGuardProbe, RenderMode::LiveResolved),
    FeatureSupport::new(Feature::PartitionDdl, RenderMode::Offline),
];

pub(crate) const ALTER_COLUMN_TYPE_FEATURES: &[FeatureSupport] = &[
    FeatureSupport::new(Feature::ExistenceGuardProbe, RenderMode::LiveResolved),
    FeatureSupport::new(Feature::AlterColumnUsing, RenderMode::Offline),
];

pub(crate) const RENAME_COLUMN_FEATURES: &[FeatureSupport] = &[FeatureSupport::new(
    Feature::RenameColumnGuard,
    RenderMode::Offline,
)];

pub(crate) const ADD_CONSTRAINT_FEATURES: &[FeatureSupport] = &[
    FeatureSupport::new(Feature::ExistenceGuardProbe, RenderMode::LiveResolved),
    FeatureSupport::new(Feature::ForeignKeyNoLocalColumn, RenderMode::Offline),
    FeatureSupport::new(Feature::CompositeForeignKey, RenderMode::Offline),
    FeatureSupport::new(Feature::NonIdForeignKey, RenderMode::Offline),
    FeatureSupport::new(Feature::ConstraintNotValid, RenderMode::Offline),
    FeatureSupport::new(Feature::TableLevelCheck, RenderMode::Offline),
    FeatureSupport::new(Feature::ExclusionConstraint, RenderMode::Offline),
];

pub(crate) const SET_COLUMN_DEFAULT_FEATURES: &[FeatureSupport] = &[
    FeatureSupport::new(Feature::ExistenceGuardProbe, RenderMode::LiveResolved),
    FeatureSupport::new(Feature::SequenceDefault, RenderMode::Offline),
];

pub(crate) const INSERT_FEATURES: &[FeatureSupport] = &[FeatureSupport::new(
    Feature::InsertOnConflict,
    RenderMode::Offline,
)];

pub(crate) const CREATE_VIEW_FEATURES: &[FeatureSupport] = &[
    FeatureSupport::new(Feature::RawViewBody, RenderMode::Offline),
    FeatureSupport::new(Feature::MaterializedView, RenderMode::Offline),
    FeatureSupport::new(
        Feature::CreateOrReplaceMaterializedView,
        RenderMode::Offline,
    ),
];

pub(crate) const CREATE_TRIGGER_FEATURES: &[FeatureSupport] = &[
    FeatureSupport::new(Feature::TriggerExecuteFunction, RenderMode::Offline),
    FeatureSupport::new(Feature::TriggerBody, RenderMode::Offline),
    FeatureSupport::new(Feature::TriggerTruncateEvent, RenderMode::Offline),
    FeatureSupport::new(Feature::TriggerStatementForEach, RenderMode::Offline),
    FeatureSupport::new(Feature::TriggerMultipleEvents, RenderMode::Offline),
    FeatureSupport::new(Feature::TriggerInsteadOfTiming, RenderMode::Offline),
    FeatureSupport::new(Feature::TriggerWhen, RenderMode::Offline),
    FeatureSupport::new(Feature::TriggerRaiseIgnore, RenderMode::Offline),
];

pub(crate) const COMMENT_FEATURES: &[FeatureSupport] =
    &[FeatureSupport::new(Feature::Comment, RenderMode::Offline)];

pub(crate) const SEQUENCE_FEATURES: &[FeatureSupport] =
    &[FeatureSupport::new(Feature::Sequence, RenderMode::Offline)];

pub(crate) const RAW_SQL_FEATURES: &[FeatureSupport] =
    &[FeatureSupport::new(Feature::RawSql, RenderMode::Offline)];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::op_support::FEATURE_SUPPORT_REGISTRY;
    use std::collections::BTreeSet;
    use zero_migrate_ir::dialect::DialectId;

    /// Every hand-written [`FeatureSupport`] this module declares, labelled by
    /// the registry group and feature it came from.
    ///
    /// This is a DISCOVERED set — it is only as wide as the registry and the
    /// `*_FEATURES` arrays happen to be — so every test that scans it needs the
    /// floor in [`every_declaration_covers_the_registered_census`].
    fn declarations() -> Vec<(String, &'static FeatureSupport)> {
        FEATURE_SUPPORT_REGISTRY
            .iter()
            .flat_map(|group| {
                group.features().iter().map(move |feature| {
                    (
                        format!("{} / {:?}", group.label(), feature.feature),
                        feature,
                    )
                })
            })
            .collect()
    }

    #[test]
    fn the_shipping_census_is_derived_from_the_vendor_registry() {
        let ids: Vec<DialectId> = crate::render::backends::VENDORS.dialects().collect();
        let census: BTreeSet<DialectId> = ids.iter().cloned().collect();
        assert_eq!(
            ids.len(),
            census.len(),
            "the vendor registry contains duplicate backend ids: {ids:?}"
        );
        assert!(
            census.len() >= 3,
            "the shipping vendor registry collapsed to {} ({census:?})",
            census.len()
        );
    }

    #[test]
    fn every_declaration_covers_the_registered_census() {
        let declarations = declarations();

        // CENSUS FLOOR. Both axes here are DISCOVERED, and a scan over a
        // discovered set fails OPEN: shrink the registry, or shrink one
        // declaration's cells, and the loop below iterates less, finds less, and
        // reports clean while the artifact has stopped making a claim it used to
        // make. The three struct fields this shape replaced did that check by
        // TYPE, for free; these two assertions are what buys it back.
        assert_eq!(
            FEATURE_SUPPORT_REGISTRY.len(),
            14,
            "the feature-support registry changed size; every scan over it is \
             only as wide as this"
        );
        assert!(
            declarations.len() >= 59,
            "only {} feature declarations were discovered; the scan below is only \
             as wide as this set",
            declarations.len()
        );

        let census: BTreeSet<DialectId> = crate::render::backends::VENDORS.dialects().collect();
        assert!(
            census.len() >= 3,
            "the shipping vendor registry collapsed to {} ({census:?})",
            census.len()
        );

        for (label, feature) in &declarations {
            let cells: Vec<DialectId> = feature.dialects.dialects().collect();
            assert_eq!(
                cells.iter().cloned().collect::<BTreeSet<_>>(),
                census,
                "{label} declares {cells:?}, not the registered census {census:?}"
            );
            // NO SORTEDNESS ASSERTION HERE, DELIBERATELY. `decision_for`
            // binary-searches `decisions`, so the ordering matters - but it is
            // enforced BY CONSTRUCTION, not by this test. The fields are private
            // and there are exactly two constructors: `backend_feature`, which
            // leaves `decisions` empty, and `from_cells`, which sorts and dedups.
            // No struct literal exists anywhere. An assertion here can therefore
            // never fail.
            //
            // This was re-added once and had to be removed again, so it is worth
            // the paragraph. Asserting it over `dialects()` is WORSE than vacuous:
            // that iterator returns the REGISTRY's order for a `backend_feature`
            // declaration (`postgres, sqlite, mysql` - shipping order), which is
            // not ascending, and that branch never reaches the binary search at
            // all (it returns early through `VENDORS.get`, a linear `find`). The
            // assertion goes RED on a tree with no defect. If sortedness ever
            // needs a guard again, the thing to guard is `from_cells`.
            for dialect in &census {
                let _ = feature.decision(dialect);
            }
        }
    }

    #[test]
    fn a_dialect_the_engine_does_not_ship_gets_no_claim_not_a_verdict() {
        let duckdb = DialectId::new("duckdb");
        assert!(
            crate::render::backends::VENDORS.get(&duckdb).is_none(),
            "this test needs an id the engine does not ship"
        );
        for (label, feature) in declarations() {
            assert_eq!(
                feature.dialects.decision_for(&duckdb),
                None,
                "{label} answers for a dialect it never declared; NO CLAIM and a \
                 refusal are different answers and only one of them is honest here"
            );
        }
    }
}
