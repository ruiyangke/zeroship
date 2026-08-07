//! The COMPOSITION ORACLE — the correctness proof for the security crown jewel.
//!
//! The composition algebra (`admit`/`restrict`/`overlay`/`finalize_charter`) is what
//! prevents privilege escalation. Prose review of it cannot be trusted (exactly as
//! with the scope lattice, which its own oracle settled). So this suite proves it
//! by brute force over a bounded universe of concrete object names, with GROUND-TRUTH
//! value maps computed DIRECTLY from the rule lists (never via the code under test):
//!
//! - **`restrict`** is the exact pointwise MEET of two trusted docs.
//! - **`overlay`** is total, per-scalar-knob presence-based last-wins, rule-lists
//!   union (base-then-over).
//! - **`admit`** is CHARTER-INHERITED: `Ok` IFF every draft grant that raises above
//!   default is `⊑` the charter pointwise; the effective grant = presence-override +
//!   inherit; and — the no-escalation invariant — **`effective ⊑ charter` for EVERY
//!   key/object** over the whole universe (all three arms: silent-inherit,
//!   narrow-to-default, narrow-above-default). Obligations union-up survive.
//! - It PINS the critic's C-1 escalation counterexample (must REJECT), the layered
//!   override, narrow-to-default, and the seal re-flatten hard-fail.
//!
//! The value maps are the ground truth; the composer is the code under test.

use zero_migrate_policy::{
    admit, finalize_charter, overlay, restrict, ComposeError, Enforcement, GrantRegion, KnobDef,
    KnobKey, KnobKind, KnobValue, ObjectName, Polarity, PolicyDoc, PolicyRegistry, RootCharter,
    RuleKind, TrustedDoc,
};

// ══════════════════════════════════════════════════════════════════════════════
// Registry — a small, deliberately-chosen set of grant knobs
// ══════════════════════════════════════════════════════════════════════════════

const BOOL_KEY: &str = "sql.raw"; // Bool grant, PerTable, default false
const UINT_KEY: &str = "runtime.lock_timeout_ms"; // UintCharter grant, PerTable, default 1
const CREATE_KEY: &str = "schema.create_table"; // Bool grant, PerTable, default false

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
                BOOL_KEY,
                KnobKind::Bool,
                Polarity::Grant,
                KnobValue::Bool(false),
            ),
            def(
                UINT_KEY,
                KnobKind::UintCharter { hard_floor: 1 },
                Polarity::Grant,
                KnobValue::Uint(1),
            ),
            def(
                CREATE_KEY,
                KnobKind::Bool,
                Polarity::Grant,
                KnobValue::Bool(false),
            ),
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
fn value_gt(
    doc: &PolicyDoc,
    kind: &KnobKind,
    default: &KnobValue,
    key: &str,
    o: &ObjectName,
) -> KnobValue {
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

/// Ground-truth PRESENCE: does `doc` have ANY grant rule on `key` covering `o`?
fn covers_gt(doc: &PolicyDoc, key: &str, o: &ObjectName) -> bool {
    doc.rules.iter().any(|rule| {
        matches!(&rule.kind, RuleKind::Grant { key: rk, .. } if rk.as_str() == key)
            && rule.scope.objects_membership(o)
    })
}

/// The ground-truth value join (loosest): Bool OR, Uint max.
fn join_gt(kind: &KnobKind, a: &KnobValue, b: &KnobValue) -> KnobValue {
    match (kind, a, b) {
        (KnobKind::Bool, KnobValue::Bool(x), KnobValue::Bool(y)) => KnobValue::Bool(*x || *y),
        (KnobKind::UintCharter { .. }, KnobValue::Uint(x), KnobValue::Uint(y)) => {
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
        (KnobKind::UintCharter { .. }, KnobValue::Uint(x), KnobValue::Uint(y)) => {
            KnobValue::Uint((*x).min(*y))
        }
        _ => panic!("oracle meet over unexpected shapes: {a:?} {b:?}"),
    }
}

/// The materialized keys + their kinds/defaults, for the ground-truth sweep.
fn materialized_keys() -> Vec<(&'static str, KnobKind, KnobValue)> {
    vec![
        (BOOL_KEY, KnobKind::Bool, KnobValue::Bool(false)),
        (
            UINT_KEY,
            KnobKind::UintCharter { hard_floor: 1 },
            KnobValue::Uint(1),
        ),
    ]
}

/// Ground-truth ACCEPT predicate for `admit(charter, draft)`: for every materialized
/// key and object where the DRAFT raises above default, the draft's value ⊑ the
/// charter's effective value. (Grants only — the generated docs carry no
/// require/inject/validate.) `charter_value` computes the charter's EFFECTIVE value at
/// `(k,o)`.
fn gt_admissible(
    charter_value: &dyn Fn(&str, &KnobKind, &KnobValue, &ObjectName) -> KnobValue,
    draft: &PolicyDoc,
    univ: &[ObjectName],
) -> bool {
    for (key, kind, default) in materialized_keys() {
        for o in univ {
            let dv = value_gt(draft, &kind, &default, key, o);
            // Only objects where the draft RAISES above default are checked.
            if leq_gt(&dv, &default) {
                continue;
            }
            let cv = charter_value(key, &kind, &default, o);
            if !leq_gt(&dv, &cv) {
                return false;
            }
        }
    }
    true
}

/// Ground-truth EFFECTIVE value of `admit(charter, draft)` at `(k,o)`: presence-based
/// override — the draft's value if the draft covers `(k,o)`, else the charter's
/// effective value (inherit).
fn gt_effective(
    charter_value: &dyn Fn(&str, &KnobKind, &KnobValue, &ObjectName) -> KnobValue,
    draft: &PolicyDoc,
    key: &str,
    kind: &KnobKind,
    default: &KnobValue,
    o: &ObjectName,
) -> KnobValue {
    if covers_gt(draft, key, o) {
        value_gt(draft, kind, default, key, o)
    } else {
        charter_value(key, kind, default, o)
    }
}

// ══════════════════════════════════════════════════════════════════════════════
// Generators — small grant docs over a bounded pattern × value pool
// ══════════════════════════════════════════════════════════════════════════════

/// The pattern pool for generated grant scopes — chosen to exercise the escalation
/// corners: `app_*` (covers app_main AND app_tmp_x), `staging`, and the exclude form
/// `app_* \ app_tmp_*`.
#[derive(Clone, Copy)]
enum Pat {
    AppStar,      // include = ["app_*"]
    AppStarNoTmp, // include = ["app_*"], exclude = ["app_tmp_*"]
    Staging,      // include = ["staging"]
    All,          // scope = "all" - the whole universe
}

impl Pat {
    fn scope_toml(self) -> &'static str {
        match self {
            Pat::AppStar => r#"scope = { include = ["app_*"] }"#,
            Pat::AppStarNoTmp => r#"scope = { include = ["app_*"], exclude = ["app_tmp_*"] }"#,
            Pat::Staging => r#"scope = { include = ["staging"] }"#,
            // A universal scope is what makes a masked hole reachable: its witness
            // is outside any specific mask, so a partition that skips masking rules
            // samples a point where the charter still grants.
            Pat::All => r#"scope = "all""#,
        }
    }
    fn all() -> Vec<Pat> {
        vec![Pat::AppStar, Pat::AppStarNoTmp, Pat::Staging, Pat::All]
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

/// Parse a generated doc as a plain (non-root) UNTRUSTED draft layer.
fn parse_draft(gens: &[Gen]) -> PolicyDoc {
    PolicyDoc::parse_toml(
        &doc_toml(gens),
        &registry(),
        zero_migrate_policy::LoadContext::NonRootLayer,
    )
    .unwrap_or_else(|e| panic!("draft parse failed: {e:?}\n{}", doc_toml(gens)))
}

/// Parse a generated doc as a root charter.
fn parse_root(gens: &[Gen]) -> RootCharter {
    RootCharter::parse_toml(&doc_toml(gens), &registry())
        .unwrap_or_else(|e| panic!("root parse failed: {e:?}\n{}", doc_toml(gens)))
}

/// Parse a generated doc as a TRUSTED catalog entry (an `overlay`/`restrict` operand).
fn parse_trusted(gens: &[Gen]) -> TrustedDoc {
    TrustedDoc::register_catalog_entry(&doc_toml(gens), &registry())
        .unwrap_or_else(|e| panic!("trusted parse failed: {e:?}\n{}", doc_toml(gens)))
}

// ══════════════════════════════════════════════════════════════════════════════
// THE CORE PROPERTY (admit): Ok ⟺ pointwise draft ⊑ charter + effective ⊑ charter
// ══════════════════════════════════════════════════════════════════════════════

#[test]
fn oracle_admit_ok_iff_pointwise_leq_and_effective_below_charter() {
    let univ = universe();
    let reg = registry();

    let bool_vals = ["true", "false"];
    let uint_vals = ["60", "600"];
    let pats = Pat::all();

    let mut total = 0usize;
    let mut accepted = 0usize;
    let mut rejected = 0usize;

    for &cpat in &pats {
        for &cpat2 in &pats {
            for cbool in bool_vals {
                for cuint in uint_vals {
                    let charter_gens = vec![
                        Gen {
                            key: BOOL_KEY,
                            pat: cpat,
                            value: cbool,
                        },
                        Gen {
                            key: UINT_KEY,
                            pat: cpat2,
                            value: cuint,
                        },
                    ];
                    let charter_doc = parse_draft(&charter_gens);
                    let root = parse_root(&charter_gens);
                    // The charter's effective value = the root doc's grant value.
                    let cval = |key: &str, kind: &KnobKind, default: &KnobValue, o: &ObjectName| {
                        value_gt(&charter_doc, kind, default, key, o)
                    };

                    for &dpat in &pats {
                        for &dpat2 in &pats {
                            for dbool in bool_vals {
                                for duint in uint_vals {
                                    let draft_gens = vec![
                                        Gen {
                                            key: BOOL_KEY,
                                            pat: dpat,
                                            value: dbool,
                                        },
                                        Gen {
                                            key: UINT_KEY,
                                            pat: dpat2,
                                            value: duint,
                                        },
                                    ];
                                    let draft = parse_draft(&draft_gens);

                                    let gt = gt_admissible(&cval, &draft, &univ);
                                    let got = admit(&root, &draft, &reg);

                                    total += 1;
                                    match (&got, gt) {
                                        (Ok(_), true) => accepted += 1,
                                        (Err(_), false) => rejected += 1,
                                        (Ok(ep), false) => panic!(
                                            "FALSE ACCEPT (escalation slipped through)!\n\
                                             charter={charter_gens:?}\n draft={draft_gens:?}\n{ep:?}"
                                        ),
                                        (Err(e), true) => panic!(
                                            "FALSE REJECT (should have composed)!\n\
                                             charter={charter_gens:?}\n draft={draft_gens:?}\n err={e:?}"
                                        ),
                                    }

                                    if let Ok(ep) = &got {
                                        for (key, kind, default) in materialized_keys() {
                                            let pk = KnobKey::parse(key).unwrap();
                                            for o in &univ {
                                                // (a) effective == presence-override GT.
                                                let want = gt_effective(
                                                    &cval, &draft, key, &kind, &default, o,
                                                );
                                                let got_v = ep.grants(&pk, o).unwrap();
                                                assert_eq!(
                                                    got_v, want,
                                                    "effective value mismatch at {o:?} key {key}"
                                                );
                                                // (b) THE NO-ESCALATION INVARIANT:
                                                //     effective ⊑ charter everywhere.
                                                let cv = cval(key, &kind, &default, o);
                                                assert!(
                                                    leq_gt(&got_v, &cv),
                                                    "effective ⋢ charter at {o:?} key {key}: \
                                                     eff={got_v:?} charter={cv:?}"
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

    assert!(total >= 1000, "sweep too small: {total}");
    assert!(
        accepted > 0 && rejected > 0,
        "sweep degenerate: acc={accepted} rej={rejected}"
    );
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
    // Charter: { timeout 60s @ app_* , 600s @ staging }.  Draft: { timeout 600s @ app_* }.
    // At app_main.t: value(charter)=60, value(draft)=600 → REJECT.
    let reg = registry();
    let root = parse_root(&[
        Gen {
            key: UINT_KEY,
            pat: Pat::AppStar,
            value: "60",
        },
        Gen {
            key: UINT_KEY,
            pat: Pat::Staging,
            value: "600",
        },
    ]);
    let draft = parse_draft(&[Gen {
        key: UINT_KEY,
        pat: Pat::AppStar,
        value: "600",
    }]);
    let got = admit(&root, &draft, &reg);
    assert!(
        matches!(got, Err(ComposeError::GrantExceedsCharter { .. })),
        "value-blind escalation must REJECT, got {got:?}"
    );
}

#[test]
fn pinned_exclude_escalation_rejects() {
    // Charter grant @ { app_* exclude app_tmp_* } = true. Draft grant @ { app_* } = true.
    // The draft grants at app_tmp_x, which the charter excludes → REJECT.
    let reg = registry();
    let root = parse_root(&[Gen {
        key: BOOL_KEY,
        pat: Pat::AppStarNoTmp,
        value: "true",
    }]);
    let draft = parse_draft(&[Gen {
        key: BOOL_KEY,
        pat: Pat::AppStar,
        value: "true",
    }]);
    let got = admit(&root, &draft, &reg);
    assert!(
        matches!(
            got,
            Err(ComposeError::GrantExceedsCharter { .. })
                | Err(ComposeError::UncoveredRegionNotRepresentable { .. })
        ),
        "exclude escalation must REJECT, got {got:?}"
    );
}

#[test]
fn pinned_strictly_inside_accepts() {
    // Charter grant @ app_* = true, timeout 600 @ app_*. Draft same bool, timeout 60 → ACCEPT.
    let reg = registry();
    let root = parse_root(&[
        Gen {
            key: BOOL_KEY,
            pat: Pat::AppStar,
            value: "true",
        },
        Gen {
            key: UINT_KEY,
            pat: Pat::AppStar,
            value: "600",
        },
    ]);
    let draft = parse_draft(&[
        Gen {
            key: BOOL_KEY,
            pat: Pat::AppStar,
            value: "true",
        },
        Gen {
            key: UINT_KEY,
            pat: Pat::AppStar,
            value: "60",
        },
    ]);
    assert!(
        admit(&root, &draft, &reg).is_ok(),
        "strictly-inside draft must ACCEPT"
    );
}

/// **THE C-1 COUNTEREXAMPLE.** Charter grants `raw_sql` on `Of{[app_*],
/// exclude=[app_secret]}` PLUS a disjoint `Of{[reports]}`; draft `raw_sql@app_secret`
/// → admit REJECTS. A `⊔`-materialized charter side would DROP the `app_secret`
/// exclude (folding the two charter rules), compute `app_secret` as covered, and
/// wrongly ACCEPT. The iterated-per-rule-∖ path keeps the exclude → reject.
#[test]
fn pinned_c1_disjoint_exclude_escalation_rejects() {
    let reg = registry();
    let root = RootCharter::parse_toml(
        r#"policy_version = 1
[[grant]]
key = "sql.raw"
value = true
scope = { include = ["app_*"], exclude = ["app_secret"] }
[[grant]]
key = "sql.raw"
value = true
scope = { include = ["reports"] }
"#,
        &reg,
    )
    .unwrap();
    // Draft grants raw_sql on app_secret — the excluded region.
    let draft = PolicyDoc::parse_toml(
        r#"policy_version = 1
[[grant]]
key = "sql.raw"
value = true
scope = { include = ["app_secret"] }
"#,
        &reg,
        zero_migrate_policy::LoadContext::NonRootLayer,
    )
    .unwrap();
    let got = admit(&root, &draft, &reg);
    assert!(
        matches!(
            got,
            Err(ComposeError::GrantExceedsCharter { .. })
                | Err(ComposeError::UncoveredRegionNotRepresentable { .. })
        ),
        "C-1 disjoint-exclude escalation MUST reject (a ⊔-materialized charter would \
         have wrongly ACCEPTED), got {got:?}"
    );

    // Sanity: the OLD ⊔-materialized formula (fold the two charter rules into one
    // scope via join, dropping the exclude) WOULD compute app_secret as covered.
    // We assert the ground truth is a reject: at app_secret.t the charter's effective
    // raw_sql is FALSE (app_* grants it but the exclude removes app_secret; reports is
    // disjoint), while the draft raises it to TRUE → true ⋢ false → escalation.
    let secret = ObjectName::table(b"app_secret".to_vec(), b"t".to_vec());
    let charter_doc = root.doc();
    let cv = value_gt(
        charter_doc,
        &KnobKind::Bool,
        &KnobValue::Bool(false),
        BOOL_KEY,
        &secret,
    );
    assert_eq!(
        cv,
        KnobValue::Bool(false),
        "charter denies app_secret (exclude honored)"
    );
}

/// **PIN the layered override.** Draft `timeout=10000@app_*` over charter
/// `timeout=30000@All` → `grants(timeout, app_main.t) == 10000` (draft narrows),
/// `grants(timeout, other.t) == 30000` (inherited from the charter). The flat
/// loosest-covering formula could not represent this (it would return 30000 at
/// app_main.t, discarding the narrower draft value).
#[test]
fn pinned_layered_override_narrow_region() {
    let reg = registry();
    let root = RootCharter::parse_toml(
        r#"policy_version = 1
[[grant]]
key = "runtime.lock_timeout_ms"
value = 30000
scope = "all"
"#,
        &reg,
    )
    .unwrap();
    let draft = PolicyDoc::parse_toml(
        r#"policy_version = 1
[[grant]]
key = "runtime.lock_timeout_ms"
value = 10000
scope = { include = ["app_*"] }
"#,
        &reg,
        zero_migrate_policy::LoadContext::NonRootLayer,
    )
    .unwrap();
    let ep = admit(&root, &draft, &reg).expect("narrowing draft is admissible");
    let pk = KnobKey::parse(UINT_KEY).unwrap();

    let app_main_t = ObjectName::table(b"app_main".to_vec(), b"t".to_vec());
    let other_t = ObjectName::table(b"other".to_vec(), b"t".to_vec());
    assert_eq!(
        ep.grants(&pk, &app_main_t),
        Some(KnobValue::Uint(10000)),
        "draft narrows timeout in app_* (layered override)"
    );
    assert_eq!(
        ep.grants(&pk, &other_t),
        Some(KnobValue::Uint(30000)),
        "outside app_*, the charter's timeout is inherited"
    );
}

/// **PIN narrow-to-default.** Draft `raw_sql=false@app_*` over charter `raw_sql=true@All`
/// → effective false in app_* (the creator asked for less and gets less; presence, not
/// raises-above-default, is the override trigger). Inherited true elsewhere.
#[test]
fn pinned_narrow_to_default_wins() {
    let reg = registry();
    let root = RootCharter::parse_toml(
        r#"policy_version = 1
[[grant]]
key = "sql.raw"
value = true
scope = "all"
"#,
        &reg,
    )
    .unwrap();
    let draft = PolicyDoc::parse_toml(
        r#"policy_version = 1
[[grant]]
key = "sql.raw"
value = false
scope = { include = ["app_*"] }
"#,
        &reg,
        zero_migrate_policy::LoadContext::NonRootLayer,
    )
    .unwrap();
    let ep = admit(&root, &draft, &reg).expect("narrow-to-default is admissible");
    let pk = KnobKey::parse(BOOL_KEY).unwrap();

    let app_main_t = ObjectName::table(b"app_main".to_vec(), b"t".to_vec());
    let other_t = ObjectName::table(b"other".to_vec(), b"t".to_vec());
    assert_eq!(
        ep.grants(&pk, &app_main_t),
        Some(KnobValue::Bool(false)),
        "draft narrow-to-default WINS in app_* (presence override)"
    );
    assert_eq!(
        ep.grants(&pk, &other_t),
        Some(KnobValue::Bool(true)),
        "outside app_*, the charter's true is inherited"
    );
}

/// **PIN silent-inherit.** A draft SILENT on a key inherits the charter's grant
/// (inherit-then-narrow, not draft-authoritative). A draft that omits a grant the
/// charter allows KEEPS it.
#[test]
fn pinned_silent_draft_inherits_charter_grant() {
    let reg = registry();
    let root = parse_root(&[Gen {
        key: BOOL_KEY,
        pat: Pat::AppStar,
        value: "true",
    }]);
    // Draft says nothing about raw_sql.
    let draft = PolicyDoc::parse_toml(
        "policy_version = 1\n",
        &reg,
        zero_migrate_policy::LoadContext::NonRootLayer,
    )
    .unwrap();
    let ep = admit(&root, &draft, &reg).unwrap();
    let pk = KnobKey::parse(BOOL_KEY).unwrap();
    let app_main_t = ObjectName::table(b"app_main".to_vec(), b"t".to_vec());
    assert_eq!(
        ep.grants(&pk, &app_main_t),
        Some(KnobValue::Bool(true)),
        "silent draft INHERITS the charter's grant"
    );
}

// ══════════════════════════════════════════════════════════════════════════════
// restrict: exact pointwise MEET + associativity
// ══════════════════════════════════════════════════════════════════════════════

#[test]
fn oracle_restrict_is_exact_pointwise_meet() {
    let univ = universe();
    let reg = registry();
    let pats = Pat::all();
    let uint_vals = ["60", "600"];
    let bool_vals = ["true", "false"];

    let mut checked = 0usize;

    for &ap in &pats {
        for av in uint_vals {
            for &bp in &pats {
                for bv in bool_vals {
                    let a_gens = vec![Gen {
                        key: UINT_KEY,
                        pat: ap,
                        value: av,
                    }];
                    let b_gens = vec![Gen {
                        key: BOOL_KEY,
                        pat: bp,
                        value: bv,
                    }];
                    let a = parse_trusted(&a_gens);
                    let b = parse_trusted(&b_gens);
                    let a_doc = parse_draft(&a_gens);
                    let b_doc = parse_draft(&b_gens);

                    // restrict → finalize → admit(empty draft) to read the charter value.
                    let restricted = restrict(&a, &b, &reg).unwrap();
                    let charter = finalize_charter(restricted).unwrap();
                    let empty = PolicyDoc::parse_toml(
                        "policy_version = 1\n",
                        &reg,
                        zero_migrate_policy::LoadContext::NonRootLayer,
                    )
                    .unwrap();
                    let ep = admit(&charter, &empty, &reg).unwrap();

                    for (key, kind, default) in materialized_keys() {
                        let pk = KnobKey::parse(key).unwrap();
                        for o in &univ {
                            let va = value_gt(&a_doc, &kind, &default, key, o);
                            let vb = value_gt(&b_doc, &kind, &default, key, o);
                            let want = meet_gt(&kind, &va, &vb);
                            let got = ep.grants(&pk, o).unwrap();
                            assert_eq!(got, want, "restrict ≠ pointwise meet at {o:?} key {key}");
                        }
                    }
                    checked += 1;
                }
            }
        }
    }
    assert!(checked >= 30, "restrict sweep too small: {checked}");
}

#[test]
fn oracle_restrict_commutative() {
    // `restrict` is a lattice MEET, hence commutative: restrict(a,b) and restrict(b,a)
    // denote the same effective grants. (True 3-way associativity is not expressible
    // with the 2-ary `TrustedDoc` signature — the meet output is not a `TrustedDoc` —
    // and the exact-meet property is already proven by `oracle_restrict_is_exact_meet`,
    // from which associativity follows by the meet laws.)
    let univ = universe();
    let reg = registry();
    let pats = Pat::all();
    let uint_vals = ["60", "600"];

    let empty = PolicyDoc::parse_toml(
        "policy_version = 1\n",
        &reg,
        zero_migrate_policy::LoadContext::NonRootLayer,
    )
    .unwrap();

    let mut checked = 0usize;
    for &pat_a in &pats {
        for val_a in uint_vals {
            for &pat_b in &pats {
                for val_b in uint_vals {
                    let doc_a = parse_trusted(&[Gen {
                        key: UINT_KEY,
                        pat: pat_a,
                        value: val_a,
                    }]);
                    let doc_b = parse_trusted(&[Gen {
                        key: UINT_KEY,
                        pat: pat_b,
                        value: val_b,
                    }]);

                    let ab = finalize_charter(restrict(&doc_a, &doc_b, &reg).unwrap()).unwrap();
                    let ba = finalize_charter(restrict(&doc_b, &doc_a, &reg).unwrap()).unwrap();
                    let ep_ab = admit(&ab, &empty, &reg).unwrap();
                    let ep_ba = admit(&ba, &empty, &reg).unwrap();

                    let pk = KnobKey::parse(UINT_KEY).unwrap();
                    for obj in &univ {
                        assert_eq!(
                            ep_ab.grants(&pk, obj),
                            ep_ba.grants(&pk, obj),
                            "restrict not commutative at {obj:?}"
                        );
                    }
                    checked += 1;
                }
            }
        }
    }
    assert!(
        checked >= 30,
        "restrict-commutativity sweep too small: {checked}"
    );
}

// ══════════════════════════════════════════════════════════════════════════════
// overlay: total, presence-based last-wins, rule-lists union
// ══════════════════════════════════════════════════════════════════════════════

#[test]
fn oracle_overlay_is_presence_last_wins() {
    let univ = universe();
    let reg = registry();
    let pats = Pat::all();
    let uint_vals = ["60", "600"];
    let bool_vals = ["true", "false"];

    let mut checked = 0usize;
    for &bp in &pats {
        for bv in uint_vals {
            for &op in &pats {
                for ov in uint_vals {
                    // base sets UINT@bp=bv; over sets UINT@op=ov. Where `over` covers,
                    // over wins (presence, last-wins); else base.
                    let base_gens = vec![Gen {
                        key: UINT_KEY,
                        pat: bp,
                        value: bv,
                    }];
                    let over_gens = vec![Gen {
                        key: UINT_KEY,
                        pat: op,
                        value: ov,
                    }];
                    let base = parse_trusted(&base_gens);
                    let over = parse_trusted(&over_gens);
                    let base_doc = parse_draft(&base_gens);
                    let over_doc = parse_draft(&over_gens);

                    let assembled = overlay(&base, &over, &reg).unwrap();
                    let charter = finalize_charter(assembled).unwrap();
                    let empty = PolicyDoc::parse_toml(
                        "policy_version = 1\n",
                        &reg,
                        zero_migrate_policy::LoadContext::NonRootLayer,
                    )
                    .unwrap();
                    let ep = admit(&charter, &empty, &reg).unwrap();

                    let kind = KnobKind::UintCharter { hard_floor: 1 };
                    let default = KnobValue::Uint(1);
                    let pk = KnobKey::parse(UINT_KEY).unwrap();
                    for o in &univ {
                        let want = if covers_gt(&over_doc, UINT_KEY, o) {
                            value_gt(&over_doc, &kind, &default, UINT_KEY, o)
                        } else {
                            value_gt(&base_doc, &kind, &default, UINT_KEY, o)
                        };
                        let got = ep.grants(&pk, o).unwrap();
                        assert_eq!(got, want, "overlay ≠ presence-last-wins at {o:?}");
                    }
                    checked += 1;
                }
            }
        }
    }
    let _ = bool_vals;
    assert!(checked >= 30, "overlay sweep too small: {checked}");
}

#[test]
fn oracle_overlay_unions_inject_and_require_lists() {
    let reg = registry_with_require();
    // base: require rls @ app_*, inject created_at @ app_*.
    let base = TrustedDoc::register_catalog_entry(
        r#"policy_version = 1
[[require]]
key = "safety.require_rls"
value = true
scope = { include = ["app_*"] }
[[inject]]
scope = { include = ["app_*"] }
columns = [ { name = "created_at", type = "timestamptz", nullable = false } ]
"#,
        &reg,
    )
    .unwrap();
    // over: require rls @ staging, inject updated_at @ app_*.
    let over = TrustedDoc::register_catalog_entry(
        r#"policy_version = 1
[[require]]
key = "safety.require_rls"
value = true
scope = { include = ["staging"] }
[[inject]]
scope = { include = ["app_*"] }
columns = [ { name = "updated_at", type = "timestamptz", nullable = false } ]
"#,
        &reg,
    )
    .unwrap();

    let charter = finalize_charter(overlay(&base, &over, &reg).unwrap()).unwrap();
    let empty = PolicyDoc::parse_toml(
        "policy_version = 1\n",
        &reg,
        zero_migrate_policy::LoadContext::NonRootLayer,
    )
    .unwrap();
    let ep = admit(&charter, &empty, &reg).unwrap();

    let app_t = ObjectName::table(b"app_main".to_vec(), b"t".to_vec());
    let staging_t = ObjectName::table(b"staging".to_vec(), b"t".to_vec());

    // Both require rules survive (accumulate).
    assert!(ep
        .obligations(&app_t)
        .iter()
        .any(|(k, _)| k.as_str() == "safety.require_rls"));
    assert!(ep
        .obligations(&staging_t)
        .iter()
        .any(|(k, _)| k.as_str() == "safety.require_rls"));
    // Both injects survive on app_* (base created_at + over updated_at).
    let injs = ep.injects_for(&app_t);
    let names: Vec<&str> = injs
        .iter()
        .flat_map(|s| s.columns.iter().map(|c| c.name.as_str()))
        .collect();
    assert!(
        names.contains(&"created_at"),
        "base inject dropped: {names:?}"
    );
    assert!(
        names.contains(&"updated_at"),
        "over inject dropped: {names:?}"
    );
    // Base-then-over order (M-3): created_at (base) precedes updated_at (over).
    let ci = names.iter().position(|n| *n == "created_at").unwrap();
    let ui = names.iter().position(|n| *n == "updated_at").unwrap();
    assert!(
        ci < ui,
        "overlay inject order must be base-then-over: {names:?}"
    );
}

// ══════════════════════════════════════════════════════════════════════════════
// Union-up: every charter require/inject/validate survives admit
// ══════════════════════════════════════════════════════════════════════════════

fn registry_with_require() -> PolicyRegistry {
    registry()
        .with([def(
            "safety.require_rls",
            KnobKind::Bool,
            Polarity::Require,
            KnobValue::Bool(false),
        )])
        .unwrap()
}

#[test]
fn union_up_charter_obligations_and_injects_and_validates_survive() {
    let reg = registry_with_require();
    let root_toml = r#"policy_version = 1
[[require]]
key = "safety.require_rls"
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
key = "schema.create_table"
value = true
scope = { include = ["app_*"] }
[[grant]]
key = "sql.raw"
value = true
scope = { include = ["staging"] }
"#;
    let root = RootCharter::parse_toml(root_toml, &reg).unwrap();

    let draft = PolicyDoc::parse_toml(
        r#"policy_version = 1
[[grant]]
key = "sql.raw"
value = true
scope = { include = ["staging"] }
"#,
        &reg,
        zero_migrate_policy::LoadContext::NonRootLayer,
    )
    .unwrap();

    let ep = admit(&root, &draft, &reg).unwrap();
    let app_t = ObjectName::table(b"app_main".to_vec(), b"t".to_vec());

    let obs = ep.obligations(&app_t);
    assert!(
        obs.iter()
            .any(|(k, v)| k.as_str() == "safety.require_rls" && *v == KnobValue::Bool(true)),
        "charter require dropped: {obs:?}"
    );
    let injs = ep.injects_for(&app_t);
    assert_eq!(injs.len(), 1, "charter inject dropped");
    assert_eq!(injs[0].columns[0].name, "created_at");
    let vals = ep.validates_for(&app_t);
    assert!(!vals.is_empty(), "charter validate dropped");
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
    let root = RootCharter::parse_toml(
        r#"policy_version = 1
[[require]]
key = "sec.require_approval"
value = "always"
scope = { include = ["app_*"] }
"#,
        &reg,
    )
    .unwrap();
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

    let ep = admit(&root, &draft, &reg).unwrap();
    let app_t = ObjectName::table(b"app_main".to_vec(), b"t".to_vec());
    let obs = ep.obligations(&app_t);

    assert!(
        obs.iter()
            .any(|(k, v)| k.as_str() == "sec.require_approval"
                && *v == KnobValue::Str("always".into())),
        "operator `always` obligation dropped by creator draft: {obs:?}"
    );
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
        "creator `never` must not lower `always`"
    );
}

// ══════════════════════════════════════════════════════════════════════════════
// Compose-time collision blame (admit: draft-vs-charter)
// ══════════════════════════════════════════════════════════════════════════════

#[test]
fn draft_inject_colliding_charter_inject_rejects_at_compose() {
    let reg = registry();
    let root = RootCharter::parse_toml(
        r#"policy_version = 1
[[inject]]
scope = { include = ["app_*"] }
columns = [ { name = "created_at", type = "timestamptz", nullable = false } ]
"#,
        &reg,
    )
    .unwrap();
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
        matches!(got, Err(ComposeError::DraftInjectCollidesCharter { .. })),
        "draft-vs-charter inject collision must REJECT at compose, got {got:?}"
    );
}

#[test]
fn draft_validate_contradicting_charter_inject_rejects_at_compose() {
    let reg = registry();
    let root = RootCharter::parse_toml(
        r#"policy_version = 1
[[inject]]
scope = { include = ["app_*"] }
columns = [ { name = "created_at", type = "timestamptz", nullable = false } ]
"#,
        &reg,
    )
    .unwrap();
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
        matches!(
            got,
            Err(ComposeError::DraftValidateContradictsCharterInject { .. })
        ),
        "draft validate contradicting charter inject must REJECT, got {got:?}"
    );
}

// ══════════════════════════════════════════════════════════════════════════════
// finalize_charter: charter-vs-charter conflicts + creatable-escape
// ══════════════════════════════════════════════════════════════════════════════

#[test]
fn charter_vs_charter_inject_collision_rejects_at_finalize() {
    use zero_migrate_policy::FinalizeError;
    let reg = registry();
    let a = TrustedDoc::register_catalog_entry(
        r#"policy_version = 1
[[inject]]
scope = { include = ["app_*"] }
columns = [ { name = "created_at", type = "timestamptz", nullable = false } ]
"#,
        &reg,
    )
    .unwrap();
    let b = TrustedDoc::register_catalog_entry(
        r#"policy_version = 1
[[inject]]
scope = { include = ["app_main"] }
columns = [ { name = "created_at", type = "text", nullable = true } ]
"#,
        &reg,
    )
    .unwrap();
    let assembled = restrict(&a, &b, &reg).unwrap(); // TOTAL — does NOT reject.
    let got = finalize_charter(assembled);
    assert!(
        matches!(got, Err(FinalizeError::CharterInjectColumnConflict { .. })),
        "charter-vs-charter inject collision must be a loud FINALIZE error, got {got:?}"
    );
}

#[test]
fn creatable_escaping_mandatory_inject_rejects_at_finalize() {
    use zero_migrate_policy::FinalizeError;
    let reg = registry();
    // A single root charter: mandatory inject on app_* only, but create_table @ all.
    // Assemble it via restrict(root_as_trusted, empty_trusted) so it flows through
    // finalize; the creatable-escape must be caught.
    let charter_doc = r#"policy_version = 1
[[inject]]
scope = { include = ["app_*"] }
mandatory = true
columns = [ { name = "created_at", type = "timestamptz", nullable = false } ]
[[grant]]
key = "schema.create_table"
value = true
scope = "all"
"#;
    // A RootCharter carries the mandatory inject; to run it through finalize we take
    // its layers via admit-less path: restrict with an empty trusted doc keeps the
    // mandatory inject and the create grant, then finalize lints the escape.
    let root = RootCharter::parse_toml(charter_doc, &reg).unwrap();
    // Wrap the SAME source as a trusted "base" is illegal (mandatory on non-root), so
    // we assert the finalize lint via the root's own finalize path: build an assembled
    // charter from the root's TrustedDoc is not possible (mandatory), so instead we
    // check the lint fires for a NON-mandatory-equivalent assembled charter below and
    // rely on admit's transitive bound for the root case.
    let _ = root;

    // Assembled (non-root) charter reproducing the escape shape WITHOUT `mandatory`
    // is not an escape (only mandatory injects gate). So we exercise the finalize lint
    // through restrict of two trusted docs where one pins a mandatory-like inject is
    // impossible on non-root. The creatable-escape lint is therefore proven on the
    // ROOT path: a root whose create grant escapes its mandatory inject must fail when
    // finalized. We finalize the root by re-parsing it as the sole layer:
    let assembled = restrict(root.as_trusted(), root.as_trusted(), &reg).unwrap();
    let got = finalize_charter(assembled);
    assert!(
        matches!(
            got,
            Err(FinalizeError::CreatableEscapesMandatoryInject { .. })
        ),
        "creatable escaping mandatory inject must REJECT at finalize, got {got:?}"
    );
}

#[test]
fn creatable_within_mandatory_inject_accepts_at_finalize() {
    let reg = registry();
    let root = RootCharter::parse_toml(
        r#"policy_version = 1
[[inject]]
scope = { include = ["app_*"] }
mandatory = true
columns = [ { name = "created_at", type = "timestamptz", nullable = false } ]
[[grant]]
key = "schema.create_table"
value = true
scope = { include = ["app_*"] }
"#,
        &reg,
    )
    .unwrap();
    let assembled = restrict(root.as_trusted(), root.as_trusted(), &reg).unwrap();
    assert!(
        finalize_charter(assembled).is_ok(),
        "creatable within mandatory inject must finalize"
    );
}

// ══════════════════════════════════════════════════════════════════════════════
// is_injected_shape (name-match-at-op-time)
// ══════════════════════════════════════════════════════════════════════════════

#[test]
fn is_injected_shape_name_matches_covering_inject() {
    use zero_migrate_policy::ShapeElement;
    let reg = registry();
    let root = RootCharter::parse_toml(
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
    assert!(ep.is_injected_shape(&app_t, &ShapeElement::Column("Created_At")));
    assert!(ep.is_injected_shape(&app_t, &ShapeElement::Index("idx_created")));
    assert!(ep.is_injected_shape(&app_t, &ShapeElement::PrimaryKey));
    assert!(!ep.is_injected_shape(&app_t, &ShapeElement::Column("other")));
    assert!(!ep.is_injected_shape(&other, &ShapeElement::Column("created_at")));
    assert!(!ep.is_injected_shape(&other, &ShapeElement::PrimaryKey));
}

// ══════════════════════════════════════════════════════════════════════════════
// Unforgeability
// ══════════════════════════════════════════════════════════════════════════════

#[test]
fn effective_policy_only_via_admit_or_deny_all() {
    let reg = registry();

    let floor = zero_migrate_policy::EffectivePolicy::deny_all(&reg);
    let o = ObjectName::table(b"app_main".to_vec(), b"t".to_vec());
    assert_eq!(
        floor.grants(&KnobKey::parse(BOOL_KEY).unwrap(), &o),
        Some(KnobValue::Bool(false))
    );

    let root = parse_root(&[Gen {
        key: BOOL_KEY,
        pat: Pat::AppStar,
        value: "true",
    }]);
    let draft = parse_draft(&[Gen {
        key: BOOL_KEY,
        pat: Pat::AppStar,
        value: "true",
    }]);
    let ep = admit(&root, &draft, &reg).unwrap();
    assert_eq!(
        ep.grants(&KnobKey::parse(BOOL_KEY).unwrap(), &o),
        Some(KnobValue::Bool(true))
    );

    // A finalized restrict charter, then admitted.
    let a = parse_trusted(&[Gen {
        key: BOOL_KEY,
        pat: Pat::AppStar,
        value: "true",
    }]);
    let b = parse_trusted(&[Gen {
        key: BOOL_KEY,
        pat: Pat::AppStar,
        value: "true",
    }]);
    let charter = finalize_charter(restrict(&a, &b, &reg).unwrap()).unwrap();
    let _ep2 = admit(&charter, &draft, &reg).unwrap();

    // There is deliberately NO other constructor: no Default, no Deserialize, no
    // public `new`. And an AssembledCharter is NOT an AdmitCharter — it cannot reach
    // admit until finalized. This test compiling is the proof the surface is closed.
}

// ══════════════════════════════════════════════════════════════════════════════
// inherit = false — a SILENT draft does NOT inherit a power-grant from the charter
// ══════════════════════════════════════════════════════════════════════════════

/// A registry whose one grant knob is a POWER GRANT (`inherit = false`), plus an
/// ordinary inheritable knob for contrast.
fn registry_with_noninherit() -> PolicyRegistry {
    let power = KnobDef {
        key: KnobKey::parse("schema.alter_injected").unwrap(),
        kind: KnobKind::Bool,
        polarity: Polarity::Grant,
        default: KnobValue::Bool(false),
        enforcement: Enforcement::Enforced,
        object_model: zero_migrate_policy::ObjectModel::PerTable,
        requires_db_privilege: false,
        inherit: false,
        docs: String::new(),
    };
    PolicyRegistry::empty()
        .with([
            power,
            def(
                BOOL_KEY,
                KnobKind::Bool,
                Polarity::Grant,
                KnobValue::Bool(false),
            ),
        ])
        .unwrap()
}

#[test]
fn silent_draft_does_not_inherit_noninherit_grant() {
    let reg = registry_with_noninherit();
    let power_key = KnobKey::parse("schema.alter_injected").unwrap();
    let bool_key = KnobKey::parse(BOOL_KEY).unwrap();

    // Charter GRANTS the power knob (and the ordinary knob) over app_*.
    let root = RootCharter::parse_toml(
        r#"policy_version = 1
[[grant]]
key = "schema.alter_injected"
value = true
scope = { include = ["app_*"] }
[[grant]]
key = "sql.raw"
value = true
scope = { include = ["app_*"] }
"#,
        &reg,
    )
    .unwrap();

    // A SILENT draft (grants nothing).
    let draft = PolicyDoc::parse_toml(
        "policy_version = 1\n",
        &reg,
        zero_migrate_policy::LoadContext::NonRootLayer,
    )
    .unwrap();
    let ep = admit(&root, &draft, &reg).unwrap();

    let app_t = ObjectName::table(b"app_main".to_vec(), b"t".to_vec());

    // The power grant is NOT inherited by the silent draft → it reads the DEFAULT
    // (deny), NOT the charter's granted `true`.
    assert_eq!(
        ep.grants(&power_key, &app_t),
        Some(KnobValue::Bool(false)),
        "an inherit=false grant must NOT flow to a silent draft"
    );
    // The ordinary (inheritable) grant IS inherited — the charter's `true` shows through.
    assert_eq!(
        ep.grants(&bool_key, &app_t),
        Some(KnobValue::Bool(true)),
        "an ordinary grant is still charter-inherited by a silent draft"
    );
}

#[test]
fn explicit_draft_may_still_earn_a_noninherit_grant_within_charter() {
    // inherit=false only blocks INHERITANCE-BY-OMISSION; a draft that EXPLICITLY
    // grants the power knob (⊑ the charter) still gets it — the normal escalation
    // check governs.
    let reg = registry_with_noninherit();
    let power_key = KnobKey::parse("schema.alter_injected").unwrap();

    let root = RootCharter::parse_toml(
        r#"policy_version = 1
[[grant]]
key = "schema.alter_injected"
value = true
scope = { include = ["app_*"] }
"#,
        &reg,
    )
    .unwrap();
    let draft = PolicyDoc::parse_toml(
        r#"policy_version = 1
[[grant]]
key = "schema.alter_injected"
value = true
scope = { include = ["app_main"] }
"#,
        &reg,
        zero_migrate_policy::LoadContext::NonRootLayer,
    )
    .unwrap();
    let ep = admit(&root, &draft, &reg).unwrap();

    let app_main_t = ObjectName::table(b"app_main".to_vec(), b"t".to_vec());
    let app_other_t = ObjectName::table(b"app_tmp_x".to_vec(), b"t".to_vec());

    // Where the draft EXPLICITLY asked (app_main), it earns the grant.
    assert_eq!(
        ep.grants(&power_key, &app_main_t),
        Some(KnobValue::Bool(true))
    );
    // Where it stayed silent (app_tmp_x, still under the charter's app_*), it does
    // NOT inherit — default deny.
    assert_eq!(
        ep.grants(&power_key, &app_other_t),
        Some(KnobValue::Bool(false))
    );
}

// Pat needs Debug for the panic messages.
impl std::fmt::Debug for Pat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Pat::AppStar => "app_*",
            Pat::AppStarNoTmp => "app_*\\app_tmp_*",
            Pat::Staging => "staging",
            Pat::All => "all",
        })
    }
}

/// A grant that a later layer pulls back anywhere is not universal, so it must not
/// read as `Top`.
///
/// `Top` is a claim about the whole universe, and the guard spends it: `sql.raw` at
/// `Top` is the fully-trusted raw posture, and `schema.cross_schema` at `Top` makes
/// the schema scope `Unconfined`. The per-layer visible region is granted-minus-
/// masked-above, and `All` minus a real mask has no glob representation, so the
/// estimate widens back to `All` - which used to be read straight back as `Top`.
#[test]
fn a_grant_narrowed_anywhere_is_scoped_not_top() {
    let reg = registry();
    // base (root): sql.raw granted over the WHOLE universe.
    let base = TrustedDoc::register_catalog_entry(
        "policy_version = 1\n[[grant]]\nkey = \"sql.raw\"\nvalue = true\nscope = \"all\"\n",
        &reg,
    )
    .unwrap();
    // over: sql.raw pulled back to the default (false) on `secret` only.
    let over = TrustedDoc::register_catalog_entry(
        "policy_version = 1\n[[grant]]\nkey = \"sql.raw\"\nvalue = false\nscope = { include = [\"secret\"] }\n",
        &reg,
    )
    .unwrap();
    let charter = finalize_charter(overlay(&base, &over, &reg).unwrap()).unwrap();
    let empty = PolicyDoc::parse_toml(
        "policy_version = 1\n",
        &reg,
        zero_migrate_policy::LoadContext::NonRootLayer,
    )
    .unwrap();
    let ep = admit(&charter, &empty, &reg).unwrap();

    let key = KnobKey::parse("sql.raw").unwrap();
    let secret_t = ObjectName::table(b"secret".to_vec(), b"t".to_vec());
    let app_t = ObjectName::table(b"app_main".to_vec(), b"t".to_vec());

    // The hole is real: the grant is genuinely below its granted value on `secret`
    // and above it everywhere else.
    assert_eq!(
        ep.grants(&key, &secret_t),
        Some(KnobValue::Bool(false)),
        "the later layer pulls sql.raw back to default on secret"
    );
    assert_eq!(
        ep.grants(&key, &app_t),
        Some(KnobValue::Bool(true)),
        "sql.raw stays granted outside the narrowed scope"
    );

    // So the region is Scoped, and universality is false.
    assert_eq!(ep.grant_region(&key), GrantRegion::Scoped);
    assert!(
        !ep.grant_is_top(&key),
        "a grant denied somewhere must never report as granted everywhere"
    );
}

/// The counterpart: an unnarrowed universal grant must still reach `Top`, so the
/// fix above does not simply deny everything.
#[test]
fn an_unnarrowed_universal_grant_is_still_top() {
    let reg = registry();
    let base = TrustedDoc::register_catalog_entry(
        "policy_version = 1\n[[grant]]\nkey = \"sql.raw\"\nvalue = true\nscope = \"all\"\n",
        &reg,
    )
    .unwrap();
    let empty_over = TrustedDoc::register_catalog_entry("policy_version = 1\n", &reg).unwrap();
    let charter = finalize_charter(overlay(&base, &empty_over, &reg).unwrap()).unwrap();
    let empty = PolicyDoc::parse_toml(
        "policy_version = 1\n",
        &reg,
        zero_migrate_policy::LoadContext::NonRootLayer,
    )
    .unwrap();
    let ep = admit(&charter, &empty, &reg).unwrap();

    let key = KnobKey::parse("sql.raw").unwrap();
    assert_eq!(ep.grant_region(&key), GrantRegion::Top);
    assert!(ep.grant_is_top(&key));
}

/// A draft cannot re-grant authority a later charter layer took away.
///
/// The charter grants `sql.raw` everywhere at the root and denies it at `secret` in a
/// second layer. `admit` partitions the draft's granted scope by the charter's rule
/// scopes and compares values at one witness per region, so `secret` has to be its own
/// region: a rule whose value is at or below default grants nothing but still MASKS,
/// and dropping it from the partition left a hole the single witness could miss.
///
/// The oracle sweep does not reach this shape - it generates one rule per key per
/// document, so no charter it builds has a masked hole.
#[test]
fn a_draft_cannot_regrant_over_a_masked_hole() {
    let reg = registry();
    let root = TrustedDoc::register_catalog_entry(
        "policy_version = 1\n[[grant]]\nkey = \"sql.raw\"\nvalue = true\nscope = \"all\"\n",
        &reg,
    )
    .unwrap();
    let masking_layer = TrustedDoc::register_catalog_entry(
        "policy_version = 1\n[[grant]]\nkey = \"sql.raw\"\nvalue = false\nscope = { include = [\"secret\"] }\n",
        &reg,
    )
    .unwrap();
    let charter = finalize_charter(overlay(&root, &masking_layer, &reg).unwrap()).unwrap();

    let key = KnobKey::parse("sql.raw").unwrap();
    let secret_t = ObjectName::table(b"secret".to_vec(), b"t".to_vec());
    let silent = || {
        PolicyDoc::parse_toml(
            "policy_version = 1\n",
            &reg,
            zero_migrate_policy::LoadContext::NonRootLayer,
        )
        .unwrap()
    };

    // The charter itself denies at secret, so there is real authority to re-grant.
    assert_eq!(
        admit(&charter, &silent(), &reg)
            .unwrap()
            .grants(&key, &secret_t),
        Some(KnobValue::Bool(false)),
        "the masking layer must deny sql.raw at secret"
    );

    // An untrusted draft re-granting over the whole universe must be refused.
    let draft = PolicyDoc::parse_toml(
        "policy_version = 1\n[[grant]]\nkey = \"sql.raw\"\nvalue = true\nscope = \"all\"\n",
        &reg,
        zero_migrate_policy::LoadContext::NonRootLayer,
    )
    .unwrap();
    let err = admit(&charter, &draft, &reg)
        .expect_err("re-granting into a masked hole must not be admitted");
    assert!(
        matches!(err, ComposeError::GrantExceedsCharter { .. }),
        "expected GrantExceedsCharter, got {err:?}"
    );

    // A draft that stays inside the charter is still admitted.
    let ok_draft = PolicyDoc::parse_toml(
        "policy_version = 1\n[[grant]]\nkey = \"sql.raw\"\nvalue = true\nscope = { include = [\"app_main\"] }\n",
        &reg,
        zero_migrate_policy::LoadContext::NonRootLayer,
    )
    .unwrap();
    let ep =
        admit(&charter, &ok_draft, &reg).expect("a draft within the charter is still admitted");
    assert_eq!(
        ep.grants(&key, &secret_t),
        Some(KnobValue::Bool(false)),
        "the mask survives an admitted draft"
    );
}

/// The admit oracle over a LAYERED charter, so a masked hole is in the universe.
///
/// The main sweep builds its charter from a single root document, so no charter it
/// generates can have a hole: one rule per key means the charter's value is constant
/// over each rule's scope. That is exactly the shape `admit` got wrong - a root
/// granting broadly with a later layer pulling back on a sub-scope, where the covered
/// region arm sampled one witness and could miss the pull-back.
///
/// This sweep composes `overlay(base, over)` for every pattern/value pair and admits
/// every draft against it. The ground truth is presence-based last-wins, the same
/// rule `oracle_overlay_is_presence_last_wins` pins, fed into the same
/// `gt_admissible` predicate the main sweep uses.
#[test]
fn oracle_admit_over_a_layered_charter_with_masked_holes() {
    let univ = universe();
    let reg = registry();
    let pats = Pat::all();
    let bool_vals = ["true", "false"];

    let mut total = 0usize;
    let mut accepted = 0usize;
    let mut rejected = 0usize;

    for &bp in &pats {
        for bv in bool_vals {
            for &op in &pats {
                for ov in bool_vals {
                    let base_gens = vec![Gen {
                        key: BOOL_KEY,
                        pat: bp,
                        value: bv,
                    }];
                    let over_gens = vec![Gen {
                        key: BOOL_KEY,
                        pat: op,
                        value: ov,
                    }];
                    let charter = finalize_charter(
                        overlay(&parse_trusted(&base_gens), &parse_trusted(&over_gens), &reg)
                            .unwrap(),
                    )
                    .unwrap();

                    // The charter's effective value: presence-based last-wins, over
                    // the base. Computed from the rule lists, never from the composer.
                    let base_doc = parse_draft(&base_gens);
                    let over_doc = parse_draft(&over_gens);
                    let cval = |key: &str, kind: &KnobKind, default: &KnobValue, o: &ObjectName| {
                        if covers_gt(&over_doc, key, o) {
                            value_gt(&over_doc, kind, default, key, o)
                        } else {
                            value_gt(&base_doc, kind, default, key, o)
                        }
                    };

                    for &dpat in &pats {
                        for dv in bool_vals {
                            let draft_gens = vec![Gen {
                                key: BOOL_KEY,
                                pat: dpat,
                                value: dv,
                            }];
                            let draft = parse_draft(&draft_gens);

                            let gt = gt_admissible(&cval, &draft, &univ);
                            let got = admit(&charter, &draft, &reg);

                            total += 1;
                            match (&got, gt) {
                                (Ok(_), true) => accepted += 1,
                                (Err(_), false) => rejected += 1,
                                (Ok(ep), false) => panic!(
                                    "FALSE ACCEPT over a layered charter (escalation \
                                     slipped through)!\n base={base_gens:?}\n \
                                     over={over_gens:?}\n draft={draft_gens:?}\n{ep:?}"
                                ),
                                (Err(e), true) => panic!(
                                    "FALSE REJECT over a layered charter (should have \
                                     composed)!\n base={base_gens:?}\n over={over_gens:?}\n \
                                     draft={draft_gens:?}\n err={e:?}"
                                ),
                            }
                        }
                    }
                }
            }
        }
    }

    assert!(total >= 200, "layered admit sweep too small: {total}");
    assert!(
        accepted > 0 && rejected > 0,
        "sweep must exercise both arms"
    );
}
