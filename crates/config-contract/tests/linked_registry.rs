//! The proof that the macro carries something a hand-written test cannot.
//!
//! Everything here is measured against declarations that live in the LIBRARY
//! crate (`zeroship_config_contract::fixtures`) while the assertions run in this
//! separate integration-test crate. A hand-written clap struct could reproduce
//! the three spellings; it could not produce a registry that a different crate
//! enumerates without naming the declaration.

use clap::CommandFactory;
use zeroship_config_contract::contract::validate_contract;
use zeroship_config_contract::fixtures::{
    FixtureControlConfig, FixtureControlConfigConsumer, FixtureControlConfigSources,
    FixtureWorkerConfig, FixtureWorkerConfigSources,
};
use zeroship_core::config::{
    CanonicalName, ConfigSpec, EnvKey, GeneratedConfig, ReadSite, Sensitivity, SourceKind,
    CONFIG_READ_SITES,
};

const FIXTURE_BINARIES: [&str; 2] = ["zeroship-fixture-control", "zeroship-fixture-worker"];

fn fixture_specs() -> Vec<ConfigSpec> {
    let mut specs = FixtureControlConfig::SPECS.to_vec();
    specs.extend_from_slice(FixtureWorkerConfig::SPECS);
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
    // Set semantics, matching contract.rs. Two readers of one identity in one
    // binary is legitimate: this test crate adds a second reader of
    // `control.port` as the positive control below.
    rows.dedup();
    rows
}

#[test]
fn read_sites_declared_in_another_crate_are_linked_and_enumerable() {
    // This is the load-bearing claim for keeping config-macros: the registry is
    // populated by expansions in the library crate, and enumerated here without
    // this crate naming a single read site. If linkme were dropping the entries
    // the vector would be empty and this test would fail rather than pass
    // vacuously.
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

const CONTROL_PORT: EnvKey<String, FixtureControlConfigConsumer> =
    EnvKey::from_static(CanonicalName::from_static("control.port"));

#[test]
fn the_matching_consumer_token_compiles_and_reads() {
    // Positive control for tests/ui/wrong_consumer.rs. Same macro, same const
    // key, same shape; the ONLY difference is that the consumer marker matches
    // the key's. That partner is what separates "the type check works" from
    // "the fixture failed for some unrelated reason".
    // Does not cover: the value itself. This asserts the call type-checks and
    // reports absence, not any particular environment content.
    let read = zeroship_core::read_config_env!(CONTROL_PORT, FixtureControlConfigConsumer);
    assert!(read.is_ok());
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
    assert_eq!(per_component.toml_path(), "control.database_url");

    let platform_global = fixture_specs()
        .into_iter()
        .find(|spec| spec.canonical().as_str() == "control_key")
        .expect("platform-global secret");
    assert_eq!(platform_global.sensitivity(), Sensitivity::Secret);
    assert_eq!(
        platform_global.toml_path(),
        "control_key",
        "a platform-global secret is a top-level key, not secrets.control_key"
    );
    assert!(
        !platform_global.toml_path().starts_with("secrets."),
        "the reserved `secrets` segment was deliberately deleted from the design"
    );
}

#[test]
fn the_resolved_struct_redacts_its_secret_without_a_hand_written_debug() {
    // Direct replacement for the hand-maintained redaction lists in
    // crates/control/src/main.rs and crates/worker/src/main.rs: the resolved
    // struct uses a DERIVED Debug, and the wrapper supplies the redaction.
    // Does not cover: an explicit `expose_secret()` that the caller then prints;
    // the wrapper removes accidental formatting, not deliberate disclosure.
    let resolved = FixtureControlConfig::resolve_config(
        FixtureControlConfigSources {
            port: Some(8443),
            database_url: None,
        },
        Some(
            &toml::from_str::<toml::Value>(
                "[control]\ndatabase_url = \"postgres://u:supersecretpw@db/zeroship\"\n",
            )
            .expect("fixture overlay"),
        ),
    )
    .expect("fixture resolves");

    let rendered = format!("{resolved:?}");
    assert!(rendered.contains("8443"), "operational values stay visible");
    assert!(
        !rendered.contains("supersecretpw"),
        "derived Debug leaked secret material: {rendered}"
    );
    assert!(rendered.contains("<redacted>"));
}
