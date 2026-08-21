//! `compose-workflow-advance-flag <deploy-dir> <worker-handler.rs>`
//!
//! Exits 0 when no file under the deploy tree arms `--workflow-advance-unsigned`
//! AND the worker still refuses it, 1 when either half fails, and 1 when the
//! gate cannot judge at all. See `zeroship_gatekit::workflow_advance_flag` for
//! why those two are one question.
//!
//! The paths are ARGUMENTS, not environment variables.

use std::path::PathBuf;
use std::process::ExitCode;

use zeroship_gatekit::workflow_advance_flag::{run, BACKSTOP_STRING, FLAG, TITLE};

const USAGE: &str = "usage: compose-workflow-advance-flag <deploy-dir> <path/to/handler.rs>";

fn main() -> ExitCode {
    let mut args = std::env::args_os().skip(1);
    let (Some(deploy), Some(backstop), None) = (args.next(), args.next(), args.next()) else {
        eprintln!("  x REFUSED: {USAGE}");
        return ExitCode::FAILURE;
    };

    let outcome = run(
        &PathBuf::from(deploy),
        FLAG,
        &PathBuf::from(backstop),
        BACKSTOP_STRING,
    );
    print!("{}", outcome.census());
    print!("{}", outcome.report().render(TITLE));
    match outcome.report().verdict().exit_code() {
        0 => ExitCode::SUCCESS,
        _ => ExitCode::FAILURE,
    }
}
