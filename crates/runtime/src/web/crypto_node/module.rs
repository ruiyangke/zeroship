//! Synthetic `node:crypto` ESM module — install path.
//!
//! Per `docs/proposals/node-crypto-native.md` §XI (D-N26).
//!
//! The single Rust↔JS boundary is `globalThis.__zeroship_node_crypto`
//! — a plain object whose properties are the named exports. The
//! Vite-side synthetic module (defined in
//! `sdks/vite-plugin/src/node-compat.ts`) imports this object and
//! re-exports each property as a named ESM export. So:
//!
//! ```javascript
//! import { createHash, randomUUID } from "node:crypto";
//! ```
//!
//! resolves to:
//!
//! ```javascript
//! const { createHash, randomUUID } = globalThis.__zeroship_node_crypto;
//! ```
//!
//! at the module-evaluation time of "node:crypto".

#![allow(unsafe_code)]

use super::{cipher, hash, hmac, kdf, key_object, keygen, misc, random, sign_verify};

/// Install `globalThis.__zeroship_node_crypto`. Called from
/// `core/init.rs::setup_globals` alongside the other native installs.
pub fn install_globals<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    global: v8::Local<v8::Object>,
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

    // Build the boundary object.
    let obj = v8::Object::new(scope);

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

    // -- WebCrypto bridge (D-N16) --
    // `webcrypto` and `subtle` need object identity with globalThis.crypto.
    // Read globalThis.crypto and assign property references.
    let crypto_key = v8::String::new(scope, "crypto").unwrap();
    if let Some(crypto_val) = global.get(scope, crypto_key.into()) {
        let webcrypto_key = v8::String::new(scope, "webcrypto").unwrap();
        obj.set(scope, webcrypto_key.into(), crypto_val);
        // crypto.subtle (read once at install time; same identity
        // because Crypto's [SameObject] caching).
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

    // Install the boundary object on globalThis.
    let key = v8::String::new(scope, "__zeroship_node_crypto").unwrap();
    global.set(scope, key.into(), obj.into());
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
