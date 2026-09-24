//! Serialize authoritative observations before reading their contributing inputs.

use super::{
    PolicyObservation,
    models::{plan_admin, publication},
};
use crate::{Error, app_facts::AppFactsSource};
use plan_admin::plans;
use publication::{workflow_policy_ledger as ledger, workflow_rollout_config as rollout};
use std::{
    rc::Rc,
    time::{Duration, Instant},
};
use zeroship_core::{
    app_id::AppId, workflow_coordination::Revision, workflow_policy::AppPolicy,
};
use zeroship_data_orm::{
    error::DbError,
    orm::{
        ConflictTarget, Database, FromRow, Insertable, IsolationLevel, TransactionOptions,
    },
    Value,
};

/// Complete operator-owned source settings, independent of creator input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RolloutPolicy {
    pub dispatch_paused: bool,
    pub ingress_disabled: bool,
    pub source_validity_ms: i64,
}

/// Platform storage only. It does not open or inspect creator databases.
///
/// # The publication bracket spans a lock and a remote read
///
/// The ledger and the operator switches are this service's own; the
/// contributing inputs are Control's and arrive through
/// [`AppFactsSource`]. [`publish`] holds a transaction on the publication
/// binding and reads the inputs over that capability while it is still open.
///
/// **What orders publications is the ledger row's write lock, not a shared
/// snapshot.** Observations run read-committed, so every statement takes a
/// fresh snapshot. What the lock buys is that a publisher waiting on it reads
/// its inputs only after the previous publisher committed, so a higher revision
/// was computed from inputs at least as new as the revision below it. That is
/// load-bearing rather than decorative: `PolicyRefresh::install` in
/// `zeroship_workflow::service::policy` refuses a lower revision but accepts
/// whatever policy a HIGHER one carries, so losing the order lets a stale
/// policy take authority and keep it.
///
/// **The argument uses one fact about the input source: a read issued after a
/// commit cannot return state older than that commit saw.** One `PostgreSQL`
/// instance gave that for free while the inputs were a second binding on the
/// same server. An API call, a replicated projection and a lagging replica do
/// not, so this store no longer assumes it: every answer carries a
/// [`SourceWatermark`], and [`publish`] refuses one below the watermark the
/// ledger already holds. See the comment at that comparison for what the
/// watermark means and why the refusal is the safe direction.
#[derive(Clone, Debug)]
pub struct ControlPolicyStore {
    facts: Rc<dyn AppFactsSource>,
    publication: Database,
}

impl ControlPolicyStore {
    /// Bind Control's facts capability and this service's own publication
    /// schema. Observations require explicit read-committed isolation; an
    /// unsupported backend refuses the operation.
    ///
    /// # Errors
    /// Rejects missing or incompatible native metadata.
    pub fn new(facts: Rc<dyn AppFactsSource>, publication: Database) -> Result<Self, Error> {
        publication.entity::<rollout::Entity>()?;
        publication.entity::<ledger::Entity>()?;
        Ok(Self { facts, publication })
    }

    /// Verify the provisioned columns and service read permissions on the
    /// publication binding.
    ///
    /// The facts capability is NOT probed here. Its readiness is an authenticated
    /// exchange with another service, and a startup probe would either make this
    /// process refuse to boot when Control is briefly down or answer for an app
    /// nobody asked about. The transport validates its origin at configuration
    /// time instead, and an unreachable Control surfaces as an unavailable
    /// observation on the first miss.
    ///
    /// # Errors
    /// Refuses unavailable or incompatible publication storage.
    pub async fn ready(&self) -> Result<(), Error> {
        async {
            let switches = self
                .publication
                .entity::<rollout::Entity>()?
                .alias("switches")?;
            self.publication
                .from(&switches)
                .select((
                    switches.column(rollout::id).select::<String>(),
                    switches.row::<RolloutRecord>(),
                ))?
                .limit(1)?
                .all()
                .await?;
            let publication = self
                .publication
                .entity::<ledger::Entity>()?
                .alias("publication")?;
            self.publication
                .from(&publication)
                .select((
                    publication.column(ledger::id).select::<String>(),
                    publication.row::<LedgerRecord>(),
                ))?
                .limit(1)?
                .all()
                .await?;
            Ok::<_, DbError>(())
        }
        .await
        .map_err(|_| Error::Unavailable)
    }

    /// Read every contributor after acquiring the publication row. Only
    /// committed observations may become source authority.
    ///
    /// # Errors
    /// Missing, malformed, expired, out-of-order or unavailable source state
    /// returns Unavailable.
    pub async fn observe(&self, app: &AppId) -> Result<PolicyObservation, Error> {
        let started = Instant::now();
        let facts = self.facts.as_ref();
        let (revision, policy, expires_at) = self
            .publication
            .transaction_with_options(
                TransactionOptions::default().isolation_level(IsolationLevel::ReadCommitted),
                |tx| async move { publish(&tx, facts, app, started).await },
            )
            .await
            .map_err(|_| Error::Unavailable)?;
        PolicyObservation::new(app.clone(), revision, policy, expires_at)
    }

    /// Publish operator switches and their finite source validity together.
    /// Existing observations retain their original deadline until refresh.
    ///
    /// # Errors
    /// Invalid validity is rejected before I/O; failed writes are unavailable.
    pub async fn set_rollout(&self, settings: RolloutPolicy) -> Result<(), Error> {
        validity(settings.source_validity_ms).map_err(|_| Error::Invalid)?;
        let _: RolloutRecord = self
            .publication
            .entity::<rollout::Entity>()?
            .upsert(
                NewRollout {
                    id: "global".into(),
                    dispatch_paused: settings.dispatch_paused,
                    ingress_disabled: settings.ingress_disabled,
                    source_validity_ms: settings.source_validity_ms,
                },
                ConflictTarget::new(rollout::id),
            )
            .await
            .map_err(|_| Error::Unavailable)?;
        Ok(())
    }
}

/// Operator provisioning of plan policy, against Control's own tables.
///
/// SEPARATE from [`ControlPolicyStore`] because the credentials are different
/// and only one of them is a service login. The workflow service's role holds
/// no write on `zeroship.plans` and, since the inputs moved behind Control's
/// endpoint, no read either; provisioning a plan is an administrative act that
/// `docs/runbooks/workflows.md` describes, performed with a credential that
/// reaches Control's schema. Keeping it on the service's store would have kept
/// a Control database binding alive in the serving path for an operation the
/// serving path never performs.
#[derive(Clone, Debug)]
pub struct PlanPolicyStore {
    inputs: Database,
}

impl PlanPolicyStore {
    /// Bind a Control schema under an administrative credential.
    ///
    /// # Errors
    /// Rejects missing or incompatible native metadata.
    pub fn new(inputs: Database) -> Result<Self, Error> {
        inputs.entity::<plans::Entity>()?;
        Ok(Self { inputs })
    }

    /// Verify the provisioned plan columns and this credential's grants.
    ///
    /// An empty plan catalog is allowed. A plan that already carries a policy
    /// must carry one this store could have written: a credential pointed at a
    /// schema whose column holds something else fails here rather than at the
    /// next publication, where the failure would name the app instead.
    ///
    /// # Errors
    /// Refuses unavailable or incompatible source storage.
    pub async fn ready(&self) -> Result<(), Error> {
        let rows = self
            .inputs
            .entity::<plans::Entity>()?
            .query()
            .limit(1)?
            .all::<PlanRecord>()
            .await
            .map_err(|_| Error::Unavailable)?;
        for row in &rows {
            if let Some(stored) = &row.workflow_policy_json {
                decode(stored).map_err(|_| Error::Unavailable)?;
            }
        }
        Ok(())
    }

    /// Set complete plan authority without changing plan eligibility or pricing.
    ///
    /// # Errors
    /// Invalid policy is rejected before I/O; missing plans and failed writes
    /// are unavailable.
    pub async fn set_plan_policy(&self, plan: &str, policy: &AppPolicy) -> Result<(), Error> {
        policy.validate().map_err(|_| Error::Invalid)?;
        let json = encode(policy).map_err(|_| Error::Invalid)?;
        let changed = self
            .inputs
            .entity::<plans::Entity>()?
            .update_many(
                plans::id.eq(plan)?,
                plans::workflow_policy_json.set(Some(json))?,
            )
            .await
            .map_err(|_| Error::Unavailable)?;
        if changed != 1 {
            return Err(Error::Unavailable);
        }
        Ok(())
    }
}

#[derive(FromRow)]
#[orm(entity = plans)]
struct PlanRecord {
    workflow_policy_json: Option<Value>,
}
#[derive(Clone, Copy, FromRow)]
#[orm(entity = rollout)]
struct RolloutRecord {
    dispatch_paused: bool,
    ingress_disabled: bool,
    source_validity_ms: i64,
}
#[derive(Insertable)]
#[orm(entity = rollout)]
struct NewRollout {
    id: String,
    dispatch_paused: bool,
    ingress_disabled: bool,
    source_validity_ms: i64,
}
#[derive(FromRow)]
#[orm(entity = ledger)]
struct LedgerRecord {
    revision: i64,
    policy_json: Option<Value>,
    source_validity_ms: Option<i64>,
    source_watermark: Option<i64>,
}
#[derive(Insertable)]
#[orm(entity = ledger)]
struct NewLedger {
    id: String,
}

/// The single operator row. A deployment that never published one has no
/// switches and no validity bound, which is refused rather than defaulted.
async fn read_rollout(publication: &Database) -> Result<RolloutRecord, DbError> {
    let switches = publication
        .entity::<rollout::Entity>()?
        .alias("switches")?;
    let rows = publication
        .from(&switches)
        .filter(switches.column(rollout::id).eq("global")?)
        .select(switches.row::<RolloutRecord>())?
        .all()
        .await?;
    let [switches] = rows.as_slice() else {
        return Err(unavailable());
    };
    Ok(*switches)
}

async fn publish(
    publication: &Database,
    facts: &dyn AppFactsSource,
    app: &AppId,
    started: Instant,
) -> Result<(Revision, AppPolicy, Instant), DbError> {
    // An ID-only upsert leaves existing publication fields unchanged while
    // holding its write lock. Every contributor is read only after that wait
    // completes, including the ones behind Control's endpoint: the order of
    // these statements is the whole of what makes a revision mean "computed
    // from inputs at least as new as the revision below it". See the note on
    // `ControlPolicyStore` for what that ordering depends on.
    let previous: LedgerRecord = publication
        .entity::<ledger::Entity>()?
        .upsert(
            NewLedger {
                id: app.as_str().into(),
            },
            ConflictTarget::new(ledger::id),
        )
        .await?;
    let switches = read_rollout(publication).await?;
    let observed = facts
        .observe(std::slice::from_ref(app))
        .await
        .map_err(|_| unavailable())?;
    let [source] = observed.apps.as_slice() else {
        return Err(unavailable());
    };
    if source.app_id != *app {
        return Err(unavailable());
    }
    // WHAT THE WATERMARK MEANS, AND WHAT IT REPLACES.
    //
    // Control reads the facts above and this watermark in ONE statement, so it
    // is at least the write position of every change visible in that
    // statement's snapshot. Two answers from one source are therefore
    // comparable: an answer at or above an earlier one saw everything the
    // earlier one saw. That is exactly the property the bracket used to get for
    // free from a single PostgreSQL instance, and this comparison is where it
    // is now paid for.
    //
    // REFUSING IS THE SAFE DIRECTION. The hazard is a STALE PERMISSIVE policy
    // taking authority: `admission`, `dispatch` and `ingress` are ANDed down
    // when an app is disabled, archived or its plan revoked, so an observation
    // that predates such a change is the one that still admits work. Refusing
    // lets the observation expire and denies; accepting would install the
    // permissive policy at a HIGHER revision, which `PolicyRefresh::install`
    // then keeps. A source that is persistently behind therefore fails closed,
    // and the refusal is correlated with the lag rather than silent.
    //
    // DO NOT REPLACE THE COUNTER WITH THE WATERMARK. Deriving `revision` from
    // this value would make a stale observation produce a LOWER revision that
    // `install` already refuses, with no comparison here at all - which looks
    // like a simplification and is not one. `install` retires the host's policy
    // epoch on ANY revision change, cancelling every operation bound to it, and
    // a watermark moves on unrelated write activity. Every routine refresh
    // would then cancel in-flight deliveries fleet-wide. The counter stays a
    // counter, and the watermark stays a fence.
    if previous
        .source_watermark
        .is_some_and(|held| observed.watermark.get() < held)
    {
        return Err(unavailable());
    }
    let mut policy = decode_source(source.plan.workflow_policy.as_ref().ok_or_else(unavailable)?)?;
    let enabled = source.workflows_enabled
        && !source.archived
        && source.plan.workflows_allowed
        && !source.plan.archived;
    policy.admission &= enabled;
    policy.dispatch &= enabled && !switches.dispatch_paused;
    policy.ingress &= enabled && !switches.ingress_disabled;
    let expires_at = started
        .checked_add(validity(switches.source_validity_ms)?)
        .filter(|deadline| *deadline > Instant::now())
        .ok_or_else(unavailable)?;
    let unchanged = match (
        previous.revision,
        previous.policy_json.as_ref(),
        previous.source_validity_ms,
        previous.source_watermark,
    ) {
        (0, None, None, None) => false,
        (revision, Some(stored), Some(source_validity_ms), Some(_)) if revision > 0 => {
            validity(source_validity_ms)?;
            decode(stored)? == policy && source_validity_ms == switches.source_validity_ms
        }
        _ => return Err(unavailable()),
    };
    let revision = if unchanged {
        // The watermark is NOT advanced here. It records the position that
        // produced the CURRENT revision, and the invariant `install` fences on
        // is about revisions: a publish that writes nothing has nothing to
        // order. Advancing it would add a write to every refresh of an
        // unchanged policy, which is the case this branch exists to avoid.
        previous.revision
    } else {
        let next = previous.revision.checked_add(1).ok_or_else(unavailable)?;
        let changes = ledger::revision
            .set(next)?
            .and(ledger::policy_json.set(Some(encode(&policy)?))?)?
            .and(ledger::source_validity_ms.set(Some(switches.source_validity_ms))?)?
            .and(ledger::source_watermark.set(Some(observed.watermark.get()))?)?;
        if publication
            .entity::<ledger::Entity>()?
            .update_many(ledger::id.eq(app.as_str())?, changes)
            .await?
            != 1
        {
            return Err(unavailable());
        }
        next
    };
    if expires_at <= Instant::now() {
        return Err(unavailable());
    }
    Ok((
        Revision::try_from(revision).map_err(|_| unavailable())?,
        policy,
        expires_at,
    ))
}

/// The plan policy exactly as Control carried it. Control does not decode it,
/// because Control does not own the policy contract; an absent, malformed or
/// invalid value is refused here.
fn decode_source(value: &serde_json::Value) -> Result<AppPolicy, DbError> {
    let policy: AppPolicy = serde_json::from_value(value.clone()).map_err(|_| unavailable())?;
    policy.validate().map_err(|_| unavailable())?;
    Ok(policy)
}
fn encode(policy: &AppPolicy) -> Result<Value, DbError> {
    serde_json::to_value(policy)
        .map(Into::into)
        .map_err(|_| unavailable())
}
fn decode(value: &Value) -> Result<AppPolicy, DbError> {
    let policy: AppPolicy =
        serde_json::from_value(serde_json::to_value(value).map_err(|_| unavailable())?)
            .map_err(|_| unavailable())?;
    policy.validate().map_err(|_| unavailable())?;
    Ok(policy)
}
fn validity(milliseconds: i64) -> Result<Duration, DbError> {
    let value = u64::try_from(milliseconds)
        .ok()
        .filter(|value| *value > 0)
        .ok_or_else(unavailable)?;
    let duration = Duration::from_millis(value);
    Instant::now()
        .checked_add(duration)
        .ok_or_else(unavailable)?;
    Ok(duration)
}
fn unavailable() -> DbError {
    DbError::internal("workflow policy source unavailable")
}

#[cfg(test)]
mod tests {
    use zeroship_core::workflow_app_facts::SourceWatermark;

    /// A watermark at or above the one the ledger holds publishes; one below it
    /// refuses. This is the comparison in [`super::publish`] isolated from its
    /// transaction, so a mutation of the operator here is caught even when the
    /// database fixture is unavailable.
    ///
    /// It does NOT prove anything consults the comparison; the publication path
    /// is covered where a real ledger row is written.
    #[test]
    fn a_watermark_below_the_one_the_ledger_holds_is_a_regression() {
        let regressed = |held: Option<i64>, observed: i64| {
            held.is_some_and(|held| SourceWatermark::new(observed).unwrap().get() < held)
        };
        // A ledger with no watermark yet admits any position: the first
        // publication has nothing to be older than.
        for first in [0, 1, i64::MAX] {
            assert!(!regressed(None, first), "{first} is the first observation");
        }
        // At or above admits; strictly below refuses.
        assert!(!regressed(Some(100), 100), "equal saw at least as much");
        assert!(!regressed(Some(100), 101));
        assert!(!regressed(Some(0), 0));
        assert!(regressed(Some(100), 99), "one position behind is stale");
        assert!(regressed(Some(100), 0));
        assert!(regressed(Some(i64::MAX), i64::MAX - 1));
    }
}
