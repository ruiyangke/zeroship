//! V8 ESM module registry — lazy compilation via import graph discovery.
//!
//! The entry and plugin adapter modules are compiled eagerly. V8's module
//! requests drive recursive import discovery before instantiation. The resolve
//! callback only looks up compiled modules.
//!
//! Creator modules outside the entry's import graph are left unparsed.

#![allow(unsafe_code)]

use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::rc::Rc;

/// A pre-resolved module to be loaded into V8.
#[derive(Debug, Clone)]
pub struct ModuleEntry {
    pub specifier: String,
    pub source: String,
}

/// Module registry stored in V8 isolate slot.
pub struct ModuleRegistry {
    /// Compiled V8 modules — populated by the lazy compilation loop.
    compiled: HashMap<String, v8::Global<v8::Module>>,
}

pub type SharedRegistry = Rc<RefCell<ModuleRegistry>>;

impl Default for ModuleRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ModuleRegistry {
    pub fn new() -> Self {
        Self {
            compiled: HashMap::new(),
        }
    }

    /// Lookup a pre-compiled module by exact specifier. The dynamic-import
    /// callback uses this; the static `resolve_callback` reads `compiled`
    /// directly because it also needs to mutate on the native fallback
    /// path. Read-only accessors keep `compiled` private.
    pub(crate) fn get(&self, specifier: &str) -> Option<&v8::Global<v8::Module>> {
        self.compiled.get(specifier)
    }

    /// Insert (or replace) a compiled module under `specifier`. Used by
    /// the dynamic-import callback when caching a freshly-minted native
    /// synthetic so subsequent imports return the same module record.
    pub(crate) fn insert(
        &mut self,
        specifier: String,
        module: v8::Global<v8::Module>,
    ) -> Option<v8::Global<v8::Module>> {
        self.compiled.insert(specifier, module)
    }
}

/// Resolve a specifier against the source map, trying common variants.
fn resolve_specifier(specifier: &str, sources: &HashMap<String, String>) -> Option<String> {
    let candidates = [
        specifier.to_string(),
        specifier.strip_prefix("./").unwrap_or(specifier).to_string(),
        format!("{specifier}.js"),
        format!("{}.js", specifier.strip_prefix("./").unwrap_or(specifier)),
    ];
    for candidate in &candidates {
        if sources.contains_key(candidate) {
            return Some(candidate.clone());
        }
    }
    None
}

/// Compile a single module from source.
///
/// Native synthetic modules are created separately by their runtime factories.
pub(crate) fn compile_module(
    scope: &mut v8::PinScope,
    specifier: &str,
    source: &str,
) -> Result<v8::Global<v8::Module>, String> {
    let source_str = v8::String::new(scope, source)
        .ok_or_else(|| format!("Source too large: {specifier}"))?;
    let name_str = v8::String::new(scope, specifier)
        .ok_or_else(|| format!("Specifier too large: {specifier}"))?;

    let origin = v8::ScriptOrigin::new(
        scope, name_str.into(),
        0, 0, false, -1, None, false, false,
        true, // is_module
        None,
    );

    let mut v8_source = v8::script_compiler::Source::new(source_str, Some(&origin));
    let module = v8::script_compiler::compile_module(scope, &mut v8_source)
        .ok_or_else(|| format!("Failed to compile: {specifier}"))?;

    Ok(v8::Global::new(scope, module))
}

/// Load modules with lazy compilation.
///
/// `entries[0]` is the entrypoint. All entries are stored as source strings,
/// but only transitively imported creator modules are compiled. Registered
/// plugin adapters and their dependency graphs are also compiled.
///
/// Returns the compiled entry module without evaluating creator code.
pub(crate) fn compile_modules(
    scope: &mut v8::PinScope,
    entries: &[ModuleEntry],
) -> Result<v8::Global<v8::Module>, String> {
    if entries.is_empty() {
        return Err("No modules to load".into());
    }

    // Build source map (specifier → source string)
    let mut sources: HashMap<String, String> = HashMap::new();
    for entry in entries {
        sources.insert(entry.specifier.clone(), entry.source.clone());
    }

    let plugin_modules = scope
        .get_slot::<super::plugin_modules::PluginModules>()
        .cloned()
        .unwrap_or_default();
    for module in &plugin_modules.0 {
        if sources
            .insert(module.specifier.to_string(), module.source.to_string())
            .is_some()
        {
            return Err(format!("Module shadows plugin source: {}", module.specifier));
        }
    }
    let host_names: HashSet<&str> = plugin_modules.0.iter()
        .map(|module| module.specifier)
        .collect();

    let registry: SharedRegistry = Rc::new(RefCell::new(ModuleRegistry::new()));

    // Compile the entry module.
    let entrypoint = &entries[0].specifier;
    let entry_source = sources.get(entrypoint)
        .ok_or_else(|| format!("Entrypoint not found: {entrypoint}"))?;
    let entry_module = compile_module(scope, entrypoint, entry_source)?;

    // Discover and compile all transitive imports (BFS).
    //
    // V8's resolve_callback can't compile modules — it must return
    // an already-compiled module. So we walk the import graph here,
    // compiling each discovered module BEFORE calling instantiate_module.
    {
        let mut queue: VecDeque<(String, v8::Global<v8::Module>)> = VecDeque::new();
        queue.push_back((entrypoint.clone(), entry_module));
        let mut scheduled = HashSet::from([entrypoint.clone()]);
        // Compile adapter roots and discover their imports before evaluation.
        // Dynamic imports then use the same cached graph as static ones.
        for module in &plugin_modules.0 {
            let compiled = compile_module(scope, module.specifier, module.source)?;
            queue.push_back((module.specifier.to_string(), compiled));
            scheduled.insert(module.specifier.to_string());
        }

        while let Some((spec, module_global)) = queue.pop_front() {
            // Store compiled module in registry
            let already_registered = registry.borrow().compiled.contains_key(&spec);
            if already_registered {
                continue;
            }
            registry.borrow_mut().compiled.insert(spec.clone(), module_global.clone());

            // Discover this module's imports via V8
            let module_local = v8::Local::new(scope, &module_global);
            let requests = module_local.get_module_requests();
            let num_requests = requests.length();

            for i in 0..num_requests {
                let request = v8::Local::<v8::ModuleRequest>::try_from(
                    requests.get(scope, i).unwrap()
                ).unwrap();
                let import_specifier = request.get_specifier().to_rust_string_lossy(scope);

                // Native synthetic module (e.g. `node:async_hooks`) — minted
                // here so the resolve callback finds it pre-instantiation.
                // Synthetic modules have no imports, so we don't queue
                // them for further discovery.
                if super::native_modules::is_native(scope, &import_specifier) {
                    if !registry.borrow().compiled.contains_key(&import_specifier) {
                        let m = super::native_modules::resolve_native(scope, &import_specifier)
                            .expect("is_native true but resolve_native returned None");
                        registry
                            .borrow_mut()
                            .compiled
                            .insert(import_specifier.clone(), v8::Global::new(scope, m));
                    }
                    continue;
                }

                // Resolve to actual source specifier
                let resolved = match resolve_specifier(&import_specifier, &sources) {
                    Some(s) => s,
                    None => return Err(format!(
                        "Cannot resolve import '{import_specifier}' from '{spec}'"
                    )),
                };

                if host_names.contains(spec.as_str())
                    && !host_names.contains(resolved.as_str())
                {
                    return Err(format!(
                        "Plugin module {spec:?} cannot import creator module {resolved:?}"
                    ));
                }

                // Skip sources already compiled or queued for discovery.
                if !scheduled.insert(resolved.clone()) {
                    continue;
                }

                // Compile the imported module
                let source = sources.get(&resolved)
                    .ok_or_else(|| format!(
                        "Source not found for '{resolved}' (imported from '{spec}')"
                    ))?;
                let compiled = compile_module(scope, &resolved, source)?;

                // Queue for import discovery (its own imports)
                queue.push_back((resolved, compiled));
            }
        }
    }

    // Store registry in isolate slot for the resolve callback
    scope.set_slot(registry.clone());

    let entry = registry.borrow().compiled.get(entrypoint).cloned()
        .ok_or_else(|| format!("Entrypoint not compiled: {entrypoint}"))?;
    Ok(entry)
}

/// An evaluated module whose top-level promise is owned by the host.
pub(crate) struct ModuleEvaluation {
    pub namespace: v8::Global<v8::Value>,
    pub promise: Option<v8::Global<v8::Promise>>,
}

impl ModuleEvaluation {
    pub fn is_ready(&self, scope: &mut v8::PinScope) -> Result<bool, String> {
        self.promise.as_ref().map_or(Ok(true), |promise| promise_ready(scope, promise))
    }
}

pub(crate) fn error_detail(scope: &mut v8::PinScope, error: v8::Local<v8::Value>) -> String {
    v8::tc_scope!(let tc, scope);
    let fallback = error.to_rust_string_lossy(tc);
    let Some(object) = error.to_object(tc) else { return fallback; };
    let key = v8::String::new(tc, "stack").unwrap();
    match object.get(tc, key.into()) {
        Some(stack) if stack.is_string() => stack.to_rust_string_lossy(tc),
        _ => fallback,
    }
}

pub(crate) fn promise_ready(
    scope: &mut v8::PinScope,
    promise: &v8::Global<v8::Promise>,
) -> Result<bool, String> {
    let promise = v8::Local::new(scope, promise);
    match promise.state() {
        v8::PromiseState::Pending => Ok(false),
        v8::PromiseState::Fulfilled => Ok(true),
        v8::PromiseState::Rejected => {
            let error = promise.result(scope);
            Err(error_detail(scope, error))
        }
    }
}

pub(crate) fn evaluate_module(
    scope: &mut v8::PinScope,
    module: &v8::Global<v8::Module>,
) -> Result<ModuleEvaluation, String> {
    v8::tc_scope!(let tc, scope);
    let module = v8::Local::new(tc, module);
    if module.get_status() == v8::ModuleStatus::Uninstantiated
        && module.instantiate_module(tc, resolve_callback) != Some(true)
    {
        return Err(tc.exception().map_or_else(
            || "module instantiation failed".into(), |error| error_detail(tc, error),
        ));
    }
    let result = module.evaluate(tc).ok_or_else(|| {
        tc.exception().map_or_else(
            || "module evaluation failed".into(), |error| error_detail(tc, error),
        )
    })?;
    let promise = v8::Local::<v8::Promise>::try_from(result).ok().map(|promise| {
        promise.mark_as_handled();
        v8::Global::new(tc, promise)
    });
    let namespace = v8::Global::new(tc, module.get_module_namespace());
    Ok(ModuleEvaluation { namespace, promise })
}

/// Load a module graph whose evaluation settles during its microtask checkpoint.
/// Runtime application startup uses the retained evaluation promise instead.
///
/// # Errors
/// Returns the linking or evaluation failure, or rejects a still-pending graph.
pub fn load_modules(
    scope: &mut v8::PinScope,
    entries: &[ModuleEntry],
) -> Result<v8::Global<v8::Value>, String> {
    let module = compile_modules(scope, entries)?;
    let evaluation = evaluate_module(scope, &module)?;
    crate::core::init::perform_microtask_checkpoint(scope);
    if !evaluation.is_ready(scope)? {
        return Err("module evaluation requires the runtime startup pump".into());
    }
    Ok(evaluation.namespace)
}

/// Invoke an SDK module export after its module evaluation has settled.
/// The caller owns the returned promise and its initialization arguments.
///
/// # Errors
/// Returns a module lookup, linking or scheduling failure. Asynchronous module
/// evaluation and export invocation failures reject the returned promise.
pub fn invoke_module_export(
    scope: &mut v8::PinScope,
    specifier: &str,
    export: &str,
    args: &[v8::Local<v8::Value>],
) -> Result<v8::Global<v8::Promise>, String> {
    let registry = scope.get_slot::<SharedRegistry>().cloned()
        .ok_or_else(|| "module registry is not initialized".to_string())?;
    let module = registry.borrow().get(specifier).cloned()
        .ok_or_else(|| format!("module {specifier:?} is not registered"))?;
    let evaluation = evaluate_module(scope, &module)?;
    let namespace = v8::Local::new(scope, evaluation.namespace);
    let name = v8::String::new(scope, export).ok_or("could not allocate export name")?;
    let args = v8::Array::new_with_elements(scope, args);
    let data = v8::Array::new_with_elements(scope, &[namespace, name.into(), args.into()]);
    let callback = v8::Function::builder(invoke_export_callback).data(data.into())
        .build(scope).ok_or("could not allocate export invocation")?;
    let promise = if let Some(promise) = evaluation.promise {
        v8::Local::new(scope, promise)
    } else {
        let resolver = v8::PromiseResolver::new(scope).ok_or("could not allocate evaluation promise")?;
        let undefined = v8::undefined(scope);
        resolver.resolve(scope, undefined.into());
        resolver.get_promise(scope)
    };
    let result = promise.then(scope, callback).ok_or("could not schedule module export")?;
    result.mark_as_handled();
    Ok(v8::Global::new(scope, result))
}

#[expect(clippy::needless_pass_by_value, reason = "V8 callback signature")]
fn invoke_export_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let data = v8::Local::<v8::Array>::try_from(args.data()).unwrap();
    let namespace = data.get_index(scope, 0).unwrap().to_object(scope).unwrap();
    let name = data.get_index(scope, 1).unwrap();
    let Some(value) = namespace.get(scope, name) else { return; };
    let Ok(function) = v8::Local::<v8::Function>::try_from(value) else {
        let name = name.to_rust_string_lossy(scope);
        let message = v8::String::new(scope, &format!("module export {name:?} is not callable")).unwrap();
        let error = v8::Exception::type_error(scope, message);
        scope.throw_exception(error);
        return;
    };
    let values = v8::Local::<v8::Array>::try_from(data.get_index(scope, 2).unwrap()).unwrap();
    let values: Vec<_> = (0..values.length()).map(|i| values.get_index(scope, i).unwrap()).collect();
    let undefined = v8::undefined(scope);
    if let Some(result) = function.call(scope, undefined.into(), &values) { rv.set(result); }
}

/// V8 resolve callback — lookups only, never compiles.
///
/// All transitively imported modules are compiled before instantiation.
/// Dynamic imports reuse this lookup for plugin adapter dependency graphs.
pub(crate) fn resolve_callback<'a>(
    context: v8::Local<'a, v8::Context>,
    specifier: v8::Local<'a, v8::String>,
    _import_attributes: v8::Local<'a, v8::FixedArray>,
    _referrer: v8::Local<'a, v8::Module>,
) -> Option<v8::Local<'a, v8::Module>> {
    v8::callback_scope!(unsafe scope, context);

    let spec = specifier.to_rust_string_lossy(scope);

    let registry: SharedRegistry = scope
        .get_slot::<SharedRegistry>()
        .expect("ModuleRegistry not in slot")
        .clone();

    {
        let reg = registry.borrow();

        // Try exact, then variants
        let candidates = [
            spec.clone(),
            spec.strip_prefix("./").unwrap_or(&spec).to_string(),
            format!("{spec}.js"),
            format!("{}.js", spec.strip_prefix("./").unwrap_or(&spec)),
        ];

        for candidate in &candidates {
            if let Some(module_global) = reg.compiled.get(candidate) {
                return Some(v8::Local::new(scope, module_global));
            }
        }
    }

    // Fallback for native modules. The eager import walk should have
    // pre-registered these already, but this guards against unusual
    // entry shapes.
    if let Some(m) = super::native_modules::resolve_native(scope, &spec) {
        registry
            .borrow_mut()
            .compiled
            .insert(spec.clone(), v8::Global::new(scope, m));
        return Some(m);
    }

    tracing::error!(specifier = %spec, "module resolution failed");
    None
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::init::init_v8;

    #[test]
    fn single_module() {
        init_v8();
        let params = v8::CreateParams::default();
        let mut isolate = v8::Isolate::new(params);

        v8::scope!(let hs, &mut isolate);
        let ctx = v8::Context::new(hs, Default::default());
        let scope = &mut v8::ContextScope::new(hs, ctx);

        let entries = vec![ModuleEntry {
            specifier: "index.js".into(),
            source: "globalThis.testValue = 42;".into(),
        }];

        load_modules(scope, &entries).unwrap();

        let global = ctx.global(scope);
        let key = v8::String::new(scope, "testValue").unwrap();
        let val = global.get(scope, key.into()).unwrap();
        assert_eq!(val.int32_value(scope).unwrap(), 42);
    }

    #[test]
    fn two_modules_import() {
        init_v8();
        let params = v8::CreateParams::default();
        let mut isolate = v8::Isolate::new(params);

        v8::scope!(let hs, &mut isolate);
        let ctx = v8::Context::new(hs, Default::default());
        let scope = &mut v8::ContextScope::new(hs, ctx);

        let entries = vec![
            ModuleEntry {
                specifier: "index.js".into(),
                source: "import { add } from './math.js'; globalThis.result = add(3, 4);".into(),
            },
            ModuleEntry {
                specifier: "math.js".into(),
                source: "export function add(a, b) { return a + b; }".into(),
            },
        ];

        load_modules(scope, &entries).unwrap();

        let global = ctx.global(scope);
        let key = v8::String::new(scope, "result").unwrap();
        let val = global.get(scope, key.into()).unwrap();
        assert_eq!(val.int32_value(scope).unwrap(), 7);
    }

    #[test]
    fn three_level_chain() {
        init_v8();
        let params = v8::CreateParams::default();
        let mut isolate = v8::Isolate::new(params);

        v8::scope!(let hs, &mut isolate);
        let ctx = v8::Context::new(hs, Default::default());
        let scope = &mut v8::ContextScope::new(hs, ctx);

        let entries = vec![
            ModuleEntry {
                specifier: "index.js".into(),
                source: r#"
                    import { greet } from './greeter.js';
                    globalThis.message = greet("World");
                "#.into(),
            },
            ModuleEntry {
                specifier: "greeter.js".into(),
                source: r#"
                    import { upper } from './utils.js';
                    export function greet(name) { return "Hello, " + upper(name) + "!"; }
                "#.into(),
            },
            ModuleEntry {
                specifier: "utils.js".into(),
                source: "export function upper(s) { return s.toUpperCase(); }".into(),
            },
        ];

        load_modules(scope, &entries).unwrap();

        let global = ctx.global(scope);
        let key = v8::String::new(scope, "message").unwrap();
        let val = global.get(scope, key.into()).unwrap();
        assert_eq!(val.to_rust_string_lossy(scope), "Hello, WORLD!");
    }

    #[test]
    fn missing_import_fails() {
        init_v8();
        let params = v8::CreateParams::default();
        let mut isolate = v8::Isolate::new(params);

        v8::scope!(let hs, &mut isolate);
        let ctx = v8::Context::new(hs, Default::default());
        let scope = &mut v8::ContextScope::new(hs, ctx);

        let entries = vec![ModuleEntry {
            specifier: "index.js".into(),
            source: "import { x } from './nonexistent.js';".into(),
        }];

        let result = load_modules(scope, &entries);
        assert!(result.is_err());
    }

    #[test]
    fn shared_module() {
        init_v8();
        let params = v8::CreateParams::default();
        let mut isolate = v8::Isolate::new(params);

        v8::scope!(let hs, &mut isolate);
        let ctx = v8::Context::new(hs, Default::default());
        let scope = &mut v8::ContextScope::new(hs, ctx);

        let entries = vec![
            ModuleEntry {
                specifier: "index.js".into(),
                source: r#"
                    import { a } from './a.js';
                    import { b } from './b.js';
                    globalThis.sum = a() + b();
                "#.into(),
            },
            ModuleEntry {
                specifier: "a.js".into(),
                source: r#"
                    import { value } from './shared.js';
                    export function a() { return value; }
                "#.into(),
            },
            ModuleEntry {
                specifier: "b.js".into(),
                source: r#"
                    import { value } from './shared.js';
                    export function b() { return value * 2; }
                "#.into(),
            },
            ModuleEntry {
                specifier: "shared.js".into(),
                source: "export const value = 10;".into(),
            },
        ];

        load_modules(scope, &entries).unwrap();

        let global = ctx.global(scope);
        let key = v8::String::new(scope, "sum").unwrap();
        let val = global.get(scope, key.into()).unwrap();
        assert_eq!(val.int32_value(scope).unwrap(), 30);
    }

    #[test]
    fn unused_modules_not_compiled() {
        // Unused modules (including one with invalid syntax) should not
        // cause errors — only transitively imported modules are compiled.
        init_v8();
        let params = v8::CreateParams::default();
        let mut isolate = v8::Isolate::new(params);

        v8::scope!(let hs, &mut isolate);
        let ctx = v8::Context::new(hs, Default::default());
        let scope = &mut v8::ContextScope::new(hs, ctx);

        let entries = vec![
            ModuleEntry {
                specifier: "index.js".into(),
                source: "export function ping() { return 'pong'; }".into(),
            },
            ModuleEntry {
                specifier: "unused.js".into(),
                source: "export function unused() { return 'never'; }".into(),
            },
            ModuleEntry {
                specifier: "invalid-syntax.js".into(),
                source: "THIS IS NOT VALID JAVASCRIPT }{}{".into(),
            },
        ];

        let result = load_modules(scope, &entries);
        assert!(result.is_ok(), "Unused modules (even invalid ones) should not cause errors");
    }
}
