//! `compose-stateful-volume <compose-file>`
//!
//! Exits 0 when every stateful service keeps its data on a named volume, 1 when
//! one does not, and 1 when the gate cannot judge at all. See
//! `zeroship_gatekit::stateful_volume` for which services those are and why the
//! list is stated rather than inferred.
//!
//! The compose path is an ARGUMENT, not an environment variable.

use std::path::PathBuf;
use std::process::ExitCode;

use zeroship_gatekit::compose::ComposeFile;
use zeroship_gatekit::stateful_volume::{run, STATEFUL, TITLE};

const USAGE: &str = "usage: compose-stateful-volume <path/to/docker-compose.yml>";

fn main() -> ExitCode {
    let mut args = std::env::args_os().skip(1);
    let (Some(path), None) = (args.next(), args.next()) else {
        eprintln!("  x REFUSED: {USAGE}");
        return ExitCode::FAILURE;
    };
    let path = PathBuf::from(path);

    let compose = match ComposeFile::load(&path) {
        Ok(compose) => compose,
        Err(error) => {
            eprintln!("  x REFUSED: {error}");
            return ExitCode::FAILURE;
        }
    };

    let outcome = run(&compose, STATEFUL);
    print!("{}", outcome.census());
    print!("{}", outcome.report().render(TITLE));
    match outcome.report().verdict().exit_code() {
        0 => ExitCode::SUCCESS,
        _ => ExitCode::FAILURE,
    }
}
