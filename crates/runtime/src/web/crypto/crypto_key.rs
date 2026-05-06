//! `CryptoKey` — V8 class with internal field 0 holding a tag byte
//! followed by the `CryptoKeyState`. The Rust-side struct IS the
//! box (so the macro's `&self` cast lines up with the storage we
//! produce in `build`).
//!
//! Per `docs/proposals/webcrypto-native.md` §V (D-2, D-9, D-10).
//!
//! The class is platform-constructed per spec §13 — the constructor
//! throws if called from JS; real instances come from
//! `subtle.{generateKey,importKey,...}` via `build`.

#![allow(unsafe_code)]

use super::key_material::{CryptoKeyState, KeyAlgorithm, KeyType, KeyUsage, CRYPTO_KEY_TAG};
use crate::state::OpError;

#[allow(unused_imports)]
use zeroship_runtime_macros::{v8_class, v8_constructor, v8_getter, v8_method, v8_to_string_tag};

// ---------------------------------------------------------------------------
// CryptoKey IDL surface
// ---------------------------------------------------------------------------

/// `#[repr(C)]` so the tag byte sits at offset 0 — the brand check
/// (`is_crypto_key`) reads it via raw pointer.
#[repr(C)]
pub struct CryptoKey {
    /// First byte of the box. Set to `CRYPTO_KEY_TAG` (`0xC1`) by the
    /// constructor / `build` helpers; never mutated. Reading it via
    /// the brand check from a non-CryptoKey allocation is bounded
    /// safely (single-byte read) — see SAFETY notes on `is_crypto_key`.
    pub tag: u8,
    /// The spec's [[type]] / [[extractable]] / [[algorithm]] /
    /// [[usages]] / [[handle]] slots, all bundled.
    pub state: CryptoKeyState,
}

#[v8_class]
#[v8_to_string_tag = "CryptoKey"]
impl CryptoKey {
    /// Throws — JS code can't `new CryptoKey()` per spec §13. Real
    /// instances are built via [`build`] from Rust, which bypasses the
    /// constructor by allocating from the instance template directly.
    #[v8_constructor]
    fn new() -> Result<CryptoKey, OpError> {
        Err(OpError::type_error("Illegal constructor"))
    }

    /// `key.type` — readonly, immutable per spec §13.
    #[v8_getter]
    #[v8_name = "type"]
    fn type_(&self) -> String {
        self.state.key_type.as_str().to_string()
    }

    /// `key.extractable` — readonly.
    #[v8_getter]
    fn extractable(&self) -> bool {
        self.state.extractable
    }

    /// `key.algorithm` — readonly per spec §13. Returns a frozen
    /// algorithm-shape object. The macro can't thread the wrapper's
    /// own JS object into a getter for `[SameObject]` caching, so the
    /// caching variant is hand-installed in `install_global`; this
    /// getter is the no-cache fallback used only when the hand-roll
    /// hasn't run (it shouldn't be reached in production).
    #[v8_getter]
    fn algorithm<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
    ) -> v8::Local<'s, v8::Value> {
        build_algorithm_object(scope, &self.state.algorithm).into()
    }

    /// `key.usages` — readonly. Same `[SameObject]` story as
    /// `algorithm` — see comment above.
    #[v8_getter]
    fn usages<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
    ) -> v8::Local<'s, v8::Value> {
        build_usages_array(scope, &self.state.usages).into()
    }
}

impl CryptoKey {
    pub fn new_box(state: CryptoKeyState) -> Self {
        CryptoKey {
            tag: CRYPTO_KEY_TAG,
            state,
        }
    }
}

// ---------------------------------------------------------------------------
// Brand check (D-10) + state accessor
// ---------------------------------------------------------------------------

/// Unspoofable instanceof check — read internal-field 0's tag byte.
/// Returns true iff `value` is a V8 Object with one internal field
/// whose External value points to a `Box<CryptoKey>` with the
/// expected `CRYPTO_KEY_TAG` byte at offset 0.
pub fn is_crypto_key(scope: &mut v8::PinScope, value: v8::Local<v8::Value>) -> bool {
    let obj = match v8::Local::<v8::Object>::try_from(value) {
        Ok(o) => o,
        Err(_) => return false,
    };
    if obj.internal_field_count() != 1 {
        return false;
    }
    let field = match obj.get_internal_field(scope, 0) {
        Some(f) => f,
        None => return false,
    };
    let ext = match v8::Local::<v8::External>::try_from(field) {
        Ok(e) => e,
        Err(_) => return false,
    };
    let ptr = ext.value() as *const u8;
    if ptr.is_null() {
        return false;
    }
    // SAFETY: We never mutate the brand byte; the External keeps the
    // box alive while the wrapper is alive. Reading the first byte of
    // an arbitrary allocation we own is sound — at worst we read a
    // non-`0xC1` byte from a different #[v8_class] wrapper and reject.
    let tag = unsafe { *ptr };
    tag == CRYPTO_KEY_TAG
}

/// Read `&CryptoKeyState` without re-running the brand check. Caller
/// MUST have verified `this` is a CryptoKey first.
///
/// SAFETY of the returned reference's lifetime: the box behind the
/// External pointer lives as long as the JS wrapper Object. Callers
/// hold a `Local<Object>` whose lifetime is bounded by the V8 scope;
/// we tie the returned `&CryptoKeyState` to that scope's lifetime
/// `'s` (NOT the input Local's lifetime, which would over-constrain).
pub fn state_unchecked<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    this: v8::Local<v8::Object>,
) -> &'s CryptoKeyState {
    let field = this.get_internal_field(scope, 0).unwrap();
    let ext: v8::Local<v8::External> = field.try_into().unwrap();
    let ptr = ext.value() as *const CryptoKey;
    // SAFETY: `ptr` was Box::into_raw'd from Box<CryptoKey> by build()
    // (or by the macro-emitted constructor finalizer); the GC
    // finalizer owns the drop, so the box is alive while the wrapper
    // is alive (the wrapper itself is reachable via the input Local
    // and outlives this scope).
    unsafe { &(*ptr).state }
}

/// Read `&CryptoKeyState` from a `v8::Local<v8::Value>` after running
/// the brand check. Returns `Err(TypeError("Expected CryptoKey"))` on
/// mismatch — convenient for op-method bodies that take a CryptoKey
/// arg.
pub fn require<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    value: v8::Local<v8::Value>,
) -> Result<&'s CryptoKeyState, OpError> {
    if !is_crypto_key(scope, value) {
        return Err(OpError::type_error(
            "Expected a CryptoKey instance",
        ));
    }
    let obj: v8::Local<v8::Object> = value.try_into().unwrap();
    Ok(state_unchecked(scope, obj))
}

// ---------------------------------------------------------------------------
// Build a CryptoKey JS wrapper from a CryptoKeyState
// ---------------------------------------------------------------------------

/// Allocate a fresh JS `CryptoKey` wrapper around the given state.
/// Bypasses the user-facing constructor (which throws); follows the
/// same `instance_template().new_instance()` + `set_internal_field` +
/// finalizer pattern the macro emits for a real `new Foo(...)` call.
pub fn build<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    state: CryptoKeyState,
) -> v8::Local<'s, v8::Object> {
    let tmpl = CryptoKey::install(scope);
    let inst_tmpl = tmpl.instance_template(scope);
    let inst = inst_tmpl
        .new_instance(scope)
        .expect("CryptoKey instance allocation failed");

    // Wire prototype to CryptoKey.prototype so `instanceof CryptoKey`
    // works. `instance_template().new_instance()` produces a bare
    // object whose [[Prototype]] is Object.prototype; we have to set
    // it manually (the constructor path does this implicitly via
    // `new`).
    let class_fn = tmpl.get_function(scope).unwrap();
    let proto_key = v8::String::new(scope, "prototype").unwrap();
    let proto_v = class_fn.get(scope, proto_key.into()).unwrap();
    inst.set_prototype(scope, proto_v);

    let boxed: Box<CryptoKey> = Box::new(CryptoKey::new_box(state));
    let raw = Box::into_raw(boxed);
    let raw_addr = raw as usize;
    let ext = v8::External::new(scope, raw as *mut std::ffi::c_void);
    inst.set_internal_field(0, ext.into());

    let weak = v8::Weak::with_guaranteed_finalizer(
        scope,
        inst,
        Box::new(move || unsafe {
            drop(Box::from_raw(raw_addr as *mut CryptoKey));
        }),
    );
    std::mem::forget(weak);

    inst
}

// ---------------------------------------------------------------------------
// Helpers — algorithm + usages frozen JS objects (D-9 SameObject backing)
// ---------------------------------------------------------------------------

fn build_algorithm_object<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    alg: &KeyAlgorithm,
) -> v8::Local<'s, v8::Object> {
    let obj = v8::Object::new(scope);
    set_str(scope, obj, "name", alg.name());

    match alg {
        KeyAlgorithm::Aes(a) => {
            let k = v8::String::new(scope, "length").unwrap();
            let v = v8::Integer::new_from_unsigned(scope, a.length);
            obj.set(scope, k.into(), v.into());
        }
        KeyAlgorithm::Hmac(h) => {
            let k = v8::String::new(scope, "length").unwrap();
            let v = v8::Integer::new_from_unsigned(scope, h.length);
            obj.set(scope, k.into(), v.into());
            // hash: { name: "SHA-256" }, frozen
            let hash_obj = v8::Object::new(scope);
            set_str(scope, hash_obj, "name", h.hash.as_str());
            freeze(scope, hash_obj);
            let k = v8::String::new(scope, "hash").unwrap();
            obj.set(scope, k.into(), hash_obj.into());
        }
        KeyAlgorithm::RsaHashed(r) => {
            let k = v8::String::new(scope, "modulusLength").unwrap();
            let v = v8::Integer::new_from_unsigned(scope, r.modulus_length);
            obj.set(scope, k.into(), v.into());
            // publicExponent — Uint8Array
            let pub_exp_v = vec_to_uint8array(scope, &r.public_exponent);
            let k = v8::String::new(scope, "publicExponent").unwrap();
            obj.set(scope, k.into(), pub_exp_v);
            // hash: { name }, frozen
            let hash_obj = v8::Object::new(scope);
            set_str(scope, hash_obj, "name", r.hash.as_str());
            freeze(scope, hash_obj);
            let k = v8::String::new(scope, "hash").unwrap();
            obj.set(scope, k.into(), hash_obj.into());
        }
        KeyAlgorithm::Ec(e) => {
            set_str(scope, obj, "namedCurve", e.named_curve.as_str());
        }
        KeyAlgorithm::Ed25519
        | KeyAlgorithm::X25519
        | KeyAlgorithm::Hkdf
        | KeyAlgorithm::Pbkdf2 => {
            // name only.
        }
    }
    freeze(scope, obj);
    obj
}

fn build_usages_array<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    usages: &[KeyUsage],
) -> v8::Local<'s, v8::Array> {
    let arr = v8::Array::new(scope, usages.len() as i32);
    for (i, u) in usages.iter().enumerate() {
        let s = v8::String::new(scope, u.as_str()).unwrap();
        arr.set_index(scope, i as u32, s.into());
    }
    freeze(scope, arr.into());
    arr
}

fn set_str<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    obj: v8::Local<'s, v8::Object>,
    name: &str,
    value: &str,
) {
    let k = v8::String::new(scope, name).unwrap();
    let v = v8::String::new(scope, value).unwrap();
    obj.set(scope, k.into(), v.into());
}

fn freeze<'s>(scope: &mut v8::PinScope<'s, '_>, obj: v8::Local<'s, v8::Object>) {
    // Object.freeze via direct Object::set_integrity_level (V8 8.x +).
    let _ = obj.set_integrity_level(scope, v8::IntegrityLevel::Frozen);
}

// Local helper for Uint8Array allocation — same shape the macro emits.
fn vec_to_uint8array<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    bytes: &[u8],
) -> v8::Local<'s, v8::Value> {
    let len = bytes.len();
    let ab = v8::ArrayBuffer::new(scope, len);
    let store = ab.get_backing_store();
    for (i, &b) in bytes.iter().enumerate() {
        store[i].set(b);
    }
    let arr = v8::Uint8Array::new(scope, ab, 0, len).unwrap();
    arr.into()
}

// #198 — `install_global` removed; bind happens via the macro-emitted
// `CryptoKey::register` invoked from `crypto::install_globals`'s
// `register_native_classes!` list. No caller used the previously-
// returned `Function`.
