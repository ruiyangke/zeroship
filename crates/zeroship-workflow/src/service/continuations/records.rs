use super::{invalid, models, AppId, HistoricalMember, Member, WorkflowServiceError};
use zeroship_core::{typed_id, workflow_coordination::RunId};
use zeroship_data_orm::orm::{FromRow, Insertable};

#[derive(FromRow, Insertable)]
#[orm(entity = models::continuation_heads)]
pub(super) struct Head {
    pub id: String,
    pub app_id: String,
    pub current_generation_id: String,
    pub revision: i64,
}

#[derive(FromRow)]
#[orm(entity = models::continuation_heads)]
pub(super) struct HeadIdentity {
    pub id: String,
    pub app_id: String,
}

#[derive(FromRow, Insertable)]
#[orm(entity = models::continuation_members)]
pub(super) struct Membership {
    pub id: String,
    pub app_id: String,
    pub head_id: String,
    pub revision: i64,
}

#[derive(FromRow)]
#[orm(entity = models::generations)]
pub(super) struct Generation {
    pub id: String,
    pub app_id: String,
    pub run_id: String,
    pub generation: i64,
    pub state: String,
    pub output: Option<String>,
    pub output_ref: Option<String>,
    pub error: Option<String>,
    pub continued_as_new_run_id: Option<String>,
}

#[derive(FromRow)]
#[orm(entity = models::runs)]
pub(super) struct Run {
    pub id: String,
    pub app_id: String,
    pub generation: i64,
    pub state: String,
}

#[derive(FromRow)]
#[orm(entity = models::steps)]
pub(super) struct ChildStep {
    pub id: String,
    pub app_id: String,
    pub run_id: String,
    pub generation: i64,
    pub ordinal: i64,
    pub kind: String,
    pub state: String,
    pub record: String,
    pub child_member_id: Option<String>,
    pub child_result_member_id: Option<String>,
}

pub(super) fn journal_id(id: &str) -> Result<(), WorkflowServiceError> {
    typed_id::parse_with_prefix(id, "wjr")
        .map(|_| ())
        .map_err(|_| invalid())
}

impl Generation {
    pub fn validate(&self, app: &AppId) -> Result<(), WorkflowServiceError> {
        journal_id(&self.id)?;
        RunId::parse(&self.run_id).map_err(|_| invalid())?;
        if self.app_id != app.as_str() || self.generation < 0 {
            return Err(invalid());
        }
        if self.state != "restarted" {
            super::super::app::parse_state(&self.state)?;
        }
        Ok(())
    }

    pub fn outcome(self) -> models::GenerationOutcome {
        models::GenerationOutcome {
            output: self.output,
            output_ref: self.output_ref,
            error: self.error,
            continued_as_new_run_id: self.continued_as_new_run_id,
        }
    }
}

impl Membership {
    pub fn identity(
        &self,
        app: &AppId,
        generation: &Generation,
    ) -> Result<HistoricalMember, WorkflowServiceError> {
        generation.validate(app)?;
        journal_id(&self.id)?;
        journal_id(&self.head_id)?;
        if self.app_id != app.as_str() || self.id != generation.id || self.revision <= 0 {
            return Err(invalid());
        }
        Ok(HistoricalMember {
            id: self.id.clone(),
            head_id: self.head_id.clone(),
            revision: self.revision,
            run_id: generation.run_id.clone(),
            generation: generation.generation,
            state: generation.state.clone(),
            outcome: models::GenerationOutcome {
                output: generation.output.clone(),
                output_ref: generation.output_ref.clone(),
                error: generation.error.clone(),
                continued_as_new_run_id: generation.continued_as_new_run_id.clone(),
            },
        })
    }
}

impl HistoricalMember {
    pub(super) fn observed(self, is_current: bool) -> Member {
        Member {
            id: self.id,
            head_id: self.head_id,
            revision: self.revision,
            run_id: self.run_id,
            generation: self.generation,
            is_current,
        }
    }
}

impl Member {
    pub(super) fn same_identity(&self, other: &Self) -> bool {
        self.id == other.id
            && self.head_id == other.head_id
            && self.revision == other.revision
            && self.run_id == other.run_id
            && self.generation == other.generation
    }
}
