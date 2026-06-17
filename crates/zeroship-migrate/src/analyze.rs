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

use crate::migration::Migration;

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
}

/// The stable advisory rule ids — **data, not logic**, mirroring the guard's
/// `denylist::rule` convention. These are NOT security rules; they never deny.
pub mod rule {
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

/// Run every analyzer over a single top-level statement node.
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
        "use CREATE INDEX CONCURRENTLY on a populated table (it cannot run inside a \
         transaction, so author it as a non-transactional migration)",
    ));
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

/// `ADD COLUMN` analyzers: `ADD_NOT_NULL_NO_DEFAULT` (NOT NULL, no default) and
/// `TABLE_REWRITE` (NOT NULL + volatile default ⇒ ACCESS EXCLUSIVE rewrite).
fn analyze_add_column(c: &protobuf::AlterTableCmd, out: &mut Vec<Advisory>) {
    let Some(NodeEnum::ColumnDef(col)) = c.def.as_ref().and_then(|d| d.node.as_ref()) else {
        return;
    };
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
    let is_check = contype == ConstrType::ConstrCheck as i32;

    // CONSTRAINT_NOT_VALIDATED: FK and CHECK support `NOT VALID` (skip_validation
    // true ⇒ NOT VALID specified). UNIQUE/PRIMARY do NOT support NOT VALID — they
    // always build a validating index under lock — so we still advise the
    // CONCURRENTLY-index path for those.
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
    } else if is_unique {
        out.push(Advisory::warning(
            rule::CONSTRAINT_NOT_VALIDATED,
            format!(
                "ADD CONSTRAINT '{}' UNIQUE builds a validating index under an ACCESS \
                 EXCLUSIVE lock and fails on any duplicate",
                constraint_label(con)
            ),
            "CREATE UNIQUE INDEX CONCURRENTLY, then ADD CONSTRAINT … UNIQUE USING INDEX \
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

/// Whether a `ColumnDef` carries a NOT NULL (`is_not_null` flag or an explicit
/// NOT NULL constraint node).
fn column_is_not_null(col: &protobuf::ColumnDef) -> bool {
    if col.is_not_null {
        return true;
    }
    col.constraints.iter().any(|con| {
        matches!(con.node.as_ref(), Some(NodeEnum::Constraint(c))
            if c.contype == ConstrType::ConstrNotnull as i32)
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
fn column_default_kind(col: &protobuf::ColumnDef) -> DefaultKind {
    for con in &col.constraints {
        if let Some(NodeEnum::Constraint(c)) = con.node.as_ref() {
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
