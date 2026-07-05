//! GENERATED FILE — do not edit by hand.
//! Source: crates/zeroship-migrate/dialect-support.toml (the single-source
//! dialect-support sidecar). Regenerate with:
//!   pnpm --filter @zeroship/migrate gen:dialect-table
//!
//! One [`DispositionRow`] per (op-kind, variant) recording the token's
//! disposition on each dialect. Faithfulness to the engine's live
//! `Support::decision()` is proven by
//! `tests/dialect_table_faithfulness.rs`. S0.1 is ADDITIVE — no engine code
//! consumes this table yet (that is S0.2).

use crate::model::support::Dialect;

/// The disposition of one (op-kind, variant) token on one dialect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Disposition {
    /// Core construct that renders/validates on this dialect.
    Portable,
    /// P12 — native where supported, absence-tolerable elsewhere. Reserved for
    /// the redesign; no current row uses it.
    TransparentDegradable,
    /// Vendor-tier construct admitted on this dialect.
    Vendor,
    /// Refused on this dialect.
    Unsupported,
}

/// One row of the generated dialect table: an (op-kind, variant) token and its
/// per-dialect disposition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DispositionRow {
    /// The op-kind wire token (e.g. `"createTable"`).
    pub kind: &'static str,
    /// The variant token distinguishing payload-dependent branches; `"base"`
    /// for payload-independent ops.
    pub variant: &'static str,
    /// Disposition on PostgreSQL.
    pub postgres: Disposition,
    /// Disposition on SQLite.
    pub sqlite: Disposition,
    /// Disposition on MySQL.
    pub mysql: Disposition,
}

impl DispositionRow {
    /// The disposition of this row on the given dialect.
    #[must_use]
    pub const fn disposition(&self, dialect: Dialect) -> Disposition {
        match dialect {
            Dialect::Postgres => self.postgres,
            Dialect::Sqlite => self.sqlite,
            Dialect::Mysql => self.mysql,
        }
    }
}

/// The generated dialect table, sorted by (kind, variant).
pub const DIALECT_TABLE: &[DispositionRow] = &[
    DispositionRow { kind: "addColumn", variant: "base", postgres: Disposition::Portable, sqlite: Disposition::Portable, mysql: Disposition::Portable },
    DispositionRow { kind: "addColumn", variant: "identity", postgres: Disposition::Portable, sqlite: Disposition::Unsupported, mysql: Disposition::Unsupported },
    DispositionRow { kind: "addColumn", variant: "nextvalDefault", postgres: Disposition::Portable, sqlite: Disposition::Unsupported, mysql: Disposition::Unsupported },
    DispositionRow { kind: "addConstraint", variant: "check", postgres: Disposition::Portable, sqlite: Disposition::Unsupported, mysql: Disposition::Unsupported },
    DispositionRow { kind: "addConstraint", variant: "exclusion", postgres: Disposition::Portable, sqlite: Disposition::Unsupported, mysql: Disposition::Unsupported },
    DispositionRow { kind: "addConstraint", variant: "fkComposite", postgres: Disposition::Portable, sqlite: Disposition::Unsupported, mysql: Disposition::Unsupported },
    DispositionRow { kind: "addConstraint", variant: "fkNoLocalColumn", postgres: Disposition::Unsupported, sqlite: Disposition::Unsupported, mysql: Disposition::Unsupported },
    DispositionRow { kind: "addConstraint", variant: "fkNonId", postgres: Disposition::Portable, sqlite: Disposition::Unsupported, mysql: Disposition::Unsupported },
    DispositionRow { kind: "addConstraint", variant: "fkNotValid", postgres: Disposition::Portable, sqlite: Disposition::Unsupported, mysql: Disposition::Unsupported },
    DispositionRow { kind: "addConstraint", variant: "fkSimple", postgres: Disposition::Portable, sqlite: Disposition::Portable, mysql: Disposition::Portable },
    DispositionRow { kind: "addConstraint", variant: "pk", postgres: Disposition::Unsupported, sqlite: Disposition::Unsupported, mysql: Disposition::Unsupported },
    DispositionRow { kind: "addConstraint", variant: "unique", postgres: Disposition::Portable, sqlite: Disposition::Portable, mysql: Disposition::Portable },
    DispositionRow { kind: "alterRole", variant: "base", postgres: Disposition::Vendor, sqlite: Disposition::Unsupported, mysql: Disposition::Unsupported },
    DispositionRow { kind: "alterSequence", variant: "base", postgres: Disposition::Portable, sqlite: Disposition::Unsupported, mysql: Disposition::Unsupported },
    DispositionRow { kind: "backfill", variant: "base", postgres: Disposition::Portable, sqlite: Disposition::Portable, mysql: Disposition::Portable },
    DispositionRow { kind: "comment", variant: "base", postgres: Disposition::Portable, sqlite: Disposition::Unsupported, mysql: Disposition::Unsupported },
    DispositionRow { kind: "createDomain", variant: "base", postgres: Disposition::Portable, sqlite: Disposition::Portable, mysql: Disposition::Portable },
    DispositionRow { kind: "createDomain", variant: "nextvalDefault", postgres: Disposition::Portable, sqlite: Disposition::Unsupported, mysql: Disposition::Unsupported },
    DispositionRow { kind: "createEnum", variant: "base", postgres: Disposition::Portable, sqlite: Disposition::Portable, mysql: Disposition::Portable },
    DispositionRow { kind: "createExtension", variant: "base", postgres: Disposition::Vendor, sqlite: Disposition::Unsupported, mysql: Disposition::Unsupported },
    DispositionRow { kind: "createFunction", variant: "base", postgres: Disposition::Vendor, sqlite: Disposition::Unsupported, mysql: Disposition::Unsupported },
    DispositionRow { kind: "createIndex", variant: "base", postgres: Disposition::Portable, sqlite: Disposition::Portable, mysql: Disposition::Portable },
    DispositionRow { kind: "createIndex", variant: "exprElement", postgres: Disposition::Portable, sqlite: Disposition::Portable, mysql: Disposition::Unsupported },
    DispositionRow { kind: "createIndex", variant: "partialWhere", postgres: Disposition::Portable, sqlite: Disposition::Portable, mysql: Disposition::Unsupported },
    DispositionRow { kind: "createIndex", variant: "pgOnlyMethodOrFeature", postgres: Disposition::Portable, sqlite: Disposition::Unsupported, mysql: Disposition::Unsupported },
    DispositionRow { kind: "createPartition", variant: "base", postgres: Disposition::Portable, sqlite: Disposition::Unsupported, mysql: Disposition::Unsupported },
    DispositionRow { kind: "createPolicy", variant: "base", postgres: Disposition::Vendor, sqlite: Disposition::Unsupported, mysql: Disposition::Unsupported },
    DispositionRow { kind: "createRole", variant: "base", postgres: Disposition::Vendor, sqlite: Disposition::Unsupported, mysql: Disposition::Unsupported },
    DispositionRow { kind: "createRole", variant: "superuserIfNotExists", postgres: Disposition::Unsupported, sqlite: Disposition::Unsupported, mysql: Disposition::Unsupported },
    DispositionRow { kind: "createSchema", variant: "base", postgres: Disposition::Vendor, sqlite: Disposition::Unsupported, mysql: Disposition::Unsupported },
    DispositionRow { kind: "createSequence", variant: "base", postgres: Disposition::Portable, sqlite: Disposition::Unsupported, mysql: Disposition::Unsupported },
    DispositionRow { kind: "createTable", variant: "base", postgres: Disposition::Portable, sqlite: Disposition::Portable, mysql: Disposition::Portable },
    DispositionRow { kind: "createTable", variant: "identityAlways", postgres: Disposition::Portable, sqlite: Disposition::Unsupported, mysql: Disposition::Unsupported },
    DispositionRow { kind: "createTable", variant: "nextvalDefault", postgres: Disposition::Portable, sqlite: Disposition::Unsupported, mysql: Disposition::Unsupported },
    DispositionRow { kind: "createTable", variant: "nonportableByDefaultIdentity", postgres: Disposition::Portable, sqlite: Disposition::Unsupported, mysql: Disposition::Unsupported },
    DispositionRow { kind: "createTable", variant: "partitioned", postgres: Disposition::Portable, sqlite: Disposition::Unsupported, mysql: Disposition::Unsupported },
    DispositionRow { kind: "createTable", variant: "pgOnlyIndexFeature", postgres: Disposition::Portable, sqlite: Disposition::Unsupported, mysql: Disposition::Unsupported },
    DispositionRow { kind: "createTrigger", variant: "bodyInsteadOf", postgres: Disposition::Unsupported, sqlite: Disposition::Portable, mysql: Disposition::Unsupported },
    DispositionRow { kind: "createTrigger", variant: "bodyMultipleEvents", postgres: Disposition::Unsupported, sqlite: Disposition::Portable, mysql: Disposition::Unsupported },
    DispositionRow { kind: "createTrigger", variant: "bodyRaiseIgnore", postgres: Disposition::Unsupported, sqlite: Disposition::Portable, mysql: Disposition::Unsupported },
    DispositionRow { kind: "createTrigger", variant: "bodySimple", postgres: Disposition::Unsupported, sqlite: Disposition::Portable, mysql: Disposition::Portable },
    DispositionRow { kind: "createTrigger", variant: "bodyStatementLevel", postgres: Disposition::Unsupported, sqlite: Disposition::Unsupported, mysql: Disposition::Unsupported },
    DispositionRow { kind: "createTrigger", variant: "bodyTruncateEvent", postgres: Disposition::Unsupported, sqlite: Disposition::Unsupported, mysql: Disposition::Unsupported },
    DispositionRow { kind: "createTrigger", variant: "bodyWhen", postgres: Disposition::Unsupported, sqlite: Disposition::Portable, mysql: Disposition::Unsupported },
    DispositionRow { kind: "createTrigger", variant: "executeFunction", postgres: Disposition::Portable, sqlite: Disposition::Unsupported, mysql: Disposition::Unsupported },
    DispositionRow { kind: "createView", variant: "base", postgres: Disposition::Portable, sqlite: Disposition::Portable, mysql: Disposition::Portable },
    DispositionRow { kind: "createView", variant: "materialized", postgres: Disposition::Vendor, sqlite: Disposition::Unsupported, mysql: Disposition::Unsupported },
    DispositionRow { kind: "createView", variant: "materializedReplace", postgres: Disposition::Unsupported, sqlite: Disposition::Unsupported, mysql: Disposition::Unsupported },
    DispositionRow { kind: "delete", variant: "base", postgres: Disposition::Portable, sqlite: Disposition::Portable, mysql: Disposition::Portable },
    DispositionRow { kind: "detachPartition", variant: "base", postgres: Disposition::Portable, sqlite: Disposition::Unsupported, mysql: Disposition::Unsupported },
    DispositionRow { kind: "disableRls", variant: "base", postgres: Disposition::Vendor, sqlite: Disposition::Unsupported, mysql: Disposition::Unsupported },
    DispositionRow { kind: "dropColumn", variant: "base", postgres: Disposition::Portable, sqlite: Disposition::Portable, mysql: Disposition::Portable },
    DispositionRow { kind: "dropColumnDefault", variant: "base", postgres: Disposition::Portable, sqlite: Disposition::Portable, mysql: Disposition::Portable },
    DispositionRow { kind: "dropColumnNotNull", variant: "base", postgres: Disposition::Portable, sqlite: Disposition::Portable, mysql: Disposition::Portable },
    DispositionRow { kind: "dropConstraint", variant: "base", postgres: Disposition::Portable, sqlite: Disposition::Portable, mysql: Disposition::Portable },
    DispositionRow { kind: "dropDomain", variant: "base", postgres: Disposition::Portable, sqlite: Disposition::Portable, mysql: Disposition::Portable },
    DispositionRow { kind: "dropEnum", variant: "base", postgres: Disposition::Portable, sqlite: Disposition::Portable, mysql: Disposition::Portable },
    DispositionRow { kind: "dropExtension", variant: "base", postgres: Disposition::Vendor, sqlite: Disposition::Unsupported, mysql: Disposition::Unsupported },
    DispositionRow { kind: "dropFunction", variant: "base", postgres: Disposition::Vendor, sqlite: Disposition::Unsupported, mysql: Disposition::Unsupported },
    DispositionRow { kind: "dropIndex", variant: "base", postgres: Disposition::Portable, sqlite: Disposition::Portable, mysql: Disposition::Portable },
    DispositionRow { kind: "dropOwnedBy", variant: "base", postgres: Disposition::Vendor, sqlite: Disposition::Unsupported, mysql: Disposition::Unsupported },
    DispositionRow { kind: "dropPartition", variant: "base", postgres: Disposition::Portable, sqlite: Disposition::Unsupported, mysql: Disposition::Unsupported },
    DispositionRow { kind: "dropPolicy", variant: "base", postgres: Disposition::Vendor, sqlite: Disposition::Unsupported, mysql: Disposition::Unsupported },
    DispositionRow { kind: "dropRole", variant: "base", postgres: Disposition::Vendor, sqlite: Disposition::Unsupported, mysql: Disposition::Unsupported },
    DispositionRow { kind: "dropSchema", variant: "base", postgres: Disposition::Vendor, sqlite: Disposition::Unsupported, mysql: Disposition::Unsupported },
    DispositionRow { kind: "dropSequence", variant: "base", postgres: Disposition::Portable, sqlite: Disposition::Unsupported, mysql: Disposition::Unsupported },
    DispositionRow { kind: "dropTable", variant: "base", postgres: Disposition::Portable, sqlite: Disposition::Portable, mysql: Disposition::Portable },
    DispositionRow { kind: "dropTrigger", variant: "base", postgres: Disposition::Portable, sqlite: Disposition::Portable, mysql: Disposition::Portable },
    DispositionRow { kind: "dropView", variant: "base", postgres: Disposition::Portable, sqlite: Disposition::Portable, mysql: Disposition::Portable },
    DispositionRow { kind: "dropView", variant: "materialized", postgres: Disposition::Vendor, sqlite: Disposition::Unsupported, mysql: Disposition::Unsupported },
    DispositionRow { kind: "enableRls", variant: "base", postgres: Disposition::Vendor, sqlite: Disposition::Unsupported, mysql: Disposition::Unsupported },
    DispositionRow { kind: "forceRls", variant: "base", postgres: Disposition::Vendor, sqlite: Disposition::Unsupported, mysql: Disposition::Unsupported },
    DispositionRow { kind: "grant", variant: "base", postgres: Disposition::Vendor, sqlite: Disposition::Unsupported, mysql: Disposition::Unsupported },
    DispositionRow { kind: "insert", variant: "base", postgres: Disposition::Portable, sqlite: Disposition::Portable, mysql: Disposition::Portable },
    DispositionRow { kind: "insert", variant: "onConflict", postgres: Disposition::Portable, sqlite: Disposition::Unsupported, mysql: Disposition::Unsupported },
    DispositionRow { kind: "noForceRls", variant: "base", postgres: Disposition::Vendor, sqlite: Disposition::Unsupported, mysql: Disposition::Unsupported },
    DispositionRow { kind: "pgRaw", variant: "base", postgres: Disposition::Vendor, sqlite: Disposition::Unsupported, mysql: Disposition::Unsupported },
    DispositionRow { kind: "renameColumn", variant: "base", postgres: Disposition::Portable, sqlite: Disposition::Portable, mysql: Disposition::Unsupported },
    DispositionRow { kind: "renameColumn", variant: "existenceGuard", postgres: Disposition::Unsupported, sqlite: Disposition::Unsupported, mysql: Disposition::Unsupported },
    DispositionRow { kind: "renameTable", variant: "base", postgres: Disposition::Portable, sqlite: Disposition::Portable, mysql: Disposition::Portable },
    DispositionRow { kind: "revoke", variant: "base", postgres: Disposition::Vendor, sqlite: Disposition::Unsupported, mysql: Disposition::Unsupported },
    DispositionRow { kind: "setColumnDefault", variant: "base", postgres: Disposition::Portable, sqlite: Disposition::Portable, mysql: Disposition::Portable },
    DispositionRow { kind: "setColumnDefault", variant: "containerOrJson", postgres: Disposition::Portable, sqlite: Disposition::Portable, mysql: Disposition::Portable },
    DispositionRow { kind: "setColumnDefault", variant: "fn", postgres: Disposition::Unsupported, sqlite: Disposition::Unsupported, mysql: Disposition::Unsupported },
    DispositionRow { kind: "setColumnDefault", variant: "nextval", postgres: Disposition::Portable, sqlite: Disposition::Unsupported, mysql: Disposition::Unsupported },
    DispositionRow { kind: "setColumnNotNull", variant: "base", postgres: Disposition::Portable, sqlite: Disposition::Portable, mysql: Disposition::Portable },
    DispositionRow { kind: "setColumnType", variant: "base", postgres: Disposition::Portable, sqlite: Disposition::Portable, mysql: Disposition::Portable },
    DispositionRow { kind: "setColumnType", variant: "using", postgres: Disposition::Unsupported, sqlite: Disposition::Unsupported, mysql: Disposition::Unsupported },
    DispositionRow { kind: "setTableOptions", variant: "base", postgres: Disposition::Portable, sqlite: Disposition::Portable, mysql: Disposition::Portable },
    DispositionRow { kind: "update", variant: "base", postgres: Disposition::Portable, sqlite: Disposition::Portable, mysql: Disposition::Portable },
    DispositionRow { kind: "validateConstraint", variant: "base", postgres: Disposition::Portable, sqlite: Disposition::Unsupported, mysql: Disposition::Unsupported },
];

/// Look up the row for an (op-kind, variant) token, if present.
#[must_use]
pub fn lookup(kind: &str, variant: &str) -> Option<&'static DispositionRow> {
    DIALECT_TABLE
        .iter()
        .find(|row| row.kind == kind && row.variant == variant)
}
