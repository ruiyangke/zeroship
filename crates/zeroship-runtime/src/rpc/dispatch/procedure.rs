//! Procedure targets retained by an application entry generation.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use crate::core::invocation::{capture_context, with_captured_context};
use crate::rpc::capability::{ProcedureKind, with_procedure_frame};

/// Preserve creator exceptions until the common transport error encoder runs.
#[derive(Clone, Debug)]
pub(crate) enum InvocationError {
    JavaScript(v8::Global<v8::Value>),
    InvalidTarget(String),
    Engine(&'static str),
}

impl InvocationError {
    pub(crate) fn describe(&self, scope: &mut v8::PinScope) -> String {
        match self {
            Self::JavaScript(error) => {
                let error = v8::Local::new(scope, error);
                crate::core::modules::error_detail(scope, error)
            }
            Self::InvalidTarget(message) => message.clone(),
            Self::Engine(message) => (*message).into(),
        }
    }
}

pub(super) fn attempt<'s, T>(
    scope: &mut v8::PinScope<'s, '_>,
    operation: impl FnOnce(&mut v8::PinScope<'s, '_>) -> Option<T>,
) -> Result<T, InvocationError> {
    v8::tc_scope!(let tc, scope);
    let result = operation(tc);
    let result = if tc.has_caught() { None } else { result };
    match result {
        Some(value) => Ok(value),
        None => match tc.exception() {
            Some(error) => Err(InvocationError::JavaScript(v8::Global::new(tc, error))),
            None => Err(InvocationError::Engine(
                "V8 could not complete procedure evaluation",
            )),
        },
    }
}

pub(super) fn property<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    object: v8::Local<'s, v8::Object>,
    name: &str,
) -> Result<v8::Local<'s, v8::Value>, InvocationError> {
    attempt(scope, |scope| {
        let key = v8::String::new(scope, name)?;
        object.get(scope, key.into())
    })
}

#[derive(Clone)]
pub(crate) struct Validator {
    receiver: v8::Global<v8::Object>,
    parse: v8::Global<v8::Function>,
}

impl Validator {
    fn from_value<'s>(
        scope: &mut v8::PinScope<'s, '_>,
        value: v8::Local<'s, v8::Value>,
    ) -> Result<Option<Self>, InvocationError> {
        if !value.is_object() || value.is_function() {
            return Ok(None);
        }
        let object = v8::Local::<v8::Object>::try_from(value).unwrap();
        let parse = property(scope, object, "parse")?;
        let Ok(parse) = v8::Local::<v8::Function>::try_from(parse) else {
            return Ok(None);
        };
        Ok(Some(Self {
            receiver: v8::Global::new(scope, object),
            parse: v8::Global::new(scope, parse),
        }))
    }

    pub(crate) fn parse<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        value: v8::Local<'s, v8::Value>,
    ) -> Result<v8::Local<'s, v8::Value>, InvocationError> {
        attempt(scope, |scope| {
            let parse = v8::Local::new(scope, &self.parse);
            let receiver = v8::Local::new(scope, &self.receiver);
            parse.call(scope, receiver.into(), &[value])
        })
    }
}

#[derive(Clone)]
pub(crate) struct Procedure {
    function: v8::Global<v8::Function>,
    pub(crate) kind: Option<ProcedureKind>,
    input: Option<Validator>,
    pub(crate) output: Option<Validator>,
    pub(crate) output_is_string: bool,
}

impl Procedure {
    pub(crate) fn from_function<'s>(
        scope: &mut v8::PinScope<'s, '_>,
        function: v8::Local<'s, v8::Function>,
    ) -> Result<Self, InvocationError> {
        let mut procedure = Self {
            function: v8::Global::new(scope, function),
            kind: None,
            input: None,
            output: None,
            output_is_string: false,
        };
        let config = property(scope, function.into(), "config")?;
        if config.is_null_or_undefined() {
            return Ok(procedure);
        }
        let config = v8::Local::<v8::Object>::try_from(config).map_err(|_| {
            InvocationError::InvalidTarget("procedure config must be an object".into())
        })?;
        let kind = property(scope, config, "kind")?;
        if !kind.is_undefined() {
            if !kind.is_string() {
                return Err(InvocationError::InvalidTarget(
                    "procedure kind must be a string".into(),
                ));
            }
            let name = kind.to_rust_string_lossy(scope);
            procedure.kind = Some(ProcedureKind::from_wire(&name).ok_or_else(|| {
                InvocationError::InvalidTarget(format!("unknown procedure kind: {name}"))
            })?);
        }
        let input = property(scope, config, "input")?;
        procedure.input = Validator::from_value(scope, input)?;
        let output = property(scope, config, "output")?;
        procedure.output = Validator::from_value(scope, output)?;
        let output_is_string = property(scope, config, "outputIsString")?;
        if !output_is_string.is_undefined() && !output_is_string.is_boolean() {
            return Err(InvocationError::InvalidTarget(
                "outputIsString must be a boolean".into(),
            ));
        }
        procedure.output_is_string = output_is_string.is_true();
        Ok(procedure)
    }

    /// Validation and handler execution share a frame. Capture it before
    /// returning so promise classification and iterator calls can re-enter it.
    pub(crate) fn invoke<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        input: v8::Local<'s, v8::Value>,
        context: v8::Local<'s, v8::Value>,
    ) -> Result<Invocation, InvokeFailure> {
        with_procedure_frame(scope, self.kind, |scope| {
            let frame = capture_context(scope);
            let input = match &self.input {
                Some(validator) => {
                    validator
                        .parse(scope, input)
                        .map_err(|error| InvokeFailure {
                            stage: FailureStage::Input,
                            error,
                            frame: frame.clone(),
                        })?
                }
                None => input,
            };
            let result = attempt(scope, |scope| {
                let function = v8::Local::new(scope, &self.function);
                let receiver = v8::undefined(scope).into();
                let result = function.call(scope, receiver, &[input, context])?;
                let resolver = v8::PromiseResolver::new(scope)?;
                resolver.get_promise(scope).mark_as_handled();
                resolver.resolve(scope, result)?;
                Some(resolver.get_promise(scope))
            })
            .map_err(|error| InvokeFailure {
                stage: FailureStage::Handler,
                error,
                frame: frame.clone(),
            })?;
            Ok(Invocation {
                promise: v8::Global::new(scope, result),
                frame,
                output: self.output.clone(),
                output_is_string: self.output_is_string,
            })
        })
    }

    pub(crate) fn allows_http_method(&self, method: &str) -> bool {
        match self.kind {
            Some(ProcedureKind::Query) => {
                method.eq_ignore_ascii_case("GET")
                    || method.eq_ignore_ascii_case("POST")
                    || method.eq_ignore_ascii_case("HEAD")
            }
            Some(ProcedureKind::Mutation) => method.eq_ignore_ascii_case("POST"),
            Some(ProcedureKind::Subscription) => method.eq_ignore_ascii_case("GET"),
            Some(ProcedureKind::Action | ProcedureKind::Stream) | None => true,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FailureStage {
    Input,
    Handler,
}

#[derive(Debug)]
pub(crate) struct InvokeFailure {
    pub(crate) stage: FailureStage,
    pub(crate) error: InvocationError,
    pub(crate) frame: v8::Global<v8::Value>,
}

pub(crate) struct Invocation {
    pub(crate) promise: v8::Global<v8::Promise>,
    pub(crate) frame: v8::Global<v8::Value>,
    pub(crate) output: Option<Validator>,
    pub(crate) output_is_string: bool,
}

impl Invocation {
    pub(crate) fn with_frame<'s, T>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        operation: impl FnOnce(&mut v8::PinScope<'s, '_>) -> T,
    ) -> T {
        with_captured_context(scope, &self.frame, operation)
    }
}

#[derive(Clone)]
enum Entry {
    Ready(Procedure),
    Unloaded {
        receiver: v8::Global<v8::Object>,
        load: v8::Global<v8::Function>,
    },
    Loading(v8::Global<v8::Promise>),
    Resolving(v8::Global<v8::Promise>),
    Failed(InvocationError),
}

pub(crate) enum Resolution {
    Ready(Procedure),
    Pending(v8::Global<v8::Promise>),
    Missing,
}

/// The host publishes a new registry for a new entry generation. Clones keep
/// the old targets and loader state alive for requests already using them.
#[derive(Clone)]
pub(crate) struct ProcedureRegistry {
    entries: HashMap<String, Rc<RefCell<Entry>>>,
    frame: v8::Global<v8::Value>,
}

impl ProcedureRegistry {
    pub(crate) fn snapshot<'s>(
        scope: &mut v8::PinScope<'s, '_>,
        dictionary: v8::Local<'s, v8::Value>,
    ) -> Result<Self, InvocationError> {
        if !dictionary.is_object() || dictionary.is_function() || dictionary.is_array() {
            return Err(InvocationError::InvalidTarget(
                "default.rpc must be a procedure dictionary".into(),
            ));
        }
        let dictionary = v8::Local::<v8::Object>::try_from(dictionary).unwrap();
        let names = attempt(scope, |scope| {
            dictionary.get_own_property_names(scope, Default::default())
        })?;
        let mut entries = HashMap::new();
        for index in 0..names.length() {
            let name = attempt(scope, |scope| names.get_index(scope, index))?;
            if name.is_symbol() {
                continue;
            }
            let name = name.to_rust_string_lossy(scope);
            if name.is_empty() {
                return Err(InvocationError::InvalidTarget(
                    "procedure wire id must not be empty".into(),
                ));
            }
            let value = property(scope, dictionary, &name)?;
            let entry = if let Ok(function) = v8::Local::<v8::Function>::try_from(value) {
                Entry::Ready(Procedure::from_function(scope, function)?)
            } else if let Ok(record) = v8::Local::<v8::Object>::try_from(value) {
                let load = property(scope, record, "load")?;
                let load = v8::Local::<v8::Function>::try_from(load).map_err(|_| {
                    InvocationError::InvalidTarget(format!(
                        "procedure {name:?} must be callable or have a load function"
                    ))
                })?;
                Entry::Unloaded {
                    receiver: v8::Global::new(scope, record),
                    load: v8::Global::new(scope, load),
                }
            } else {
                return Err(InvocationError::InvalidTarget(format!(
                    "procedure {name:?} must be callable or a loader record"
                )));
            };
            entries.insert(name, Rc::new(RefCell::new(entry)));
        }
        Ok(Self {
            entries,
            frame: crate::core::invocation::capture_context(scope),
        })
    }

    pub(crate) fn resolve(
        &self,
        scope: &mut v8::PinScope,
        name: &str,
    ) -> Result<Resolution, InvocationError> {
        let Some(slot) = self.entries.get(name) else {
            return Ok(Resolution::Missing);
        };
        let entry = slot.borrow().clone();
        match entry {
            Entry::Ready(procedure) => Ok(Resolution::Ready(procedure)),
            Entry::Failed(error) => Err(error),
            Entry::Resolving(promise) => Ok(Resolution::Pending(promise)),
            Entry::Unloaded { receiver, load } => {
                crate::core::invocation::with_captured_context(scope, &self.frame, |scope| {
                    let resolver = v8::PromiseResolver::new(scope).ok_or(
                        InvocationError::Engine("could not allocate lazy procedure promise"),
                    )?;
                    let promise = resolver.get_promise(scope);
                    promise.mark_as_handled();
                    let retained = v8::Global::new(scope, promise);
                    // Publish pending state before invoking creator code. Re-entry
                    // shares this load and never holds a RefCell borrow into V8.
                    *slot.borrow_mut() = Entry::Loading(retained.clone());
                    let loaded = attempt(scope, |scope| {
                        let load = v8::Local::new(scope, load);
                        let receiver = v8::Local::new(scope, receiver);
                        load.call(scope, receiver.into(), &[])
                    });
                    match loaded {
                        Ok(value) => {
                            resolver.resolve(scope, value);
                        }
                        Err(error) => {
                            *slot.borrow_mut() = Entry::Failed(error.clone());
                            if let InvocationError::JavaScript(reason) = &error {
                                let reason = v8::Local::new(scope, reason);
                                resolver.reject(scope, reason);
                            }
                            return Err(error);
                        }
                    }
                    Ok(Resolution::Pending(retained))
                })
            }
            Entry::Loading(promise) => {
                let local = v8::Local::new(scope, &promise);
                let result = match local.state() {
                    v8::PromiseState::Pending => return Ok(Resolution::Pending(promise)),
                    v8::PromiseState::Rejected => Err(InvocationError::JavaScript(
                        v8::Global::new(scope, local.result(scope)),
                    )),
                    v8::PromiseState::Fulfilled => {
                        *slot.borrow_mut() = Entry::Resolving(promise.clone());
                        let value = local.result(scope);
                        match v8::Local::<v8::Function>::try_from(value) {
                            Ok(function) => crate::core::invocation::with_captured_context(
                                scope,
                                &self.frame,
                                |scope| Procedure::from_function(scope, function),
                            ),
                            Err(_) => Err(InvocationError::InvalidTarget(format!(
                                "loader for {name:?} did not return a procedure function"
                            ))),
                        }
                    }
                };
                *slot.borrow_mut() = match &result {
                    Ok(procedure) => Entry::Ready(procedure.clone()),
                    Err(error) => Entry::Failed(error.clone()),
                };
                result.map(Resolution::Ready)
            }
        }
    }
}

#[cfg(test)]
mod tests;
