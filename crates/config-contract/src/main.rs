use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use zeroship_config_contract::inventory::{
    collect_rust_sources, collect_tracked_rust_sources, format_tsv, scan_sources, OverlayLeaves,
};
use zeroship_config_contract::metadata::check_workspace;
use zeroship_config_contract::raw_env::{scan_sources_by_role, RawEnvViolation};

const USAGE: &str = "usage: zeroship-config-contract \
[check-metadata [path/to/Cargo.toml] | inventory [--format tsv] [--root DIR] \
| raw-env [--root DIR]]";

fn main() {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    match args.first().map(String::as_str) {
        None | Some("check-metadata") => check_metadata(args.get(1).map(PathBuf::from)),
        Some("inventory") => inventory(&args[1..]),
        Some("raw-env") => raw_env(&args[1..]),
        Some(other) => {
            eprintln!("{USAGE}; got {other:?}");
            std::process::exit(2);
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

/// Report every remaining raw environment access and every declared key.
///
/// This is the Step 4 worklist and, once it reaches zero violations, the
/// evidence that the gate in `crates/config-contract/tests/` can be believed.
/// Rows go to stdout, counts to stderr, so a redirected run keeps a clean list.
fn raw_env(args: &[String]) {
    let mut root = PathBuf::from(".");
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

    match scan_sources_by_role(&sources) {
        Ok(report) => {
            let mut classes: BTreeMap<&str, usize> = BTreeMap::new();
            for key in &report.declared_keys {
                *classes.entry(key.class.as_str()).or_default() += 1;
                println!("declared\t{}\t{}\t{}", key.class, key.name, key.file);
            }
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
            for (class, count) in classes {
                eprintln!("config raw-env: class {class}: {count}");
            }
        }
        Err(violations) => {
            let mut kinds: BTreeMap<&str, usize> = BTreeMap::new();
            for violation in &violations {
                let kind = match violation {
                    RawEnvViolation::Read(_) => "read",
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
            std::process::exit(1);
        }
    }
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

    let overlay_path = root.join("crates/core/src/config/file.rs");
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
