//! Host-owned application startup and requests waiting for readiness.

use std::time::Instant;

use super::channel::ResultSender;
use super::modules::ModuleEvaluation;
use super::{application_entry::ApplicationEntry, dev_entry::DevEntryLoader};
use crate::{EnvSnapshot, RequestCtx, SettledFetch};
use std::rc::Rc;

pub enum EvaluationPhase {
    Running,
    Adapters {
        entry: v8::Global<v8::Module>,
        promises: Vec<v8::Global<v8::Promise>>,
    },
    Creator(ModuleEvaluation),
    Dev {
        loader: DevEntryLoader,
        application: Option<Rc<ApplicationEntry>>,
    },
}

pub enum StartupState {
    Uninitialized,
    Evaluating {
        started: Instant,
        descriptor: Option<serde_json::Value>,
        phase: EvaluationPhase,
    },
    Finalizing,
    Ready,
    Failed(String),
}

impl StartupState {
    pub const fn is_pending(&self) -> bool {
        matches!(self, Self::Evaluating { .. } | Self::Finalizing)
    }

    pub const fn started(&self) -> Option<Instant> {
        match self {
            Self::Evaluating { started, .. } => Some(*started),
            _ => None,
        }
    }
}

pub struct WaitingRequest {
    pub started: Instant,
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    pub env: EnvSnapshot,
    pub ctx: RequestCtx,
    pub user_json: Option<String>,
    pub reply: ResultSender<Result<SettledFetch, super::runtime::DispatchError>>,
}

pub fn failure_response(error: &str) -> crate::FetchOutcome {
    crate::FetchOutcome::Response {
        status: 500,
        headers: vec![("content-type".into(), "application/json".into())],
        body: serde_json::json!({ "message": error, "name": "Error" })
            .to_string()
            .into_bytes(),
        logs: vec![],
    }
}
