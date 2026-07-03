//! Static support declarations for the migration op DSL.
//!
//! Validation consumes these declarations before expression walking. Lowering
//! still keeps defensive guards for invalid direct callers, but author-facing
//! dialect/refusal diagnostics are sourced from this support matrix.

use crate::model::capability::VendorCapability;

pub use crate::model::validate::Dialect;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DialectSet(u8);

impl DialectSet {
    const POSTGRES: u8 = 0b001;
    const SQLITE: u8 = 0b010;
    const MYSQL: u8 = 0b100;

    #[must_use]
    pub const fn empty() -> Self {
        Self(0)
    }

    #[must_use]
    pub const fn all() -> Self {
        Self(Self::POSTGRES | Self::SQLITE | Self::MYSQL)
    }

    #[must_use]
    pub const fn from_bools(postgres: bool, sqlite: bool, mysql: bool) -> Self {
        let mut bits = 0;
        if postgres {
            bits |= Self::POSTGRES;
        }
        if sqlite {
            bits |= Self::SQLITE;
        }
        if mysql {
            bits |= Self::MYSQL;
        }
        Self(bits)
    }

    #[must_use]
    pub const fn contains(self, dialect: Dialect) -> bool {
        let bit = match dialect {
            Dialect::Postgres => Self::POSTGRES,
            Dialect::Sqlite => Self::SQLITE,
            Dialect::Mysql => Self::MYSQL,
        };
        self.0 & bit != 0
    }

    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }
}

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DialectSupport {
    pub postgres: SupportDecision,
    pub sqlite: SupportDecision,
    pub mysql: SupportDecision,
}

impl DialectSupport {
    #[must_use]
    pub const fn new(
        postgres: SupportDecision,
        sqlite: SupportDecision,
        mysql: SupportDecision,
    ) -> Self {
        Self {
            postgres,
            sqlite,
            mysql,
        }
    }

    #[must_use]
    pub const fn all_supported(render: RenderMode) -> Self {
        Self::new(supported(render), supported(render), supported(render))
    }

    #[must_use]
    pub const fn unsupported_all(code: &'static str, reason: &'static str) -> Self {
        Self::new(
            unsupported(code, reason),
            unsupported(code, reason),
            unsupported(code, reason),
        )
    }

    #[must_use]
    pub const fn postgres_only(render: RenderMode, reason: &'static str) -> Self {
        Self::new(
            supported(render),
            unsupported(crate::model::validate::CODE_UNSUPPORTED, reason),
            unsupported(crate::model::validate::CODE_UNSUPPORTED, reason),
        )
    }

    #[must_use]
    pub const fn decision(self, dialect: Dialect) -> SupportDecision {
        match dialect {
            Dialect::Postgres => self.postgres,
            Dialect::Sqlite => self.sqlite,
            Dialect::Mysql => self.mysql,
        }
    }

    #[must_use]
    pub const fn supported_dialects(self) -> DialectSet {
        DialectSet::from_bools(
            self.postgres.is_supported(),
            self.sqlite.is_supported(),
            self.mysql.is_supported(),
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SupportTier {
    Core,
    Vendor(&'static [VendorCapability]),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Feature {
    UserPrimaryKey,
    PartialIndex,
    ExpressionIndex,
    NonBtreeIndexMethod,
    TableLevelForeignKey,
    TableLevelUnique,
    TableLevelCheck,
    CompositeForeignKey,
    ForeignKeyNoLocalColumn,
    NonIdForeignKey,
    ExclusionConstraint,
    NativeAlterColumn,
    AlterColumnUsing,
    SynthDefault,
    RenameColumnGuard,
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
    pub const fn decision(self, dialect: Dialect) -> SupportDecision {
        self.dialects.decision(dialect)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
    pub const fn core(
        dialects: DialectSupport,
        features: &'static [FeatureSupport],
    ) -> Self {
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
    pub const fn decision(&self, dialect: Dialect) -> SupportDecision {
        self.dialects.decision(dialect)
    }

    #[must_use]
    pub const fn supported_dialects(&self) -> DialectSet {
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
pub(crate) const CAP_POLICY: &[VendorCapability] = &[VendorCapability::Policy];
pub(crate) const CAP_FUNCTION: &[VendorCapability] = &[VendorCapability::Function];
pub(crate) const CAP_RAW_SQL: &[VendorCapability] = &[VendorCapability::RawSql];
pub(crate) const CAP_MATERIALIZED_VIEW: &[VendorCapability] =
    &[VendorCapability::MaterializedView];
pub(crate) const CAP_RAW_MATERIALIZED_VIEW: &[VendorCapability] = &[
    VendorCapability::RawViewBody,
    VendorCapability::MaterializedView,
];

const UNSUPPORTED: &str = crate::model::validate::CODE_UNSUPPORTED;

const UNSUPPORTED_ALL_PRIMARY_KEY: DialectSupport = DialectSupport::unsupported_all(
    UNSUPPORTED,
    "standalone/table-level primary key constraints are deferred; createTable.primaryKey is validated by the active table-shape policy",
);

const PG_ONLY_TABLE_LEVEL_CHECK: DialectSupport = DialectSupport::postgres_only(
    RenderMode::Offline,
    "table-level CHECK expression rendering is PostgreSQL-only in the current engine",
);

const UNSUPPORTED_ALL_SYNTH_DEFAULT: DialectSupport = DialectSupport::unsupported_all(
    UNSUPPORTED,
    "synth default rendering is deferred until the expression/default renderer lands",
);

const PG_ONLY_SYNTH_DEFAULT: DialectSupport = DialectSupport::postgres_only(
    RenderMode::Offline,
    "synth default rendering is PostgreSQL-only in the current engine",
);

const UNSUPPORTED_ALL_FK_NO_LOCAL_COLUMN: DialectSupport =
    DialectSupport::unsupported_all(UNSUPPORTED, "foreign keys need exactly one local column");

const UNSUPPORTED_ALL_COMPOSITE_FK: DialectSupport =
    DialectSupport::unsupported_all(UNSUPPORTED, "multi-column foreign keys are a later wave");

const UNSUPPORTED_ALL_NON_ID_FK: DialectSupport = DialectSupport::unsupported_all(
    UNSUPPORTED,
    "foreign keys referencing non-id columns are a later wave",
);

const PG_ONLY_SEQUENCE: DialectSupport = DialectSupport::postgres_only(
    RenderMode::Offline,
    "standalone sequence objects are PostgreSQL-only in the current engine",
);

const PG_ONLY_COMMENT: DialectSupport = DialectSupport::postgres_only(
    RenderMode::Offline,
    "COMMENT ON is PostgreSQL-only in the current engine",
);

const PG_ONLY_MATERIALIZED_VIEW: DialectSupport = DialectSupport::postgres_only(
    RenderMode::Offline,
    "materialized views are PostgreSQL-only in the current engine",
);

const PG_ONLY_EXCLUSION_CONSTRAINT: DialectSupport = DialectSupport::postgres_only(
    RenderMode::Offline,
    "exclusion constraints are PostgreSQL-only in the current engine",
);

pub(crate) const CREATE_TABLE_FEATURES: &[FeatureSupport] = &[
    FeatureSupport::new(Feature::UserPrimaryKey, UNSUPPORTED_ALL_PRIMARY_KEY),
    FeatureSupport::new(Feature::SynthDefault, PG_ONLY_SYNTH_DEFAULT),
    FeatureSupport::new(Feature::TableLevelCheck, PG_ONLY_TABLE_LEVEL_CHECK),
    FeatureSupport::new(
        Feature::TableLevelForeignKey,
        DialectSupport::new(
            supported(RenderMode::Offline),
            unsupported(
                UNSUPPORTED,
                "SQLite createTable table-level foreign keys are not threaded into the emitter",
            ),
            supported(RenderMode::Offline),
        ),
    ),
    FeatureSupport::new(Feature::ForeignKeyNoLocalColumn, UNSUPPORTED_ALL_FK_NO_LOCAL_COLUMN),
    FeatureSupport::new(Feature::CompositeForeignKey, UNSUPPORTED_ALL_COMPOSITE_FK),
    FeatureSupport::new(Feature::NonIdForeignKey, UNSUPPORTED_ALL_NON_ID_FK),
    FeatureSupport::new(
        Feature::TableLevelUnique,
        DialectSupport::new(
            supported(RenderMode::Offline),
            unsupported(
                UNSUPPORTED,
                "SQLite createTable table-level unique constraints are not threaded into the emitter",
            ),
            supported(RenderMode::Offline),
        ),
    ),
    FeatureSupport::new(Feature::ExclusionConstraint, PG_ONLY_EXCLUSION_CONSTRAINT),
    FeatureSupport::new(
        Feature::ExpressionIndex,
        DialectSupport::new(
            supported(RenderMode::Offline),
            supported(RenderMode::Offline),
            unsupported(
                UNSUPPORTED,
                "createIndex expression elements are not supported on MySQL",
            ),
        ),
    ),
    FeatureSupport::new(
        Feature::PartialIndex,
        DialectSupport::new(
            supported(RenderMode::Offline),
            supported(RenderMode::Offline),
            unsupported(
                UNSUPPORTED,
                "MySQL does not support partial indexes",
            ),
        ),
    ),
    FeatureSupport::new(
        Feature::NonBtreeIndexMethod,
        DialectSupport::postgres_only(
            RenderMode::Offline,
            "non-btree index methods are unsupported on SQLite/MySQL",
        ),
    ),
];

pub(crate) const ADD_COLUMN_FEATURES: &[FeatureSupport] =
    &[FeatureSupport::new(Feature::SynthDefault, PG_ONLY_SYNTH_DEFAULT)];

pub(crate) const CREATE_INDEX_FEATURES: &[FeatureSupport] = &[
    FeatureSupport::new(
        Feature::ExpressionIndex,
        DialectSupport::new(
            supported(RenderMode::Offline),
            supported(RenderMode::Offline),
            unsupported(
                UNSUPPORTED,
                "createIndex expression elements are not supported on MySQL",
            ),
        ),
    ),
    FeatureSupport::new(
        Feature::PartialIndex,
        DialectSupport::new(
            supported(RenderMode::Offline),
            supported(RenderMode::Offline),
            unsupported(
                UNSUPPORTED,
                "MySQL does not support partial indexes",
            ),
        ),
    ),
    FeatureSupport::new(
        Feature::NonBtreeIndexMethod,
        DialectSupport::postgres_only(
            RenderMode::Offline,
            "non-btree index methods are unsupported on SQLite/MySQL",
        ),
    ),
];

pub(crate) const ALTER_COLUMN_TYPE_FEATURES: &[FeatureSupport] = &[FeatureSupport::new(
    Feature::AlterColumnUsing,
    DialectSupport::unsupported_all(
        UNSUPPORTED,
        "setColumnType.using expression rendering is deferred in the current engine",
    ),
)];

pub(crate) const RENAME_COLUMN_FEATURES: &[FeatureSupport] = &[FeatureSupport::new(
    Feature::RenameColumnGuard,
    DialectSupport::unsupported_all(
        UNSUPPORTED,
        "renameColumn ifExists guards cannot be attributed to a single migration unit today",
    ),
)];

pub(crate) const ADD_CONSTRAINT_FEATURES: &[FeatureSupport] = &[
    FeatureSupport::new(Feature::UserPrimaryKey, UNSUPPORTED_ALL_PRIMARY_KEY),
    FeatureSupport::new(Feature::ForeignKeyNoLocalColumn, UNSUPPORTED_ALL_FK_NO_LOCAL_COLUMN),
    FeatureSupport::new(Feature::CompositeForeignKey, UNSUPPORTED_ALL_COMPOSITE_FK),
    FeatureSupport::new(Feature::NonIdForeignKey, UNSUPPORTED_ALL_NON_ID_FK),
    FeatureSupport::new(Feature::TableLevelCheck, PG_ONLY_TABLE_LEVEL_CHECK),
    FeatureSupport::new(
        Feature::ExclusionConstraint,
        DialectSupport::postgres_only(
            RenderMode::Offline,
            "exclusion constraints are PostgreSQL-only in the current engine",
        ),
    ),
];

pub(crate) const SET_COLUMN_DEFAULT_FEATURES: &[FeatureSupport] =
    &[FeatureSupport::new(Feature::SynthDefault, UNSUPPORTED_ALL_SYNTH_DEFAULT)];

pub(crate) const INSERT_FEATURES: &[FeatureSupport] = &[FeatureSupport::new(
    Feature::InsertOnConflict,
    DialectSupport::postgres_only(
        RenderMode::Offline,
        "insert onConflict is PostgreSQL-only in the current engine",
    ),
)];

pub(crate) const CREATE_VIEW_FEATURES: &[FeatureSupport] = &[
    FeatureSupport::new(Feature::RawViewBody, DialectSupport::all_supported(RenderMode::Offline)),
    FeatureSupport::new(Feature::MaterializedView, PG_ONLY_MATERIALIZED_VIEW),
    FeatureSupport::new(
        Feature::CreateOrReplaceMaterializedView,
        DialectSupport::unsupported_all(
            UNSUPPORTED,
            "Postgres has no CREATE OR REPLACE MATERIALIZED VIEW and the other dialects have no materialized views",
        ),
    ),
];

pub(crate) const CREATE_TRIGGER_FEATURES: &[FeatureSupport] = &[
    FeatureSupport::new(
        Feature::TriggerExecuteFunction,
        DialectSupport::postgres_only(
            RenderMode::Offline,
            "SQLite/MySQL have no CREATE TRIGGER EXECUTE FUNCTION form",
        ),
    ),
    FeatureSupport::new(
        Feature::TriggerBody,
        DialectSupport::new(
            unsupported(
                UNSUPPORTED,
                "Postgres triggers must execute a named trigger function",
            ),
            supported(RenderMode::Offline),
            supported(RenderMode::Offline),
        ),
    ),
    FeatureSupport::new(
        Feature::TriggerTruncateEvent,
        DialectSupport::postgres_only(
            RenderMode::Offline,
            "SQLite/MySQL have no TRUNCATE trigger event",
        ),
    ),
    FeatureSupport::new(
        Feature::TriggerStatementForEach,
        DialectSupport::postgres_only(
            RenderMode::Offline,
            "SQLite/MySQL triggers are row-level only",
        ),
    ),
    FeatureSupport::new(
        Feature::TriggerMultipleEvents,
        DialectSupport::new(
            supported(RenderMode::Offline),
            supported(RenderMode::Offline),
            unsupported(
                UNSUPPORTED,
                "MySQL CREATE TRIGGER accepts exactly one trigger event",
            ),
        ),
    ),
    FeatureSupport::new(
        Feature::TriggerInsteadOfTiming,
        DialectSupport::new(
            supported(RenderMode::Offline),
            supported(RenderMode::Offline),
            unsupported(UNSUPPORTED, "MySQL does not support INSTEAD OF triggers"),
        ),
    ),
    FeatureSupport::new(
        Feature::TriggerWhen,
        DialectSupport::new(
            supported(RenderMode::Offline),
            supported(RenderMode::Offline),
            unsupported(UNSUPPORTED, "MySQL triggers do not support WHEN predicates"),
        ),
    ),
    FeatureSupport::new(
        Feature::TriggerRaiseIgnore,
        DialectSupport::new(
            unsupported(
                UNSUPPORTED,
                "Postgres trigger bodies are unsupported; named functions must be used",
            ),
            supported(RenderMode::Offline),
            unsupported(UNSUPPORTED, "MySQL cannot render RAISE IGNORE"),
        ),
    ),
];

pub(crate) const COMMENT_FEATURES: &[FeatureSupport] =
    &[FeatureSupport::new(Feature::Comment, PG_ONLY_COMMENT)];

pub(crate) const SEQUENCE_FEATURES: &[FeatureSupport] =
    &[FeatureSupport::new(Feature::Sequence, PG_ONLY_SEQUENCE)];

pub(crate) const PG_RAW_FEATURES: &[FeatureSupport] = &[
    FeatureSupport::new(
        Feature::RawSql,
        DialectSupport::postgres_only(
            RenderMode::Offline,
            "pgRaw statements are PostgreSQL-only",
        ),
    ),
];
