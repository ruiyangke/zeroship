//! The proof that the macro carries something a hand-written test cannot.
//!
//! Everything here is measured against declarations that live in the LIBRARY
//! crate (`zeroship_config_contract::fixtures`) while the assertions run in this
//! separate integration-test crate. This file names no read site of its own; the
//! one hand-written accessor call lives in tests/typed_accessor.rs, a different
//! test binary with a different registry.
//!
//! THE MACRO REGISTERS BY TWO INDEPENDENT PATHS, and a test that does not
//! distinguish them can be satisfied by either alone:
//!
//!   path 1  the direct `#[distributed_slice]` statics emitted per field
//!           (config-macros `emit`, the `read_sites` block). Supplies
//!           `Cli`/`CliFile`/`Toml`, plus `Env` for an operational field.
//!   path 2  the `read_config_env!` expansion inside the generated secret
//!           resolver, which registers through the core macro. Supplies `Env`
//!           for a secret field, and NOTHING else -- it has no other kind to
//!           emit.
//!
//! So `Cli`, `CliFile` and `Toml` are exclusive to path 1. Any claim about the
//! attribute registering must be phrased over those kinds, not over "the
//! registry is non-empty", which path 2 satisfies on its own.

use clap::{CommandFactory, Parser};
use zeroship_config_contract::contract::validate_contract;
use zeroship_config_contract::fixtures::{
    FixtureControlConfig, FixtureControlConfigSources, FixtureControls, FixtureControlsSources,
    FixtureWorkerConfig, FixtureWorkerConfigSources,
};
use zeroship_core::config::{
    CheckFormat, ConfigSpec, GeneratedConfig, ReadSite, Sensitivity, SourceKind,
    CONFIG_READ_SITES,
};

const FIXTURE_BINARIES: [&str; 3] = [
    "zeroship-fixture-control",
    "zeroship-fixture-gate",
    "zeroship-fixture-worker",
];

fn fixture_specs() -> Vec<ConfigSpec> {
    let mut specs = FixtureControlConfig::SPECS.to_vec();
    specs.extend_from_slice(FixtureWorkerConfig::SPECS);
    specs.extend_from_slice(FixtureControls::SPECS);
    specs
}

fn fixture_sites() -> Vec<ReadSite> {
    CONFIG_READ_SITES
        .iter()
        .copied()
        .filter(|site| FIXTURE_BINARIES.contains(&site.consumer().target()))
        .collect()
}

fn tuples(sites: &[ReadSite]) -> Vec<(String, String, SourceKind)> {
    let mut rows = sites
        .iter()
        .map(|site| {
            (
                site.canonical().as_str().to_owned(),
                site.consumer().target().to_owned(),
                site.source(),
            )
        })
        .collect::<Vec<_>>();
    rows.sort();
    // Deliberately NOT deduplicated. Nothing in this binary registers a site by
    // hand any more, so each declared source must appear exactly once; a second
    // copy would mean the macro registered the same tuple twice.
    rows
}

#[test]
fn read_sites_declared_in_another_crate_are_linked_and_enumerable() {
    // What this measures: linkme retention and CROSS-CRATE enumeration. The
    // entries are emitted in the library crate and read here, which is the
    // property a five-service registry needs.
    //
    // What this does NOT measure, despite an earlier version of this comment
    // claiming it did: that the ATTRIBUTE registered them. Path 2 (see the
    // module header) supplies an Env site for each secret field on its own, so
    // deleting every path-1 static leaves this test green. Measured, not
    // assumed: mutation M20/M30 does exactly that and this test still passes.
    // The path-1 claim is
    // `the_attribute_registers_the_sources_no_env_read_can_supply` below.
    //
    // Does not cover: retention under `--release`, LTO, `-C linker-plugin-lto`,
    // or a cdylib/staticlib target. Only the dev-profile rlib path is measured.
    let sites = fixture_sites();
    assert!(
        !sites.is_empty(),
        "the linked read-site registry is empty, so every set comparison below \
         would be vacuous"
    );

    let mut found_binaries = sites
        .iter()
        .map(|site| site.consumer().target())
        .collect::<Vec<_>>();
    found_binaries.sort_unstable();
    found_binaries.dedup();
    assert_eq!(found_binaries, FIXTURE_BINARIES);

    let declaring = sites
        .iter()
        .filter(|site| site.location().0.ends_with("fixtures.rs"))
        .count();
    assert!(
        declaring > 0,
        "no read site was attributed to the declaring file"
    );
    for site in &sites {
        assert!(site.location().1 > 0, "read site has no source line");
    }
}

#[test]
fn the_attribute_registers_the_sources_no_env_read_can_supply() {
    // The path-1 claim, stated over the kinds only path 1 can emit. A
    // `read_config_env!` expansion registers `SourceKind::Env` and nothing
    // else, so Cli, CliFile and Toml sites exist if and only if the attribute's
    // own `#[distributed_slice]` statics were emitted and linked. Deleting
    // those attributes turns this red while leaving every Env site intact.
    // Does not cover: whether the registered tuple is CORRECT. That is the
    // set equality in the next test; this one only proves path 1 exists.
    let sites = fixture_sites();
    let kinds = sites
        .iter()
        .map(|site| site.source())
        .collect::<Vec<SourceKind>>();

    for exclusive in [SourceKind::Cli, SourceKind::CliFile, SourceKind::Toml] {
        assert!(
            kinds.contains(&exclusive),
            "no {exclusive:?} read site is linked; path 2 cannot emit this kind, \
             so the attribute's own registrations are missing. Linked kinds: \
             {kinds:?}"
        );
    }

    // The counterpart: path 2 is load-bearing too. A secret field gets its Env
    // site only from the resolver's read_config_env! expansion.
    let secret_env = sites.iter().any(|site| {
        site.source() == SourceKind::Env && site.canonical().as_str() == "control.database_url"
    });
    assert!(
        secret_env,
        "the secret resolver registered no Env read site, so path 2 is missing"
    );
}

#[test]
fn every_declared_source_has_exactly_one_linked_reader() {
    // Mutation target: deleting a field from fixtures.rs removes both its spec
    // and its read sites, so this stays green; deleting only the generated
    // ReadSite emission (or only the spec) makes validate_contract fail.
    // Does not cover: sources a later step disables by cfg. Step 1 declares no
    // cfg-gated field, so the equality here is unconditional.
    let specs = fixture_specs();
    let sites = fixture_sites();

    let expected = specs
        .iter()
        .flat_map(|spec| {
            spec.consumers().iter().flat_map(move |consumer| {
                spec.sources().iter().map(move |source| {
                    (
                        spec.canonical().as_str().to_owned(),
                        consumer.target().to_owned(),
                        *source,
                    )
                })
            })
        })
        .collect::<Vec<_>>();
    let mut expected = expected;
    expected.sort();

    assert_eq!(tuples(&sites), expected);
    validate_contract(&specs, &sites).expect("linked fixtures satisfy the contract");
}

#[test]
fn compiled_clap_metadata_equals_the_declared_projections() {
    // The expansion tests in config-macros assert on token text. This asserts on
    // the Command clap actually built, so a derive-composition change that
    // silently dropped `env` or renamed a long would fail here.
    // Does not cover: precedence between the CLI, environment and overlay at
    // run time; nothing in Step 1 resolves a live process.
    let control = FixtureControlConfigSources::command();
    let port = control
        .get_arguments()
        .find(|arg| arg.get_id() == "port")
        .expect("port argument");
    assert_eq!(port.get_long(), Some("port"));
    assert_eq!(
        port.get_env().and_then(std::ffi::OsStr::to_str),
        Some("ZEROSHIP_CONTROL_PORT")
    );

    let database_url = control
        .get_arguments()
        .find(|arg| arg.get_id() == "database_url")
        .expect("database_url argument");
    assert_eq!(database_url.get_long(), Some("database-url-file"));
    assert_eq!(
        database_url.get_env(),
        None,
        "a secret must never reach clap as an environment-backed value argument"
    );

    let worker = FixtureWorkerConfigSources::command();
    let isolates = worker
        .get_arguments()
        .find(|arg| arg.get_id() == "max_pinned_isolates_per_app")
        .expect("isolate argument");
    assert_eq!(isolates.get_long(), Some("max-pinned-isolates-per-app"));
    assert_eq!(
        isolates.get_env().and_then(std::ffi::OsStr::to_str),
        Some("ZEROSHIP_WORKER_MAX_PINNED_ISOLATES_PER_APP")
    );

    let control_key = worker
        .get_arguments()
        .find(|arg| arg.get_id() == "control_key")
        .expect("control_key argument");
    assert_eq!(
        control_key.get_long(),
        Some("control-key-file"),
        "a platform-global identity has no leading scope segment to strip"
    );
    assert_eq!(control_key.get_env(), None);
}

#[test]
fn the_two_transform_implementations_agree() {
    // The projections exist TWICE: config-macros computes them to write the
    // clap attributes, and zeroship-core computes them for ConfigSpec. The
    // macro cannot call core (core depends on the macro), so the duplication is
    // structural and only a comparison can stop the two drifting apart. The
    // literals in the test above pin what the answer should be; this pins that
    // both halves give the SAME answer.
    // Does not cover: a change applied identically to both implementations.
    // Nothing here can catch a transform that is wrong in the same way twice.
    for (command, specs) in [
        (FixtureControlConfigSources::command(), FixtureControlConfig::SPECS),
        (FixtureWorkerConfigSources::command(), FixtureWorkerConfig::SPECS),
        (FixtureControlsSources::command(), FixtureControls::SPECS),
    ] {
        for spec in specs {
            let consumer = spec.consumers()[0];
            let arg = command
                .get_arguments()
                .find(|arg| arg.get_id() == spec.arg_id())
                .unwrap_or_else(|| panic!("no clap argument for {}", spec.arg_id()));

            assert_eq!(
                arg.get_long().map(str::to_owned),
                spec.flag_name(consumer),
                "clap long and ConfigSpec flag projection disagree for {}",
                spec.canonical().as_str()
            );

            let clap_env = arg
                .get_env()
                .and_then(std::ffi::OsStr::to_str)
                .map(str::to_owned);
            match spec.sensitivity() {
                Sensitivity::Operational => assert_eq!(
                    clap_env,
                    spec.env_name(),
                    "clap env and ConfigSpec env projection disagree for {}",
                    spec.canonical().as_str()
                ),
                Sensitivity::Secret => assert_eq!(
                    clap_env,
                    None,
                    "a secret carrier must expose no clap env, while its \
                     ConfigSpec still names {:?}",
                    spec.env_name()
                ),
            }
        }
    }
}

#[test]
fn a_secret_projects_to_its_component_table_not_a_secrets_section() {
    // The 2026-08-12 amendment: there is no reserved `secrets` segment, and a
    // secret's TOML path is its canonical path inside its component's table.
    // Does not cover: whether a deployment file actually uses that path; the
    // ops-TOML gate is a later step.
    let per_component = fixture_specs()
        .into_iter()
        .find(|spec| spec.canonical().as_str() == "control.database_url")
        .expect("component-scoped secret");
    assert_eq!(per_component.sensitivity(), Sensitivity::Secret);
    assert_eq!(per_component.toml_path(), Some("control.database_url"));

    let platform_global = fixture_specs()
        .into_iter()
        .find(|spec| spec.canonical().as_str() == "control_key")
        .expect("platform-global secret");
    assert_eq!(platform_global.sensitivity(), Sensitivity::Secret);
    assert_eq!(
        platform_global.toml_path(),
        Some("control_key"),
        "a platform-global secret is a top-level key, not secrets.control_key"
    );
    assert!(
        !platform_global
            .toml_path()
            .is_some_and(|path| path.starts_with("secrets.")),
        "the reserved `secrets` segment was deliberately deleted from the design"
    );
}

#[test]
fn a_control_has_no_overlay_slot_at_all() {
    // The complement of the assertion above: a class whose TOML projection is
    // absent, not merely unused. A later ops-TOML gate walks toml_path(), so
    // `Some("config")` would make `config = "..."` a valid overlay leaf and
    // reintroduce reading the overlay selector from the overlay.
    // Does not cover: what the gate then does with None. No gate is active yet.
    for canonical in ["config", "no_config", "check_config", "check_config_format"] {
        let spec = FixtureControls::SPECS
            .iter()
            .find(|spec| spec.canonical().as_str() == canonical)
            .unwrap_or_else(|| panic!("no spec for {canonical}"));
        assert_eq!(spec.toml_path(), None, "{canonical} has an overlay slot");
        assert!(
            !spec.sources().contains(&SourceKind::Toml),
            "{canonical} declares a TOML source"
        );
    }

    let command = FixtureControls::SPECS
        .iter()
        .find(|spec| spec.canonical().as_str() == "check_config")
        .expect("command control");
    assert_eq!(command.env_name(), None);
    assert_eq!(command.sources(), [SourceKind::Cli]);

    let bootstrap = FixtureControls::SPECS
        .iter()
        .find(|spec| spec.canonical().as_str() == "config")
        .expect("bootstrap control");
    assert_eq!(bootstrap.env_name().as_deref(), Some("ZEROSHIP_CONFIG"));
    assert_eq!(bootstrap.sources(), [SourceKind::Cli, SourceKind::Env]);
}

#[test]
fn a_control_resolves_with_no_overlay_and_keeps_clap_precedence() {
    // Controls are resolved BEFORE any overlay exists, so this passes `None`
    // where the operational fixtures pass an overlay. It also pins the two
    // carrier shapes a control needs: a bare `--no-config` flag and a
    // `--config` whose absence is a value rather than an error.
    // Does not cover: flag-over-env precedence, which clap owns.
    let sources = FixtureControlsSources::try_parse_from([
        "zeroship-fixture-gate",
        "--no-config",
        "--check-config-format",
        "json",
    ])
    .expect("controls parse");
    let resolved = FixtureControls::resolve_config(sources, None).expect("controls resolve");

    assert_eq!(resolved.config.get(), &None);
    assert!(*resolved.no_config.get());
    assert!(!*resolved.check_config.get());
    assert_eq!(resolved.check_config_format.get(), &CheckFormat::Json);
    assert!(!*resolved.allow_unsigned_advance.get());

    let defaulted = FixtureControls::resolve_config(
        FixtureControlsSources::try_parse_from(["zeroship-fixture-gate"]).expect("bare parse"),
        None,
    )
    .expect("controls resolve");
    assert_eq!(
        defaulted.check_config_format.get(),
        &CheckFormat::Text,
        "an absent valued control falls back to its compiled default"
    );
}

#[test]
fn a_flag_control_accepts_the_workspace_boolean_grammar() {
    // Regression. `ArgAction::SetTrue` parses an ENVIRONMENT value with clap's
    // strict bool parser, so `ZEROSHIP_NO_CONFIG=1` was rejected outright:
    //   error: invalid value '1' for '--no-config' [possible values: true, false]
    // That is the one spelling an operator reaches for first, and every other
    // boolean input in this workspace accepts it via `parse_bool_flag`.
    //
    // Asserted through the CLI rather than the environment because the env tier
    // is process-global and would race sibling tests; the value parser is the
    // same object on both tiers, which is the whole point of the fix.
    for (spelling, expected) in [
        ("1", true),
        ("0", false),
        ("true", true),
        ("FALSE", false),
        ("yes", true),
        ("no", false),
    ] {
        let parsed = FixtureControlsSources::try_parse_from([
            "zeroship-fixture-gate",
            &format!("--no-config={spelling}"),
        ])
        .unwrap_or_else(|error| panic!("--no-config={spelling} rejected: {error}"));
        assert_eq!(parsed.no_config, expected, "--no-config={spelling}");
    }

    assert!(
        FixtureControlsSources::try_parse_from(["zeroship-fixture-gate", "--no-config"])
            .expect("bare flag")
            .no_config,
        "bare presence must still mean true"
    );
    assert!(
        FixtureControlsSources::try_parse_from(["zeroship-fixture-gate", "--no-config=maybe"])
            .is_err(),
        "an unrecognised spelling must still be an error, not a silent true"
    );

    // Does not cover: that clap applies this parser to the environment tier.
    // Only a process-level test can show that, and tests/config_check_e2e.sh
    // exercises the real binaries.
}

#[test]
fn the_resolved_struct_redacts_its_secret_without_a_hand_written_debug() {
    // Direct replacement for the hand-maintained redaction lists in
    // crates/zeroship-control/src/main.rs and crates/zeroship-worker/src/main.rs: the resolved
    // struct uses a DERIVED Debug, and the wrapper supplies the redaction.
    // Does not cover: an explicit `expose_secret()` that the caller then prints;
    // the wrapper removes accidental formatting, not deliberate disclosure.
    const SENTINEL: &str = "postgres://u:supersecretpw@db/zeroship";
    let overlay = toml::from_str::<toml::Value>(&format!(
        "[control]\ndatabase_url = {SENTINEL:?}\n"
    ))
    .expect("fixture overlay");

    let resolved = FixtureControlConfig::resolve_config(
        FixtureControlConfigSources {
            port: Some(8443),
            database_url: None,
            check_config: false,
        },
        Some(&overlay),
    )
    .expect("fixture resolves");

    let rendered = format!("{resolved:?}");
    assert!(rendered.contains("8443"), "operational values stay visible");
    assert!(
        !rendered.contains("supersecretpw"),
        "derived Debug leaked secret material: {rendered}"
    );
    // Not just the whole value: a prefix, and the LENGTH, are leaks too. The
    // wrapper prints the supplying TIER and nothing measured from the material.
    for length in 8..=SENTINEL.len() {
        assert!(
            !rendered.contains(&SENTINEL[..length]),
            "derived Debug leaked a {length}-char prefix: {rendered}"
        );
    }
    assert!(!rendered.contains(&SENTINEL.len().to_string()));
    assert!(rendered.contains("Secret(configured from Toml)"));

    // The one-variable partner: the SAME declaration resolved by a dry run
    // establishes the same source and holds no material at all. Without this,
    // "the secret is redacted" would also be true of a resolver that silently
    // failed to read anything.
    let checked = FixtureControlConfig::resolve_config(
        FixtureControlConfigSources {
            port: Some(8443),
            database_url: None,
            check_config: true,
        },
        Some(&overlay),
    )
    .expect("fixture resolves under a dry run");
    assert!(checked.database_url.is_configured());
    assert_eq!(
        checked.database_url.expose_secret().map(String::as_str),
        Some(SENTINEL),
        "an in-memory literal keeps its material so strength guards still run"
    );
}

#[test]
fn a_dry_run_resolves_a_secret_file_reference_without_opening_it() {
    // The property `--check-config` owes: SOURCE POLICY and FORMAT, no I/O. The
    // referenced path deliberately does not exist, so a resolver that opened it
    // could not return Ok - and the boot-mode partner below proves the path is
    // genuinely unreadable rather than the reference being ignored.
    // Does not cover: a real binary routing --check-config to this mode. That is
    // the macro's wiring, asserted end to end by tests/config_check_e2e.sh.
    let overlay = toml::from_str::<toml::Value>(
        "[control]\ndatabase_url = \"urn:zeroship:file:/no/such/zeroship/contract/dsn\"\n",
    )
    .expect("fixture overlay");

    let checked = FixtureControlConfig::resolve_config(
        FixtureControlConfigSources {
            port: Some(8443),
            database_url: None,
            check_config: true,
        },
        Some(&overlay),
    )
    .expect("a dry run must not open the file");
    assert!(checked.database_url.is_configured());
    assert_eq!(checked.database_url.expose_secret(), None);

    assert!(
        FixtureControlConfig::resolve_config(
            FixtureControlConfigSources {
                port: Some(8443),
                database_url: None,
                check_config: false,
            },
            Some(&overlay),
        )
        .is_err(),
        "the same reference on the boot path must fail to read"
    );
}

#[test]
fn a_secret_source_outside_the_supply_set_is_refused_in_both_modes() {
    // The deleted schemes, through the GENERATED resolver rather than through
    // the parser directly: an env-to-env alias, a Vault URN and an AWS ARN are
    // no longer expressible, so they fail source policy in a dry run instead of
    // passing it and failing at boot.
    // Does not cover: the same values appearing in a deployment file nothing
    // reads. That is a tracked-tree search, and Step 6 owns it.
    for deleted in [
        "urn:zeroship:env:SOME_OTHER_VAR",
        "urn:zeroship:vault:secret/data/app",
        "arn:aws:secretsmanager:us-east-1:123:secret:x",
    ] {
        let overlay = toml::from_str::<toml::Value>(&format!(
            "[control]\ndatabase_url = {deleted:?}\n"
        ))
        .expect("fixture overlay");
        for check_config in [true, false] {
            let error = FixtureControlConfig::resolve_config(
                FixtureControlConfigSources {
                    port: Some(8443),
                    database_url: None,
                    check_config,
                },
                Some(&overlay),
            )
            .expect_err("a deleted scheme must be refused");
            let message = error.to_string();
            assert!(message.contains("control.database_url"));
            assert!(
                !message.contains(deleted),
                "the diagnostic echoed the rejected input: {message}"
            );
        }
    }

    // The one-variable partner: the same field, the same table, the one scheme
    // that survives - and it resolves.
    let overlay = toml::from_str::<toml::Value>(
        "[control]\ndatabase_url = \"urn:zeroship:file:/no/such/path\"\n",
    )
    .expect("fixture overlay");
    assert!(FixtureControlConfig::resolve_config(
        FixtureControlConfigSources {
            port: Some(8443),
            database_url: None,
            check_config: true,
        },
        Some(&overlay),
    )
    .is_ok());
}
