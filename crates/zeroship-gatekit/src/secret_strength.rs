//! Every secret `deploy/compose` supplies must satisfy the strength rule THE
//! PRODUCT ITSELF ENFORCES.
//!
//! THE DEFECT THIS EXISTS FOR, measured 2026-08-11 by bringing the stack up on
//! a fresh database. Three of five services could not start, and two of the
//! three failures were the same root cause: the compose file shipped
//! placeholder secrets shorter than the minimum the binaries refuse below.
//!
//! ```text
//! worker:  refusing to start with unsafe ZEROSHIP_WORKER_KEY
//!          ZEROSHIP_WORKER_KEY is too short (10 bytes); minimum 32 bytes
//! control: config: ZEROSHIP_WORKER_KEY / --worker-key: secret reference env
//!          var 'ZEROSHIP_WORKER_KEY' is not set
//! ```
//!
//! Both refusals are the PRODUCT BEHAVING CORRECTLY. The defect was entirely
//! in the shipped configuration.
//!
//! WHY THE RULE SET IS TYPED DATA, AND NOT DERIVED FROM PROSE. The shell gate
//! this replaces derived its rules by regexing the product's own refusal
//! MESSAGES out of `crates/core/src/config/secrets.rs`. That coupling is
//! unenforceable by construction, and it broke: `2c56e92a3` (2026-08-13)
//! replaced the baked-in variable name in those messages with a `{label}`
//! format parameter - a correct change, since one validator serves binaries
//! reading different variables - and the regex matched nothing from that day
//! on. The gate's anti-vacuity guard caught it, so it went red rather than
//! falsely green, but the invariant went unenforced for seven days.
//!
//! The rules now come from [`zeroship_core::config::PLATFORM_SECRETS`], a
//! const slice the product's own generator and validators read. A change to
//! the rules is a change to compiler-checked data, so this gate cannot
//! silently de-enumerate again. Whether each row states the floor its
//! validator applies is asserted in `crates/core` by
//! `platform_secret_rows_state_the_floor_their_validator_applies`.
//!
//! WHAT THIS DOES NOT CHECK, so a green is not over-read:
//!   - ENTROPY of a literal. Length is not strength; generated values are
//!     covered by the CLI generator tests, while this gate verifies the wiring
//!     that requires them.
//!   - secrets with NO floor (`SecretStrength::Unrestricted`). They are
//!     excluded from the rule set by construction and counted separately.
//!   - WHICH SERVICE gets which secret. That mapping is asserted by the CLI
//!     provisioning tests and ultimately by a stack bring-up.
//!   - files mounted as secrets (`*_FILE` variables pointing into
//!     `/etc/zeroship/secrets`). Their contents are not in the repository.

use zeroship_core::config::{PlatformSecret, SecretStrength};

use crate::compose::{ComposeFile, ComposeValue};
use crate::report::Report;

/// The subset of `rules` this gate can judge: rows that state a floor.
///
/// `Unrestricted` rows carry no length to compare against, so including them
/// would inflate the rule count with checks that cannot fail - the shape that
/// makes a floor meaningless.
#[must_use]
pub fn enforced_rules(rules: &'static [PlatformSecret]) -> Vec<&'static PlatformSecret> {
    rules
        .iter()
        .filter(|rule| rule.strength != SecretStrength::Unrestricted)
        .collect()
}

/// Judge `compose` against `rules`.
///
/// `rules` is a parameter rather than a hardcoded reference to
/// `PLATFORM_SECRETS` so the anti-vacuity guard is reachable from a test: pass
/// an empty slice and the report must REFUSE. The binary passes the real
/// table.
#[must_use]
pub fn run(compose: &ComposeFile, rules: &'static [PlatformSecret]) -> Report {
    let mut report = Report::new();

    let enforced = enforced_rules(rules);
    if enforced.is_empty() {
        report.refuse(
            "zeroship_core::config::PLATFORM_SECRETS yielded ZERO strength rules. Either the \
             table was emptied or every row lost its floor; a gate that checks nothing must not \
             report success.",
        );
        return report;
    }

    if compose.services.is_empty() {
        report.refuse(format!(
            "parsed ZERO services out of {}. The file is empty, or it is not the compose file \
             this gate was pointed at.",
            compose.path().display()
        ));
        return report;
    }

    for rule in enforced {
        let floor = match rule.strength {
            SecretStrength::RawBytes(n) => format!("{n}-byte minimum"),
            SecretStrength::DecodedBytes(n) => format!("{n}-decoded-byte minimum"),
            SecretStrength::Unrestricted => unreachable!("filtered by enforced_rules"),
        };

        let occurrences = compose.environment_occurrences(rule.env);
        if occurrences.is_empty() {
            report.pass(format!(
                "{} has a {floor} and compose supplies no value (nothing to check)",
                rule.env
            ));
            continue;
        }

        for (service, value) in occurrences {
            let Some(raw) = value else {
                report.fail(format!(
                    "{} in service '{service}' is declared with no value, so it passes through \
                     from the host and is unset in CI; require it with \
                     ${{{}:?run zeroship dev init}}",
                    rule.env, rule.env
                ));
                continue;
            };
            judge(&mut report, rule, &floor, service, raw);
        }
    }

    report
}

/// Judge one `(service, value)` occurrence of one rule.
fn judge(report: &mut Report, rule: &PlatformSecret, floor: &str, service: &str, raw: &str) {
    match ComposeValue::parse(raw) {
        // An interpolation is CONFIGURATION SYNTAX, never secret material.
        // Measuring its source text as a key is the mistake the shell gate
        // guarded against by exact-matching one blessed spelling; this checks
        // the property instead - it must refuse to start when unset, and it
        // must name its own variable.
        ComposeValue::Required { name, reason } => {
            if name != rule.env {
                report.fail(format!(
                    "{} in service '{service}' is required from a DIFFERENT variable '{name}'; \
                     the value an operator sets would never reach it",
                    rule.env
                ));
            } else if reason.is_empty() {
                report.fail(format!(
                    "{} in service '{service}' is required with an empty message; say what to run",
                    rule.env
                ));
            } else {
                report.pass(format!(
                    "{} is required from the environment in '{service}' ({floor}, no built-in \
                     weak default)",
                    rule.env
                ));
            }
        }
        // THE ARM THAT CATCHES THE 2026-08-11 DEFECT. A default IS shipped by
        // this file and is what runs whenever the variable is unset, so it is
        // measured exactly like a literal.
        ComposeValue::Defaulted { default, .. } => {
            measure(report, rule, floor, service, default, "built-in default");
        }
        ComposeValue::Substituted { name } => {
            report.fail(format!(
                "{} in service '{service}' substitutes ${{{name}}} with no default and no \
                 refusal; an unset {name} silently becomes the empty string",
                rule.env
            ));
        }
        ComposeValue::Composite(text) => {
            report.fail(format!(
                "{} in service '{service}' embeds an interpolation in surrounding text \
                 ('{text}'); a secret must be the whole value",
                rule.env
            ));
        }
        ComposeValue::Literal(literal) => {
            measure(report, rule, floor, service, literal, "literal");
        }
    }
}

/// Run the product's OWN validator on shipped material.
///
/// Not a length comparison re-implemented here: `rule.validate` is the
/// function the binary calls at boot, so this also refuses the known public
/// development values, which a length check would wave through.
fn measure(
    report: &mut Report,
    rule: &PlatformSecret,
    floor: &str,
    service: &str,
    material: &str,
    kind: &str,
) {
    match rule.validate(material) {
        Ok(()) => report.pass(format!(
            "{} {kind} in '{service}' is {} bytes and satisfies the {floor}",
            rule.env,
            material.len()
        )),
        Err(message) => report.fail(format!(
            "{} {kind} in '{service}' is REFUSED by the product's own check, so the service will \
             not start: {message}",
            rule.env
        )),
    }
}

#[cfg(test)]
mod tests {
    use zeroship_core::config::{PlatformSecret, MIN_SECRET_BYTES, PLATFORM_SECRETS};

    use super::{enforced_rules, run};
    use crate::compose::ComposeFile;
    use crate::report::Verdict;

    fn compose(worker_key: &str) -> ComposeFile {
        ComposeFile::from_yaml(&format!(
            "services:\n  worker:\n    environment:\n      ZEROSHIP_WORKER_KEY: {worker_key}\n"
        ))
        .expect("fixture parses")
    }

    /// THE ANTI-VACUITY GUARD, driven without editing the table. An empty rule
    /// set must REFUSE.
    #[test]
    fn an_empty_rule_table_refuses_rather_than_passing() {
        const NONE: &[PlatformSecret] = &[];
        let report = run(&compose("${ZEROSHIP_WORKER_KEY:?run zeroship dev init}"), NONE);
        match report.verdict() {
            Verdict::Refused(reason) => {
                assert!(
                    reason.contains("PLATFORM_SECRETS"),
                    "the refusal must name the table it read: {reason}"
                );
                assert!(reason.contains("ZERO strength rules"), "{reason}");
            }
            other => panic!("an empty rule table must refuse, got {other:?}"),
        }

        // The one-variable partner: the SAME compose file against the REAL
        // table is green, so the refusal above is about the empty rule set and
        // not about the fixture.
        assert!(matches!(
            run(
                &compose("${ZEROSHIP_WORKER_KEY:?run zeroship dev init}"),
                PLATFORM_SECRETS
            )
            .verdict(),
            Verdict::Green { .. }
        ));
    }

    /// THE REGRESSION TEST FOR THE 2026-08-11 OUTAGE: a literal below the
    /// floor must go RED and must NAME the key.
    #[test]
    fn a_literal_below_the_floor_is_red_and_names_the_key() {
        let report = run(&compose("dev-worker-key"), PLATFORM_SECRETS);
        assert!(
            matches!(report.verdict(), Verdict::Red { failures: 1, .. }),
            "got {:?}",
            report.verdict()
        );
        let failure = report
            .checks()
            .iter()
            .find(|check| !check.ok)
            .expect("one failure");
        assert!(failure.detail.contains("ZEROSHIP_WORKER_KEY"), "{failure:?}");
        assert!(failure.detail.contains("too short"), "{failure:?}");

        // The one-variable partner: the same literal one byte longer than the
        // floor passes, so the failure is about LENGTH and not about literals.
        assert!(matches!(
            run(&compose(&"a".repeat(MIN_SECRET_BYTES)), PLATFORM_SECRETS).verdict(),
            Verdict::Green { .. }
        ));
    }

    /// A `${VAR:-weak}` default is material this repository SHIPS. The shell
    /// gate measured it and so does this one; the arm exists because a default
    /// is the shape a placeholder secret takes once someone "fixes" a literal.
    #[test]
    fn a_weak_built_in_default_is_measured_not_waved_through() {
        let report = run(&compose("${ZEROSHIP_WORKER_KEY:-short}"), PLATFORM_SECRETS);
        assert!(matches!(report.verdict(), Verdict::Red { failures: 1, .. }));

        // The one-variable partner: the SAME interpolation with `:?` instead
        // of `:-` ships nothing and passes.
        assert!(matches!(
            run(
                &compose("${ZEROSHIP_WORKER_KEY:?run zeroship dev init}"),
                PLATFORM_SECRETS
            )
            .verdict(),
            Verdict::Green { .. }
        ));
    }

    /// A known PUBLIC development value that is long enough must still fail.
    /// A gate that only compared lengths would pass this.
    #[test]
    fn a_long_known_weak_value_still_fails() {
        let weak = "dev-stash-signing-key-not-for-production";
        assert!(weak.len() >= MIN_SECRET_BYTES);
        let file = ComposeFile::from_yaml(&format!(
            "services:\n  gateway:\n    environment:\n      \
             ZEROSHIP_GATEWAY_STASH_SIGNING_KEY: {weak}\n"
        ))
        .expect("fixture parses");
        assert!(matches!(
            run(&file, PLATFORM_SECRETS).verdict(),
            Verdict::Red { failures: 1, .. }
        ));
    }

    /// A required interpolation naming the WRONG variable reads as configured
    /// and supplies nothing. The shell gate caught this by exact-matching one
    /// blessed string; this catches it by comparing the name.
    #[test]
    fn a_required_interpolation_of_another_variable_fails() {
        let report = run(&compose("${SOME_OTHER_VAR:?run zeroship dev init}"), PLATFORM_SECRETS);
        assert!(matches!(report.verdict(), Verdict::Red { failures: 1, .. }));
        assert!(report
            .checks()
            .iter()
            .any(|c| !c.ok && c.detail.contains("DIFFERENT variable 'SOME_OTHER_VAR'")));
    }

    #[test]
    fn a_bare_substitution_fails_because_unset_means_empty() {
        let report = run(&compose("${ZEROSHIP_WORKER_KEY}"), PLATFORM_SECRETS);
        assert!(matches!(report.verdict(), Verdict::Red { failures: 1, .. }));
    }

    /// An empty compose file must refuse, not report a vacuous green built out
    /// of "nothing to check" passes.
    #[test]
    fn a_compose_file_with_no_services_refuses() {
        let file = ComposeFile::from_yaml("services: {}\n").expect("parses");
        match run(&file, PLATFORM_SECRETS).verdict() {
            Verdict::Refused(reason) => assert!(reason.contains("ZERO services"), "{reason}"),
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn the_rule_set_excludes_rows_without_a_floor() {
        let enforced = enforced_rules(PLATFORM_SECRETS);
        assert!(!enforced.is_empty());
        assert!(enforced.len() < PLATFORM_SECRETS.len(), "some row has no floor");
        assert!(enforced.iter().any(|r| r.env == "ZEROSHIP_WORKER_KEY"));
        // Both stash keys are separate rows. The shell gate's regex produced
        // one un-prefixed `STASH_SIGNING_KEY` that matched NEITHER compose
        // key, so both went unchecked through its "nothing to check" arm.
        assert!(enforced
            .iter()
            .any(|r| r.env == "ZEROSHIP_GATEWAY_STASH_SIGNING_KEY"));
        assert!(enforced
            .iter()
            .any(|r| r.env == "ZEROSHIP_AUTH_STASH_SIGNING_KEY"));
        assert!(!enforced.iter().any(|r| r.env == "ZEROSHIP_CONTROL_KEY"));
    }
}
