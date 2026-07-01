//! Statement parsing + classification (design §2.1 / §1.4 foundation).
//!
//! `classify` runs each statement through the **real Postgres parser**
//! (`pg_query`/`libpg_query`) and maps it to a [`StatementClass`]: its DDL kind,
//! whether it is additive vs destructive, whether it can run in a transaction,
//! and the set of explicitly schema-qualified objects it touches. The security
//! guard (`crate::guard`) is built on top of this.

use pg_query::protobuf::node::Node as NodeEnum;
use pg_query::protobuf::{
    self, AExprKind, AlterTableType, BoolExprType, CmdType, MergeMatchKind, ObjectType,
};
use pg_query::NodeRef;

/// Error parsing SQL for classification.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ParseError {
    /// `libpg_query` rejected the SQL (syntax error, etc.).
    #[error("failed to parse SQL: {0}")]
    Syntax(String),
}

/// The kind of statement, at the granularity the migration engine cares about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DdlKind {
    CreateTable,
    DropTable,
    AddColumn,
    DropColumn,
    AlterColumnType,
    /// `ALTER COLUMN … SET NOT NULL` — gated (a full-table validating scan +
    /// ACCESS EXCLUSIVE lock, and it ABORTS if any existing row is NULL; the
    /// row-less shadow cannot catch that). See `crate::guard::flags_for`.
    SetNotNull,
    RenameColumn,
    RenameTable,
    CreateIndex,
    CreateIndexConcurrently,
    AddConstraint,
    DropConstraint,
    CreateExtension,
    CreateFunction,
    CreateTrigger,
    CreateRole,
    Grant,
    AlterSystem,
    Copy,
    /// Data-manipulation (INSERT/UPDATE/DELETE/MERGE/TRUNCATE).
    Dml,
    Select,
    /// Any other statement kind, carrying the `libpg_query` node name so the
    /// guard can deny-by-default unrecognized-but-dangerous constructs.
    Other(String),
}

/// Data-security classification for `data_security.destructive_ops`.
///
/// This is deliberately a trichotomy, not `Option<destructive_label>`:
/// - [`Self::NonDestructive`] means the classifier positively recognizes the
///   statement as non-data-lossy.
/// - [`Self::Destructive`] is a known destructive/data-lossy operation.
/// - [`Self::Unknown`] is neither. `forbid` must deny this class fail-closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataSecurityClass {
    /// Positively recognized as non-data-lossy. Other guard layers may still
    /// deny it for privilege, confinement, RCE, or policy reasons.
    NonDestructive,
    /// Known destructive/data-lossy operation, with its canonical label.
    Destructive(&'static str),
    /// Not positively classified. This is denied under `destructive_ops=forbid`
    /// and warned under `destructive_ops=warn`.
    Unknown,
}

impl DataSecurityClass {
    /// Canonical destructive operation label, when this class is destructive.
    #[must_use]
    pub const fn destructive_operation(self) -> Option<&'static str> {
        match self {
            Self::Destructive(operation) => Some(operation),
            Self::NonDestructive | Self::Unknown => None,
        }
    }
}

/// One classified statement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatementClass {
    /// The DDL/DML kind.
    pub kind: DdlKind,
    /// Adds structure without losing data (CREATE TABLE, ADD COLUMN, etc.).
    pub additive: bool,
    /// Loses data (DROP/TRUNCATE/lossy type change). The guard *flags* this;
    /// it does not deny it (the gate, built later, decides).
    pub destructive: bool,
    /// Canonical destructive operation label when [`Self::destructive`] is true.
    /// The data-security guard consumes this exact classifier output so the
    /// policy gate cannot drift into a narrower hand-rolled SQL allowlist.
    pub destructive_operation: Option<&'static str>,
    /// Definite trichotomy used by `data_security.destructive_ops`.
    pub data_security: DataSecurityClass,
    /// Cannot run inside a transaction block (CONCURRENTLY, ALTER TYPE ADD
    /// VALUE, VACUUM) — needs the two-phase apply path.
    pub non_transactional: bool,
    /// Every explicitly schema-qualified object the statement references
    /// (deduped, in first-seen order). Drives the cross-schema confinement
    /// check in `crate::guard`.
    pub referenced_schemas: Vec<String>,
    /// The raw SQL text of this statement (best-effort: the deparsed form).
    pub raw: String,
}

/// Classify every statement in `sql`, one [`StatementClass`] per statement,
/// in source order.
///
/// # Errors
/// [`ParseError::Syntax`] if `libpg_query` cannot parse the input.
pub fn classify(sql: &str) -> Result<Vec<StatementClass>, ParseError> {
    let parsed = pg_query::parse(sql).map_err(|e| ParseError::Syntax(e.to_string()))?;
    let mut out = Vec::with_capacity(parsed.protobuf.stmts.len());
    for raw_stmt in &parsed.protobuf.stmts {
        let Some(node) = raw_stmt.stmt.as_ref().and_then(|s| s.node.as_ref()) else {
            continue;
        };
        let raw = raw_statement_text(sql, raw_stmt);
        out.push(classify_node(node, raw));
    }
    Ok(out)
}

/// Slice the original source for a single statement using `libpg_query`'s
/// reported `stmt_location`/`stmt_len` (byte offsets into the input). A zero
/// `stmt_len` (single trailing statement) means "to end of input".
fn raw_statement_text(sql: &str, raw_stmt: &protobuf::RawStmt) -> String {
    let start = usize::try_from(raw_stmt.stmt_location).unwrap_or(0).min(sql.len());
    let len = usize::try_from(raw_stmt.stmt_len).unwrap_or(0);
    let end = if len == 0 { sql.len() } else { (start + len).min(sql.len()) };
    sql.get(start..end).unwrap_or("").trim().to_string()
}

/// Classify a single top-level statement node.
fn classify_node(node: &NodeEnum, raw: String) -> StatementClass {
    let kind = kind_of(node);
    let additive = matches!(
        kind,
        DdlKind::CreateTable
            | DdlKind::AddColumn
            | DdlKind::CreateIndex
            | DdlKind::CreateIndexConcurrently
            | DdlKind::AddConstraint
            | DdlKind::CreateExtension
            | DdlKind::CreateFunction
            | DdlKind::CreateTrigger
    );
    let data_security = data_security_class(node);
    let destructive_operation = data_security.destructive_operation();
    let destructive = destructive_operation.is_some();
    let non_transactional = matches!(kind, DdlKind::CreateIndexConcurrently)
        || is_concurrent_drop_index(node)
        || is_alter_type_add_value(node)
        || matches!(node, NodeEnum::VacuumStmt(_));

    StatementClass {
        kind,
        additive,
        destructive,
        destructive_operation,
        data_security,
        non_transactional,
        referenced_schemas: collect_schemas(node),
        raw,
    }
}

/// Map a top-level node to its [`DdlKind`].
#[allow(clippy::too_many_lines)]
fn kind_of(node: &NodeEnum) -> DdlKind {
    match node {
        NodeEnum::CreateStmt(_) => DdlKind::CreateTable,
        NodeEnum::IndexStmt(idx) => {
            if idx.concurrent {
                DdlKind::CreateIndexConcurrently
            } else {
                DdlKind::CreateIndex
            }
        }
        NodeEnum::AlterTableStmt(at) => alter_table_kind(at),
        NodeEnum::RenameStmt(r) => {
            // ObjectColumn => RENAME COLUMN; ObjectTable => RENAME TABLE.
            if r.rename_type == ObjectType::ObjectColumn as i32 {
                DdlKind::RenameColumn
            } else if r.rename_type == ObjectType::ObjectTable as i32 {
                DdlKind::RenameTable
            } else {
                DdlKind::Other(format!("RenameStmt({})", r.rename_type))
            }
        }
        NodeEnum::DropStmt(d) => {
            if d.remove_type == ObjectType::ObjectTable as i32 {
                DdlKind::DropTable
            } else {
                DdlKind::Other(format!("DropStmt({})", d.remove_type))
            }
        }
        NodeEnum::CreateExtensionStmt(_) => DdlKind::CreateExtension,
        NodeEnum::CreateFunctionStmt(_) => DdlKind::CreateFunction,
        NodeEnum::CreateTrigStmt(_) => DdlKind::CreateTrigger,
        NodeEnum::CreateRoleStmt(_) => DdlKind::CreateRole,
        NodeEnum::GrantStmt(_) | NodeEnum::GrantRoleStmt(_) => DdlKind::Grant,
        NodeEnum::AlterSystemStmt(_) => DdlKind::AlterSystem,
        NodeEnum::CopyStmt(_) => DdlKind::Copy,
        NodeEnum::SelectStmt(_) => DdlKind::Select,
        NodeEnum::InsertStmt(_)
        | NodeEnum::UpdateStmt(_)
        | NodeEnum::DeleteStmt(_)
        | NodeEnum::MergeStmt(_)
        | NodeEnum::TruncateStmt(_) => DdlKind::Dml,
        other => DdlKind::Other(node_variant_name(other)),
    }
}

/// Map an `ALTER TABLE` to a single [`DdlKind`] from its first interesting
/// subcommand. A multi-action ALTER (rare in generated migrations) is
/// summarised by its first command; the guard still walks *all* subcommands
/// for danger via `collect_schemas` + the deny-list, so this summary is for
/// classification display, not safety.
fn alter_table_kind(at: &protobuf::AlterTableStmt) -> DdlKind {
    for cmd in &at.cmds {
        if let Some(NodeEnum::AlterTableCmd(c)) = cmd.node.as_ref() {
            let st = c.subtype;
            if st == AlterTableType::AtAddColumn as i32 {
                return DdlKind::AddColumn;
            } else if st == AlterTableType::AtDropColumn as i32 {
                return DdlKind::DropColumn;
            } else if st == AlterTableType::AtAlterColumnType as i32 {
                return DdlKind::AlterColumnType;
            } else if st == AlterTableType::AtSetNotNull as i32 {
                return DdlKind::SetNotNull;
            } else if st == AlterTableType::AtAddConstraint as i32 {
                return DdlKind::AddConstraint;
            } else if st == AlterTableType::AtDropConstraint as i32 {
                return DdlKind::DropConstraint;
            }
        }
    }
    DdlKind::Other("AlterTableStmt".to_string())
}

/// The canonical destructive/data-lossy statement classifier.
///
/// Security policy (`data_security.destructive_ops`) intentionally consumes this
/// classifier output instead of maintaining a second SQL-shape list in the guard.
/// Keep every data-lossy shape here so plan flags, approval, warnings, and
/// forbid-mode denials stay in lockstep.
#[must_use]
pub fn destructive_operation(node: &NodeEnum) -> Option<&'static str> {
    data_security_class(node).destructive_operation()
}

/// Trichotomy used by `data_security.destructive_ops`.
///
/// `forbid` must allow only [`DataSecurityClass::NonDestructive`]. The positive
/// allowlist here is intentionally explicit; every statement shape in neither
/// the destructive set nor this allowlist is [`DataSecurityClass::Unknown`].
#[must_use]
pub fn data_security_class(node: &NodeEnum) -> DataSecurityClass {
    if let Some(operation) = destructive_operation_inner(node) {
        return DataSecurityClass::Destructive(operation);
    }
    if is_non_destructive_statement(node) {
        return DataSecurityClass::NonDestructive;
    }
    DataSecurityClass::Unknown
}

fn destructive_operation_inner(node: &NodeEnum) -> Option<&'static str> {
    match node {
        NodeEnum::TruncateStmt(_) => Some("TRUNCATE"),
        NodeEnum::DropOwnedStmt(_) => Some("DROP OWNED"),
        NodeEnum::DropStmt(drop) => destructive_drop_operation(drop.remove_type),
        NodeEnum::UpdateStmt(update) => destructive_update_operation(update),
        NodeEnum::DeleteStmt(delete) => destructive_delete_operation(delete),
        NodeEnum::MergeStmt(merge) if merge_has_matched_delete(merge) => {
            Some("MERGE matched DELETE")
        }
        NodeEnum::AlterTableStmt(at) => destructive_alter_table_operation(at),
        _ => None,
    }
}

fn is_non_destructive_statement(node: &NodeEnum) -> bool {
    match node {
        NodeEnum::CreateStmt(_)
        | NodeEnum::IndexStmt(_)
        | NodeEnum::RenameStmt(_)
        | NodeEnum::CommentStmt(_)
        | NodeEnum::ViewStmt(_)
        | NodeEnum::CreateTableAsStmt(_)
        | NodeEnum::RefreshMatViewStmt(_)
        | NodeEnum::CreateEnumStmt(_)
        | NodeEnum::CompositeTypeStmt(_)
        | NodeEnum::CreateRangeStmt(_)
        | NodeEnum::AlterEnumStmt(_)
        | NodeEnum::AlterTypeStmt(_)
        | NodeEnum::CreateDomainStmt(_)
        | NodeEnum::AlterDomainStmt(_)
        | NodeEnum::CreateSeqStmt(_)
        | NodeEnum::AlterSeqStmt(_)
        | NodeEnum::CreateExtensionStmt(_)
        | NodeEnum::CreateFunctionStmt(_)
        | NodeEnum::AlterFunctionStmt(_)
        | NodeEnum::CreateTrigStmt(_)
        | NodeEnum::CreateRoleStmt(_)
        | NodeEnum::AlterRoleStmt(_)
        | NodeEnum::AlterRoleSetStmt(_)
        | NodeEnum::DropRoleStmt(_)
        | NodeEnum::GrantStmt(_)
        | NodeEnum::GrantRoleStmt(_)
        | NodeEnum::AlterDefaultPrivilegesStmt(_)
        | NodeEnum::CreateSchemaStmt(_)
        | NodeEnum::CreatePolicyStmt(_)
        | NodeEnum::CopyStmt(_)
        | NodeEnum::VariableSetStmt(_)
        | NodeEnum::TransactionStmt(_)
        | NodeEnum::SelectStmt(_)
        | NodeEnum::InsertStmt(_)
        | NodeEnum::VacuumStmt(_)
        | NodeEnum::ClusterStmt(_)
        | NodeEnum::ReindexStmt(_) => true,
        NodeEnum::AlterTableStmt(at) => alter_table_is_non_destructive(at),
        _ => false,
    }
}

fn alter_table_is_non_destructive(at: &protobuf::AlterTableStmt) -> bool {
    at.cmds.iter().all(|cmd| {
        let Some(NodeEnum::AlterTableCmd(c)) = cmd.node.as_ref() else {
            return false;
        };
        let subtype = c.subtype;
        [
            AlterTableType::AtAddColumn,
            AlterTableType::AtColumnDefault,
            AlterTableType::AtCookedColumnDefault,
            AlterTableType::AtDropNotNull,
            AlterTableType::AtSetNotNull,
            AlterTableType::AtSetStatistics,
            AlterTableType::AtSetOptions,
            AlterTableType::AtResetOptions,
            AlterTableType::AtSetStorage,
            AlterTableType::AtSetCompression,
            AlterTableType::AtAddIndex,
            AlterTableType::AtAddConstraint,
            AlterTableType::AtAlterConstraint,
            AlterTableType::AtValidateConstraint,
            AlterTableType::AtAddIndexConstraint,
            AlterTableType::AtSetRelOptions,
            AlterTableType::AtResetRelOptions,
            AlterTableType::AtSetIdentity,
            AlterTableType::AtDropIdentity,
            AlterTableType::AtAddIdentity,
            AlterTableType::AtAttachPartition,
            AlterTableType::AtEnableRowSecurity,
            AlterTableType::AtForceRowSecurity,
            AlterTableType::AtNoForceRowSecurity,
            AlterTableType::AtDisableRowSecurity,
        ]
        .iter()
        .any(|allowed| subtype == *allowed as i32)
    })
}

fn destructive_update_operation(update: &protobuf::UpdateStmt) -> Option<&'static str> {
    match update.where_clause.as_deref().and_then(|n| n.node.as_ref()) {
        None => Some("UPDATE without WHERE"),
        Some(where_clause) if is_static_true_expr(where_clause) => {
            Some("UPDATE with non-restrictive WHERE")
        }
        Some(_) => None,
    }
}

fn destructive_delete_operation(delete: &protobuf::DeleteStmt) -> Option<&'static str> {
    match delete.where_clause.as_deref().and_then(|n| n.node.as_ref()) {
        None => Some("DELETE without WHERE"),
        Some(where_clause) if is_static_true_expr(where_clause) => {
            Some("DELETE with non-restrictive WHERE")
        }
        Some(_) => None,
    }
}

fn merge_has_matched_delete(merge: &protobuf::MergeStmt) -> bool {
    merge.merge_when_clauses.iter().any(|clause| {
        matches!(
            clause.node.as_ref(),
            Some(NodeEnum::MergeWhenClause(c))
                if c.match_kind == MergeMatchKind::MergeWhenMatched as i32
                    && c.command_type == CmdType::CmdDelete as i32
        )
    })
}

fn is_static_true_expr(node: &NodeEnum) -> bool {
    match node {
        NodeEnum::AConst(c) => const_bool(c) == Some(true),
        NodeEnum::AExpr(expr) => {
            expr.kind == AExprKind::AexprOp as i32
                && operator_is(expr.name.as_slice(), "=")
                && expr
                    .lexpr
                    .as_ref()
                    .and_then(|n| n.node.as_ref())
                    .and_then(const_value)
                    .zip(
                        expr.rexpr
                            .as_ref()
                            .and_then(|n| n.node.as_ref())
                            .and_then(const_value),
                    )
                    .is_some_and(|(left, right)| left == right)
        }
        NodeEnum::BoolExpr(expr) if expr.boolop == BoolExprType::AndExpr as i32 => expr
            .args
            .iter()
            .all(|arg| arg.node.as_ref().is_some_and(is_static_true_expr)),
        NodeEnum::BoolExpr(expr) if expr.boolop == BoolExprType::OrExpr as i32 => expr
            .args
            .iter()
            .any(|arg| arg.node.as_ref().is_some_and(is_static_true_expr)),
        NodeEnum::BoolExpr(expr) if expr.boolop == BoolExprType::NotExpr as i32 => expr
            .args
            .first()
            .and_then(|arg| arg.node.as_ref())
            .is_some_and(is_static_false_expr),
        _ => false,
    }
}

fn is_static_false_expr(node: &NodeEnum) -> bool {
    match node {
        NodeEnum::AConst(c) => const_bool(c) == Some(false),
        NodeEnum::AExpr(expr) => {
            expr.kind == AExprKind::AexprOp as i32
                && operator_is(expr.name.as_slice(), "=")
                && expr
                    .lexpr
                    .as_ref()
                    .and_then(|n| n.node.as_ref())
                    .and_then(const_value)
                    .zip(
                        expr.rexpr
                            .as_ref()
                            .and_then(|n| n.node.as_ref())
                            .and_then(const_value),
                    )
                    .is_some_and(|(left, right)| left != right)
        }
        NodeEnum::BoolExpr(expr) if expr.boolop == BoolExprType::AndExpr as i32 => expr
            .args
            .iter()
            .any(|arg| arg.node.as_ref().is_some_and(is_static_false_expr)),
        NodeEnum::BoolExpr(expr) if expr.boolop == BoolExprType::OrExpr as i32 => expr
            .args
            .iter()
            .all(|arg| arg.node.as_ref().is_some_and(is_static_false_expr)),
        NodeEnum::BoolExpr(expr) if expr.boolop == BoolExprType::NotExpr as i32 => expr
            .args
            .first()
            .and_then(|arg| arg.node.as_ref())
            .is_some_and(is_static_true_expr),
        _ => false,
    }
}

fn operator_is(name: &[protobuf::Node], op: &str) -> bool {
    name.iter()
        .filter_map(|n| string_value(n.node.as_ref()))
        .next_back()
        .is_some_and(|s| s == op)
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ConstValue {
    Bool(bool),
    Int(i32),
    Float(String),
    String(String),
}

fn const_value(node: &NodeEnum) -> Option<ConstValue> {
    let NodeEnum::AConst(c) = node else {
        return None;
    };
    if c.isnull {
        return None;
    }
    match c.val.as_ref()? {
        protobuf::a_const::Val::Boolval(v) => Some(ConstValue::Bool(v.boolval)),
        protobuf::a_const::Val::Ival(v) => Some(ConstValue::Int(v.ival)),
        protobuf::a_const::Val::Fval(v) => Some(ConstValue::Float(v.fval.clone())),
        protobuf::a_const::Val::Sval(v) => Some(ConstValue::String(v.sval.clone())),
        protobuf::a_const::Val::Bsval(v) => Some(ConstValue::String(v.bsval.clone())),
    }
}

fn const_bool(c: &protobuf::AConst) -> Option<bool> {
    match c.val.as_ref()? {
        protobuf::a_const::Val::Boolval(v) => Some(v.boolval),
        _ => None,
    }
}

fn destructive_drop_operation(remove_type: i32) -> Option<&'static str> {
    match ObjectType::try_from(remove_type).ok()? {
        ObjectType::ObjectTable => Some("DROP TABLE"),
        ObjectType::ObjectColumn => Some("DROP COLUMN"),
        ObjectType::ObjectSchema => Some("DROP SCHEMA"),
        ObjectType::ObjectView => Some("DROP VIEW"),
        ObjectType::ObjectMatview => Some("DROP MATERIALIZED VIEW"),
        ObjectType::ObjectSequence => Some("DROP SEQUENCE"),
        ObjectType::ObjectType => Some("DROP TYPE"),
        ObjectType::ObjectDomain => Some("DROP DOMAIN"),
        ObjectType::ObjectFunction | ObjectType::ObjectProcedure | ObjectType::ObjectRoutine => {
            Some("DROP FUNCTION")
        }
        ObjectType::ObjectIndex => Some("DROP INDEX"),
        ObjectType::ObjectTabconstraint | ObjectType::ObjectDomconstraint => {
            Some("DROP CONSTRAINT")
        }
        _ => None,
    }
}

fn destructive_alter_table_operation(at: &protobuf::AlterTableStmt) -> Option<&'static str> {
    at.cmds.iter().find_map(|cmd| {
        let Some(NodeEnum::AlterTableCmd(c)) = cmd.node.as_ref() else {
            return None;
        };
        if c.subtype == AlterTableType::AtDropColumn as i32 {
            Some("DROP COLUMN")
        } else if c.subtype == AlterTableType::AtDropConstraint as i32 {
            Some("DROP CONSTRAINT")
        } else if c.subtype == AlterTableType::AtAlterColumnType as i32 {
            Some("ALTER COLUMN TYPE")
        } else if c.subtype == AlterTableType::AtDetachPartition as i32
            || c.subtype == AlterTableType::AtDetachPartitionFinalize as i32
        {
            Some("DETACH PARTITION")
        } else {
            None
        }
    })
}

/// `DROP INDEX CONCURRENTLY` also cannot run in a transaction.
const fn is_concurrent_drop_index(node: &NodeEnum) -> bool {
    matches!(node, NodeEnum::DropStmt(d) if d.concurrent)
}

/// `ALTER TYPE … ADD VALUE` cannot run in a transaction block (pre-PG12
/// behaviour the engine still honours conservatively).
const fn is_alter_type_add_value(node: &NodeEnum) -> bool {
    matches!(node, NodeEnum::AlterEnumStmt(e) if !e.new_val.is_empty())
}

/// Collect every explicitly schema-qualified object name referenced anywhere
/// in a statement tree (deduped, first-seen order).
///
/// Public so the security guard (`crate::guard`) can run the same
/// cross-schema confinement check over any node, including ones nested in a
/// re-parsed function/DO body.
#[must_use]
pub fn referenced_schemas(node: &NodeEnum) -> Vec<String> {
    collect_schemas(node)
}

/// Collect every explicitly schema-qualified object name referenced anywhere
/// in the statement tree (deduped, first-seen order).
///
/// Walks all `RangeVar` nodes via the `libpg_query` node iterator — this catches
/// schema qualification in FROM/JOIN, DML targets, ALTER/DROP targets, etc.
fn collect_schemas(node: &NodeEnum) -> Vec<String> {
    let mut seen: Vec<String> = Vec::new();
    for (n, _depth, _ctx, _f) in node.nodes() {
        let schema = match n {
            NodeRef::RangeVar(v) => Some(&v.schemaname),
            _ => None,
        };
        if let Some(s) = schema {
            if !s.is_empty() && !seen.iter().any(|e| e == s) {
                seen.push(s.clone());
            }
        }
    }
    // DROP / RENAME / ALTER targets sometimes carry the schema in a qualified
    // name list rather than a RangeVar; pull those too.
    collect_qualified_object_schemas(node, &mut seen);
    seen
}

/// Pull schema names out of qualified object-name lists (DROP SCHEMA s,
/// DROP TABLE s.t, etc.) that are not modelled as `RangeVar`.
fn collect_qualified_object_schemas(node: &NodeEnum, seen: &mut Vec<String>) {
    if let NodeEnum::DropStmt(d) = node {
        for obj in &d.objects {
            // For DROP TABLE the object is a List of String nodes
            // [schema, table]; for DROP SCHEMA it is a single String (the
            // schema name itself — which IS a cross-schema target).
            if let Some(NodeEnum::List(list)) = obj.node.as_ref() {
                let parts: Vec<String> = list
                    .items
                    .iter()
                    .filter_map(|i| string_value(i.node.as_ref()))
                    .collect();
                if parts.len() >= 2 {
                    push_unique(seen, parts[0].clone());
                }
            }
        }
        // DROP SCHEMA <name>: the schema name is the target itself.
        if d.remove_type == ObjectType::ObjectSchema as i32 {
            for obj in &d.objects {
                if let Some(s) = string_value(obj.node.as_ref()) {
                    push_unique(seen, s);
                }
            }
        }
    }
}

fn push_unique(seen: &mut Vec<String>, s: String) {
    if !s.is_empty() && !seen.iter().any(|e| e == &s) {
        seen.push(s);
    }
}

/// Extract the inner string of a `String`/`Ident`-shaped node.
fn string_value(node: Option<&NodeEnum>) -> Option<String> {
    match node {
        Some(NodeEnum::String(s)) => Some(s.sval.clone()),
        _ => None,
    }
}

/// A stable name for a `NodeEnum` variant (for `DdlKind::Other`).
fn node_variant_name(node: &NodeEnum) -> String {
    // Debug of the enum starts with the variant name, e.g. "VacuumStmt(...)".
    let dbg = format!("{node:?}");
    dbg.split(['(', ' ', '{'])
        .next()
        .unwrap_or("Unknown")
        .to_string()
}

// ---------------------------------------------------------------------------
// Ownership-relevant relation extraction (HIGH-2)
// ---------------------------------------------------------------------------

/// Whether a statement **establishes** ownership of the relation it targets (a
/// `CREATE TABLE` — the deploying app becomes the owner) or **requires** existing
/// ownership of it (an `ALTER`/`DROP`/`RENAME`/DML — a non-owner may not touch it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OwnershipNeed {
    /// `CREATE TABLE` — establishes ownership for the deploying app.
    Establishes,
    /// `ALTER` / `RENAME` / DML (INSERT/UPDATE/DELETE/TRUNCATE) / `CREATE INDEX` —
    /// requires the deploying app to already own the target relation.
    RequiresOwnership,
    /// `DROP TABLE` — requires ownership; a target of UNKNOWN ownership fails
    /// closed (distinct so the caller can raise `DropOfUnownedTable`, mirroring the
    /// declarative differ).
    RequiresOwnershipForDrop,
}

/// One ownership-relevant relation a statement touches: the BARE relation name
/// (schema qualifier stripped — ownership maps are keyed by table name, as in the
/// declarative differ) and the [`OwnershipNeed`] the statement places on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TouchedRelation {
    /// The bare relation name (no schema qualifier).
    pub relation: String,
    /// What the statement needs of this relation's ownership.
    pub need: OwnershipNeed,
}

/// Extract the ownership-relevant target relations of every statement in `sql`,
/// in source order, for the submit-path ownership enforcement (HIGH-2 — the
/// adapter peer of the declarative differ's `enforce_ownership`).
///
/// The submit path takes a raw `up` script, so — unlike the declarative differ,
/// which diffs structured snapshots — it must read the targets back out of the
/// parse tree. This is the SAME real-Postgres parse the guard uses (`pg_query`),
/// so it cannot be bypassed by exotic syntax a hand-rolled parser would misread.
///
/// # Shapes covered (the ownership check is enforced on these)
/// - `CREATE TABLE t` ⇒ [`OwnershipNeed::Establishes`] for `t`.
/// - `ALTER TABLE t …` ⇒ [`OwnershipNeed::RequiresOwnership`].
/// - `DROP TABLE t` ⇒ [`OwnershipNeed::RequiresOwnershipForDrop`] (per table).
/// - `RENAME TABLE`/`RENAME COLUMN` (a `RenameStmt` whose `relation` is set) ⇒
///   [`OwnershipNeed::RequiresOwnership`].
/// - `INSERT`/`UPDATE`/`DELETE` into a relation, and `TRUNCATE t [, …]` ⇒
///   [`OwnershipNeed::RequiresOwnership`] (per target relation).
/// - `CREATE INDEX … ON t` ⇒ [`OwnershipNeed::RequiresOwnership`] of `t`.
///
/// # Shapes intentionally NOT producing an ownership target (PUNTED)
/// These touch no project table or are confinement-checked elsewhere, so they
/// yield NO [`TouchedRelation`] (the caller does not gate them on ownership):
/// - relation-less DDL: `CREATE EXTENSION`/`FUNCTION`/`TRIGGER`, `CREATE SCHEMA`,
///   `COMMENT`, enum `ALTER TYPE … ADD VALUE`, etc. (the guard's deny-list +
///   cross-schema confinement own these).
/// - `SELECT` (read-only — no ownership write semantics).
/// - `MERGE` (not modelled as `DdlKind::Dml` target here — punted; a MERGE that
///   writes a foreign table is NOT caught by this pass and falls to the line-2
///   least-privilege `migrator` role; noted, not silently narrowed).
/// - DROP of a non-table object (index/view/sequence/…): the table-ownership map
///   keys on tables, so a non-table DROP yields no target (guard + role govern it).
/// - relations named only via a string literal passed to a name-resolving builtin
///   (`to_regclass('other.t')`, `nextval`): invisible to the structural walk
///   (the guard's literal-schema checks cover cross-schema; cross-app ownership
///   inside the SAME schema via a literal is punted to the role).
///
/// # Errors
/// [`ParseError::Syntax`] if `libpg_query` cannot parse the input (deny-by-default
/// upstream — the guard already rejects unparseable SQL before this is reached).
pub fn relations_touched(sql: &str) -> Result<Vec<TouchedRelation>, ParseError> {
    let parsed = pg_query::parse(sql).map_err(|e| ParseError::Syntax(e.to_string()))?;
    let mut out = Vec::new();
    for raw_stmt in &parsed.protobuf.stmts {
        let Some(node) = raw_stmt.stmt.as_ref().and_then(|s| s.node.as_ref()) else {
            continue;
        };
        collect_touched(node, &mut out);
    }
    Ok(out)
}

/// Push the ownership-relevant target(s) of one top-level statement node.
fn collect_touched(node: &NodeEnum, out: &mut Vec<TouchedRelation>) {
    match node {
        NodeEnum::CreateStmt(c) => {
            if let Some(rel) = c.relation.as_ref() {
                push_touched(out, &rel.relname, OwnershipNeed::Establishes);
            }
        }
        NodeEnum::AlterTableStmt(at) => {
            if let Some(rel) = at.relation.as_ref() {
                push_touched(out, &rel.relname, OwnershipNeed::RequiresOwnership);
            }
        }
        NodeEnum::RenameStmt(r) => {
            if let Some(rel) = r.relation.as_ref() {
                push_touched(out, &rel.relname, OwnershipNeed::RequiresOwnership);
            }
        }
        NodeEnum::IndexStmt(idx) => {
            if let Some(rel) = idx.relation.as_ref() {
                push_touched(out, &rel.relname, OwnershipNeed::RequiresOwnership);
            }
        }
        NodeEnum::InsertStmt(i) => {
            if let Some(rel) = i.relation.as_ref() {
                push_touched(out, &rel.relname, OwnershipNeed::RequiresOwnership);
            }
        }
        NodeEnum::UpdateStmt(u) => {
            if let Some(rel) = u.relation.as_ref() {
                push_touched(out, &rel.relname, OwnershipNeed::RequiresOwnership);
            }
        }
        NodeEnum::DeleteStmt(d) => {
            if let Some(rel) = d.relation.as_ref() {
                push_touched(out, &rel.relname, OwnershipNeed::RequiresOwnership);
            }
        }
        NodeEnum::TruncateStmt(t) => {
            for relnode in &t.relations {
                if let Some(NodeEnum::RangeVar(rel)) = relnode.node.as_ref() {
                    push_touched(out, &rel.relname, OwnershipNeed::RequiresOwnership);
                }
            }
        }
        NodeEnum::DropStmt(d) => {
            // Only DROP TABLE keys the table-ownership map. The object is a List
            // of String nodes [schema?, table]; the LAST element is the relname.
            if d.remove_type == ObjectType::ObjectTable as i32 {
                for obj in &d.objects {
                    if let Some(NodeEnum::List(list)) = obj.node.as_ref() {
                        if let Some(last) = list
                            .items
                            .iter()
                            .filter_map(|i| string_value(i.node.as_ref()))
                            .next_back()
                        {
                            push_touched(out, &last, OwnershipNeed::RequiresOwnershipForDrop);
                        }
                    }
                }
            }
        }
        _ => {}
    }
}

/// Push a `(relation, need)` if the relname is non-empty, deduping on relname so a
/// multi-statement script touching one table repeatedly yields one entry (keeping
/// the STRONGEST need: a drop/alter is not weakened by a later establishes).
fn push_touched(out: &mut Vec<TouchedRelation>, relname: &str, need: OwnershipNeed) {
    if relname.is_empty() {
        return;
    }
    if let Some(existing) = out.iter_mut().find(|t| t.relation == relname) {
        // Keep the strongest need: a RequiresOwnership* never downgrades to
        // Establishes (a CREATE then ALTER of the same table in one script still
        // establishes; but an ALTER of a foreign table is never excused by a
        // sibling CREATE of a DIFFERENT-but-same-named relation — same name folds).
        if existing.need == OwnershipNeed::Establishes && need != OwnershipNeed::Establishes {
            existing.need = need;
        }
        return;
    }
    out.push(TouchedRelation {
        relation: relname.to_string(),
        need,
    });
}
