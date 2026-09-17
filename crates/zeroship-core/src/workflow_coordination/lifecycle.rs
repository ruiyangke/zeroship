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
#[serde(rename_all = "lowercase")]
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
}

impl RunState {
    /// Every state a run can rest in for good, as the journal stores them.
    ///
    /// Queries that select live runs by state string read this rather than
    /// spelling the set again, so a new terminal state reaches them too.
    pub const TERMINAL: [&'static str; 4] = [
        Self::Completed.as_str(),
        Self::Failed.as_str(),
        Self::Cancelled.as_str(),
        Self::Stalled.as_str(),
    ];

    /// `Stalled` rests here with the other three: the platform has given up on
    /// the run, so nothing further will dispatch it, its parents are owed their
    /// notification, and its delivery job can settle instead of being kept
    /// alive by a manager that reads this to decide.
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed | Self::Cancelled | Self::Stalled
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
            _ => Err(format!("unknown workflow state: {value}")),
        }
    }
}
