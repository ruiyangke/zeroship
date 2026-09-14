//! Stable creator-owned child identity across physical generations.
//!
//! Callers hold the app lock and commit membership changes with the originating
//! lifecycle transition and publication. These helpers never open a transaction.

use super::{models, store::Transaction};
use crate::{operations::RunState, WorkflowServiceError};
use zeroship_core::app_id::AppId;
use zeroship_data_orm::orm::FindOptions;

mod mutation;
mod read;
mod records;
mod sources;
pub use mutation::{advance, create, restart};
pub use read::{by_id, historical, member, resolve, waiting};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoricalMember {
    pub id: String,
    pub head_id: String,
    pub revision: i64,
    pub run_id: String,
    pub generation: i64,
    pub state: String,
    pub outcome: models::GenerationOutcome,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Member {
    pub id: String,
    pub head_id: String,
    pub revision: i64,
    pub run_id: String,
    pub generation: i64,
    pub is_current: bool,
}

pub struct ResolvedChild {
    pub accepted: Member,
    pub current: Member,
    pub state: RunState,
    pub outcome: models::GenerationOutcome,
}

fn one() -> FindOptions {
    FindOptions {
        limit: Some(1),
        ..Default::default()
    }
}

fn invalid() -> WorkflowServiceError {
    WorkflowServiceError::Internal("invalid workflow continuation journal".into())
}
