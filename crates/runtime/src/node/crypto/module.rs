//! Native `node:crypto` ESM module.
//!
//! See `docs/proposals/node-crypto-native.md` §XI.
//!
//! The runtime resolves `import { createHash } from "node:crypto"` to a
//! V8 `SyntheticModule` whose exports are populated lazily by
//! [`evaluate`] on first import. Earlier versions went through a
//! `globalThis.__zeroship_node_crypto` boundary object that the
//! Vite-side virtual module re-exported; native synthetic modules
//! eliminate the indirection.

#![allow(unsafe_code)]

use super::{cipher, hash, hmac, kdf, key_object, keygen, misc, random, sign_verify};

/// Mint a synthetic ESM record for `node:crypto`. Called by the module
/// loader's `resolve_native` when user code imports the specifier.
pub fn synthetic_module<'s>(
    scope: &mut v8::PinScope<'s, '_>,
) -> v8::Local<'s, v8::Module> {
    let module_name = v8::String::new(scope, "node:crypto").unwrap();
    let names = export_names();
    let export_strings: Vec<v8::Local<v8::String>> = names
        .iter()
        .map(|n| v8::String::new(scope, n).unwrap())
        .collect();
    v8::Module::create_synthetic_module(scope, module_name, &export_strings, evaluate)
}

/// SyntheticModule evaluation — installs each export on the module.
fn evaluate<'s>(
    context: v8::Local<'s, v8::Context>,
    module: v8::Local<'s, v8::Module>,
) -> Option<v8::Local<'s, v8::Value>> {
    v8::callback_scope!(unsafe scope, context);

    // Build a namespace-shaped Object once, then mirror its properties
    // into both the module's named exports and the `default` export.
    // Using an object as the staging area keeps the per-callback wire
    // compatible with the previous `__zeroship_node_crypto` shape.
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

/// Names exported by `node:crypto`. The order matches the legacy
/// `__zeroship_node_crypto` install order.
fn export_names() -> &'static [&'static str] {
    &[
        "createHash",
        "createHmac",
        "randomBytes",
        "randomFillSync",
        "randomFill",
        "randomInt",
        "randomUUID",
        "getRandomValues",
        "pbkdf2Sync",
        "pbkdf2",
        "hkdfSync",
        "hkdf",
        "scryptSync",
        "scrypt",
        "createSecretKey",
        "createPublicKey",
        "createPrivateKey",
        "KeyObject",
        "PublicKeyObject",
        "PrivateKeyObject",
        "SecretKeyObject",
        "createSign",
        "createVerify",
        "sign",
        "verify",
        "publicEncrypt",
        "privateDecrypt",
        "createCipheriv",
        "createDecipheriv",
        "createCipher",
        "createDecipher",
        "getCipherInfo",
        "generateKeySync",
        "generateKeyPairSync",
        "timingSafeEqual",
        "getHashes",
        "getCiphers",
        "getCurves",
        "getFips",
        "setFips",
        "secureHeapUsed",
        "webcrypto",
        "subtle",
        "fips",
        "constants",
        "default",
    ]
}

/// Populate `obj` with the full `node:crypto` surface — factories,
/// classes, WebCrypto bridge, and constants. Shared between the
/// SyntheticModule eval path (via [`evaluate`]) and any direct-global
/// call site (currently the `crypto_node` test harness).
pub fn populate<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    obj: v8::Local<v8::Object>,
) {
    // First install the Hash + Hmac classes so JS can `instanceof` them.
    let _hash_class = hash::Hash::install(scope);
    let _hmac_class = hmac::Hmac::install(scope);
    let _ko_class = key_object::KeyObject::install(scope);
    let _pub_class = key_object::PublicKeyObject::install(scope);
    let _priv_class = key_object::PrivateKeyObject::install(scope);
    let _sec_class = key_object::SecretKeyObject::install(scope);
    let _sign_class = sign_verify::Sign::install(scope);
    let _verify_class = sign_verify::Verify::install(scope);
    let _cipher_class = cipher::Cipher::install(scope);

    // -- Hash + Hmac factories --
    set_fn(scope, obj, "createHash", hash::create_hash_callback);
    set_fn(scope, obj, "createHmac", hmac::create_hmac_callback);

    // -- Random --
    set_fn(scope, obj, "randomBytes", random::random_bytes_callback);
    set_fn(scope, obj, "randomFillSync", random::random_fill_sync_callback);
    set_fn(scope, obj, "randomFill", random::random_fill_callback);
    set_fn(scope, obj, "randomInt", random::random_int_callback);
    set_fn(scope, obj, "randomUUID", random::random_uuid_callback);
    set_fn(scope, obj, "getRandomValues", random::get_random_values_callback);

    // -- KDFs --
    set_fn(scope, obj, "pbkdf2Sync", kdf::pbkdf2_sync_callback);
    set_fn(scope, obj, "pbkdf2", kdf::pbkdf2_callback);
    set_fn(scope, obj, "hkdfSync", kdf::hkdf_sync_callback);
    set_fn(scope, obj, "hkdf", kdf::hkdf_callback);
    set_fn(scope, obj, "scryptSync", kdf::scrypt_sync_callback);
    set_fn(scope, obj, "scrypt", kdf::scrypt_callback);

    // -- KeyObject + factories --
    set_fn(scope, obj, "createSecretKey", key_object::create_secret_key_callback);
    set_fn(scope, obj, "createPublicKey", key_object::create_public_key_callback);
    set_fn(scope, obj, "createPrivateKey", key_object::create_private_key_callback);

    // Expose the KeyObject class itself + the static `from` so the Node
    // pattern `KeyObject.from(cryptoKey)` resolves.
    {
        let ko_tmpl = key_object::KeyObject::install(scope);
        let ko_fn = ko_tmpl.get_function(scope).unwrap();
        let from_key = v8::String::new(scope, "from").unwrap();
        let from_fn = v8::Function::new(scope, key_object::key_object_from_callback).unwrap();
        ko_fn.set(scope, from_key.into(), from_fn.into());
        let k = v8::String::new(scope, "KeyObject").unwrap();
        obj.set(scope, k.into(), ko_fn.into());
    }
    {
        let pub_tmpl = key_object::PublicKeyObject::install(scope);
        let pub_fn = pub_tmpl.get_function(scope).unwrap();
        let k = v8::String::new(scope, "PublicKeyObject").unwrap();
        obj.set(scope, k.into(), pub_fn.into());
    }
    {
        let priv_tmpl = key_object::PrivateKeyObject::install(scope);
        let priv_fn = priv_tmpl.get_function(scope).unwrap();
        let k = v8::String::new(scope, "PrivateKeyObject").unwrap();
        obj.set(scope, k.into(), priv_fn.into());
    }
    {
        let sec_tmpl = key_object::SecretKeyObject::install(scope);
        let sec_fn = sec_tmpl.get_function(scope).unwrap();
        let k = v8::String::new(scope, "SecretKeyObject").unwrap();
        obj.set(scope, k.into(), sec_fn.into());
    }

    // -- Sign / Verify --
    set_fn(scope, obj, "createSign", sign_verify::create_sign_callback);
    set_fn(scope, obj, "createVerify", sign_verify::create_verify_callback);
    set_fn(scope, obj, "sign", sign_verify::sign_callback);
    set_fn(scope, obj, "verify", sign_verify::verify_callback);
    set_fn(scope, obj, "publicEncrypt", sign_verify::public_encrypt_callback);
    set_fn(scope, obj, "privateDecrypt", sign_verify::private_decrypt_callback);

    // -- Cipher / Decipher --
    set_fn(scope, obj, "createCipheriv", cipher::create_cipheriv_callback);
    set_fn(scope, obj, "createDecipheriv", cipher::create_decipheriv_callback);
    set_fn(scope, obj, "createCipher", cipher::create_cipher_callback);
    set_fn(scope, obj, "createDecipher", cipher::create_decipher_callback);
    set_fn(scope, obj, "getCipherInfo", cipher::get_cipher_info_callback);

    // -- Key generation --
    set_fn(scope, obj, "generateKeySync", keygen::generate_key_sync_callback);
    set_fn(scope, obj, "generateKeyPairSync", keygen::generate_key_pair_sync_callback);

    // -- Misc --
    set_fn(scope, obj, "timingSafeEqual", misc::timing_safe_equal_callback);
    set_fn(scope, obj, "getHashes", misc::get_hashes_callback);
    set_fn(scope, obj, "getCiphers", misc::get_ciphers_callback);
    set_fn(scope, obj, "getCurves", misc::get_curves_callback);
    set_fn(scope, obj, "getFips", misc::get_fips_callback);
    set_fn(scope, obj, "setFips", misc::set_fips_callback);
    set_fn(scope, obj, "secureHeapUsed", misc::secure_heap_used_callback);

    // -- WebCrypto bridge --
    // `webcrypto` and `subtle` need object identity with globalThis.crypto.
    let global = scope.get_current_context().global(scope);
    let crypto_key = v8::String::new(scope, "crypto").unwrap();
    if let Some(crypto_val) = global.get(scope, crypto_key.into()) {
        let webcrypto_key = v8::String::new(scope, "webcrypto").unwrap();
        obj.set(scope, webcrypto_key.into(), crypto_val);
        if let Ok(crypto_obj) = v8::Local::<v8::Object>::try_from(crypto_val) {
            let subtle_key = v8::String::new(scope, "subtle").unwrap();
            if let Some(subtle_val) = crypto_obj.get(scope, subtle_key.into()) {
                obj.set(scope, subtle_key.into(), subtle_val);
            }
        }
    }

    // -- fips constant (read-only-ish) --
    let fips_k = v8::String::new(scope, "fips").unwrap();
    let zero = v8::Integer::new(scope, 0);
    obj.set(scope, fips_k.into(), zero.into());

    // -- constants object (RSA padding constants etc.) --
    let constants = v8::Object::new(scope);
    let pairs = [
        ("RSA_PKCS1_PADDING", 1),
        ("RSA_NO_PADDING", 3),
        ("RSA_PKCS1_OAEP_PADDING", 4),
        ("RSA_PKCS1_PSS_PADDING", 6),
        ("RSA_PSS_SALTLEN_DIGEST", -1),
        ("RSA_PSS_SALTLEN_MAX_SIGN", -2),
        ("RSA_PSS_SALTLEN_AUTO", -2),
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
