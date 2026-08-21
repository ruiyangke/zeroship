//! Every stateful service in `deploy/compose` must keep its data on a NAMED
//! VOLUME.
//!
//! THE DEFECT THIS EXISTS FOR, found 2026-08-11 while walking the operator
//! deploy (scenario 18). The compose file gave durable named volumes to the
//! cache and to the replayable log, and NONE to the database:
//!
//! ```text
//! redis      redis-data:/data                       <- a cache
//! redpanda   redpanda-data:/var/lib/redpanda/data   <- a replayable log
//! worker     bundles:, app-storage:
//! postgres   (nothing)                              <- every creator's data
//! ```
//!
//! Postgres' only `volumes:` entry was a read-only bind of
//! `../ops/postgres-init.sql` into `/docker-entrypoint-initdb.d`. PGDATA
//! therefore lived in the container's writable layer, so `docker compose down`
//! (or any `up` that recreates the container after a config change) destroyed
//! the platform database: control state, auth, billing, the platform migration
//! journal, and every `app_*` schema. The cache and the log survived it.
//!
//! The asymmetry is why this reads as an oversight rather than a decision.
//! Nobody deliberately makes a Redis cache durable and the system of record
//! ephemeral.
//!
//! WHAT THIS DOES NOT CHECK, so a green is not over-read:
//!   - it does not run docker and does not inspect a live container
//!   - it does not verify the volume is mounted at the RIGHT path for the
//!     image; it checks that a named volume is attached at all
//!   - it says nothing about backups, retention, or whether the host directory
//!     behind a named volume is itself durable
//!   - `docker compose down -v` removes named volumes too; this gate does not
//!     protect against that, and nothing in a compose file can

use crate::arm_census::GateRun;
use crate::compose::ComposeFile;

/// The gate id, as it appears in every census line.
pub const GATE: &str = "compose_stateful_volume";

/// The report title.
pub const TITLE: &str = "stateful compose services keep data on a named volume";

/// Services whose data must outlive the container.
///
/// LISTED EXPLICITLY rather than inferred: "is this service stateful" is a
/// judgement the file cannot express, and a wrong inference here would either
/// miss the database or demand a volume for a stateless binary. It is also the
/// corpus the gate's verdict count is bound to, so a service dropping out of
/// the parse cannot shrink the run silently.
pub const STATEFUL: &[&str] = &["postgres", "redis", "redpanda"];

/// How many top-level named volumes the file must declare before a mount can be
/// matched against them.
///
/// MEASURED 2026-08-20 and re-measured 2026-08-21: 6 declared under the
/// top-level `volumes:` key. Floor well under that - adding or removing one
/// volume for an unrelated service should not trip this, while the failure it
/// guards against, the `volumes:` key not being read at all, drops the count to
/// zero rather than to three.
const DECLARED_VOLUMES_FLOOR: usize = 3;

/// Judge `compose`.
///
/// `stateful` is a parameter so the empty case - a gate handed nothing to rule
/// on - is reachable from a test rather than only by editing the source.
#[must_use]
pub fn run(compose: &ComposeFile, stateful: &[&str]) -> GateRun {
    let mut run = GateRun::new(GATE);

    let declared: Vec<&str> = compose.volumes.keys().map(String::as_str).collect();
    if !run.arm("declared_volumes", declared.len(), DECLARED_VOLUMES_FLOOR) {
        return run;
    }

    for name in stateful {
        let Some(service) = compose.services.get(*name) else {
            run.report_mut().fail(format!(
                "service '{name}' not found in {} - this gate's premise is stale",
                compose.path().display()
            ));
            continue;
        };
        // A named-volume mount is `<name>:<path>` where <name> is declared at
        // the top level. A bind mount (`../ops/foo.sql:/bar`) has a slash or a
        // dot before the colon and does NOT count - that is exactly the
        // distinction postgres fell through, since its init-script bind looks
        // like a volumes entry at a glance.
        let mounted = service.volumes.iter().find_map(|mount| {
            let (source, target) = mount.split_once(':')?;
            let named = declared.iter().find(|volume| **volume == source)?;
            target.starts_with('/').then_some(*named)
        });
        match mounted {
            Some(volume) => run
                .report_mut()
                .pass(format!("{name} keeps data on named volume '{volume}'")),
            None => run.report_mut().fail(format!(
                "{name} mounts NO named volume; its data lives in the container layer and dies \
                 with the container"
            )),
        }
    }

    // Counts verdicts that RAN, not that PASSED: a mutation moves an outcome
    // BETWEEN those columns, so only a LOST verdict drops the sum. BOUND TO THE
    // CORPUS THIS RUN WAS HANDED - the stateful list - rather than to a number
    // measured once, so adding a stateful service raises the expectation in the
    // same edit that creates the obligation.
    let verdicts = run.report().checks().len();
    if verdicts != stateful.len() {
        run.report_mut().fail(format!(
            "{verdicts} verdict(s) for {} stateful service(s); one reached no branch, so nothing \
             was said about it",
            stateful.len()
        ));
    }

    run
}

#[cfg(test)]
mod tests {
    use super::{run, STATEFUL};
    use crate::compose::ComposeFile;
    use crate::report::Verdict;

    const DURABLE: &str = "\
services:
  postgres:
    image: postgres:16
    volumes:
      - ../ops/postgres-init.sql:/docker-entrypoint-initdb.d/init.sql:ro
      - pgdata:/var/lib/postgresql/data
  redis:
    volumes:
      - redis-data:/data
  redpanda:
    volumes:
      - redpanda-data:/var/lib/redpanda/data
volumes:
  pgdata:
  bundles:
  app-storage:
  redis-data:
  redpanda-data:
  verdaccio_storage:
";

    #[test]
    fn every_stateful_service_on_a_named_volume_is_green() {
        let compose = ComposeFile::from_yaml(DURABLE).expect("parses");
        let outcome = run(&compose, STATEFUL);
        assert_eq!(
            outcome.report().verdict(),
            Verdict::Green { checks: 3 },
            "{}",
            outcome.report().render("t")
        );
    }

    /// The defect this gate was written for, staged: postgres keeps only its
    /// init-script BIND, which looks like a volumes entry and is not durable.
    /// One variable from the green above.
    #[test]
    fn a_bind_mount_alone_does_not_count_as_durable() {
        let yaml = DURABLE.replace("      - pgdata:/var/lib/postgresql/data\n", "");
        let compose = ComposeFile::from_yaml(&yaml).expect("parses");
        let outcome = run(&compose, STATEFUL);
        assert!(
            outcome.report().checks().iter().any(|check| {
                !check.ok && check.detail.starts_with("postgres mounts NO named volume")
            }),
            "{}",
            outcome.report().render("t")
        );
    }

    /// A mount naming a volume the file never declares is not a named volume,
    /// however much it looks like one.
    #[test]
    fn a_mount_of_an_undeclared_volume_is_not_a_named_volume() {
        let yaml = DURABLE.replace("      - redis-data:/data\n", "      - typo-data:/data\n");
        let compose = ComposeFile::from_yaml(&yaml).expect("parses");
        let outcome = run(&compose, STATEFUL);
        assert!(outcome
            .report()
            .checks()
            .iter()
            .any(|check| !check.ok && check.detail.starts_with("redis mounts NO named volume")));
    }

    /// The anti-vacuity pair: no top-level volumes at all is a REFUSAL, because
    /// every service would then report "no named volume" and the gate would be
    /// making a confident finding about a file it could not read.
    #[test]
    fn a_file_with_no_declared_volumes_refuses() {
        let yaml = DURABLE.split("volumes:\n  pgdata:").next().unwrap().to_owned();
        let compose = ComposeFile::from_yaml(&yaml).expect("parses");
        let outcome = run(&compose, STATEFUL);
        match outcome.report().verdict() {
            Verdict::Refused(reason) => assert!(reason.contains("declared_volumes")),
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn a_stateful_service_missing_from_the_file_is_named_not_skipped() {
        let compose = ComposeFile::from_yaml(DURABLE).expect("parses");
        let outcome = run(&compose, &["postgres", "redis", "redpanda", "clickhouse"]);
        assert!(outcome
            .report()
            .checks()
            .iter()
            .any(|check| !check.ok && check.detail.contains("'clickhouse' not found")));
    }
}
