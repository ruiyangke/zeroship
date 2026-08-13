use std::fs;
use std::path::Path;

use zeroship_config_contract::raw_env::{check_rust_source, check_sources, RawEnvViolation};

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
        RawEnvViolation::Read(path) if path.ends_with("std::env::var")
    )));
}

#[test]
fn typed_accessor_source_is_not_a_raw_read() {
    // Positive control for the two fixtures above: the sanctioned spelling of
    // the same operation, differing only in going through the registering
    // macro. Without it, a scanner that rejected everything would still look
    // like a working gate.
    // Does not cover: whether the named key exists; that is a registry check.
    let source = r#"
fn load() {
    let _ = read_config_env!(DATABASE_KEY, ControlToken::new());
}
"#;

    check_rust_source(source).expect("typed accessor is allowed");
}

#[test]
fn calling_the_typed_accessor_directly_bypasses_registration() {
    // Mutation: hold a legitimate typed key but call the accessor the macro
    // wraps, so no ReadSite is emitted and the read is invisible to the
    // registry. This is the only way to read a declared key untracked, which is
    // why "every read is enumerable" needs it banned rather than assumed.
    // Does not cover: reads inside the accessor's own defining module, which
    // the eventual tracked-file gate must exempt by path.
    let source = r#"
fn load() {
    let _ = zeroship_core::config::read_typed_env(KEY, TOKEN);
}
"#;
    let errors = check_rust_source(source).expect_err("unregistered read must fail");

    assert!(errors.iter().any(|error| matches!(
        error,
        RawEnvViolation::UnregisteredRead(path) if path.ends_with("read_typed_env")
    )));
}

#[test]
fn a_scan_that_examined_nothing_is_a_failure() {
    // Mutation: the file enumeration feeding the scanner returns nothing, which
    // is exactly what a moved directory or a broken glob produces. Per-file
    // checking is silently clean in that case; the set-level entry point is not.
    // Does not cover: an enumeration that finds SOME files but misses the ones
    // that matter. Only the zero case is distinguishable here.
    let errors = check_sources(&[]).expect_err("an empty scan must fail");

    assert_eq!(errors, vec![RawEnvViolation::EmptyScan]);

    let scanned = check_sources(&[("clean.rs".to_owned(), "fn main() {}".to_owned())])
        .expect("a non-empty clean scan passes");
    assert_eq!(scanned, 1);
}
