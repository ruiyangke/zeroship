//! Minimal native synthetic `node:events`.
//!
//! The implementation is JS because EventEmitter is object bookkeeping,
//! not a kernel primitive. `node:net` calls [`ensure_event_emitter`] so a
//! direct `import "node:net"` works even when `node:events` has not been
//! imported first.

#![allow(unsafe_code)]

const EVENTS_JS: &str = include_str!("events.js");

pub fn synthetic_module<'s>(
    scope: &mut v8::PinScope<'s, '_>,
) -> v8::Local<'s, v8::Module> {
    let module_name = v8::String::new(scope, "node:events").unwrap();
    let names = export_names();
    let export_strings: Vec<v8::Local<v8::String>> = names
        .iter()
        .map(|n| v8::String::new(scope, n).unwrap())
        .collect();
    v8::Module::create_synthetic_module(scope, module_name, &export_strings, evaluate)
}

pub(crate) fn ensure_event_emitter<'s>(
    scope: &mut v8::PinScope<'s, '_>,
) -> Option<v8::Local<'s, v8::Object>> {
    let context = scope.get_current_context();
    let global = context.global(scope);
    let key = v8::String::new(scope, "__zsEventsNs").unwrap();
    if let Some(existing) = global.get(scope, key.into()) {
        if existing.is_object() {
            return v8::Local::<v8::Object>::try_from(existing).ok();
        }
    }

    let src = v8::String::new(scope, EVENTS_JS)?;
    let script = v8::Script::compile(scope, src, None)?;
    let result = script.run(scope)?;
    let ns = v8::Local::<v8::Object>::try_from(result).ok()?;
    global.set(scope, key.into(), ns.into());

    let emitter_key = v8::String::new(scope, "__zsEventEmitter").unwrap();
    let ctor_key = v8::String::new(scope, "EventEmitter").unwrap();
    if let Some(ctor) = ns.get(scope, ctor_key.into()) {
        global.set(scope, emitter_key.into(), ctor);
    }
    Some(ns)
}

fn evaluate<'s>(
    context: v8::Local<'s, v8::Context>,
    module: v8::Local<'s, v8::Module>,
) -> Option<v8::Local<'s, v8::Value>> {
    v8::callback_scope!(unsafe scope, context);

    let ns = ensure_event_emitter(scope)?;
    for name in export_names() {
        if *name == "default" {
            continue;
        }
        let key = v8::String::new(scope, name).unwrap();
        let val = ns
            .get(scope, key.into())
            .unwrap_or_else(|| v8::undefined(scope).into());
        let _ = module.set_synthetic_module_export(scope, key, val);
    }

    let default_key = v8::String::new(scope, "default").unwrap();
    let _ = module.set_synthetic_module_export(scope, default_key, ns.into());
    Some(v8::undefined(scope).into())
}

fn export_names() -> &'static [&'static str] {
    &["EventEmitter", "default"]
}
