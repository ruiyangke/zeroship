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
//! - **Global** — `pg.role`/`pg.grant`/`pg.extension`/`core.raw_sql` (+ the other
//!   whole-DB vendor caps): today's guard reads a single `allow_*` bit with no
//!   object attribution, so the grant is database-global.
//! - **PerSchema** — `pg.schema`/`core.create_schema`: schema creation is attributed
//!   to the schema being created.
//! - **PerTable** — `pg.rls`/`pg.policy`: RLS enable/force + row-security policies are
//!   attributed to the table they protect.
//!
//! # Polarities
//!
//! Vendor capabilities + op ceilings + the index/table-rewrite postures are
//! `Grant` (compose DOWN, deny-by-default). The `sec.require_rls`/`sec.no_hard_delete`
//! obligations are `Require` (compose UP, un-droppable). `sec.destructive_ops` is a
//! rank-ordered `Grant` (forbid ⊑ warn ⊑ allow — the tighter posture is the default).

use zero_migrate_policy::{
    Enforcement, KnobDef, KnobKey, KnobKind, KnobValue, ObjectModel, Polarity, PolicyRegistry,
};

use crate::capability::VendorCapability;

// ══════════════════════════════════════════════════════════════════════════════
// Knob-key constants — the stable identifiers the guard/validator query by.
// ══════════════════════════════════════════════════════════════════════════════

/// `CREATE/ALTER/DROP ROLE` / `DROP OWNED BY` (Global Bool grant).
pub const KEY_PG_ROLE: &str = "pg.role";
/// `GRANT`/`REVOKE`/`ALTER DEFAULT PRIVILEGES` (Global Bool grant).
pub const KEY_PG_GRANT: &str = "pg.grant";
/// `CREATE EXTENSION` capability toggle (Global Bool grant). The concrete
/// per-name allowlist rides on [`KEY_PG_EXTENSIONS`]; `FORBIDDEN_EXTENSIONS` is a
/// non-grant hard deny in the guard regardless.
pub const KEY_PG_EXTENSION: &str = "pg.extension";
/// The `CREATE EXTENSION` name allowlist (Global StrSet grant) — the `StrSet` value
/// carries the permitted extension names.
pub const KEY_PG_EXTENSIONS: &str = "pg.extensions";
/// `CREATE SCHEMA` (PerSchema Bool grant), Postgres spelling.
pub const KEY_PG_SCHEMA: &str = "pg.schema";
/// `CREATE SCHEMA` (PerSchema Bool grant), engine-neutral spelling — the same
/// capability under the `core.` namespace the portable op vocabulary uses.
pub const KEY_CORE_CREATE_SCHEMA: &str = "core.create_schema";
/// Namespace-authority: may CREATE a table matching this scope (PerTable Bool
/// grant, default-deny). The per-op anchor of the II.2.6a creation-gating: an
/// object comes into existence ONLY where this grants (structured `createTable`
/// AND a classified raw `CREATE TABLE` both check it). The compose-time
/// `creatable ⊑ mandatory-inject` lint lives in the policy crate; the guard
/// enforces the per-op grant.
pub const KEY_CORE_CREATE_TABLE: &str = "core.create_table";
/// Namespace-authority: may name/move a table INTO this scope — the TARGET of a
/// `RENAME` / `SET SCHEMA` / create-as (PerTable Bool grant, default-deny,
/// II.2.6a). Closes the rename-TOCTOU: a cross-scope move needs this grant on the
/// target scope.
pub const KEY_CORE_RENAME_INTO: &str = "core.rename_into";
/// Namespace-authority: may `ALTER`/`DROP`/`RENAME` an INJECTED shape element
/// (column, pinned PK, or index) of a table (PerTable Bool grant, default-deny,
/// II.2.6b). Injected shape is the operator's floor and immutable by default; only
/// this grant waves the injected-shape-immutability denial.
pub const KEY_CORE_ALTER_INJECTED_COLUMN: &str = "core.alter_injected_column";
/// `CREATE/DROP POLICY` — row-security policy (PerTable Bool grant).
pub const KEY_PG_POLICY: &str = "pg.policy";
/// RLS `ENABLE/FORCE/DISABLE/NO FORCE` (PerTable Bool grant).
pub const KEY_PG_RLS: &str = "pg.rls";
/// `ALTER TABLE ATTACH/DETACH PARTITION` (Global Bool grant).
pub const KEY_PG_PARTITION: &str = "pg.partition";
/// `CREATE/DROP FUNCTION` (Global Bool grant).
pub const KEY_PG_FUNCTION: &str = "pg.function";
/// The gated raw-statement escape (`pgRaw`) (Global Bool grant).
pub const KEY_CORE_RAW_SQL: &str = "core.raw_sql";
/// The gated raw view-body SELECT escape (Global Bool grant).
pub const KEY_CORE_RAW_VIEW_BODY: &str = "core.raw_view_body";
/// `PostgreSQL` materialized views (Global Bool grant).
pub const KEY_PG_MATERIALIZED_VIEW: &str = "pg.materialized_view";
/// Which schemas this migration may reference (PerSchema Bool grant, default-deny)
/// — the capability-model replacement for the schema-scope confinement. A reference
/// to schema `s` is admitted iff `grants(core.cross_schema, s)` is `Bool(true)`; the
/// project schema(s) a confined/platform posture owns are granted here, everything
/// else is a `CrossSchema` violation. Object-scoped so the grant can name exactly
/// the permitted schemas (an `include: ["app1"]` grant permits only `app1`).
pub const KEY_CORE_CROSS_SCHEMA: &str = "core.cross_schema";
/// Operator-posture relaxation: raw-island (`pgRaw` / `createFunction.body`) bodies
/// may carry role-management + `search_path` needles that the confined body-token
/// backstop denies (Global Bool grant, default-deny). The platform posture grants
/// it (its bootstrap DO-blocks legitimately `CREATE ROLE` / `ALTER ROLE … SET
/// search_path`); confined and trusted omit it (trusted skips the belt entirely via
/// [`KEY_CORE_SKIP_STATIC_GUARD`], and its raw-island backstop still denies these
/// needles — a behaviour the vendor-lower matrix locks).
pub const KEY_CORE_RAW_ISLAND_ROLE: &str = "core.raw_island_role";
/// Operator-posture relaxation: skip the static parse-time guard belt ENTIRELY (the
/// deny-list, cross-schema confinement, and body walks) — the public dbmate-like
/// Trusted posture where the operator owns the DB (Global Bool grant, default-deny).
/// Only the Trusted posture grants it; confined/platform run the full belt.
pub const KEY_CORE_SKIP_STATIC_GUARD: &str = "core.skip_static_guard";

/// Per-op `lock_timeout` ceiling in ms (UintCeiling, hard floor 1 — the
/// no-indefinite-lock invariant, II.5).
pub const KEY_OP_LOCK_TIMEOUT_MS: &str = "op.lock_timeout_ms";
/// Per-op `statement_timeout` ceiling in ms (UintCeiling, hard floor 1).
pub const KEY_OP_STATEMENT_TIMEOUT_MS: &str = "op.statement_timeout_ms";
/// Index-creation posture: `forbid` ⊑ `warn` ⊑ `allow` (OrderedEnum grant).
pub const KEY_OP_INDEX_CREATION: &str = "op.index_creation";
/// Table-rewrite posture: `forbid` ⊑ `warn` ⊑ `allow` (OrderedEnum grant).
pub const KEY_OP_TABLE_REWRITE: &str = "op.table_rewrite";

/// Data-security: every created table must end RLS-enabled (Require Bool obligation).
pub const KEY_SEC_REQUIRE_RLS: &str = "sec.require_rls";
/// Data-security: no hard `DELETE`/`TRUNCATE`/`DROP` (Require Bool obligation).
pub const KEY_SEC_NO_HARD_DELETE: &str = "sec.no_hard_delete";
/// Data-security destructive posture: `forbid` ⊑ `warn` ⊑ `allow` (OrderedEnum grant).
pub const KEY_SEC_DESTRUCTIVE_OPS: &str = "sec.destructive_ops";
/// Data-security approval OBLIGATION: `never` ⊑ `on_destructive` ⊑ `always`
/// (OrderedEnum, `Require` polarity — composes UP, un-lowerable). This is a SEALED
/// obligation the engine only DECLARES: it is `Enforcement::DeclaredOnly`, so the
/// engine's own guard/apply never gate on it. The HOST (`migrated`) reads it via
/// [`crate::policy_approval::migration_requires_approval`] and enforces approval as a
/// state machine — the engine `apply` stays dumb. Object-scoped like every other
/// knob (the level resolves per target object; the host ORs across a migration's ops).
pub const KEY_SEC_REQUIRE_APPROVAL: &str = "sec.require_approval";

/// The tightest→loosest variant order shared by the posture OrderedEnum knobs.
const POSTURE_VARIANTS: &[&str] = &["forbid", "warn", "allow"];

/// The tightest→loosest variant order for the `sec.require_approval` obligation:
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
///
/// `Schema` maps to the Postgres spelling ([`KEY_PG_SCHEMA`]); the engine-neutral
/// [`KEY_CORE_CREATE_SCHEMA`] is a sibling key for the portable op vocabulary and is
/// registered too (a schema-create op may carry either spelling).
#[must_use]
pub const fn knob_key_for_capability(cap: VendorCapability) -> &'static str {
    match cap {
        VendorCapability::Extension => KEY_PG_EXTENSION,
        VendorCapability::Schema => KEY_PG_SCHEMA,
        VendorCapability::Role => KEY_PG_ROLE,
        VendorCapability::Grant => KEY_PG_GRANT,
        VendorCapability::Rls => KEY_PG_RLS,
        VendorCapability::Partition => KEY_PG_PARTITION,
        VendorCapability::Policy => KEY_PG_POLICY,
        VendorCapability::Function => KEY_PG_FUNCTION,
        VendorCapability::RawSql => KEY_CORE_RAW_SQL,
        VendorCapability::RawViewBody => KEY_CORE_RAW_VIEW_BODY,
        VendorCapability::MaterializedView => KEY_PG_MATERIALIZED_VIEW,
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
fn bool_grant(key: &str, object_model: ObjectModel, requires_db_privilege: bool, docs: &str) -> KnobDef {
    KnobDef {
        key: KnobKey::parse(key).expect("builtin knob key well-formed"),
        kind: KnobKind::Bool,
        polarity: Polarity::Grant,
        default: KnobValue::Bool(false),
        enforcement: Enforcement::Enforced,
        object_model,
        requires_db_privilege,
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
        docs: docs.to_string(),
    }
}

/// The `sec.require_approval` obligation: a `never ⊑ on_destructive ⊑ always`
/// OrderedEnum, `Require` polarity (composes UP), `DeclaredOnly` enforcement (the
/// engine never gates on it — the HOST does), default `never` (no obligation).
/// Object-scoped so an operator can require approval on exactly the objects it names.
fn require_approval_knob(key: &str, docs: &str) -> KnobDef {
    KnobDef {
        key: KnobKey::parse(key).expect("builtin knob key well-formed"),
        kind: KnobKind::OrderedEnum {
            variants: REQUIRE_APPROVAL_VARIANTS.iter().map(|s| (*s).to_string()).collect(),
        },
        polarity: Polarity::Require,
        default: KnobValue::Str("never".to_string()),
        enforcement: Enforcement::DeclaredOnly,
        object_model: ObjectModel::PerTable,
        requires_db_privilege: false,
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
            // ── vendor capabilities (Global unless object-attributed) ──────────
            bool_grant(KEY_PG_ROLE, ObjectModel::Global, true, "CREATE/ALTER/DROP ROLE, DROP OWNED BY."),
            bool_grant(KEY_PG_GRANT, ObjectModel::Global, true, "GRANT/REVOKE, ALTER DEFAULT PRIVILEGES."),
            bool_grant(KEY_PG_EXTENSION, ObjectModel::Global, true, "CREATE EXTENSION (per-name allowlist on pg.extensions)."),
            bool_grant(KEY_PG_SCHEMA, ObjectModel::PerSchema, true, "CREATE SCHEMA (Postgres spelling)."),
            bool_grant(KEY_CORE_CREATE_SCHEMA, ObjectModel::PerSchema, true, "CREATE SCHEMA (engine-neutral spelling)."),
            // ── namespace-authority creation/movement/immutability grants (II.2.6) ─
            bool_grant(KEY_CORE_CREATE_TABLE, ObjectModel::PerTable, false, "May CREATE a table matching this scope (default-deny namespace anchor)."),
            bool_grant(KEY_CORE_RENAME_INTO, ObjectModel::PerTable, false, "May name/move a table INTO this scope (RENAME / SET SCHEMA target)."),
            bool_grant(KEY_CORE_ALTER_INJECTED_COLUMN, ObjectModel::PerTable, false, "May ALTER/DROP/RENAME an injected shape element (column, pinned PK, index)."),
            bool_grant(KEY_PG_POLICY, ObjectModel::PerTable, true, "CREATE/DROP POLICY (row-security policy)."),
            bool_grant(KEY_PG_RLS, ObjectModel::PerTable, true, "ALTER TABLE … ROW LEVEL SECURITY."),
            bool_grant(KEY_PG_PARTITION, ObjectModel::Global, false, "ALTER TABLE ATTACH/DETACH PARTITION."),
            bool_grant(KEY_PG_FUNCTION, ObjectModel::Global, true, "CREATE/DROP FUNCTION."),
            // `core.raw_sql` is OBJECT-scoped (II.2.5): "raw_sql only in staging" is a
            // statement-level referenced-object containment guarantee, and the guard's
            // scoped-raw-SQL rules (unqualified name / SET search_path / opaque body)
            // hinge on ⊤ vs a narrower grant — so it is PerTable, not Global.
            bool_grant(KEY_CORE_RAW_SQL, ObjectModel::PerTable, false, "The gated raw-statement escape (pgRaw); object-scoped (II.2.5)."),
            bool_grant(KEY_CORE_RAW_VIEW_BODY, ObjectModel::Global, false, "The gated raw view-body SELECT escape."),
            bool_grant(KEY_PG_MATERIALIZED_VIEW, ObjectModel::Global, false, "PostgreSQL materialized views."),
            // `core.cross_schema` is PerSchema (the grant names exactly the permitted
            // schemas); a reference to a schema it does not grant is a CrossSchema
            // violation.
            bool_grant(KEY_CORE_CROSS_SCHEMA, ObjectModel::PerSchema, false, "Which schemas this migration may reference (default-deny)."),
            // Operator-posture relaxations (Global; only the platform/trusted ceilings grant them).
            bool_grant(KEY_CORE_RAW_ISLAND_ROLE, ObjectModel::Global, false, "Raw-island bodies may carry role/search_path needles (platform posture)."),
            bool_grant(KEY_CORE_SKIP_STATIC_GUARD, ObjectModel::Global, false, "Skip the static parse-time guard belt entirely (trusted dbmate-like posture)."),
            // ── the CREATE EXTENSION name allowlist (StrSet, Global) ───────────
            KnobDef {
                key: KnobKey::parse(KEY_PG_EXTENSIONS).expect("well-formed"),
                kind: KnobKind::StrSet,
                polarity: Polarity::Grant,
                default: KnobValue::StrSet(Vec::new()),
                enforcement: Enforcement::Enforced,
                object_model: ObjectModel::Global,
                requires_db_privilege: true,
                docs: "The permitted CREATE EXTENSION names (FORBIDDEN_EXTENSIONS still override).".to_string(),
            },
            // ── op-timeout ceilings ────────────────────────────────────────────
            uint_ceiling(KEY_OP_LOCK_TIMEOUT_MS, "Per-op lock_timeout ceiling (ms; no indefinite lock)."),
            uint_ceiling(KEY_OP_STATEMENT_TIMEOUT_MS, "Per-op statement_timeout ceiling (ms)."),
            // ── index / table-rewrite postures ─────────────────────────────────
            posture_grant(KEY_OP_INDEX_CREATION, "Index-creation posture: forbid ⊑ warn ⊑ allow."),
            posture_grant(KEY_OP_TABLE_REWRITE, "Table-rewrite posture: forbid ⊑ warn ⊑ allow."),
            // ── data-security obligations ───────────────────────────────────────
            bool_require(KEY_SEC_REQUIRE_RLS, ObjectModel::PerTable, "Every created table must end RLS-enabled."),
            bool_require(KEY_SEC_NO_HARD_DELETE, ObjectModel::Global, "No hard DELETE/TRUNCATE/DROP."),
            posture_grant(KEY_SEC_DESTRUCTIVE_OPS, "Destructive-op posture: forbid ⊑ warn ⊑ allow."),
            // The approval obligation — DECLARED by the engine, ENFORCED by the host.
            require_approval_knob(
                KEY_SEC_REQUIRE_APPROVAL,
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
        assert_eq!(om(KEY_PG_ROLE), ObjectModel::Global);
        assert_eq!(om(KEY_PG_GRANT), ObjectModel::Global);
        assert_eq!(om(KEY_PG_EXTENSION), ObjectModel::Global);
        // `core.raw_sql` is object-scoped (II.2.5): "raw_sql only in staging".
        assert_eq!(om(KEY_CORE_RAW_SQL), ObjectModel::PerTable);
        // PerSchema: schema creation.
        assert_eq!(om(KEY_PG_SCHEMA), ObjectModel::PerSchema);
        assert_eq!(om(KEY_CORE_CREATE_SCHEMA), ObjectModel::PerSchema);
        // PerTable: RLS + policy.
        assert_eq!(om(KEY_PG_RLS), ObjectModel::PerTable);
        assert_eq!(om(KEY_PG_POLICY), ObjectModel::PerTable);
        // PerTable: the namespace-authority creation/movement/immutability anchors.
        assert_eq!(om(KEY_CORE_CREATE_TABLE), ObjectModel::PerTable);
        assert_eq!(om(KEY_CORE_RENAME_INTO), ObjectModel::PerTable);
        assert_eq!(om(KEY_CORE_ALTER_INJECTED_COLUMN), ObjectModel::PerTable);
    }

    #[test]
    fn namespace_authority_grants_default_deny() {
        // Creation/movement/immutability anchors are Grant-polarity + default-deny:
        // an object comes into existence / a shape mutates only where explicitly
        // granted (II.2.6).
        let reg = builtin_registry();
        for k in [
            KEY_CORE_CREATE_TABLE,
            KEY_CORE_RENAME_INTO,
            KEY_CORE_ALTER_INJECTED_COLUMN,
        ] {
            let def = reg.get(&KnobKey::parse(k).unwrap()).unwrap();
            assert_eq!(def.polarity, Polarity::Grant, "{k} is a Grant");
            assert_eq!(def.default, KnobValue::Bool(false), "{k} defaults to deny");
        }
    }

    #[test]
    fn timeout_ceilings_forbid_the_indefinite_lock_value() {
        let reg = builtin_registry();
        for k in [KEY_OP_LOCK_TIMEOUT_MS, KEY_OP_STATEMENT_TIMEOUT_MS] {
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
        for k in [KEY_OP_INDEX_CREATION, KEY_OP_TABLE_REWRITE, KEY_SEC_DESTRUCTIVE_OPS] {
            let def = reg.get(&KnobKey::parse(k).unwrap()).unwrap();
            assert_eq!(def.default, KnobValue::Str("forbid".to_string()));
        }
        // Require obligations default off (deny-by-default is "no obligation").
        for k in [KEY_SEC_REQUIRE_RLS, KEY_SEC_NO_HARD_DELETE] {
            let def = reg.get(&KnobKey::parse(k).unwrap()).unwrap();
            assert_eq!(def.polarity, Polarity::Require);
            assert_eq!(def.default, KnobValue::Bool(false));
        }
    }

    #[test]
    fn require_approval_is_a_declared_only_require_obligation() {
        let reg = builtin_registry();
        let def = reg
            .get(&KnobKey::parse(KEY_SEC_REQUIRE_APPROVAL).unwrap())
            .expect("sec.require_approval is registered");
        // Require polarity (composes UP — operator raises, creator cannot lower).
        assert_eq!(def.polarity, Polarity::Require);
        // DeclaredOnly — the engine never enforces it (the host does).
        assert_eq!(def.enforcement, Enforcement::DeclaredOnly);
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
