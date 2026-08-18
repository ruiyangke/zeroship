//! The generated test overlay is held to the real config contract.
//!
//! `deploy/ops/zeroship.test.toml` is written by
//! `tests/provision_test_backends.sh` and names the PostgreSQL and Redis every
//! suite in this workspace dials. It replaced eight environment variables that
//! all meant "the test database", and the whole reason a file is better than
//! eight names is that a file can be VALIDATED. A test topology in a document
//! nothing parses is the same sprawl with fewer places to look.
//!
//! So this is the validation. `FileConfig` carries `deny_unknown_fields`, which
//! means a misspelled key is a parse ERROR rather than a value that silently
//! configures nothing - the failure mode that let `gateway.broker_secret` sit
//! in the schema until 2026-08-13 while the gateway declared
//! `broker_secret_file`, so setting the documented key did nothing AND the real
//! key was rejected, both halves invisible from either side.

use std::path::PathBuf;

use zeroship_core::config::file::FileConfig;
use zeroship_core::config::test_overlay::{overlay_path, PROVISION_COMMAND};

/// The exact shape `tests/provision_test_backends.sh` writes.
const GENERATED_SHAPE: &str = "\
[control]
database_url = \"postgres://postgres:zeroship@127.0.0.1:5440/zeroship\"

[worker]
kv_url = \"redis://127.0.0.1:6390\"
";

fn write_temp(name: &str, contents: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "zeroship-test-overlay-{name}-{}.toml",
        std::process::id()
    ));
    std::fs::write(path.as_path(), contents).expect("write temp overlay");
    path
}

/// A typo in the overlay is REJECTED, and the correct spelling is not.
///
/// Both arms are here because only the pair discriminates: a parser that
/// rejected every document would satisfy the negative arm alone, and a parser
/// with `deny_unknown_fields` removed would satisfy the positive arm alone.
///
/// This does not need the file to exist, so it is the arm that runs on a
/// checkout that has never provisioned anything.
#[test]
fn a_misspelled_key_in_the_generated_shape_is_rejected() {
    let good = write_temp("good", GENERATED_SHAPE);
    let parsed = FileConfig::load(Some(good.as_path())).expect("the generated shape parses");
    assert_eq!(
        parsed.control.database_url.as_deref(),
        Some("postgres://postgres:zeroship@127.0.0.1:5440/zeroship")
    );
    assert_eq!(
        parsed.worker.kv_url.as_deref(),
        Some("redis://127.0.0.1:6390")
    );
    let _ = std::fs::remove_file(good.as_path());

    // One character, in one key, in the same document.
    let typo_text = GENERATED_SHAPE.replace("database_url", "databse_url");
    let typo = write_temp("typo", &typo_text);
    let error = FileConfig::load(Some(typo.as_path()))
        .expect_err("an unknown key must not be accepted in silence");
    let message = error.to_string();
    assert!(
        message.contains("databse_url"),
        "the rejection must name the offending key; got: {message}"
    );
    let _ = std::fs::remove_file(typo.as_path());
}

/// When the overlay HAS been generated, it must satisfy the same contract and
/// carry the two values every suite resolves from it.
///
/// Skipped rather than failed when absent, and that is the one skip in this
/// file: `cargo test -p zeroship-core` is run constantly on checkouts that have
/// no docker and never provisioned a backend, and failing it there would say
/// "the config is wrong" when the truth is "there is no config yet". The suites
/// themselves cannot skip this way - `zeroship_core::config::test_overlay::load`
/// panics with the provisioning command, because a suite that runs without a
/// database is the thing this workspace stopped tolerating.
#[test]
fn the_generated_overlay_parses_and_names_both_backends() {
    let path = overlay_path();
    if !path.exists() {
        eprintln!(
            "ZEROSHIP-TEST-SKIPPED: no {} yet; run {PROVISION_COMMAND}",
            path.display()
        );
        return;
    }

    let parsed = FileConfig::load(Some(path.as_path())).unwrap_or_else(|error| {
        panic!(
            "{} does not satisfy the config contract: {error}\n\
             It is generated, so fix {PROVISION_COMMAND} rather than the file.",
            path.display()
        )
    });

    let dsn = parsed
        .control
        .database_url
        .expect("the generated overlay must carry [control] database_url");
    assert!(
        dsn.starts_with("postgres://") || dsn.starts_with("postgresql://"),
        "[control] database_url is not a PostgreSQL DSN: {dsn}"
    );

    let kv = parsed
        .worker
        .kv_url
        .expect("the generated overlay must carry [worker] kv_url");
    assert!(
        kv.starts_with("redis://") || kv.starts_with("rediss://"),
        "[worker] kv_url is not a Redis URL: {kv}"
    );
}
