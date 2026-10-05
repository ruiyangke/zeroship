//! Host-configured development entry loading and generation ownership.

use std::cell::Cell;
use std::rc::Rc;
use std::time::Instant;

use super::application_entry::{ApplicationEntry, read_field};

#[derive(Default)]
struct Invalidation(Cell<u64>);

enum LoadState {
    Empty,
    Loading {
        generation: u64,
        started: Instant,
        value: v8::Global<v8::Value>,
    },
    Ready(u64),
    Failed(u64, String),
    /// A load given up on before it settled: its wall deadline passed, or V8
    /// terminated it. It may still be running, and loads are serialized, so no
    /// later load may start in this isolate. Every generation answers with the
    /// error until the host replaces the runtime.
    Abandoned {
        generation: u64,
        error: String,
    },
}

pub(crate) struct DevEntryLoader {
    function: v8::Global<v8::Function>,
    invalidation: Rc<Invalidation>,
    state: LoadState,
}

impl DevEntryLoader {
    /// Rust selects this export and passes the callback directly to it. The
    /// callback is never published as a creator-visible global.
    pub fn create(
        scope: &mut v8::PinScope,
        namespace: &v8::Global<v8::Value>,
        export: &str,
    ) -> Result<Self, String> {
        let namespace = v8::Local::new(scope, namespace)
            .to_object(scope)
            .ok_or("invalid dev host module namespace")?;
        let factory = read_field(scope, namespace, export)?;
        let factory = v8::Local::<v8::Function>::try_from(factory)
            .map_err(|_| format!("dev entry loader factory {export:?} must be a function"))?;
        let invalidation = Rc::new(Invalidation::default());
        scope.set_slot(invalidation.clone());
        let invalidate = crate::callback::function(scope, invalidate_callback)
            .ok_or("could not create dev invalidation callback")?;
        let function = call(scope, factory, &[invalidate.into()])?;
        let function = v8::Local::<v8::Function>::try_from(function)
            .map_err(|_| "dev entry loader factory must return a function")?;
        Ok(Self {
            function: v8::Global::new(scope, function),
            invalidation,
            state: LoadState::Empty,
        })
    }

    pub fn result(&self) -> Result<bool, String> {
        let generation = self.invalidation.0.get();
        match &self.state {
            LoadState::Ready(loaded) if *loaded == generation => Ok(true),
            LoadState::Failed(loaded, error) if *loaded == generation => Err(error.clone()),
            LoadState::Abandoned { error, .. } => Err(error.clone()),
            _ => Ok(false),
        }
    }

    pub fn is_pending(&self) -> bool {
        matches!(self.result(), Ok(false))
    }

    pub fn started(&self) -> Option<Instant> {
        match self.state {
            LoadState::Loading { started, .. } => Some(started),
            _ => None,
        }
    }

    pub fn work_generation(&self) -> u64 {
        match self.state {
            LoadState::Loading { generation, .. } | LoadState::Abandoned { generation, .. } => {
                generation
            }
            _ => self.invalidation.0.get(),
        }
    }

    /// Give up on the load of `generation` without waiting for it to settle.
    /// The caller names the generation it read from `work_generation` before
    /// the load ran: a load that invalidated its own generation leaves no trace
    /// of which one it was.
    pub fn abandon(&mut self, generation: u64, error: String) {
        self.state = LoadState::Abandoned { generation, error };
    }

    /// For an abandoned load, whether the host has reported a source change
    /// since that load began.
    pub fn superseded_since_abandoned(&self) -> Option<bool> {
        match self.state {
            LoadState::Abandoned { generation, .. } => Some(self.invalidation.0.get() > generation),
            _ => None,
        }
    }

    pub fn poll_initial(
        &mut self,
        scope: &mut v8::PinScope,
    ) -> Result<Option<Rc<ApplicationEntry>>, String> {
        if self.invalidation.0.get() != 0 {
            return Err("dev entry changed during startup; a fresh runtime is required".into());
        }
        let result = self.poll(scope);
        if self.invalidation.0.get() != 0 {
            return Err("dev entry changed during startup; a fresh runtime is required".into());
        }
        result
    }

    /// Loads are serialized. An obsolete load settles before another load can
    /// touch the module runner cache, and its result is discarded.
    pub fn poll(
        &mut self,
        scope: &mut v8::PinScope,
    ) -> Result<Option<Rc<ApplicationEntry>>, String> {
        if self.result()? {
            return Ok(None);
        }
        if !matches!(self.state, LoadState::Loading { .. }) {
            let generation = self.invalidation.0.get();
            let started = Instant::now();
            let function = v8::Local::new(scope, &self.function);
            let value = match call(scope, function, &[]) {
                Ok(value) => v8::Global::new(scope, value),
                Err(error) => {
                    self.state = LoadState::Failed(generation, error);
                    return self.result().map(|_| None);
                }
            };
            self.state = LoadState::Loading {
                generation,
                started,
                value,
            };
        }

        crate::core::init::perform_microtask_checkpoint(scope);
        let LoadState::Loading {
            generation, value, ..
        } = &self.state
        else {
            unreachable!()
        };
        let generation = *generation;
        let value = v8::Local::new(scope, value);
        let settled = if let Ok(promise) = v8::Local::<v8::Promise>::try_from(value) {
            match promise.state() {
                v8::PromiseState::Pending => return Ok(None),
                v8::PromiseState::Rejected => {
                    let error = promise.result(scope);
                    Err(super::modules::error_detail(scope, error))
                }
                v8::PromiseState::Fulfilled => Ok(promise.result(scope)),
            }
        } else {
            Ok(value)
        };

        if generation != self.invalidation.0.get() {
            self.state = LoadState::Empty;
            notify(scope);
            return Ok(None);
        }

        let entry = settled.and_then(|value| {
            if !value.is_object() || value.is_array() || value.is_function() {
                return Err("dev entry loader must return an entry object".into());
            }
            let object = v8::Local::<v8::Object>::try_from(value).unwrap();
            let receiver = read_field(scope, object, "userDefault")?;
            ApplicationEntry::capture(scope, value, Some(receiver))
        });
        if generation != self.invalidation.0.get() {
            self.state = LoadState::Empty;
            notify(scope);
            return Ok(None);
        }

        match entry {
            Ok(entry) => {
                self.state = LoadState::Ready(generation);
                Ok(Some(Rc::new(entry)))
            }
            Err(error) => {
                self.state = LoadState::Failed(generation, error.clone());
                Err(error)
            }
        }
    }
}

/// Whether the host has reported a source change since this isolate's dev
/// entry loader was created, which is when its first load began.
pub(crate) fn invalidated_since_first_load(isolate: &v8::Isolate) -> bool {
    isolate
        .get_slot::<Rc<Invalidation>>()
        .is_some_and(|invalidation| invalidation.0.get() != 0)
}

fn call<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    function: v8::Local<v8::Function>,
    args: &[v8::Local<v8::Value>],
) -> Result<v8::Local<'s, v8::Value>, String> {
    v8::tc_scope!(let tc, scope);
    let receiver = v8::undefined(tc).into();
    function.call(tc, receiver, args).ok_or_else(|| {
        tc.exception().map_or_else(
            || "dev entry loader failed".into(),
            |error| super::modules::error_detail(tc, error),
        )
    })
}

fn invalidate_callback(
    scope: &mut v8::PinScope,
    _args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue,
) {
    if let Some(invalidation) = scope.get_slot::<Rc<Invalidation>>() {
        invalidation.0.set(
            invalidation
                .0
                .get()
                .checked_add(1)
                .expect("dev generation exhausted"),
        );
        notify(scope);
    }
}

fn notify(scope: &mut v8::PinScope) {
    if let Some(state) = scope.get_slot::<crate::state::SharedState>() {
        state.borrow().notify_pump();
    }
}
