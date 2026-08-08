//! The VENDOR capability-composition policy.
//!
//! The privileged `zero-migrate` primitives (roles, grants, RLS/policies,
//! functions, extensions, schemas, the gated raw escape) are gated NOT
//! by a hard-coded "platform" profile name but by a **composition of boolean
//! capability flags** + a schema allowlist. A vendor op declares the closed set of
//! [`VendorCapability`] values it needs (computed by
//! `zero_migrate::model::op_support::vendor_capabilities`); the active
//! [`VendorCapabilities`] set either grants them (the op lowers) or REFUSES it
//! fail-closed at validate ([`crate::validate`]) AND again at lower (the rendered
//! SQL hits the Confined deny-list at the second gate).
//!
//! # Why flags, not a profile name
//!
//! The operator asked for a capability-COMPOSITION model so the gate is
//! orthogonal to the trust-profile machinery: the existing
//! [`crate::policy::TrustProfile`] (`Confined`/`Platform`/`Trusted`) MAPS onto
//! NAMED PRESETS ([`VendorCapabilities::confined`] / [`operator`] / [`local`]),
//! but the gate keys on `caps.allow_role`, never on `trust == Confined`. A future
//! "local dev" or "CI" posture can compose its own flag set without touching the
//! gate. The three-profile mapping is [`VendorCapabilities::for_trust`].
//!
//! [`operator`]: VendorCapabilities::operator
//! [`local`]: VendorCapabilities::local

use crate::policy::{SchemaScope, TrustProfile};

/// A zero-sized operator capability token.
///
/// The token type lives with the capability model (in the `zero-migrate-ir` leaf)
/// so operator-side engine seams can name it without depending on the runner. The
/// PRIVATE `()` field blocks only the struct-literal form. The token is freely
/// mintable by any dependent crate through the public [`OperatorCapability::new`],
/// through `Default`, and through `for_test` when a downstream turns on the additive
/// `test-support` feature.
///
/// It authorises nothing. The two functions that take one bind it and never read it,
/// and the config they build is what the public `ExecutorConfig::new` returns anyway.
/// Privilege comes from the composed `EffectivePolicy` argument and from nowhere else
/// - that is the unforgeable type, and its own docs record that it replaced this
/// token. Do NOT hang a real check on holding one of these.
#[derive(Debug, Clone)]
pub struct OperatorCapability(());

/// Alias name for the zero-sized token that gates sealed shared-infra apply.
///
/// This is an alias, not a second forgeable token: it still has a private field,
/// so external crates cannot construct it.
pub type SealApplier = OperatorCapability;

impl OperatorCapability {
    /// The production mint. Reachable across the crate graph (the engine runner
    /// is the single production caller). The token does not select or construct a
    /// policy; policy-bearing APIs require an explicit composed policy.
    #[must_use]
    pub const fn new() -> Self {
        Self(())
    }

    /// **Test-support seam.** Lets the engine-crate test suite exercise
    /// operator-gated entry points. Gated behind the `test-support` feature, so it
    /// never reaches a production build.
    #[cfg(feature = "test-support")]
    #[must_use]
    pub const fn for_test() -> Self {
        Self::new()
    }
}

impl Default for OperatorCapability {
    fn default() -> Self {
        Self::new()
    }
}

/// The CLOSED set of vendor capabilities a privileged op can require.
/// Each [`crate::ir::Op`] vendor variant maps to one or more of these through
/// `zero_migrate::model::op_support::vendor_capabilities`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VendorCapability {
    /// `CREATE/DROP EXTENSION` ([`VendorCapabilities::allow_extension`]).
    Extension,
    /// `CREATE/DROP SCHEMA` ([`VendorCapabilities::allow_schema`]).
    Schema,
    /// `CREATE/ALTER/DROP ROLE` / `DROP OWNED BY` ([`VendorCapabilities::allow_role`]).
    Role,
    /// `GRANT`/`REVOKE` ([`VendorCapabilities::allow_grant`]).
    Grant,
    /// RLS `ENABLE`/`FORCE`/`DISABLE`/`NO FORCE` ([`VendorCapabilities::allow_rls`]).
    Rls,
    /// `ALTER TABLE ATTACH PARTITION` ([`VendorCapabilities::allow_partition`]).
    Partition,
    /// `CREATE/DROP POLICY` ([`VendorCapabilities::allow_policy`]).
    Policy,
    /// `CREATE/DROP FUNCTION` ([`VendorCapabilities::allow_function`]).
    Function,
    /// The gated raw escape (`pgRaw`) ([`VendorCapabilities::allow_raw_sql`]).
    RawSql,
    /// The gated raw view-body SELECT escape ([`VendorCapabilities::allow_raw_view_body`]).
    RawViewBody,
    /// `PostgreSQL` materialized views ([`VendorCapabilities::allow_materialized_view`]).
    MaterializedView,
}

impl VendorCapability {
    /// A short, stable lower-camel token for diagnostics (the `op` field of the
    /// `VENDOR_OP_DENIED` envelope's reason).
    #[must_use]
    pub const fn as_token(self) -> &'static str {
        match self {
            Self::Extension => "extension",
            Self::Schema => "schema",
            Self::Role => "role",
            Self::Grant => "grant",
            Self::Rls => "rls",
            Self::Partition => "partition",
            Self::Policy => "policy",
            Self::Function => "function",
            Self::RawSql => "rawSql",
            Self::RawViewBody => "rawViewBody",
            Self::MaterializedView => "materializedView",
        }
    }

    /// The capability-flag NAME the operator composes (for the suggested-fix text).
    #[must_use]
    pub const fn flag_name(self) -> &'static str {
        match self {
            Self::Extension => "allowExtension",
            Self::Schema => "allowSchema",
            Self::Role => "allowRole",
            Self::Grant => "allowGrant",
            Self::Rls => "allowRls",
            Self::Partition => "allowPartition",
            Self::Policy => "allowPolicy",
            Self::Function => "allowFunction",
            Self::RawSql => "allowRawSql",
            Self::RawViewBody => "allowRawViewBody",
            Self::MaterializedView => "allowMaterializedView",
        }
    }
}

/// The active VENDOR capability set — a composition of boolean flags + a schema
/// allowlist. The gate ([`grants`](Self::grants)) keys on the
/// flags; the named presets ([`confined`](Self::confined) /
/// [`operator`](Self::operator) / [`local`](Self::local)) are the compositions the
/// trust profiles map onto.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VendorCapabilities {
    /// `CREATE/DROP EXTENSION`.
    pub allow_extension: bool,
    /// `CREATE/DROP SCHEMA`.
    pub allow_schema: bool,
    /// `CREATE/ALTER/DROP ROLE` / `DROP OWNED BY`.
    pub allow_role: bool,
    /// `GRANT`/`REVOKE`.
    pub allow_grant: bool,
    /// RLS enable/force/disable/no-force.
    pub allow_rls: bool,
    /// `PostgreSQL` partition attach.
    pub allow_partition: bool,
    /// `CREATE/DROP POLICY`.
    pub allow_policy: bool,
    /// `CREATE/DROP FUNCTION` (the raw body escape).
    pub allow_function: bool,
    /// The gated raw-statement escape (`pgRaw`).
    pub allow_raw_sql: bool,
    /// The gated raw view-body SELECT escape.
    pub allow_raw_view_body: bool,
    /// `PostgreSQL` materialized views.
    pub allow_materialized_view: bool,
    /// Whether references to schemas OTHER than the (single) project schema are
    /// admitted (the multi-schema operator posture). Cross-schema confinement is
    /// ALSO enforced by [`SchemaScope`] at the existing schema-scope gate; this flag is the
    /// capability-model mirror so the policy is self-describing.
    pub allow_cross_schema: bool,
    /// The schema allowlist this capability set permits (empty ⇒ no widening; the
    /// project schema is always implicitly permitted by the schema-scope gate).
    pub schemas: Vec<String>,
}

impl VendorCapabilities {
    /// The **confined** preset (the untrusted creator/AI posture): NO vendor
    /// capability, NO cross-schema. Every vendor op is refused fail-closed. This is
    /// the composition [`TrustProfile::Confined`] maps onto.
    #[must_use]
    pub const fn confined() -> Self {
        Self {
            allow_extension: false,
            allow_schema: false,
            allow_role: false,
            allow_grant: false,
            allow_rls: false,
            allow_partition: false,
            allow_policy: false,
            allow_function: false,
            allow_raw_sql: false,
            allow_raw_view_body: false,
            allow_materialized_view: false,
            allow_cross_schema: false,
            schemas: Vec::new(),
        }
    }

    /// The **operator** preset (the trusted platform/operator posture): EVERY
    /// vendor capability enabled. This is the composition [`TrustProfile::Platform`]
    /// and [`TrustProfile::Trusted`] map onto. `schemas` is filled from the active
    /// allowlist by [`from_scope`](Self::from_scope).
    #[must_use]
    pub const fn operator() -> Self {
        Self {
            allow_extension: true,
            allow_schema: true,
            allow_role: true,
            allow_grant: true,
            allow_rls: true,
            allow_partition: true,
            allow_policy: true,
            allow_function: true,
            allow_raw_sql: true,
            allow_raw_view_body: true,
            allow_materialized_view: true,
            allow_cross_schema: true,
            schemas: Vec::new(),
        }
    }

    /// The **local** preset (an in-between dev/CI posture): structural vendor DDL
    /// (extensions, schemas, grants, RLS, policies, functions) is
    /// enabled, but ROLE management and the raw `pgRaw` escape are NOT — a local
    /// dev DB does not mint roles and never needs the last-resort raw escape. This
    /// preset is not wired to a `TrustProfile` (there is no `Local` profile); it is
    /// available for a caller composing a bespoke gate.
    #[must_use]
    pub const fn local() -> Self {
        Self {
            allow_extension: true,
            allow_schema: true,
            allow_role: false,
            allow_grant: true,
            allow_rls: true,
            allow_partition: true,
            allow_policy: true,
            allow_function: true,
            allow_raw_sql: false,
            allow_raw_view_body: false,
            allow_materialized_view: true,
            allow_cross_schema: true,
            schemas: Vec::new(),
        }
    }

    /// Map a [`TrustProfile`] onto its named preset: Confined ⇒
    /// [`confined`](Self::confined); Platform / Trusted ⇒ [`operator`](Self::operator).
    /// The `TrustProfile` is the EXISTING operator-gated machinery; this is the
    /// single bridge from it to the capability composition.
    #[must_use]
    pub const fn for_trust(trust: TrustProfile) -> Self {
        match trust {
            TrustProfile::Confined => Self::confined(),
            TrustProfile::Platform | TrustProfile::Trusted => Self::operator(),
        }
    }

    /// Derive the capability set from the validate-layer
    /// [`SchemaScope`](crate::policy::SchemaScope) the loader threads. Guarded paths
    /// derive this scope from the caller-supplied effective policy:
    /// - `None` ⇒ omitted/default public capability ⇒ [`confined`](Self::confined).
    /// - `Some(Single(_))` ⇒ **Confined** (the creator/AI posture) ⇒ [`confined`](Self::confined).
    /// - `Some(Allowlist(list))` ⇒ **Platform** ⇒ [`operator`](Self::operator) with
    ///   `schemas = list`.
    /// - `Some(Unconfined)` ⇒ **Trusted** ⇒ [`operator`](Self::operator) with no
    ///   validate-time cross-schema confinement.
    ///
    /// `None` is intentionally least-privilege so future public callers cannot
    /// accidentally enable vendor ops by omitting a capability. Wider scopes must
    /// come from an explicitly authored policy on guarded paths.
    #[must_use]
    pub fn from_scope(scope: Option<&SchemaScope>) -> Self {
        match scope {
            None | Some(SchemaScope::Single(_)) => Self::confined(),
            Some(SchemaScope::Allowlist(list)) => {
                let mut caps = Self::operator();
                caps.schemas = list.clone();
                caps
            }
            Some(SchemaScope::Unconfined) => Self::operator(),
        }
    }

    /// Does this capability set GRANT `cap`? The fail-closed gate predicate:
    /// a vendor op whose required capability is NOT granted is
    /// refused.
    #[must_use]
    pub const fn grants(&self, cap: VendorCapability) -> bool {
        match cap {
            VendorCapability::Extension => self.allow_extension,
            VendorCapability::Schema => self.allow_schema,
            VendorCapability::Role => self.allow_role,
            VendorCapability::Grant => self.allow_grant,
            VendorCapability::Rls => self.allow_rls,
            VendorCapability::Partition => self.allow_partition,
            VendorCapability::Policy => self.allow_policy,
            VendorCapability::Function => self.allow_function,
            VendorCapability::RawSql => self.allow_raw_sql,
            VendorCapability::RawViewBody => self.allow_raw_view_body,
            VendorCapability::MaterializedView => self.allow_materialized_view,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn confined_grants_no_vendor_capability() {
        let c = VendorCapabilities::confined();
        for cap in [
            VendorCapability::Extension,
            VendorCapability::Schema,
            VendorCapability::Role,
            VendorCapability::Grant,
            VendorCapability::Rls,
            VendorCapability::Partition,
            VendorCapability::Policy,
            VendorCapability::Function,
            VendorCapability::RawSql,
            VendorCapability::RawViewBody,
            VendorCapability::MaterializedView,
        ] {
            assert!(!c.grants(cap), "confined must NOT grant {cap:?}");
        }
    }

    #[test]
    fn operator_grants_every_vendor_capability() {
        let o = VendorCapabilities::operator();
        for cap in [
            VendorCapability::Extension,
            VendorCapability::Schema,
            VendorCapability::Role,
            VendorCapability::Grant,
            VendorCapability::Rls,
            VendorCapability::Partition,
            VendorCapability::Policy,
            VendorCapability::Function,
            VendorCapability::RawSql,
            VendorCapability::RawViewBody,
            VendorCapability::MaterializedView,
        ] {
            assert!(o.grants(cap), "operator must grant {cap:?}");
        }
    }

    #[test]
    fn local_is_in_between_no_role_no_raw() {
        let l = VendorCapabilities::local();
        assert!(l.grants(VendorCapability::Function));
        assert!(l.grants(VendorCapability::Policy));
        assert!(l.grants(VendorCapability::Partition));
        assert!(l.grants(VendorCapability::MaterializedView));
        assert!(
            !l.grants(VendorCapability::Role),
            "local must not mint roles"
        );
        assert!(
            !l.grants(VendorCapability::RawSql),
            "local must not allow the raw escape"
        );
        assert!(
            !l.grants(VendorCapability::RawViewBody),
            "local must not allow raw view bodies"
        );
    }

    #[test]
    fn for_trust_maps_profiles_onto_presets() {
        assert_eq!(
            VendorCapabilities::for_trust(TrustProfile::Confined),
            VendorCapabilities::confined()
        );
        assert_eq!(
            VendorCapabilities::for_trust(TrustProfile::Platform),
            VendorCapabilities::operator()
        );
        assert_eq!(
            VendorCapabilities::for_trust(TrustProfile::Trusted),
            VendorCapabilities::operator()
        );
    }

    #[test]
    fn from_scope_distinguishes_the_three_postures() {
        // Confined (Single) → no vendor caps.
        let confined = VendorCapabilities::from_scope(Some(&SchemaScope::Single("app1".into())));
        assert!(!confined.grants(VendorCapability::Role));
        // Platform (Allowlist) → all caps + the schemas carried.
        let platform = VendorCapabilities::from_scope(Some(&SchemaScope::Allowlist(vec![
            "zero_migrate".into(),
            "public".into(),
        ])));
        assert!(platform.grants(VendorCapability::Role));
        assert_eq!(
            platform.schemas,
            vec!["zero_migrate".to_string(), "public".to_string()]
        );
        // Omitted/default public capability (None) → confined, not operator.
        let defaulted = VendorCapabilities::from_scope(None);
        assert!(!defaulted.grants(VendorCapability::RawSql));
        assert!(!defaulted.grants(VendorCapability::RawViewBody));
        // Explicit Trusted (Unconfined) → all caps.
        let trusted = VendorCapabilities::from_scope(Some(&SchemaScope::Unconfined));
        assert!(trusted.grants(VendorCapability::RawSql));
        assert!(trusted.grants(VendorCapability::RawViewBody));
    }
}
