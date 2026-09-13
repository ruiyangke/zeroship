//! Application startup, driven by the runtime's existing event pump.

use std::sync::{Arc, atomic::Ordering};
use std::time::Instant;

use super::{
    PendingReply, RuntimeInner, pass_through_on_exception_noop_callback, wait_until_noop_callback,
};
use crate::core::invocation::{InvocationContext, with_context_preserving_ambient};
use crate::core::modules::{evaluate_module, promise_ready};
use crate::core::startup::{EvaluationPhase, StartupState};
use crate::modules::ModuleEntry;

impl RuntimeInner {
    pub(super) fn initialize_modules(
        &mut self,
        modules: &[ModuleEntry],
        env: &crate::EnvSnapshot,
    ) -> Result<bool, String> {
        if self.host_interrupt.load(Ordering::Acquire) {
            self.fail_startup("runtime execution interrupted".into());
        }
        if matches!(self.startup, StartupState::Uninitialized) {
            self.state.borrow_mut().set_env_snapshot(env);
            self.register_startup_cpu_timer();
            let started = Instant::now();
            let prepared = {
                v8::scope!(let handle_scope, &mut self.isolate);
                let context = v8::Local::new(handle_scope, &self.context);
                let scope = &mut v8::ContextScope::new(handle_scope, context);
                let env_json = self.state.borrow().env_json.clone();
                let env = crate::plugin::build_env_object(scope, &self.plugins, &env_json);
                self.state.borrow_mut().env_obj = Some(env);

                let ctx = v8::Object::new(scope);
                let key = v8::String::new(scope, "waitUntil").unwrap();
                let function = v8::Function::new(scope, wait_until_noop_callback).unwrap();
                ctx.set(scope, key.into(), function.into());
                let key = v8::String::new(scope, "passThroughOnException").unwrap();
                let function =
                    v8::Function::new(scope, pass_through_on_exception_noop_callback).unwrap();
                ctx.set(scope, key.into(), function.into());
                ctx.set_integrity_level(scope, v8::IntegrityLevel::Frozen);
                self.state.borrow_mut().ctx_obj = Some(v8::Global::new(scope, ctx));

                with_context_preserving_ambient(scope, &InvocationContext::default(), |scope| {
                    crate::init::prepare_application(scope, modules, &self.plugins)
                })
            };
            if self.check_v8_terminated() {
                self.fail_startup(self.termination_message().into());
            } else {
                self.startup = match prepared {
                    Ok(prepared) => StartupState::Evaluating {
                        started,
                        descriptor: prepared.descriptor,
                        phase: EvaluationPhase::Adapters {
                            entry: prepared.entry,
                            promises: prepared.promises,
                        },
                    },
                    Err(error) => StartupState::Failed(error),
                };
            }
        }
        self.advance_startup();
        self.startup_result()
    }

    pub(super) fn startup_result(&self) -> Result<bool, String> {
        match &self.startup {
            StartupState::Ready => Ok(true),
            StartupState::Failed(error) => Err(format!("module init failed: {error}")),
            _ => Ok(false),
        }
    }

    pub(super) fn startup_deadline(&self) -> Option<Instant> {
        Some(self.startup.started()? + self.wall_timeout?)
    }

    pub(super) fn advance_startup(&mut self) {
        if self.host_interrupt.load(Ordering::Acquire) {
            self.fail_startup("runtime execution interrupted".into());
        }
        for request in std::mem::take(&mut self.waiting_startup_requests) {
            if request.ctx.cancel.is_cancelled() {
                request
                    .reply
                    .send(Err("Request cancelled during startup".into()));
            } else {
                self.waiting_startup_requests.push(request);
            }
        }
        if self
            .startup_deadline()
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            self.fail_startup("startup wall timeout".into());
        }
        self.poll_startup_evaluation();
        if matches!(self.startup, StartupState::Ready | StartupState::Failed(_)) {
            for waker in self.startup_waiters.drain(..) {
                waker.wake();
            }
            self.release_startup_requests();
        }
    }

    fn poll_startup_evaluation(&mut self) {
        let StartupState::Evaluating { started, phase, .. } = &mut self.startup else {
            return;
        };
        let started = *started;
        let phase = std::mem::replace(phase, EvaluationPhase::Running);
        self.arm_cpu_timer();
        let advanced = {
            v8::scope!(let handle_scope, &mut self.isolate);
            let context = v8::Local::new(handle_scope, &self.context);
            let scope = &mut v8::ContextScope::new(handle_scope, context);
            with_context_preserving_ambient(scope, &InvocationContext::default(), |scope| {
                crate::core::init::perform_microtask_checkpoint(scope);
                let evaluation = match phase {
                    EvaluationPhase::Adapters { entry, promises } => {
                        let mut ready = true;
                        for promise in &promises {
                            ready &= promise_ready(scope, promise)?;
                        }
                        if !ready {
                            return Ok((EvaluationPhase::Adapters { entry, promises }, false));
                        }
                        let evaluation = evaluate_module(scope, &entry)?;
                        crate::core::init::perform_microtask_checkpoint(scope);
                        evaluation
                    }
                    EvaluationPhase::Creator(evaluation) => evaluation,
                    EvaluationPhase::Running => {
                        return Err("startup evaluation is already running".into());
                    }
                };
                let ready = evaluation.is_ready(scope)?;
                Ok((EvaluationPhase::Creator(evaluation), ready))
            })
        };
        self.disarm_cpu_timer();
        if self.check_v8_terminated() {
            self.fail_startup(self.termination_message().into());
            return;
        }
        if self
            .wall_timeout
            .is_some_and(|limit| started.elapsed() >= limit)
        {
            self.fail_startup("startup wall timeout".into());
            return;
        }
        match advanced {
            Err(error) => self.fail_startup(error),
            Ok((EvaluationPhase::Creator(evaluation), true)) => {
                let StartupState::Evaluating { descriptor, .. } =
                    std::mem::replace(&mut self.startup, StartupState::Finalizing)
                else {
                    unreachable!("only an evaluating startup can finalize");
                };
                self.arm_cpu_timer();
                let result = self.publish_application(&evaluation.namespace, descriptor.as_ref());
                self.disarm_cpu_timer();
                if self.check_v8_terminated() {
                    self.fail_startup(self.termination_message().into());
                } else if self
                    .wall_timeout
                    .is_some_and(|limit| started.elapsed() >= limit)
                {
                    self.fail_startup("startup wall timeout".into());
                } else {
                    self.startup = match result {
                        Ok(()) => StartupState::Ready,
                        Err(error) => StartupState::Failed(error),
                    };
                }
            }
            Ok((phase, _)) => {
                if let StartupState::Evaluating { phase: waiting, .. } = &mut self.startup {
                    *waiting = phase;
                }
            }
        }
    }

    pub(super) fn fail_startup(&mut self, error: String) {
        self.startup = StartupState::Failed(error);
        self.fetch_handler_fn = None;
        self.fetch_fast_fn = None;
        self.rpc_fn = None;
        self.workflow_fn = None;
    }

    fn publish_application(
        &mut self,
        namespace: &v8::Global<v8::Value>,
        descriptor: Option<&serde_json::Value>,
    ) -> Result<(), String> {
        v8::scope!(let handle_scope, &mut self.isolate);
        let context = v8::Local::new(handle_scope, &self.context);
        let scope = &mut v8::ContextScope::new(handle_scope, context);
        let entries =
            with_context_preserving_ambient(scope, &InvocationContext::default(), |scope| {
                for plugin in &self.plugins {
                    let namespace =
                        crate::plugin::runtime_plugin_namespace(scope, plugin.namespace())?;
                    plugin.finalize_runtime(scope, namespace, descriptor)?;
                }
                v8::tc_scope!(let tc, scope);
                let ns = v8::Local::new(tc, namespace)
                    .to_object(tc)
                    .ok_or("invalid module namespace")?;
                let key = v8::String::new(tc, "default").unwrap();
                let default = ns
                    .get(tc, key.into())
                    .ok_or("could not read module default export")?;
                let mut entries = Vec::new();
                for name in ["fetch", "fetchFast", "rpc", "workflow"] {
                    let entry = if default.is_null_or_undefined() {
                        None
                    } else {
                        let object = default
                            .to_object(tc)
                            .ok_or("invalid module default export")?;
                        let key = v8::String::new(tc, name).unwrap();
                        let value = object.get(tc, key.into()).ok_or_else(|| {
                            tc.exception().map_or_else(
                                || format!("could not read default.{name}"),
                                |error| crate::core::modules::error_detail(tc, error),
                            )
                        })?;
                        v8::Local::<v8::Function>::try_from(value)
                            .ok()
                            .map(|function| v8::Global::new(tc, function))
                    };
                    entries.push(entry);
                }
                Ok::<_, String>(entries)
            })?;
        let mut entries = entries.into_iter();
        self.fetch_handler_fn = entries.next().unwrap();
        self.fetch_fast_fn = entries.next().unwrap();
        self.rpc_fn = entries.next().unwrap();
        self.workflow_fn = entries.next().unwrap();
        Ok(())
    }

    fn release_startup_requests(&mut self) {
        for request in std::mem::take(&mut self.waiting_startup_requests) {
            if request.ctx.cancel.is_cancelled() {
                request
                    .reply
                    .send(Err("Request cancelled during startup".into()));
                continue;
            }
            let request_id = self.next_direct_request_id;
            let outcome = self.call_fetch_handler(
                &[],
                &request.method,
                &request.url,
                &request.headers,
                &request.body,
                &request.env,
                request.ctx,
                request.user_json,
            );
            let settled = match outcome {
                crate::FetchOutcome::Response {
                    status,
                    headers,
                    body,
                    logs,
                } => crate::SettledFetch::Response {
                    status,
                    headers,
                    body,
                    logs,
                },
                crate::FetchOutcome::Stream {
                    status,
                    headers,
                    body_reader,
                    logs,
                } => crate::SettledFetch::Stream {
                    status,
                    headers,
                    body_reader,
                    logs,
                },
                crate::FetchOutcome::WebSocketUpgrade { ws_id, headers } => {
                    crate::SettledFetch::WebSocketUpgrade {
                        ws_id,
                        headers,
                        logs: vec![],
                    }
                }
                crate::FetchOutcome::Pending { .. } => {
                    // Dispatch owns the pending promise. Deliver its eventual
                    // result directly to the original startup waiter.
                    self.pending_requests
                        .get_mut(&request_id)
                        .expect("pending dispatch must register its request")
                        .reply = PendingReply::Fetch(request.reply);
                    continue;
                }
            };
            request.reply.send(Ok(settled));
        }
    }

    fn register_startup_cpu_timer(&mut self) {
        #[cfg(target_os = "linux")]
        if self.cpu_limit.is_some() && self.cpu_timer.is_none() {
            let handle = self.isolate.thread_safe_handle();
            self.cpu_note
                .store(false, std::sync::atomic::Ordering::Relaxed);
            match crate::cpu_timer::CpuTimer::new(handle, Arc::clone(&self.cpu_note)) {
                Ok(timer) => self.cpu_timer = Some(timer),
                Err(error) => tracing::error!(%error, "cpu-timer initialisation failed"),
            }
        }
    }
}
