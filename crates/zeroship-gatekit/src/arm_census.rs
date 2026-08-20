//! The meta-gate: every gate must be able to say how much it examined.
//!
//! # What went wrong, and why a meta-gate rather than more gates
//!
//! Four repository gates were found vacuous within a few hours on 2026-08-20,
//! each by a human reading rather than by any test:
//!
//! - `ws_subscription_stub_gate.sh` arm 1 watched ONE identifier that the
//!   commit it was written for had just deleted. It examined 0 names and
//!   printed green, while three live phantom identifiers sat in the file it
//!   guards - one of them already written down in a review doc a week earlier.
//! - `skip_marker_gate.sh` had its whole discriminating set removed by its own
//!   allowlist: 8 raw hits, 8 excused, 0 ruled on.
//! - `deploy_scripts_gate.sh`'s argv-secret scan had one pre-filter row, on the
//!   single service the filter excludes, so it examined 0.
//! - `compose_secret_strength_gate.sh` derived its rule set by regexing another
//!   file's refusal message; the message improved and the rule set went to 0.
//!   ITS ANTI-VACUITY GUARD FIRED, so it went red rather than falsely green.
//!   That is the counter-example proving the fix works - and the only reason
//!   anybody went looking for the other three.
//!
//! One shape: A CHECK THAT EXAMINES NOTHING AND A CLEAN TREE PRINT THE SAME
//! THING.
//!
//! # Why this is not itself a census
//!
//! The obvious meta-gate holds a table of expected per-gate counts and fails
//! when one drifts. That table BECOMES THE DEFECT IT IS FIXING: it is a census,
//! it goes stale, and four gates in this repo went red the same week purely
//! because a new crate landed and each held a list a new crate is by
//! construction absent from.
//!
//! So the floor for each arm lives IN THAT ARM'S GATE, beside the code that
//! produces the number, maintained by whoever changes that code
//! (`tests/lib/gate_arms.sh` enforces it at the point of measurement, which is
//! also where the diagnosis is). This module checks the PROPERTY, never the
//! VALUES: that every gate participates in the contract at all. There is
//! exactly one number here - the enumeration floor - and it counts gate FILES,
//! which are added and deleted deliberately, not scraped out of a corpus.
//!
//! # What this misses, stated so nobody reads it as complete
//!
//! - It cannot tell whether an arm's declared count is the RIGHT count. An arm
//!   that declares its pre-filter total instead of the number it ruled on
//!   passes here and is exactly the `skip_marker_gate` failure.
//! - It cannot tell whether a floor is well chosen. A floor of 1 on an arm
//!   that should see 400 passes here.
//! - It is static. An arm whose declaration sits behind a `case` that stopped
//!   matching never executes; this module sees the source and is content. The
//!   `--run` mode of the binary closes that for gates it is given, and CI
//!   closes it for the rest by running each gate as its own step, where
//!   `gate_arms_finish` refuses on zero declared arms.

use std::collections::BTreeSet;

use crate::report::Report;

/// One gate script, as read from disk.
#[derive(Debug)]
pub struct GateFile {
    /// Display name - the basename, so a failure line points at a file.
    pub name: String,
    /// The script's full source.
    pub source: String,
}

/// One `gate_arm` call found in a gate's source.
#[derive(Debug, PartialEq, Eq)]
pub struct ArmCall {
    /// The arm id, first argument.
    pub arm: String,
    /// The floor argument as written: `Some(n)` when it is a literal integer,
    /// `None` when it is a variable this module cannot evaluate.
    pub floor: Option<i64>,
}

/// The pseudo-arm recorded for a shim that delegates to a gate implemented
/// elsewhere (today: a Rust binary).
///
/// IT IS NOT AN EXEMPTION, and the distinction is load-bearing, because an
/// allowlist that excused the interesting cases is how `skip_marker_gate.sh`
/// came to rule on nothing. A shim satisfies the STATIC half of the contract
/// here and is held to the dynamic half at runtime: `gate_arms_delegate` in
/// `tests/lib/gate_arms.sh` refuses unless the delegate emits at least one real
/// `zsgate-arm` line, on every run. The id is deliberately not a valid shell
/// arm id, so it cannot collide with one a gate declares.
pub const DELEGATE_ARM: &str = "<delegate>";

/// Extract the `gate_arm` calls from a shell script.
///
/// A call counts only when `gate_arm` is the command being run: first token of
/// the line, or immediately after `if`, `elif`, `!`, `if !` or `&&`/`||`. A
/// gate's own prose about `gate_arm` is not a call, and neither is
/// `gate_arms_init` or `gate_arms_finish`, which share the prefix.
#[must_use]
pub fn arm_calls(source: &str) -> Vec<ArmCall> {
    let mut calls = Vec::new();
    for line in source.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with('#') {
            continue;
        }
        // Strip the control-flow words a call may hide behind, one at a time,
        // so `if ! gate_arm ...` is seen. Anything else in front means this is
        // not a bare invocation.
        let mut rest = trimmed;
        loop {
            let stripped = ["if ", "elif ", "! ", "&& ", "|| ", "then "]
                .iter()
                .find_map(|prefix| rest.strip_prefix(*prefix));
            match stripped {
                Some(next) => rest = next.trim_start(),
                None => break,
            }
        }
        if rest.starts_with("gate_arms_delegate ") {
            calls.push(ArmCall {
                arm: DELEGATE_ARM.to_owned(),
                floor: None,
            });
            continue;
        }
        let Some(args) = rest.strip_prefix("gate_arm ") else {
            continue;
        };
        let mut fields = args.split_whitespace();
        let Some(arm) = fields.next() else { continue };
        // Second field is the examined count, always a variable in practice
        // and never checkable from here.
        let _examined = fields.next();
        // `gate_arm x "$n" 3; then` and `gate_arm x "$n" 3 || exit 1` both put
        // shell punctuation against the floor token. Strip it before parsing;
        // otherwise a literal floor reads as "a variable I cannot evaluate" and
        // the floor-of-zero check silently stops applying to every call written
        // in the `if !` form - a filter that excuses exactly the cases it was
        // built to rule on, which is this repo's founding bug.
        let floor = fields.next().and_then(|raw| {
            raw.trim_matches(|c: char| c == '"' || c == '\'' || c == ';' || c.is_whitespace())
                .parse::<i64>()
                .ok()
        });
        calls.push(ArmCall {
            arm: arm.trim_matches(|c| c == '"' || c == '\'').to_owned(),
            floor,
        });
    }
    calls
}

/// Audit a set of gate scripts against the arm contract.
///
/// `enumeration_floor` is the number of gate files below which this refuses
/// outright: a meta-gate handed an empty or truncated list of gates checks
/// nothing and would otherwise print the same green as a clean tree. The gates
/// are passed IN rather than globbed here so the empty case is reachable from a
/// test instead of only by deleting files.
#[must_use]
pub fn audit(gates: &[GateFile], enumeration_floor: usize) -> Report {
    let mut report = Report::new();

    if gates.len() < enumeration_floor {
        report.refuse(format!(
            "enumerated {} gate script(s), below the floor of {enumeration_floor}. Either the \
             glob stopped matching or gates were deleted; a meta-gate that checks zero gates is \
             the joke version of this check, so this is a refusal and not a pass. If gates were \
             deliberately removed, lower the floor in the same commit that removes them.",
            gates.len()
        ));
        return report;
    }

    for gate in gates {
        let calls = arm_calls(&gate.source);
        let sources_lib = gate.source.contains("lib/gate_arms.sh");
        let finishes = gate.source.contains("gate_arms_finish");
        let inits = gate.source.contains("gate_arms_init");

        if calls.is_empty() {
            report.fail(format!(
                "{}: declares no arms. Every gate must state, per arm, how many items that arm \
                 ruled on and the floor that number must clear - see tests/lib/gate_arms.sh. \
                 Without it, an arm whose enumeration collapses to zero prints the same green as \
                 a clean tree.",
                gate.name
            ));
            continue;
        }

        let mut problems: Vec<String> = Vec::new();
        if !sources_lib {
            problems.push("does not source tests/lib/gate_arms.sh".to_owned());
        }
        if !inits {
            problems.push("never calls gate_arms_init".to_owned());
        }
        if !finishes {
            problems.push(
                "never calls gate_arms_finish, so its arms' refusals reach no exit code".to_owned(),
            );
        }

        // A floor of zero is a DECLARED VACUITY - it says out loud that this
        // arm may rule on nothing and still pass. The shell library refuses it
        // too; checking it here as well means a gate CI happens not to run
        // cannot carry one.
        for call in &calls {
            match call.floor {
                Some(floor) if floor < 1 => problems.push(format!(
                    "arm '{}' declares a floor of {floor}; a floor under 1 permits an arm that \
                     examines nothing to pass",
                    call.arm
                )),
                _ => {}
            }
        }

        // A repeated arm id means one arm is reporting another's count.
        //
        // LITERAL IDS ONLY. A gate that loops over a set of subjects writes
        // `gate_arm "$compose_arm" ...`, and two such calls in a helper are two
        // DIFFERENT arms at runtime with the same spelling in the source.
        // Reporting that as a duplicate would fail a correct gate, and the
        // lesson this repo keeps relearning is that a check which cries wolf
        // gets weakened until it means nothing. Runtime duplicates are caught
        // where the values exist: `gate_arm` in tests/lib/gate_arms.sh refuses
        // an id it has already seen in that run.
        let mut seen: BTreeSet<&str> = BTreeSet::new();
        for call in calls.iter().filter(|call| !call.arm.contains('$')) {
            if !seen.insert(call.arm.as_str()) {
                problems.push(format!(
                    "arm '{}' is declared twice; two arms sharing an id means one is vouching for \
                     the other's count",
                    call.arm
                ));
            }
        }

        if problems.is_empty() {
            report.pass(format!(
                "{}: {} arm(s) declared with a floor each",
                gate.name,
                calls.len()
            ));
        } else {
            report.fail(format!("{}: {}", gate.name, problems.join("; ")));
        }
    }

    report
}

/// One arm a RUST gate ruled on, rendered in the wire format the shell library
/// emits.
///
/// A gate written in Rust owes the same account as a shell one, and it has to
/// be the SAME account: `tests/compose_secret_strength_gate.sh` is a shim over
/// a Rust binary, and `gate_arms_delegate` refuses unless the binary emits
/// these lines. Two spellings of the census would let the shim be satisfied by
/// something the meta-gate cannot read.
#[derive(Debug)]
pub struct Arm<'a> {
    /// Gate id, matching what `gate_arms_init` would take.
    pub gate: &'a str,
    /// Arm id.
    pub arm: &'a str,
    /// Items this arm ruled on.
    pub examined: usize,
    /// The floor that number must clear.
    pub floor: usize,
}

impl Arm<'_> {
    /// The census line, without a trailing newline.
    #[must_use]
    pub fn line(&self) -> String {
        format!(
            "zsgate-arm gate={} arm={} examined={} floor={}",
            self.gate, self.arm, self.examined, self.floor
        )
    }

    /// Whether the arm had enough to rule on to mean anything.
    #[must_use]
    pub const fn cleared(&self) -> bool {
        self.examined >= self.floor && self.floor >= 1
    }
}

/// One `zsgate-arm` census line, as emitted by a gate at runtime.
#[derive(Debug, PartialEq, Eq)]
pub struct EmittedArm {
    /// Gate id from `gate_arms_init`.
    pub gate: String,
    /// Arm id.
    pub arm: String,
    /// Items the arm ruled on.
    pub examined: i64,
    /// The floor it had to clear.
    pub floor: i64,
}

/// Parse the `zsgate-arm` lines out of a gate's captured output.
///
/// The line is a WIRE FORMAT, pinned by `tests/lib_gate_arms_selftest.sh` on
/// the producing side. It is deliberately not prose: the compose-secret failure
/// above was caused by one program regexing another program's human-readable
/// message, and this is the same relationship one level up.
#[must_use]
pub fn parse_emissions(output: &str) -> Vec<EmittedArm> {
    let mut out = Vec::new();
    for line in output.lines() {
        let Some(rest) = line.trim().strip_prefix("zsgate-arm ") else {
            continue;
        };
        let mut gate = None;
        let mut arm = None;
        let mut examined = None;
        let mut floor = None;
        for field in rest.split_whitespace() {
            let Some((key, value)) = field.split_once('=') else {
                continue;
            };
            match key {
                "gate" => gate = Some(value.to_owned()),
                "arm" => arm = Some(value.to_owned()),
                "examined" => examined = value.parse::<i64>().ok(),
                "floor" => floor = value.parse::<i64>().ok(),
                _ => {}
            }
        }
        if let (Some(gate), Some(arm), Some(examined), Some(floor)) = (gate, arm, examined, floor) {
            out.push(EmittedArm {
                gate,
                arm,
                examined,
                floor,
            });
        }
    }
    out
}

/// Rule on the arms a gate actually emitted at runtime.
///
/// Separate from [`audit`] because it answers a different question: `audit`
/// asks whether a gate DECLARES its arms, this asks whether the arms it ran
/// had anything to rule on. A gate whose declaration sits behind a branch that
/// stopped being taken passes the first and is caught here.
///
/// `emitted` empty is a refusal: a gate that ran and emitted no census line is
/// indistinguishable, from here, from a gate that was never invoked.
#[must_use]
pub fn rule_on_emissions(gate_name: &str, emitted: &[EmittedArm]) -> Report {
    let mut report = Report::new();
    if emitted.is_empty() {
        report.refuse(format!(
            "{gate_name} emitted no zsgate-arm line. Either it does not participate in the arm \
             contract, or the run did not reach any arm - both mean this tells you nothing about \
             the tree."
        ));
        return report;
    }
    for arm in emitted {
        if arm.examined < arm.floor {
            report.fail(format!(
                "{}/{} ruled on {} item(s), floor {}. The arm's enumeration collapsed; its clean \
                 result says nothing.",
                arm.gate, arm.arm, arm.examined, arm.floor
            ));
        } else {
            report.pass(format!(
                "{}/{} ruled on {} item(s), floor {}",
                arm.gate, arm.arm, arm.examined, arm.floor
            ));
        }
    }
    report
}

#[cfg(test)]
mod tests {
    use super::{arm_calls, audit, parse_emissions, rule_on_emissions, ArmCall, EmittedArm, GateFile};
    use crate::report::Verdict;

    fn gate(name: &str, source: &str) -> GateFile {
        GateFile {
            name: name.to_owned(),
            source: source.to_owned(),
        }
    }

    const COMPLIANT: &str = r#"
. "$ROOT/tests/lib/gate_arms.sh"
gate_arms_init widget
gate_arm citations "$n" 40
gate_arms_finish || exit 1
"#;

    /// THE ANTI-VACUITY GUARD FOR THE META-GATE ITSELF. Handed no gates it must
    /// refuse, because a meta-gate that checks zero gates prints exactly what a
    /// clean repository prints.
    #[test]
    fn an_empty_gate_list_is_a_refusal_not_a_pass() {
        let report = audit(&[], 3);
        assert!(matches!(report.verdict(), Verdict::Refused(_)));
        assert_eq!(report.verdict().exit_code(), 1);
    }

    /// One variable different from the case above - the list is no longer
    /// short - and it is green. Without this pair the refusal proves only that
    /// the function RAN.
    #[test]
    fn a_full_gate_list_of_compliant_gates_is_green() {
        let gates = [
            gate("a_gate.sh", COMPLIANT),
            gate("b_gate.sh", COMPLIANT),
            gate("c_gate.sh", COMPLIANT),
        ];
        assert_eq!(audit(&gates, 3).verdict(), Verdict::Green { checks: 3 });
    }

    /// The enumeration SHRINKING is the failure mode, not just emptiness: a
    /// glob that half-matches is the likelier accident.
    #[test]
    fn a_shrunken_gate_list_is_a_refusal() {
        let gates = [gate("a_gate.sh", COMPLIANT), gate("b_gate.sh", COMPLIANT)];
        let report = audit(&gates, 3);
        match report.verdict() {
            Verdict::Refused(reason) => assert!(reason.contains("below the floor of 3")),
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn a_gate_that_declares_no_arms_is_named() {
        let gates = [
            gate("a_gate.sh", COMPLIANT),
            gate("silent_gate.sh", "echo hello\nexit 0\n"),
            gate("c_gate.sh", COMPLIANT),
        ];
        let report = audit(&gates, 3);
        assert_eq!(
            report.verdict(),
            Verdict::Red {
                checks: 3,
                failures: 1
            }
        );
        let named = report
            .checks()
            .iter()
            .filter(|check| !check.ok)
            .any(|check| check.detail.starts_with("silent_gate.sh: declares no arms"));
        assert!(named, "the failing gate must be named: {:?}", report.checks());
    }

    #[test]
    fn a_declared_floor_of_zero_is_refused() {
        let gates = [
            gate("a_gate.sh", COMPLIANT),
            gate(
                "lax_gate.sh",
                ". \"$ROOT/tests/lib/gate_arms.sh\"\ngate_arms_init lax\ngate_arm thing \"$n\" 0\ngate_arms_finish\n",
            ),
            gate("c_gate.sh", COMPLIANT),
        ];
        let report = audit(&gates, 3);
        let named = report
            .checks()
            .iter()
            .filter(|check| !check.ok)
            .any(|check| check.detail.contains("lax_gate.sh") && check.detail.contains("floor of 0"));
        assert!(named, "{:?}", report.checks());
    }

    #[test]
    fn a_gate_that_never_finishes_leaks_its_refusals() {
        let gates = [
            gate("a_gate.sh", COMPLIANT),
            gate(
                "leaky_gate.sh",
                ". \"$ROOT/tests/lib/gate_arms.sh\"\ngate_arms_init leaky\ngate_arm thing \"$n\" 3\n",
            ),
            gate("c_gate.sh", COMPLIANT),
        ];
        let report = audit(&gates, 3);
        let named = report.checks().iter().any(|check| {
            !check.ok && check.detail.contains("leaky_gate.sh") && check.detail.contains("gate_arms_finish")
        });
        assert!(named, "{:?}", report.checks());
    }

    #[test]
    fn a_duplicated_arm_id_is_named() {
        let gates = [
            gate("a_gate.sh", COMPLIANT),
            gate(
                "dup_gate.sh",
                ". \"$ROOT/tests/lib/gate_arms.sh\"\ngate_arms_init dup\ngate_arm x \"$n\" 3\ngate_arm x \"$m\" 3\ngate_arms_finish\n",
            ),
            gate("c_gate.sh", COMPLIANT),
        ];
        let report = audit(&gates, 3);
        let named = report
            .checks()
            .iter()
            .any(|check| !check.ok && check.detail.contains("declared twice"));
        assert!(named, "{:?}", report.checks());
    }

    /// A gate that declares one arm per subject in a loop writes the same
    /// SOURCE line twice with a different runtime value. Reporting that as a
    /// duplicate would fail `config_name_alignment_gate.sh`, which does exactly
    /// this over seven subjects.
    #[test]
    fn a_variable_arm_id_written_twice_is_not_a_duplicate() {
        let looped = ". \"$ROOT/tests/lib/gate_arms.sh\"\ngate_arms_init loop\n\
                      gate_arm \"$arm\" \"$n\" 5\ngate_arm \"$arm\" \"$m\" 5\ngate_arms_finish\n";
        let gates = [
            gate("a_gate.sh", COMPLIANT),
            gate("loop_gate.sh", looped),
            gate("c_gate.sh", COMPLIANT),
        ];
        assert_eq!(audit(&gates, 3).verdict(), Verdict::Green { checks: 3 });

        // ONE VARIABLE: the ids are literal, so the repetition is real and
        // one arm's count would be vouching for the other's.
        let literal = ". \"$ROOT/tests/lib/gate_arms.sh\"\ngate_arms_init loop\n\
                       gate_arm arm \"$n\" 5\ngate_arm arm \"$m\" 5\ngate_arms_finish\n";
        let gates = [
            gate("a_gate.sh", COMPLIANT),
            gate("loop_gate.sh", literal),
            gate("c_gate.sh", COMPLIANT),
        ];
        assert!(audit(&gates, 3)
            .checks()
            .iter()
            .any(|check| !check.ok && check.detail.contains("declared twice")));
    }

    /// Prose about the contract must not be mistaken for participation in it -
    /// otherwise a gate could satisfy this meta-gate with a comment, which is
    /// the class of defect the whole exercise is about.
    #[test]
    fn a_comment_mentioning_the_helper_is_not_a_declaration() {
        let gates = [
            gate("a_gate.sh", COMPLIANT),
            gate(
                "commented_gate.sh",
                "# gate_arm citations \"$n\" 40 -- one day\n. \"$ROOT/tests/lib/gate_arms.sh\"\ngate_arms_init c\ngate_arms_finish\n",
            ),
            gate("c_gate.sh", COMPLIANT),
        ];
        let report = audit(&gates, 3);
        let named = report
            .checks()
            .iter()
            .any(|check| !check.ok && check.detail.contains("commented_gate.sh"));
        assert!(named, "{:?}", report.checks());
    }

    /// A shim over a Rust gate participates through `gate_arms_delegate`, and
    /// the pseudo-arm is what the static half can see. Paired below with the
    /// case that must still fail, so this is not a hole.
    #[test]
    fn a_delegating_shim_satisfies_the_static_contract() {
        let shim = ". \"$ROOT/tests/lib/gate_arms.sh\"\ngate_arms_init shim\n\
                    gate_arms_delegate cargo run --bin thing\ngate_arms_finish\n";
        let gates = [
            gate("a_gate.sh", COMPLIANT),
            gate("shim_gate.sh", shim),
            gate("c_gate.sh", COMPLIANT),
        ];
        assert_eq!(audit(&gates, 3).verdict(), Verdict::Green { checks: 3 });

        // ONE VARIABLE: the same shim without the delegate call. It execs the
        // binary directly, owes no account, and must fail - otherwise every
        // gate could opt out by becoming a shim.
        let bare = ". \"$ROOT/tests/lib/gate_arms.sh\"\ngate_arms_init shim\n\
                    exec cargo run --bin thing\n";
        let gates = [
            gate("a_gate.sh", COMPLIANT),
            gate("shim_gate.sh", bare),
            gate("c_gate.sh", COMPLIANT),
        ];
        let report = audit(&gates, 3);
        assert!(report
            .checks()
            .iter()
            .any(|check| !check.ok && check.detail.starts_with("shim_gate.sh: declares no arms")));
    }

    #[test]
    fn a_call_behind_if_not_is_still_a_call() {
        let calls = arm_calls("if ! gate_arm citations \"$n\" 40; then\n  fail x\nfi\n");
        assert_eq!(
            calls,
            vec![ArmCall {
                arm: "citations".to_owned(),
                floor: Some(40)
            }]
        );
    }

    #[test]
    fn a_variable_floor_parses_as_unknown_rather_than_zero() {
        let calls = arm_calls("gate_arm citations \"$n\" \"$FLOOR\"\n");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].floor, None);
        // And an unknown floor is NOT reported as a floor of zero: that would
        // fail every gate that names its floor, and the lesson of this repo is
        // that a gate crying wolf gets its floor lowered until it means nothing.
        let gates = [
            gate("a_gate.sh", COMPLIANT),
            gate(
                "var_gate.sh",
                ". \"$ROOT/tests/lib/gate_arms.sh\"\ngate_arms_init v\ngate_arm x \"$n\" \"$FLOOR\"\ngate_arms_finish\n",
            ),
            gate("c_gate.sh", COMPLIANT),
        ];
        assert_eq!(audit(&gates, 3).verdict(), Verdict::Green { checks: 3 });
    }

    /// The two halves of the wire format pinned against each other. A Rust
    /// gate that renders a line the parser cannot read is a delegate that
    /// silently stops participating - the same relationship, one level up, as
    /// the compose-secret gate regexing another program's prose.
    #[test]
    fn a_rendered_arm_line_parses_back_to_itself() {
        let arm = super::Arm {
            gate: "compose_secret_strength",
            arm: "secret_rules",
            examined: 8,
            floor: 4,
        };
        assert_eq!(
            parse_emissions(&arm.line()),
            vec![EmittedArm {
                gate: "compose_secret_strength".to_owned(),
                arm: "secret_rules".to_owned(),
                examined: 8,
                floor: 4,
            }]
        );
        assert!(arm.cleared());

        // One variable: the same arm with nothing to rule on.
        let empty = super::Arm {
            examined: 0,
            ..arm
        };
        assert!(!empty.cleared());
    }

    #[test]
    fn emissions_parse_to_their_fields() {
        let out = "ws subscription stub gate\n\
                   zsgate-arm gate=ws arm=citations examined=83 floor=40\n\
                   zsgate-arm gate=ws arm=doc examined=2 floor=2\n\
                   zsgate-arms gate=ws arms=2 refusals=0\n";
        assert_eq!(
            parse_emissions(out),
            vec![
                EmittedArm {
                    gate: "ws".to_owned(),
                    arm: "citations".to_owned(),
                    examined: 83,
                    floor: 40
                },
                EmittedArm {
                    gate: "ws".to_owned(),
                    arm: "doc".to_owned(),
                    examined: 2,
                    floor: 2
                },
            ]
        );
    }

    #[test]
    fn a_run_that_emitted_nothing_is_a_refusal() {
        let report = rule_on_emissions("some_gate.sh", &[]);
        assert!(matches!(report.verdict(), Verdict::Refused(_)));
    }

    #[test]
    fn a_collapsed_arm_is_named_with_its_gate() {
        let emitted = [
            EmittedArm {
                gate: "ws_subscription_stub".to_owned(),
                arm: "comment_citations".to_owned(),
                examined: 0,
                floor: 40,
            },
            EmittedArm {
                gate: "ws_subscription_stub".to_owned(),
                arm: "doc_agreement".to_owned(),
                examined: 2,
                floor: 2,
            },
        ];
        let report = rule_on_emissions("ws_subscription_stub_gate.sh", &emitted);
        assert_eq!(
            report.verdict(),
            Verdict::Red {
                checks: 2,
                failures: 1
            }
        );
        assert!(report
            .checks()
            .iter()
            .any(|check| !check.ok
                && check
                    .detail
                    .starts_with("ws_subscription_stub/comment_citations ruled on 0")));
    }
}
