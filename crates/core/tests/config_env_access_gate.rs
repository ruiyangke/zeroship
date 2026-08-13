//! The workspace gate: no first-party raw environment read outside two files.
//!
//! Section 4.5 of `docs/proposals/2026-08-11-config-name-alignment.md` asks for
//! two independent lines. Clippy's `disallowed_methods` deny is the first and
//! runs in the compiler; this is the second, and it exists because the first
//! only sees code the current cfg activates. A read behind a disabled feature
//! compiles out, the lint says nothing, and a different feature selection ships
//! it. This parses tracked SOURCE, so a disabled cfg is still source.
//!
//! Enumeration is `git ls-files`, following the repository's existing
//! source-gate pattern in `crates/core/tests/source_is_greppable_test.rs`. The
//! scanner refuses an empty file set, so a moved directory fails loudly instead
//! of reporting zero violations.
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
const TRACKED_BLOCKERS: &[(&str, &str)] = &[(
    "crates/core/src/config/secrets.rs",
    "SecretRef::Env resolves a variable NAME that arrives at run time inside a \
     secret reference, so it cannot become a typed key. Step 5 of the config \
     name-alignment proposal deletes the env-to-env reference arm entirely; \
     that is the exit, and the proposal states this read is not allowlisted in \
     the final gate.",
)];

/// Fixture and vendored trees that exist in order to CONTAIN violations.
///
/// `tests/fixtures` and `tests/ui` under the contract crate hold deliberate
/// bad code that the scanner's own tests feed to it. Scanning them would make
/// this gate fail on the files that prove the gate works.
const FIXTURE_PREFIXES: &[&str] = &[
    "crates/config-contract/tests/fixtures/",
    "crates/config-contract/tests/ui/",
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
         library test modules:\n{}\n\nConvert each to a typed key. See \
         crates/core/src/config/declared.rs.",
        violations
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n")
    );
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
}

#[test]
fn the_gate_rejects_an_illicit_lint_suppression() {
    // Mutation, applied in-memory so the run and the mutation cannot come
    // apart: an ordinary file silences the very lint that is the compiler half
    // of this gate. If only the raw call were checked, a file could silence
    // Clippy and satisfy the source scan by aliasing, so both halves would be
    // defeated by one edit.
    // Does not cover: a suppression at the CRATE level in a Cargo.toml lint
    // table. That is what crates/config-contract/tests/workspace_lints.rs
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
