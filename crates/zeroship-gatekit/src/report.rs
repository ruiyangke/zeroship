//! What a gate returns, and how it prints.
//!
//! THE RULE THIS ENCODES, taken from the shell harness it replaces: a gate has
//! THREE outcomes, not two. Green and red are the obvious pair; the third is
//! REFUSED - the gate could not run, or ran and found nothing to check. A gate
//! that treats "checked nothing" as green is the false-green shape this repo
//! has been burned by, so [`Report`] cannot express it: [`Report::verdict`]
//! refuses on zero checks and there is no way to ask it not to.

use std::fmt::Write as _;

/// One thing a gate looked at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    /// Whether it held.
    pub ok: bool,
    /// What was checked, in the operator's terms.
    pub detail: String,
}

/// A gate's accumulated result.
#[derive(Debug, Clone, Default)]
pub struct Report {
    checks: Vec<Check>,
    refusal: Option<String>,
}

/// A gate's final answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Every check held.
    Green {
        /// How many ran.
        checks: usize,
    },
    /// Checks ran and some failed.
    Red {
        /// How many ran.
        checks: usize,
        /// How many failed.
        failures: usize,
    },
    /// The gate did not produce a judgement. NOT a pass.
    Refused(String),
}

impl Verdict {
    /// The process exit code. Refused and red are both 1: a gate that could
    /// not check is not a gate that passed.
    #[must_use]
    pub const fn exit_code(&self) -> i32 {
        match self {
            Self::Green { .. } => 0,
            Self::Red { .. } | Self::Refused(_) => 1,
        }
    }
}

impl Report {
    /// An empty report.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a check that held.
    pub fn pass(&mut self, detail: impl Into<String>) {
        self.checks.push(Check {
            ok: true,
            detail: detail.into(),
        });
    }

    /// Record a check that did not hold.
    pub fn fail(&mut self, detail: impl Into<String>) {
        self.checks.push(Check {
            ok: false,
            detail: detail.into(),
        });
    }

    /// Record that the gate cannot judge at all. The FIRST refusal is kept:
    /// later ones are usually consequences of it.
    pub fn refuse(&mut self, reason: impl Into<String>) {
        if self.refusal.is_none() {
            self.refusal = Some(reason.into());
        }
    }

    /// Everything the gate looked at, in the order it looked.
    #[must_use]
    pub fn checks(&self) -> &[Check] {
        &self.checks
    }

    /// The verdict.
    ///
    /// An explicit refusal wins. Otherwise ZERO CHECKS IS A REFUSAL, never a
    /// green - the whole point of this type.
    #[must_use]
    pub fn verdict(&self) -> Verdict {
        if let Some(reason) = &self.refusal {
            return Verdict::Refused(reason.clone());
        }
        let failures = self.checks.iter().filter(|check| !check.ok).count();
        match (self.checks.len(), failures) {
            (0, _) => Verdict::Refused(
                "the gate ran and checked nothing; a gate that checks nothing must not report \
                 success"
                    .to_owned(),
            ),
            (checks, 0) => Verdict::Green { checks },
            (checks, failures) => Verdict::Red { checks, failures },
        }
    }

    /// Render the report the way the shell harness does, so a human reading CI
    /// output sees the same shape as every other gate in `tests/`.
    #[must_use]
    pub fn render(&self, title: &str) -> String {
        let mut out = String::new();
        let rule = "=".repeat(44);
        let _ = writeln!(out, "{rule}\n  {title}\n{rule}");
        for check in &self.checks {
            let _ = if check.ok {
                writeln!(out, "  ok   {}", check.detail)
            } else {
                writeln!(out, "  FAIL {}", check.detail)
            };
        }
        match self.verdict() {
            Verdict::Green { checks } => {
                let _ = writeln!(out, "\n  {checks} passed, 0 failed, {checks} ran");
            }
            Verdict::Red { checks, failures } => {
                let _ = writeln!(
                    out,
                    "\n  {} passed, {failures} failed, {checks} ran",
                    checks - failures
                );
            }
            Verdict::Refused(reason) => {
                let _ = writeln!(out, "\n  x REFUSED: {reason}");
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::{Report, Verdict};

    /// THE ANTI-VACUITY GUARD. The shell gate this replaces had one, it fired
    /// on 2026-08-13 when its rule regex went blind, and it is the only reason
    /// the breakage was visible instead of silently green. It is preserved
    /// here as a property of the type: there is no path from zero checks to
    /// `Green`.
    #[test]
    fn zero_checks_is_a_refusal_not_a_pass() {
        let empty = Report::new();
        assert!(matches!(empty.verdict(), Verdict::Refused(_)));
        assert_eq!(empty.verdict().exit_code(), 1);

        // The one-variable partner: the same report with ONE passing check is
        // green, so the refusal above is about emptiness and not about the
        // type refusing everything.
        let mut one = Report::new();
        one.pass("something");
        assert_eq!(one.verdict(), Verdict::Green { checks: 1 });
        assert_eq!(one.verdict().exit_code(), 0);
    }

    #[test]
    fn an_explicit_refusal_outranks_passing_checks() {
        let mut report = Report::new();
        report.pass("a");
        report.pass("b");
        report.refuse("the rule table is empty");
        report.refuse("a later, consequential reason");
        assert_eq!(
            report.verdict(),
            Verdict::Refused("the rule table is empty".to_owned())
        );
        assert!(report.render("t").contains("x REFUSED: the rule table is empty"));
    }

    #[test]
    fn a_single_failure_makes_the_whole_run_red() {
        let mut report = Report::new();
        report.pass("a");
        report.fail("b");
        assert_eq!(
            report.verdict(),
            Verdict::Red {
                checks: 2,
                failures: 1
            }
        );
        assert_eq!(report.verdict().exit_code(), 1);
        assert!(report.render("t").contains("1 passed, 1 failed, 2 ran"));
    }
}
