//! The generated test overlay is held to the real config contract.
//!
//! `deploy/ops/zeroship.test.toml` is written by
//! `tests/provision_test_backends.sh` and names the shared PostgreSQL used by
//! suites that consume this overlay. It replaced eight environment variables that
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
const GENERATED_SHAPE: &str = r#"[control]
database_url = "postgres://postgres:zeroship@127.0.0.1:5440/zeroship"
"#;

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
    assert!(parsed.worker.kv_config.is_none());
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
/// carry the PostgreSQL DSN its consumers resolve from it.
///
/// AN ABSENT OVERLAY FAILS. It used to announce a skip here - the argument
/// being that `cargo test -p zeroship-core` runs constantly on checkouts that
/// never provisioned a backend, and that failing there would say "the config is
/// wrong" when the truth is "there is no config yet". That distinction is real
/// and is now made in the message rather than in the exit status: the refusal
/// says the overlay has not been generated and names the one command that
/// generates it. The status stays red, because a skip here is indistinguishable
/// from a pass, and this is the only test that rules on the shape every suite
/// resolves its backends from.
#[test]
fn the_generated_overlay_parses_and_names_postgres() {
    let path = overlay_path();
    assert!(
        path.exists(),
        "The generated test overlay does not exist, and this test rules on it.\n\
         \n\
         \x20 wanted: {path}\n\
         \n\
         NOTHING IS WRONG WITH YOUR CONFIGURATION - there is not one yet. This\n\
         file is generated, never hand-written. Generate it:\n\
         \x20 {PROVISION_COMMAND}\n\
         \n\
         Both forms of that script write the overlay; `--check` adopts servers\n\
         that are already running instead of starting its own.\n\
         \n\
         There is no environment variable that makes this a skip. Suites that use this\n\
         overlay require it to name their shared PostgreSQL server.",
        path = path.display()
    );

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
}
