//! V8 host callback for dynamic imports.
//!
//! Imports resolve relative to the importing module against retained bundle
//! sources and plugin adapters, then native factories and the core `zeroship`
//! facade. Dependency graphs compile on demand and share cached module identity
//! with static imports. This callback does not fetch source code.
//!
//! Evaluation runs in a promise continuation, and the import resolves only
//! after V8's cached evaluation promise settles. Linking and evaluation errors
//! propagate through that promise with their original cause.
//!
//! Installed before creator evaluation by `RuntimeInner::new_with_plugins`.

#![allow(unsafe_code)]

use crate::core::modules::{self, SharedRegistry};
use crate::core::native_modules;

/// Look up the canonical name already resolved by the import callback.
fn registry_lookup<'s>(
    scope: &v8::PinScope<'s, '_>,
    spec: &str,
) -> Option<v8::Local<'s, v8::Module>> {
    let registry = scope.get_slot::<SharedRegistry>()?.clone();
    let reg = registry.borrow();
    reg.get(spec).map(|module| v8::Local::new(scope, module))
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

/// Compile dynamic bundle imports through the static resolver, then defer
/// evaluation to a promise continuation. V8 compilation exceptions reject the
/// import without poisoning subsequent module resolution.
pub(crate) fn host_import_module_dynamically_callback<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    _host_defined_options: v8::Local<'s, v8::Data>,
    resource_name: v8::Local<'s, v8::Value>,
    specifier: v8::Local<'s, v8::String>,
    _import_attributes: v8::Local<'s, v8::FixedArray>,
) -> Option<v8::Local<'s, v8::Promise>> {
    let resolver = v8::PromiseResolver::new(scope)?;
    let spec = specifier.to_rust_string_lossy(scope);
    let referrer = resource_name.to_rust_string_lossy(scope);
    v8::tc_scope!(let scope, scope);
    match modules::dynamic_module(scope, &spec, &referrer) {
        Ok(Some(resolved)) => {
            let resolved = v8::String::new(scope, &resolved)?;
            return import_registered(scope, resolver, resolved);
        }
        Ok(None) => {}
        Err(error) => {
            if let Some(exception) = scope.exception() {
                resolver.reject(scope, exception);
                return Some(resolver.get_promise(scope));
            }
            return Some(reject_typeerror(scope, resolver, &error));
        }
    }

    if let Some(module) = native_modules::resolve_native(scope, &spec) {
        cache_into_registry(scope, &spec, module);
        return import_registered(scope, resolver, specifier);
    }

    if spec == "zeroship" {
        // The core facade has no imports. Plugin adapters are already in the
        // registry and own their source delivery independently of this module.
        let module = match modules::compile_module(scope, &spec, crate::init::ZEROSHIP_MODULE_JS) {
            Ok(module) => module,
            Err(error) => {
                if let Some(exception) = scope.exception() {
                    resolver.reject(scope, exception);
                    return Some(resolver.get_promise(scope));
                }
                return Some(reject_typeerror(scope, resolver, &error));
            }
        };
        let module = v8::Local::new(scope, module);
        cache_into_registry(scope, &spec, module);
        return import_registered(scope, resolver, specifier);
    }

    Some(reject_typeerror(
        scope,
        resolver,
        &format!("Cannot find module '{spec}'"),
    ))
}
