//! A pending RPC advances from shared target loading to procedure completion.
//! The runtime pump owns this state and polls it without JavaScript trampolines.

use crate::core::invocation::{capture_context, with_captured_context};

use super::procedure::{
    FailureStage, Invocation, InvocationError, InvokeFailure, ProcedureRegistry, Resolution,
};

struct Target {
    registry: ProcedureRegistry,
    name: String,
    input: v8::Global<v8::Value>,
    context: v8::Global<v8::Value>,
}

enum Phase {
    Resolving(Target),
    Running(Invocation),
    Complete,
}

pub(crate) enum CallProgress {
    Pending(v8::Global<v8::Promise>),
    Complete {
        invocation: Invocation,
        value: v8::Global<v8::Value>,
    },
    Missing(String),
}

pub(crate) struct RpcCall {
    frame: v8::Global<v8::Value>,
    phase: Phase,
}

impl RpcCall {
    pub(crate) fn new<'s>(
        scope: &mut v8::PinScope<'s, '_>,
        registry: ProcedureRegistry,
        name: String,
        input: v8::Local<'s, v8::Value>,
        context: v8::Local<'s, v8::Value>,
    ) -> Self {
        Self {
            frame: capture_context(scope),
            phase: Phase::Resolving(Target {
                registry,
                name,
                input: v8::Global::new(scope, input),
                context: v8::Global::new(scope, context),
            }),
        }
    }

    pub(crate) fn poll(&mut self, scope: &mut v8::PinScope) -> Result<CallProgress, InvokeFailure> {
        let frame = self.frame.clone();
        with_captured_context(scope, &frame, |scope| {
            loop {
                match std::mem::replace(&mut self.phase, Phase::Complete) {
                    Phase::Resolving(target) => {
                        let resolved =
                            target
                                .registry
                                .resolve(scope, &target.name)
                                .map_err(|error| InvokeFailure {
                                    stage: FailureStage::Handler,
                                    error,
                                    frame: frame.clone(),
                                })?;
                        match resolved {
                            Resolution::Missing => return Ok(CallProgress::Missing(target.name)),
                            Resolution::Pending(promise) => {
                                self.phase = Phase::Resolving(target);
                                return Ok(CallProgress::Pending(promise));
                            }
                            Resolution::Ready(procedure) => {
                                let input = v8::Local::new(scope, target.input);
                                let context = v8::Local::new(scope, target.context);
                                self.phase =
                                    Phase::Running(procedure.invoke(scope, input, context)?);
                            }
                        }
                    }
                    Phase::Running(invocation) => {
                        let promise = v8::Local::new(scope, &invocation.promise);
                        match promise.state() {
                            v8::PromiseState::Pending => {
                                let promise = invocation.promise.clone();
                                self.phase = Phase::Running(invocation);
                                return Ok(CallProgress::Pending(promise));
                            }
                            v8::PromiseState::Fulfilled => {
                                let value = promise.result(scope);
                                let value = v8::Global::new(scope, value);
                                return Ok(CallProgress::Complete { invocation, value });
                            }
                            v8::PromiseState::Rejected => {
                                let error = promise.result(scope);
                                return Err(InvokeFailure {
                                    stage: FailureStage::Handler,
                                    error: InvocationError::JavaScript(v8::Global::new(
                                        scope, error,
                                    )),
                                    frame: invocation.frame,
                                });
                            }
                        }
                    }
                    Phase::Complete => {
                        return Err(InvokeFailure {
                            stage: FailureStage::Handler,
                            error: InvocationError::Engine("RPC call was already completed"),
                            frame: frame.clone(),
                        });
                    }
                }
            }
        })
    }
}
