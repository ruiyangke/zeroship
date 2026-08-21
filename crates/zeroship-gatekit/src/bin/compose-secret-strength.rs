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
use zeroship_gatekit::arm_census::Arm;
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
    //
    // It goes out in the SAME wire format the shell gates use, because
    // tests/compose_secret_strength_gate.sh is a shim and gate_arms_delegate
    // refuses unless this binary emits one. The rule set going to zero is the
    // 2026-08-13 failure this whole crate was written for; it is now the same
    // refusal, spelled the same way, as an arm collapsing in any other gate.
    //
    // FLOOR 4 against 6 rules today - MEASURED 2026-08-20 by running this
    // binary: "6 of 8 platform secrets carry a strength floor". Under today's
    // count so that deliberately unrestricting a secret does not fail the gate,
    // well above the zero that de-enumeration produces.
    let enforced = enforced_rules(PLATFORM_SECRETS).len();
    let arm = Arm {
        gate: "compose_secret_strength",
        arm: "secret_rules",
        examined: enforced,
        floor: 4,
    };
    println!("{}", arm.line());
    println!(
        "  rules: {enforced} of {} platform secrets carry a strength floor \
         (zeroship_core::config::PLATFORM_SECRETS)",
        PLATFORM_SECRETS.len()
    );
    if !arm.cleared() {
        eprintln!(
            "  x REFUSED: only {enforced} strength rule(s) enumerated, floor {}. The rule table \
             stopped enumerating; a clean compose file and an empty rule set print the same thing.",
            arm.floor
        );
        return ExitCode::FAILURE;
    }

    // THE SECOND ARM, and it exists because the first one could not see the
    // rows that matter most to the credential boot gate. `secret_rules` counts
    // rows with a LENGTH floor, which is 6 of 8; the two it drops are
    // `ZEROSHIP_CONTROL_KEY` and `ZEROSHIP_MIGRATED_POLICY_SEAL_KEY`, whose only
    // rule is "not empty, not the placeholder". `run` judges every row, so this
    // arm reports what that is - and the two counts differing is the point,
    // because a single number could not have shown the hole.
    //
    // FLOOR 6 against 8 rows today, MEASURED by running this binary. Bound to
    // the table this gate reads, not to a constant recorded elsewhere.
    let all_rules = PLATFORM_SECRETS.len();
    let placeholder_arm = Arm {
        gate: "compose_secret_strength",
        arm: "placeholder_rules",
        examined: all_rules,
        floor: 6,
    };
    println!("{}", placeholder_arm.line());
    println!(
        "  rules: {all_rules} platform secrets judged for the empty/placeholder case \
         (including the {} with no length floor)",
        all_rules - enforced
    );
    if !placeholder_arm.cleared() {
        eprintln!(
            "  x REFUSED: only {all_rules} platform secret(s) enumerated, floor {}. \
             zeroship_core::config::PLATFORM_SECRETS stopped enumerating.",
            placeholder_arm.floor
        );
        return ExitCode::FAILURE;
    }

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
