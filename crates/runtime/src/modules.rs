//! V8 ESM module registry — lazy compilation via import graph discovery.
//!
//! Only the entry module is compiled eagerly. Its imports are discovered via
//! `v8::Module::get_module_requests()`, compiled, and their imports discovered
//! recursively — all BEFORE `instantiate_module` is called. The resolve
//! callback only does lookups into the pre-compiled registry.
//!
//! This means: if an app has 1000 modules but the entry only imports 3
//! (transitively), only 4 modules are compiled. The other 996 are never
//! parsed by V8.

#![allow(unsafe_code)]

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;

pub use zeroship_bundle::ModuleEntry;

/// Module registry stored in V8 isolate slot.
pub struct ModuleRegistry {
    /// Compiled V8 modules — populated by the lazy compilation loop.
    compiled: HashMap<String, v8::Global<v8::Module>>,
}

pub type SharedRegistry = Rc<RefCell<ModuleRegistry>>;

impl ModuleRegistry {
    pub fn new() -> Self {
        Self {
            compiled: HashMap::new(),
        }
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
fn compile_module(
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
/// but only transitively imported modules are compiled.
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

    let registry: SharedRegistry = Rc::new(RefCell::new(ModuleRegistry::new()));

    // Phase 1: Compile entry module
    let entrypoint = &entries[0].specifier;
    let entry_source = sources.get(entrypoint)
        .ok_or_else(|| format!("Entrypoint not found: {entrypoint}"))?;
    let entry_module = compile_module(scope, entrypoint, entry_source)?;

    // Phase 2: Discover and compile all transitive imports (BFS)
    //
    // V8's resolve_callback can't compile modules — it must return
    // an already-compiled module. So we walk the import graph here,
    // compiling each discovered module BEFORE calling instantiate_module.
    {
        let mut queue: VecDeque<(String, v8::Global<v8::Module>)> = VecDeque::new();
        queue.push_back((entrypoint.clone(), entry_module));

        while let Some((spec, module_global)) = queue.pop_front() {
            // Store compiled module in registry
            let already_registered = registry.borrow().compiled.contains_key(&spec);
            if !already_registered {
                registry.borrow_mut().compiled.insert(spec.clone(), module_global.clone());
            }

            // Discover this module's imports via V8
            let module_local = v8::Local::new(scope, &module_global);
            let requests = module_local.get_module_requests();
            let num_requests = requests.length();

            for i in 0..num_requests {
                let request = v8::Local::<v8::ModuleRequest>::try_from(
                    requests.get(scope, i).unwrap()
                ).unwrap();
                let import_specifier = request.get_specifier().to_rust_string_lossy(scope);

                // Resolve to actual source specifier
                let resolved = match resolve_specifier(&import_specifier, &sources) {
                    Some(s) => s,
                    None => return Err(format!(
                        "Cannot resolve import '{import_specifier}' from '{spec}'"
                    )),
                };

                // Skip if already compiled
                if registry.borrow().compiled.contains_key(&resolved) {
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

    // Phase 3: Instantiate entrypoint
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

    // Phase 4: Evaluate
    let eval_rejection: Option<v8::Global<v8::Value>> = {
        let reg = registry.borrow();
        let module_global = reg.compiled.get(entrypoint).unwrap();
        let module = v8::Local::new(scope, module_global);

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

        scope.perform_microtask_checkpoint();

        if let Some(exc) = sync_exc {
            let local = v8::Local::new(scope, &exc);
            eprintln!("[v8] evaluate sync threw: {}", local.to_rust_string_lossy(scope));
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
        eprintln!("[v8] evaluate rejected: {}", local.to_rust_string_lossy(scope));
        if let Some(obj) = local.to_object(scope) {
            let stack_key = v8::String::new(scope, "stack").unwrap();
            if let Some(stack_val) = obj.get(scope, stack_key.into()) {
                if !stack_val.is_undefined() {
                    eprintln!("[v8] stack: {}", stack_val.to_rust_string_lossy(scope));
                }
            }
        }
        return Err(format!("Evaluate rejected: {entrypoint}"));
    }

    // Phase 5: Extract namespace
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
/// All transitively imported modules are pre-compiled in Phase 2.
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

    eprintln!("[modules] Cannot resolve: {spec}");
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
