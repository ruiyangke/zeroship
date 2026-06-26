//! The VENDOR capability-composition policy (vendor spec §3).
//!
//! The privileged `@zeroship/migrate/pg` primitives (roles, grants, RLS/policies,
//! triggers, functions, extensions, schemas, the gated raw escape) are gated NOT
//! by a hard-coded "platform" profile name but by a **composition of boolean
//! capability flags** + a schema allowlist. A vendor op declares the ONE
//! [`VendorCapability`] it needs ([`crate::ir::Op::vendor_capability`]); the active
//! [`VendorCapabilities`] set either grants it (the op lowers) or REFUSES it
//! fail-closed at validate ([`crate::validate`]) AND again at lower (the rendered
//! SQL hits the Confined deny-list, §3.2 gate 2).
//!
//! # Why flags, not a profile name
//!
//! The operator asked for a capability-COMPOSITION model so the gate is
//! orthogonal to the trust-profile machinery: the existing
//! [`crate::guard::TrustProfile`] (`Confined`/`Platform`/`Trusted`) MAPS onto
//! NAMED PRESETS ([`VendorCapabilities::confined`] / [`operator`] / [`local`]),
//! but the gate keys on `caps.allow_role`, never on `trust == Confined`. A future
//! "local dev" or "CI" posture can compose its own flag set without touching the
//! gate. The three-profile mapping is [`VendorCapabilities::for_trust`].
//!
//! [`operator`]: VendorCapabilities::operator
//! [`local`]: VendorCapabilities::local

use crate::guard::{SchemaScope, TrustProfile};

/// The CLOSED set of vendor capabilities a privileged op can require (vendor spec
/// §3.2). Each [`crate::ir::Op`] vendor variant maps to exactly one of these via
/// [`crate::ir::Op::vendor_capability`].
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
    /// `CREATE/DROP POLICY` ([`VendorCapabilities::allow_policy`]).
    Policy,
    /// `CREATE/DROP TRIGGER` ([`VendorCapabilities::allow_trigger`]).
    Trigger,
    /// `CREATE/DROP FUNCTION` ([`VendorCapabilities::allow_function`]).
    Function,
    /// The gated raw escape (`pg.sql`) ([`VendorCapabilities::allow_raw_sql`]).
    RawSql,
}

impl VendorCapability {
    /// A short, stable lower-camel token for diagnostics (the `op` field of the
    /// `VENDOR_OP_DENIED` envelope's reason).
    #[must_use]
    pub fn as_token(self) -> &'static str {
        match self {
            VendorCapability::Extension => "extension",
            VendorCapability::Schema => "schema",
            VendorCapability::Role => "role",
            VendorCapability::Grant => "grant",
            VendorCapability::Rls => "rls",
            VendorCapability::Policy => "policy",
            VendorCapability::Trigger => "trigger",
            VendorCapability::Function => "function",
            VendorCapability::RawSql => "rawSql",
        }
    }

    /// The capability-flag NAME the operator composes (for the suggested-fix text).
    #[must_use]
    pub fn flag_name(self) -> &'static str {
        match self {
            VendorCapability::Extension => "allowExtension",
            VendorCapability::Schema => "allowSchema",
            VendorCapability::Role => "allowRole",
            VendorCapability::Grant => "allowGrant",
            VendorCapability::Rls => "allowRls",
            VendorCapability::Policy => "allowPolicy",
            VendorCapability::Trigger => "allowTrigger",
            VendorCapability::Function => "allowFunction",
            VendorCapability::RawSql => "allowRawSql",
        }
    }
}

/// The active VENDOR capability set — a composition of boolean flags + a schema
/// allowlist (vendor spec §3). The gate ([`grants`](Self::grants)) keys on the
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
    /// `CREATE/DROP POLICY`.
    pub allow_policy: bool,
    /// `CREATE/DROP TRIGGER`.
    pub allow_trigger: bool,
    /// `CREATE/DROP FUNCTION` (the raw body escape).
    pub allow_function: bool,
    /// The gated raw-statement escape (`pg.sql`).
    pub allow_raw_sql: bool,
    /// Whether references to schemas OTHER than the (single) project schema are
    /// admitted (the multi-schema operator posture). Cross-schema confinement is
    /// ALSO enforced by [`SchemaScope`] at the existing PR10 gate; this flag is the
    /// capability-model mirror so the policy is self-describing.
    pub allow_cross_schema: bool,
    /// The schema allowlist this capability set permits (empty ⇒ no widening; the
    /// project schema is always implicitly permitted by the PR10 gate).
    pub schemas: Vec<String>,
}

impl VendorCapabilities {
    /// The **confined** preset (the untrusted creator/AI posture): NO vendor
    /// capability, NO cross-schema. Every vendor op is refused fail-closed. This is
    /// the composition [`TrustProfile::Confined`] maps onto.
    #[must_use]
    pub fn confined() -> Self {
        Self {
            allow_extension: false,
            allow_schema: false,
            allow_role: false,
            allow_grant: false,
            allow_rls: false,
            allow_policy: false,
            allow_trigger: false,
            allow_function: false,
            allow_raw_sql: false,
            allow_cross_schema: false,
            schemas: Vec::new(),
        }
    }

    /// The **operator** preset (the trusted platform/operator posture): EVERY
    /// vendor capability enabled. This is the composition [`TrustProfile::Platform`]
    /// and [`TrustProfile::Trusted`] map onto. `schemas` is filled from the active
    /// allowlist by [`from_scope`](Self::from_scope).
    #[must_use]
    pub fn operator() -> Self {
        Self {
            allow_extension: true,
            allow_schema: true,
            allow_role: true,
            allow_grant: true,
            allow_rls: true,
            allow_policy: true,
            allow_trigger: true,
            allow_function: true,
            allow_raw_sql: true,
            allow_cross_schema: true,
            schemas: Vec::new(),
        }
    }

    /// The **local** preset (an in-between dev/CI posture): structural vendor DDL
    /// (extensions, schemas, grants, RLS, policies, triggers, functions) is
    /// enabled, but ROLE management and the raw `pg.sql` escape are NOT — a local
    /// dev DB does not mint roles and never needs the last-resort raw escape. This
    /// preset is not wired to a `TrustProfile` (there is no `Local` profile); it is
    /// available for a caller composing a bespoke gate.
    #[must_use]
    pub fn local() -> Self {
        Self {
            allow_extension: true,
            allow_schema: true,
            allow_role: false,
            allow_grant: true,
            allow_rls: true,
            allow_policy: true,
            allow_trigger: true,
            allow_function: true,
            allow_raw_sql: false,
            allow_cross_schema: true,
            schemas: Vec::new(),
        }
    }

    /// Map a [`TrustProfile`] onto its named preset (vendor spec §3.1): Confined ⇒
    /// [`confined`](Self::confined); Platform / Trusted ⇒ [`operator`](Self::operator).
    /// The TrustProfile is the EXISTING operator-gated machinery; this is the
    /// single bridge from it to the capability composition.
    #[must_use]
    pub fn for_trust(trust: TrustProfile) -> Self {
        match trust {
            TrustProfile::Confined => Self::confined(),
            TrustProfile::Platform | TrustProfile::Trusted => Self::operator(),
        }
    }

    /// Derive the capability set from the validate-layer
    /// [`SchemaScope`](crate::guard::SchemaScope) the loader threads (vendor spec
    /// §3.2). The scope is produced ONLY by the operator-gated `GuardConfig` ctors,
    /// so it is a faithful, non-spoofable trust signal:
    /// - `None` ⇒ **Trusted** (the public dbmate posture) ⇒ [`operator`](Self::operator).
    /// - `Some(Single(_))` ⇒ **Confined** (the creator/AI posture) ⇒ [`confined`](Self::confined).
    /// - `Some(Allowlist(list))` ⇒ **Platform** ⇒ [`operator`](Self::operator) with
    ///   `schemas = list`.
    ///
    /// Only `confined()` produces `Single`; only `platform()`/`trusted()` produce
    /// `Allowlist`/`None` — so this mapping is exact, by construction
    /// (`guard.rs` `schema_scope`).
    ///
    /// # GUARD-RAIL — `None ⇒ operator` is intentional, NOT fail-open
    ///
    /// `None` maps to the FULL [`operator`](Self::operator) capability set
    /// because `None` is produced EXCLUSIVELY by a **Trusted** `GuardConfig`
    /// (`GuardConfig::schema_scope` returns `None` only for `TrustProfile::Trusted`,
    /// `guard.rs`), and a Trusted config can be minted ONLY with an
    /// `OperatorCapability` token inside `platform_runner` — never from a
    /// creator-reachable path. Every CREATOR entry threads `Some(Single(_))`
    /// explicitly: `IrAuthor::load_and_lower` pins
    /// `Some(SchemaScope::Single(project_schema))` (`ir_author.rs`), and
    /// `load_and_lower_guarded` derives the scope from a `GuardConfig::confined`
    /// the submit ingress forces (`submit.rs`) ⇒ `Some(Single)`. So no
    /// creator-reachable call ever passes `None` here.
    ///
    /// This is pinned by an end-to-end test (`vendor_via_creator_load_path_is_denied`
    /// in `ir_author.rs`): a vendor op loaded through the Confined creator entry is
    /// refused `VENDOR_OP_DENIED`, proving the creator path's scope is `Single`,
    /// never the `None` that would grant operator caps. A FUTURE caller that
    /// hands `None` to the validate layer for a creator IR would be granting full
    /// vendor caps — so `None` MUST stay an operator-only, `OperatorCapability`-gated
    /// signal; do not introduce a non-Trusted `None` producer.
    #[must_use]
    pub fn from_scope(scope: Option<&SchemaScope>) -> Self {
        match scope {
            None => Self::operator(),
            Some(SchemaScope::Single(_)) => Self::confined(),
            Some(SchemaScope::Allowlist(list)) => {
                let mut caps = Self::operator();
                caps.schemas = list.clone();
                caps
            }
        }
    }

    /// Does this capability set GRANT `cap`? The fail-closed gate predicate
    /// (vendor spec §3.2): a vendor op whose required capability is NOT granted is
    /// refused.
    #[must_use]
    pub fn grants(&self, cap: VendorCapability) -> bool {
        match cap {
            VendorCapability::Extension => self.allow_extension,
            VendorCapability::Schema => self.allow_schema,
            VendorCapability::Role => self.allow_role,
            VendorCapability::Grant => self.allow_grant,
            VendorCapability::Rls => self.allow_rls,
            VendorCapability::Policy => self.allow_policy,
            VendorCapability::Trigger => self.allow_trigger,
            VendorCapability::Function => self.allow_function,
            VendorCapability::RawSql => self.allow_raw_sql,
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
            VendorCapability::Policy,
            VendorCapability::Trigger,
            VendorCapability::Function,
            VendorCapability::RawSql,
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
            VendorCapability::Policy,
            VendorCapability::Trigger,
            VendorCapability::Function,
            VendorCapability::RawSql,
        ] {
            assert!(o.grants(cap), "operator must grant {cap:?}");
        }
    }

    #[test]
    fn local_is_in_between_no_role_no_raw() {
        let l = VendorCapabilities::local();
        assert!(l.grants(VendorCapability::Function));
        assert!(l.grants(VendorCapability::Policy));
        assert!(!l.grants(VendorCapability::Role), "local must not mint roles");
        assert!(!l.grants(VendorCapability::RawSql), "local must not allow the raw escape");
    }

    #[test]
    fn for_trust_maps_profiles_onto_presets() {
        assert_eq!(VendorCapabilities::for_trust(TrustProfile::Confined), VendorCapabilities::confined());
        assert_eq!(VendorCapabilities::for_trust(TrustProfile::Platform), VendorCapabilities::operator());
        assert_eq!(VendorCapabilities::for_trust(TrustProfile::Trusted), VendorCapabilities::operator());
    }

    #[test]
    fn from_scope_distinguishes_the_three_postures() {
        // Confined (Single) → no vendor caps.
        let confined = VendorCapabilities::from_scope(Some(&SchemaScope::Single("app1".into())));
        assert!(!confined.grants(VendorCapability::Role));
        // Platform (Allowlist) → all caps + the schemas carried.
        let platform = VendorCapabilities::from_scope(Some(&SchemaScope::Allowlist(vec![
            "zeroship".into(),
            "public".into(),
        ])));
        assert!(platform.grants(VendorCapability::Role));
        assert_eq!(platform.schemas, vec!["zeroship".to_string(), "public".to_string()]);
        // Trusted (None) → all caps.
        let trusted = VendorCapabilities::from_scope(None);
        assert!(trusted.grants(VendorCapability::RawSql));
    }
}
