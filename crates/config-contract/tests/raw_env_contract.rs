use std::fs;
use std::path::Path;

use zeroship_config_contract::raw_env::{RawEnvViolation, check_rust_source};

#[test]
fn cfg_disabled_aliased_raw_read_is_rejected() {
    // Mutation: import std::env::var under another name behind a cfg that is
    // false in this build, then pass it a dynamic key like metering's helper.
    // Does not cover: compiler-resolved aliases across modules or environment
    // FFI beyond the explicit source patterns; Step 4's Clippy gate does that.
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/raw_read_alias.rs");
    let source = fs::read_to_string(&fixture).expect("read raw-read fixture");
    let errors = check_rust_source(&source).expect_err("raw read must fail");

    assert!(errors.iter().any(|error| matches!(
        error,
        RawEnvViolation::Import(path) if path.contains("std::env::var")
    )));
}

#[test]
fn dynamic_helper_argument_does_not_hide_a_raw_read() {
    // Mutation: the environment name is supplied through a parameter, so a
    // literal-name grep would find nothing.
    // Does not cover: whether the caller's key has a ConfigSpec; this fallback
    // bans the raw method, while the compiled registry proves declarations.
    let source = r#"
fn env_nonempty(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|value| !value.is_empty())
}
"#;
    let errors = check_rust_source(source).expect_err("helper raw read must fail");

    assert!(errors.iter().any(|error| matches!(
        error,
        RawEnvViolation::Read(path) if path == "std::env::var"
    )));
}

#[test]
fn typed_accessor_source_is_not_a_raw_read() {
    let source = r#"
fn load() {
    let _ = read_config_env!(DATABASE_KEY, ControlToken::new());
}
"#;

    check_rust_source(source).expect("typed accessor is allowed");
}
