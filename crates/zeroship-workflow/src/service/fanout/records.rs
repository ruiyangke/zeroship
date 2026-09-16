use super::{invalid, models, AppId, BroadcastId, JobSpec, WorkflowServiceError};
use serde::{Deserialize, Serialize};
use zeroship_data_orm::orm::{FromRow, Insertable};

#[derive(FromRow, Insertable)]
#[orm(entity = models::topics)]
pub(super) struct TopicRecord {
    pub id: String,
    pub app_id: String,
    pub topic: String,
    pub signal_epoch: i64,
    pub accepted_sequence: i64,
    pub completed_sequence: i64,
}
impl TopicRecord {
    pub fn validate(&self, app: &AppId, topic: &str) -> Result<(), WorkflowServiceError> {
        if self.app_id != app.as_str()
            || self.topic != topic
            || self.accepted_sequence < 0
            || self.completed_sequence < 0
            || self.completed_sequence > self.accepted_sequence
        {
            return Err(invalid());
        }
        Ok(())
    }
}

#[derive(FromRow, Insertable)]
#[orm(entity = models::broadcasts)]
pub(super) struct Broadcast {
    pub id: String,
    pub app_id: String,
    pub topic: String,
    pub signal_type: String,
    pub payload: String,
    pub created_at: i64,
    pub cursor: i64,
    pub cutoff_sequence: i64,
    pub origin: String,
    pub finished: i64,
    pub sequence: i64,
    pub revision: i64,
}
impl Broadcast {
    pub fn validate(&self, app: &AppId, id: &BroadcastId) -> Result<(), WorkflowServiceError> {
        if self.app_id != app.as_str()
            || self.id != id.as_str()
            || self.sequence <= 0
            || self.revision <= 0
            || self.cursor < 0
            || self.cursor > self.cutoff_sequence
            || !matches!(self.finished, 0 | 1)
            || self.created_at < 0
        {
            return Err(invalid());
        }
        super::super::signals::validate_topic(&self.topic)?;
        crate::validation::signal_type(&self.signal_type)?;
        Ok(())
    }
    pub fn definition(&self) -> Definition {
        Definition {
            id: self.id.clone(),
            topic: self.topic.clone(),
            sequence: self.sequence,
            signal_type: self.signal_type.clone(),
            payload: self.payload.clone(),
            origin: self.origin.clone(),
            created_at: self.created_at,
            cutoff_sequence: self.cutoff_sequence,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Definition {
    pub id: String,
    pub topic: String,
    pub sequence: i64,
    pub signal_type: String,
    pub payload: String,
    pub origin: String,
    pub created_at: i64,
    pub cutoff_sequence: i64,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PageResult {
    pub broadcast: Definition,
    pub revision: i64,
    pub before: i64,
    pub after: i64,
    pub finished: bool,
    pub delivered: i64,
    pub successors: Vec<JobSpec>,
}

#[derive(FromRow, Insertable)]
#[orm(entity = models::fanout_pages)]
pub(super) struct Page {
    pub id: String,
    pub app_id: String,
    pub broadcast_id: String,
    pub revision: i64,
    pub result: String,
}

#[derive(Insertable)]
#[orm(entity = models::job_receipts)]
pub(super) struct Pending {
    pub id: String,
    pub app_id: String,
    pub specification: String,
    pub created_at: i64,
}
