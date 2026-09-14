use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use zeroship_core::config::ConfigSpec;

use zeroship_config_contract::audit::{compare, from_inventory, from_specs};
use zeroship_config_contract::contract::validate_contract;
use zeroship_config_contract::docs;
use zeroship_config_contract::inventory::{
    collect_rust_sources, format_tsv, scan_sources, InventoryRow, OverlayLeaves,
};
use zeroship_config_contract::metadata::check_workspace;
use zeroship_config_contract::registry::{platform_read_sites, platform_specs, DECLARING_BINARIES};

const USAGE: &str = "usage: zeroship-config-contract \
[check-metadata [path/to/Cargo.toml] | inventory [--format tsv] [--root DIR] \
| audit [--root DIR] | contract | env-vars-doc [--root DIR] [--check]]";

/// The generated half of the environment reference.
const ENV_VARS_DOC: &str = "docs/reference/env-vars.md";

fn main() {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    match args.first().map(String::as_str) {
        None | Some("check-metadata") => check_metadata(args.get(1).map(PathBuf::from)),
        Some("inventory") => inventory(&args[1..]),
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
    let extracted = from_inventory(&extract_rows(&root), &DECLARING_BINARIES);
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
