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

        // 2b. System-catalog relation reads/writes — `pg_catalog.pg_authid`,
        //     unqualified `pg_shadow`/`pg_user`, `information_schema.*`. These
        //     leak roles/passwords/source and are never a project's own table.
        Self::check_system_catalog_relations(json, raw)?;

        // 3. Dangerous function calls anywhere in the FULL expression tree
        //    (file/network functions in SELECT/DML/DEFAULT/CHECK/VALUES/etc.).
        Self::check_dangerous_functions(json, raw)?;

        // 3b. Belt: a `'pg_read_file'::regprocedure` / `::regproc` cast names a
        //     dangerous function as a TEXT literal the FuncCall walk never sees.
        Self::check_regproc_casts(json, raw)?;

        // 3c. Cross-schema via a STRING LITERAL argument: a schema-qualified
        //     object named inside a literal passed to a `reg*` cast or a
        //     name-resolving builtin (`nextval`/`setval`/`to_regclass`/…) is an
        //     `A_Const`, invisible to the structural schema walker. Re-check the
        //     literal's leading `schema.` qualifier for confinement.
        self.check_literal_schema_refs(json, raw)?;

        // 3d. `set_config('search_path'|'role'|…, …)` is the function form of a
        //     `SET <param>` the structural VariableSetStmt gate denies — deny
        //     the call form identically.
        Self::check_set_config_calls(json, raw)?;

        // 3e. `query_to_xml('SELECT … FROM control.users', …)` & family take a
        //     free-form SQL string the server executes; the embedded SQL is
        //     never re-parsed by the walks above, so a cross-schema read,
        //     file-access function, or DDL hidden in the literal slips past.
        //     Re-parse each such literal and run the SAME guard recursively
        //     (reusing `check_body_text`, the body re-parse machinery).
        //     A non-literal/runtime query arg is out of parse-scope — line-2
        //     (least-priv `migrator` role) defense, same limit as `set_config`.
        self.check_sql_string_arg_calls(json, raw)?;

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
                // The funcname is the CREATION TARGET, not a call qualifier:
                // defining a function INTO `public`/`pg_catalog`/
                // `information_schema`/another tenant schema is denied (no
                // shared-schema exemption — that applies only to call sites).
                self.check_func_def_target(&f.funcname, raw)?;
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
                // ALTER FUNCTION targets an existing function; touching one in
                // a shared/system/foreign schema is out of remit (the func is
                // named in `func.objname`, an ObjectWithArgs).
                if let Some(func) = a.func.as_ref() {
                    self.check_func_def_target(&func.objname, raw)?;
                }
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

    /// Deny a function-DEFINING statement whose funcname targets a schema
    /// other than the project's own. The funcname here is a *creation target*
    /// (`CREATE FUNCTION public.evil()` / `ALTER FUNCTION control.f()`), NOT a
    /// call qualifier — so the `public`/`pg_catalog`/`information_schema`
    /// exemptions that apply at call sites do NOT apply: defining into any
    /// non-project schema is denied. `name` is the funcname/objname list of
    /// protobuf String nodes; an unqualified name (single part) is fine — it
    /// resolves under the pinned `search_path`.
    fn check_func_def_target(
        &self,
        name: &[protobuf::Node],
        raw: &str,
    ) -> Result<(), GuardError> {
        let parts: Vec<&str> = name
            .iter()
            .filter_map(|n| match n.node.as_ref() {
                Some(NodeEnum::String(s)) => Some(s.sval.as_str()),
                _ => None,
            })
            .collect();
        if parts.len() >= 2 {
            let schema = parts[0];
            if !schema.eq_ignore_ascii_case(&self.cfg.project_schema) {
                return Err(GuardError::CrossSchema {
                    schema: schema.to_string(),
                    statement: raw.to_string(),
                });
            }
        }
        Ok(())
    }

    /// Deny any `RangeVar` relation read/write that targets a system catalog.
    ///
    /// Catches both spellings the cross-schema walk cannot:
    ///   - qualified `pg_catalog.pg_authid` / `information_schema.tables`
    ///     (a `pg_catalog`/`information_schema` `RangeVar.schemaname`);
    ///   - **unqualified** `pg_shadow` / `pg_user` / `pg_authid` — the schema
    ///     is empty so the cross-schema walk sees nothing, but the `pg_`
    ///     prefix is reserved for system catalogs (a creator relation may not
    ///     use it), so an unqualified `pg_*` relation resolves to the catalog.
    fn check_system_catalog_relations(json: &Value, raw: &str) -> Result<(), GuardError> {
        let mut found = false;
        walk_range_vars(json, &mut |schema, relname| {
            let catalog_schema = is_neutral_catalog_schema(schema);
            let catalog_relname = relname.len() >= 3 && relname[..3].eq_ignore_ascii_case("pg_");
            if catalog_schema || (schema.is_empty() && catalog_relname) {
                found = true;
                return true;
            }
            false
        });
        if found {
            return Err(denied(rule::SYSTEM_CATALOG_ACCESS, raw));
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

    /// Deny a `<literal>::regprocedure` / `::regproc` cast whose literal names a
    /// `FILE_ACCESS` / `NETWORK` function. `'pg_read_file'::regprocedure` resolves
    /// the named function by OID at runtime — a dangerous capability the
    /// `FuncCall` walk misses because the function name is a bare string
    /// literal, not a call node. The literal may carry an argument signature
    /// (`'pg_read_file(text)'`); we match on the leading identifier.
    fn check_regproc_casts(json: &Value, raw: &str) -> Result<(), GuardError> {
        let mut found: Option<&'static str> = None;
        walk_regproc_casts(json, &mut |fname| {
            if denylist::list_contains_ci(denylist::FILE_ACCESS_FUNCTIONS, fname) {
                found = Some(rule::FILE_ACCESS_FUNCTION);
                return true;
            }
            if denylist::list_contains_ci(denylist::NETWORK_FUNCTIONS, fname) {
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

    /// Deny a schema-qualified object named inside a STRING LITERAL passed to a
    /// `reg*` cast or a name-resolving builtin.
    ///
    /// `'control.users'::regclass`, `nextval('control.s')`,
    /// `setval('control.billing_seq', 0)`, `to_regclass('control.users')`,
    /// `pg_get_serial_sequence('control.t','id')` all reach (read *or*
    /// mutate) a foreign-tenant object whose schema lives in an `A_Const`
    /// string literal — invisible to [`foreign_schema_in_tree`], which only
    /// sees structural qualified-name nodes. Here we parse the literal's
    /// leading `schema.` qualifier and run it through the SAME cross-schema
    /// policy (own-schema OK; otherwise denied — these are concrete object
    /// targets, so no shared-schema exemption, same as
    /// [`SchemaSlot::Object`]).
    ///
    /// SCOPE: only LITERAL arguments in these specific positions. A plain data
    /// literal (`INSERT … VALUES ('control.t')`) is untouched. A
    /// runtime-constructed argument (`nextval(some_var)`, `nextval('a'||'b')`)
    /// is NOT an `A_Const` and cannot be resolved at parse time — that is the
    /// line-2 (least-priv `migrator` role) defense's job, same limit the
    /// `format('%I', …)` arm acknowledges.
    fn check_literal_schema_refs(&self, json: &Value, raw: &str) -> Result<(), GuardError> {
        let mut found: Option<String> = None;
        walk_literal_schema_refs(json, &mut |literal, is_namespace_resolver| {
            // For `regnamespace`/`to_regnamespace` the literal IS a bare schema
            // name (`'control'`); for object resolvers the schema is the
            // leading `schema.` qualifier (`'control.t'`).
            let schema = if is_namespace_resolver {
                let s = literal.trim().trim_matches('"').trim();
                if s.is_empty() {
                    return false;
                }
                s.to_string()
            } else {
                match literal_schema_qualifier(literal) {
                    Some(s) => s,
                    None => return false,
                }
            };
            if schema.eq_ignore_ascii_case(&self.cfg.project_schema) {
                return false;
            }
            // Concrete object target — Object-slot policy: no shared-schema
            // exemption (`public.t`/`pg_catalog.t`/`control.t` all denied).
            found = Some(schema);
            true
        });
        if let Some(schema) = found {
            return Err(GuardError::CrossSchema {
                schema,
                statement: raw.to_string(),
            });
        }
        Ok(())
    }

    /// Deny `set_config('search_path'|'role'|'session_authorization', …)`.
    ///
    /// `set_config(param, value, is_local)` is the function-call form of `SET
    /// param = value`. The structural [`NodeEnum::VariableSetStmt`] gate denies
    /// a `SET search_path`/`role`/`session_authorization`, but a `FuncCall`
    /// slips past it — so the call form is denied identically by matching the
    /// first string-literal argument against [`denylist::FORBIDDEN_SET_PARAMS`].
    /// A benign GUC (`statement_timeout`) stays allowed, mirroring the
    /// structural SET allowance. (Runtime-constructed param names are not
    /// literals and are out of parse-time scope — the line-2 role defense.)
    fn check_set_config_calls(json: &Value, raw: &str) -> Result<(), GuardError> {
        let mut denied_param = false;
        walk_set_config_calls(json, &mut |param| {
            if denylist::list_contains_ci(denylist::FORBIDDEN_SET_PARAMS, param) {
                denied_param = true;
                return true;
            }
            false
        });
        if denied_param {
            return Err(denied(rule::FORBIDDEN_SET, raw));
        }
        Ok(())
    }

    /// Deny dangerous SQL hidden in a `query_to_xml`-family string-literal arg.
    ///
    /// The XML-emitting table functions ([`denylist::SQL_STRING_ARG_FUNCTIONS`])
    /// take a free-form SQL string as their first argument that the server then
    /// executes. The structural func-walk and cross-schema walk are blind to it
    /// (the SQL lives in an `A_Const` text literal, not a parse subtree). We
    /// extract each such literal and run the SAME guard recursively via
    /// [`Self::check_body_text`] — re-parse + recurse (catching cross-schema
    /// reads, file/network funcs, embedded DDL) plus the token-scan + body
    /// cross-schema backstops. Only the literal (`A_Const`) form is in
    /// parse-scope; a runtime-constructed query arg is the line-2 role's job.
    fn check_sql_string_arg_calls(&self, json: &Value, raw: &str) -> Result<(), GuardError> {
        let mut sql_literals: Vec<String> = Vec::new();
        walk_sql_string_arg_calls(json, &mut |literal| {
            sql_literals.push(literal.to_string());
            false
        });
        for literal in sql_literals {
            // Re-run the FULL guard on the embedded SQL, attributing any denial
            // to the enclosing statement's text for accurate reporting.
            self.check_body_text(&literal, raw)?;
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
        // No per-migration timeout derivable from a single SQL blob; the author
        // sets it explicitly when a long backfill/index needs a higher ceiling.
        timeout_ms: None,
        // A guard-derived flag set is for one-shot SQL; the online expand/contract
        // phase is set by the ExpandContractAuthor, never inferred from SQL.
        phase: None,
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
        // Partition (de)attach. The partition's RangeVar is walked by
        // `check_cross_schema` independently, so an own-schema partition is
        // safe and a cross-schema one (`… ATTACH PARTITION control.x`) is
        // still denied there.
        A::AtAttachPartition,
        A::AtDetachPartition,
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

/// Walk the ENTIRE serialized parse tree for `RangeVar` nodes (relation
/// references), invoking `visit(schemaname, relname)` for each. A `RangeVar` is
/// the object carrying a `relname` *and* a `schemaname` sibling. `visit`
/// returns `true` to short-circuit.
fn walk_range_vars(v: &Value, visit: &mut dyn FnMut(&str, &str) -> bool) -> bool {
    match v {
        Value::Object(map) => {
            if let (Some(Value::String(rel)), Some(Value::String(schema))) =
                (map.get("relname"), map.get("schemaname"))
            {
                if visit(schema, rel) {
                    return true;
                }
            }
            map.values().any(|c| walk_range_vars(c, visit))
        }
        Value::Array(items) => items.iter().any(|i| walk_range_vars(i, visit)),
        _ => false,
    }
}

/// Walk the ENTIRE serialized parse tree for `TypeCast` nodes whose target type
/// is `regprocedure`/`regproc`/`regprocedureout`-family and whose argument is a
/// string literal naming a function; invoke `visit` with the leading function
/// identifier of that literal. `visit` returns `true` to short-circuit.
fn walk_regproc_casts(v: &Value, visit: &mut dyn FnMut(&str) -> bool) -> bool {
    match v {
        Value::Object(map) => {
            if let Some(cast) = map.get("TypeCast") {
                if type_is_regproc(cast.get("type_name")) {
                    if let Some(name) = type_cast_string_literal(cast.get("arg")) {
                        if visit(regproc_leading_ident(&name)) {
                            return true;
                        }
                    }
                }
            }
            map.values().any(|c| walk_regproc_casts(c, visit))
        }
        Value::Array(items) => items.iter().any(|i| walk_regproc_casts(i, visit)),
        _ => false,
    }
}

/// Walk the ENTIRE serialized parse tree for the two string-literal-carried
/// schema leaks and invoke `visit` with the literal text:
///   - a `TypeCast` to any [`denylist::REG_TYPES`] member whose argument is a
///     string literal (`'control.users'::regclass`);
///   - a `FuncCall` to any [`denylist::NAME_RESOLVER_FUNCTIONS`] member whose
///     FIRST argument is a string literal (`nextval('control.s')`,
///     `pg_get_serial_sequence('control.t','id')`).
///
/// `visit(literal, is_namespace_resolver)` returns `true` to short-circuit.
/// `is_namespace_resolver` is `true` for `regnamespace` / `to_regnamespace`,
/// where the literal IS a bare schema name (no `schema.object` split); `false`
/// for object resolvers where the schema is the leading `schema.` qualifier.
/// Only literal (`A_Const`) arguments are inspected — runtime-constructed names
/// are not visible at parse time.
fn walk_literal_schema_refs(v: &Value, visit: &mut dyn FnMut(&str, bool) -> bool) -> bool {
    match v {
        Value::Object(map) => {
            if let Some(cast) = map.get("TypeCast") {
                if let Some(reg) = reg_family_name(cast.get("type_name")) {
                    if let Some(lit) = type_cast_string_literal(cast.get("arg")) {
                        if visit(&lit, reg.eq_ignore_ascii_case("regnamespace")) {
                            return true;
                        }
                    }
                }
            }
            if let Some(call) = map.get("FuncCall") {
                if let Some(name) = name_resolver_func(call.get("funcname")) {
                    if let Some(lit) = first_arg_string_literal(call.get("args")) {
                        if visit(&lit, name.eq_ignore_ascii_case("to_regnamespace")) {
                            return true;
                        }
                    }
                }
                // Stat/predicate builtins whose first `text` arg is a relation
                // NAME (`pg_relation_size('control.t')`,
                // `has_table_privilege('control.users','SELECT')`). The schema
                // is the literal's leading `schema.` qualifier — an object
                // resolver, not a namespace one.
                if func_is_text_relation_name(call.get("funcname")) {
                    if let Some(lit) = first_arg_string_literal(call.get("args")) {
                        if visit(&lit, false) {
                            return true;
                        }
                    }
                }
                // Schema-export builtins (`schema_to_xml('control', …)`) whose
                // first `text` arg is a bare SCHEMA NAME — a namespace resolver,
                // like `regnamespace`/`to_regnamespace`.
                if func_is_namespace_name(call.get("funcname")) {
                    if let Some(lit) = first_arg_string_literal(call.get("args")) {
                        if visit(&lit, true) {
                            return true;
                        }
                    }
                }
                // Object-address resolvers carry the schema as the FIRST element
                // of an array literal in the SECOND argument
                // (`pg_get_object_address('table', '{control,t}', …)`). That
                // element IS the schema (like a namespace resolver).
                if func_is_object_address(call.get("funcname")) {
                    if let Some(schema) = object_address_array_schema(call.get("args")) {
                        if visit(&schema, true) {
                            return true;
                        }
                    }
                }
            }
            map.values().any(|c| walk_literal_schema_refs(c, visit))
        }
        Value::Array(items) => items.iter().any(|i| walk_literal_schema_refs(i, visit)),
        _ => false,
    }
}

/// `type_name`'s trailing (bare) name if it is a member of the `reg*`
/// pseudo-type family (`pg_catalog.regclass` resolves the same), else `None`.
fn reg_family_name(type_name: Option<&Value>) -> Option<String> {
    let parts = qualified_list_parts(type_name.and_then(|t| t.get("names")))?;
    let last = parts.last()?;
    denylist::list_contains_ci(denylist::REG_TYPES, last).then(|| last.clone())
}

/// `funcname`'s trailing (bare) name if it is a name-resolving builtin
/// (`pg_catalog.nextval` resolves the same builtin), else `None`.
fn name_resolver_func(funcname: Option<&Value>) -> Option<String> {
    let parts = qualified_list_parts(funcname)?;
    let last = parts.last()?;
    denylist::list_contains_ci(denylist::NAME_RESOLVER_FUNCTIONS, last).then(|| last.clone())
}

/// Is `funcname`'s trailing (bare) name an object-address resolver whose schema
/// rides in an array literal? (`pg_catalog.pg_get_object_address` resolves the
/// same builtin.)
fn func_is_object_address(funcname: Option<&Value>) -> bool {
    let Some(parts) = qualified_list_parts(funcname) else {
        return false;
    };
    parts
        .last()
        .is_some_and(|f| denylist::list_contains_ci(denylist::OBJECT_ADDRESS_FUNCTIONS, f))
}

/// Is `funcname`'s trailing (bare) name a stat/predicate builtin whose first
/// `text` argument is a relation name? (`pg_catalog.pg_relation_size` resolves
/// the same builtin.)
fn func_is_text_relation_name(funcname: Option<&Value>) -> bool {
    let Some(parts) = qualified_list_parts(funcname) else {
        return false;
    };
    parts
        .last()
        .is_some_and(|f| denylist::list_contains_ci(denylist::TEXT_RELATION_NAME_FUNCTIONS, f))
}

/// Is `funcname`'s trailing (bare) name a schema-export builtin whose first
/// `text` argument is a bare schema name? (`pg_catalog.schema_to_xml` resolves
/// the same builtin.)
fn func_is_namespace_name(funcname: Option<&Value>) -> bool {
    let Some(parts) = qualified_list_parts(funcname) else {
        return false;
    };
    parts
        .last()
        .is_some_and(|f| denylist::list_contains_ci(denylist::NAMESPACE_NAME_FUNCTIONS, f))
}

/// The schema element of an object-address call's name array: the SECOND
/// argument is a Postgres array text literal `'{schema,object,…}'` whose first
/// element is the schema. Returns that first element, or `None` if absent.
fn object_address_array_schema(args: Option<&Value>) -> Option<String> {
    let Some(Value::Array(arr)) = args else {
        return None;
    };
    // args[1] is the object-name array literal.
    let second = arr.get(1)?;
    let lit = second
        .get("node")?
        .get("AConst")?
        .get("val")?
        .get("Sval")?
        .get("sval")?
        .as_str()?;
    parse_pg_array_first_element(lit)
}

/// First element of a Postgres array text literal (`{a,b}` → `a`, `{"a b",c}`
/// → `a b`). Best-effort: handles the unquoted and double-quoted element forms.
fn parse_pg_array_first_element(lit: &str) -> Option<String> {
    let inner = lit.trim().strip_prefix('{')?;
    let inner = inner.strip_suffix('}').unwrap_or(inner);
    let inner = inner.trim_start();
    if inner.is_empty() {
        return None;
    }
    let Some(rest) = inner.strip_prefix('"') else {
        // Unquoted element: up to the next comma.
        let first = inner.split(',').next().unwrap_or(inner).trim();
        return if first.is_empty() {
            None
        } else {
            Some(first.to_string())
        };
    };
    // Quoted element: read to the next unescaped quote.
    let mut out = String::new();
    let mut chars = rest.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                if let Some(n) = chars.next() {
                    out.push(n);
                }
            }
            '"' => break,
            other => out.push(other),
        }
    }
    Some(out)
}

/// Walk the tree for `set_config(<param-literal>, …)` calls, invoking `visit`
/// with the first string-literal argument (the GUC name). `visit` returns
/// `true` to short-circuit.
fn walk_set_config_calls(v: &Value, visit: &mut dyn FnMut(&str) -> bool) -> bool {
    match v {
        Value::Object(map) => {
            if let Some(call) = map.get("FuncCall") {
                if func_is_set_config(call.get("funcname")) {
                    if let Some(param) = first_arg_string_literal(call.get("args")) {
                        if visit(&param) {
                            return true;
                        }
                    }
                }
            }
            map.values().any(|c| walk_set_config_calls(c, visit))
        }
        Value::Array(items) => items.iter().any(|i| walk_set_config_calls(i, visit)),
        _ => false,
    }
}

/// Walk the tree for `query_to_xml`-family calls (and the rest of
/// [`denylist::SQL_STRING_ARG_FUNCTIONS`]), invoking `visit` with the first
/// string-literal argument (the embedded SQL). `visit` returns `true` to
/// short-circuit.
fn walk_sql_string_arg_calls(v: &Value, visit: &mut dyn FnMut(&str) -> bool) -> bool {
    match v {
        Value::Object(map) => {
            if let Some(call) = map.get("FuncCall") {
                if func_is_sql_string_arg(call.get("funcname")) {
                    if let Some(sql) = first_arg_string_literal(call.get("args")) {
                        if visit(&sql) {
                            return true;
                        }
                    }
                }
            }
            map.values().any(|c| walk_sql_string_arg_calls(c, visit))
        }
        Value::Array(items) => items.iter().any(|i| walk_sql_string_arg_calls(i, visit)),
        _ => false,
    }
}

/// Is `funcname`'s trailing (bare) name a `query_to_xml`-family SQL-string sink?
/// (`pg_catalog.query_to_xml` resolves the same builtin.)
fn func_is_sql_string_arg(funcname: Option<&Value>) -> bool {
    let Some(parts) = qualified_list_parts(funcname) else {
        return false;
    };
    parts
        .last()
        .is_some_and(|f| denylist::list_contains_ci(denylist::SQL_STRING_ARG_FUNCTIONS, f))
}

/// Is `funcname`'s trailing (bare) name `set_config`? (`pg_catalog.set_config`
/// resolves the same builtin.)
fn func_is_set_config(funcname: Option<&Value>) -> bool {
    let Some(parts) = qualified_list_parts(funcname) else {
        return false;
    };
    parts
        .last()
        .is_some_and(|f| f.eq_ignore_ascii_case(denylist::SET_CONFIG_FUNCTION))
}

/// The string-literal value of a `FuncCall.args[0]` (`A_Const { Sval }`), if the
/// first argument is a bare string literal. Each arg is wrapped in a `node`.
fn first_arg_string_literal(args: Option<&Value>) -> Option<String> {
    let Some(Value::Array(arr)) = args else {
        return None;
    };
    let first = arr.first()?;
    first
        .get("node")?
        .get("AConst")?
        .get("val")?
        .get("Sval")?
        .get("sval")?
        .as_str()
        .map(str::to_string)
}

/// The leading `schema.` qualifier of an object-name string literal, or `None`
/// if the literal is unqualified (a bare name) or carries no schema part.
///
/// Drops any argument signature (`schema.f(text)` → schema part of `schema.f`)
/// before splitting, then returns the first dotted component. Honors a
/// double-quoted leading component (`"My Schema".t` → `My Schema`); a leading
/// dot or empty schema yields `None`.
fn literal_schema_qualifier(lit: &str) -> Option<String> {
    // Strip a trailing argument signature (`f(int, text)`) — reg*procedure
    // literals may carry one; the schema is in the head before `(`.
    let head = lit.split('(').next().unwrap_or("").trim();
    if head.is_empty() {
        return None;
    }
    let (schema, rest) = split_first_qualifier(head);
    // A schema is present only when there is a trailing component after the
    // first dot (`control.t` → `control`; bare `t` → no schema).
    if rest.is_empty() {
        return None;
    }
    let schema = schema.trim();
    if schema.is_empty() {
        None
    } else {
        Some(schema.to_string())
    }
}

/// Split an object-name string on its FIRST top-level `.` separator, honoring a
/// double-quoted leading identifier (where `.` inside the quotes is literal).
/// Returns `(first_component, rest_after_dot)`; `rest` is `""` when there is no
/// dot (an unqualified name).
fn split_first_qualifier(s: &str) -> (String, &str) {
    let bytes = s.as_bytes();
    if bytes.first() == Some(&b'"') {
        // Quoted identifier: scan to the closing quote (doubled "" stays inside).
        let mut i = 1;
        let mut ident = String::new();
        while i < bytes.len() {
            if bytes[i] == b'"' {
                if i + 1 < bytes.len() && bytes[i + 1] == b'"' {
                    ident.push('"');
                    i += 2;
                    continue;
                }
                i += 1; // consume closing quote
                break;
            }
            ident.push(bytes[i] as char);
            i += 1;
        }
        // After the closing quote, a `.` introduces the rest.
        let rest = s.get(i..).unwrap_or("");
        let rest = rest.strip_prefix('.').unwrap_or("");
        (ident, rest)
    } else {
        match s.split_once('.') {
            Some((first, rest)) => (first.to_string(), rest),
            None => (s.to_string(), ""),
        }
    }
}

/// Does a `type_name` resolve to the `regprocedure`/`regproc` reg* family
/// (a function reference by name/OID)?
fn type_is_regproc(type_name: Option<&Value>) -> bool {
    let Some(parts) = qualified_list_parts(type_name.and_then(|t| t.get("names"))) else {
        return false;
    };
    // The bare type name is the trailing part (a `pg_catalog.regprocedure`
    // spelling is possible).
    matches!(
        parts.last().map(String::as_str),
        Some("regprocedure" | "regproc")
    )
}

/// Extract the string-literal value of a `TypeCast.arg` (`AConst { Sval }`).
fn type_cast_string_literal(arg: Option<&Value>) -> Option<String> {
    arg?.get("node")?
        .get("AConst")?
        .get("val")?
        .get("Sval")?
        .get("sval")?
        .as_str()
        .map(str::to_string)
}

/// The leading identifier of a regproc literal, dropping any argument signature
/// and schema qualifier: `pg_read_file(text)` → `pg_read_file`,
/// `pg_catalog.pg_read_file` → `pg_read_file`.
fn regproc_leading_ident(lit: &str) -> &str {
    let head = lit.trim().split('(').next().unwrap_or("").trim();
    head.rsplit('.').next().unwrap_or(head).trim()
}

/// Walk the ENTIRE serialized parse tree for any explicit reference to a schema
/// other than `project_schema`, covering every slot a schema name can hide in.
/// The walker is **typed by slot** (not a key-string allowlist): each
/// schema-qualified-name-bearing node contributes its schema with the
/// [`SchemaSlot`] that fixes the per-slot exemption policy. The slots:
///   - [`SchemaSlot::RangeVar`] — `RangeVar.schemaname` (FROM/DML/ALTER/DROP/
///     partition/INHERIT relation targets). Catalog reads
///     (`pg_catalog.pg_authid`, `information_schema.tables`) are NOT exempt.
///   - [`SchemaSlot::Object`] — `newschema` (SET SCHEMA), `CreateSchemaStmt`
///     `schemaname`, `CommentStmt`/`DropStmt` object lists, index `opclass`,
///     `COLLATE` `collname`, sequence `OWNED BY` — concrete object targets.
///     No exemption beyond own-schema.
///   - [`SchemaSlot::TypeRef`] — `TypeName.names` (column/return/param/cast/
///     `OF` type). Builtins desugar to `pg_catalog.<t>`, so `pg_catalog` (+
///     catalog/`public`) stay exempt here; a foreign tenant type is flagged.
///   - [`SchemaSlot::FuncCall`] — `FuncCall`/trigger/CALL `funcname` *call*
///     qualifier. `pg_catalog`/`information_schema`/`public` calls are routine
///     and exempt; a tenant-schema call is flagged.
///
/// Function *definition* targets (`CreateFunctionStmt`/`AlterFunctionStmt`
/// funcname) are NOT walked here — they are a *creation target*, never a call
/// qualifier, so they are checked directly against the project schema in
/// [`SqlGuard::check_func_def_target`] with NO shared-schema exemption
/// (`public.evil`, `pg_catalog.evil`, `information_schema.evil` all denied).
///
/// Returns the first foreign schema found.
fn foreign_schema_in_tree(v: &Value, project_schema: &str) -> Option<String> {
    let mut found: Option<String> = None;
    walk_schema_names(v, &mut |schema, slot| {
        if schema.is_empty() || schema.eq_ignore_ascii_case(project_schema) {
            return false;
        }
        if slot_exempts_schema(slot, schema) {
            return false;
        }
        found = Some(schema.to_string());
        true
    });
    found
}

/// Per-slot schema exemption. A neutral schema is exempt only in the slots
/// where naming it is routine and benign — never as a broad whitelist.
fn slot_exempts_schema(slot: SchemaSlot, schema: &str) -> bool {
    match slot {
        // Nothing is exempt:
        //   - RangeVar/Object: concrete relation/object targets reach a real
        //     object outside the pinned schema (catalog reads denied here);
        //   - CreationTarget: planting/altering a type INTO a schema (CREATE
        //     TYPE … AS ENUM/RANGE, ALTER TYPE …) — mirrors a function
        //     *definition* target, so `public`/`pg_catalog`/`control` are all
        //     denied, same as any other tenant schema.
        SchemaSlot::RangeVar | SchemaSlot::Object | SchemaSlot::CreationTarget => false,
        // Built-in-bearing object sub-slots: index `opclass` and `COLLATE`
        // `collname` routinely name a `pg_catalog` builtin
        // (`pg_catalog.text_ops`, `pg_catalog."C"`). `pg_catalog` ONLY is
        // exempt here (not `public`, not other catalog schemas, not a tenant
        // schema — `control.myops` stays denied).
        SchemaSlot::BuiltinObject => schema.eq_ignore_ascii_case("pg_catalog"),
        // Type references (builtins desugar to `pg_catalog.<type>`) and
        // function *call* qualifiers: catalog + `public` are routine and
        // benign. Catalog table *reads* go via RangeVar (not exempt there);
        // function *definition* targets are checked separately (no exemption).
        SchemaSlot::TypeRef | SchemaSlot::FuncCall => {
            is_neutral_catalog_schema(schema) || schema.eq_ignore_ascii_case("public")
        }
    }
}

/// The server's own catalog/temp schemas — never a cross-tenant target. Used
/// only for the slots where naming them is benign (type refs, function calls);
/// table reads from them are caught via [`SchemaSlot::RangeVar`].
fn is_neutral_catalog_schema(schema: &str) -> bool {
    ["pg_catalog", "pg_temp", "pg_toast", "information_schema"]
        .iter()
        .any(|s| schema.eq_ignore_ascii_case(s))
}

/// Which kind of parse-tree slot a candidate schema name came from. Drives the
/// per-slot exemption policy in [`slot_exempts_schema`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SchemaSlot {
    /// `RangeVar.schemaname` — a relation read/write/target. Catalog reads are
    /// NOT exempt here.
    RangeVar,
    /// A concrete object/schema target: `newschema`, `CreateSchemaStmt`
    /// `schemaname`, COMMENT/RENAME `object`, DROP `objects`, `ObjectWithArgs`
    /// `objname`, sequence `OWNED BY` / identity `SEQUENCE NAME`.
    Object,
    /// A creation/alter target carried in a `type_name` qualified-name *list*
    /// (`CreateEnumStmt`/`CreateRangeStmt`/`AlterEnumStmt`/`AlterTypeStmt`):
    /// planting/altering a type INTO a schema. Like a function-definition
    /// target, NO shared-schema exemption — `public.e`/`pg_catalog.e`/
    /// `control.e` are all denied. (Distinct from [`SchemaSlot::TypeRef`],
    /// which is a `TypeName.names` *reference* where builtins are exempt.)
    CreationTarget,
    /// A built-in-bearing object sub-slot: index `opclass` / `COLLATE`
    /// `collname`. These routinely name a `pg_catalog` builtin
    /// (`pg_catalog.text_ops`, `pg_catalog."C"`), so `pg_catalog` ONLY is
    /// exempt; a foreign tenant/platform opclass or collation is denied.
    BuiltinObject,
    /// A type reference (`TypeName.names`): column/return/param/cast/`OF` type.
    TypeRef,
    /// A function-name *call* qualifier (`FuncCall`/trigger/CALL `funcname`).
    /// Function *definition* targets are NOT this slot — they are checked
    /// against the project schema directly in [`SqlGuard::check_func_def_target`]
    /// (no shared-schema exemption).
    FuncCall,
}

/// The traversal behind [`foreign_schema_in_tree`]. Invokes `visit(schema,
/// slot)` for every candidate schema string; returns `true` once `visit`
/// short-circuits.
///
/// Typed by node shape, not a key-string allowlist: each schema-qualified-name
/// node contributes its schema with the slot that fixes its exemption policy.
fn walk_schema_names(v: &Value, visit: &mut dyn FnMut(&str, SchemaSlot) -> bool) -> bool {
    match v {
        Value::Object(map) => {
            // A `RangeVar` (relation target) carries its schema in
            // `schemaname` AND a `relname` sibling — the relation slot.
            if map.contains_key("relname") {
                if let Some(Value::String(s)) = map.get("schemaname") {
                    if !s.is_empty() && visit(s, SchemaSlot::RangeVar) {
                        return true;
                    }
                }
            } else if let Some(Value::String(s)) = map.get("schemaname") {
                // `CreateSchemaStmt.schemaname` (no `relname` sibling) — a
                // concrete schema target.
                if !s.is_empty() && visit(s, SchemaSlot::Object) {
                    return true;
                }
            }
            // `AlterObjectSchemaStmt.newschema` (`… SET SCHEMA control`).
            if let Some(Value::String(s)) = map.get("newschema") {
                if !s.is_empty() && visit(s, SchemaSlot::Object) {
                    return true;
                }
            }
            // `TypeName.names` — column/return/param/cast/OF type reference.
            // The presence of the `names` key alongside type-name siblings is
            // the TypeName tell.
            if let Some(schema) = qualified_list_schema(map.get("names")) {
                if visit(&schema, SchemaSlot::TypeRef) {
                    return true;
                }
            }
            // `type_name` as a qualified-name *list* (NOT a nested `TypeName`
            // object): the creation/alter target of CreateEnumStmt /
            // CreateRangeStmt / AlterEnumStmt / AlterTypeStmt. (`CreateStmt`/
            // `ColumnDef` carry `type_name` as a nested `TypeName` *object*
            // whose schema lives under `names`, handled above — a non-array
            // `type_name` yields no parts here, so this is target-only.)
            if let Some(schema) = qualified_list_schema(map.get("type_name")) {
                if visit(&schema, SchemaSlot::CreationTarget) {
                    return true;
                }
            }
            // Qualified function-name *call* lists: trigger/CALL/FuncCall.
            if let Some(schema) = qualified_list_schema(map.get("funcname")) {
                if visit(&schema, SchemaSlot::FuncCall) {
                    return true;
                }
            }
            // COMMENT/RENAME/DEPENDS `object` (singular) + `ObjectWithArgs`
            // `objname` — a concrete object target ([schema, object]).
            for key in ["object", "objname"] {
                if let Some(schema) = qualified_list_schema(map.get(key)) {
                    if visit(&schema, SchemaSlot::Object) {
                        return true;
                    }
                }
            }
            // DROP/GRANT `objects` (PLURAL): a list whose *items* are each a
            // qualified-name node (`List`/`TypeName`/`ObjectWithArgs`). Walk
            // each item's own qualifier — flattening the outer array would
            // mis-read it. (`DROP TYPE control.t` carries a `TypeName` item,
            // already covered by the `names` walk above; tables/indexes/
            // views/sequences/triggers/functions carry a `List`/`ObjectWithArgs`
            // the singular `object`/`names` keys never reach.)
            if let Some(Value::Array(items)) = map.get("objects") {
                for item in items {
                    if let Some(schema) = qualified_list_schema(Some(item)) {
                        if visit(&schema, SchemaSlot::Object) {
                            return true;
                        }
                    }
                }
            }
            // Built-in-bearing sub-slots: index `opclass` ([schema, opclass]),
            // COLLATE `collname` ([schema, collation]). `pg_catalog` builtins
            // are exempt here (and only here) via SchemaSlot::BuiltinObject.
            for key in ["opclass", "collname"] {
                if let Some(schema) = qualified_list_schema(map.get(key)) {
                    if visit(&schema, SchemaSlot::BuiltinObject) {
                        return true;
                    }
                }
            }
            // DefElem object slots:
            //   - `owned_by`: `OWNED BY <schema>.<table>.<column>` — 2-part
            //     (`table.col`, no schema) or 3-part (`schema.table.col`);
            //     schema present only at 3 parts.
            //   - `sequence_name`: identity `… (SEQUENCE NAME <schema>.s)` —
            //     a `List[schema, name]` (schema present at 2+ parts).
            match map.get("defname").and_then(Value::as_str) {
                Some("owned_by") => {
                    if let Some(schema) = owned_by_schema(map.get("arg")) {
                        if visit(&schema, SchemaSlot::Object) {
                            return true;
                        }
                    }
                }
                Some("sequence_name") => {
                    if let Some(schema) = qualified_list_schema(map.get("arg")) {
                        if visit(&schema, SchemaSlot::Object) {
                            return true;
                        }
                    }
                }
                _ => {}
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
///   - a bare array (`CreateTrigStmt.funcname`, `CallStmt…funcname`,
///     `TypeName.names`, `IndexElem.opclass`, `CollateClause.collname`):
///     `[{node:{String}}, …]`
///   - a `List` node (`CommentStmt.object`): `{node:{List:{items:[…]}}}`
fn qualified_list_schema(v: Option<&Value>) -> Option<String> {
    let parts = qualified_list_parts(v)?;
    if parts.len() >= 2 {
        Some(parts[0].clone())
    } else {
        None
    }
}

/// The schema of an `OWNED BY` target. The list is `[table, col]` (no schema,
/// 2 parts) or `[schema, table, col]` (3 parts) — so a schema is present only
/// at 3+ parts, and is `parts[0]`.
fn owned_by_schema(v: Option<&Value>) -> Option<String> {
    let parts = qualified_list_parts(v)?;
    if parts.len() >= 3 {
        Some(parts[0].clone())
    } else {
        None
    }
}

/// Flatten a qualified-name list (bare array or `List` node) to its String
/// parts.
fn qualified_list_parts(v: Option<&Value>) -> Option<Vec<String>> {
    let arr = match v {
        Some(Value::Array(a)) => a.as_slice(),
        Some(obj) => match obj.get("node").and_then(|n| n.get("List")).and_then(|l| l.get("items"))
        {
            Some(Value::Array(a)) => a.as_slice(),
            _ => return None,
        },
        None => return None,
    };
    Some(arr.iter().filter_map(json_string_node).collect())
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
