//! The VENDOR (`@zeroship/migrate`) Postgres render seam (vendor spec §4.4).
//!
//! Renders the privileged vendor [`Op`](crate::model::ir::Op) variants to **structured**
//! Postgres DDL: identifiers double-quoted via the crate's single quoting seam
//! ([`quote_ident_checked`]), policy/trigger predicates rendered from the CLOSED
//! [`Expr`](crate::model::expr::Expr) AST via the existing inline renderer
//! ([`render_predicate_pg`]) — **never string concatenation**. The function `body`
//! and the `pgRaw` escape are the two raw-string fields: they are embedded
//! VERBATIM and the WHOLE rendered statement is then `pg_query`-parsed by the
//! guard at the lower seam (so the body is scanned, vendor spec §3.3).
//!
//! Every vendor op is `dialect_scope = PgOnly`: this module only renders Postgres,
//! and the lower seam ([`crate::render::lower`]) hard-rejects a SQLite target before
//! reaching here (vendor spec §4.3). The render is pure (no DB, no live schema).
//!
//! # NOT in this module
//!
//! The capability GATE (vendor spec §3.2) lives in [`crate::model::validate`]
//! (`VENDOR_OP_DENIED` at load) + the rendered-SQL deny-list (the guard at lower).
//! This module is render-only; it assumes the op already passed both gates.

use crate::render::dml::{quote_ident_checked, render_predicate_pg, DmlError, IdentQuoteError};
use crate::model::ir::{is_valid_pg_type_ref, GrantTarget, Op, Privilege, TriggerAction};

/// A single rendered vendor statement: a name (for the journaled `Migration`), the
/// forward SQL (no trailing `;`), and the reverse SQL (or `None` for an
/// irreversible op). A vendor op renders to ONE OR MORE of these (e.g. a
/// `createRole` with `setSearchPath` renders a `CREATE ROLE` + a follow-on
/// `ALTER ROLE … SET search_path` statement).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VendorStatement {
    /// A stable, human-readable name for the journaled migration unit.
    pub name: String,
    /// The forward SQL (no trailing `;`).
    pub up: String,
    /// The reverse SQL, or `None` when there is no structural inverse.
    pub down: Option<String>,
}

/// A failure rendering a vendor op to Postgres DDL.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum VendorError {
    /// An identifier could not be quoted (empty / NUL).
    #[error("vendor render: identifier not quotable: {0}")]
    Ident(#[from] IdentQuoteError),
    /// A policy `USING`/`WITH CHECK` or trigger `WHEN` predicate could not be
    /// rendered from its closed AST.
    #[error("vendor render: predicate not renderable: {0}")]
    Predicate(String),
    /// A required list was empty (no privileges, no roles, no events, …).
    #[error("vendor render: {what} must be non-empty")]
    EmptyList {
        /// What was empty.
        what: &'static str,
    },
    /// A trigger action is not renderable on Postgres.
    #[error("vendor render: trigger action {kind} is unsupported on Postgres")]
    UnsupportedTriggerAction {
        /// Stable unsupported-kind token.
        kind: &'static str,
    },
    /// `CREATE ROLE IF NOT EXISTS` is synthesized with an opaque PL/pgSQL DO
    /// wrapper; SUPERUSER must never be hidden inside that body.
    #[error("vendor render: createRole cannot combine superuser:true with ifNotExists:true")]
    SuperuserIfNotExistsUnsupported,
    /// A function signature type was not in the conservative type-reference
    /// grammar.
    #[error("vendor render: unsafe function type reference in {slot}: {value:?}")]
    InvalidTypeRef {
        /// The field being rendered.
        slot: &'static str,
        /// The rejected value.
        value: String,
    },
    /// A reserved pseudo-role (e.g. `PUBLIC`) was used where a concrete role is
    /// required. `DROP OWNED BY PUBLIC` errors at apply, so reject it at render.
    #[error("vendor render: {what} cannot target the reserved pseudo-role {role:?}")]
    ReservedRole {
        /// What was being rendered.
        what: &'static str,
        /// The rejected pseudo-role.
        role: String,
    },
    /// Two mutually-exclusive instructions were supplied in one op (e.g. an
    /// `alterRole` carrying both `setSearchPath` and `resetSearchPath:true`).
    #[error("vendor render: {what}")]
    ContradictoryArgs {
        /// A human description of the contradiction.
        what: &'static str,
    },
}

/// Quote an identifier through the crate's single seam, mapping the error.
fn qid(ident: &str) -> Result<String, VendorError> {
    Ok(quote_ident_checked(ident)?)
}

/// `"schema"."name"` — both parts quoted.
fn qualified(schema: &str, name: &str) -> Result<String, VendorError> {
    Ok(format!("{}.{}", qid(schema)?, qid(name)?))
}

/// A role/grantee name. The reserved `"public"` sentinel renders the unquoted
/// `PUBLIC` keyword (PG's pseudo-role); every other name is quoted.
fn role_ref(name: &str) -> Result<String, VendorError> {
    if name.eq_ignore_ascii_case("public") {
        Ok("PUBLIC".to_string())
    } else {
        qid(name)
    }
}

/// Pick a dollar-quote tag that does NOT occur in `body`, so a function body
/// containing the literal `$zsfn$` cannot prematurely terminate the dollar-quoted
/// literal (SA-14). Returns the canonical `$zsfn$` when the body is collision-free
/// (so existing renders / goldens are byte-stable), else `$zsfn0$`, `$zsfn1$`, …
fn function_body_dollar_tag(body: &str) -> String {
    let mut tag = String::from("$zsfn$");
    let mut i = 0u64;
    while body.contains(&tag) {
        tag = format!("$zsfn{i}$");
        i += 1;
    }
    tag
}

/// Single-quote-escape a SQL string literal (`'` ⇒ `''`).
fn sql_str(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// Render a `search_path` list as the `SET search_path = a, b` clause body (each
/// element quoted as an identifier).
fn search_path_list(items: &[String]) -> Result<String, VendorError> {
    let parts: Result<Vec<_>, _> = items.iter().map(|s| qid(s)).collect();
    Ok(parts?.join(", "))
}

/// Render a GRANT/REVOKE target to its `ON …` SQL clause + a default schema for
/// table targets.
fn grant_target_sql(target: &GrantTarget, default_schema: &str) -> Result<String, VendorError> {
    Ok(match target {
        GrantTarget::Table { names, schema } => {
            if names.is_empty() {
                return Err(VendorError::EmptyList { what: "grant table names" });
            }
            let sch = schema.as_deref().unwrap_or(default_schema);
            let parts: Result<Vec<_>, _> = names.iter().map(|n| qualified(sch, n)).collect();
            parts?.join(", ")
        }
        GrantTarget::Schema { names } => {
            if names.is_empty() {
                return Err(VendorError::EmptyList { what: "grant schema names" });
            }
            let parts: Result<Vec<_>, _> = names.iter().map(|n| qid(n)).collect();
            format!("SCHEMA {}", parts?.join(", "))
        }
        GrantTarget::Sequence { r#in } => {
            format!("ALL SEQUENCES IN SCHEMA {}", qid(r#in)?)
        }
        GrantTarget::Database { names } => {
            if names.is_empty() {
                return Err(VendorError::EmptyList { what: "grant database names" });
            }
            let parts: Result<Vec<_>, _> = names.iter().map(|n| qid(n)).collect();
            format!("DATABASE {}", parts?.join(", "))
        }
    })
}

/// Render the comma-joined privilege list (`All` ⇒ `ALL PRIVILEGES`).
fn privileges_sql(privs: &[Privilege]) -> Result<String, VendorError> {
    if privs.is_empty() {
        return Err(VendorError::EmptyList { what: "privileges" });
    }
    // `All` subsumes everything — render it alone.
    if privs.contains(&Privilege::All) {
        return Ok("ALL PRIVILEGES".to_string());
    }
    Ok(privs.iter().map(|p| p.as_sql()).collect::<Vec<_>>().join(", "))
}

/// Render the comma-joined role list.
fn roles_sql(roles: &[String], what: &'static str) -> Result<String, VendorError> {
    if roles.is_empty() {
        return Err(VendorError::EmptyList { what });
    }
    let parts: Result<Vec<_>, _> = roles.iter().map(|r| role_ref(r)).collect();
    Ok(parts?.join(", "))
}

/// Render a closed-AST predicate, mapping the renderer error.
fn predicate(expr: &crate::model::expr::Expr) -> Result<String, VendorError> {
    render_predicate_pg(expr).map_err(|e: DmlError| VendorError::Predicate(e.to_string()))
}

fn type_ref_sql<'a>(value: &'a str, slot: &'static str) -> Result<&'a str, VendorError> {
    if is_valid_pg_type_ref(value) {
        Ok(value)
    } else {
        Err(VendorError::InvalidTypeRef {
            slot,
            value: value.to_string(),
        })
    }
}

/// Render a VENDOR op to its ordered Postgres statement list (vendor spec §4.4).
/// `eff_schema` is the effective schema the lower seam resolved (the op's own
/// `schema` qualifier → connection default → project schema), used to qualify
/// table-/policy-/trigger-scoped objects.
///
/// # Errors
/// [`VendorError`] on an invalid identifier, an unrenderable predicate, or an
/// empty required list. The caller (`IrAuthor`) is responsible for the SQLite
/// `PgOnly` refusal BEFORE calling this.
#[allow(clippy::too_many_lines)]
pub fn render_vendor_op(op: &Op, eff_schema: &str) -> Result<Vec<VendorStatement>, VendorError> {
    Ok(match op {
        // ── Schemas ──────────────────────────────────────────────────────────
        Op::CreateSchema { name, if_not_exists, authorization } => {
            let mut up = String::from("CREATE SCHEMA ");
            if if_not_exists.unwrap_or(false) {
                up.push_str("IF NOT EXISTS ");
            }
            up.push_str(&qid(name)?);
            if let Some(role) = authorization {
                up.push_str(&format!(" AUTHORIZATION {}", role_ref(role)?));
            }
            vec![VendorStatement {
                name: format!("create_schema_{name}"),
                up,
                down: Some(format!("DROP SCHEMA IF EXISTS {}", qid(name)?)),
            }]
        }
        Op::DropSchema { name, if_exists, cascade } => {
            let mut up = String::from("DROP SCHEMA ");
            if if_exists.unwrap_or(false) {
                up.push_str("IF EXISTS ");
            }
            up.push_str(&qid(name)?);
            if cascade.unwrap_or(false) {
                up.push_str(" CASCADE");
            }
            vec![VendorStatement { name: format!("drop_schema_{name}"), up, down: None }]
        }
        // ── Extensions ───────────────────────────────────────────────────────
        Op::CreateExtension { name, if_not_exists, schema } => {
            let mut up = String::from("CREATE EXTENSION ");
            if if_not_exists.unwrap_or(false) {
                up.push_str("IF NOT EXISTS ");
            }
            up.push_str(&qid(name)?);
            if let Some(sch) = schema {
                up.push_str(&format!(" WITH SCHEMA {}", qid(sch)?));
            }
            vec![VendorStatement {
                name: format!("create_extension_{name}"),
                up,
                down: Some(format!("DROP EXTENSION IF EXISTS {}", qid(name)?)),
            }]
        }
        Op::DropExtension { name, if_exists } => {
            let mut up = String::from("DROP EXTENSION ");
            if if_exists.unwrap_or(false) {
                up.push_str("IF EXISTS ");
            }
            up.push_str(&qid(name)?);
            vec![VendorStatement { name: format!("drop_extension_{name}"), up, down: None }]
        }
        // ── Roles ────────────────────────────────────────────────────────────
        Op::CreateRole {
            name,
            login,
            password,
            bypass_rls,
            create_role,
            create_db,
            superuser,
            in_role,
            set_search_path,
            if_not_exists,
        } => {
            if superuser.unwrap_or(false) && if_not_exists.unwrap_or(false) {
                return Err(VendorError::SuperuserIfNotExistsUnsupported);
            }
            let qname = qid(name)?;
            let mut opts = String::new();
            if login.unwrap_or(false) {
                opts.push_str(" LOGIN");
            }
            if let Some(pw) = password {
                opts.push_str(&format!(" PASSWORD {}", sql_str(pw)));
            }
            if bypass_rls.unwrap_or(false) {
                opts.push_str(" BYPASSRLS");
            }
            if create_role.unwrap_or(false) {
                opts.push_str(" CREATEROLE");
            }
            if create_db.unwrap_or(false) {
                opts.push_str(" CREATEDB");
            }
            // SUPERUSER renders verbatim; the guard deny-list REFUSES it in all
            // profiles (vendor spec §3.4) — render-here, refuse-at-guard.
            if superuser.unwrap_or(false) {
                opts.push_str(" SUPERUSER");
            }
            if let Some(roles) = in_role {
                if !roles.is_empty() {
                    let parts: Result<Vec<_>, _> = roles.iter().map(|r| qid(r)).collect();
                    opts.push_str(&format!(" IN ROLE {}", parts?.join(", ")));
                }
            }
            let create_stmt = format!("CREATE ROLE {qname}{opts}");
            // `ifNotExists` is engine-synthesized via a `pg_roles` probe (there is
            // no native CREATE ROLE IF NOT EXISTS) — vendor spec §2.9. The DO body
            // is re-parsed + deny-scanned by the guard.
            let up_create = if if_not_exists.unwrap_or(false) {
                format!(
                    "DO $$ BEGIN IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = {}) THEN {}; END IF; END $$",
                    sql_str(name),
                    create_stmt,
                )
            } else {
                create_stmt
            };
            let mut out = vec![VendorStatement {
                name: format!("create_role_{name}"),
                up: up_create,
                down: Some(format!("DROP ROLE IF EXISTS {qname}")),
            }];
            // The `ALTER ROLE … SET search_path` the platform needs (×17, 0025).
            if let Some(sp) = set_search_path {
                if !sp.is_empty() {
                    out.push(VendorStatement {
                        name: format!("role_search_path_{name}"),
                        up: format!(
                            "ALTER ROLE {qname} SET search_path = {}",
                            search_path_list(sp)?
                        ),
                        down: Some(format!("ALTER ROLE {qname} RESET search_path")),
                    });
                }
            }
            out
        }
        Op::AlterRole { name, set_search_path, reset_search_path } => {
            let qname = qid(name)?;
            // SA-16: `setSearchPath` + `resetSearchPath:true` is contradictory (the
            // renderer would silently prefer RESET and drop the SET). Refuse it so
            // the author resolves the intent. NOTE the createRole-vs-alterRole
            // asymmetry below: createRole's search_path push carries a RESET `down`
            // (its inverse is well-defined), whereas an alterRole is a one-way edit
            // over a possibly-already-customized role, so it journals `down: None`.
            if reset_search_path.unwrap_or(false)
                && set_search_path.as_ref().is_some_and(|s| !s.is_empty())
            {
                return Err(VendorError::ContradictoryArgs {
                    what: "alterRole cannot combine setSearchPath with resetSearchPath:true",
                });
            }
            // An `alterRole` carrying NEITHER a `setSearchPath` nor an explicit
            // `resetSearchPath` has no instruction. Fail CLOSED (vendor review
            // LOW-5): an instruction-less alter must NOT silently fabricate a
            // destructive `RESET search_path` (which would wipe a role's pinned
            // search_path the author never asked to touch).
            let up = if reset_search_path.unwrap_or(false) {
                format!("ALTER ROLE {qname} RESET search_path")
            } else if let Some(sp) = set_search_path.as_ref().filter(|s| !s.is_empty()) {
                format!("ALTER ROLE {qname} SET search_path = {}", search_path_list(sp)?)
            } else {
                return Err(VendorError::EmptyList { what: "alterRole instruction (setSearchPath or resetSearchPath)" });
            };
            vec![VendorStatement { name: format!("alter_role_{name}"), up, down: None }]
        }
        Op::DropRole { name, if_exists } => {
            let mut up = String::from("DROP ROLE ");
            if if_exists.unwrap_or(false) {
                up.push_str("IF EXISTS ");
            }
            up.push_str(&qid(name)?);
            vec![VendorStatement { name: format!("drop_role_{name}"), up, down: None }]
        }
        Op::DropOwnedBy { roles } => {
            // Validate BEFORE the map+join (SA-15): an empty list and the reserved
            // `PUBLIC` pseudo-role (`DROP OWNED BY PUBLIC` errors at apply) are both
            // refused fail-closed rather than rendered into an apply-time failure.
            if roles.is_empty() {
                return Err(VendorError::EmptyList { what: "drop owned by roles" });
            }
            if let Some(r) = roles.iter().find(|r| r.eq_ignore_ascii_case("public")) {
                return Err(VendorError::ReservedRole { what: "drop owned by roles", role: r.clone() });
            }
            let parts: Result<Vec<_>, _> = roles.iter().map(|r| role_ref(r)).collect();
            let roles_sql = parts?.join(", ");
            vec![VendorStatement {
                name: "drop_owned_by".to_string(),
                up: format!("DROP OWNED BY {roles_sql}"),
                down: None,
            }]
        }
        // ── Grants ───────────────────────────────────────────────────────────
        Op::Grant { privileges, on, to, with_grant_option } => {
            let privs = privileges_sql(privileges)?;
            let target = grant_target_sql(on, eff_schema)?;
            let grantees = roles_sql(to, "grant to roles")?;
            let mut up = format!("GRANT {privs} ON {target} TO {grantees}");
            if with_grant_option.unwrap_or(false) {
                up.push_str(" WITH GRANT OPTION");
            }
            let down = format!("REVOKE {privs} ON {target} FROM {grantees}");
            vec![VendorStatement { name: "grant".to_string(), up, down: Some(down) }]
        }
        Op::Revoke { privileges, on, from } => {
            let privs = privileges_sql(privileges)?;
            let target = grant_target_sql(on, eff_schema)?;
            let grantees = roles_sql(from, "revoke from roles")?;
            vec![VendorStatement {
                name: "revoke".to_string(),
                up: format!("REVOKE {privs} ON {target} FROM {grantees}"),
                down: None,
            }]
        }
        // ── Row-Level Security ────────────────────────────────────────────────
        Op::SetRls { table, enabled, forced, .. } => {
            set_rls_stmts(eff_schema, table, *enabled, *forced)?
        }
        // ── Policies ─────────────────────────────────────────────────────────
        Op::CreatePolicy { name, table, for_cmd, to, using, with_check, .. } => {
            let qtable = qualified(eff_schema, table)?;
            let qname = qid(name)?;
            let mut up = format!(
                "CREATE POLICY {qname} ON {qtable} FOR {}",
                for_cmd.as_sql()
            );
            if let Some(roles) = to {
                if !roles.is_empty() {
                    up.push_str(&format!(" TO {}", roles_sql(roles, "policy roles")?));
                }
            }
            up.push_str(&format!(" USING ({})", predicate(using)?));
            if let Some(wc) = with_check {
                up.push_str(&format!(" WITH CHECK ({})", predicate(wc)?));
            }
            vec![VendorStatement {
                name: format!("create_policy_{name}_{table}"),
                up,
                down: Some(format!("DROP POLICY IF EXISTS {qname} ON {qtable}")),
            }]
        }
        Op::DropPolicy { name, table, if_exists, .. } => {
            let qtable = qualified(eff_schema, table)?;
            let mut up = String::from("DROP POLICY ");
            if if_exists.unwrap_or(false) {
                up.push_str("IF EXISTS ");
            }
            up.push_str(&format!("{} ON {qtable}", qid(name)?));
            vec![VendorStatement { name: format!("drop_policy_{name}_{table}"), up, down: None }]
        }
        // ── Triggers ─────────────────────────────────────────────────────────
        Op::CreateTrigger { name, table, timing, events, for_each, action, when, .. } => {
            if events.is_empty() {
                return Err(VendorError::EmptyList { what: "trigger events" });
            }
            let qtable = qualified(eff_schema, table)?;
            let qname = qid(name)?;
            let events_sql =
                events.iter().map(|e| e.as_sql()).collect::<Vec<_>>().join(" OR ");
            let mut up = format!(
                "CREATE TRIGGER {qname} {} {events_sql} ON {qtable} FOR EACH {}",
                timing.as_sql(),
                for_each.as_sql(),
            );
            if let Some(w) = when {
                up.push_str(&format!(" WHEN ({})", predicate(w)?));
            }
            match action {
                TriggerAction::ExecuteFunction { name } => {
                    // The function name is quoted and schema-qualified to the
                    // effective schema (the platform functions live there).
                    up.push_str(&format!(" EXECUTE FUNCTION {}()", qualified(eff_schema, name)?));
                }
                TriggerAction::Body { .. } => {
                    return Err(VendorError::UnsupportedTriggerAction { kind: "triggerBody" });
                }
            }
            vec![VendorStatement {
                name: format!("create_trigger_{name}_{table}"),
                up,
                down: Some(format!("DROP TRIGGER IF EXISTS {qname} ON {qtable}")),
            }]
        }
        Op::DropTrigger { name, table, if_exists, .. } => {
            let qtable = qualified(eff_schema, table)?;
            let mut up = String::from("DROP TRIGGER ");
            if if_exists.unwrap_or(false) {
                up.push_str("IF EXISTS ");
            }
            up.push_str(&format!("{} ON {qtable}", qid(name)?));
            vec![VendorStatement { name: format!("drop_trigger_{name}_{table}"), up, down: None }]
        }
        // ── Functions ────────────────────────────────────────────────────────
        Op::CreateFunction { name, schema, args, returns, language, replace, volatility, body } => {
            let qname = match schema {
                Some(sch) => qualified(sch, name)?,
                None => qid(name)?,
            };
            let args_sql = match args {
                Some(list) => list
                    .iter()
                    .map(|a| {
                        let mode = a.mode.map(|m| format!("{} ", m.as_sql())).unwrap_or_default();
                        let nm = a
                            .name
                            .as_ref()
                            .map(|n| qid(n).map(|n| format!("{n} ")))
                            .transpose()?
                            .unwrap_or_default();
                        let ty = type_ref_sql(&a.ty, "createFunction.args[].type")?;
                        Ok(format!("{mode}{nm}{ty}"))
                    })
                    .collect::<Result<Vec<_>, VendorError>>()?
                    .join(", "),
                None => String::new(),
            };
            let or_replace = if replace.unwrap_or(false) { "CREATE OR REPLACE" } else { "CREATE" };
            let vol = volatility.map(|v| format!(" {}", v.as_sql())).unwrap_or_default();
            let returns = type_ref_sql(returns, "createFunction.returns")?;
            // The raw `body` is embedded VERBATIM inside a `$zsfn$ … $zsfn$` dollar
            // tag; the WHOLE statement is `pg_query`-parsed + deny-scanned by the
            // guard at the lower seam (vendor spec §3.3). The tag is chosen to not
            // collide with the body (SA-14) — a body literally containing `$zsfn$`
            // would otherwise terminate the quote early.
            let tag = function_body_dollar_tag(body);
            let up = format!(
                "{or_replace} FUNCTION {qname}({args_sql}) RETURNS {returns} LANGUAGE {}{vol} AS {tag}\n{body}\n{tag}",
                language.as_sql(),
            );
            let down = format!("DROP FUNCTION IF EXISTS {qname}({args_sql})");
            vec![VendorStatement {
                name: format!("create_function_{name}"),
                up,
                down: Some(down),
            }]
        }
        Op::DropFunction { name, schema, arg_types, if_exists } => {
            let qname = match schema {
                Some(sch) => qualified(sch, name)?,
                None => qid(name)?,
            };
            let args_sql = arg_types
                .as_ref()
                .map(|types| {
                    types
                        .iter()
                        .map(|ty| type_ref_sql(ty, "dropFunction.argTypes[]").map(str::to_string))
                        .collect::<Result<Vec<_>, VendorError>>()
                        .map(|types| types.join(", "))
                })
                .transpose()?
                .unwrap_or_default();
            let mut up = String::from("DROP FUNCTION ");
            if if_exists.unwrap_or(false) {
                up.push_str("IF EXISTS ");
            }
            up.push_str(&format!("{qname}({args_sql})"));
            vec![VendorStatement { name: format!("drop_function_{name}"), up, down: None }]
        }
        // ── The gated raw escape ──────────────────────────────────────────────
        Op::PgRaw { sql, .. } => {
            // The verbatim SQL is embedded as-is and the WHOLE statement is
            // `pg_query`-parsed + deny-scanned by the guard.
            vec![VendorStatement { name: "pg_raw".to_string(), up: sql.clone(), down: None }]
        }
        // Non-vendor ops never reach here (the lower seam routes only vendor ops).
        _ => return Err(VendorError::Predicate("non-vendor op routed to vendor renderer".to_string())),
    })
}

fn set_rls_stmts(
    eff_schema: &str,
    table: &str,
    enabled: Option<bool>,
    forced: Option<bool>,
) -> Result<Vec<VendorStatement>, VendorError> {
    let mut stmts = Vec::new();
    if let Some(enabled) = enabled {
        let (verb, inverse) = if enabled {
            ("ENABLE", "DISABLE")
        } else {
            ("DISABLE", "ENABLE")
        };
        stmts.extend(rls_stmt(eff_schema, table, verb, inverse)?);
    }
    if let Some(forced) = forced {
        let (verb, inverse) = if forced {
            ("FORCE", "NO FORCE")
        } else {
            ("NO FORCE", "FORCE")
        };
        stmts.extend(rls_stmt(eff_schema, table, verb, inverse)?);
    }
    Ok(stmts)
}

/// Render an `ALTER TABLE … <verb> ROW LEVEL SECURITY` RLS statement + its inverse.
fn rls_stmt(
    eff_schema: &str,
    table: &str,
    verb: &str,
    inverse: &str,
) -> Result<Vec<VendorStatement>, VendorError> {
    let qtable = qualified(eff_schema, table)?;
    let tag = verb.to_ascii_lowercase().replace(' ', "_");
    Ok(vec![VendorStatement {
        name: format!("rls_{tag}_{table}"),
        up: format!("ALTER TABLE {qtable} {verb} ROW LEVEL SECURITY"),
        down: Some(format!("ALTER TABLE {qtable} {inverse} ROW LEVEL SECURITY")),
    }])
}
