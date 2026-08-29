//! The workspace gate: no first-party raw environment read outside two files,
//! and no environment WRITE anywhere at all.
//!
//! The write half has no exempt file. `std::env::set_var` / `remove_var`
//! mutate state every thread in the process shares, which is why Rust 2024
//! made them `unsafe`, and nothing here needs to: a test states its
//! configuration through typed values, and a test that needs a variable to
//! reach a BINARY gives it to a child process through `Command::env`. That
//! second form is deliberately untouched by both halves of this gate.
//!
//! Section 4.5 of `docs/proposals/2026-08-11-config-name-alignment.md` asks for
//! two independent lines. Clippy's `disallowed_methods` deny is the first and
//! runs in the compiler; this is the second, and it exists because the first
//! only sees code the current cfg activates. A read behind a disabled feature
//! compiles out, the lint says nothing, and a different feature selection ships
//! it. This parses tracked SOURCE, so a disabled cfg is still source.
//!
//! Enumeration is `git ls-files`, following the repository's existing
//! source-gate pattern in `crates/zeroship-core/tests/source_is_greppable_test.rs`.
//! The scanner refuses an empty file set, so a moved directory fails loudly instead
//! of reporting zero violations.
//!
//! That claim held for the ENUMERATION and not for the path constants beside it. Both
//! `CENTRAL_ACCESSOR` and `FIXTURE_PREFIXES` kept pre-rename `crates/core` /
//! `crates/config-contract` spellings after the crates became `zeroship-*`, and neither
//! failed loudly: one silently reclassified the exempt file as ordinary, the other
//! silently stopped filtering. `every_fixture_prefix_still_matches_a_tracked_file` and
//! the read in `central_accessor_functions_carry_the_disallowed_methods_allow` now make
//! a path that names nothing a failure.
//!
//! WHAT THIS CANNOT SEE: reads inside a dependency, a read produced by a macro
//! this scanner cannot expand, and a name assembled at run time. The first is
//! deliberate - vendored code is not ours to rewrite. The second is why macro
//! BODIES are walked rather than skipped. The third is why the typed accessors
//! take a key and not a `&str`, and it is also why the one remaining blocker
//! below cannot simply be converted.

use std::collections::BTreeSet;
use std::path::Path;

use zeroship_config_contract::inventory::collect_tracked_rust_sources;
use zeroship_config_contract::raw_env::{scan_sources_by_role, RawEnvViolation};

/// Files that still contain a raw read, with the reason each is not converted.
///
/// This is NOT an allowlist. Every entry is a tracked blocker with a named exit,
/// and `the_blocker_list_cannot_go_stale` fails if an entry stops violating -
/// so the list can only shrink, and a fixed blocker that nobody deleted is a
/// test failure rather than a permanent exemption.
const TRACKED_BLOCKERS: &[(&str, &str)] = &[];

/// Fixture and vendored trees that exist in order to CONTAIN violations.
///
/// `tests/fixtures` and `tests/ui` under the contract crate hold deliberate
/// bad code that the scanner's own tests feed to it. Scanning them would make
/// this gate fail on the files that prove the gate works.
///
/// These said `crates/config-contract/...` until 2026-08-28, from before the crate was
/// renamed to `zeroship-config-contract`. A prefix that matches nothing filters nothing,
/// so the deliberately-bad fixtures were scanned as ordinary first-party source and the
/// gate failed on the two files that exist to prove it works. That is the same stale-path
/// failure as `CENTRAL_ACCESSOR`, and it is why `fixture_prefixes_still_match_a_fixture`
/// below asserts these resolve rather than trusting them to.
const FIXTURE_PREFIXES: &[&str] = &[
    "crates/zeroship-config-contract/tests/fixtures/",
    "crates/zeroship-config-contract/tests/ui/",
];

fn workspace_root() -> &'static Path {
    // CARGO_MANIFEST_DIR is crates/core; the workspace root is two levels up.
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/core has a workspace root above it")
}

fn scan() -> (usize, Vec<RawEnvViolation>) {
    let root = workspace_root();
    let sources = collect_tracked_rust_sources(root)
        .unwrap_or_else(|errors| panic!("could not enumerate tracked Rust source: {errors:?}"));
    let filtered = sources
        .into_iter()
        .filter(|(path, _)| !FIXTURE_PREFIXES.iter().any(|prefix| path.starts_with(prefix)))
        .collect::<Vec<_>>();
    let count = filtered.len();
    match scan_sources_by_role(&filtered) {
        Ok(_) => (count, Vec::new()),
        Err(violations) => (count, violations),
    }
}

fn violating_files(violations: &[RawEnvViolation]) -> BTreeSet<String> {
    violations
        .iter()
        .filter_map(|violation| {
            // Every prefixed violation prints as "<kind>: <path>: <detail>".
            let text = violation.to_string();
            let after_kind = text.split_once(": ")?.1;
            Some(after_kind.split(':').next()?.to_owned())
        })
        .collect()
}

#[test]
fn no_tracked_first_party_source_reads_the_environment_raw() {
    let (files, violations) = scan();

    // The scan must have examined a realistic tree. A gate that walked an empty
    // or tiny set would pass for the wrong reason, and the failure mode is
    // invisible: zero violations reads exactly like a clean workspace.
    assert!(
        files > 500,
        "only {files} tracked Rust files were scanned; the enumeration is broken"
    );

    let offenders = violating_files(&violations);
    let blocked = TRACKED_BLOCKERS
        .iter()
        .map(|(path, _)| (*path).to_owned())
        .collect::<BTreeSet<_>>();
    let unexpected = offenders.difference(&blocked).collect::<Vec<_>>();

    assert!(
        unexpected.is_empty(),
        "raw environment access outside the central accessor and the sealed \
         library test modules:\n{}\n\nConvert a READ to a typed key (see \
         crates/zeroship-core/src/config/declared.rs). A WRITE has no typed form and no \
         exempt file: state the value as typed configuration, or give it to a \
         child process with Command::env.",
        violations
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n")
    );
}

/// Every fixture prefix must still match a tracked file.
///
/// The one-variable partner to the filter itself. `FIXTURE_PREFIXES` is used with
/// `starts_with`, and a prefix that matches nothing is indistinguishable from a prefix
/// that matches only clean files: both remove zero entries and neither says anything.
/// That is exactly how the pre-rename `crates/config-contract/...` spellings survived -
/// they kept filtering, they just filtered an empty set, and the gate went red on the
/// fixtures instead of on real source.
///
/// Asserting the prefixes RESOLVE, rather than asserting a count, is what keeps this
/// honest across a fixture being added or removed.
#[test]
fn every_fixture_prefix_still_matches_a_tracked_file() {
    let sources = collect_tracked_rust_sources(workspace_root())
        .unwrap_or_else(|errors| panic!("could not enumerate tracked Rust source: {errors:?}"));

    for prefix in FIXTURE_PREFIXES {
        assert!(
            sources.iter().any(|(path, _)| path.starts_with(prefix)),
            "no tracked file starts with {prefix:?}, so this prefix filters nothing. \
             A directory was renamed and this constant was not; fix the path rather \
             than deleting the entry, or the fixtures it covers become violations."
        );
    }
}

#[test]
fn the_blocker_list_cannot_go_stale() {
    // The one-variable partner to the test above: that one asks "is anything
    // NEW violating", this one asks "is anything LISTED no longer violating".
    // Without it a blocker that Step 5 fixes would sit here forever as a
    // permanent exemption wearing the word "blocker".
    let (_files, violations) = scan();
    let offenders = violating_files(&violations);

    for (path, reason) in TRACKED_BLOCKERS {
        assert!(
            offenders.contains(*path),
            "{path} is listed as a tracked blocker but no longer has a raw \
             read. Delete its entry from TRACKED_BLOCKERS. Its recorded reason \
             was: {reason}"
        );
    }

    // The list is EMPTY, so the loop above asserts nothing and would keep
    // passing forever. State the claim the list existed to defer instead: both
    // entries named the env-to-env secret indirection - `SecretRef::Env` and
    // the billing provider's `env:<NAME>` handle - and both facilities are
    // gone, so the gate is now unconditional rather than conditional.
    assert!(
        TRACKED_BLOCKERS.is_empty() && offenders.is_empty(),
        "the exemption list is empty, so no tracked first-party file may hold a \
         raw environment read: {offenders:?}"
    );
}

#[test]
fn the_gate_rejects_an_illicit_lint_suppression() {
    // Mutation, applied in-memory so the run and the mutation cannot come
    // apart: an ordinary file silences the very lint that is the compiler half
    // of this gate. If only the raw call were checked, a file could silence
    // Clippy and satisfy the source scan by aliasing, so both halves would be
    // defeated by one edit.
    // Does not cover: a suppression at the CRATE level in a Cargo.toml lint
    // table. That is what crates/zeroship-config-contract/tests/workspace_lints.rs
    // exists for.
    let source = r"
#[allow(clippy::disallowed_methods)]
fn sneaky() -> Option<String> {
    None
}
";
    let errors = zeroship_config_contract::raw_env::check_rust_source(source)
        .expect_err("an illicit allow must fail");
    assert!(
        errors
            .iter()
            .any(|error| matches!(error, RawEnvViolation::IllicitAllow(_))),
        "expected an IllicitAllow, got {errors:?}"
    );

    // The one-variable partner: the same attribute in the sealed library test
    // role, where it is exactly what the carve-out permits.
    let sealed = r#"
pub enum TestEnvKey {
    Only,
}

impl TestEnvKey {
    const fn name(self) -> &'static str {
        match self {
            Self::Only => "PG_TEST_URL",
        }
    }
}

#[allow(clippy::disallowed_methods)]
pub fn get(key: TestEnvKey) -> Option<String> {
    std::env::var(key.name()).ok()
}
"#;
    zeroship_config_contract::raw_env::check_rust_source_with_role(
        sealed,
        zeroship_config_contract::raw_env::FileRole::SealedLibraryTest,
    )
    .expect("the sealed accessor shape is permitted");
}

/// Whether the line directly above `fn_signature` is the exact
/// `#[allow(clippy::disallowed_methods)]` attribute.
///
/// Line-adjacency, not a syn attribute walk: the three functions this checks
/// are named and their shape (one doc comment, one attribute, then the `fn`
/// line) is fixed by convention in `crates/zeroship-core/src/config/env.rs` itself, so
/// a textual check is exact and does not need a second parser dependency.
fn fn_immediately_preceded_by_allow(source: &str, fn_signature: &str) -> bool {
    let lines: Vec<&str> = source.lines().collect();
    let Some(fn_line) = lines.iter().position(|line| line.contains(fn_signature)) else {
        panic!("signature {fn_signature:?} not found in source; the check target moved");
    };
    fn_line > 0 && lines[fn_line - 1].trim() == "#[allow(clippy::disallowed_methods)]"
}

/// The regression this guard exists for: `ba6edcd88` made
/// `crates/zeroship-core/src/config/env.rs` the sole raw-environment boundary and
/// documented it as clippy-exempt, but never added the attribute clippy
/// itself requires to grant that exemption. `35bd1598d`, the commit that
/// landed the `disallowed_methods` deny nine minutes later, added the
/// matching allow to every `libs/*/tests/common/env.rs` sealed accessor but
/// never touched this file - so main's `cargo clippy -p zeroship-core
/// --all-targets` was red from that commit forward and nothing local caught
/// it: `cargo test` never runs clippy, and the source gate above only
/// forbids the attribute OUTSIDE this file, never requires it INSIDE.
///
/// This closes that gap without shelling out to `cargo clippy` (slow, and a
/// second copy of the compiler's own check): it asserts the textual shape
/// that makes the suppression effective, on the three functions that are the
/// only place `std::env::var`, `var_os` and `vars` may legally appear.
#[test]
fn central_accessor_functions_carry_the_disallowed_methods_allow() {
    let path =
        workspace_root().join(zeroship_config_contract::raw_env::CENTRAL_ACCESSOR);
    let source = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));

    for fn_signature in [
        "fn raw_var(key: &str) -> Result<Option<String>, ()> {",
        "fn raw_var_os(key: &str) -> Option<OsString> {",
        "fn raw_vars() -> Vec<(String, String)> {",
    ] {
        assert!(
            fn_immediately_preceded_by_allow(&source, fn_signature),
            "{} is missing `#[allow(clippy::disallowed_methods)]` on the line \
             directly above `{fn_signature}` - this is the exempted raw-env \
             boundary and clippy denies the call without it",
            path.display()
        );
    }
}

/// The one-variable partner: prove the detector says NO for the exact shape
/// that caused the regression (attribute absent), not just YES for the
/// current file. Without this, `fn_immediately_preceded_by_allow` could
/// return `true` unconditionally and the test above would still pass.
#[test]
fn the_allow_detector_rejects_a_missing_attribute() {
    let with_allow = "/// doc\n#[allow(clippy::disallowed_methods)]\npub(crate) fn raw_var(key: &str) -> Result<Option<String>, ()> {\n";
    let without_allow = "/// doc\npub(crate) fn raw_var(key: &str) -> Result<Option<String>, ()> {\n";
    let signature = "fn raw_var(key: &str) -> Result<Option<String>, ()> {";

    assert!(fn_immediately_preceded_by_allow(with_allow, signature));
    assert!(!fn_immediately_preceded_by_allow(without_allow, signature));
}
