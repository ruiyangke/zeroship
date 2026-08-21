//! `deploy/` must not pass `--workflow-advance-unsigned` while the gateway's
//! internal workflow-advance edge is still unauthenticated.
//!
//! WHY THIS EXISTS, found 2026-08-12 by running
//! `tests/e2e_gateway_workflow_advance_authz.sh` end to end. Two facts were on
//! record in two different places, tracked as two different problems, and they
//! are one fact:
//!
//! ```text
//! PRODUCT  (the blocker that has no creator workaround):
//!          deploy/ passes --workflow-advance-unsigned nowhere and the worker
//!          403s without it, so a deployment as shipped in deploy/compose
//!          cannot advance a workflow at all.
//!
//! SECURITY (tasks #199 / #300 / #338): the gateway's
//!          /__zeroship/internal/workflow-advance edge has no authorization
//!          beyond a Host check, and takes app_id from the caller's JSON body,
//!          so the caller selects WHICH APP's workflow to advance.
//! ```
//!
//! Measured, both arms of the same switch, in one run:
//!
//! ```text
//! worker WITH the flag     -> HTTP 200 {"ack":true}, run queued -> completed,
//!                             step-rows 0 -> 1   (the #199 gap, reproduced)
//! worker WITHOUT the flag  -> HTTP 403 {"error":"workflow advance unsigned
//!                             disabled"}, run stayed queued
//! ```
//!
//! So the 403 that makes durable workflows unusable in the shipped deployment
//! is exactly what makes the #199 exploit unreachable in the shipped
//! deployment. THE OBVIOUS ONE-LINE FIX FOR THE BLOCKER ARMS THE EXPLOIT IN THE
//! SAME CHANGE. Nobody had written that down, because each side was recorded by
//! someone looking at only one of the two. See task #354.
//!
//! This gate exists so that coupling cannot be crossed silently. It does not
//! decide the fix - that is an operator call, and the recommendation on #354 is
//! to build the signed-advance path the flag's own doc says "replaces it in a
//! later durable-workflows task", which closes both at once.
//!
//! WHAT THIS DOES NOT CHECK, so a green is not over-read:
//!   - it CANNOT tell whether the gateway edge has since gained authorization.
//!     Detecting "this handler authenticates now" by text search is the kind of
//!     spelling-for-behaviour substitution that has produced wrong answers in
//!     this repo before. If the edge is fixed, this gate must be retired or
//!     rewritten BY A HUMAN, in the same commit as the fix. The deploy arm
//!     failing is a prompt to think, not proof that the change is wrong.
//!   - it does not run docker, does not boot a stack, and probes nothing. The
//!     behavioural claims above come from
//!     `tests/e2e_gateway_workflow_advance_authz.sh`, which is where they stay
//!     measured.
//!   - the backstop arm matches an error-message STRING. If the refusal is
//!     reworded but kept, it false-alarms; if it is deleted and the same string
//!     is written somewhere inert, it false-passes. It is a tripwire on the
//!     thing the probe asserts, not a proof the refusal still executes.

use std::path::{Path, PathBuf};

use crate::arm_census::GateRun;

/// The gate id, as it appears in every census line.
pub const GATE: &str = "compose_workflow_advance_flag";

/// The report title.
pub const TITLE: &str = "deploy/ does not arm the unsigned workflow-advance path";

/// The CLI flag that arms the unsigned path.
pub const FLAG: &str = "workflow-advance-unsigned";

/// The worker's refusal message, which is what makes the absence above
/// protective.
pub const BACKSTOP_STRING: &str = "workflow advance unsigned disabled";

/// How few files under `deploy/` means the scan lost its target.
///
/// The PASS state of the deploy arm is "zero hits" by design, so the hit count
/// cannot be that arm's examined-count: a scan that read no files also reports
/// zero hits. The count is what was actually searched.
///
/// MEASURED 2026-08-20 and re-measured 2026-08-21: 22 files under `deploy/`.
/// Floor well under that - adding or removing a handful of ops files should not
/// trip it, while the failure guarded against drops it to zero.
const DEPLOY_FILES_FLOOR: usize = 8;

/// How few lines in the backstop file means it is truncated rather than
/// changed.
///
/// This arm enumerates nothing the string match iterates over; it is a sanity
/// check that the file being read is a real source file and not an emptied one,
/// which a substring search alone cannot tell apart from a genuine absence of
/// the string.
///
/// MEASURED 2026-08-20 and re-measured 2026-08-21: 4039 lines in
/// `crates/worker/src/handler.rs`.
const BACKSTOP_LINES_FLOOR: usize = 500;

/// Every regular file under `dir`, recursively.
///
/// Symlinks are not followed and do not count, matching `find -type f`: a
/// symlink's target is either already enumerated or outside the tree being
/// ruled on.
fn files_under(dir: &Path) -> Result<Vec<PathBuf>, String> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_owned()];
    while let Some(current) = stack.pop() {
        let entries = std::fs::read_dir(&current)
            .map_err(|error| format!("cannot read {}: {error}", current.display()))?;
        for entry in entries {
            let entry =
                entry.map_err(|error| format!("cannot read an entry of {}: {error}", current.display()))?;
            let path = entry.path();
            let meta = std::fs::symlink_metadata(&path)
                .map_err(|error| format!("cannot stat {}: {error}", path.display()))?;
            if meta.is_dir() {
                stack.push(path);
            } else if meta.is_file() {
                out.push(path);
            }
        }
    }
    out.sort();
    Ok(out)
}

/// Judge the deploy tree and the worker backstop.
///
/// Both paths and both strings are parameters, so every arm's empty case is
/// reachable from a test rather than only by deleting repository files.
#[must_use]
pub fn run(deploy_dir: &Path, flag: &str, backstop_file: &Path, backstop_string: &str) -> GateRun {
    let mut run = GateRun::new(GATE);

    match files_under(deploy_dir) {
        Err(error) => {
            run.report_mut().refuse(format!(
                "{error} - this gate is pointed at a path that moved"
            ));
        }
        Ok(files) => {
            if run.arm("deploy_flag_scan", files.len(), DEPLOY_FILES_FLOOR) {
                // Every file, not only compose: the flag has no environment
                // binding today, so a CLI spelling in a `command:` is the only
                // known vector - but scanning the whole tree costs nothing and
                // catches a spelling nobody predicted.
                let mut hits = Vec::new();
                for path in &files {
                    let Ok(text) = std::fs::read_to_string(path) else {
                        continue;
                    };
                    for (number, line) in text.lines().enumerate() {
                        if line.contains(flag) {
                            hits.push(format!("{}:{}: {}", path.display(), number + 1, line.trim()));
                        }
                    }
                }
                if hits.is_empty() {
                    run.report_mut()
                        .pass(format!("no file under {} passes --{flag}", deploy_dir.display()));
                } else {
                    run.report_mut().fail(format!(
                        "{} arms the unsigned workflow-advance path, which ALSO makes the #199 \
                         gateway authz gap live on the public listener - they are the same flag \
                         (task #354). If the gateway edge now authenticates, retire this gate in \
                         the same commit and say so; it cannot detect that itself. Hits:\n       {}",
                        deploy_dir.display(),
                        hits.join("\n       ")
                    ));
                }
            }
        }
    }

    // The absence above only protects while the worker actually refuses. If the
    // backstop is deleted, "deploy/ has no flag" stops meaning anything and the
    // arm above would keep printing green over a live edge.
    match std::fs::read_to_string(backstop_file) {
        Err(error) => {
            run.report_mut().refuse(format!(
                "cannot read {}: {error} - the backstop cannot be checked",
                backstop_file.display()
            ));
        }
        Ok(text) => {
            if run.arm("worker_backstop", text.lines().count(), BACKSTOP_LINES_FLOOR) {
                if text.contains(backstop_string) {
                    run.report_mut().pass(format!(
                        "worker still refuses unsigned advance (\"{backstop_string}\")"
                    ));
                } else {
                    run.report_mut().fail(format!(
                        "the worker's unsigned-advance refusal is GONE from {}. The deploy arm is \
                         now vacuous: with no backstop, the absence of the flag in deploy/ protects \
                         nothing. Re-measure with tests/e2e_gateway_workflow_advance_authz.sh \
                         before shipping.",
                        backstop_file.display()
                    ));
                }
            }
        }
    }

    run
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::{run, BACKSTOP_STRING, FLAG};
    use crate::report::Verdict;

    /// A deploy tree of `files` files and a backstop of `lines` lines, both
    /// large enough to clear their floors unless a test says otherwise.
    struct Fixture {
        root: PathBuf,
    }

    impl Fixture {
        fn new(name: &str, files: usize, backstop_lines: usize, backstop_text: &str) -> Self {
            let root = std::env::temp_dir().join(format!("zsgate-wf-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(root.join("deploy/ops")).expect("mkdir");
            for index in 0..files {
                std::fs::write(
                    root.join(format!("deploy/ops/file-{index}.yml")),
                    "image: alpine\n",
                )
                .expect("write");
            }
            let mut backstop = String::new();
            for _ in 0..backstop_lines {
                backstop.push_str("// padding\n");
            }
            backstop.push_str(backstop_text);
            std::fs::write(root.join("handler.rs"), backstop).expect("write");
            Self { root }
        }

        fn deploy(&self) -> PathBuf {
            self.root.join("deploy")
        }

        fn backstop(&self) -> PathBuf {
            self.root.join("handler.rs")
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn a_deploy_tree_without_the_flag_and_a_live_backstop_is_green() {
        let fixture = Fixture::new(
            "clean",
            10,
            600,
            "return Err(\"workflow advance unsigned disabled\");\n",
        );
        let outcome = run(&fixture.deploy(), FLAG, &fixture.backstop(), BACKSTOP_STRING);
        assert_eq!(
            outcome.report().verdict(),
            Verdict::Green { checks: 2 },
            "{}",
            outcome.report().render("t")
        );
    }

    /// ONE VARIABLE from the green above: a single deploy file now passes the
    /// flag, and it must be named with its path and line.
    #[test]
    fn a_deploy_file_arming_the_flag_is_named() {
        let fixture = Fixture::new(
            "armed",
            10,
            600,
            "return Err(\"workflow advance unsigned disabled\");\n",
        );
        std::fs::write(
            fixture.deploy().join("ops/file-3.yml"),
            "command: zeroship-worker --workflow-advance-unsigned\n",
        )
        .expect("write");
        let outcome = run(&fixture.deploy(), FLAG, &fixture.backstop(), BACKSTOP_STRING);
        let named = outcome
            .report()
            .checks()
            .iter()
            .any(|check| !check.ok && check.detail.contains("file-3.yml:1:"));
        assert!(named, "{}", outcome.report().render("t"));
    }

    /// The other half of the coupling: the flag is absent from deploy/ but the
    /// worker no longer refuses, so the first arm's green means nothing.
    #[test]
    fn a_deleted_backstop_is_named_even_though_deploy_is_clean() {
        let fixture = Fixture::new("nobackstop", 10, 600, "// the refusal was deleted\n");
        let outcome = run(&fixture.deploy(), FLAG, &fixture.backstop(), BACKSTOP_STRING);
        assert!(outcome
            .report()
            .checks()
            .iter()
            .any(|check| !check.ok && check.detail.contains("refusal is GONE")));
    }

    /// An emptied deploy tree and a clean one both produce zero hits. Only the
    /// arm tells them apart, so it must refuse rather than pass.
    #[test]
    fn an_emptied_deploy_tree_refuses_rather_than_reporting_no_hits() {
        let fixture = Fixture::new("empty", 0, 600, "workflow advance unsigned disabled\n");
        let outcome = run(&fixture.deploy(), FLAG, &fixture.backstop(), BACKSTOP_STRING);
        match outcome.report().verdict() {
            Verdict::Refused(reason) => assert!(reason.contains("deploy_flag_scan")),
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    /// A truncated backstop file has no refusal string either, and looks
    /// exactly like a deleted refusal. The arm separates them.
    #[test]
    fn a_truncated_backstop_refuses_rather_than_claiming_the_refusal_is_gone() {
        let fixture = Fixture::new("truncated", 10, 3, "");
        let outcome = run(&fixture.deploy(), FLAG, &fixture.backstop(), BACKSTOP_STRING);
        match outcome.report().verdict() {
            Verdict::Refused(reason) => assert!(reason.contains("worker_backstop")),
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn a_missing_deploy_directory_is_a_refusal() {
        let outcome = run(
            Path::new("/nonexistent-deploy-tree-for-this-test"),
            FLAG,
            Path::new("/nonexistent-handler.rs"),
            BACKSTOP_STRING,
        );
        assert!(matches!(outcome.report().verdict(), Verdict::Refused(_)));
    }
}
