//! Compile-time gate on `deploy/policies/`.
//!
//! It parse-checks every `.cedar` file, parses `zeroship.cedarschema`, and runs
//! the SAME `ValidationMode::Strict` validation the crate runs at boot.
//!
//! The third of those exists because wiring a schema into
//! `engine::load_platform_policies` introduced a new way to fail: every service
//! that authorizes anything calls that function on startup, so a malformed
//! schema, a typo'd action id or an undeclared context key would have become a
//! fleet-wide boot failure discovered at deploy time. Here the same defect is a
//! compile error on the machine that wrote it.
//!
//! WHAT THIS IS NOT. Validation is per FILE, so it cannot rule on whether a
//! file is in `PLATFORM_POLICY_SOURCES` - a policy that is valid and loaded by
//! nobody still authorizes nothing. `engine`'s
//! `every_policy_file_on_disk_is_wired_into_the_loaded_set` and
//! `tests/organization_policy_ladder_gate.sh` rule on that.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use cedar_policy::{PolicySet, Schema, ValidationMode, Validator};

const POLICY_DIR: &str = "../../deploy/policies";
const SCHEMA_FILE: &str = "../../deploy/policies/zeroship.cedarschema";

fn main() {
    println!("cargo:rerun-if-changed={POLICY_DIR}");

    let schema_source = std::fs::read_to_string(SCHEMA_FILE)
        .unwrap_or_else(|err| panic!("read {SCHEMA_FILE}: {err}"));
    let schema = match Schema::from_str(&schema_source) {
        Ok(schema) => schema,
        Err(err) => panic!("schema parse error in {SCHEMA_FILE}: {err}"),
    };

    let mut policy_files = Vec::new();
    walk_dir_for_cedar_files(Path::new(POLICY_DIR), &mut policy_files);
    policy_files.sort();

    assert!(
        !policy_files.is_empty(),
        "no .cedar files under {POLICY_DIR}: the walk found the wrong directory, \
         so a clean result here would be a statement about nothing"
    );

    for path in policy_files {
        let source = std::fs::read_to_string(&path).expect("read .cedar file");
        let policies = match PolicySet::from_str(&source) {
            Ok(policies) => policies,
            Err(err) => panic!("policy parse error in {}: {err}", path.display()),
        };

        // Warnings are refused alongside errors. The warning that matters here
        // is "policy is impossible": a band that can never fire is the exact
        // failure this whole layer exists to surface, and it is silent at
        // runtime because an allow-list nothing matches and an allow-list that
        // is absent produce the identical denial.
        let result = Validator::new(schema.clone()).validate(&policies, ValidationMode::Strict);
        if !result.validation_passed_without_warnings() {
            let mut report = format!("policy validation failed in {}:\n", path.display());
            for error in result.validation_errors() {
                writeln!(report, "  error: {error}").expect("writing to String is infallible");
            }
            for warning in result.validation_warnings() {
                writeln!(report, "  warning: {warning}").expect("writing to String is infallible");
            }
            panic!("{report}");
        }
    }
}

fn walk_dir_for_cedar_files(dir: &Path, policy_files: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("read policies directory") {
        let entry = entry.expect("read policies directory entry");
        let path = entry.path();

        if path.is_dir() {
            walk_dir_for_cedar_files(&path, policy_files);
        } else if path
            .extension()
            .is_some_and(|extension| extension == "cedar")
        {
            policy_files.push(path);
        }
    }
}
