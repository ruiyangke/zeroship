use std::path::PathBuf;

use zeroship_config_contract::docs;
use zeroship_config_contract::metadata::check_workspace;
use zeroship_config_contract::registry::platform_specs;

const USAGE: &str = "usage: zeroship-config-contract \
[check-metadata [path/to/Cargo.toml] | env-vars-doc [--root DIR] [--check]]";

/// The generated half of the environment reference.
const ENV_VARS_DOC: &str = "docs/reference/env-vars.md";

fn main() {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    match args.first().map(String::as_str) {
        None | Some("check-metadata") => check_metadata(args.get(1).map(PathBuf::from)),
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

/// Render, or verify, the generated region of the environment-variables reference.
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
