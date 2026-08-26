//! Native synthetic `node:tls` over compio_tls.

#![allow(unsafe_code)]
#![cfg(feature = "runtime_tls")]

const TLS_JS: &str = include_str!("tls.js");

pub fn synthetic_module<'s>(
    scope: &mut v8::PinScope<'s, '_>,
) -> v8::Local<'s, v8::Module> {
    let module_name = v8::String::new(scope, "node:tls").unwrap();
    let names = export_names();
    let export_strings: Vec<v8::Local<v8::String>> = names
        .iter()
        .map(|n| v8::String::new(scope, n).unwrap())
        .collect();
    v8::Module::create_synthetic_module(scope, module_name, &export_strings, evaluate)
}

fn evaluate<'s>(
    context: v8::Local<'s, v8::Context>,
    module: v8::Local<'s, v8::Module>,
) -> Option<v8::Local<'s, v8::Value>> {
    v8::callback_scope!(unsafe scope, context);

    crate::node::net::ensure_net_namespace(scope)?;

    let src = v8::String::new(scope, TLS_JS)?;
    let script = v8::Script::compile(scope, src, None)?;
    let ns_val = script.run(scope)?;
    let ns = v8::Local::<v8::Object>::try_from(ns_val).ok()?;

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
    &["TLSSocket", "connect", "createSecureContext", "default"]
}
