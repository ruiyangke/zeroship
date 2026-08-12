use std::path::PathBuf;

use zeroship_config_contract::metadata::check_workspace;

fn main() {
    let mut args = std::env::args_os().skip(1);
    let manifest = match args.next() {
        None => PathBuf::from("Cargo.toml"),
        Some(command) if command == "check-metadata" => args
            .next()
            .map_or_else(|| PathBuf::from("Cargo.toml"), PathBuf::from),
        Some(other) => {
            eprintln!(
                "usage: zeroship-config-contract [check-metadata [path/to/Cargo.toml]]; got {:?}",
                other
            );
            std::process::exit(2);
        }
    };
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
