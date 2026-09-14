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
                    crate::init::prepare_application(
                        scope,
                        modules,
                        &self.plugins,
                        self.dev_entry_factory.is_some(),
                    )
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
            StartupState::Ready => self
                .dev_entry_loader
                .as_ref()
                .map_or(Ok(true), crate::core::dev_entry::DevEntryLoader::result),
            StartupState::Failed(error) => Err(format!("module init failed: {error}")),
            _ => Ok(false),
        }
    }

    pub(super) fn startup_deadline(&self) -> Option<Instant> {
        Some(self.startup.started()? + self.wall_timeout?)
    }

    pub(super) fn dev_entry_deadline(&self) -> Option<Instant> {
        self.dev_entry_loader
            .as_ref()?
            .started()?
            .checked_add(self.wall_timeout?)
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
            } else if matches!(self.startup, StartupState::Ready)
                && self
                    .wall_timeout
                    .is_some_and(|limit| request.started.elapsed() >= limit)
            {
                request
                    .reply
                    .send(Err("Request wall timeout while loading entry".into()));
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
        if matches!(self.startup, StartupState::Ready) {
            self.advance_dev_entry();
        }
        if !matches!(self.startup_result(), Ok(false)) {
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
        let dev_entry_factory = self.dev_entry_factory.clone();
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
                        self.state.borrow_mut().startup_declarations_open = true;
                        let evaluation = evaluate_module(scope, &entry)?;
                        crate::core::init::perform_microtask_checkpoint(scope);
                        evaluation
                    }
                    EvaluationPhase::Creator(evaluation) => evaluation,
                    EvaluationPhase::Dev {
                        mut loader,
                        application,
                    } => {
                        debug_assert!(application.is_none());
                        let application = loader.poll_initial(scope)?;
                        let ready = application.is_some();
                        return Ok((
                            EvaluationPhase::Dev {
                                loader,
                                application,
                            },
                            ready,
                        ));
                    }
                    EvaluationPhase::Running => {
                        return Err("startup evaluation is already running".into());
                    }
                };
                let ready = evaluation.is_ready(scope)?;
                if ready
                    && let Some(export) = dev_entry_factory.as_deref()
                {
                    let mut loader = crate::core::dev_entry::DevEntryLoader::create(
                        scope,
                        &evaluation.namespace,
                        export,
                    )?;
                    let application = loader.poll_initial(scope)?;
                    let ready = application.is_some();
                    return Ok((
                        EvaluationPhase::Dev {
                            loader,
                            application,
                        },
                        ready,
                    ));
                }
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
            Ok((phase, true)) => {
                self.state.borrow_mut().startup_declarations_open = false;
                let StartupState::Evaluating { descriptor, .. } =
                    std::mem::replace(&mut self.startup, StartupState::Finalizing)
                else {
                    unreachable!("only an evaluating startup can finalize");
                };
                self.arm_cpu_timer();
                let mut dev_publication = None;
                let result = match phase {
                    EvaluationPhase::Creator(evaluation) => {
                        self.publish_application(&evaluation.namespace, descriptor.as_ref())
                    }
                    EvaluationPhase::Dev {
                        loader,
                        application: Some(application),
                    } => self.finalize_plugins(descriptor.as_ref()).map(|()| {
                        dev_publication = Some((loader, application));
                    }),
                    _ => unreachable!("only a ready entry can finalize"),
                };
                self.disarm_cpu_timer();
                if self.check_v8_terminated() {
                    self.fail_startup(self.termination_message().into());
                } else if self
                    .wall_timeout
                    .is_some_and(|limit| started.elapsed() >= limit)
                {
                    self.fail_startup("startup wall timeout".into());
                } else {
                    match result {
                        Ok(()) => {
                            if let Some((loader, application)) = dev_publication {
                                self.dev_entry_loader = Some(loader);
                                self.application = Some(application);
                            }
                            self.startup = StartupState::Ready;
                        }
                        Err(error) => self.fail_startup(error),
                    }
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
        self.state.borrow_mut().startup_declarations_open = false;
        self.startup = StartupState::Failed(error);
        self.application = None;
        self.dev_entry_loader = None;
        self.workflow_fn = None;
    }

    fn advance_dev_entry(&mut self) {
        if self
            .dev_entry_loader
            .as_ref()
            .is_none_or(|loader| !loader.is_pending())
        {
            return;
        }
        if self
            .dev_entry_deadline()
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            self.dev_entry_loader.as_mut().unwrap().fail_current(
                "dev entry loading wall timeout; a fresh runtime is required".into(),
            );
            return;
        }

        self.dev_entry_cpu_running = true;
        self.arm_cpu_timer();
        let result = {
            v8::scope!(let handle_scope, &mut self.isolate);
            let context = v8::Local::new(handle_scope, &self.context);
            let scope = &mut v8::ContextScope::new(handle_scope, context);
            with_context_preserving_ambient(scope, &InvocationContext::default(), |scope| {
                self.dev_entry_loader.as_mut().unwrap().poll(scope)
            })
        };
        self.disarm_cpu_timer();
        self.dev_entry_cpu_running = false;
        if self.check_v8_terminated() {
            let error = self.termination_message().to_owned();
            self.dev_entry_loader
                .as_mut()
                .unwrap()
                .fail_current(error);
        } else if let Ok(Some(application)) = result {
            self.application = Some(application);
        }
        // A compile or validation failure remains cached on the loader for
        // this generation. Existing requests retain their captured callables.
    }

    fn finalize_plugins(
        &mut self,
        descriptor: Option<&serde_json::Value>,
    ) -> Result<(), String> {
        v8::scope!(let handle_scope, &mut self.isolate);
        let context = v8::Local::new(handle_scope, &self.context);
        let scope = &mut v8::ContextScope::new(handle_scope, context);
        with_context_preserving_ambient(scope, &InvocationContext::default(), |scope| {
            for plugin in &self.plugins {
                let namespace =
                    crate::plugin::runtime_plugin_namespace(scope, plugin.namespace())?;
                plugin.finalize_runtime(scope, namespace, descriptor)?;
            }
            Ok(())
        })
    }

    fn publish_application(
        &mut self,
        namespace: &v8::Global<v8::Value>,
        descriptor: Option<&serde_json::Value>,
    ) -> Result<(), String> {
        v8::scope!(let handle_scope, &mut self.isolate);
        let context = v8::Local::new(handle_scope, &self.context);
        let scope = &mut v8::ContextScope::new(handle_scope, context);
        let (application, workflow) =
            with_context_preserving_ambient(scope, &InvocationContext::default(), |scope| {
                let ns = v8::Local::new(scope, namespace).to_object(scope)
                    .ok_or("invalid module namespace")?;
                let default = crate::core::application_entry::read_field(scope, ns, "default")?;
                let application = crate::core::application_entry::ApplicationEntry::capture(scope, default, None)?;
                let workflow = if default.is_null_or_undefined() { None } else {
                    let object = default.to_object(scope).ok_or("invalid module default export")?;
                    let value = crate::core::application_entry::read_field(scope, object, "workflow")?;
                    v8::Local::<v8::Function>::try_from(value).ok()
                        .map(|function| v8::Global::new(scope, function))
                };
                for plugin in &self.plugins {
                    let namespace = crate::plugin::runtime_plugin_namespace(scope, plugin.namespace())?;
                    plugin.finalize_runtime(scope, namespace, descriptor)?;
                }
                Ok::<_, String>((application, workflow))
            })?;
        self.application = Some(std::rc::Rc::new(application));
        self.workflow_fn = workflow;
        Ok(())
    }

    fn release_startup_requests(&mut self) {
        if matches!(self.startup_result(), Ok(false)) {
            return;
        }
        for request in std::mem::take(&mut self.waiting_startup_requests) {
            if request.ctx.cancel.is_cancelled() {
                request
                    .reply
                    .send(Err("Request cancelled during startup".into()));
                continue;
            }
            let request_id = self.next_direct_request_id;
            let outcome = self.call_fetch_handler_started(
                &[],
                &request.method,
                &request.url,
                &request.headers,
                &request.body,
                &request.env,
                request.ctx,
                request.user_json,
                request.started,
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
