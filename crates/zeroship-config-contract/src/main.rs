use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use zeroship_core::config::ConfigSpec;

use zeroship_config_contract::audit::{compare, from_inventory, from_specs};
use zeroship_config_contract::contract::validate_contract;
use zeroship_config_contract::docs;
use zeroship_config_contract::inventory::{
    collect_rust_sources, collect_tracked_rust_sources, format_tsv, scan_sources, InventoryRow,
    OverlayLeaves,
};
use zeroship_config_contract::metadata::check_workspace;
use zeroship_config_contract::raw_env::{collect_declared_keys, scan_sources_by_role, RawEnvViolation};
use zeroship_config_contract::registry::{platform_read_sites, platform_specs, DECLARING_BINARIES};

const USAGE: &str = "usage: zeroship-config-contract \
[check-metadata [path/to/Cargo.toml] | inventory [--format tsv] [--root DIR] \
| raw-env [--root DIR] | audit [--root DIR] | contract | env-vars-doc [--root DIR] [--check]]";

/// The generated half of the environment reference.
const ENV_VARS_DOC: &str = "docs/reference/env-vars.md";

fn main() {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    match args.first().map(String::as_str) {
        None | Some("check-metadata") => check_metadata(args.get(1).map(PathBuf::from)),
        Some("inventory") => inventory(&args[1..]),
        Some("raw-env") => raw_env(&args[1..]),
        Some("audit") => audit(&args[1..]),
        Some("contract") => contract(&args[1..]),
        Some("env-vars-doc") => env_vars_doc(&args[1..]),
        Some(other) => {
            eprintln!("{USAGE}; got {other:?}");
            std::process::exit(2);
        }
    }
}

/// Parse the `--root DIR` and `--check` options shared by the new subcommands.
fn parse_root_and_check(args: &[String], allow_check: bool) -> (PathBuf, bool) {
    let mut root = PathBuf::from(".");
    let mut check = false;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--root" => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("{USAGE}");
                    std::process::exit(2);
                };
                root = PathBuf::from(value);
                index += 2;
            }
            "--check" if allow_check => {
                check = true;
                index += 1;
            }
            other => {
                eprintln!("{USAGE}; got {other:?}");
                std::process::exit(2);
            }
        }
    }
    (root, check)
}

/// Require every declared overlay path to be a leaf the TOML schema accepts.
///
/// `FileConfig` is `deny_unknown_fields`, so a canonical overlay path with no
/// matching field is not merely undocumented - the whole file fails to load and
/// the tier the contract advertises cannot be used at all. Nothing compared the
/// two until 2026-08-13, when this found `gateway.broker_secret_file`: the
/// gateway declared it, the schema instead carried a `broker_secret` string
/// nothing read, and the shipped example overlay could not be written to use
/// either one.
fn check_overlay_schema(specs: &[ConfigSpec], overlay: &OverlayLeaves) -> usize {
    let known = overlay.paths().iter().map(String::as_str).collect::<BTreeSet<&str>>();
    let mut missing = 0usize;
    let mut checked = 0usize;
    for spec in specs {
        let Some(path) = spec.toml_path() else {
            continue;
        };
        checked += 1;
        if !known.contains(path) {
            eprintln!(
                "config audit: {path} is a declared overlay path with no field in \
                 FileConfig; deny_unknown_fields rejects any overlay that uses it"
            );
            missing += 1;
        }
    }
    if checked == 0 {
        eprintln!("config audit: no declaration has an overlay path; nothing was compared");
        std::process::exit(1);
    }
    missing
}

/// Read and parse the overlay schema, or exit.
fn overlay_leaves(root: &Path) -> OverlayLeaves {
    let overlay_path = root.join("crates/zeroship-core/src/config/file.rs");
    match std::fs::read_to_string(&overlay_path) {
        Ok(source) => match OverlayLeaves::from_source(&overlay_path.display().to_string(), &source)
        {
            Ok(leaves) => leaves,
            Err(error) => {
                eprintln!("config audit: {error}");
                std::process::exit(1);
            }
        },
        Err(error) => {
            eprintln!(
                "config audit: cannot read {}: {error}",
                overlay_path.display()
            );
            std::process::exit(1);
        }
    }
}

/// Run the source extraction and return only its rows.
///
/// Findings are reported and are fatal: an extraction that could not parse part
/// of the tree would produce a SHORTER row set, and a shorter set on one side of
/// an equality check is exactly the failure mode that reads as agreement.
fn extract_rows(root: &Path) -> Vec<InventoryRow> {
    let overlay_path = root.join("crates/zeroship-core/src/config/file.rs");
    let overlay = match std::fs::read_to_string(&overlay_path) {
        Ok(source) => match OverlayLeaves::from_source(&overlay_path.display().to_string(), &source)
        {
            Ok(leaves) => leaves,
            Err(error) => {
                eprintln!("config audit: {error}");
                std::process::exit(1);
            }
        },
        Err(error) => {
            eprintln!(
                "config audit: cannot read {}: {error}",
                overlay_path.display()
            );
            std::process::exit(1);
        }
    };
    let sources = match collect_rust_sources(root, &["crates"]) {
        Ok(sources) => sources,
        Err(errors) => {
            for error in errors {
                eprintln!("config audit: {error}");
            }
            std::process::exit(1);
        }
    };
    match scan_sources(&sources, &overlay) {
        Ok(report) => {
            if !report.findings.is_empty() {
                for finding in &report.findings {
                    eprintln!("config audit: extraction finding: {finding}");
                }
                std::process::exit(1);
            }
            report.rows
        }
        Err(errors) => {
            for error in errors {
                eprintln!("config audit: {error}");
            }
            std::process::exit(1);
        }
    }
}

/// Require the compiled contract and the source extraction to agree exactly.
///
/// This is the proposal's "keep the extraction command as an audit that must
/// equal the generated set". The two sides do not share a projection or a
/// parser; see the header of `crates/zeroship-config-contract/src/audit.rs`.
fn audit(args: &[String]) {
    let (root, _) = parse_root_and_check(args, false);
    let specs = platform_specs();
    let sites = platform_read_sites();

    // Validate the compiled side BEFORE comparing. A registry with a collision
    // or an unread declaration would still compare equal to an extraction that
    // reproduced the same mistake, so equality alone is not enough.
    if let Err(errors) = validate_contract(&specs, &sites) {
        for error in errors {
            eprintln!("config audit: compiled contract: {error}");
        }
        std::process::exit(1);
    }

    let missing = check_overlay_schema(&specs, &overlay_leaves(&root));
    if missing > 0 {
        eprintln!(
            "config audit: {missing} declared overlay path(s) the TOML schema does not accept"
        );
        std::process::exit(1);
    }

    let compiled = from_specs(&specs);
    let extracted = from_inventory(&extract_rows(&root), DECLARING_BINARIES);
    match compare(&compiled, &extracted) {
        Ok(count) => {
            eprintln!(
                "config audit: {count} projections agree across {} binaries \
                 ({} compiled declarations, {} linked read sites)",
                DECLARING_BINARIES.len(),
                specs.len(),
                sites.len(),
            );
        }
        Err(errors) => {
            for error in &errors {
                eprintln!("config audit: {error}");
            }
            eprintln!(
                "config audit: {} disagreement(s) between the compiled contract and \
                 the source extraction",
                errors.len()
            );
            std::process::exit(1);
        }
    }
}

/// Emit the compiled contract as TSV, for the text gates to join against.
///
/// Exists so the Compose and ops-TOML checks can stay text-only shell scripts
/// beside their siblings in `tests/` while still deriving their expected set
/// from the COMPILED registry rather than from a list in the script. A shell
/// gate that carries its own copy of the names is satisfiable by editing the
/// copy, which is the shape this repository has been burned by before.
fn contract(args: &[String]) {
    let (_, _) = parse_root_and_check(args, false);
    let specs = platform_specs();
    if specs.is_empty() {
        eprintln!("config contract: zero declarations linked; nothing to emit");
        std::process::exit(1);
    }
    println!("consumer\tcanonical\tclass\tflag\tenv\ttoml");
    let rows = from_specs(&specs);
    for row in &rows {
        println!(
            "{}\t{}\t{}\t{}\t{}\t{}",
            row.consumer, row.canonical, row.class, row.flag, row.env, row.toml
        );
    }
    eprintln!("config contract: {} projections", rows.len());
}

/// Render, or verify, the generated region of `docs/reference/env-vars.md`.
fn env_vars_doc(args: &[String]) {
    let (root, check) = parse_root_and_check(args, true);
    let specs = platform_specs();
    let settings = docs::collect(&specs);
    let generated = docs::render(&settings);
    let path = root.join(ENV_VARS_DOC);
    let display = path.display().to_string();
    let document = match std::fs::read_to_string(&path) {
        Ok(document) => document,
        Err(error) => {
            eprintln!("config env-vars-doc: cannot read {display}: {error}");
            std::process::exit(1);
        }
    };

    if check {
        match docs::check(ENV_VARS_DOC, &document, &generated) {
            Ok(()) => eprintln!(
                "config env-vars-doc: {display} matches the compiled contract \
                 ({} canonical settings)",
                settings.len()
            ),
            Err(error) => {
                eprintln!("config env-vars-doc: {error}");
                std::process::exit(1);
            }
        }
        return;
    }

    match docs::splice(ENV_VARS_DOC, &document, &generated) {
        Ok(updated) => {
            if updated == document {
                eprintln!("config env-vars-doc: {display} already current");
                return;
            }
            if let Err(error) = std::fs::write(&path, updated) {
                eprintln!("config env-vars-doc: cannot write {display}: {error}");
                std::process::exit(1);
            }
            eprintln!(
                "config env-vars-doc: rewrote the generated region of {display} \
                 ({} canonical settings)",
                settings.len()
            );
        }
        Err(error) => {
            eprintln!("config env-vars-doc: {error}");
            std::process::exit(1);
        }
    }
}

fn check_metadata(manifest: Option<PathBuf>) {
    let manifest = manifest.unwrap_or_else(|| PathBuf::from("Cargo.toml"));
    match check_workspace(&manifest) {
        Ok(summary) => println!(
            "config contract: {} binaries classified ({} platform, {} excluded)",
            summary.binaries, summary.platform, summary.excluded
        ),
        Err(errors) => {
            for error in errors {
                eprintln!("config contract: {error}");
            }
            std::process::exit(1);
        }
    }
}

/// Print every declared key and the per-class totals.
fn report_declared_keys(sources: &[(String, String)]) {
    let keys = match collect_declared_keys(sources) {
        Ok(keys) => keys,
        Err(errors) => {
            for error in errors {
                eprintln!("config raw-env: census: {error}");
            }
            return;
        }
    };
    let mut classes: BTreeMap<&str, (usize, BTreeSet<&str>)> = BTreeMap::new();
    for key in &keys {
        println!("declared\t{}\t{}\t{}", key.class, key.name, key.file);
        let entry = classes.entry(key.class.as_str()).or_default();
        entry.0 += 1;
        entry.1.insert(key.name.as_str());
    }
    eprintln!("config raw-env: {} declared key sites", keys.len());
    for (class, (sites, names)) in classes {
        eprintln!(
            "config raw-env: class {class}: {sites} sites, {} distinct names",
            names.len()
        );
    }
}

/// Report every remaining raw environment access and every declared key.
///
/// This is the Step 4 worklist and, once it reaches zero violations, the
/// evidence that the gate in `crates/zeroship-config-contract/tests/` can be believed.
/// Rows go to stdout, counts to stderr, so a redirected run keeps a clean list.
fn raw_env(args: &[String]) {
    let mut root = PathBuf::from(".");
    let mut gate = false;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--root" => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("{USAGE}");
                    std::process::exit(2);
                };
                root = PathBuf::from(value);
                index += 2;
            }
            "--gate" => {
                gate = true;
                index += 1;
            }
            other => {
                eprintln!("{USAGE}; got {other:?}");
                std::process::exit(2);
            }
        }
    }

    let sources = match collect_tracked_rust_sources(&root) {
        Ok(sources) => sources,
        Err(errors) => {
            for error in errors {
                eprintln!("config raw-env: {error}");
            }
            std::process::exit(1);
        }
    };

    if gate && !planted_fixtures_are_tracked(&sources) {
        eprintln!(
            "config raw-env: gate: REFUSED: no tracked Rust file lies under \
             {PLANTED_VIOLATION_DIR}, so the constant this mode classifies by names \
             nothing. Every planted violation would be reported as an unexpected one \
             and every real one would be indistinguishable from them. Repoint the \
             constant at the fixtures rather than reading this run's verdict."
        );
        std::process::exit(1);
    }

    match scan_sources_by_role(&sources) {
        Ok(report) => {
            report_declared_keys(&sources);
            for permitted in &report.permitted_raw {
                println!("permitted-raw\t-\t-\t{permitted}");
            }
            eprintln!(
                "config raw-env: {} tracked files, 0 violations, {} declared keys, \
                 {} role-permitted raw accesses",
                report.files,
                report.declared_keys.len(),
                report.permitted_raw.len()
            );
            if gate {
                // A CLEAN scan is the FAILING case for the gate. The planted
                // fixtures under crates/zeroship-config-contract/tests/fixtures/ break
                // the rule on purpose, so zero violations means the scanner
                // stopped seeing them - the exact false green this mode exists
                // to make impossible.
                eprintln!(
                    "config raw-env: gate: zero violations, but {PLANTED_VIOLATIONS} are \
                     planted under {PLANTED_VIOLATION_DIR}. A scan that misses a deliberate \
                     violation cannot be trusted to catch an accidental one."
                );
                std::process::exit(1);
            }
        }
        Err(violations) => {
            // Print the census ANYWAY. A report that withholds the class counts
            // whenever anything is still unconverted is useless during the
            // conversion it exists to measure, which is the only time anyone
            // runs it.
            report_declared_keys(&sources);
            let mut kinds: BTreeMap<&str, usize> = BTreeMap::new();
            for violation in &violations {
                let kind = match violation {
                    RawEnvViolation::Read(_) => "read",
                    RawEnvViolation::Write(_) => "write",
                    RawEnvViolation::Import(_) => "import",
                    RawEnvViolation::CompileTime(_) => "compile-time",
                    RawEnvViolation::UnregisteredRead(_) => "unregistered",
                    RawEnvViolation::IllicitAllow(_) => "illicit-allow",
                    RawEnvViolation::SealedShape(_) => "sealed-shape",
                    RawEnvViolation::InvalidKeyName(_) => "invalid-key-name",
                    RawEnvViolation::MisclassifiedKey(_) => "misclassified-key",
                    RawEnvViolation::Parse(_) => "parse",
                    RawEnvViolation::EmptyScan => "empty-scan",
                };
                *kinds.entry(kind).or_default() += 1;
                println!("violation\t{kind}\t{violation}");
            }
            eprintln!(
                "config raw-env: {} tracked files, {} violations",
                sources.len(),
                violations.len()
            );
            for (kind, count) in kinds {
                eprintln!("config raw-env: {kind}: {count}");
            }
            if gate {
                std::process::exit(gate_verdict(&violations));
            }
            std::process::exit(1);
        }
    }
}

/// Files whose raw reads are PLANTED, and whose absence is itself a failure.
///
/// The scanner is proved to discriminate by fixtures that break the rule on
/// purpose. Those fixtures are tracked Rust, so a scan of the tracked tree
/// finds them and a bare `raw-env` correctly exits 1 with two violations. That
/// makes the subcommand unusable as a CI gate as written, which is why this
/// mode exists.
///
/// DERIVED FROM THE PACKAGE NAME, not spelled out. This was the literal
/// `crates/config-contract/tests/fixtures/` from `105a75131`
/// ("every crate directory is named for the package it holds", which renamed the
/// directory to `crates/zeroship-config-contract`) until 2026-09-04, and it named
/// nothing for that whole period: `gate_verdict` classifies by
/// `to_string().contains(...)`, so a prefix matching nothing put all four planted
/// violations in the UNEXPECTED column and `--gate` exited 1 on every tree. That
/// is the third site of one defect. `raw_env::CENTRAL_ACCESSOR` and
/// `crates/zeroship-core/tests/config_env_access_gate.rs`'s `FIXTURE_PREFIXES` were
/// the first two, both repaired on 2026-08-28; this one was missed because it is
/// in a `[[bin]]` that no `#[test]` linked. `env!("CARGO_PKG_NAME")` makes the
/// rename impossible to survive: cargo supplies the name, so a package rename
/// moves this string in the same build.
///
/// The one assumption left is the `crates/` root, which cargo cannot supply.
/// `refuse_unless_planted_fixtures_are_tracked` below is what stops that
/// assumption failing silently.
const PLANTED_VIOLATION_DIR: &str = concat!("crates/", env!("CARGO_PKG_NAME"), "/tests/fixtures/");

/// The expected number of planted violations.
///
/// A FLOOR, not a ceiling, is the wrong shape here: fewer means the scanner
/// stopped seeing a rule it is supposed to enforce, and more means someone
/// added a fixture without saying so. Both are worth a failure.
///
/// FOUR, two per fixture: `raw_read_alias.rs` and `raw_write_alias.rs` each
/// bind a `std::env` function under another name behind a false cfg and then
/// call it, so each yields one import violation and one use violation. The
/// write fixture is what keeps the WRITE rule provably alive - without it the
/// rule could stop firing entirely and this gate would report clean.
const PLANTED_VIOLATIONS: usize = 4;

/// Whether any enumerated source actually lies under [`PLANTED_VIOLATION_DIR`].
///
/// The one-variable partner to the classification in [`gate_verdict`]. That
/// function sorts violations into "planted" and "unexpected" by prefix, and a
/// prefix matching nothing is indistinguishable from a prefix matching only
/// clean files: both put everything in the second column, and the resulting
/// failure names the fixtures rather than the constant. Asking the ENUMERATION
/// whether the prefix resolves separates those two states before the verdict is
/// computed.
///
/// This is the same guard `crates/zeroship-core/tests/config_env_access_gate.rs`
/// added for `FIXTURE_PREFIXES` on 2026-08-28, at the sibling site that was
/// repaired then and this one was not.
fn planted_fixtures_are_tracked(sources: &[(String, String)]) -> bool {
    sources
        .iter()
        .any(|(path, _)| path.starts_with(PLANTED_VIOLATION_DIR))
}

/// Decide the gate exit status from a violation set.
///
/// Splitting this out of `raw_env` keeps the rule readable: every violation
/// must come from the planted-fixture directory, and the planted ones must all
/// still be found. A gate that only checked the first half would pass on a
/// scanner that had gone blind.
fn gate_verdict(violations: &[RawEnvViolation]) -> i32 {
    let mut unexpected = 0usize;
    let mut planted = 0usize;
    for violation in violations {
        if violation.to_string().contains(PLANTED_VIOLATION_DIR) {
            planted += 1;
        } else {
            unexpected += 1;
            eprintln!("config raw-env: gate: unexpected violation: {violation}");
        }
    }
    if unexpected > 0 {
        eprintln!(
            "config raw-env: gate: {unexpected} violation(s) outside {PLANTED_VIOLATION_DIR}"
        );
        return 1;
    }
    if planted != PLANTED_VIOLATIONS {
        eprintln!(
            "config raw-env: gate: expected exactly {PLANTED_VIOLATIONS} planted violations \
             under {PLANTED_VIOLATION_DIR}, found {planted}. Fewer means the scanner stopped \
             recognising a rule it enforces; more means an unannounced fixture."
        );
        return 1;
    }
    eprintln!(
        "config raw-env: gate: clean; the {planted} planted fixture violations are still \
         detected, so the scanner is not silently passing everything"
    );
    0
}

/// Emit every configuration name declared in the tracked crate tree.
///
/// The summary goes to stderr and the rows to stdout, so a redirected run keeps
/// a clean TSV while the counts stay visible. Those counts are the point: the
/// before/after row totals are what make "every row converted or classified"
/// checkable rather than asserted.
fn inventory(args: &[String]) {
    let mut root = PathBuf::from(".");
    let mut format = String::from("tsv");
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--format" => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("{USAGE}");
                    std::process::exit(2);
                };
                format.clone_from(value);
                index += 2;
            }
            "--root" => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("{USAGE}");
                    std::process::exit(2);
                };
                root = PathBuf::from(value);
                index += 2;
            }
            other => {
                eprintln!("{USAGE}; got {other:?}");
                std::process::exit(2);
            }
        }
    }
    if format != "tsv" {
        eprintln!("config inventory: only --format tsv is implemented; got {format:?}");
        std::process::exit(2);
    }

    let overlay_path = root.join("crates/zeroship-core/src/config/file.rs");
    let overlay = match std::fs::read_to_string(&overlay_path) {
        Ok(source) => match OverlayLeaves::from_source(&overlay_path.display().to_string(), &source)
        {
            Ok(leaves) => leaves,
            Err(error) => {
                eprintln!("config inventory: {error}");
                std::process::exit(1);
            }
        },
        Err(error) => {
            eprintln!(
                "config inventory: cannot read {}: {error}",
                overlay_path.display()
            );
            std::process::exit(1);
        }
    };

    let sources = match collect_rust_sources(Path::new(&root), &["crates"]) {
        Ok(sources) => sources,
        Err(errors) => {
            for error in errors {
                eprintln!("config inventory: {error}");
            }
            std::process::exit(1);
        }
    };
    match scan_sources(&sources, &overlay) {
        Ok(report) => {
            // Rows first, findings after. A mid-conversion tree will have
            // findings; printing the checklist anyway is the whole point of
            // this being usable DURING the conversion it measures.
            print!("{}", format_tsv(&report.rows));
            let summary = report.summary;
            eprintln!(
                "config inventory: {} files, {} command structs, {} rows \
                 ({} converted, {} unconverted)",
                summary.files,
                summary.structs,
                summary.rows,
                summary.converted,
                summary.unconverted
            );
            for finding in &report.findings {
                eprintln!("config inventory: {finding}");
            }
            if !report.findings.is_empty() {
                eprintln!(
                    "config inventory: {} finding(s); rows above are still complete",
                    report.findings.len()
                );
                std::process::exit(1);
            }
        }
        Err(errors) => {
            for error in errors {
                eprintln!("config inventory: {error}");
            }
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `CARGO_MANIFEST_DIR` is this crate's directory; the workspace root is two
    /// levels above it. Deliberately NOT derived from `PLANTED_VIOLATION_DIR`,
    /// which is the thing under test.
    fn workspace_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .expect("this crate directory has a workspace root two levels above it")
            .to_path_buf()
    }

    /// The regression. `PLANTED_VIOLATION_DIR` must name a directory that really
    /// holds tracked Rust.
    ///
    /// FAILS BEFORE THE FIX: the constant read `crates/config-contract/tests/fixtures/`
    /// from the `105a75131` crate-directory rename until 2026-09-04, matched no
    /// tracked file, and so put all four planted violations in the "unexpected"
    /// column - `tests/config_name_alignment_gate.sh` section 5 was red on every
    /// tree for that whole period, blaming the two fixture files that exist to
    /// prove the scanner works.
    ///
    /// This test lives in the `[[bin]]` because the constant does. That is why the
    /// 2026-08-28 sweep repaired `raw_env::CENTRAL_ACCESSOR` and
    /// `config_env_access_gate.rs`'s `FIXTURE_PREFIXES` and missed this one: both
    /// of those are reachable from a `#[test]` in a library or integration target,
    /// and nothing linked this one.
    #[test]
    fn planted_violation_dir_names_tracked_fixtures() {
        let sources = collect_tracked_rust_sources(&workspace_root())
            .unwrap_or_else(|errors| panic!("could not enumerate tracked Rust source: {errors:?}"));

        // Anti-vacuity: an empty or tiny enumeration would fail the assertion
        // below for the wrong reason, and "the prefix names nothing" would be
        // reported when the truth is "nothing was enumerated".
        assert!(
            sources.len() > 500,
            "only {} tracked Rust files were enumerated; the enumeration is broken, \
             so this test cannot rule on the prefix",
            sources.len()
        );

        let matched = sources
            .iter()
            .filter(|(path, _)| path.starts_with(PLANTED_VIOLATION_DIR))
            .count();
        assert!(
            matched > 0,
            "no tracked file starts with {PLANTED_VIOLATION_DIR:?}, so `gate_verdict` \
             classifies every planted violation as an unexpected one. A directory moved \
             and this constant did not."
        );
    }

    /// The one-variable partner: prove the resolve check says NO for the shape
    /// that caused the defect. Without it `planted_fixtures_are_tracked` could
    /// return `true` unconditionally and the test above would still pass.
    #[test]
    fn the_resolve_check_rejects_a_prefix_that_names_nothing() {
        let elsewhere = vec![(
            "crates/zeroship-core/src/config/env.rs".to_owned(),
            String::new(),
        )];
        assert!(!planted_fixtures_are_tracked(&elsewhere));

        let under_the_prefix = vec![(
            format!("{PLANTED_VIOLATION_DIR}raw_read_alias.rs"),
            String::new(),
        )];
        assert!(planted_fixtures_are_tracked(&under_the_prefix));
    }
}
