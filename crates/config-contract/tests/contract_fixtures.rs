use zeroship_config_contract::contract::{ContractError, validate_contract};
use zeroship_core::config::{CanonicalName, ConfigSpec, Consumer, ReadSite, SourceKind};

const CONTROL: Consumer = Consumer::new("zeroship-control", "control");
const WORKER: Consumer = Consumer::new("zeroship-worker", "worker");
const CONTROL_ONLY: &[Consumer] = &[CONTROL];
const WORKER_ONLY: &[Consumer] = &[WORKER];

fn operational(name: &'static str, consumers: &'static [Consumer]) -> ConfigSpec {
    ConfigSpec::operational(
        CanonicalName::from_static(name),
        consumers,
        "fixture",
        "fixture",
        "String",
        None,
    )
}

fn secret(name: &'static str, consumers: &'static [Consumer]) -> ConfigSpec {
    ConfigSpec::secret(
        CanonicalName::from_static(name),
        consumers,
        "fixture",
        "fixture",
        "String",
    )
}

fn sites(specs: &[ConfigSpec]) -> Vec<ReadSite> {
    specs
        .iter()
        .flat_map(|spec| {
            spec.consumers().iter().flat_map(move |consumer| {
                spec.sources().iter().map(move |source| {
                    ReadSite::new(
                        spec.canonical(),
                        *consumer,
                        *source,
                        "fixture.rs",
                        1,
                        1,
                    )
                })
            })
        })
        .collect()
}

#[test]
fn exact_collapsed_names_fail_the_global_env_registry() {
    // Mutation: these two different canonical identities collapse dots and
    // underscores into the exact same environment spelling.
    // Does not cover: whether a deployment supplies either variable; this is
    // registry uniqueness, while Compose/runtime gates are later steps.
    let specs = [
        operational("a.b_c", CONTROL_ONLY),
        operational("a_b.c", WORKER_ONLY),
    ];
    let errors = validate_contract(&specs, &sites(&specs)).expect_err("env collision must fail");

    assert!(errors.iter().any(|error| matches!(
        error,
        ContractError::EnvCollision { projection, .. }
            if projection == "ZEROSHIP_A_B_C"
    )));
}

#[test]
fn exact_collapsed_names_fail_the_same_binary_flag_registry() {
    // Mutation: the same required pair is consumed by one binary, so both also
    // project to --a-b-c after localization/kebab conversion.
    // Does not cover: the same flag in different binaries, which is allowed.
    let specs = [
        operational("a.b_c", CONTROL_ONLY),
        operational("a_b.c", CONTROL_ONLY),
    ];
    let errors = validate_contract(&specs, &sites(&specs)).expect_err("flag collision must fail");

    assert!(errors.iter().any(|error| matches!(
        error,
        ContractError::FlagCollision { projection, consumer, .. }
            if projection == "a-b-c" && consumer == "zeroship-control"
    )));
}

#[test]
fn secret_suffix_can_collide_with_an_operational_flag() {
    // Mutation: operational control.token_file and secret control.token both
    // project to --token-file in the same binary.
    // Does not cover: clap's own duplicate-long diagnostics; this detects the
    // collision before assembling one Command.
    let specs = [
        operational("control.token_file", CONTROL_ONLY),
        secret("control.token", CONTROL_ONLY),
    ];
    let errors = validate_contract(&specs, &sites(&specs)).expect_err("flag collision must fail");

    assert!(errors.iter().any(|error| matches!(
        error,
        ContractError::FlagCollision { projection, .. } if projection == "token-file"
    )));
}

#[test]
fn a_removed_reader_fails_set_but_unread_equality() {
    // Mutation: remove the environment ReadSite while leaving its declared
    // source in ConfigSpec.
    // Does not cover: linker retention of macro-generated entries; the compiled
    // macro fixture separately enumerates CONFIG_READ_SITES.
    let specs = [operational("control.port", CONTROL_ONLY)];
    let mut reads = sites(&specs);
    reads.retain(|site| site.source() != SourceKind::Env);
    let errors = validate_contract(&specs, &reads).expect_err("missing reader must fail");

    assert!(errors.iter().any(|error| matches!(
        error,
        ContractError::DeclaredButUnread { canonical, kind: SourceKind::Env, .. }
            if canonical == "control.port"
    )));
}

#[test]
fn a_wrong_consumer_is_both_unread_and_undeclared() {
    // Mutation: the read site keeps the identity/source but claims worker as
    // consumer instead of control.
    // Does not cover: the compile-time EnvKey consumer mismatch. That is
    // tests/ui/wrong_consumer.rs, with its positive control in
    // linked_registry.rs.
    let specs = [operational("control.port", CONTROL_ONLY)];
    let mut reads = sites(&specs);
    let env = reads
        .iter_mut()
        .find(|site| site.source() == SourceKind::Env)
        .expect("env site");
    *env = ReadSite::new(
        CanonicalName::from_static("control.port"),
        WORKER,
        SourceKind::Env,
        "fixture.rs",
        2,
        1,
    );
    let errors = validate_contract(&specs, &reads).expect_err("wrong consumer must fail");

    assert!(errors.iter().any(|error| matches!(
        error,
        ContractError::DeclaredButUnread { consumer, .. }
            if consumer == "zeroship-control"
    )));
    assert!(errors.iter().any(|error| matches!(
        error,
        ContractError::UndeclaredReadSite { consumer, .. }
            if consumer == "zeroship-worker"
    )));
}

#[test]
fn two_consumers_of_one_identity_may_differ_only_in_their_default() {
    // THE 2026-08-12 AMENDMENT to Section 4.1. The original coalescing rule
    // demanded that consumers agree on the DEFAULT as well, which would have
    // rejected the five `observability.log_filter` declarations already in the
    // tree: each defaults to a tracing directive naming its own crate, so no
    // single value is correct for all of them.
    //
    // This is the positive control for the two failures below: the specs are
    // identical apart from the one property the rule must NOT compare.
    let specs = [
        ConfigSpec::operational(
            CanonicalName::from_static("observability.log_filter"),
            CONTROL_ONLY,
            "log_filter",
            "log_filter",
            "String",
            Some("info,zeroship_control=debug"),
        ),
        ConfigSpec::operational(
            CanonicalName::from_static("observability.log_filter"),
            WORKER_ONLY,
            "log_filter",
            "log_filter",
            "String",
            Some("info,zeroship_worker=debug"),
        ),
    ];
    validate_contract(&specs, &sites(&specs))
        .expect("a per-binary default is a legitimate configuration, not a violation");
}

#[test]
fn two_consumers_of_one_identity_may_not_differ_in_class_or_type() {
    // The other half of the amended rule, one variable at a time against the
    // positive control above. Both mutations keep one canonical name, so the
    // operator still sees exactly one ZEROSHIP_OBSERVABILITY_LOG_FILTER; what
    // differs is what that one variable MEANS to each process.
    //
    // Does not cover: the same divergence written through the attribute. That
    // cannot be expressed - `#[config(shared = ...)]` reads the class and type
    // from one table - and the refusal is asserted in config-macros.
    let retyped = [
        ConfigSpec::operational(
            CanonicalName::from_static("observability.log_filter"),
            CONTROL_ONLY,
            "log_filter",
            "log_filter",
            "String",
            None,
        ),
        ConfigSpec::operational(
            CanonicalName::from_static("observability.log_filter"),
            WORKER_ONLY,
            "log_filter",
            "log_filter",
            "PathBuf",
            None,
        ),
    ];
    let errors =
        validate_contract(&retyped, &sites(&retyped)).expect_err("a retyped identity must fail");
    assert!(
        errors.iter().any(|error| matches!(
            error,
            ContractError::SharedIdentityDisagreement { property: "type", canonical, .. }
                if canonical == "observability.log_filter"
        )),
        "{errors:?}"
    );

    let reclassed = [
        operational("observability.log_filter", CONTROL_ONLY),
        secret("observability.log_filter", WORKER_ONLY),
    ];
    let errors = validate_contract(&reclassed, &sites(&reclassed))
        .expect_err("a reclassed identity must fail");
    // A class change also moves the supply set, so both properties are reported;
    // asserting on `class` alone would pass if only the supply set had drifted.
    for property in ["class", "supply set"] {
        assert!(
            errors.iter().any(|error| matches!(
                error,
                ContractError::SharedIdentityDisagreement { property: reported, .. }
                    if *reported == property
            )),
            "no {property} disagreement reported: {errors:?}"
        );
    }
}

#[test]
fn empty_contract_extraction_fails_in_both_directions() {
    // Mutation: neither declaration nor read-site extraction finds anything.
    // Does not cover: Cargo binary extraction, which has a distinct metadata
    // anti-vacuity fixture.
    let errors = validate_contract(&[], &[]).expect_err("empty contract must fail");

    assert!(errors.contains(&ContractError::EmptySpecs));
    assert!(errors.contains(&ContractError::EmptyReadSites));
}
