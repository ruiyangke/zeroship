//! Journal model metadata comes from the canonical migration artifact.

use super::{schema, store::database_error};
use crate::WorkflowServiceError;
use zeroship_data_orm::{binding::DbBinding, descriptor, orm::FromRow, Value};

zeroship_data_orm::orm::schema!(pub journal = "../../schema/schema.runtime.json");
pub use journal::{
    __zeroship_workflow_app_state as app_state, __zeroship_workflow_broadcasts as broadcasts,
    __zeroship_workflow_deployment_holds as deployment_holds,
    __zeroship_workflow_deploys as deploys, __zeroship_workflow_generations as generations,
    __zeroship_workflow_occurrences as occurrences,
    __zeroship_workflow_payload_refs as payload_refs, __zeroship_workflow_payloads as payloads,
    __zeroship_workflow_requests as requests, __zeroship_workflow_runs as runs,
    __zeroship_workflow_schedules as schedules,
    __zeroship_workflow_schema_version as schema_version, __zeroship_workflow_signals as signals,
    __zeroship_workflow_steps as steps, __zeroship_workflow_subscriptions as subscriptions,
    __zeroship_workflow_tasks as tasks, __zeroship_workflow_topics as topics,
    __zeroship_workflow_waits as waits,
};

#[derive(FromRow)]
#[orm(entity = schema_version)]
pub struct Fingerprint {
    pub fingerprint: String,
}

#[derive(FromRow)]
#[orm(entity = deploys)]
pub struct DeploymentManifest {
    pub manifest: String,
}

#[derive(FromRow)]
#[orm(entity = deploys)]
pub struct DeploymentHash {
    pub hash: String,
}

#[derive(FromRow)]
#[orm(entity = requests)]
pub struct RequestResult {
    pub operation: String,
    pub digest: String,
    pub result: String,
}

#[derive(FromRow)]
#[orm(entity = runs)]
pub struct KeyedRun {
    pub id: String,
    pub state: String,
}

#[derive(FromRow)]
#[orm(entity = runs)]
pub struct RunHead {
    pub state: String,
    pub generation: i64,
}

#[derive(FromRow)]
#[orm(entity = generations)]
pub struct GenerationOutcome {
    pub output: Option<String>,
    pub output_ref: Option<String>,
    pub error: Option<String>,
}

#[derive(FromRow)]
#[orm(entity = generations)]
pub struct GenerationInput {
    pub input: String,
    pub input_ref: Option<String>,
    pub started_at: i64,
}

#[derive(FromRow)]
#[orm(entity = steps)]
pub struct StoredStep {
    pub ordinal: i64,
    pub record: String,
}

#[derive(FromRow)]
#[orm(entity = steps)]
pub struct CompensationRecord {
    pub compensation_attempts: i64,
    pub compensation_retry_ms: i64,
    pub compensation_due_at: Option<i64>,
}

#[derive(FromRow)]
#[orm(entity = steps)]
pub struct CompensationFailure {
    pub ordinal: i64,
    pub compensation_error: Option<String>,
}

#[derive(FromRow)]
#[orm(entity = steps)]
pub struct ParentStep {
    pub run_id: String,
    pub generation: i64,
    pub ordinal: i64,
    pub record: String,
}

#[derive(FromRow)]
#[orm(entity = tasks)]
pub struct TaskRecord {
    pub id: String,
    pub app_id: String,
    pub run_id: String,
    pub generation: i64,
    pub epoch: i64,
    pub deadline: i64,
    pub state: String,
    pub completion_digest: Option<String>,
    pub receipt: Option<String>,
}

#[derive(FromRow)]
#[orm(entity = app_state)]
pub struct SubscriptionSequence {
    pub subscription_sequence: i64,
}

#[derive(FromRow)]
#[orm(entity = app_state)]
pub struct AppSignalEpoch {
    pub signal_epoch: i64,
}

#[derive(FromRow)]
#[orm(entity = topics)]
pub struct TopicSignalEpoch {
    pub signal_epoch: i64,
}

#[derive(FromRow)]
#[orm(entity = subscriptions)]
pub struct SubscriptionRecipient {
    pub run_id: String,
    pub generation: i64,
    pub ordinal: i64,
    pub sequence: i64,
}

#[derive(FromRow)]
#[orm(entity = signals)]
pub struct SignalMessage {
    pub id: String,
    pub payload: String,
    pub created_at: i64,
    pub origin: String,
    pub delivery: String,
    pub topic: Option<String>,
}

#[derive(FromRow)]
#[orm(entity = schedules)]
pub struct ScheduleRecord {
    pub id: String,
    pub name: String,
    pub deploy_id: String,
    pub definition: String,
    pub next_at: Option<i64>,
    pub anchor_at: i64,
    pub revision: i64,
}

#[derive(FromRow)]
#[orm(entity = payloads)]
pub struct PayloadRecord {
    pub id: String,
    pub hash: String,
    pub size: i64,
    pub content_type: Option<String>,
    pub state: String,
    pub expires_at: i64,
}

/// Compose the journal with the app's existing descriptors in one publication.
/// A conflicting host descriptor fails without replacing the previous binding.
pub fn install(binding: &DbBinding) -> Result<(), WorkflowServiceError> {
    let artifact: Value =
        serde_json::from_str(schema::RUNTIME_DESCRIPTOR).map_err(|_| schema::incompatible())?;
    let tables = artifact["collections"]
        .as_object()
        .ok_or_else(schema::incompatible)?;
    let mut collections: std::collections::BTreeMap<_, _> =
        descriptor::declared_collections(binding)
            .into_iter()
            .map(|(name, fields)| (name, fields.as_ref().clone()))
            .collect();
    for (name, table) in tables {
        let fields = table.get("fields").ok_or_else(schema::incompatible)?;
        if let Some(existing) = collections.get(name) {
            if existing != fields {
                return Err(schema::incompatible());
            }
        } else {
            collections.insert(name.clone(), fields.clone());
        }
    }
    descriptor::install_collections(binding, collections.into_iter().collect())
        .map_err(database_error)
}
