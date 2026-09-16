//! V8 classes backing `env.workflows`.
//!
//! `env.workflows` is a native object with a named-property interceptor:
//! `env.workflows.Checkout` lazily mints a `WorkflowHandle` for workflow
//! name `Checkout`. That handle exposes `start(opts)` and `get(runId)`;
//! `start()` resolves to a native `WorkflowRun` object with lifecycle
//! methods that call the control-plane instance API.

#![allow(unsafe_code)]

use std::future::Future;
use std::rc::Rc;

use serde_json::{Map, Value};
use zeroship_runtime::state::{OpError, OpResult, ResolveValue, SharedState};
use zeroship_runtime_macros::v8_class;
#[allow(unused_imports)]
use zeroship_runtime_macros::{v8_constructor, v8_getter, v8_method, v8_name};

use zeroship_workflow::backend::SharedWorkflowBackend;
use zeroship_workflow::operations::{RestartOptions, RunOperation, SignalOptions, StartOptions};
use zeroship_workflow::WorkflowServiceError;

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct Workflows {
    pub(crate) backend: SharedWorkflowBackend,
}

#[derive(Debug)]
pub struct WorkflowHandle {
    pub(crate) backend: SharedWorkflowBackend,
    pub(crate) workflow_name: String,
}

#[derive(Debug)]
pub struct WorkflowRun {
    pub(crate) backend: SharedWorkflowBackend,
    pub(crate) run_id: String,
}

#[derive(Clone)]
pub(crate) struct TaskOutputReader {
    pub reader: Rc<zeroship_workflow::service::runner::TaskPayloadReader>,
    pub interrupt: zeroship_runtime::RuntimeInterrupt,
}

// ---------------------------------------------------------------------------
// JS value helpers
// ---------------------------------------------------------------------------

fn js_type_name(v: v8::Local<v8::Value>) -> &'static str {
    if v.is_string() {
        "string"
    } else if v.is_number() {
        "number"
    } else if v.is_boolean() {
        "boolean"
    } else if v.is_function() {
        "function"
    } else if v.is_array() {
        "array"
    } else if v.is_null() {
        "null"
    } else if v.is_undefined() {
        "undefined"
    } else {
        "object"
    }
}

fn value_to_json(
    scope: &mut v8::PinScope<'_, '_>,
    value: v8::Local<v8::Value>,
    context: &str,
) -> Result<Value, OpError> {
    if value.is_undefined() {
        return Ok(Value::Null);
    }
    let Some(json_str) = v8::json::stringify(scope, value) else {
        return Err(OpError::type_error(format!(
            "{context} must be JSON-serializable"
        )));
    };
    let raw = json_str.to_rust_string_lossy(scope);
    serde_json::from_str(&raw).map_err(|e| {
        OpError::type_error(format!(
            "{context} must be valid JSON-serializable data: {e}"
        ))
    })
}

fn read_options_object(
    scope: &mut v8::PinScope<'_, '_>,
    opts: v8::Local<v8::Value>,
    method: &str,
) -> Result<Map<String, Value>, OpError> {
    if opts.is_null_or_undefined() {
        return Ok(Map::new());
    }
    if !opts.is_object() || opts.is_array() {
        return Err(OpError::type_error(format!(
            "{method}: options must be an object, got {}",
            js_type_name(opts)
        )));
    }
    match value_to_json(scope, opts, method)? {
        Value::Object(map) => Ok(map),
        other => Err(OpError::type_error(format!(
            "{method}: options must serialize to an object, got {}",
            json_type_name(&other)
        ))),
    }
}

fn json_type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn start_body(
    scope: &mut v8::PinScope<'_, '_>,
    opts: v8::Local<v8::Value>,
) -> Result<StartOptions, OpError> {
    let mut opts = read_options_object(scope, opts, "workflows.start")?;
    let input = opts.remove("input").unwrap_or(Value::Null);
    let mut body = Map::new();
    body.insert("input".to_string(), input);
    if let Some(key) = opts.remove("key") {
        match key {
            Value::Null => {}
            Value::String(_) => {
                body.insert("key".to_string(), key);
            }
            other => {
                return Err(OpError::type_error(format!(
                    "workflows.start: key must be a string, got {}",
                    json_type_name(&other)
                )));
            }
        }
    }
    if let Some(on_conflict) = opts.remove("onConflict") {
        match on_conflict {
            Value::Null => {}
            Value::String(_) | Value::Object(_) => {
                let policy = match on_conflict {
                    Value::Object(mut map) => map.remove("policy").unwrap_or(Value::Null),
                    value => value,
                };
                body.insert("onConflict".to_string(), policy);
            }
            other => {
                return Err(OpError::type_error(format!(
                    "workflows.start: onConflict must be a string or object, got {}",
                    json_type_name(&other)
                )));
            }
        }
    }
    serde_json::from_value(Value::Object(body))
        .map_err(|error| OpError::type_error(format!("workflows.start: {error}")))
}

fn signal_body(
    scope: &mut v8::PinScope<'_, '_>,
    opts: v8::Local<v8::Value>,
) -> Result<SignalOptions, OpError> {
    let mut opts = read_options_object(scope, opts, "workflow.signal")?;
    let Some(signal_type) = opts.remove("type") else {
        return Err(OpError::type_error(
            "workflow.signal: type must be provided",
        ));
    };
    let signal_type = match signal_type {
        Value::String(s) if !s.is_empty() => s,
        Value::String(_) => {
            return Err(OpError::type_error(
                "workflow.signal: type must be a non-empty string",
            ));
        }
        other => {
            return Err(OpError::type_error(format!(
                "workflow.signal: type must be a string, got {}",
                json_type_name(&other)
            )));
        }
    };
    Ok(SignalOptions {
        signal_type,
        payload: opts.remove("payload").unwrap_or(Value::Null),
    })
}

fn restart_body(
    scope: &mut v8::PinScope<'_, '_>,
    opts: v8::Local<v8::Value>,
) -> Result<RestartOptions, OpError> {
    let mut opts = read_options_object(scope, opts, "workflow.restart")?;
    let mut body = Map::new();
    if let Some(from) = opts.remove("from") {
        match from {
            Value::Null => {}
            Value::Object(map) => {
                let Some(name) = map.get("name") else {
                    return Err(OpError::type_error(
                        "workflow.restart: from.name must be provided",
                    ));
                };
                match name {
                    Value::String(s) if !s.is_empty() => {}
                    Value::String(_) => {
                        return Err(OpError::type_error(
                            "workflow.restart: from.name must be a non-empty string",
                        ));
                    }
                    other => {
                        return Err(OpError::type_error(format!(
                            "workflow.restart: from.name must be a string, got {}",
                            json_type_name(other)
                        )));
                    }
                }
                if let Some(occurrence) = map.get("occurrence") {
                    let ok = occurrence.as_i64().is_some_and(|n| n >= 0);
                    if !ok {
                        return Err(OpError::type_error(
                            "workflow.restart: from.occurrence must be a non-negative integer",
                        ));
                    }
                }
                body.insert("from".to_string(), Value::Object(map));
            }
            other => {
                return Err(OpError::type_error(format!(
                    "workflow.restart: from must be an object, got {}",
                    json_type_name(&other)
                )));
            }
        }
    }
    if let Some(deploy) = opts.remove("deploy") {
        match deploy {
            Value::Null => {}
            Value::String(_) | Value::Object(_) => {
                let pin = match deploy {
                    Value::Object(mut map) => map.remove("pin").unwrap_or(Value::Null),
                    value => value,
                };
                body.insert("deploy".to_string(), pin);
            }
            other => {
                return Err(OpError::type_error(format!(
                    "workflow.restart: deploy must be a string or object, got {}",
                    json_type_name(&other)
                )));
            }
        }
    }
    serde_json::from_value(Value::Object(body))
        .map_err(|error| OpError::type_error(format!("workflow.restart: {error}")))
}

fn runtime_state(scope: &mut v8::PinScope<'_, '_>) -> SharedState {
    scope
        .get_slot::<SharedState>()
        .expect("RuntimeState not in isolate slot")
        .clone()
}

fn setup_promise<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    state: &SharedState,
) -> (
    v8::Global<v8::PromiseResolver>,
    Option<u64>,
    v8::Local<'s, v8::Promise>,
) {
    let resolver = v8::PromiseResolver::new(scope).unwrap();
    let promise = resolver.get_promise(scope);
    let global_resolver = v8::Global::new(scope, resolver);
    let request_id = state.borrow().executing_request_id;
    (global_resolver, request_id, promise)
}

fn dispatch_json<'s, T: serde::Serialize + 'static>(
    scope: &mut v8::PinScope<'s, '_>,
    op: impl Future<Output = Result<T, WorkflowServiceError>> + 'static,
) -> v8::Local<'s, v8::Promise> {
    let state = runtime_state(scope);
    let (resolver, request_id, promise) = setup_promise(scope, &state);
    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        let value = match op.await {
            Ok(value) => match serde_json::to_string(&value) {
                Ok(json) => ResolveValue::Json(json),
                Err(error) => ResolveValue::RejectError(OpError::error(error.to_string())),
            },
            Err(e) => ResolveValue::RejectError(crate::error::to_op_error(e)),
        };
        OpResult::JsValue {
            resolver,
            value,
            request_id,
        }
    }));
    promise
}

fn dispatch_start<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    backend: SharedWorkflowBackend,
    workflow_name: String,
    options: StartOptions,
) -> v8::Local<'s, v8::Promise> {
    dispatch_run_handle(scope, backend.clone(), async move {
        backend
            .start(workflow_name, options)
            .await
            .map(|run| run.id)
    })
}

fn dispatch_restart<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    backend: SharedWorkflowBackend,
    run_id: String,
    options: RestartOptions,
) -> v8::Local<'s, v8::Promise> {
    dispatch_run_handle(scope, backend.clone(), async move {
        backend.restart(run_id, options).await.map(|run| run.run_id)
    })
}

fn dispatch_run_handle<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    backend: SharedWorkflowBackend,
    operation: impl Future<Output = Result<String, WorkflowServiceError>> + 'static,
) -> v8::Local<'s, v8::Promise> {
    let state = runtime_state(scope);
    let (resolver, request_id, promise) = setup_promise(scope, &state);
    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        let value = match operation.await {
            Ok(run_id) => {
                let resolver_for_continuation = resolver.clone();
                ResolveValue::Continuation(Box::new(move |scope, _state| {
                    let local_resolver = v8::Local::new(scope, &resolver_for_continuation);
                    match mint_workflow_run(scope, backend, run_id) {
                        Some(obj) => {
                            local_resolver.resolve(scope, obj.into());
                        }
                        None => {
                            let error = OpError::error("failed to mint WorkflowRun");
                            let exception = error.to_exception(scope);
                            local_resolver.reject(scope, exception);
                        }
                    }
                }))
            }
            Err(error) => ResolveValue::RejectError(crate::error::to_op_error(error)),
        };
        OpResult::JsValue {
            resolver,
            value,
            request_id,
        }
    }));
    promise
}

// ---------------------------------------------------------------------------
// v8_class surfaces
// ---------------------------------------------------------------------------

#[v8_class]
#[allow(dead_code)]
impl Workflows {
    #[v8_constructor]
    fn new() -> Result<Workflows, OpError> {
        Err(OpError::type_error("Illegal constructor"))
    }
}

#[v8_class]
#[allow(dead_code)]
impl WorkflowHandle {
    #[v8_constructor]
    fn new() -> Result<WorkflowHandle, OpError> {
        Err(OpError::type_error("Illegal constructor"))
    }

    /// `env.workflows.Checkout.start({ input, key, onConflict })`.
    #[v8_method]
    fn start<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        opts: v8::Local<v8::Value>,
    ) -> Result<v8::Local<'s, v8::Value>, OpError> {
        let body = start_body(scope, opts)?;
        Ok(dispatch_start(
            scope,
            self.backend.clone(),
            self.workflow_name.clone(),
            body,
        )
        .into())
    }

    /// `env.workflows.Checkout.get(runId)` → WorkflowRun.
    #[v8_method]
    fn get<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        run_id: String,
    ) -> Result<v8::Local<'s, v8::Object>, OpError> {
        if run_id.is_empty() {
            return Err(OpError::type_error(
                "workflow.get: runId must be a non-empty string",
            ));
        }
        mint_workflow_run(scope, self.backend.clone(), run_id)
            .ok_or_else(|| OpError::error("workflow.get: failed to mint WorkflowRun"))
    }
}

#[v8_class]
#[allow(dead_code)]
impl WorkflowRun {
    #[v8_constructor]
    fn new() -> Result<WorkflowRun, OpError> {
        Err(OpError::type_error("Illegal constructor"))
    }

    #[v8_getter]
    fn id(&self) -> String {
        self.run_id.clone()
    }

    #[v8_method]
    #[v8_name = "readStepOutput"]
    fn read_step_output<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        name: String,
        occurrence: f64,
    ) -> Result<v8::Local<'s, v8::Value>, OpError> {
        if name.is_empty()
            || !occurrence.is_finite()
            || occurrence < 0.0
            || occurrence.fract() != 0.0
            || occurrence > f64::from(u32::MAX)
        {
            return Err(OpError::type_error(
                "readStepOutput requires a step name and a nonnegative integer occurrence",
            ));
        }
        let state = runtime_state(scope);
        let (resolver, request_id, promise) = setup_promise(scope, &state);
        let backend = self.backend.clone();
        let run_id = self.run_id.clone();
        let task_reader = scope
            .get_slot::<TaskOutputReader>()
            .filter(|reader| reader.reader.run_id() == run_id)
            .cloned();
        state.borrow_mut().spawned_ops.push(Box::pin(async move {
            let read = if let Some(reader) = task_reader {
                let result = reader.reader.read_step_output(&name, occurrence as u32).await;
                if reader.reader.check().is_err() {
                    // App code cannot catch a lost replay dependency and then
                    // continue producing external effects or journal outcomes.
                    reader.interrupt.cancel();
                }
                result
            } else {
                backend
                    .read_step_output(run_id, name, occurrence as u32)
                    .await
            };
            let value = match read {
                Ok(bytes) => ResolveValue::Bytes(bytes),
                Err(error) => ResolveValue::RejectError(crate::error::to_op_error(error)),
            };
            OpResult::JsValue {
                resolver,
                value,
                request_id,
            }
        }));
        Ok(promise.into())
    }

    /// `run.status()` → Promise<{ state, output, error }>.
    #[v8_method]
    fn status<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
    ) -> Result<v8::Local<'s, v8::Value>, OpError> {
        let backend = self.backend.clone();
        let run_id = self.run_id.clone();
        Ok(dispatch_json(scope, async move { backend.status(run_id).await }).into())
    }

    /// `run.signal({ type, payload })` → Promise<{ id }>.
    #[v8_method]
    fn signal<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        opts: v8::Local<v8::Value>,
    ) -> Result<v8::Local<'s, v8::Value>, OpError> {
        let body = signal_body(scope, opts)?;
        let backend = self.backend.clone();
        let run_id = self.run_id.clone();
        Ok(dispatch_json(scope, async move { backend.signal(run_id, body).await }).into())
    }

    #[v8_method]
    fn pause<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
    ) -> Result<v8::Local<'s, v8::Value>, OpError> {
        let backend = self.backend.clone();
        let run_id = self.run_id.clone();
        Ok(dispatch_json(scope, async move {
            backend.transition(run_id, RunOperation::Pause).await
        })
        .into())
    }

    #[v8_method]
    fn resume<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
    ) -> Result<v8::Local<'s, v8::Value>, OpError> {
        let backend = self.backend.clone();
        let run_id = self.run_id.clone();
        Ok(dispatch_json(scope, async move {
            backend.transition(run_id, RunOperation::Resume).await
        })
        .into())
    }

    #[v8_method]
    fn cancel<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        _opts: v8::Local<v8::Value>,
    ) -> Result<v8::Local<'s, v8::Value>, OpError> {
        let backend = self.backend.clone();
        let run_id = self.run_id.clone();
        Ok(dispatch_json(scope, async move {
            backend.transition(run_id, RunOperation::Cancel).await
        })
        .into())
    }

    #[v8_method]
    fn restart<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        opts: v8::Local<v8::Value>,
    ) -> Result<v8::Local<'s, v8::Value>, OpError> {
        let body = restart_body(scope, opts)?;
        Ok(dispatch_restart(scope, self.backend.clone(), self.run_id.clone(), body).into())
    }
}

// ---------------------------------------------------------------------------
// Named-property getter
// ---------------------------------------------------------------------------

#[must_use]
pub fn is_excluded_workflow_property(name: &str) -> bool {
    matches!(
        name,
        "" | "then"
            | "toJSON"
            | "inspect"
            | "constructor"
            | "prototype"
            | "__proto__"
            | "__defineGetter__"
            | "__defineSetter__"
            | "__lookupGetter__"
            | "__lookupSetter__"
            | "hasOwnProperty"
            | "isPrototypeOf"
            | "propertyIsEnumerable"
            | "toLocaleString"
            | "toString"
            | "valueOf"
    )
}

fn state_from_object<'a, T>(
    scope: &mut v8::PinScope<'_, '_>,
    obj: v8::Local<v8::Object>,
) -> Option<&'a T> {
    let data = obj.get_internal_field(scope, 0)?;
    let ext = v8::Local::<v8::External>::try_from(data).ok()?;
    let ptr = ext.value() as *const T;
    // SAFETY: every mint_* function below stores a Box<T> in internal field
    // 0 and registers a V8 Weak finalizer. The callback runs while the holder
    // object is alive, so this borrowed pointer remains valid for the callback.
    unsafe { ptr.as_ref() }
}

fn workflows_named_getter(
    scope: &mut v8::PinScope,
    key: v8::Local<v8::Name>,
    args: v8::PropertyCallbackArguments,
    mut rv: v8::ReturnValue<v8::Value>,
) -> v8::Intercepted {
    let Ok(key) = v8::Local::<v8::String>::try_from(key) else {
        rv.set(v8::undefined(scope).into());
        return v8::Intercepted::kYes;
    };
    let name = key.to_rust_string_lossy(scope);
    if is_excluded_workflow_property(&name) {
        rv.set(v8::undefined(scope).into());
        return v8::Intercepted::kYes;
    }
    let holder = args.holder();
    let Some(state) = state_from_object::<Workflows>(scope, holder) else {
        return v8::Intercepted::kNo;
    };
    match mint_workflow_handle(scope, state.backend.clone(), name) {
        Some(handle) => {
            rv.set(handle.into());
            v8::Intercepted::kYes
        }
        None => {
            rv.set(v8::undefined(scope).into());
            v8::Intercepted::kYes
        }
    }
}

// ---------------------------------------------------------------------------
// Mint helpers
// ---------------------------------------------------------------------------

fn set_proto<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    obj: v8::Local<'s, v8::Object>,
    class_tmpl: v8::Local<'s, v8::FunctionTemplate>,
) -> Option<()> {
    let class_fn = class_tmpl.get_function(scope)?;
    let proto_key = v8::String::new(scope, "prototype")?;
    let proto_v = class_fn.get(scope, proto_key.into())?;
    obj.set_prototype(scope, proto_v);
    Some(())
}

fn install_state<T: 'static>(
    scope: &mut v8::PinScope<'_, '_>,
    obj: v8::Local<v8::Object>,
    state: T,
) {
    let boxed = Box::new(state);
    let raw = Box::into_raw(boxed);
    let raw_addr = raw as usize;
    let ext = v8::External::new(scope, raw as *mut std::ffi::c_void);
    obj.set_internal_field(0, ext.into());
    let weak = v8::Weak::with_guaranteed_finalizer(
        scope,
        obj,
        Box::new(move || unsafe {
            drop(Box::from_raw(raw_addr as *mut T));
        }),
    );
    std::mem::forget(weak);
}

pub fn mint_workflows<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    backend: SharedWorkflowBackend,
) -> Option<v8::Local<'s, v8::Object>> {
    let class_tmpl = Workflows::install(scope);
    let inst_tmpl = class_tmpl.instance_template(scope);
    inst_tmpl.set_named_property_handler(
        v8::NamedPropertyHandlerConfiguration::new().getter(workflows_named_getter),
    );
    let obj = inst_tmpl.new_instance(scope)?;
    set_proto(scope, obj, class_tmpl)?;
    install_state(scope, obj, Workflows { backend });
    Some(obj)
}

pub fn mint_workflow_handle<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    backend: SharedWorkflowBackend,
    workflow_name: String,
) -> Option<v8::Local<'s, v8::Object>> {
    let class_tmpl = WorkflowHandle::install(scope);
    let obj = class_tmpl.instance_template(scope).new_instance(scope)?;
    set_proto(scope, obj, class_tmpl)?;
    install_state(
        scope,
        obj,
        WorkflowHandle {
            backend,
            workflow_name,
        },
    );
    Some(obj)
}

pub fn mint_workflow_run<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    backend: SharedWorkflowBackend,
    run_id: String,
) -> Option<v8::Local<'s, v8::Object>> {
    let class_tmpl = WorkflowRun::install(scope);
    let obj = class_tmpl.instance_template(scope).new_instance(scope)?;
    set_proto(scope, obj, class_tmpl)?;
    install_state(scope, obj, WorkflowRun { backend, run_id });
    Some(obj)
}
