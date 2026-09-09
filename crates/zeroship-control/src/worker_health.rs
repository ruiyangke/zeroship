//! Liveness observation for enrolled worker instances.
//!
//! WHY THIS EXISTS. `zeroship.worker_instances` records what control DECLARED
//! about an instance, and `status = 'active'` therefore names a set of instances
//! that were once enrolled -- not a set that is alive. Enrolment happens once per
//! worker boot, nothing transitions a row on its own, and a crash-looping worker
//! leaves a fresh `active` row behind on every attempt. An eligible set computed
//! from that column alone would route to processes that are gone. Something has
//! to observe liveness, and this module is that something.
//!
//! THE ONE RULE THIS MODULE EXISTS TO KEEP: **it never writes what it observes
//! into `status`.** The column is a closed set over what a writer declared, and
//! `db/migrations-ts/20260907000300_worker_instances.ts` already states the
//! reason readiness is excluded from it -- readiness is a probe result, it is
//! derived, it expires, and it belongs to whatever performed the probe. Admitting
//! it would make one column carry two kinds of fact and leave readers disagreeing
//! about which of the two they are holding.
//!
//! THE TRAP IF THAT RULE IS BROKEN, stated because it is not hypothetical.
//! `gone` is TERMINAL: there is no path back from it, control holds no DELETE on
//! the table, and a worker re-enters the registry only by restarting. A monitor
//! that wrote `gone` on a failed probe would convert a transient network blip
//! into the PERMANENT eviction of a healthy worker, recoverable only by killing
//! the process it just evicted. The blast radius grows with fleet size and peaks
//! during exactly the partial-partition conditions that produce the blip. So the
//! monitor keeps what it saw, the table keeps what control declared, and neither
//! is written into the other.
//!
//! WHAT PROMOTES A LONG-UNHEALTHY INSTANCE TO `gone` IS NOT DECIDED HERE, and
//! this module deliberately ships no reaper. That is a retention decision with an
//! operator in it, not a probe timeout.
//!
//! THE PROBE TARGET IS THE DERIVED ADDRESS. `advertise_host` and
//! `advertise_port` are written by control from the accepted enrolment socket and
//! frozen by a trigger, so probing them cannot be redirected by the registrant.
//! Probing a registrant-supplied address would hand a role-key holder the ability
//! to answer health checks on behalf of a worker it does not run.
//!
//! `tick` RETURNS WHAT IT SAW rather than only logging it. A monitor that logs
//! "unhealthy" and changes nothing prints exactly what a working monitor prints;
//! that substitution survived three of four arms when it was tried against the
//! worker's startup refusal elsewhere in this redesign. Callers -- and the tests
//! that bind this module -- rule on the returned report and on the row, never on
//! a log line.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Only instances control still declares enrolled are worth probing. This is the
/// same spelling `worker_enrolment` writes; a status outside the column's closed
/// set cannot reach the table, so no other value needs an arm here.
const ENROLLED_STATUS: &str = "active";

/// How long a single probe may take before it counts as unhealthy. A probe that
/// hangs is not evidence of health, and an unbounded one would stall the sweep
/// behind the slowest dead address in the fleet.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// How long the monitor sleeps between sweeps.
pub const DEFAULT_SWEEP_SECS: u64 = 5;

/// What one probe observed. This is NOT a status and is never persisted as one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Liveness {
    /// The instance answered its health endpoint.
    Healthy,
    /// The instance did not answer, answered too slowly, or answered with a
    /// non-success status. All three are the same fact to a router: do not send
    /// this instance work.
    Unhealthy,
}

/// One observation, with the moment it was taken. The instant is what lets a
/// reader require FRESHNESS rather than trusting a reading of unknown age.
#[derive(Debug, Clone, Copy)]
struct Observation {
    liveness: Liveness,
    observed_at: Instant,
}

/// The monitor's own state: in memory, short-lived, never written to the
/// registry. This is the half of the split that holds "what a probe saw".
#[derive(Debug, Default)]
pub struct HealthView {
    // The guard is never held across an await -- every method below takes it,
    // finishes, and drops it. Holding it across the probe would serialise the
    // whole sweep behind one slow address.
    seen: Mutex<HashMap<String, Observation>>,
}

impl HealthView {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record what a probe saw. Overwrites any earlier observation for the same
    /// instance: the monitor holds the LATEST reading, not a history.
    pub fn record(&self, instance_id: &str, liveness: Liveness, observed_at: Instant) {
        let mut seen = self.seen.lock().expect("health view mutex");
        seen.insert(
            instance_id.to_string(),
            Observation {
                liveness,
                observed_at,
            },
        );
    }

    /// The latest reading for an instance, if the monitor has one at all.
    ///
    /// `None` means NEVER PROBED, which is not the same as unhealthy and must not
    /// be collapsed into it by a caller: an instance enrolled a moment ago has no
    /// reading yet, and treating that as unhealthy would make a fresh worker
    /// ineligible for its first sweep.
    #[must_use]
    pub fn latest(&self, instance_id: &str) -> Option<Liveness> {
        let seen = self.seen.lock().expect("health view mutex");
        seen.get(instance_id).map(|o| o.liveness)
    }

    /// Whether the instance was observed healthy recently enough to route to.
    ///
    /// This is the predicate the eligible set intersects with the declared
    /// `active` status. A reading older than `freshness` is refused rather than
    /// trusted, because a monitor that stopped sweeping must not leave the fleet
    /// looking permanently healthy.
    #[must_use]
    pub fn healthy_within(&self, instance_id: &str, freshness: Duration, now: Instant) -> bool {
        let seen = self.seen.lock().expect("health view mutex");
        seen.get(instance_id).is_some_and(|o| {
            o.liveness == Liveness::Healthy
                && now.saturating_duration_since(o.observed_at) <= freshness
        })
    }

    /// Drop readings for instances the sweep no longer sees, so the map does not
    /// grow without bound across the lifetime of the process. This forgets state
    /// about instances control no longer declares active; it does not write
    /// anything anywhere.
    pub fn retain_only(&self, live_ids: &[String]) {
        let mut seen = self.seen.lock().expect("health view mutex");
        seen.retain(|id, _| live_ids.iter().any(|live| live == id));
    }
}

/// One instance as the registry describes it, reduced to what a probe needs.
#[derive(Debug, Clone)]
pub struct ProbeTarget {
    pub instance_id: String,
    pub host: IpAddr,
    pub port: i32,
}

/// What one sweep observed. Returned so a caller can act on the result and a
/// test can rule on it, instead of both being asked to trust a log line.
#[derive(Debug, Default)]
pub struct SweepReport {
    pub observed: Vec<(String, Liveness)>,
}

impl SweepReport {
    /// The reading for one instance in this sweep, if it was probed at all.
    #[must_use]
    pub fn liveness_of(&self, instance_id: &str) -> Option<Liveness> {
        self.observed
            .iter()
            .find(|(id, _)| id == instance_id)
            .map(|(_, liveness)| *liveness)
    }
}

/// Read every instance control still declares enrolled.
///
/// # Errors
///
/// Returns the driver's error when the registry cannot be read. A caller must
/// not treat an unreadable registry as an empty fleet: that would mark every
/// worker unroutable at the moment control lost its database.
pub async fn enrolled_targets(
    pg: &compio_postgres::Client,
) -> Result<Vec<ProbeTarget>, compio_postgres::Error> {
    let rows = pg
        .query(
            "SELECT id, advertise_host, advertise_port \
             FROM zeroship.worker_instances WHERE status = $1",
            &[&ENROLLED_STATUS],
        )
        .await?;
    Ok(rows
        .iter()
        .map(|row| ProbeTarget {
            instance_id: row.get(0),
            host: row.get(1),
            port: row.get(2),
        })
        .collect())
}

/// Probe one instance's health endpoint at its DERIVED address.
///
/// Every failure mode collapses to `Unhealthy` on purpose: a refused connection,
/// a timeout and a 500 are the same instruction to a router. The distinction
/// between them is diagnostic, and diagnostics belong in the log this returns
/// alongside, not in the routing decision.
pub async fn probe(client: &cyper::Client, target: &ProbeTarget) -> Liveness {
    let url = format!("http://{}:{}/healthz", target.host, target.port);
    let Ok(request) = client.get(&url) else {
        return Liveness::Unhealthy;
    };
    match compio::time::timeout(PROBE_TIMEOUT, request.send()).await {
        Ok(Ok(response)) if response.status().is_success() => Liveness::Healthy,
        _ => Liveness::Unhealthy,
    }
}

/// One sweep: read the declared-enrolled set, probe each at its derived address,
/// and record what was seen in the monitor's own state.
///
/// THIS FUNCTION ISSUES NO WRITE TO `zeroship.worker_instances`, AND THAT IS THE
/// PROPERTY ITS TESTS BIND. It holds exactly one statement against the registry,
/// the SELECT in `enrolled_targets`.
///
/// # Errors
///
/// Returns the driver's error when the registry cannot be read.
pub async fn tick(
    pg: &compio_postgres::Client,
    client: &cyper::Client,
    view: &HealthView,
) -> Result<SweepReport, compio_postgres::Error> {
    let targets = enrolled_targets(pg).await?;
    let mut report = SweepReport::default();
    for target in &targets {
        let liveness = probe(client, target).await;
        view.record(&target.instance_id, liveness, Instant::now());
        report.observed.push((target.instance_id.clone(), liveness));
    }
    let live_ids: Vec<String> = targets.into_iter().map(|t| t.instance_id).collect();
    view.retain_only(&live_ids);
    Ok(report)
}

/// Sweep forever. The loop logs a failed read and keeps going: a monitor that
/// exits on one unreadable registry stops observing the whole fleet.
pub async fn run(
    pg: std::sync::Arc<compio_postgres::Client>,
    view: std::sync::Arc<HealthView>,
    sweep_secs: u64,
) {
    let client = cyper::Client::new();
    loop {
        match tick(&pg, &client, &view).await {
            Ok(report) => {
                let unhealthy = report
                    .observed
                    .iter()
                    .filter(|(_, liveness)| *liveness == Liveness::Unhealthy)
                    .count();
                if unhealthy > 0 {
                    tracing::warn!(
                        unhealthy,
                        probed = report.observed.len(),
                        "worker health sweep observed unhealthy instances"
                    );
                }
            }
            Err(error) => {
                tracing::error!(%error, "worker health sweep could not read the registry");
            }
        }
        compio::time::sleep(Duration::from_secs(sweep_secs)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id() -> &'static str {
        "wkr_0000000000000000000000001"
    }

    /// `None` and `Unhealthy` are different facts, and a caller that collapsed
    /// them would make a just-enrolled instance ineligible before its first
    /// sweep. This pins the distinction at the type the eligible set reads.
    #[test]
    fn never_probed_is_not_unhealthy() {
        let view = HealthView::new();
        assert_eq!(view.latest(id()), None);
        view.record(id(), Liveness::Unhealthy, Instant::now());
        assert_eq!(view.latest(id()), Some(Liveness::Unhealthy));
    }

    /// A stale healthy reading must not keep an instance routable. Without this,
    /// a monitor that stopped sweeping would leave the whole fleet looking
    /// permanently healthy.
    #[test]
    fn a_stale_healthy_reading_is_refused() {
        let view = HealthView::new();
        let now = Instant::now();
        let long_ago = now - Duration::from_secs(3600);
        view.record(id(), Liveness::Healthy, long_ago);

        assert!(
            view.healthy_within(id(), Duration::from_secs(7200), now),
            "a reading inside the freshness window is trusted"
        );
        assert!(
            !view.healthy_within(id(), Duration::from_secs(10), now),
            "a reading older than the freshness window is refused"
        );
    }

    /// The latest reading wins: the monitor holds a current view, not a history.
    #[test]
    fn a_later_reading_replaces_an_earlier_one() {
        let view = HealthView::new();
        let now = Instant::now();
        view.record(id(), Liveness::Unhealthy, now - Duration::from_secs(1));
        view.record(id(), Liveness::Healthy, now);
        assert_eq!(view.latest(id()), Some(Liveness::Healthy));
        assert!(view.healthy_within(id(), Duration::from_secs(30), now));
    }

    /// Forgetting an instance the sweep no longer sees keeps the map bounded.
    #[test]
    fn readings_for_vanished_instances_are_forgotten() {
        let view = HealthView::new();
        view.record(id(), Liveness::Healthy, Instant::now());
        view.retain_only(&["wkr_0000000000000000000000002".to_string()]);
        assert_eq!(view.latest(id()), None);
    }
}
