//! Document-loader legality-gate suite (Phase 1b-i). Per gate: one ACCEPT + one
//! REJECT asserting the exact `LoadError` variant. Plus registry-digest stability,
//! the doc's II.3 example TOML loading legally as a root ceiling, and the II.2.7
//! name-normalization cases. No composition (Phase 1b-ii).

use zero_migrate_policy::{
    Enforcement, KnobDef, KnobKey, KnobKind, KnobValue, LoadContext, LoadError, LoadWarning,
    ObjectModel, Polarity, PolicyDoc, PolicyRegistry, RuleKind, Scope, ValidatePredicate,
};

// ── registry fixtures ────────────────────────────────────────────────────────────

fn def(
    key: &str,
    kind: KnobKind,
    polarity: Polarity,
    default: KnobValue,
    om: ObjectModel,
    requires_db_privilege: bool,
) -> KnobDef {
    KnobDef {
        key: KnobKey::parse(key).unwrap(),
        kind,
        polarity,
        default,
        enforcement: Enforcement::Enforced,
        object_model: om,
        requires_db_privilege,
        inherit: true,
        docs: String::new(),
    }
}

/// A registry with the knobs the II.3 example + the gate tests reference.
fn registry() -> PolicyRegistry {
    PolicyRegistry::empty()
        .with([
            def(
                "code.extension",
                KnobKind::Bool,
                Polarity::Grant,
                KnobValue::Bool(false),
                ObjectModel::Global,
                true,
            ),
            def(
                "sql.raw",
                KnobKind::Bool,
                Polarity::Grant,
                KnobValue::Bool(false),
                ObjectModel::PerTable,
                false,
            ),
            def(
                "schema.create_table",
                KnobKind::Bool,
                Polarity::Grant,
                KnobValue::Bool(false),
                ObjectModel::PerTable,
                false,
            ),
            def(
                "schema.create_schema",
                KnobKind::Bool,
                Polarity::Grant,
                KnobValue::Bool(false),
                ObjectModel::PerSchema,
                false,
            ),
            def(
                "safety.require_rls",
                KnobKind::Bool,
                Polarity::Require,
                KnobValue::Bool(false),
                ObjectModel::PerTable,
                false,
            ),
            def(
                "runtime.lock_timeout_ms",
                KnobKind::UintCeiling { hard_floor: 1 },
                Polarity::Grant,
                KnobValue::Uint(1),
                ObjectModel::PerTable,
                false,
            ),
        ])
        .unwrap()
}

fn load_root(toml: &str) -> Result<PolicyDoc, LoadError> {
    PolicyDoc::parse_toml(toml, &registry(), LoadContext::RootCeiling)
}

fn load_draft(toml: &str) -> Result<PolicyDoc, LoadError> {
    PolicyDoc::parse_toml(toml, &registry(), LoadContext::NonRootLayer)
}

// ── gate: policy_version ─────────────────────────────────────────────────────────

#[test]
fn policy_version_known_accepts() {
    let doc = load_root("policy_version = 1\n").unwrap();
    assert_eq!(doc.policy_version, 1);
    assert!(doc.rules.is_empty()); // empty doc => grant/require/inject/validate nothing
}

#[test]
fn policy_version_unknown_rejects() {
    let e = load_root("policy_version = 2\n").unwrap_err();
    assert_eq!(e, LoadError::UnknownPolicyVersion { found: 2 });
}

// ── gate: unknown / malformed knob key ───────────────────────────────────────────

#[test]
fn known_key_accepts() {
    let doc = load_root(
        r#"policy_version = 1
[[grant]]
key = "sql.raw"
value = true
scope = { include = ["staging"] }
"#,
    )
    .unwrap();
    assert_eq!(doc.rules.len(), 1);
}

#[test]
fn unknown_key_rejects() {
    let e = load_root(
        r#"policy_version = 1
[[grant]]
key = "acme.unregistered"
value = true
scope = "all"
"#,
    )
    .unwrap_err();
    assert_eq!(
        e,
        LoadError::UnknownKnobKey {
            key: "acme.unregistered".into()
        }
    );
}

#[test]
fn malformed_key_rejects() {
    let e = load_root(
        r#"policy_version = 1
[[grant]]
key = "nodotkey"
value = true
scope = "all"
"#,
    )
    .unwrap_err();
    assert_eq!(
        e,
        LoadError::MalformedKnobKey {
            key: "nodotkey".into()
        }
    );
}

// ── gate: section ↔ polarity ─────────────────────────────────────────────────────

#[test]
fn grant_of_grant_polarity_accepts() {
    // schema.create_table is Grant-polarity → legal under [[grant]].
    assert!(load_root(
        r#"policy_version = 1
[[grant]]
key = "schema.create_table"
value = true
scope = { include = ["app_*"] }
"#,
    )
    .is_ok());
}

#[test]
fn grant_of_require_polarity_rejects() {
    // safety.require_rls is Require-polarity → illegal under [[grant]].
    let e = load_root(
        r#"policy_version = 1
[[grant]]
key = "safety.require_rls"
value = true
scope = { include = ["tenant_*"] }
"#,
    )
    .unwrap_err();
    assert!(matches!(
        e,
        LoadError::SectionPolarityMismatch { ref key, expected: Polarity::Grant, found: Polarity::Require }
            if key == "safety.require_rls"
    ));
}

#[test]
fn require_of_grant_polarity_rejects() {
    let e = load_root(
        r#"policy_version = 1
[[require]]
key = "sql.raw"
value = true
scope = { include = ["app_*"] }
"#,
    )
    .unwrap_err();
    assert!(matches!(
        e,
        LoadError::SectionPolarityMismatch {
            expected: Polarity::Require,
            found: Polarity::Grant,
            ..
        }
    ));
}

// ── gate: knob value validity (kind + hard floor) ────────────────────────────────

#[test]
fn valid_uint_ceiling_accepts() {
    assert!(load_root(
        r#"policy_version = 1
[[grant]]
key = "runtime.lock_timeout_ms"
value = 3000
scope = { include = ["app_*"] }
"#,
    )
    .is_ok());
}

#[test]
fn value_below_hard_floor_rejects() {
    let e = load_root(
        r#"policy_version = 1
[[grant]]
key = "runtime.lock_timeout_ms"
value = 0
scope = { include = ["app_*"] }
"#,
    )
    .unwrap_err();
    assert!(
        matches!(e, LoadError::InvalidKnobValue { ref key, .. } if key == "runtime.lock_timeout_ms")
    );
}

#[test]
fn value_wrong_type_rejects() {
    // Bool value for a UintCeiling knob.
    let e = load_root(
        r#"policy_version = 1
[[grant]]
key = "runtime.lock_timeout_ms"
value = true
scope = { include = ["app_*"] }
"#,
    )
    .unwrap_err();
    assert!(matches!(e, LoadError::InvalidKnobValue { .. }));
}

// ── gate: Global-knob scope legality ─────────────────────────────────────────────

#[test]
fn global_knob_scope_all_accepts() {
    let doc = load_root(
        r#"policy_version = 1
[[grant]]
key = "code.extension"
value = true
scope = "all"
"#,
    )
    .unwrap();
    // Effective scope stays All (exempt from any default-scope meet).
    assert_eq!(doc.rules.len(), 1);
    assert_eq!(doc.rules[0].scope, Scope::All);
}

#[test]
fn global_knob_narrow_scope_rejects() {
    let e = load_root(
        r#"policy_version = 1
[[grant]]
key = "code.extension"
value = true
scope = { include = ["app_*"] }
"#,
    )
    .unwrap_err();
    assert_eq!(
        e,
        LoadError::ScopeIllegalForGlobalKnob {
            key: "code.extension".into()
        }
    );
}

#[test]
fn global_knob_of_star_scope_rejects_syntactically() {
    // `Of{["*"]}` denotes the universe but is NOT the syntactic `All` token — a
    // Global knob must be spelled `all` loudly (II.2.3 syntactic-⊤ decision).
    let e = load_root(
        r#"policy_version = 1
[[grant]]
key = "code.extension"
value = true
scope = { include = ["*"] }
"#,
    )
    .unwrap_err();
    assert_eq!(
        e,
        LoadError::ScopeIllegalForGlobalKnob {
            key: "code.extension".into()
        }
    );
}

#[test]
fn global_knob_omitted_scope_rejects() {
    // Omission would inherit the (narrow) default — a load error for a Global knob.
    let e = load_root(
        r#"policy_version = 1
[default_scope]
include = ["app_*"]
[[grant]]
key = "code.extension"
value = true
"#,
    )
    .unwrap_err();
    assert_eq!(
        e,
        LoadError::ScopeIllegalForGlobalKnob {
            key: "code.extension".into()
        }
    );
}

#[test]
fn global_knob_exempt_from_default_meet() {
    // The doc's own example: default_scope = app_* + code.extension scope = all is
    // LEGAL, and the Global rule's effective scope stays All, NOT app_*.
    let doc = load_root(
        r#"policy_version = 1
[default_scope]
include = ["app_*"]
[[grant]]
key = "code.extension"
value = true
scope = "all"
"#,
    )
    .unwrap();
    let g = doc
        .rules
        .iter()
        .find(|r| matches!(&r.kind, RuleKind::Grant { .. }))
        .unwrap();
    assert_eq!(g.scope, Scope::All);
}

// ── gate: PerSchema granularity ──────────────────────────────────────────────────

#[test]
fn per_schema_knob_schema_scope_accepts() {
    assert!(load_root(
        r#"policy_version = 1
[[grant]]
key = "schema.create_schema"
value = true
scope = { include = ["app_*"] }
"#,
    )
    .is_ok());
}

#[test]
fn per_schema_knob_table_scope_rejects() {
    let e = load_root(
        r#"policy_version = 1
[[grant]]
key = "schema.create_schema"
value = true
scope = { include = ["app_main.events"] }
"#,
    )
    .unwrap_err();
    assert_eq!(
        e,
        LoadError::ScopeTooGranularForKnob {
            key: "schema.create_schema".into()
        }
    );
}

// ── gate: grant scope unbounded (A3) ─────────────────────────────────────────────

#[test]
fn grant_with_default_scope_accepts() {
    // A grant with no own scope is fine when a default_scope exists (it inherits).
    let doc = load_root(
        r#"policy_version = 1
[default_scope]
include = ["app_*"]
[[grant]]
key = "sql.raw"
value = true
"#,
    )
    .unwrap();
    let g = &doc.rules[0];
    // Inherited the app_* default.
    assert!(matches!(&g.scope, Scope::Of { .. }));
}

#[test]
fn grant_without_scope_or_default_rejects() {
    let e = load_root(
        r#"policy_version = 1
[[grant]]
key = "sql.raw"
value = true
"#,
    )
    .unwrap_err();
    assert_eq!(
        e,
        LoadError::GrantScopeUnbounded {
            key: "sql.raw".into()
        }
    );
}

// ── gate: mandatory inject on non-root layer ─────────────────────────────────────

const MANDATORY_INJECT_DOC: &str = r#"policy_version = 1
[[inject]]
scope = { include = ["app_*"] }
mandatory = true
columns = [ { name = "created_at", type = "timestamptz", nullable = false } ]
"#;

#[test]
fn mandatory_inject_on_root_accepts() {
    let doc = load_root(MANDATORY_INJECT_DOC).unwrap();
    assert_eq!(doc.rules.len(), 1);
    match &doc.rules[0].kind {
        RuleKind::Inject { spec } => {
            assert!(spec.mandatory);
            assert_eq!(spec.columns.len(), 1);
        }
        _ => panic!("expected an inject rule"),
    }
}

#[test]
fn mandatory_inject_on_draft_rejects() {
    let e = load_draft(MANDATORY_INJECT_DOC).unwrap_err();
    assert_eq!(e, LoadError::MandatoryInjectOnNonRootLayer);
}

#[test]
fn non_mandatory_inject_on_draft_accepts() {
    let doc = load_draft(
        r#"policy_version = 1
[[inject]]
scope = { include = ["app_*"] }
columns = [ { name = "created_at", type = "timestamptz", nullable = false } ]
"#,
    )
    .unwrap();
    assert_eq!(doc.rules.len(), 1);
}

// ── gate: self-contradictory inject + validate ───────────────────────────────────

#[test]
fn non_contradictory_inject_validate_accepts() {
    // Inject created_at + forbid a DIFFERENT column → consistent.
    assert!(load_root(
        r#"policy_version = 1
[[inject]]
scope = { include = ["app_*"] }
columns = [ { name = "created_at", type = "timestamptz", nullable = false } ]
[[validate]]
scope = { include = ["app_*"] }
predicate = { kind = "forbidden_columns", names = ["secret"] }
"#,
    )
    .is_ok());
}

#[test]
fn contradictory_inject_validate_rejects() {
    let e = load_root(
        r#"policy_version = 1
[[inject]]
scope = { include = ["app_*"] }
columns = [ { name = "created_at", type = "timestamptz", nullable = false } ]
[[validate]]
scope = { include = ["app_*"] }
predicate = { kind = "forbidden_columns", names = ["created_at"] }
"#,
    )
    .unwrap_err();
    assert!(matches!(
        e,
        LoadError::SelfContradictoryInjectValidate { .. }
    ));
}

#[test]
fn contradiction_on_disjoint_scope_accepts() {
    // Inject in app_*, forbid the same name in staging — disjoint scopes, no clash.
    assert!(load_root(
        r#"policy_version = 1
[[inject]]
scope = { include = ["app_*"] }
columns = [ { name = "created_at", type = "timestamptz", nullable = false } ]
[[validate]]
scope = { include = ["staging"] }
predicate = { kind = "forbidden_columns", names = ["created_at"] }
"#,
    )
    .is_ok());
}

#[test]
fn contradiction_detected_via_case_fold() {
    // Inject `Created_At` (folds to created_at) forbidden by `created_at`.
    let e = load_root(
        r#"policy_version = 1
[[inject]]
scope = { include = ["app_*"] }
columns = [ { name = "Created_At", type = "timestamptz", nullable = false } ]
[[validate]]
scope = { include = ["app_*"] }
predicate = { kind = "forbidden_columns", names = ["created_at"] }
"#,
    )
    .unwrap_err();
    assert!(matches!(
        e,
        LoadError::SelfContradictoryInjectValidate { .. }
    ));
}

// ── gate: malformed scope / empty include ────────────────────────────────────────

#[test]
fn empty_include_rejects() {
    let e = load_root(
        r#"policy_version = 1
[[grant]]
key = "sql.raw"
value = true
scope = { include = [] }
"#,
    )
    .unwrap_err();
    assert_eq!(e, LoadError::EmptyInclude);
}

#[test]
fn malformed_scope_pattern_rejects() {
    let e = load_root(
        r#"policy_version = 1
[[grant]]
key = "sql.raw"
value = true
scope = { include = ["a.b.c"] }
"#,
    )
    .unwrap_err();
    assert!(matches!(e, LoadError::MalformedScope { .. }));
}

// ── deny_unknown_fields ──────────────────────────────────────────────────────────

#[test]
fn unknown_field_rejects() {
    let e = load_root(
        r#"policy_version = 1
[[grant]]
key = "sql.raw"
value = true
scope = "all"
bogus = 3
"#,
    )
    .unwrap_err();
    assert!(matches!(e, LoadError::Parse { .. }));
}

// ── dead rule → warning ──────────────────────────────────────────────────────────

#[test]
fn dead_rule_warns_not_errors() {
    // default_scope app_* ⊓ own scope staging = disjoint = Nothing → dead rule.
    let doc = load_root(
        r#"policy_version = 1
[default_scope]
include = ["app_*"]
[[grant]]
key = "sql.raw"
value = true
scope = { include = ["staging"] }
"#,
    )
    .unwrap();
    assert_eq!(doc.rules.len(), 1);
    assert_eq!(doc.rules[0].scope, Scope::Nothing);
    assert_eq!(doc.warnings, vec![LoadWarning::DeadRule { index: 0 }]);
}

// ── registry digest stability ────────────────────────────────────────────────────

#[test]
fn registry_digest_is_shuffle_stable() {
    let a = def(
        "sql.raw",
        KnobKind::Bool,
        Polarity::Grant,
        KnobValue::Bool(false),
        ObjectModel::PerTable,
        false,
    );
    let b = def(
        "code.extension",
        KnobKind::Bool,
        Polarity::Grant,
        KnobValue::Bool(false),
        ObjectModel::Global,
        true,
    );
    let c = def(
        "safety.require_rls",
        KnobKind::Bool,
        Polarity::Require,
        KnobValue::Bool(false),
        ObjectModel::PerTable,
        false,
    );
    let r1 = PolicyRegistry::empty()
        .with([a.clone(), b.clone(), c.clone()])
        .unwrap();
    let r2 = PolicyRegistry::empty().with([c, b, a]).unwrap();
    assert_eq!(r1.digest(), r2.digest());
    assert_eq!(r1.digest_hex(), r2.digest_hex());
}

#[test]
fn registry_digest_changes_on_requires_db_privilege() {
    let d1 = def(
        "code.extension",
        KnobKind::Bool,
        Polarity::Grant,
        KnobValue::Bool(false),
        ObjectModel::Global,
        true,
    );
    let d2 = def(
        "code.extension",
        KnobKind::Bool,
        Polarity::Grant,
        KnobValue::Bool(false),
        ObjectModel::Global,
        false,
    );
    let r1 = PolicyRegistry::empty().with([d1]).unwrap();
    let r2 = PolicyRegistry::empty().with([d2]).unwrap();
    assert_ne!(r1.digest(), r2.digest());
}

#[test]
fn registry_digest_changes_on_object_model() {
    let d1 = def(
        "sql.raw",
        KnobKind::Bool,
        Polarity::Grant,
        KnobValue::Bool(false),
        ObjectModel::PerTable,
        false,
    );
    let d2 = def(
        "sql.raw",
        KnobKind::Bool,
        Polarity::Grant,
        KnobValue::Bool(false),
        ObjectModel::PerSchema,
        false,
    );
    let r1 = PolicyRegistry::empty().with([d1]).unwrap();
    let r2 = PolicyRegistry::empty().with([d2]).unwrap();
    assert_ne!(r1.digest(), r2.digest());
}

// ── the doc's II.3 example TOML loads legally as a root ceiling ───────────────────

#[test]
fn doc_example_loads_as_root_ceiling() {
    let example = r#"policy_version = 1

[default_scope]
include = ["app_*"]

[[grant]]
key   = "code.extension"
value = true
scope = "all"

[[grant]]
key   = "sql.raw"
value = true
scope = { include = ["staging"] }

[[grant]]
key   = "schema.create_table"
value = true
scope = { include = ["app_*"] }

[[require]]
key   = "safety.require_rls"
value = true
scope = { include = ["tenant_*"] }

[[inject]]
scope   = { include = ["*"], exclude = ["*.journal", "public.schema_migrations"] }
columns = [ { name = "created_at", type = "timestamptz", nullable = false } ]

[[validate]]
scope     = { include = ["app_*"] }
predicate = { kind = "table_name_forbidden", patterns = ["*.journal", "*.schema_migrations"] }

[[validate]]
scope     = { include = ["app_*"] }
predicate = { kind = "has_primary_key" }
"#;

    let doc = load_root(example).unwrap();
    // 3 grants + 1 require + 1 inject + 2 validates = 7 rules.
    assert_eq!(doc.rules.len(), 7);

    // The Global code.extension grant stays All despite default_scope = app_*.
    let pg = doc
        .rules
        .iter()
        .find(
            |r| matches!(&r.kind, RuleKind::Grant { key, .. } if key.as_str() == "code.extension"),
        )
        .unwrap();
    assert_eq!(pg.scope, Scope::All);

    // The raw_sql grant narrowed to app_* ⊓ staging. staging is disjoint from
    // app_* → Nothing → a dead-rule warning (the example's staging is outside
    // the app_* default; that is a legal-but-inert composition).
    let raw = doc
        .rules
        .iter()
        .find(|r| matches!(&r.kind, RuleKind::Grant { key, .. } if key.as_str() == "sql.raw"))
        .unwrap();
    assert_eq!(raw.scope, Scope::Nothing);
    assert!(!doc.warnings.is_empty());

    // The has_primary_key validate is present.
    assert!(doc.rules.iter().any(|r| matches!(
        &r.kind,
        RuleKind::Validate {
            pred: ValidatePredicate::HasPrimaryKey
        }
    )));
}

// ── JSON front-end parity ────────────────────────────────────────────────────────

#[test]
fn json_front_end_loads() {
    let json = r#"{
        "policy_version": 1,
        "default_scope": { "include": ["app_*"] },
        "grant": [ { "key": "sql.raw", "value": true, "scope": { "include": ["app_main"] } } ],
        "validate": [ { "scope": { "include": ["app_*"] }, "predicate": { "kind": "has_primary_key" } } ]
    }"#;
    let doc = PolicyDoc::parse_json(json, &registry(), LoadContext::RootCeiling).unwrap();
    assert_eq!(doc.rules.len(), 2);
}

// ── name normalization (II.2.7) through the loader ───────────────────────────────

#[test]
fn normalization_unquoted_folds_lowercase() {
    // Scope pattern `App_*` folds to app_* — a proper Of, not a malformed scope.
    let doc = load_root(
        r#"policy_version = 1
[[grant]]
key = "sql.raw"
value = true
scope = { include = ["App_*"] }
"#,
    )
    .unwrap();
    match &doc.rules[0].scope {
        Scope::Of { include, .. } => {
            // The folded schema glob is app_* (matches app_x).
            assert!(include[0].matches(&zero_migrate_policy::ObjectName::schema(b"app_x".to_vec())));
        }
        other => panic!("expected Of, got {other:?}"),
    }
}

#[test]
fn normalization_quoted_verbatim() {
    // A quoted `"App_x"` scope is byte-exact — matches App_x, not app_x.
    let doc = load_root(
        r#"policy_version = 1
[[grant]]
key = "sql.raw"
value = true
scope = { include = ["\"App_x\""] }
"#,
    )
    .unwrap();
    match &doc.rules[0].scope {
        Scope::Of { include, .. } => {
            assert!(include[0].matches(&zero_migrate_policy::ObjectName::schema(b"App_x".to_vec())));
            assert!(
                !include[0].matches(&zero_migrate_policy::ObjectName::schema(b"app_x".to_vec()))
            );
        }
        other => panic!("expected Of, got {other:?}"),
    }
}

#[test]
fn normalization_quoted_dot_is_one_segment() {
    // `"a.b"` is ONE segment; the schema pattern `a` must not match object `a.b`.
    // Loaded on a PerTable knob (so a dotted single-segment quoted name is legal).
    let doc = load_root(
        r#"policy_version = 1
[[grant]]
key = "sql.raw"
value = true
scope = { include = ["\"a.b\""] }
"#,
    )
    .unwrap();
    match &doc.rules[0].scope {
        Scope::Of { include, .. } => {
            // Single segment: the schema glob is the literal `a.b`; table glob is `*`.
            let obj = zero_migrate_policy::normalize_pg_identifier("\"a.b\"").unwrap();
            assert!(include[0].matches(&obj));
            // The unrelated schema `a` object is NOT matched.
            assert!(!include[0].matches(&zero_migrate_policy::ObjectName::schema(b"a".to_vec())));
        }
        other => panic!("expected Of, got {other:?}"),
    }
}

// ══════════════════════════════════════════════════════════════════════════════
// extends (II.7, H-1): draft-forbid + trusted catalog resolution + cycle detection
// ══════════════════════════════════════════════════════════════════════════════

/// A trivial in-memory trusted catalog for the extends tests.
struct MapCatalog(std::collections::BTreeMap<String, String>);

impl zero_migrate_policy::ProfileCatalog for MapCatalog {
    fn get_source(&self, name: &str) -> Option<String> {
        self.0.get(name).cloned()
    }
}

fn catalog(entries: &[(&str, &str)]) -> MapCatalog {
    MapCatalog(
        entries
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
    )
}

#[test]
fn extends_in_untrusted_draft_is_hard_error() {
    // A creator draft (NonRootLayer) carrying `extends` is rejected outright (H-1).
    let err = PolicyDoc::parse_toml(
        "policy_version = 1\nextends = \"base\"\n",
        &registry(),
        LoadContext::NonRootLayer,
    )
    .unwrap_err();
    assert_eq!(err, LoadError::ExtendsForbiddenInDraft);
}

#[test]
fn extends_trusted_resolves_against_catalog() {
    // A trusted catalog entry inherits `base`'s rules under its own.
    let cat = catalog(&[(
        "base",
        r#"policy_version = 1
[[grant]]
key = "sql.raw"
value = true
scope = { include = ["app_*"] }
"#,
    )]);
    let doc = PolicyDoc::parse_toml_with_catalog(
        r#"policy_version = 1
extends = "base"
[[grant]]
key = "runtime.lock_timeout_ms"
value = 5000
scope = { include = ["app_*"] }
"#,
        &registry(),
        LoadContext::TrustedCatalogEntry,
        &cat,
    )
    .unwrap();
    // Both the base's raw_sql grant and this doc's timeout grant are present.
    let keys: Vec<&str> = doc
        .rules
        .iter()
        .filter_map(|r| match &r.kind {
            RuleKind::Grant { key, .. } => Some(key.as_str()),
            _ => None,
        })
        .collect();
    assert!(
        keys.contains(&"sql.raw"),
        "base rule not inherited: {keys:?}"
    );
    assert!(
        keys.contains(&"runtime.lock_timeout_ms"),
        "own rule missing: {keys:?}"
    );
}

#[test]
fn extends_unknown_base_is_error() {
    let cat = catalog(&[]);
    let err = PolicyDoc::parse_toml_with_catalog(
        "policy_version = 1\nextends = \"nope\"\n",
        &registry(),
        LoadContext::TrustedCatalogEntry,
        &cat,
    )
    .unwrap_err();
    assert_eq!(
        err,
        LoadError::ExtendsUnknownBase {
            base: "nope".into()
        }
    );
}

#[test]
fn extends_cycle_is_detected() {
    // a extends b, b extends a → cycle.
    let cat = catalog(&[
        ("a", "policy_version = 1\nextends = \"b\"\n"),
        ("b", "policy_version = 1\nextends = \"a\"\n"),
    ]);
    let err = PolicyDoc::parse_toml_with_catalog(
        "policy_version = 1\nextends = \"a\"\n",
        &registry(),
        LoadContext::TrustedCatalogEntry,
        &cat,
    )
    .unwrap_err();
    assert!(
        matches!(err, LoadError::ExtendsCycle { .. }),
        "expected cycle, got {err:?}"
    );
}

#[test]
fn extends_without_catalog_is_unknown_base() {
    // A trusted doc with `extends` but no catalog (plain parse_toml) fails closed.
    let err = PolicyDoc::parse_toml(
        "policy_version = 1\nextends = \"base\"\n",
        &registry(),
        LoadContext::RootCeiling,
    )
    .unwrap_err();
    assert!(
        matches!(err, LoadError::ExtendsUnknownBase { .. }),
        "expected unknown base, got {err:?}"
    );
}
