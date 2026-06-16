//! The SQL security guard — parse-time deny-list + cross-schema confinement
//! (design §1.4 / §1.5). **The security heart of the engine.**
//!
//! Migrations are privileged arbitrary-SQL authored by untrusted creators AND a
//! prompt-injectable AI. This guard is the *first* line of defense-in-depth: it
//! parses every statement with the real Postgres parser and rejects the
//! dangerous set (RCE / privilege-escalation / cross-tenant / file / SSRF)
//! *regardless of the submitted SQL*. The least-privilege `migrator` role
//! (built later) is the second line — the DB rejects the same ops even if SQL
//! slips past parse.
//!
//! Two postures, by threat class:
//! - **Deny** (hard error): RCE, privilege escalation, cross-tenant access,
//!   filesystem/network reach. These can never be auto-confirmed.
//! - **Flag** (`GuardReport.destructive`): data loss (`DROP`/`TRUNCATE`/lossy
//!   type change). The guard does not deny these — the gate (built later)
//!   decides on data loss. The guard only surfaces them.
//!
//! **Deny-by-default:** an unrecognized statement that *could* be dangerous is
//! denied, not waved through.

pub mod denylist;

use pg_query::protobuf::node::Node as NodeEnum;
use pg_query::protobuf::{self, ObjectType};
use pg_query::NodeRef;

use crate::classify::{classify, ParseError, StatementClass};
use denylist::rule;

/// Per-project guard configuration.
#[derive(Debug, Clone)]
pub struct GuardConfig {
    /// The one schema this project's migrations may touch. Any explicitly
    /// schema-qualified reference to a *different* schema is a cross-tenant
    /// violation.
    pub project_schema: String,
    /// Extensions this project is permitted to `CREATE EXTENSION`. Anything
    /// not listed is denied (deny-by-default), and the [`denylist`]'s
    /// `FORBIDDEN_EXTENSIONS` are denied even if mistakenly listed here.
    pub extension_allowlist: Vec<String>,
}

/// A guard rejection.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GuardError {
    /// A hard-denied dangerous construct (RCE / priv-esc / file / network).
    #[error("denied by rule '{rule}': {statement}")]
    Denied {
        /// The stable rule id (see [`denylist::rule`]).
        rule: &'static str,
        /// The offending statement text.
        statement: String,
    },
    /// A reference to a schema outside the project schema (cross-tenant).
    #[error("cross-schema access to '{schema}' denied: {statement}")]
    CrossSchema {
        /// The foreign schema that was referenced.
        schema: String,
        /// The offending statement text.
        statement: String,
    },
    /// The SQL could not be parsed (deny-by-default: it never reaches the DB).
    #[error("parse error: {0}")]
    Parse(#[from] ParseError),
}

/// The result of a passing [`SqlGuard::check`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuardReport {
    /// The classification of every statement (in order).
    pub classes: Vec<StatementClass>,
    /// True if *any* statement is destructive (data loss). The gate decides.
    pub destructive: bool,
    /// Non-fatal advisories (lock-heavy ops, etc. — filled in Task 5).
    pub warnings: Vec<String>,
}

/// The SQL security guard.
#[derive(Debug, Clone)]
pub struct SqlGuard {
    cfg: GuardConfig,
}

impl SqlGuard {
    /// Construct a guard for a project.
    #[must_use]
    pub const fn new(cfg: GuardConfig) -> Self {
        Self { cfg }
    }

    /// Check a migration's SQL. Returns a [`GuardReport`] if every statement is
    /// safe (destructive ops flagged, not denied), or a [`GuardError`] on the
    /// first dangerous/cross-tenant/unparseable construct.
    ///
    /// # Errors
    /// - [`GuardError::Denied`] — a hard-denied construct (incl. ones nested
    ///   inside `DO $$…$$` blocks and function bodies).
    /// - [`GuardError::CrossSchema`] — a reference outside the project schema.
    /// - [`GuardError::Parse`] — unparseable SQL (deny-by-default).
    pub fn check(&self, sql: &str) -> Result<GuardReport, GuardError> {
        let classes = classify(sql)?;

        // Walk the full parse tree once per statement for danger + bodies.
        let parsed = pg_query::parse(sql).map_err(|e| ParseError::Syntax(e.to_string()))?;
        for raw_stmt in &parsed.protobuf.stmts {
            let Some(node) = raw_stmt.stmt.as_ref().and_then(|s| s.node.as_ref()) else {
                continue;
            };
            let raw = stmt_text(sql, raw_stmt);
            self.check_node(node, &raw)?;
        }

        let destructive = classes.iter().any(|c| c.destructive);
        Ok(GuardReport {
            classes,
            destructive,
            warnings: Vec::new(),
        })
    }

    /// Check one top-level statement node (and everything nested under it).
    fn check_node(&self, node: &NodeEnum, raw: &str) -> Result<(), GuardError> {
        // 1. Statement-kind denies (the coarse, structural rules).
        self.check_statement_kind(node, raw)?;

        // 2. Cross-schema confinement — any explicit foreign schema is denied.
        self.check_cross_schema(node, raw)?;

        // 3. Dangerous function calls anywhere in the expression tree
        //    (file/network functions in SELECT/DML/DEFAULT/CHECK/etc.).
        Self::check_dangerous_functions(node, raw)?;

        // 4. Recurse into DO blocks and function bodies — the must-inspect
        //    case. A dangerous construct hidden in a body is still dangerous.
        self.check_bodies(node, raw)?;

        Ok(())
    }

    /// Structural per-statement-kind deny rules.
    #[allow(clippy::too_many_lines)]
    fn check_statement_kind(&self, node: &NodeEnum, raw: &str) -> Result<(), GuardError> {
        match node {
            // COPY … PROGRAM = shell RCE; COPY … <file> = filesystem.
            // COPY … TO STDOUT / FROM STDIN (no program, no filename) is fine.
            NodeEnum::CopyStmt(c) => {
                if c.is_program {
                    return Err(denied(rule::COPY_PROGRAM, raw));
                }
                if !c.filename.is_empty() {
                    return Err(denied(rule::COPY_FILE, raw));
                }
            }
            // CREATE FUNCTION/PROCEDURE in an untrusted language.
            NodeEnum::CreateFunctionStmt(f) => {
                if let Some(lang) = function_language(&f.options) {
                    if !denylist::is_trusted_language(&lang) {
                        return Err(denied(rule::UNTRUSTED_LANGUAGE, raw));
                    }
                }
            }
            // CREATE EXTENSION — deny-by-default unless allowlisted, and never
            // the hard-forbidden set.
            NodeEnum::CreateExtensionStmt(e) => {
                let name = e.extname.to_ascii_lowercase();
                if denylist::list_contains_ci(denylist::FORBIDDEN_EXTENSIONS, &name) {
                    return Err(denied(rule::FORBIDDEN_EXTENSION, raw));
                }
                let allowed = self
                    .cfg
                    .extension_allowlist
                    .iter()
                    .any(|a| a.eq_ignore_ascii_case(&name));
                if !allowed {
                    return Err(denied(rule::EXTENSION_NOT_ALLOWLISTED, raw));
                }
            }
            // ALTER SYSTEM — cluster-wide config, always denied.
            NodeEnum::AlterSystemStmt(_) => return Err(denied(rule::ALTER_SYSTEM, raw)),
            // Role management — privilege escalation.
            NodeEnum::CreateRoleStmt(_)
            | NodeEnum::AlterRoleStmt(_)
            | NodeEnum::AlterRoleSetStmt(_) => {
                return Err(denied(rule::ROLE_MANAGEMENT, raw))
            }
            // DROP ROLE / DROP USER — its own statement node, plus the
            // DropStmt-with-ObjectRole spelling some clients emit.
            NodeEnum::DropRoleStmt(_) => return Err(denied(rule::ROLE_MANAGEMENT, raw)),
            NodeEnum::DropStmt(d) if d.remove_type == ObjectType::ObjectRole as i32 => {
                return Err(denied(rule::ROLE_MANAGEMENT, raw))
            }
            // GRANT / REVOKE / role-membership grants — privilege management.
            NodeEnum::GrantStmt(_)
            | NodeEnum::GrantRoleStmt(_)
            | NodeEnum::AlterDefaultPrivilegesStmt(_) => {
                return Err(denied(rule::PRIVILEGE_MANAGEMENT, raw))
            }
            // SET search_path / role / session authorization — confinement
            // escape. VariableSetStmt.kind for SET ROLE is also caught here.
            NodeEnum::VariableSetStmt(s) => {
                let name = s.name.to_ascii_lowercase();
                if denylist::list_contains_ci(denylist::FORBIDDEN_SET_PARAMS, &name) {
                    return Err(denied(rule::FORBIDDEN_SET, raw));
                }
                // SET ROLE / SET SESSION AUTHORIZATION carry an empty `name`
                // but a dedicated kind; deny by the raw-text shape as a belt.
                let r = raw.to_ascii_lowercase();
                if r.starts_with("set role")
                    || r.starts_with("set session authorization")
                    || r.starts_with("set local role")
                {
                    return Err(denied(rule::SET_ROLE, raw));
                }
            }
            // Database / FDW management — out of a project migrator's remit.
            NodeEnum::CreatedbStmt(_)
            | NodeEnum::AlterDatabaseStmt(_)
            | NodeEnum::AlterDatabaseSetStmt(_)
            | NodeEnum::DropdbStmt(_) => {
                return Err(denied(rule::DATABASE_MANAGEMENT, raw))
            }
            NodeEnum::CreateFdwStmt(_)
            | NodeEnum::CreateForeignServerStmt(_)
            | NodeEnum::CreateForeignTableStmt(_)
            | NodeEnum::CreateUserMappingStmt(_) => {
                return Err(denied(rule::FDW_MANAGEMENT, raw))
            }
            // LOAD <library> — loads a shared object into the backend (RCE).
            NodeEnum::LoadStmt(_) => return Err(denied(rule::LOAD_LIBRARY, raw)),
            _ => {}
        }
        Ok(())
    }

    /// Deny any explicit reference to a schema other than the project schema.
    fn check_cross_schema(&self, node: &NodeEnum, raw: &str) -> Result<(), GuardError> {
        let schemas = crate::classify::referenced_schemas(node);
        for schema in schemas {
            if !schema.eq_ignore_ascii_case(&self.cfg.project_schema) {
                return Err(GuardError::CrossSchema {
                    schema,
                    statement: raw.to_string(),
                });
            }
        }
        Ok(())
    }

    /// Deny file-access / network function calls anywhere in the tree.
    fn check_dangerous_functions(node: &NodeEnum, raw: &str) -> Result<(), GuardError> {
        for (n, _d, _c, _f) in node.nodes() {
            let fname = match n {
                NodeRef::FuncCall(fc) => last_name_part(&fc.funcname),
                _ => None,
            };
            if let Some(name) = fname {
                if denylist::list_contains_ci(denylist::FILE_ACCESS_FUNCTIONS, &name) {
                    return Err(denied(rule::FILE_ACCESS_FUNCTION, raw));
                }
                if denylist::list_contains_ci(denylist::NETWORK_FUNCTIONS, &name) {
                    return Err(denied(rule::NETWORK_FUNCTION, raw));
                }
            }
        }
        Ok(())
    }

    /// Recurse into DO-block + function bodies and re-check the embedded SQL.
    ///
    /// PL/pgSQL and SQL bodies are opaque *strings* in the parse tree, so a
    /// dangerous construct inside them is invisible to a top-level walk. We:
    ///   1. extract every body string (DO `args`, CREATE FUNCTION `as`);
    ///   2. attempt to re-parse it as SQL and recurse the guard (catches
    ///      embedded statements + EXECUTE 'literal sql');
    ///   3. additionally token-scan the body text for dangerous names that a
    ///      partial PL/pgSQL parse would miss (deny-by-default).
    fn check_bodies(&self, node: &NodeEnum, raw: &str) -> Result<(), GuardError> {
        let bodies: Vec<String> = match node {
            NodeEnum::DoStmt(d) => def_elem_string_args(&d.args),
            NodeEnum::CreateFunctionStmt(f) => function_body_strings(&f.options),
            _ => Vec::new(),
        };
        for body in bodies {
            self.check_body_text(&body, raw)?;
        }
        Ok(())
    }

    /// Check one body string: re-parse + recurse, then token-scan.
    fn check_body_text(&self, body: &str, raw: &str) -> Result<(), GuardError> {
        // (a) Re-parse the body (and any EXECUTE 'literal') as SQL and recurse.
        //     PL/pgSQL wrappers (BEGIN/END/PERFORM) won't fully parse, so this
        //     is best-effort; the token scan below is the backstop.
        if let Ok(parsed) = pg_query::parse(body) {
            for raw_stmt in &parsed.protobuf.stmts {
                if let Some(inner) = raw_stmt.stmt.as_ref().and_then(|s| s.node.as_ref()) {
                    let inner_raw = stmt_text(body, raw_stmt);
                    // Recurse with the inner statement's own text for accurate
                    // error reporting.
                    self.check_node(inner, &inner_raw)?;
                }
            }
        }

        // (b) Re-parse embedded string literals (EXECUTE 'CREATE ROLE …') —
        //     find single-quoted SQL fragments and re-check them.
        for literal in extract_string_literals(body) {
            if let Ok(parsed) = pg_query::parse(&literal) {
                for raw_stmt in &parsed.protobuf.stmts {
                    if let Some(inner) = raw_stmt.stmt.as_ref().and_then(|s| s.node.as_ref()) {
                        self.check_node(inner, &literal)?;
                    }
                }
            }
        }

        // (c) Token-scan backstop — catch dangerous names a partial parse of a
        //     PL/pgSQL body would never surface as a FuncCall/Stmt node.
        let lower = body.to_ascii_lowercase();
        for &f in denylist::FILE_ACCESS_FUNCTIONS {
            if word_present(&lower, f) {
                return Err(denied(rule::BODY_INSPECTION, raw));
            }
        }
        for &f in denylist::NETWORK_FUNCTIONS {
            if word_present(&lower, f) {
                return Err(denied(rule::BODY_INSPECTION, raw));
            }
        }
        // search_path escape / alter system / role mgmt hidden in EXECUTE text.
        for needle in ["alter system", "create role", "create user", "drop role"] {
            if lower.contains(needle) {
                return Err(denied(rule::BODY_INSPECTION, raw));
            }
        }
        if lower.contains("search_path") {
            return Err(denied(rule::BODY_INSPECTION, raw));
        }
        // COPY … PROGRAM hidden in a body.
        if lower.contains("program") && lower.contains("copy") {
            return Err(denied(rule::BODY_INSPECTION, raw));
        }
        // Untrusted-language nested CREATE FUNCTION inside a body.
        if lower.contains("language plpythonu")
            || lower.contains("language plperlu")
            || lower.contains("language c ")
        {
            return Err(denied(rule::BODY_INSPECTION, raw));
        }
        // (d) Cross-schema: any `schema.` qualifier in the body that is not the
        //     project schema is a cross-tenant reference the body re-parse
        //     could not surface (PL/pgSQL BEGIN/END wrappers don't parse as
        //     plain SQL). Deny-by-default.
        if let Some(schema) = foreign_schema_in_body(body, &self.cfg.project_schema) {
            return Err(GuardError::CrossSchema {
                schema,
                statement: raw.to_string(),
            });
        }
        Ok(())
    }
}

/// Scan a body string for a `<schema>.<object>` qualifier that names a known
/// **platform schema** (`control`/`auth`/`billing`) — the cross-tenant target
/// a prompt-injected migration would aim at. Returns that schema.
///
/// This is a lexical backstop for PL/pgSQL bodies that do not parse as plain
/// SQL (so the structural `RangeVar` check never sees them). It is deliberately
/// scoped to the platform schemas rather than "any dotted identifier" so it
/// does not false-positive on PL/pgSQL record fields (`NEW.col`, `OLD.col`) or
/// table-alias column refs (`p.id`). Cross-references to *another project's*
/// schema still go through real parsed statements (CREATE/INSERT/DROP carry a
/// `RangeVar`), which `check_cross_schema` catches structurally; and the
/// project's own role/pinned-search_path is the runtime confinement for the
/// rest. `project_schema` is excluded so a project legitimately naming its own
/// schema in a body is fine.
fn foreign_schema_in_body(body: &str, project_schema: &str) -> Option<String> {
    let bytes = body.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // Find a dot with an identifier char on both sides.
        if bytes[i] == b'.' && i > 0 && is_ident_byte(bytes[i - 1]) {
            // Walk left to the start of the left identifier.
            let mut s = i;
            while s > 0 && is_ident_byte(bytes[s - 1]) {
                s -= 1;
            }
            if bytes.get(i + 1).copied().is_some_and(is_ident_byte) {
                let schema = &body[s..i];
                if !schema.eq_ignore_ascii_case(project_schema)
                    && denylist::list_contains_ci(denylist::PLATFORM_SCHEMAS, schema)
                {
                    return Some(schema.to_string());
                }
            }
        }
        i += 1;
    }
    None
}

/// Build a [`GuardError::Denied`].
fn denied(rule: &'static str, statement: &str) -> GuardError {
    GuardError::Denied {
        rule,
        statement: statement.to_string(),
    }
}

/// The trailing identifier of a (possibly schema-qualified) name list, e.g.
/// `pg_catalog.pg_read_file` → `pg_read_file`.
fn last_name_part(parts: &[protobuf::Node]) -> Option<String> {
    parts.iter().rev().find_map(|n| match n.node.as_ref() {
        Some(NodeEnum::String(s)) => Some(s.sval.clone()),
        _ => None,
    })
}

/// Extract the `language` option of a CREATE FUNCTION.
fn function_language(options: &[protobuf::Node]) -> Option<String> {
    for opt in options {
        if let Some(NodeEnum::DefElem(d)) = opt.node.as_ref() {
            if d.defname.eq_ignore_ascii_case("language") {
                if let Some(NodeEnum::String(s)) = d.arg.as_ref().and_then(|a| a.node.as_ref()) {
                    return Some(s.sval.clone());
                }
            }
        }
    }
    None
}

/// Extract the body string(s) of a CREATE FUNCTION (`AS $$…$$`). The `as`
/// `DefElem`'s arg is a List of String nodes.
fn function_body_strings(options: &[protobuf::Node]) -> Vec<String> {
    let mut out = Vec::new();
    for opt in options {
        if let Some(NodeEnum::DefElem(d)) = opt.node.as_ref() {
            if d.defname.eq_ignore_ascii_case("as") {
                if let Some(arg) = d.arg.as_ref().and_then(|a| a.node.as_ref()) {
                    match arg {
                        NodeEnum::String(s) => out.push(s.sval.clone()),
                        NodeEnum::List(list) => {
                            for item in &list.items {
                                if let Some(NodeEnum::String(s)) = item.node.as_ref() {
                                    out.push(s.sval.clone());
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
    }
    out
}

/// Extract `DefElem` string args (DO block body lives in such an arg).
fn def_elem_string_args(args: &[protobuf::Node]) -> Vec<String> {
    let mut out = Vec::new();
    for a in args {
        if let Some(NodeEnum::DefElem(d)) = a.node.as_ref() {
            if let Some(NodeEnum::String(s)) = d.arg.as_ref().and_then(|x| x.node.as_ref()) {
                out.push(s.sval.clone());
            }
        }
    }
    out
}

/// Pull single-quoted string literals out of a body (best-effort, for
/// `EXECUTE 'literal sql'`). Handles doubled-quote `''` escapes.
fn extract_string_literals(body: &str) -> Vec<String> {
    let mut out = Vec::new();
    let bytes = body.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\'' {
            let mut j = i + 1;
            let mut buf = String::new();
            while j < bytes.len() {
                if bytes[j] == b'\'' {
                    if j + 1 < bytes.len() && bytes[j + 1] == b'\'' {
                        buf.push('\'');
                        j += 2;
                        continue;
                    }
                    break;
                }
                buf.push(bytes[j] as char);
                j += 1;
            }
            if !buf.is_empty() {
                out.push(buf);
            }
            i = j + 1;
        } else {
            i += 1;
        }
    }
    out
}

/// Whole-word match (so `pg_read_file` does not match `my_pg_read_files`).
fn word_present(haystack: &str, needle: &str) -> bool {
    let mut start = 0;
    while let Some(pos) = haystack[start..].find(needle) {
        let abs = start + pos;
        let before = abs.checked_sub(1).map(|p| haystack.as_bytes()[p]);
        let after = haystack.as_bytes().get(abs + needle.len()).copied();
        let ok_before = before.is_none_or(|b| !is_ident_byte(b));
        let ok_after = after.is_none_or(|b| !is_ident_byte(b));
        if ok_before && ok_after {
            return true;
        }
        start = abs + 1;
    }
    false
}

const fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Slice the original source for a statement using its byte offsets.
fn stmt_text(sql: &str, raw_stmt: &protobuf::RawStmt) -> String {
    let start = usize::try_from(raw_stmt.stmt_location).unwrap_or(0).min(sql.len());
    let len = usize::try_from(raw_stmt.stmt_len).unwrap_or(0);
    let end = if len == 0 { sql.len() } else { (start + len).min(sql.len()) };
    sql.get(start..end).unwrap_or("").trim().to_string()
}
