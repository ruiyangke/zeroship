//! Seal round-trip + tamper-detection suite (§II.7). Proves the seal binds the
//! resolved rule set, the registry digest, the `(dialect, matcher_version)` matcher
//! semantics, and the ceiling version — and HARD-FAILS on any mismatch.
//!
//! The seal MACs a FRESHLY-composed [`EffectivePolicy`]; "tampering" means presenting
//! a DIFFERENT composed policy (a mutated rule/scope) to `verify`, which must fail
//! the tag. Because `EffectivePolicy` is unforgeable, we tamper by composing a
//! different document — exactly what an attacker who swapped the sealed policy would
//! present.

use zero_migrate_policy::{
    compose_strict, seal, Enforcement, KnobDef, KnobKey, KnobKind, KnobValue, PolicyDoc,
    PolicyRegistry, Polarity, RootCeiling, SealError,
};

const MAC_KEY: &[u8] = b"a-shared-fleet-mac-key-32-bytes!!";
const DIALECT: &str = "postgres";
const MATCHER: u32 = 1;
const CEILING_VER: u64 = 7;

fn def(key: &str, kind: KnobKind, polarity: Polarity, default: KnobValue) -> KnobDef {
    KnobDef {
        key: KnobKey::parse(key).unwrap(),
        kind,
        polarity,
        default,
        enforcement: Enforcement::Enforced,
        object_model: zero_migrate_policy::ObjectModel::PerTable,
        requires_db_privilege: false,
        docs: String::new(),
    }
}

fn registry() -> PolicyRegistry {
    PolicyRegistry::empty()
        .with([
            def("core.raw_sql", KnobKind::Bool, Polarity::Grant, KnobValue::Bool(false)),
            def(
                "op.lock_timeout_ms",
                KnobKind::UintCeiling { hard_floor: 1 },
                Polarity::Grant,
                KnobValue::Uint(1),
            ),
            def("sec.require_rls", KnobKind::Bool, Polarity::Require, KnobValue::Bool(false)),
        ])
        .unwrap()
}

/// A registry that differs only in a knob's `requires_db_privilege` — same keys, so
/// a document loads identically, but the digest differs (II.2.1).
fn registry_flipped_privilege() -> PolicyRegistry {
    PolicyRegistry::empty()
        .with([
            {
                let mut d = def("core.raw_sql", KnobKind::Bool, Polarity::Grant, KnobValue::Bool(false));
                d.requires_db_privilege = true;
                d
            },
            def(
                "op.lock_timeout_ms",
                KnobKind::UintCeiling { hard_floor: 1 },
                Polarity::Grant,
                KnobValue::Uint(1),
            ),
            def("sec.require_rls", KnobKind::Bool, Polarity::Require, KnobValue::Bool(false)),
        ])
        .unwrap()
}

const ROOT_TOML: &str = r#"policy_version = 1
[[grant]]
key = "core.raw_sql"
value = true
scope = { include = ["app_*"] }
[[grant]]
key = "op.lock_timeout_ms"
value = 600
scope = { include = ["app_*"] }
[[require]]
key = "sec.require_rls"
value = true
scope = { include = ["app_*"] }
[[inject]]
scope = { include = ["app_*"] }
columns = [ { name = "created_at", type = "timestamptz", nullable = false } ]
[[validate]]
scope = { include = ["app_*"] }
predicate = { kind = "has_primary_key" }
"#;

/// Compose the reference policy: the root ceiling against an empty draft (all ceiling
/// rules survive union-up; the draft adds no grants so nothing escalates).
fn reference_policy(reg: &PolicyRegistry) -> zero_migrate_policy::EffectivePolicy {
    let root = RootCeiling::parse_toml(ROOT_TOML, reg).unwrap();
    let draft = PolicyDoc::parse_toml(
        "policy_version = 1\n",
        reg,
        zero_migrate_policy::LoadContext::NonRootLayer,
    )
    .unwrap();
    compose_strict(&root, &draft, reg).unwrap()
}

// ══════════════════════════════════════════════════════════════════════════════
// round-trip
// ══════════════════════════════════════════════════════════════════════════════

#[test]
fn good_seal_round_trips() {
    let reg = registry();
    let policy = reference_policy(&reg);
    let sealed = seal(&policy, MAC_KEY, [7u8; 16], DIALECT, MATCHER, CEILING_VER);

    // A freshly-composed identical policy verifies.
    let fresh = reference_policy(&reg);
    assert_eq!(
        sealed.verify(MAC_KEY, &fresh, &reg.digest(), DIALECT, MATCHER, CEILING_VER),
        Ok(())
    );
}

#[test]
fn wrong_mac_key_fails() {
    let reg = registry();
    let policy = reference_policy(&reg);
    let sealed = seal(&policy, MAC_KEY, [7u8; 16], DIALECT, MATCHER, CEILING_VER);
    let fresh = reference_policy(&reg);
    assert_eq!(
        sealed.verify(b"a-different-mac-key-of-length-32!", &fresh, &reg.digest(), DIALECT, MATCHER, CEILING_VER),
        Err(SealError::TagMismatch)
    );
}

// ══════════════════════════════════════════════════════════════════════════════
// tamper: a mutated rule / scope fails the tag
// ══════════════════════════════════════════════════════════════════════════════

#[test]
fn tampered_grant_value_fails() {
    // The effective grant map in strict ingress is the DRAFT's (bounded by the
    // ceiling). So a grant-value tamper means the DRAFT carries a different value.
    let reg = registry();
    let root = RootCeiling::parse_toml(ROOT_TOML, &reg).unwrap();

    // Draft grants timeout 600 @ app_* (within the ceiling's 600 @ app_*) → seal it.
    let draft_600 = PolicyDoc::parse_toml(
        r#"policy_version = 1
[[grant]]
key = "op.lock_timeout_ms"
value = 600
scope = { include = ["app_*"] }
"#,
        &reg,
        zero_migrate_policy::LoadContext::NonRootLayer,
    )
    .unwrap();
    let policy_600 = compose_strict(&root, &draft_600, &reg).unwrap();
    let sealed = seal(&policy_600, MAC_KEY, [1u8; 16], DIALECT, MATCHER, CEILING_VER);

    // A tightened tamper: draft 60 @ app_* — a DIFFERENT effective grant map.
    let draft_60 = PolicyDoc::parse_toml(
        r#"policy_version = 1
[[grant]]
key = "op.lock_timeout_ms"
value = 60
scope = { include = ["app_*"] }
"#,
        &reg,
        zero_migrate_policy::LoadContext::NonRootLayer,
    )
    .unwrap();
    let tampered = compose_strict(&root, &draft_60, &reg).unwrap();

    assert_eq!(
        sealed.verify(MAC_KEY, &tampered, &reg.digest(), DIALECT, MATCHER, CEILING_VER),
        Err(SealError::TagMismatch)
    );
}

#[test]
fn tampered_scope_fails() {
    // Tamper a UNION-UP (ceiling-sourced) rule's scope: the inject scope. It flows
    // into the sealed rule set verbatim, so narrowing it changes the canonical bytes.
    let reg = registry();
    let sealed = seal(&reference_policy(&reg), MAC_KEY, [2u8; 16], DIALECT, MATCHER, CEILING_VER);

    // Narrow ONLY the inject scope (app_* → app_main). The [[inject]] block is the
    // only one whose `include` we rewrite here.
    let tampered_root = ROOT_TOML.replace(
        "[[inject]]\nscope = { include = [\"app_*\"] }",
        "[[inject]]\nscope = { include = [\"app_main\"] }",
    );
    assert_ne!(tampered_root, ROOT_TOML, "the inject-scope rewrite must land");
    let root = RootCeiling::parse_toml(&tampered_root, &reg).unwrap();
    let draft = PolicyDoc::parse_toml(
        "policy_version = 1\n",
        &reg,
        zero_migrate_policy::LoadContext::NonRootLayer,
    )
    .unwrap();
    let tampered = compose_strict(&root, &draft, &reg).unwrap();

    assert_eq!(
        sealed.verify(MAC_KEY, &tampered, &reg.digest(), DIALECT, MATCHER, CEILING_VER),
        Err(SealError::TagMismatch)
    );
}

#[test]
fn tampered_inject_column_fails() {
    let reg = registry();
    let sealed = seal(&reference_policy(&reg), MAC_KEY, [3u8; 16], DIALECT, MATCHER, CEILING_VER);

    // Inject a differently-typed column — a different rule set.
    let tampered_root = ROOT_TOML.replace("timestamptz", "text");
    let root = RootCeiling::parse_toml(&tampered_root, &reg).unwrap();
    let draft = PolicyDoc::parse_toml(
        "policy_version = 1\n",
        &reg,
        zero_migrate_policy::LoadContext::NonRootLayer,
    )
    .unwrap();
    let tampered = compose_strict(&root, &draft, &reg).unwrap();

    assert_eq!(
        sealed.verify(MAC_KEY, &tampered, &reg.digest(), DIALECT, MATCHER, CEILING_VER),
        Err(SealError::TagMismatch)
    );
}

// ══════════════════════════════════════════════════════════════════════════════
// binding: registry digest / dialect / matcher / ceiling version
// ══════════════════════════════════════════════════════════════════════════════

#[test]
fn wrong_registry_digest_fails() {
    let reg = registry();
    let sealed = seal(&reference_policy(&reg), MAC_KEY, [4u8; 16], DIALECT, MATCHER, CEILING_VER);

    // Verify presenting a DIFFERENT registry digest (a flipped requires_db_privilege
    // — same keys, different enforcement semantics) → hard fail (II.2.1).
    let flipped = registry_flipped_privilege();
    let fresh = reference_policy(&reg);
    assert_eq!(
        sealed.verify(MAC_KEY, &fresh, &flipped.digest(), DIALECT, MATCHER, CEILING_VER),
        Err(SealError::RegistryDigestMismatch)
    );
    // And the digests genuinely differ.
    assert_ne!(reg.digest(), flipped.digest());
}

#[test]
fn wrong_dialect_fails() {
    let reg = registry();
    let sealed = seal(&reference_policy(&reg), MAC_KEY, [5u8; 16], DIALECT, MATCHER, CEILING_VER);
    let fresh = reference_policy(&reg);
    assert_eq!(
        sealed.verify(MAC_KEY, &fresh, &reg.digest(), "mysql", MATCHER, CEILING_VER),
        Err(SealError::DialectMismatch)
    );
}

#[test]
fn wrong_matcher_version_fails() {
    let reg = registry();
    let sealed = seal(&reference_policy(&reg), MAC_KEY, [6u8; 16], DIALECT, MATCHER, CEILING_VER);
    let fresh = reference_policy(&reg);
    assert_eq!(
        sealed.verify(MAC_KEY, &fresh, &reg.digest(), DIALECT, MATCHER + 1, CEILING_VER),
        Err(SealError::MatcherVersionMismatch)
    );
}

#[test]
fn wrong_ceiling_version_fails() {
    let reg = registry();
    let sealed = seal(&reference_policy(&reg), MAC_KEY, [8u8; 16], DIALECT, MATCHER, CEILING_VER);
    let fresh = reference_policy(&reg);
    assert_eq!(
        sealed.verify(MAC_KEY, &fresh, &reg.digest(), DIALECT, MATCHER, CEILING_VER + 1),
        Err(SealError::CeilingVersionMismatch)
    );
}

#[test]
fn nonce_swap_invalidates_tag() {
    let reg = registry();
    let policy = reference_policy(&reg);
    let s1 = seal(&policy, MAC_KEY, [1u8; 16], DIALECT, MATCHER, CEILING_VER);
    let s2 = seal(&policy, MAC_KEY, [2u8; 16], DIALECT, MATCHER, CEILING_VER);
    // Same policy + binding, different nonce → different tags (nonce is MAC'd).
    assert_ne!(s1, s2);
}

// ══════════════════════════════════════════════════════════════════════════════
// inject total order is sealed (reorder → tag mismatch)
// ══════════════════════════════════════════════════════════════════════════════

#[test]
fn inject_reorder_changes_tag() {
    let reg = registry();
    // Two injects in one order.
    let a_toml = r#"policy_version = 1
[[inject]]
scope = { include = ["app_*"] }
columns = [ { name = "created_at", type = "timestamptz", nullable = false } ]
[[inject]]
scope = { include = ["app_*"] }
columns = [ { name = "updated_at", type = "timestamptz", nullable = false } ]
"#;
    // The reversed order — a different sealed total order.
    let b_toml = r#"policy_version = 1
[[inject]]
scope = { include = ["app_*"] }
columns = [ { name = "updated_at", type = "timestamptz", nullable = false } ]
[[inject]]
scope = { include = ["app_*"] }
columns = [ { name = "created_at", type = "timestamptz", nullable = false } ]
"#;
    let empty = PolicyDoc::parse_toml(
        "policy_version = 1\n",
        &reg,
        zero_migrate_policy::LoadContext::NonRootLayer,
    )
    .unwrap();

    let pa = compose_strict(&RootCeiling::parse_toml(a_toml, &reg).unwrap(), &empty, &reg).unwrap();
    let pb = compose_strict(&RootCeiling::parse_toml(b_toml, &reg).unwrap(), &empty, &reg).unwrap();

    let sa = seal(&pa, MAC_KEY, [9u8; 16], DIALECT, MATCHER, CEILING_VER);
    // The seal minted over order-A must NOT verify against order-B's policy.
    assert_eq!(
        sa.verify(MAC_KEY, &pb, &reg.digest(), DIALECT, MATCHER, CEILING_VER),
        Err(SealError::TagMismatch)
    );
}
