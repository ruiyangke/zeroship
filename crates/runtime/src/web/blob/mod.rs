//! Native `Blob` and `File` implementations per WHATWG File API.
//!
//! See `blob.rs` and `file.rs` for the per-class details. This module
//! root only exposes the install entry point used by `init.rs::setup_globals`.

pub mod blob;
pub mod file;

pub use blob::Blob;
pub use file::File;

/// Install `globalThis.Blob` and `globalThis.File`, wiring File's
/// prototype to inherit from Blob.prototype so `file instanceof Blob`
/// is true and Blob methods inherited via the chain still work.
///
/// Must be called AFTER `install_native_streams` because `Blob.stream()`
/// calls `globalThis.ReadableStream`. The `init.rs::load_polyfills_and_modules`
/// flow already orders streams before Blob.
pub fn install_globals(scope: &mut v8::PinScope, global: v8::Local<v8::Object>) {
    // ----- Install Blob -----
    //
    // #198 — left as direct install/bind because File below needs the
    // resolved `blob_class_fn` (for `file_class_fn.set_prototype(blob)`),
    // and `register_native_classes!` doesn't return the bound Function.
    // Re-fetching via `global.get(...)` after registration would just be
    // strictly more code, so keep the manual sequence here.
    let blob_tmpl = Blob::install(scope);
    let blob_class_fn = blob_tmpl.get_function(scope).unwrap();
    let blob_key = v8::String::new(scope, "Blob").unwrap();
    global.set(scope, blob_key.into(), blob_class_fn.into());

    // ----- Install File -----
    let file_tmpl = File::install(scope);
    let file_class_fn = file_tmpl.get_function(scope).unwrap();

    // Wire File.prototype's [[Prototype]] to Blob.prototype so:
    //   - file instanceof Blob === true
    //   - inherited Blob methods are reachable via the chain
    // Even though we override the inherited methods on File.prototype
    // directly, the prototype link is still spec-required (so e.g.
    // `Object.getPrototypeOf(File.prototype) === Blob.prototype`).
    let proto_key = v8::String::new(scope, "prototype").unwrap();
    let blob_proto_v = blob_class_fn.get(scope, proto_key.into()).unwrap();
    let file_proto_v = file_class_fn.get(scope, proto_key.into()).unwrap();
    if let Ok(file_proto) = v8::Local::<v8::Object>::try_from(file_proto_v) {
        file_proto.set_prototype(scope, blob_proto_v);
    }

    // Wire File class function's [[Prototype]] to Blob class function so
    // `Object.getPrototypeOf(File) === Blob` matches WebIDL §3.7's
    // class-of-class chain. This is what makes `class Foo extends File`
    // also extend Blob.
    file_class_fn.set_prototype(scope, blob_class_fn.into());

    let file_key = v8::String::new(scope, "File").unwrap();
    global.set(scope, file_key.into(), file_class_fn.into());
}
