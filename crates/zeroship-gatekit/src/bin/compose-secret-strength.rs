//! `compose-secret-strength <compose-file>`
//!
//! Exits 0 when every secret the compose file supplies satisfies the strength
//! rule the product enforces on it, 1 when one does not, and 1 when the gate
//! cannot judge at all. See `zeroship_gatekit::secret_strength` for what the
//! rule set is and why it is typed data.
//!
//! The compose path is an ARGUMENT, not an environment variable: a gate whose
//! target depends on how the process was launched cannot be reasoned about
//! from its invocation.

use std::path::PathBuf;
use std::process::ExitCode;

use zeroship_core::config::PLATFORM_SECRETS;
use zeroship_gatekit::compose::ComposeFile;
use zeroship_gatekit::secret_strength::{enforced_rules, run};

const USAGE: &str = "usage: compose-secret-strength <path/to/docker-compose.yml>";

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

    // Print the rule count BEFORE the verdict. A gate's most important number
    // is how much it enumerated: a smaller green is not a pass, and the only
    // way to see that is to print the count on every run.
    println!(
        "  rules: {} of {} platform secrets carry a strength floor \
         (zeroship_core::config::PLATFORM_SECRETS)",
        enforced_rules(PLATFORM_SECRETS).len(),
        PLATFORM_SECRETS.len()
    );

    let report = run(&compose, PLATFORM_SECRETS);
    let verdict = report.verdict();
    // `render` already carries the refusal line, so printing one here too
    // would double it in a CI log that merges the streams.
    print!(
        "{}",
        report.render("compose secrets meet the product's own strength rule")
    );
    match verdict.exit_code() {
        0 => ExitCode::SUCCESS,
        _ => ExitCode::FAILURE,
    }
}
