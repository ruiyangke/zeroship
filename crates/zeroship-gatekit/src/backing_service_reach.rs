//! Every backing service `deploy/compose` RUNS must be one that some compose
//! service is CONFIGURED TO REACH.
//!
//! THE DEFECT THIS EXISTS FOR, found 2026-08-20 and written up in
//! `docs/proposals/2026-08-20-metering-transport-not-configured.md`. Compose
//! declares `redpanda`, healthchecks it, gives it a persistent volume, and
//! `deploy/scripts/deploy-remote.sh` runs `docker compose up -d`, so ON THE
//! DEPLOYED HOST THE BROKER RUNS. Nothing publishes to it and nothing consumes
//! from it: the only two mentions of `METERING_BROKERS` in the compose file are
//! COMMENTS. The worker and gateway producers therefore take
//! `build_usage_outbox`'s `Ok(None)` arm and drain-and-drop every usage event,
//! control never spawns the forwarder or spend recompute, no app is ever
//! invoiced, and the spend ladder cannot fire because spend is structurally
//! zero. That had been true for 43 days when it was found - since `redpanda`
//! entered compose in `c30c3baac` (2026-07-08).
//!
//! THE SHAPE, and it is why a running broker is worse than an absent one:
//!
//! ```text
//! a broker nobody publishes to
//!   == a deployment with no broker
//!   == an app that served no traffic
//! ```
//!
//! Three states, one appearance. `docker compose ps` says healthy, `/readyz`
//! says ready, the creator's usage endpoint says zero, and the only signal is a
//! WARN line every 10 seconds in a service that logs at info. The comment beside
//! the service even asserts the wiring exists ("In compose that value is
//! `redpanda:9092`"), beside a file where that value appears nowhere.
//!
//! THIS GATE IS RED ON THE TRACKED TREE, deliberately, and is deliberately NOT
//! a step in `.github/workflows/ci.yml`. Redpanda is genuinely unreached, so a
//! correct gate fails today; softening it to land green would have made it a
//! gate that passed for the whole 43 days the gap existed. WIRE IT INTO ci.yml
//! IN THE SAME COMMIT THAT CONFIGURES THE BROKER, and not before: a red required
//! step on unrelated work teaches people to ignore it, which is a worse outcome
//! than the gap it reports.
//!
//! WHAT THIS DOES NOT CATCH, so a green is not over-read. This gate rules on
//! CONFIGURATION, and configuration is a PROXY for the thing that actually
//! matters, which is that usage flows. The gap between the two is not small:
//!
//!   - A service configured with a broker address that never publishes a single
//!     event passes here. The proxy is "is this service told how to reach that
//!     one", not "did a record land". This gate goes green the moment somebody
//!     adds one `environment:` line, with no event transported, ever.
//!   - It reads a parsed file. It does not run docker, does not resolve compose
//!     DNS, does not dial a port, and does not know whether the address is
//!     correct - only that the service name appears as a host in another
//!     service's configuration. `ZEROSHIP_METERING_BROKERS: redpanda:9999`
//!     passes.
//!   - The reach predicate needs `<service>:<port>`. A setting that names a host
//!     with no port, an IP literal, a network alias, or a value that arrives
//!     through a mounted credential FILE is invisible to it. That last one is
//!     live today, not hypothetical: `migrate` reaches postgres through
//!     `/etc/zeroship/secrets/migrate-dsn` and scores no edge here. It is only
//!     harmless because postgres has five other consumers - if brokers ever
//!     become a credential, the same blindness would apply to redpanda and this
//!     gate would report a false red.
//!   - `depends_on` is NOT counted; see [`CONFIG_EXCLUDED_KEYS`].
//!   - It says nothing about whether the broker, database or cache is correctly
//!     sized, reachable, healthy, or backed up.
//!
//! The honest claim is narrow: this gate distinguishes "compose runs a backing
//! service that no service was told about" from "compose runs a backing service
//! that at least one service was told about". As of 2026-08-21 that is exactly
//! the distinction nothing else in the tree can make.

use std::collections::BTreeMap;

use crate::arm_census::GateRun;
use crate::compose::ComposeFile;

/// The gate id, as it appears in every census line.
pub const GATE: &str = "compose_backing_service_reach";

/// The report title.
pub const TITLE: &str = "every backing service in deploy/compose is one somebody is configured to reach";

/// Service-body keys the reach scan is NOT allowed to see, each for a reason
/// that has a false positive behind it.
///
/// ```text
/// image:        `image: postgres:16` is the postgres NAME beside a NUMBER and
///               matches the predicate exactly. It is not reach.
/// volumes:      `../ops/postgres-init.sql:/docker-entrypoint-initdb.d/...`
///               likewise.
/// healthcheck:  a self-probe (`pg_isready -U postgres`), never reach.
/// ports:        publication, not reach - and `127.0.0.1:19092` is how the HOST
///               talks to redpanda, which is not a compose service.
/// networks:     aliases, which are names for OTHER services to use, not uses.
/// build:, deploy:, security_opt:  no host ever appears there.
/// ```
///
/// ORDERING IS NOT REACH. `depends_on` is start ordering and health gating, not
/// configuration, and this list settles the question rather than arguing it:
///   - it is not SUFFICIENT. Nothing `depends_on` redpanda today, so counting it
///     would not change today's verdict - but it WOULD make this gate go green
///     the day somebody adds `depends_on: redpanda` for start ordering, with no
///     address configured and no event transported.
///   - it is not NECESSARY. `migrate` names postgres in a `depends_on` AND reaches
///     it; `gateway` does the same for worker. Every real
///     consumer in this file with a `depends_on` entry also carries an
///     address.
///
/// Everything NOT listed here is scanned, including keys nobody has modelled -
/// see `Service::body_scalars`. An exclusion list that is a fixed set of
/// INCLUDED keys would go blind the day a host moved into a new key.
pub const CONFIG_EXCLUDED_KEYS: &[&str] = &[
    "build",
    "depends_on",
    "deploy",
    "healthcheck",
    "image",
    "networks",
    "ports",
    "security_opt",
    "volumes",
];

/// Services entered from OUTSIDE the compose network, so nothing inside names
/// them. Checked: must publish a port on all interfaces.
pub const EDGE_SERVICES: &[&str] = &["caddy"];

/// Services consumed by the HOST over a published loopback port.
///
/// Checked: must publish a loopback port AND have zero in-compose consumers -
/// the day a service is configured to reach one, it has become a backend and
/// this goes red saying so rather than quietly continuing to excuse it.
///
/// WHY VERDACCIO, since that is the one judgement call here:
/// `docs/runbooks/private-registry.md` publishes packages to it from the host
/// through `127.0.0.1:4873` and, where an in-network route is wanted, tells the
/// operator to attach the container to the SANDBOX project's network by hand.
/// No service in this file is meant to dial it. Redpanda is the opposite in
/// every respect: it advertises an in-compose address
/// (`internal://redpanda:9092`), the producers have a setting whose documented
/// compose value is exactly that address, and no service carries it.
pub const HOST_TOOL_SERVICES: &[&str] = &["verdaccio"];

/// How few services means the parse, not the stack, is what shrank.
///
/// MEASURED 2026-08-21: 11 services in the tracked compose file. Everything
/// below is derived from this list, so a parse that lost it would enumerate
/// nothing, find no backing services, rule on nothing, and print a clean green.
/// Floor well under 11: the stack would have to shed more than half its
/// services to reach it, while the failure guarded against drops it to 0.
const SERVICES_FLOOR: usize = 5;

/// How few reach edges anywhere in the file means the scanner broke.
///
/// THIS IS THE CONTROL ARM. It separates "redpanda is genuinely unreached" from
/// "the config scan broke". Those two produce opposite-looking output - a broken
/// scan reports every service unreached, which is loud - but a loud wrong answer
/// is still a wrong answer, and the one number that tells them apart is how many
/// edges the same scanner found elsewhere in the same file.
///
/// MEASURED 2026-08-21: 12 edges (postgres 5, control 2, auth 2, migrated 1,
/// worker 1, redis 1). Floor well under that: ordinary edits add or drop one
/// address, while a scan whose exclusion filter or body walk broke collapses it
/// to 0.
const REACH_EDGES_FLOOR: usize = 4;

/// How few non-built services means the `build:` derivation inverted.
///
/// MEASURED 2026-08-21: 5 services run a third-party image (verdaccio,
/// postgres, redis, redpanda, caddy); the other 6 share `deploy/Dockerfile`.
/// Floor well under 5, because the collapse this guards against is total: a
/// `build:` test that matched everything leaves 0 candidates, 0 backends, 0
/// verdicts and a green. The opposite break - matching nothing, so all 11 are
/// candidates - is caught instead by the role refusal, which names each one.
const CANDIDATES_FLOOR: usize = 2;

/// How few verdicts means a candidate fell through every branch.
///
/// MEASURED 2026-08-21: 5 candidates get a verdict - 3 backends plus 2 role
/// assertions. Equal to the candidate count BY ASSERTION, checked in [`run`],
/// and that equality is the point: one arm counts what was enumerated, the other
/// what was ruled on, and the gate is red when they differ.
const VERDICTS_FLOOR: usize = 3;

/// How few backends means the role lists swallowed a real one.
///
/// MEASURED 2026-08-21: 3 backends. Floor 2, which postgres and redis clear on
/// their own, so this arm cannot go vacuous while the platform still has a
/// database and a cache. Deleting redpanda is a legitimate way to fix this gate
/// and leaves 2, which still clears.
const BACKENDS_FLOOR: usize = 2;

/// Whether `text` names `target` as a host with a port.
///
/// The four live forms in the tracked file are all `<service-name>:<port>`
/// inside a value, whatever precedes it (`@`, `//`, `=`, a space):
///
/// ```text
/// control    ZEROSHIP_CONTROL_DATABASE_URL: ...@postgres:5432/zeroship
/// worker     ZEROSHIP_WORKER_KV_URL:        redis://redis:6379
/// gateway    command:  --control-url http://control:9090
/// control    ZEROSHIP_CONTROL_MIGRATED_URL: ${...:-http://migrated:9091}
/// ```
///
/// so the predicate is bound to the hazard - a name used as a host - and not to
/// one spelling of it. The character before must not be one that could make the
/// match a suffix of a longer name (`my-postgres:5432` is a different service).
fn names_host(text: &str, target: &str) -> bool {
    let needle = format!("{target}:");
    let mut from = 0;
    while let Some(offset) = text[from..].find(&needle) {
        let start = from + offset;
        let before_ok = start == 0
            || !text[..start]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-');
        let after_ok = text[start + needle.len()..]
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_digit());
        if before_ok && after_ok {
            return true;
        }
        from = start + needle.len();
    }
    false
}

/// The edge is entered from outside, so nothing inside names it. What it owes
/// instead is that it is still an entrance at all.
fn rule_on_edge(run: &mut GateRun, name: &str, ports: &[String]) {
    if ports.iter().any(|spec| !spec.starts_with("127.0.0.1:")) {
        run.report_mut().pass(format!(
            "{name} is the edge (publishes on all interfaces); reached from outside, not from \
             inside"
        ));
    } else {
        run.report_mut().fail(format!(
            "{name} is declared the edge but publishes nothing on all interfaces - either the role \
             is wrong or the edge stopped being the entrance"
        ));
    }
}

/// A host-tool must be reachable BY THE HOST and by nothing in compose. The day
/// a compose service is configured to reach it, it has become a backend.
fn rule_on_host_tool(run: &mut GateRun, name: &str, ports: &[String], consumers: &[&str]) {
    if !consumers.is_empty() {
        run.report_mut().fail(format!(
            "{name} is declared a host-tool but is now configured to be reached by: {} - it is a \
             backend; move it out of the host-tool role",
            consumers.join(" ")
        ));
    } else if ports.iter().any(|spec| spec.starts_with("127.0.0.1:")) {
        run.report_mut().pass(format!(
            "{name} is a host-tool (loopback port for the host; no compose service dials it)"
        ));
    } else {
        run.report_mut().fail(format!(
            "{name} is declared a host-tool but publishes no loopback port, so the host cannot \
             reach it either - nothing uses this service"
        ));
    }
}

/// A backend must have at least one consumer configured with its address.
fn rule_on_backend(run: &mut GateRun, name: &str, consumers: &[&str], platform: &[&str]) {
    if consumers.is_empty() {
        run.report_mut().fail(format!(
            "{name} RUNS AND NOBODY IS CONFIGURED TO REACH IT\n       compose declares '{name}', \
             starts it, healthchecks it and gives it a volume, and no service names '{name}:<port>' \
             in its command:, environment: or env_file:. A backing service nobody publishes to is \
             indistinguishable from an absent one and from an idle platform.\n       The services \
             that could reach it are the ones this repo builds: {}\n       Add the address the way \
             the backing services that PASS above are wired - an environment: entry on each \
             consumer whose value names the service as a host, e.g. worker's \
             ZEROSHIP_WORKER_KV_URL: redis://redis:6379. For redpanda specifically the producers \
             are the worker and the gateway (metering.brokers / ZEROSHIP_METERING_BROKERS) and the \
             consumer is control (control.stream_transport); see \
             docs/proposals/2026-08-20-metering-transport-not-configured.md. Otherwise delete the \
             service: running it costs memory, a volume and a healthcheck, and buys a comment that \
             says it is configured.",
            platform.join(" ")
        ));
    } else {
        run.report_mut().pass(format!(
            "{name} is reached by {} service(s): {}",
            consumers.len(),
            consumers.join(" ")
        ));
    }
}

/// Judge `compose`.
///
/// The role lists are parameters so a gate handed a file none of them describes
/// is reachable from a test, and so the refusal below - a role naming a service
/// that is not a candidate - can be exercised without editing this source.
#[must_use]
pub fn run(compose: &ComposeFile, edge: &[&str], host_tools: &[&str]) -> GateRun {
    let mut run = GateRun::new(GATE);

    let services: Vec<&str> = compose.services.keys().map(String::as_str).collect();
    if !run.arm("compose_services", services.len(), SERVICES_FLOOR) {
        return run;
    }

    let config: BTreeMap<&str, String> = compose
        .services
        .iter()
        .map(|(name, service)| {
            (
                name.as_str(),
                service.body_scalars(CONFIG_EXCLUDED_KEYS).join("\n"),
            )
        })
        .collect();

    // Scan every ordered pair and count the edges found ANYWHERE in the file,
    // not only the ones involving a backing service.
    let mut consumers_of: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    let mut edges = 0;
    for &target in &services {
        let mut who = Vec::new();
        for &consumer in &services {
            if consumer == target {
                continue;
            }
            if names_host(&config[consumer], target) {
                who.push(consumer);
                edges += 1;
            }
        }
        consumers_of.insert(target, who);
    }
    if !run.arm("reach_edges", edges, REACH_EDGES_FLOOR) {
        return run;
    }

    // DERIVED, not listed: a service this repository does not BUILD runs
    // somebody else's image and is infrastructure the platform sits on top of.
    // That is a property of the file, so a new third-party service is picked up
    // the day it lands. A hardcoded `postgres redis redpanda` list would not be:
    // it is a census, and a census cannot contain a service nobody has added
    // yet.
    let mut candidates = Vec::new();
    let mut platform = Vec::new();
    for &name in &services {
        if compose.services[name].builds() {
            platform.push(name);
        } else {
            candidates.push(name);
        }
    }
    if !run.arm("infra_candidates", candidates.len(), CANDIDATES_FLOOR) {
        return run;
    }

    // A role naming a service that no longer exists is the census failure
    // wearing the uniform of the fix: it excuses nothing, silently, forever.
    for declared in edge.iter().chain(host_tools.iter()) {
        if !candidates.contains(declared) {
            run.report_mut().refuse(format!(
                "a role is declared for '{declared}', which is not a non-built service in {}. \
                 Either it was renamed or removed, or it now has a build: stanza. Update the role \
                 lists in the same commit; a role pointing at nothing exempts nothing and hides it.",
                compose.path().display()
            ));
            return run;
        }
    }

    // Rule on every candidate. Exactly one verdict each, no silent skips: a
    // candidate named by NEITHER role is a BACKEND and must be reached. That
    // default is fail-closed on purpose, and it is the anti-rot property - a
    // third-party service added tomorrow is presumed to be something the
    // platform dials, and turns this gate red until either a service is
    // configured to reach it or somebody states what it is instead.
    let mut verdicts = 0;
    let mut backends = 0;
    for &name in &candidates {
        let ports = compose.services[name].published_ports();
        let consumers = &consumers_of[name];
        verdicts += 1;
        if edge.contains(&name) {
            rule_on_edge(&mut run, name, &ports);
        } else if host_tools.contains(&name) {
            rule_on_host_tool(&mut run, name, &ports, consumers);
        } else {
            backends += 1;
            rule_on_backend(&mut run, name, consumers, &platform);
        }
    }

    // BOUND TO THE CORPUS THIS RUN WAS HANDED, not to a number measured once.
    // The expectation is the candidate count from the arm above, so a candidate
    // that fell through every branch - the failure where a check quietly filters
    // its awkward cases out of its own expectation until the comparison always
    // balances - shows up as a mismatch rather than as a smaller green.
    if verdicts != candidates.len() {
        run.report_mut().fail(format!(
            "{verdicts} verdict(s) for {} candidate(s); one reached no branch, so nothing was said \
             about it",
            candidates.len()
        ));
    }

    run.arm("infra_verdicts", verdicts, VERDICTS_FLOOR);
    run.arm("backends_ruled", backends, BACKENDS_FLOOR);
    run
}

#[cfg(test)]
mod tests {
    use super::{names_host, run, EDGE_SERVICES, HOST_TOOL_SERVICES};
    use crate::compose::ComposeFile;
    use crate::report::Verdict;

    /// A miniature of the tracked stack: two built services, an edge, a
    /// host-tool, and two backends that ARE reached.
    const WIRED: &str = "\
services:
  caddy:
    image: caddy:2-alpine
    ports:
      - \"80:80\"
  verdaccio:
    image: verdaccio/verdaccio:6
    ports:
      - \"127.0.0.1:4873:4873\"
  postgres:
    image: postgres:16
    ports:
      - \"127.0.0.1:5440:5432\"
    healthcheck:
      test: [\"CMD-SHELL\", \"pg_isready -U postgres\"]
  redis:
    image: redis:7
  control:
    build:
      context: ../..
    image: zeroship-platform:dev
    command: zeroship-control --port 9090
    environment:
      ZEROSHIP_CONTROL_DATABASE_URL: postgres://u:p@postgres:5432/zeroship
      ZEROSHIP_CONTROL_KV_URL: redis://redis:6379
  worker:
    build:
      context: ../..
    image: zeroship-platform:dev
    command: zeroship-worker --control-url http://control:9090
    environment:
      ZEROSHIP_WORKER_KV_URL: redis://redis:6379
      ZEROSHIP_WORKER_DATABASE_URL: postgres://u:p@postgres:5432/zeroship
";

    #[test]
    fn a_stack_whose_backing_services_are_all_reached_is_green() {
        let compose = ComposeFile::from_yaml(WIRED).expect("parses");
        let outcome = run(&compose, EDGE_SERVICES, HOST_TOOL_SERVICES);
        assert!(
            matches!(outcome.report().verdict(), Verdict::Green { .. }),
            "{}",
            outcome.report().render("t")
        );
    }

    /// THE DEFECT, staged: a broker that runs and that nobody is configured to
    /// reach. One variable from the green above.
    #[test]
    fn an_unreached_backing_service_is_named() {
        let yaml = format!(
            "{WIRED}  redpanda:\n    image: redpandadata/redpanda:latest\n    ports:\n      \
             - \"127.0.0.1:19092:19092\"\n    command:\n      - --advertise-kafka-addr\n      \
             - internal://redpanda:9092\n"
        );
        let compose = ComposeFile::from_yaml(&yaml).expect("parses");
        let outcome = run(&compose, EDGE_SERVICES, HOST_TOOL_SERVICES);
        assert!(
            outcome.report().checks().iter().any(|check| {
                !check.ok && check.detail.starts_with("redpanda RUNS AND NOBODY IS CONFIGURED")
            }),
            "{}",
            outcome.report().render("t")
        );
        // A service advertising its OWN address must not count as reaching
        // itself, or every backing service would pass by naming itself.
        assert!(!outcome
            .report()
            .checks()
            .iter()
            .any(|check| check.ok && check.detail.starts_with("redpanda is reached")));
    }

    /// The control arm: a file whose services carry no configuration at all
    /// would have every backing service unreached. That is a loud, confident,
    /// wrong finding, and only the edge count separates it from a real one.
    #[test]
    fn a_file_with_no_configured_addresses_refuses_rather_than_reporting_them_all_unreached() {
        let bare = "\
services:
  caddy:
    image: caddy:2-alpine
    ports:
      - \"80:80\"
  verdaccio:
    image: verdaccio/verdaccio:6
    ports:
      - \"127.0.0.1:4873:4873\"
  postgres:
    image: postgres:16
  redis:
    image: redis:7
  redpanda:
    image: redpandadata/redpanda:latest
";
        let compose = ComposeFile::from_yaml(bare).expect("parses");
        let outcome = run(&compose, EDGE_SERVICES, HOST_TOOL_SERVICES);
        match outcome.report().verdict() {
            Verdict::Refused(reason) => assert!(reason.contains("reach_edges"), "{reason}"),
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    /// A host-tool that something now dials has become a backend, and the role
    /// must stop excusing it.
    #[test]
    fn a_host_tool_that_gained_a_consumer_is_named() {
        let yaml = WIRED.replace(
            "      ZEROSHIP_WORKER_KV_URL: redis://redis:6379\n",
            "      ZEROSHIP_WORKER_KV_URL: redis://redis:6379\n      NPM_REGISTRY: \
             http://verdaccio:4873\n",
        );
        let compose = ComposeFile::from_yaml(&yaml).expect("parses");
        let outcome = run(&compose, EDGE_SERVICES, HOST_TOOL_SERVICES);
        assert!(outcome.report().checks().iter().any(|check| {
            !check.ok && check.detail.contains("declared a host-tool but is now configured")
        }));
    }

    /// A role pointing at a service that is not a candidate exempts nothing and
    /// hides that it exempts nothing.
    #[test]
    fn a_role_naming_a_service_that_is_not_a_candidate_refuses() {
        let compose = ComposeFile::from_yaml(WIRED).expect("parses");
        let outcome = run(&compose, EDGE_SERVICES, &["verdaccio", "renamed-away"]);
        match outcome.report().verdict() {
            Verdict::Refused(reason) => assert!(reason.contains("renamed-away"), "{reason}"),
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    /// The predicate is bound to "a name used as a host", so the excluded
    /// blocks and the near-misses have to be shown NOT to match.
    #[test]
    fn the_reach_predicate_matches_a_host_and_not_a_lookalike() {
        assert!(names_host("postgres://u:p@postgres:5432/zeroship", "postgres"));
        assert!(names_host("redis://redis:6379", "redis"));
        assert!(names_host("--control-url http://control:9090", "control"));
        assert!(names_host("${X:-http://migrated:9091}", "migrated"));
        // ONE VARIABLE each: the same shapes that must NOT count. `image:
        // postgres:16` is absent here on purpose - it DOES match this
        // predicate, and is kept out by CONFIG_EXCLUDED_KEYS instead, which is
        // the honest place for it.
        assert!(!names_host("my-postgres:5432", "postgres"));
        assert!(!names_host("app.postgres:5432", "postgres"));
        assert!(!names_host("postgres:latest", "postgres"));
        assert!(!names_host("pg_isready -U postgres", "postgres"));
    }
}
