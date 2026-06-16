//! Statement parsing + classification (design §2.1 / §1.4 foundation).
//!
//! `classify` runs each statement through the **real Postgres parser**
//! (`pg_query`/`libpg_query`) and maps it to a [`StatementClass`]: its DDL kind,
//! whether it is additive vs destructive, whether it can run in a transaction,
//! and the set of explicitly schema-qualified objects it touches. The security
//! guard (`crate::guard`) is built on top of this.

use pg_query::protobuf::node::Node as NodeEnum;
use pg_query::protobuf::{self, AlterTableType, ObjectType};
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
    let destructive = matches!(
        kind,
        DdlKind::DropTable | DdlKind::DropColumn | DdlKind::DropConstraint
    ) || is_truncate(node)
        || is_lossy_alter_type(node);
    let non_transactional = matches!(kind, DdlKind::CreateIndexConcurrently)
        || is_concurrent_drop_index(node)
        || is_alter_type_add_value(node)
        || matches!(node, NodeEnum::VacuumStmt(_));

    StatementClass {
        kind,
        additive,
        destructive,
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
            } else if st == AlterTableType::AtAddConstraint as i32 {
                return DdlKind::AddConstraint;
            } else if st == AlterTableType::AtDropConstraint as i32 {
                return DdlKind::DropConstraint;
            }
        }
    }
    DdlKind::Other("AlterTableStmt".to_string())
}

/// True if any `ALTER TABLE` subcommand is a lossy `ALTER COLUMN … TYPE`.
/// Conservatively treats *every* type change as potentially lossy (the
/// engine cannot prove a widening is lossless without column metadata).
fn is_lossy_alter_type(node: &NodeEnum) -> bool {
    if let NodeEnum::AlterTableStmt(at) = node {
        return at.cmds.iter().any(|cmd| {
            matches!(
                cmd.node.as_ref(),
                Some(NodeEnum::AlterTableCmd(c))
                    if c.subtype == AlterTableType::AtAlterColumnType as i32
            )
        });
    }
    false
}

const fn is_truncate(node: &NodeEnum) -> bool {
    matches!(node, NodeEnum::TruncateStmt(_))
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
