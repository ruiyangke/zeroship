//! The journal halves a merged job exchange carries, and the envelopes that
//! carry them beside the manager's own halves.
//!
//! WHY THIS IS A PORT AND NOT A SET OF TYPES. The journal's types belong to the
//! engine, and the engine already depends on this crate, so naming them here
//! would be a Cargo cycle as well as the crate-ownership violation
//! `workflow_process_dependencies_follow_crate_ownership` refuses. The engine
//! holds them; this crate only has to put them on a wire.
//!
//! WHY THE BOUNDS ARE SERDE AND NOTHING ELSE. This client validates nothing
//! inside a journal half. Every check it makes is on [`Delivery`] fields it
//! already owns: that the reply names the worker whose key signed the request,
//! the app and revision the request asked for, and the same immutable delivery
//! it sent. It has no view inside the journal half and needs none, so an
//! identity bound would be a capability it never exercises.

use serde::{de::DeserializeOwned, Deserialize, Serialize};
use zeroship_core::workflow_jobs::{Delivery, DeliveryLease, JobOutcome, JobSpec};

/// The journal payloads one implementation's merged exchanges carry.
///
/// The precedent is `JobTransport::Lease` in `zeroship-workflow-runner`, which
/// lets the runner drive delivery without naming this crate's `LeasedJob`. This
/// is that technique inverted across the same seam.
pub trait JobJournal {
    /// How a holder names the live journal task it is renewing.
    type Claim: Serialize;
    /// What a claim's journal half answers.
    type Acceptance: DeserializeOwned;
    /// What a renewal's journal half answers.
    type Renewal: DeserializeOwned;
    /// What a holder reports for the journal half of a settlement. Its reply
    /// carries no journal half, for the reason recorded below `Exclusive`.
    type Execution: Serialize;
}

/// A claimed delivery and, when the journal accepted the operation it names,
/// what that acceptance answered.
///
/// The halves are NAMED rather than flattened; see
/// `a_job_envelope_refuses_an_unnamed_field` for what that buys.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ClaimedDelivery<A> {
    pub lease: DeliveryLease,
    /// Absent for an operation the journal does not accept work for -- every
    /// maintenance kind -- and present for the one that it does.
    #[serde(default = "absent", skip_serializing_if = "Option::is_none")]
    pub accepted: Option<A>,
}

/// A renewal request: the delivery whose queue lease is extended, and the
/// journal task whose creator lease is extended with it.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RenewDelivery<C> {
    pub delivery: Delivery,
    /// Absent when the caller holds no journal task under this delivery, which
    /// is every maintenance operation: those have a queue lease and no task.
    #[serde(default = "absent", skip_serializing_if = "Option::is_none")]
    pub task: Option<C>,
}

/// What one renewal answers: the extended queue lease, and the journal renewal
/// when the request carried a task.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RenewedDelivery<R> {
    pub lease: DeliveryLease,
    #[serde(default = "absent", skip_serializing_if = "Option::is_none")]
    pub renewal: Option<R>,
}

/// A settlement request, carrying either an outcome the caller's own journal
/// already committed or the execution this service must commit first.
///
/// EXACTLY ONE OF THE TWO. The outcome of an execution is not known until the
/// journal commits it, so a caller reporting one cannot also name the other;
/// [`Self::reported`] is what refuses a body that names both or neither, since
/// serde cannot express the exclusion between two optional fields.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SettleDelivery<C> {
    pub delivery: Delivery,
    /// The outcome the caller's journal already holds for this delivery.
    #[serde(default = "absent", skip_serializing_if = "Option::is_none")]
    pub outcome: Option<JobOutcome>,
    /// Successors the caller publishes with that outcome, in one transaction.
    ///
    /// Always serialized, empty or not: with the outcome present this body is
    /// exactly the `Settlement` it reports, which is one shape for a reader to
    /// hold rather than two that differ by an absent list.
    #[serde(default)]
    pub successors: Vec<JobSpec>,
    /// The execution to commit into the journal, whose outcome then settles the
    /// delivery.
    #[serde(default = "absent", skip_serializing_if = "Option::is_none")]
    pub execution: Option<C>,
}

/// Which half of a settlement request the caller supplied.
#[derive(Debug)]
pub enum Reported<'a, C> {
    /// An outcome and successors the caller's own journal already committed.
    Outcome(&'a JobOutcome),
    /// An execution whose outcome this service must commit before settling.
    Execution(&'a C),
}

impl<C> SettleDelivery<C> {
    /// Which half this request carries.
    ///
    /// # Errors
    /// Reports a body naming both halves or neither.
    pub fn reported(&self) -> Result<Reported<'_, C>, Exclusive> {
        match (&self.outcome, &self.execution) {
            (Some(outcome), None) => Ok(Reported::Outcome(outcome)),
            (None, Some(execution)) => {
                if self.successors.is_empty() {
                    Ok(Reported::Execution(execution))
                } else {
                    // A journal completion publishes its own successors from
                    // the frontier it commits. A caller naming them here would
                    // be naming the successors of an outcome it has not seen.
                    Err(Exclusive)
                }
            }
            _ => Err(Exclusive),
        }
    }
}

/// A settlement request named both halves, neither, or successors beside an
/// execution.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("a settlement reports either a committed outcome or an execution, never both")]
pub struct Exclusive;

// A SETTLEMENT ANSWERS WITH ONE HALF, because the other adds nothing. The
// journal's own receipt names the logical job and the outcome it committed, and
// the queue receipt this endpoint already returns carries that same outcome --
// the server derives the settlement FROM the receipt, so the two cannot
// disagree. A caller therefore reconstructs the journal receipt from the
// delivery it sent and the outcome it got back, and every field it holds is one
// this client validated. Sending it instead would put a second copy on the wire
// with no check to make against it, which is exactly the half a merged reply
// must not acquire.

/// `None`, named rather than derived: `#[serde(default)]` on a generic field
/// makes the derive demand `Default` from the type parameter, which a journal
/// payload has no reason to implement.
const fn absent<T>() -> Option<T> {
    None
}

#[cfg(test)]
mod tests {
    use super::{ClaimedDelivery, Exclusive, RenewDelivery, Reported, SettleDelivery};
    use serde_json::{json, Value};
    use zeroship_core::{
        app_id::AppId,
        workflow_coordination::{RunId, WorkerId},
        workflow_jobs::{DeploymentId, JobId, JobOutcome},
    };

    fn delivery() -> Value {
        json!({
            "job": {
                "id": JobId::mint().as_str(),
                "appId": AppId::mint().as_str(),
                "operation": {
                    "kind": "advance",
                    "deploymentId": DeploymentId::mint().as_str(),
                    "runId": RunId::mint().as_str(),
                    "generation": 0,
                    "revision": 1,
                },
                "availableAt": 1,
            },
            "workerId": WorkerId::mint().as_str(),
            "assignmentRevision": 1,
            "attempt": 1,
            "deadline": 1,
        })
    }

    /// Every merged envelope refuses a field its contract does not name, on the
    /// request side and the reply side alike.
    ///
    /// This is the property that decided the shape. Naming the manager's half
    /// instead of flattening it costs these bodies their byte compatibility with
    /// the single-half ones they replace, and buys this: an envelope with a
    /// flattened field cannot declare `deny_unknown_fields` and does not
    /// inherit the flattened type's own, so one would admit creator data beside
    /// its metadata. The control arm parses first, because a rejection that
    /// rejects everything measures nothing.
    #[test]
    fn a_job_envelope_refuses_an_unnamed_field() {
        let intruded = |mut body: Value| {
            body["customerPayload"] = json!("must stay in creator storage");
            body
        };
        let renewal = json!({"delivery": delivery()});
        serde_json::from_value::<RenewDelivery<Value>>(renewal.clone()).unwrap();
        assert!(serde_json::from_value::<RenewDelivery<Value>>(intruded(renewal)).is_err());

        let claimed = json!({"lease": {"delivery": delivery(), "remainingMs": 1000}});
        serde_json::from_value::<ClaimedDelivery<Value>>(claimed.clone()).unwrap();
        assert!(serde_json::from_value::<ClaimedDelivery<Value>>(intruded(claimed)).is_err());

        let settle = json!({"delivery": delivery(), "outcome": {"kind": "completed"}});
        serde_json::from_value::<SettleDelivery<Value>>(settle.clone()).unwrap();
        assert!(serde_json::from_value::<SettleDelivery<Value>>(intruded(settle)).is_err());
    }

    /// The journal half of a settlement is optional in the shape and exclusive
    /// in the contract, so the exclusion is a check rather than a type.
    #[test]
    fn a_settlement_reports_one_half_and_refuses_both_or_neither() {
        let decoded =
            |body: Value| serde_json::from_value::<SettleDelivery<Value>>(body).unwrap();
        let reported = json!({"delivery": delivery(), "outcome": {"kind": "completed"}});
        assert!(matches!(
            decoded(reported.clone()).reported(),
            Ok(Reported::Outcome(JobOutcome::Completed {}))
        ));
        let executed = json!({"delivery": delivery(), "execution": {"outcomes": []}});
        assert!(matches!(
            decoded(executed.clone()).reported(),
            Ok(Reported::Execution(_))
        ));

        assert_eq!(
            decoded(json!({"delivery": delivery()})).reported().unwrap_err(),
            Exclusive
        );
        let mut both = reported;
        both["execution"] = json!({"outcomes": []});
        assert_eq!(decoded(both).reported().unwrap_err(), Exclusive);
        // Successors belong to an outcome the caller has already seen, so they
        // cannot ride an execution whose outcome the journal has yet to decide.
        let mut published = executed;
        published["successors"] = json!([delivery()["job"].clone()]);
        assert_eq!(decoded(published).reported().unwrap_err(), Exclusive);
    }
}
