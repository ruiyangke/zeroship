//! Payload authority bound to a trusted task assignment and its replay journal.

#![allow(
    clippy::future_not_send,
    reason = "task payload I/O runs on its compio host thread"
)]

mod objects;
#[cfg(test)]
mod tests;
pub use objects::{
    AppPayloads, HostPayloads, ObjectStepOutputs, PayloadObjects, PayloadRead, RunPayloads,
    WorkerPayloads,
};
pub(crate) use objects::ObjectWriter;

use crate::{TaskPayloadLimits, WorkerTasks};
use zeroship_workflow::{
    engine::{JournalStep, WorkflowOutputRef},
    service::{delivery::PayloadConfirmation, RequestId, TaskAssignment, TaskToken},
    validation, WorkflowInvocation, WorkflowServiceError,
};
use async_trait::async_trait;
use futures::{StreamExt, TryStreamExt};
use serde::Serialize;
use serde_json::{value::RawValue, Value};
use std::{
    cell::RefCell,
    rc::Rc,
    task::{Poll, Waker},
};
use zeroship_bundle::LoadedWorker;
use zeroship_core::{
    app_id::AppId,
    workflow_policy::MAX_CHILD_OUTPUT_BYTES_CEILING,
};
use zeroship_storage::backend::BoxChunkSource;

/// Host-only payload authority; task credentials never enter the app isolate.
#[async_trait(?Send)]
pub trait TaskPayloads {
    async fn executable(
        &self,
        task: &str,
        token: &TaskToken,
    ) -> Result<LoadedWorker, WorkflowServiceError>;
    async fn stage(
        &self,
        task: &str,
        token: &TaskToken,
        request: &RequestId,
        reference: WorkflowOutputRef,
        body: BoxChunkSource,
    ) -> Result<UploadReceipt, WorkflowServiceError>;
    async fn read(
        &self,
        task: &str,
        token: &TaskToken,
        reference: &WorkflowOutputRef,
    ) -> Result<PayloadRead, WorkflowServiceError>;
}

#[async_trait(?Send)]
impl TaskPayloads for WorkerTasks {
    async fn executable(
        &self,
        task: &str,
        token: &TaskToken,
    ) -> Result<LoadedWorker, WorkflowServiceError> {
        self.service
            .task_executable(&self.worker, task, token)
            .await
    }
    async fn stage(
        &self,
        task: &str,
        token: &TaskToken,
        request: &RequestId,
        reference: WorkflowOutputRef,
        body: BoxChunkSource,
    ) -> Result<UploadReceipt, WorkflowServiceError> {
        let staged = self
            .service
            .payloads(&self.objects)
            .stage(&self.worker, task, token, request, reference, body)
            .await?;
        // This host holds the object store, so staging confirmed under its own
        // lock before returning and there is nothing left for the settlement to
        // do. A host that writes across a boundary answers with a confirmation.
        Ok(UploadReceipt {
            id: staged.id,
            reference: staged.reference,
            confirm: None,
        })
    }
    async fn read(
        &self,
        task: &str,
        token: &TaskToken,
        reference: &WorkflowOutputRef,
    ) -> Result<PayloadRead, WorkflowServiceError> {
        self.service
            .payloads(&self.objects)
            .read_task(&self.worker, task, token, reference)
            .await
    }
}

/// How many child outputs one dispatch reads at once. The replay budget bounds
/// what the outputs add up to; this bounds how many reads are open against the
/// transport together.
///
/// The splice path applies it with `buffer_unordered`. The isolate reads the
/// larger child outputs itself, so the host publishes this bound in the replay
/// envelope and the bridge batches its reads by it: one number, both paths.
pub const CHILD_OUTPUT_READS: usize = 8;

/// A journal step's output as a replay envelope carries it.
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum ReplayOutput {
    /// A value the journal holds inline.
    Inline(Value),
    /// A completed child's output at or under the inline threshold: the bytes
    /// its object holds, spliced into the envelope verbatim, so they are
    /// neither parsed into a tree nor encoded again.
    Stored(Box<RawValue>),
}

/// The refusal a child output that is not the JSON its run returned earns.
///
/// It is a permanent data fault, not a transient one: the bytes in the object
/// store will not become JSON on a later attempt. `Internal` is not retried, so
/// the run rests on the fault instead of spending its delivery budget on it.
fn child_output_error() -> WorkflowServiceError {
    WorkflowServiceError::Internal("workflow child output payload is not valid JSON".into())
}

/// `bytes` as the JSON a child output must be, or [`child_output_error`]. A
/// child's output is the JSON its run returned, so any other bytes are a fault
/// in what holds them rather than a value the parent can be handed.
fn child_output(bytes: Vec<u8>) -> Result<Box<RawValue>, WorkflowServiceError> {
    String::from_utf8(bytes)
        .ok()
        .and_then(|text| RawValue::from_string(text).ok())
        .ok_or_else(child_output_error)
}

/// A [`WorkflowInvocation`] as the replay envelope serializes it: its borrowed
/// identity and hydrated trigger, with the replay journal in place of the
/// assigned one.
///
/// The field order and `camelCase` spelling mirror [`WorkflowInvocation`], and
/// the host adds one field of its own: `child_output_reads`, the bound the
/// bridge batches the referenced child outputs by. The invocation fields are
/// byte-identical to serializing an invocation whose journal is `journal`.
/// Borrowing the invocation keeps the assigned journal from being cloned only
/// to be discarded.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ReplayEnvelope<'a, O> {
    app_id: &'a str,
    deploy_id: &'a str,
    deploy_hash: &'a str,
    run_id: &'a str,
    generation: i64,
    workflow_name: &'a str,
    phase: &'a str,
    trigger: &'a zeroship_workflow::WorkflowTrigger,
    journal: &'a [JournalStep<O>],
    child_output_reads: usize,
}

fn replay_envelope<'a, O>(
    invocation: &'a WorkflowInvocation,
    trigger: &'a zeroship_workflow::WorkflowTrigger,
    journal: &'a [JournalStep<O>],
) -> ReplayEnvelope<'a, O> {
    ReplayEnvelope {
        app_id: &invocation.app_id,
        deploy_id: &invocation.deploy_id,
        deploy_hash: &invocation.deploy_hash,
        run_id: &invocation.run_id,
        generation: invocation.generation,
        workflow_name: &invocation.workflow_name,
        phase: &invocation.phase,
        trigger,
        journal,
        child_output_reads: CHILD_OUTPUT_READS,
    }
}

/// Resolves names only in the assigned journal, including after run restart.
///
/// Customer object reads still require the assignment's live lease. Inline values
/// are already part of the trusted replay envelope and need no further I/O.
pub struct TaskPayloadReader {
    transport: Rc<dyn TaskPayloads>,
    task: String,
    token: TaskToken,
    app: AppId,
    invocation: WorkflowInvocation,
    max_bytes: usize,
    failure: RefCell<Option<WorkflowServiceError>>,
    failure_waker: RefCell<Option<Waker>>,
}
impl std::fmt::Debug for TaskPayloadReader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TaskPayloadReader")
            .field("task", &self.task)
            .finish_non_exhaustive()
    }
}
impl TaskPayloadReader {
    /// Load the immutable executable under the captured task authority.
    ///
    /// # Errors
    /// Rejects expired claims, missing artifacts and integrity failures.
    pub async fn executable(&self) -> Result<LoadedWorker, WorkflowServiceError> {
        self.transport.executable(&self.task, &self.token).await
    }
    /// Capture the replay journal and read budget from a host assignment.
    ///
    /// # Errors
    /// Rejects an empty budget or invalid app identity.
    pub fn new(
        transport: Rc<dyn TaskPayloads>,
        assignment: &TaskAssignment,
        max_bytes: usize,
    ) -> Result<Self, WorkflowServiceError> {
        if max_bytes == 0 {
            return Err(WorkflowServiceError::InvalidRequest(
                "workflow payload read limit must be positive".into(),
            ));
        }
        Ok(Self {
            transport,
            task: assignment.id.clone(),
            token: assignment.token.clone(),
            app: AppId::parse(&assignment.invocation.app_id).map_err(|_| {
                WorkflowServiceError::InvalidRequest(
                    "invalid workflow assignment app identity".into(),
                )
            })?,
            invocation: assignment.invocation.clone(),
            max_bytes,
            failure: RefCell::new(None),
            failure_waker: RefCell::new(None),
        })
    }

    #[must_use]
    pub fn run_id(&self) -> &str {
        &self.invocation.run_id
    }

    #[must_use]
    pub const fn app_id(&self) -> &AppId {
        &self.app
    }

    /// Check whether replay lost access to a required referenced payload.
    ///
    /// # Errors
    /// Once a reference read fails, this reader remains failed. The host must
    /// abandon this execution rather than commit a caught app exception as a
    /// new journal outcome. A fresh task attempt receives a fresh reader.
    pub fn check(&self) -> Result<(), WorkflowServiceError> {
        self.failure.borrow().clone().map_or(Ok(()), Err)
    }

    /// Wait for a failed replay dependency.
    ///
    /// The execution host owns this waiter
    /// and races it against the runtime result; stopping V8 need not settle its
    /// JavaScript promises.
    pub async fn failed(&self) -> WorkflowServiceError {
        std::future::poll_fn(|cx| {
            if let Err(error) = self.check() {
                Poll::Ready(error)
            } else {
                *self.failure_waker.borrow_mut() = Some(cx.waker().clone());
                Poll::Pending
            }
        })
        .await
    }

    /// Hydrate only the input reference captured from this assignment.
    ///
    /// # Errors
    /// Rejects oversized, unavailable, corrupt or non-JSON input payloads.
    pub async fn input(&self) -> Result<Option<Value>, WorkflowServiceError> {
        let Some(reference) = &self.invocation.trigger.input_ref else {
            return Ok(None);
        };
        let bytes = self.bytes(reference).await?;
        serde_json::from_slice(&bytes).map(Some).map_err(|_| {
            WorkflowServiceError::Unavailable("workflow input payload is not valid JSON".into())
        })
    }

    /// The replay envelope for this reader's assignment: its JSON, replaying
    /// the assigned journal with each completed child's output at or under the
    /// inline threshold in place of its descriptor ([`Self::replay_journal`]).
    ///
    /// The identity and trigger come from the assigned invocation the reader
    /// already holds, never from a caller, so a caller cannot name another run
    /// or app. The trigger's referenced input is hydrated into it here, as the
    /// isolate replays it.
    ///
    /// # Errors
    /// Whatever [`Self::input`] or [`Self::replay_journal`] refuses.
    pub async fn replay_envelope(
        &self,
        limits: TaskPayloadLimits,
    ) -> Result<String, WorkflowServiceError> {
        let mut trigger = self.invocation.trigger.clone();
        if let Some(input) = self.input().await? {
            trigger.input = Some(input);
            trigger.input_ref = None;
        }
        self.replay_journal(limits)
            .await?
            .map_or_else(
                || {
                    serde_json::to_string(&replay_envelope(
                        &self.invocation,
                        &trigger,
                        &self.invocation.journal,
                    ))
                },
                |journal| {
                    serde_json::to_string(&replay_envelope(
                        &self.invocation,
                        &trigger,
                        &journal,
                    ))
                },
            )
            .map_err(|_| WorkflowServiceError::Internal("invalid workflow invocation".into()))
    }

    /// The assigned journal as the isolate replays it, when it names a child
    /// output this host splices: each completed child whose output is at or
    /// under `limits.max_inline_bytes` carries the bytes it returned in place
    /// of the descriptor of the object holding them. `None` when it names
    /// none, since the assigned journal is then already what the isolate
    /// replays.
    ///
    /// A larger child output keeps its descriptor and stays out of the
    /// envelope. The isolate reads it through [`Self::read_step_output`], all
    /// of them before the body runs, so `step.call` resolves to either kind of
    /// output in the same microtask round as every other journal value. A read
    /// the body awaited would settle in I/O completion order, so which step the
    /// body issued next would depend on which object arrived first, and a
    /// replay could reach its ordinals in another order than the dispatch that
    /// journaled them. Splicing here and reading the rest before the body keep
    /// replay a function of the journal alone, as [`Self::input`] does for the
    /// trigger, and leave the threshold the host's own choice, since which side
    /// of it an output falls on changes nothing the body observes.
    ///
    /// The spliced bytes are carried verbatim ([`ReplayOutput::Stored`]), so
    /// the envelope grows by exactly the sizes the descriptors name. Their sum
    /// is held to `limits.max_replay_bytes` before anything is read. That is a
    /// boundary assertion rather than a limit a creator meets: the journal
    /// admits child outputs only up to `AppPolicy::max_child_output_bytes`, and
    /// [`TaskPayloadLimits::validate_configured`] holds a configured host's
    /// replay budget at that bound's ceiling, so a journal the service admitted
    /// cannot reach it. The reads run [`CHILD_OUTPUT_READS`] at a time.
    ///
    /// A child that returned nothing reached the journal as JSON null with no
    /// object, and is left as it is. A `run` step's object stays a reference:
    /// the body asked for one, and reads it through [`Self::read_step_output`].
    ///
    /// # Errors
    /// Refuses a spliced total the host cannot represent, and one over
    /// `limits.max_replay_bytes`, as internal faults; and oversized,
    /// unavailable, corrupt or non-JSON child outputs.
    pub(crate) async fn replay_journal(
        &self,
        limits: TaskPayloadLimits,
    ) -> Result<Option<Vec<JournalStep<ReplayOutput>>>, WorkflowServiceError> {
        let spliced = |step: &JournalStep| {
            step.kind == "child"
                && step.output_ref.as_ref().is_some_and(|reference| {
                    usize::try_from(reference.size)
                        .is_ok_and(|size| size <= limits.max_inline_bytes)
                })
        };
        // Every completed child output is read in full, spliced here or read by
        // the isolate through its descriptor, so the platform's child-output
        // ceiling bounds the whole set. Admission enforces it where the journal
        // grows; the host asserts it independently before it reads any of them,
        // so a journal admitted before the ceiling existed, or one that is
        // otherwise corrupt, is refused rather than read in full.
        let child_total = self
            .invocation
            .journal
            .iter()
            .filter(|step| step.kind == "child")
            .filter_map(|step| step.output_ref.as_ref())
            .try_fold(0_usize, |total, reference| {
                total.checked_add(usize::try_from(reference.size).ok()?)
            });
        let Some(child_total) = child_total else {
            return Err(WorkflowServiceError::Internal(
                "workflow child output total is unrepresentable".into(),
            ));
        };
        if child_total > MAX_CHILD_OUTPUT_BYTES_CEILING {
            return Err(WorkflowServiceError::Internal(
                "workflow child outputs exceed the platform ceiling".into(),
            ));
        }
        if !self.invocation.journal.iter().any(spliced) {
            return Ok(None);
        }
        let stored = self
            .invocation
            .journal
            .iter()
            .filter_map(|step| step.output_ref.as_ref().filter(|_| spliced(step)))
            .try_fold(0_usize, |total, reference| {
                total.checked_add(usize::try_from(reference.size).ok()?)
            });
        let Some(stored) = stored else {
            return Err(WorkflowServiceError::Internal(
                "workflow child output total is unrepresentable".into(),
            ));
        };
        if stored > limits.max_replay_bytes {
            return Err(WorkflowServiceError::Internal(
                "workflow child outputs exceed this host's replay budget".into(),
            ));
        }
        let reads = self
            .invocation
            .journal
            .iter()
            .enumerate()
            .filter_map(|(index, step)| {
                let reference = step.output_ref.as_ref().filter(|_| spliced(step))?;
                Some(async move {
                    let raw = child_output(self.bytes(reference).await?)?;
                    Ok::<_, WorkflowServiceError>((index, raw))
                })
            });
        let mut outputs: Vec<(usize, Box<RawValue>)> = futures::stream::iter(reads)
            .buffer_unordered(CHILD_OUTPUT_READS)
            .try_collect()
            .await?;
        outputs.sort_unstable_by_key(|(index, _)| *index);
        let mut outputs = outputs.into_iter().peekable();
        Ok(Some(
            self.invocation
                .journal
                .iter()
                .enumerate()
                .map(|(index, step)| {
                    let mut output_ref = step.output_ref.clone();
                    let output = match outputs.next_if(|(at, _)| *at == index) {
                        Some((_, raw)) => {
                            output_ref = None;
                            Some(ReplayOutput::Stored(raw))
                        }
                        None => step.output.clone().map(ReplayOutput::Inline),
                    };
                    JournalStep {
                        ordinal: step.ordinal,
                        name: step.name.clone(),
                        name_occurrence: step.name_occurrence,
                        kind: step.kind.clone(),
                        state: step.state.clone(),
                        output,
                        output_ref,
                        error: step.error.clone(),
                        child_run_id: step.child_run_id.clone(),
                        compensation_state: step.compensation_state.clone(),
                    }
                })
                .collect(),
        ))
    }

    /// Read a completed occurrence's bytes from the assigned journal.
    ///
    /// A `run` step's by-reference output is arbitrary bytes the body asked
    /// for, so nothing here parses it. A child's output is read as its JSON by
    /// [`Self::read_child_output`] instead.
    ///
    /// # Errors
    /// Rejects invalid selectors, absent outputs, expired task authority and
    /// oversized, unavailable or corrupt payloads.
    pub async fn read_step_output(
        &self,
        name: &str,
        occurrence: u32,
    ) -> Result<Vec<u8>, WorkflowServiceError> {
        let step = self.completed_step(name, occurrence)?;
        if let Some(reference) = &step.output_ref {
            return self.bytes(reference).await;
        }
        let bytes = serde_json::to_vec(&step.output).map_err(|_| {
            WorkflowServiceError::Internal("invalid assigned workflow output".into())
        })?;
        if bytes.len() > self.max_bytes {
            return Err(WorkflowServiceError::PayloadTooLarge);
        }
        Ok(bytes)
    }

    /// Read a completed child occurrence as the JSON its run returned.
    ///
    /// The value is parsed once, here, so the isolate never parses it again.
    /// Bytes that are not the JSON the child returned fail this reader as an
    /// unreadable reference does, so the host abandons the execution instead of
    /// letting the isolate hand the body a parse failure it could catch and
    /// journal past.
    ///
    /// # Errors
    /// Rejects invalid selectors, a step that is not a child, expired task
    /// authority and a child output that is not JSON.
    pub async fn read_child_output(
        &self,
        name: &str,
        occurrence: u32,
    ) -> Result<Value, WorkflowServiceError> {
        let step = self.completed_step(name, occurrence)?;
        if step.kind != "child" {
            let error =
                WorkflowServiceError::InvalidRequest("workflow step output is not a child's".into());
            self.fail(&error);
            return Err(error);
        }
        let Some(reference) = &step.output_ref else {
            return Ok(step.output.clone().unwrap_or(Value::Null));
        };
        let bytes = self.bytes(reference).await?;
        serde_json::from_slice(&bytes).map_err(|_| {
            let error = child_output_error();
            self.fail(&error);
            error
        })
    }

    /// Resolve a completed occurrence from the assigned journal, failing this
    /// reader for any selector it cannot answer.
    ///
    /// A selector the assigned journal does not answer is a lost replay
    /// dependency, the same as an unreadable reference: the host must abandon
    /// the execution rather than let the isolate journal past the failure.
    fn completed_step(
        &self,
        name: &str,
        occurrence: u32,
    ) -> Result<&JournalStep, WorkflowServiceError> {
        if let Err(error) = validation::step_name(name) {
            self.fail(&error);
            return Err(error);
        }
        let occurrence = match i32::try_from(occurrence) {
            Ok(occurrence) => occurrence,
            Err(_) => {
                let error =
                    WorkflowServiceError::InvalidRequest("invalid step name occurrence".into());
                self.fail(&error);
                return Err(error);
            }
        };
        self.invocation
            .journal
            .iter()
            .find(|step| {
                step.name == name && step.name_occurrence == occurrence && step.state == "completed"
            })
            .ok_or_else(|| {
                let error = WorkflowServiceError::NotFound(
                    "workflow step output not found in assigned journal".into(),
                );
                self.fail(&error);
                error
            })
    }

    async fn bytes(&self, reference: &WorkflowOutputRef) -> Result<Vec<u8>, WorkflowServiceError> {
        self.check()?;
        let result = self.read_verified(reference).await;
        if let Err(error) = &result {
            self.fail(error);
        }
        // Another concurrent read may have failed while this one was pending.
        self.check()?;
        result
    }

    /// Hold this reader failed from now on, keeping the first failure, and
    /// wake the host waiting in [`Self::failed`].
    fn fail(&self, error: &WorkflowServiceError) {
        self.failure
            .borrow_mut()
            .get_or_insert_with(|| error.clone());
        let waker = self.failure_waker.borrow_mut().take();
        if let Some(waker) = waker {
            waker.wake();
        }
    }

    async fn read_verified(
        &self,
        reference: &WorkflowOutputRef,
    ) -> Result<Vec<u8>, WorkflowServiceError> {
        // Refuse before asking the transport to open an oversized object.
        if usize::try_from(reference.size)
            .ok()
            .is_none_or(|size| size > self.max_bytes)
        {
            return Err(WorkflowServiceError::PayloadTooLarge);
        }
        let read = self
            .transport
            .read(&self.task, &self.token, reference)
            .await?;
        if read.reference != *reference {
            return Err(WorkflowServiceError::Unavailable(
                "workflow replay payload descriptor changed".into(),
            ));
        }
        read.into_bytes(self.max_bytes).await
    }
}

/// What one upload answered, and whether its settlement still owes a confirm.
///
/// # Why this is not `StagedPayload`
///
/// `StagedPayload` promises the object is written AND the row says `staged`. A
/// host holding the object store can promise both, because it confirms under the
/// same lock it wrote under. A host writing across a request boundary cannot: the
/// reservation and the confirm are separate calls, and the confirm has to land in
/// the transaction that commits the frontier referencing the object -- `promote`
/// resolves no `uploading` row, so an outcome naming an unconfirmed object is
/// refused as a missing payload.
///
/// So `confirm` is the difference between the two hosts, and it is deliberately
/// not an implementation detail of either: the settlement is what carries it, and
/// only the host that uploaded knows whether one is owed.
///
/// `reference` is answered rather than assumed, so the caller can refuse a
/// receipt describing another object -- a check that cannot fire in process,
/// where the descriptor is returned by value from the caller's own argument, and
/// can over a wire where the reply is decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadReceipt {
    pub id: String,
    pub reference: WorkflowOutputRef,
    pub confirm: Option<PayloadConfirmation>,
}
