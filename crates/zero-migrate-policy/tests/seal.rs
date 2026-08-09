//! Seal round-trip + tamper-detection suite (§II.7). Proves the seal binds the
//! resolved rule set, the registry digest, the `(dialect, matcher_version)` matcher
//! semantics, and the charter version — and HARD-FAILS on any mismatch.
//!
//! The seal MACs a FRESHLY-composed [`EffectivePolicy`]; "tampering" means presenting
//! a DIFFERENT composed policy (a mutated rule/scope) to `verify`, which must fail
//! the tag. Because `EffectivePolicy` is unforgeable, we tamper by composing a
//! different document — exactly what an attacker who swapped the sealed policy would
//! present.

use zero_migrate_policy::{
    admit, finalize_charter, overlay, seal, Enforcement, KnobDef, KnobKey, KnobKind, KnobValue,
    Polarity, PolicyDoc, PolicyRegistry, RootCharter, SealError, TrustedDoc,
};

const MAC_KEY: &[u8] = b"a-shared-fleet-mac-key-32-bytes!!";
const DIALECT: &str = "postgres";
const MATCHER: u32 = 1;
const CHARTER_VER: u64 = 7;

fn def(key: &str, kind: KnobKind, polarity: Polarity, default: KnobValue) -> KnobDef {
    KnobDef {
        key: KnobKey::parse(key).unwrap(),
        kind,
        polarity,
        default,
        enforcement: Enforcement::Enforced,
        object_model: zero_migrate_policy::ObjectModel::PerTable,
        requires_db_privilege: false,
        inherit: true,
        docs: String::new(),
    }
}

fn registry() -> PolicyRegistry {
    PolicyRegistry::empty()
        .with([
            def(
                "sql.raw",
                KnobKind::Bool,
                Polarity::Grant,
                KnobValue::Bool(false),
            ),
            def(
                "runtime.lock_timeout_ms",
                KnobKind::UintCharter { hard_floor: 1 },
                Polarity::Grant,
                KnobValue::Uint(1),
            ),
            def(
                "safety.require_rls",
                KnobKind::Bool,
                Polarity::Require,
                KnobValue::Bool(false),
            ),
        ])
        .unwrap()
}

/// A registry that differs only in a knob's `requires_db_privilege` — same keys, so
/// a document loads identically, but the digest differs (II.2.1).
fn registry_flipped_privilege() -> PolicyRegistry {
    PolicyRegistry::empty()
        .with([
            {
                let mut d = def(
                    "sql.raw",
                    KnobKind::Bool,
                    Polarity::Grant,
                    KnobValue::Bool(false),
                );
                d.requires_db_privilege = true;
                d
            },
            def(
                "runtime.lock_timeout_ms",
                KnobKind::UintCharter { hard_floor: 1 },
                Polarity::Grant,
                KnobValue::Uint(1),
            ),
            def(
                "safety.require_rls",
                KnobKind::Bool,
                Polarity::Require,
                KnobValue::Bool(false),
            ),
        ])
        .unwrap()
}

const ROOT_TOML: &str = r#"policy_version = 1
[[grant]]
key = "sql.raw"
value = true
scope = { include = ["app_*"] }
[[grant]]
key = "runtime.lock_timeout_ms"
value = 600
scope = { include = ["app_*"] }
[[require]]
key = "safety.require_rls"
value = true
scope = { include = ["app_*"] }
[[inject]]
scope = { include = ["app_*"] }
columns = [ { name = "created_at", type = "timestamptz", nullable = false } ]
[[validate]]
scope = { include = ["app_*"] }
predicate = { kind = "forbidden_columns", names = ["ssn"] }
"#;

/// Compose the reference policy: the root charter against an empty draft (all charter
/// rules survive union-up; the draft adds no grants so nothing escalates).
fn reference_policy(reg: &PolicyRegistry) -> zero_migrate_policy::EffectivePolicy {
    let root = RootCharter::parse_toml(ROOT_TOML, reg).unwrap();
    let draft = PolicyDoc::parse_toml(
        "policy_version = 1\n",
        reg,
        zero_migrate_policy::LoadContext::NonRootLayer,
    )
    .unwrap();
    admit(&root, &draft, reg).unwrap()
}

// ══════════════════════════════════════════════════════════════════════════════
// round-trip
// ══════════════════════════════════════════════════════════════════════════════

#[test]
fn good_seal_round_trips() {
    let reg = registry();
    let policy = reference_policy(&reg);
    let sealed = seal(&policy, MAC_KEY, [7u8; 16], DIALECT, MATCHER, CHARTER_VER);

    // A freshly-composed identical policy verifies.
    let fresh = reference_policy(&reg);
    assert_eq!(
        sealed.verify(
            MAC_KEY,
            &fresh,
            &reg.digest(),
            DIALECT,
            MATCHER,
            CHARTER_VER
        ),
        Ok(())
    );
}

#[test]
fn wrong_mac_key_fails() {
    let reg = registry();
    let policy = reference_policy(&reg);
    let sealed = seal(&policy, MAC_KEY, [7u8; 16], DIALECT, MATCHER, CHARTER_VER);
    let fresh = reference_policy(&reg);
    assert_eq!(
        sealed.verify(
            b"a-different-mac-key-of-length-32!",
            &fresh,
            &reg.digest(),
            DIALECT,
            MATCHER,
            CHARTER_VER
        ),
        Err(SealError::TagMismatch)
    );
}

// ══════════════════════════════════════════════════════════════════════════════
// tamper: a mutated rule / scope fails the tag
// ══════════════════════════════════════════════════════════════════════════════

#[test]
fn tampered_grant_value_fails() {
    // The effective grant map in strict ingress is the DRAFT's (bounded by the
    // charter). So a grant-value tamper means the DRAFT carries a different value.
    let reg = registry();
    let root = RootCharter::parse_toml(ROOT_TOML, &reg).unwrap();

    // Draft grants timeout 600 @ app_* (within the charter's 600 @ app_*) → seal it.
    let draft_600 = PolicyDoc::parse_toml(
        r#"policy_version = 1
[[grant]]
key = "runtime.lock_timeout_ms"
value = 600
scope = { include = ["app_*"] }
"#,
        &reg,
        zero_migrate_policy::LoadContext::NonRootLayer,
    )
    .unwrap();
    let policy_600 = admit(&root, &draft_600, &reg).unwrap();
    let sealed = seal(
        &policy_600,
        MAC_KEY,
        [1u8; 16],
        DIALECT,
        MATCHER,
        CHARTER_VER,
    );

    // A tightened tamper: draft 60 @ app_* — a DIFFERENT effective grant map.
    let draft_60 = PolicyDoc::parse_toml(
        r#"policy_version = 1
[[grant]]
key = "runtime.lock_timeout_ms"
value = 60
scope = { include = ["app_*"] }
"#,
        &reg,
        zero_migrate_policy::LoadContext::NonRootLayer,
    )
    .unwrap();
    let tampered = admit(&root, &draft_60, &reg).unwrap();

    assert_eq!(
        sealed.verify(
            MAC_KEY,
            &tampered,
            &reg.digest(),
            DIALECT,
            MATCHER,
            CHARTER_VER
        ),
        Err(SealError::TagMismatch)
    );
}

#[test]
fn tampered_scope_fails() {
    // Tamper a UNION-UP (charter-sourced) rule's scope: the inject scope. It flows
    // into the sealed rule set verbatim, so narrowing it changes the canonical bytes.
    let reg = registry();
    let sealed = seal(
        &reference_policy(&reg),
        MAC_KEY,
        [2u8; 16],
        DIALECT,
        MATCHER,
        CHARTER_VER,
    );

    // Narrow ONLY the inject scope (app_* → app_main). The [[inject]] block is the
    // only one whose `include` we rewrite here.
    let tampered_root = ROOT_TOML.replace(
        "[[inject]]\nscope = { include = [\"app_*\"] }",
        "[[inject]]\nscope = { include = [\"app_main\"] }",
    );
    assert_ne!(
        tampered_root, ROOT_TOML,
        "the inject-scope rewrite must land"
    );
    let root = RootCharter::parse_toml(&tampered_root, &reg).unwrap();
    let draft = PolicyDoc::parse_toml(
        "policy_version = 1\n",
        &reg,
        zero_migrate_policy::LoadContext::NonRootLayer,
    )
    .unwrap();
    let tampered = admit(&root, &draft, &reg).unwrap();

    assert_eq!(
        sealed.verify(
            MAC_KEY,
            &tampered,
            &reg.digest(),
            DIALECT,
            MATCHER,
            CHARTER_VER
        ),
        Err(SealError::TagMismatch)
    );
}

#[test]
fn tampered_inject_column_fails() {
    let reg = registry();
    let sealed = seal(
        &reference_policy(&reg),
        MAC_KEY,
        [3u8; 16],
        DIALECT,
        MATCHER,
        CHARTER_VER,
    );

    // Inject a differently-typed column — a different rule set.
    let tampered_root = ROOT_TOML.replace("timestamptz", "text");
    let root = RootCharter::parse_toml(&tampered_root, &reg).unwrap();
    let draft = PolicyDoc::parse_toml(
        "policy_version = 1\n",
        &reg,
        zero_migrate_policy::LoadContext::NonRootLayer,
    )
    .unwrap();
    let tampered = admit(&root, &draft, &reg).unwrap();

    assert_eq!(
        sealed.verify(
            MAC_KEY,
            &tampered,
            &reg.digest(),
            DIALECT,
            MATCHER,
            CHARTER_VER
        ),
        Err(SealError::TagMismatch)
    );
}

// ══════════════════════════════════════════════════════════════════════════════
// binding: registry digest / dialect / matcher / charter version
// ══════════════════════════════════════════════════════════════════════════════

#[test]
fn wrong_registry_digest_fails() {
    let reg = registry();
    let sealed = seal(
        &reference_policy(&reg),
        MAC_KEY,
        [4u8; 16],
        DIALECT,
        MATCHER,
        CHARTER_VER,
    );

    // Verify presenting a DIFFERENT registry digest (a flipped requires_db_privilege
    // — same keys, different enforcement semantics) → hard fail (II.2.1).
    let flipped = registry_flipped_privilege();
    let fresh = reference_policy(&reg);
    assert_eq!(
        sealed.verify(
            MAC_KEY,
            &fresh,
            &flipped.digest(),
            DIALECT,
            MATCHER,
            CHARTER_VER
        ),
        Err(SealError::RegistryDigestMismatch)
    );
    // And the digests genuinely differ.
    assert_ne!(reg.digest(), flipped.digest());
}

/// A seal minted under registry A must not verify a policy composed under registry B,
/// even when the caller pins the SEAL'S OWN digest - the tautological call a verifier
/// makes when it reads the pin off the seal it was handed.
///
/// The pin arm (`wrong_registry_digest_fails`) cannot cover this: it compares the seal
/// against the caller's value and never touches the policy. The two registries here
/// differ only in `requires_db_privilege`, which enters `KnobDef::canonical_encoding`
/// and nothing else - the same document loads, composes and canonicalizes to
/// BYTE-IDENTICAL sealed rule bytes under either - so the registry digest is the only
/// thing separating them, and `verify` has to derive it from the policy rather than
/// echo the caller's parameter back into the MAC.
#[test]
fn policy_composed_under_another_registry_fails() {
    let reg = registry();
    let flipped = registry_flipped_privilege();
    assert_ne!(reg.digest(), flipped.digest(), "premise: digests differ");

    let sealed = seal(
        &reference_policy(&reg),
        MAC_KEY,
        [10u8; 16],
        DIALECT,
        MATCHER,
        CHARTER_VER,
    );

    // The same document composed under B: identical rules, different registry.
    let policy_b = reference_policy(&flipped);
    assert_eq!(
        sealed.verify(
            MAC_KEY,
            &policy_b,
            &sealed.registry_digest(),
            DIALECT,
            MATCHER,
            CHARTER_VER
        ),
        Err(SealError::PolicyComposedUnderOtherRegistry)
    );
}

#[test]
fn wrong_dialect_fails() {
    let reg = registry();
    let sealed = seal(
        &reference_policy(&reg),
        MAC_KEY,
        [5u8; 16],
        DIALECT,
        MATCHER,
        CHARTER_VER,
    );
    let fresh = reference_policy(&reg);
    assert_eq!(
        sealed.verify(
            MAC_KEY,
            &fresh,
            &reg.digest(),
            "mysql",
            MATCHER,
            CHARTER_VER
        ),
        Err(SealError::DialectMismatch)
    );
}

#[test]
fn wrong_matcher_version_fails() {
    let reg = registry();
    let sealed = seal(
        &reference_policy(&reg),
        MAC_KEY,
        [6u8; 16],
        DIALECT,
        MATCHER,
        CHARTER_VER,
    );
    let fresh = reference_policy(&reg);
    assert_eq!(
        sealed.verify(
            MAC_KEY,
            &fresh,
            &reg.digest(),
            DIALECT,
            MATCHER + 1,
            CHARTER_VER
        ),
        Err(SealError::MatcherVersionMismatch)
    );
}

#[test]
fn wrong_charter_version_fails() {
    let reg = registry();
    let sealed = seal(
        &reference_policy(&reg),
        MAC_KEY,
        [8u8; 16],
        DIALECT,
        MATCHER,
        CHARTER_VER,
    );
    let fresh = reference_policy(&reg);
    assert_eq!(
        sealed.verify(
            MAC_KEY,
            &fresh,
            &reg.digest(),
            DIALECT,
            MATCHER,
            CHARTER_VER + 1
        ),
        Err(SealError::CharterVersionMismatch)
    );
}

#[test]
fn nonce_swap_invalidates_tag() {
    let reg = registry();
    let policy = reference_policy(&reg);
    let s1 = seal(&policy, MAC_KEY, [1u8; 16], DIALECT, MATCHER, CHARTER_VER);
    let s2 = seal(&policy, MAC_KEY, [2u8; 16], DIALECT, MATCHER, CHARTER_VER);
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

    let pa = admit(
        &RootCharter::parse_toml(a_toml, &reg).unwrap(),
        &empty,
        &reg,
    )
    .unwrap();
    let pb = admit(
        &RootCharter::parse_toml(b_toml, &reg).unwrap(),
        &empty,
        &reg,
    )
    .unwrap();

    let sa = seal(&pa, MAC_KEY, [9u8; 16], DIALECT, MATCHER, CHARTER_VER);
    // The seal minted over order-A must NOT verify against order-B's policy.
    assert_eq!(
        sa.verify(MAC_KEY, &pb, &reg.digest(), DIALECT, MATCHER, CHARTER_VER),
        Err(SealError::TagMismatch)
    );
}

/// **The seal binds LAYER BOUNDARIES (H-4 / II.7).** Two policies with the SAME
/// flattened grant set but DIFFERENT layer stacks encode differently, so a re-flatten
/// tamper fails the MAC. Policy A is a 2-layer charter `overlay(base, env)` (env grants
/// raw_sql@staging, base grants raw_sql@app_*) admitted with an empty draft → a stack
/// `[draft(empty)] over [env] over [base]`. Policy B is a SINGLE-layer root charter
/// carrying the SAME two grant rules flattened into one layer → `[draft(empty)] over
/// [base(both rules)]`. Their flattened grant sets are identical; only the layering
/// differs. The seal from A must NOT verify against B.
#[test]
fn layer_reflatten_changes_tag() {
    let reg = registry();

    // Policy A: overlay(base, env) — two trusted layers.
    let base = TrustedDoc::register_catalog_entry(
        r#"policy_version = 1
[[grant]]
key = "sql.raw"
value = true
scope = { include = ["app_*"] }
"#,
        &reg,
    )
    .unwrap();
    let env = TrustedDoc::register_catalog_entry(
        r#"policy_version = 1
[[grant]]
key = "sql.raw"
value = true
scope = { include = ["staging"] }
"#,
        &reg,
    )
    .unwrap();
    let charter_a = finalize_charter(overlay(&base, &env, &reg).unwrap()).unwrap();
    let empty = PolicyDoc::parse_toml(
        "policy_version = 1\n",
        &reg,
        zero_migrate_policy::LoadContext::NonRootLayer,
    )
    .unwrap();
    let pa = admit(&charter_a, &empty, &reg).unwrap();

    // Policy B: a SINGLE root layer carrying BOTH grant rules — same flattened set.
    let flat_root = RootCharter::parse_toml(
        r#"policy_version = 1
[[grant]]
key = "sql.raw"
value = true
scope = { include = ["app_*"] }
[[grant]]
key = "sql.raw"
value = true
scope = { include = ["staging"] }
"#,
        &reg,
    )
    .unwrap();
    let pb = admit(&flat_root, &empty, &reg).unwrap();

    // The two denote the SAME effective grants (sanity), but different layer stacks.
    let staging_t = zero_migrate_policy::ObjectName::table(b"staging".to_vec(), b"t".to_vec());
    let app_t = zero_migrate_policy::ObjectName::table(b"app_main".to_vec(), b"t".to_vec());
    let rk = KnobKey::parse("sql.raw").unwrap();
    assert_eq!(pa.grants(&rk, &staging_t), pb.grants(&rk, &staging_t));
    assert_eq!(pa.grants(&rk, &app_t), pb.grants(&rk, &app_t));

    // But the seal binds the layer boundaries: A's seal must NOT verify against B.
    let sa = seal(&pa, MAC_KEY, [3u8; 16], DIALECT, MATCHER, CHARTER_VER);
    assert_eq!(
        sa.verify(MAC_KEY, &pb, &reg.digest(), DIALECT, MATCHER, CHARTER_VER),
        Err(SealError::TagMismatch),
        "a re-flattened layer stack must FAIL the MAC (layer boundaries are bound)"
    );
    // A verifies against itself (round-trip sanity).
    assert!(sa
        .verify(MAC_KEY, &pa, &reg.digest(), DIALECT, MATCHER, CHARTER_VER)
        .is_ok());
}

/// Two globs that RENDER the same but match different sets must not seal the same.
///
/// The seal used to encode `SegGlob::render`, justified by render being injective
/// over the `(prefix, suffix, has_star)` triple. That holds only while the triple is
/// canonical - at most one `*`, none inside the literal pieces. `SegGlob::parse`
/// enforces it, so the strict TOML loader cannot break it, but the public
/// `SegGlob::infix` can.
#[test]
fn globs_rendering_alike_but_matching_differently_seal_differently() {
    use zero_migrate_policy::scope::glob::SegGlob;

    let a = SegGlob::infix(b"a*".to_vec(), b"b".to_vec());
    let b = SegGlob::infix(b"a".to_vec(), b"*b".to_vec());

    // The premise: same spelling, different membership.
    assert_eq!(a.render(), b.render(), "the two globs render identically");
    assert_ne!(a, b, "but they are not the same glob");
    assert!(a.matches(b"a*zb") && !b.matches(b"a*zb"));
    assert!(b.matches(b"az*b") && !a.matches(b"az*b"));

    // The loader cannot produce either: a second `*` is refused at parse.
    assert!(
        SegGlob::parse("a**b").is_none(),
        "the strict loader still refuses a second star"
    );

    // The canonical seal encoding must therefore separate them on the pieces rather
    // than the spelling. That encoding is private, so the assertion lives beside it
    // as a unit test in `seal.rs`; this test pins the premise it rests on.
}
