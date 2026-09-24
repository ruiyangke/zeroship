//! Unit tests for the CLI's `zeroship.jsonc` reader.
//!
//! WHAT THESE DO NOT CATCH, stated so the coverage is not overread:
//!
//! - They do not compare this reader against the TypeScript one in process.
//!   Both readers consume generated contracts from the shared schema, and each
//!   owning suite exercises the committed cross-tool fixture.
//! - They do not exercise `locate` against a real `ZEROSHIP_CONFIG`, because
//!   setting process environment in a threaded test runner races every other
//!   test in the binary. Process-level CLI tests own that environment boundary.
//! - `write_app_id` is tested on temp files, including a real disk edit after
//!   load.

use super::*;

fn app_id(raw: &str) -> AppId {
    AppId::parse(raw).expect("test app id must be canonical")
}

fn cfg(text: &str) -> ProjectConfig {
    ProjectConfig::parse(PathBuf::from("zeroship.jsonc"), text.to_string())
        .expect("fixture must parse")
}

#[test]
fn committed_cross_tool_fixture_resolves_in_rust() {
    let text = include_str!("../../../../tests/fixtures/project-config/zeroship.jsonc");
    let config = ProjectConfig::parse(PathBuf::from("zeroship.jsonc"), text.to_string())
        .expect("committed cross-tool fixture must parse");
    let root = config.resolve(None).expect("resolve fixture root");
    assert_eq!(root.str("name"), Some("config-fixture"));
    assert_eq!(root.database_labels(), vec!["main", "analytics"]);
    assert_eq!(root.app_labels(), vec!["storefront", "admin"]);
    assert_eq!(
        root.database_id("main").expect("dereference main"),
        "dbs_03evr3oqx1200yyd6zj2cebfw"
    );
    assert_eq!(root.app_databases("storefront"), vec!["main", "analytics"]);
    assert_eq!(root.app_primary("storefront"), Some("main"));

    let staging = config
        .resolve(Some("staging"))
        .expect("resolve fixture environment");
    assert_eq!(
        staging.str("control"),
        Some("https://control.staging.zeroship.ai")
    );
    // The environment overrides the ID under each label and NOTHING else: the
    // label, the build-time paths and the app wiring are the same artifact
    // across environments.
    assert_eq!(
        staging.database_id("main").expect("dereference staging main"),
        "dbs_03evr3oqx1200uzh8k6gycpgg"
    );
    assert_eq!(
        staging.str("databases.main.out"),
        Some("generated/zeroship/main")
    );
    assert_eq!(staging.app_databases("storefront"), vec!["main", "analytics"]);
    assert!(staging.is_protected());
}

const FULL: &str = r#"{
  // A comment, which is the whole reason the format is JSONC.
  "$schema": "https://zeroship.ai/schema/project-v1.json",
  "name": "demo-app",
  "control": "https://control.zeroship.ai",
  "runtime_date": "2026-08-14",
  "build": { "mode": "full", "dist": "dist", "output": "dist/app.zship" },
  "databases": {
    "main": { "id": "dbs_03evr3oqx1200yyd6zj2cebfw", "migrations": "migrations", "out": "generated/zeroship" }
  },
  "apps": {
    "storefront": { "app": "app_034klb07lrb9jgma6imvmx000", "databases": ["main"], "primary": "main" }
  },
  "secrets": ["STRIPE_SECRET_KEY"],
  "environments": {
    "staging": {
      "control": "https://control.staging.zeroship.ai",
      "apps": { "storefront": { "app": "app_034klb07lrb9jgma6imvmx001" } },
      "databases": { "main": { "id": "dbs_03evr3oqx1200uzh8k6gycpgg" } },
      "protected": true
    }
  }
}"#;

/// The smallest file that satisfies the schema: no app, no database, no
/// optional key. Several checks need a valid document whose shape they are not
/// about.
const BARE: &str = r#"{"name":"a","control":"u","runtime_date":"2026-08-14",
  "build":{"mode":"full","dist":"d","output":"o"},"databases":{},"apps":{}}"#;

/// The root resolution reads the root, and `environments` never leaks into it.
#[test]
fn root_resolution_drops_the_environments_block() {
    let r = cfg(FULL).resolve(None).expect("resolve root");
    assert_eq!(r.str("apps.storefront.app"), Some("app_034klb07lrb9jgma6imvmx000"));
    assert_eq!(r.str("control"), Some("https://control.zeroship.ai"));
    assert_eq!(r.str("databases.main.out"), Some("generated/zeroship"));
    assert!(r.get("environments").is_none(), "environments must not survive resolution");
    assert!(r.get("$schema").is_none(), "$schema is an editor hint, not config");
    assert_eq!(r.origin, Source::File);
}

/// An environment's `control` REPLACES the root's, and its label maps merge
/// ENTRY BY ENTRY and MEMBER BY MEMBER rather than wiping the entry: it
/// overrides the id under a label and never the paths beside it.
#[test]
fn environment_overlay_replaces_target_and_merges_the_rest() {
    let r = cfg(FULL).resolve(Some("staging")).expect("resolve staging");
    assert_eq!(r.str("apps.storefront.app"), Some("app_034klb07lrb9jgma6imvmx001"));
    assert_eq!(r.str("control"), Some("https://control.staging.zeroship.ai"));
    assert_eq!(r.database_id("main").unwrap(), "dbs_03evr3oqx1200uzh8k6gycpgg");
    // NOT stated by the environment, so carried through from the root entry.
    assert_eq!(r.str("databases.main.migrations"), Some("migrations"));
    assert_eq!(r.str("databases.main.out"), Some("generated/zeroship"));
    assert_eq!(r.app_databases("storefront"), vec!["main"]);
    assert_eq!(r.app_primary("storefront"), Some("main"));
    assert!(r.is_protected());
    assert_eq!(r.origin, Source::FileEnvironment("staging".into()));
}

/// An environment that omits any of `apps`, `control` or `databases` is
/// REFUSED. Inheriting the root `app` there is the silent cross-targeting the
/// rule exists to prevent - staging's control plane, production's app id - and
/// inheriting a database id is the same mistake one level worse, because it
/// lands WRITES in the wrong data rather than the wrong code.
#[test]
fn an_environment_missing_any_of_the_three_non_inheritable_keys_is_refused() {
    for (removed, fragment) in [
        (
            "\"apps\": { \"storefront\": { \"app\": \"app_034klb07lrb9jgma6imvmx001\" } },\n      ",
            "environments.staging.apps",
        ),
        (
            "\"databases\": { \"main\": { \"id\": \"dbs_03evr3oqx1200uzh8k6gycpgg\" } },\n      ",
            "environments.staging.databases",
        ),
        (
            "\"control\": \"https://control.staging.zeroship.ai\",\n      ",
            "environments.staging.control",
        ),
    ] {
        let text = FULL.replace(removed, "");
        assert_ne!(text, FULL, "the fixture must actually change for {fragment}");
        let err = ProjectConfig::parse(PathBuf::from("zeroship.jsonc"), text)
            .expect_err("a half-specified environment must not parse");
        assert!(err.contains(fragment), "{err}");
        assert!(err.contains("NON-INHERITABLE"), "{err}");
    }
    // The control: the untouched fixture parses, so the three refusals above
    // are about the removal and not about the fixture.
    cfg(FULL);
}

/// An environment map that covers only SOME of the root's labels is refused.
/// Partial coverage is the same cross-target as an absent map, hiding behind a
/// key that is present.
#[test]
fn an_environment_that_leaves_a_label_out_of_a_map_is_refused() {
    let two = FULL.replace(
        "\"main\": { \"id\": \"dbs_03evr3oqx1200yyd6zj2cebfw\", \"migrations\": \"migrations\", \"out\": \"generated/zeroship\" }",
        "\"main\": { \"id\": \"dbs_03evr3oqx1200yyd6zj2cebfw\", \"migrations\": \"migrations\", \"out\": \"generated/zeroship\" },\n    \"events\": { \"id\": \"dbs_03evr3oqx1200qyvgmdnjrsla\", \"migrations\": \"migrations/events\", \"out\": \"generated/events\" }",
    );
    assert_ne!(two, FULL, "the second database must actually be added");
    let err = ProjectConfig::parse(PathBuf::from("zeroship.jsonc"), two.clone())
        .expect_err("an environment that names one of two databases must not parse");
    assert!(err.contains("does not name `events`"), "{err}");
    assert!(err.contains("NON-INHERITABLE"), "{err}");

    // Its control: with the environment covering BOTH labels the same file
    // parses, so the refusal is about coverage rather than about the second
    // database existing.
    let covered = two.replace(
        "\"databases\": { \"main\": { \"id\": \"dbs_03evr3oqx1200uzh8k6gycpgg\" } }",
        "\"databases\": { \"main\": { \"id\": \"dbs_03evr3oqx1200uzh8k6gycpgg\" }, \"events\": { \"id\": \"dbs_03evr3oqx12012zcpsh30ivwy\" } }",
    );
    cfg(&covered);
}

/// An environment naming a label the root does not declare is refused too: an
/// environment overrides the id under a label, never the label itself.
#[test]
fn an_environment_naming_an_undeclared_label_is_refused() {
    let text = FULL.replace(
        "\"databases\": { \"main\": { \"id\": \"dbs_03evr3oqx1200uzh8k6gycpgg\" } }",
        "\"databases\": { \"main\": { \"id\": \"dbs_03evr3oqx1200uzh8k6gycpgg\" }, \"typo\": { \"id\": \"dbs_03evr3oqx12012zcpsh30ivwy\" } }",
    );
    assert_ne!(text, FULL, "the stray label must actually be added");
    let err = ProjectConfig::parse(PathBuf::from("zeroship.jsonc"), text)
        .expect_err("an environment label with no root entry must not parse");
    assert!(err.contains("environments.staging.databases.typo"), "{err}");
    assert!(err.contains("never the label itself"), "{err}");
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

/// The no-Rust-defaults constraint, asserted directly: a file
/// that omits a cross-tool key produces an error NAMING the key, never a
/// value. This is the test that would fail if somebody added a fallback.
#[test]
fn an_absent_cross_tool_key_errors_naming_it_rather_than_defaulting() {
    let text = FULL.replace("\"migrations\": \"migrations\", ", "");
    assert_ne!(text, FULL, "the key must actually be removed");
    let c = ProjectConfig::parse(PathBuf::from("zeroship.jsonc"), text)
        .expect_err("databases.main.migrations is required by the schema");
    assert!(c.contains("databases.main.migrations"), "{c}");

    // And the same shape one level down: a key the schema does not require but
    // the CLI reads.
    let r = cfg(FULL).resolve(None).unwrap();
    let err = r.require("build.serverEntry").expect_err("absent key");
    assert!(err.contains("build.serverEntry"), "{err}");
    assert!(err.contains("no default"), "{err}");
}

/// Every CLI-read default not explicitly marked safe must produce an error
/// here, not a value. This checks the rule against the generated tables rather
/// than against a second hand-maintained field list.
///
/// It is the intersection that matters. `build.mode` has a default and is NOT
/// CLI-read, so a TypeScript-only default there is correct and this test says
/// nothing about it.
#[test]
fn unsafe_cli_read_defaults_do_not_reach_rust() {
    let stripped: Vec<&str> = generated::SCHEMA_DEFAULTED_FIELDS
        .iter()
        .copied()
        .filter(|f| generated::CLI_READ_FIELDS.contains(f))
        .filter(|f| {
            !generated::RESOLVED_OPTIONAL_DEFAULTS_JSON
                .iter()
                .any(|(path, _)| path == f)
        })
        .collect();
    assert!(
        !stripped.is_empty(),
        "the intersection is empty, so this test proves nothing - check the generated tables"
    );

    // A file that omits each of them in turn. `require` must refuse.
    // Present in the full fixture, so each resolves...
    let r = cfg(FULL).resolve(None).unwrap();
    for field in &stripped {
        assert!(r.require(field).is_ok(), "{field} should resolve here");
    }
    // ...and absent, it errors rather than producing the schema default.
    let empty = ProjectConfig {
        path: PathBuf::from("zeroship.jsonc"),
        text: String::new(),
        root: Map::new(),
        project_root: std::env::current_dir().unwrap(),
    };
    let r = empty.resolve(None).unwrap();
    for field in &stripped {
        let err = r.require(field).expect_err("must not default");
        assert!(err.contains(field), "{err}");
    }
}

#[test]
fn absent_secrets_resolve_to_the_schema_safe_empty_default() {
    let text = FULL.replace("  \"secrets\": [\"STRIPE_SECRET_KEY\"],\n", "");
    let resolved = cfg(&text).resolve(None).unwrap();
    assert_eq!(resolved.get("secrets"), Some(&Value::Array(Vec::new())));
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
        r#"{"name":"a","control":"u","runtime_date":"2026-08-14","password":"hunter2","build":{"mode":"full","dist":"d","output":"o"},"databases":{},"apps":{}}"#,
        r#"{"name":"a","control":"u","runtime_date":"2026-08-14","build":{"mode":"full","dist":"d","output":"o","token":"x"},"databases":{},"apps":{}}"#,
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
fn build_dist_cannot_be_the_project_root_or_one_of_its_ancestors() {
    for dist in [".", "..", "../..", "dist/..", "/tmp"] {
        let text = FULL.replace(
            "\"dist\": \"dist\"",
            &format!("\"dist\": {}", serde_json::to_string(dist).unwrap()),
        );
        let err = ProjectConfig::parse(
            PathBuf::from("/tmp/project/zeroship.jsonc"),
            text,
        )
        .expect_err("a dist containing zeroship.jsonc must not parse");
        assert!(err.contains("build.dist"), "{err}");
        assert!(err.contains("ancestor"), "{err}");
        assert!(err.contains("zeroship.jsonc"), "{err}");
    }
}

#[cfg(unix)]
#[test]
fn build_dist_cannot_symlink_to_the_project_root() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join(CONFIG_FILENAME);
    let linked_root = dir.path().join("linked-root");
    std::os::unix::fs::symlink(".", &linked_root).unwrap();
    let text = FULL.replace("\"dist\": \"dist\"", "\"dist\": \"linked-root\"");

    let err = ProjectConfig::parse(config_path, text)
        .expect_err("a symlinked dist containing zeroship.jsonc must not parse");
    assert!(err.contains("build.dist"), "{err}");
    assert!(err.contains("zeroship.jsonc"), "{err}");
}

#[test]
fn build_output_cannot_target_the_project_root_an_ancestor_or_an_existing_source_file() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join(CONFIG_FILENAME);
    let source_path = dir.path().join("src/main.rs");
    std::fs::create_dir_all(source_path.parent().unwrap()).unwrap();
    std::fs::write(&source_path, "fn main() {}\n").unwrap();

    for output in [".", "..", CONFIG_FILENAME, "src/main.rs"] {
        let text = FULL.replace(
            "\"output\": \"dist/app.zship\"",
            &format!("\"output\": {}", serde_json::to_string(output).unwrap()),
        );
        std::fs::write(&config_path, &text).unwrap();
        let err = ProjectConfig::parse(config_path.clone(), text)
            .expect_err("an output that can overwrite creator data must not parse");
        assert!(err.contains("build.output"), "{err}");
    }
}

#[test]
fn build_output_may_replace_an_existing_generated_artifact() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join(CONFIG_FILENAME);
    let output_path = dir.path().join("dist/app.zship");
    std::fs::create_dir_all(output_path.parent().unwrap()).unwrap();
    std::fs::write(&output_path, "old artifact").unwrap();

    ProjectConfig::parse(config_path, FULL.to_string())
        .expect("an existing generated artifact remains a valid output");
}

#[test]
fn a_databases_out_cannot_target_the_project_root_or_one_of_its_ancestors() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join(CONFIG_FILENAME);

    for out in [".", "..", "generated/zeroship/../..", "/tmp"] {
        let text = FULL.replace(
            "\"out\": \"generated/zeroship\"",
            &format!("\"out\": {}", serde_json::to_string(out).unwrap()),
        );
        assert_ne!(text, FULL, "the out path must actually change for {out}");
        let err = ProjectConfig::parse(config_path.clone(), text)
            .expect_err("a gen-types directory containing creator files must not parse");
        assert!(err.contains("databases.main.out"), "{err}");
    }
}

/// Two databases may not share one gen-types directory. The three filenames in
/// it are fixed, so a shared directory is one database's schema silently
/// standing in for another's.
#[test]
fn two_databases_cannot_share_one_gen_types_directory() {
    let text = FULL.replace(
        "\"main\": { \"id\": \"dbs_03evr3oqx1200yyd6zj2cebfw\", \"migrations\": \"migrations\", \"out\": \"generated/zeroship\" }",
        "\"main\": { \"id\": \"dbs_03evr3oqx1200yyd6zj2cebfw\", \"migrations\": \"migrations\", \"out\": \"generated/zeroship\" },\n    \"events\": { \"id\": \"dbs_03evr3oqx1200qyvgmdnjrsla\", \"migrations\": \"migrations/events\", \"out\": \"generated/zeroship\" }",
    );
    let text = text.replace(
        "\"databases\": { \"main\": { \"id\": \"dbs_03evr3oqx1200uzh8k6gycpgg\" } }",
        "\"databases\": { \"main\": { \"id\": \"dbs_03evr3oqx1200uzh8k6gycpgg\" }, \"events\": { \"id\": \"dbs_03evr3oqx12012zcpsh30ivwy\" } }",
    );
    let err = ProjectConfig::parse(PathBuf::from("zeroship.jsonc"), text.clone())
        .expect_err("two databases sharing one out dir must not parse");
    assert!(err.contains("gen-types directory"), "{err}");

    // Its control: the same pair with distinct directories parses, so the
    // refusal is about the collision and not about the second database.
    cfg(&text.replace(
        "\"migrations\": \"migrations/events\", \"out\": \"generated/zeroship\"",
        "\"migrations\": \"migrations/events\", \"out\": \"generated/events\"",
    ));
}

#[test]
fn an_unknown_top_level_key_names_the_known_ones() {
    let text = FULL.replace("\"name\": \"demo-app\",", "\"name\": \"demo-app\", \"rpcEndpoint\": \"/_rpc\",");
    let err = ProjectConfig::parse(PathBuf::from("zeroship.jsonc"), text)
        .expect_err("an unknown key must not parse");
    assert!(err.contains("rpcEndpoint"), "{err}");
    assert!(err.contains("databases"), "the message must list the known keys: {err}");
}

/// The canonical dump is sorted and compact, so the TypeScript side can be
/// compared to it byte for byte.
#[test]
fn canonical_json_is_sorted_and_compact() {
    let r = cfg(FULL).resolve(None).unwrap();
    let dump = r.canonical_json();
    assert!(dump.starts_with("{\"apps\":"), "{dump}");
    assert!(!dump.contains('\n'), "{dump}");
    let keys: Vec<&str> = ["apps", "build", "control", "databases", "name", "runtime_date", "secrets"]
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

/// THE ONE-VARIABLE PAIR for the no-default rule. Same call, same key, same
/// fallback; the only difference is whether a config file was found.
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
        "\"app\": \"app_034klb07lrb9jgma6imvmx000\",",
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
// JSONC preservation cases
// ---------------------------------------------------------------------------

#[test]
fn jsonc_comments_trailing_commas_and_string_markers_parse() {
    let text = r#"{
      // URL and comment markers inside strings remain data.
      "name": "demo-app",
      "control": "https://control.zeroship.ai",
      "runtime_date": "2026-08-14",
      "build": {
        "mode": "full",
        "serverEntry": "src/x\"/*y*/,}.ts",
        "dist": "dist",
        "output": "dist/app.zship",
      },
      "databases": { "main": { "id": "dbs_03evr3oqx1200yyd6zj2cebfw", "migrations": "migrations", "out": "generated/zeroship", }, },
      "apps": { "storefront": { "databases": ["main",], "primary": "main", }, },
    }"#;
    let resolved = cfg(text).resolve(None).expect("JSONC must resolve");
    assert_eq!(resolved.str("control"), Some("https://control.zeroship.ai"));
    assert_eq!(resolved.str("build.serverEntry"), Some("src/x\"/*y*/,}.ts"));
}

#[test]
fn crlf_jsonc_parses() {
    let text = FULL.replace('\n', "\r\n");
    let resolved = cfg(&text).resolve(None).expect("CRLF JSONC must resolve");
    assert_eq!(resolved.str("name"), Some("demo-app"));
}

#[test]
fn bare_carriage_return_line_endings_are_rejected() {
    let text = FULL.replace('\n', "\r");
    let err = ProjectConfig::parse(PathBuf::from("zeroship.jsonc"), text)
        .expect_err("bare carriage returns must not be parsed differently from TypeScript");
    assert!(err.contains("bare carriage return"), "{err}");
}

#[test]
fn unicode_escaped_keys_and_values_parse() {
    let text = FULL
        .replacen("\"name\": \"demo-app\"", "\"\\u006eame\": \"demo-app\"", 1)
        .replacen(
            "\"mode\": \"full\"",
            "\"mode\": \"full\", \"server\\u0045ntry\": \"caf\\u00e9-\\ud83d\\ude00.ts\"",
            1,
        );
    let resolved = cfg(&text).resolve(None).expect("escaped JSON must resolve");
    assert_eq!(resolved.str("name"), Some("demo-app"));
    assert_eq!(
        resolved.str("build.serverEntry"),
        Some("caf\u{e9}-\u{1f600}.ts")
    );
}

#[test]
fn unpaired_surrogate_is_rejected() {
    let text = FULL.replacen(
        "\"mode\": \"full\"",
        "\"mode\": \"full\", \"serverEntry\": \"src/\\ud800.ts\"",
        1,
    );
    let err = ProjectConfig::parse(PathBuf::from("zeroship.jsonc"), text)
        .expect_err("an unpaired surrogate is not a Unicode scalar value");
    assert!(err.contains("unpaired high surrogate"), "{err}");
}

#[test]
fn bom_is_rejected() {
    let text = format!("\u{feff}{FULL}");
    ProjectConfig::parse(PathBuf::from("zeroship.jsonc"), text)
        .expect_err("a leading BOM was rejected by the existing reader");
}

#[test]
fn raw_form_feed_outside_comment_is_rejected() {
    let text = FULL.replacen("{\n", "{\u{000c}\n", 1);
    ProjectConfig::parse(PathBuf::from("zeroship.jsonc"), text)
        .expect_err("raw form feed is not JSON whitespace");
}

#[test]
fn raw_form_feed_inside_comment_is_accepted() {
    let text = FULL.replacen("// A comment", "// A\u{000c} comment", 1);
    let resolved = cfg(&text).resolve(None).expect("comment content is ignored");
    assert_eq!(resolved.str("name"), Some("demo-app"));
}

#[test]
fn raw_control_character_inside_string_is_rejected() {
    let text = FULL.replacen(
        "https://control.zeroship.ai",
        "https://control.\nzeroship.ai",
        1,
    );
    ProjectConfig::parse(PathBuf::from("zeroship.jsonc"), text)
        .expect_err("raw newlines are not valid inside JSON strings");
    super::jsonc::parse(r#"{"value":"line\nbreak"}"#)
        .expect("the escaped form remains valid JSON");
}

#[test]
fn non_json_unicode_whitespace_is_rejected() {
    let text = FULL.replacen("{\n", "{\u{00a0}\n", 1);
    ProjectConfig::parse(PathBuf::from("zeroship.jsonc"), text)
        .expect_err("non-breaking space is not JSON whitespace");
}

#[test]
fn loose_json_extensions_are_rejected() {
    let cases = [
        ("unquoted property", FULL.replacen("\"name\":", "name:", 1)),
        (
            "missing comma",
            FULL.replacen(
                "\"name\": \"demo-app\",\n  \"control\"",
                "\"name\": \"demo-app\"\n  \"control\"",
                1,
            ),
        ),
        (
            "single-quoted string",
            FULL.replacen("\"demo-app\"", "'demo-app'", 1),
        ),
    ];
    for (label, text) in cases {
        ProjectConfig::parse(PathBuf::from("zeroship.jsonc"), text)
            .expect_err(label);
    }
    for (label, text) in [
        ("hexadecimal number", r#"{"value":0x10}"#),
        ("unary plus", r#"{"value":+1}"#),
    ] {
        super::jsonc::parse(text).expect_err(label);
    }
}

#[test]
fn proto_key_is_rejected() {
    let text = FULL.replacen(
        "{\n",
        "{\n  \"__proto__\": { \"control\": \"https://attacker.invalid\" },\n",
        1,
    );
    let err = ProjectConfig::parse(PathBuf::from("zeroship.jsonc"), text)
        .expect_err("unknown __proto__ must not disappear during parsing");
    assert!(err.contains("__proto__"), "{err}");
}

// ---------------------------------------------------------------------------
// Writeback
// ---------------------------------------------------------------------------

/// The splice rewrites the labelled `app` value and NOTHING else - comments,
/// key order and whitespace outside the value span are byte-identical.
#[test]
fn write_app_id_splices_the_value_and_leaves_every_other_byte() {
    let dir = std::env::temp_dir().join(format!("zs-pc-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("splice.jsonc");
    std::fs::write(&path, FULL).unwrap();

    let c = ProjectConfig::load(&path).unwrap();
    c.write_app_id("storefront", &app_id("app_034klb07lrb9jgma6imvmx002"))
        .unwrap();

    let after = std::fs::read_to_string(&path).unwrap();
    let expected = FULL.replace(
        "\"app_034klb07lrb9jgma6imvmx000\"",
        "\"app_034klb07lrb9jgma6imvmx002\"",
    );
    assert_eq!(after, expected, "the splice touched something other than the app value");
    assert!(after.contains("// A comment"), "the comment must survive");
    std::fs::remove_file(&path).ok();
}

/// The splice descends to the NAMED label, and the same key name at another
/// depth is not the target. `environments.staging.apps.storefront.app` sits at
/// exactly the shape a one-level finder would take for the root's.
#[test]
fn write_app_id_never_takes_the_environment_entry_for_the_root_one() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(CONFIG_FILENAME);
    std::fs::write(&path, FULL).unwrap();

    ProjectConfig::load(&path)
        .unwrap()
        .write_app_id("storefront", &app_id("app_034klb07lrb9jgma6imvmx007"))
        .unwrap();

    let after = std::fs::read_to_string(&path).unwrap();
    let reread = ProjectConfig::parse(path.clone(), after.clone()).unwrap();
    assert_eq!(
        reread.resolve(None).unwrap().str("apps.storefront.app"),
        Some("app_034klb07lrb9jgma6imvmx007")
    );
    assert_eq!(
        reread
            .resolve(Some("staging"))
            .unwrap()
            .str("apps.storefront.app"),
        Some("app_034klb07lrb9jgma6imvmx001"),
        "the environment's own id must be untouched"
    );
}

/// Appending changes only the insertion site. This one exact comparison covers
/// comments, member order, an interior blank line, trailing commas, CRLF, and
/// multibyte text together so preserving five while losing one cannot pass.
#[test]
fn write_app_id_appends_a_missing_member_without_reformatting_the_file() {
    let multibyte = "caf\u{e9}-\u{1f600}";
    let text = format!(
        "{{\r\n\
         \x20\x20// {multibyte}\r\n\
         \x20\x20\"$schema\": \"https://zeroship.ai/schema/project-v1.json\",\r\n\
         \x20\x20\"name\": \"demo-app\",\r\n\
         \x20\x20\"control\": \"https://control.zeroship.ai\",\r\n\
         \x20\x20\"runtime_date\": \"2026-08-14\",\r\n\
         \r\n\
         \x20\x20// Keep this group and its blank line.\r\n\
         \x20\x20\"build\": {{ \"mode\": \"full\", \"dist\": \"dist\", \"output\": \"dist/app.zship\" }},\r\n\
         \x20\x20\"databases\": {{}},\r\n\
         \x20\x20\"apps\": {{ \"storefront\": {{ \"databases\": [] }} }},\r\n\
         \x20\x20\"secrets\": [],\r\n\
         }}\r\n"
    );
    // The CST reflows the ENTRY it appends into, and nothing outside it: the
    // comment, the blank line, the CRLF endings, the trailing comma and the
    // multibyte text are all still there, byte for byte.
    let expected = text.replacen(
        "\"apps\": { \"storefront\": { \"databases\": [] } },",
        "\"apps\": { \"storefront\": {\r\n      \"databases\": [],\r\n      \"app\": \"app_034klb07lrb9jgma6imvmx003\"\r\n    } },",
        1,
    );
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("append.jsonc");
    std::fs::write(&path, &text).unwrap();

    let config = ProjectConfig::load(&path).unwrap();
    config
        .write_app_id("storefront", &app_id("app_034klb07lrb9jgma6imvmx003"))
        .unwrap();

    let after = std::fs::read_to_string(&path).unwrap();
    assert_eq!(
        after, expected,
        "the new member must be the only formatting change"
    );
    assert_eq!(
        ProjectConfig::load(&path)
            .unwrap()
            .resolve(None)
            .unwrap()
            .str("apps.storefront.app"),
        Some("app_034klb07lrb9jgma6imvmx003"),
        "the Rust reader must accept the written file"
    );
}

/// A label the file does not declare is a naming error, not a block to invent.
#[test]
fn write_app_id_refuses_a_label_the_file_does_not_declare() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(CONFIG_FILENAME);
    std::fs::write(&path, FULL).unwrap();

    let error = ProjectConfig::load(&path)
        .unwrap()
        .write_app_id("admin", &app_id("app_034klb07lrb9jgma6imvmx009"))
        .expect_err("an undeclared label has no entry to write into");
    assert!(error.contains("apps.admin.app"), "{error}");
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        FULL,
        "a refused writeback must leave the file alone"
    );
}

#[test]
fn write_app_id_refuses_to_overwrite_a_file_changed_since_load() {
    let original = FULL.replace("\"app\": \"app_034klb07lrb9jgma6imvmx000\", ", "");
    assert_ne!(original, FULL, "the root app id must actually be removed");
    let creator_edit = original.replace(
        "// A comment, which is the whole reason the format is JSONC.",
        "// A concurrent creator edit that must survive.",
    );
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(CONFIG_FILENAME);
    std::fs::write(&path, &original).unwrap();

    let config = ProjectConfig::load(&path).unwrap();
    std::fs::write(&path, &creator_edit).unwrap();
    let error = config
        .write_app_id("storefront", &app_id("app_034klb07lrb9jgma6imvmx008"))
        .expect_err("writeback must refuse a file edited after load");

    assert!(error.contains("changed since it was loaded"), "{error}");
    assert_eq!(
        std::fs::read_to_string(path).unwrap(),
        creator_edit,
        "the concurrent creator edit must remain byte-for-byte intact"
    );
}

/// `CstObject::append` reflows the object it appends into. That object is now
/// one labelled ENTRY, so the root's own trivia - including the blank lines
/// touching its braces, which a root-level append would have normalised - is
/// outside the edit. Pin the whole output so a library upgrade may change how
/// the entry is laid out but may not quietly start eating anything else.
#[test]
fn write_app_id_reflows_only_the_entry_it_appends_into() {
    let text = r#"{

  // The leading comment must survive.
  "name": "demo-app",
  "control": "https://control.zeroship.ai",
  "runtime_date": "2026-08-14",

  // The interior group must survive.
  "build": { "mode": "full", "dist": "dist", "output": "dist/app.zship" },
  "databases": {},
  "apps": {
    "storefront": { "databases": [] }
  },
  "secrets": []

}
"#;
    let expected = r#"{

  // The leading comment must survive.
  "name": "demo-app",
  "control": "https://control.zeroship.ai",
  "runtime_date": "2026-08-14",

  // The interior group must survive.
  "build": { "mode": "full", "dist": "dist", "output": "dist/app.zship" },
  "databases": {},
  "apps": {
    "storefront": {
      "databases": [],
      "app": "app_034klb07lrb9jgma6imvmx005"
    }
  },
  "secrets": []

}
"#;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("root-blank-lines.jsonc");
    std::fs::write(&path, text).unwrap();

    ProjectConfig::load(&path)
        .unwrap()
        .write_app_id("storefront", &app_id("app_034klb07lrb9jgma6imvmx005"))
        .unwrap();

    assert_eq!(std::fs::read_to_string(&path).unwrap(), expected);
}

/// The generated CST is validated as a complete project config before the
/// original path is touched. Construct an intentionally inconsistent internal
/// value to make that otherwise defensive failure arm observable.
#[test]
fn write_app_id_reparses_before_writing() {
    let text = "{\n  \"name\": \"demo-app\",\n  \"apps\": { \"storefront\": {} }\n}\n";
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("invalid-project.jsonc");
    std::fs::write(&path, text).unwrap();
    let config = ProjectConfig {
        path: path.clone(),
        text: text.to_string(),
        root: Map::new(),
        project_root: dir.path().to_path_buf(),
    };

    let error = config
        .write_app_id("storefront", &app_id("app_034klb07lrb9jgma6imvmx006"))
        .expect_err("the generated text is valid JSONC but not a valid project config");

    assert!(error.contains("refusing to write a file that would not parse"), "{error}");
    assert!(error.contains("control"), "{error}");
    assert_eq!(
        std::fs::read_to_string(path).unwrap(),
        text,
        "validation must happen before the write"
    );
}

#[test]
fn write_app_id_uses_original_byte_span_with_crlf_unicode_and_escaped_key() {
    let text = FULL
        .replacen(
            "// A comment, which is the whole reason the format is JSONC.",
            "// caf\u{e9}-\u{1f600}",
            1,
        )
        .replacen("\"app\": \"app_034klb07", "\"\\u0061pp\": \"app_034klb07", 1)
        .replace('\n', "\r\n");
    let dir = std::env::temp_dir().join(format!("zs-pc-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("splice-original-span.jsonc");
    std::fs::write(&path, &text).unwrap();

    let config = ProjectConfig::load(&path).unwrap();
    config
        .write_app_id("storefront", &app_id("app_034klb07lrb9jgma6imvmx004"))
        .unwrap();

    let after = std::fs::read_to_string(&path).unwrap();
    let expected = text.replace(
        "\"app_034klb07lrb9jgma6imvmx000\"",
        "\"app_034klb07lrb9jgma6imvmx004\"",
    );
    assert_eq!(after, expected, "only the original app value span may change");
    std::fs::remove_file(&path).ok();
}

// ---------------------------------------------------------------------------
// Label selection
// ---------------------------------------------------------------------------

fn argv(flags: &[&str]) -> Vec<String> {
    std::iter::once("zeroship".to_string())
        .chain(std::iter::once("deploy".to_string()))
        .chain(flags.iter().map(|f| (*f).to_string()))
        .collect()
}

/// A workspace declaring one app implies it, and the id comes from the file.
#[test]
fn a_sole_declared_app_is_the_target_without_a_flag() {
    let resolved = cfg(FULL).resolve(None).unwrap();
    let selection = select_app(&argv(&[]), Some(&resolved)).expect("the sole app is implied");
    assert_eq!(selection.label.as_deref(), Some("storefront"));
    let id = selection.id.expect("the file carries an id");
    assert_eq!(id.value, "app_034klb07lrb9jgma6imvmx000");
    assert_eq!(
        id.source.describe(),
        "zeroship.jsonc apps.storefront",
        "the provenance line must name the entry a creator would edit"
    );
}

/// With several declared, the command must be told which - and with none
/// declared there is nothing to imply.
#[test]
fn several_declared_apps_require_a_label_and_none_is_an_error() {
    let two = FULL.replace(
        "\"storefront\": { \"app\": \"app_034klb07lrb9jgma6imvmx000\", \"databases\": [\"main\"], \"primary\": \"main\" }",
        "\"storefront\": { \"app\": \"app_034klb07lrb9jgma6imvmx000\", \"databases\": [\"main\"], \"primary\": \"main\" },\n    \"admin\": { \"app\": \"app_034klb07lrb9jgma6imvmx001\", \"databases\": [\"main\"], \"primary\": \"main\" }",
    );
    let two = two.replace(
        "\"apps\": { \"storefront\": { \"app\": \"app_034klb07lrb9jgma6imvmx001\" } }",
        "\"apps\": { \"storefront\": { \"app\": \"app_034klb07lrb9jgma6imvmx001\" }, \"admin\": { \"app\": \"app_034klb07lrb9jgma6imvmx002\" } }",
    );
    let resolved = cfg(&two).resolve(None).unwrap();
    let err = select_app(&argv(&[]), Some(&resolved))
        .expect_err("two declared apps cannot be implied");
    assert!(err.contains("more than one app"), "{err}");
    assert!(err.contains("storefront") && err.contains("admin"), "{err}");

    let picked = select_app(&argv(&["--app=admin"]), Some(&resolved)).expect("named");
    assert_eq!(picked.label.as_deref(), Some("admin"));
    assert_eq!(
        picked.id.expect("id").value,
        "app_034klb07lrb9jgma6imvmx001"
    );

    let none = cfg(BARE).resolve(None).unwrap();
    let err = select_app(&argv(&[]), Some(&none)).expect_err("no app to imply");
    assert!(err.contains("declares no apps"), "{err}");
}

/// With a file present `--app` names a LABEL, so a raw id is a naming error
/// that lists the labels. With NO file there are no labels and the same flag
/// is the id.
#[test]
fn the_app_flag_is_a_label_with_a_file_and_an_id_without_one() {
    let resolved = cfg(FULL).resolve(None).unwrap();
    let err = select_app(&argv(&["--app=app_034klb07lrb9jgma6imvmx000"]), Some(&resolved))
        .expect_err("an id is not a label");
    assert!(err.contains("names no app"), "{err}");
    assert!(err.contains("storefront"), "{err}");
    assert!(err.contains("never travels as an identifier"), "{err}");

    let without =
        select_app(&argv(&["--app=app_034klb07lrb9jgma6imvmx000"]), None).expect("no file, an id");
    assert!(without.label.is_none());
    assert_eq!(
        without.id.expect("id").value,
        "app_034klb07lrb9jgma6imvmx000"
    );
}

/// `--database` names one of the FILE's labels, and a workspace declaring one
/// implies it - the same rule `--app` follows, one section over.
#[test]
fn the_database_flag_names_one_of_the_files_labels() {
    let resolved = cfg(FULL).resolve(None).unwrap();
    assert_eq!(
        resolved.database_labels(),
        vec!["main"],
        "the precondition for the implied arm below"
    );
    assert_eq!(
        select_database(&argv(&[]), &resolved).expect("one declared database is implied"),
        "main"
    );
    assert_eq!(
        select_database(&argv(&["--database=main"]), &resolved).expect("named"),
        "main"
    );
    let err = select_database(&argv(&["--database=events"]), &resolved)
        .expect_err("a database this file does not declare is not addressable");
    assert!(err.contains("names no database"), "{err}");
    assert!(err.contains("main"), "{err}");
}

/// SEVERAL DECLARED: the flag is demanded rather than one of them guessed.
///
/// Read off the committed cross-tool fixture, which declares two. A caller that
/// picked the first would write a schema into a database nobody named, and
/// `apps.admin` not using `analytics` no longer narrows anything: the selection
/// is workspace-wide because a database belongs to its project, not to an app.
#[test]
fn several_declared_databases_demand_the_flag_and_none_is_app_scoped() {
    let text = include_str!("../../../../tests/fixtures/project-config/zeroship.jsonc");
    let resolved = cfg(text).resolve(None).unwrap();
    assert_eq!(resolved.database_labels(), vec!["main", "analytics"]);

    let err = select_database(&argv(&[]), &resolved).expect_err("an ambiguous target is refused");
    assert!(err.contains("more than one database"), "{err}");
    assert!(err.contains("--database=<label>"), "{err}");
    assert!(err.contains("analytics"), "{err}");

    // The app-scoped rule this replaces refused `analytics` whenever the
    // selected app did not use it. The fixture still carries such an app, so
    // the assertion below is a measurement rather than a restatement.
    assert!(!resolved.app_databases("admin").contains(&"analytics"));
    assert_eq!(
        select_database(&argv(&["--database=analytics"]), &resolved)
            .expect("a database is addressable without an app that uses it"),
        "analytics"
    );
}

/// NO DATABASES AT ALL: the refusal names the section to add one to, rather
/// than reporting an ambiguity that does not exist.
#[test]
fn a_file_declaring_no_databases_says_so() {
    let resolved = cfg(BARE).resolve(None).unwrap();
    assert!(resolved.database_labels().is_empty());
    let err = select_database(&argv(&[]), &resolved).expect_err("there is nothing to select");
    assert!(err.contains("declares no databases"), "{err}");
}

// ---------------------------------------------------------------------------
// The app-to-database wiring
// ---------------------------------------------------------------------------

/// An app names database LABELS, and every one has to resolve in this file.
/// Declaring a database grants nothing - deploy verifies the binding - but a
/// label that resolves to nothing could not even be dereferenced to an id.
#[test]
fn an_app_naming_an_undeclared_database_is_refused() {
    let text = FULL.replace("\"databases\": [\"main\"]", "\"databases\": [\"main\", \"ghost\"]");
    assert_ne!(text, FULL, "the stray label must actually be added");
    let err = ProjectConfig::parse(PathBuf::from("zeroship.jsonc"), text)
        .expect_err("an app naming an undeclared database must not parse");
    assert!(err.contains("apps.storefront.databases"), "{err}");
    assert!(err.contains("ghost"), "{err}");
    assert!(err.contains("declared: main"), "{err}");
}

/// The primary is `env.db`, and `env.db === env.databases[primary]` holds by
/// object identity, so it cannot be inferred: an app that uses a database and
/// names no primary is refused, and one that names a primary it does not use
/// is refused too.
#[test]
fn the_primary_must_be_stated_and_must_be_one_of_the_apps_own_databases() {
    let missing = FULL.replace(", \"primary\": \"main\" }", " }");
    assert_ne!(missing, FULL, "the primary must actually be removed");
    let err = ProjectConfig::parse(PathBuf::from("zeroship.jsonc"), missing)
        .expect_err("an app that uses a database must name its primary");
    assert!(err.contains("names no `primary`"), "{err}");

    let foreign = FULL.replace("\"primary\": \"main\"", "\"primary\": \"analytics\"");
    assert_ne!(foreign, FULL, "the primary must actually change");
    let err = ProjectConfig::parse(PathBuf::from("zeroship.jsonc"), foreign)
        .expect_err("a primary the app does not use must not parse");
    assert!(err.contains("not one of"), "{err}");

    // The control: an app declaring NO database needs no primary at all.
    cfg(BARE);
}

/// A workspace where one app makes `analytics` its `env.db` and another merely
/// uses it. Both are typed from that database's SINGLE generated `env.db.ts`,
/// which declares `Env.db` for a primary and not otherwise.
const SPLIT_PRIMACY: &str = r#"{
  "$schema": "https://zeroship.ai/schema/project-v1.json",
  "name": "demo-app",
  "control": "https://control.zeroship.ai",
  "runtime_date": "2026-08-14",
  "build": { "mode": "full", "dist": "dist", "output": "dist/app.zship" },
  "databases": {
    "main": { "id": "dbs_03evr3oqx1200yyd6zj2cebfw", "migrations": "migrations", "out": "generated/zeroship/main" },
    "analytics": { "id": "dbs_03evr3oqx1200qyvgmdnjrsla", "migrations": "migrations/analytics", "out": "generated/zeroship/analytics" }
  },
  "apps": {
    "storefront": { "databases": ["main", "analytics"], "primary": "main" },
    "reporting": { "databases": ["analytics"], "primary": "analytics" }
  },
  "secrets": []
}"#;

/// A database is the `env.db` of every app that uses it, or of none of them.
/// One database has one gen-types directory, so it has one `env.db.ts`, and
/// that file cannot both declare `Env.db` and not declare it.
#[test]
fn a_database_that_is_one_apps_primary_and_anothers_secondary_is_refused() {
    let err = ProjectConfig::parse(
        PathBuf::from("zeroship.jsonc"),
        SPLIT_PRIMACY.to_string(),
    )
    .expect_err("split primacy must not parse");
    assert!(err.contains("makes `analytics` its `primary`"), "{err}");
    assert!(err.contains("uses it without naming it"), "{err}");
    assert!(err.contains("databases.analytics.out"), "{err}");

    // THE CONTROL, differing in one variable: the same two apps, the same
    // shared database, agreeing that it is the primary. Two apps sharing a
    // database is the ordinary shape and must stay accepted, or the rule would
    // refuse every multi-app workspace rather than the contradiction.
    let agreed = SPLIT_PRIMACY.replace(
        "\"storefront\": { \"databases\": [\"main\", \"analytics\"], \"primary\": \"main\" }",
        "\"storefront\": { \"databases\": [\"analytics\"], \"primary\": \"analytics\" }",
    );
    assert_ne!(agreed, SPLIT_PRIMACY, "storefront's wiring must actually change");
    let resolved = ProjectConfig::parse(PathBuf::from("zeroship.jsonc"), agreed)
        .expect("agreeing apps must parse")
        .resolve(None)
        .expect("and resolve");
    assert_eq!(resolved.app_primary("storefront"), Some("analytics"));
    assert_eq!(resolved.app_primary("reporting"), Some("analytics"));
}

/// A label is a member name on `env.databases` as well as a key here, so it is
/// constrained to what reads as one. `__proto__` is the case that makes the
/// rule load-bearing rather than cosmetic.
#[test]
fn a_label_that_is_not_a_usable_member_name_is_refused() {
    for (bad, section) in [
        ("\"__proto__\": { \"id\": \"dbs_", "databases"),
        ("\"Main\": { \"id\": \"dbs_", "databases"),
    ] {
        let text = FULL.replace("\"main\": { \"id\": \"dbs_", bad);
        assert_ne!(text, FULL, "the label must actually change for {section}");
        let err = ProjectConfig::parse(PathBuf::from("zeroship.jsonc"), text)
            .expect_err("an unusable label must not parse");
        assert!(err.contains("not a usable label"), "{err}");
    }
}

/// A database id is the identifier of real data. A value that is not one is
/// refused here rather than sent to a server that would answer 404 for a
/// reason the creator cannot see.
#[test]
fn a_malformed_database_id_is_refused_naming_the_command_that_prints_one() {
    let text = FULL.replace("dbs_03evr3oqx1200yyd6zj2cebfw", "main");
    assert_ne!(text, FULL, "the id must actually change");
    let err = ProjectConfig::parse(PathBuf::from("zeroship.jsonc"), text)
        .expect_err("a database name where an id belongs must not parse");
    assert!(err.contains("databases.main.id"), "{err}");
    assert!(err.contains("zeroship db create"), "{err}");
}
