//! The four compose gates, run against the files this repository actually
//! ships.
//!
//! The unit tests in each module drive synthetic fixtures, which proves the
//! LOGIC. This proves the gates can still read the real tree: a model that
//! stopped parsing `deploy/compose/docker-compose.yml`, or a walk that stopped
//! finding `deploy/`, would leave every unit test green while the gate refused
//! in CI.
//!
//! Paths are relative because cargo runs a test binary with the working
//! directory set to its package root. No environment variable is involved.

use std::path::Path;

use zeroship_gatekit::arm_census::GateRun;
use zeroship_gatekit::backing_service_reach;
use zeroship_gatekit::compose::ComposeFile;
use zeroship_gatekit::port_exposure;
use zeroship_gatekit::report::Verdict;
use zeroship_gatekit::stateful_volume;
use zeroship_gatekit::workflow_advance_flag;

const COMPOSE: &str = "../../deploy/compose/docker-compose.yml";
const CADDYFILE: &str = "../../deploy/ops/Caddyfile";
const DEPLOY: &str = "../../deploy";
const WORKER_HANDLER: &str = "../../crates/worker/src/handler.rs";

fn shipped_compose() -> ComposeFile {
    let path = Path::new(COMPOSE);
    assert!(
        path.is_file(),
        "{COMPOSE} not found from the package root; the gates have nothing to check"
    );
    ComposeFile::load(path).expect("the shipped compose file must parse")
}

/// Every arm the run declared cleared its floor.
///
/// Asserted separately from the verdict because they are different questions: a
/// gate can be green about a tree it could not read, and the arms are the only
/// thing that says otherwise.
fn arms_cleared(run: &GateRun) -> bool {
    !run.arms().is_empty() && run.arms().iter().all(zeroship_gatekit::arm_census::Arm::cleared)
}

#[test]
fn the_shipped_stack_publishes_only_the_edge_on_all_interfaces() {
    let caddyfile = std::fs::read_to_string(CADDYFILE).expect("the shipped Caddyfile must be read");
    let run = port_exposure::run(&shipped_compose(), &caddyfile);
    assert!(arms_cleared(&run), "{}", run.census());
    assert!(
        matches!(run.report().verdict(), Verdict::Green { .. }),
        "{}{}",
        run.census(),
        run.report().render(port_exposure::TITLE)
    );
}

#[test]
fn the_shipped_stack_keeps_stateful_data_on_named_volumes() {
    let run = stateful_volume::run(&shipped_compose(), stateful_volume::STATEFUL);
    assert!(arms_cleared(&run), "{}", run.census());
    assert!(
        matches!(run.report().verdict(), Verdict::Green { .. }),
        "{}{}",
        run.census(),
        run.report().render(stateful_volume::TITLE)
    );
}

#[test]
fn the_shipped_deploy_tree_does_not_arm_the_unsigned_advance_path() {
    let run = workflow_advance_flag::run(
        Path::new(DEPLOY),
        workflow_advance_flag::FLAG,
        Path::new(WORKER_HANDLER),
        workflow_advance_flag::BACKSTOP_STRING,
    );
    assert!(arms_cleared(&run), "{}", run.census());
    assert!(
        matches!(run.report().verdict(), Verdict::Green { .. }),
        "{}{}",
        run.census(),
        run.report().render(workflow_advance_flag::TITLE)
    );
}

/// THE ONE THAT IS RED ON PURPOSE, and this test pins the redness.
///
/// Redpanda runs in the shipped compose file and nothing is configured to reach
/// it - the 43-day metering gap written up in
/// `docs/proposals/2026-08-20-metering-transport-not-configured.md`. A change
/// that turned this gate green without configuring the broker would have
/// deleted the only instrument in the tree that can see that gap, and would
/// look exactly like a successful port. So the expected outcome is asserted, not
/// the absence of failures.
///
/// WHEN YOU CONFIGURE THE BROKER, this test fails, and that failure is the
/// prompt to finish the job: flip the assertion to green, delete the "not in
/// CI" note in `backing_service_reach`, and add the gate to
/// `.github/workflows/ci.yml` - all in the same commit.
#[test]
fn the_shipped_stack_still_runs_a_broker_nobody_is_configured_to_reach() {
    let run = backing_service_reach::run(
        &shipped_compose(),
        backing_service_reach::EDGE_SERVICES,
        backing_service_reach::HOST_TOOL_SERVICES,
    );
    // The arms first: a gate whose enumeration collapsed reports every backing
    // service unreached, which would satisfy the assertion below for entirely
    // the wrong reason.
    assert!(arms_cleared(&run), "{}", run.census());

    let rendered = run.report().render(backing_service_reach::TITLE);
    assert_eq!(
        run.report().verdict(),
        Verdict::Red {
            checks: 5,
            failures: 1
        },
        "{rendered}"
    );
    assert!(
        run.report().checks().iter().any(|check| {
            !check.ok && check.detail.starts_with("redpanda RUNS AND NOBODY IS CONFIGURED TO REACH")
        }),
        "the one failure must be redpanda, by name: {rendered}"
    );
    // And the control: the OTHER backing services are reached, so the red above
    // is a finding about redpanda and not about the scanner.
    for reached in ["postgres", "redis"] {
        assert!(
            run.report()
                .checks()
                .iter()
                .any(|check| check.ok && check.detail.starts_with(&format!("{reached} is reached"))),
            "{reached} must be reported reached: {rendered}"
        );
    }
}
