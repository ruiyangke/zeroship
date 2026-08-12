use std::path::{Path, PathBuf};

use zeroship_config_contract::inventory::{
    collect_rust_sources, format_tsv, scan_sources, OverlayLeaves,
};
use zeroship_config_contract::metadata::check_workspace;

const USAGE: &str = "usage: zeroship-config-contract \
[check-metadata [path/to/Cargo.toml] | inventory [--format tsv] [--root DIR]]";

fn main() {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    match args.first().map(String::as_str) {
        None | Some("check-metadata") => check_metadata(args.get(1).map(PathBuf::from)),
        Some("inventory") => inventory(&args[1..]),
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
        Ok((rows, summary)) => {
            print!("{}", format_tsv(&rows));
            eprintln!(
                "config inventory: {} files, {} command structs, {} rows \
                 ({} converted, {} unconverted)",
                summary.files,
                summary.structs,
                summary.rows,
                summary.converted,
                summary.unconverted
            );
        }
        Err(errors) => {
            for error in errors {
                eprintln!("config inventory: {error}");
            }
            std::process::exit(1);
        }
    }
}
