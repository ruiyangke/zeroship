use zeroship_runtime::state::OpError;
use zeroship_workflow::WorkflowRpcError;

pub(crate) fn to_op_error(error: WorkflowRpcError) -> OpError {
    let code = match &error {
        WorkflowRpcError::InvalidRequest(message) => return OpError::type_error(message.clone()),
        WorkflowRpcError::Transport(_) => "workflow_transport_error",
        WorkflowRpcError::Timeout => "workflow_timeout",
        WorkflowRpcError::Http { .. } => "workflow_http_error",
        WorkflowRpcError::Decode(_) => "workflow_decode_error",
    };
    OpError::coded(code, error.to_string(), None::<String>)
}
