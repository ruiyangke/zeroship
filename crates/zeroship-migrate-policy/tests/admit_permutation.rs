//! Admission must not depend on the order a draft's rules are written.
//!
//! The escalation check used to partition the draft's granted scope by charter rule
//! scopes and compare values at ONE witness per region. A draft carrying two values
//! inside a single charter region had only the witness's value compared, and the witness
//! is derived from the first include pattern - so writing the compliant rule first
//! admitted a draft that writing the escalating rule first refused.
//!
//! These use a custom registry because the builtin one has no object-scoped ordered
//! grant. The charter-side twin of this defect needs no custom registry and is pinned at
//! the engine entry point in `crates/zero-migrate/tests/charter_root_bound.rs`.

use zero_migrate_policy::{
    admit, ComposeError, Enforcement, KnobDef, KnobKey, KnobKind, KnobValue, LoadContext,
    ObjectModel, Polarity, PolicyDoc, PolicyRegistry, RootCharter,
};

const KEY: &str = "safety.posture";

fn registry() -> PolicyRegistry {
    PolicyRegistry::empty()
        .with([KnobDef {
            key: KnobKey::parse(KEY).expect("well-formed key"),
            kind: KnobKind::OrderedEnum {
                variants: vec!["forbid".into(), "warn".into(), "allow".into()],
            },
            polarity: Polarity::Grant,
            default: KnobValue::Str("forbid".into()),
            enforcement: Enforcement::Enforced,
            object_model: ObjectModel::PerSchema,
            requires_db_privilege: false,
            inherit: true,
            docs: String::new(),
        }])
        .expect("single-knob registry is valid")
}

/// One charter rule, so the old partition drew ONE region over both draft rules.
fn charter(reg: &PolicyRegistry) -> RootCharter {
    RootCharter::parse_toml(
        &format!(
            r#"policy_version = 1
[[grant]]
key = "{KEY}"
value = "warn"
scope = {{ include = ["app_*"] }}
"#
        ),
        reg,
    )
    .expect("charter loads")
}

fn draft(reg: &PolicyRegistry, rules: &[(&str, &str)]) -> PolicyDoc {
    let mut src = String::from("policy_version = 1\n");
    for (schema, value) in rules {
        src.push_str(&format!(
            "[[grant]]\nkey = \"{KEY}\"\nvalue = \"{value}\"\nscope = {{ include = [\"{schema}\"] }}\n"
        ));
    }
    PolicyDoc::parse_toml(&src, reg, LoadContext::NonRootLayer).expect("draft loads")
}

#[test]
fn an_escalating_rule_is_refused_whichever_order_it_is_written_in() {
    let reg = registry();
    let root = charter(&reg);
    // `allow` exceeds the charter's `warn`; `warn` is exactly at it. Both orders must
    // reach the same verdict, and that verdict must be refusal.
    for order in [
        [("app_one", "warn"), ("app_two", "allow")],
        [("app_two", "allow"), ("app_one", "warn")],
    ] {
        let err = admit(&root, &draft(&reg, &order), &reg).unwrap_err();
        assert!(
            matches!(err, ComposeError::GrantExceedsCharter { .. }),
            "order {order:?} must refuse the escalation, got {err:?}"
        );
    }
}

#[test]
fn two_compliant_rules_still_compose_in_either_order() {
    // The control. Both refusals above must come from the escalating VALUE, not from
    // the mere presence of two draft rules in one charter region.
    let reg = registry();
    let root = charter(&reg);
    for order in [
        [("app_one", "warn"), ("app_two", "forbid")],
        [("app_two", "forbid"), ("app_one", "warn")],
    ] {
        admit(&root, &draft(&reg, &order), &reg)
            .unwrap_or_else(|e| panic!("order {order:?} is within the charter, got {e:?}"));
    }
}

#[test]
fn three_overlapping_draft_rules_are_all_bounded() {
    // Overlapping scopes are where a partition-and-sample check is weakest: an
    // intersection of two rules is not an atom, so a region can hold points covered by a
    // third rule and points not. Proving each rule against the charter over its own
    // scope needs no such reasoning, and this pins that it does not regress into it.
    let reg = registry();
    let root = charter(&reg);
    let escalating = draft(
        &reg,
        &[
            ("app_a*", "warn"),
            ("app_ab*", "warn"),
            ("app_abc", "allow"),
        ],
    );
    let err = admit(&root, &escalating, &reg).unwrap_err();
    assert!(
        matches!(err, ComposeError::GrantExceedsCharter { .. }),
        "the innermost overlapping rule escalates and must be caught, got {err:?}"
    );

    let compliant = draft(
        &reg,
        &[("app_a*", "warn"), ("app_ab*", "warn"), ("app_abc", "warn")],
    );
    admit(&root, &compliant, &reg).expect("the same three scopes at the charter's value compose");
}
