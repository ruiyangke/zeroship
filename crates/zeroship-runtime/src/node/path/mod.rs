//! Native `node:path` (POSIX).
//!
//! npm packages reach for `path.join` / `path.resolve` everywhere —
//! file-extension routing, asset URL building, dynamic require shims.
//! unenv@2 ships a `node/path` shim, but it's a thin wrapper that costs
//! a fetchModule + ModuleRunner eval per import in dev. Hoisting the
//! whole module into a V8 SyntheticModule kills that round trip and
//! lets the prod bundle keep the bare `import { join } from "node:path"`.
//!
//! ## POSIX-only
//!
//! The runtime targets Linux V8 — there's no Win32 filesystem, no
//! drive letters, no backslash separators. `path.win32` installs as a
//! Proxy that throws on any method call (a generic
//! `ERR_METHOD_NOT_IMPLEMENTED`); `sep` / `delimiter` are still
//! readable since some npm packages probe them defensively. Real
//! Win32 semantics would mean carrying ~600 LOC of UNC + drive-letter
//! handling that no creator app ever needs.
//!
//! ## Implementation
//!
//! All ops are pure string manipulation — no FFI, no syscalls. The
//! code lives in [`PATH_JS`] (an `include_str!`-d JS file) and runs as
//! a single IIFE during synthetic-module evaluation. The IIFE returns
//! an object whose properties become the module's named exports +
//! default. Algorithm shape mirrors Node's `lib/path.js` POSIX branch
//! and Deno's `std/path/posix`; semantics follow the Node 22 spec.

#![allow(unsafe_code)]

const PATH_JS: &str = include_str!("path.js");

/// Mint a synthetic ESM record for `node:path`. Called from
/// `core::native_modules::resolve_native`.
pub fn synthetic_module<'s>(
    scope: &mut v8::PinScope<'s, '_>,
) -> v8::Local<'s, v8::Module> {
    let module_name = v8::String::new(scope, "node:path").unwrap();
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

    // Compile + run the JS IIFE — returns the namespace object.
    let src = v8::String::new(scope, PATH_JS).unwrap();
    let script = v8::Script::compile(scope, src, None).unwrap();
    let ns_val = script.run(scope).unwrap();
    let ns: v8::Local<v8::Object> = ns_val.try_into().unwrap();

    // Mirror named properties into module exports, plus default.
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

/// All exported names. Mirrors Node's `path` module surface — drop
/// none even if some are property reads (`sep`, `delimiter`,
/// `posix`, `win32`); ESM static analyzers complain about missing
/// named exports.
fn export_names() -> &'static [&'static str] {
    &[
        "sep",
        "delimiter",
        "resolve",
        "normalize",
        "isAbsolute",
        "join",
        "relative",
        "dirname",
        "basename",
        "extname",
        "format",
        "parse",
        "posix",
        "win32",
        "default",
    ]
}
