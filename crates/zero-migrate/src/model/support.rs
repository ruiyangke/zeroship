//! Static support declarations for the migration op DSL.
//!
//! Validation consumes these declarations before expression walking. Lowering
//! still keeps defensive guards for invalid direct callers, but author-facing
//! dialect/refusal diagnostics are sourced from this support matrix.

use std::borrow::Cow;

use crate::model::capability::VendorCapability;
use crate::model::dialect_table::Disposition;
use zero_migrate_ir::dialect::{DialectId, MYSQL, POSTGRES, SQLITE};

pub use crate::model::validate::SqlDialect;

impl Disposition {
    /// Whether this generated-table disposition admits the token on its dialect —
    /// everything except an explicit `Unsupported` refusal renders/validates
    /// (portable core, admitted vendor, and the reserved transparent-degradable
    /// class). This is the single supported-vs-refused reading of the generated
    /// dialect vocabulary, shared by `Op::support`'s cell assembly and the
    /// PostgreSQL-only expression gate below.
    #[must_use]
    pub const fn is_supported(self) -> bool {
        !matches!(self, Disposition::Unsupported)
    }
}

/// The disposition of a PostgreSQL-only EXPRESSION node on a dialect. Expression
/// nodes are not op-kinds, so they have no row in the generated (op-keyed) dialect
/// table; but their dialect verdict is exactly the canonical PG-only-core shape the
/// table records for every `pg = portable, sqlite/mysql = unsupported` op — so the
/// PG-only-expression gate reads that shape off the generated [`Disposition`]
/// vocabulary instead of a bespoke `== Postgres` arm.
#[must_use]
pub const fn pg_only_expr_disposition(dialect: SqlDialect) -> Disposition {
    match dialect {
        SqlDialect::Postgres => Disposition::Portable,
        SqlDialect::Sqlite | SqlDialect::Mysql => Disposition::Unsupported,
    }
}

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

/// The dialects this engine currently SHIPS, in ascending [`DialectId`] order.
///
/// The ONE place the support matrix names its full backend set. 28 of the 39
/// declarations below are built from this slice by [`admitting`] or
/// [`on_every_dialect`] rather than from three arguments in vendor order, so a
/// fourth backend picks up their existing decision here and needs no edit there.
///
/// The other 11 spell their cells out, because they state a DIFFERENT refusal
/// reason per dialect and no census-built shorthand can carry that. Those 11 do
/// need a cell for a fourth backend — and that is the point, not a gap: a
/// declaration that names a reason per dialect has nothing honest to say about a
/// dialect nobody wrote one for. `every_declaration_covers_the_shipping_census`
/// fails and names each of them, rather than letting the declaration silently
/// narrow.
///
/// Either way, no site takes its cells positionally in vendor order and no site
/// needs a new struct field — which is the whole difference between this shape
/// and the three vendor-named fields it replaces.
///
/// Sorted and deduplicated, because [`DialectSupport::decision_for`] binary-searches
/// the cells built from it. `the_shipping_census_is_the_engine_dialect_set` pins
/// both that ordering and the agreement with [`DialectSet::all`].
pub const SHIPPING_DIALECTS: &[DialectId] = &[MYSQL, POSTGRES, SQLITE];

/// One cell per shipping dialect, in [`SHIPPING_DIALECTS`] order.
///
/// A fixed-size ARRAY rather than a slice for one reason: a `const fn` cannot
/// return a `&'static` slice it builds from its own arguments, but it can return
/// an array by value, and `&`-ing that array inside a `const` item promotes it to
/// `'static`. That is what lets [`admitting`] and [`on_every_dialect`] stay
/// `const` while taking the render mode and refusal reason as parameters.
pub type DialectCells = [(DialectId, SupportDecision); SHIPPING_DIALECTS.len()];

/// Content equality for two ids, in `const` context.
///
/// `DialectId`'s derived `PartialEq` compares the STRING (see its own docs), but
/// `str`'s `==` is not callable from a `const fn`, so the same comparison is
/// spelled out over the bytes here.
const fn id_eq(left: &DialectId, right: &DialectId) -> bool {
    let (left, right) = (left.as_str().as_bytes(), right.as_str().as_bytes());
    if left.len() != right.len() {
        return false;
    }
    let mut i = 0;
    while i < left.len() {
        if left[i] != right[i] {
            return false;
        }
        i += 1;
    }
    true
}

const fn admits(admitted: &[DialectId], id: &DialectId) -> bool {
    let mut i = 0;
    while i < admitted.len() {
        if id_eq(&admitted[i], id) {
            return true;
        }
        i += 1;
    }
    false
}

/// The same `decision` on every shipping dialect.
///
/// Replaces the `all_supported` / `unsupported_all` pair, which differed only in
/// which decision they repeated three times.
#[must_use]
pub const fn on_every_dialect(decision: SupportDecision) -> DialectCells {
    [
        (shipping_dialect(0), decision),
        (shipping_dialect(1), decision),
        (shipping_dialect(2), decision),
    ]
}

/// `Supported { render }` on each admitted id, `refusal` on every other shipping
/// dialect.
///
/// Replaces the vendor-NAMED `postgres_only` helper. Admitting the
/// `POSTGRES_ONLY` id slice still names PostgreSQL — a PostgreSQL-only feature
/// is a claim about
/// PostgreSQL, and saying so is honest — but it no longer names, or positionally
/// implies, the dialects it refuses. A fourth backend lands in `refusal` with the
/// reason its author already wrote, rather than needing a fourth argument.
#[must_use]
pub const fn admitting(
    admitted: &[DialectId],
    render: RenderMode,
    refusal: SupportDecision,
) -> DialectCells {
    [
        admission_cell(admitted, 0, render, refusal),
        admission_cell(admitted, 1, render, refusal),
        admission_cell(admitted, 2, render, refusal),
    ]
}

const fn shipping_dialect(index: usize) -> DialectId {
    DialectId::new(SHIPPING_DIALECTS[index].as_str())
}

const fn admission_cell(
    admitted: &[DialectId],
    index: usize,
    render: RenderMode,
    refusal: SupportDecision,
) -> (DialectId, SupportDecision) {
    let id = shipping_dialect(index);
    let decision = if admits(admitted, &id) {
        supported(render)
    } else {
        refusal
    };
    (id, decision)
}

/// The support decision for an op or feature, PER DIALECT.
///
/// An ASSOCIATION LIST keyed by [`DialectId`], not one field per vendor. The
/// previous shape put every dialect this engine ships in the type itself, so a
/// fourth backend could not declare its support without editing a struct in a
/// crate it does not own, and every one of the ~40 construction sites passed its
/// three cells in vendor order. It is the same closed-set problem [`DialectId`]
/// exists to remove, in a shape that is not an enum and so survived deleting one
/// — exactly like [`crate::model::dialect_table::DispositionRow`], which this
/// mirrors.
///
/// Cells are sorted by id and deduplicated, the same discipline
/// [`DialectSet`] uses, so lookup is a binary search.
///
/// `Cow`, not `&'static [_]`, because the op-level declaration is assembled at
/// RUNTIME from the generated table (`op_support::support`) while every feature
/// declaration below is a `const`. That costs `Copy`; the readers all take
/// `&self`, so no call site changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DialectSupport {
    decisions: Cow<'static, [(DialectId, SupportDecision)]>,
}

impl DialectSupport {
    /// Declare from cells that are already sorted by id and deduplicated.
    ///
    /// `const`, so the feature tables below stay `const` items. It cannot check
    /// the ordering it relies on — a `const fn` has nowhere to report — so
    /// `every_declaration_covers_the_shipping_census` checks it instead, over
    /// every declaration the feature registry can reach.
    #[must_use]
    pub const fn declared(decisions: &'static [(DialectId, SupportDecision)]) -> Self {
        Self {
            decisions: Cow::Borrowed(decisions),
        }
    }

    /// Declare from cells in any order; they are sorted and deduplicated here.
    ///
    /// The runtime door, used by `op_support::support` to assemble an op's cells
    /// from the generated dialect table.
    #[must_use]
    pub fn from_cells(cells: impl IntoIterator<Item = (DialectId, SupportDecision)>) -> Self {
        let mut decisions: Vec<(DialectId, SupportDecision)> = cells.into_iter().collect();
        decisions.sort_by(|(left, _), (right, _)| left.cmp(right));
        decisions.dedup_by(|(left, _), (right, _)| left == right);
        Self {
            decisions: Cow::Owned(decisions),
        }
    }

    /// The decision this declaration states for `id`, or `None` if it states none.
    ///
    /// `None` means THIS DECLARATION MAKES NO CLAIM, which is not the same as
    /// `Unsupported` (an authored refusal, with a reason an operator can act on).
    /// Callers needing a verdict must choose which they mean; [`Self::decision`]
    /// panics rather than pick one silently. The distinction is the one
    /// [`crate::model::dialect_table::DispositionRow::disposition_for`] draws, for
    /// the same reason.
    #[must_use]
    pub fn decision_for(&self, id: &DialectId) -> Option<SupportDecision> {
        self.decisions
            .binary_search_by(|(dialect, _)| dialect.cmp(id))
            .ok()
            .map(|i| self.decisions[i].1)
    }

    /// The decision for one of the dialects this engine ships.
    ///
    /// Panics if this declaration states no cell for it. Deliberate, and the same
    /// contract [`crate::model::dialect_table::DispositionRow::disposition`] has:
    /// a missing cell is a declaration defect, and both of the other answers hide
    /// it — treating it as supported fails open into a render that was never
    /// declared, and treating it as unsupported invents a refusal with no reason
    /// to show anyone.
    #[must_use]
    pub fn decision(&self, dialect: SqlDialect) -> SupportDecision {
        let id = dialect.id();
        self.decision_for(&id)
            .unwrap_or_else(|| panic!("support declaration states no decision for {id}"))
    }

    /// The dialects this declaration states a decision for, in ascending id order.
    pub fn dialects(&self) -> impl Iterator<Item = DialectId> + '_ {
        self.decisions.iter().map(|(id, _)| id.clone())
    }

    /// The subset of the stated dialects whose decision is supported.
    ///
    /// ITS ONLY ASSERTION CHECKS THE EMPTINESS BIT, NOT THE MEMBERS.
    /// `op_support_matrix::support_declarations_cover_every_op_and_dialect` reads
    /// this and says "DialectSet must agree with per-dialect decisions", which
    /// reads like a cross-check of the membership. It is not, and that was
    /// MEASURED, not inferred: returning `DialectSet::all()` for every op that
    /// supports anything — wrong members for `pgRaw` and every other PG-only op,
    /// right emptiness bit — leaves the lib suite and the whole `dialect_matrix`
    /// suite reporting clean. Predates this shape (the same hole existed when
    /// this was `from_bools` over three fields); recorded so the next reader does
    /// not take the message for the guarantee.
    #[must_use]
    pub fn supported_dialects(&self) -> DialectSet {
        self.decisions
            .iter()
            .filter(|(_, decision)| decision.is_supported())
            .map(|(id, _)| id.clone())
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

// Not `Copy`: `DialectSupport` owns a `Cow` so an op's cells can be assembled at
// runtime. Every reader takes `&self`, so no call site changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeatureSupport {
    pub feature: Feature,
    pub dialects: DialectSupport,
}

impl FeatureSupport {
    #[must_use]
    pub const fn new(feature: Feature, dialects: DialectSupport) -> Self {
        Self { feature, dialects }
    }

    #[must_use]
    pub fn decision(&self, dialect: SqlDialect) -> SupportDecision {
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
    pub fn decision(&self, dialect: SqlDialect) -> SupportDecision {
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

const UNSUPPORTED: &str = crate::model::validate::CODE_UNSUPPORTED;
static POSTGRES_ONLY: [DialectId; 1] = [DialectId::new("postgres")];

// A borrowed `DialectSupport` must point at an actual static now that
// `DialectId` can own its wire spelling and therefore has a destructor. A
// reference to an inline array in a `const` would otherwise ask const-eval to
// drop the promoted ids. Each expansion gives the cells their own scoped
// static, with no allocation or leaked input.
macro_rules! declared {
    ($cells:expr) => {{
        static CELLS: DialectCells = $cells;
        DialectSupport::declared(&CELLS)
    }};
}

const PG_ONLY_TABLE_LEVEL_CHECK: DialectSupport = declared!(admitting(
    &POSTGRES_ONLY,
    RenderMode::Offline,
    unsupported(
        UNSUPPORTED,
        "table-level CHECK expression rendering is PostgreSQL-only in the current engine",
    ),
));

const PG_ONLY_SEQUENCE_DEFAULT: DialectSupport = declared!(admitting(
    &POSTGRES_ONLY,
    RenderMode::Offline,
    unsupported(
        UNSUPPORTED,
        "nextval sequence defaults are PostgreSQL-only; SQLite/MySQL have no standalone sequences",
    ),
));

const UNSUPPORTED_ALL_FK_NO_LOCAL_COLUMN: DialectSupport = declared!(on_every_dialect(
    unsupported(UNSUPPORTED, "foreign keys need at least one local column",)
));

const PORTABLE_FOREIGN_KEY: DialectSupport =
    declared!(on_every_dialect(supported(RenderMode::Offline)));

const PG_ONLY_CONSTRAINT_NOT_VALID: DialectSupport = declared!(admitting(
    &POSTGRES_ONLY,
    RenderMode::Offline,
    unsupported(
        UNSUPPORTED,
        "NOT VALID online constraint adoption (addForeignKey/addCheck { notValid }) is PostgreSQL-only; SQLite/MySQL have no NOT VALID / VALIDATE CONSTRAINT",
    ),
));

const PG_ONLY_SEQUENCE: DialectSupport = declared!(admitting(
    &POSTGRES_ONLY,
    RenderMode::Offline,
    unsupported(
        UNSUPPORTED,
        "standalone sequence objects are PostgreSQL-only in the current engine",
    ),
));

const PG_ONLY_COMMENT: DialectSupport = declared!(admitting(
    &POSTGRES_ONLY,
    RenderMode::Offline,
    unsupported(
        UNSUPPORTED,
        "COMMENT ON is PostgreSQL-only in the current engine",
    ),
));

const PG_ONLY_MATERIALIZED_VIEW: DialectSupport = declared!(admitting(
    &POSTGRES_ONLY,
    RenderMode::Offline,
    unsupported(
        UNSUPPORTED,
        "materialized views are PostgreSQL-only in the current engine",
    ),
));

const PG_ONLY_EXCLUSION_CONSTRAINT: DialectSupport = declared!(admitting(
    &POSTGRES_ONLY,
    RenderMode::Offline,
    unsupported(
        UNSUPPORTED,
        "exclusion constraints are PostgreSQL-only in the current engine",
    ),
));

const PG_ONLY_INDEX_INCLUDE: DialectSupport = declared!(admitting(
    &POSTGRES_ONLY,
    RenderMode::Offline,
    unsupported(UNSUPPORTED, "index INCLUDE columns are PostgreSQL-only"),
));

const PG_ONLY_INDEX_STORAGE_PARAMS: DialectSupport = declared!(admitting(
    &POSTGRES_ONLY,
    RenderMode::Offline,
    unsupported(
        UNSUPPORTED,
        "index WITH storage parameters are PostgreSQL-only",
    ),
));

const PG_ONLY_INDEX_ONLY: DialectSupport = declared!(admitting(
    &POSTGRES_ONLY,
    RenderMode::Offline,
    unsupported(UNSUPPORTED, "CREATE INDEX ON ONLY is PostgreSQL-only"),
));

const PG_ONLY_INDEX_NULLS_NOT_DISTINCT: DialectSupport = declared!(admitting(
    &POSTGRES_ONLY,
    RenderMode::Offline,
    unsupported(
        UNSUPPORTED,
        "UNIQUE INDEX NULLS NOT DISTINCT is PostgreSQL-only (PG 15+)",
    ),
));

const PG_ONLY_INDEX_OPCLASS: DialectSupport = declared!(admitting(
    &POSTGRES_ONLY,
    RenderMode::Offline,
    unsupported(
        UNSUPPORTED,
        "per-column index operator classes are PostgreSQL-only",
    ),
));

const PG_ONLY_INDEX_COLLATION: DialectSupport = declared!(admitting(
    &POSTGRES_ONLY,
    RenderMode::Offline,
    unsupported(
        UNSUPPORTED,
        "per-column index collations are PostgreSQL-only",
    ),
));

/// Whether an authored `ifNotExists`/`ifExists` is ENFORCED at apply. PostgreSQL,
/// SQLite, and MySQL DDL migrations carrying a probe resolve it against the live
/// catalog at the apply call site under the apply lock. Non-type MySQL verdicts
/// are delegated to the shared probe decision; present table/column creates are
/// refused until MySQL can prove modifier-preserving column-type equality.
///
/// Declares the ENFORCEMENT only. It does NOT say whether a given op accepts a
/// guard at all (`renameColumn` is refused on every dialect - see
/// `Feature::RenameColumnGuard`), and it does NOT cover `dropView`, whose lowered
/// DDL retains `DROP VIEW IF EXISTS` in addition to the catalog probe. Validation
/// never gates on this feature: it is declared so the generated support matrix
/// states the guard story per dialect.
const EXISTENCE_GUARD_PROBE: DialectSupport = declared!([
    (
        MYSQL,
        unsupported(
            UNSUPPORTED,
            "MySQL catalog probes enforce presence-only and non-column-type decisions, but any decision requiring column-type equality is refused until modifier-preserving equality is implemented",
        ),
    ),
    (POSTGRES, supported(RenderMode::LiveResolved)),
    (SQLITE, supported(RenderMode::LiveResolved)),
]);

pub(crate) const CREATE_TABLE_FEATURES: &[FeatureSupport] = &[
    FeatureSupport::new(Feature::ExistenceGuardProbe, EXISTENCE_GUARD_PROBE),
    FeatureSupport::new(Feature::SequenceDefault, PG_ONLY_SEQUENCE_DEFAULT),
    FeatureSupport::new(Feature::TableLevelCheck, PG_ONLY_TABLE_LEVEL_CHECK),
    FeatureSupport::new(
        Feature::TableLevelForeignKey,
        PORTABLE_FOREIGN_KEY,
    ),
    FeatureSupport::new(Feature::ForeignKeyNoLocalColumn, UNSUPPORTED_ALL_FK_NO_LOCAL_COLUMN),
    FeatureSupport::new(Feature::CompositeForeignKey, PORTABLE_FOREIGN_KEY),
    FeatureSupport::new(Feature::NonIdForeignKey, PORTABLE_FOREIGN_KEY),
    FeatureSupport::new(
        Feature::TableLevelUnique,
        declared!([
            (MYSQL, supported(RenderMode::Offline)),
            (POSTGRES, supported(RenderMode::Offline)),
            (
                SQLITE,
                unsupported(
                    UNSUPPORTED,
                    "SQLite createTable table-level unique constraints are not threaded into the emitter",
                ),
            ),
        ]),
    ),
    FeatureSupport::new(Feature::ExclusionConstraint, PG_ONLY_EXCLUSION_CONSTRAINT),
    FeatureSupport::new(
        Feature::ExpressionIndex,
        declared!([
            (
                MYSQL,
                unsupported(
                    UNSUPPORTED,
                    "createIndex expression elements are not supported on MySQL",
                ),
            ),
            (POSTGRES, supported(RenderMode::Offline)),
            (SQLITE, supported(RenderMode::Offline)),
        ]),
    ),
    FeatureSupport::new(
        Feature::PartialIndex,
        declared!([
            (
                MYSQL,
                unsupported(UNSUPPORTED, "MySQL does not support partial indexes"),
            ),
            (POSTGRES, supported(RenderMode::Offline)),
            (SQLITE, supported(RenderMode::Offline)),
        ]),
    ),
    FeatureSupport::new(Feature::IndexInclude, PG_ONLY_INDEX_INCLUDE),
    FeatureSupport::new(Feature::IndexStorageParams, PG_ONLY_INDEX_STORAGE_PARAMS),
    FeatureSupport::new(Feature::IndexOnly, PG_ONLY_INDEX_ONLY),
    FeatureSupport::new(Feature::IndexNullsNotDistinct, PG_ONLY_INDEX_NULLS_NOT_DISTINCT),
    FeatureSupport::new(Feature::IndexOpclass, PG_ONLY_INDEX_OPCLASS),
    FeatureSupport::new(Feature::IndexCollation, PG_ONLY_INDEX_COLLATION),
    FeatureSupport::new(
        Feature::NonBtreeIndexMethod,
        declared!(admitting(
            &POSTGRES_ONLY,
            RenderMode::Offline,
            unsupported(
                UNSUPPORTED,
                "non-btree index methods are unsupported on SQLite/MySQL",
            ),
        )),
    ),
];

pub(crate) const ADD_COLUMN_FEATURES: &[FeatureSupport] = &[
    FeatureSupport::new(Feature::ExistenceGuardProbe, EXISTENCE_GUARD_PROBE),
    FeatureSupport::new(Feature::SequenceDefault, PG_ONLY_SEQUENCE_DEFAULT),
];

pub(crate) const CREATE_INDEX_FEATURES: &[FeatureSupport] = &[
    FeatureSupport::new(Feature::ExistenceGuardProbe, EXISTENCE_GUARD_PROBE),
    FeatureSupport::new(
        Feature::ExpressionIndex,
        declared!([
            (
                MYSQL,
                unsupported(
                    UNSUPPORTED,
                    "createIndex expression elements are not supported on MySQL",
                ),
            ),
            (POSTGRES, supported(RenderMode::Offline)),
            (SQLITE, supported(RenderMode::Offline)),
        ]),
    ),
    FeatureSupport::new(
        Feature::PartialIndex,
        declared!([
            (
                MYSQL,
                unsupported(UNSUPPORTED, "MySQL does not support partial indexes"),
            ),
            (POSTGRES, supported(RenderMode::Offline)),
            (SQLITE, supported(RenderMode::Offline)),
        ]),
    ),
    FeatureSupport::new(Feature::IndexInclude, PG_ONLY_INDEX_INCLUDE),
    FeatureSupport::new(Feature::IndexStorageParams, PG_ONLY_INDEX_STORAGE_PARAMS),
    FeatureSupport::new(Feature::IndexOnly, PG_ONLY_INDEX_ONLY),
    FeatureSupport::new(
        Feature::IndexNullsNotDistinct,
        PG_ONLY_INDEX_NULLS_NOT_DISTINCT,
    ),
    FeatureSupport::new(Feature::IndexOpclass, PG_ONLY_INDEX_OPCLASS),
    FeatureSupport::new(Feature::IndexCollation, PG_ONLY_INDEX_COLLATION),
    FeatureSupport::new(
        Feature::NonBtreeIndexMethod,
        declared!(admitting(
            &POSTGRES_ONLY,
            RenderMode::Offline,
            unsupported(
                UNSUPPORTED,
                "non-btree index methods are unsupported on SQLite/MySQL",
            ),
        )),
    ),
];

pub(crate) const PARTITION_FEATURES: &[FeatureSupport] = &[
    FeatureSupport::new(Feature::ExistenceGuardProbe, EXISTENCE_GUARD_PROBE),
    FeatureSupport::new(
        Feature::PartitionDdl,
        declared!(on_every_dialect(supported(RenderMode::Offline))),
    ),
];

pub(crate) const ALTER_COLUMN_TYPE_FEATURES: &[FeatureSupport] = &[
    FeatureSupport::new(Feature::ExistenceGuardProbe, EXISTENCE_GUARD_PROBE),
    FeatureSupport::new(
        Feature::AlterColumnUsing,
        declared!(on_every_dialect(unsupported(
            UNSUPPORTED,
            "setColumnType.using expression rendering is deferred in the current engine",
        ))),
    ),
];

pub(crate) const RENAME_COLUMN_FEATURES: &[FeatureSupport] = &[FeatureSupport::new(
    Feature::RenameColumnGuard,
    declared!(on_every_dialect(unsupported(
        UNSUPPORTED,
        "renameColumn ifExists guards cannot be attributed to a single migration unit today",
    ))),
)];

pub(crate) const ADD_CONSTRAINT_FEATURES: &[FeatureSupport] = &[
    FeatureSupport::new(Feature::ExistenceGuardProbe, EXISTENCE_GUARD_PROBE),
    FeatureSupport::new(
        Feature::ForeignKeyNoLocalColumn,
        UNSUPPORTED_ALL_FK_NO_LOCAL_COLUMN,
    ),
    FeatureSupport::new(Feature::CompositeForeignKey, PORTABLE_FOREIGN_KEY),
    FeatureSupport::new(Feature::NonIdForeignKey, PORTABLE_FOREIGN_KEY),
    FeatureSupport::new(Feature::ConstraintNotValid, PG_ONLY_CONSTRAINT_NOT_VALID),
    FeatureSupport::new(Feature::TableLevelCheck, PG_ONLY_TABLE_LEVEL_CHECK),
    FeatureSupport::new(
        Feature::ExclusionConstraint,
        declared!(admitting(
            &POSTGRES_ONLY,
            RenderMode::Offline,
            unsupported(
                UNSUPPORTED,
                "exclusion constraints are PostgreSQL-only in the current engine",
            ),
        )),
    ),
];

pub(crate) const SET_COLUMN_DEFAULT_FEATURES: &[FeatureSupport] = &[
    FeatureSupport::new(Feature::ExistenceGuardProbe, EXISTENCE_GUARD_PROBE),
    FeatureSupport::new(Feature::SequenceDefault, PG_ONLY_SEQUENCE_DEFAULT),
];

pub(crate) const INSERT_FEATURES: &[FeatureSupport] = &[FeatureSupport::new(
    Feature::InsertOnConflict,
    declared!(on_every_dialect(supported(RenderMode::Offline))),
)];

pub(crate) const CREATE_VIEW_FEATURES: &[FeatureSupport] = &[
    FeatureSupport::new(
        Feature::RawViewBody,
        declared!(on_every_dialect(supported(RenderMode::Offline))),
    ),
    FeatureSupport::new(Feature::MaterializedView, PG_ONLY_MATERIALIZED_VIEW),
    FeatureSupport::new(
        Feature::CreateOrReplaceMaterializedView,
        declared!(on_every_dialect(unsupported(
            UNSUPPORTED,
            "Postgres has no CREATE OR REPLACE MATERIALIZED VIEW and the other dialects have no materialized views",
        ))),
    ),
];

pub(crate) const CREATE_TRIGGER_FEATURES: &[FeatureSupport] = &[
    FeatureSupport::new(
        Feature::TriggerExecuteFunction,
        declared!(admitting(
            &POSTGRES_ONLY,
            RenderMode::Offline,
            unsupported(
                UNSUPPORTED,
                "SQLite/MySQL have no CREATE TRIGGER EXECUTE FUNCTION form",
            ),
        )),
    ),
    FeatureSupport::new(
        Feature::TriggerBody,
        declared!([
            (MYSQL, supported(RenderMode::Offline)),
            (
                POSTGRES,
                unsupported(
                    UNSUPPORTED,
                    "Postgres triggers must execute a named trigger function",
                ),
            ),
            (SQLITE, supported(RenderMode::Offline)),
        ]),
    ),
    FeatureSupport::new(
        Feature::TriggerTruncateEvent,
        declared!(admitting(
            &POSTGRES_ONLY,
            RenderMode::Offline,
            unsupported(UNSUPPORTED, "SQLite/MySQL have no TRUNCATE trigger event"),
        )),
    ),
    FeatureSupport::new(
        Feature::TriggerStatementForEach,
        declared!(admitting(
            &POSTGRES_ONLY,
            RenderMode::Offline,
            unsupported(UNSUPPORTED, "SQLite/MySQL triggers are row-level only"),
        )),
    ),
    FeatureSupport::new(
        Feature::TriggerMultipleEvents,
        // SQLite shares MySQL's constraint here, and used to be declared supported
        // beside a message that already spelled out why it could not be. Both
        // grammars take exactly one event; only PostgreSQL accepts `INSERT OR
        // UPDATE`. Declaring SQLite supported meant the engine lowered the
        // PostgreSQL spelling and SQLite's parser rejected it AFTER the migration's
        // earlier statements had run - `near "OR": syntax error`, mid-deploy.
        declared!([
            (
                MYSQL,
                unsupported(
                    UNSUPPORTED,
                    "MySQL CREATE TRIGGER accepts exactly one trigger event",
                ),
            ),
            (POSTGRES, supported(RenderMode::Offline)),
            (
                SQLITE,
                unsupported(
                    UNSUPPORTED,
                    "SQLite CREATE TRIGGER accepts exactly one trigger event",
                ),
            ),
        ]),
    ),
    FeatureSupport::new(
        Feature::TriggerInsteadOfTiming,
        declared!([
            (
                MYSQL,
                unsupported(UNSUPPORTED, "MySQL does not support INSTEAD OF triggers"),
            ),
            (POSTGRES, supported(RenderMode::Offline)),
            (SQLITE, supported(RenderMode::Offline)),
        ]),
    ),
    FeatureSupport::new(
        Feature::TriggerWhen,
        declared!([
            (
                MYSQL,
                unsupported(UNSUPPORTED, "MySQL triggers do not support WHEN predicates"),
            ),
            (POSTGRES, supported(RenderMode::Offline)),
            (SQLITE, supported(RenderMode::Offline)),
        ]),
    ),
    FeatureSupport::new(
        Feature::TriggerRaiseIgnore,
        declared!([
            (
                MYSQL,
                unsupported(UNSUPPORTED, "MySQL cannot render RAISE IGNORE"),
            ),
            (
                POSTGRES,
                unsupported(
                    UNSUPPORTED,
                    "Postgres trigger bodies are unsupported; named functions must be used",
                ),
            ),
            (SQLITE, supported(RenderMode::Offline)),
        ]),
    ),
];

pub(crate) const COMMENT_FEATURES: &[FeatureSupport] =
    &[FeatureSupport::new(Feature::Comment, PG_ONLY_COMMENT)];

pub(crate) const SEQUENCE_FEATURES: &[FeatureSupport] =
    &[FeatureSupport::new(Feature::Sequence, PG_ONLY_SEQUENCE)];

pub(crate) const PG_RAW_FEATURES: &[FeatureSupport] = &[FeatureSupport::new(
    Feature::RawSql,
    declared!(admitting(
        &POSTGRES_ONLY,
        RenderMode::Offline,
        unsupported(UNSUPPORTED, "pgRaw statements are PostgreSQL-only"),
    )),
)];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::op_support::FEATURE_SUPPORT_REGISTRY;
    use std::collections::BTreeSet;
    use zero_migrate_ir::dialect::DialectId;

    /// Every hand-written [`DialectSupport`] this module declares, labelled by
    /// the registry group and feature it came from.
    ///
    /// This is a DISCOVERED set — it is only as wide as the registry and the
    /// `*_FEATURES` arrays happen to be — so every test that scans it needs the
    /// floor in [`every_declaration_covers_the_shipping_census`].
    fn declarations() -> Vec<(String, &'static DialectSupport)> {
        FEATURE_SUPPORT_REGISTRY
            .iter()
            .flat_map(|group| {
                group.features().iter().map(move |feature| {
                    (
                        format!("{} / {:?}", group.label(), feature.feature),
                        &feature.dialects,
                    )
                })
            })
            .collect()
    }

    #[test]
    fn the_shipping_census_is_the_engine_dialect_set() {
        assert_eq!(
            SHIPPING_DIALECTS.iter().cloned().collect::<DialectSet>(),
            DialectSet::all(),
            "SHIPPING_DIALECTS and DialectSet::all name different backends"
        );
        assert!(
            SHIPPING_DIALECTS.windows(2).all(|pair| pair[0] < pair[1]),
            "SHIPPING_DIALECTS must be sorted and deduplicated: every declaration \
             built from it is binary-searched"
        );
        for dialect in [SqlDialect::Postgres, SqlDialect::Sqlite, SqlDialect::Mysql] {
            assert!(
                SHIPPING_DIALECTS.contains(&dialect.id()),
                "{dialect:?} is a closed-enum variant with no cell in the census"
            );
        }
    }

    #[test]
    fn every_declaration_covers_the_shipping_census() {
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

        let census: BTreeSet<DialectId> = SHIPPING_DIALECTS.iter().cloned().collect();
        assert!(
            census.len() >= 3,
            "the shipping dialect census collapsed to {} ({census:?})",
            census.len()
        );

        for (label, support) in &declarations {
            let cells: Vec<DialectId> = support.dialects().collect();
            assert_eq!(
                cells.iter().cloned().collect::<BTreeSet<_>>(),
                census,
                "{label} declares {cells:?}, not the shipping census {census:?} — a \
                 declaration that names fewer dialects makes no claim where it used to"
            );
            assert!(
                cells.windows(2).all(|pair| pair[0] < pair[1]),
                "{label} declares {cells:?}, which is not sorted and deduplicated by \
                 DialectId — `decision_for` binary-searches these cells"
            );
        }
    }

    #[test]
    fn a_dialect_the_engine_does_not_ship_gets_no_claim_not_a_verdict() {
        let duckdb = DialectId::new("duckdb");
        assert!(
            !SHIPPING_DIALECTS.contains(&duckdb),
            "this test needs an id the engine does not ship"
        );
        for (label, support) in declarations() {
            assert_eq!(
                support.decision_for(&duckdb),
                None,
                "{label} answers for a dialect it never declared; NO CLAIM and a \
                 refusal are different answers and only one of them is honest here"
            );
        }
    }
}
