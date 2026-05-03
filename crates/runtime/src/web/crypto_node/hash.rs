//! `Hash` class — `crypto.createHash(algorithm, options?)`.
//!
//! Per `docs/proposals/node-crypto-native.md` §V.2 (D-N9). Exposes:
//! - `update(data, inputEncoding?)` returns `this` for chaining
//! - `digest(outputEncoding?)` returns Buffer or string per encoding
//! - `copy(options?)` returns a fresh Hash with the same in-progress
//!   state (Hash.copy exists; Hmac.copy does NOT — D-N10)
//!
//! Throws `ERR_CRYPTO_HASH_FINALIZED` on update / digest after digest.

#![allow(unsafe_code)]

use super::buffer;
use super::super::crypto::kernel::digest::{DigestContext, KernelHashAlgo};
use super::super::crypto::kernel::error::KernelError;
use crate::state::OpError;

#[allow(unused_imports)]
use zeroship_runtime_macros::{v8_class, v8_constructor, v8_method, v8_name, v8_to_string_tag};

/// Public Hash struct — wraps a kernel `DigestContext`.
pub struct Hash {
    ctx: DigestContext,
}

impl Hash {
    pub(crate) fn from_ctx(ctx: DigestContext) -> Self {
        Self { ctx }
    }
}

#[v8_class]
#[v8_to_string_tag = "Hash"]
impl Hash {
    /// Constructor is illegal — instances come from `createHash`. We
    /// throw a TypeError matching Node's `Hash` (which extends
    /// `LazyTransform` and has its prototype's constructor set to a
    /// thrower).
    #[v8_constructor]
    fn new() -> Result<Hash, OpError> {
        Err(OpError::type_error(
            "Hash is not a constructor — use crypto.createHash(name)",
        ))
    }

    /// `hash.update(data, inputEncoding?)` — chainable, returns `this`.
    /// Per spec, `inputEncoding` is IGNORED when `data` is a non-string
    /// (Buffer, TypedArray, DataView, ArrayBuffer).
    ///
    /// We return undefined here; the JS-side synthetic module wraps
    /// `update` with a thin closure that does `(...) => { native(); return this; }`
    /// so the chainable semantic survives without the macro having to
    /// thread `this` through. (The macro's wrapper-local synthesis
    /// expects `v8::Local<v8::Object>` without an explicit `'s`
    /// lifetime, but the return value of the method needs `'s`-bound
    /// — the two constraints conflict in our current shape.)
    #[v8_method]
    fn update(
        &mut self,
        scope: &mut v8::PinScope,
        data: v8::Local<v8::Value>,
        input_encoding: Option<String>,
    ) -> Result<(), OpError> {
        let bytes = buffer::extract_input(scope, data, input_encoding.as_deref())?;
        self.ctx.update(&bytes).map_err(map_kernel_err)?;
        Ok(())
    }

    /// `hash.digest(outputEncoding?)` — finalises and returns Buffer
    /// (no encoding) or string (encoding present). After this, further
    /// `update` / `digest` throws `ERR_CRYPTO_HASH_FINALIZED`.
    #[v8_method]
    fn digest<'s>(
        &mut self,
        scope: &mut v8::PinScope<'s, '_>,
        output_encoding: Option<String>,
    ) -> Result<v8::Local<'s, v8::Value>, OpError> {
        let bytes = self.ctx.finalize().map_err(map_kernel_err)?;
        buffer::emit_output(scope, &bytes, output_encoding.as_deref())
    }

    /// `hash.copy(options?)` — fresh Hash with cloned in-progress state.
    /// Per D-N9 + Node parity. The `options` arg is currently ignored
    /// (Node uses it for outputLength on XOF hashes — we don't ship
    /// SHAKE/XOF in Stage B).
    #[v8_method]
    fn copy<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        _options: v8::Local<v8::Value>,
    ) -> v8::Local<'s, v8::Value> {
        let cloned_ctx = self.ctx.clone_state();
        let new_hash = Hash::from_ctx(cloned_ctx);
        build(scope, new_hash).into()
    }
}

/// Map kernel errors to Node-shaped errors.
fn map_kernel_err(err: KernelError) -> OpError {
    match err {
        KernelError::HashFinalised => {
            OpError::node("ERR_CRYPTO_HASH_FINALIZED", "Digest already called")
        }
        KernelError::HmacFinalised => {
            OpError::node("ERR_CRYPTO_HASH_FINALIZED", "Digest already called")
        }
        KernelError::UnsupportedAlgorithm(name) => OpError::node(
            "ERR_OSSL_EVP_INVALID_DIGEST",
            format!("Unsupported algorithm: {name}"),
        ),
        KernelError::InvalidKeyLength => {
            OpError::node("ERR_CRYPTO_INVALID_KEYLEN", "Invalid key length")
        }
        KernelError::OperationFailed(s) => OpError::node(
            "ERR_CRYPTO_OPERATION_FAILED",
            format!("Operation failed: {s}"),
        ),
        KernelError::InvalidKdfParams(s) => OpError::node(
            "ERR_CRYPTO_INVALID_SCRYPT_PARAMS",
            format!("Invalid KDF parameters: {s}"),
        ),
    }
}

/// `crypto.createHash(name, options?)` factory. Resolves `name` to a
/// `KernelHashAlgo` and creates a fresh Hash.
pub fn create_hash<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    name: &str,
    _options: Option<v8::Local<v8::Value>>,
) -> Result<v8::Local<'s, v8::Value>, OpError> {
    let algo = KernelHashAlgo::from_str(name).ok_or_else(|| {
        OpError::node(
            "ERR_OSSL_EVP_INVALID_DIGEST",
            format!("Digest method not supported: {name}"),
        )
    })?;
    let hash = Hash::from_ctx(DigestContext::new(algo));
    Ok(build(scope, hash).into())
}

/// Build a Hash JS wrapper from a Rust Hash. Mirrors the
/// `crypto_key::build` pattern.
pub fn build<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    hash: Hash,
) -> v8::Local<'s, v8::Object> {
    let tmpl = Hash::install(scope);
    let inst_tmpl = tmpl.instance_template(scope);
    let inst = inst_tmpl
        .new_instance(scope)
        .expect("Hash instance allocation failed");

    let class_fn = tmpl.get_function(scope).unwrap();
    let proto_key = v8::String::new(scope, "prototype").unwrap();
    let proto_v = class_fn.get(scope, proto_key.into()).unwrap();
    inst.set_prototype(scope, proto_v);

    let boxed: Box<Hash> = Box::new(hash);
    let raw = Box::into_raw(boxed);
    let raw_addr = raw as usize;
    let ext = v8::External::new(scope, raw as *mut std::ffi::c_void);
    inst.set_internal_field(0, ext.into());
    let weak = v8::Weak::with_guaranteed_finalizer(
        scope,
        inst,
        Box::new(move || unsafe {
            drop(Box::from_raw(raw_addr as *mut Hash));
        }),
    );
    std::mem::forget(weak);
    inst
}

/// `crypto.createHash(name, options?)` — top-level free function entry
/// point bound to `globalThis.__zeroship_node_crypto.createHash` by
/// `module::install_globals`.
pub(crate) fn create_hash_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    if args.length() < 1 {
        let exc = crate::node_error::build_node_exception(
            scope,
            "ERR_INVALID_ARG_TYPE",
            "createHash: algorithm is required",
        );
        scope.throw_exception(exc);
        return;
    }
    let name = args.get(0).to_rust_string_lossy(scope);
    let options = if args.length() >= 2 {
        Some(args.get(1))
    } else {
        None
    };
    match create_hash(scope, &name, options) {
        Ok(v) => rv.set(v),
        Err(err) => {
            let exc = crate::web::crypto::helpers::op_error_to_v8(scope, err);
            scope.throw_exception(exc);
        }
    }
}
