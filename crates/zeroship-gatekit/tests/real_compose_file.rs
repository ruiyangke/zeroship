//! The gate, run against the compose file this repository actually ships.
//!
//! The unit tests in `secret_strength` drive synthetic fixtures, which proves
//! the LOGIC. This proves the gate can still read the real file: a model that
//! stopped parsing `deploy/compose/docker-compose.yml` would leave every unit
//! test green while the gate refused in CI.
//!
//! The path is relative because cargo runs a test binary with the working
//! directory set to its package root. No environment variable is involved.

use std::path::Path;

use zeroship_core::config::PLATFORM_SECRETS;
use zeroship_gatekit::compose::ComposeFile;
use zeroship_gatekit::report::Verdict;
use zeroship_gatekit::secret_strength::{enforced_rules, run};

const COMPOSE: &str = "../../deploy/compose/docker-compose.yml";

#[test]
fn the_shipped_compose_file_parses_and_passes() {
    let path = Path::new(COMPOSE);
    assert!(
        path.is_file(),
        "{COMPOSE} not found from the package root; the gate has nothing to check"
    );
    let compose = ComposeFile::load(path).expect("the shipped compose file must parse");

    // The model has to have SEEN something. A parse that yields an empty file
    // and a parse that yields the real one both "succeed".
    assert!(
        compose.services.len() >= 5,
        "only {} services parsed out of {COMPOSE}",
        compose.services.len()
    );

    let rules = enforced_rules(PLATFORM_SECRETS);
    assert!(
        !rules.is_empty(),
        "zero strength rules; the gate would check nothing"
    );

    let report = run(&compose, PLATFORM_SECRETS);
    let verdict = report.verdict();
    assert!(
        matches!(verdict, Verdict::Green { .. }),
        "{}",
        report.render("compose secrets")
    );

    // Every rule must have produced at least one check, so a rule whose name
    // stopped matching anything in compose cannot hide inside a green.
    assert!(
        report.checks().len() >= rules.len(),
        "{} checks for {} rules",
        report.checks().len(),
        rules.len()
    );

    // THE FAILURE MODE THE SHELL GATE COULD NOT SEE. Its rule regex produced
    // an UNPREFIXED `STASH_SIGNING_KEY`, which matches neither
    // ZEROSHIP_GATEWAY_STASH_SIGNING_KEY nor ZEROSHIP_AUTH_STASH_SIGNING_KEY
    // in compose. Both went unchecked through its "compose supplies no value"
    // arm, and that arm PASSES - so a name that stopped matching anything read
    // exactly like a name with nothing to check.
    //
    // Here the two are told apart: every enforced rule must be genuinely
    // SUPPLIED by the shipped compose file. That is a property of this
    // repository today (measured 2026-08-20: 6 of 6, 9 occurrences), not a
    // rule about compose files in general, which is why it is asserted here
    // and not inside the gate.
    for rule in &rules {
        assert!(
            !compose.environment_occurrences(rule.env).is_empty(),
            "{} carries a strength floor but {COMPOSE} supplies it nowhere; either the \
             deployment lost a secret or the name stopped matching",
            rule.env
        );
    }
}
