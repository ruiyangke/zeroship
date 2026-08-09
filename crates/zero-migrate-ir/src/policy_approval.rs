//! The `safety.require_approval` decision query — the host-facing side of the SEALED
//! approval obligation.
//!
//! Approval is a **sealed policy obligation the engine does not enforce but a HOST
//! does** ([`crate::policy_registry::KEY_SAFETY_REQUIRE_APPROVAL`],
//! `Enforcement::HostEnforced` — so it may be sealed at a non-default value, M-2).
//! The engine `apply` stays a dumb executor; it never gates on this knob. Instead the
//! HOST (`migrated`) composes the effective policy, asks
//! [`migration_requires_approval`] whether the migration's ops require approval, and
//! enforces the answer as a state machine (a status column + `approved_checksum`).
//!
//! This module is the bridge between the two crates that must both be in scope for the
//! query — the PDP [`EffectivePolicy`] (from `zero-migrate-policy`) and the [`Op`]
//! vocabulary (from this crate). It resolves the `safety.require_approval` LEVEL per
//! target object (the knob is object-scoped) and ORs the per-op requirement across the
//! whole migration:
//!
//! - `never` → never requires approval;
//! - `always` → always requires approval;
//! - `on_destructive` → requires approval iff the op is [`Op::is_destructive`] (the
//!   same destructive notion the guard's `safety.destructive_ops` classifier uses).

use zero_migrate_policy::{
    normalize_pg_identifier, EffectivePolicy, KnobKey, KnobValue, ObjectName,
};

use crate::ir::Op;
use crate::policy_registry::{KEY_SAFETY_REQUIRE_APPROVAL, REQUIRE_APPROVAL_VARIANTS};

/// The resolved `safety.require_approval` obligation level at one object, tightest→
/// loosest: `never` ⊑ `on_destructive` ⊑ `always`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalLevel {
    /// No approval obligation (the default / tightest).
    Never,
    /// Approval required iff the op is destructive.
    OnDestructive,
    /// Approval required for every op (loosest).
    Always,
}

impl ApprovalLevel {
    /// Parse one `safety.require_approval` variant string. An unknown variant fails
    /// CLOSED to the safest interpretation for an obligation — `Always` (require
    /// approval) — so a policy shape the host does not understand never silently
    /// un-gates a migration.
    fn from_variant(v: &str) -> Self {
        match v {
            "never" => Self::Never,
            "on_destructive" => Self::OnDestructive,
            "always" => Self::Always,
            _ => Self::Always,
        }
    }

    /// The 0-based rank in the tightest→loosest order (the `Require` value order:
    /// composition unions UP toward the loosest, so a higher rank wins).
    const fn rank(self) -> u8 {
        match self {
            Self::Never => 0,
            Self::OnDestructive => 1,
            Self::Always => 2,
        }
    }

    /// The LOOSER (higher-obligation) of two levels — the `Require` union-up meet used
    /// to fold the multiple covering obligation rules at one object into one level.
    #[must_use]
    fn loosest(self, other: Self) -> Self {
        if self.rank() >= other.rank() {
            self
        } else {
            other
        }
    }
}

/// The concrete object a single op targets, for the object-scoped `require_approval`
/// resolution. A table op resolves to `default_schema.table` (or its own schema
/// qualifier when present); a schema/namespace-level op with no table resolves to the
/// schema it names, else the migration's `default_schema`. Identifiers are PG-folded
/// via [`normalize_pg_identifier`] so the resolution matches the composer's scope
/// matcher exactly (an un-normalizable name fails closed to the `default_schema`
/// object).
fn object_for_op(op: &Op, default_schema: &str) -> ObjectName {
    concrete_object_for_op(op, default_schema).unwrap_or_else(|| {
        normalize_pg_identifier(default_schema)
            .unwrap_or_else(|| ObjectName::schema(default_schema.as_bytes().to_vec()))
    })
}

/// The concrete object `op` targets, or `None` when the name it carries does not
/// normalize - the shared construction behind both object-scoped policy queries on the
/// IR path: `object_for_op` for the approval obligation (module-private, so not a
/// linkable doc target), and the capability resolution in [`crate::policy_capability`].
///
/// Callers pick their own answer to an un-normalizable name: the approval query falls
/// back to the `default_schema` object, while a capability grant fails closed, because
/// a target that cannot be named is not provably inside any narrower scope.
///
/// A table op resolves to `default_schema.table` (or its own schema qualifier when
/// present); an op with no table resolves to the schema it names, else the migration's
/// `default_schema`. Identifiers are PG-folded via [`normalize_pg_identifier`] so the
/// resolution matches the composer's scope matcher exactly.
#[must_use]
pub fn concrete_object_for_op(op: &Op, default_schema: &str) -> Option<ObjectName> {
    let schema = op.schema().unwrap_or(default_schema);
    if let Some(table) = op.touched_table() {
        return normalize_pg_identifier(&format!("{schema}.{table}"));
    }
    // A schema/role/db-level op (DropSchema, roles, grants, raw islands): resolve to
    // the schema it names when it carries one, else the migration's default schema.
    normalize_pg_identifier(schema)
}

/// Resolve the effective `safety.require_approval` level at `object`: the LOOSEST
/// (highest-obligation) covering require rule's value, or [`ApprovalLevel::Never`]
/// when no rule covers it (the knob default). This mirrors the PDP obligation
/// union-up: [`EffectivePolicy::obligations`] returns every covering require rule at
/// the object, and a `Require` obligation composes toward its loosest covering value.
#[must_use]
pub fn require_approval_level(effective: &EffectivePolicy, object: &ObjectName) -> ApprovalLevel {
    let key = approval_key();
    let mut level = ApprovalLevel::Never;
    for (k, v) in effective.obligations(object) {
        if k == key {
            if let KnobValue::Str(variant) = v {
                level = level.loosest(ApprovalLevel::from_variant(&variant));
            }
        }
    }
    level
}

/// Does the given migration (its `ops`, targeting `default_schema`) require operator
/// approval under the composed `effective` policy?
///
/// The per-op requirement is the object-scoped `safety.require_approval` level resolved
/// at the op's target object, applied to the op's destructiveness:
/// `never`→false, `always`→true, `on_destructive`→[`Op::is_destructive`]. The
/// migration requires approval iff ANY op does (OR across ops). Empty ops → false.
#[must_use]
pub fn migration_requires_approval(
    effective: &EffectivePolicy,
    ops: &[Op],
    default_schema: &str,
) -> bool {
    ops.iter().any(|op| {
        let object = object_for_op(op, default_schema);
        match require_approval_level(effective, &object) {
            ApprovalLevel::Never => false,
            ApprovalLevel::Always => true,
            ApprovalLevel::OnDestructive => op.is_destructive(),
        }
    })
}

fn approval_key() -> KnobKey {
    KnobKey::parse(KEY_SAFETY_REQUIRE_APPROVAL)
        .expect("safety.require_approval builtin knob key is well-formed")
}

/// The `safety.require_approval` variant strings, re-exported for hosts that author the
/// obligation (kept in lockstep with [`ApprovalLevel`] via a compile-time check).
#[must_use]
pub const fn variants() -> &'static [&'static str] {
    REQUIRE_APPROVAL_VARIANTS
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy_registry::builtin_registry;
    use zero_migrate_policy::{admit, LoadContext, PolicyDoc, RootCharter};

    /// Compose an [`EffectivePolicy`] over the builtin registry from a charter TOML
    /// (the operator obligation) against an empty creator draft.
    fn effective_from_charter(charter_toml: &str) -> EffectivePolicy {
        let reg = builtin_registry();
        let root = RootCharter::parse_toml(charter_toml, &reg).expect("charter parses");
        let empty_draft =
            PolicyDoc::parse_toml("policy_version = 1\n", &reg, LoadContext::NonRootLayer)
                .expect("empty draft parses");
        admit(&root, &empty_draft, &reg).expect("composes")
    }

    fn drop_table(schema: &str, table: &str) -> Op {
        Op::DropTable {
            table: table.to_string(),
            cascade: None,
            schema: Some(schema.to_string()),
            existence_guard: None,
        }
    }

    fn add_column(schema: &str, table: &str) -> Op {
        Op::AddColumn {
            table: table.to_string(),
            column: "c".to_string(),
            ty: crate::ir::ColType::Text,
            nullable: Some(true),
            default: None,
            value_format: None,
            vector_metric: None,
            case_sensitive: None,
            mask: None,
            generated: None,
            identity: None,
            schema: Some(schema.to_string()),
            existence_guard: None,
        }
    }

    #[test]
    fn never_requires_no_approval() {
        // No require_approval rule at all → default `never`.
        let ep = effective_from_charter("policy_version = 1\n");
        assert!(!migration_requires_approval(
            &ep,
            &[drop_table("app_main", "t"), add_column("app_main", "t")],
            "app_main"
        ));
    }

    #[test]
    fn always_requires_approval_for_any_op() {
        let ep = effective_from_charter(
            "policy_version = 1\n[[require]]\nkey = \"safety.require_approval\"\nvalue = \"always\"\nscope = \"all\"\n",
        );
        // Even a purely additive op requires approval under `always`.
        assert!(migration_requires_approval(
            &ep,
            &[add_column("app_main", "t")],
            "app_main"
        ));
    }

    #[test]
    fn on_destructive_gates_only_destructive_ops() {
        let ep = effective_from_charter(
            "policy_version = 1\n[[require]]\nkey = \"safety.require_approval\"\nvalue = \"on_destructive\"\nscope = \"all\"\n",
        );
        // Additive-only migration → no approval.
        assert!(!migration_requires_approval(
            &ep,
            &[add_column("app_main", "t")],
            "app_main"
        ));
        // A destructive op in the mix → approval required.
        assert!(migration_requires_approval(
            &ep,
            &[add_column("app_main", "t"), drop_table("app_main", "old")],
            "app_main"
        ));
    }

    #[test]
    fn level_resolves_per_object_scope() {
        // `always` scoped ONLY to app_secret.*; app_main is left at the default `never`.
        let ep = effective_from_charter(
            "policy_version = 1\n[[require]]\nkey = \"safety.require_approval\"\nvalue = \"always\"\nscope = { include = [\"app_secret\"] }\n",
        );
        // An additive op on the unscoped schema → no approval.
        assert!(!migration_requires_approval(
            &ep,
            &[add_column("app_main", "t")],
            "app_main"
        ));
        // The same op on the scoped schema → approval (always).
        assert!(migration_requires_approval(
            &ep,
            &[add_column("app_secret", "t")],
            "app_secret"
        ));
    }

    #[test]
    fn empty_ops_never_require_approval() {
        let ep = effective_from_charter(
            "policy_version = 1\n[[require]]\nkey = \"safety.require_approval\"\nvalue = \"always\"\nscope = \"all\"\n",
        );
        assert!(!migration_requires_approval(&ep, &[], "app_main"));
    }

    #[test]
    fn destructive_classification_tracks_the_op_set() {
        assert!(drop_table("s", "t").is_destructive());
        assert!(!add_column("s", "t").is_destructive());
        // A DROP COLUMN is destructive (guard: DROP COLUMN).
        assert!(Op::DropColumn {
            table: "t".to_string(),
            column: "c".to_string(),
            schema: None,
            existence_guard: None,
        }
        .is_destructive());
        // A DROP INDEX is NON-destructive at the SQL level (guard treats it as
        // reversible structure).
        assert!(!Op::DropIndex {
            name: "i".to_string(),
            table: None,
            schema: None,
            unique: None,
            existence_guard: None,
            concurrently: None,
        }
        .is_destructive());
    }
}
