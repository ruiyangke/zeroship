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
use zeroship_core::workflow_jobs::{Delivery, DeliveryLease, JobSpec};
pub use zeroship_core::workflow_jobs::ClaimedDelivery;

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
    /// carries no journal half, for the reason recorded below [`SettleDelivery`].
    type Execution: Serialize;
    /// What the journal answers when asked for a logical job's committed
    /// outcome.
    ///
    /// This is the half a holder reads after an UNCERTAIN settlement: the reply
    /// it lost may have committed, and the receipt is how it finds out without
    /// risking a second commit. So it is a read of durable journal state rather
    /// than a half of any one exchange, which is why it has no envelope of its
    /// own beside the manager.
    type Receipt: DeserializeOwned;
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

/// A settlement request: the delivery, and the execution the service commits
/// before settling it, when the holder has one to report.
///
/// THERE IS NO OUTCOME HERE AND NO SUCCESSOR. The queue is settled with what the
/// journal decides, never with what the holder says: an execution's outcome is
/// what its commit produces, and a body with no execution is settled from the
/// receipt the journal already holds for the job -- the recovery for a holder
/// whose earlier settlement reply was lost, or whose claim found the job already
/// committed. A journal receipt carries no successors, so a settlement publishes
/// nothing a worker named.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SettleDelivery<C> {
    pub delivery: Delivery,
    /// The execution to commit into the journal, whose outcome then settles the
    /// delivery. Absent asks the service to settle from the journal's receipt.
    #[serde(default = "absent", skip_serializing_if = "Option::is_none")]
    pub execution: Option<C>,
}

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
    use super::{ClaimedDelivery, RenewDelivery, SettleDelivery};
    use serde_json::{json, Value};
    use zeroship_core::{
        app_id::AppId,
        workflow_coordination::{RunId, WorkerId},
        workflow_jobs::{DeploymentId, JobId},
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

        let claimed = json!({"lease": {"delivery": delivery(), "remainingMs": 1000, "attemptRemainingMs": 1000}});
        serde_json::from_value::<ClaimedDelivery<Value>>(claimed.clone()).unwrap();
        assert!(serde_json::from_value::<ClaimedDelivery<Value>>(intruded(claimed)).is_err());

        let settle = json!({"delivery": delivery()});
        serde_json::from_value::<SettleDelivery<Value>>(settle.clone()).unwrap();
        assert!(serde_json::from_value::<SettleDelivery<Value>>(intruded(settle)).is_err());
    }

    /// A settlement names a delivery and, optionally, an execution, and nothing
    /// that would let a holder choose the outcome or publish a job.
    ///
    /// Both accepted shapes parse first, so the refusals below measure the field
    /// they add rather than a body that never parsed.
    #[test]
    fn a_settlement_carries_no_outcome_and_no_successor() {
        let decoded =
            |body: &Value| serde_json::from_value::<SettleDelivery<Value>>(body.clone());
        let committed = json!({"delivery": delivery()});
        assert!(decoded(&committed).unwrap().execution.is_none());
        let executed = json!({"delivery": delivery(), "execution": {"outcomes": []}});
        assert!(decoded(&executed).unwrap().execution.is_some());

        let refused = [
            ("outcome", json!({"kind": "completed"})),
            ("successors", json!([delivery()["job"].clone()])),
            ("successors", json!([])),
        ];
        assert!(!refused.is_empty());
        for base in [&committed, &executed] {
            for (field, value) in &refused {
                let mut body = base.clone();
                body[field] = value.clone();
                assert!(decoded(&body).is_err(), "{body}");
            }
        }
    }
}

/// A release request for creator work that stopped, or for an app that could
/// not be prepared.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReleaseDelivery<C> {
    pub delivery: Delivery,
    pub task: Option<C>,
    pub reason: GiveBackReason,
}

/// Why an executable delivery is returned without a settlement.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GiveBackReason {
    /// The holder could not prepare the app, so no attempt began. The row
    /// waits out a back-off and counts no attempt.
    PreparationFailed,
    /// An attempt began and stopped without a receipt: it failed, was told to
    /// stop, or its host drained. The row is claimable at once and the attempt
    /// counts toward the delivery budget.
    Interrupted,
    /// The holder received the delivery and began nothing, through no fault of
    /// the job: its journal task arrived with no time left, or the holder
    /// stopped before starting it. The row is claimable at once and counts
    /// nothing.
    Unsent,
}

/// A request for the committed outcome of one logical job.
///
/// ADDRESSED BY THE JOB AND NOT BY THE DELIVERY, because that is what the
/// question is about: a receipt belongs to the logical job, and the caller is
/// asking whether ANY attempt committed one -- including the attempt whose reply
/// it lost. Naming an attempt would ask a narrower question than the recovery
/// needs.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct JobReceiptQuery {
    pub job: JobSpec,
}
