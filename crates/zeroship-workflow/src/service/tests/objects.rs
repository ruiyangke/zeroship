//! An in-memory payload store for admission contracts.
//!
//! Admission records which payload a run owns and orchestrates the object
//! effects; it moves no bytes and holds no object store, so its contracts
//! supply the effects themselves. Byte transport, digest verification and the
//! authority guard on a returned body belong to `zeroship-workflow-runner` and
//! are exercised there.
//!
//! This store holds the writer's contract: it refuses a body that does not
//! match its descriptor, exactly as the object writer does.

use crate::{
    engine::WorkflowOutputRef,
    service::{PayloadDeleter, PayloadOpener, PayloadTarget, PayloadWriter},
    WorkflowServiceError,
};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use zeroship_core::app_id::AppId;

/// An injected deletion outcome, consumed by the first matching deletion.
#[derive(Debug)]
pub(in crate::service) enum Fault {
    /// The deletion refuses and the object survives.
    Fail(String),
    /// The object is deleted and the reply is lost.
    LostDeleteReply(String),
    /// The deletion never answers.
    Hang(String),
    /// The deletion announces itself, then waits to be released.
    Gate(String, flume::Sender<()>, flume::Receiver<()>),
}

#[derive(Debug, Default)]
struct State {
    stored: BTreeMap<(String, String), Vec<u8>>,
    deletes: Vec<String>,
    faults: Vec<Fault>,
}

/// Clones share one store, so a reopened service sees the same objects.
#[derive(Debug, Default, Clone)]
pub(in crate::service) struct Objects(Arc<Mutex<State>>);

impl Objects {
    pub fn new() -> Self {
        Self::default()
    }

    /// Write `body` for the payload admission is about to stage.
    pub fn upload(&self, body: &[u8]) -> Upload<'_> {
        Upload {
            objects: self,
            body: body.to_vec(),
        }
    }

    /// Read the object admission has proven this caller owns.
    pub const fn open(&self) -> Open<'_> {
        Open(self)
    }

    /// Put an object no admission record accounts for, as an upload dispatched
    /// by a dead writer does when it arrives after its record expired.
    pub fn put(&self, app: &AppId, id: &str, body: &[u8]) {
        self.0
            .lock()
            .unwrap()
            .stored
            .insert((app.as_str().to_owned(), id.to_owned()), body.to_vec());
    }

    pub fn get(&self, app: &AppId, id: &str) -> Option<Vec<u8>> {
        self.0
            .lock()
            .unwrap()
            .stored
            .get(&(app.as_str().to_owned(), id.to_owned()))
            .cloned()
    }

    pub fn exists(&self, app: &AppId, id: &str) -> bool {
        self.get(app, id).is_some()
    }

    /// Every object this app holds, by the payload id it is keyed under.
    pub fn stored_for(&self, app: &AppId) -> Vec<(String, Vec<u8>)> {
        self.0
            .lock()
            .unwrap()
            .stored
            .iter()
            .filter(|((owner, _), _)| owner == app.as_str())
            .map(|((_, id), body)| (id.clone(), body.clone()))
            .collect()
    }

    /// Every payload a deletion was attempted for, in order.
    pub fn deletes(&self) -> Vec<String> {
        self.0.lock().unwrap().deletes.clone()
    }

    pub fn fail(&self, id: &str) {
        self.0.lock().unwrap().faults.push(Fault::Fail(id.into()));
    }

    pub fn lose_delete_reply(&self, id: &str) {
        self.0
            .lock()
            .unwrap()
            .faults
            .push(Fault::LostDeleteReply(id.into()));
    }

    pub fn hang(&self, id: &str) {
        self.0.lock().unwrap().faults.push(Fault::Hang(id.into()));
    }

    /// Announce the next deletion of `id` and hold it until released.
    pub fn gate(&self, id: &str) -> (flume::Receiver<()>, flume::Sender<()>) {
        let (entered, waiting) = flume::bounded(1);
        let (resume, gate) = flume::bounded(1);
        self.0
            .lock()
            .unwrap()
            .faults
            .push(Fault::Gate(id.into(), entered, gate));
        (waiting, resume)
    }

    fn take_fault(&self, id: &str) -> Option<Fault> {
        let mut state = self.0.lock().unwrap();
        let found = state.faults.iter().position(|fault| {
            let (Fault::Fail(name)
            | Fault::LostDeleteReply(name)
            | Fault::Hang(name)
            | Fault::Gate(name, _, _)) = fault;
            name == id
        })?;
        Some(state.faults.remove(found))
    }
}

/// The effect `stage_payload` runs while it holds the upload claim.
#[derive(Debug)]
pub(in crate::service) struct Upload<'a> {
    objects: &'a Objects,
    body: Vec<u8>,
}
#[async_trait::async_trait(?Send)]
impl PayloadWriter for Upload<'_> {
    async fn write(
        self,
        target: PayloadTarget<'_>,
        budget: Duration,
    ) -> Result<(), WorkflowServiceError> {
        if Instant::now().checked_add(budget).is_none() {
            return Err(WorkflowServiceError::Timeout);
        }
        if !matches(&self.body, target.reference) {
            return Err(WorkflowServiceError::InvalidRequest(
                "workflow payload did not match its descriptor".into(),
            ));
        }
        self.objects.put(target.app, target.id, &self.body);
        Ok(())
    }
}

/// The effect the payload reads run while admission holds the run lock.
#[derive(Debug, Clone, Copy)]
pub(in crate::service) struct Open<'a>(&'a Objects);
#[async_trait::async_trait(?Send)]
impl PayloadOpener for Open<'_> {
    type Read = Vec<u8>;
    async fn open(self, target: PayloadTarget<'_>) -> Result<Vec<u8>, WorkflowServiceError> {
        let body = self.0.get(target.app, target.id).ok_or_else(|| {
            WorkflowServiceError::Unavailable("committed workflow payload is missing".into())
        })?;
        if !matches(&body, target.reference) {
            return Err(WorkflowServiceError::Unavailable(
                "committed workflow payload size changed".into(),
            ));
        }
        Ok(body)
    }
}

#[async_trait::async_trait(?Send)]
impl PayloadDeleter for Objects {
    async fn delete(&self, app: &AppId, id: &str) -> Result<(), WorkflowServiceError> {
        self.0.lock().unwrap().deletes.push(id.to_owned());
        let key = (app.as_str().to_owned(), id.to_owned());
        match self.take_fault(id) {
            Some(Fault::Fail(_)) => {
                return Err(WorkflowServiceError::InvalidRequest(
                    "injected delete failure".into(),
                ))
            }
            Some(Fault::LostDeleteReply(_)) => {
                self.0.lock().unwrap().stored.remove(&key);
                return Err(WorkflowServiceError::InvalidRequest(
                    "injected lost delete reply".into(),
                ));
            }
            Some(Fault::Hang(_)) => std::future::pending::<()>().await,
            Some(Fault::Gate(_, entered, resume)) => {
                entered.send_async(()).await.unwrap();
                resume.recv_async().await.unwrap();
            }
            None => {}
        }
        self.0.lock().unwrap().stored.remove(&key);
        Ok(())
    }
}

/// The creator seam's byte read over this store, standing in for the object
/// reader the workflow host supplies in production.
#[derive(Debug, Clone)]
pub(in crate::service) struct StepOutputs {
    objects: Objects,
    limit: usize,
}
impl StepOutputs {
    pub fn shared(objects: &Objects, limit: usize) -> crate::SharedStepOutputs {
        Arc::new(Self {
            objects: objects.clone(),
            limit,
        })
    }
}
#[async_trait::async_trait(?Send)]
impl crate::StepOutputReader for StepOutputs {
    async fn read(
        &self,
        api: &crate::service::AppWorkflows,
        run_id: &str,
        name: &str,
        occurrence: u32,
    ) -> Result<Vec<u8>, WorkflowServiceError> {
        let bytes = match api
            .read_step_output(run_id, name, occurrence, self.objects.open())
            .await?
        {
            crate::service::StepOutput::Object(bytes) => bytes,
            crate::service::StepOutput::Inline(value) => serde_json::to_vec(&value)
                .map_err(|_| WorkflowServiceError::Internal("invalid step output".into()))?,
        };
        if bytes.len() > self.limit {
            return Err(WorkflowServiceError::PayloadTooLarge);
        }
        Ok(bytes)
    }

    async fn read_output(
        &self,
        api: &crate::service::AppWorkflows,
        run_id: &str,
    ) -> Result<Vec<u8>, WorkflowServiceError> {
        let bytes = api.read_output(run_id, self.objects.open()).await?;
        if bytes.len() > self.limit {
            return Err(WorkflowServiceError::PayloadTooLarge);
        }
        Ok(bytes)
    }
}

/// The host's start-input staging over this store, standing in for the object
/// writer the workflow host supplies in production.
#[async_trait::async_trait(?Send)]
impl crate::InputStager for Objects {
    async fn stage_input(
        &self,
        api: &crate::service::AppWorkflows,
        request: &crate::service::RequestId,
        input: &serde_json::Value,
    ) -> Result<WorkflowOutputRef, WorkflowServiceError> {
        let bytes = serde_json::to_vec(input).map_err(|_| {
            WorkflowServiceError::InvalidRequest("invalid workflow run input".into())
        })?;
        let reference = WorkflowOutputRef {
            hash: format!("{:x}", Sha256::digest(&bytes)),
            size: i64::try_from(bytes.len()).map_err(|_| WorkflowServiceError::PayloadTooLarge)?,
            content_type: Some("application/json".into()),
        };
        api.stage_input(request, reference.clone(), self.upload(&bytes))
            .await?;
        Ok(reference)
    }
}

impl Objects {
    /// This store as the seam a backend stages start inputs through.
    pub fn stager(&self) -> crate::SharedInputStager {
        Arc::new(self.clone())
    }

    /// The object `input` becomes, ready to be named by a start.
    ///
    /// A host stages a caller's start value before the journal ever sees it, so
    /// a contract that starts a run from a value stages it the same way. The
    /// object is ownerless until the generation that names it takes its edge,
    /// so this runs outside any transaction the contract holds.
    pub async fn start_input(
        &self,
        api: &crate::service::AppWorkflows,
        input: serde_json::Value,
    ) -> Option<WorkflowOutputRef> {
        crate::service::payloads::stage_start_input(
            self,
            api,
            &crate::service::RequestId::mint(),
            &input,
        )
        .await
        .unwrap()
    }
}

fn matches(body: &[u8], reference: &WorkflowOutputRef) -> bool {
    i64::try_from(body.len()).is_ok_and(|size| size == reference.size)
        && format!("{:x}", Sha256::digest(body)) == reference.hash
}
