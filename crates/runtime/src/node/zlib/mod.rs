//! Native `node:zlib`.
//!
//! Adds zlib compression and decompression to the runtime in the same
//! `SyntheticModule` shape used for `node:async_hooks` and
//! `node:crypto`.
//!
//! ## Surface
//!
//! Sync + async one-shot functions for the four formats Node ships:
//! gzip (RFC 1952), zlib-wrapped DEFLATE (RFC 1950, exposed as
//! `deflate` / `inflate`), raw DEFLATE (RFC 1951, `deflateRaw` /
//! `inflateRaw`), and Brotli (RFC 7932).
//!
//! Stream constructors (`createGzip`, `createGunzip`, ...) require
//! Node Streams interop and are deferred — they install as throwing
//! stubs that point users at `gzipSync` / `CompressionStream`.
//!
//! ## Backend
//!
//! Reuses `flate2` + `brotli` already linked in for the WHATWG
//! `CompressionStream` codec. The one-shot path lives in
//! [`compress`] — it's a thin shim over `flate2`'s `read::*Decoder`
//! and `write::*Encoder` types.

#![allow(unsafe_code)]

mod compress;

use crate::node::crypto::buffer;
use crate::node::crypto::random_callback_helpers::schedule_node_cb;

/// Mint a synthetic ESM record for `node:zlib`. Called from
/// `core::native_modules::resolve_native`.
pub fn synthetic_module<'s>(
    scope: &mut v8::PinScope<'s, '_>,
) -> v8::Local<'s, v8::Module> {
    let module_name = v8::String::new(scope, "node:zlib").unwrap();
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

    // Build a namespace object then mirror into named exports + default.
    let ns = v8::Object::new(scope);
    populate(scope, ns);

    for name in export_names() {
        if *name == "default" { continue; }
        let key = v8::String::new(scope, name).unwrap();
        let val = ns.get(scope, key.into()).unwrap_or_else(|| v8::undefined(scope).into());
        let _ = module.set_synthetic_module_export(scope, key, val);
    }
    let default_key = v8::String::new(scope, "default").unwrap();
    let _ = module.set_synthetic_module_export(scope, default_key, ns.into());

    Some(v8::undefined(scope).into())
}

fn export_names() -> &'static [&'static str] {
    &[
        // gzip / gunzip
        "gzip", "gzipSync", "gunzip", "gunzipSync",
        // zlib (DEFLATE w/ wrapper)
        "deflate", "deflateSync", "inflate", "inflateSync",
        // raw DEFLATE
        "deflateRaw", "deflateRawSync", "inflateRaw", "inflateRawSync",
        // Brotli
        "brotliCompress", "brotliCompressSync", "brotliDecompress", "brotliDecompressSync",
        // Stream constructors — stubbed (throw) until Node Streams lands.
        "createGzip", "createGunzip", "createDeflate", "createInflate",
        "createDeflateRaw", "createInflateRaw",
        "createBrotliCompress", "createBrotliDecompress",
        // constants
        "constants",
        "default",
    ]
}

fn populate<'s>(scope: &mut v8::PinScope<'s, '_>, obj: v8::Local<v8::Object>) {
    // Sync one-shots.
    set_fn(scope, obj, "gzipSync", op_gzip_sync);
    set_fn(scope, obj, "gunzipSync", op_gunzip_sync);
    set_fn(scope, obj, "deflateSync", op_deflate_sync);
    set_fn(scope, obj, "inflateSync", op_inflate_sync);
    set_fn(scope, obj, "deflateRawSync", op_deflate_raw_sync);
    set_fn(scope, obj, "inflateRawSync", op_inflate_raw_sync);
    set_fn(scope, obj, "brotliCompressSync", op_brotli_compress_sync);
    set_fn(scope, obj, "brotliDecompressSync", op_brotli_decompress_sync);

    // Async (callback) variants. We run the codec inline on the V8
    // thread and fire `cb(null, buf)` via a microtask — the same shape
    // `crypto.pbkdf2` and `randomBytes` use. A future change can route
    // large inputs to a `spawn_blocking` pool.
    set_fn(scope, obj, "gzip", op_gzip);
    set_fn(scope, obj, "gunzip", op_gunzip);
    set_fn(scope, obj, "deflate", op_deflate);
    set_fn(scope, obj, "inflate", op_inflate);
    set_fn(scope, obj, "deflateRaw", op_deflate_raw);
    set_fn(scope, obj, "inflateRaw", op_inflate_raw);
    set_fn(scope, obj, "brotliCompress", op_brotli_compress);
    set_fn(scope, obj, "brotliDecompress", op_brotli_decompress);

    // Stream constructors — throw "not implemented" with a pointer at
    // the sync / WHATWG alternatives. Per-namespace scoped error
    // messages so npm tracebacks make sense.
    install_stream_stub(scope, obj, "createGzip");
    install_stream_stub(scope, obj, "createGunzip");
    install_stream_stub(scope, obj, "createDeflate");
    install_stream_stub(scope, obj, "createInflate");
    install_stream_stub(scope, obj, "createDeflateRaw");
    install_stream_stub(scope, obj, "createInflateRaw");
    install_stream_stub(scope, obj, "createBrotliCompress");
    install_stream_stub(scope, obj, "createBrotliDecompress");

    // `constants` — Node ships ~80 of these; npm packages mostly probe
    // a handful (Z_NO_FLUSH, Z_DEFAULT_COMPRESSION, ...). Cover the
    // ones that show up in real code.
    let constants = v8::Object::new(scope);
    let pairs: &[(&str, i32)] = &[
        // Flush values
        ("Z_NO_FLUSH", 0),
        ("Z_PARTIAL_FLUSH", 1),
        ("Z_SYNC_FLUSH", 2),
        ("Z_FULL_FLUSH", 3),
        ("Z_FINISH", 4),
        ("Z_BLOCK", 5),
        ("Z_TREES", 6),
        // Return codes
        ("Z_OK", 0),
        ("Z_STREAM_END", 1),
        ("Z_NEED_DICT", 2),
        ("Z_ERRNO", -1),
        ("Z_STREAM_ERROR", -2),
        ("Z_DATA_ERROR", -3),
        ("Z_MEM_ERROR", -4),
        ("Z_BUF_ERROR", -5),
        ("Z_VERSION_ERROR", -6),
        // Compression levels
        ("Z_NO_COMPRESSION", 0),
        ("Z_BEST_SPEED", 1),
        ("Z_BEST_COMPRESSION", 9),
        ("Z_DEFAULT_COMPRESSION", -1),
        // Strategies
        ("Z_FILTERED", 1),
        ("Z_HUFFMAN_ONLY", 2),
        ("Z_RLE", 3),
        ("Z_FIXED", 4),
        ("Z_DEFAULT_STRATEGY", 0),
        // Brotli mode / quality
        ("BROTLI_OPERATION_PROCESS", 0),
        ("BROTLI_OPERATION_FLUSH", 1),
        ("BROTLI_OPERATION_FINISH", 2),
        ("BROTLI_PARAM_MODE", 0),
        ("BROTLI_MODE_GENERIC", 0),
        ("BROTLI_MODE_TEXT", 1),
        ("BROTLI_MODE_FONT", 2),
        ("BROTLI_PARAM_QUALITY", 1),
        ("BROTLI_MIN_QUALITY", 0),
        ("BROTLI_MAX_QUALITY", 11),
        ("BROTLI_PARAM_LGWIN", 3),
        ("BROTLI_MIN_WINDOW_BITS", 10),
        ("BROTLI_MAX_WINDOW_BITS", 24),
    ];
    for (name, val) in pairs.iter() {
        let k = v8::String::new(scope, name).unwrap();
        let v = v8::Integer::new(scope, *val);
        constants.set(scope, k.into(), v.into());
    }
    let constants_k = v8::String::new(scope, "constants").unwrap();
    obj.set(scope, constants_k.into(), constants.into());
}

fn set_fn<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    obj: v8::Local<v8::Object>,
    name: &str,
    callback: impl v8::MapFnTo<v8::FunctionCallback>,
) {
    let f = v8::Function::new(scope, callback).unwrap();
    let k = v8::String::new(scope, name).unwrap();
    obj.set(scope, k.into(), f.into());
}

fn install_stream_stub<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    obj: v8::Local<v8::Object>,
    name: &'static str,
) {
    // Inline a tiny JS thunk — no Rust state, no FFI overhead. The
    // message points at the in-tree alternatives so the AI builder /
    // npm package author has a clear next step.
    let src = format!(
        r#"(function {name}() {{ throw Object.assign(new Error("node:zlib {name} is not yet implemented; use {name}Sync or CompressionStream from the WHATWG streams API."), {{ code: "ERR_METHOD_NOT_IMPLEMENTED" }}); }})"#
    );
    let v = v8::String::new(scope, &src).unwrap();
    let script = v8::Script::compile(scope, v, None).unwrap();
    let fn_val = script.run(scope).unwrap();
    let k = v8::String::new(scope, name).unwrap();
    obj.set(scope, k.into(), fn_val);
}

// ---------------------------------------------------------------------------
// Argument parsing
// ---------------------------------------------------------------------------

/// Pull bytes out of the first arg. Accepts Buffer / Uint8Array /
/// ArrayBuffer / string (utf-8). String is the Node convention for
/// shorthand inputs.
fn read_input(
    scope: &mut v8::PinScope,
    value: v8::Local<v8::Value>,
) -> Result<Vec<u8>, &'static str> {
    if let Ok(view) = v8::Local::<v8::ArrayBufferView>::try_from(value) {
        let mut buf = vec![0u8; view.byte_length()];
        view.copy_contents(&mut buf);
        return Ok(buf);
    }
    if let Ok(ab) = v8::Local::<v8::ArrayBuffer>::try_from(value) {
        let store = ab.get_backing_store();
        let mut buf = vec![0u8; ab.byte_length()];
        for (i, b) in buf.iter_mut().enumerate() {
            *b = store[i].get();
        }
        return Ok(buf);
    }
    if value.is_string() {
        let s = value.to_rust_string_lossy(scope);
        return Ok(s.into_bytes());
    }
    Err("input must be a Buffer, TypedArray, ArrayBuffer, or string")
}

/// Read `level` from an optional options object. Defaults to flate2's
/// `Compression::default()` (level 6) — same as Node's
/// `Z_DEFAULT_COMPRESSION` resolved value.
fn read_level(scope: &mut v8::PinScope, value: v8::Local<v8::Value>) -> u32 {
    if !value.is_object() || value.is_null_or_undefined() {
        return 6;
    }
    let Ok(opts) = v8::Local::<v8::Object>::try_from(value) else { return 6 };
    let key = v8::String::new(scope, "level").unwrap();
    let Some(v) = opts.get(scope, key.into()) else { return 6 };
    if v.is_undefined() || v.is_null() { return 6; }
    let n = v.number_value(scope).unwrap_or(6.0);
    // Node accepts -1 (Z_DEFAULT_COMPRESSION) and 0..=9. Clamp out of
    // range; flate2's `Compression::new` itself accepts 0..=9.
    if !n.is_finite() || n < 0.0 { 6 } else if n > 9.0 { 9 } else { n as u32 }
}

/// Sync `(input, [opts])` parse. Reads bytes + level only; callbacks
/// are pulled out separately by `parse_async_args` to keep the
/// V8-callback lifetimes simple.
fn parse_sync_args(
    scope: &mut v8::PinScope,
    args: &v8::FunctionCallbackArguments,
) -> Result<(Vec<u8>, u32), String> {
    if args.length() < 1 {
        return Err("input is required".into());
    }
    let input = read_input(scope, args.get(0)).map_err(|s| s.to_string())?;
    let level = if args.length() >= 2 {
        read_level(scope, args.get(1))
    } else {
        6
    };
    Ok((input, level))
}

/// Async `(input, [opts], cb)` parse. Returns bytes + level + the
/// callback as a `v8::Global` so the caller can drop the args borrow
/// before scheduling.
fn parse_async_args<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    args: &v8::FunctionCallbackArguments<'s>,
) -> Result<(Vec<u8>, u32, v8::Local<'s, v8::Function>), String> {
    let n = args.length();
    if n < 1 {
        return Err("input is required".into());
    }
    let input = read_input(scope, args.get(0)).map_err(|s| s.to_string())?;

    let (level, cb_val) = match n {
        0 => unreachable!(),
        1 => return Err("callback is required".into()),
        2 => (6u32, args.get(1)),
        _ => (read_level(scope, args.get(1)), args.get(n - 1)),
    };
    if !cb_val.is_function() {
        return Err("callback must be a function".into());
    }
    Ok((input, level, cb_val.try_into().unwrap()))
}

// ---------------------------------------------------------------------------
// Sync ops
// ---------------------------------------------------------------------------

fn run_sync<F>(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
    op: F,
) where
    F: FnOnce(&[u8], u32) -> Result<Vec<u8>, String>,
{
    let (input, level) = match parse_sync_args(scope, &args) {
        Ok(p) => p,
        Err(msg) => {
            let exc = crate::node_error::build_node_exception(
                scope, "ERR_INVALID_ARG_TYPE", &msg,
            );
            scope.throw_exception(exc);
            return;
        }
    };
    match op(&input, level) {
        Ok(out) => {
            rv.set(buffer::emit_buffer(scope, &out));
        }
        Err(e) => {
            let exc = crate::node_error::build_node_exception(
                scope, "Z_DATA_ERROR", &e,
            );
            scope.throw_exception(exc);
        }
    }
}

fn op_gzip_sync(scope: &mut v8::PinScope, args: v8::FunctionCallbackArguments, rv: v8::ReturnValue) {
    run_sync(scope, args, rv, compress::gzip);
}
fn op_gunzip_sync(scope: &mut v8::PinScope, args: v8::FunctionCallbackArguments, rv: v8::ReturnValue) {
    run_sync(scope, args, rv, |b, _| compress::gunzip(b));
}
fn op_deflate_sync(scope: &mut v8::PinScope, args: v8::FunctionCallbackArguments, rv: v8::ReturnValue) {
    run_sync(scope, args, rv, compress::deflate);
}
fn op_inflate_sync(scope: &mut v8::PinScope, args: v8::FunctionCallbackArguments, rv: v8::ReturnValue) {
    run_sync(scope, args, rv, |b, _| compress::inflate(b));
}
fn op_deflate_raw_sync(scope: &mut v8::PinScope, args: v8::FunctionCallbackArguments, rv: v8::ReturnValue) {
    run_sync(scope, args, rv, compress::deflate_raw);
}
fn op_inflate_raw_sync(scope: &mut v8::PinScope, args: v8::FunctionCallbackArguments, rv: v8::ReturnValue) {
    run_sync(scope, args, rv, |b, _| compress::inflate_raw(b));
}
fn op_brotli_compress_sync(scope: &mut v8::PinScope, args: v8::FunctionCallbackArguments, rv: v8::ReturnValue) {
    run_sync(scope, args, rv, |b, _| compress::brotli_compress(b));
}
fn op_brotli_decompress_sync(scope: &mut v8::PinScope, args: v8::FunctionCallbackArguments, rv: v8::ReturnValue) {
    run_sync(scope, args, rv, |b, _| compress::brotli_decompress(b));
}

// ---------------------------------------------------------------------------
// Async (callback) ops
// ---------------------------------------------------------------------------

fn run_async<'s, F>(
    scope: &mut v8::PinScope<'s, '_>,
    args: v8::FunctionCallbackArguments<'s>,
    op: F,
) where
    F: FnOnce(&[u8], u32) -> Result<Vec<u8>, String>,
{
    let (input, level, cb) = match parse_async_args(scope, &args) {
        Ok(p) => p,
        Err(msg) => {
            let exc = crate::node_error::build_node_exception(
                scope, "ERR_INVALID_ARG_TYPE", &msg,
            );
            scope.throw_exception(exc);
            return;
        }
    };
    match op(&input, level) {
        Ok(out) => {
            let buf = buffer::emit_buffer(scope, &out);
            schedule_node_cb(scope, cb, None, Some(buf));
        }
        Err(e) => {
            let err = crate::node_error::build_node_exception(scope, "Z_DATA_ERROR", &e);
            schedule_node_cb(scope, cb, Some(err), None);
        }
    }
}

fn op_gzip<'s>(scope: &mut v8::PinScope<'s, '_>, args: v8::FunctionCallbackArguments<'s>, _rv: v8::ReturnValue) {
    run_async(scope, args, compress::gzip);
}
fn op_gunzip<'s>(scope: &mut v8::PinScope<'s, '_>, args: v8::FunctionCallbackArguments<'s>, _rv: v8::ReturnValue) {
    run_async(scope, args, |b, _| compress::gunzip(b));
}
fn op_deflate<'s>(scope: &mut v8::PinScope<'s, '_>, args: v8::FunctionCallbackArguments<'s>, _rv: v8::ReturnValue) {
    run_async(scope, args, compress::deflate);
}
fn op_inflate<'s>(scope: &mut v8::PinScope<'s, '_>, args: v8::FunctionCallbackArguments<'s>, _rv: v8::ReturnValue) {
    run_async(scope, args, |b, _| compress::inflate(b));
}
fn op_deflate_raw<'s>(scope: &mut v8::PinScope<'s, '_>, args: v8::FunctionCallbackArguments<'s>, _rv: v8::ReturnValue) {
    run_async(scope, args, compress::deflate_raw);
}
fn op_inflate_raw<'s>(scope: &mut v8::PinScope<'s, '_>, args: v8::FunctionCallbackArguments<'s>, _rv: v8::ReturnValue) {
    run_async(scope, args, |b, _| compress::inflate_raw(b));
}
fn op_brotli_compress<'s>(scope: &mut v8::PinScope<'s, '_>, args: v8::FunctionCallbackArguments<'s>, _rv: v8::ReturnValue) {
    run_async(scope, args, |b, _| compress::brotli_compress(b));
}
fn op_brotli_decompress<'s>(scope: &mut v8::PinScope<'s, '_>, args: v8::FunctionCallbackArguments<'s>, _rv: v8::ReturnValue) {
    run_async(scope, args, |b, _| compress::brotli_decompress(b));
}
