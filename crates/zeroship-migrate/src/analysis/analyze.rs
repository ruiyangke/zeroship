//! Operational safety analyzers — Atlas-style **advisory** lint suite (v3 Plan B).
//!
//! These analyzers flag migrations that are *operationally risky but not a
//! security threat*: data loss, backward-incompatible renames, lock-heavy DDL,
//! full-table rewrites, un-validated constraints, missing FK indexes. They emit
//! [`Advisory`]s — a `Warning`/`Notice` severity carrying a human message and a
//! safer-alternative `suggestion` — so the AI/creator (and the declarative
//! differ) can see the footgun and the expand-contract path before applying.
//!
//! # These are ADVISORY, NEVER load-bearing for security
//!
//! The security boundary is [`crate::guard`] (the parse-time deny-list +
//! cross-schema confinement) plus the least-privilege `migrator` role. The
//! destructive-data-loss gate is the engine's approval gate
//! ([`crate::engine::MigrationEngine::apply`]). **Nothing here denies, blocks,
//! or gates anything.** An analyzer that fails to fire (a false negative) is a
//! quality regression, NOT a security hole — the guard and the role still reject
//! the dangerous *security* surface, and the approval gate still confirms data
//! loss. Conversely, a spurious advisory (false positive) is noise, never a
//! denial. Do not move a security check here, and do not rely on an analyzer to
//! stop an attack.
//!
//! # Surface
//!
//! - [`analyze`] runs every analyzer over a SQL string and returns the
//!   advisories. It is the engine for the guard's `GuardReport.advisories` and
//!   for [`analyze_migration`].
//! - [`analyze_migration`] runs [`analyze`] over a [`Migration`]'s `up` — the
//!   seam the declarative differ (and a future plan UI) uses to attach
//!   operational advisories to each generated migration (e.g. a gated `DROP` or
//!   a `SET NOT NULL` surfaces the expand-contract suggestion).

use pg_query::protobuf::node::Node as NodeEnum;
use pg_query::protobuf::{self, AlterTableType, ConstrType, ObjectType};

use crate::model::migration::Migration;

/// The severity of an [`Advisory`]. Advisory-only — neither level denies or
/// gates; both are informational signals about an operational footgun.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Severity {
    /// A risky operation likely to cause downtime, data loss, or break running
    /// code (a lock-heavy rewrite, a destructive drop, a backward-incompatible
    /// rename). The migration still applies — this is a heads-up, not a denial.
    Warning,
    /// A softer performance/footprint note (e.g. an FK column with no supporting
    /// index). Worth fixing, lower urgency than a [`Severity::Warning`].
    Notice,
}

/// One operational advisory emitted by an analyzer.
///
/// Carries a stable [`rule`](Self::rule) id (so callers can suppress/route a
/// specific analyzer), a [`severity`](Self::severity), a human
/// [`message`](Self::message) describing the footgun, and an optional
/// [`suggestion`](Self::suggestion) naming the safer alternative (usually the
/// expand-contract path).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Advisory {
    /// The stable analyzer rule id (see [`rule`]). Distinct from the guard's
    /// deny-list `rule` namespace — these never deny.
    pub rule: &'static str,
    /// How urgent the advisory is.
    pub severity: Severity,
    /// A human-readable description of the operational risk.
    pub message: String,
    /// The safer alternative to suggest, if any.
    pub suggestion: Option<String>,
}

impl Advisory {
    /// Build a `Warning`-severity advisory.
    fn warning(rule: &'static str, message: String, suggestion: &str) -> Self {
        Self {
            rule,
            severity: Severity::Warning,
            message,
            suggestion: Some(suggestion.to_string()),
        }
    }

    /// Build a `Notice`-severity advisory.
    fn notice(rule: &'static str, message: String, suggestion: &str) -> Self {
        Self {
            rule,
            severity: Severity::Notice,
            message,
            suggestion: Some(suggestion.to_string()),
        }
    }

    /// Build the structured warning emitted by `data_security.destructive_ops = "warn"`.
    pub(crate) fn destructive_ops_warn(operation: &str, statement: &str) -> Self {
        Self {
            rule: rule::DATA_SECURITY_DESTRUCTIVE_OPS_WARN,
            severity: Severity::Warning,
            message: format!(
                "data_security.destructive_ops=warn: {operation} is destructive and requires review: {statement}"
            ),
            suggestion: Some(
                "review this destructive migration explicitly, or set destructive_ops=\"forbid\" to refuse it"
                    .to_string(),
            ),
        }
    }

    /// Build the structured warning emitted when
    /// `data_security.destructive_ops = "warn"` sees an unclassified statement.
    pub(crate) fn destructive_ops_unknown_warn(statement: &str) -> Self {
        Self {
            rule: rule::DATA_SECURITY_UNCLASSIFIED_OPS_WARN,
            severity: Severity::Warning,
            message: format!(
                "data_security.destructive_ops=warn: statement is not positively classified as non-destructive and requires review: {statement}"
            ),
            suggestion: Some(
                "review this migration explicitly; destructive_ops=\"forbid\" refuses unclassified statements fail-closed"
                    .to_string(),
            ),
        }
    }
}

/// The stable advisory rule ids — **data, not logic**, mirroring the guard's
/// `denylist::rule` convention. These are NOT security rules; they never deny.
pub mod rule {
    /// `data_security.destructive_ops = "warn"` surfaced a destructive operation.
    pub const DATA_SECURITY_DESTRUCTIVE_OPS_WARN: &str = "DATA_SECURITY_DESTRUCTIVE_OPS_WARN";
    /// `data_security.destructive_ops = "warn"` surfaced an unclassified operation.
    pub const DATA_SECURITY_UNCLASSIFIED_OPS_WARN: &str =
        "DATA_SECURITY_UNCLASSIFIED_OPS_WARN";
    /// `DROP TABLE`/`DROP COLUMN`/`DROP CONSTRAINT` — irreversible data loss.
    pub const DESTRUCTIVE_DROP: &str = "DESTRUCTIVE_DROP";
    /// `RENAME COLUMN`/`RENAME TABLE` — breaks code reading the old name.
    pub const BACKWARD_INCOMPATIBLE_RENAME: &str = "BACKWARD_INCOMPATIBLE_RENAME";
    /// `ALTER COLUMN … TYPE` — may lose data / rewrites the table.
    pub const LOSSY_TYPE_CHANGE: &str = "LOSSY_TYPE_CHANGE";
    /// `ADD COLUMN NOT NULL` with no default — fails on a non-empty table.
    pub const ADD_NOT_NULL_NO_DEFAULT: &str = "ADD_NOT_NULL_NO_DEFAULT";
    /// `ALTER COLUMN … SET NOT NULL` — full table scan under lock.
    pub const SET_NOT_NULL_FULL_SCAN: &str = "SET_NOT_NULL_FULL_SCAN";
    /// `ADD CONSTRAINT` (FK/UNIQUE/CHECK) without `NOT VALID` — validates all
    /// existing rows under lock.
    pub const CONSTRAINT_NOT_VALIDATED: &str = "CONSTRAINT_NOT_VALIDATED";
    /// Plain `CREATE INDEX` (not `CONCURRENTLY`) — blocks writes for the build.
    pub const NON_CONCURRENT_INDEX: &str = "NON_CONCURRENT_INDEX";
    /// An `ACCESS EXCLUSIVE` table rewrite (volatile-default ADD COLUMN / ALTER
    /// TYPE).
    pub const TABLE_REWRITE: &str = "TABLE_REWRITE";
    /// An FK referencing column with no supporting index in the same migration.
    pub const FK_WITHOUT_INDEX: &str = "FK_WITHOUT_INDEX";
    /// `TRUNCATE` — deletes all rows; irreversible and not MVCC-rolled-back the
    /// way a `DELETE` is (it resets storage; under some setups it cannot be
    /// rolled back cleanly).
    pub const TRUNCATE_DATA_LOSS: &str = "TRUNCATE_DATA_LOSS";
    /// A lock-heavy maintenance op — `CLUSTER`, `VACUUM FULL`, or a non-concurrent
    /// `REINDEX` — that takes an ACCESS EXCLUSIVE / heavy lock for its duration.
    pub const LOCK_HEAVY_MAINTENANCE: &str = "LOCK_HEAVY_MAINTENANCE";
}

/// Run every analyzer over `sql` and return the advisories (in source order,
/// then analyzer order within a statement).
///
/// Unparseable SQL yields no advisories — the guard already denies it; the
/// analyzers are a *best-effort enrichment* on top of a parseable statement.
#[must_use]
pub fn analyze(sql: &str) -> Vec<Advisory> {
    let Ok(parsed) = pg_query::parse(sql) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for raw_stmt in &parsed.protobuf.stmts {
        if let Some(node) = raw_stmt.stmt.as_ref().and_then(|s| s.node.as_ref()) {
            analyze_node(node, &mut out);
        }
    }
    out
}

/// Run [`analyze`] over a migration's forward (`up`) SQL — the seam the
/// declarative differ and a future plan UI use to attach operational advisories
/// to each generated migration.
///
/// Only `up` is analyzed: the `down` is run only on an operator-approved
/// rollback, and an advisory there would be noise on the common (forward) path.
#[must_use]
pub fn analyze_migration(migration: &Migration) -> Vec<Advisory> {
    analyze(&migration.up)
}

/// Every column that gains a **covering index** anywhere in `sql`.
///
/// Sources: a leading index column from a `CREATE INDEX`, an inline `PRIMARY
/// KEY`/`UNIQUE` (on `CREATE TABLE` or `ALTER TABLE … ADD COLUMN`), an `ALTER
/// TABLE … ADD CONSTRAINT PRIMARY KEY/UNIQUE`, or an `ALTER TABLE … ADD INDEX`
/// subcommand.
///
/// This is the **plan-aware** seam (review finding #8): the per-statement
/// [`analyze`] only sees one statement and so suppresses `FK_WITHOUT_INDEX` only
/// for an index in the SAME statement. A [`crate::render::declarative::DeclarativePlan`]
/// aggregates this across *every* migration (plus the desired snapshot) so an FK
/// whose covering index is created in a SEPARATE migration of the same plan is no
/// longer flagged. [`analyze`] itself is unchanged — it has no plan view.
#[must_use]
pub fn indexed_columns(sql: &str) -> Vec<String> {
    let Ok(parsed) = pg_query::parse(sql) else {
        return Vec::new();
    };
    let mut cols = Vec::new();
    for raw_stmt in &parsed.protobuf.stmts {
        if let Some(node) = raw_stmt.stmt.as_ref().and_then(|s| s.node.as_ref()) {
            collect_indexed_columns_in_node(node, &mut cols);
        }
    }
    cols
}

/// The FK **referencing** column(s) that an FK in `sql` wants a supporting index on.
///
/// These are the `FK_WITHOUT_INDEX` subjects. Used by the plan seam to recompute
/// suppression against the plan-wide [`indexed_columns`] set.
#[must_use]
pub fn fk_columns_needing_index(sql: &str) -> Vec<String> {
    let Ok(parsed) = pg_query::parse(sql) else {
        return Vec::new();
    };
    let mut cols = Vec::new();
    for raw_stmt in &parsed.protobuf.stmts {
        let Some(NodeEnum::AlterTableStmt(at)) =
            raw_stmt.stmt.as_ref().and_then(|s| s.node.as_ref())
        else {
            continue;
        };
        for cmd in &at.cmds {
            let Some(NodeEnum::AlterTableCmd(c)) = cmd.node.as_ref() else {
                continue;
            };
            if c.subtype != AlterTableType::AtAddConstraint as i32 {
                continue;
            }
            if let Some(NodeEnum::Constraint(con)) = c.def.as_ref().and_then(|d| d.node.as_ref()) {
                if con.contype == ConstrType::ConstrForeign as i32 {
                    cols.extend(fk_referencing_columns(con));
                }
            }
        }
    }
    cols
}

/// Collect covering-index columns from a single top-level node (CREATE INDEX,
/// CREATE TABLE inline / table-level PK·UNIQUE, ALTER TABLE inline / ADD
/// CONSTRAINT / ADD INDEX). The plan-aware index-coverage counterpart to
/// [`analyze_node`].
fn collect_indexed_columns_in_node(node: &NodeEnum, cols: &mut Vec<String>) {
    match node {
        NodeEnum::IndexStmt(idx) => cols.extend(index_key_columns(idx)),
        NodeEnum::AlterTableStmt(at) => cols.extend(collect_indexed_columns_in_alter(at)),
        NodeEnum::CreateStmt(create) => {
            for elt in &create.table_elts {
                match elt.node.as_ref() {
                    // An inline PRIMARY KEY/UNIQUE on a column definition.
                    Some(NodeEnum::ColumnDef(col)) => {
                        if inline_index_constraint_kind(col).is_some() {
                            cols.push(col.colname.clone());
                        }
                    }
                    // A table-level PRIMARY KEY/UNIQUE constraint (keys list).
                    Some(NodeEnum::Constraint(con))
                        if con.contype == ConstrType::ConstrUnique as i32
                            || con.contype == ConstrType::ConstrPrimary as i32 =>
                    {
                        cols.extend(string_list(&con.keys));
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
}

/// Run every analyzer over a single **top-level** statement node.
///
/// This is TOP-LEVEL-only by design: unlike [`crate::guard`] (which walks
/// `DO`-block / function bodies to enforce the security deny-list), the analyzers
/// do NOT recurse into nested statement bodies. Advisories are non-security
/// enrichment, so a footgun buried inside a `DO $$ … $$` block is simply not
/// flagged — it is not a security hole (the guard + role still cover the
/// dangerous surface). `CreateStmt` (`CREATE TABLE`) is intentionally NOT
/// dispatched: its inline `PRIMARY KEY`/`UNIQUE`/`NOT NULL` constraints apply to
/// a brand-new empty table and would otherwise false-positive the
/// populated-table lock/scan advisories.
fn analyze_node(node: &NodeEnum, out: &mut Vec<Advisory>) {
    match node {
        // ---- CREATE INDEX (non-concurrent) ----
        NodeEnum::IndexStmt(idx) => analyze_create_index(idx, out),
        // ---- DROP TABLE / COLUMN / CONSTRAINT (the DROP-stmt forms) ----
        NodeEnum::DropStmt(d) => analyze_drop_stmt(d, out),
        // ---- RENAME TABLE / COLUMN ----
        NodeEnum::RenameStmt(r) => analyze_rename_stmt(r, out),
        // ---- ALTER TABLE: the bulk of the operational analyzers ----
        NodeEnum::AlterTableStmt(at) => analyze_alter_table(at, out),
        // ---- TRUNCATE: irreversible deletion of every row ----
        NodeEnum::TruncateStmt(_) => analyze_truncate(out),
        // ---- CLUSTER / VACUUM FULL / REINDEX: lock-heavy maintenance ----
        NodeEnum::ClusterStmt(_) => analyze_cluster(out),
        NodeEnum::VacuumStmt(v) => analyze_vacuum(v, out),
        NodeEnum::ReindexStmt(r) => analyze_reindex(r, out),
        _ => {}
    }
}

/// `NON_CONCURRENT_INDEX` — a plain `CREATE INDEX` holds a SHARE lock that
/// blocks writes for the whole build; suggest `CONCURRENTLY` on a populated
/// table.
fn analyze_create_index(idx: &protobuf::IndexStmt, out: &mut Vec<Advisory>) {
    if idx.concurrent {
        return;
    }
    let rel = idx.relation.as_ref().map_or("?", |r| r.relname.as_str());
    out.push(Advisory::warning(
        rule::NON_CONCURRENT_INDEX,
        format!(
            "CREATE INDEX on '{rel}' is not CONCURRENTLY: it takes a SHARE lock and \
             blocks writes for the entire build"
        ),
        "use CREATE INDEX CONCURRENTLY on a populated table — but it cannot run inside \
         a transaction block, so it cannot share a transaction with other DDL: author \
         it as its OWN non-transactional migration",
    ));
}

/// `TRUNCATE_DATA_LOSS` — `TRUNCATE` removes every row at once; it is
/// irreversible and (unlike a `DELETE`) resets the table's storage.
fn analyze_truncate(out: &mut Vec<Advisory>) {
    out.push(Advisory::warning(
        rule::TRUNCATE_DATA_LOSS,
        "TRUNCATE deletes all rows in the target table(s) — irreversible data loss \
         (it resets storage, and under some setups cannot be cleanly rolled back)"
            .to_string(),
        "if you need to keep the data, copy it out first; a TRUNCATE always requires \
         explicit approval (it is destructive)",
    ));
}

/// `LOCK_HEAVY_MAINTENANCE` — `CLUSTER` rewrites the table under an ACCESS
/// EXCLUSIVE lock for its whole duration.
fn analyze_cluster(out: &mut Vec<Advisory>) {
    out.push(Advisory::warning(
        rule::LOCK_HEAVY_MAINTENANCE,
        "CLUSTER rewrites the table under an ACCESS EXCLUSIVE lock for the entire \
         operation, blocking all reads and writes"
            .to_string(),
        "run it in a maintenance window, or use pg_repack for an online rewrite",
    ));
}

/// `LOCK_HEAVY_MAINTENANCE` — `VACUUM FULL` (the `full` option) rewrites the
/// table under an ACCESS EXCLUSIVE lock. A plain `VACUUM` does NOT and is silent.
fn analyze_vacuum(v: &protobuf::VacuumStmt, out: &mut Vec<Advisory>) {
    if !def_elem_present(&v.options, "full") {
        return;
    }
    out.push(Advisory::warning(
        rule::LOCK_HEAVY_MAINTENANCE,
        "VACUUM FULL rewrites the table under an ACCESS EXCLUSIVE lock for the entire \
         operation, blocking all reads and writes"
            .to_string(),
        "use plain VACUUM (no FULL) for routine bloat, or pg_repack for an online \
         space reclaim",
    ));
}

/// `LOCK_HEAVY_MAINTENANCE` — a non-concurrent `REINDEX` holds a lock that blocks
/// writes (and, for some forms, reads) while it rebuilds. `REINDEX … CONCURRENTLY`
/// (the `concurrently` param) is the safe form and is silent.
fn analyze_reindex(r: &protobuf::ReindexStmt, out: &mut Vec<Advisory>) {
    if def_elem_present(&r.params, "concurrently") {
        return;
    }
    out.push(Advisory::warning(
        rule::LOCK_HEAVY_MAINTENANCE,
        "REINDEX (non-concurrent) holds a lock that blocks writes while it rebuilds \
         the index"
            .to_string(),
        "use REINDEX … CONCURRENTLY (it cannot run inside a transaction, so author it \
         as its own non-transactional migration)",
    ));
}

/// Is a `DefElem` with `defname == name` present in a `DefElem` option list?
/// (`VACUUM FULL` ⇒ a `full` option; `REINDEX … CONCURRENTLY` ⇒ a `concurrently`
/// param — both arrive as `DefElem`s rather than struct flags.)
fn def_elem_present(opts: &[protobuf::Node], name: &str) -> bool {
    opts.iter().any(|n| {
        matches!(n.node.as_ref(), Some(NodeEnum::DefElem(d)) if d.defname.eq_ignore_ascii_case(name))
    })
}

/// `DESTRUCTIVE_DROP` — `DROP TABLE`/`VIEW`/`SEQUENCE`/… via the `DropStmt`
/// form. `DROP COLUMN`/`DROP CONSTRAINT` arrive as `AlterTableCmd`s, handled in
/// [`analyze_alter_table`].
fn analyze_drop_stmt(d: &protobuf::DropStmt, out: &mut Vec<Advisory>) {
    // Only the table-shaped drops are a data-loss footgun worth the
    // expand-contract message; a DROP INDEX/TRIGGER/FUNCTION is reversible
    // structure, not data.
    let object = match ObjectType::try_from(d.remove_type) {
        Ok(ObjectType::ObjectTable) => "table",
        Ok(ObjectType::ObjectMatview) => "materialized view",
        Ok(ObjectType::ObjectColumn) => "column", // belt: DROP COLUMN spelled as DropStmt
        _ => return,
    };
    out.push(Advisory::warning(
        rule::DESTRUCTIVE_DROP,
        format!(
            "DROP of a {object} is irreversible data loss and breaks any running code \
             still reading it"
        ),
        "stop reading it in app code first, then drop in a later deploy (expand-contract); \
         a destructive drop always requires explicit approval",
    ));
}

/// `BACKWARD_INCOMPATIBLE_RENAME` — `RENAME COLUMN`/`RENAME TABLE` breaks any
/// running code reading the old name.
fn analyze_rename_stmt(r: &protobuf::RenameStmt, out: &mut Vec<Advisory>) {
    let what = if r.rename_type == ObjectType::ObjectColumn as i32 {
        "column"
    } else if r.rename_type == ObjectType::ObjectTable as i32 {
        "table"
    } else {
        return;
    };
    out.push(Advisory::warning(
        rule::BACKWARD_INCOMPATIBLE_RENAME,
        format!(
            "RENAME {what} '{}' → '{}' breaks running code that still reads the old name \
             (it is not an online, backward-compatible change)",
            r.subname, r.newname
        ),
        "use the online expand-contract rename path: add the new name, dual-write + \
         backfill, switch app code, then drop the old name in a later deploy",
    ));
}

/// All `ALTER TABLE`-borne analyzers. One ALTER TABLE may carry several
/// subcommands; each is analyzed independently.
fn analyze_alter_table(at: &protobuf::AlterTableStmt, out: &mut Vec<Advisory>) {
    // Index columns created in THIS statement (rare in an ALTER, but an
    // AT_AddIndex subcommand or a same-batch CREATE INDEX is handled by the
    // top-level walk). Collect the set of columns gaining an FK so we can warn
    // on any without a supporting index in the same statement.
    let indexed_in_stmt = collect_indexed_columns_in_alter(at);

    for cmd in &at.cmds {
        let Some(NodeEnum::AlterTableCmd(c)) = cmd.node.as_ref() else {
            continue;
        };
        let subtype = c.subtype;
        if subtype == AlterTableType::AtAddColumn as i32 {
            analyze_add_column(c, out);
        } else if subtype == AlterTableType::AtDropColumn as i32 {
            analyze_drop_column(c, out);
        } else if subtype == AlterTableType::AtSetNotNull as i32 {
            analyze_set_not_null(c, out);
        } else if subtype == AlterTableType::AtAlterColumnType as i32 {
            analyze_alter_column_type(c, out);
        } else if subtype == AlterTableType::AtAddConstraint as i32 {
            analyze_add_constraint(c, &indexed_in_stmt, out);
        } else if subtype == AlterTableType::AtDropConstraint as i32 {
            analyze_drop_constraint(c, out);
        }
    }
}

/// `ADD COLUMN` analyzers: `ADD_NOT_NULL_NO_DEFAULT` (NOT NULL — incl. inline
/// PRIMARY KEY — with no default), `TABLE_REWRITE` (volatile default or a STORED
/// generated expression ⇒ ACCESS EXCLUSIVE rewrite), and the lock advisory for an
/// inline `PRIMARY KEY`/`UNIQUE` (each builds an index under ACCESS EXCLUSIVE).
fn analyze_add_column(c: &protobuf::AlterTableCmd, out: &mut Vec<Advisory>) {
    let Some(NodeEnum::ColumnDef(col)) = c.def.as_ref().and_then(|d| d.node.as_ref()) else {
        return;
    };

    // An inline PRIMARY KEY / UNIQUE builds a (unique) index under ACCESS
    // EXCLUSIVE on a populated table — the same footgun as ADD CONSTRAINT
    // UNIQUE. Emit the lock advisory and point at the CONCURRENTLY-index path.
    if let Some(kind) = inline_index_constraint_kind(col) {
        out.push(Advisory::warning(
            rule::CONSTRAINT_NOT_VALIDATED,
            format!(
                "ADD COLUMN '{}' {kind} builds a validating index under an ACCESS \
                 EXCLUSIVE lock (and fails on any duplicate)",
                col.colname
            ),
            "add the column first, then CREATE UNIQUE INDEX CONCURRENTLY and ADD \
             CONSTRAINT … USING INDEX (avoids the long exclusive lock)",
        ));
    }

    let not_null = column_is_not_null(col);
    let default = column_default_kind(col);

    match (not_null, default) {
        // NOT NULL with NO default — fails outright on any non-empty table.
        (true, DefaultKind::None) => out.push(Advisory::warning(
            rule::ADD_NOT_NULL_NO_DEFAULT,
            format!(
                "ADD COLUMN '{}' NOT NULL with no DEFAULT fails on a non-empty table \
                 (existing rows have no value)",
                col.colname
            ),
            "add the column nullable, backfill it, then SET NOT NULL (or add it NOT NULL \
             with a constant DEFAULT)",
        )),
        // NOT NULL + volatile default — full table rewrite under ACCESS
        // EXCLUSIVE (the constant-default fast path does NOT rewrite).
        (true, DefaultKind::Volatile) => out.push(Advisory::warning(
            rule::TABLE_REWRITE,
            format!(
                "ADD COLUMN '{}' NOT NULL with a volatile DEFAULT forces a full table \
                 rewrite under an ACCESS EXCLUSIVE lock",
                col.colname
            ),
            "use a constant DEFAULT (PG11+ metadata-only fast path), or add the column \
             nullable + backfill in a separate step",
        )),
        // A volatile default on a NULLABLE column still rewrites the table.
        (false, DefaultKind::Volatile) => out.push(Advisory::warning(
            rule::TABLE_REWRITE,
            format!(
                "ADD COLUMN '{}' with a volatile DEFAULT forces a full table rewrite \
                 under an ACCESS EXCLUSIVE lock",
                col.colname
            ),
            "use a constant DEFAULT (PG11+ metadata-only fast path), or backfill in a \
             separate step",
        )),
        _ => {}
    }
}

/// `DESTRUCTIVE_DROP` for `ALTER TABLE … DROP COLUMN`.
fn analyze_drop_column(c: &protobuf::AlterTableCmd, out: &mut Vec<Advisory>) {
    out.push(Advisory::warning(
        rule::DESTRUCTIVE_DROP,
        format!(
            "DROP COLUMN '{}' is irreversible data loss and breaks any running code still \
             reading it",
            c.name
        ),
        "stop reading the column in app code first, then drop it in a later deploy \
         (expand-contract); a destructive drop always requires explicit approval",
    ));
}

/// `DESTRUCTIVE_DROP` for `ALTER TABLE … DROP CONSTRAINT` — drops a
/// data-integrity guarantee.
fn analyze_drop_constraint(c: &protobuf::AlterTableCmd, out: &mut Vec<Advisory>) {
    out.push(Advisory::warning(
        rule::DESTRUCTIVE_DROP,
        format!(
            "DROP CONSTRAINT '{}' removes a data-integrity guarantee; data that violates \
             it can then be written",
            c.name
        ),
        "confirm no invariant depends on it; re-adding it later validates against all \
         existing rows under lock",
    ));
}

/// `SET_NOT_NULL_FULL_SCAN` — `ALTER COLUMN … SET NOT NULL` scans the whole
/// table under an ACCESS EXCLUSIVE lock to verify no existing NULLs.
fn analyze_set_not_null(c: &protobuf::AlterTableCmd, out: &mut Vec<Advisory>) {
    out.push(Advisory::warning(
        rule::SET_NOT_NULL_FULL_SCAN,
        format!(
            "SET NOT NULL on '{}' scans the entire table under an ACCESS EXCLUSIVE lock to \
             verify no existing NULLs",
            c.name
        ),
        "add a CHECK (col IS NOT NULL) NOT VALID, VALIDATE CONSTRAINT (no exclusive lock), \
         then SET NOT NULL (PG12+ uses the validated constraint to skip the scan)",
    ));
}

/// `LOSSY_TYPE_CHANGE` — `ALTER COLUMN … TYPE` may lose data and usually
/// rewrites the table under an ACCESS EXCLUSIVE lock.
fn analyze_alter_column_type(c: &protobuf::AlterTableCmd, out: &mut Vec<Advisory>) {
    out.push(Advisory::warning(
        rule::LOSSY_TYPE_CHANGE,
        format!(
            "ALTER COLUMN '{}' TYPE may lose data (narrowing/incompatible conversion) and \
             rewrites the table under an ACCESS EXCLUSIVE lock",
            c.name
        ),
        "add a new column of the target type, backfill with a vetted conversion, swap, \
         then drop the old column (expand-contract)",
    ));
}

/// `CONSTRAINT_NOT_VALIDATED` (and `FK_WITHOUT_INDEX`) for
/// `ALTER TABLE … ADD CONSTRAINT`.
///
/// A FK/UNIQUE/CHECK added without `NOT VALID` validates against every existing
/// row while holding a lock — and can fail outright on dirty data. A FK whose
/// referencing column has no supporting index makes every cascade/lookup a scan.
fn analyze_add_constraint(
    c: &protobuf::AlterTableCmd,
    indexed_in_stmt: &[String],
    out: &mut Vec<Advisory>,
) {
    let Some(NodeEnum::Constraint(con)) = c.def.as_ref().and_then(|d| d.node.as_ref()) else {
        return;
    };
    let contype = con.contype;
    let is_fk = contype == ConstrType::ConstrForeign as i32;
    let is_unique = contype == ConstrType::ConstrUnique as i32;
    let is_primary = contype == ConstrType::ConstrPrimary as i32;
    let is_check = contype == ConstrType::ConstrCheck as i32;

    // CONSTRAINT_NOT_VALIDATED: FK and CHECK support `NOT VALID` (skip_validation
    // true ⇒ NOT VALID specified). UNIQUE/PRIMARY do NOT support NOT VALID — they
    // always build a validating index under lock — so we still advise the
    // CONCURRENTLY-index path for those. Attaching a pre-built index
    // (`USING INDEX`, `con.indexname` set) is the SAFE path we recommend, so we
    // skip the advisory there (false-positive fix).
    if (is_fk || is_check) && !con.skip_validation {
        out.push(Advisory::warning(
            rule::CONSTRAINT_NOT_VALIDATED,
            format!(
                "ADD CONSTRAINT '{}' validates against all existing rows while holding a \
                 lock (and fails outright on any violating row)",
                constraint_label(con)
            ),
            "add it NOT VALID first (fast, no full scan), then VALIDATE CONSTRAINT \
             separately (takes only a SHARE UPDATE EXCLUSIVE lock)",
        ));
    } else if (is_unique || is_primary) && con.indexname.is_empty() {
        let kind = if is_primary { "PRIMARY KEY" } else { "UNIQUE" };
        out.push(Advisory::warning(
            rule::CONSTRAINT_NOT_VALIDATED,
            format!(
                "ADD CONSTRAINT '{}' {kind} builds a validating index under an ACCESS \
                 EXCLUSIVE lock and fails on any duplicate",
                constraint_label(con)
            ),
            "CREATE UNIQUE INDEX CONCURRENTLY, then ADD CONSTRAINT … USING INDEX \
             (avoids the long exclusive lock)",
        ));
    }

    // FK_WITHOUT_INDEX: an FK's *referencing* column(s) should have a supporting
    // index, else every cascade/lookup scans the child table.
    if is_fk {
        let fk_cols = fk_referencing_columns(con);
        let unindexed: Vec<String> = fk_cols
            .into_iter()
            .filter(|col| {
                !indexed_in_stmt
                    .iter()
                    .any(|i| i.eq_ignore_ascii_case(col))
            })
            .collect();
        if !unindexed.is_empty() {
            out.push(Advisory::notice(
                rule::FK_WITHOUT_INDEX,
                format!(
                    "FK on column(s) ({}) has no supporting index in this migration; FK \
                     lookups and ON DELETE/UPDATE cascades will scan the table",
                    unindexed.join(", ")
                ),
                "add an index on the referencing column(s) (CREATE INDEX CONCURRENTLY)",
            ));
        }
    }
}

/// Columns that gain an index in the SAME `ALTER TABLE` (an `AT_AddIndex` /
/// `AT_AddIndexConstraint` subcommand or an inline UNIQUE/PK constraint). Used
/// to suppress an `FK_WITHOUT_INDEX` advisory when the FK column is indexed in
/// the same statement.
fn collect_indexed_columns_in_alter(at: &protobuf::AlterTableStmt) -> Vec<String> {
    let mut cols = Vec::new();
    for cmd in &at.cmds {
        let Some(NodeEnum::AlterTableCmd(c)) = cmd.node.as_ref() else {
            continue;
        };
        // An inline UNIQUE/PRIMARY constraint creates a supporting index over
        // its key columns.
        if let Some(NodeEnum::Constraint(con)) = c.def.as_ref().and_then(|d| d.node.as_ref()) {
            if con.contype == ConstrType::ConstrUnique as i32
                || con.contype == ConstrType::ConstrPrimary as i32
            {
                cols.extend(string_list(&con.keys));
            }
        }
        // An AT_AddIndex carries an IndexStmt whose key columns are indexed.
        if let Some(NodeEnum::IndexStmt(idx)) = c.def.as_ref().and_then(|d| d.node.as_ref()) {
            cols.extend(index_key_columns(idx));
        }
    }
    cols
}

/// The referencing (child) column names of an FK constraint (`fk_attrs`).
fn fk_referencing_columns(con: &protobuf::Constraint) -> Vec<String> {
    string_list(&con.fk_attrs)
}

/// The leading-column names of an index (its key `IndexElem`s).
fn index_key_columns(idx: &protobuf::IndexStmt) -> Vec<String> {
    idx.index_params
        .iter()
        .filter_map(|p| match p.node.as_ref() {
            Some(NodeEnum::IndexElem(e)) if !e.name.is_empty() => Some(e.name.clone()),
            _ => None,
        })
        .collect()
}

/// Flatten a list of `String` nodes (constraint keys / `fk_attrs`) to their values.
fn string_list(nodes: &[protobuf::Node]) -> Vec<String> {
    nodes
        .iter()
        .filter_map(|n| match n.node.as_ref() {
            Some(NodeEnum::String(s)) => Some(s.sval.clone()),
            _ => None,
        })
        .collect()
}

/// A human label for a constraint: its name if given, else its kind.
fn constraint_label(con: &protobuf::Constraint) -> String {
    if con.conname.is_empty() {
        match ConstrType::try_from(con.contype) {
            Ok(ConstrType::ConstrForeign) => "<foreign key>".to_string(),
            Ok(ConstrType::ConstrUnique) => "<unique>".to_string(),
            Ok(ConstrType::ConstrCheck) => "<check>".to_string(),
            _ => "<constraint>".to_string(),
        }
    } else {
        con.conname.clone()
    }
}

/// If a `ColumnDef` carries an inline `PRIMARY KEY` or `UNIQUE`, the human label
/// of that constraint (each builds an index under ACCESS EXCLUSIVE on a populated
/// table). Returns `None` for a column with no inline index-building constraint.
fn inline_index_constraint_kind(col: &protobuf::ColumnDef) -> Option<&'static str> {
    col.constraints.iter().find_map(|con| match con.node.as_ref() {
        Some(NodeEnum::Constraint(c)) if c.contype == ConstrType::ConstrPrimary as i32 => {
            Some("PRIMARY KEY")
        }
        Some(NodeEnum::Constraint(c)) if c.contype == ConstrType::ConstrUnique as i32 => {
            Some("UNIQUE")
        }
        _ => None,
    })
}

/// Whether a `ColumnDef` carries a NOT NULL: the `is_not_null` flag, an explicit
/// NOT NULL constraint node, or an inline `PRIMARY KEY` (which implies NOT NULL —
/// so `ADD COLUMN x int PRIMARY KEY` on a non-empty table fails exactly like an
/// explicit NOT NULL with no default).
fn column_is_not_null(col: &protobuf::ColumnDef) -> bool {
    if col.is_not_null {
        return true;
    }
    col.constraints.iter().any(|con| {
        matches!(con.node.as_ref(), Some(NodeEnum::Constraint(c))
            if c.contype == ConstrType::ConstrNotnull as i32
                || c.contype == ConstrType::ConstrPrimary as i32)
    })
}

/// The kind of DEFAULT a column declares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DefaultKind {
    /// No DEFAULT clause.
    None,
    /// A constant DEFAULT (literal) — the PG11+ metadata-only fast path.
    Constant,
    /// A DEFAULT whose expression contains a function call — treated as volatile
    /// (forces a table rewrite). Conservative: we cannot prove stability without
    /// a catalog lookup.
    Volatile,
}

/// Classify a column's DEFAULT clause.
///
/// A `GENERATED ALWAYS AS (…) STORED` column (`ConstrGenerated`) is materialised
/// for every existing row, so adding one rewrites the table under an ACCESS
/// EXCLUSIVE lock — classified as [`DefaultKind::Volatile`] so it surfaces a
/// `TABLE_REWRITE` advisory.
fn column_default_kind(col: &protobuf::ColumnDef) -> DefaultKind {
    for con in &col.constraints {
        if let Some(NodeEnum::Constraint(c)) = con.node.as_ref() {
            if c.contype == ConstrType::ConstrGenerated as i32 {
                // A STORED generated column is computed for every row on ADD →
                // full table rewrite.
                return DefaultKind::Volatile;
            }
            if c.contype == ConstrType::ConstrDefault as i32 {
                return match c.raw_expr.as_ref().and_then(|e| e.node.as_ref()) {
                    Some(expr) if expr_contains_func_call(expr) => DefaultKind::Volatile,
                    // A bare constant DEFAULT (or no expr node) is the PG11+
                    // metadata-only fast path — not a rewrite.
                    Some(_) | None => DefaultKind::Constant,
                };
            }
        }
    }
    DefaultKind::None
}

/// Does an expression tree contain any function call? (volatility heuristic,
/// mirrors the guard's `expr_contains_func_call`).
fn expr_contains_func_call(expr: &NodeEnum) -> bool {
    use pg_query::NodeRef;
    expr.nodes()
        .iter()
        .any(|(n, _, _, _)| matches!(n, NodeRef::FuncCall(_)))
}
