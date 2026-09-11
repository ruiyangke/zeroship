use super::*;
use crate::{
    engine::WorkflowOutputRef,
    service::{
        wire::{
            ReadAppPayload, ReadTaskPayload, PAYLOAD_HEADER, REQUEST_ID_HEADER, TASK_TOKEN_HEADER,
        },
        PayloadRead, PayloadSlot, StagedPayload,
    },
};
use bytes::Bytes;
use futures::{channel::mpsc, SinkExt, StreamExt};
use std::{cell::RefCell, rc::Rc, time::Instant};
use zeroship_storage::{
    backend::{BoxChunkSource, ChunkResult, ChunkSource},
    StorageError,
};

impl WorkflowEndpoint {
    async fn download<T: Serialize>(
        &self,
        path: &str,
        authorization: &str,
        body: T,
        expected: Option<&WorkflowOutputRef>,
    ) -> Result<PayloadRead, WorkflowServiceError> {
        let client = CLIENT.with(Clone::clone);
        let body = serde_json::to_vec(&body).map_err(|_| {
            WorkflowServiceError::InvalidRequest("invalid workflow payload request".into())
        })?;
        let builder = client
            .post(format!("{}{path}", self.base))
            .map_err(|_| invalid_request())?
            .header("authorization", authorization)
            .map_err(|_| WorkflowServiceError::Unauthenticated)?
            .header("content-type", "application/json")
            .map_err(|_| invalid_request())?
            .body(body);
        let deadline = Instant::now()
            .checked_add(self.timeout)
            .ok_or_else(invalid_request)?;
        compio::time::timeout(self.timeout, async {
            let response = builder.send().await.map_err(|_| {
                WorkflowServiceError::Unavailable("workflow payload service unreachable".into())
            })?;
            if !response.status().is_success() {
                return self
                    .decode_response::<serde_json::Value>(response)
                    .await
                    .and_then(|_| {
                        Err(WorkflowServiceError::Unavailable(
                            "invalid workflow payload error".into(),
                        ))
                    });
            }
            let reference: WorkflowOutputRef = response
                .headers()
                .get(PAYLOAD_HEADER)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| serde_json::from_str(value).ok())
                .ok_or_else(|| {
                    WorkflowServiceError::Unavailable("missing workflow payload descriptor".into())
                })?;
            if expected.is_some_and(|expected| expected != &reference) {
                return Err(WorkflowServiceError::Unavailable(
                    "workflow payload descriptor changed".into(),
                ));
            }
            PayloadRead::checked(reference, Box::new(ResponseBody { response, deadline }))
        })
        .await
        .map_err(|_| WorkflowServiceError::Timeout)?
    }
}

impl RemoteTasks {
    pub async fn stage_payload(
        &self,
        task_id: &str,
        token: &TaskToken,
        request: &RequestId,
        reference: WorkflowOutputRef,
        mut body: BoxChunkSource,
    ) -> Result<StagedPayload, WorkflowServiceError> {
        crate::service::payloads::validate_reference(&reference)?;
        let client = CLIENT.with(Clone::clone);
        let descriptor = serde_json::to_string(&reference).map_err(|_| invalid_request())?;
        let builder = client
            .post(format!(
                "{}/v1/tasks/{}/payloads",
                self.endpoint.base,
                segment(task_id)
            ))
            .map_err(|_| invalid_request())?
            .header("authorization", self.authorization()?)
            .map_err(|_| WorkflowServiceError::Unauthenticated)?
            .header(TASK_TOKEN_HEADER, token.as_str())
            .map_err(|_| WorkflowServiceError::Unauthenticated)?
            .header(REQUEST_ID_HEADER, request.as_str())
            .map_err(|_| invalid_request())?
            .header(PAYLOAD_HEADER, descriptor)
            .map_err(|_| invalid_request())?;
        let (mut sender, receiver) = mpsc::channel::<Bytes>(1);
        let source_error = Rc::new(RefCell::new(None));
        let producer_error = source_error.clone();
        let producer = compio::runtime::spawn(async move {
            while let Some(chunk) = body.next_chunk().await {
                match chunk {
                    Ok(chunk) => {
                        if sender.send(chunk).await.is_err() {
                            break;
                        }
                    }
                    Err(_) => {
                        *producer_error.borrow_mut() = Some(WorkflowServiceError::Unavailable(
                            "workflow upload source interrupted".into(),
                        ));
                        break;
                    }
                }
            }
        });
        let result = compio::time::timeout(self.endpoint.timeout, async {
            let response = builder
                .body(cyper::Body::stream(receiver.map(Ok)))
                .send()
                .await
                .map_err(|_| {
                    WorkflowServiceError::Unavailable("workflow upload interrupted".into())
                })?;
            self.endpoint
                .decode_response::<StagedPayload>(response)
                .await
        })
        .await
        .map_err(|_| WorkflowServiceError::Timeout)?;
        // An accepted retry may return its receipt without consuming this body.
        // Dropping the producer cancels that source and bounds its queue.
        drop(producer);
        if result.is_err() {
            if let Some(error) = source_error.borrow_mut().take() {
                return Err(error);
            }
        }
        let staged = result?;
        if staged.reference != reference {
            return Err(WorkflowServiceError::Unavailable(
                "workflow upload receipt changed".into(),
            ));
        }
        Ok(staged)
    }

    pub async fn read_payload(
        &self,
        task_id: &str,
        token: &TaskToken,
        reference: &WorkflowOutputRef,
    ) -> Result<PayloadRead, WorkflowServiceError> {
        self.endpoint
            .download(
                &format!("/v1/tasks/{}/payloads/read", segment(task_id)),
                &self.authorization()?,
                ReadTaskPayload {
                    token: token.clone(),
                    reference: reference.clone(),
                },
                Some(reference),
            )
            .await
    }
}

impl RemoteAppWorkflows {
    pub async fn read_payload(
        &self,
        run_id: &str,
        generation: i64,
        slot: PayloadSlot,
    ) -> Result<PayloadRead, WorkflowServiceError> {
        self.endpoint
            .download(
                &self.path(&format!("workflow-runs/{}/payloads/read", segment(run_id))),
                &self.authorization(),
                ReadAppPayload { generation, slot },
                None,
            )
            .await
    }
}

fn invalid_request() -> WorkflowServiceError {
    WorkflowServiceError::InvalidRequest("invalid workflow payload request".into())
}

struct ResponseBody {
    response: cyper::Response,
    deadline: Instant,
}
#[async_trait::async_trait(?Send)]
impl ChunkSource for ResponseBody {
    async fn next_chunk(&mut self) -> Option<ChunkResult> {
        let remaining = self.deadline.saturating_duration_since(Instant::now());
        match compio::time::timeout(remaining, self.response.next()).await {
            Ok(Some(Ok(chunk))) => Some(Ok(chunk)),
            Ok(None) => None,
            Ok(Some(Err(_))) => Some(Err(StorageError::Stream(
                "workflow download interrupted".into(),
            ))),
            Err(_) => Some(Err(StorageError::Stream(
                "workflow download timed out".into(),
            ))),
        }
    }
}
