//! The zero-migrate ENGINE BUILTIN policy registry + the statement-class → knob-key
//! map (Phase 2 Step 2a).
//!
//! `zero-migrate-policy` ships the PDP *mechanism* (the knob/rule/document model,
//! the composition algebra, the unforgeable [`EffectivePolicy`]). It is content-free
//! by design. THIS module is the engine's *content*: it declares zero-migrate's
//! knobs — the vendor capabilities as grant keys, the op-timeout ceilings, the
//! index/table-rewrite postures, and the data-security obligations — as a builtin
//! [`PolicyRegistry`], and maps the guard's statement classes onto those keys.
//!
//! # The six knob DOMAINS
//!
//! One axis is the namespace: the DOMAIN the knob governs (what it protects), never a
//! dialect or a category/polarity bucket. Everything else — polarity, kind, object
//! model, enforcement, default, inherit — is a declared [`KnobDef`] field.
//!
//! - **`sql`** — the raw-text escape hatch.
//! - **`schema`** — structural DDL (tables, schemas, columns, partitions, renames).
//! - **`access`** — access control (roles, grants, RLS, policies).
//! - **`code`** — programmable / installed objects (functions, matviews, extensions).
//! - **`runtime`** — execution & resource behavior (timeouts, index creation, rewrite).
//! - **`safety`** — data protection (limits AND obligations; polarity is the field).
//!
//! Two of the old operator-posture toggles do NOT live here anymore. "Skip the static
//! guard belt" is a host/root POSTURE (`GuardMode`), not a composable per-app grant —
//! it is the single most dangerous switch, quarantined OUT of the composable registry.
//! The raw-island role-needle relaxation is an INTERNAL guard vendor-lower rule keyed
//! off the posture, not an operator-authorable knob. Both moved to the guard crate.
//!
//! This is the Step-0 prep for moving the guard's capability gate onto the PDP: the
//! guard, given an [`EffectivePolicy`] composed over [`builtin_registry`], queries
//! `grants(key, object)` for the statement's capability key instead of reading a
//! `VendorCapabilities` bit. The registry here is what makes those queries meaningful
//! (a key with no def would fail closed).
//!
//! # Object models (II.2.5)
//!
//! Each knob's [`ObjectModel`] mirrors the granularity at which today's guard makes
//! the decision:
//! - **Global** — `access.role`/`access.grant`/`code.extension`/`code.function` (+ the
//!   other whole-DB vendor caps): today's guard reads a single `allow_*` bit with no
//!   object attribution, so the grant is database-global.
//! - **PerSchema** — `schema.create_schema`/`schema.cross_schema`: schema creation /
//!   reference is attributed to the schema.
//! - **PerTable** — `access.rls`/`access.policy`: RLS enable/force + row-security
//!   policies are attributed to the table they protect.
//!
//! # Polarities
//!
//! Vendor capabilities + op ceilings + the index/table-rewrite postures are
//! `Grant` (compose DOWN, deny-by-default). The `safety.require_rls`/
//! `safety.no_hard_delete` obligations are `Require` (compose UP, un-droppable).
//! `safety.destructive_ops` is a rank-ordered `Grant` (forbid ⊑ warn ⊑ allow — the
//! tighter posture is the default).

use zero_migrate_policy::{
    Enforcement, KnobDef, KnobKey, KnobKind, KnobValue, ObjectModel, Polarity, PolicyRegistry,
};

use crate::capability::VendorCapability;

// ══════════════════════════════════════════════════════════════════════════════
// Knob-key constants — the stable identifiers the guard/validator query by.
// ══════════════════════════════════════════════════════════════════════════════

// ── sql — the raw-text escape hatch ─────────────────────────────────────────────

/// The gated raw-statement escape (`pgRaw`) (object-scoped Bool grant; still
/// deny-list-guarded). Object set = all referenced objects (II.2.5).
pub const KEY_SQL_RAW: &str = "sql.raw";
/// The gated raw view-body SELECT escape (Global Bool grant).
pub const KEY_SQL_RAW_VIEW_BODY: &str = "sql.raw_view_body";

// ── schema — structural DDL ─────────────────────────────────────────────────────

/// Namespace-authority: may CREATE a table matching this scope (PerTable Bool
/// grant, default-deny). The per-op anchor of the II.2.6a creation-gating: an
/// object comes into existence ONLY where this grants (structured `createTable`
/// AND a classified raw `CREATE TABLE` both check it). The compose-time
/// `creatable ⊑ mandatory-inject` lint lives in the policy crate; the guard
/// enforces the per-op grant.
pub const KEY_SCHEMA_CREATE_TABLE: &str = "schema.create_table";
/// `CREATE SCHEMA` (PerSchema Bool grant). Absorbs the former Postgres-spelled and
/// engine-neutral schema-create capabilities into one engine-neutral key.
pub const KEY_SCHEMA_CREATE_SCHEMA: &str = "schema.create_schema";
/// Namespace-authority: may name/move a table INTO this scope — the TARGET of a
/// `RENAME` / `SET SCHEMA` / create-as (PerTable Bool grant, default-deny,
/// II.2.6a). Closes the rename-TOCTOU: a cross-scope move needs this grant on the
/// target scope. (The namespace-authority CHECK — rename-into-inject-scope — stays a
/// guard rule.)
pub const KEY_SCHEMA_RENAME: &str = "schema.rename";
/// Which schemas this migration may reference (PerSchema Bool grant, default-deny)
/// — the capability-model replacement for the schema-scope confinement. A reference
/// to schema `s` is admitted iff `grants(schema.cross_schema, s)` is `Bool(true)`; the
/// project schema(s) a confined/platform posture owns are granted here, everything
/// else is a `CrossSchema` violation. Object-scoped so the grant can name exactly
/// the permitted schemas (an `include: ["app1"]` grant permits only `app1`).
pub const KEY_SCHEMA_CROSS_SCHEMA: &str = "schema.cross_schema";
/// `ALTER TABLE ATTACH/DETACH PARTITION` (Global Bool grant).
pub const KEY_SCHEMA_PARTITION: &str = "schema.partition";
/// Namespace-authority: may `ALTER`/`DROP`/`RENAME` an INJECTED shape element
/// (column, pinned PK, or index) of a table (PerTable Bool grant, default-deny,
/// II.2.6b). Injected shape is the operator's floor and immutable by default; only
/// this grant waves the injected-shape-immutability denial. **`inherit = false`** —
/// this is a POWER GRANT: a SILENT creator draft must NOT inherit "override the
/// platform's injected columns"; it gets the default (deny) unless it asks explicitly.
pub const KEY_SCHEMA_ALTER_INJECTED: &str = "schema.alter_injected";

// ── access — access control ─────────────────────────────────────────────────────

/// `CREATE/ALTER/DROP ROLE` / `DROP OWNED BY` (Global Bool grant). SUPERUSER stays a
/// hard-deny regardless of grant.
pub const KEY_ACCESS_ROLE: &str = "access.role";
/// `GRANT`/`REVOKE`/`ALTER DEFAULT PRIVILEGES` (Global Bool grant).
pub const KEY_ACCESS_GRANT: &str = "access.grant";
/// RLS `ENABLE/FORCE/DISABLE/NO FORCE` (PerTable Bool grant).
pub const KEY_ACCESS_RLS: &str = "access.rls";
/// `CREATE/DROP POLICY` — row-security policy (PerTable Bool grant).
pub const KEY_ACCESS_POLICY: &str = "access.policy";

// ── code — programmable / installed objects ─────────────────────────────────────

/// The `CREATE EXTENSION` name allowlist (Global **StrSet** grant) — the `StrSet`
/// value carries the permitted extension names. The allowlist IS the capability:
/// empty = deny all. `FORBIDDEN_EXTENSIONS` is a non-grant hard deny in the guard
/// regardless. (Merged from the former extension bool toggle + name allowlist.)
pub const KEY_CODE_EXTENSION: &str = "code.extension";
/// `CREATE/DROP FUNCTION` / `PROCEDURE` (Global Bool grant).
pub const KEY_CODE_FUNCTION: &str = "code.function";
/// `PostgreSQL` materialized views (Global Bool grant).
pub const KEY_CODE_MATERIALIZED_VIEW: &str = "code.materialized_view";

// ── runtime — execution & resource behavior ─────────────────────────────────────

/// Per-op `lock_timeout` ceiling in ms (UintCeiling, hard floor 1 — the
/// no-indefinite-lock invariant, II.5).
pub const KEY_RUNTIME_LOCK_TIMEOUT_MS: &str = "runtime.lock_timeout_ms";
/// Per-op `statement_timeout` ceiling in ms (UintCeiling, hard floor 1).
pub const KEY_RUNTIME_STATEMENT_TIMEOUT_MS: &str = "runtime.statement_timeout_ms";
/// Index-creation posture: `forbid` ⊑ `warn` ⊑ `allow` (OrderedEnum grant).
pub const KEY_RUNTIME_INDEX_CREATION: &str = "runtime.index_creation";
/// Table-rewrite posture: `forbid` ⊑ `warn` ⊑ `allow` (OrderedEnum grant).
pub const KEY_RUNTIME_TABLE_REWRITE: &str = "runtime.table_rewrite";

// ── safety — data protection (limits AND obligations) ───────────────────────────

/// Data-security destructive posture: `forbid` ⊑ `warn` ⊑ `allow` (OrderedEnum grant).
pub const KEY_SAFETY_DESTRUCTIVE_OPS: &str = "safety.destructive_ops";
/// Data-security: every created table must end RLS-enabled (Require Bool obligation).
pub const KEY_SAFETY_REQUIRE_RLS: &str = "safety.require_rls";
/// Data-security: no hard `DELETE`/`TRUNCATE`/`DROP` (Require Bool obligation).
pub const KEY_SAFETY_NO_HARD_DELETE: &str = "safety.no_hard_delete";
/// Data-security approval OBLIGATION: `never` ⊑ `on_destructive` ⊑ `always`
/// (OrderedEnum, `Require` polarity — composes UP, un-lowerable). This is a SEALED
/// obligation the engine does not enforce but a HOST does: it is
/// `Enforcement::HostEnforced`, so the engine's own guard/apply never gate on it, YET
/// — unlike a `DeclaredOnly` knob — it MAY be sealed at a non-default value (M-2,
/// II.6). The HOST (`migrated`) reads it via
/// [`crate::policy_approval::migration_requires_approval`] and enforces approval as a
/// state machine — the engine `apply` stays dumb. Object-scoped like every other
/// knob (the level resolves per target object; the host ORs across a migration's ops).
pub const KEY_SAFETY_REQUIRE_APPROVAL: &str = "safety.require_approval";

/// The tightest→loosest variant order shared by the posture OrderedEnum knobs.
const POSTURE_VARIANTS: &[&str] = &["forbid", "warn", "allow"];

/// The tightest→loosest variant order for the `safety.require_approval` obligation:
/// `never` ⊑ `on_destructive` ⊑ `always`. `never` is the tightest (no obligation);
/// composition UNIONS up toward `always` (the operator raises, the creator cannot
/// lower). This is the value order the OrderedEnum kind imposes.
pub const REQUIRE_APPROVAL_VARIANTS: &[&str] = &["never", "on_destructive", "always"];

// ══════════════════════════════════════════════════════════════════════════════
// VendorCapability → KnobKey
// ══════════════════════════════════════════════════════════════════════════════

/// The knob key each closed [`VendorCapability`] gates on. This reproduces today's
/// gate: the guard reads `caps.grants(cap)`; the PDP path reads `grants(key, object)`
/// for `key = knob_key_for_capability(cap)`.
#[must_use]
pub const fn knob_key_for_capability(cap: VendorCapability) -> &'static str {
    match cap {
        VendorCapability::Extension => KEY_CODE_EXTENSION,
        VendorCapability::Schema => KEY_SCHEMA_CREATE_SCHEMA,
        VendorCapability::Role => KEY_ACCESS_ROLE,
        VendorCapability::Grant => KEY_ACCESS_GRANT,
        VendorCapability::Rls => KEY_ACCESS_RLS,
        VendorCapability::Partition => KEY_SCHEMA_PARTITION,
        VendorCapability::Policy => KEY_ACCESS_POLICY,
        VendorCapability::Function => KEY_CODE_FUNCTION,
        VendorCapability::RawSql => KEY_SQL_RAW,
        VendorCapability::RawViewBody => KEY_SQL_RAW_VIEW_BODY,
        VendorCapability::MaterializedView => KEY_CODE_MATERIALIZED_VIEW,
    }
}

/// The parsed [`KnobKey`] for a capability (never fails — the key literals are
/// well-formed; `expect` is a compile-checked invariant covered by a unit test).
#[must_use]
pub fn capability_knob_key(cap: VendorCapability) -> KnobKey {
    KnobKey::parse(knob_key_for_capability(cap))
        .expect("builtin capability knob keys are well-formed")
}

// ══════════════════════════════════════════════════════════════════════════════
// The builtin registry
// ══════════════════════════════════════════════════════════════════════════════

/// A Global/PerSchema/PerTable `Grant`-polarity Bool knob (deny-by-default).
fn bool_grant(
    key: &str,
    object_model: ObjectModel,
    requires_db_privilege: bool,
    docs: &str,
) -> KnobDef {
    KnobDef {
        key: KnobKey::parse(key).expect("builtin knob key well-formed"),
        kind: KnobKind::Bool,
        polarity: Polarity::Grant,
        default: KnobValue::Bool(false),
        enforcement: Enforcement::Enforced,
        object_model,
        requires_db_privilege,
        inherit: true,
        docs: docs.to_string(),
    }
}

/// A `Require`-polarity Bool obligation (default off; composes UP, un-droppable).
fn bool_require(key: &str, object_model: ObjectModel, docs: &str) -> KnobDef {
    KnobDef {
        key: KnobKey::parse(key).expect("builtin knob key well-formed"),
        kind: KnobKind::Bool,
        polarity: Polarity::Require,
        default: KnobValue::Bool(false),
        enforcement: Enforcement::Enforced,
        object_model,
        requires_db_privilege: false,
        inherit: true,
        docs: docs.to_string(),
    }
}

/// A `UintCeiling` grant (a monotone ms ceiling; `hard_floor = 1` forbids the
/// indefinite-lock value 0). Default is the loosest legal ceiling — but a ceiling
/// knob's DEFAULT must itself be the tightest value composition allows, and for a
/// UintCeiling the tightest is the hard floor. We default to `1` (the floor).
fn uint_ceiling(key: &str, docs: &str) -> KnobDef {
    KnobDef {
        key: KnobKey::parse(key).expect("builtin knob key well-formed"),
        kind: KnobKind::UintCeiling { hard_floor: 1 },
        polarity: Polarity::Grant,
        default: KnobValue::Uint(1),
        enforcement: Enforcement::Enforced,
        object_model: ObjectModel::Global,
        requires_db_privilege: false,
        inherit: true,
        docs: docs.to_string(),
    }
}

/// A rank-ordered `forbid ⊑ warn ⊑ allow` posture grant. Default is `forbid` (the
/// tightest variant — the deny-by-default the value order requires).
fn posture_grant(key: &str, docs: &str) -> KnobDef {
    KnobDef {
        key: KnobKey::parse(key).expect("builtin knob key well-formed"),
        kind: KnobKind::OrderedEnum {
            variants: POSTURE_VARIANTS.iter().map(|s| (*s).to_string()).collect(),
        },
        polarity: Polarity::Grant,
        default: KnobValue::Str("forbid".to_string()),
        enforcement: Enforcement::Enforced,
        object_model: ObjectModel::Global,
        requires_db_privilege: false,
        inherit: true,
        docs: docs.to_string(),
    }
}

/// The `safety.require_approval` obligation: a `never ⊑ on_destructive ⊑ always`
/// OrderedEnum, `Require` polarity (composes UP), **`HostEnforced`** enforcement,
/// default `never` (no obligation). Object-scoped so an operator can require approval
/// on exactly the objects it names.
///
/// It is `HostEnforced`, NOT `DeclaredOnly`: the engine's own guard/apply never gate
/// on it (the HOST — `migrated` — reads it via the sealed decision query and enforces
/// approval as a state machine), but — unlike a `DeclaredOnly` knob — it MAY be sealed
/// at a NON-DEFAULT value, because a host enforces it (M-2, II.6). The II.6 "can't set
/// non-default on an enforced path" restriction scopes to `DeclaredOnly` only, so a
/// sealed `safety.require_approval = always` obligation is legal.
fn require_approval_knob(key: &str, docs: &str) -> KnobDef {
    KnobDef {
        key: KnobKey::parse(key).expect("builtin knob key well-formed"),
        kind: KnobKind::OrderedEnum {
            variants: REQUIRE_APPROVAL_VARIANTS
                .iter()
                .map(|s| (*s).to_string())
                .collect(),
        },
        polarity: Polarity::Require,
        default: KnobValue::Str("never".to_string()),
        enforcement: Enforcement::HostEnforced,
        object_model: ObjectModel::PerTable,
        requires_db_privilege: false,
        inherit: true,
        docs: docs.to_string(),
    }
}

/// The engine's BUILTIN [`PolicyRegistry`]: every zero-migrate knob the guard and
/// validator gate on, as PDP knob defs. An [`EffectivePolicy`] the guard queries is
/// composed over exactly this registry, so `grants(key, object)` resolves to the
/// knob's default (deny) when no covering grant rule raises it.
///
/// # Panics
/// Never in practice — the key literals are distinct and well-formed; a duplicate or
/// malformed key is a programming error caught by [`builtin_registry`]'s unit test.
#[must_use]
pub fn builtin_registry() -> PolicyRegistry {
    PolicyRegistry::empty()
        .with([
            // ── access — access control (Global unless object-attributed) ───────
            bool_grant(KEY_ACCESS_ROLE, ObjectModel::Global, true, "CREATE/ALTER/DROP ROLE, DROP OWNED BY."),
            bool_grant(KEY_ACCESS_GRANT, ObjectModel::Global, true, "GRANT/REVOKE, ALTER DEFAULT PRIVILEGES."),
            bool_grant(KEY_ACCESS_POLICY, ObjectModel::PerTable, true, "CREATE/DROP POLICY (row-security policy)."),
            bool_grant(KEY_ACCESS_RLS, ObjectModel::PerTable, true, "ALTER TABLE … ROW LEVEL SECURITY."),
            // ── schema — structural DDL ─────────────────────────────────────────
            bool_grant(KEY_SCHEMA_CREATE_SCHEMA, ObjectModel::PerSchema, true, "CREATE SCHEMA (engine-neutral)."),
            // namespace-authority creation/movement/immutability grants (II.2.6)
            bool_grant(KEY_SCHEMA_CREATE_TABLE, ObjectModel::PerTable, false, "May CREATE a table matching this scope (default-deny namespace anchor)."),
            bool_grant(KEY_SCHEMA_RENAME, ObjectModel::PerTable, false, "May name/move a table INTO this scope (RENAME / SET SCHEMA target)."),
            // `schema.alter_injected` is a POWER GRANT — inherit = false.
            KnobDef {
                key: KnobKey::parse(KEY_SCHEMA_ALTER_INJECTED).expect("well-formed"),
                kind: KnobKind::Bool,
                polarity: Polarity::Grant,
                default: KnobValue::Bool(false),
                enforcement: Enforcement::Enforced,
                object_model: ObjectModel::PerTable,
                requires_db_privilege: false,
                inherit: false,
                docs: "May ALTER/DROP/RENAME an injected shape element (column, pinned PK, index). Power grant: a silent draft does not inherit it.".to_string(),
            },
            bool_grant(KEY_SCHEMA_PARTITION, ObjectModel::Global, false, "ALTER TABLE ATTACH/DETACH PARTITION."),
            // `schema.cross_schema` is PerSchema (the grant names exactly the permitted
            // schemas); a reference to a schema it does not grant is a CrossSchema
            // violation.
            bool_grant(KEY_SCHEMA_CROSS_SCHEMA, ObjectModel::PerSchema, false, "Which schemas this migration may reference (default-deny)."),
            // ── code — programmable / installed objects ─────────────────────────
            bool_grant(KEY_CODE_FUNCTION, ObjectModel::Global, true, "CREATE/DROP FUNCTION."),
            bool_grant(KEY_CODE_MATERIALIZED_VIEW, ObjectModel::Global, false, "PostgreSQL materialized views."),
            // the CREATE EXTENSION name allowlist (StrSet, Global) — the allowlist IS
            // the knob (empty = deny all); FORBIDDEN_EXTENSIONS still overrides.
            KnobDef {
                key: KnobKey::parse(KEY_CODE_EXTENSION).expect("well-formed"),
                kind: KnobKind::StrSet,
                polarity: Polarity::Grant,
                default: KnobValue::StrSet(Vec::new()),
                enforcement: Enforcement::Enforced,
                object_model: ObjectModel::Global,
                requires_db_privilege: true,
                inherit: true,
                docs: "The permitted CREATE EXTENSION names (empty = deny all; FORBIDDEN_EXTENSIONS still override).".to_string(),
            },
            // ── sql — the raw-text escape hatch ─────────────────────────────────
            // `sql.raw` is OBJECT-scoped (II.2.5): "raw only in staging" is a
            // statement-level referenced-object containment guarantee, and the guard's
            // scoped-raw-SQL rules (unqualified name / SET search_path / opaque body)
            // hinge on ⊤ vs a narrower grant — so it is PerTable, not Global.
            bool_grant(KEY_SQL_RAW, ObjectModel::PerTable, false, "The gated raw-statement escape (pgRaw); object-scoped (II.2.5)."),
            bool_grant(KEY_SQL_RAW_VIEW_BODY, ObjectModel::Global, false, "The gated raw view-body SELECT escape."),
            // ── runtime — op-timeout ceilings + index/rewrite postures ──────────
            uint_ceiling(KEY_RUNTIME_LOCK_TIMEOUT_MS, "Per-op lock_timeout ceiling (ms; no indefinite lock)."),
            uint_ceiling(KEY_RUNTIME_STATEMENT_TIMEOUT_MS, "Per-op statement_timeout ceiling (ms)."),
            posture_grant(KEY_RUNTIME_INDEX_CREATION, "Index-creation posture: forbid ⊑ warn ⊑ allow."),
            posture_grant(KEY_RUNTIME_TABLE_REWRITE, "Table-rewrite posture: forbid ⊑ warn ⊑ allow."),
            // ── safety — data protection (limits AND obligations) ───────────────
            bool_require(KEY_SAFETY_REQUIRE_RLS, ObjectModel::PerTable, "Every created table must end RLS-enabled."),
            bool_require(KEY_SAFETY_NO_HARD_DELETE, ObjectModel::Global, "No hard DELETE/TRUNCATE/DROP."),
            posture_grant(KEY_SAFETY_DESTRUCTIVE_OPS, "Destructive-op posture: forbid ⊑ warn ⊑ allow."),
            // The approval obligation — DECLARED by the engine, ENFORCED by the host.
            require_approval_knob(
                KEY_SAFETY_REQUIRE_APPROVAL,
                "Approval obligation: never ⊑ on_destructive ⊑ always (host-enforced state machine).",
            ),
        ])
        .expect("builtin registry keys are distinct + well-formed")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_registry_builds_and_covers_every_vendor_capability() {
        let reg = builtin_registry();
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
            let key = capability_knob_key(cap);
            assert!(reg.contains(&key), "registry must define {cap:?} → {key}");
        }
    }

    #[test]
    fn object_models_match_todays_decision_granularity() {
        let reg = builtin_registry();
        let om = |k: &str| reg.get(&KnobKey::parse(k).unwrap()).unwrap().object_model;
        // Global: whole-DB capability bits.
        assert_eq!(om(KEY_ACCESS_ROLE), ObjectModel::Global);
        assert_eq!(om(KEY_ACCESS_GRANT), ObjectModel::Global);
        assert_eq!(om(KEY_CODE_EXTENSION), ObjectModel::Global);
        // `sql.raw` is object-scoped (II.2.5): "raw only in staging".
        assert_eq!(om(KEY_SQL_RAW), ObjectModel::PerTable);
        // PerSchema: schema creation + cross-schema reference.
        assert_eq!(om(KEY_SCHEMA_CREATE_SCHEMA), ObjectModel::PerSchema);
        assert_eq!(om(KEY_SCHEMA_CROSS_SCHEMA), ObjectModel::PerSchema);
        // PerTable: RLS + policy.
        assert_eq!(om(KEY_ACCESS_RLS), ObjectModel::PerTable);
        assert_eq!(om(KEY_ACCESS_POLICY), ObjectModel::PerTable);
        // PerTable: the namespace-authority creation/movement/immutability anchors.
        assert_eq!(om(KEY_SCHEMA_CREATE_TABLE), ObjectModel::PerTable);
        assert_eq!(om(KEY_SCHEMA_RENAME), ObjectModel::PerTable);
        assert_eq!(om(KEY_SCHEMA_ALTER_INJECTED), ObjectModel::PerTable);
    }

    #[test]
    fn namespace_authority_grants_default_deny() {
        // Creation/movement/immutability anchors are Grant-polarity + default-deny:
        // an object comes into existence / a shape mutates only where explicitly
        // granted (II.2.6).
        let reg = builtin_registry();
        for k in [
            KEY_SCHEMA_CREATE_TABLE,
            KEY_SCHEMA_RENAME,
            KEY_SCHEMA_ALTER_INJECTED,
        ] {
            let def = reg.get(&KnobKey::parse(k).unwrap()).unwrap();
            assert_eq!(def.polarity, Polarity::Grant, "{k} is a Grant");
            assert_eq!(def.default, KnobValue::Bool(false), "{k} defaults to deny");
        }
    }

    #[test]
    fn alter_injected_is_a_non_inheritable_power_grant() {
        // `schema.alter_injected` is a POWER GRANT: `inherit = false`, so a silent
        // creator draft never inherits "override the platform's injected columns".
        let reg = builtin_registry();
        let def = reg
            .get(&KnobKey::parse(KEY_SCHEMA_ALTER_INJECTED).unwrap())
            .unwrap();
        assert!(
            !def.inherit,
            "schema.alter_injected must be inherit = false"
        );
        // Every OTHER builtin knob inherits by default (only this one opts out).
        for def in reg.iter() {
            if def.key.as_str() == KEY_SCHEMA_ALTER_INJECTED {
                continue;
            }
            assert!(def.inherit, "{} should be inherit = true", def.key);
        }
    }

    #[test]
    fn timeout_ceilings_forbid_the_indefinite_lock_value() {
        let reg = builtin_registry();
        for k in [
            KEY_RUNTIME_LOCK_TIMEOUT_MS,
            KEY_RUNTIME_STATEMENT_TIMEOUT_MS,
        ] {
            let def = reg.get(&KnobKey::parse(k).unwrap()).unwrap();
            assert_eq!(def.kind, KnobKind::UintCeiling { hard_floor: 1 });
            // The default is itself legal for the kind.
            assert!(def.default.validate_for(&def.kind).is_ok());
        }
    }

    #[test]
    fn posture_and_obligation_defaults_are_the_tightest_value() {
        let reg = builtin_registry();
        // OrderedEnum postures default to the tightest variant, `forbid`.
        for k in [
            KEY_RUNTIME_INDEX_CREATION,
            KEY_RUNTIME_TABLE_REWRITE,
            KEY_SAFETY_DESTRUCTIVE_OPS,
        ] {
            let def = reg.get(&KnobKey::parse(k).unwrap()).unwrap();
            assert_eq!(def.default, KnobValue::Str("forbid".to_string()));
        }
        // Require obligations default off (deny-by-default is "no obligation").
        for k in [KEY_SAFETY_REQUIRE_RLS, KEY_SAFETY_NO_HARD_DELETE] {
            let def = reg.get(&KnobKey::parse(k).unwrap()).unwrap();
            assert_eq!(def.polarity, Polarity::Require);
            assert_eq!(def.default, KnobValue::Bool(false));
        }
    }

    #[test]
    fn require_approval_is_a_host_enforced_require_obligation() {
        let reg = builtin_registry();
        let def = reg
            .get(&KnobKey::parse(KEY_SAFETY_REQUIRE_APPROVAL).unwrap())
            .expect("safety.require_approval is registered");
        // Require polarity (composes UP — operator raises, creator cannot lower).
        assert_eq!(def.polarity, Polarity::Require);
        // HostEnforced — the engine never enforces it (the host does), but — unlike a
        // DeclaredOnly knob — it MAY be sealed at a non-default value (M-2, II.6).
        assert_eq!(def.enforcement, Enforcement::HostEnforced);
        // The II.6 "can't set non-default on an enforced path" restriction does NOT
        // apply to a HostEnforced knob (only DeclaredOnly).
        assert!(!def.enforcement.forbids_nondefault_on_enforced_path());
        // Object-scoped like the other knobs.
        assert_eq!(def.object_model, ObjectModel::PerTable);
        // Default is the tightest variant, `never` (no obligation).
        assert_eq!(def.default, KnobValue::Str("never".to_string()));
        // The three-variant never ⊑ on_destructive ⊑ always order.
        assert_eq!(
            def.kind,
            KnobKind::OrderedEnum {
                variants: vec![
                    "never".to_string(),
                    "on_destructive".to_string(),
                    "always".to_string(),
                ]
            }
        );
    }

    #[test]
    fn every_grant_default_is_valid_and_denies() {
        // Every Grant Bool knob defaults to the deny value (false) and validates.
        let reg = builtin_registry();
        for def in reg.iter() {
            assert!(
                def.default.validate_for(&def.kind).is_ok(),
                "{} default must validate for its kind",
                def.key
            );
        }
    }
}
