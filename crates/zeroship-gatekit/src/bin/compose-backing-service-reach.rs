//! `compose-backing-service-reach <compose-file>`
//!
//! Exits 0 when every backing service compose runs is one some compose service
//! is configured to reach, 1 when one is not, and 1 when the gate cannot judge
//! at all.
//!
//! IT IS RED ON THE TRACKED TREE, deliberately: redpanda runs and nothing is
//! configured to publish to it. See `zeroship_gatekit::backing_service_reach`
//! and `docs/proposals/2026-08-20-metering-transport-not-configured.md`. Do not
//! "fix" this binary to make it green - configure the broker, or delete the
//! service.
//!
//! The compose path is an ARGUMENT, not an environment variable.

use std::path::PathBuf;
use std::process::ExitCode;

use zeroship_gatekit::backing_service_reach::{run, EDGE_SERVICES, HOST_TOOL_SERVICES, TITLE};
use zeroship_gatekit::compose::ComposeFile;

const USAGE: &str = "usage: compose-backing-service-reach <path/to/docker-compose.yml>";

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

    println!("  compose file: {}", path.display());
    let outcome = run(&compose, EDGE_SERVICES, HOST_TOOL_SERVICES);
    print!("{}", outcome.census());
    print!("{}", outcome.report().render(TITLE));
    match outcome.report().verdict().exit_code() {
        0 => ExitCode::SUCCESS,
        _ => ExitCode::FAILURE,
    }
}
