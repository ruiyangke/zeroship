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

use pg_query::protobuf::AlterTableType;

use crate::classify::{classify, ParseError, StatementClass};
use crate::migration::MigrationFlags;
use denylist::rule;
use serde_json::Value;

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
            // Serialize the ONE statement subtree to JSON for the generic
            // full-tree walks (dangerous funcs + every schema reference). This
            // sidesteps `node.nodes()`, whose hand-written traversal skips
            // column DEFAULT / CHECK / VALUES / RULE-action subtrees.
            let json = serde_json::to_value(raw_stmt).unwrap_or(Value::Null);
            self.check_node(node, &json, &raw)?;
        }

        // Collect non-fatal lint advisories (lock-heavy ops, etc.).
        let mut warnings = Vec::new();
        for raw_stmt in &parsed.protobuf.stmts {
            if let Some(node) = raw_stmt.stmt.as_ref().and_then(|s| s.node.as_ref()) {
                lint_warnings(node, &mut warnings);
            }
        }

        let destructive = classes.iter().any(|c| c.destructive);
        Ok(GuardReport {
            classes,
            destructive,
            warnings,
        })
    }

    /// Check one top-level statement node (and everything nested under it).
    ///
    /// `json` is the `serde_json` serialization of the statement's `RawStmt`
    /// subtree — used by the generic full-tree walks (Root Cause 2 fix) so we
    /// visit EVERY node, including the slots `pg_query::nodes()` skips (column
    /// DEFAULT, CHECK, VALUES lists, RULE actions, SET SCHEMA targets, …).
    fn check_node(&self, node: &NodeEnum, json: &Value, raw: &str) -> Result<(), GuardError> {
        // 1. Statement-kind gate: DENY-BY-DEFAULT. Only an enumerated set of
        //    known-safe migration statements passes; everything else is denied.
        self.check_statement_kind(node, raw)?;

        // 2. Cross-schema confinement — any explicit foreign schema, anywhere
        //    in the full tree (RangeVar, SET SCHEMA newschema, CreateSchema,
        //    trigger/CALL funcname, COMMENT object, INHERIT target, …).
        self.check_cross_schema(json, raw)?;

        // 3. Dangerous function calls anywhere in the FULL expression tree
        //    (file/network functions in SELECT/DML/DEFAULT/CHECK/VALUES/etc.).
        Self::check_dangerous_functions(json, raw)?;

        // 4. Recurse into DO blocks and function bodies — the must-inspect
        //    case. A dangerous construct hidden in a body is still dangerous.
        self.check_bodies(node, raw)?;

        Ok(())
    }

    /// Statement-kind gate, **deny-by-default** (Root Cause 1 fix).
    ///
    /// A curated allowlist of known-safe migration statement kinds passes;
    /// every other statement node is denied (`UNRECOGNIZED_DANGEROUS`). The
    /// recognized-dangerous kinds are matched first so they get a precise rule
    /// id (better diagnostics) — but the *default* arm is DENY, not allow.
    #[allow(clippy::too_many_lines)]
    fn check_statement_kind(&self, node: &NodeEnum, raw: &str) -> Result<(), GuardError> {
        match node {
            // ---- Recognized-dangerous: precise rule ids ----
            // COPY … PROGRAM = shell RCE; COPY … <file> = filesystem.
            // COPY … TO STDOUT / FROM STDIN (no program, no filename) is fine.
            NodeEnum::CopyStmt(c) => {
                if c.is_program {
                    return Err(denied(rule::COPY_PROGRAM, raw));
                }
                if !c.filename.is_empty() {
                    return Err(denied(rule::COPY_FILE, raw));
                }
                // Plain COPY … TO STDOUT / FROM STDIN — safe.
                return Ok(());
            }
            // ALTER SYSTEM — cluster-wide config, always denied.
            NodeEnum::AlterSystemStmt(_) => return Err(denied(rule::ALTER_SYSTEM, raw)),
            // Role management — privilege escalation.
            NodeEnum::CreateRoleStmt(_)
            | NodeEnum::AlterRoleStmt(_)
            | NodeEnum::AlterRoleSetStmt(_)
            | NodeEnum::DropRoleStmt(_) => return Err(denied(rule::ROLE_MANAGEMENT, raw)),
            // GRANT / REVOKE / role-membership grants — privilege management.
            NodeEnum::GrantStmt(_)
            | NodeEnum::GrantRoleStmt(_)
            | NodeEnum::AlterDefaultPrivilegesStmt(_) => {
                return Err(denied(rule::PRIVILEGE_MANAGEMENT, raw))
            }
            // Database / FDW management — out of a project migrator's remit.
            NodeEnum::CreatedbStmt(_)
            | NodeEnum::AlterDatabaseStmt(_)
            | NodeEnum::AlterDatabaseSetStmt(_)
            | NodeEnum::DropdbStmt(_) => return Err(denied(rule::DATABASE_MANAGEMENT, raw)),
            NodeEnum::CreateFdwStmt(_)
            | NodeEnum::CreateForeignServerStmt(_)
            | NodeEnum::CreateForeignTableStmt(_)
            | NodeEnum::CreateUserMappingStmt(_)
            | NodeEnum::ImportForeignSchemaStmt(_) => {
                return Err(denied(rule::FDW_MANAGEMENT, raw))
            }
            // LOAD <library> — loads a shared object into the backend (RCE).
            NodeEnum::LoadStmt(_) => return Err(denied(rule::LOAD_LIBRARY, raw)),

            // ---- Allowlisted-safe (with per-kind sub-checks) ----
            NodeEnum::CreateFunctionStmt(f) => {
                // Untrusted language (plpythonu/plperlu/c/…) — RCE.
                if let Some(lang) = function_language(&f.options) {
                    if !denylist::is_trusted_language(&lang) {
                        return Err(denied(rule::UNTRUSTED_LANGUAGE, raw));
                    }
                }
                // SECURITY DEFINER — runs with the migrator's privilege once
                // installed; an escalation primitive. Deny.
                if function_is_security_definer(&f.options) {
                    return Err(denied(rule::SECURITY_DEFINER, raw));
                }
                // A persisted `SET search_path` on the function escapes
                // confinement; deny (the DefElem name is `set`).
                if function_sets_forbidden_param(&f.options) {
                    return Err(denied(rule::FUNCTION_SET_SEARCH_PATH, raw));
                }
            }
            NodeEnum::AlterFunctionStmt(a) => {
                // ALTER FUNCTION … SECURITY DEFINER / SET search_path = …
                if alter_function_is_security_definer(&a.actions) {
                    return Err(denied(rule::SECURITY_DEFINER, raw));
                }
                if alter_function_sets_forbidden_param(&a.actions) {
                    return Err(denied(rule::FUNCTION_SET_SEARCH_PATH, raw));
                }
            }
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
                // A benign typed SET (statement_timeout, etc.) is allowed.
            }
            NodeEnum::AlterTableStmt(at) => {
                // ALTER TABLE is safe ONLY for the enumerated subcommand set;
                // OWNER TO / INHERIT / REPLICA IDENTITY / generic-options are
                // out of remit and denied.
                Self::check_alter_table_cmds(at, raw)?;
            }
            NodeEnum::DropStmt(d) => {
                // DROP is safe only for the enumerated object types; DROP ROLE
                // (etc.) via the DropStmt spelling is denied.
                if d.remove_type == ObjectType::ObjectRole as i32 {
                    return Err(denied(rule::ROLE_MANAGEMENT, raw));
                }
                if !is_safe_drop_object(d.remove_type) {
                    return Err(denied(rule::UNRECOGNIZED_DANGEROUS, raw));
                }
            }
            NodeEnum::TransactionStmt(t) => {
                // BEGIN/START/COMMIT/ROLLBACK/SAVEPOINT/RELEASE/ROLLBACK TO are
                // fine; two-phase PREPARE TRANSACTION / COMMIT PREPARED /
                // ROLLBACK PREPARED reach the cluster's prepared-xact namespace
                // and are out of remit — denied.
                if !is_safe_transaction_kind(t.kind) {
                    return Err(denied(rule::UNRECOGNIZED_DANGEROUS, raw));
                }
            }

            // ---- Unconditionally-safe migration statement kinds ----
            NodeEnum::CreateStmt(_)
            | NodeEnum::IndexStmt(_)
            | NodeEnum::RenameStmt(_)
            | NodeEnum::CommentStmt(_)
            | NodeEnum::CreateTrigStmt(_)
            | NodeEnum::ViewStmt(_)
            | NodeEnum::CreateTableAsStmt(_)
            | NodeEnum::RefreshMatViewStmt(_)
            | NodeEnum::CreateEnumStmt(_)
            | NodeEnum::CompositeTypeStmt(_)
            | NodeEnum::CreateRangeStmt(_)
            | NodeEnum::AlterEnumStmt(_)
            | NodeEnum::AlterTypeStmt(_)
            | NodeEnum::CreateSeqStmt(_)
            | NodeEnum::AlterSeqStmt(_)
            | NodeEnum::SelectStmt(_)
            | NodeEnum::InsertStmt(_)
            | NodeEnum::UpdateStmt(_)
            | NodeEnum::DeleteStmt(_)
            | NodeEnum::MergeStmt(_)
            | NodeEnum::TruncateStmt(_)
            | NodeEnum::VacuumStmt(_)
            | NodeEnum::ClusterStmt(_)
            | NodeEnum::ReindexStmt(_)
            | NodeEnum::DoStmt(_) => {}

            // ---- DENY-BY-DEFAULT: every unenumerated statement kind ----
            _ => return Err(denied(rule::UNRECOGNIZED_DANGEROUS, raw)),
        }
        Ok(())
    }

    /// Reject `ALTER TABLE` subcommands outside the safe migration set.
    fn check_alter_table_cmds(
        at: &protobuf::AlterTableStmt,
        raw: &str,
    ) -> Result<(), GuardError> {
        for cmd in &at.cmds {
            if let Some(NodeEnum::AlterTableCmd(c)) = cmd.node.as_ref() {
                if !is_safe_alter_table_subtype(c.subtype) {
                    return Err(denied(rule::UNSAFE_ALTER_TABLE_CMD, raw));
                }
            }
        }
        Ok(())
    }

    /// Deny any explicit reference to a schema other than the project schema,
    /// found ANYWHERE in the full parse tree (Root Cause 2 fix).
    fn check_cross_schema(&self, json: &Value, raw: &str) -> Result<(), GuardError> {
        if let Some(schema) = foreign_schema_in_tree(json, &self.cfg.project_schema) {
            return Err(GuardError::CrossSchema {
                schema,
                statement: raw.to_string(),
            });
        }
        Ok(())
    }

    /// Deny file-access / network function calls anywhere in the FULL tree.
    fn check_dangerous_functions(json: &Value, raw: &str) -> Result<(), GuardError> {
        let mut found: Option<&'static str> = None;
        walk_func_names(json, &mut |name| {
            if denylist::list_contains_ci(denylist::FILE_ACCESS_FUNCTIONS, name) {
                found = Some(rule::FILE_ACCESS_FUNCTION);
                return true;
            }
            if denylist::list_contains_ci(denylist::NETWORK_FUNCTIONS, name) {
                found = Some(rule::NETWORK_FUNCTION);
                return true;
            }
            false
        });
        if let Some(r) = found {
            return Err(denied(r, raw));
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
                    let json = serde_json::to_value(raw_stmt).unwrap_or(Value::Null);
                    // Recurse with the inner statement's own text for accurate
                    // error reporting.
                    self.check_node(inner, &json, &inner_raw)?;
                }
            }
        }

        // (b) Re-parse embedded string literals (EXECUTE 'CREATE ROLE …') —
        //     find single-quoted SQL fragments and re-check them.
        for literal in extract_string_literals(body) {
            if let Ok(parsed) = pg_query::parse(&literal) {
                for raw_stmt in &parsed.protobuf.stmts {
                    if let Some(inner) = raw_stmt.stmt.as_ref().and_then(|s| s.node.as_ref()) {
                        let json = serde_json::to_value(raw_stmt).unwrap_or(Value::Null);
                        self.check_node(inner, &json, &literal)?;
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
        // (e) Runtime-constructed SQL: a PL/pgSQL body that builds a
        //     schema-qualified name via `format('%I.…', s)` never shows the
        //     target schema as a `schema.ident` adjacency — it's a bare
        //     string literal (`s := 'control'`) or a `format()` arg. Flag any
        //     bare literal that names a platform schema, and — when the body
        //     uses an `%I` identifier template — any bare-identifier literal
        //     that is not the project schema (reaching ANOTHER project's
        //     schema). Deny-by-default for the dynamic-SQL class.
        if let Some(schema) =
            foreign_schema_literal_in_body(body, &self.cfg.project_schema)
        {
            return Err(GuardError::CrossSchema {
                schema,
                statement: raw.to_string(),
            });
        }
        Ok(())
    }
}

/// Scan a PL/pgSQL body's **bare string literals** for a cross-tenant schema
/// name that `format('%I.…', s)`-style dynamic SQL would interpolate.
///
/// The structural/`schema.ident` checks never see these — the schema is a
/// runtime value (`s := 'control'`) or a `format()` argument, not an adjacency.
/// Two postures, both deny-by-default for the dynamic-SQL class:
///   1. Any bare literal that *is* a platform schema (`control`/`auth`/
///      `billing`) — these have no legitimate use as data in a creator body.
///   2. If the body uses an `%I` identifier-format template (the tell of
///      dynamic schema/relation interpolation), any bare *identifier* literal
///      that is not the project schema — reaching another project's schema.
fn foreign_schema_literal_in_body(body: &str, project_schema: &str) -> Option<String> {
    let uses_ident_template = body.to_ascii_lowercase().contains("%i");
    for literal in extract_string_literals(body) {
        let lit = literal.trim();
        // (1) platform schema named directly.
        if denylist::list_contains_ci(denylist::PLATFORM_SCHEMAS, lit)
            && !lit.eq_ignore_ascii_case(project_schema)
        {
            return Some(lit.to_string());
        }
        // (2) bare identifier reaching another schema under an %I template.
        if uses_ident_template
            && !lit.eq_ignore_ascii_case(project_schema)
            && is_bare_identifier(lit)
            && looks_like_schema_name(lit)
        {
            return Some(lit.to_string());
        }
    }
    None
}

/// A literal that is a single bare SQL identifier (`[A-Za-z_][A-Za-z0-9_]*`),
/// the shape a schema/relation name interpolated via `%I` would take.
fn is_bare_identifier(s: &str) -> bool {
    let mut bytes = s.bytes();
    match bytes.next() {
        Some(b) if b.is_ascii_alphabetic() || b == b'_' => {}
        _ => return false,
    }
    bytes.all(is_ident_byte)
}

/// Heuristic: does a bare identifier look like a schema name a migration would
/// target? We flag the platform schemas plus anything matching the project
/// prefix convention (`project_…`) — the multi-tenant schemas a body could
/// reach. A short data token like `'active'` does not match, avoiding
/// false-positives on legitimate seed data passed through `%I`-bearing bodies.
fn looks_like_schema_name(s: &str) -> bool {
    let l = s.to_ascii_lowercase();
    denylist::list_contains_ci(denylist::PLATFORM_SCHEMAS, &l) || l.starts_with("project_")
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

/// Derive the migration flags from a passing [`GuardReport`] (design §1.6).
///
/// - `destructive` (data loss) ⇒ `requires_approval` (the gate must confirm;
///   AI never auto-applies destructive ops).
/// - any non-transactional statement (CONCURRENTLY, ALTER TYPE ADD VALUE,
///   VACUUM) ⇒ `transactional = false` (the two-phase apply path).
///
/// `online` is an authoring-time facet (expand-contract sequencing), not
/// derivable from a single SQL blob, so it stays at its default here.
#[must_use]
pub fn flags_for(report: &GuardReport) -> MigrationFlags {
    let non_transactional = report.classes.iter().any(|c| c.non_transactional);
    MigrationFlags {
        transactional: !non_transactional,
        destructive: report.destructive,
        online: false,
        requires_approval: report.destructive,
    }
}

/// Append non-fatal lint advisories for lock-heavy / rewrite-forcing ops.
fn lint_warnings(node: &NodeEnum, warnings: &mut Vec<String>) {
    match node {
        // Plain CREATE INDEX (not CONCURRENTLY) holds a SHARE lock blocking
        // writes for the whole build — suggest CONCURRENTLY.
        NodeEnum::IndexStmt(idx) if !idx.concurrent => {
            warnings.push(format!(
                "CREATE INDEX on '{}' is not CONCURRENTLY: it blocks writes for the build; \
                 prefer CREATE INDEX CONCURRENTLY on a populated table",
                idx.relation.as_ref().map_or("?", |r| r.relname.as_str())
            ));
        }
        // ADD COLUMN … NOT NULL DEFAULT <volatile> forces a full table rewrite
        // under ACCESS EXCLUSIVE — warn. A constant default is the metadata-only
        // fast path (PG11+) and is not flagged.
        NodeEnum::AlterTableStmt(at) => {
            for cmd in &at.cmds {
                if let Some(NodeEnum::AlterTableCmd(c)) = cmd.node.as_ref() {
                    if c.subtype == AlterTableType::AtAddColumn as i32 {
                        if let Some(NodeEnum::ColumnDef(col)) =
                            c.def.as_ref().and_then(|d| d.node.as_ref())
                        {
                            if column_is_not_null_with_volatile_default(col) {
                                warnings.push(format!(
                                    "ADD COLUMN '{}' NOT NULL with a volatile DEFAULT forces a full \
                                     table rewrite under an ACCESS EXCLUSIVE lock; backfill in a \
                                     separate step or use a constant default",
                                    col.colname
                                ));
                            }
                        }
                    }
                }
            }
        }
        _ => {}
    }
}

/// True if a column def has BOTH a NOT NULL constraint and a *volatile*
/// DEFAULT expression (a function call — `now()`, `gen_random_uuid()`,
/// `random()`, etc.). A literal/constant default is the fast path and returns
/// false.
fn column_is_not_null_with_volatile_default(col: &protobuf::ColumnDef) -> bool {
    use pg_query::protobuf::ConstrType;
    let mut not_null = col.is_not_null;
    let mut volatile_default = false;
    for con in &col.constraints {
        if let Some(NodeEnum::Constraint(c)) = con.node.as_ref() {
            if c.contype == ConstrType::ConstrNotnull as i32 {
                not_null = true;
            }
            if c.contype == ConstrType::ConstrDefault as i32 {
                // A DEFAULT whose expression contains a function call is treated
                // as volatile (conservative: we cannot prove stability without
                // catalog lookup). A bare const (A_Const) is the fast path.
                if let Some(expr) = c.raw_expr.as_ref().and_then(|e| e.node.as_ref()) {
                    volatile_default = expr_contains_func_call(expr);
                }
            }
        }
    }
    not_null && volatile_default
}

/// Does an expression tree contain any function call? (volatility heuristic)
fn expr_contains_func_call(expr: &NodeEnum) -> bool {
    expr.nodes()
        .iter()
        .any(|(n, _, _, _)| matches!(n, NodeRef::FuncCall(_)))
}

// ---------------------------------------------------------------------------
// Deny-by-default allowlist predicates (Root Cause 1)
// ---------------------------------------------------------------------------

/// The `ObjectType`s a creator migration may `DROP`. Anything else (role,
/// schema, extension, FDW, subscription, publication, …) is denied-by-default.
fn is_safe_drop_object(remove_type: i32) -> bool {
    [
        ObjectType::ObjectTable,
        ObjectType::ObjectIndex,
        ObjectType::ObjectView,
        ObjectType::ObjectMatview,
        ObjectType::ObjectSequence,
        ObjectType::ObjectType,
        ObjectType::ObjectDomain,
        ObjectType::ObjectFunction,
        ObjectType::ObjectTrigger,
        ObjectType::ObjectRule,
        ObjectType::ObjectColumn,
    ]
    .iter()
    .any(|t| remove_type == *t as i32)
}

/// The `AlterTableType` subcommands a creator migration may use. OWNER TO,
/// INHERIT, REPLICA IDENTITY, generic-options, tablespace moves, etc. are
/// denied-by-default (privilege transfer / cross-tenant reparent / out of
/// remit).
fn is_safe_alter_table_subtype(subtype: i32) -> bool {
    use AlterTableType as A;
    [
        A::AtAddColumn,
        A::AtColumnDefault,
        A::AtCookedColumnDefault,
        A::AtDropNotNull,
        A::AtSetNotNull,
        A::AtSetStatistics,
        A::AtSetOptions,
        A::AtResetOptions,
        A::AtSetStorage,
        A::AtSetCompression,
        A::AtDropColumn,
        A::AtAddIndex,
        A::AtAddConstraint,
        A::AtAlterConstraint,
        A::AtValidateConstraint,
        A::AtAddIndexConstraint,
        A::AtDropConstraint,
        A::AtAlterColumnType,
        A::AtSetRelOptions,
        A::AtResetRelOptions,
        A::AtSetIdentity,
        A::AtDropIdentity,
        A::AtAddIdentity,
    ]
    .iter()
    .any(|t| subtype == *t as i32)
}

/// Transaction-control kinds a migration may issue. Two-phase commit kinds
/// (`PREPARE TRANSACTION` / `COMMIT PREPARED` / `ROLLBACK PREPARED`) reach the
/// cluster's prepared-transaction namespace and are denied-by-default.
fn is_safe_transaction_kind(kind: i32) -> bool {
    use protobuf::TransactionStmtKind as K;
    [
        K::TransStmtBegin,
        K::TransStmtStart,
        K::TransStmtCommit,
        K::TransStmtRollback,
        K::TransStmtSavepoint,
        K::TransStmtRelease,
        K::TransStmtRollbackTo,
    ]
    .iter()
    .any(|k| kind == *k as i32)
}

/// True if a CREATE FUNCTION carries the `security` definer option.
fn function_is_security_definer(options: &[protobuf::Node]) -> bool {
    options.iter().any(|opt| {
        matches!(opt.node.as_ref(), Some(NodeEnum::DefElem(d))
            if d.defname.eq_ignore_ascii_case("security")
                && def_elem_bool(d) == Some(true))
    })
}

/// True if a CREATE FUNCTION pins a forbidden `SET <param>` (`search_path`/role).
fn function_sets_forbidden_param(options: &[protobuf::Node]) -> bool {
    options.iter().any(|opt| {
        matches!(opt.node.as_ref(), Some(NodeEnum::DefElem(d))
            if def_elem_is_forbidden_set(d))
    })
}

/// True if any ALTER FUNCTION action is `SECURITY DEFINER`.
fn alter_function_is_security_definer(actions: &[protobuf::Node]) -> bool {
    actions.iter().any(|opt| {
        matches!(opt.node.as_ref(), Some(NodeEnum::DefElem(d))
            if d.defname.eq_ignore_ascii_case("security")
                && def_elem_bool(d) == Some(true))
    })
}

/// True if any ALTER FUNCTION action is a forbidden `SET <param>`.
fn alter_function_sets_forbidden_param(actions: &[protobuf::Node]) -> bool {
    actions.iter().any(|opt| {
        matches!(opt.node.as_ref(), Some(NodeEnum::DefElem(d))
            if def_elem_is_forbidden_set(d))
    })
}

/// A function `DefElem` of the form `SET <param> = …` whose param is in
/// [`denylist::FORBIDDEN_SET_PARAMS`] (e.g. `SET search_path = control`). The
/// nested arg is a `VariableSetStmt` carrying the target param name.
fn def_elem_is_forbidden_set(d: &protobuf::DefElem) -> bool {
    if !d.defname.eq_ignore_ascii_case("set") {
        return false;
    }
    if let Some(NodeEnum::VariableSetStmt(v)) = d.arg.as_ref().and_then(|a| a.node.as_ref()) {
        return denylist::list_contains_ci(denylist::FORBIDDEN_SET_PARAMS, &v.name);
    }
    false
}

/// Read a boolean-valued `DefElem` (the `security` option carries a `Boolean`
/// arg: `SECURITY DEFINER` → true, `SECURITY INVOKER` → false).
fn def_elem_bool(d: &protobuf::DefElem) -> Option<bool> {
    match d.arg.as_ref().and_then(|a| a.node.as_ref()) {
        Some(NodeEnum::Boolean(b)) => Some(b.boolval),
        Some(NodeEnum::Integer(i)) => Some(i.ival != 0),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Generic full-parse-tree JSON walkers (Root Cause 2)
// ---------------------------------------------------------------------------

/// Walk the ENTIRE serialized parse tree and invoke `visit` with the trailing
/// name part of every `FuncCall` / `CallStmt` function name found anywhere
/// (column DEFAULT, CHECK, VALUES lists, RULE actions, sub-selects — every
/// slot, unlike `pg_query::nodes()`). `visit` returns `true` to short-circuit.
fn walk_func_names(v: &Value, visit: &mut dyn FnMut(&str) -> bool) -> bool {
    match v {
        Value::Object(map) => {
            // A FuncCall (or CallStmt's funccall) carries a `funcname` array of
            // String nodes; the trailing element is the bare function name.
            if let Some(Value::Array(parts)) = map.get("funcname") {
                if let Some(name) = json_last_string_part(parts) {
                    if visit(&name) {
                        return true;
                    }
                }
            }
            for child in map.values() {
                if walk_func_names(child, visit) {
                    return true;
                }
            }
            false
        }
        Value::Array(items) => items.iter().any(|i| walk_func_names(i, visit)),
        _ => false,
    }
}

/// Walk the ENTIRE serialized parse tree for any explicit reference to a schema
/// other than `project_schema`, covering every slot a schema name can hide in:
///   - `RangeVar.schemaname` (tables in FROM/DML/ALTER/DROP/INHERIT targets)
///   - `AlterObjectSchemaStmt.newschema` (`… SET SCHEMA control`)
///   - `CreateSchemaStmt.schemaname` (`CREATE SCHEMA control`)
///   - qualified name lists: trigger/CALL `funcname`, `CommentStmt.object`,
///     DROP object lists — a 2+-part `[schema, object]` String list.
///
/// Returns the first foreign schema found.
///
/// Neutral schemas are excluded so legitimate migrations are not over-denied:
///   - the server's own catalogs (`pg_catalog`, `pg_temp`,
///     `information_schema`) are never a tenant — qualified builtins like
///     `pg_catalog.length(…)` or `value::pg_catalog.int4` are benign and
///     skipped in **every** slot.
///   - `public` is the shared default schema; an explicit `public.fn(…)`
///     function qualification is routine, so `public` is skipped **only** in
///     the function-name slot. A *table*/object reference to `public` (a
///     `RangeVar` or COMMENT/DROP target) is still flagged — those reach a
///     concrete object outside the pinned project schema.
fn foreign_schema_in_tree(v: &Value, project_schema: &str) -> Option<String> {
    let mut found: Option<String> = None;
    walk_schema_names(v, &mut |schema, slot| {
        if schema.is_empty() || schema.eq_ignore_ascii_case(project_schema) {
            return false;
        }
        if is_system_catalog_schema(schema) {
            return false;
        }
        if slot == SchemaSlot::FuncName && schema.eq_ignore_ascii_case("public") {
            return false;
        }
        found = Some(schema.to_string());
        true
    });
    found
}

/// True for the server's own catalog/temp schemas — never a cross-tenant
/// target, so qualified references to them are benign in any slot.
fn is_system_catalog_schema(schema: &str) -> bool {
    ["pg_catalog", "pg_temp", "pg_toast", "information_schema"]
        .iter()
        .any(|s| schema.eq_ignore_ascii_case(s))
}

/// Which kind of parse-tree slot a candidate schema name came from. Drives the
/// `public`-is-benign-for-functions relaxation in [`foreign_schema_in_tree`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SchemaSlot {
    /// `RangeVar.schemaname` / `AlterObjectSchemaStmt.newschema` /
    /// `CreateSchemaStmt.schemaname` / `CommentStmt.object` — a concrete
    /// relation/object/schema target.
    Object,
    /// A function-name qualifier (`FuncCall`/trigger/CALL `funcname`).
    FuncName,
}

/// The traversal behind [`foreign_schema_in_tree`]. Invokes `visit(schema,
/// slot)` for every candidate schema string; returns `true` once `visit`
/// short-circuits.
fn walk_schema_names(v: &Value, visit: &mut dyn FnMut(&str, SchemaSlot) -> bool) -> bool {
    match v {
        Value::Object(map) => {
            // Direct schema-name string fields (concrete object/schema target).
            for key in ["schemaname", "newschema"] {
                if let Some(Value::String(s)) = map.get(key) {
                    if !s.is_empty() && visit(s, SchemaSlot::Object) {
                        return true;
                    }
                }
            }
            // Qualified function-name lists: trigger/CALL/FuncCall `funcname`.
            if let Some(schema) = qualified_list_schema(map.get("funcname")) {
                if visit(&schema, SchemaSlot::FuncName) {
                    return true;
                }
            }
            // Qualified object lists: COMMENT/DROP `object` ([schema, object]).
            if let Some(schema) = qualified_list_schema(map.get("object")) {
                if visit(&schema, SchemaSlot::Object) {
                    return true;
                }
            }
            for child in map.values() {
                if walk_schema_names(child, visit) {
                    return true;
                }
            }
            false
        }
        Value::Array(items) => items.iter().any(|i| walk_schema_names(i, visit)),
        _ => false,
    }
}

/// If `v` is a qualified-name list with 2+ String parts, return the FIRST part
/// (the schema qualifier). A single-part list is an unqualified name (no
/// schema) and returns `None`.
///
/// Handles both spellings the parse tree uses:
///   - a bare array (`CreateTrigStmt.funcname`, `CallStmt…funcname`):
///     `[{node:{String}}, …]`
///   - a `List` node (`CommentStmt.object`): `{node:{List:{items:[…]}}}`
fn qualified_list_schema(v: Option<&Value>) -> Option<String> {
    let arr = match v {
        Some(Value::Array(a)) => a.as_slice(),
        Some(obj) => match obj.get("node").and_then(|n| n.get("List")).and_then(|l| l.get("items"))
        {
            Some(Value::Array(a)) => a.as_slice(),
            _ => return None,
        },
        None => return None,
    };
    let parts: Vec<String> = arr.iter().filter_map(json_string_node).collect();
    if parts.len() >= 2 {
        Some(parts[0].clone())
    } else {
        None
    }
}

/// The trailing String of a `funcname`-style array (the bare name).
fn json_last_string_part(parts: &[Value]) -> Option<String> {
    parts.iter().rev().find_map(json_string_node)
}

/// Extract the inner string of a `{"node":{"String":{"sval":"…"}}}` value.
fn json_string_node(v: &Value) -> Option<String> {
    v.get("node")?
        .get("String")?
        .get("sval")?
        .as_str()
        .map(str::to_string)
}

/// Build a [`GuardError::Denied`].
fn denied(rule: &'static str, statement: &str) -> GuardError {
    GuardError::Denied {
        rule,
        statement: statement.to_string(),
    }
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
