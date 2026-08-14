//! Unit tests for the CLI's `zeroship.jsonc` reader.
//!
//! WHAT THESE DO NOT CATCH, stated so the coverage is not overread:
//!
//! - They do not compare this reader against the TypeScript one. Byte-equality
//!   of the two resolved dumps is `tests/project_config_gate.sh` check 3, and it
//!   needs both binaries; nothing here can see a divergent TS default.
//! - They do not exercise `locate` against a real `ZEROSHIP_CONFIG`, because
//!   setting process environment in a threaded test runner races every other
//!   test in the binary. The env arm is covered by the shell gate.
//! - `write_app` is tested on a temp file; it does not prove the splice is safe
//!   under a concurrent editor.

use super::*;

fn cfg(text: &str) -> ProjectConfig {
    ProjectConfig::parse(PathBuf::from("zeroship.jsonc"), text.to_string())
        .expect("fixture must parse")
}

const FULL: &str = r#"{
  // A comment, which is the whole reason the format is JSONC.
  "$schema": "https://zeroship.ai/schema/project-v1.json",
  "name": "demo-app",
  "app": "11111111-1111-4111-8111-111111111111",
  "control": "https://control.zeroship.ai",
  "runtime_date": "2026-08-14",
  "build": { "mode": "full", "dist": "dist", "output": "dist/app.zship" },
  "migrations": { "dir": "migrations", "out": "generated/zeroship" },
  "secrets": ["STRIPE_SECRET_KEY"],
  "environments": {
    "staging": {
      "app": "22222222-2222-4222-8222-222222222222",
      "control": "https://control.staging.zeroship.ai",
      "protected": true,
      "migrations": { "out": "generated/staging" }
    }
  }
}"#;

/// The root resolution reads the root, and `environments` never leaks into it.
#[test]
fn root_resolution_drops_the_environments_block() {
    let r = cfg(FULL).resolve(None).expect("resolve root");
    assert_eq!(r.str("app"), Some("11111111-1111-4111-8111-111111111111"));
    assert_eq!(r.str("control"), Some("https://control.zeroship.ai"));
    assert_eq!(r.str("migrations.out"), Some("generated/zeroship"));
    assert!(r.get("environments").is_none(), "environments must not survive resolution");
    assert!(r.get("$schema").is_none(), "$schema is an editor hint, not config");
    assert_eq!(r.origin, Source::File);
}

/// An environment's `app` and `control` REPLACE the root's, and its partial
/// `migrations` merges member by member rather than wiping the block.
#[test]
fn environment_overlay_replaces_target_and_merges_the_rest() {
    let r = cfg(FULL).resolve(Some("staging")).expect("resolve staging");
    assert_eq!(r.str("app"), Some("22222222-2222-4222-8222-222222222222"));
    assert_eq!(r.str("control"), Some("https://control.staging.zeroship.ai"));
    assert_eq!(r.str("migrations.out"), Some("generated/staging"));
    // NOT stated by the environment, so inherited.
    assert_eq!(r.str("migrations.dir"), Some("migrations"));
    assert!(r.is_protected());
    assert_eq!(r.origin, Source::FileEnvironment("staging".into()));
}

/// An environment that names only a `control` is REFUSED. Inheriting the root
/// `app` there is precisely the silent cross-targeting the rule exists to
/// prevent: staging's control plane, production's app id.
#[test]
fn an_environment_without_both_app_and_control_is_refused() {
    let text = FULL.replace(
        "\"app\": \"22222222-2222-4222-8222-222222222222\",\n      ",
        "",
    );
    let err = ProjectConfig::parse(PathBuf::from("zeroship.jsonc"), text)
        .expect_err("a half-specified environment must not parse");
    assert!(err.contains("environments.staging.app"), "{err}");
    assert!(err.contains("NON-INHERITABLE"), "{err}");
}

/// Naming an environment that does not exist lists the ones that do, rather
/// than falling back to the root - a fallback here would deploy to production
/// on a typo'd `--env=stagng`.
#[test]
fn an_unknown_environment_is_an_error_naming_the_known_ones() {
    let err = cfg(FULL)
        .resolve(Some("stagng"))
        .expect_err("unknown environment must fail");
    assert!(err.contains("--env=stagng"), "{err}");
    assert!(err.contains("staging"), "{err}");
}

/// The no-Rust-defaults constraint (proposal 7.3), asserted directly: a file
/// that omits a cross-tool key produces an error NAMING the key, never a
/// value. This is the test that would fail if somebody added a fallback.
#[test]
fn an_absent_cross_tool_key_errors_naming_it_rather_than_defaulting() {
    let text = FULL.replace("\"out\": \"generated/zeroship\"", "\"out\": \"x\"");
    let text = text.replace("\"dir\": \"migrations\", ", "");
    let c = ProjectConfig::parse(PathBuf::from("zeroship.jsonc"), text)
        .expect_err("migrations.dir is required by the schema");
    assert!(c.contains("migrations.dir"), "{c}");

    // And the same shape one level down: a key the schema does not require but
    // the CLI reads.
    let r = cfg(FULL).resolve(None).unwrap();
    let err = r.require("build.serverEntry").expect_err("absent key");
    assert!(err.contains("build.serverEntry"), "{err}");
    assert!(err.contains("no default"), "{err}");
}

/// EVERY field the schema gives a default AND the CLI reads must produce an
/// error here, not a value. This is 7.3 checked against the generated tables
/// rather than against a list somebody typed: add a `default` to a CLI-read
/// property in the schema and this test starts failing until the Rust side is
/// still fallback-free.
///
/// It is the intersection that matters. `build.mode` has a default and is NOT
/// CLI-read, so a TypeScript-only default there is correct and this test says
/// nothing about it.
#[test]
fn no_cli_read_field_has_a_rust_side_default() {
    let stripped: Vec<&str> = generated::SCHEMA_DEFAULTED_FIELDS
        .iter()
        .copied()
        .filter(|f| generated::CLI_READ_FIELDS.contains(f))
        .collect();
    assert!(
        !stripped.is_empty(),
        "the intersection is empty, so this test proves nothing - check the generated tables"
    );

    // A file that omits each of them in turn. `require` must refuse.
    let bare = r#"{"name":"a","control":"u","runtime_date":"2026-08-14","build":{"mode":"full","dist":"d","output":"o"},"migrations":{"dir":"m","out":"g"}}"#;
    let r = cfg(bare).resolve(None).unwrap();
    for field in &stripped {
        // Present in this fixture, so it resolves...
        assert!(r.require(field).is_ok(), "{field} should resolve here");
    }
    // ...and absent, it errors rather than producing the schema default.
    let empty = ProjectConfig {
        path: PathBuf::from("zeroship.jsonc"),
        text: String::new(),
        root: Map::new(),
    };
    let r = empty.resolve(None).unwrap();
    for field in &stripped {
        let err = r.require(field).expect_err("must not default");
        assert!(err.contains(field), "{err}");
    }
}

/// A `$schema` naming a different contract is refused rather than validated
/// against v1 rules and reported as the creator's typos.
#[test]
fn a_foreign_schema_id_is_refused() {
    let text = FULL.replace(generated::SCHEMA_ID, "https://zeroship.ai/schema/project-v9.json");
    let err = ProjectConfig::parse(PathBuf::from("zeroship.jsonc"), text)
        .expect_err("a v9 document must not be read as v1");
    assert!(err.contains("project-v9"), "{err}");
}

/// A `password` key anywhere is a parse error that names where the value
/// belongs. `additionalProperties: false` already rejects it; this asserts the
/// message is the useful one.
#[test]
fn a_secret_shaped_key_is_refused_with_the_place_it_belongs() {
    for text in [
        r#"{"name":"a","control":"u","runtime_date":"2026-08-14","password":"hunter2","build":{"mode":"full","dist":"d","output":"o"},"migrations":{"dir":"m","out":"g"}}"#,
        r#"{"name":"a","control":"u","runtime_date":"2026-08-14","build":{"mode":"full","dist":"d","output":"o","token":"x"},"migrations":{"dir":"m","out":"g"}}"#,
    ] {
        let err = ProjectConfig::parse(PathBuf::from("zeroship.jsonc"), text.to_string())
            .expect_err("a secret-shaped key must not parse");
        assert!(err.contains("zeroship secret set"), "{err}");
        assert!(err.contains(".env"), "{err}");
    }
}

/// `secrets` holds NAMES. A lowercase entry (which is what a value looks like)
/// is refused by the same pattern the CLI already enforces on `secret set`.
#[test]
fn secrets_entries_must_be_names_not_values() {
    let text = FULL.replace("\"STRIPE_SECRET_KEY\"", "\"sk_test_deadbeef\"");
    let err = ProjectConfig::parse(PathBuf::from("zeroship.jsonc"), text)
        .expect_err("a value-shaped secrets entry must not parse");
    assert!(err.contains("holds NAMES"), "{err}");
}

#[test]
fn a_bad_runtime_date_is_refused_and_a_good_one_is_not() {
    let bad = FULL.replace("\"2026-08-14\"", "\"August 2026\"");
    let err = ProjectConfig::parse(PathBuf::from("zeroship.jsonc"), bad)
        .expect_err("free-text date must not parse");
    assert!(err.contains("runtime_date"), "{err}");
    assert!(cfg(FULL).resolve(None).is_ok());
}

#[test]
fn an_unknown_top_level_key_names_the_known_ones() {
    let text = FULL.replace("\"name\": \"demo-app\",", "\"name\": \"demo-app\", \"rpcEndpoint\": \"/_rpc\",");
    let err = ProjectConfig::parse(PathBuf::from("zeroship.jsonc"), text)
        .expect_err("an unknown key must not parse");
    assert!(err.contains("rpcEndpoint"), "{err}");
    assert!(err.contains("migrations"), "the message must list the known keys: {err}");
}

/// The canonical dump is sorted and compact, so the TypeScript side can be
/// compared to it byte for byte.
#[test]
fn canonical_json_is_sorted_and_compact() {
    let r = cfg(FULL).resolve(None).unwrap();
    let dump = r.canonical_json();
    assert!(dump.starts_with("{\"app\":"), "{dump}");
    assert!(!dump.contains('\n'), "{dump}");
    let keys: Vec<&str> = ["app", "build", "control", "migrations", "name", "runtime_date", "secrets"]
        .into_iter()
        .collect();
    let mut cursor = 0usize;
    for k in keys {
        let needle = format!("\"{k}\":");
        let at = dump[cursor..].find(&needle).unwrap_or_else(|| panic!("{k} missing from {dump}"));
        cursor += at + needle.len();
    }
}

// ---------------------------------------------------------------------------
// Precedence
// ---------------------------------------------------------------------------

fn s(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

/// flag > env var > file. Each arm is asserted with the LOWER-precedence
/// sources still present, so a broken arm shows as the wrong source rather than
/// as an absent value.
#[test]
fn precedence_is_flag_then_env_then_file() {
    let c = cfg(FULL);
    let r = c.resolve(None).unwrap();

    let from_flag = resolve_value(
        &s(&["zeroship", "deploy", "--control=http://flag"]),
        "--control",
        Some("ZEROSHIP_CONTROL_URL"),
        Some("http://env".into()),
        Some(&r),
        "control",
        Some("http://localhost:9090"),
    )
    .unwrap();
    assert_eq!(from_flag.value, "http://flag");
    assert_eq!(from_flag.source, Source::Flag("--control"));

    let from_env = resolve_value(
        &s(&["zeroship", "deploy"]),
        "--control",
        Some("ZEROSHIP_CONTROL_URL"),
        Some("http://env".into()),
        Some(&r),
        "control",
        Some("http://localhost:9090"),
    )
    .unwrap();
    assert_eq!(from_env.value, "http://env");
    assert_eq!(from_env.source, Source::EnvVar("ZEROSHIP_CONTROL_URL"));

    let from_file = resolve_value(
        &s(&["zeroship", "deploy"]),
        "--control",
        Some("ZEROSHIP_CONTROL_URL"),
        None,
        Some(&r),
        "control",
        Some("http://localhost:9090"),
    )
    .unwrap();
    assert_eq!(from_file.value, "https://control.zeroship.ai");
    assert_eq!(from_file.source, Source::File);
}

/// THE ONE-VARIABLE PAIR for 7.3. Same call, same key, same fallback; the only
/// difference is whether a config file was found.
///
/// - No file  -> the compiled fallback, exactly today's behaviour.
/// - A file that does not say -> an error naming the key.
///
/// A guess is only wrong when it can contradict something written down.
#[test]
fn the_compiled_fallback_survives_only_when_there_is_no_file() {
    let no_file = resolve_value(
        &s(&["zeroship", "deploy"]),
        "--control",
        None,
        None,
        None,
        "control",
        Some("http://localhost:9090"),
    )
    .unwrap();
    assert_eq!(no_file.value, "http://localhost:9090");
    assert_eq!(no_file.source, Source::Fallback);

    // A file that does not say. `control` cannot be absent - the schema
    // requires it, which is the stronger guarantee - so the pair is shown on
    // `app`, the one CLI-read key the schema deliberately leaves optional
    // (a fresh project has no app id yet).
    let silent_file = cfg(&FULL.replace(
        "\"app\": \"11111111-1111-4111-8111-111111111111\",",
        "",
    ));
    let r = silent_file.resolve(None).expect("parses without app");
    let err = resolve_value(
        &s(&["zeroship", "deploy"]),
        "--app",
        None,
        None,
        Some(&r),
        "app",
        // Even WITH a fallback offered, the file's silence wins as an error.
        Some("some-fallback-app"),
    )
    .expect_err("a file that does not say must not be guessed for");
    assert!(err.contains("`app`"), "{err}");
    assert!(err.contains("no default"), "{err}");
    assert!(!err.contains("some-fallback-app"), "the fallback must not leak: {err}");
}

/// With no file and no flag, `--app` is an ERROR naming the flag, not the
/// `.expect()` panic it was.
#[test]
fn a_missing_app_with_no_file_is_an_error_not_a_panic() {
    let err = resolve_value(
        &s(&["zeroship", "deploy", "app.zship"]),
        "--app",
        None,
        None,
        None,
        "app",
        None,
    )
    .expect_err("no app anywhere");
    assert!(err.contains("--app=<value> is required"), "{err}");
    assert!(err.contains(CONFIG_FILENAME), "{err}");
}

// ---------------------------------------------------------------------------
// Writeback
// ---------------------------------------------------------------------------

/// The splice rewrites the `app` value and NOTHING else - comments, key order
/// and whitespace outside the value span are byte-identical.
#[test]
fn write_app_splices_the_value_and_leaves_every_other_byte() {
    let dir = std::env::temp_dir().join(format!("zs-pc-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("splice.jsonc");
    std::fs::write(&path, FULL).unwrap();

    let c = ProjectConfig::load(&path).unwrap();
    assert_eq!(c.write_app("33333333-3333-4333-8333-333333333333").unwrap(), WriteOutcome::Spliced);

    let after = std::fs::read_to_string(&path).unwrap();
    let expected = FULL.replace(
        "\"11111111-1111-4111-8111-111111111111\"",
        "\"33333333-3333-4333-8333-333333333333\"",
    );
    assert_eq!(after, expected, "the splice touched something other than the app value");
    assert!(after.contains("// A comment"), "the comment must survive");
    std::fs::remove_file(&path).ok();
}

/// With no `app` member the writeback REFUSES rather than inventing an
/// insertion point. Inserting into arbitrary JSONC is where round-trip
/// libraries get ugly; the creator gets the line to paste instead.
#[test]
fn write_app_refuses_to_insert_a_missing_member() {
    let text = FULL.replace("\"app\": \"11111111-1111-4111-8111-111111111111\",\n  ", "");
    let dir = std::env::temp_dir().join(format!("zs-pc-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("noapp.jsonc");
    std::fs::write(&path, &text).unwrap();

    let c = ProjectConfig::load(&path).unwrap();
    assert_eq!(c.write_app("44444444-4444-4444-8444-444444444444").unwrap(), WriteOutcome::PrintInstead);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), text, "the file must be untouched");
    std::fs::remove_file(&path).ok();
}
