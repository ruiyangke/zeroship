//! `Hmac` class — `crypto.createHmac(algorithm, key, options?)`.
//!
//! See `docs/proposals/node-crypto-native.md` §V.3. Same
//! surface as Hash: update + digest. **No `copy()`** — Hmac doesn't
//! have one in Node.
//!
//! Empty keys: per round-3 review (XVII.13b), Node silently accepts
//! empty HMAC keys. We match (this is the Node-compat path; the
//! defense-in-depth empty-key check on the WebCrypto surface lives
//! over there for spec compliance).

#![allow(unsafe_code)]

use super::buffer;
use crate::crypto_ops::digest::KernelHashAlgo;
use crate::crypto_ops::error::KernelError;
use crate::crypto_ops::hmac::HmacContext;
use crate::state::OpError;

#[allow(unused_imports)]
use zeroship_runtime_macros::{v8_class, v8_constructor, v8_method, v8_to_string_tag};

pub struct Hmac {
    ctx: HmacContext,
}

#[v8_class]
#[v8_to_string_tag = "Hmac"]
impl Hmac {
    #[v8_constructor]
    fn new() -> Result<Hmac, OpError> {
        Err(OpError::type_error(
            "Hmac is not a constructor — use crypto.createHmac(name, key)",
        ))
    }

    /// `hmac.update(data, inputEncoding?)` — chainable (JS-side wrapper
    /// supplies `return this`).
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

    /// `hmac.digest(outputEncoding?)`.
    #[v8_method]
    fn digest<'s>(
        &mut self,
        scope: &mut v8::PinScope<'s, '_>,
        output_encoding: Option<String>,
    ) -> Result<v8::Local<'s, v8::Value>, OpError> {
        let bytes = self.ctx.finalize().map_err(map_kernel_err)?;
        buffer::emit_output(scope, &bytes, output_encoding.as_deref())
    }
}

fn map_kernel_err(err: KernelError) -> OpError {
    match err {
        KernelError::HashFinalised | KernelError::HmacFinalised => {
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

/// `crypto.createHmac(name, key, options?)` factory.
pub fn create_hmac<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    name: &str,
    key: v8::Local<v8::Value>,
    _options: Option<v8::Local<v8::Value>>,
) -> Result<v8::Local<'s, v8::Value>, OpError> {
    let algo = KernelHashAlgo::from_str(name).ok_or_else(|| {
        OpError::node(
            "ERR_OSSL_EVP_INVALID_DIGEST",
            format!("Digest method not supported: {name}"),
        )
    })?;
    // Default key encoding for string input is utf8 — matches Node.
    let key_bytes = buffer::extract_input(scope, key, Some("utf8"))?;
    let ctx = HmacContext::new(algo, &key_bytes).map_err(map_kernel_err)?;
    Ok(build(scope, Hmac { ctx }).into())
}

pub fn build<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    hmac: Hmac,
) -> v8::Local<'s, v8::Object> {
    let tmpl = Hmac::install(scope);
    let inst_tmpl = tmpl.instance_template(scope);
    let inst = inst_tmpl.new_instance(scope).expect("Hmac instance allocation");

    let class_fn = tmpl.get_function(scope).unwrap();
    let proto_key = v8::String::new(scope, "prototype").unwrap();
    let proto_v = class_fn.get(scope, proto_key.into()).unwrap();
    inst.set_prototype(scope, proto_v);

    let boxed: Box<Hmac> = Box::new(hmac);
    let raw = Box::into_raw(boxed);
    let raw_addr = raw as usize;
    let ext = v8::External::new(scope, raw as *mut std::ffi::c_void);
    inst.set_internal_field(0, ext.into());
    let weak = v8::Weak::with_guaranteed_finalizer(
        scope,
        inst,
        Box::new(move || unsafe {
            drop(Box::from_raw(raw_addr as *mut Hmac));
        }),
    );
    std::mem::forget(weak);
    inst
}

pub(crate) fn create_hmac_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    if args.length() < 2 {
        let exc = crate::node_error::build_node_exception(
            scope,
            "ERR_INVALID_ARG_TYPE",
            "createHmac: algorithm and key are required",
        );
        scope.throw_exception(exc);
        return;
    }
    let name = args.get(0).to_rust_string_lossy(scope);
    let key = args.get(1);
    let options = if args.length() >= 3 {
        Some(args.get(2))
    } else {
        None
    };
    match create_hmac(scope, &name, key, options) {
        Ok(v) => rv.set(v),
        Err(err) => {
            let exc = crate::web::crypto::helpers::op_error_to_v8(scope, err);
            scope.throw_exception(exc);
        }
    }
}
