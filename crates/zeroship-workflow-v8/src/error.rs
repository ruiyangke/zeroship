use zeroship_runtime::state::OpError;
use zeroship_workflow::WorkflowServiceError;

pub(crate) fn to_op_error(error: WorkflowServiceError) -> OpError {
    if let WorkflowServiceError::InvalidRequest(message) = &error {
        return OpError::type_error(message.clone());
    }
    let message = match &error {
        WorkflowServiceError::Internal(_) => "workflow operation failed".to_owned(),
        WorkflowServiceError::Unavailable(_) => "workflow service is unavailable".to_owned(),
        _ => error.to_string(),
    };
    OpError::coded(error.code(), message, None::<String>)
}
