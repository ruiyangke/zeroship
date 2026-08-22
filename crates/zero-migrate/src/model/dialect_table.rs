//! GENERATED FILE — do not edit by hand.
//! Source: crates/zero-migrate/dialect-support.toml (the single-source
//! dialect-support sidecar). Regenerate with:
//!   pnpm --filter zero-migrate gen:dialect-table
//!
//! One [`DispositionRow`] per (op-kind, variant) recording the token's
//! disposition on each dialect, KEYED BY [`DialectId`] rather than by one struct
//! field per vendor. That keying is the point: a fourth backend adds a column to
//! the sidecar and nothing here, in the generator, or in core changes shape.
//!
//! Which test proves what: `tests/dialect_table_faithfulness.rs` proves the
//! corpus ⟷ table bijection and the sidecar ⟷ table transcription (it CANNOT
//! prove agreement with `Op::support()` — `Op::support()` reads this table, so
//! that check would be tautological). `op_support_matrix.rs` is the behavioural
//! gate; `dialect_conformance_live.rs` is the live one.
//!
//! Engine code DOES read this table: `crate::model::op_support` calls
//! `dialect_table::lookup(kind, variant)`. The TypeScript mirror has no reader
//! outside its own file; state the two sides separately, because they have
//! already drifted apart once.

use crate::model::support::SqlDialect;
use zero_migrate_ir::dialect::DialectId;

/// The disposition of one (op-kind, variant) token on one dialect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Disposition {
    /// Core construct that renders/validates on this dialect.
    Portable,
    /// Native where supported, absence-tolerable elsewhere.
    ///
    /// LIVE, not reserved. This doc used to end "Reserved; no current row uses
    /// it" and both halves were false: `createPartition/base` and
    /// `createTable/partitionedCollapse` carry it on both non-PostgreSQL
    /// dialects, `op_support` groups it with portable and vendor as SUPPORTED,
    /// and lowering acts on it by collapsing a partition child into its parent
    /// behind a mirror guard. The sidecar's own legend carried the same error
    /// until it was measured; this is the last copy of it.
    TransparentDegradable,
    /// Vendor-tier construct admitted on this dialect.
    Vendor,
    /// Refused on this dialect.
    Unsupported,
}

/// One row of the generated dialect table: an (op-kind, variant) token and its
/// per-dialect disposition.
///
/// The dispositions are an ASSOCIATION LIST keyed by [`DialectId`], not one field
/// per vendor. The previous shape put every dialect this engine ships in the type
/// itself, so a fourth backend could not declare its dispositions without editing
/// a struct in a crate it does not own — the same closed-set problem
/// [`DialectId`] exists to remove, in a shape that is not an enum and so was not
/// removed by deleting one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DispositionRow {
    /// The op-kind wire token (e.g. `"createTable"`).
    pub kind: &'static str,
    /// The variant token distinguishing payload-dependent branches; `"base"`
    /// for payload-independent ops.
    pub variant: &'static str,
    /// This token's disposition per dialect, sorted by [`DialectId`] and
    /// deduplicated — the same sorted-slice discipline
    /// [`zero_migrate_ir::dialect::DialectSet`] uses — so lookup is a binary
    /// search and the emitted order is stable.
    pub dispositions: &'static [(DialectId, Disposition)],
}

impl DispositionRow {
    /// The disposition this row declares for `id`, or `None` if it declares none.
    ///
    /// `None` means THE TABLE MAKES NO CLAIM, which is not the same as
    /// `Unsupported` (an explicit refusal). Callers that need a verdict must
    /// decide which they mean; [`Self::disposition`] panics rather than pick one
    /// silently.
    #[must_use]
    pub fn disposition_for(&self, id: &DialectId) -> Option<Disposition> {
        self.dispositions
            .binary_search_by(|(dialect, _)| dialect.cmp(id))
            .ok()
            .map(|i| self.dispositions[i].1)
    }

    /// The disposition of this row on the given dialect.
    ///
    /// Panics if the row declares no cell for it. That is deliberate and matches
    /// the existing contract of [`lookup`]'s caller, which already panics on a
    /// missing ROW: a missing cell is a generation defect, and both of the other
    /// answers hide it — treating it as supported fails open, and treating it as
    /// `Unsupported` invents a refusal the sidecar never authored.
    #[must_use]
    pub fn disposition(&self, dialect: SqlDialect) -> Disposition {
        let id = dialect.id();
        self.disposition_for(&id).unwrap_or_else(|| {
            panic!(
                "dialect table row {}/{} declares no disposition for {id}",
                self.kind, self.variant
            )
        })
    }

    /// The dialects this row declares a disposition for, in ascending id order.
    pub fn dialects(&self) -> impl Iterator<Item = DialectId> + '_ {
        self.dispositions.iter().map(|(id, _)| id.clone())
    }
}

/// The generated dialect table, sorted by (kind, variant).
///
/// `#[rustfmt::skip]` keeps each row on one line: this file is generator-owned
/// (the drift test byte-compares it against `gen:dialect-table`), and letting
/// `cargo fmt` reflow the rows would put the committed form permanently at odds
/// with the generator's output.
#[rustfmt::skip]
pub const DIALECT_TABLE: &[DispositionRow] = &[
    DispositionRow { kind: "addColumn", variant: "base", dispositions: &[(DialectId::new("mysql"), Disposition::Portable), (DialectId::new("postgres"), Disposition::Portable), (DialectId::new("sqlite"), Disposition::Portable)] },
    DispositionRow { kind: "addColumn", variant: "identity", dispositions: &[(DialectId::new("mysql"), Disposition::Unsupported), (DialectId::new("postgres"), Disposition::Portable), (DialectId::new("sqlite"), Disposition::Unsupported)] },
    DispositionRow { kind: "addColumn", variant: "nextvalDefault", dispositions: &[(DialectId::new("mysql"), Disposition::Unsupported), (DialectId::new("postgres"), Disposition::Portable), (DialectId::new("sqlite"), Disposition::Unsupported)] },
    DispositionRow { kind: "addConstraint", variant: "check", dispositions: &[(DialectId::new("mysql"), Disposition::Unsupported), (DialectId::new("postgres"), Disposition::Portable), (DialectId::new("sqlite"), Disposition::Unsupported)] },
    DispositionRow { kind: "addConstraint", variant: "exclusion", dispositions: &[(DialectId::new("mysql"), Disposition::Unsupported), (DialectId::new("postgres"), Disposition::Portable), (DialectId::new("sqlite"), Disposition::Unsupported)] },
    DispositionRow { kind: "addConstraint", variant: "fkComposite", dispositions: &[(DialectId::new("mysql"), Disposition::Portable), (DialectId::new("postgres"), Disposition::Portable), (DialectId::new("sqlite"), Disposition::Portable)] },
    DispositionRow { kind: "addConstraint", variant: "fkNoLocalColumn", dispositions: &[(DialectId::new("mysql"), Disposition::Unsupported), (DialectId::new("postgres"), Disposition::Unsupported), (DialectId::new("sqlite"), Disposition::Unsupported)] },
    DispositionRow { kind: "addConstraint", variant: "fkNonId", dispositions: &[(DialectId::new("mysql"), Disposition::Portable), (DialectId::new("postgres"), Disposition::Portable), (DialectId::new("sqlite"), Disposition::Portable)] },
    DispositionRow { kind: "addConstraint", variant: "fkNotValid", dispositions: &[(DialectId::new("mysql"), Disposition::Unsupported), (DialectId::new("postgres"), Disposition::Portable), (DialectId::new("sqlite"), Disposition::Unsupported)] },
    DispositionRow { kind: "addConstraint", variant: "fkSimple", dispositions: &[(DialectId::new("mysql"), Disposition::Portable), (DialectId::new("postgres"), Disposition::Portable), (DialectId::new("sqlite"), Disposition::Portable)] },
    DispositionRow { kind: "addConstraint", variant: "unique", dispositions: &[(DialectId::new("mysql"), Disposition::Portable), (DialectId::new("postgres"), Disposition::Portable), (DialectId::new("sqlite"), Disposition::Unsupported)] },
    DispositionRow { kind: "alterPrimaryKey", variant: "base", dispositions: &[(DialectId::new("mysql"), Disposition::Portable), (DialectId::new("postgres"), Disposition::Portable), (DialectId::new("sqlite"), Disposition::Portable)] },
    DispositionRow { kind: "alterRole", variant: "base", dispositions: &[(DialectId::new("mysql"), Disposition::Unsupported), (DialectId::new("postgres"), Disposition::Vendor), (DialectId::new("sqlite"), Disposition::Unsupported)] },
    DispositionRow { kind: "alterSequence", variant: "base", dispositions: &[(DialectId::new("mysql"), Disposition::Unsupported), (DialectId::new("postgres"), Disposition::Portable), (DialectId::new("sqlite"), Disposition::Unsupported)] },
    DispositionRow { kind: "attachPartition", variant: "base", dispositions: &[(DialectId::new("mysql"), Disposition::Unsupported), (DialectId::new("postgres"), Disposition::Vendor), (DialectId::new("sqlite"), Disposition::Unsupported)] },
    DispositionRow { kind: "backfill", variant: "base", dispositions: &[(DialectId::new("mysql"), Disposition::Portable), (DialectId::new("postgres"), Disposition::Portable), (DialectId::new("sqlite"), Disposition::Portable)] },
    DispositionRow { kind: "comment", variant: "base", dispositions: &[(DialectId::new("mysql"), Disposition::Unsupported), (DialectId::new("postgres"), Disposition::Portable), (DialectId::new("sqlite"), Disposition::Unsupported)] },
    DispositionRow { kind: "createDomain", variant: "base", dispositions: &[(DialectId::new("mysql"), Disposition::Portable), (DialectId::new("postgres"), Disposition::Portable), (DialectId::new("sqlite"), Disposition::Portable)] },
    DispositionRow { kind: "createDomain", variant: "nextvalDefault", dispositions: &[(DialectId::new("mysql"), Disposition::Unsupported), (DialectId::new("postgres"), Disposition::Portable), (DialectId::new("sqlite"), Disposition::Unsupported)] },
    DispositionRow { kind: "createEnum", variant: "base", dispositions: &[(DialectId::new("mysql"), Disposition::Portable), (DialectId::new("postgres"), Disposition::Portable), (DialectId::new("sqlite"), Disposition::Portable)] },
    DispositionRow { kind: "createExtension", variant: "base", dispositions: &[(DialectId::new("mysql"), Disposition::Unsupported), (DialectId::new("postgres"), Disposition::Vendor), (DialectId::new("sqlite"), Disposition::Unsupported)] },
    DispositionRow { kind: "createFunction", variant: "base", dispositions: &[(DialectId::new("mysql"), Disposition::Unsupported), (DialectId::new("postgres"), Disposition::Vendor), (DialectId::new("sqlite"), Disposition::Unsupported)] },
    DispositionRow { kind: "createIndex", variant: "base", dispositions: &[(DialectId::new("mysql"), Disposition::Portable), (DialectId::new("postgres"), Disposition::Portable), (DialectId::new("sqlite"), Disposition::Portable)] },
    DispositionRow { kind: "createIndex", variant: "exprElement", dispositions: &[(DialectId::new("mysql"), Disposition::Unsupported), (DialectId::new("postgres"), Disposition::Portable), (DialectId::new("sqlite"), Disposition::Portable)] },
    DispositionRow { kind: "createIndex", variant: "partialWhere", dispositions: &[(DialectId::new("mysql"), Disposition::Unsupported), (DialectId::new("postgres"), Disposition::Portable), (DialectId::new("sqlite"), Disposition::Portable)] },
    DispositionRow { kind: "createIndex", variant: "pgOnlyMethodOrFeature", dispositions: &[(DialectId::new("mysql"), Disposition::Unsupported), (DialectId::new("postgres"), Disposition::Portable), (DialectId::new("sqlite"), Disposition::Unsupported)] },
    DispositionRow { kind: "createPartition", variant: "base", dispositions: &[(DialectId::new("mysql"), Disposition::TransparentDegradable), (DialectId::new("postgres"), Disposition::Portable), (DialectId::new("sqlite"), Disposition::TransparentDegradable)] },
    DispositionRow { kind: "createPolicy", variant: "base", dispositions: &[(DialectId::new("mysql"), Disposition::Unsupported), (DialectId::new("postgres"), Disposition::Vendor), (DialectId::new("sqlite"), Disposition::Unsupported)] },
    DispositionRow { kind: "createRole", variant: "base", dispositions: &[(DialectId::new("mysql"), Disposition::Unsupported), (DialectId::new("postgres"), Disposition::Vendor), (DialectId::new("sqlite"), Disposition::Unsupported)] },
    DispositionRow { kind: "createRole", variant: "superuserIfNotExists", dispositions: &[(DialectId::new("mysql"), Disposition::Unsupported), (DialectId::new("postgres"), Disposition::Unsupported), (DialectId::new("sqlite"), Disposition::Unsupported)] },
    DispositionRow { kind: "createSchema", variant: "base", dispositions: &[(DialectId::new("mysql"), Disposition::Unsupported), (DialectId::new("postgres"), Disposition::Vendor), (DialectId::new("sqlite"), Disposition::Unsupported)] },
    DispositionRow { kind: "createSequence", variant: "base", dispositions: &[(DialectId::new("mysql"), Disposition::Unsupported), (DialectId::new("postgres"), Disposition::Portable), (DialectId::new("sqlite"), Disposition::Unsupported)] },
    DispositionRow { kind: "createTable", variant: "base", dispositions: &[(DialectId::new("mysql"), Disposition::Portable), (DialectId::new("postgres"), Disposition::Portable), (DialectId::new("sqlite"), Disposition::Portable)] },
    DispositionRow { kind: "createTable", variant: "identityAlways", dispositions: &[(DialectId::new("mysql"), Disposition::Unsupported), (DialectId::new("postgres"), Disposition::Portable), (DialectId::new("sqlite"), Disposition::Unsupported)] },
    DispositionRow { kind: "createTable", variant: "nextvalDefault", dispositions: &[(DialectId::new("mysql"), Disposition::Unsupported), (DialectId::new("postgres"), Disposition::Portable), (DialectId::new("sqlite"), Disposition::Unsupported)] },
    DispositionRow { kind: "createTable", variant: "nonportableByDefaultIdentity", dispositions: &[(DialectId::new("mysql"), Disposition::Unsupported), (DialectId::new("postgres"), Disposition::Portable), (DialectId::new("sqlite"), Disposition::Unsupported)] },
    DispositionRow { kind: "createTable", variant: "partitioned", dispositions: &[(DialectId::new("mysql"), Disposition::Unsupported), (DialectId::new("postgres"), Disposition::Portable), (DialectId::new("sqlite"), Disposition::Unsupported)] },
    DispositionRow { kind: "createTable", variant: "partitionedCollapse", dispositions: &[(DialectId::new("mysql"), Disposition::TransparentDegradable), (DialectId::new("postgres"), Disposition::Portable), (DialectId::new("sqlite"), Disposition::TransparentDegradable)] },
    DispositionRow { kind: "createTable", variant: "pgOnlyIndexFeature", dispositions: &[(DialectId::new("mysql"), Disposition::Unsupported), (DialectId::new("postgres"), Disposition::Portable), (DialectId::new("sqlite"), Disposition::Unsupported)] },
    DispositionRow { kind: "createTrigger", variant: "bodyInsteadOf", dispositions: &[(DialectId::new("mysql"), Disposition::Unsupported), (DialectId::new("postgres"), Disposition::Unsupported), (DialectId::new("sqlite"), Disposition::Portable)] },
    DispositionRow { kind: "createTrigger", variant: "bodyMultipleEvents", dispositions: &[(DialectId::new("mysql"), Disposition::Unsupported), (DialectId::new("postgres"), Disposition::Unsupported), (DialectId::new("sqlite"), Disposition::Unsupported)] },
    DispositionRow { kind: "createTrigger", variant: "bodyRaiseIgnore", dispositions: &[(DialectId::new("mysql"), Disposition::Unsupported), (DialectId::new("postgres"), Disposition::Unsupported), (DialectId::new("sqlite"), Disposition::Portable)] },
    DispositionRow { kind: "createTrigger", variant: "bodySimple", dispositions: &[(DialectId::new("mysql"), Disposition::Portable), (DialectId::new("postgres"), Disposition::Unsupported), (DialectId::new("sqlite"), Disposition::Portable)] },
    DispositionRow { kind: "createTrigger", variant: "bodyStatementLevel", dispositions: &[(DialectId::new("mysql"), Disposition::Unsupported), (DialectId::new("postgres"), Disposition::Unsupported), (DialectId::new("sqlite"), Disposition::Unsupported)] },
    DispositionRow { kind: "createTrigger", variant: "bodyTruncateEvent", dispositions: &[(DialectId::new("mysql"), Disposition::Unsupported), (DialectId::new("postgres"), Disposition::Unsupported), (DialectId::new("sqlite"), Disposition::Unsupported)] },
    DispositionRow { kind: "createTrigger", variant: "bodyWhen", dispositions: &[(DialectId::new("mysql"), Disposition::Unsupported), (DialectId::new("postgres"), Disposition::Unsupported), (DialectId::new("sqlite"), Disposition::Portable)] },
    DispositionRow { kind: "createTrigger", variant: "executeFunction", dispositions: &[(DialectId::new("mysql"), Disposition::Unsupported), (DialectId::new("postgres"), Disposition::Portable), (DialectId::new("sqlite"), Disposition::Unsupported)] },
    DispositionRow { kind: "createView", variant: "base", dispositions: &[(DialectId::new("mysql"), Disposition::Portable), (DialectId::new("postgres"), Disposition::Portable), (DialectId::new("sqlite"), Disposition::Portable)] },
    DispositionRow { kind: "createView", variant: "materialized", dispositions: &[(DialectId::new("mysql"), Disposition::Unsupported), (DialectId::new("postgres"), Disposition::Vendor), (DialectId::new("sqlite"), Disposition::Unsupported)] },
    DispositionRow { kind: "createView", variant: "materializedReplace", dispositions: &[(DialectId::new("mysql"), Disposition::Unsupported), (DialectId::new("postgres"), Disposition::Unsupported), (DialectId::new("sqlite"), Disposition::Unsupported)] },
    DispositionRow { kind: "delete", variant: "base", dispositions: &[(DialectId::new("mysql"), Disposition::Portable), (DialectId::new("postgres"), Disposition::Portable), (DialectId::new("sqlite"), Disposition::Portable)] },
    DispositionRow { kind: "detachPartition", variant: "base", dispositions: &[(DialectId::new("mysql"), Disposition::Unsupported), (DialectId::new("postgres"), Disposition::Portable), (DialectId::new("sqlite"), Disposition::Unsupported)] },
    DispositionRow { kind: "dialectal", variant: "base", dispositions: &[(DialectId::new("mysql"), Disposition::Portable), (DialectId::new("postgres"), Disposition::Portable), (DialectId::new("sqlite"), Disposition::Portable)] },
    DispositionRow { kind: "dropColumn", variant: "base", dispositions: &[(DialectId::new("mysql"), Disposition::Portable), (DialectId::new("postgres"), Disposition::Portable), (DialectId::new("sqlite"), Disposition::Portable)] },
    DispositionRow { kind: "dropColumnDefault", variant: "base", dispositions: &[(DialectId::new("mysql"), Disposition::Portable), (DialectId::new("postgres"), Disposition::Portable), (DialectId::new("sqlite"), Disposition::Unsupported)] },
    DispositionRow { kind: "dropColumnNotNull", variant: "base", dispositions: &[(DialectId::new("mysql"), Disposition::Unsupported), (DialectId::new("postgres"), Disposition::Portable), (DialectId::new("sqlite"), Disposition::Unsupported)] },
    DispositionRow { kind: "dropConstraint", variant: "base", dispositions: &[(DialectId::new("mysql"), Disposition::Portable), (DialectId::new("postgres"), Disposition::Portable), (DialectId::new("sqlite"), Disposition::Portable)] },
    DispositionRow { kind: "dropDomain", variant: "base", dispositions: &[(DialectId::new("mysql"), Disposition::Portable), (DialectId::new("postgres"), Disposition::Portable), (DialectId::new("sqlite"), Disposition::Portable)] },
    DispositionRow { kind: "dropEnum", variant: "base", dispositions: &[(DialectId::new("mysql"), Disposition::Portable), (DialectId::new("postgres"), Disposition::Portable), (DialectId::new("sqlite"), Disposition::Portable)] },
    DispositionRow { kind: "dropExtension", variant: "base", dispositions: &[(DialectId::new("mysql"), Disposition::Unsupported), (DialectId::new("postgres"), Disposition::Vendor), (DialectId::new("sqlite"), Disposition::Unsupported)] },
    DispositionRow { kind: "dropFunction", variant: "base", dispositions: &[(DialectId::new("mysql"), Disposition::Unsupported), (DialectId::new("postgres"), Disposition::Vendor), (DialectId::new("sqlite"), Disposition::Unsupported)] },
    DispositionRow { kind: "dropIndex", variant: "base", dispositions: &[(DialectId::new("mysql"), Disposition::Portable), (DialectId::new("postgres"), Disposition::Portable), (DialectId::new("sqlite"), Disposition::Portable)] },
    DispositionRow { kind: "dropOwnedBy", variant: "base", dispositions: &[(DialectId::new("mysql"), Disposition::Unsupported), (DialectId::new("postgres"), Disposition::Vendor), (DialectId::new("sqlite"), Disposition::Unsupported)] },
    DispositionRow { kind: "dropPartition", variant: "base", dispositions: &[(DialectId::new("mysql"), Disposition::Portable), (DialectId::new("postgres"), Disposition::Portable), (DialectId::new("sqlite"), Disposition::Portable)] },
    DispositionRow { kind: "dropPolicy", variant: "base", dispositions: &[(DialectId::new("mysql"), Disposition::Unsupported), (DialectId::new("postgres"), Disposition::Vendor), (DialectId::new("sqlite"), Disposition::Unsupported)] },
    DispositionRow { kind: "dropRole", variant: "base", dispositions: &[(DialectId::new("mysql"), Disposition::Unsupported), (DialectId::new("postgres"), Disposition::Vendor), (DialectId::new("sqlite"), Disposition::Unsupported)] },
    DispositionRow { kind: "dropSchema", variant: "base", dispositions: &[(DialectId::new("mysql"), Disposition::Unsupported), (DialectId::new("postgres"), Disposition::Vendor), (DialectId::new("sqlite"), Disposition::Unsupported)] },
    DispositionRow { kind: "dropSequence", variant: "base", dispositions: &[(DialectId::new("mysql"), Disposition::Unsupported), (DialectId::new("postgres"), Disposition::Portable), (DialectId::new("sqlite"), Disposition::Unsupported)] },
    DispositionRow { kind: "dropTable", variant: "base", dispositions: &[(DialectId::new("mysql"), Disposition::Portable), (DialectId::new("postgres"), Disposition::Portable), (DialectId::new("sqlite"), Disposition::Portable)] },
    DispositionRow { kind: "dropTrigger", variant: "base", dispositions: &[(DialectId::new("mysql"), Disposition::Portable), (DialectId::new("postgres"), Disposition::Portable), (DialectId::new("sqlite"), Disposition::Portable)] },
    DispositionRow { kind: "dropView", variant: "base", dispositions: &[(DialectId::new("mysql"), Disposition::Portable), (DialectId::new("postgres"), Disposition::Portable), (DialectId::new("sqlite"), Disposition::Portable)] },
    DispositionRow { kind: "dropView", variant: "materialized", dispositions: &[(DialectId::new("mysql"), Disposition::Unsupported), (DialectId::new("postgres"), Disposition::Vendor), (DialectId::new("sqlite"), Disposition::Unsupported)] },
    DispositionRow { kind: "grant", variant: "base", dispositions: &[(DialectId::new("mysql"), Disposition::Unsupported), (DialectId::new("postgres"), Disposition::Vendor), (DialectId::new("sqlite"), Disposition::Unsupported)] },
    DispositionRow { kind: "insert", variant: "base", dispositions: &[(DialectId::new("mysql"), Disposition::Portable), (DialectId::new("postgres"), Disposition::Portable), (DialectId::new("sqlite"), Disposition::Portable)] },
    DispositionRow { kind: "insert", variant: "onConflictDoNothing", dispositions: &[(DialectId::new("mysql"), Disposition::Unsupported), (DialectId::new("postgres"), Disposition::Portable), (DialectId::new("sqlite"), Disposition::Portable)] },
    DispositionRow { kind: "insert", variant: "onConflictDoUpdate", dispositions: &[(DialectId::new("mysql"), Disposition::Portable), (DialectId::new("postgres"), Disposition::Portable), (DialectId::new("sqlite"), Disposition::Portable)] },
    DispositionRow { kind: "pgRaw", variant: "base", dispositions: &[(DialectId::new("mysql"), Disposition::Unsupported), (DialectId::new("postgres"), Disposition::Vendor), (DialectId::new("sqlite"), Disposition::Unsupported)] },
    DispositionRow { kind: "renameColumn", variant: "base", dispositions: &[(DialectId::new("mysql"), Disposition::Unsupported), (DialectId::new("postgres"), Disposition::Portable), (DialectId::new("sqlite"), Disposition::Portable)] },
    DispositionRow { kind: "renameColumn", variant: "existenceGuard", dispositions: &[(DialectId::new("mysql"), Disposition::Unsupported), (DialectId::new("postgres"), Disposition::Unsupported), (DialectId::new("sqlite"), Disposition::Unsupported)] },
    DispositionRow { kind: "renameTable", variant: "base", dispositions: &[(DialectId::new("mysql"), Disposition::Portable), (DialectId::new("postgres"), Disposition::Portable), (DialectId::new("sqlite"), Disposition::Portable)] },
    DispositionRow { kind: "revoke", variant: "base", dispositions: &[(DialectId::new("mysql"), Disposition::Unsupported), (DialectId::new("postgres"), Disposition::Vendor), (DialectId::new("sqlite"), Disposition::Unsupported)] },
    DispositionRow { kind: "setColumnDefault", variant: "base", dispositions: &[(DialectId::new("mysql"), Disposition::Portable), (DialectId::new("postgres"), Disposition::Portable), (DialectId::new("sqlite"), Disposition::Unsupported)] },
    DispositionRow { kind: "setColumnDefault", variant: "containerOrJson", dispositions: &[(DialectId::new("mysql"), Disposition::Portable), (DialectId::new("postgres"), Disposition::Portable), (DialectId::new("sqlite"), Disposition::Unsupported)] },
    DispositionRow { kind: "setColumnDefault", variant: "nextval", dispositions: &[(DialectId::new("mysql"), Disposition::Unsupported), (DialectId::new("postgres"), Disposition::Portable), (DialectId::new("sqlite"), Disposition::Unsupported)] },
    DispositionRow { kind: "setColumnNotNull", variant: "base", dispositions: &[(DialectId::new("mysql"), Disposition::Unsupported), (DialectId::new("postgres"), Disposition::Portable), (DialectId::new("sqlite"), Disposition::Unsupported)] },
    DispositionRow { kind: "setColumnType", variant: "base", dispositions: &[(DialectId::new("mysql"), Disposition::Portable), (DialectId::new("postgres"), Disposition::Portable), (DialectId::new("sqlite"), Disposition::Unsupported)] },
    DispositionRow { kind: "setColumnType", variant: "using", dispositions: &[(DialectId::new("mysql"), Disposition::Unsupported), (DialectId::new("postgres"), Disposition::Unsupported), (DialectId::new("sqlite"), Disposition::Unsupported)] },
    DispositionRow { kind: "setRls", variant: "base", dispositions: &[(DialectId::new("mysql"), Disposition::Unsupported), (DialectId::new("postgres"), Disposition::Vendor), (DialectId::new("sqlite"), Disposition::Unsupported)] },
    DispositionRow { kind: "setTableOptions", variant: "base", dispositions: &[(DialectId::new("mysql"), Disposition::Portable), (DialectId::new("postgres"), Disposition::Portable), (DialectId::new("sqlite"), Disposition::Portable)] },
    DispositionRow { kind: "synchronizeIdentity", variant: "base", dispositions: &[(DialectId::new("mysql"), Disposition::Portable), (DialectId::new("postgres"), Disposition::Portable), (DialectId::new("sqlite"), Disposition::Portable)] },
    DispositionRow { kind: "update", variant: "base", dispositions: &[(DialectId::new("mysql"), Disposition::Portable), (DialectId::new("postgres"), Disposition::Portable), (DialectId::new("sqlite"), Disposition::Portable)] },
    DispositionRow { kind: "validateConstraint", variant: "base", dispositions: &[(DialectId::new("mysql"), Disposition::Unsupported), (DialectId::new("postgres"), Disposition::Portable), (DialectId::new("sqlite"), Disposition::Unsupported)] },
];

/// Look up the row for an (op-kind, variant) token, if present.
#[must_use]
pub fn lookup(kind: &str, variant: &str) -> Option<&'static DispositionRow> {
    DIALECT_TABLE
        .iter()
        .find(|row| row.kind == kind && row.variant == variant)
}
