//! The SQL security guard — parse-time deny-list + cross-schema confinement.
//! **The security heart of the engine.**
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
use crate::analysis::classify::{classify, DataSecurityClass, DdlKind, ParseError, StatementClass};
use denylist::rule;
use serde_json::Value;
use zero_migrate_ir::capability::{OperatorCapability, VendorCapabilities};
use zero_migrate_ir::dialect::SqlDialect;
use zero_migrate_ir::ir::{MigrationIr, Op};
use zero_migrate_ir::migration::MigrationFlags;
use zero_migrate_ir::policy::DestructiveOps;
use zero_migrate_ir::policy::SchemaScope;
use zero_migrate_ir::policy_registry;
use zero_migrate_policy::{
    normalize_pg_identifier, EffectivePolicy, GrantRegion, KnobKey, KnobValue, ObjectName,
    ShapeElement,
};

/// Stable NAMESPACE-authority policy rule ids (II.2.5 / II.2.6). These are the
/// conservative-deny rules the policy redesign introduces on top of the deny-list:
/// raw-SQL create/DDL classification, per-op creation-gating, and injected-shape
/// immutability. Each fails closed with the design's named error code.
pub mod namespace_rule {
    /// II.2.5 — a raw create (`CREATE TABLE` / CTAS / `SELECT INTO` / `LIKE` /
    /// `PARTITION OF` / `CREATE TABLE AS EXECUTE` / `INHERITS`) targets an object an
    /// `inject` rule covers; injection cannot rewrite raw text, so only the
    /// structured DSL may create an injected table.
    pub const RAW_CREATE_IN_INJECT_SCOPE: &str = "RawCreateInInjectScope";
    /// II.2.6a — a create (`CREATE TABLE`, structured or classified-raw) is not
    /// covered by a `schema.create_table` grant (default-deny namespace anchor).
    pub const CREATE_TABLE_NOT_GRANTED: &str = "CreateTableNotGranted";
    /// II.2.6a — a `CREATE SCHEMA` (structured or classified-raw) is not covered by
    /// a `schema.create_schema` grant.
    pub const CREATE_SCHEMA_NOT_GRANTED: &str = "CreateSchemaNotGranted";
    /// II.2.5 — a raw rename / `SET SCHEMA` moves a table INTO an inject scope; the
    /// engine cannot re-inject over raw text, so the move is denied.
    pub const RAW_RENAME_INTO_INJECT_SCOPE: &str = "RawRenameIntoInjectScope";
    /// II.2.6a — a rename/move into a scope is not covered by a `schema.rename`
    /// grant at the target.
    pub const RENAME_INTO_NOT_GRANTED: &str = "RenameIntoNotGranted";
    /// II.2.5 — an unqualified object reference under a non-⊤ `sql.raw` grant is
    /// unattributable (no live search_path to resolve it) → ⊤-only → deny.
    pub const UNQUALIFIED_NAME_UNDER_SCOPED_RAW_SQL: &str = "UnqualifiedNameUnderScopedRawSql";
    /// II.2.5 — `SET search_path` (or equivalent) under a non-⊤ `sql.raw` grant
    /// mutates the very name-resolution context attribution depends on → refused.
    pub const SEARCH_PATH_UNDER_SCOPED_RAW_SQL: &str = "SearchPathUnderScopedRawSql";
    /// II.2.5 — an opaque-body construct (`CREATE FUNCTION`/`PROCEDURE`/`TRIGGER`/
    /// `DO`) under a non-⊤ `sql.raw` grant defeats statement-level attribution.
    pub const OPAQUE_BODY_UNDER_SCOPED_RAW_SQL: &str = "OpaqueBodyUnderScopedRawSql";
    /// II.2.5 — a raw statement the parser cannot classify into exactly one shape,
    /// or whose target is dynamic/unqualified, is unattributable under a non-⊤ grant.
    pub const UNATTRIBUTABLE_RAW_UNDER_SCOPED_RAW_SQL: &str = "UnattributableRawUnderScopedRawSql";
    /// II.2.6b — an `ALTER`/`DROP COLUMN`/`RENAME` touching a column the covering
    /// inject rule contributes, without an explicit `schema.alter_injected` grant.
    pub const INJECTED_SHAPE_IMMUTABLE: &str = "InjectedShapeImmutable";
    /// II.2.6b — an index-mutating op on an injected index.
    pub const INJECTED_INDEX_IMMUTABLE: &str = "InjectedIndexImmutable";
    /// II.2.6b — a PK-replacing/dropping op on a table whose PK a covering inject
    /// rule pins.
    pub const INJECTED_PRIMARY_KEY_IMMUTABLE: &str = "InjectedPrimaryKeyImmutable";
    /// II.2.6b (H3) — a rename-into where a name-matching element diverges
    /// structurally from the injected shape (type/nullability/default/key/PK-columns).
    pub const INJECTED_SHAPE_CONFORMANCE_MISMATCH: &str = "InjectedShapeConformanceMismatch";
}

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
/// `Platform` (the platform-internal posture) AND `Trusted` (the public
/// dbmate-like posture). A zero-sized capability token
/// owned by [`zero_migrate_ir::capability`].
///
/// [`GuardConfig::platform`] / [`GuardConfig::trusted`] and
/// [`crate::conn::ExecutorConfig::platform`] /
/// [`crate::conn::ExecutorConfig::trusted`] take a `&OperatorCapability`, so the
/// ability to produce `Platform`/`Trusted` is gated on holding a token minted by
/// an operator-side named seam. The token is generic across the two operator
/// profiles because both share the identical security model: the operator
/// running the binary holds it; no creator path can.
///
/// The engine-construction POSTURE that decides whether the static parse-time guard
/// belt runs at all. This is NOT a composable policy knob: "run without the deny-list
/// guard" is the single most dangerous switch, so it is a root/host-set posture on the
/// guard config — it can neither be granted, inherited, nor drafted by a creator.
///
/// - `Enforced` (the default) — the full belt runs: the deny-list, cross-schema
///   confinement, and body walks. Confined and Platform both run `Enforced`.
/// - `Off` — the public dbmate-like Trusted posture (Track A): the operator owns the
///   DB, so there is NO untrusted boundary and the whole belt is skipped (arbitrary
///   SQL applies as the connecting role). Raw islands embedded in structured IR still
///   run their deny-list backstop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuardMode {
    /// Run the full static parse-time guard belt (Confined / Platform).
    Enforced,
    /// Skip the belt entirely — the Trusted dbmate-like posture (host owns the DB).
    Off,
}

/// Per-guard configuration.
///
/// All fields are **private**: a `GuardConfig` is obtained ONLY through
/// [`GuardConfig::confined`] (the safe default anyone may construct),
/// [`GuardConfig::platform`], or [`GuardConfig::trusted`] (both require an
/// [`OperatorCapability`] token). This is what makes the trust invariant
/// true at the public boundary — an external crate cannot write a
/// `GuardConfig { trust: Platform, .. }` / `{ trust: Trusted, .. }` literal, and
/// in-crate operator code produces `Platform`/`Trusted` through named token-mint
/// seams.
#[derive(Debug, Clone)]
pub struct GuardConfig {
    /// PRIVATE (PHASE 4). The target SQL dialect this guard config is for.
    ///
    /// - `Postgres` (the default) — the `libpg_query` line-1 guard runs
    ///   ([`SqlGuard::check`] parses + deny-walks the SQL). Every pre-PHASE-4
    ///   call site keeps this dialect, byte-identical.
    /// - `Sqlite` — the **descriptor-diff-only** Confined path:
    ///   `libpg_query` cannot parse `SQLite`, so there is NO line-1 parse guard;
    ///   the line-2 defense is the `SqliteBackend`'s runtime authorizer. The
    ///   Confined `SQLite` path accepts ONLY descriptor-diff-generated DDL — an
    ///   untrusted RAW SQL string presented to [`SqlGuard::check`] is REFUSED
    ///   fail-closed (it must come from the descriptor emitter). `Platform` is a
    ///   PG-only posture → it fail-closes to `Confined` on `SQLite`
    ///   ([`GuardConfig::for_dialect`]).
    dialect: SqlDialect,
    /// PRIVATE. The unforgeable [`EffectivePolicy`] — the SINGLE source the guard's
    /// every composable decision queries. The capability gate asks `grants(key,
    /// object)` for the statement's builtin knob key; cross-schema confinement asks
    /// `grants(schema.cross_schema, schema)`; the data-security obligations read
    /// `obligations`/`grants` on the safety.* knobs. There is no separate posture/scope
    /// state for the composable knobs — the policy IS the posture.
    effective: EffectivePolicy,
    /// PRIVATE. The root/host-set [`GuardMode`] — whether the static parse-time belt
    /// runs. This is NOT a composable knob (it quarantines the "skip the guard" switch
    /// out of the policy registry); the belt-skip reads it, and the raw-island
    /// role-needle relaxation keys off it + the `access.role` grant internally.
    guard_mode: GuardMode,
}

impl GuardConfig {
    /// Construct a guard directly from one composed [`EffectivePolicy`] + dialect, at
    /// the default [`GuardMode::Enforced`] posture (the full belt runs). The effective
    /// policy is the SINGLE source for injection and every composable guard decision.
    #[must_use]
    pub fn from_policy(effective: EffectivePolicy, dialect: SqlDialect) -> Self {
        Self::from_policy_with_mode(effective, dialect, GuardMode::Enforced)
    }

    /// Construct a guard from a composed [`EffectivePolicy`] + dialect + an explicit
    /// root/host-set [`GuardMode`]. `GuardMode::Off` is the Trusted dbmate-like posture
    /// (belt skipped). The mode is NOT derivable from the policy — it is a posture the
    /// host sets, never a composable grant.
    #[must_use]
    pub fn from_policy_with_mode(
        effective: EffectivePolicy,
        dialect: SqlDialect,
        guard_mode: GuardMode,
    ) -> Self {
        Self {
            dialect,
            effective,
            guard_mode,
        }
    }

    /// Replace the composed policy while preserving this config's dialect and
    /// host-selected guard mode.
    #[must_use]
    pub fn with_effective_policy(mut self, effective: EffectivePolicy) -> Self {
        self.effective = effective;
        self
    }

    /// Construct a **Confined** guard whose PDP is a caller-composed
    /// [`EffectivePolicy`] (Phase 2 Step 2b). Unlike [`GuardConfig::confined`] — whose
    /// effective policy is derived from the fixed confined [`VendorCapabilities`] and
    /// therefore carries no `inject` rules — this takes an `EffectivePolicy` that may
    /// carry inject/validate rules + scoped creation grants, so the guard's raw-SQL
    /// classification (II.2.5: `RawCreateInInjectScope` / creation-gating) and
    /// injected-shape immutability (II.2.6b) can be exercised against a real inject
    /// scope. The `EffectivePolicy` is unforgeable (only `admit`/
    /// `restrict` produce one), so a caller cannot smuggle an escalated grant
    /// through it.
    #[must_use]
    pub fn confined_with_effective(
        _project_schema: impl Into<String>,
        effective: EffectivePolicy,
    ) -> Self {
        Self::from_policy(effective, SqlDialect::Postgres)
    }

    /// The ONLY constructor reachable from the submission ingress and every
    /// creator-path author. Always `Confined`, single-schema, empty extensions.
    /// Needs NO token — Confined is the safe default anyone may construct.
    #[must_use]
    pub fn confined(project_schema: impl Into<String>) -> Self {
        Self::from_policy(
            confined_effective_policy(&project_schema.into(), &[], false, DestructiveOps::Allow),
            // Default to the PG line-1 guard — byte-identical to before PHASE 4.
            SqlDialect::Postgres,
        )
    }

    /// PHASE 4 — the Confined **`SQLite`** config. Like
    /// [`GuardConfig::confined`] but for the `SQLite` dialect: there is NO
    /// `libpg_query` line-1 guard (it cannot parse `SQLite`); the line-2 defense is
    /// the `SqliteBackend`'s runtime authorizer, and authoring is
    /// descriptor-diff-only. [`SqlGuard::check`] on this config REFUSES an
    /// untrusted raw SQL string (fail-closed): the only legitimate `SQLite` DDL
    /// comes from the descriptor emitter, never a hand-written string. Needs no
    /// token — Confined is the safe default anyone may construct.
    #[must_use]
    pub fn confined_sqlite(project_schema: impl Into<String>) -> Self {
        Self::from_policy(
            confined_effective_policy(&project_schema.into(), &[], false, DestructiveOps::Allow),
            SqlDialect::Sqlite,
        )
    }

    /// Confined **`MySQL`** config. `MySQL` live apply accepts descriptor-generated
    /// DDL through the `MySQL` backend; raw SQL still has no `MySQL` parser/deny-walk
    /// and is refused by [`SqlGuard::check`] instead of being mis-vetted by
    /// `libpg_query`.
    #[must_use]
    pub fn confined_mysql(project_schema: impl Into<String>) -> Self {
        Self::from_policy(
            confined_effective_policy(&project_schema.into(), &[], false, DestructiveOps::Allow),
            SqlDialect::Mysql,
        )
    }

    /// PHASE 4 — fail-closed dialect selection. Returns the guard config
    /// appropriate for `dialect`, mapping the requested profile down where `SQLite`
    /// has no equivalent:
    ///
    /// - `Postgres` → `self` unchanged (every profile is valid on PG).
    /// - `Sqlite` → **always Confined `SQLite`**. `Platform` is a PG-only posture
    ///   (the widened multi-schema allowlist has no `SQLite` analog — `main` IS the
    ///   one app file), so a Platform config fail-closes to Confined `SQLite`; a
    ///   Confined or Trusted config likewise becomes Confined `SQLite` (Trusted's
    ///   relaxed authorizer is a separate `SqliteBackend` concern, not a guard
    ///   one). This is the design's "Platform → fail-closed Confined on `SQLite`".
    #[must_use]
    pub fn for_dialect(self, dialect: SqlDialect) -> Self {
        match dialect {
            SqlDialect::Postgres => self,
            SqlDialect::Sqlite => {
                // Preserve the project schema (the first owned schema) where we have
                // one; otherwise empty. A privileged PG allowlist is dropped — it has
                // no SQLite meaning. Fail closed to Confined SQLite.
                Self::confined_sqlite(self.first_owned_schema())
            }
            SqlDialect::Mysql => {
                // MySQL uses the descriptor-generated DDL guard. Drop any
                // privileged PG posture and keep only the first owned schema.
                Self::confined_mysql(self.first_owned_schema())
            }
        }
    }

    /// The first schema this config OWNS (the confined project schema, or the first
    /// entry of a platform allowlist), derived from the `schema.cross_schema` grant's
    /// literal schema includes. Empty when the grant is `⊤` / globbed / absent.
    fn first_owned_schema(&self) -> String {
        owned_schemas_from_effective(&self.effective)
            .into_iter()
            .next()
            .unwrap_or_default()
    }

    /// Confined-path builder: set the `CREATE EXTENSION` allowlist. The creator
    /// path legitimately carries a per-project extension allowlist (e.g. the
    /// declarative author at `author.rs`), and a non-empty allowlist is NOT a
    /// privilege escalation — `FORBIDDEN_EXTENSIONS` still override it and the
    /// trust posture stays `Confined`. Platform configs set their allowlist via
    /// [`GuardConfig::platform`] instead.
    #[must_use]
    pub fn with_extension_allowlist(self, extensions: Vec<String>) -> Self {
        // Recompose the effective policy, replacing the `code.extension` StrSet grant
        // with the new allowlist while preserving every other input read back from
        // the current policy (owned schemas, vendor caps, data-security posture).
        let inputs = PolicyInputs::from_effective(&self.effective);
        let dialect = self.dialect;
        Self::from_policy(inputs.with_extensions(extensions).compose(), dialect)
    }

    /// Tighten the guard with data-security policy knobs from a sealed profile.
    ///
    /// This builder is safe to expose because it can only add validation/denial
    /// obligations to whatever posture the caller can already construct.
    #[must_use]
    pub fn with_data_security(self, require_rls: bool, destructive_ops: DestructiveOps) -> Self {
        let inputs = PolicyInputs::from_effective(&self.effective);
        let dialect = self.dialect;
        Self::from_policy(
            inputs
                .with_data_security(require_rls, destructive_ops)
                .compose(),
            dialect,
        )
    }

    /// Platform profile. REQUIRES an [`OperatorCapability`] token, minted by
    /// operator-side named seams. The `_cap` arg is the in-crate enforcement. The
    /// platform posture grants the full vendor capability set, cross-schema over the
    /// `schemas` allowlist, and the the internal raw-island role relaxation (GuardMode::Enforced + access.role) relaxation.
    #[must_use]
    pub fn platform(
        _cap: &OperatorCapability,
        schemas: Vec<String>,
        extension_allowlist: Vec<String>,
    ) -> Self {
        // Platform is a PG-only posture (it fail-closes to Confined on SQLite via
        // `for_dialect`); the config itself is always PG.
        Self::from_policy(
            platform_effective_policy(&schemas, &extension_allowlist),
            SqlDialect::Postgres,
        )
    }

    /// Trusted profile — the public dbmate-like posture (Track A). REQUIRES an
    /// [`OperatorCapability`] token, EXACTLY like [`GuardConfig::platform`], so
    /// neither an external crate nor an in-crate creator-path module
    /// (`submit`/`engine`) can produce a Trusted guard. The deny-list, the
    /// cross-schema confinement, and the body walks are all SKIPPED by
    /// [`SqlGuard::check`] under Trusted (arbitrary SQL applies as the connecting
    /// role) — the belt-skip is the root/host-set [`GuardMode::Off`] this posture (and
    /// ONLY this posture) carries; the destructive/transactional/approval flags are
    /// still derived.
    #[must_use]
    pub fn trusted(_cap: &OperatorCapability) -> Self {
        Self::from_policy_with_mode(
            trusted_effective_policy(),
            SqlDialect::Postgres,
            GuardMode::Off,
        )
    }

    /// PHASE 4 — the target SQL dialect this guard config vets.
    #[must_use]
    pub(crate) const fn dialect(&self) -> SqlDialect {
        self.dialect
    }

    /// Whether this config skips the confined deny-list belt entirely (the Trusted
    /// dbmate-like posture) — the root/host-set [`GuardMode::Off`]. `pub`: the engine's
    /// profile behaviour-lock tests assert it across the crate boundary.
    #[must_use]
    pub fn skips_denylist_belt(&self) -> bool {
        matches!(self.guard_mode, GuardMode::Off)
    }

    /// The root/host-set [`GuardMode`] posture this config carries.
    #[must_use]
    pub fn guard_mode(&self) -> GuardMode {
        self.guard_mode
    }

    /// The schema-confinement scope this guard config enforces, for the
    /// validate-time cross-schema gate. Derived from the effective policy's
    /// `schema.cross_schema` grant:
    /// - a `⊤` (whole-universe) grant ⇒ `Unconfined` (the Trusted operator posture);
    /// - a finite set of owned schemas ⇒ `Single(s)` for one, `Allowlist([…])` for
    ///   several (Confined / Platform);
    /// - no owned schema (empty) ⇒ `Single("")` (the degenerate default).
    ///
    /// This is the SINGLE source of truth that maps the policy to the validator's
    /// confinement scope, so the parse-guard cross-schema denial (line 1) and the
    /// friendlier validate-time refusal agree on the permitted set.
    #[must_use]
    pub fn schema_scope(&self) -> Option<SchemaScope> {
        let key = KnobKey::parse(policy_registry::KEY_SCHEMA_CROSS_SCHEMA).ok()?;
        if matches!(self.effective.grant_region(&key), GrantRegion::Top) {
            return Some(SchemaScope::Unconfined);
        }
        let owned = owned_schemas_from_effective(&self.effective);
        // The operator (Platform) posture is a schema ALLOWLIST even for a single
        // owned schema — it grants the privileged vendor set (`access.role`); a Confined
        // posture (no privileged caps) with one owned schema is a `Single` pin.
        let is_operator_posture = self.grants_global_bool(policy_registry::KEY_ACCESS_ROLE);
        Some(match owned.as_slice() {
            [one] if !is_operator_posture => SchemaScope::Single(one.clone()),
            [] if !is_operator_posture => SchemaScope::Single(String::new()),
            _ => SchemaScope::Allowlist(owned),
        })
    }

    /// Data-security RLS requirement carried into the guard (the `safety.require_rls`
    /// obligation).
    #[must_use]
    pub fn require_rls(&self) -> bool {
        self.obligates_require_rls(&global_witness())
    }

    /// Data-security destructive-op posture carried into the guard (the
    /// `safety.destructive_ops` grant).
    #[must_use]
    pub fn destructive_ops(&self) -> DestructiveOps {
        self.effective_destructive_ops()
    }

    // ── PDP decision-query helpers (Phase 2 Step 2a) ───────────────────────────
    // The guard's capability + data-security gate asks these instead of reading a
    // raw `VendorCapabilities` bit / `require_rls` / `destructive_ops` field. All
    // scope resolution lives inside the `EffectivePolicy`; the guard passes a
    // concrete object and reads back a value.

    /// Does the effective policy GRANT the whole-DB (Global) capability `key` at
    /// `object`? A Global Bool grant resolves the same at every object; we pass a
    /// stable global witness. Reproduces `caps.grants(cap)`: absent grant ⇒ the
    /// knob default (`false`, deny).
    fn grants_global_bool(&self, key: &str) -> bool {
        self.grants_bool_at(key, &global_witness())
    }

    /// Does the effective policy grant Bool `key` at the concrete `object`? For a
    /// PerSchema/PerTable knob the object attributes the grant. Non-Bool / unknown
    /// keys fail closed to `false`.
    fn grants_bool_at(&self, key: &str, object: &ObjectName) -> bool {
        let Some(k) = KnobKey::parse(key).ok() else {
            return false;
        };
        matches!(
            self.effective.grants(&k, object),
            Some(KnobValue::Bool(true))
        )
    }

    /// Whether the effective policy admits a DROP object class beyond
    /// [`is_safe_drop_object`] (the `.down.sql`-only reverses: schema/extension/
    /// policy — DROP ROLE is handled by its own arm). Reproduces
    /// `platform_drop_object_allowed` via the PDP.
    fn grants_drop_object(&self, remove_type: i32) -> bool {
        if remove_type == ObjectType::ObjectSchema as i32 {
            return self.grants_global_bool(policy_registry::KEY_SCHEMA_CREATE_SCHEMA);
        }
        if remove_type == ObjectType::ObjectExtension as i32 {
            return self.grants_extension_capability();
        }
        if remove_type == ObjectType::ObjectPolicy as i32 {
            return self.grants_global_bool(policy_registry::KEY_ACCESS_POLICY);
        }
        if remove_type == ObjectType::ObjectRole as i32 {
            return self.grants_global_bool(policy_registry::KEY_ACCESS_ROLE);
        }
        false
    }

    /// Whether the effective policy holds the EXTENSION capability at all — i.e. the
    /// `code.extension` StrSet allowlist is non-empty. The allowlist IS the capability:
    /// empty = deny all (so no CREATE/DROP EXTENSION). `FORBIDDEN_EXTENSIONS` still
    /// overrides which specific names may be created.
    fn grants_extension_capability(&self) -> bool {
        !self.granted_extension_allowlist().is_empty()
    }

    /// The permitted `CREATE EXTENSION` names granted by the effective policy — the
    /// `code.extension` StrSet grant value at the global witness. Empty when no
    /// grant covers it (deny-by-default). `FORBIDDEN_EXTENSIONS` still overrides
    /// this in the guard regardless.
    fn granted_extension_allowlist(&self) -> Vec<String> {
        let Some(k) = KnobKey::parse(policy_registry::KEY_CODE_EXTENSION).ok() else {
            return Vec::new();
        };
        match self.effective.grants(&k, &global_witness()) {
            Some(KnobValue::StrSet(names)) => names,
            _ => Vec::new(),
        }
    }

    /// True iff the effective policy obligates RLS on `object` — the
    /// `safety.require_rls` Require obligation covers it. Reproduces the
    /// `self.require_rls` read, object-scoped.
    fn obligates_require_rls(&self, object: &ObjectName) -> bool {
        let Some(want) = KnobKey::parse(policy_registry::KEY_SAFETY_REQUIRE_RLS).ok() else {
            return false;
        };
        self.effective
            .obligations(object)
            .iter()
            .any(|(k, v)| *k == want && matches!(v, KnobValue::Bool(true)))
    }

    /// The effective destructive-op posture — the `safety.destructive_ops` OrderedEnum
    /// grant value at the global witness, mapped back onto [`DestructiveOps`]. The
    /// deny-by-default value is `forbid`; an absent grant resolves to the knob
    /// default (`forbid`). This drives the destructive gating.
    fn effective_destructive_ops(&self) -> DestructiveOps {
        let Some(k) = KnobKey::parse(policy_registry::KEY_SAFETY_DESTRUCTIVE_OPS).ok() else {
            return DestructiveOps::Forbid;
        };
        match self.effective.grants(&k, &global_witness()) {
            Some(KnobValue::Str(s)) => match s.as_str() {
                "allow" => DestructiveOps::Allow,
                "warn" => DestructiveOps::Warn,
                _ => DestructiveOps::Forbid,
            },
            _ => DestructiveOps::Forbid,
        }
    }

    // ── namespace-authority decision queries (Phase 2 Step 2b, II.2.5/II.2.6) ───

    /// The project schema an UNQUALIFIED relation resolves to under this config's
    /// cross-schema pin — the sole schema owned by the effective policy's
    /// `schema.cross_schema` grant. `None` when there is no unique owned schema (empty
    /// confined default, multi-schema platform, or a `⊤` grant) — an unqualified name
    /// is then not uniquely attributable by the guard.
    fn pinned_schema(&self) -> Option<String> {
        match owned_schemas_from_effective(&self.effective).as_slice() {
            [one] if !one.is_empty() => Some(one.clone()),
            _ => None,
        }
    }

    /// True iff the effective policy grants Bool `key` (default-deny) at the concrete
    /// normalized `object`. Fails closed (`false`) on an unknown key or non-Bool value.
    fn grants_namespace_bool(&self, key: &str, object: &ObjectName) -> bool {
        self.grants_bool_at(key, object)
    }

    /// The [`GrantRegion`] of `sql.raw` — the ⊤/Scoped/Ungranted posture the
    /// scoped-raw-SQL rules (II.2.5) turn on.
    fn raw_sql_region(&self) -> GrantRegion {
        match KnobKey::parse(policy_registry::KEY_SQL_RAW) {
            Ok(k) => self.effective.grant_region(&k),
            Err(_) => GrantRegion::Ungranted,
        }
    }

    /// Does ANY covering `inject` rule contribute to `object`? A raw create/rename
    /// into an inject scope is denied (injection can't rewrite raw text, II.2.5).
    fn injects_cover(&self, object: &ObjectName) -> bool {
        !self.effective.injects_for(object).is_empty()
    }

    /// Is `element` on `object` an injected shape element (II.2.6b, name-match-at-op-time)?
    fn is_injected_shape(&self, object: &ObjectName, element: &ShapeElement) -> bool {
        self.effective.is_injected_shape(object, element)
    }

    /// Cross-schema confinement, decided directly on the PDP: is a reference to
    /// `schema` admitted? True iff the effective policy grants `schema.cross_schema`
    /// at the (PG-normalized) schema object — the project schema(s) a confined/
    /// platform posture owns are granted; every other schema is a `CrossSchema`
    /// violation (default-deny). An un-normalizable schema name (empty / malformed)
    /// is NOT admitted (fail-closed). This replaces the derived-`SchemaScope`
    /// `permits(schema)` read.
    fn grants_cross_schema(&self, schema: &str) -> bool {
        let Some(k) = KnobKey::parse(policy_registry::KEY_SCHEMA_CROSS_SCHEMA).ok() else {
            return false;
        };
        let Some(object) = normalize_pg_identifier(schema) else {
            return false;
        };
        matches!(
            self.effective.grants(&k, &object),
            Some(KnobValue::Bool(true))
        )
    }
}

impl Default for GuardConfig {
    /// Confined, empty single-schema, empty extensions, PG dialect — today's
    /// behaviour.
    fn default() -> Self {
        Self::confined(String::new())
    }
}

/// A stable concrete object for a Global-model capability query. Every Global grant
/// (`⊤`-scope or absent) resolves the same at every object, so any witness decides
/// it; `zsg` is an arbitrary fixed schema that never collides with a real target
/// (the value is irrelevant for a ⊤-scope / default grant).
fn global_witness() -> ObjectName {
    ObjectName::schema(b"zsg".to_vec())
}

fn grant_bool_from_effective(effective: &EffectivePolicy, key: &str, object: &ObjectName) -> bool {
    let Some(k) = KnobKey::parse(key).ok() else {
        return false;
    };
    matches!(effective.grants(&k, object), Some(KnobValue::Bool(true)))
}

fn grant_str_set_from_effective(effective: &EffectivePolicy, key: &str) -> Vec<String> {
    let Some(k) = KnobKey::parse(key).ok() else {
        return Vec::new();
    };
    match effective.grants(&k, &global_witness()) {
        Some(KnobValue::StrSet(names)) => names,
        _ => Vec::new(),
    }
}

/// The literal schemas an effective policy OWNS — the `schema.cross_schema` grant's
/// literal schema includes (the project schema(s) a confined/platform posture
/// carries). Empty for a `⊤` / globbed / absent grant.
fn owned_schemas_from_effective(effective: &EffectivePolicy) -> Vec<String> {
    let Some(k) = KnobKey::parse(policy_registry::KEY_SCHEMA_CROSS_SCHEMA).ok() else {
        return Vec::new();
    };
    effective
        .grant_literal_schema_includes(&k)
        .unwrap_or_default()
}

fn destructive_ops_from_effective(effective: &EffectivePolicy) -> DestructiveOps {
    let Some(k) = KnobKey::parse(policy_registry::KEY_SAFETY_DESTRUCTIVE_OPS).ok() else {
        return DestructiveOps::Forbid;
    };
    match effective.grants(&k, &global_witness()) {
        Some(KnobValue::Str(s)) => match s.as_str() {
            "allow" => DestructiveOps::Allow,
            "warn" => DestructiveOps::Warn,
            _ => DestructiveOps::Forbid,
        },
        _ => DestructiveOps::Forbid,
    }
}

fn require_rls_from_effective(effective: &EffectivePolicy) -> bool {
    let Some(want) = KnobKey::parse(policy_registry::KEY_SAFETY_REQUIRE_RLS).ok() else {
        return false;
    };
    effective
        .obligations(&global_witness())
        .iter()
        .any(|(k, v)| *k == want && matches!(v, KnobValue::Bool(true)))
}

/// The composition inputs behind a guard's [`EffectivePolicy`]. This is the single
/// place the guard's posture is expressed as PDP grants/obligations — every named
/// constructor and every tightening builder routes through here, and
/// [`PolicyInputs::from_effective`] reads an existing policy back into inputs so a
/// builder (`with_extension_allowlist`/`with_data_security`) can recompose without a
/// separate posture-state cache.
///
/// The knobs it composes:
/// - each held vendor capability (Global Bool) at `⊤`;
/// - `schema.cross_schema` + `schema.create_table` + `schema.rename` scoped to the
///   OWNED schemas (`include: [...]`, or `all` when there is no owned schema);
/// - the `code.extension` StrSet allowlist;
/// - the data-security `safety.require_rls` obligation + `safety.destructive_ops` grant.
///
/// The two former operator-posture toggles are NOT here: the belt-skip is the
/// root/host-set [`GuardMode`] (not a composable grant), and the raw-island role-needle
/// relaxation is an INTERNAL guard vendor-lower rule keyed off the posture.
struct PolicyInputs {
    caps: VendorCapabilities,
    /// The schemas this posture OWNS (`schema.cross_schema` + creation grants scoped
    /// here). Empty ⇒ a `⊤` grant (the degenerate default / Trusted).
    owned_schemas: Vec<String>,
    extension_allowlist: Vec<String>,
    require_rls: bool,
    destructive_ops: DestructiveOps,
}

impl PolicyInputs {
    /// A confined posture over `project_schema` (empty ⇒ the degenerate `⊤` default):
    /// no vendor caps, cross-schema/creation scoped to the project schema, no
    /// operator relaxations.
    fn confined(project_schema: &str, extensions: &[String]) -> Self {
        let owned_schemas = if project_schema.is_empty() {
            Vec::new()
        } else {
            vec![project_schema.to_string()]
        };
        Self {
            caps: VendorCapabilities::confined(),
            owned_schemas,
            extension_allowlist: extensions.to_vec(),
            require_rls: false,
            destructive_ops: DestructiveOps::Allow,
        }
    }

    /// The platform posture: the full vendor capability set, cross-schema/creation
    /// over the `schemas` allowlist, and the extension allowlist. The raw-island
    /// role-needle relaxation is NOT a grant here — it is the guard's internal
    /// vendor-lower rule (`GuardMode::Enforced` + `access.role`), which the platform
    /// posture satisfies by holding `access.role` under `Enforced`.
    fn platform(schemas: &[String], extensions: &[String]) -> Self {
        Self {
            caps: VendorCapabilities::operator(),
            owned_schemas: schemas.to_vec(),
            extension_allowlist: extensions.to_vec(),
            require_rls: false,
            destructive_ops: DestructiveOps::Allow,
        }
    }

    /// The trusted (dbmate-like) posture: every vendor capability, cross-schema at
    /// `⊤` (no confinement). The belt-skip is the root/host-set [`GuardMode::Off`] the
    /// [`GuardConfig::trusted`] constructor stamps — NOT a grant — so the scoped grants
    /// are never consulted. The raw-island body backstop still denies role/search_path
    /// needles under Trusted (its internal rule relaxes ONLY under `GuardMode::Enforced`
    /// + `access.role`, and Trusted is `Off`), a behaviour the vendor-lower matrix locks.
    fn trusted() -> Self {
        Self {
            caps: VendorCapabilities::operator(),
            owned_schemas: Vec::new(),
            extension_allowlist: Vec::new(),
            require_rls: false,
            destructive_ops: DestructiveOps::Allow,
        }
    }

    /// Read the composition inputs back out of an already-composed policy, so a
    /// tightening builder can recompose. Every field is a PDP read; there is no
    /// separate posture cache.
    fn from_effective(effective: &EffectivePolicy) -> Self {
        let w = global_witness();
        let mut caps = VendorCapabilities::confined();
        caps.allow_role =
            grant_bool_from_effective(effective, policy_registry::KEY_ACCESS_ROLE, &w);
        caps.allow_grant =
            grant_bool_from_effective(effective, policy_registry::KEY_ACCESS_GRANT, &w);
        caps.allow_extension =
            !grant_str_set_from_effective(effective, policy_registry::KEY_CODE_EXTENSION)
                .is_empty();
        caps.allow_schema =
            grant_bool_from_effective(effective, policy_registry::KEY_SCHEMA_CREATE_SCHEMA, &w);
        caps.allow_policy =
            grant_bool_from_effective(effective, policy_registry::KEY_ACCESS_POLICY, &w);
        caps.allow_rls = grant_bool_from_effective(effective, policy_registry::KEY_ACCESS_RLS, &w);
        caps.allow_partition =
            grant_bool_from_effective(effective, policy_registry::KEY_SCHEMA_PARTITION, &w);
        caps.allow_function =
            grant_bool_from_effective(effective, policy_registry::KEY_CODE_FUNCTION, &w);
        caps.allow_raw_sql = grant_bool_from_effective(effective, policy_registry::KEY_SQL_RAW, &w);
        caps.allow_raw_view_body =
            grant_bool_from_effective(effective, policy_registry::KEY_SQL_RAW_VIEW_BODY, &w);
        caps.allow_materialized_view =
            grant_bool_from_effective(effective, policy_registry::KEY_CODE_MATERIALIZED_VIEW, &w);
        let owned_schemas = owned_schemas_from_effective(effective);
        caps.allow_cross_schema =
            grant_bool_from_effective(effective, policy_registry::KEY_SCHEMA_CROSS_SCHEMA, &w);
        caps.schemas = owned_schemas.clone();
        Self {
            caps,
            owned_schemas,
            extension_allowlist: grant_str_set_from_effective(
                effective,
                policy_registry::KEY_CODE_EXTENSION,
            ),
            require_rls: require_rls_from_effective(effective),
            destructive_ops: destructive_ops_from_effective(effective),
        }
    }

    fn with_extensions(mut self, extensions: Vec<String>) -> Self {
        self.extension_allowlist = extensions;
        self
    }

    fn with_owned_schemas(mut self, owned_schemas: Vec<String>) -> Self {
        self.caps.schemas = owned_schemas.clone();
        self.owned_schemas = owned_schemas;
        self
    }

    fn with_data_security(mut self, require_rls: bool, destructive_ops: DestructiveOps) -> Self {
        self.require_rls = require_rls;
        self.destructive_ops = destructive_ops;
        self
    }

    /// Compose these inputs into the unforgeable [`EffectivePolicy`]. Total: any
    /// failure in the fixed-literal build fails closed to [`EffectivePolicy::deny_all`].
    fn compose(&self) -> EffectivePolicy {
        let registry = policy_registry::builtin_registry();

        // ⊤-scope grant/require rules, plus the schema-scoped cross-schema/creation
        // grants, as a JSON policy document (JSON so names/values escape safely).
        let mut grant_rules: Vec<serde_json::Value> = Vec::new();
        let mut require_rules: Vec<serde_json::Value> = Vec::new();

        let mut grant_bool = |key: &str, on: bool| {
            if on {
                grant_rules.push(serde_json::json!({ "key": key, "value": true, "scope": "all" }));
            }
        };

        // ── vendor capabilities → their builtin knob keys ──────────────────────
        grant_bool(policy_registry::KEY_ACCESS_ROLE, self.caps.allow_role);
        grant_bool(policy_registry::KEY_ACCESS_GRANT, self.caps.allow_grant);
        grant_bool(
            policy_registry::KEY_SCHEMA_CREATE_SCHEMA,
            self.caps.allow_schema,
        );
        grant_bool(policy_registry::KEY_ACCESS_POLICY, self.caps.allow_policy);
        grant_bool(policy_registry::KEY_ACCESS_RLS, self.caps.allow_rls);
        grant_bool(
            policy_registry::KEY_SCHEMA_PARTITION,
            self.caps.allow_partition,
        );
        grant_bool(policy_registry::KEY_CODE_FUNCTION, self.caps.allow_function);
        grant_bool(policy_registry::KEY_SQL_RAW, self.caps.allow_raw_sql);
        grant_bool(
            policy_registry::KEY_SQL_RAW_VIEW_BODY,
            self.caps.allow_raw_view_body,
        );
        grant_bool(
            policy_registry::KEY_CODE_MATERIALIZED_VIEW,
            self.caps.allow_materialized_view,
        );

        // ── cross-schema + namespace-authority creation-gating (II.2.6a) ───────
        // `schema.cross_schema` (the confinement) + `schema.create_table`/`schema.rename`
        // (creation-gating) are all scoped to the OWNED schemas: a confined migration
        // may reference/create/rename in its own project schema(s) but nowhere else.
        // An empty owned set grants `all` (the degenerate default / Trusted — which
        // skips the belt anyway, so the grant is never consulted).
        {
            let scope = if self.owned_schemas.is_empty() {
                serde_json::json!("all")
            } else {
                serde_json::json!({ "include": self.owned_schemas })
            };
            for key in [
                policy_registry::KEY_SCHEMA_CROSS_SCHEMA,
                policy_registry::KEY_SCHEMA_CREATE_TABLE,
                policy_registry::KEY_SCHEMA_RENAME,
            ] {
                grant_rules.push(serde_json::json!({
                    "key": key,
                    "value": true,
                    "scope": scope,
                }));
            }
        }

        // ── CREATE EXTENSION name allowlist (StrSet) — the allowlist IS the knob ─
        if !self.extension_allowlist.is_empty() {
            grant_rules.push(serde_json::json!({
                "key": policy_registry::KEY_CODE_EXTENSION,
                "value": self.extension_allowlist,
                "scope": "all",
            }));
        }

        // ── data-security posture ──────────────────────────────────────────────
        if self.require_rls {
            require_rules.push(serde_json::json!({
                "key": policy_registry::KEY_SAFETY_REQUIRE_RLS,
                "value": true,
                "scope": "all",
            }));
        }
        let destructive_variant = match self.destructive_ops {
            DestructiveOps::Allow => Some("allow"),
            DestructiveOps::Warn => Some("warn"),
            // Forbid is the deny default; it needs no explicit grant.
            DestructiveOps::Forbid => None,
        };
        if let Some(variant) = destructive_variant {
            grant_rules.push(serde_json::json!({
                "key": policy_registry::KEY_SAFETY_DESTRUCTIVE_OPS,
                "value": variant,
                "scope": "all",
            }));
        }

        // No rules ⇒ the deny-all floor (confined). Avoids composing an empty ceiling.
        if grant_rules.is_empty() && require_rules.is_empty() {
            return EffectivePolicy::deny_all(&registry);
        }

        // GRANTS compose DOWN (draft is the tightest layer, bounded by the ceiling);
        // REQUIRES compose UP (ceiling ∪ draft). So the capability grants live in the
        // DRAFT (`draft ⊑ ceiling` holds by equality), and the obligation lives in
        // the ceiling only. Any failure fails closed to deny_all.
        let ceiling_doc = serde_json::json!({
            "policy_version": 1,
            "grant": grant_rules,
            "require": require_rules,
        });
        let draft_doc = serde_json::json!({
            "policy_version": 1,
            "grant": grant_rules,
        });

        let composed =
            zero_migrate_policy::RootCeiling::parse_json(&ceiling_doc.to_string(), &registry)
                .ok()
                .and_then(|ceiling| {
                    let draft = zero_migrate_policy::PolicyDoc::parse_json(
                        &draft_doc.to_string(),
                        &registry,
                        zero_migrate_policy::LoadContext::NonRootLayer,
                    )
                    .ok()?;
                    zero_migrate_policy::admit(&ceiling, &draft, &registry).ok()
                });
        composed.unwrap_or_else(|| EffectivePolicy::deny_all(&registry))
    }
}

/// A confined effective policy over `project_schema`, folding the data-security
/// posture. This is the sole confined-constructor composition helper.
fn confined_effective_policy(
    project_schema: &str,
    extensions: &[String],
    require_rls: bool,
    destructive_ops: DestructiveOps,
) -> EffectivePolicy {
    PolicyInputs::confined(project_schema, extensions)
        .with_data_security(require_rls, destructive_ops)
        .compose()
}

/// The platform posture as a composed effective policy.
fn platform_effective_policy(schemas: &[String], extensions: &[String]) -> EffectivePolicy {
    PolicyInputs::platform(schemas, extensions).compose()
}

/// The trusted (dbmate-like) posture as a composed effective policy.
fn trusted_effective_policy() -> EffectivePolicy {
    PolicyInputs::trusted().compose()
}

/// T8 — the EXTERNAL trust boundary, pinned as `compile_fail` doctests. A doctest
/// is compiled as a SEPARATE crate that `use`s `zero_migrate_guard`, so it
/// exercises exactly the boundary an external consumer of this crate sits behind.
///
/// (1) An external crate cannot write a `GuardConfig { .. }` struct literal — the
/// fields (`dialect`, `effective`) are private, so a privileged profile can never
/// be forged by a literal (the `EffectivePolicy` is itself unforgeable). This MUST
/// fail to compile:
///
/// ```compile_fail
/// use zero_migrate_guard::guard::GuardConfig;
/// let _ = GuardConfig {
///     dialect: zero_migrate_ir::dialect::SqlDialect::Postgres,
///     effective: unimplemented!(),
/// };
/// ```
///
/// (2) The only unforgeable input to a privileged `GuardConfig` is a composed
/// `EffectivePolicy` — an external crate cannot construct one by a literal (its
/// fields are private and it has no public constructor other than `deny_all`). This
/// MUST fail to compile:
///
/// ```compile_fail
/// let _ = zero_migrate_policy::EffectivePolicy {
///     registry: unimplemented!(),
///     grants: unimplemented!(),
///     requires: vec![],
///     injects: vec![],
///     validates: vec![],
/// };
/// ```
///
/// (3) The privileged `GuardConfig::platform` / `trusted` constructors take an
/// [`OperatorCapability`] TOKEN. The token type is nameable (it lives in the
/// `zero-migrate-ir` leaf), but it has a PRIVATE field, so an external crate
/// cannot forge one by a struct literal. This MUST fail to compile:
///
/// ```compile_fail
/// use zero_migrate_ir::capability::OperatorCapability;
/// let _forged = OperatorCapability(());
/// ```
///
/// Minting the token via [`OperatorCapability::new`] is, by design (2026-06-30
/// engine-server model), a privileged operation reserved for the engine runner —
/// trust follows "who runs the engine + which config it injects," not token
/// un-nameability. The one external boundary crate in this workspace
/// (`zero-migrate-node`) only ever calls the safe Confined constructor.
///
/// For contrast, the safe Confined constructor IS reachable externally:
///
/// ```
/// use zero_migrate_guard::guard::GuardConfig;
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
    /// A NAMESPACE-authority rule (II.2.5 raw-SQL classification / II.2.6
    /// creation-gating + injected-shape immutability) denied the migration. Each is
    /// a conservative-deny with the design's named error code (see [`namespace_rule`]).
    #[error("namespace policy '{rule}' denied: {statement}")]
    NamespacePolicy {
        /// The stable namespace rule id (see [`namespace_rule`]).
        rule: &'static str,
        /// The offending statement text.
        statement: String,
    },
    /// The SQL could not be parsed (deny-by-default: it never reaches the DB).
    #[error("parse error: {0}")]
    Parse(#[from] ParseError),
    /// PHASE 4 — a raw SQL string was presented to [`SqlGuard::check`] on the
    /// Confined **`SQLite`** path, which accepts ONLY descriptor-diff-generated DDL.
    /// `libpg_query` cannot vet `SQLite`, so there is no line-1
    /// parse guard for raw `SQLite` SQL; the only safe `SQLite` DDL comes from the
    /// engine's descriptor emitter (validated at the author boundary, line-2
    /// enforced by the `SqliteBackend` authorizer). A hand-written / untrusted
    /// `SQLite` SQL string is therefore refused fail-closed.
    #[error(
        "raw SQL is not accepted on the Confined SQLite path: SQLite migrations \
         must be descriptor-diff-generated (libpg_query cannot vet SQLite; the \
         SqliteBackend authorizer is the line-2 defense)"
    )]
    SqliteRawSqlRejected,
    /// A raw SQL string was presented to the Postgres guard on the `MySQL` path.
    /// `MySQL` has no parser/deny-walk in this crate, so raw SQL is refused
    /// fail-closed instead of being mis-vetted by `libpg_query`.
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

    /// Backstop for the two IR raw islands (`pgRaw` and `createFunction.body`)
    /// under the Trusted operator profile. Trusted still skips project-schema
    /// confinement for general SQL files, but arbitrary SQL strings embedded inside
    /// otherwise structured IR must not bypass the deny-list for host-reaching or
    /// privilege-escalating constructs.
    ///
    /// # Errors
    /// [`GuardError`] when parsing fails or a deny-listed construct is found.
    pub fn check_raw_island_sql_backstop(&self, sql: &str) -> Result<(), GuardError> {
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
    pub fn check_raw_island_body_backstop(&self, body: &str, raw: &str) -> Result<(), GuardError> {
        self.check_body_text(body, raw)
    }

    /// Check one top-level statement node (and everything nested under it).
    ///
    /// `json` is the `serde_json` serialization of the statement's `RawStmt`
    /// subtree — used by the generic full-tree walks (Root Cause 2 fix) so we
    /// visit EVERY node, including the slots `pg_query::nodes()` skips (column
    /// DEFAULT, CHECK, VALUES lists, RULE actions, SET SCHEMA targets, …).
    fn check_node(&self, node: &NodeEnum, json: &Value, raw: &str) -> Result<(), GuardError> {
        // 0. Scoped-raw-SQL refusals (II.2.5) run FIRST, but ONLY under a Scoped
        //    (non-⊤) `sql.raw` grant — the posture that HAS relaxed raw SQL, so
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

        // 2. Cross-schema confinement — any explicit foreign schema, anywhere
        //    in the full tree (RangeVar, SET SCHEMA newschema, CreateSchema,
        //    trigger/CALL funcname, COMMENT object, INHERIT target, …). Owns the
        //    diagnostic for a foreign-schema reference (`CrossSchema`), so it runs
        //    before the namespace creation-gating below — a cross-tenant `CREATE TABLE
        //    other.t` is a cross-schema violation first, not a creation-gating one.
        self.check_cross_schema(json, raw)?;

        // 2c. NAMESPACE-authority structural gate (II.2.5 raw-SQL create/rename
        //     classification / II.2.6 creation-gating + injected-shape immutability):
        //     a raw create must pass `schema.create_table` and is denied in any inject
        //     scope; a rename/move needs `schema.rename` and is denied into an
        //     inject scope; an alter/drop of an injected shape element is immutable.
        //     Runs AFTER cross-schema so a foreign-schema target reports `CrossSchema`.
        self.check_namespace_structural(node, raw)?;

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

    /// NAMESPACE-authority STRUCTURAL gate (Phase 2 Step 2b — II.2.5 raw create/
    /// rename classification + II.2.6 creation-gating / injected-shape immutability).
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
    ///   `into_clause` = `SELECT … INTO`) must pass `schema.create_table` at the target
    ///   AND is denied wherever any inject rule covers it (`RawCreateInInjectScope`);
    /// - a `CreateSchemaStmt` must pass `schema.create_schema`;
    /// - a `RenameStmt`/`AlterObjectSchemaStmt` that moves a table checks
    ///   `schema.rename` and is denied into any inject scope
    ///   (`RawRenameIntoInjectScope`); a `RENAME COLUMN` of an injected column is
    ///   immutable;
    /// - an `AlterTableStmt` touching an injected column/PK is immutable
    ///   (`InjectedShapeImmutable`/`InjectedPrimaryKeyImmutable`).
    fn check_namespace_structural(&self, node: &NodeEnum, raw: &str) -> Result<(), GuardError> {
        match node {
            // ── raw create: CREATE TABLE (incl. LIKE / INHERITS / PARTITION OF) ──
            NodeEnum::CreateStmt(c) => {
                let target = self.resolve_relation_target(c.relation.as_ref(), raw)?;
                self.gate_raw_create(&target, raw)?;
            }
            // ── CTAS / CREATE TABLE AS EXECUTE ──────────────────────────────────
            NodeEnum::CreateTableAsStmt(cta) => {
                // A materialized view is not a table create; only OBJECT_TABLE /
                // SELECT INTO bring a table into existence. Other objtypes fall
                // through (matview creation is gated by its own vendor cap).
                if cta.objtype == ObjectType::ObjectTable as i32 || cta.is_select_into {
                    let rel = cta.into.as_ref().and_then(|i| i.rel.as_ref());
                    let target = self.resolve_relation_target(rel, raw)?;
                    self.gate_raw_create(&target, raw)?;
                }
            }
            // ── SELECT … INTO <table> ───────────────────────────────────────────
            // pg_query parses `SELECT … INTO t` as a `SelectStmt` carrying an
            // `into_clause`, NOT a `CreateTableAsStmt`. It still brings a table into
            // existence, so it is a raw create and gets the same gate.
            NodeEnum::SelectStmt(s) => {
                if let Some(into) = s.into_clause.as_ref() {
                    let target = self.resolve_relation_target(into.rel.as_ref(), raw)?;
                    self.gate_raw_create(&target, raw)?;
                }
            }
            // ── CREATE SCHEMA ───────────────────────────────────────────────────
            NodeEnum::CreateSchemaStmt(cs) => {
                // An unqualified/dynamic schema name is unattributable → fail closed
                // unless create_schema is ⊤. `schemaname` empty ⇒ AUTHORIZATION-only
                // form; treat as unattributable.
                let name = cs.schemaname.trim();
                let obj = if name.is_empty() {
                    None
                } else {
                    normalize_pg_identifier(name)
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
            // ── RENAME (table / column) ─────────────────────────────────────────
            NodeEnum::RenameStmt(r) => self.check_rename(r, raw)?,
            // ── SET SCHEMA (move a table across schemas) ────────────────────────
            NodeEnum::AlterObjectSchemaStmt(a) => self.check_set_schema(a, raw)?,
            // ── ALTER TABLE (injected-shape immutability) ───────────────────────
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
        let Some(rel) = rel else {
            return Err(namespace_denied(
                namespace_rule::UNATTRIBUTABLE_RAW_UNDER_SCOPED_RAW_SQL,
                raw,
            ));
        };
        let relname = rel.relname.trim();
        if relname.is_empty() {
            return Err(namespace_denied(
                namespace_rule::UNATTRIBUTABLE_RAW_UNDER_SCOPED_RAW_SQL,
                raw,
            ));
        }
        let schema = if rel.schemaname.trim().is_empty() {
            // Unqualified. Resolve to the pinned project schema; if none, the name is
            // unattributable → deny (fail-closed).
            match self.cfg.pinned_schema() {
                Some(s) => s,
                None => {
                    return Err(namespace_denied(
                        namespace_rule::UNQUALIFIED_NAME_UNDER_SCOPED_RAW_SQL,
                        raw,
                    ))
                }
            }
        } else {
            rel.schemaname.trim().to_string()
        };
        let qualified = format!("{schema}.{relname}");
        normalize_pg_identifier(&qualified).ok_or_else(|| {
            namespace_denied(namespace_rule::UNATTRIBUTABLE_RAW_UNDER_SCOPED_RAW_SQL, raw)
        })
    }

    /// Gate a raw CREATE-TABLE-shaped statement: `schema.create_table` must grant the
    /// target AND no inject rule may cover it (II.2.5/II.2.6a). The inject check comes
    /// first — a table inside an inject scope can ONLY be created via the structured
    /// DSL, so even a `schema.create_table` grant cannot admit a raw create there.
    fn gate_raw_create(&self, target: &ObjectName, raw: &str) -> Result<(), GuardError> {
        if self.cfg.injects_cover(target) {
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

    /// `ALTER TABLE … RENAME TO` (table move) / `RENAME COLUMN` (injected-column
    /// immutability). A bare same-schema table rename still re-anchors under
    /// `schema.rename` at the target and is denied into any inject scope.
    fn check_rename(&self, r: &protobuf::RenameStmt, raw: &str) -> Result<(), GuardError> {
        let is_table = r.rename_type == ObjectType::ObjectTable as i32;
        let is_column = r.rename_type == ObjectType::ObjectColumn as i32;
        if !is_table && !is_column {
            return Ok(());
        }
        // The object being renamed (its current name).
        let source = self.resolve_relation_target(r.relation.as_ref(), raw)?;
        if is_column {
            // RENAME COLUMN <subname> — immutable if the OLD column name is injected.
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

    /// `ALTER TABLE … SET SCHEMA <newschema>` — a cross-schema table move.
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
    /// - denied into ANY inject scope (`RawRenameIntoInjectScope` — the moved table
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
        // ALTER TABLE only — a matview/index objtype carries no injected table shape.
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
            // DROP CONSTRAINT — may drop the pinned PK. We cannot always tell which
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

    /// Scoped-raw-SQL refusals (II.2.5): under a Scoped (non-⊤) `sql.raw` grant,
    /// an opaque-body construct (`CREATE FUNCTION`/`PROCEDURE`/`TRIGGER`/`DO`), a
    /// `SET search_path` (or `set_config`/`ALTER ROLE|DATABASE … SET search_path`),
    /// and any unqualified object reference are unattributable and DENIED — only a
    /// ⊤-scoped grant may carry them.
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
            // ALTER ROLE / ALTER DATABASE … SET search_path — a persisted GUC.
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
        // `set_config('search_path', …)` (the function form) + any UNQUALIFIED object
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
                let is_pg_catalog = relname.len() >= 3 && relname[..3].eq_ignore_ascii_case("pg_");
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
            // Role management — privilege escalation. ALLOW iff Platform:
            // the platform schema migrations must CREATE/ALTER/DROP roles and
            // pin their search_path (0025/0027). Confined still hard-denies.
            //
            // SUPERUSER is the ONE role attribute that stays HARD-DENIED even
            // under Platform: a superuser bypasses RLS and
            // reaches the host (file I/O, `COPY … PROGRAM`). Platform widens
            // privilege *within* the DB, never *host* reach — so a
            // `CREATE/ALTER ROLE … SUPERUSER` is refused before the Platform
            // allow. (Trusted skips the whole deny-list earlier — operator owns
            // the DB.) This guards the vendor `createRole({ superuser: true })`
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
            // GRANT / REVOKE / role-membership grants — privilege management.
            // ALLOW iff Platform: the platform schema migrations grant
            // CONNECT/USAGE/etc. (0025/0027). Confined still hard-denies.
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
                // FORBIDDEN_EXTENSIONS is a non-grant HARD DENY in BOTH profiles,
                // overriding any allowlist grant.
                if denylist::list_contains_ci(denylist::FORBIDDEN_EXTENSIONS, &name) {
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
                // admitted (0025).
                self.check_alter_table_cmds(at, raw)?;
            }
            NodeEnum::DropStmt(d) => {
                // DROP ROLE via the DropStmt spelling — ALLOW iff Platform
                // (the `.down.sql` reverse of CREATE ROLE), else deny.
                if d.remove_type == ObjectType::ObjectRole as i32 {
                    if self.cfg.grants_global_bool(policy_registry::KEY_ACCESS_ROLE) {
                        return Ok(());
                    }
                    return Err(denied(rule::ROLE_MANAGEMENT, raw));
                }
                // DROP is safe only for the enumerated object types. Under
                // Platform the extra set (schema/extension/policy — the
                // `.down.sql`-only reverses) is also admitted.
                let drop_allowed = is_safe_drop_object(d.remove_type)
                    || self.cfg.grants_drop_object(d.remove_type);
                if !drop_allowed {
                    return Err(denied(rule::UNRECOGNIZED_DANGEROUS, raw));
                }
            }
            // CREATE SCHEMA — deny-by-default for Confined; ALLOW iff Platform
            // (platform migrations create platform schemas). When
            // Platform, fall through to the cross-schema confinement below (the
            // schema being created is checked against the allowlist there).
            NodeEnum::CreateSchemaStmt(_) => {
                if !self.cfg.grants_global_bool(policy_registry::KEY_SCHEMA_CREATE_SCHEMA) {
                    return Err(denied(rule::UNRECOGNIZED_DANGEROUS, raw));
                }
            }
            // CREATE POLICY (RLS) — deny-by-default for Confined; ALLOW iff
            // Platform (0025 RLS policies). When Platform, fall through;
            // cross-schema confinement on the policy's table still runs below.
            NodeEnum::CreatePolicyStmt(_) => {
                if !self.cfg.grants_global_bool(policy_registry::KEY_ACCESS_POLICY) {
                    return Err(denied(rule::UNRECOGNIZED_DANGEROUS, raw));
                }
            }
            // DROP OWNED BY <role> — deny-by-default for Confined; ALLOW iff
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
    /// Platform the four RLS subtypes are additionally admitted.
    fn check_alter_table_cmds(
        &self,
        at: &protobuf::AlterTableStmt,
        raw: &str,
    ) -> Result<(), GuardError> {
        let allow_rls = self.cfg.grants_global_bool(policy_registry::KEY_ACCESS_RLS);
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
        if let Some(schema) = foreign_schema_in_tree(json, &|s| self.cfg.grants_cross_schema(s)) {
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
            if !self.cfg.grants_cross_schema(schema) {
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
            if self.cfg.grants_cross_schema(&schema) {
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
        // Under Platform the role-management + search_path needles are
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
        // The role-management body needles are relaxed under the PLATFORM posture
        // ONLY — now an INTERNAL guard vendor-lower rule (no operator-authorable knob):
        // the belt is running (`GuardMode::Enforced`) AND the config holds the
        // `access.role` capability. Platform is `Enforced` + `access.role` → relaxed;
        // Confined is `Enforced` without `access.role` → denied; Trusted is
        // `GuardMode::Off` → the whole belt is skipped in `check()`, but its raw-island
        // body backstop (which reaches here with `Off`) still DENIES these needles, a
        // behaviour the vendor-lower matrix locks.
        let allow_role = !self.cfg.skips_denylist_belt()
            && self
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
        if let Some(schema) = foreign_schema_in_body(body, &|s| self.cfg.grants_cross_schema(s)) {
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
            foreign_schema_literal_in_body(body, &|s| self.cfg.grants_cross_schema(s))
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
pub fn check_raw_view_body_text(
    body: &str,
    raw: &str,
    scope: Option<&SchemaScope>,
) -> Result<(), GuardError> {
    // The view-body scanner runs under a Confined posture pinned to the caller's
    // owned schemas. Map the (transitional) `SchemaScope` onto the owned-schema
    // include set the confined effective policy grants `schema.cross_schema` over:
    //   - `Single(s)`      ⇒ `[s]` (a single owned project schema);
    //   - `Allowlist(v)`   ⇒ `v` (the platform allowlist);
    //   - `Unconfined`/None ⇒ empty ⇒ a `⊤` cross-schema grant (no confinement).
    let owned: Vec<String> = match scope {
        Some(SchemaScope::Single(s)) if !s.is_empty() => vec![s.clone()],
        Some(SchemaScope::Single(_)) | None => Vec::new(),
        Some(SchemaScope::Allowlist(v)) => v.clone(),
        Some(SchemaScope::Unconfined) => Vec::new(),
    };
    let inputs = PolicyInputs::confined("", &[]).with_owned_schemas(owned);
    let guard = SqlGuard::new(GuardConfig::from_policy(
        inputs.compose(),
        SqlDialect::Postgres,
    ));
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
fn foreign_schema_literal_in_body(body: &str, permits: &dyn Fn(&str) -> bool) -> Option<String> {
    let uses_ident_template = body.to_ascii_lowercase().contains("%i");
    for literal in extract_string_literals(body) {
        let lit = literal.trim();
        // A schema the PDP admits (`grants(schema.cross_schema, lit)`) is never a
        // violation — the project schema(s) a confined/platform posture owns, plus
        // any operator-supplied schema.
        if permits(lit) {
            continue;
        }
        // (1) platform schema named directly. The `PLATFORM_SCHEMAS` lexical
        //     backstop fires for any schema in PLATFORM_SCHEMAS that the scope
        //     did NOT permit (port schemas `zero_migrate`/`public`
        //     are not in PLATFORM_SCHEMAS, so they already pass).
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

/// Derive the migration flags from a passing [`GuardReport`].
///
/// - `destructive` (data loss) ⇒ `requires_approval` (the gate must confirm;
///   AI never auto-applies destructive ops).
/// - any non-transactional statement (CONCURRENTLY, ALTER TYPE ADD VALUE,
///   VACUUM) ⇒ `transactional = false` (the two-phase apply path).
/// - a `RENAME COLUMN` / `RENAME TABLE` ⇒ `requires_approval` even though it is
///   NOT data-loss `destructive`: a rename is app-breaking /
///   backward-incompatible (it silently breaks every reader of the old name), so
///   it must be operator-confirmed, never auto-applied. (The declarative
///   expand-contract rename path does NOT emit a bare `RenameStmt` — it emits
///   ADD COLUMN + trigger + backfill + DROP via `ExpandContractAuthor` — so this
///   gate is scoped to a literal `RENAME` in a submitted `up`.)
/// - an `ALTER COLUMN … SET NOT NULL` ⇒ `requires_approval`: it takes an
///   ACCESS EXCLUSIVE lock + a full-table validating scan and ABORTS if any
///   existing row is NULL — and the row-less shadow CANNOT catch that abort/lock,
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
// The per-engine line-1 guard seam (multi-engine abstraction).
// ---------------------------------------------------------------------------

/// The **dialect-neutral** result of a passing [`MigrationGuard::check`].
///
/// This is the line-1 output the core engine actually consumes: the engine's
/// `plan()`/`apply` only read `destructive` (to drive the destructive/approval
/// gate) and `advisories` (to surface operational footguns) — see
/// [`crate::engine::MigrationEngine::plan`]. Deliberately **does not** carry the
/// PG-specific `classes: Vec<StatementClass>` (the `libpg_query` `DdlKind`
/// vocabulary): that stays *inside* the PG guard ([`SqlGuard`]/[`GuardReport`]),
/// because a non-PG engine (`SQLite` descriptor diff, a future non-PG parser) has no
/// `DdlKind` to populate. Keeping the neutral seam free of PG vocabulary is
/// what lets a new engine bring its own line-1 without inheriting `libpg_query`.
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
    /// never deny or gate. Empty for engines that emit none (e.g. `SQLite`'s
    /// descriptor path).
    pub advisories: Vec<Advisory>,
}

/// The **per-engine line-1 defense**, behind a trait so the core engine never
/// selects it by dialect (`if dialect == Sqlite`) — it asks [`guard_for`] and
/// runs whatever line-1 that engine brings.
///
/// - **Postgres** ([`PgGuard`]) — the `libpg_query` parse + deny-list + classify +
///   analyze ([`SqlGuard`]), mapped onto the neutral [`GuardOutcome`].
/// - **`SQLite`** ([`SqliteDescriptorGuard`]) — the descriptor-diff path is trusted
///   by construction (validated at the author boundary, line-2 enforced by the
///   `SqliteBackend` authorizer at apply), so `check` returns the **empty/clean**
///   outcome. The raw-untrusted-SQL fail-closed (`libpg_query` cannot vet `SQLite`)
///   lives on [`SqlGuard::check`] itself — if the PG guard is ever mis-handed a
///   SQLite-keyed config it returns [`GuardError::SqliteRawSqlRejected`] rather
///   than mis-parsing (the existing defensive property).
/// - A future non-PG engine brings its own parser/allowlist impl.
///
/// `GuardOutcome` / [`GuardError`] are shared + neutral; each engine's parser is
/// its own concern.
pub trait MigrationGuard {
    /// Run line-1 over a migration's `up` SQL. `Ok(GuardOutcome)` when every
    /// statement is safe (destructive ops flagged, not denied); `Err` on the
    /// first hard-denied / cross-tenant / unparseable / raw-rejected construct.
    ///
    /// # Errors
    /// Engine-specific: PG surfaces [`GuardError::Denied`] /
    /// [`GuardError::CrossSchema`] / [`GuardError::Parse`]; `SQLite`'s descriptor
    /// path does not deny (it trusts), so its `check` is infallible in practice.
    fn check(&self, up: &str) -> Result<GuardOutcome, GuardError>;
}

/// The Postgres line-1: the existing [`SqlGuard`] (`libpg_query` deny-list +
/// cross-schema confinement + classify + analyze) behind [`MigrationGuard`].
/// Behavior-identical to calling [`SqlGuard::check`] — `check` only drops the
/// PG-specific `classes` from the returned report (the neutral seam).
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
        // engine seam consumes. `flags_for` and the other `classes`
        // consumers call `SqlGuard::check` directly, never through this seam.
        Ok(GuardOutcome {
            destructive: report.destructive,
            advisories: report.advisories,
        })
    }
}

/// The `SQLite` line-1: the descriptor-diff path is **trusted by construction**.
///
/// `SQLite` migrations are produced ONLY by the declarative differ
/// ([`crate::render::declarative::DeclarativeAuthor::diff`]) — there is no raw-SQL `SQLite`
/// author. `libpg_query` cannot parse `SQLite`, so there is no string deny-list to
/// run; the line-1 vet is the descriptor emitter at the author boundary and the
/// line-2 defense is the `SqliteBackend`'s runtime authorizer applied per
/// statement at execution. So `check` returns the **empty**
/// [`GuardOutcome`] — exactly the pre-seam `plan_sqlite_trusted` report + the
/// executor's `run_string_guard == false` skip, now expressed as a per-engine
/// guard instead of an `if dialect == Sqlite` branch.
///
/// (The raw-untrusted-SQL fail-closed — refusing a hand-written `SQLite` string
/// handed to the *PG* guard — stays on [`SqlGuard::check`] as
/// [`GuardError::SqliteRawSqlRejected`]; that defensive property is unchanged.)
#[derive(Debug, Clone, Copy, Default)]
pub struct SqliteDescriptorGuard;

impl SqliteDescriptorGuard {
    /// Construct the `SQLite` descriptor guard (stateless).
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
/// Postgres → [`PgGuard`] (`libpg_query` deny-list); `SQLite` → [`SqliteDescriptorGuard`]
/// (trusted descriptor path). This replaces the `if dialect == Sqlite` branch in
/// `plan()` — the core no longer knows `SQLite` by name; it asks for the dialect's
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
pub struct IrDataSecurityError {
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
pub fn check_ir_data_security_policy(
    cfg: &GuardConfig,
    ir: &MigrationIr,
) -> Result<(), IrDataSecurityError> {
    // The require-RLS obligation now rides in the effective policy (the ⊤-scope
    // `safety.require_rls` Require rule); a policy with no such obligation covering
    // tables is a no-op. Probe the global witness — the obligation is authored
    // ⊤-scope, so any covered object confirms it.
    if !cfg.obligates_require_rls(&global_witness()) {
        return Ok(());
    }

    fn push_policy_ops<'a>(
        cfg: &GuardConfig,
        op_index: usize,
        op: &'a Op,
        out: &mut Vec<(usize, &'a Op)>,
    ) {
        if let Op::Dialectal {
            default,
            pg,
            sqlite,
            mysql,
        } = op
        {
            let own = match cfg.dialect() {
                SqlDialect::Postgres => pg.as_deref(),
                SqlDialect::Sqlite => sqlite.as_deref(),
                SqlDialect::Mysql => mysql.as_deref(),
            };
            if let Some(leg) = own.or(default.as_deref()) {
                for inner in leg {
                    push_policy_ops(cfg, op_index, inner, out);
                }
            }
        } else {
            out.push((op_index, op));
        }
    }

    let mut policy_ops = Vec::new();
    for (op_index, op) in ir.ops.iter().enumerate() {
        push_policy_ops(cfg, op_index, op, &mut policy_ops);
    }

    let mut tables: BTreeMap<(String, String), RlsTableState> = BTreeMap::new();
    for (op_index, op) in policy_ops {
        match op {
            Op::CreateTable { name, schema, .. } | Op::CreatePartition { name, schema, .. } => {
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
            Op::SetRls {
                table,
                schema,
                enabled,
                forced,
            } => {
                if enabled == &Some(false) {
                    return Err(IrDataSecurityError {
                        op_index,
                        source: GuardError::DataSecurityPolicy {
                            rule: data_security_rule::REQUIRE_RLS,
                            statement: format!(
                                "setRls {table:?} enabled:false is forbidden while data_security.require_rls=true"
                            ),
                        },
                    });
                }
                if forced == &Some(false) {
                    return Err(IrDataSecurityError {
                        op_index,
                        source: GuardError::DataSecurityPolicy {
                            rule: data_security_rule::REQUIRE_RLS,
                            statement: format!(
                                "setRls {table:?} forced:false is forbidden while data_security.require_rls=true"
                            ),
                        },
                    });
                }
                if enabled == &Some(true) {
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
            }
            Op::AttachPartition { .. } | Op::DetachPartition { .. } => {}
            Op::DropTable { table, schema, .. }
            | Op::DropPartition {
                name: table,
                schema,
                ..
            } => {
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
            Op::RenameTable {
                table, to, schema, ..
            } => {
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
    let effective_schema = schema.clone().unwrap_or_else(|| {
        // An unqualified table resolves to the config's sole owned schema (the pinned
        // project schema); no unique owned schema ⇒ empty.
        match owned_schemas_from_effective(&cfg.effective).as_slice() {
            [one] => one.clone(),
            _ => String::new(),
        }
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
/// attribute. The attribute is a `DefElem` named `superuser`
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
// lower pipeline lives in `zero-migrate/src/guard_vendor_lower_tests.rs`.
// ===========================================================================
#[cfg(test)]
mod white_box_tests {
    use super::*;

    /// The body token-scan backstop's literal extractor must preserve multi-byte
    /// UTF-8 verbatim. Pre-fix it built each literal via `bytes[j] as char`, which
    /// truncates every non-ASCII byte to a Latin-1 codepoint — corrupting the
    /// literal and potentially splitting a dangerous token off from its adjacent
    /// multi-byte char so the word-scan misses it. This pins faithful extraction.
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
