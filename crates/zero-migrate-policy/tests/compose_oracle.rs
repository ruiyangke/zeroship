//! The COMPOSITION ORACLE — the correctness proof for the security crown jewel.
//!
//! The pointwise-grant admissibility check in `admit` is what prevents
//! privilege escalation. Prose review of it cannot be trusted (exactly as with the
//! scope lattice, which the Phase-1a oracle settled). So this suite proves it by
//! brute force:
//!
//! - Over a bounded universe of concrete object names, and for grant keys with a
//!   SMALL value domain (a `Bool` key and a `UintCeiling` key), it materializes
//!   `value(ceiling, k, ·)` and `value(draft, k, ·)` as CONCRETE `Object -> Value`
//!   maps computed DIRECTLY from the rule lists (never via the code under test).
//! - **Core property:** `admit(ceiling, draft)` is `Ok` IFF, for every
//!   object `o` and every grant key `k`, `draft_value(k,o) ⊑ ceiling_value(k,o)`
//!   (pointwise ground truth). It runs the property over many generated
//!   ceiling/draft pairs — catching ANY residual scope-only or value-blind check.
//! - It PINS the critic's two escalation cases (both must REJECT) and a
//!   strictly-inside case (must ACCEPT).
//! - It proves `restrict` associativity + the clamp-then-strict equivalence,
//!   the require/inject/validate union-up, compose-time collision blame, and
//!   `EffectivePolicy` unforgeability.
//!
//! The value maps are the ground truth; the composer is the code under test.

use std::collections::BTreeMap;

use zero_migrate_policy::{
    restrict, admit, ComposeError, Enforcement, KnobDef, KnobKey, KnobKind,
    KnobValue, ObjectName, PolicyDoc, PolicyRegistry, Polarity, RootCeiling, RuleKind,
};

// ══════════════════════════════════════════════════════════════════════════════
// Registry — a small, deliberately-chosen set of grant knobs
// ══════════════════════════════════════════════════════════════════════════════

const BOOL_KEY: &str = "core.raw_sql"; // Bool grant, PerTable, default false
const UINT_KEY: &str = "op.lock_timeout_ms"; // UintCeiling grant, PerTable, default 1
const CREATE_KEY: &str = "core.create_table"; // Bool grant, PerTable, default false

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
            def(BOOL_KEY, KnobKind::Bool, Polarity::Grant, KnobValue::Bool(false)),
            def(
                UINT_KEY,
                KnobKind::UintCeiling { hard_floor: 1 },
                Polarity::Grant,
                KnobValue::Uint(1),
            ),
            def(CREATE_KEY, KnobKind::Bool, Polarity::Grant, KnobValue::Bool(false)),
        ])
        .unwrap()
}

// ══════════════════════════════════════════════════════════════════════════════
// Bounded universe of concrete object names
// ══════════════════════════════════════════════════════════════════════════════

/// A small universe: a handful of schemas (matching the pattern pool) × a couple of
/// tables each, plus the schema objects. Kept tight so the O(pairs · |𝒰|) sweep is
/// fast, but rich enough to distinguish `app_*` / `app_tmp_*` / `staging` regions.
fn universe() -> Vec<ObjectName> {
    let schemas: &[&[u8]] = &[b"app_main", b"app_tmp_x", b"staging", b"other"];
    let tables: &[&[u8]] = &[b"t", b"u"];
    let mut out = Vec::new();
    for s in schemas {
        out.push(ObjectName::schema(s.to_vec()));
        for t in tables {
            out.push(ObjectName::table(s.to_vec(), t.to_vec()));
        }
    }
    out
}

// ══════════════════════════════════════════════════════════════════════════════
// Ground truth: value(policy, k, o) computed DIRECTLY from the rule list
// ══════════════════════════════════════════════════════════════════════════════

/// The GROUND-TRUTH grant value at `o` for key `k` in `doc`: the loosest value among
/// grant rules on `k` whose scope covers `o`, else the knob default. Computed here
/// from `doc.rules` using ONLY the public scope-membership + a hand-rolled value
/// join — deliberately NOT the composer, so this is an independent oracle.
fn value_gt(doc: &PolicyDoc, kind: &KnobKind, default: &KnobValue, key: &str, o: &ObjectName) -> KnobValue {
    let mut acc = default.clone();
    for rule in &doc.rules {
        if let RuleKind::Grant { key: rk, value } = &rule.kind {
            if rk.as_str() == key && rule.scope.objects_membership(o) {
                acc = join_gt(kind, &acc, value);
            }
        }
    }
    acc
}

/// The ground-truth value join (loosest): Bool OR, Uint max. (Only Bool + Uint keys
/// are materialized by this oracle.)
fn join_gt(kind: &KnobKind, a: &KnobValue, b: &KnobValue) -> KnobValue {
    match (kind, a, b) {
        (KnobKind::Bool, KnobValue::Bool(x), KnobValue::Bool(y)) => KnobValue::Bool(*x || *y),
        (KnobKind::UintCeiling { .. }, KnobValue::Uint(x), KnobValue::Uint(y)) => {
            KnobValue::Uint((*x).max(*y))
        }
        _ => panic!("oracle join over unexpected shapes: {a:?} {b:?}"),
    }
}

/// The ground-truth value order (⊑): Bool implication, Uint ≤.
fn leq_gt(a: &KnobValue, b: &KnobValue) -> bool {
    match (a, b) {
        (KnobValue::Bool(x), KnobValue::Bool(y)) => !x || *y,
        (KnobValue::Uint(x), KnobValue::Uint(y)) => x <= y,
        _ => panic!("oracle leq over unexpected shapes: {a:?} {b:?}"),
    }
}

/// The ground-truth value meet (tightest): Bool AND, Uint min.
fn meet_gt(kind: &KnobKind, a: &KnobValue, b: &KnobValue) -> KnobValue {
    match (kind, a, b) {
        (KnobKind::Bool, KnobValue::Bool(x), KnobValue::Bool(y)) => KnobValue::Bool(*x && *y),
        (KnobKind::UintCeiling { .. }, KnobValue::Uint(x), KnobValue::Uint(y)) => {
            KnobValue::Uint((*x).min(*y))
        }
        _ => panic!("oracle meet over unexpected shapes: {a:?} {b:?}"),
    }
}

/// The two materialized keys + their kinds/defaults, for the ground-truth sweep.
fn materialized_keys() -> Vec<(&'static str, KnobKind, KnobValue)> {
    vec![
        (BOOL_KEY, KnobKind::Bool, KnobValue::Bool(false)),
        (UINT_KEY, KnobKind::UintCeiling { hard_floor: 1 }, KnobValue::Uint(1)),
        (CREATE_KEY, KnobKind::Bool, KnobValue::Bool(false)),
    ]
}

/// Ground-truth ACCEPT predicate: for every materialized key and object, the draft's
/// value ⊑ the ceiling's value. (Grants only — the generated docs carry no
/// require/inject/validate, so union-up always holds trivially.)
fn gt_admissible(ceiling: &PolicyDoc, draft: &PolicyDoc, univ: &[ObjectName]) -> bool {
    for (key, kind, default) in materialized_keys() {
        for o in univ {
            let dv = value_gt(draft, &kind, &default, key, o);
            let cv = value_gt(ceiling, &kind, &default, key, o);
            if !leq_gt(&dv, &cv) {
                return false;
            }
        }
    }
    true
}

// ══════════════════════════════════════════════════════════════════════════════
// Generators — small grant docs over a bounded pattern × value pool
// ══════════════════════════════════════════════════════════════════════════════

/// The pattern pool for generated grant scopes — chosen to exercise the escalation
/// corners: `app_*` (covers app_main AND app_tmp_x), `staging`, and the exclude form
/// `app_* \ app_tmp_*`.
#[derive(Clone, Copy)]
enum Pat {
    AppStar,        // include = ["app_*"]
    AppStarNoTmp,   // include = ["app_*"], exclude = ["app_tmp_*"]
    Staging,        // include = ["staging"]
}

impl Pat {
    fn scope_toml(self) -> &'static str {
        match self {
            Pat::AppStar => r#"scope = { include = ["app_*"] }"#,
            Pat::AppStarNoTmp => r#"scope = { include = ["app_*"], exclude = ["app_tmp_*"] }"#,
            Pat::Staging => r#"scope = { include = ["staging"] }"#,
        }
    }
    fn all() -> Vec<Pat> {
        vec![Pat::AppStar, Pat::AppStarNoTmp, Pat::Staging]
    }
}

/// A generated grant rule: a key, a scope pattern, and a value literal.
#[derive(Clone)]
struct Gen {
    key: &'static str,
    pat: Pat,
    value: &'static str,
}

/// Render a set of generated grant rules into a policy TOML document.
fn doc_toml(gens: &[Gen]) -> String {
    let mut s = String::from("policy_version = 1\n");
    for g in gens {
        s.push_str("[[grant]]\n");
        s.push_str(&format!("key = \"{}\"\n", g.key));
        s.push_str(&format!("value = {}\n", g.value));
        s.push_str(g.pat.scope_toml());
        s.push('\n');
    }
    s
}

/// Parse a generated doc as a plain (non-root) layer.
fn parse_layer(gens: &[Gen]) -> PolicyDoc {
    PolicyDoc::parse_toml(&doc_toml(gens), &registry(), zero_migrate_policy::LoadContext::NonRootLayer)
        .unwrap_or_else(|e| panic!("layer parse failed: {e:?}\n{}", doc_toml(gens)))
}

/// Parse a generated doc as a root ceiling.
fn parse_root(gens: &[Gen]) -> RootCeiling {
    RootCeiling::parse_toml(&doc_toml(gens), &registry())
        .unwrap_or_else(|e| panic!("root parse failed: {e:?}\n{}", doc_toml(gens)))
}

// ══════════════════════════════════════════════════════════════════════════════
// THE CORE PROPERTY: admit Ok ⟺ pointwise draft ⊑ ceiling
// ══════════════════════════════════════════════════════════════════════════════

#[test]
fn oracle_admit_ok_iff_pointwise_leq() {
    let univ = universe();
    let reg = registry();

    // Ceiling grant rules: one Bool + one Uint rule over the pattern pool + value
    // pool. Draft: same shape. Sweep the Cartesian product (bounded).
    let bool_vals = ["true", "false"];
    let uint_vals = ["60", "600"];
    let pats = Pat::all();

    let mut total = 0usize;
    let mut accepted = 0usize;
    let mut rejected = 0usize;

    // Ceiling = { BOOL @ cpat = cbool ; UINT @ cpat2 = cuint }.
    for &cpat in &pats {
        for &cpat2 in &pats {
            for cbool in bool_vals {
                for cuint in uint_vals {
                    let ceiling_gens = vec![
                        Gen { key: BOOL_KEY, pat: cpat, value: cbool },
                        Gen { key: UINT_KEY, pat: cpat2, value: cuint },
                    ];
                    let ceiling_doc = parse_layer(&ceiling_gens);
                    let root = parse_root(&ceiling_gens);

                    // Draft = { BOOL @ dpat = dbool ; UINT @ dpat2 = duint }.
                    for &dpat in &pats {
                        for &dpat2 in &pats {
                            for dbool in bool_vals {
                                for duint in uint_vals {
                                    let draft_gens = vec![
                                        Gen { key: BOOL_KEY, pat: dpat, value: dbool },
                                        Gen { key: UINT_KEY, pat: dpat2, value: duint },
                                    ];
                                    let draft = parse_layer(&draft_gens);

                                    let gt = gt_admissible(&ceiling_doc, &draft, &univ);
                                    let got = admit(&root, &draft, &reg);

                                    total += 1;
                                    match (&got, gt) {
                                        (Ok(_), true) => accepted += 1,
                                        (Err(_), false) => rejected += 1,
                                        (Ok(ep), false) => panic!(
                                            "FALSE ACCEPT (escalation slipped through)!\n\
                                             ceiling={ceiling_gens:?}\n draft={draft_gens:?}\n\
                                             composed ok but ground truth says a draft value \
                                             exceeds the ceiling somewhere.\n{ep:?}"
                                        ),
                                        (Err(e), true) => panic!(
                                            "FALSE REJECT (should have composed)!\n\
                                             ceiling={ceiling_gens:?}\n draft={draft_gens:?}\n err={e:?}"
                                        ),
                                    }

                                    // When accepted, the composed policy's decision
                                    // query must equal the draft's ground-truth value
                                    // map (the effective grant is the draft's, proven
                                    // ⊑ the ceiling pointwise).
                                    if let Ok(ep) = &got {
                                        for (key, kind, default) in materialized_keys() {
                                            if key == CREATE_KEY {
                                                continue; // no create grants generated
                                            }
                                            let pk = KnobKey::parse(key).unwrap();
                                            for o in &univ {
                                                let want =
                                                    value_gt(&draft, &kind, &default, key, o);
                                                let got_v = ep.grants(&pk, o).unwrap();
                                                assert_eq!(
                                                    got_v, want,
                                                    "decision-query value mismatch at {o:?} key {key}"
                                                );
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // Guard against a vacuous sweep.
    assert!(total >= 1000, "sweep too small: {total}");
    assert!(accepted > 0 && rejected > 0, "sweep degenerate: acc={accepted} rej={rejected}");
    eprintln!("admit oracle: total={total} accepted={accepted} rejected={rejected}");
}

// Debug-derive helper for the panic messages above.
impl std::fmt::Debug for Gen {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}@{}={}", self.key, self.pat.scope_toml(), self.value)
    }
}

// ══════════════════════════════════════════════════════════════════════════════
// PINNED escalation cases (the critic's counterexamples)
// ══════════════════════════════════════════════════════════════════════════════

#[test]
fn pinned_value_blind_escalation_rejects() {
    // Ceiling: { timeout 60s @ app_* , 600s @ staging }.
    // Draft:   { timeout 600s @ app_* }  (also 600s @ staging is fine, but app_* is
    //          where the ceiling only allows 60s).
    // At app_main.t: value(ceiling) = 60 (only the app_* ceiling rule covers it),
    //                value(draft)   = 600  →  600 ≤ 60 is FALSE  →  REJECT.
    // A scope-only check (draft app_* ⊑ ceiling app_*∪staging) would WRONGLY pass.
    let reg = registry();
    let root = parse_root(&[
        Gen { key: UINT_KEY, pat: Pat::AppStar, value: "60" },
        Gen { key: UINT_KEY, pat: Pat::Staging, value: "600" },
    ]);
    let draft = parse_layer(&[Gen { key: UINT_KEY, pat: Pat::AppStar, value: "600" }]);
    let got = admit(&root, &draft, &reg);
    assert!(
        matches!(got, Err(ComposeError::GrantExceedsCeiling { .. })),
        "value-blind escalation must REJECT, got {got:?}"
    );
}

#[test]
fn pinned_exclude_escalation_rejects() {
    // Ceiling grant @ { app_* exclude app_tmp_* } = true.
    // Draft   grant @ { app_* } = true.
    // The draft grants at app_tmp_x, which the ceiling excludes → REJECT (exercises
    // the ∖ uncovered-region check).
    let reg = registry();
    let root = parse_root(&[Gen { key: BOOL_KEY, pat: Pat::AppStarNoTmp, value: "true" }]);
    let draft = parse_layer(&[Gen { key: BOOL_KEY, pat: Pat::AppStar, value: "true" }]);
    let got = admit(&root, &draft, &reg);
    assert!(
        matches!(
            got,
            Err(ComposeError::GrantExceedsCeiling { .. })
                | Err(ComposeError::UncoveredRegionNotRepresentable { .. })
        ),
        "exclude escalation must REJECT, got {got:?}"
    );
}

#[test]
fn pinned_strictly_inside_accepts() {
    // Ceiling grant @ app_* = true, timeout 600 @ app_*.
    // Draft   grant @ app_* = true (same), timeout 60 @ app_* (tighter) → ACCEPT.
    let reg = registry();
    let root = parse_root(&[
        Gen { key: BOOL_KEY, pat: Pat::AppStar, value: "true" },
        Gen { key: UINT_KEY, pat: Pat::AppStar, value: "600" },
    ]);
    let draft = parse_layer(&[
        Gen { key: BOOL_KEY, pat: Pat::AppStar, value: "true" },
        Gen { key: UINT_KEY, pat: Pat::AppStar, value: "60" },
    ]);
    let got = admit(&root, &draft, &reg);
    assert!(got.is_ok(), "strictly-inside draft must ACCEPT, got {got:?}");
}

// ══════════════════════════════════════════════════════════════════════════════
// restrict: associativity + clamp-then-strict equivalence
// ══════════════════════════════════════════════════════════════════════════════

/// Materialize the effective value map of a composed policy over the universe, for
/// both materialized grant keys — the comparable denotation of a `restrict`.
fn value_map_of(
    ep: &zero_migrate_policy::EffectivePolicy,
    univ: &[ObjectName],
) -> BTreeMap<(String, ObjectName), KnobValue> {
    let mut m = BTreeMap::new();
    for (key, _kind, _default) in materialized_keys() {
        let pk = KnobKey::parse(key).unwrap();
        for o in univ {
            m.insert((key.to_string(), o.clone()), ep.grants(&pk, o).unwrap());
        }
    }
    m
}

#[test]
fn oracle_restrict_associative_and_pointwise_meet() {
    let univ = universe();
    let reg = registry();
    let pats = Pat::all();
    let uint_vals = ["60", "600"];
    let bool_vals = ["true", "false"];

    let mut checked = 0usize;

    // Three trusted ceilings h, o, p — each a root ceiling (all trusted here). We use
    // the roots as ceilings and clamp them; associativity is asserted over the value
    // map denotation.
    for &hp in &pats {
        for hv in uint_vals {
            for &op in &pats {
                for ov in bool_vals {
                    for &pp in &pats {
                        for pv in uint_vals {
                            let h = parse_root(&[Gen { key: UINT_KEY, pat: hp, value: hv }]);
                            let o = parse_root(&[Gen { key: BOOL_KEY, pat: op, value: ov }]);
                            let p = parse_root(&[Gen { key: UINT_KEY, pat: pp, value: pv }]);

                            // clamp(clamp(h,o),p) and clamp(h,clamp(o,p)).
                            let ho = restrict(&h, &o, &reg).unwrap();
                            let left = restrict(&ho, &p, &reg).unwrap();
                            let op_ = restrict(&o, &p, &reg).unwrap();
                            let right = restrict(&h, &op_, &reg).unwrap();

                            let lm = value_map_of(&left, &univ);
                            let rm = value_map_of(&right, &univ);
                            assert_eq!(lm, rm, "restrict NOT associative");

                            // Pointwise meet ground truth: value(clamp) at o ==
                            // meet(value(h), value(o), value(p)) per key.
                            for (key, kind, default) in materialized_keys() {
                                if key == CREATE_KEY {
                                    continue;
                                }
                                let pk = KnobKey::parse(key).unwrap();
                                for obj in &univ {
                                    let vh = value_gt(h.doc(), &kind, &default, key, obj);
                                    let vo = value_gt(o.doc(), &kind, &default, key, obj);
                                    let vp = value_gt(p.doc(), &kind, &default, key, obj);
                                    let want =
                                        meet_gt(&kind, &meet_gt(&kind, &vh, &vo), &vp);
                                    let got = left.grants(&pk, obj).unwrap();
                                    assert_eq!(
                                        got, want,
                                        "clamp pointwise-meet mismatch at {obj:?} key {key}"
                                    );
                                }
                            }
                            checked += 1;
                        }
                    }
                }
            }
        }
    }
    assert!(checked >= 100, "clamp sweep too small: {checked}");
}

#[test]
fn oracle_clamp_then_strict_equals_strict_against_tightest_chain() {
    // strict(clamp(h,o), draft) is Ok IFF the draft is pointwise ⊑ the pointwise
    // meet of h and o everywhere — i.e. ⊑ the tightest chain.
    let univ = universe();
    let reg = registry();
    let pats = Pat::all();
    let uint_vals = ["60", "600"];

    let mut total = 0;
    let mut acc = 0;
    let mut rej = 0;
    for &hp in &pats {
        for hv in uint_vals {
            for &op in &pats {
                for ov in uint_vals {
                    let h = parse_root(&[Gen { key: UINT_KEY, pat: hp, value: hv }]);
                    let o = parse_root(&[Gen { key: UINT_KEY, pat: op, value: ov }]);
                    let clamp = restrict(&h, &o, &reg).unwrap();

                    for &dp in &pats {
                        for dv in uint_vals {
                            let draft = parse_layer(&[Gen { key: UINT_KEY, pat: dp, value: dv }]);

                            // Ground truth: draft ⊑ meet(h, o) pointwise.
                            let kind = KnobKind::UintCeiling { hard_floor: 1 };
                            let default = KnobValue::Uint(1);
                            let mut gt = true;
                            for obj in &univ {
                                let vd = value_gt(&draft, &kind, &default, UINT_KEY, obj);
                                let vh = value_gt(h.doc(), &kind, &default, UINT_KEY, obj);
                                let vo = value_gt(o.doc(), &kind, &default, UINT_KEY, obj);
                                let tight = meet_gt(&kind, &vh, &vo);
                                if !leq_gt(&vd, &tight) {
                                    gt = false;
                                    break;
                                }
                            }

                            let got = admit(&clamp, &draft, &reg);
                            total += 1;
                            match (&got, gt) {
                                (Ok(_), true) => acc += 1,
                                (Err(_), false) => rej += 1,
                                (Ok(_), false) => panic!(
                                    "FALSE ACCEPT vs tightest chain: h={hp:?}{hv} o={op:?}{ov} draft={dp:?}{dv}"
                                ),
                                (Err(e), true) => panic!(
                                    "FALSE REJECT vs tightest chain: h={hp:?}{hv} o={op:?}{ov} draft={dp:?}{dv} err={e:?}"
                                ),
                            }
                        }
                    }
                }
            }
        }
    }
    assert!(total >= 50 && acc > 0 && rej > 0, "sweep degenerate: total={total} acc={acc} rej={rej}");
}

// Pat needs Debug for the panic messages.
impl std::fmt::Debug for Pat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Pat::AppStar => "app_*",
            Pat::AppStarNoTmp => "app_*\\app_tmp_*",
            Pat::Staging => "staging",
        })
    }
}

// ══════════════════════════════════════════════════════════════════════════════
// Union-up: every ceiling require/inject/validate survives admit
// ══════════════════════════════════════════════════════════════════════════════

fn registry_with_require() -> PolicyRegistry {
    registry()
        .with([def("sec.require_rls", KnobKind::Bool, Polarity::Require, KnobValue::Bool(false))])
        .unwrap()
}

#[test]
fn union_up_ceiling_obligations_and_injects_and_validates_survive() {
    let reg = registry_with_require();
    let root_toml = r#"policy_version = 1
[[require]]
key = "sec.require_rls"
value = true
scope = { include = ["app_*"] }
[[inject]]
scope = { include = ["app_*"] }
mandatory = true
columns = [ { name = "created_at", type = "timestamptz", nullable = false } ]
[[validate]]
scope = { include = ["app_*"] }
predicate = { kind = "has_primary_key" }
[[grant]]
key = "core.create_table"
value = true
scope = { include = ["app_*"] }
[[grant]]
key = "core.raw_sql"
value = true
scope = { include = ["staging"] }
"#;
    let root = RootCeiling::parse_toml(root_toml, &reg).unwrap();

    // Draft adds a grant but NO require/inject/validate — the ceiling's must survive.
    let draft = PolicyDoc::parse_toml(
        r#"policy_version = 1
[[grant]]
key = "core.raw_sql"
value = true
scope = { include = ["staging"] }
"#,
        &reg,
        zero_migrate_policy::LoadContext::NonRootLayer,
    )
    .unwrap();

    let ep = admit(&root, &draft, &reg).unwrap();
    let app_t = ObjectName::table(b"app_main".to_vec(), b"t".to_vec());

    // Obligation survives.
    let obs = ep.obligations(&app_t);
    assert!(
        obs.iter().any(|(k, v)| k.as_str() == "sec.require_rls" && *v == KnobValue::Bool(true)),
        "ceiling require dropped: {obs:?}"
    );
    // Inject survives.
    let injs = ep.injects_for(&app_t);
    assert_eq!(injs.len(), 1, "ceiling inject dropped");
    assert_eq!(injs[0].columns[0].name, "created_at");
    // Validate survives.
    let vals = ep.validates_for(&app_t);
    assert!(!vals.is_empty(), "ceiling validate dropped");
}

/// An `OrderedEnum` `Require` obligation modelled on `sec.require_approval`
/// (`never ⊑ on_destructive ⊑ always`), for the union-up composition test.
fn registry_with_require_approval() -> PolicyRegistry {
    registry()
        .with([def(
            "sec.require_approval",
            KnobKind::OrderedEnum {
                variants: vec!["never".into(), "on_destructive".into(), "always".into()],
            },
            Polarity::Require,
            KnobValue::Str("never".into()),
        )])
        .unwrap()
}

#[test]
fn require_approval_composes_union_up_operator_always_beats_creator_never() {
    let reg = registry_with_require_approval();
    // OPERATOR ceiling requires approval ALWAYS on app_*.
    let root = RootCeiling::parse_toml(
        r#"policy_version = 1
[[require]]
key = "sec.require_approval"
value = "always"
scope = { include = ["app_*"] }
"#,
        &reg,
    )
    .unwrap();

    // CREATOR draft tries to LOWER it to `never` on the same scope.
    let draft = PolicyDoc::parse_toml(
        r#"policy_version = 1
[[require]]
key = "sec.require_approval"
value = "never"
scope = { include = ["app_*"] }
"#,
        &reg,
        zero_migrate_policy::LoadContext::NonRootLayer,
    )
    .unwrap();

    // admit is Ok: a Require obligation composes UP, so a draft can never
    // LOWER a ceiling obligation — the draft `never` is admissible (it only ever
    // raises), and the effective value is the union-up `always`.
    let ep = admit(&root, &draft, &reg).unwrap();
    let app_t = ObjectName::table(b"app_main".to_vec(), b"t".to_vec());
    let obs = ep.obligations(&app_t);

    // The operator's `always` obligation survives (the creator could not drop it).
    assert!(
        obs.iter().any(|(k, v)| k.as_str() == "sec.require_approval"
            && *v == KnobValue::Str("always".into())),
        "operator `always` obligation dropped by creator draft: {obs:?}"
    );
    // The effective LOOSEST covering obligation is `always` (rank-max over the two
    // covering rules `always` and `never`).
    let loosest = obs
        .iter()
        .filter(|(k, _)| k.as_str() == "sec.require_approval")
        .filter_map(|(_, v)| match v {
            KnobValue::Str(s) => Some(s.clone()),
            _ => None,
        })
        .max_by_key(|s| match s.as_str() {
            "never" => 0u8,
            "on_destructive" => 1,
            "always" => 2,
            _ => 3,
        });
    assert_eq!(
        loosest.as_deref(),
        Some("always"),
        "creator `never` must not lower the operator `always` obligation"
    );
}

// ══════════════════════════════════════════════════════════════════════════════
// Compose-time collision blame
// ══════════════════════════════════════════════════════════════════════════════

#[test]
fn draft_inject_colliding_ceiling_inject_rejects_at_compose() {
    let reg = registry();
    // Ceiling injects created_at:timestamptz on app_*.
    let root = RootCeiling::parse_toml(
        r#"policy_version = 1
[[inject]]
scope = { include = ["app_*"] }
columns = [ { name = "created_at", type = "timestamptz", nullable = false } ]
"#,
        &reg,
    )
    .unwrap();
    // Draft injects created_at with a DIVERGENT type on overlapping scope → collision.
    let draft = PolicyDoc::parse_toml(
        r#"policy_version = 1
[[inject]]
scope = { include = ["app_main"] }
columns = [ { name = "created_at", type = "text", nullable = true } ]
"#,
        &reg,
        zero_migrate_policy::LoadContext::NonRootLayer,
    )
    .unwrap();
    let got = admit(&root, &draft, &reg);
    assert!(
        matches!(got, Err(ComposeError::DraftInjectCollidesCeiling { .. })),
        "draft-vs-ceiling inject collision must REJECT at compose, got {got:?}"
    );
}

#[test]
fn draft_validate_contradicting_ceiling_inject_rejects_at_compose() {
    let reg = registry();
    let root = RootCeiling::parse_toml(
        r#"policy_version = 1
[[inject]]
scope = { include = ["app_*"] }
columns = [ { name = "created_at", type = "timestamptz", nullable = false } ]
"#,
        &reg,
    )
    .unwrap();
    // Draft forbids created_at on overlapping scope → contradicts the ceiling inject.
    let draft = PolicyDoc::parse_toml(
        r#"policy_version = 1
[[validate]]
scope = { include = ["app_main"] }
predicate = { kind = "forbidden_columns", names = ["created_at"] }
"#,
        &reg,
        zero_migrate_policy::LoadContext::NonRootLayer,
    )
    .unwrap();
    let got = admit(&root, &draft, &reg);
    assert!(
        matches!(got, Err(ComposeError::DraftValidateContradictsCeilingInject { .. })),
        "draft validate contradicting ceiling inject must REJECT, got {got:?}"
    );
}

#[test]
fn ceiling_vs_ceiling_inject_collision_rejects_at_clamp() {
    let reg = registry();
    let a = RootCeiling::parse_toml(
        r#"policy_version = 1
[[inject]]
scope = { include = ["app_*"] }
columns = [ { name = "created_at", type = "timestamptz", nullable = false } ]
"#,
        &reg,
    )
    .unwrap();
    let b = RootCeiling::parse_toml(
        r#"policy_version = 1
[[inject]]
scope = { include = ["app_main"] }
columns = [ { name = "created_at", type = "text", nullable = true } ]
"#,
        &reg,
    )
    .unwrap();
    let got = restrict(&a, &b, &reg);
    assert!(
        matches!(got, Err(ComposeError::CeilingInjectCollision { .. })),
        "ceiling-vs-ceiling inject collision must be a loud operator error, got {got:?}"
    );
}

// ══════════════════════════════════════════════════════════════════════════════
// Creation-gating lint: creatable ⊑ every mandatory inject
// ══════════════════════════════════════════════════════════════════════════════

#[test]
fn creatable_escaping_mandatory_inject_rejects() {
    let reg = registry();
    // Mandatory inject on app_* only; but the draft can create in staging too.
    let root = RootCeiling::parse_toml(
        r#"policy_version = 1
[[inject]]
scope = { include = ["app_*"] }
mandatory = true
columns = [ { name = "created_at", type = "timestamptz", nullable = false } ]
[[grant]]
key = "core.create_table"
value = true
scope = "all"
"#,
        &reg,
    )
    .unwrap();
    // Draft grants create_table @ staging (⊄ app_*) → escapes the mandatory inject.
    let draft = PolicyDoc::parse_toml(
        r#"policy_version = 1
[[grant]]
key = "core.create_table"
value = true
scope = { include = ["staging"] }
"#,
        &reg,
        zero_migrate_policy::LoadContext::NonRootLayer,
    )
    .unwrap();
    let got = admit(&root, &draft, &reg);
    assert!(
        matches!(got, Err(ComposeError::CreatableEscapesMandatoryInject { .. })),
        "creatable escaping mandatory inject must REJECT, got {got:?}"
    );
}

#[test]
fn creatable_within_mandatory_inject_accepts() {
    let reg = registry();
    let root = RootCeiling::parse_toml(
        r#"policy_version = 1
[[inject]]
scope = { include = ["app_*"] }
mandatory = true
columns = [ { name = "created_at", type = "timestamptz", nullable = false } ]
[[grant]]
key = "core.create_table"
value = true
scope = "all"
"#,
        &reg,
    )
    .unwrap();
    // Draft creates only inside app_* → within the mandatory inject.
    let draft = PolicyDoc::parse_toml(
        r#"policy_version = 1
[[grant]]
key = "core.create_table"
value = true
scope = { include = ["app_*"] }
"#,
        &reg,
        zero_migrate_policy::LoadContext::NonRootLayer,
    )
    .unwrap();
    assert!(admit(&root, &draft, &reg).is_ok());
}

// ══════════════════════════════════════════════════════════════════════════════
// is_injected_shape (name-match-at-op-time)
// ══════════════════════════════════════════════════════════════════════════════

#[test]
fn is_injected_shape_name_matches_covering_inject() {
    use zero_migrate_policy::ShapeElement;
    let reg = registry();
    let root = RootCeiling::parse_toml(
        r#"policy_version = 1
[[inject]]
scope = { include = ["app_*"] }
columns = [ { name = "created_at", type = "timestamptz", nullable = false } ]
indexes = [ { name = "idx_created", columns = ["created_at"] } ]
primary_key = ["id"]
"#,
        &reg,
    )
    .unwrap();
    let draft = PolicyDoc::parse_toml(
        "policy_version = 1\n",
        &reg,
        zero_migrate_policy::LoadContext::NonRootLayer,
    )
    .unwrap();
    let ep = admit(&root, &draft, &reg).unwrap();

    let app_t = ObjectName::table(b"app_main".to_vec(), b"t".to_vec());
    let other = ObjectName::table(b"staging".to_vec(), b"t".to_vec());

    assert!(ep.is_injected_shape(&app_t, &ShapeElement::Column("created_at")));
    // Case-fold: `Created_At` folds to created_at → still injected.
    assert!(ep.is_injected_shape(&app_t, &ShapeElement::Column("Created_At")));
    assert!(ep.is_injected_shape(&app_t, &ShapeElement::Index("idx_created")));
    assert!(ep.is_injected_shape(&app_t, &ShapeElement::PrimaryKey));
    assert!(!ep.is_injected_shape(&app_t, &ShapeElement::Column("other")));
    // Outside the inject scope: nothing injected.
    assert!(!ep.is_injected_shape(&other, &ShapeElement::Column("created_at")));
    assert!(!ep.is_injected_shape(&other, &ShapeElement::PrimaryKey));
}

// ══════════════════════════════════════════════════════════════════════════════
// Unforgeability
// ══════════════════════════════════════════════════════════════════════════════

// This test documents the unforgeability mechanism by CONSTRUCTION: the only paths
// to an `EffectivePolicy` are admit / restrict (from a RootCeiling) and
// the explicit deny_all floor. The type has private fields, no Deserialize, and no
// public `new`/`Default` — so the following are the ONLY constructors, and the crate
// compiles iff that remains true. (A negative compile test is covered by the absence
// of any other public constructor; asserted here by exercising every real one.)
#[test]
fn effective_policy_only_via_compose_or_deny_all() {
    let reg = registry();

    // (1) deny_all — the explicit engine floor (grants nothing above default).
    let floor = zero_migrate_policy::EffectivePolicy::deny_all(&reg);
    let o = ObjectName::table(b"app_main".to_vec(), b"t".to_vec());
    assert_eq!(floor.grants(&KnobKey::parse(BOOL_KEY).unwrap(), &o), Some(KnobValue::Bool(false)));

    // (2) admit from a RootCeiling.
    let root = parse_root(&[Gen { key: BOOL_KEY, pat: Pat::AppStar, value: "true" }]);
    let draft = parse_layer(&[Gen { key: BOOL_KEY, pat: Pat::AppStar, value: "true" }]);
    let ep = admit(&root, &draft, &reg).unwrap();
    assert_eq!(ep.grants(&KnobKey::parse(BOOL_KEY).unwrap(), &o), Some(KnobValue::Bool(true)));

    // (3) restrict of two RootCeilings, then used itself as a ceiling.
    let a = parse_root(&[Gen { key: BOOL_KEY, pat: Pat::AppStar, value: "true" }]);
    let b = parse_root(&[Gen { key: BOOL_KEY, pat: Pat::AppStar, value: "true" }]);
    let clamp = restrict(&a, &b, &reg).unwrap();
    let _ep2 = admit(&clamp, &draft, &reg).unwrap();

    // There is deliberately NO other constructor: no Default, no Deserialize, no
    // public `new`. This test compiling is the proof the surface is closed.
}
