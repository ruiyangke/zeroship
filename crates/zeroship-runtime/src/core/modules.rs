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
    sources: HashMap<String, String>,
    host_names: HashSet<String>,
    host_only_names: HashSet<String>,
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
            sources: HashMap::new(),
            host_names: HashSet::new(),
            host_only_names: HashSet::new(),
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

    fn check_source_import(&self, referrer: &str, resolved: &str) -> Result<(), String> {
        if !self.host_only_names.contains(referrer) && self.host_only_names.contains(resolved) {
            return Err(format!(
                "Module {referrer:?} cannot import host-only module {resolved:?}"
            ));
        }
        if self.host_names.contains(referrer) && !self.host_names.contains(resolved) {
            return Err(format!(
                "Plugin module {referrer:?} cannot import creator module {resolved:?}"
            ));
        }
        Ok(())
    }
}

/// Resolve relative paths against their importing module, with no root fallback.
fn resolve_specifier(
    specifier: &str,
    referrer: &str,
    exists: impl Fn(&str) -> bool,
) -> Option<String> {
    let resolved = if specifier.starts_with("./") || specifier.starts_with("../") {
        let base = referrer.rsplit_once('/').map_or("", |(base, _)| base);
        let mut parts = Vec::new();
        for part in base.split('/').chain(specifier.split('/')) {
            match part {
                "" | "." => {}
                ".." => {
                    parts.pop()?;
                }
                part => parts.push(part),
            }
        }
        let prefix = if referrer.starts_with('/') { "/" } else { "" };
        format!("{prefix}{}", parts.join("/"))
    } else {
        specifier.to_owned()
    };
    [resolved.clone(), format!("{resolved}.js")]
        .into_iter()
        .find(|candidate| exists(candidate))
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
    for module in &plugin_modules.modules {
        if sources
            .insert(module.specifier.to_string(), module.source.to_string())
            .is_some()
        {
            return Err(format!("Module shadows plugin source: {}", module.specifier));
        }
    }
    let registry = Rc::new(RefCell::new(ModuleRegistry {
        compiled: HashMap::new(),
        sources,
        host_names: plugin_modules
            .modules
            .iter()
            .map(|module| module.specifier.to_owned())
            .collect(),
        host_only_names: plugin_modules
            .host_only
            .iter()
            .map(|specifier| (*specifier).to_owned())
            .collect(),
    }));
    scope.set_slot(registry.clone());
    let entry = compile_registered_graph(scope, &registry, &entries[0].specifier)?;
    for module in &plugin_modules.modules {
        compile_registered_graph(scope, &registry, module.specifier)?;
    }
    Ok(entry)
}

/// Look up a module the compiled graph already holds, by entry specifier.
///
/// Callers use this to reach a module the registry owns instead of compiling
/// or evaluating a second copy of it under another specifier.
pub(crate) fn registered_module(
    scope: &mut v8::PinScope,
    entry: Option<&ModuleEntry>,
) -> Option<v8::Global<v8::Module>> {
    let specifier = &entry?.specifier;
    let registry = scope.get_slot::<SharedRegistry>().cloned()?;
    let registry = registry.borrow();
    registry.get(specifier).cloned()
}

/// Prepare a dynamic dependency and return its canonical registry name.
/// Evaluation is deferred until the importing module leaves its sync frame.
pub(crate) fn dynamic_module(
    scope: &mut v8::PinScope,
    specifier: &str,
    referrer: &str,
) -> Result<Option<String>, String> {
    let Some(registry) = scope.get_slot::<SharedRegistry>().cloned() else {
        return Ok(None);
    };
    let resolved = {
        let registry = registry.borrow();
        resolve_specifier(specifier, referrer, |name| {
            registry.sources.contains_key(name) || registry.compiled.contains_key(name)
        })
    };
    let Some(root) = resolved else {
        return Ok(None);
    };
    {
        let registry = registry.borrow();
        if registry.host_only_names.contains(&root) && !registry.host_only_names.contains(referrer) {
            return Err(format!("Cannot find module '{specifier}'"));
        }
    }
    if !super::native_modules::is_native(scope, &root) {
        registry.borrow().check_source_import(referrer, &root)?;
    }
    compile_registered_graph(scope, &registry, &root)?;
    Ok(Some(root))
}

fn compile_registered_graph(
    scope: &mut v8::PinScope,
    registry: &SharedRegistry,
    root: &str,
) -> Result<v8::Global<v8::Module>, String> {
    if let Some(module) = registry.borrow().get(root).cloned() { return Ok(module); }
    let source = registry.borrow().sources.get(root).cloned()
        .ok_or_else(|| format!("Module source not found: {root}"))?;
    let entry = compile_module(scope, root, &source)?;
    let mut queue = VecDeque::from([(root.to_owned(), entry.clone())]);
    let mut scheduled = HashSet::from([root.to_owned()]);
    let mut compiled = HashMap::new();
    while let Some((specifier, module)) = queue.pop_front() {
        compiled.insert(specifier.clone(), module.clone());
        let module = v8::Local::new(scope, &module);
        let requests = module.get_module_requests();
        for index in 0..requests.length() {
            let request = v8::Local::<v8::ModuleRequest>::try_from(requests.get(scope, index).unwrap()).unwrap();
            let import = request.get_specifier().to_rust_string_lossy(scope);
            if super::native_modules::is_native(scope, &import) {
                if registry.borrow().get(&import).is_none() && !compiled.contains_key(&import) {
                    let module = super::native_modules::resolve_native(scope, &import)
                        .expect("registered native module");
                    compiled.insert(import, v8::Global::new(scope, module));
                }
                continue;
            }
            let (resolved, source) = {
                let registry = registry.borrow();
                let resolved = resolve_specifier(&import, &specifier, |name| {
                    registry.sources.contains_key(name)
                })
                .ok_or_else(|| format!("Cannot resolve import '{import}' from '{specifier}'"))?;
                registry.check_source_import(&specifier, &resolved)?;
                if registry.get(&resolved).is_some() || !scheduled.insert(resolved.clone()) { continue; }
                let source = registry.sources.get(&resolved).expect("resolved module source").clone();
                (resolved, source)
            };
            let module = compile_module(scope, &resolved, &source)?;
            queue.push_back((resolved, module));
        }
    }
    // Publish the graph together so a failed import cannot leave a partial root.
    registry.borrow_mut().compiled.extend(compiled);
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
    referrer: v8::Local<'a, v8::Module>,
) -> Option<v8::Local<'a, v8::Module>> {
    v8::callback_scope!(unsafe scope, context);

    let spec = specifier.to_rust_string_lossy(scope);

    let registry: SharedRegistry = scope
        .get_slot::<SharedRegistry>()
        .expect("ModuleRegistry not in slot")
        .clone();

    {
        let reg = registry.borrow();

        let referrer_name = reg
            .compiled
            .iter()
            .find(|(_, module)| v8::Local::new(scope, *module) == referrer)
            .map(|(name, _)| name.as_str())?;
        if let Some(resolved) =
            resolve_specifier(&spec, referrer_name, |name| reg.compiled.contains_key(name))
        {
            return reg
                .get(&resolved)
                .map(|module| v8::Local::new(scope, module));
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
