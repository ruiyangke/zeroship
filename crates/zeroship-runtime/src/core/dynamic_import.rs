//! V8 host callback for dynamic imports.
//!
//! Imports resolve against the compiled module registry, then native module
//! factories and embedded bootstrap sources. Resolved modules are cached so
//! static and dynamic imports share identity. Arbitrary missing creator modules
//! are rejected; this callback does not fetch source code.
//!
//! Evaluation runs in a promise continuation, and the import resolves only
//! after V8's cached evaluation promise settles. Linking and evaluation errors
//! propagate through that promise with their original cause.
//!
//! Installed before creator evaluation by `RuntimeInner::new_with_plugins`.

#![allow(unsafe_code)]

use crate::core::bootstrap_modules;
use crate::core::modules::{self, SharedRegistry};
use crate::core::native_modules;

/// Variants tried for an unknown specifier — same set as the static
/// `resolve_callback` so dynamic and static specifier shapes resolve to
/// the same compiled module.
fn registry_lookup<'s>(
    scope: &v8::PinScope<'s, '_>,
    spec: &str,
) -> Option<v8::Local<'s, v8::Module>> {
    let registry = scope.get_slot::<SharedRegistry>()?.clone();
    let reg = registry.borrow();
    let candidates = [
        spec.to_string(),
        spec.strip_prefix("./").unwrap_or(spec).to_string(),
        format!("{spec}.js"),
        format!("{}.js", spec.strip_prefix("./").unwrap_or(spec)),
    ];
    for candidate in &candidates {
        if let Some(g) = reg.get(candidate) {
            return Some(v8::Local::new(scope, g));
        }
    }
    None
}

/// Cache a freshly-minted native synthetic into the registry under its
/// bare specifier so a subsequent dynamic OR static import of
/// `node:foo` hits the same module record.
fn cache_into_registry<'s>(
    scope: &v8::PinScope<'s, '_>,
    spec: &str,
    module: v8::Local<'s, v8::Module>,
) {
    if let Some(registry) = scope.get_slot::<SharedRegistry>() {
        let reg = registry.clone();
        let g = v8::Global::new(scope, module);
        reg.borrow_mut().insert(spec.to_string(), g);
    }
}

/// Return a namespace only after its module evaluation has settled.
#[expect(clippy::needless_pass_by_value, reason = "V8 callback signature")]
fn evaluated_namespace(
    _scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    rv.set(args.data());
}

/// Runs as a promise continuation, after the importing module has left its
/// synchronous evaluation frame. Reentrant imports can then reuse V8's cached
/// evaluation promise without attempting to evaluate an active sync module.
#[expect(clippy::needless_pass_by_value, reason = "V8 callback signature")]
fn evaluate_import(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let specifier = args.data().to_rust_string_lossy(scope);
    let Some(module) = registry_lookup(scope, &specifier) else {
        let message = v8::String::new(scope, &format!("Cannot find module '{specifier}'")).unwrap();
        let error = v8::Exception::type_error(scope, message);
        scope.throw_exception(error);
        return;
    };
    if module.get_status() == v8::ModuleStatus::Uninstantiated
        && module.instantiate_module(scope, modules::resolve_callback) != Some(true)
    {
        // V8's linking exception rejects the promise running this callback.
        return;
    }
    let Some(value) = module.evaluate(scope) else {
        return;
    };
    let namespace = module.get_module_namespace();
    let Ok(evaluation) = v8::Local::<v8::Promise>::try_from(value) else {
        // Native synthetic evaluators can complete synchronously without a
        // promise. Their exports are already populated at this point.
        rv.set(namespace);
        return;
    };
    let Some(on_fulfilled) = v8::Function::builder(evaluated_namespace)
        .data(namespace)
        .build(scope)
    else {
        return;
    };
    if let Some(result) = evaluation.then(scope, on_fulfilled) {
        rv.set(result.into());
    }
}

fn import_registered<'s>(
    scope: &v8::PinScope<'s, '_>,
    resolver: v8::Local<'s, v8::PromiseResolver>,
    specifier: v8::Local<'s, v8::String>,
) -> Option<v8::Local<'s, v8::Promise>> {
    let callback = v8::Function::builder(evaluate_import)
        .data(specifier.into())
        .build(scope)?;
    let undefined = v8::undefined(scope);
    resolver.resolve(scope, undefined.into());
    resolver.get_promise(scope).then(scope, callback)
}

/// Resolve an embedded bootstrap module through [`bootstrap_modules`].
///
/// The runtime, not the bundle, owns these: it injects the code that
/// imports them (`runtime-entry.js` spliced into the bootstrap `index.js`),
/// so it must guarantee they resolve regardless of what the tree-shaken
/// `.zship` carries (ISS-63).
///
/// Strategy: compile the requested module AND its transitive
/// runtime-provided dependency closure into the per-isolate registry, then
/// instantiate with the SAME [`modules::resolve_callback`] the static graph
/// uses — so the bootstrap module's own `import ... from "@zeroship/db/internal"`
/// / `"zeroship"` lines resolve against the registry we just populated. The
/// requested module is left cached in the registry, so a later static OR
/// dynamic import of the same specifier hits path 1.
///
/// Returns `None` if `spec` isn't a runtime-provided module (caller falls
/// through to the not-found rejection) or if compilation fails (treated as
/// a not-found miss — a compile error here means the embedded dist drifted,
/// which the resolution tests catch).
fn resolve_bootstrap_module<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    spec: &str,
) -> Option<v8::Local<'s, v8::Module>> {
    if !bootstrap_modules::is_bootstrap_module(spec) {
        return None;
    }
    let registry = scope.get_slot::<SharedRegistry>()?.clone();

    // Walk the transitive runtime-provided closure (DFS) and compile each
    // member into the registry if absent. `zeroship` is frequently already
    // present because creator code statically imports it. Compiling a member doesn't evaluate it — that
    // happens during the entry's `instantiate_module` + `evaluate` below,
    // exactly as `load_modules` does for the static graph.
    let mut stack: Vec<&str> = vec![spec];
    let mut seen: Vec<&str> = Vec::new();
    while let Some(cur) = stack.pop() {
        if seen.contains(&cur) {
            continue;
        }
        seen.push(cur);

        let already = registry.borrow().get(cur).is_some();
        if !already {
            let source = bootstrap_modules::source_for(cur)?;
            let module = modules::compile_module(scope, cur, source).ok()?;
            registry.borrow_mut().insert(cur.to_string(), module);
        }
        for dep in bootstrap_modules::deps_of(cur) {
            stack.push(dep);
        }
    }

    // Hand back the requested module. The host callback instantiates it
    // through `modules::resolve_callback`, which now finds every transitive
    // import in the registry.
    let g = registry.borrow().get(spec)?.clone();
    Some(v8::Local::new(scope, &g))
}

/// Reject `resolver` with `TypeError(message)` and return its promise.
fn reject_typeerror<'s>(
    scope: &v8::PinScope<'s, '_>,
    resolver: v8::Local<'s, v8::PromiseResolver>,
    message: &str,
) -> v8::Local<'s, v8::Promise> {
    let msg = v8::String::new(scope, message).unwrap();
    let exc = v8::Exception::type_error(scope, msg);
    let promise = resolver.get_promise(scope);
    resolver.reject(scope, exc);
    promise
}

/// `set_host_import_module_dynamically_callback` target. Returns
/// `None` only on `PromiseResolver::new` failure (stack overflow, OOM)
/// — resolution and evaluation otherwise settle the returned promise.
pub(crate) fn host_import_module_dynamically_callback<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    _host_defined_options: v8::Local<'s, v8::Data>,
    _resource_name: v8::Local<'s, v8::Value>,
    specifier: v8::Local<'s, v8::String>,
    _import_attributes: v8::Local<'s, v8::FixedArray>,
) -> Option<v8::Local<'s, v8::Promise>> {
    let resolver = v8::PromiseResolver::new(scope)?;
    let spec = specifier.to_rust_string_lossy(scope);

    if registry_lookup(scope, &spec).is_some() {
        return import_registered(scope, resolver, specifier);
    }

    if let Some(module) = native_modules::resolve_native(scope, &spec) {
        cache_into_registry(scope, &spec, module);
        return import_registered(scope, resolver, specifier);
    }

    if resolve_bootstrap_module(scope, &spec).is_some() {
        return import_registered(scope, resolver, specifier);
    }

    Some(reject_typeerror(
        scope,
        resolver,
        &format!("Cannot find module '{spec}'"),
    ))
}
