//! `compose-port-exposure <compose-file> <caddyfile>`
//!
//! Exits 0 when every published port is loopback-bound except the edge's and
//! the control route's posture is consistent, 1 when one is not, and 1 when the
//! gate cannot judge at all. See `zeroship_gatekit::port_exposure` for what the
//! rule is and what a green does not cover.
//!
//! The paths are ARGUMENTS, not environment variables: a gate whose target
//! depends on how the process was launched cannot be reasoned about from its
//! invocation.

use std::path::PathBuf;
use std::process::ExitCode;

use zeroship_gatekit::compose::ComposeFile;
use zeroship_gatekit::port_exposure::{run, TITLE};

const USAGE: &str = "usage: compose-port-exposure <path/to/docker-compose.yml> <path/to/Caddyfile>";

fn main() -> ExitCode {
    let mut args = std::env::args_os().skip(1);
    let (Some(compose_path), Some(caddy_path), None) = (args.next(), args.next(), args.next())
    else {
        eprintln!("  x REFUSED: {USAGE}");
        return ExitCode::FAILURE;
    };
    let compose_path = PathBuf::from(compose_path);
    let caddy_path = PathBuf::from(caddy_path);

    let compose = match ComposeFile::load(&compose_path) {
        Ok(compose) => compose,
        Err(error) => {
            eprintln!("  x REFUSED: {error}");
            return ExitCode::FAILURE;
        }
    };
    let caddyfile = match std::fs::read_to_string(&caddy_path) {
        Ok(text) => text,
        Err(error) => {
            eprintln!("  x REFUSED: read {}: {error}", caddy_path.display());
            return ExitCode::FAILURE;
        }
    };

    let outcome = run(&compose, &caddyfile);
    // The census goes out BEFORE the verdict. A gate's most important number is
    // how much it enumerated: a smaller green is not a pass, and the only way to
    // see that is to print the count on every run.
    print!("{}", outcome.census());
    print!("{}", outcome.report().render(TITLE));
    match outcome.report().verdict().exit_code() {
        0 => ExitCode::SUCCESS,
        _ => ExitCode::FAILURE,
    }
}
