use super::{invalid, models, AppId, JobSpec, PropagationId, WorkflowServiceError};
use serde::{Deserialize, Serialize};
use zeroship_core::{typed_id, workflow_coordination::RunId};
use zeroship_data_orm::orm::{FromRow, Insertable};

/// What one obligation delivers from its source generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Kind {
    /// Cancellation intent for the source generation's cascading children.
    Cascade,
    /// Wake-ups for parents waiting on the source generation's head.
    Notify,
}
impl Kind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cascade => "cascade",
            Self::Notify => "notify",
        }
    }
    fn parse(value: &str) -> Result<Self, WorkflowServiceError> {
        match value {
            "cascade" => Ok(Self::Cascade),
            "notify" => Ok(Self::Notify),
            _ => Err(invalid()),
        }
    }
    /// Cascade pages move over child run identities; notify pages over parent
    /// wait identities.
    fn validate_cursor(self, cursor: &str) -> Result<(), WorkflowServiceError> {
        let valid = match self {
            Self::Cascade => RunId::parse(cursor).is_ok(),
            Self::Notify => typed_id::parse_with_prefix(cursor, "wjr").is_ok(),
        };
        if valid {
            Ok(())
        } else {
            Err(invalid())
        }
    }
}

#[derive(Debug, FromRow, Insertable)]
#[orm(entity = models::propagations)]
pub(super) struct Obligation {
    pub id: String,
    pub app_id: String,
    pub run_id: String,
    pub generation: i64,
    pub kind: String,
    pub cursor: Option<String>,
    pub revision: i64,
    pub finished: i64,
    pub created_at: i64,
}
impl Obligation {
    pub fn validate(&self, app: &AppId) -> Result<Kind, WorkflowServiceError> {
        PropagationId::parse(&self.id).map_err(|_| invalid())?;
        RunId::parse(&self.run_id).map_err(|_| invalid())?;
        let kind = Kind::parse(&self.kind)?;
        if self.app_id != app.as_str()
            || self.generation < 0
            || self.revision <= 0
            || !matches!(self.finished, 0 | 1)
            || self.created_at < 0
            || (self.revision == 1 && self.cursor.is_some())
        {
            return Err(invalid());
        }
        if let Some(cursor) = &self.cursor {
            kind.validate_cursor(cursor)?;
        }
        Ok(kind)
    }
    pub fn definition(&self) -> Result<Definition, WorkflowServiceError> {
        Ok(Definition {
            id: self.id.clone(),
            kind: Kind::parse(&self.kind)?,
            run_id: self.run_id.clone(),
            generation: self.generation,
            created_at: self.created_at,
        })
    }
}

/// Immutable obligation identity copied into every page result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Definition {
    pub id: String,
    pub kind: Kind,
    pub run_id: String,
    pub generation: i64,
    pub created_at: i64,
}

/// The closed result of one committed page. `superseded` records a notify page
/// whose head no longer names the terminal source generation.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PageResult {
    pub propagation: Definition,
    pub revision: i64,
    pub before: Option<String>,
    pub after: Option<String>,
    pub finished: bool,
    pub superseded: bool,
    pub affected: i64,
    pub successors: Vec<JobSpec>,
}

#[derive(FromRow, Insertable)]
#[orm(entity = models::propagation_pages)]
pub(super) struct Page {
    pub id: String,
    pub app_id: String,
    pub propagation_id: String,
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
