//! `gate-arm-census <tests-dir> [--run <gate.sh>...]`
//!
//! The meta-gate. It rules on the gates themselves: every gate script must be
//! able to say, per arm, how many items that arm ruled on and the floor that
//! number must clear. See `zeroship_gatekit::arm_census` for what went wrong
//! four times on 2026-08-20 and why the floors deliberately live in the gates
//! rather than in a table here.
//!
//! Two modes, answering two different questions:
//!
//! - default (STATIC): does every gate under `<tests-dir>` PARTICIPATE in the
//!   contract? This is what CI runs, because it is a property of the source and
//!   costs milliseconds. It catches the gate that never declared an arm.
//! - `--run <gate.sh>...` (DYNAMIC): additionally execute the named gates and
//!   rule on the counts they actually emit. This catches an arm whose
//!   declaration is behind a branch that stopped being taken, and an arm whose
//!   enumeration collapsed. The gates are NAMED ARGUMENTS rather than a list
//!   held here: a built-in "these gates are cheap enough to run" table would be
//!   a census, which is the failure this whole gate exists to stop. CI does not
//!   need such a list either, because it already runs each gate as its own
//!   step, where the arm's own floor refuses at the point of measurement.
//!
//! Paths are ARGUMENTS, never environment variables: a gate whose target
//! depends on how the process was launched cannot be reasoned about from its
//! invocation.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use zeroship_gatekit::arm_census::{audit, parse_emissions, rule_on_emissions, GateFile};
use zeroship_gatekit::report::Verdict;

const USAGE: &str = "usage: gate-arm-census <tests-dir> [--run <gate.sh>...]";

/// How few gate scripts means the enumeration itself broke.
///
/// THE ONLY NUMBER IN THIS BINARY, and it counts FILES, not findings. 22 gate
/// scripts existed on 2026-08-20 (21, plus `zero_tokio_gate.sh`); gates are
/// added and deleted by hand, so a drop is a decision somebody made and should
/// record here in the same commit, not an accident to be absorbed. Set at the
/// observed count on purpose: a slack floor here would let the glob half-break
/// unnoticed, which is the precise failure this binary exists to catch one
/// level down.
const GATE_FILE_FLOOR: usize = 22;

fn read_gates(dir: &Path) -> Result<Vec<GateFile>, String> {
    let entries =
        std::fs::read_dir(dir).map_err(|error| format!("cannot read {}: {error}", dir.display()))?;
    let mut gates = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| format!("cannot read an entry of {}: {error}", dir.display()))?;
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if !name.ends_with("_gate.sh") {
            continue;
        }
        let source = std::fs::read_to_string(&path)
            .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
        gates.push(GateFile {
            name: name.to_owned(),
            source,
        });
    }
    gates.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(gates)
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some((dir, rest)) = args.split_first() else {
        eprintln!("  x REFUSED: {USAGE}");
        return ExitCode::FAILURE;
    };
    let mut to_run: Vec<PathBuf> = Vec::new();
    match rest.split_first() {
        None => {}
        Some((flag, paths)) if flag == "--run" => {
            if paths.is_empty() {
                eprintln!("  x REFUSED: --run needs at least one gate script. {USAGE}");
                return ExitCode::FAILURE;
            }
            to_run = paths.iter().map(PathBuf::from).collect();
        }
        Some(_) => {
            eprintln!("  x REFUSED: {USAGE}");
            return ExitCode::FAILURE;
        }
    }

    let gates = match read_gates(Path::new(dir)) {
        Ok(gates) => gates,
        Err(error) => {
            eprintln!("  x REFUSED: {error}");
            return ExitCode::FAILURE;
        }
    };

    // Print the enumeration BEFORE the verdict. A gate's most important number
    // is how much it enumerated: a smaller green is not a pass, and the only
    // way to see that is to print the count on every run.
    println!(
        "  gates enumerated: {} under {dir} (floor {GATE_FILE_FLOOR})",
        gates.len()
    );

    let report = audit(&gates, GATE_FILE_FLOOR);
    let verdict = report.verdict();
    print!(
        "{}",
        report.render("every gate declares, per arm, how much it examined")
    );
    let mut failed = verdict.exit_code() != 0;

    for path in &to_run {
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("<unnamed>")
            .to_owned();
        // The gate's own exit code is deliberately IGNORED. This mode asks one
        // question - did each arm have anything to rule on - and a gate that is
        // red about the tree still answers it. Conflating the two would make
        // this report every ordinary gate failure a second time.
        let output = Command::new("bash").arg(path).output();
        let run_report = match output {
            Err(error) => {
                let mut report = zeroship_gatekit::report::Report::new();
                report.refuse(format!("{name}: could not execute: {error}"));
                report
            }
            Ok(output) => {
                let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
                text.push_str(&String::from_utf8_lossy(&output.stderr));
                rule_on_emissions(&name, &parse_emissions(&text))
            }
        };
        let run_verdict = run_report.verdict();
        print!("{}", run_report.render(&format!("arms {name} actually ran")));
        if run_verdict.exit_code() != 0 {
            failed = true;
        }
        if let Verdict::Refused(reason) = run_verdict {
            eprintln!("  x REFUSED: {reason}");
        }
    }

    if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}
