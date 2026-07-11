//! The concrete public-`v8`-backed host impl.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Once;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Duration;

use zero_migrate::runtime_host::{
    AuthoringHost, GlobalsProfile, HostEvalError, JsDriverHost, JsDriverTimeout, ModuleEntry,
    NetPolicy, RecorderPlatform,
};

// ---------------------------------------------------------------------------
// Process-global V8 platform init.
// ---------------------------------------------------------------------------

/// Committed platform flavor: 0 = uninit, 1 = multi-threaded, 2 = single-threaded.
static FLAVOR: AtomicU8 = AtomicU8::new(0);
static INIT_MULTI: Once = Once::new();
static INIT_SINGLE: Once = Once::new();

/// Initialise the default (multi-threaded) V8 platform once. Idempotent; safe to
/// call from every isolate-constructing entry.
fn ensure_platform_multi() {
    INIT_MULTI.call_once(|| {
        if FLAVOR.load(Ordering::SeqCst) == 2 {
            // The single-threaded flavor was already committed; do not double-init.
            return;
        }
        let platform = v8::new_default_platform(0, false).make_shared();
        v8::V8::initialize_platform(platform);
        v8::V8::initialize();
        let _ = FLAVOR.compare_exchange(0, 1, Ordering::SeqCst, Ordering::SeqCst);
    });
}

// ---------------------------------------------------------------------------
// The host handle.
// ---------------------------------------------------------------------------

/// The public-`v8`-backed engine host. Zero-sized: all state is per-call isolate
/// state, mirroring the seam's stateless-host contract.
#[derive(Debug, Default, Clone, Copy)]
pub struct ZeroMigrateHost;

impl ZeroMigrateHost {
    /// Construct the host handle.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

// ---------------------------------------------------------------------------
// RecorderPlatform
// ---------------------------------------------------------------------------

impl RecorderPlatform for ZeroMigrateHost {
    fn init_single_threaded(&self) {
        INIT_SINGLE.call_once(|| {
            if FLAVOR.load(Ordering::SeqCst) == 0 {
                let platform =
                    v8::new_single_threaded_default_platform(false).make_shared();
                v8::V8::initialize_platform(platform);
                v8::V8::initialize();
                FLAVOR.store(2, Ordering::SeqCst);
            }
        });
    }

    fn platform_flavor(&self) -> u8 {
        FLAVOR.load(Ordering::SeqCst)
    }
}

// ---------------------------------------------------------------------------
// AuthoringHost — real ESM module-graph eval + globals.
// ---------------------------------------------------------------------------

thread_local! {
    /// The module registry for the in-flight `eval_frontend` call: specifier →
    /// compiled module. The ESM resolve callback keys into this to satisfy imports.
    static MODULES: RefCell<HashMap<String, v8::Global<v8::Module>>> = RefCell::new(HashMap::new());
}

impl AuthoringHost for ZeroMigrateHost {
    fn eval_frontend(
        &self,
        modules: &[ModuleEntry],
        profile: GlobalsProfile,
        inputs: &[(&str, &str)],
        outputs: &[&str],
        heap_limit_mb: Option<u32>,
    ) -> Result<HashMap<String, String>, HostEvalError> {
        ensure_platform_multi();

        let mut params = v8::CreateParams::default();
        if let Some(mb) = heap_limit_mb {
            let bytes = (mb as usize) * 1024 * 1024;
            params = params.heap_limits(0, bytes);
        }
        let isolate = &mut v8::Isolate::new(params);

        let result = eval_in_isolate(isolate, modules, profile, inputs, outputs);
        // Always clear the per-call registry so globals don't leak across calls.
        MODULES.with(|m| m.borrow_mut().clear());
        result
    }
}

fn eval_in_isolate(
    isolate: &mut v8::Isolate,
    modules: &[ModuleEntry],
    profile: GlobalsProfile,
    inputs: &[(&str, &str)],
    outputs: &[&str],
) -> Result<HashMap<String, String>, HostEvalError> {
    v8::scope!(let handle_scope, isolate);
    let context = v8::Context::new(handle_scope, v8::ContextOptions::default());
    let scope = &mut v8::ContextScope::new(handle_scope, context);

    install_globals(scope, context, profile);

    // Set input string globals BEFORE eval.
    let global = context.global(scope);
    for (name, value) in inputs {
        let key = v8::String::new(scope, name)
            .ok_or_else(|| HostEvalError::MissingOutput((*name).to_string()))?;
        let val = v8::String::new(scope, value)
            .ok_or_else(|| HostEvalError::MissingOutput((*name).to_string()))?;
        global.set(scope, key.into(), val.into());
    }

    // Compile the whole graph so the resolve callback can find every specifier.
    compile_and_register(scope, modules)?;

    let entry_spec = modules
        .first()
        .map(|m| m.specifier.clone())
        .ok_or_else(|| HostEvalError::Install("empty module graph".to_string()))?;
    let entry = MODULES
        .with(|m| m.borrow().get(&entry_spec).cloned())
        .ok_or_else(|| HostEvalError::Install("entry module missing".to_string()))?;

    eval_entry(scope, &entry)?;

    // Read back output string globals (microtasks drain automatically under the
    // default Auto policy after module evaluation).
    let global = context.global(scope);
    let mut out = HashMap::with_capacity(outputs.len());
    for name in outputs {
        let key = v8::String::new(scope, name)
            .ok_or_else(|| HostEvalError::MissingOutput((*name).to_string()))?;
        match global.get(scope, key.into()) {
            Some(v) if v.is_string() => {
                out.insert((*name).to_string(), v.to_rust_string_lossy(scope));
            }
            _ => return Err(HostEvalError::MissingOutput((*name).to_string())),
        }
    }
    Ok(out)
}

/// Compile every module and register it by specifier for the resolve callback.
fn compile_and_register(
    scope: &mut v8::PinScope,
    modules: &[ModuleEntry],
) -> Result<(), HostEvalError> {
    for entry in modules {
        let source_str = v8::String::new(scope, &entry.source).ok_or_else(|| {
            HostEvalError::Install(format!("alloc source for {}", entry.specifier))
        })?;
        let name = v8::String::new(scope, &entry.specifier)
            .ok_or_else(|| HostEvalError::Install(format!("alloc name for {}", entry.specifier)))?;

        let origin = v8::ScriptOrigin::new(
            scope,
            name.into(),
            0,     // resource_line_offset
            0,     // resource_column_offset
            false, // resource_is_shared_cross_origin
            0,     // script_id
            None,  // source_map_url
            false, // resource_is_opaque
            false, // is_wasm
            true,  // is_module
            None,  // host_defined_options
        );
        let mut src = v8::script_compiler::Source::new(source_str, Some(&origin));
        let module = v8::script_compiler::compile_module(scope, &mut src)
            .ok_or_else(|| HostEvalError::Install(format!("compile {}", entry.specifier)))?;

        let global = v8::Global::new(scope, module);
        MODULES.with(|m| {
            m.borrow_mut().insert(entry.specifier.clone(), global);
        });
    }
    Ok(())
}

/// Instantiate + evaluate the entry module, mapping failures to `Install`.
fn eval_entry(
    scope: &mut v8::PinScope,
    entry: &v8::Global<v8::Module>,
) -> Result<(), HostEvalError> {
    let module = v8::Local::new(scope, entry);

    v8::tc_scope!(let tc, scope);
    let instantiated = module.instantiate_module(tc, resolve_callback);
    if instantiated != Some(true) || module.get_status() == v8::ModuleStatus::Errored {
        let msg = tc
            .exception()
            .map(|e| e.to_rust_string_lossy(tc))
            .unwrap_or_else(|| "<no exception>".to_string());
        return Err(HostEvalError::Install(format!("instantiate: {msg}")));
    }

    let eval_result = module.evaluate(tc);
    if module.get_status() == v8::ModuleStatus::Errored || eval_result.is_none() {
        let msg = tc
            .exception()
            .map(|e| e.to_rust_string_lossy(tc))
            .unwrap_or_else(|| "<no exception>".to_string());
        return Err(HostEvalError::Install(format!("evaluate: {msg}")));
    }
    Ok(())
}

/// ESM resolve callback — resolves a specifier to a previously-compiled module.
fn resolve_callback<'s>(
    context: v8::Local<'s, v8::Context>,
    specifier: v8::Local<'s, v8::String>,
    _import_attributes: v8::Local<'s, v8::FixedArray>,
    _referrer: v8::Local<'s, v8::Module>,
) -> Option<v8::Local<'s, v8::Module>> {
    // SAFETY: invoked synchronously inside `instantiate_module`, on the isolate
    // thread, with the module-instantiation context current. The returned module's
    // Local lifetime `'s` ties to `context`, matching the callback contract.
    v8::callback_scope!(unsafe scope, context);
    let spec = specifier.to_rust_string_lossy(scope);
    MODULES.with(|m| m.borrow().get(&spec).map(|g| v8::Local::new(scope, g)))
}

/// Install a minimal but real globals profile onto the context's global object.
fn install_globals(
    scope: &mut v8::PinScope,
    context: v8::Local<v8::Context>,
    _profile: GlobalsProfile,
) {
    let global = context.global(scope);

    // globalThis (self-reference).
    if let Some(key) = v8::String::new(scope, "globalThis") {
        global.set(scope, key.into(), global.into());
    }

    // A no-op console so `console.*` doesn't throw.
    let console = v8::Object::new(scope);
    for method in ["log", "error", "warn", "info", "debug"] {
        if let (Some(fname), Some(f)) = (
            v8::String::new(scope, method),
            v8::Function::new(scope, noop_callback),
        ) {
            console.set(scope, fname.into(), f.into());
        }
    }
    if let Some(ckey) = v8::String::new(scope, "console") {
        global.set(scope, ckey.into(), console.into());
    }
}

fn noop_callback(
    _scope: &mut v8::PinScope,
    _args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue<v8::Value>,
) {
}

// ---------------------------------------------------------------------------
// JsDriverHost — partial (see the module docs / README).
// ---------------------------------------------------------------------------

/// Opaque driver-isolate handle. In a full impl this wraps the mysql2/node:net
/// isolate + its pump; in this pass it is a marker so the trait's associated type
/// resolves and the MySQL backend type-checks.
#[derive(Debug)]
pub struct DriverSession {
    _private: (),
}

impl JsDriverHost for ZeroMigrateHost {
    type Session = DriverSession;

    fn open_js_driver(
        &self,
        _modules: &[ModuleEntry],
        _net_policy: NetPolicy,
        _dsn_json: String,
        _command_timeout: Duration,
    ) -> Result<Self::Session, HostEvalError> {
        Err(HostEvalError::Install(
            "JsDriverHost (mysql2-over-node:net driver isolate) is not implemented in the \
             standalone public-v8 host yet — this needs a node:net socket polyfill + event-loop \
             pump. The AuthoringHost + RecorderPlatform seams are fully implemented."
                .to_string(),
        ))
    }

    async fn js_driver_command(
        &self,
        _session: &mut Self::Session,
        _id: u64,
        _payload: String,
    ) -> Result<String, JsDriverTimeout> {
        Err(JsDriverTimeout {
            elapsed: Duration::ZERO,
        })
    }

    fn close_js_driver(&self, _session: &mut Self::Session) {}
}
