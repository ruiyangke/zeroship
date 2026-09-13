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
    /// Uncompiled bundle modules remain available to dynamic imports.
    sources: HashMap<String, String>,
    /// Adapter ownership applies to dependencies discovered by either import path.
    plugin_modules: HashSet<String>,
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
            sources: HashMap::new(),
            plugin_modules: HashSet::new(),
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

    fn check_source_import(&self, referrer: &str, resolved: &str) -> Result<(), String> {
        if self.plugin_modules.contains(referrer)
            && resolved != "zeroship"
            && !self.plugin_modules.contains(resolved)
        {
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
/// Shared with the dynamic-import callback for the core `zeroship` facade.
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

/// Compile a module and its static dependency closure without evaluating it.
/// Register each module before visiting imports so cycles share module records.
fn compile_graph(
    scope: &mut v8::PinScope,
    registry: &SharedRegistry,
    root: &str,
) -> Result<v8::Global<v8::Module>, String> {
    let mut queue = VecDeque::from([root.to_owned()]);
    let mut visited = HashSet::new();
    while let Some(spec) = queue.pop_front() {
        if !visited.insert(spec.clone()) {
            continue;
        }
        let existing = registry.borrow().get(&spec).cloned();
        let module = if let Some(module) = existing {
            module
        } else {
            let source = registry
                .borrow()
                .sources
                .get(&spec)
                .cloned()
                .ok_or_else(|| format!("Source not found for '{spec}'"))?;
            let module = compile_module(scope, &spec, &source)?;
            registry.borrow_mut().insert(spec.clone(), module.clone());
            module
        };
        let module = v8::Local::new(scope, &module);
        if !module.is_source_text_module() {
            continue;
        }
        let requests = module.get_module_requests();
        for index in 0..requests.length() {
            let request =
                v8::Local::<v8::ModuleRequest>::try_from(requests.get(scope, index).unwrap())
                    .unwrap();
            let imported = request.get_specifier().to_rust_string_lossy(scope);
            if super::native_modules::is_native(scope, &imported) {
                if registry.borrow().get(&imported).is_none() {
                    let module = super::native_modules::resolve_native(scope, &imported)
                        .ok_or_else(|| format!("Cannot resolve native module '{imported}'"))?;
                    registry
                        .borrow_mut()
                        .insert(imported, v8::Global::new(scope, module));
                }
                continue;
            }
            let resolved = {
                let reg = registry.borrow();
                resolve_specifier(&imported, &spec, |name| {
                    reg.sources.contains_key(name) || reg.compiled.contains_key(name)
                })
            }
            .ok_or_else(|| format!("Cannot resolve import '{imported}' from '{spec}'"))?;
            registry.borrow().check_source_import(&spec, &resolved)?;
            queue.push_back(resolved);
        }
    }
    registry
        .borrow()
        .get(root)
        .cloned()
        .ok_or_else(|| format!("Module not compiled: {root}"))
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
        let reg = registry.borrow();
        resolve_specifier(specifier, referrer, |name| {
            reg.sources.contains_key(name) || reg.compiled.contains_key(name)
        })
    };
    let Some(root) = resolved else {
        return Ok(None);
    };
    if !super::native_modules::is_native(scope, &root) {
        registry.borrow().check_source_import(referrer, &root)?;
    }
    if let Some(module) = registry.borrow().get(&root).cloned() {
        // Linking has already prepared this module's complete static closure.
        if v8::Local::new(scope, &module).get_status() != v8::ModuleStatus::Uninstantiated {
            return Ok(Some(root));
        }
    }
    compile_graph(scope, &registry, &root)?;
    Ok(Some(root))
}

/// Load modules with lazy compilation.
///
/// `entries[0]` is the entrypoint. All entries are stored as source strings,
/// but only transitively imported creator modules are compiled. Registered
/// plugin adapters and their dependency graphs are also compiled.
///
/// Returns the entrypoint module's namespace object (contains exports).
pub fn load_modules(
    scope: &mut v8::PinScope,
    entries: &[ModuleEntry],
) -> Result<v8::Global<v8::Value>, String> {
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
    let host_names: HashSet<String> = plugin_modules
        .0
        .iter()
        .map(|module| module.specifier.to_owned())
        .collect();

    let registry: SharedRegistry = Rc::new(RefCell::new(ModuleRegistry::new()));

    {
        let mut reg = registry.borrow_mut();
        reg.sources = sources;
        reg.plugin_modules = host_names;
    }

    // Store registry in isolate slot for the resolve callback
    scope.set_slot(registry.clone());
    let entrypoint = &entries[0].specifier;
    compile_graph(scope, &registry, entrypoint)?;
    // Validate adapter roots even when creator code only imports them lazily.
    for module in &plugin_modules.0 {
        compile_graph(scope, &registry, module.specifier)?;
    }

    // Instantiate the entrypoint.
    // resolve_callback only does lookups — all modules are pre-compiled.
    {
        let reg = registry.borrow();
        let module_global = reg.compiled.get(entrypoint)
            .ok_or_else(|| format!("Entrypoint not compiled: {entrypoint}"))?;
        let module = v8::Local::new(scope, module_global);

        let ok = module.instantiate_module(scope, resolve_callback);
        if ok.is_none() || ok == Some(false) {
            return Err(format!("Failed to instantiate: {entrypoint}"));
        }
    }

    // Evaluate.
    //
    // The registry borrow is released BEFORE `module.evaluate()`: a
    // top-level `await import(...)` in the entry (e.g. the bootstrap
    // `runtime-entry.js`'s `import("zeroship:db/internal")`)
    // fires the dynamic-import host callback synchronously during evaluate
    // AND during the microtask checkpoint below. That callback may
    // `borrow_mut()` the registry to cache a freshly-resolved module — so
    // holding a shared borrow across evaluate would `RefCell`-panic
    // (a non-unwinding abort). We only need the borrow to fetch the
    // module handle; clone it out and drop the guard immediately.
    let entry_module_g = {
        let reg = registry.borrow();
        reg.compiled.get(entrypoint).unwrap().clone()
    };
    let eval_rejection: Option<v8::Global<v8::Value>> = {
        let module = v8::Local::new(scope, &entry_module_g);

        let (result_global, sync_exc) = {
            v8::tc_scope!(let tc, scope);
            let r = module.evaluate(tc);
            if tc.has_caught() {
                let exc = tc.exception().map(|e| v8::Global::new(tc, e));
                (None, exc)
            } else {
                (r.map(|v| v8::Global::new(tc, v)), None)
            }
        };

        crate::core::init::perform_microtask_checkpoint(scope);

        if let Some(exc) = sync_exc {
            let local = v8::Local::new(scope, &exc);
            tracing::error!(
                error = %local.to_rust_string_lossy(scope),
                "v8 evaluate sync threw"
            );
            return Err(format!("Failed to evaluate: {entrypoint}"));
        }

        let Some(result_g) = result_global else {
            return Err(format!("Failed to evaluate: {entrypoint}"));
        };
        let result = v8::Local::new(scope, &result_g);
        if result.is_promise() {
            let promise: v8::Local<v8::Promise> = result.try_into().unwrap();
            if promise.state() == v8::PromiseState::Rejected {
                Some(v8::Global::new(scope, promise.result(scope)))
            } else {
                None
            }
        } else {
            None
        }
    };

    if let Some(rej) = eval_rejection {
        let local = v8::Local::new(scope, &rej);
        let error = local.to_rust_string_lossy(scope);
        let mut detail = error.clone();
        tracing::error!(
            error = %error,
            "v8 evaluate rejected"
        );
        if let Some(obj) = local.to_object(scope) {
            let stack_key = v8::String::new(scope, "stack").unwrap();
            if let Some(stack_val) = obj.get(scope, stack_key.into())
                && !stack_val.is_undefined()
            {
                detail = stack_val.to_rust_string_lossy(scope);
                tracing::error!(stack = %stack_val.to_rust_string_lossy(scope), "v8 evaluate rejected stack");
            }
        }
        return Err(format!("Evaluate rejected: {entrypoint}: {detail}"));
    }

    // Extract the namespace.
    let namespace = {
        let reg = registry.borrow();
        let module_global = reg.compiled.get(entrypoint).unwrap();
        let module = v8::Local::new(scope, module_global);
        let ns = module.get_module_namespace();
        v8::Global::new(scope, ns)
    };

    Ok(namespace)
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
