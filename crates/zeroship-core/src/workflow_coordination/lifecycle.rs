//! Workflow lifecycle metadata shared by the coordinator and customer worker.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RunOperation {
    Pause,
    Resume,
    Cancel,
}

impl RunOperation {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pause => "pause",
            Self::Resume => "resume",
            Self::Cancel => "cancel",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RestartDeploy {
    Started,
    Latest,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RestartTarget {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub occurrence: Option<u32>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RestartOptions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<RestartTarget>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deploy: Option<RestartDeploy>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum InvalidRestart {
    #[error("restart target name must not be empty")]
    EmptyTargetName,
    #[error("restart target occurrence exceeds the journal ordinal range")]
    TargetOccurrenceOutOfRange,
    #[error("partial restart cannot change deploy pin")]
    PartialLatest,
}

impl RestartOptions {
    /// Resolve the effective deployment policy before selecting its prerequisite.
    ///
    /// # Errors
    /// Rejects invalid task boundaries and partial restarts onto latest code.
    pub fn effective_deploy(&self) -> Result<RestartDeploy, InvalidRestart> {
        if let Some(target) = &self.from {
            if target.name.is_empty() {
                return Err(InvalidRestart::EmptyTargetName);
            }
            if target
                .occurrence
                .is_some_and(|value| value > i32::MAX as u32)
            {
                return Err(InvalidRestart::TargetOccurrenceOutOfRange);
            }
            if self.deploy == Some(RestartDeploy::Latest) {
                return Err(InvalidRestart::PartialLatest);
            }
            Ok(RestartDeploy::Started)
        } else {
            Ok(self.deploy.unwrap_or(RestartDeploy::Latest))
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RunState {
    Queued,
    Running,
    Sleeping,
    Waiting,
    Paused,
    Stalled,
    Compensating,
    Completed,
    Failed,
    Cancelled,
    ContinuedAsNew,
}

impl RunState {
    /// Every state the enum declares, in declaration order.
    ///
    /// [`Self::position`] is an exhaustive match into this array, so a variant
    /// missing here has no slot to point at and the pair fails
    /// `every_run_state_occupies_the_slot_it_names`.
    pub const ALL: [Self; 11] = [
        Self::Queued,
        Self::Running,
        Self::Sleeping,
        Self::Waiting,
        Self::Paused,
        Self::Stalled,
        Self::Compensating,
        Self::Completed,
        Self::Failed,
        Self::Cancelled,
        Self::ContinuedAsNew,
    ];

    /// Every state a run can rest in for good, as the journal stores them.
    ///
    /// Queries that select live runs by state string read this rather than
    /// spelling the set again, so a new terminal state reaches them too.
    pub const TERMINAL: [&'static str; 5] = [
        Self::Completed.as_str(),
        Self::Failed.as_str(),
        Self::Cancelled.as_str(),
        Self::Stalled.as_str(),
        Self::ContinuedAsNew.as_str(),
    ];

    /// This state's index in [`Self::ALL`].
    ///
    /// The match is exhaustive, so a new variant cannot compile without an arm,
    /// and the only arm that survives the round trip through `ALL` is one whose
    /// index holds that same variant — which `ALL` cannot offer without listing
    /// it. That is what binds the array to the variant set.
    #[must_use]
    pub const fn position(self) -> usize {
        match self {
            Self::Queued => 0,
            Self::Running => 1,
            Self::Sleeping => 2,
            Self::Waiting => 3,
            Self::Paused => 4,
            Self::Stalled => 5,
            Self::Compensating => 6,
            Self::Completed => 7,
            Self::Failed => 8,
            Self::Cancelled => 9,
            Self::ContinuedAsNew => 10,
        }
    }

    /// `Stalled` and `ContinuedAsNew` rest here with the outcomes: the platform
    /// will dispatch none of them again. A stalled run is one the platform gave
    /// up on, so its parents are owed their notification and its delivery job
    /// can settle instead of being kept alive by a manager that reads this to
    /// decide. A continued run handed its work to a successor, so the identity
    /// that carries on is the successor's and this one is finished.
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed | Self::Cancelled | Self::Stalled | Self::ContinuedAsNew
        )
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Sleeping => "sleeping",
            Self::Waiting => "waiting",
            Self::Paused => "paused",
            Self::Stalled => "stalled",
            Self::Compensating => "compensating",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::ContinuedAsNew => "continuedAsNew",
        }
    }
}

impl std::str::FromStr for RunState {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "queued" => Ok(Self::Queued),
            "running" => Ok(Self::Running),
            "sleeping" => Ok(Self::Sleeping),
            "waiting" => Ok(Self::Waiting),
            "paused" => Ok(Self::Paused),
            "stalled" => Ok(Self::Stalled),
            "compensating" => Ok(Self::Compensating),
            "completed" => Ok(Self::Completed),
            "failed" => Ok(Self::Failed),
            "cancelled" => Ok(Self::Cancelled),
            "continuedAsNew" => Ok(Self::ContinuedAsNew),
            _ => Err(format!("unknown workflow state: {value}")),
        }
    }
}

/// The value a creator delivers to a waiting run, and the kind it answers to.
///
/// `payload` is the creator's own JSON. It answers to `AppPolicy::max_input_bytes`
/// wherever it is admitted, which is the same bound a run's input and a step
/// checkpoint answer to, so a transport carrying one derives its request budget
/// from `MAX_INPUT_BYTES_CEILING` rather than from a payload budget.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignalOptions {
    #[serde(rename = "type")]
    pub signal_type: String,
    #[serde(default)]
    pub payload: serde_json::Value,
}

/// The signal the journal recorded, named by the id it was given.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeliveredSignal {
    pub id: String,
}

/// What a run has settled into, and what it left behind.
///
/// `output` LOCATES the result rather than carrying it: a run's result is the
/// object its descriptor names, so this field is the descriptor and the bytes
/// come from a separate payload read. That is what keeps this reply small
/// whatever the run returned, and it is why a status exchange needs no payload
/// budget.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunStatus {
    pub state: RunState,
    pub output: Option<serde_json::Value>,
    pub error: Option<serde_json::Value>,
    /// The run a `continuedAsNew` close handed this run's work to.
    ///
    /// Typed and platform-minted, so it is not confusable with whatever JSON a
    /// creator returned in `output`. Absent from the response unless the run
    /// actually produced a successor.
    #[serde(
        default,
        rename = "continuedAsNew",
        skip_serializing_if = "Option::is_none"
    )]
    pub continued_as_new_run_id: Option<String>,
}
