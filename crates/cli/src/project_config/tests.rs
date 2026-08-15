//! Unit tests for the CLI's `zeroship.jsonc` reader.
//!
//! WHAT THESE DO NOT CATCH, stated so the coverage is not overread:
//!
//! - They do not compare this reader against the TypeScript one. Byte-equality
//!   of the two resolved dumps is checked by `tests/project_config_gate.sh` and
//!   needs both binaries; nothing here can see a divergent TS default.
//! - They do not exercise `locate` against a real `ZEROSHIP_CONFIG`, because
//!   setting process environment in a threaded test runner races every other
//!   test in the binary. The env arm is covered by the shell gate.
//! - `write_app` is tested on temp files, including a real disk edit after load.

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

/// The no-Rust-defaults constraint, asserted directly: a file
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
/// error here, not a value. This checks the rule against the generated tables
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
        let err = ProjectConfig::parse_with_root(config_path.clone(), text, dir.path())
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

    ProjectConfig::parse_with_root(config_path, FULL.to_string(), dir.path())
        .expect("an existing generated artifact remains a valid output");
}

#[test]
fn migrations_out_cannot_target_the_project_root_or_one_of_its_ancestors() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join(CONFIG_FILENAME);

    for out in [".", "..", "generated/zeroship/../..", "/tmp"] {
        let text = FULL.replace(
            "\"out\": \"generated/zeroship\"",
            &format!("\"out\": {}", serde_json::to_string(out).unwrap()),
        );
        let err = ProjectConfig::parse_with_root(config_path.clone(), text, dir.path())
            .expect_err("a gen-types directory containing creator files must not parse");
        assert!(err.contains("migrations.out"), "{err}");
    }
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
      "migrations": { "dir": "migrations", "out": "generated/zeroship", },
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
                "\"name\": \"demo-app\",\n  \"app\"",
                "\"name\": \"demo-app\"\n  \"app\"",
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

/// The splice rewrites the `app` value and NOTHING else - comments, key order
/// and whitespace outside the value span are byte-identical.
#[test]
fn write_app_splices_the_value_and_leaves_every_other_byte() {
    let dir = std::env::temp_dir().join(format!("zs-pc-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("splice.jsonc");
    std::fs::write(&path, FULL).unwrap();

    let c = ProjectConfig::load(&path).unwrap();
    c.write_app("33333333-3333-4333-8333-333333333333")
        .unwrap();

    let after = std::fs::read_to_string(&path).unwrap();
    let expected = FULL.replace(
        "\"11111111-1111-4111-8111-111111111111\"",
        "\"33333333-3333-4333-8333-333333333333\"",
    );
    assert_eq!(after, expected, "the splice touched something other than the app value");
    assert!(after.contains("// A comment"), "the comment must survive");
    std::fs::remove_file(&path).ok();
}

/// Appending changes only the insertion site. This one exact comparison covers
/// comments, member order, an interior blank line, trailing commas, CRLF, and
/// multibyte text together so preserving five while losing one cannot pass.
#[test]
fn write_app_appends_a_missing_member_without_reformatting_the_file() {
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
         \x20\x20\"migrations\": {{ \"dir\": \"migrations\", \"out\": \"generated/zeroship\" }},\r\n\
         \x20\x20\"secrets\": [],\r\n\
         }}\r\n"
    );
    let expected = text.replacen(
        "\r\n}\r\n",
        "\r\n  \"app\": \"44444444-4444-4444-8444-444444444444\",\r\n}\r\n",
        1,
    );
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("append.jsonc");
    std::fs::write(&path, &text).unwrap();

    let config = ProjectConfig::load(&path).unwrap();
    config
        .write_app("44444444-4444-4444-8444-444444444444")
        .unwrap();

    let after = std::fs::read_to_string(&path).unwrap();
    assert_eq!(
        after, expected,
        "the new final member must be the only formatting change"
    );
    assert_eq!(
        ProjectConfig::load(&path)
            .unwrap()
            .resolve(None)
            .unwrap()
            .str("app"),
        Some("44444444-4444-4444-8444-444444444444"),
        "the Rust reader must accept the written file"
    );
}

#[test]
fn write_app_refuses_to_overwrite_a_file_changed_since_load() {
    let original = FULL.replace(
        "  \"app\": \"11111111-1111-4111-8111-111111111111\",\n",
        "",
    );
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
        .write_app("99999999-9999-4999-8999-999999999999")
        .expect_err("writeback must refuse a file edited after load");

    assert!(error.contains("changed since it was loaded"), "{error}");
    assert_eq!(
        std::fs::read_to_string(path).unwrap(),
        creator_edit,
        "the concurrent creator edit must remain byte-for-byte intact"
    );
}

/// `CstObject::append` deliberately normalises extra blank lines touching the
/// root braces. Pin its whole output so a library upgrade may change those two
/// sites, but may not quietly start eating the nearby comments or other trivia.
#[test]
fn write_app_only_normalizes_blank_lines_touching_the_root_braces() {
    let text = r#"{

  // The leading comment must survive.
  "name": "demo-app",
  "control": "https://control.zeroship.ai",
  "runtime_date": "2026-08-14",

  // The interior group must survive.
  "build": { "mode": "full", "dist": "dist", "output": "dist/app.zship" },
  "migrations": { "dir": "migrations", "out": "generated/zeroship" },
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
  "migrations": { "dir": "migrations", "out": "generated/zeroship" },
  "secrets": [],
  "app": "66666666-6666-4666-8666-666666666666"
}
"#;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("root-blank-lines.jsonc");
    std::fs::write(&path, text).unwrap();

    ProjectConfig::load(&path)
        .unwrap()
        .write_app("66666666-6666-4666-8666-666666666666")
        .unwrap();

    assert_eq!(std::fs::read_to_string(&path).unwrap(), expected);
}

/// The generated CST is validated as a complete project config before the
/// original path is touched. Construct an intentionally inconsistent internal
/// value to make that otherwise defensive failure arm observable.
#[test]
fn write_app_reparses_before_writing() {
    let text = "{\n  \"name\": \"demo-app\"\n}\n";
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("invalid-project.jsonc");
    std::fs::write(&path, text).unwrap();
    let config = ProjectConfig {
        path: path.clone(),
        text: text.to_string(),
        root: Map::new(),
    };

    let error = config
        .write_app("77777777-7777-4777-8777-777777777777")
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
fn write_app_uses_original_byte_span_with_crlf_unicode_and_escaped_key() {
    let text = FULL
        .replacen(
            "// A comment, which is the whole reason the format is JSONC.",
            "// caf\u{e9}-\u{1f600}",
            1,
        )
        .replacen("  \"app\":", "  \"\\u0061pp\":", 1)
        .replace('\n', "\r\n");
    let dir = std::env::temp_dir().join(format!("zs-pc-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("splice-original-span.jsonc");
    std::fs::write(&path, &text).unwrap();

    let config = ProjectConfig::load(&path).unwrap();
    config
        .write_app("55555555-5555-4555-8555-555555555555")
        .unwrap();

    let after = std::fs::read_to_string(&path).unwrap();
    let expected = text.replace(
        "\"11111111-1111-4111-8111-111111111111\"",
        "\"55555555-5555-4555-8555-555555555555\"",
    );
    assert_eq!(after, expected, "only the original app value span may change");
    std::fs::remove_file(&path).ok();
}
