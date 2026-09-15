//! Startup settings and credentials exercised through the generated resolver.

use super::*;
use zeroship_core::config::{GeneratedConfig, SourceKind, SERVICE_CREDENTIAL_SENTINEL};

struct SecretFile(tempfile::NamedTempFile);

impl SecretFile {
    fn new(contents: &str) -> Self {
        use std::io::Write;
        let mut file = tempfile::NamedTempFile::new().expect("private secret file");
        file.write_all(contents.as_bytes())
            .expect("write fixture secret");
        Self(file)
    }

    fn arg(&self) -> &str {
        self.0.path().to_str().expect("utf8 fixture path")
    }
}

fn resolve(args: &[&str]) -> WorkerSettings {
    let mut argv = vec!["zeroship-worker"];
    argv.extend_from_slice(args);
    WorkerSettings::resolve_config(
        WorkerSettingsSources::try_parse_from(argv).expect("worker sources parse"),
        None,
    )
    .expect("worker settings resolve")
}

#[test]
fn worker_cli_has_no_security_relaxation_flag() {
    let error = WorkerSettingsSources::try_parse_from(["zeroship-worker", "--dev-insecure"])
        .expect_err("deleted --dev-insecure flag must be rejected");
    assert_eq!(error.kind(), clap::error::ErrorKind::UnknownArgument);
}

#[test]
fn worker_env_has_no_security_relaxation_binding() {
    assert!(WorkerSettingsSources::try_parse_from(["zeroship-worker"]).is_ok());
}

#[test]
fn worker_threads_default_resolves_to_positive_count() {
    assert!(zeroship_worker::config::default_worker_threads() > 0);

    let flagged = WorkerSettingsSources::try_parse_from(["zeroship-worker", "--threads", "3"])
        .expect("--threads parses");
    assert_eq!(flagged.threads, Some(3));
}

#[test]
fn worker_rejects_sqlite_database_url() {
    assert!(worker_rejects_db_url("sqlite:.zeroship/dev.sqlite"));
    assert!(worker_rejects_db_url("sqlite://./data/app.sqlite"));
    assert!(worker_rejects_db_url("file:./local.db"));
    assert!(worker_rejects_db_url(":memory:"));
    assert!(worker_rejects_db_url("/var/lib/zeroship/dev.sqlite"));
}

#[test]
fn worker_rejects_invalid_and_unsupported_database_selectors() {
    for selector in [
        "sqlite:",
        "sqlite::memory:",
        "file:db?mode=memory",
        "mysql://localhost/db",
    ] {
        assert!(worker_rejects_db_url(selector), "{selector}");
    }
}

#[test]
fn worker_accepts_postgres_database_url() {
    assert!(!worker_rejects_db_url("postgres://localhost/dev"));
    assert!(!worker_rejects_db_url("postgresql://u:p@host:5432/db"));
    assert!(!worker_rejects_db_url(""));
}

#[test]
fn worker_thread_flag_uses_unambiguous_name() {
    let sources = WorkerSettingsSources::try_parse_from([
        "zeroship-worker",
        "--threads",
        "3",
        "--max-isolates",
        "200",
        "--poll-interval",
        "5",
        "--shutdown-timeout",
        "30",
    ])
    .expect("--threads should parse");
    assert_eq!(sources.threads, Some(3));

    let old_flag = WorkerSettingsSources::try_parse_from(["zeroship-worker", "--workers", "3"]);
    assert!(
        old_flag.is_err(),
        "--workers must not parse for worker threads"
    );
    let renamed =
        WorkerSettingsSources::try_parse_from(["zeroship-worker", "--worker-threads", "3"]);
    assert!(
        renamed.is_err(),
        "the pre-conversion flag must be gone, not aliased"
    );
}

#[test]
fn worker_numeric_fields_reject_bad_input() {
    let err = WorkerSettingsSources::try_parse_from(["zeroship-worker", "--max-isolates", "abc"])
        .expect_err("bad max-isolates should be a clap error");
    assert_eq!(err.kind(), clap::error::ErrorKind::ValueValidation);
}

#[test]
fn a_missing_or_placeholder_control_key_still_fails_the_boot_guard() {
    let good = SecretFile::new("control-key-material");

    let settings = resolve(&[]);
    assert!(!settings.control_key.is_configured());
    let absent = audit_credentials(&worker_credentials(&settings));
    assert_eq!(absent.weak().len(), 1, "{absent:?}");
    assert_eq!(absent.weak()[0].subsystem, "control-version-poll");
    assert!(absent.weak()[0].unset);
    assert!(absent.weak()[0].message.contains("required"), "{absent:?}");
    assert!(
        absent.weak()[0].message.contains("ZEROSHIP_CONTROL_KEY"),
        "{absent:?}"
    );

    let sentinel = SecretFile::new(SERVICE_CREDENTIAL_SENTINEL);
    let settings = resolve(&["--control-key-file", sentinel.arg()]);
    assert_eq!(settings.control_key.source(), Some(SourceKind::CliFile));
    let placeholder = audit_credentials(&worker_credentials(&settings));
    assert_eq!(placeholder.weak().len(), 1, "{placeholder:?}");
    assert_eq!(placeholder.weak()[0].message, absent.weak()[0].message);

    let ok = audit_credentials(&worker_credentials(&resolve(&[
        "--control-key-file",
        good.arg(),
    ])));
    assert!(ok.is_ok(), "a real control key passes every guard: {ok:?}");
    assert_eq!(ok.checked(), 1, "the worker audits exactly one credential");
}

#[test]
fn a_check_config_run_neither_reads_nor_judges_a_secret_file() {
    let directory = tempfile::tempdir().expect("private missing-secret directory");
    let missing = directory.path().join("absent-control-key");
    assert!(!missing.exists(), "the fixture path must really be absent");
    let missing = missing.to_str().expect("utf8 path").to_owned();

    let dry = resolve(&["--check-config", "--control-key-file", &missing]);
    assert!(dry.control_key.is_configured());
    assert_eq!(
        dry.control_key.expose_secret(),
        None,
        "--check-config must not have read the file"
    );
    let posture = audit_credentials(&worker_credentials(&dry));
    assert!(
        posture
            .weak()
            .iter()
            .all(|weak| weak.subsystem != "control-version-poll"),
        "an unread secret must not be judged: {posture:?}"
    );
    assert_eq!(posture.unread(), 1, "and it must be COUNTED as unread");

    let boot =
        WorkerSettingsSources::try_parse_from(["zeroship-worker", "--control-key-file", &missing])
            .expect("worker sources parse");
    assert!(WorkerSettings::resolve_config(boot, None).is_err());
}

#[test]
fn a_resolved_secret_never_formats_its_material_or_its_length() {
    const SENTINEL: &str = "k9x2m7q4v8b3n6z1p5t0w4y7r2j8h5d3";
    let file = SecretFile::new(SENTINEL);
    let settings = resolve(&["--kv-config-file", file.arg()]);

    assert!(settings.kv_config.is_configured());
    assert_eq!(
        settings.kv_config.expose_str(),
        SENTINEL,
        "the boot path must still get the real material"
    );

    let field = format!("{:?}", settings.kv_config);
    for length in 4..=SENTINEL.len() {
        assert!(
            !field.contains(&SENTINEL[..length]),
            "Debug leaked a {length}-char prefix of the secret: {field}"
        );
    }
    assert!(
        !field.contains(&SENTINEL.len().to_string()),
        "Debug leaked the secret's length: {field}"
    );
    assert_eq!(field, "Secret(configured from CliFile)");

    let rendered = format!("{settings:?}");
    assert!(!rendered.contains(SENTINEL), "{rendered}");
    assert!(
        rendered.contains("Secret(configured from CliFile)"),
        "{rendered}"
    );
}

#[test]
fn no_worker_secret_has_a_value_flag() {
    use clap::CommandFactory;

    let command = WorkerSettingsSources::command();
    let longs = command
        .get_arguments()
        .filter_map(|arg| arg.get_long().map(str::to_owned))
        .collect::<Vec<_>>();
    for secret in ["control-key", "database-url", "kv-config"] {
        assert!(
            longs.iter().any(|long| long == &format!("{secret}-file")),
            "{secret} must offer a -file path flag: {longs:?}"
        );
        assert!(
            !longs.iter().any(|long| long == secret),
            "{secret} must NOT offer a value flag: {longs:?}"
        );
    }
    assert!(!longs.iter().any(|long| long == "db"), "{longs:?}");
    assert!(longs.iter().any(|long| long == "storage-url"), "{longs:?}");
}
