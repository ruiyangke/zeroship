//! V8 ESM module registry — loads pre-resolved modules into V8's native module system.
//!
//! The bundler (SWC/esbuild) resolves all imports at build time and produces
//! a set of modules with resolved specifiers. The registry loads them into V8
//! and lets V8 handle the module graph.

#![allow(unsafe_code)]

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

/// A pre-resolved module to be loaded into V8.
#[derive(Debug, Clone)]
pub struct ModuleEntry {
    pub specifier: String,
    pub source: String,
}

/// Internal compiled module.
struct CompiledModule {
    module: v8::Global<v8::Module>,
}

/// Module registry stored in V8 isolate slot.
pub(crate) struct ModuleRegistry {
    modules: HashMap<String, CompiledModule>,
}

pub(crate) type SharedRegistry = Rc<RefCell<ModuleRegistry>>;

impl ModuleRegistry {
    pub(crate) fn new() -> Self {
        Self {
            modules: HashMap::new(),
        }
    }
}

/// Compile and register all modules, instantiate and evaluate the entrypoint.
///
/// `entries[0]` is the entrypoint. All others are dependencies.
/// Returns the entrypoint module's namespace object (contains the exports).
pub(crate) fn load_modules(
    scope: &mut v8::PinScope,
    entries: &[ModuleEntry],
) -> Result<v8::Global<v8::Value>, String> {
    if entries.is_empty() {
        return Err("No modules to load".into());
    }

    let registry: SharedRegistry = Rc::new(RefCell::new(ModuleRegistry::new()));

    // Phase 1: Compile all modules and register in the registry
    for entry in entries {
        let source_str = v8::String::new(scope, &entry.source)
            .ok_or_else(|| format!("Failed to create source for {}", entry.specifier))?;

        let name_str = v8::String::new(scope, &entry.specifier)
            .ok_or_else(|| format!("Failed to create name for {}", entry.specifier))?;

        let origin = v8::ScriptOrigin::new(
            scope,
            name_str.into(),
            0, 0, false, -1, None, false, false,
            true, // is_module = true
            None,
        );

        let mut v8_source = v8::script_compiler::Source::new(source_str, Some(&origin));

        let module = v8::script_compiler::compile_module(scope, &mut v8_source)
            .ok_or_else(|| format!("Failed to compile module: {}", entry.specifier))?;

        let global = v8::Global::new(scope, module);
        registry.borrow_mut().modules.insert(
            entry.specifier.clone(),
            CompiledModule { module: global },
        );
    }

    // Store registry in isolate slot for the resolve callback
    scope.set_slot(registry.clone());

    // Phase 2: Instantiate entrypoint (V8 walks import graph via callback)
    let entrypoint = &entries[0].specifier;
    {
        let reg = registry.borrow();
        let cm = reg.modules.get(entrypoint)
            .ok_or_else(|| format!("Entrypoint not found: {entrypoint}"))?;
        let module = v8::Local::new(scope, &cm.module);

        let ok = module.instantiate_module(scope, resolve_callback);
        if ok.is_none() || ok == Some(false) {
            return Err(format!("Failed to instantiate: {entrypoint}"));
        }
    }

    // Phase 3: Evaluate
    {
        let reg = registry.borrow();
        let cm = reg.modules.get(entrypoint).unwrap();
        let module = v8::Local::new(scope, &cm.module);

        let result = module.evaluate(scope);
        if result.is_none() {
            return Err(format!("Failed to evaluate: {entrypoint}"));
        }
        // Drain microtasks (for top-level await)
        scope.perform_microtask_checkpoint();
    }

    // Phase 4: Extract entrypoint namespace (contains the module's exports)
    let namespace = {
        let reg = registry.borrow();
        let cm = reg.modules.get(entrypoint).unwrap();
        let module = v8::Local::new(scope, &cm.module);
        let ns = module.get_module_namespace();
        v8::Global::new(scope, ns)
    };

    Ok(namespace)
}

/// V8 resolve callback — called when V8 encounters `import ... from '...'`.
fn resolve_callback<'a>(
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

    let reg = registry.borrow();

    // Try exact, then ./stripped, then with .js
    let candidates = [
        spec.clone(),
        spec.strip_prefix("./").unwrap_or(&spec).to_string(),
        format!("{spec}.js"),
        format!("{}.js", spec.strip_prefix("./").unwrap_or(&spec)),
    ];

    for candidate in &candidates {
        if let Some(cm) = reg.modules.get(candidate) {
            return Some(v8::Local::new(scope, &cm.module));
        }
    }

    eprintln!("[modules] Cannot resolve: {spec}");
    None
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::init_v8;

    fn run_modules(entries: &[ModuleEntry]) -> Result<v8::OwnedIsolate, String> {
        init_v8();
        let params = v8::CreateParams::default();
        let mut isolate = v8::Isolate::new(params);

        {
            v8::scope!(let handle_scope, &mut isolate);
            let context = v8::Context::new(handle_scope, Default::default());
            let scope = &mut v8::ContextScope::new(handle_scope, context);
            let _namespace = load_modules(scope, entries)?;
        }

        Ok(isolate)
    }

    fn get_global(isolate: &mut v8::OwnedIsolate, key: &str) -> String {
        v8::scope!(let handle_scope, isolate);
        let context = v8::Context::new(handle_scope, Default::default());
        let scope = &mut v8::ContextScope::new(handle_scope, context);
        let global = scope.get_current_context().global(scope);
        let k = v8::String::new(scope, key).unwrap();
        match global.get(scope, k.into()) {
            Some(v) => v.to_rust_string_lossy(scope),
            None => "undefined".to_string(),
        }
    }

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
        // Two modules import the same dependency
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
        assert_eq!(val.int32_value(scope).unwrap(), 30); // 10 + 10*2
    }
}
