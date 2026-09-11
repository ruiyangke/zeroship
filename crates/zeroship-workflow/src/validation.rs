//! Validation shared by Rust callers, local persistence and HTTP handlers.

use crate::{operations::StartOptions, WorkflowServiceError};

pub const WORKFLOW_NAME_MAX_BYTES: usize = 128;
pub const WORKFLOW_KEY_MAX_BYTES: usize = 1024;
pub const SIGNAL_TYPE_MAX_BYTES: usize = 256;

/// # Errors
/// Rejects empty, oversized or reserved workflow names.
pub fn workflow_name(name: &str) -> Result<(), WorkflowServiceError> {
    bounded_nonempty(name, WORKFLOW_NAME_MAX_BYTES, "workflow name")?;
    if name.starts_with("__zs.") {
        return Err(WorkflowServiceError::InvalidRequest(
            "workflow name uses a reserved prefix".into(),
        ));
    }
    Ok(())
}

/// # Errors
/// Rejects an explicitly empty or oversized deduplication key.
pub fn key(key: Option<&str>) -> Result<(), WorkflowServiceError> {
    if let Some(key) = key {
        bounded_nonempty(key, WORKFLOW_KEY_MAX_BYTES, "workflow key")?;
    }
    Ok(())
}

/// # Errors
/// Rejects invalid start options before persistence or network I/O.
pub fn start(options: &StartOptions) -> Result<(), WorkflowServiceError> {
    key(options.key.as_deref())
}

/// # Errors
/// Rejects empty or oversized types and refuses the system signal namespace.
pub fn signal_type(signal_type: &str) -> Result<(), WorkflowServiceError> {
    bounded_nonempty(signal_type, SIGNAL_TYPE_MAX_BYTES, "signal type")?;
    if signal_type.starts_with("__zs.") {
        return Err(WorkflowServiceError::PermissionDenied);
    }
    Ok(())
}

fn bounded_nonempty(value: &str, max: usize, name: &str) -> Result<(), WorkflowServiceError> {
    if value.is_empty() || value.len() > max {
        return Err(WorkflowServiceError::InvalidRequest(format!(
            "{name} must be nonempty and at most {max} bytes"
        )));
    }
    Ok(())
}
