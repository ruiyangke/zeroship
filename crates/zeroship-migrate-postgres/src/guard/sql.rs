//! The SQL security guard - parse-time deny-list + cross-schema confinement.
//! **The security heart of the engine.**
//!
//! Migrations are privileged arbitrary-SQL authored by untrusted creators AND a
//! prompt-injectable AI. This guard is the *first* line of defense-in-depth: it
//! parses every statement with the real Postgres parser and rejects the
//! dangerous set (RCE / privilege-escalation / cross-tenant / file / SSRF)
//! *regardless of the submitted SQL*. The least-privilege `migrator` role
//! (built later) is the second line - the DB rejects the same ops even if SQL
//! slips past parse.
//!
//! Two postures, by threat class:
//! - **Deny** (hard error): RCE, privilege escalation, cross-tenant access,
//!   filesystem/network reach. These can never be auto-confirmed.
//! - **Flag** (`GuardReport.destructive`): data loss (`DROP`/`TRUNCATE`/lossy
//!   type change). The guard does not deny these - the gate (built later)
//!   decides on data loss. The guard only surfaces them.
//!
//! **Deny-by-default:** an unrecognized statement that *could* be dangerous is
//! denied, not waved through.

use std::collections::BTreeSet;

use pg_query::protobuf::node::Node as NodeEnum;
use pg_query::protobuf::{self, ConstrType, ObjectType};

use pg_query::protobuf::AlterTableType;

use crate::analysis::analyze::Advisory;
use crate::analysis::classify::{classify, DataSecurityClass, DdlKind, ParseError, StatementClass};
use crate::guard::denylist::rule;
use serde_json::Value;
use zeroship_migrate_ir::migration::MigrationFlags;
use zeroship_migrate_ir::policy::DestructiveOps;
use zeroship_migrate_ir::policy::SchemaScope;
use zeroship_migrate_ir::policy_registry;
use zeroship_migrate_policy::{normalize_object_name, GrantRegion, ObjectName, ShapeElement};

// The NEUTRAL guard seam lives in the backend contract crate, below every vendor, so
// a vendor crate can implement `MigrationGuard` without depending on the engine that
// composes it. What is left HERE is everything that needs a PostgreSQL parser:
// `SqlGuard`, the deny-walk, the classifier, the analyzers.
//
// `check_ir_data_security_policy` lives in the backend contract and asks the vendor
// that owns the raw door, through `MigrationGuard::raw_island_escapes_rls_net_state`.
// The PostgreSQL answer is `SqlGuard::raw_island_within_require_rls` below.
//
// These are IMPORTED, not re-exported. This module's signatures name them, but a
// caller wanting the neutral vocabulary must reach `zeroship_migrate_backend::guard` for
// it. A re-export would have a VENDOR crate handing out the neutral seam under its
// own name, which is the confusion this arrangement exists to prevent.
use zeroship_migrate_backend::guard::{
    data_security_rule, DeclaredCreateShape, GuardConfig, GuardError, InjectedCreateShape,
};

use crate::DIALECT;

pub use zeroship_migrate_backend::guard::namespace_rule;

/// Whether the effective policy admits a DROP object class beyond
/// [`is_safe_drop_object`] (the `.down.sql`-only reverses: schema/extension/policy -
/// DROP ROLE is handled by its own arm). Every arm is a PDP question about the
/// composed policy; none of it is a hardcoded platform-posture allowance.
///
/// `object` is the concrete target the statement names, resolved by
/// [`drop_object_targets`]: the schema for `DROP SCHEMA`, the policy's table for
/// `DROP POLICY`, the extension name for `DROP EXTENSION`. All three knobs address
/// something narrower than the whole database, so a charter that grants them on one
/// schema/table/extension must not decide a drop of another.
///
/// # Why this is a free function here rather than a `GuardConfig` method
///
/// `remove_type` is a raw `libpg_query` `ObjectType` discriminant, and the body
/// decodes it. [`GuardConfig`] moved to `zeroship-migrate-backend`, which sits below every
/// vendor and carries no SQL parser - so keeping this as a method would have dragged
/// `pg_query` down there with it, and from there under `zeroship-migrate-sqlite` and
/// `zeroship-migrate-mysql`, which build without it today. Translating one vendor's parse
/// enum was never the neutral config's job anyway; it is the PostgreSQL guard's. The
/// three policy questions it asks are unchanged.
fn grants_drop_object(cfg: &GuardConfig, remove_type: i32, object: Option<&ObjectName>) -> bool {
    if remove_type == ObjectType::ObjectSchema as i32 {
        return cfg.grants_object_bool(policy_registry::KEY_SCHEMA_CREATE_SCHEMA, object);
    }
    if remove_type == ObjectType::ObjectExtension as i32 {
        return grants_extension_drop(cfg, object);
    }
    if remove_type == ObjectType::ObjectPolicy as i32 {
        return cfg.grants_object_bool(policy_registry::KEY_ACCESS_POLICY, object);
    }
    if remove_type == ObjectType::ObjectRole as i32 {
        return cfg.grants_global_bool(policy_registry::KEY_ACCESS_ROLE);
    }
    false
}

/// Whether the effective policy admits `DROP EXTENSION` of the extension `object`
/// names.
///
/// This asks exactly what the `CreateExtensionStmt` arm of [`SqlGuard::check`] asks,
/// in the same order: the hard deny first, then membership in the `code.extension`
/// allowlist. `code.extension` is the one capability knob whose value is a SET OF
/// NAMES, so "holds the extension capability" and "may name THIS extension" are two
/// different questions. The drop side must ask the SECOND one, "may name THIS
/// extension", never merely whether the allowlist is empty: the coarser question
/// would hand a charter authority over every extension in the database the moment
/// it was granted one.
///
/// # An unresolvable name refuses, it does not fall back
///
/// `object` is `None` when [`drop_object_targets`] cannot resolve the statement to
/// one extension name: a parse node that is not a bare identifier, an empty name, or
/// a name that does not fold to a single segment. The answer there is no, with no
/// fallback to a coarser question. `code.extension` has no whole-universe spelling -
/// its value is an enumerated set of names, never a glob - so unlike the Bool knobs
/// beside it there is no `Top` grant an unnamed target could still be provably inside.
/// Refusing is the only sound answer, and it is the one that stays sound if the
/// grammar ever grows a spelling this resolver has not seen.
///
/// A `DROP` naming SEVERAL extensions is not that case. The caller resolves each name
/// and requires every one of them to be granted, the same way `DROP SCHEMA a, b`
/// already works, so `DROP EXTENSION granted, other` is refused because of `other` and
/// not because it named two.
fn grants_extension_drop(cfg: &GuardConfig, object: Option<&ObjectName>) -> bool {
    let Some(name) = object.and_then(unqualified_name) else {
        return false;
    };
    // FORBIDDEN_EXTENSIONS is a non-grant HARD DENY, and it is not a create-side
    // rule. A name the charter has no authority to install is a name it has no
    // authority to name in DDL at all: were the drop side to skip this, listing a
    // forbidden name in the allowlist would buy back on one side the authority the
    // other side refuses, which is precisely what "not grantable" rules out.
    if crate::guard::denylist::list_contains_ci(crate::guard::denylist::FORBIDDEN_EXTENSIONS, &name)
    {
        return false;
    }
    cfg.granted_extension_allowlist()
        .iter()
        .any(|allowed| allowed.eq_ignore_ascii_case(&name))
}

/// The single-segment name an [`ObjectName`] carries, or `None` when it carries a
/// qualified `schema.table` name or bytes that are not UTF-8.
///
/// [`ObjectName`] is the resolver's only arity-carrying shape, and its one-segment
/// form is what a database-global name with no qualifier - an extension - resolves
/// to. A two-segment result means the resolver read the text as `schema.table`, which
/// no extension name is, so it answers `None` and its caller refuses.
fn unqualified_name(object: &ObjectName) -> Option<String> {
    if object.table.is_some() {
        return None;
    }
    String::from_utf8(object.schema.clone()).ok()
}

/// The concrete object a raw `RangeVar` names, or `None` when the parse names no
/// relation the guard can attribute to one object. Same attribution rule as
/// [`raw_relation_target`]; the two failure arms collapse to `None` because an
/// object-scoped grant treats "cannot name the target" the same way whichever way the
/// name failed.
fn optional_relation_target<D: GuardDecisions + ?Sized>(
    cfg: &D,
    rel: Option<&protobuf::RangeVar>,
) -> Option<ObjectName> {
    let (schemaname, relname) = match rel {
        Some(rel) => (rel.schemaname.as_str(), rel.relname.as_str()),
        None => ("", ""),
    };
    named_relation_target(cfg, schemaname, relname)
}

/// [`raw_relation_target`] with both failure arms collapsed to `None`.
fn named_relation_target<D: GuardDecisions + ?Sized>(
    cfg: &D,
    schemaname: &str,
    relname: &str,
) -> Option<ObjectName> {
    match raw_relation_target(cfg, schemaname, relname) {
        RawRelationTarget::Resolved(object) => Some(object),
        RawRelationTarget::UnpinnedSchema | RawRelationTarget::Unattributable => None,
    }
}

/// The concrete objects a `DROP` names, for the name-scoped members of the extra
/// drop set: the schema of each `DROP SCHEMA` name, the EXTENSION each `DROP
/// EXTENSION` names, and the TABLE each `DROP POLICY` names (that knob is `PerTable`,
/// so a policy is decided at the table it protects).
///
/// A schema and an extension are both bare single-part `String` nodes in `objects`
/// and both resolve through the same fold, which is the point: `code.extension`
/// matches an authored allowlist entry against a name the statement spells, and an
/// unquoted identifier downcases on the way in, so `DROP EXTENSION PostGIS` and
/// `CREATE EXTENSION PostGIS` reach the same entry. Where this fold is STRICTER than
/// the create side's plain downcase - a name carrying an unquoted dot resolves as
/// `schema.table` and one carrying a stray quote resolves as nothing - the difference
/// only ever refuses, and no real extension name is spelled either way.
///
/// Every other remove type is decided by a Global knob that ignores the object, so
/// they answer with a single `None`. The result is never empty: an empty list would
/// make the caller's "every target is granted" vacuously true.
fn drop_object_targets<D: GuardDecisions + ?Sized>(
    cfg: &D,
    drop: &protobuf::DropStmt,
) -> Vec<Option<ObjectName>> {
    let targets: Vec<Option<ObjectName>> = if drop.remove_type == ObjectType::ObjectSchema as i32
        || drop.remove_type == ObjectType::ObjectExtension as i32
    {
        drop.objects
            .iter()
            .map(|item| match item.node.as_ref() {
                Some(NodeEnum::String(s)) => normalize_object_name(s.sval.trim()),
                _ => None,
            })
            .collect()
    } else if drop.remove_type == ObjectType::ObjectPolicy as i32 {
        drop.objects
            .iter()
            .map(|item| policy_relation_target(cfg, item))
            .collect()
    } else {
        Vec::new()
    };
    if targets.is_empty() {
        return vec![None];
    }
    targets
}

/// The table one `DROP POLICY` list entry protects. The grammar appends the policy
/// name to the relation's own name parts, so the relation is everything before the
/// last element: `[policy]`, `[table, policy]` or `[schema, table, policy]`.
fn policy_relation_target<D: GuardDecisions + ?Sized>(
    cfg: &D,
    item: &protobuf::Node,
) -> Option<ObjectName> {
    let Some(NodeEnum::List(list)) = item.node.as_ref() else {
        return None;
    };
    let parts: Vec<&str> = list
        .items
        .iter()
        .filter_map(|part| match part.node.as_ref() {
            Some(NodeEnum::String(s)) => Some(s.sval.as_str()),
            _ => None,
        })
        .collect();
    if parts.len() != list.items.len() {
        return None;
    }
    let (schemaname, relname) = match parts.split_last().map(|(_, rest)| rest)? {
        [relname] => ("", *relname),
        [schemaname, relname] => (*schemaname, *relname),
        _ => return None,
    };
    named_relation_target(cfg, schemaname, relname)
}

/// What a raw statement's `RangeVar` attributes to.
enum RawRelationTarget {
    /// The concrete PG-folded object the relation names.
    Resolved(ObjectName),
    /// Unqualified, and the config owns no unique schema to resolve it against.
    UnpinnedSchema,
    /// No name, or a name no identifier fold accepts.
    Unattributable,
}

/// Attribute one raw `RangeVar` to a concrete object. An unqualified relation
/// resolves to the config's pinned project schema (the search_path is pinned under
/// Confined); with no unique pinned schema the name is unattributable.
///
/// This is the single relation-attribution rule for raw SQL: the namespace gate turns
/// the two failure arms into its own denial codes, and the data-security walk treats
/// both as "not provably outside the obligation".
fn raw_relation_target<D: GuardDecisions + ?Sized>(
    cfg: &D,
    schemaname: &str,
    relname: &str,
) -> RawRelationTarget {
    let relname = relname.trim();
    if relname.is_empty() {
        return RawRelationTarget::Unattributable;
    }
    let schema = if schemaname.trim().is_empty() {
        match cfg.pinned_schema() {
            Some(s) => s,
            None => return RawRelationTarget::UnpinnedSchema,
        }
    } else {
        schemaname.trim().to_string()
    };
    match normalize_object_name(&format!("{schema}.{relname}")) {
        Some(object) => RawRelationTarget::Resolved(object),
        None => RawRelationTarget::Unattributable,
    }
}

/// Read the shape a `CREATE TABLE` declares, or `None` when the parse cannot
/// enumerate it.
///
/// Declared names are taken VERBATIM. `libpg_query` has already applied
/// PostgreSQL's identifier fold when it filled `ColumnDef.colname` and a
/// constraint's key strings, so `Created_At` arrives as `created_at` while
/// `"Created_At"` arrives preserved; re-folding here would erase the very
/// quoted/unquoted distinction the parser resolved and let `"Created_At"` pass for
/// the injected `created_at`. Trimming would likewise merge the distinct columns
/// `"id "` and `id`.
///
/// `None` means "unprovable, deny": a `LIKE` clause, `INHERITS`, `PARTITION OF`, or
/// `OF <type>` draws columns from a relation the statement does not spell out, so
/// the declared list is short by construction; an unrecognized table element is
/// treated the same way rather than silently ignored; and two primary-key
/// declarations are a shape Postgres itself rejects, so there is no single key to
/// compare.
fn declared_create_shape(create: &protobuf::CreateStmt) -> Option<DeclaredCreateShape> {
    if !create.inh_relations.is_empty()
        || create.partbound.is_some()
        || create.of_typename.is_some()
    {
        return None;
    }
    let mut columns = BTreeSet::new();
    let mut declared_keys: Vec<Vec<Vec<u8>>> = Vec::new();
    for elt in &create.table_elts {
        match elt.node.as_ref() {
            Some(NodeEnum::ColumnDef(col)) => {
                let name = col.colname.as_bytes().to_vec();
                if col.constraints.iter().any(is_primary_key_constraint) {
                    declared_keys.push(vec![name.clone()]);
                }
                columns.insert(name);
            }
            Some(NodeEnum::Constraint(con)) => {
                if con.contype == ConstrType::ConstrPrimary as i32 {
                    declared_keys.push(constraint_key_columns(con)?);
                }
            }
            _ => return None,
        }
    }
    let primary_key = match declared_keys.len() {
        0 => None,
        1 => declared_keys.pop(),
        _ => return None,
    };
    Some(DeclaredCreateShape::new(columns, primary_key))
}

/// Is this node a column-level `PRIMARY KEY` marker?
fn is_primary_key_constraint(node: &protobuf::Node) -> bool {
    matches!(
        node.node.as_ref(),
        Some(NodeEnum::Constraint(con)) if con.contype == ConstrType::ConstrPrimary as i32
    )
}

/// The ordered column list of a table-level `PRIMARY KEY (...)`, in the verbatim
/// identifier bytes the parser resolved. `None` when a key entry is not a plain
/// identifier: an unreadable key cannot be compared to a pinned one, so the caller
/// denies.
fn constraint_key_columns(con: &protobuf::Constraint) -> Option<Vec<Vec<u8>>> {
    con.keys
        .iter()
        .map(|key| match key.node.as_ref() {
            Some(NodeEnum::String(s)) => Some(s.sval.as_bytes().to_vec()),
            _ => None,
        })
        .collect()
}

/// The result of a passing [`SqlGuard::check`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuardReport {
    /// The classification of every statement (in order).
    pub classes: Vec<StatementClass>,
    /// True if *any* statement is destructive (data loss). The gate decides.
    pub destructive: bool,
    /// Operational [`Advisory`]s - lock-heavy ops,
    /// destructive/backward-incompatible shapes, missing FK indexes, etc.
    /// **Advisory-only:** these never deny or gate (the deny-list +
    /// least-privilege role own security; the engine gate owns data-loss
    /// approval). They enrich the report so the AI/creator sees the operational
    /// footgun and the safer alternative. See [`crate::analysis::analyze`].
    pub advisories: Vec<Advisory>,
}

trait GuardDecisions {
    fn raw_sql_region(&self) -> GrantRegion;
    fn pinned_schema(&self) -> Option<String>;
    fn grants_namespace_bool(&self, key: &str, object: &ObjectName) -> bool;
    fn injects_cover(&self, object: &ObjectName) -> bool;
    fn covering_inject_shapes(&self, object: &ObjectName) -> Vec<InjectedCreateShape>;
    fn grants_global_bool(&self, key: &str) -> bool;
    fn grants_object_bool(&self, key: &str, object: Option<&ObjectName>) -> bool;
    fn grants_drop_object(&self, remove_type: i32, object: Option<&ObjectName>) -> bool;
    fn granted_extension_allowlist(&self) -> Vec<String>;
    fn permits_schema(&self, schema: &str) -> bool;
    fn is_injected_shape(&self, object: &ObjectName, element: &ShapeElement) -> bool;
}

impl GuardDecisions for GuardConfig {
    fn raw_sql_region(&self) -> GrantRegion {
        Self::raw_sql_region(self)
    }

    fn pinned_schema(&self) -> Option<String> {
        Self::pinned_schema(self)
    }

    fn grants_namespace_bool(&self, key: &str, object: &ObjectName) -> bool {
        Self::grants_namespace_bool(self, key, object)
    }

    fn injects_cover(&self, object: &ObjectName) -> bool {
        Self::injects_cover(self, object)
    }

    fn covering_inject_shapes(&self, object: &ObjectName) -> Vec<InjectedCreateShape> {
        Self::covering_inject_shapes(self, object)
    }

    fn grants_global_bool(&self, key: &str) -> bool {
        Self::grants_global_bool(self, key)
    }

    fn grants_object_bool(&self, key: &str, object: Option<&ObjectName>) -> bool {
        Self::grants_object_bool(self, key, object)
    }

    fn grants_drop_object(&self, remove_type: i32, object: Option<&ObjectName>) -> bool {
        grants_drop_object(self, remove_type, object)
    }

    fn granted_extension_allowlist(&self) -> Vec<String> {
        Self::granted_extension_allowlist(self)
    }

    fn permits_schema(&self, schema: &str) -> bool {
        Self::permits_schema(self, schema)
    }

    fn is_injected_shape(&self, object: &ObjectName, element: &ShapeElement) -> bool {
        Self::is_injected_shape(self, object, element)
    }
}

struct BodyScopeDecisions<'a> {
    scope: Option<&'a SchemaScope>,
}

impl BodyScopeDecisions<'_> {
    /// Defer to [`SchemaScope::permits`] rather than re-deciding admission here:
    /// `Single("")` is what `GuardConfig::schema_scope` produces for a policy that
    /// owns NO schema, i.e. the tightest posture there is, so it must not admit every
    /// cross-tenant reference. `Unconfined` is the variant that means "permit
    /// everything", and it has to be chosen deliberately.
    ///
    /// A `None` scope is the caller declining to confine at all; the body deny-list
    /// still runs.
    fn permits(&self, schema: &str) -> bool {
        self.scope.is_none_or(|scope| scope.permits(schema))
    }
}

impl GuardDecisions for BodyScopeDecisions<'_> {
    fn raw_sql_region(&self) -> GrantRegion {
        GrantRegion::Ungranted
    }

    fn pinned_schema(&self) -> Option<String> {
        match self.scope {
            Some(SchemaScope::Single(schema)) if !schema.is_empty() => Some(schema.clone()),
            Some(SchemaScope::Allowlist(schemas)) if schemas.len() == 1 => schemas.first().cloned(),
            Some(SchemaScope::Policy { project_schema, .. }) if !project_schema.is_empty() => {
                Some(project_schema.clone())
            }
            _ => None,
        }
    }

    fn grants_namespace_bool(&self, key: &str, object: &ObjectName) -> bool {
        if !matches!(
            key,
            policy_registry::KEY_SCHEMA_CROSS_SCHEMA
                | policy_registry::KEY_SCHEMA_CREATE_TABLE
                | policy_registry::KEY_SCHEMA_RENAME
        ) {
            return false;
        }
        std::str::from_utf8(&object.schema).is_ok_and(|schema| self.permits(schema))
    }

    fn injects_cover(&self, _object: &ObjectName) -> bool {
        false
    }

    fn covering_inject_shapes(&self, _object: &ObjectName) -> Vec<InjectedCreateShape> {
        Vec::new()
    }

    fn grants_global_bool(&self, _key: &str) -> bool {
        false
    }

    fn grants_object_bool(&self, _key: &str, _object: Option<&ObjectName>) -> bool {
        false
    }

    fn grants_drop_object(&self, _remove_type: i32, _object: Option<&ObjectName>) -> bool {
        false
    }

    fn granted_extension_allowlist(&self) -> Vec<String> {
        Vec::new()
    }

    fn permits_schema(&self, schema: &str) -> bool {
        self.permits(schema)
    }

    /// Answers "not injected" for every element, which is the value that LETS a
    /// rename through. Its three readers - the rename source, the rename target and
    /// the target primary key - each deny only when this is true, so under body
    /// scope the injected-shape immutability rule cannot fire whatever the policy
    /// says.
    ///
    /// This IS read: `check_body_text` re-parses the body as SQL and recurses into
    /// `check_node` for every statement AND for every embedded string literal, and
    /// `check_node` routes to `check_namespace_structural`, which owns the
    /// immutability decision. An `EXECUTE 'ALTER TABLE t RENAME COLUMN ...'` reaches
    /// it.
    ///
    /// `check_raw_view_body_text` is the sole constructor of this adapter, and it DOES
    /// have a production caller: `validate_raw_view_body_sql`, in
    /// `zeroship-migrate-core`'s `model::validate`, on every raw `viewBody`.
    ///
    /// What bounds the damage is the gate directly above that call, not the caller's
    /// absence. A raw view body is refused unless it parses as a single top-level
    /// SELECT - "DDL, DML, COPY, and utility statements are refused" - so a rename can
    /// never arrive as the body itself. It arrives only through the literal arm, and a
    /// string literal inside a view body is data that never executes. That is why the
    /// permissive answer is not a live escalation here, and it is a narrower claim than
    /// "nothing calls this".
    ///
    /// Still unresolved, and a public-API question rather than a local one: this is
    /// `pub`, so an embedder who calls it directly on text that is NOT gated to a single
    /// SELECT gets a scanner whose injected-shape rule silently never fires.
    fn is_injected_shape(&self, _object: &ObjectName, _element: &ShapeElement) -> bool {
        false
    }
}

struct GuardWalker<'a, D> {
    cfg: &'a D,
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

    fn walker(&self) -> GuardWalker<'_, GuardConfig> {
        GuardWalker { cfg: &self.cfg }
    }

    fn refuse_non_postgres_raw_sql(&self) -> Result<(), GuardError> {
        if self.cfg.dialect() != &DIALECT {
            return Err(GuardError::RawSqlRejected {
                dialect: self.cfg.dialect().clone(),
            });
        }
        Ok(())
    }

    /// Check a migration's SQL. Returns a [`GuardReport`] if every statement is
    /// safe (destructive ops flagged, not denied), or a [`GuardError`] on the
    /// first dangerous/cross-tenant/unparseable construct.
    ///
    /// # Errors
    /// - [`GuardError::Denied`] - a hard-denied construct (incl. ones nested
    ///   inside `DO $$...$$` blocks and function bodies).
    /// - [`GuardError::CrossSchema`] - a reference outside the project schema.
    /// - [`GuardError::Parse`] - unparseable SQL (deny-by-default).
    ///
    /// Every config runs every walk - deny-list, cross-schema and body - and how far
    /// a caller's SQL gets is decided by the composed policy alone, never by a
    /// root/host-set guard mode.
    pub fn check(&self, sql: &str) -> Result<GuardReport, GuardError> {
        // Non-Postgres fail-closed backstop. `SqlGuard` is the **Postgres** line-1
        // (libpg_query below); it is the PG arm of the per-engine
        // [`MigrationGuard`] seam ([`PgGuard`] wraps it). The engine never selects
        // `SqlGuard` for SQLite OR MySQL - each routes through its own registered
        // descriptor guard, the trusted descriptor-diff path. The arm just below
        // defends against a wrong caller for either shipping or future backends.
        // This arm is the defensive fail-closed for the *wrong caller*: if a raw,
        // untrusted non-Postgres string is ever handed to the PG guard,
        // `libpg_query` cannot vet it, so we reject rather than mis-parse.
        self.refuse_non_postgres_raw_sql()?;

        let classes = classify(sql)?;

        // Walk the full parse tree once per statement: the data-security policy check,
        // then the deny-list / cross-schema / body walk. Both run for every config.
        let parsed = pg_query::parse(sql).map_err(|e| ParseError::Syntax(e.to_string()))?;
        let mut data_security_advisories = Vec::new();
        let mut class_index = 0;
        for raw_stmt in &parsed.protobuf.stmts {
            let Some(node) = raw_stmt.stmt.as_ref().and_then(|s| s.node.as_ref()) else {
                continue;
            };
            let class = classes.get(class_index).ok_or_else(|| {
                GuardError::Parse(ParseError::Syntax(
                    "internal classifier/parse statement count mismatch".to_string(),
                ))
            })?;
            class_index += 1;
            let raw = stmt_text(sql, raw_stmt);
            self.walker().check_sql_data_security_policy(
                class,
                &raw,
                &mut data_security_advisories,
            )?;

            // Serialize the ONE statement subtree to JSON for the generic
            // full-tree walks (dangerous funcs + every schema reference). This
            // sidesteps `node.nodes()`, whose hand-written traversal skips
            // column DEFAULT / CHECK / VALUES / RULE-action subtrees.
            let json = guard_stmt_json(raw_stmt, &raw)?;
            self.walker().check_node(node, &json, &raw)?;
        }

        // Collect operational advisories (lock-heavy / destructive / rename /
        // missing-FK-index shapes). These are ADVISORY ONLY - see
        // `crate::analysis::analyze`; they never deny or gate. We reuse the single parse
        // already done above by re-running the analyzer engine over the SQL.
        let mut advisories = crate::analysis::analyze::analyze(sql);
        advisories.extend(data_security_advisories);

        let destructive = classes.iter().any(|c| c.destructive);
        Ok(GuardReport {
            classes,
            destructive,
            advisories,
        })
    }

    /// Backstop for the two IR raw islands (`raw` and `createFunction.body`): the
    /// deny-list for host-reaching and privilege-escalating constructs, WITHOUT
    /// project-schema confinement.
    ///
    /// Arbitrary SQL embedded inside otherwise structured IR must still be refused a
    /// host reach. `render::lower` reaches PostgreSQL's answer through
    /// [`MigrationGuard::check_raw_island_sql`], which this backs.
    ///
    /// [`MigrationGuard::check_raw_island_sql`]: zeroship_migrate_backend::guard::MigrationGuard::check_raw_island_sql
    ///
    /// # Errors
    /// [`GuardError`] when parsing fails or a deny-listed construct is found.
    pub fn check_raw_island_sql_backstop(&self, sql: &str) -> Result<(), GuardError> {
        self.refuse_non_postgres_raw_sql()?;

        let parsed = pg_query::parse(sql).map_err(|e| ParseError::Syntax(e.to_string()))?;
        for raw_stmt in &parsed.protobuf.stmts {
            let Some(node) = raw_stmt.stmt.as_ref().and_then(|s| s.node.as_ref()) else {
                continue;
            };
            let raw = stmt_text(sql, raw_stmt);
            let json = guard_stmt_json(raw_stmt, &raw)?;
            self.walker()
                .check_node_raw_island_backstop(node, &json, &raw)?;
        }
        Ok(())
    }

    /// The function-body peer of [`Self::check_raw_island_sql_backstop`], uncalled on
    /// the lowering path for the same reason. PL/pgSQL is only
    /// best-effort parseable as SQL, so this intentionally reuses the existing body
    /// scanner: parse what can be parsed, inspect dynamic SQL literals, then token
    /// scan for deny-listed names.
    pub fn check_raw_island_body_backstop(&self, body: &str, raw: &str) -> Result<(), GuardError> {
        self.refuse_non_postgres_raw_sql()?;
        self.walker().check_body_text(body, raw)
    }

    /// Is a raw island inside the reach of a `safety.require_rls` obligation?
    ///
    /// This is PostgreSQL's answer to
    /// [`zeroship_migrate_backend::guard::MigrationGuard::raw_island_escapes_rls_net_state`].
    /// The neutral net-state
    /// walk in `zeroship_migrate_backend::guard::check_ir_data_security_policy` decides
    /// `require_rls` over structured ops for every dialect; the raw island is the one
    /// op whose net table state is not derivable from the IR, so it is asked HERE,
    /// where the parser is.
    ///
    /// The guard cannot enumerate a raw island's net table state, so an island the
    /// obligation reaches is refused, as it always has been. The REACH is per
    /// relation: each statement is attributed to the relations its parse names,
    /// resolved by `raw_relation_target` exactly as a raw create target is, and an
    /// island naming only relations the obligation does not cover is admitted.
    ///
    /// Three cases carry no usable attribution and are treated as inside the reach:
    /// SQL the Postgres parser rejects, a relation with no schema the config can pin,
    /// and a statement naming no relation at all (`SET`, `DO`, `CALL`: a body that can
    /// create a table this parse never shows us). All three are only refused when an
    /// obligation is authored to refuse them against.
    #[must_use]
    pub fn raw_island_within_require_rls(&self, sql: &str) -> bool {
        let cfg = &self.cfg;
        if !cfg.require_rls_authored_anywhere() {
            return false;
        }
        let Ok(parsed) = pg_query::parse(sql) else {
            return true;
        };
        for raw_stmt in &parsed.protobuf.stmts {
            let Ok(json) = serde_json::to_value(raw_stmt) else {
                return true;
            };
            let mut named_a_relation = false;
            let mut within = false;
            walk_range_vars(&json, &mut |schemaname, relname| {
                named_a_relation = true;
                within = match raw_relation_target(cfg, schemaname, relname) {
                    RawRelationTarget::Resolved(object) => cfg.requires_rls_at(&object),
                    RawRelationTarget::UnpinnedSchema | RawRelationTarget::Unattributable => true,
                };
                within
            });
            if within || !named_a_relation {
                return true;
            }
        }
        // An island of zero statements names nothing to refuse.
        false
    }
}

impl<D: GuardDecisions> GuardWalker<'_, D> {
    /// Check one top-level statement node (and everything nested under it).
    ///
    /// `json` is the `serde_json` serialization of the statement's `RawStmt`
    /// subtree - used by the generic full-tree walks so we
    /// visit EVERY node, including the slots `pg_query::nodes()` skips (column
    /// DEFAULT, CHECK, VALUES lists, RULE actions, SET SCHEMA targets, ...).
    fn check_node(&self, node: &NodeEnum, json: &Value, raw: &str) -> Result<(), GuardError> {
        // 0. Scoped-raw-SQL refusals (II.2.5) run FIRST, but ONLY under a Scoped
        //    (non-Top) `sql.raw` grant - the posture that HAS relaxed raw SQL, so
        //    the refined namespace refusal (SearchPathUnderScopedRawSql /
        //    OpaqueBodyUnderScopedRawSql / UnqualifiedNameUnderScopedRawSql) owns the
        //    diagnostic instead of the deny-list belt. Under the plain confined path
        //    (raw_sql Ungranted) this is a no-op and the belt keeps its codes.
        if self.cfg.raw_sql_region() == GrantRegion::Scoped {
            self.check_scoped_raw_sql(node, raw)?;
        }

        // 1. Statement-kind gate: DENY-BY-DEFAULT. Only an enumerated set of
        //    known-safe migration statements passes; everything else is denied.
        self.check_statement_kind(node, raw)?;

        // 2. Cross-schema confinement - any explicit foreign schema, anywhere
        //    in the full tree (RangeVar, SET SCHEMA newschema, CreateSchema,
        //    trigger/CALL funcname, COMMENT object, INHERIT target, ...). Owns the
        //    diagnostic for a foreign-schema reference (`CrossSchema`), so it runs
        //    before the namespace creation-gating below - a cross-tenant `CREATE TABLE
        //    other.t` is a cross-schema violation first, not a creation-gating one.
        self.check_cross_schema(json, raw)?;

        // 2c. NAMESPACE-authority structural gate (II.2.5 raw-SQL create/rename
        //     classification / II.2.6 creation-gating + injected-shape immutability):
        //     a raw create must pass `schema.create_table` and, in an inject scope,
        //     must declare the injected shape; a rename/move needs `schema.rename`
        //     and is denied into an inject scope; an alter/drop of an injected shape
        //     element is immutable.
        //     Runs AFTER cross-schema so a foreign-schema target reports `CrossSchema`.
        self.check_namespace_structural(node, raw)?;

        // 2b. System-catalog relation reads/writes - `pg_catalog.pg_authid`,
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
        //     name-resolving builtin (`nextval`/`setval`/`to_regclass`/...) is an
        //     `A_Const`, invisible to the structural schema walker. Re-check the
        //     literal's leading `schema.` qualifier for confinement.
        self.check_literal_schema_refs(json, raw)?;

        // 3d. `set_config('search_path'|'role'|..., ...)` is the function form of a
        //     `SET <param>` the structural VariableSetStmt gate denies - deny
        //     the call form identically.
        Self::check_set_config_calls(json, raw)?;

        // 3e. `query_to_xml('SELECT ... FROM control.users', ...)` & family take a
        //     free-form SQL string the server executes; the embedded SQL is
        //     never re-parsed by the walks above, so a cross-schema read,
        //     file-access function, or DDL hidden in the literal slips past.
        //     Re-parse each such literal and run the SAME guard recursively
        //     (reusing `check_body_text`, the body re-parse machinery).
        //     A non-literal/runtime query arg is out of parse-scope - line-2
        //     (least-priv `migrator` role) defense, same limit as `set_config`.
        self.check_sql_string_arg_calls(json, raw)?;

        // 4. Recurse into DO blocks and function bodies - the must-inspect
        //    case. A dangerous construct hidden in a body is still dangerous.
        self.check_bodies(node, raw)?;

        Ok(())
    }

    /// NAMESPACE-authority STRUCTURAL gate: it classifies a raw create/rename,
    /// gates creation on the target grant, and holds injected shapes immutable.
    /// The scoped-raw-SQL refusals (`SET search_path` / opaque body / unqualified
    /// name) are a SEPARATE method ([`Self::check_scoped_raw_sql`]) run earlier, only
    /// under a Scoped `sql.raw` grant.
    ///
    /// A raw statement the guard sees via [`SqlGuard::check`] must ALSO clear the
    /// structured gate for whatever it does. Injection is an IR transform that cannot
    /// rewrite raw text, so every rule here fails closed. Dispatch is by parsed shape:
    ///
    /// - a **create** (`CreateStmt` incl. `LIKE`/`INHERITS`/`PARTITION OF`,
    ///   `CreateTableAsStmt` = CTAS / `CREATE TABLE AS EXECUTE`, `SelectStmt` with an
    ///   `into_clause` = `SELECT ... INTO`) must pass `schema.create_table` at the target
    ///   AND, wherever an inject rule covers it, must declare every injected column and
    ///   exactly the pinned primary key (`RawCreateInInjectScope`). A create whose
    ///   columns the parse cannot enumerate (`LIKE`/`INHERITS`/`PARTITION OF`/`OF
    ///   <type>`/CTAS/`SELECT INTO`) can never prove that and stays denied;
    /// - a `CreateSchemaStmt` must pass `schema.create_schema`;
    /// - a `RenameStmt`/`AlterObjectSchemaStmt` that moves a table checks
    ///   `schema.rename` and is denied into any inject scope
    ///   (`RawRenameIntoInjectScope`); a `RENAME COLUMN` of an injected column is
    ///   immutable;
    /// - an `AlterTableStmt` touching an injected column/PK is immutable
    ///   (`InjectedShapeImmutable`/`InjectedPrimaryKeyImmutable`).
    fn check_namespace_structural(&self, node: &NodeEnum, raw: &str) -> Result<(), GuardError> {
        match node {
            // -- raw create: CREATE TABLE (incl. LIKE / INHERITS / PARTITION OF) --
            NodeEnum::CreateStmt(c) => {
                let target = self.resolve_relation_target(c.relation.as_ref(), raw)?;
                self.gate_raw_create(&target, raw, Some(c))?;
            }
            // -- CTAS / CREATE TABLE AS EXECUTE ----------------------------------
            NodeEnum::CreateTableAsStmt(cta) => {
                // A materialized view is not a table create; only OBJECT_TABLE /
                // SELECT INTO bring a table into existence. Other objtypes fall
                // through (matview creation is gated by its own vendor cap).
                if cta.objtype == ObjectType::ObjectTable as i32 || cta.is_select_into {
                    let rel = cta.into.as_ref().and_then(|i| i.rel.as_ref());
                    let target = self.resolve_relation_target(rel, raw)?;
                    self.gate_raw_create(&target, raw, None)?;
                }
            }
            // -- SELECT ... INTO <table> -------------------------------------------
            // pg_query parses `SELECT ... INTO t` as a `SelectStmt` carrying an
            // `into_clause`, NOT a `CreateTableAsStmt`. It still brings a table into
            // existence, so it is a raw create and gets the same gate.
            NodeEnum::SelectStmt(s) => {
                if let Some(into) = s.into_clause.as_ref() {
                    let target = self.resolve_relation_target(into.rel.as_ref(), raw)?;
                    self.gate_raw_create(&target, raw, None)?;
                }
            }
            // -- CREATE SCHEMA ---------------------------------------------------
            NodeEnum::CreateSchemaStmt(cs) => {
                // An unqualified/dynamic schema name is unattributable -> fail closed
                // unless create_schema is Top. `schemaname` empty => AUTHORIZATION-only
                // form; treat as unattributable.
                let name = cs.schemaname.trim();
                let obj = if name.is_empty() {
                    None
                } else {
                    normalize_object_name(name)
                };
                match obj {
                    Some(schema_obj) => {
                        if !self.cfg.grants_namespace_bool(
                            policy_registry::KEY_SCHEMA_CREATE_SCHEMA,
                            &schema_obj,
                        ) {
                            return Err(namespace_denied(
                                namespace_rule::CREATE_SCHEMA_NOT_GRANTED,
                                raw,
                            ));
                        }
                    }
                    None => {
                        return Err(namespace_denied(
                            namespace_rule::CREATE_SCHEMA_NOT_GRANTED,
                            raw,
                        ))
                    }
                }
            }
            // -- RENAME (table / column) -----------------------------------------
            NodeEnum::RenameStmt(r) => self.check_rename(r, raw)?,
            // -- SET SCHEMA (move a table across schemas) ------------------------
            NodeEnum::AlterObjectSchemaStmt(a) => self.check_set_schema(a, raw)?,
            // -- ALTER TABLE (injected-shape immutability) -----------------------
            NodeEnum::AlterTableStmt(at) => self.check_alter_table_injected(at, raw)?,
            _ => {}
        }
        Ok(())
    }

    /// Resolve the concrete normalized [`ObjectName`] a create/rename targets. An
    /// unqualified relation resolves to the config's pinned project schema (the
    /// search_path is pinned under Confined); when there is no unique pinned schema,
    /// the target is unattributable and the statement fails closed under any grant.
    fn resolve_relation_target(
        &self,
        rel: Option<&protobuf::RangeVar>,
        raw: &str,
    ) -> Result<ObjectName, GuardError> {
        let (schemaname, relname) = match rel {
            Some(rel) => (rel.schemaname.as_str(), rel.relname.as_str()),
            None => ("", ""),
        };
        match raw_relation_target(self.cfg, schemaname, relname) {
            RawRelationTarget::Resolved(object) => Ok(object),
            RawRelationTarget::UnpinnedSchema => Err(namespace_denied(
                namespace_rule::UNQUALIFIED_NAME_UNDER_SCOPED_RAW_SQL,
                raw,
            )),
            RawRelationTarget::Unattributable => Err(namespace_denied(
                namespace_rule::UNATTRIBUTABLE_RAW_UNDER_SCOPED_RAW_SQL,
                raw,
            )),
        }
    }

    /// Gate a CREATE-TABLE-shaped statement: inside an inject scope the statement
    /// must CONFORM to every covering inject, and `schema.create_table` must grant the
    /// target (II.2.5/II.2.6a).
    ///
    /// Conformance is decided from the statement text alone, because the same text is
    /// re-guarded at apply/baseline/precondition sites that carry no op and no origin.
    /// The property it delivers is narrower than the IR-level
    /// `resolved_create_table_matches_inject` predicate: text proves only that the
    /// create omits no injected column slot and contradicts no pinned primary key. It
    /// says nothing about column types, which the parse cannot compare against the
    /// spec.
    ///
    /// It also says nothing about injected INDEXES, and cannot: a `CREATE INDEX` is a
    /// separate statement, so a `CREATE TABLE` can never carry proof of an index
    /// obligation while the guard decides one statement at a time. An admitted raw
    /// create may therefore drop an injected index, which is an accepted loosening
    /// against the blanket deny this rule replaced. Injected indexes are not a reason
    /// to deny the create: the structured resolver emits them as separate statements
    /// right after it.
    ///
    /// `declared` is the parsed create when the statement has a column list at all;
    /// `None` for CTAS / `SELECT INTO`, which declare no columns and therefore can
    /// never prove conformance.
    fn gate_raw_create(
        &self,
        target: &ObjectName,
        raw: &str,
        declared: Option<&protobuf::CreateStmt>,
    ) -> Result<(), GuardError> {
        let injects = self.cfg.covering_inject_shapes(target);
        if !injects.is_empty()
            && !declared
                .and_then(declared_create_shape)
                .is_some_and(|shape| shape.conforms_to(&injects))
        {
            return Err(namespace_denied(
                namespace_rule::RAW_CREATE_IN_INJECT_SCOPE,
                raw,
            ));
        }
        if !self
            .cfg
            .grants_namespace_bool(policy_registry::KEY_SCHEMA_CREATE_TABLE, target)
        {
            return Err(namespace_denied(
                namespace_rule::CREATE_TABLE_NOT_GRANTED,
                raw,
            ));
        }
        Ok(())
    }

    /// `ALTER TABLE ... RENAME TO` (table move) / `RENAME COLUMN` (injected-column
    /// immutability). A bare same-schema table rename still re-anchors under
    /// `schema.rename` at the target and is denied into any inject scope.
    fn check_rename(&self, r: &protobuf::RenameStmt, raw: &str) -> Result<(), GuardError> {
        let is_table = r.rename_type == ObjectType::ObjectTable as i32;
        let is_column = r.rename_type == ObjectType::ObjectColumn as i32;
        if !is_table && !is_column {
            // `RenameStmt` covers `ALTER <anything> RENAME TO`, and a role, database,
            // or schema rename carries its target in `subname`/`newname` rather than
            // in a relation or object list. Those scalar slots are invisible to
            // `walk_schema_names`, so falling through to `Ok` here granted through a
            // rename exactly the authority the other spellings hard-deny: `DROP
            // SCHEMA control` is refused while `ALTER SCHEMA control RENAME TO x`
            // was not. Route each back to the rule that owns it.
            //
            // Object types that name their target through `relation` or `object`
            // (index, view, sequence, type, function, ...) still fall through: the
            // cross-schema walk already reaches those and denies a foreign target.
            if r.rename_type == ObjectType::ObjectRole as i32 {
                if self
                    .cfg
                    .grants_global_bool(policy_registry::KEY_ACCESS_ROLE)
                {
                    return Ok(());
                }
                return Err(denied(rule::ROLE_MANAGEMENT, raw));
            }
            if r.rename_type == ObjectType::ObjectDatabase as i32 {
                return Err(denied(rule::DATABASE_MANAGEMENT, raw));
            }
            if r.rename_type == ObjectType::ObjectSchema as i32 {
                // Both ends matter: renaming a schema you do not own takes it away,
                // and renaming your own onto another name claims that one.
                for schema in [r.subname.trim(), r.newname.trim()] {
                    if !schema.is_empty() && !self.cfg.permits_schema(schema) {
                        return Err(GuardError::CrossSchema {
                            schema: schema.to_string(),
                            statement: raw.to_string(),
                        });
                    }
                }
                return Ok(());
            }
            return Ok(());
        }
        // The object being renamed (its current name).
        let source = self.resolve_relation_target(r.relation.as_ref(), raw)?;
        if is_column {
            // RENAME COLUMN <subname> - immutable if the OLD column name is injected.
            let col = r.subname.trim();
            if !col.is_empty()
                && self
                    .cfg
                    .is_injected_shape(&source, &ShapeElement::Column(col))
                && !self
                    .cfg
                    .grants_namespace_bool(policy_registry::KEY_SCHEMA_ALTER_INJECTED, &source)
            {
                return Err(namespace_denied(
                    namespace_rule::INJECTED_SHAPE_IMMUTABLE,
                    raw,
                ));
            }
            return Ok(());
        }
        // Table rename: the new name is `newname` in the SAME schema as the source
        // (RENAME TO cannot change schema). Re-anchor at the target.
        let new_schema = source.schema.clone();
        let new_name = r.newname.trim();
        if new_name.is_empty() {
            return Err(namespace_denied(
                namespace_rule::UNATTRIBUTABLE_RAW_UNDER_SCOPED_RAW_SQL,
                raw,
            ));
        }
        let target = ObjectName {
            schema: new_schema,
            table: Some(new_name.as_bytes().to_vec()),
        };
        self.gate_rename_into(&source, &target, raw)
    }

    /// `ALTER TABLE ... SET SCHEMA <newschema>` - a cross-schema table move.
    fn check_set_schema(
        &self,
        a: &protobuf::AlterObjectSchemaStmt,
        raw: &str,
    ) -> Result<(), GuardError> {
        if a.object_type != ObjectType::ObjectTable as i32 {
            return Ok(());
        }
        let source = self.resolve_relation_target(a.relation.as_ref(), raw)?;
        let new_schema = a.newschema.trim();
        if new_schema.is_empty() {
            return Err(namespace_denied(
                namespace_rule::UNATTRIBUTABLE_RAW_UNDER_SCOPED_RAW_SQL,
                raw,
            ));
        }
        let table = source
            .table
            .clone()
            .unwrap_or_else(|| source.schema.clone());
        let target = ObjectName {
            schema: new_schema.as_bytes().to_vec(),
            table: Some(table),
        };
        self.gate_rename_into(&source, &target, raw)
    }

    /// Shared rename/move gate (II.2.5/II.2.6b/d). Only fires when the move CROSSES a
    /// scope boundary (the covering inject/rename-grant set differs before vs after):
    /// - denied into ANY inject scope (`RawRenameIntoInjectScope` - the moved table
    ///   would owe an injection the raw path cannot supply);
    /// - otherwise requires `schema.rename` at the target.
    fn gate_rename_into(
        &self,
        source: &ObjectName,
        target: &ObjectName,
        raw: &str,
    ) -> Result<(), GuardError> {
        // A no-op rename (same normalized name) crosses no boundary.
        if source == target {
            return Ok(());
        }
        // Moving INTO an inject scope: raw text cannot carry the injection.
        if self.cfg.injects_cover(target) {
            return Err(namespace_denied(
                namespace_rule::RAW_RENAME_INTO_INJECT_SCOPE,
                raw,
            ));
        }
        if !self
            .cfg
            .grants_namespace_bool(policy_registry::KEY_SCHEMA_RENAME, target)
        {
            return Err(namespace_denied(
                namespace_rule::RENAME_INTO_NOT_GRANTED,
                raw,
            ));
        }
        Ok(())
    }

    /// Injected-shape immutability for `ALTER TABLE` subcommands (II.2.6b): a
    /// `DROP COLUMN` / `ALTER COLUMN` (type/nullability) on an injected column, or a
    /// `DROP CONSTRAINT` of a pinned PK, is denied unless `schema.alter_injected`
    /// grants the table. An injected-index `DROP INDEX` is handled on the `DropStmt`
    /// arm (indexes are not `ALTER TABLE` subcommands here).
    fn check_alter_table_injected(
        &self,
        at: &protobuf::AlterTableStmt,
        raw: &str,
    ) -> Result<(), GuardError> {
        // ALTER TABLE only - a matview/index objtype carries no injected table shape.
        if at.objtype != ObjectType::ObjectTable as i32
            && at.objtype != ObjectType::ObjectType as i32
        {
            // objtype 0 (unset) is the common ALTER TABLE spelling; only skip a
            // clearly non-table objtype. Fall through for TABLE / unset.
            if at.objtype != 0 {
                return Ok(());
            }
        }
        let target = self.resolve_relation_target(at.relation.as_ref(), raw)?;
        let granted = self
            .cfg
            .grants_namespace_bool(policy_registry::KEY_SCHEMA_ALTER_INJECTED, &target);
        for cmd in &at.cmds {
            let Some(NodeEnum::AlterTableCmd(c)) = cmd.node.as_ref() else {
                continue;
            };
            use AlterTableType as A;
            let col = c.name.trim();
            // Column-touching subtypes: DROP COLUMN / ALTER COLUMN TYPE /
            // SET|DROP NOT NULL / SET DEFAULT / DROP IDENTITY, etc.
            let touches_column = matches!(
                AlterTableType::try_from(c.subtype),
                Ok(A::AtDropColumn
                    | A::AtAlterColumnType
                    | A::AtColumnDefault
                    | A::AtCookedColumnDefault
                    | A::AtDropNotNull
                    | A::AtSetNotNull
                    | A::AtDropIdentity
                    | A::AtSetIdentity
                    | A::AtAddIdentity)
            );
            if touches_column
                && !col.is_empty()
                && self
                    .cfg
                    .is_injected_shape(&target, &ShapeElement::Column(col))
                && !granted
            {
                return Err(namespace_denied(
                    namespace_rule::INJECTED_SHAPE_IMMUTABLE,
                    raw,
                ));
            }
            // DROP CONSTRAINT - may drop the pinned PK. We cannot always tell which
            // constraint is the PK from the name alone, so if the table's PK is
            // pinned by a covering inject rule, a DROP CONSTRAINT is immutable
            // (fail-closed) unless granted.
            if matches!(AlterTableType::try_from(c.subtype), Ok(A::AtDropConstraint))
                && self
                    .cfg
                    .is_injected_shape(&target, &ShapeElement::PrimaryKey)
                && !granted
            {
                return Err(namespace_denied(
                    namespace_rule::INJECTED_PRIMARY_KEY_IMMUTABLE,
                    raw,
                ));
            }
        }
        Ok(())
    }

    /// Scoped-raw-SQL refusals (II.2.5): under a Scoped (non-Top) `sql.raw` grant,
    /// an opaque-body construct (`CREATE FUNCTION`/`PROCEDURE`/`TRIGGER`/`DO`), a
    /// `SET search_path` (or `set_config`/`ALTER ROLE|DATABASE ... SET search_path`),
    /// and any unqualified object reference are unattributable and DENIED - only a
    /// Top-scoped grant may carry them.
    fn check_scoped_raw_sql(&self, node: &NodeEnum, raw: &str) -> Result<(), GuardError> {
        match node {
            // Opaque bodies: the outer parse cannot see what the body touches.
            NodeEnum::CreateFunctionStmt(_)
            | NodeEnum::CreateTrigStmt(_)
            | NodeEnum::DoStmt(_)
            | NodeEnum::AlterFunctionStmt(_) => {
                return Err(namespace_denied(
                    namespace_rule::OPAQUE_BODY_UNDER_SCOPED_RAW_SQL,
                    raw,
                ));
            }
            // SET search_path (and role/session_authorization name-resolution GUCs).
            NodeEnum::VariableSetStmt(s) => {
                let name = s.name.to_ascii_lowercase();
                if name == "search_path" || raw.to_ascii_lowercase().contains("search_path") {
                    return Err(namespace_denied(
                        namespace_rule::SEARCH_PATH_UNDER_SCOPED_RAW_SQL,
                        raw,
                    ));
                }
            }
            // ALTER ROLE / ALTER DATABASE ... SET search_path - a persisted GUC.
            NodeEnum::AlterRoleSetStmt(_) | NodeEnum::AlterDatabaseSetStmt(_)
                if raw.to_ascii_lowercase().contains("search_path") =>
            {
                return Err(namespace_denied(
                    namespace_rule::SEARCH_PATH_UNDER_SCOPED_RAW_SQL,
                    raw,
                ));
            }
            _ => {}
        }
        // `set_config('search_path', ...)` (the function form) + any UNQUALIFIED object
        // reference are both unattributable under a scoped grant: re-serialize the
        // statement and walk it structurally. An `A_Const` first arg to `set_config`
        // naming `search_path` refuses; a `RangeVar` with an empty schemaname and a
        // non-`pg_` relname is an unqualified reference (fail-closed).
        if let Ok(json) = pg_query::parse(raw)
            .map_err(|e| ParseError::Syntax(e.to_string()))
            .and_then(|p| {
                let Some(first) = p.protobuf.stmts.first() else {
                    return Ok(Value::Null);
                };
                guard_stmt_json(first, raw).map_err(|_| ParseError::Syntax("json".into()))
            })
        {
            let mut set_config_search_path = false;
            walk_set_config_calls(&json, &mut |param| {
                if param.eq_ignore_ascii_case("search_path") {
                    set_config_search_path = true;
                    return true;
                }
                false
            });
            if set_config_search_path {
                return Err(namespace_denied(
                    namespace_rule::SEARCH_PATH_UNDER_SCOPED_RAW_SQL,
                    raw,
                ));
            }
            let mut unqualified = false;
            walk_range_vars(&json, &mut |schema, relname| {
                let is_pg_catalog = has_pg_catalog_prefix(relname);
                if schema.trim().is_empty() && !relname.trim().is_empty() && !is_pg_catalog {
                    unqualified = true;
                    return true;
                }
                false
            });
            if unqualified {
                return Err(namespace_denied(
                    namespace_rule::UNQUALIFIED_NAME_UNDER_SCOPED_RAW_SQL,
                    raw,
                ));
            }
        }
        Ok(())
    }

    /// Raw-island variant of [`Self::check_node`]. It preserves the deny-list and
    /// body scanning but skips project-schema confinement: schema ownership was the
    /// belt-off posture's to decide, and this backstop only had to keep dangerous
    /// arbitrary raw text out.
    fn check_node_raw_island_backstop(
        &self,
        node: &NodeEnum,
        json: &Value,
        raw: &str,
    ) -> Result<(), GuardError> {
        self.check_statement_kind(node, raw)?;
        Self::check_system_catalog_relations(json, raw)?;
        Self::check_dangerous_functions(json, raw)?;
        Self::check_regproc_casts(json, raw)?;
        Self::check_set_config_calls(json, raw)?;
        self.check_sql_string_arg_calls(json, raw)?;
        self.check_bodies(node, raw)?;
        Ok(())
    }

    /// Statement-kind gate, **deny-by-default**.
    ///
    /// A curated allowlist of known-safe migration statement kinds passes;
    /// every other statement node is denied (`UNRECOGNIZED_DANGEROUS`). The
    /// recognized-dangerous kinds are matched first so they get a precise rule
    /// id (better diagnostics) - but the *default* arm is DENY, not allow.
    #[allow(clippy::too_many_lines)]
    fn check_statement_kind(&self, node: &NodeEnum, raw: &str) -> Result<(), GuardError> {
        match node {
            // ---- Recognized-dangerous: precise rule ids ----
            // COPY ... PROGRAM = shell RCE; COPY ... <file> = filesystem.
            // COPY ... TO STDOUT / FROM STDIN (no program, no filename) is fine.
            NodeEnum::CopyStmt(c) => {
                if c.is_program {
                    return Err(denied(rule::COPY_PROGRAM, raw));
                }
                if !c.filename.is_empty() {
                    return Err(denied(rule::COPY_FILE, raw));
                }
                // Plain COPY ... TO STDOUT / FROM STDIN - safe.
                return Ok(());
            }
            // ALTER SYSTEM - cluster-wide config, always denied (BOTH profiles).
            NodeEnum::AlterSystemStmt(_) => return Err(denied(rule::ALTER_SYSTEM, raw)),
            // Role management - privilege escalation. ALLOW iff Platform:
            // the platform schema migrations must CREATE/ALTER/DROP roles and
            // pin their search_path. Confined still hard-denies.
            //
            // SUPERUSER is the ONE role attribute that stays HARD-DENIED even
            // under Platform: a superuser bypasses RLS and
            // reaches the host (file I/O, `COPY ... PROGRAM`). Platform widens
            // privilege *within* the DB, never *host* reach - so a
            // `CREATE/ALTER ROLE ... SUPERUSER` is refused before the Platform
            // allow. This guards the vendor `createRole({ superuser: true })`
            // render-here-refuse-at-guard backstop.
            NodeEnum::CreateRoleStmt(s) => {
                // SUPERUSER stays a HARD DENY (non-grant hard rule) even under a
                // granting policy: it bypasses RLS + reaches the host.
                if role_grants_superuser(&s.options) {
                    return Err(denied(rule::SUPERUSER_ROLE, raw));
                }
                if self.cfg.grants_global_bool(policy_registry::KEY_ACCESS_ROLE) {
                    return Ok(());
                }
                return Err(denied(rule::ROLE_MANAGEMENT, raw));
            }
            NodeEnum::AlterRoleStmt(s) => {
                if role_grants_superuser(&s.options) {
                    return Err(denied(rule::SUPERUSER_ROLE, raw));
                }
                if self.cfg.grants_global_bool(policy_registry::KEY_ACCESS_ROLE) {
                    return Ok(());
                }
                return Err(denied(rule::ROLE_MANAGEMENT, raw));
            }
            NodeEnum::AlterRoleSetStmt(_) | NodeEnum::DropRoleStmt(_) => {
                if self.cfg.grants_global_bool(policy_registry::KEY_ACCESS_ROLE) {
                    return Ok(());
                }
                return Err(denied(rule::ROLE_MANAGEMENT, raw));
            }
            // GRANT / REVOKE / role-membership grants - privilege management.
            // ALLOW iff Platform: the platform schema migrations grant
            // CONNECT/USAGE/etc. Confined still hard-denies.
            NodeEnum::GrantStmt(s) => {
                if grant_stmt_grants_privileged_role(s) {
                    return Err(denied(rule::PRIVILEGED_ROLE_GRANT, raw));
                }
                if self.cfg.grants_global_bool(policy_registry::KEY_ACCESS_GRANT) {
                    return Ok(());
                }
                return Err(denied(rule::PRIVILEGE_MANAGEMENT, raw));
            }
            NodeEnum::GrantRoleStmt(s) => {
                if grant_role_stmt_grants_privileged_role(s) {
                    return Err(denied(rule::PRIVILEGED_ROLE_GRANT, raw));
                }
                if self.cfg.grants_global_bool(policy_registry::KEY_ACCESS_GRANT) {
                    return Ok(());
                }
                return Err(denied(rule::PRIVILEGE_MANAGEMENT, raw));
            }
            NodeEnum::AlterDefaultPrivilegesStmt(s) => {
                if alter_default_privileges_grants_privileged_role(s) {
                    return Err(denied(rule::PRIVILEGED_ROLE_GRANT, raw));
                }
                if self.cfg.grants_global_bool(policy_registry::KEY_ACCESS_GRANT) {
                    return Ok(());
                }
                return Err(denied(rule::PRIVILEGE_MANAGEMENT, raw));
            }
            // Database / FDW management - out of a project migrator's remit.
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
            // LOAD <library> - loads a shared object into the backend (RCE).
            NodeEnum::LoadStmt(_) => return Err(denied(rule::LOAD_LIBRARY, raw)),

            // ---- Allowlisted-safe (with per-kind sub-checks) ----
            NodeEnum::CreateFunctionStmt(f) => {
                // The funcname is the CREATION TARGET, not a call qualifier:
                // defining a function INTO `public`/`pg_catalog`/
                // `information_schema`/another tenant schema is denied (no
                // shared-schema exemption - that applies only to call sites).
                self.check_func_def_target(&f.funcname, raw)?;
                // Untrusted language (plpythonu/plperlu/c/...) - RCE.
                if let Some(lang) = function_language(&f.options) {
                    if !crate::guard::denylist::is_trusted_language(&lang) {
                        return Err(denied(rule::UNTRUSTED_LANGUAGE, raw));
                    }
                }
                // SECURITY DEFINER - runs with the migrator's privilege once
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
                // ALTER FUNCTION ... SECURITY DEFINER / SET search_path = ...
                if alter_function_is_security_definer(&a.actions) {
                    return Err(denied(rule::SECURITY_DEFINER, raw));
                }
                if alter_function_sets_forbidden_param(&a.actions) {
                    return Err(denied(rule::FUNCTION_SET_SEARCH_PATH, raw));
                }
            }
            NodeEnum::CreateExtensionStmt(e) => {
                let name = e.extname.to_ascii_lowercase();
                // FORBIDDEN_EXTENSIONS is a non-grant HARD DENY in BOTH profiles,
                // overriding any allowlist grant.
                if crate::guard::denylist::list_contains_ci(crate::guard::denylist::FORBIDDEN_EXTENSIONS, &name) {
                    return Err(denied(rule::FORBIDDEN_EXTENSION, raw));
                }
                // The per-name allowlist is the `code.extension` StrSet grant value.
                let allowed = self
                    .cfg
                    .granted_extension_allowlist()
                    .iter()
                    .any(|a| a.eq_ignore_ascii_case(&name));
                if !allowed {
                    return Err(denied(rule::EXTENSION_NOT_ALLOWLISTED, raw));
                }
            }
            NodeEnum::VariableSetStmt(s) => {
                let name = s.name.to_ascii_lowercase();
                if crate::guard::denylist::list_contains_ci(crate::guard::denylist::FORBIDDEN_SET_PARAMS, &name) {
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
                // Role ownership and RLS changes need their respective charter
                // grants. Reparenting, replica identity and generic options
                // remain outside the safe migration set.
                self.check_alter_table_cmds(at, raw)?;
            }
            NodeEnum::DropStmt(d) => {
                // DROP ROLE via the DropStmt spelling - ALLOW iff Platform
                // (the `.down.sql` reverse of CREATE ROLE), else deny.
                if d.remove_type == ObjectType::ObjectRole as i32 {
                    if self.cfg.grants_global_bool(policy_registry::KEY_ACCESS_ROLE) {
                        return Ok(());
                    }
                    return Err(denied(rule::ROLE_MANAGEMENT, raw));
                }
                // DROP is safe only for the enumerated object types. Under
                // Platform the extra set (schema/extension/policy - the
                // `.down.sql`-only reverses) is also admitted.
                //
                // Every member of that set is name-scoped, so each named target is
                // decided at itself and EVERY target must be granted: `DROP SCHEMA
                // owned, other` is not a drop the `owned` grant covers, and
                // `DROP EXTENSION granted, other` is not one the allowlist covers.
                let drop_allowed = is_safe_drop_object(d.remove_type)
                    || drop_object_targets(self.cfg, d)
                        .iter()
                        .all(|target| self.cfg.grants_drop_object(d.remove_type, target.as_ref()));
                if !drop_allowed {
                    return Err(denied(rule::UNRECOGNIZED_DANGEROUS, raw));
                }
                // DROP SCHEMA additionally has to be CONFINED, not merely granted.
                //
                // `grants_drop_object` answers `ObjectSchema` from the
                // `schema.create_schema` grant alone, which a charter may hold over
                // `all` while `cross_schema` stays scoped to the owned set - the shape
                // this engine's own operator fixture builds. Without the check below
                // such a charter could DROP a schema it could not CREATE:
                //
                //     CREATE SCHEMA control        -> CrossSchema
                //     DROP SCHEMA control CASCADE  -> admitted
                //
                // The name is a bare single-part String in `objects`, and
                // `qualified_list_schema` needs 2+ parts, so the cross-schema walk
                // that catches every other spelling never sees it.
                if d.remove_type == ObjectType::ObjectSchema as i32 {
                    for item in &d.objects {
                        if let Some(NodeEnum::String(s)) = item.node.as_ref() {
                            let schema = s.sval.trim();
                            if !schema.is_empty() && !self.cfg.permits_schema(schema) {
                                return Err(GuardError::CrossSchema {
                                    schema: schema.to_string(),
                                    statement: raw.to_string(),
                                });
                            }
                        }
                    }
                }
            }
            // CREATE SCHEMA - deny-by-default for Confined; ALLOW iff Platform
            // (platform migrations create platform schemas). When
            // Platform, fall through to the cross-schema confinement below (the
            // schema being created is checked against the allowlist there).
            NodeEnum::CreateSchemaStmt(cs) => {
                // Decided at the schema being created, the same object
                // `check_namespace_structural` resolves. A `CREATE SCHEMA
                // AUTHORIZATION joe` names no schema; that target is unattributable
                // here and the namespace gate owns its refusal.
                let target = normalize_object_name(cs.schemaname.trim());
                if !self.cfg.grants_object_bool(
                    policy_registry::KEY_SCHEMA_CREATE_SCHEMA,
                    target.as_ref(),
                ) {
                    return Err(denied(rule::UNRECOGNIZED_DANGEROUS, raw));
                }
            }
            // CREATE POLICY (RLS) - deny-by-default for Confined; ALLOW iff
            // Platform (0025 RLS policies). When Platform, fall through;
            // cross-schema confinement on the policy's table still runs below.
            NodeEnum::CreatePolicyStmt(p) => {
                // `access.policy` is PerTable, so the policy is decided at the table
                // it protects - the relation the statement's `ON` clause names.
                let target = optional_relation_target(self.cfg, p.table.as_ref());
                if !self
                    .cfg
                    .grants_object_bool(policy_registry::KEY_ACCESS_POLICY, target.as_ref())
                {
                    return Err(denied(rule::UNRECOGNIZED_DANGEROUS, raw));
                }
            }
            // DROP OWNED BY <role> - deny-by-default for Confined; ALLOW iff
            // Platform (0025 rollback DO-block).
            NodeEnum::DropOwnedStmt(_) => {
                if self.cfg.grants_global_bool(policy_registry::KEY_ACCESS_ROLE) {
                    return Ok(());
                }
                return Err(denied(rule::UNRECOGNIZED_DANGEROUS, raw));
            }
            NodeEnum::TransactionStmt(t) => {
                // BEGIN/START/COMMIT/ROLLBACK/SAVEPOINT/RELEASE/ROLLBACK TO are
                // fine; two-phase PREPARE TRANSACTION / COMMIT PREPARED /
                // ROLLBACK PREPARED reach the cluster's prepared-xact namespace
                // and are out of remit - denied.
                if !is_safe_transaction_kind(t.kind) {
                    return Err(denied(rule::UNRECOGNIZED_DANGEROUS, raw));
                }
            }

            // ---- Unconditionally-safe migration statement kinds ----
            NodeEnum::CreateStmt(_)
            | NodeEnum::IndexStmt(_)
            | NodeEnum::RenameStmt(_)
            | NodeEnum::CreateTrigStmt(_)
            | NodeEnum::ViewStmt(_)
            | NodeEnum::CreateTableAsStmt(_)
            | NodeEnum::RefreshMatViewStmt(_)
            | NodeEnum::CreateEnumStmt(_)
            | NodeEnum::CompositeTypeStmt(_)
            | NodeEnum::CreateRangeStmt(_)
            | NodeEnum::AlterEnumStmt(_)
            | NodeEnum::AlterTypeStmt(_)
            // CREATE DOMAIN / ALTER DOMAIN - a domain is a constrained base
            // type (`CREATE DOMAIN d AS text CHECK (...)`); altering one is
            // `ADD`/`DROP CONSTRAINT`/`SET`. No privilege, RCE, or host reach -
            // ordinary schema DDL, safe under BOTH profiles, same class as
            // CREATE ENUM / CREATE TYPE above. The domain's CREATION TARGET
            // schema (`domainname`) and its base-type schema (`type_name`) are
            // still confined by `check_cross_schema` for the Confined profile.
            | NodeEnum::CreateDomainStmt(_)
            | NodeEnum::AlterDomainStmt(_)
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
            => {}

            // `REINDEX SCHEMA <s>` / `REINDEX DATABASE <d>` name their target in a
            // bare `name` string, not a relation or object list, so the cross-schema
            // walk never sees it. That admitted `REINDEX SCHEMA control`: rebuilding
            // every index in a schema you do not own takes an ACCESS EXCLUSIVE lock on
            // each of its tables, which is a cross-tenant outage. `REINDEX DATABASE`
            // is the same reach over everything at once.
            //
            // INDEX/TABLE kinds carry a `relation` and are already covered.
            NodeEnum::ReindexStmt(r) => {
                if r.kind == protobuf::ReindexObjectType::ReindexObjectDatabase as i32
                    || r.kind == protobuf::ReindexObjectType::ReindexObjectSystem as i32
                {
                    return Err(denied(rule::DATABASE_MANAGEMENT, raw));
                }
                if r.kind == protobuf::ReindexObjectType::ReindexObjectSchema as i32 {
                    let schema = r.name.trim();
                    if !schema.is_empty() && !self.cfg.permits_schema(schema) {
                        return Err(GuardError::CrossSchema {
                            schema: schema.to_string(),
                            statement: raw.to_string(),
                        });
                    }
                }
            }

            // `COMMENT ON SCHEMA <s>` carries its target as a bare string in `object`,
            // the same slot the cross-schema walk skips, so commenting on another
            // tenant's schema was admitted. Every other COMMENT target names a
            // relation or a qualified list and is already covered.
            NodeEnum::CommentStmt(c) => {
                if c.objtype == ObjectType::ObjectSchema as i32 {
                    if let Some(NodeEnum::String(s)) =
                        c.object.as_ref().and_then(|o| o.node.as_ref())
                    {
                        let schema = s.sval.trim();
                        if !schema.is_empty() && !self.cfg.permits_schema(schema) {
                            return Err(GuardError::CrossSchema {
                                schema: schema.to_string(),
                                statement: raw.to_string(),
                            });
                        }
                    }
                }
            }

            // `DO [LANGUAGE lang] $$...$$` runs an anonymous block in an arbitrary
            // procedural language, so it needs the same language check as
            // `CREATE FUNCTION`: the language is a `DefElem` in exactly the same
            // shape. Without it, `DO LANGUAGE plpythonu $$ import os $$` was admitted
            // while the function spelling of the same body was denied as RCE. An
            // absent `LANGUAGE` is plpgsql, which is trusted.
            NodeEnum::DoStmt(d) => {
                if let Some(lang) = function_language(&d.args) {
                    if !crate::guard::denylist::is_trusted_language(&lang) {
                        return Err(denied(rule::UNTRUSTED_LANGUAGE, raw));
                    }
                }
            }

            // ---- DENY-BY-DEFAULT: every unenumerated statement kind ----
            _ => return Err(denied(rule::UNRECOGNIZED_DANGEROUS, raw)),
        }
        Ok(())
    }

    /// Reject `ALTER TABLE` subcommands outside the safe migration set or the
    /// capabilities explicitly granted by the author-owned charter.
    fn check_alter_table_cmds(
        &self,
        at: &protobuf::AlterTableStmt,
        raw: &str,
    ) -> Result<(), GuardError> {
        // `access.rls` is PerTable, so the four RLS subtypes are decided at the table
        // this ALTER TABLE names, not at a fixed witness.
        let target = optional_relation_target(self.cfg, at.relation.as_ref());
        let allow_rls = self
            .cfg
            .grants_object_bool(policy_registry::KEY_ACCESS_RLS, target.as_ref());
        for cmd in &at.cmds {
            if let Some(NodeEnum::AlterTableCmd(c)) = cmd.node.as_ref() {
                let ownership = c.subtype == AlterTableType::AtChangeOwner as i32;
                if ownership && c.newowner.as_ref().is_none_or(|role| {
                    role.roletype != protobuf::RoleSpecType::RolespecCstring as i32
                        || role.rolename.is_empty() || role_spec_names_privileged_role(role)
                }) {
                    return Err(denied(rule::PRIVILEGED_ROLE_GRANT, raw));
                }
                let subtype_allowed = is_safe_alter_table_subtype(c.subtype)
                    || (allow_rls && is_platform_alter_table_subtype(c.subtype))
                    || (ownership && self.cfg.grants_global_bool(policy_registry::KEY_ACCESS_ROLE));
                if !subtype_allowed {
                    return Err(denied(rule::UNSAFE_ALTER_TABLE_CMD, raw));
                }
            }
        }
        Ok(())
    }

    /// Deny any explicit reference to a schema other than the project schema,
    /// found ANYWHERE in the full parse tree, not only in the slots
    /// `pg_query::nodes()` enumerates.
    fn check_cross_schema(&self, json: &Value, raw: &str) -> Result<(), GuardError> {
        if let Some(schema) = foreign_schema_in_tree(json, &|s| self.cfg.permits_schema(s)) {
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
    /// call qualifier - so the `public`/`pg_catalog`/`information_schema`
    /// exemptions that apply at call sites do NOT apply: defining into any
    /// non-project schema is denied. `name` is the funcname/objname list of
    /// protobuf String nodes; an unqualified name (single part) is fine - it
    /// resolves under the pinned `search_path`.
    fn check_func_def_target(&self, name: &[protobuf::Node], raw: &str) -> Result<(), GuardError> {
        let parts: Vec<&str> = name
            .iter()
            .filter_map(|n| match n.node.as_ref() {
                Some(NodeEnum::String(s)) => Some(s.sval.as_str()),
                _ => None,
            })
            .collect();
        if parts.len() >= 2 {
            let schema = parts[0];
            if !self.cfg.permits_schema(schema) {
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
    ///   - **unqualified** `pg_shadow` / `pg_user` / `pg_authid` - the schema
    ///     is empty so the cross-schema walk sees nothing, but the `pg_`
    ///     prefix is reserved for system catalogs (a creator relation may not
    ///     use it), so an unqualified `pg_*` relation resolves to the catalog.
    fn check_system_catalog_relations(json: &Value, raw: &str) -> Result<(), GuardError> {
        let mut found = false;
        walk_range_vars(json, &mut |schema, relname| {
            let catalog_schema = is_neutral_catalog_schema(schema);
            let catalog_relname = has_pg_catalog_prefix(relname);
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
            if crate::guard::denylist::list_contains_ci(
                crate::guard::denylist::FILE_ACCESS_FUNCTIONS,
                name,
            ) {
                found = Some(rule::FILE_ACCESS_FUNCTION);
                return true;
            }
            if crate::guard::denylist::list_contains_ci(
                crate::guard::denylist::NETWORK_FUNCTIONS,
                name,
            ) {
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
    /// function on the `FILE_ACCESS_FUNCTIONS` / `NETWORK_FUNCTIONS` denylists.
    /// `'pg_read_file'::regprocedure` resolves the named function by OID at
    /// runtime - a dangerous capability the
    /// `FuncCall` walk misses because the function name is a bare string
    /// literal, not a call node. The literal may carry an argument signature
    /// (`'pg_read_file(text)'`); we match on the leading identifier.
    fn check_regproc_casts(json: &Value, raw: &str) -> Result<(), GuardError> {
        let mut found: Option<&'static str> = None;
        walk_regproc_casts(json, &mut |fname| {
            if crate::guard::denylist::list_contains_ci(
                crate::guard::denylist::FILE_ACCESS_FUNCTIONS,
                fname,
            ) {
                found = Some(rule::FILE_ACCESS_FUNCTION);
                return true;
            }
            if crate::guard::denylist::list_contains_ci(
                crate::guard::denylist::NETWORK_FUNCTIONS,
                fname,
            ) {
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
    /// string literal - invisible to [`foreign_schema_in_tree`], which only
    /// sees structural qualified-name nodes. Here we parse the literal's
    /// leading `schema.` qualifier and run it through the SAME cross-schema
    /// policy (own-schema OK; otherwise denied - these are concrete object
    /// targets, so no shared-schema exemption, same as
    /// [`SchemaSlot::Object`]).
    ///
    /// SCOPE: only LITERAL arguments in these specific positions. A plain data
    /// literal (`INSERT ... VALUES ('control.t')`) is untouched. A
    /// runtime-constructed argument (`nextval(some_var)`, `nextval('a'||'b')`)
    /// is NOT an `A_Const` and cannot be resolved at parse time - that is the
    /// line-2 (least-priv `migrator` role) defense's job, same limit the
    /// `format('%I', ...)` arm acknowledges.
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
            if self.cfg.permits_schema(&schema) {
                return false;
            }
            // Concrete object target - Object-slot policy: no shared-schema
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

    /// Deny `set_config('search_path'|'role'|'session_authorization', ...)`.
    ///
    /// `set_config(param, value, is_local)` is the function-call form of `SET
    /// param = value`. The structural [`NodeEnum::VariableSetStmt`] gate denies
    /// a `SET search_path`/`role`/`session_authorization`, but a `FuncCall`
    /// slips past it - so the call form is denied identically by matching the
    /// first string-literal argument against [`crate::guard::denylist::FORBIDDEN_SET_PARAMS`].
    /// A benign GUC (`statement_timeout`) stays allowed, mirroring the
    /// structural SET allowance. (Runtime-constructed param names are not
    /// literals and are out of parse-time scope - the line-2 role defense.)
    fn check_set_config_calls(json: &Value, raw: &str) -> Result<(), GuardError> {
        let mut denied_param = false;
        walk_set_config_calls(json, &mut |param| {
            if crate::guard::denylist::list_contains_ci(
                crate::guard::denylist::FORBIDDEN_SET_PARAMS,
                param,
            ) {
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
    /// The XML-emitting table functions ([`crate::guard::denylist::SQL_STRING_ARG_FUNCTIONS`])
    /// take a free-form SQL string as their first argument that the server then
    /// executes. The structural func-walk and cross-schema walk are blind to it
    /// (the SQL lives in an `A_Const` text literal, not a parse subtree). We
    /// extract each such literal and run the SAME guard recursively via
    /// [`Self::check_body_text`] - re-parse + recurse (catching cross-schema
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
                    let json = guard_stmt_json(raw_stmt, &inner_raw)?;
                    // Recurse with the inner statement's own text for accurate
                    // error reporting.
                    self.check_node(inner, &json, &inner_raw)?;
                }
            }
        }

        // (b) Re-parse embedded string literals (EXECUTE 'CREATE ROLE ...') -
        //     find single-quoted SQL fragments and re-check them.
        for literal in extract_string_literals(body) {
            if let Ok(parsed) = pg_query::parse(&literal) {
                for raw_stmt in &parsed.protobuf.stmts {
                    if let Some(inner) = raw_stmt.stmt.as_ref().and_then(|s| s.node.as_ref()) {
                        let json = guard_stmt_json(raw_stmt, &literal)?;
                        self.check_node(inner, &json, &literal)?;
                    }
                }
            }
        }

        // (c) Token-scan backstop - catch dangerous names a partial parse of a
        //     PL/pgSQL body would never surface as a FuncCall/Stmt node.
        let lower = body.to_ascii_lowercase();
        for &f in crate::guard::denylist::FILE_ACCESS_FUNCTIONS {
            if word_present(&lower, f) {
                return Err(denied(rule::BODY_INSPECTION, raw));
            }
        }
        for &f in crate::guard::denylist::NETWORK_FUNCTIONS {
            if word_present(&lower, f) {
                return Err(denied(rule::BODY_INSPECTION, raw));
            }
        }
        // search_path escape / alter system / role mgmt hidden in EXECUTE text.
        // Under Platform the role-management + search_path needles are
        // relaxed - 0025's bootstrap DO-block legitimately EXECUTEs
        // `CREATE ROLE ...` / `ALTER ROLE ... SET search_path ...` - but `ALTER
        // SYSTEM` and `SUPERUSER` STAY hard in BOTH profiles (neither has any
        // place in any migration). The recursion arm (a) has already admitted
        // the genuinely parsed CREATE ROLE / GRANT nodes under Platform; this
        // token scan is the lexical backstop for PL/pgSQL bodies that do not
        // parse as top-level SQL.
        if body_contains_superuser_role_escalation(&lower) {
            return Err(denied(rule::BODY_INSPECTION, raw));
        }
        // The role-management body needles are relaxed for a config holding the
        // `access.role` capability - an INTERNAL guard vendor-lower rule, not an
        // operator-authorable knob. Platform holds it and is relaxed; Confined does
        // not and is denied.
        let allow_role = self
            .cfg
            .grants_global_bool(policy_registry::KEY_ACCESS_ROLE);
        let needles: &[&str] = if allow_role {
            &["alter system"]
        } else {
            &["alter system", "create role", "create user", "drop role"]
        };
        for needle in needles {
            if lower.contains(needle) {
                return Err(denied(rule::BODY_INSPECTION, raw));
            }
        }
        // The privilege verbs the top level hard-denies but this backstop had no
        // needle for. `CREATE ROLE` hidden in EXECUTE text was caught while `GRANT`
        // in the same shape was not, which is an inconsistency rather than a
        // decision.
        //
        // Matched on identifier boundaries, so `grant` does not fire on a column
        // named `grant_total`. Relaxed under Platform alongside role management,
        // whose bootstrap legitimately GRANTs.
        //
        // A lexical scan is a backstop, not a boundary, and it is worth being precise
        // about what it cannot do. It only sees CONTIGUOUS text, so a body that
        // splits the verb across a concatenation defeats it: `'ALTER TABLE t OWNER '
        // || 'TO postgres'` never contains `owner to`. Catching that would need the
        // bare word `owner`, which collides with ordinary column names (`owner_app`
        // appears throughout this engine's own schema), so the false-positive cost
        // is worse than the hole. Runtime-constructed SQL is the migrator role's
        // problem, not the parser's.
        if !allow_role {
            for needle in ["grant", "revoke", "security definer", "default privileges"] {
                if word_present(&lower, needle) {
                    return Err(denied(rule::BODY_INSPECTION, raw));
                }
            }
        }
        if !allow_role && lower.contains("search_path") {
            return Err(denied(rule::BODY_INSPECTION, raw));
        }
        // COPY ... PROGRAM hidden in a body.
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
        if let Some(schema) = foreign_schema_in_body(body, &|s| self.cfg.permits_schema(s)) {
            return Err(GuardError::CrossSchema {
                schema,
                statement: raw.to_string(),
            });
        }
        // (e) Runtime-constructed SQL: a PL/pgSQL body that builds a
        //     schema-qualified name via `format('%I....', s)` never shows the
        //     target schema as a `schema.ident` adjacency - it's a bare
        //     string literal (`s := 'control'`) or a `format()` arg. Flag any
        //     bare literal that names a platform schema, and - when the body
        //     uses an `%I` identifier template - any bare-identifier literal
        //     that is not the project schema (reaching ANOTHER project's
        //     schema). Deny-by-default for the dynamic-SQL class.
        if let Some(schema) = foreign_schema_literal_in_body(body, &|s| self.cfg.permits_schema(s))
        {
            return Err(GuardError::CrossSchema {
                schema,
                statement: raw.to_string(),
            });
        }
        Ok(())
    }
}

/// The destructive-op gate, scoped to the walker that carries a real policy.
///
/// It reads the effective `safety.destructive_ops` posture, so it belongs where
/// that posture exists rather than on every `GuardDecisions` implementor. Keeping
/// it generic forced the trait to demand a posture from decision types that have
/// no policy to answer with, and the only answer available to them was a literal.
/// `GuardConfig` resolves it from the effective policy; nothing else has to
/// invent one.
impl GuardWalker<'_, GuardConfig> {
    fn check_sql_data_security_policy(
        &self,
        class: &StatementClass,
        raw: &str,
        advisories: &mut Vec<Advisory>,
    ) -> Result<(), GuardError> {
        // The destructive posture now rides in the effective policy (the
        // `safety.destructive_ops` grant); query it rather than the field.
        match self.cfg.effective_destructive_ops() {
            DestructiveOps::Forbid => match class.data_security {
                DataSecurityClass::NonDestructive => Ok(()),
                DataSecurityClass::Destructive(operation) => Err(GuardError::DataSecurityPolicy {
                    rule: data_security_rule::DESTRUCTIVE_OPS_FORBID,
                    statement: format!("{operation}: {raw}"),
                }),
                DataSecurityClass::Unknown => Err(GuardError::DataSecurityPolicy {
                    rule: data_security_rule::UNCLASSIFIED_OP_DENIED_UNDER_FORBID,
                    statement: format!(
                        "unclassified operation denied under destructive_ops=forbid: {raw}"
                    ),
                }),
            },
            DestructiveOps::Warn => {
                match class.data_security {
                    DataSecurityClass::NonDestructive => {}
                    DataSecurityClass::Destructive(operation) => {
                        advisories.push(Advisory::destructive_ops_warn(operation, raw));
                    }
                    DataSecurityClass::Unknown => {
                        advisories.push(Advisory::destructive_ops_unknown_warn(raw));
                    }
                }
                Ok(())
            }
            DestructiveOps::Allow => Ok(()),
        }
    }
}

/// Run the same raw-body deny-list scanner used for function bodies over a raw
/// view SELECT body. This is intentionally narrower than [`SqlGuard::check`]:
/// callers separately assert that the body parses as exactly one top-level
/// `SELECT`, then use this helper for the body reparse/string-literal/token
/// backstops (`pg_read_file`, network functions, COPY PROGRAM, dynamic
/// cross-schema text, etc.).
///
/// # Errors
/// [`GuardError`] when the body scanner finds a denied token or cross-schema
/// reference.
pub fn check_raw_view_body_text(
    body: &str,
    raw: &str,
    scope: Option<&SchemaScope>,
) -> Result<(), GuardError> {
    // A view body is exactly one top-level SELECT. Checking that HERE rather than
    // trusting the caller is what makes the function safe to call directly: the
    // adapter below answers "not injected" for every element, so the injected-shape
    // immutability rule cannot fire, and that answer is only sound for text which
    // cannot carry a rename in the first place.
    //
    // [`check_raw_view_body`] just below refuses the same shapes first, with a
    // structured defect the PostgreSQL vendor turns into user-facing diagnostics -
    // which statement index, which dialect, what to write instead. That path owns
    // those messages; this check owns only the precondition, so the two do not
    // compete. The engine reaches either one through PostgreSQL's answer via
    // `ValidationPolicy::raw_view_body_refusal`, which is what stops a MySQL or
    // SQLite body from being vetted against PG grammar.
    let parsed = pg_query::parse(body).map_err(|_| denied(rule::VIEW_BODY_NOT_A_SELECT, raw))?;
    let [only] = parsed.protobuf.stmts.as_slice() else {
        return Err(denied(rule::VIEW_BODY_NOT_A_SELECT, raw));
    };
    if !matches!(
        only.stmt.as_ref().and_then(|stmt| stmt.node.as_ref()),
        Some(NodeEnum::SelectStmt(_))
    ) {
        return Err(denied(rule::VIEW_BODY_NOT_A_SELECT, raw));
    }

    // This compatibility scanner evaluates the supplied validation scope directly.
    // It does not turn that scope into an EffectivePolicy or invent policy grants.
    let decisions = BodyScopeDecisions { scope };
    GuardWalker { cfg: &decisions }.check_body_text(body, raw)
}

/// Why PostgreSQL's parser refuses a raw view body, as a structured fact rather
/// than as prose.
///
/// Every variant is something only a PARSER can establish. That is precisely why
/// this shape exists: the engine's authoring validator holds the op index, the
/// target dialect and the error code, but it cannot derive any of these facts
/// without a parser, and it must not acquire one - reaching `pg_query` from
/// neutral core is what made a MySQL or SQLite raw view body get vetted against
/// PostgreSQL's grammar. So the fact is derived HERE, mapped to operator-facing
/// text by `zeroship-migrate-postgres` (the vendor owns its own wording), and wrapped
/// in the authoring envelope by the engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RawViewBodyDefect {
    /// PostgreSQL's parser rejected the body outright. Carries the parser's own
    /// message.
    Unparseable(String),
    /// The body carries a statement count other than exactly one (a
    /// semicolon-chained second statement, or none at all). Carries the count.
    NotExactlyOneStatement(usize),
    /// The single statement parsed, but is not a `SELECT` - DDL, DML, `COPY` and
    /// the utility statements all land here.
    NotASelect,
    /// The `SELECT` carries an `INTO` clause, which creates a table and is
    /// therefore not a read-only view body.
    SelectInto,
    /// The read-only body scanner denied a token (`pg_read_file`, `COPY PROGRAM`,
    /// a network function, a dynamic-SQL literal) or a cross-schema reference.
    BodyScanner(GuardError),
}

/// Vet a raw view body with PostgreSQL's parser: exactly one top-level statement,
/// that statement a `SELECT`, no `SELECT INTO`, then the read-only body deny-list
/// scan ([`check_raw_view_body_text`]).
///
/// This is the WHOLE of PostgreSQL's raw-view-body posture in one call, so that
/// the vendor seam has a single thing to delegate to and neutral core has nothing
/// left to parse. It performs exactly the checks the engine's
/// `validate_raw_view_body_sql` performs inline, in the same order, so the
/// PostgreSQL path is behaviour-identical.
///
/// # Errors
/// [`RawViewBodyDefect`] naming which of the four shape rules, or the body
/// scanner, refused the text.
pub fn check_raw_view_body(
    sql: &str,
    scope: Option<&SchemaScope>,
) -> Result<(), RawViewBodyDefect> {
    let parsed = pg_query::parse(sql).map_err(|e| RawViewBodyDefect::Unparseable(e.to_string()))?;
    if parsed.protobuf.stmts.len() != 1 {
        return Err(RawViewBodyDefect::NotExactlyOneStatement(
            parsed.protobuf.stmts.len(),
        ));
    }
    let stmt = parsed
        .protobuf
        .stmts
        .first()
        .and_then(|raw| raw.stmt.as_ref())
        .and_then(|stmt| stmt.node.as_ref());
    let Some(NodeEnum::SelectStmt(select)) = stmt else {
        return Err(RawViewBodyDefect::NotASelect);
    };
    if select.into_clause.is_some() {
        return Err(RawViewBodyDefect::SelectInto);
    }
    check_raw_view_body_text(sql, sql, scope).map_err(RawViewBodyDefect::BodyScanner)
}

/// Scan a PL/pgSQL body's **bare string literals** for a cross-tenant schema
/// name that `format('%I....', s)`-style dynamic SQL would interpolate.
///
/// The structural/`schema.ident` checks never see these - the schema is a
/// runtime value (`s := 'control'`) or a `format()` argument, not an adjacency.
/// Two postures, both deny-by-default for the dynamic-SQL class:
///   1. Any bare literal that *is* a platform schema (`control`/`auth`/
///      `billing`) - these have no legitimate use as data in a creator body.
///   2. If the body uses an `%I` identifier-format template (the tell of
///      dynamic schema/relation interpolation), any bare *identifier* literal
///      that is not the project schema - reaching another project's schema.
fn foreign_schema_literal_in_body(body: &str, permits: &dyn Fn(&str) -> bool) -> Option<String> {
    let uses_ident_template = body.to_ascii_lowercase().contains("%i");
    for literal in extract_string_literals(body) {
        let lit = literal.trim();
        // A schema the PDP admits (`grants(schema.cross_schema, lit)`) is never a
        // violation - the project schema(s) a confined/platform posture owns, plus
        // any operator-supplied schema.
        if permits(lit) {
            continue;
        }
        // (1) platform schema named directly. The `PLATFORM_SCHEMAS` lexical
        //     backstop fires for any schema in PLATFORM_SCHEMAS that the scope
        //     did NOT permit (port schemas `zero_migrate`/`public`
        //     are not in PLATFORM_SCHEMAS, so they already pass).
        if crate::guard::denylist::list_contains_ci(crate::guard::denylist::PLATFORM_SCHEMAS, lit) {
            return Some(lit.to_string());
        }
        // (2) bare identifier reaching another schema under an %I template.
        if uses_ident_template && is_bare_identifier(lit) && looks_like_schema_name(lit) {
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
/// prefix convention (`project_...`) - the multi-tenant schemas a body could
/// reach. A short data token like `'active'` does not match, avoiding
/// false-positives on legitimate seed data passed through `%I`-bearing bodies.
fn looks_like_schema_name(s: &str) -> bool {
    let l = s.to_ascii_lowercase();
    crate::guard::denylist::list_contains_ci(crate::guard::denylist::PLATFORM_SCHEMAS, &l)
        || l.starts_with("project_")
}

/// Scan a body string for a `<schema>.<object>` qualifier that names a known
/// **platform schema** (`control`/`auth`/`billing`) - the cross-tenant target
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
fn foreign_schema_in_body(body: &str, permits: &dyn Fn(&str) -> bool) -> Option<String> {
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
                // A PDP-admitted schema is never a violation (the project schema(s)
                // a confined/platform posture owns). The `PLATFORM_SCHEMAS` backstop
                // fires for any non-permitted schema in PLATFORM_SCHEMAS (the port
                // schemas are not in it).
                if !permits(schema)
                    && crate::guard::denylist::list_contains_ci(
                        crate::guard::denylist::PLATFORM_SCHEMAS,
                        schema,
                    )
                {
                    return Some(schema.to_string());
                }
            }
        }
        i += 1;
    }
    None
}

/// Derive the migration flags from a passing [`GuardReport`].
///
/// - `destructive` (data loss) => `requires_approval` (the gate must confirm;
///   AI never auto-applies destructive ops).
/// - any non-transactional statement (CONCURRENTLY, ALTER TYPE ADD VALUE,
///   VACUUM) => `transactional = false` (the two-phase apply path).
/// - a `RENAME COLUMN` / `RENAME TABLE` => `requires_approval` even though it is
///   NOT data-loss `destructive`: a rename is app-breaking /
///   backward-incompatible (it silently breaks every reader of the old name), so
///   it must be operator-confirmed, never auto-applied. (The declarative
///   expand-contract rename path does NOT emit a bare `RenameStmt` - it emits
///   ADD COLUMN + trigger + backfill + DROP via `ExpandContractAuthor` - so this
///   gate is scoped to a literal `RENAME` in a submitted `up`.)
/// - an `ALTER COLUMN ... SET NOT NULL` => `requires_approval`: it takes an
///   ACCESS EXCLUSIVE lock + a full-table validating scan and ABORTS if any
///   existing row is NULL - and the row-less shadow CANNOT catch that abort/lock,
///   so it is gated regardless of the (necessarily clean) dry-run.
///
/// `online` is an authoring-time facet (expand-contract sequencing), not
/// derivable from a single SQL blob, so it stays at its default here.
#[must_use]
pub fn flags_for(report: &GuardReport) -> MigrationFlags {
    let non_transactional = report.classes.iter().any(|c| c.non_transactional);
    // A bare RENAME COLUMN / RENAME TABLE is gated (requires_approval) even
    // though it is not data-loss-destructive: it is backward-incompatible.
    let has_rename = report
        .classes
        .iter()
        .any(|c| matches!(c.kind, DdlKind::RenameColumn | DdlKind::RenameTable));
    // SET NOT NULL is gated regardless of the dry-run: the row-less shadow
    // has no data, so it can never reproduce the populated-column abort / the
    // ACCESS EXCLUSIVE validating-scan lock a SET NOT NULL takes on a real table.
    let has_set_not_null = report
        .classes
        .iter()
        .any(|c| matches!(c.kind, DdlKind::SetNotNull));
    MigrationFlags {
        transactional: !non_transactional,
        destructive: report.destructive,
        online: false,
        requires_approval: report.destructive || has_rename || has_set_not_null,
        // No per-migration timeout derivable from a single SQL blob; the author
        // sets it explicitly when a long backfill/index needs a higher ceiling.
        timeout_ms: None,
        // Likewise the per-migration lock-acquisition budget (the maintenance-
        // window override) is authoring-time, never inferred from a SQL blob -
        // defaults to the SHORT executor-wide lock-safety default.
        lock_timeout_ms: None,
        // A guard-derived flag set is for one-shot SQL; the online expand/contract
        // phase is set by the ExpandContractAuthor, never inferred from SQL.
        phase: None,
        // Repeatable is an authoring-time facet (a stable-identity, replace-style
        // R__ migration), not derivable from a single SQL blob - defaults off.
        repeatable: false,
    }
}

// ---------------------------------------------------------------------------
// Deny-by-default allowlist predicates
// ---------------------------------------------------------------------------

/// The `ObjectType`s a creator migration may `DROP`. Anything else (role,
/// schema, extension, FDW, subscription, publication, ...) is denied-by-default.
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

/// The additional `AlterTableType` subtypes a **Platform** migration may use
/// beyond [`is_safe_alter_table_subtype`]: the four RLS toggles
/// (ENABLE / FORCE / NO FORCE / DISABLE ROW LEVEL SECURITY). Confined never
/// admits these.
fn is_platform_alter_table_subtype(subtype: i32) -> bool {
    use AlterTableType as A;
    [
        A::AtEnableRowSecurity,
        A::AtForceRowSecurity,
        A::AtNoForceRowSecurity,
        A::AtDisableRowSecurity,
    ]
    .iter()
    .any(|t| subtype == *t as i32)
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
        // safe and a cross-schema one (`... ATTACH PARTITION control.x`) is
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

/// True if a `CREATE ROLE` / `ALTER ROLE` options list grants the `SUPERUSER`
/// attribute. The attribute is a `DefElem` named `superuser`
/// with a boolean arg (`SUPERUSER` => true, `NOSUPERUSER` => false). Denied in
/// ALL profiles including Platform - superuser is host-reaching, not merely
/// in-DB privilege.
fn role_grants_superuser(options: &[protobuf::Node]) -> bool {
    options.iter().any(|opt| {
        matches!(opt.node.as_ref(), Some(NodeEnum::DefElem(d))
            if d.defname.eq_ignore_ascii_case("superuser")
                && def_elem_bool(d) == Some(true))
    })
}

fn grant_stmt_grants_privileged_role(stmt: &protobuf::GrantStmt) -> bool {
    stmt.is_grant && stmt.grantees.iter().any(node_names_privileged_role)
}

fn grant_role_stmt_grants_privileged_role(stmt: &protobuf::GrantRoleStmt) -> bool {
    stmt.is_grant
        && (stmt.granted_roles.iter().any(node_names_privileged_role)
            || stmt.grantee_roles.iter().any(node_names_privileged_role))
}

fn alter_default_privileges_grants_privileged_role(
    stmt: &protobuf::AlterDefaultPrivilegesStmt,
) -> bool {
    stmt.action
        .as_ref()
        .is_some_and(grant_stmt_grants_privileged_role)
}

fn node_names_privileged_role(node: &protobuf::Node) -> bool {
    match node.node.as_ref() {
        Some(NodeEnum::RoleSpec(role)) => role_spec_names_privileged_role(role),
        // GrantRoleStmt.granted_roles is represented as AccessPriv by the PG AST
        // because of the GRANT privilege-vs-role grammar ambiguity.
        Some(NodeEnum::AccessPriv(role)) => is_privileged_role_name(&role.priv_name),
        Some(NodeEnum::String(s)) => is_privileged_role_name(&s.sval),
        _ => false,
    }
}

fn role_spec_names_privileged_role(role: &protobuf::RoleSpec) -> bool {
    role.roletype == protobuf::RoleSpecType::RolespecCstring as i32
        && is_privileged_role_name(&role.rolename)
}

fn is_privileged_role_name(name: &str) -> bool {
    crate::guard::denylist::list_contains_ci(crate::guard::denylist::PRIVILEGED_ROLES, name)
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

/// A function `DefElem` of the form `SET <param> = ...` whose param is in
/// [`crate::guard::denylist::FORBIDDEN_SET_PARAMS`] (e.g. `SET search_path = control`). The
/// nested arg is a `VariableSetStmt` carrying the target param name.
fn def_elem_is_forbidden_set(d: &protobuf::DefElem) -> bool {
    if !d.defname.eq_ignore_ascii_case("set") {
        return false;
    }
    if let Some(NodeEnum::VariableSetStmt(v)) = d.arg.as_ref().and_then(|a| a.node.as_ref()) {
        return crate::guard::denylist::list_contains_ci(
            crate::guard::denylist::FORBIDDEN_SET_PARAMS,
            &v.name,
        );
    }
    false
}

/// Read a boolean-valued `DefElem` (the `security` option carries a `Boolean`
/// arg: `SECURITY DEFINER` -> true, `SECURITY INVOKER` -> false).
fn def_elem_bool(d: &protobuf::DefElem) -> Option<bool> {
    match d.arg.as_ref().and_then(|a| a.node.as_ref()) {
        Some(NodeEnum::Boolean(b)) => Some(b.boolval),
        Some(NodeEnum::Integer(i)) => Some(i.ival != 0),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Generic full-parse-tree JSON walkers
// ---------------------------------------------------------------------------

/// Walk the ENTIRE serialized parse tree and invoke `visit` with the trailing
/// name part of every `FuncCall` / `CallStmt` function name found anywhere
/// (column DEFAULT, CHECK, VALUES lists, RULE actions, sub-selects - every
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
///   - a `TypeCast` to any [`crate::guard::denylist::REG_TYPES`] member whose argument is a
///     string literal (`'control.users'::regclass`);
///   - a `FuncCall` to any [`crate::guard::denylist::NAME_RESOLVER_FUNCTIONS`] member whose
///     FIRST argument is a string literal (`nextval('control.s')`,
///     `pg_get_serial_sequence('control.t','id')`).
///
/// `visit(literal, is_namespace_resolver)` returns `true` to short-circuit.
/// `is_namespace_resolver` is `true` for `regnamespace` / `to_regnamespace`,
/// where the literal IS a bare schema name (no `schema.object` split); `false`
/// for object resolvers where the schema is the leading `schema.` qualifier.
/// Only literal (`A_Const`) arguments are inspected - runtime-constructed names
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
                // is the literal's leading `schema.` qualifier - an object
                // resolver, not a namespace one.
                if func_is_text_relation_name(call.get("funcname")) {
                    if let Some(lit) = first_arg_string_literal(call.get("args")) {
                        if visit(&lit, false) {
                            return true;
                        }
                    }
                }
                // Schema-export builtins (`schema_to_xml('control', ...)`) whose
                // first `text` arg is a bare SCHEMA NAME - a namespace resolver,
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
                // (`pg_get_object_address('table', '{control,t}', ...)`). That
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
    crate::guard::denylist::list_contains_ci(crate::guard::denylist::REG_TYPES, last)
        .then(|| last.clone())
}

/// `funcname`'s trailing (bare) name if it is a name-resolving builtin
/// (`pg_catalog.nextval` resolves the same builtin), else `None`.
fn name_resolver_func(funcname: Option<&Value>) -> Option<String> {
    let parts = qualified_list_parts(funcname)?;
    let last = parts.last()?;
    crate::guard::denylist::list_contains_ci(crate::guard::denylist::NAME_RESOLVER_FUNCTIONS, last)
        .then(|| last.clone())
}

/// Is `funcname`'s trailing (bare) name an object-address resolver whose schema
/// rides in an array literal? (`pg_catalog.pg_get_object_address` resolves the
/// same builtin.)
fn func_is_object_address(funcname: Option<&Value>) -> bool {
    let Some(parts) = qualified_list_parts(funcname) else {
        return false;
    };
    parts.last().is_some_and(|f| {
        crate::guard::denylist::list_contains_ci(
            crate::guard::denylist::OBJECT_ADDRESS_FUNCTIONS,
            f,
        )
    })
}

/// Is `funcname`'s trailing (bare) name a stat/predicate builtin whose first
/// `text` argument is a relation name? (`pg_catalog.pg_relation_size` resolves
/// the same builtin.)
fn func_is_text_relation_name(funcname: Option<&Value>) -> bool {
    let Some(parts) = qualified_list_parts(funcname) else {
        return false;
    };
    parts.last().is_some_and(|f| {
        crate::guard::denylist::list_contains_ci(
            crate::guard::denylist::TEXT_RELATION_NAME_FUNCTIONS,
            f,
        )
    })
}

/// Is `funcname`'s trailing (bare) name a schema-export builtin whose first
/// `text` argument is a bare schema name? (`pg_catalog.schema_to_xml` resolves
/// the same builtin.)
fn func_is_namespace_name(funcname: Option<&Value>) -> bool {
    let Some(parts) = qualified_list_parts(funcname) else {
        return false;
    };
    parts.last().is_some_and(|f| {
        crate::guard::denylist::list_contains_ci(
            crate::guard::denylist::NAMESPACE_NAME_FUNCTIONS,
            f,
        )
    })
}

/// The schema element of an object-address call's name array: the SECOND
/// argument is a Postgres array text literal `'{schema,object,...}'` whose first
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

/// First element of a Postgres array text literal (`{a,b}` -> `a`, `{"a b",c}`
/// -> `a b`). Best-effort: handles the unquoted and double-quoted element forms.
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

/// Walk the tree for `set_config(<param-literal>, ...)` calls, invoking `visit`
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
/// [`crate::guard::denylist::SQL_STRING_ARG_FUNCTIONS`]), invoking `visit` with the first
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
    parts.last().is_some_and(|f| {
        crate::guard::denylist::list_contains_ci(
            crate::guard::denylist::SQL_STRING_ARG_FUNCTIONS,
            f,
        )
    })
}

/// Is `funcname`'s trailing (bare) name `set_config`? (`pg_catalog.set_config`
/// resolves the same builtin.)
fn func_is_set_config(funcname: Option<&Value>) -> bool {
    let Some(parts) = qualified_list_parts(funcname) else {
        return false;
    };
    parts
        .last()
        .is_some_and(|f| f.eq_ignore_ascii_case(crate::guard::denylist::SET_CONFIG_FUNCTION))
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
/// Drops any argument signature (`schema.f(text)` -> schema part of `schema.f`)
/// before splitting, then returns the first dotted component. Honors a
/// double-quoted leading component (`"My Schema".t` -> `My Schema`); a leading
/// dot or empty schema yields `None`.
fn literal_schema_qualifier(lit: &str) -> Option<String> {
    // Strip a trailing argument signature (`f(int, text)`) - reg*procedure
    // literals may carry one; the schema is in the head before `(`.
    let head = lit.split('(').next().unwrap_or("").trim();
    if head.is_empty() {
        return None;
    }
    let (schema, rest) = split_first_qualifier(head);
    // A schema is present only when there is a trailing component after the
    // first dot (`control.t` -> `control`; bare `t` -> no schema).
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
/// and schema qualifier: `pg_read_file(text)` -> `pg_read_file`,
/// `pg_catalog.pg_read_file` -> `pg_read_file`.
fn regproc_leading_ident(lit: &str) -> &str {
    let head = lit.trim().split('(').next().unwrap_or("").trim();
    head.rsplit('.').next().unwrap_or(head).trim()
}

/// Walk the ENTIRE serialized parse tree for any explicit reference to a schema
/// other than `project_schema`, covering every slot a schema name can hide in.
/// The walker is **typed by slot** (not a key-string allowlist): each
/// schema-qualified-name-bearing node contributes its schema with the
/// [`SchemaSlot`] that fixes the per-slot exemption policy. The slots:
///   - [`SchemaSlot::RangeVar`] - `RangeVar.schemaname` (FROM/DML/ALTER/DROP/
///     partition/INHERIT relation targets). Catalog reads
///     (`pg_catalog.pg_authid`, `information_schema.tables`) are NOT exempt.
///   - [`SchemaSlot::Object`] - `newschema` (SET SCHEMA), `CreateSchemaStmt`
///     `schemaname`, `CommentStmt`/`DropStmt` object lists, index `opclass`,
///     `COLLATE` `collname`, sequence `OWNED BY` - concrete object targets.
///     No exemption beyond own-schema.
///   - [`SchemaSlot::TypeRef`] - `TypeName.names` (column/return/param/cast/
///     `OF` type). Builtins desugar to `pg_catalog.<t>`, so `pg_catalog` (+
///     catalog/`public`) stay exempt here; a foreign tenant type is flagged.
///   - [`SchemaSlot::FuncCall`] - `FuncCall`/trigger/CALL `funcname` *call*
///     qualifier. `pg_catalog`/`information_schema`/`public` calls are routine
///     and exempt; a tenant-schema call is flagged.
///
/// Function *definition* targets (`CreateFunctionStmt`/`AlterFunctionStmt`
/// funcname) are NOT walked here - they are a *creation target*, never a call
/// qualifier, so they are checked directly against the project schema in
/// [`GuardWalker::check_func_def_target`] with NO shared-schema exemption
/// (`public.evil`, `pg_catalog.evil`, `information_schema.evil` all denied).
///
/// Returns the first foreign schema found. `permits` is the PDP cross-schema
/// decision (`grants(schema.cross_schema, schema)`); the per-slot well-known-schema
/// exemptions ([`slot_exempts_schema`]) remain a FIXED guard hard-rule applied
/// AFTER the PDP admission, keyed off the reference-slot kind.
fn foreign_schema_in_tree(v: &Value, permits: &dyn Fn(&str) -> bool) -> Option<String> {
    let mut found: Option<String> = None;
    walk_schema_names(v, &mut |schema, slot| {
        if schema.is_empty() || permits(schema) {
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
/// where naming it is routine and benign - never as a broad whitelist.
fn slot_exempts_schema(slot: SchemaSlot, schema: &str) -> bool {
    match slot {
        // Nothing is exempt:
        //   - RangeVar/Object: concrete relation/object targets reach a real
        //     object outside the pinned schema (catalog reads denied here);
        //   - CreationTarget: planting/altering a type INTO a schema (CREATE
        //     TYPE ... AS ENUM/RANGE, ALTER TYPE ...) - mirrors a function
        //     *definition* target, so `public`/`pg_catalog`/`control` are all
        //     denied, same as any other tenant schema.
        SchemaSlot::RangeVar | SchemaSlot::Object | SchemaSlot::CreationTarget => false,
        // Built-in-bearing object sub-slots: index `opclass` and `COLLATE`
        // `collname` routinely name a `pg_catalog` builtin
        // (`pg_catalog.text_ops`, `pg_catalog."C"`). `pg_catalog` ONLY is
        // exempt here (not `public`, not other catalog schemas, not a tenant
        // schema - `control.myops` stays denied).
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

/// The server's own catalog/temp schemas - never a cross-tenant target. Used
/// only for the slots where naming them is benign (type refs, function calls);
/// table reads from them are caught via [`SchemaSlot::RangeVar`].
fn is_neutral_catalog_schema(schema: &str) -> bool {
    ["pg_catalog", "pg_temp", "pg_toast", "information_schema"]
        .iter()
        .any(|s| schema.eq_ignore_ascii_case(s))
}

/// Whether an identifier carries the `pg_` catalog prefix, which marks an
/// unqualified relation as resolving to the server's own catalog.
///
/// Compares raw bytes. Identifiers reach here verbatim from the parse tree, so a
/// `&str` slice at index 3 panics whenever the third byte lands inside a
/// multi-byte character: a three-character name whose last character is a
/// two-byte letter is four bytes long, and `[..3]` splits that letter.
fn has_pg_catalog_prefix(relname: &str) -> bool {
    relname
        .as_bytes()
        .get(..3)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(b"pg_"))
}

/// Which kind of parse-tree slot a candidate schema name came from. Drives the
/// per-slot exemption policy in [`slot_exempts_schema`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SchemaSlot {
    /// `RangeVar.schemaname` - a relation read/write/target. Catalog reads are
    /// NOT exempt here.
    RangeVar,
    /// A concrete object/schema target: `newschema`, `CreateSchemaStmt`
    /// `schemaname`, COMMENT/RENAME `object`, DROP `objects`, `ObjectWithArgs`
    /// `objname`, sequence `OWNED BY` / identity `SEQUENCE NAME`.
    Object,
    /// A creation/alter target carried in a `type_name` qualified-name *list*
    /// (`CreateEnumStmt`/`CreateRangeStmt`/`AlterEnumStmt`/`AlterTypeStmt`):
    /// planting/altering a type INTO a schema. Like a function-definition
    /// target, NO shared-schema exemption - `public.e`/`pg_catalog.e`/
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
    /// Function *definition* targets are NOT this slot - they are checked
    /// against the project schema directly in [`GuardWalker::check_func_def_target`]
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
            // `schemaname` AND a `relname` sibling - the relation slot.
            if map.contains_key("relname") {
                if let Some(Value::String(s)) = map.get("schemaname") {
                    if !s.is_empty() && visit(s, SchemaSlot::RangeVar) {
                        return true;
                    }
                }
            } else if let Some(Value::String(s)) = map.get("schemaname") {
                // `CreateSchemaStmt.schemaname` (no `relname` sibling) - a
                // concrete schema target.
                if !s.is_empty() && visit(s, SchemaSlot::Object) {
                    return true;
                }
            }
            // `AlterObjectSchemaStmt.newschema` (`... SET SCHEMA control`).
            if let Some(Value::String(s)) = map.get("newschema") {
                if !s.is_empty() && visit(s, SchemaSlot::Object) {
                    return true;
                }
            }
            // `TypeName.names` - column/return/param/cast/OF type reference.
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
            // whose schema lives under `names`, handled above - a non-array
            // `type_name` yields no parts here, so this is target-only.)
            if let Some(schema) = qualified_list_schema(map.get("type_name")) {
                if visit(&schema, SchemaSlot::CreationTarget) {
                    return true;
                }
            }
            // `CreateDomainStmt.domainname` - the schema-qualified creation
            // target of `CREATE DOMAIN <schema>.<name> AS ...`. Same confinement
            // class as the type-creation `type_name` target above: a Confined
            // migrator may not plant a domain into a foreign/system schema.
            if let Some(schema) = qualified_list_schema(map.get("domainname")) {
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
            // `objname` - a concrete object target ([schema, object]).
            for key in ["object", "objname"] {
                if let Some(schema) = qualified_list_schema(map.get(key)) {
                    if visit(&schema, SchemaSlot::Object) {
                        return true;
                    }
                }
            }
            // DROP/GRANT `objects` (PLURAL): a list whose *items* are each a
            // qualified-name node (`List`/`TypeName`/`ObjectWithArgs`). Walk
            // each item's own qualifier - flattening the outer array would
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
            //   - `owned_by`: `OWNED BY <schema>.<table>.<column>` - 2-part
            //     (`table.col`, no schema) or 3-part (`schema.table.col`);
            //     schema present only at 3 parts.
            //   - `sequence_name`: identity `... (SEQUENCE NAME <schema>.s)` -
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
                Some("schema") => {
                    // `CreateExtensionStmt ... WITH SCHEMA <name>` - a bare String DefElem
                    // arg. Confine the WITH SCHEMA target so the rendered-SQL guard
                    // (gate 2) independently scopes it, restoring gate-1/gate-2 parity
                    // with `createSchema`. Strictly tighter: anything passing
                    // gate 1 is already in scope, so this adds no false-positives.
                    if let Some(s) = map.get("arg").and_then(json_string_node) {
                        if !s.is_empty() && visit(&s, SchemaSlot::Object) {
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
///   - a bare array (`CreateTrigStmt.funcname`, `CallStmt...funcname`,
///     `TypeName.names`, `IndexElem.opclass`, `CollateClause.collname`):
///     `[{node:{String}}, ...]`
///   - a `List` node (`CommentStmt.object`): `{node:{List:{items:[...]}}}`
fn qualified_list_schema(v: Option<&Value>) -> Option<String> {
    let parts = qualified_list_parts(v)?;
    if parts.len() >= 2 {
        Some(parts[0].clone())
    } else {
        None
    }
}

/// The schema of an `OWNED BY` target. The list is `[table, col]` (no schema,
/// 2 parts) or `[schema, table, col]` (3 parts) - so a schema is present only
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
        Some(obj) => match obj
            .get("node")
            .and_then(|n| n.get("List"))
            .and_then(|l| l.get("items"))
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

/// Extract the inner string of a `{"node":{"String":{"sval":"..."}}}` value.
fn json_string_node(v: &Value) -> Option<String> {
    v.get("node")?
        .get("String")?
        .get("sval")?
        .as_str()
        .map(str::to_string)
}

fn guard_stmt_json<T: serde::Serialize>(raw_stmt: &T, raw: &str) -> Result<Value, GuardError> {
    serde_json::to_value(raw_stmt).map_err(|_| denied(rule::INTERNAL_GUARD_ERROR, raw))
}

/// Build a [`GuardError::Denied`].
fn denied(rule: &'static str, statement: &str) -> GuardError {
    GuardError::Denied {
        rule,
        statement: statement.to_string(),
    }
}

/// Build a [`GuardError::NamespacePolicy`] for a namespace-authority denial
/// (II.2.5/II.2.6). Distinct from [`denied`] (deny-list) so callers can match the
/// named error code on a separate variant.
fn namespace_denied(rule: &'static str, statement: &str) -> GuardError {
    GuardError::NamespacePolicy {
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

/// Extract the body string(s) of a CREATE FUNCTION (`AS $$...$$`). The `as`
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
///
/// Iterates real `char`s (not raw bytes): a `bytes[j] as char` cast truncates
/// every non-ASCII UTF-8 byte to a Latin-1 codepoint, corrupting any multi-byte
/// character inside a literal - which could split a dangerous token off from its
/// adjacent multi-byte char and let the backstop's word-scan miss it. The primary
/// defense is the `pg_query` parse + least-priv role; this is the body token-scan
/// backstop, so its extraction must be byte-faithful.
#[must_use]
pub fn extract_string_literals(body: &str) -> Vec<String> {
    let mut out = Vec::new();
    // (byte_offset, char) pairs so we can index forward over the source faithfully.
    let chars: Vec<(usize, char)> = body.char_indices().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i].1 == '\'' {
            let mut j = i + 1;
            let mut buf = String::new();
            while j < chars.len() {
                if chars[j].1 == '\'' {
                    if j + 1 < chars.len() && chars[j + 1].1 == '\'' {
                        buf.push('\'');
                        j += 2;
                        continue;
                    }
                    break;
                }
                buf.push(chars[j].1);
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

fn body_contains_superuser_role_escalation(lower: &str) -> bool {
    word_present(lower, "superuser")
        && (lower.contains("create role")
            || lower.contains("create user")
            || lower.contains("alter role")
            || lower.contains("alter user"))
}

/// Slice the original source for a statement using its byte offsets.
fn stmt_text(sql: &str, raw_stmt: &protobuf::RawStmt) -> String {
    let start = usize::try_from(raw_stmt.stmt_location)
        .unwrap_or(0)
        .min(sql.len());
    let len = usize::try_from(raw_stmt.stmt_len).unwrap_or(0);
    let end = if len == 0 {
        sql.len()
    } else {
        (start + len).min(sql.len())
    };
    sql.get(start..end).unwrap_or("").trim().to_string()
}

// ===========================================================================
// White-box tests for guard-crate-internal helpers (the string-literal
// extractor's UTF-8 faithfulness + the fail-closed statement-JSON serializer).
// These probe private fns (`word_present`, `guard_stmt_json`), so they MUST live
// in-crate. The behaviour-lock suite that drives the guard through the engine's
// lower pipeline lives in `zero-migrate/tests/policy_charter/guard_vendor_lower.rs`.
// ===========================================================================
#[cfg(test)]
mod white_box_tests {
    use super::*;

    /// The body token-scan backstop's literal extractor must preserve multi-byte
    /// UTF-8 verbatim: `bytes[j] as char` truncates every non-ASCII byte to a
    /// Latin-1 codepoint, which would corrupt the literal and could split a
    /// dangerous token off from its adjacent multi-byte char so the word-scan
    /// misses it. This pins faithful extraction.
    #[test]
    fn m3_extract_string_literals_preserves_multibyte_utf8() {
        let body = "EXECUTE 'café λ pg_read_file 名→ done'";
        let got = extract_string_literals(body);
        assert_eq!(
            got,
            vec!["café λ pg_read_file 名→ done".to_string()],
            "literal must be extracted byte-for-byte (no Latin-1 truncation)"
        );
        assert!(
            word_present(&got[0], "pg_read_file"),
            "pg_read_file must remain findable in the faithfully-extracted literal"
        );
        assert_eq!(
            extract_string_literals("'日本語'"),
            vec!["日本語".to_string()]
        );
    }

    #[test]
    fn guard_statement_json_serialization_error_fails_closed() {
        struct BadSerialize;

        impl serde::Serialize for BadSerialize {
            fn serialize<S>(&self, _serializer: S) -> Result<S::Ok, S::Error>
            where
                S: serde::Serializer,
            {
                Err(serde::ser::Error::custom("forced serialization failure"))
            }
        }

        assert!(
            serde_json::to_value(&BadSerialize).is_err(),
            "test precondition: BadSerialize must fail JSON serialization"
        );
        assert!(
            guard_stmt_json(&BadSerialize, "SELECT 1").is_err(),
            "guard must deny-by-default on statement JSON serialization failure"
        );
    }
}
