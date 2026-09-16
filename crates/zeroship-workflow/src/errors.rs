use compio_postgres::error::SqlState;

/// Operation failures shared by embedded callers and remote clients.
///
/// HTTP status codes and response bodies are translated by the client adapter;
/// embedded storage and execution do not manufacture transport failures.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkflowServiceError {
    InvalidRequest(String),
    Unauthenticated,
    PermissionDenied,
    NotFound(String),
    Conflict(String),
    ResourceExhausted(String),
    PayloadTooLarge,
    Unavailable(String),
    Timeout,
    /// Ingress acceptance needs an open manager epoch above the carried one.
    /// Retry after establishing it; `None` means no epoch was held and the
    /// journal has closed none. It is never a durable customer refusal.
    IngressFenced(Option<zeroship_core::workflow_coordination::Revision>),
    Internal(String),
}

impl WorkflowServiceError {
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::InvalidRequest(_) => "workflow_invalid_request",
            Self::Unauthenticated => "workflow_unauthenticated",
            Self::PermissionDenied => "workflow_permission_denied",
            Self::NotFound(_) => "workflow_not_found",
            Self::Conflict(_) => "workflow_conflict",
            Self::ResourceExhausted(_) => "workflow_resource_exhausted",
            Self::PayloadTooLarge => "workflow_payload_too_large",
            Self::Unavailable(_) => "workflow_unavailable",
            Self::Timeout => "workflow_timeout",
            Self::IngressFenced(_) => "workflow_ingress_fenced",
            Self::Internal(_) => "workflow_internal_error",
        }
    }
}

impl std::fmt::Display for WorkflowServiceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidRequest(message)
            | Self::NotFound(message)
            | Self::Conflict(message)
            | Self::ResourceExhausted(message)
            | Self::Unavailable(message)
            | Self::Internal(message) => f.write_str(message),
            Self::Unauthenticated => f.write_str("workflow credentials are missing or expired"),
            Self::PermissionDenied => f.write_str("workflow operation is not permitted"),
            Self::PayloadTooLarge => f.write_str("workflow payload exceeds the configured limit"),
            Self::Timeout => f.write_str("workflow operation timed out"),
            Self::IngressFenced(_) => {
                f.write_str("workflow ingress requires a newer recovery epoch")
            }
        }
    }
}

impl std::error::Error for WorkflowServiceError {}

impl From<zeroship_core::workflow_policy::InvalidPolicy> for WorkflowServiceError {
    fn from(error: zeroship_core::workflow_policy::InvalidPolicy) -> Self {
        Self::InvalidRequest(error.to_string())
    }
}

impl From<zeroship_workflow_calendar::CalendarError> for WorkflowServiceError {
    fn from(error: zeroship_workflow_calendar::CalendarError) -> Self {
        Self::InvalidRequest(error.to_string())
    }
}

impl From<zeroship_core::workflow_deployments::Error> for WorkflowServiceError {
    fn from(error: zeroship_core::workflow_deployments::Error) -> Self {
        match error {
            zeroship_core::workflow_deployments::Error::InvalidHolder => {
                Self::InvalidRequest("invalid deployment holder".into())
            }
            zeroship_core::workflow_deployments::Error::GenerationExhausted => {
                Self::ResourceExhausted("deployment hold generation exhausted".into())
            }
        }
    }
}

#[derive(Debug)]
pub enum WorkflowError {
    Invalid(String),
    CompensableCarry(String),
    Deadlock(String),
    Db(String),
}

impl std::fmt::Display for WorkflowError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(msg) => write!(f, "invalid workflow StepResult: {msg}"),
            Self::CompensableCarry(msg) => write!(f, "CompensableCarryError: {msg}"),
            Self::Deadlock(msg) => write!(f, "deadlock: {msg}"),
            Self::Db(msg) => write!(f, "{msg}"),
        }
    }
}

impl std::error::Error for WorkflowError {}

impl From<compio_postgres::Error> for WorkflowError {
    fn from(e: compio_postgres::Error) -> Self {
        if e.code() == Some(&SqlState::T_R_DEADLOCK_DETECTED) {
            Self::Deadlock(e.to_string())
        } else {
            let msg = e.to_string();
            let full = match source_chain(&e) {
                Some(chain) => format!("{msg}: {chain}"),
                None => msg,
            };
            Self::Db(full)
        }
    }
}

fn source_chain(err: &dyn std::error::Error) -> Option<String> {
    let mut out = String::new();
    let mut cur = err.source();
    while let Some(e) = cur {
        if !out.is_empty() {
            out.push_str(" | ");
        }
        out.push_str(&e.to_string());
        cur = e.source();
    }
    (!out.is_empty()).then_some(out)
}
