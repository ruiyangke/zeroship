//! Shared deployment retention identities and metadata, without database access.
//!
//! Journal holders survive worker replacement. A journal holder does not confer
//! authority on a manager queue; the host authenticates each retention caller.

use crate::{
    app_id::AppId, typed_id, workflow_coordination::Revision, workflow_jobs::DeploymentId,
};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    #[error("invalid deployment holder")]
    InvalidHolder,
    #[error("deployment hold generation exhausted")]
    GenerationExhausted,
}

/// Stable host retention identity, independent of process instances.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HoldScope {
    app: AppId,
    holder: String,
}
impl HoldScope {
    /// Stable journal identity for the app, preserved across worker replacement.
    #[must_use]
    pub fn for_app(app: AppId) -> Self {
        let holder = format!("dhl_{}", app.as_str().trim_start_matches("app_"));
        Self { app, holder }
    }

    /// Stable queue identity for the app's authoritative manager namespace.
    /// Queue and journal generations cannot release each other's retention.
    #[must_use]
    pub fn for_queue(app: AppId) -> Self {
        let holder = format!("dqh_{}", app.as_str().trim_start_matches("app_"));
        Self { app, holder }
    }

    #[must_use]
    pub fn app(&self) -> &AppId {
        &self.app
    }

    #[must_use]
    pub fn holder(&self) -> &str {
        &self.holder
    }

    /// The trusted host obtains the holder class from authenticated authority.
    pub fn new(app: AppId, holder: String) -> Result<Self, Error> {
        typed_id::parse_with_prefix(&holder, "dhl")
            .or_else(|_| typed_id::parse_with_prefix(&holder, "dqh"))
            .map_err(|_| Error::InvalidHolder)?;
        Ok(Self { app, holder })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "i64", into = "i64")]
pub struct HoldGeneration(i64);
impl HoldGeneration {
    #[must_use]
    pub const fn get(self) -> i64 {
        self.0
    }
    pub fn next(self) -> Result<Self, Error> {
        self.0
            .checked_add(1)
            .map(Self)
            .ok_or(Error::GenerationExhausted)
    }
}
impl TryFrom<i64> for HoldGeneration {
    type Error = &'static str;
    fn try_from(value: i64) -> Result<Self, Self::Error> {
        if value > 0 {
            Ok(Self(value))
        } else {
            Err("deployment hold generation must be positive")
        }
    }
}
impl From<HoldGeneration> for i64 {
    fn from(value: HoldGeneration) -> Self {
        value.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HoldState {
    Held,
    Released,
}
impl HoldState {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Held => "held",
            Self::Released => "released",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HoldReceipt {
    pub app_id: AppId,
    pub deploy_id: String,
    pub holder_id: String,
    pub generation: HoldGeneration,
    pub state: HoldState,
    pub deploy_hash: String,
}

/// Metadata only. Control derives journal ownership after authenticating the
/// worker and verifying this app assignment with the coordinator.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HoldRequest {
    pub app_id: AppId,
    pub assignment_revision: Revision,
    pub deploy_id: String,
    pub generation: HoldGeneration,
}

/// Metadata selected by the trusted workflow manager. Control derives the queue
/// holder from the verified service role, never from request fields.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct QueueHoldRequest {
    pub app_id: AppId,
    pub deploy_id: DeploymentId,
    pub generation: HoldGeneration,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn journal_scope_is_stable_across_worker_replacement() {
        let app = AppId::mint();
        let first = HoldScope::for_app(app.clone());
        assert_eq!(first, HoldScope::for_app(app.clone()));
        assert_ne!(first, HoldScope::for_app(AppId::mint()));
        assert_eq!(
            HoldScope::new(app, first.holder().to_owned()).unwrap(),
            first
        );
        assert_eq!(
            HoldScope::new(AppId::mint(), typed_id::generate("wrk")),
            Err(Error::InvalidHolder)
        );
    }

    #[test]
    fn queue_scope_is_stable_and_distinct_from_journal_scope() {
        let app = AppId::mint();
        let queue = HoldScope::for_queue(app.clone());
        let journal = HoldScope::for_app(app.clone());
        assert_eq!(queue, HoldScope::for_queue(app.clone()));
        assert_ne!(queue, journal);
        assert_ne!(queue, HoldScope::for_queue(AppId::mint()));
        assert_eq!(queue.app(), &app);
        typed_id::parse_with_prefix(queue.holder(), "dqh").unwrap();
        assert_eq!(
            HoldScope::new(app, queue.holder().to_owned()).unwrap(),
            queue
        );
    }

    #[test]
    fn queue_hold_request_preserves_typed_scope_and_rejects_body_authority() {
        let request = QueueHoldRequest {
            app_id: AppId::mint(),
            deploy_id: DeploymentId::mint(),
            generation: HoldGeneration::try_from(1).unwrap(),
        };
        let encoded = serde_json::to_value(&request).unwrap();
        assert_eq!(
            serde_json::from_value::<QueueHoldRequest>(encoded.clone()).unwrap(),
            request
        );
        for field in [
            "holderId",
            "assignmentRevision",
            "workerId",
            "input",
            "credentials",
        ] {
            let mut invalid = encoded.clone();
            invalid[field] = serde_json::json!({"forged":true});
            assert!(
                serde_json::from_value::<QueueHoldRequest>(invalid).is_err(),
                "{field}"
            );
        }
        for (field, value) in [
            ("appId", serde_json::json!(DeploymentId::mint())),
            ("deployId", serde_json::json!(AppId::mint())),
            ("deployId", serde_json::json!("dep_invalid")),
            ("generation", serde_json::json!(0)),
            ("generation", serde_json::json!(-1)),
            ("generation", serde_json::json!(1.5)),
        ] {
            let mut invalid = encoded.clone();
            invalid[field] = value;
            assert!(
                serde_json::from_value::<QueueHoldRequest>(invalid).is_err(),
                "{field}"
            );
        }
        for field in ["appId", "deployId", "generation"] {
            let mut invalid = encoded.clone();
            invalid.as_object_mut().unwrap().remove(field);
            assert!(
                serde_json::from_value::<QueueHoldRequest>(invalid).is_err(),
                "{field}"
            );
        }
    }

    #[test]
    fn hold_generations_reject_invalid_values_and_overflow() {
        for invalid in [0, -1, i64::MIN] {
            assert!(HoldGeneration::try_from(invalid).is_err());
            assert!(serde_json::from_value::<HoldGeneration>(serde_json::json!(invalid)).is_err());
        }
        let maximum = HoldGeneration::try_from(i64::MAX).unwrap();
        assert_eq!(maximum.next(), Err(Error::GenerationExhausted));
        let first = HoldGeneration::try_from(1).unwrap();
        assert_eq!(first.next().unwrap().get(), 2);
        assert_eq!(serde_json::to_value(first).unwrap(), serde_json::json!(1));
    }

    #[test]
    fn receipt_preserves_metadata_contract() {
        let app = AppId::mint();
        let scope = HoldScope::for_app(app.clone());
        let generation = HoldGeneration::try_from(1).unwrap();
        let receipt = HoldReceipt {
            app_id: app,
            deploy_id: typed_id::generate("dep"),
            holder_id: scope.holder().to_owned(),
            generation,
            state: HoldState::Held,
            deploy_hash: "a".repeat(64),
        };
        let encoded = serde_json::to_value(&receipt).unwrap();
        assert_eq!(encoded["state"], "held");
        assert_eq!(encoded["generation"], 1);
        assert_eq!(
            serde_json::from_value::<HoldReceipt>(encoded.clone()).unwrap(),
            receipt
        );
        let mut invalid = encoded;
        invalid["unknown"] = serde_json::json!(true);
        assert!(serde_json::from_value::<HoldReceipt>(invalid).is_err());
    }
    #[test]
    fn hold_request_rejects_missing_authority_and_unrecognized_metadata() {
        let request = HoldRequest {
            app_id: AppId::mint(),
            assignment_revision: Revision::try_from(1).unwrap(),
            deploy_id: typed_id::generate("dep"),
            generation: HoldGeneration::try_from(1).unwrap(),
        };
        let encoded = serde_json::to_value(&request).unwrap();
        assert_eq!(
            serde_json::from_value::<HoldRequest>(encoded.clone()).unwrap(),
            request
        );
        let mut missing = encoded.clone();
        missing
            .as_object_mut()
            .unwrap()
            .remove("assignmentRevision");
        assert!(serde_json::from_value::<HoldRequest>(missing).is_err());
        let mut invalid = encoded;
        invalid["holderId"] = serde_json::json!(typed_id::generate("dhl"));
        assert!(serde_json::from_value::<HoldRequest>(invalid).is_err());
    }
}
