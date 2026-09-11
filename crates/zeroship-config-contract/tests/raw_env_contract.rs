use std::fs;
use std::path::Path;

use zeroship_config_contract::raw_env::{
    check_rust_source, check_rust_source_with_role, check_sources, FileRole, RawEnvViolation,
};

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

#[test]
fn a_production_library_read_fails_even_in_libs() {
    // Section 4.5 permits a sealed test-key accessor in `libs/*`, and nothing
    // else there. A published, zeroship-independent driver takes resolved
    // options from its caller; if the carve-out leaked into `src/`, the rule
    // "platform processes declare and read external credentials before
    // injecting them into a library" would have no teeth.
    // Does not cover: whether the library's public API actually accepts the
    // resolved option. That is a design property, not a scannable one.
    let source = r#"
pub fn credentials() -> Option<String> {
    std::env::var("AWS_SECRET_ACCESS_KEY").ok()
}
"#;
    let role = FileRole::for_path("libs/compio-s3/src/lib.rs");
    assert_eq!(role, FileRole::Ordinary, "libs/src is not a carve-out path");

    let errors = check_rust_source_with_role(source, role)
        .expect_err("a production library read must fail");
    assert!(errors
        .iter()
        .any(|error| matches!(error, RawEnvViolation::Read(_))));
}

#[test]
fn a_library_test_read_outside_the_sealed_module_fails() {
    // The other half: the carve-out is one FILE, not the `tests/` tree. A test
    // that reads raw beside the sealed module would make the sealed module
    // decorative.
    // Does not cover: a test that calls the sealed accessor with a key the enum
    // does not have; that does not compile, so there is nothing to scan.
    let source = r#"
fn url() -> Option<String> {
    std::env::var("PG_TEST_URL").ok()
}
"#;
    let role = FileRole::for_path("libs/compio-postgres/tests/suite/integration.rs");
    assert_eq!(role, FileRole::Ordinary, "only tests/common/env.rs is sealed");

    let errors = check_rust_source_with_role(source, role)
        .expect_err("a raw read outside the sealed module must fail");
    assert!(errors
        .iter()
        .any(|error| matches!(error, RawEnvViolation::Read(_))));

    // The one-variable partner: byte-identical body, sealed path, plus the
    // shape the carve-out requires. If the path check were ignored, these two
    // would agree.
    let sealed = r#"
pub enum TestEnvKey {
    PgTestUrl,
}

impl TestEnvKey {
    const fn name(self) -> &'static str {
        match self {
            Self::PgTestUrl => "PG_TEST_URL",
        }
    }
}

#[allow(clippy::disallowed_methods)]
pub fn get(key: TestEnvKey) -> Option<String> {
    std::env::var(key.name()).ok()
}
"#;
    let sealed_role = FileRole::for_path("libs/compio-postgres/tests/common/env.rs");
    assert_eq!(sealed_role, FileRole::SealedLibraryTest);
    check_rust_source_with_role(sealed, sealed_role).expect("the sealed shape passes");
}

#[test]
fn the_sealed_path_alone_does_not_seal_anything() {
    // Mutation: keep the blessed PATH and drop each shape property in turn. A
    // path-only exemption would mean "raw reads are fine if you name the file
    // env.rs", which is a rename away from no gate at all.
    // Does not cover: an enum whose `name` arms compute their strings. The set
    // stays finite but stops being greppable; that gap is stated in
    // check_sealed_shape's own documentation.
    let path = "libs/compio-redis/tests/common/env.rs";
    let role = FileRole::for_path(path);

    let two_doors = r#"
pub enum TestEnvKey {
    A,
}

impl TestEnvKey {
    const fn name(self) -> &'static str {
        match self {
            Self::A => "REDIS_TEST_URL",
        }
    }
}

pub fn get(key: TestEnvKey) -> Option<String> {
    std::env::var(key.name()).ok()
}

pub fn other(key: TestEnvKey) -> Option<std::ffi::OsString> {
    std::env::var_os(key.name())
}
"#;
    let errors = check_rust_source_with_role(two_doors, role)
        .expect_err("two raw accesses are not a sealed accessor");
    assert!(errors
        .iter()
        .any(|error| matches!(error, RawEnvViolation::SealedShape(detail)
            if detail.contains("raw accesses"))));

    let literal_argument = r#"
pub enum TestEnvKey {
    A,
}

pub fn get(_key: TestEnvKey) -> Option<String> {
    std::env::var("REDIS_TEST_URL").ok()
}
"#;
    let errors = check_rust_source_with_role(literal_argument, role)
        .expect_err("a literal argument bypasses the key");
    assert!(errors
        .iter()
        .any(|error| matches!(error, RawEnvViolation::SealedShape(detail)
            if detail.contains("string literal"))));

    let no_enum = r#"
pub fn get(name: &str) -> Option<String> {
    std::env::var(name).ok()
}
"#;
    let errors = check_rust_source_with_role(no_enum, role)
        .expect_err("no key enum means no closed set");
    assert!(errors
        .iter()
        .any(|error| matches!(error, RawEnvViolation::SealedShape(detail)
            if detail.contains("closed set"))));
}

#[test]
fn a_build_script_may_read_only_the_named_build_inputs() {
    // Section 4.5 exempts compiler and Cargo inputs explicitly: they exist only
    // while the crate compiles, no operator sets them, and there is no process
    // for a consumer marker to name.
    // Does not cover: a build script that reads a build input through a helper
    // taking `&str`. The literal is what the exemption is keyed on, so that
    // shape fails - correctly, but with a message about the read rather than
    // about the missing literal.
    let allowed = r#"
fn main() {
    let out = std::env::var("OUT_DIR").unwrap();
    println!("cargo:rerun-if-changed={out}");
}
"#;
    let role = FileRole::for_path("crates/zeroship-authz/build.rs");
    assert_eq!(role, FileRole::BuildScript);
    let report = check_rust_source_with_role(allowed, role).expect("OUT_DIR is a build input");
    assert_eq!(report.permitted_raw.len(), 1);

    // The one-variable partner: same file, same role, a name that is process
    // configuration rather than a build input.
    let denied = r#"
fn main() {
    let _ = std::env::var("ZEROSHIP_CONTROL_KEY");
}
"#;
    let errors = check_rust_source_with_role(denied, role)
        .expect_err("a build script may not read process configuration");
    assert!(errors
        .iter()
        .any(|error| matches!(error, RawEnvViolation::Read(_))));
}

#[test]
fn a_raw_read_inside_a_macro_body_is_found() {
    // Regression for a REAL blind spot found on 2026-08-12: syn's default walk
    // stops at a macro's token stream, so
    // `assert!(std::env::var(NAME).is_err())` in
    // crates/zeroship-storage/src/limits.rs was invisible, and the file's
    // other read made the omission look like a correct count.
    // Does not cover: an ALIASED read inside a macro body that does not parse
    // as an expression list. The text fallback matches literal spellings only.
    let source = r#"
fn check() {
    assert!(std::env::var("SOMETHING").is_err());
}
"#;
    let errors = check_rust_source(source).expect_err("a read inside assert! must fail");
    assert!(errors
        .iter()
        .any(|error| matches!(error, RawEnvViolation::Read(path)
            if path.ends_with("std::env::var"))));

    // The one-variable partner: a macro body that MENTIONS the spelling in a
    // string literal without performing a read. Flagging this made the scanner
    // fail on its own test file, so the distinction is load-bearing.
    let mention = r#"
fn describe(path: &str) -> bool {
    matches!(path, p if p.ends_with("std::env::var"))
}
"#;
    check_rust_source(mention).expect("mentioning the spelling is not reading");
}

// ---------------------------------------------------------------------------
// The WRITE half. A mutation of the process environment is permitted in no
// role, which is the one way these differ from the read cases above.
// ---------------------------------------------------------------------------

#[test]
fn a_direct_environment_mutation_is_rejected() {
    // Mutation: the plainest spelling of the thing the gate exists to stop.
    // Does not cover: a mutation inside a dependency, or one produced by a
    // macro this scanner cannot expand.
    let source = r#"
fn plant() {
    std::env::set_var("ZEROSHIP_DEV", "1");
    std::env::remove_var("ZEROSHIP_DEV");
}
"#;
    let errors = check_rust_source(source).expect_err("an environment write must fail");

    assert!(
        errors.iter().any(|error| matches!(
            error,
            RawEnvViolation::Write(detail) if detail.ends_with("std::env::set_var")
        )),
        "set_var must be reported: {errors:?}"
    );
    assert!(
        errors.iter().any(|error| matches!(
            error,
            RawEnvViolation::Write(detail) if detail.ends_with("std::env::remove_var")
        )),
        "remove_var must be reported: {errors:?}"
    );
}

#[test]
fn giving_a_child_process_an_environment_is_not_a_mutation() {
    // THE ONE-VARIABLE CONTROL for the case above, and the distinction the
    // whole gate turns on. This code sets the same variable to the same value
    // for a process; it differs only in WHOSE environment it touches. A gate
    // that flagged this would be bound to "the code mentions an environment
    // variable" rather than to "the code mutates THIS process".
    // Does not cover: a child spawned through a shell string, where the
    // assignment is inside an opaque argument.
    let source = r#"
fn spawn() {
    let mut command = std::process::Command::new("zeroship-gate");
    command.env("ZEROSHIP_DEV", "1");
    command.env_remove("ZEROSHIP_DEV");
    command.env_clear();
}
"#;

    check_rust_source(source).expect("Command::env is an argument, not a mutation");
}

#[test]
fn an_unrelated_set_var_method_is_not_a_mutation() {
    // The second control, and the one a naive text search fails. The control
    // plane owns `EnvStore::set_var`, which writes a creator app's variables
    // to PostgreSQL and has nothing to do with the process environment. It is
    // a METHOD, so it is unreachable from the path-based write check; asserting
    // that here is what stops a later "just grep for set_var" simplification.
    let source = r#"
async fn store_one(store: &EnvStore) {
    store.set_var(app, "FOO", "bar").await.unwrap();
    let cleared = store.remove_var(app, "FOO").await;
}
"#;

    check_rust_source(source).expect("a method named set_var is not std::env::set_var");
}

#[test]
fn an_aliased_environment_mutation_is_rejected() {
    // Mutation: the two evasions a literal `std::env::set_var` search misses -
    // importing the function under another name, and aliasing the module. Both
    // are behind a cfg that is false in this build, which is the whole reason
    // this source gate exists beside the Clippy one.
    let aliased_function = r#"
#[cfg(feature = "never-enabled")]
use std::env::set_var as plant;

#[cfg(feature = "never-enabled")]
fn go() {
    plant("ZEROSHIP_DEV", "1");
}
"#;
    let errors =
        check_rust_source(aliased_function).expect_err("an aliased write must fail");
    assert!(
        errors
            .iter()
            .any(|error| matches!(error, RawEnvViolation::Write(_))),
        "an aliased import must be reported as a write: {errors:?}"
    );

    let aliased_module = r#"
use std::env as sys;

fn go() {
    sys::set_var("ZEROSHIP_DEV", "1");
}
"#;
    let errors = check_rust_source(aliased_module).expect_err("a module-aliased write must fail");
    assert!(
        errors.iter().any(|error| matches!(
            error,
            RawEnvViolation::Write(detail) if detail.ends_with("sys::set_var")
        )),
        "a module alias must be reported as a write: {errors:?}"
    );
}

#[test]
fn no_role_is_exempt_from_the_write_rule() {
    // The read rule has two exempt roles; the write rule has none, and this is
    // the assertion that keeps them apart. `crates/zeroship-core/src/config/env.rs` is
    // the one file permitted to READ raw, and it still may not write - it has
    // no reason to, and an exemption there would be the one place a mutation
    // could hide from both halves of the gate.
    let source = r#"
fn raw_set(key: &str, value: &str) {
    std::env::set_var(key, value);
}
"#;
    for role in [
        FileRole::Ordinary,
        FileRole::CentralAccessor,
        FileRole::SealedLibraryTest,
        FileRole::BuildScript,
    ] {
        let errors = check_rust_source_with_role(source, role)
            .err()
            .unwrap_or_else(|| panic!("{role:?} must not permit an environment write"));
        assert!(
            errors
                .iter()
                .any(|error| matches!(error, RawEnvViolation::Write(_))),
            "{role:?} reported no Write violation: {errors:?}"
        );
    }
}
