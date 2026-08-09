//! The vendor-capability decision query: does the composed charter authorize the
//! privileged primitive an op renders?
//!
//! This is the policy-side peer of [`crate::capability::VendorCapabilities`]. That
//! type derives authority from a [`SchemaScope`](crate::policy::SchemaScope), which
//! answers a different question: which SCHEMAS a migration may touch. Deriving
//! `access.rls` from `schema.cross_schema` grants a charter authority it never
//! authored, so the lower gate asks the [`EffectivePolicy`] instead, at the knob
//! [`capability_knob_key`] names and at the object the op targets.
//!
//! Every capability knob but `code.extension` is a `Bool` grant. `code.extension` is
//! the `StrSet` allowlist whose non-emptiness IS the capability, which is how the
//! guard reads it too.

use zero_migrate_policy::{EffectivePolicy, GrantRegion, KnobKey, KnobValue, ObjectName};

use crate::capability::VendorCapability;
use crate::ir::Op;
use crate::policy_approval::concrete_object_for_op;
use crate::policy_registry::{capability_knob_key, KEY_CODE_EXTENSION};

/// The object a capability grant for `op` must cover, or `None` when the op names no
/// object the resolution can attribute the grant to.
///
/// `createSchema` / `dropSchema` name the schema they operate on in their own `name`
/// field rather than in a `schema()` qualifier, so the shared
/// [`concrete_object_for_op`] would resolve them at the migration's `default_schema` -
/// a different schema than the one the statement acts on, which is exactly the scope
/// erasure a `PerSchema` grant must not suffer. They are resolved at their own name.
///
/// Everything else uses the shared construction. Role, grant and extension ops carry
/// no object; they resolve to the `default_schema`, which their `Global` knobs make
/// harmless - a `Global` knob's rule is forced to `scope = all` at load, so it holds
/// the same value at every object.
#[must_use]
pub fn capability_object_for_op(op: &Op, default_schema: &str) -> Option<ObjectName> {
    match op {
        Op::CreateSchema { name, .. } | Op::DropSchema { name, .. } => {
            zero_migrate_policy::normalize_pg_identifier(name)
        }
        _ => concrete_object_for_op(op, default_schema),
    }
}

/// Does `effective` GRANT `capability` for a statement targeting `object`?
///
/// `object` is `None` when the target could not be named. Such a target is not
/// provably inside any narrower scope, so only a whole-universe grant reaches it -
/// the same fail-closed rule the raw-SQL guard applies to a statement whose relation
/// it cannot attribute.
#[must_use]
pub fn policy_grants_capability(
    effective: &EffectivePolicy,
    capability: VendorCapability,
    object: Option<&ObjectName>,
) -> bool {
    let key = capability_knob_key(capability);
    if key.as_str() == KEY_CODE_EXTENSION {
        // The allowlist IS the capability: empty means deny every CREATE/DROP
        // EXTENSION. `FORBIDDEN_EXTENSIONS` still decides which names may be created.
        return matches!(
            grant_value_at(effective, &key, object),
            Some(KnobValue::StrSet(names)) if !names.is_empty()
        );
    }
    matches!(
        grant_value_at(effective, &key, object),
        Some(KnobValue::Bool(true))
    )
}

/// The granted value of `key` at `object`, or - for a target that could not be named -
/// the value of a whole-universe ([`GrantRegion::Top`]) rule only. A rule narrower
/// than the universe answers `None` there, because the caller holds no object to test
/// it against.
fn grant_value_at(
    effective: &EffectivePolicy,
    key: &KnobKey,
    object: Option<&ObjectName>,
) -> Option<KnobValue> {
    match object {
        Some(object) => effective.grants(key, object),
        None => {
            if matches!(effective.grant_region(key), GrantRegion::Top) {
                effective.grants(key, &unnamed_target_witness())
            } else {
                None
            }
        }
    }
}

/// The stand-in object for reading a rule already established to hold everywhere.
/// Sound only under that condition, which [`grant_value_at`] checks via
/// [`EffectivePolicy::grant_region`] before it reaches here.
fn unnamed_target_witness() -> ObjectName {
    ObjectName::schema(b"zsg".to_vec())
}
