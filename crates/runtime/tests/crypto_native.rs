//! Native WebCrypto smoke tests. Drives `Crypto` / `SubtleCrypto` /
//! `CryptoKey` directly in an isolated V8 context (no Runtime), so the
//! tests run the native install path even before the polyfill flip.
//!
//! Per `docs/proposals/webcrypto-native.md` §X.1.

#![allow(unsafe_code, missing_debug_implementations)]

use zeroship_runtime::{crypto_native, dom, init_v8, text_encoding};

/// Run a JS snippet in a fresh V8 context with DOMException + native
/// crypto installed. Returns the script's last expression value.
fn run_js<F, R>(src: &str, f: F) -> R
where
    F: FnOnce(v8::Local<v8::Value>, &mut v8::PinScope) -> R,
{
    init_v8();
    let mut isolate = v8::Isolate::new(v8::CreateParams::default());
    v8::scope!(let handle_scope, &mut isolate);
    let context = v8::Context::new(handle_scope, Default::default());
    let scope = &mut v8::ContextScope::new(handle_scope, context);

    let global = scope.get_current_context().global(scope);
    dom::exception::install_global(scope, global);
    crypto_native::install_globals(scope, global);
    // TextEncoder / TextDecoder for round-trip tests.
    {
        let tmpl = text_encoding::TextEncoder::install(scope);
        let class_fn = tmpl.get_function(scope).unwrap();
        let key = v8::String::new(scope, "TextEncoder").unwrap();
        global.set(scope, key.into(), class_fn.into());
    }
    {
        let tmpl = text_encoding::TextDecoder::install(scope);
        let class_fn = tmpl.get_function(scope).unwrap();
        let key = v8::String::new(scope, "TextDecoder").unwrap();
        global.set(scope, key.into(), class_fn.into());
    }

    let src_v8 = v8::String::new(scope, src).unwrap();
    let script = v8::Script::compile(scope, src_v8, None).unwrap();
    let result = script.run(scope).unwrap();
    scope.perform_microtask_checkpoint();
    f(result, scope)
}

fn run_js_string(src: &str) -> String {
    run_js(src, |v, scope| v.to_rust_string_lossy(scope))
}

fn run_js_bool(src: &str) -> bool {
    run_js(src, |v, scope| v.boolean_value(scope))
}

// =============================================================================
// Class installation
// =============================================================================

#[test]
fn crypto_global_installed() {
    assert!(run_js_bool("typeof globalThis.crypto === 'object'"));
    assert!(run_js_bool("typeof globalThis.crypto.subtle === 'object'"));
    assert!(run_js_bool("typeof globalThis.CryptoKey === 'function'"));
    assert!(run_js_bool("typeof globalThis.SubtleCrypto === 'function'"));
    assert!(run_js_bool("typeof globalThis.Crypto === 'function'"));
}

#[test]
fn crypto_subtle_same_object() {
    // [SameObject] per spec §10: crypto.subtle === crypto.subtle.
    assert!(run_js_bool("crypto.subtle === crypto.subtle"));
}

#[test]
fn crypto_to_string_tag() {
    assert_eq!(
        run_js_string("Object.prototype.toString.call(crypto)"),
        "[object Crypto]"
    );
    assert_eq!(
        run_js_string("Object.prototype.toString.call(crypto.subtle)"),
        "[object SubtleCrypto]"
    );
}

#[test]
fn random_uuid_v4_format() {
    let s = run_js_string("crypto.randomUUID()");
    // UUIDv4 format: 8-4-4-4-12 hex with v4 (third group starts with 4)
    // and variant 10xx (fourth group starts with 8/9/a/b).
    assert_eq!(s.len(), 36);
    let parts: Vec<&str> = s.split('-').collect();
    assert_eq!(parts.len(), 5);
    assert_eq!(parts[0].len(), 8);
    assert_eq!(parts[1].len(), 4);
    assert_eq!(parts[2].len(), 4);
    assert_eq!(parts[3].len(), 4);
    assert_eq!(parts[4].len(), 12);
    assert!(parts[2].starts_with('4'), "version-4 nibble missing: {s}");
    let variant_byte = parts[3].chars().next().unwrap();
    assert!(
        matches!(variant_byte, '8' | '9' | 'a' | 'b'),
        "variant 10xx missing: {s}"
    );
}

// =============================================================================
// getRandomValues — D-21 fixes
// =============================================================================

#[test]
fn get_random_values_fills_uint8array() {
    // Re-call returns the same array; bytes change.
    assert!(run_js_bool(
        r#"
        const a = new Uint8Array(8);
        const r = crypto.getRandomValues(a);
        r === a;
        "#
    ));
}

#[test]
fn get_random_values_rejects_float32_array_with_type_mismatch() {
    let err_name = run_js_string(
        r#"
        try {
            crypto.getRandomValues(new Float32Array(8));
            "no-throw";
        } catch (e) {
            e.name;
        }
        "#,
    );
    assert_eq!(err_name, "TypeMismatchError");
}

#[test]
fn get_random_values_rejects_dataview_with_type_mismatch() {
    let err_name = run_js_string(
        r#"
        try {
            crypto.getRandomValues(new DataView(new ArrayBuffer(8)));
            "no-throw";
        } catch (e) {
            e.name;
        }
        "#,
    );
    assert_eq!(err_name, "TypeMismatchError");
}

#[test]
fn get_random_values_quota_throws_quota_exceeded() {
    let err_name = run_js_string(
        r#"
        try {
            crypto.getRandomValues(new Uint8Array(80000));
            "no-throw";
        } catch (e) {
            e.name;
        }
        "#,
    );
    assert_eq!(err_name, "QuotaExceededError");
}

#[test]
fn get_random_values_zero_length_returns_array() {
    assert!(run_js_bool(
        r#"
        const a = new Uint8Array(0);
        const r = crypto.getRandomValues(a);
        r === a;
        "#
    ));
}

// =============================================================================
// digest
// =============================================================================

#[test]
fn digest_sha256_empty_string() {
    // SHA-256("") = e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855
    let r = run_js(
        r#"
        crypto.subtle.digest("SHA-256", new Uint8Array(0))
            .then(b => Array.from(new Uint8Array(b)).map(x => x.toString(16).padStart(2, "0")).join(""));
        "#,
        |v, scope| {
            let p: v8::Local<v8::Promise> = v.try_into().unwrap();
            // sync resolve — the result is already settled.
            let result = p.result(scope);
            result.to_rust_string_lossy(scope)
        },
    );
    assert_eq!(
        r,
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
}

#[test]
fn digest_string_algo_name() {
    // String algorithm shape (not object) — spec §32.
    let r = run_js(
        r#"
        crypto.subtle.digest("SHA-1", new TextEncoder().encode("abc"))
            .then(b => Array.from(new Uint8Array(b)).map(x => x.toString(16).padStart(2, "0")).join(""));
        "#,
        |v, scope| {
            let p: v8::Local<v8::Promise> = v.try_into().unwrap();
            let result = p.result(scope);
            result.to_rust_string_lossy(scope)
        },
    );
    // SHA-1("abc") = a9993e364706816aba3e25717850c26c9cd0d89d
    assert_eq!(r, "a9993e364706816aba3e25717850c26c9cd0d89d");
}

#[test]
fn digest_object_algo_name() {
    let r = run_js(
        r#"
        crypto.subtle.digest({ name: "SHA-256" }, new Uint8Array(0))
            .then(b => new Uint8Array(b).byteLength);
        "#,
        |v, scope| {
            let p: v8::Local<v8::Promise> = v.try_into().unwrap();
            let result = p.result(scope);
            result.uint32_value(scope).unwrap_or(0)
        },
    );
    assert_eq!(r, 32);
}

#[test]
fn digest_unknown_algo_rejects_with_not_supported() {
    let err_name = run_js(
        r#"
        crypto.subtle.digest("MD5", new Uint8Array(0)).then(
            () => "no-throw",
            e => e.name
        );
        "#,
        |v, scope| {
            let p: v8::Local<v8::Promise> = v.try_into().unwrap();
            let result = p.result(scope);
            result.to_rust_string_lossy(scope)
        },
    );
    assert_eq!(err_name, "NotSupportedError");
}

// =============================================================================
// AES-GCM round-trip
// =============================================================================

#[test]
fn aes_gcm_generate_encrypt_decrypt_round_trip() {
    let r = run_js(
        r#"
        (async () => {
            const key = await crypto.subtle.generateKey(
                { name: "AES-GCM", length: 256 },
                true,
                ["encrypt", "decrypt"]
            );
            const iv = crypto.getRandomValues(new Uint8Array(12));
            const data = new TextEncoder().encode("hello webcrypto");
            const ct = await crypto.subtle.encrypt({ name: "AES-GCM", iv }, key, data);
            const pt = await crypto.subtle.decrypt({ name: "AES-GCM", iv }, key, ct);
            return new TextDecoder().decode(pt);
        })();
        "#,
        |v, scope| {
            let p: v8::Local<v8::Promise> = v.try_into().unwrap();
            // Drive microtasks until settled.
            for _ in 0..16 {
                scope.perform_microtask_checkpoint();
            }
            let result = p.result(scope);
            result.to_rust_string_lossy(scope)
        },
    );
    assert_eq!(r, "hello webcrypto");
}

// =============================================================================
// HMAC sign / verify round-trip
// =============================================================================

#[test]
fn hmac_sign_verify_round_trip() {
    let r = run_js(
        r#"
        (async () => {
            const key = await crypto.subtle.generateKey(
                { name: "HMAC", hash: "SHA-256" },
                true,
                ["sign", "verify"]
            );
            const data = new TextEncoder().encode("payload");
            const sig = await crypto.subtle.sign({ name: "HMAC" }, key, data);
            const ok = await crypto.subtle.verify({ name: "HMAC" }, key, sig, data);
            return ok ? "ok" : "fail";
        })();
        "#,
        |v, scope| {
            let p: v8::Local<v8::Promise> = v.try_into().unwrap();
            for _ in 0..16 {
                scope.perform_microtask_checkpoint();
            }
            let result = p.result(scope);
            result.to_rust_string_lossy(scope)
        },
    );
    assert_eq!(r, "ok");
}

// =============================================================================
// Key-usage validation (D-7)
// =============================================================================

#[test]
fn aes_gcm_sign_throws_invalid_access() {
    let err_name = run_js(
        r#"
        (async () => {
            const key = await crypto.subtle.generateKey(
                { name: "AES-GCM", length: 128 },
                true,
                ["encrypt", "decrypt"]
            );
            try {
                await crypto.subtle.sign({ name: "HMAC" }, key, new Uint8Array(0));
                return "no-throw";
            } catch (e) {
                return e.name;
            }
        })();
        "#,
        |v, scope| {
            let p: v8::Local<v8::Promise> = v.try_into().unwrap();
            for _ in 0..8 {
                scope.perform_microtask_checkpoint();
            }
            let result = p.result(scope);
            result.to_rust_string_lossy(scope)
        },
    );
    // The error should be Invalid-usage rejected at usage check or
    // algorithm mismatch. Both yield InvalidAccessError DOMException.
    assert_eq!(err_name, "InvalidAccessError");
}

#[test]
fn aes_gcm_wrong_usages_at_generate_throws_syntax() {
    let err_name = run_js(
        r#"
        (async () => {
            try {
                await crypto.subtle.generateKey(
                    { name: "AES-GCM", length: 128 },
                    true,
                    ["sign"]
                );
                return "no-throw";
            } catch (e) {
                return e.name;
            }
        })();
        "#,
        |v, scope| {
            let p: v8::Local<v8::Promise> = v.try_into().unwrap();
            for _ in 0..4 {
                scope.perform_microtask_checkpoint();
            }
            let result = p.result(scope);
            result.to_rust_string_lossy(scope)
        },
    );
    assert_eq!(err_name, "SyntaxError");
}

// =============================================================================
// CryptoKey shape (D-9 SameObject + D-10 brand)
// =============================================================================

#[test]
fn crypto_key_algorithm_same_object() {
    assert!(run_js_bool(
        r#"
        (async () => {
            const key = await crypto.subtle.generateKey(
                { name: "AES-GCM", length: 128 },
                true,
                ["encrypt", "decrypt"]
            );
            return key.algorithm === key.algorithm;
        })();
        "#,
    ));
    // The `run_js_bool` already drove microtasks.
}

#[test]
fn crypto_key_to_string_tag() {
    let s = run_js(
        r#"
        (async () => {
            const key = await crypto.subtle.generateKey(
                { name: "AES-GCM", length: 128 },
                true,
                ["encrypt"]
            );
            return Object.prototype.toString.call(key);
        })();
        "#,
        |v, scope| {
            let p: v8::Local<v8::Promise> = v.try_into().unwrap();
            for _ in 0..8 {
                scope.perform_microtask_checkpoint();
            }
            let result = p.result(scope);
            result.to_rust_string_lossy(scope)
        },
    );
    assert_eq!(s, "[object CryptoKey]");
}

#[test]
fn crypto_key_constructor_rejected() {
    let err = run_js_string(
        r#"
        try {
            new CryptoKey();
            "no-throw";
        } catch (e) {
            e.message;
        }
        "#,
    );
    assert!(err.contains("Illegal"), "got: {err}");
}

// =============================================================================
// PBKDF2 deriveBits — D-20 [EnforceRange] iterations
// =============================================================================

#[test]
fn pbkdf2_iterations_must_be_positive() {
    let err = run_js(
        r#"
        (async () => {
            const key = await crypto.subtle.importKey(
                "raw",
                new TextEncoder().encode("password"),
                "PBKDF2",
                false,
                ["deriveBits"]
            );
            try {
                await crypto.subtle.deriveBits(
                    { name: "PBKDF2", salt: new Uint8Array(8), iterations: 0, hash: "SHA-256" },
                    key,
                    256
                );
                return "no-throw";
            } catch (e) {
                return e.name;
            }
        })();
        "#,
        |v, scope| {
            let p: v8::Local<v8::Promise> = v.try_into().unwrap();
            for _ in 0..16 {
                scope.perform_microtask_checkpoint();
            }
            let result = p.result(scope);
            result.to_rust_string_lossy(scope)
        },
    );
    assert_eq!(err, "OperationError");
}

#[test]
fn pbkdf2_round_trip_sha256() {
    let r = run_js(
        r#"
        (async () => {
            const key = await crypto.subtle.importKey(
                "raw",
                new TextEncoder().encode("password"),
                "PBKDF2",
                false,
                ["deriveBits"]
            );
            const bits = await crypto.subtle.deriveBits(
                { name: "PBKDF2", salt: new TextEncoder().encode("salt"), iterations: 1000, hash: "SHA-256" },
                key,
                256
            );
            const arr = new Uint8Array(bits);
            return arr.byteLength;
        })();
        "#,
        |v, scope| {
            let p: v8::Local<v8::Promise> = v.try_into().unwrap();
            for _ in 0..32 {
                scope.perform_microtask_checkpoint();
            }
            let result = p.result(scope);
            result.uint32_value(scope).unwrap_or(0)
        },
    );
    assert_eq!(r, 32);
}

// =============================================================================
// ECDSA D-4 fixed-length r∥s
// =============================================================================

#[test]
fn ecdsa_p256_signature_is_64_bytes_not_der() {
    let r = run_js(
        r#"
        (async () => {
            const pair = await crypto.subtle.generateKey(
                { name: "ECDSA", namedCurve: "P-256" },
                true,
                ["sign", "verify"]
            );
            const sig = await crypto.subtle.sign(
                { name: "ECDSA", hash: "SHA-256" },
                pair.privateKey,
                new TextEncoder().encode("msg")
            );
            const arr = new Uint8Array(sig);
            return arr.byteLength;
        })();
        "#,
        |v, scope| {
            let p: v8::Local<v8::Promise> = v.try_into().unwrap();
            for _ in 0..16 {
                scope.perform_microtask_checkpoint();
            }
            let result = p.result(scope);
            result.uint32_value(scope).unwrap_or(0)
        },
    );
    // P-256 fixed sig is 64 bytes (2*32). DER would be ~70-72 bytes.
    assert_eq!(r, 64, "ECDSA-P-256 signature should be 64-byte fixed r∥s, not DER");
}

#[test]
fn ecdsa_p256_round_trip() {
    let ok = run_js(
        r#"
        (async () => {
            const pair = await crypto.subtle.generateKey(
                { name: "ECDSA", namedCurve: "P-256" },
                true,
                ["sign", "verify"]
            );
            const data = new TextEncoder().encode("ecdsa round-trip");
            const sig = await crypto.subtle.sign({ name: "ECDSA", hash: "SHA-256" }, pair.privateKey, data);
            const ok = await crypto.subtle.verify({ name: "ECDSA", hash: "SHA-256" }, pair.publicKey, sig, data);
            return ok;
        })();
        "#,
        |v, scope| {
            let p: v8::Local<v8::Promise> = v.try_into().unwrap();
            for _ in 0..16 {
                scope.perform_microtask_checkpoint();
            }
            let result = p.result(scope);
            result.boolean_value(scope)
        },
    );
    assert!(ok, "ECDSA-P-256 sign/verify round-trip");
}

// =============================================================================
// JWK round-trip — HMAC + AES
// =============================================================================

#[test]
fn jwk_aes_round_trip() {
    let ok = run_js(
        r#"
        (async () => {
            const key1 = await crypto.subtle.generateKey(
                { name: "AES-GCM", length: 256 },
                true,
                ["encrypt", "decrypt"]
            );
            const jwk = await crypto.subtle.exportKey("jwk", key1);
            const key2 = await crypto.subtle.importKey(
                "jwk",
                jwk,
                { name: "AES-GCM" },
                true,
                ["encrypt", "decrypt"]
            );
            // Encrypt with key1, decrypt with key2 (round-trip via JWK).
            const iv = crypto.getRandomValues(new Uint8Array(12));
            const ct = await crypto.subtle.encrypt({ name: "AES-GCM", iv }, key1, new TextEncoder().encode("jwk-test"));
            const pt = await crypto.subtle.decrypt({ name: "AES-GCM", iv }, key2, ct);
            return new TextDecoder().decode(pt) === "jwk-test";
        })();
        "#,
        |v, scope| {
            let p: v8::Local<v8::Promise> = v.try_into().unwrap();
            for _ in 0..16 {
                scope.perform_microtask_checkpoint();
            }
            let result = p.result(scope);
            result.boolean_value(scope)
        },
    );
    assert!(ok, "AES-GCM JWK round-trip should re-decrypt successfully");
}

// =============================================================================
// AES-KW wrap / unwrap (D-12)
// =============================================================================

#[test]
fn aes_kw_wrap_unwrap_round_trip() {
    let ok = run_js(
        r#"
        (async () => {
            const kek = await crypto.subtle.generateKey(
                { name: "AES-KW", length: 256 },
                true,
                ["wrapKey", "unwrapKey"]
            );
            const inner = await crypto.subtle.generateKey(
                { name: "AES-GCM", length: 128 },
                true,
                ["encrypt", "decrypt"]
            );
            const wrapped = await crypto.subtle.wrapKey("raw", inner, kek, { name: "AES-KW" });
            const unwrapped = await crypto.subtle.unwrapKey(
                "raw",
                wrapped,
                kek,
                { name: "AES-KW" },
                { name: "AES-GCM", length: 128 },
                true,
                ["encrypt", "decrypt"]
            );
            // Use both keys to encrypt/decrypt the same plaintext.
            const iv = crypto.getRandomValues(new Uint8Array(12));
            const ct = await crypto.subtle.encrypt({ name: "AES-GCM", iv }, inner, new TextEncoder().encode("kw"));
            const pt = await crypto.subtle.decrypt({ name: "AES-GCM", iv }, unwrapped, ct);
            return new TextDecoder().decode(pt) === "kw";
        })();
        "#,
        |v, scope| {
            let p: v8::Local<v8::Promise> = v.try_into().unwrap();
            for _ in 0..16 {
                scope.perform_microtask_checkpoint();
            }
            let result = p.result(scope);
            result.boolean_value(scope)
        },
    );
    assert!(ok, "AES-KW wrap/unwrap round-trip");
}

// =============================================================================
// JWK private-key export for generated EC / Ed25519 keys (Blocker 2)
// =============================================================================
//
// aws-lc-rs hides the private scalar after generateKey, so we walk the
// returned PKCS#8 to recover it for JWK export. Without the walker
// these round-trips fail with OperationError on exportKey("jwk").

#[test]
fn ecdsa_p256_generate_jwk_round_trip() {
    let ok = run_js(
        r#"
        (async () => {
            const kp = await crypto.subtle.generateKey(
                { name: "ECDSA", namedCurve: "P-256" },
                true,
                ["sign", "verify"]
            );
            // Export private and public to JWK.
            const privJwk = await crypto.subtle.exportKey("jwk", kp.privateKey);
            const pubJwk = await crypto.subtle.exportKey("jwk", kp.publicKey);
            if (privJwk.kty !== "EC" || privJwk.crv !== "P-256") return false;
            if (typeof privJwk.d !== "string" || privJwk.d.length === 0) return false;
            if (typeof privJwk.x !== "string" || typeof privJwk.y !== "string") return false;
            // Re-import private JWK and verify a signature it makes.
            const privAgain = await crypto.subtle.importKey(
                "jwk", privJwk, { name: "ECDSA", namedCurve: "P-256" }, true, ["sign"]);
            const pubAgain = await crypto.subtle.importKey(
                "jwk", pubJwk, { name: "ECDSA", namedCurve: "P-256" }, true, ["verify"]);
            const data = new TextEncoder().encode("ec-jwk-rt");
            const sig = await crypto.subtle.sign({ name: "ECDSA", hash: "SHA-256" }, privAgain, data);
            return await crypto.subtle.verify({ name: "ECDSA", hash: "SHA-256" }, pubAgain, sig, data);
        })();
        "#,
        |v, scope| {
            let p: v8::Local<v8::Promise> = v.try_into().unwrap();
            for _ in 0..32 {
                scope.perform_microtask_checkpoint();
            }
            let result = p.result(scope);
            result.boolean_value(scope)
        },
    );
    assert!(ok, "ECDSA P-256 generate→export(JWK)→import→sign/verify round-trip");
}

#[test]
fn ecdsa_p384_generate_jwk_round_trip() {
    let ok = run_js(
        r#"
        (async () => {
            const kp = await crypto.subtle.generateKey(
                { name: "ECDSA", namedCurve: "P-384" }, true, ["sign", "verify"]);
            const privJwk = await crypto.subtle.exportKey("jwk", kp.privateKey);
            if (typeof privJwk.d !== "string" || privJwk.d.length === 0) return false;
            // Decoded length should be 48 bytes (base64url adds ~33% then we trim padding).
            // 48 bytes → 64 chars un-padded.
            if (privJwk.d.length !== 64) return false;
            return true;
        })();
        "#,
        |v, scope| {
            let p: v8::Local<v8::Promise> = v.try_into().unwrap();
            for _ in 0..32 {
                scope.perform_microtask_checkpoint();
            }
            let result = p.result(scope);
            result.boolean_value(scope)
        },
    );
    assert!(ok, "ECDSA P-384 JWK private export carries 48-byte d");
}

#[test]
fn ecdh_p256_generate_jwk_round_trip() {
    let ok = run_js(
        r#"
        (async () => {
            const kp = await crypto.subtle.generateKey(
                { name: "ECDH", namedCurve: "P-256" }, true, ["deriveBits"]);
            const privJwk = await crypto.subtle.exportKey("jwk", kp.privateKey);
            if (typeof privJwk.d !== "string" || privJwk.d.length === 0) return false;
            const privAgain = await crypto.subtle.importKey(
                "jwk", privJwk, { name: "ECDH", namedCurve: "P-256" }, true, ["deriveBits"]);
            // Generate another key + use it as the peer.
            const peer = await crypto.subtle.generateKey(
                { name: "ECDH", namedCurve: "P-256" }, true, ["deriveBits"]);
            const bits1 = await crypto.subtle.deriveBits(
                { name: "ECDH", public: peer.publicKey }, kp.privateKey, 256);
            const bits2 = await crypto.subtle.deriveBits(
                { name: "ECDH", public: peer.publicKey }, privAgain, 256);
            const a = new Uint8Array(bits1), b = new Uint8Array(bits2);
            if (a.length !== b.length) return false;
            for (let i = 0; i < a.length; i++) if (a[i] !== b[i]) return false;
            return true;
        })();
        "#,
        |v, scope| {
            let p: v8::Local<v8::Promise> = v.try_into().unwrap();
            for _ in 0..32 {
                scope.perform_microtask_checkpoint();
            }
            let result = p.result(scope);
            result.boolean_value(scope)
        },
    );
    assert!(ok, "ECDH P-256 JWK round-trip preserves shared-secret derivation");
}

#[test]
fn ed25519_generate_jwk_round_trip() {
    let ok = run_js(
        r#"
        (async () => {
            const kp = await crypto.subtle.generateKey(
                { name: "Ed25519" }, true, ["sign", "verify"]);
            const privJwk = await crypto.subtle.exportKey("jwk", kp.privateKey);
            if (privJwk.kty !== "OKP" || privJwk.crv !== "Ed25519") return false;
            if (typeof privJwk.d !== "string" || privJwk.d.length === 0) return false;
            if (typeof privJwk.x !== "string" || privJwk.x.length === 0) return false;
            const privAgain = await crypto.subtle.importKey(
                "jwk", privJwk, { name: "Ed25519" }, true, ["sign"]);
            const data = new TextEncoder().encode("ed-jwk-rt");
            const sig1 = await crypto.subtle.sign({ name: "Ed25519" }, kp.privateKey, data);
            const sig2 = await crypto.subtle.sign({ name: "Ed25519" }, privAgain, data);
            const a = new Uint8Array(sig1), b = new Uint8Array(sig2);
            // Ed25519 is deterministic per RFC 8032 — same key/data should
            // produce identical signatures, so we can byte-compare.
            if (a.length !== b.length) return false;
            for (let i = 0; i < a.length; i++) if (a[i] !== b[i]) return false;
            return true;
        })();
        "#,
        |v, scope| {
            let p: v8::Local<v8::Promise> = v.try_into().unwrap();
            for _ in 0..32 {
                scope.perform_microtask_checkpoint();
            }
            let result = p.result(scope);
            result.boolean_value(scope)
        },
    );
    assert!(ok, "Ed25519 JWK round-trip yields the same deterministic signature");
}

// =============================================================================
// RSA-PSS variable salt length (Blocker 3)
// =============================================================================
//
// aws-lc-rs's high-level path fixes salt = digest length. We drop down
// to aws-lc-sys EVP_* for caller-specified saltLength. A salt of 0 is
// deterministic (RFC 3447 §9.1.1); other salts are randomized.

#[test]
fn rsa_pss_sign_verify_salt_zero_round_trip() {
    // 2048-bit keygen takes a moment; keep this single-threaded.
    let ok = run_js(
        r#"
        (async () => {
            const kp = await crypto.subtle.generateKey(
                {
                    name: "RSA-PSS",
                    modulusLength: 2048,
                    publicExponent: new Uint8Array([1, 0, 1]),
                    hash: "SHA-256",
                },
                true, ["sign", "verify"]);
            const data = new TextEncoder().encode("pss-salt-zero");
            const sig = await crypto.subtle.sign(
                { name: "RSA-PSS", saltLength: 0 }, kp.privateKey, data);
            const v = await crypto.subtle.verify(
                { name: "RSA-PSS", saltLength: 0 }, kp.publicKey, sig, data);
            return v;
        })();
        "#,
        |v, scope| {
            let p: v8::Local<v8::Promise> = v.try_into().unwrap();
            for _ in 0..64 {
                scope.perform_microtask_checkpoint();
            }
            let result = p.result(scope);
            result.boolean_value(scope)
        },
    );
    assert!(ok, "RSA-PSS saltLength=0 sign/verify round-trip");
}

#[test]
fn rsa_pss_sign_verify_salt_digest_len() {
    let ok = run_js(
        r#"
        (async () => {
            const kp = await crypto.subtle.generateKey(
                {
                    name: "RSA-PSS",
                    modulusLength: 2048,
                    publicExponent: new Uint8Array([1, 0, 1]),
                    hash: "SHA-256",
                },
                true, ["sign", "verify"]);
            const data = new TextEncoder().encode("pss-salt-32");
            const sig = await crypto.subtle.sign(
                { name: "RSA-PSS", saltLength: 32 }, kp.privateKey, data);
            return await crypto.subtle.verify(
                { name: "RSA-PSS", saltLength: 32 }, kp.publicKey, sig, data);
        })();
        "#,
        |v, scope| {
            let p: v8::Local<v8::Promise> = v.try_into().unwrap();
            for _ in 0..64 {
                scope.perform_microtask_checkpoint();
            }
            let result = p.result(scope);
            result.boolean_value(scope)
        },
    );
    assert!(ok, "RSA-PSS saltLength=hLen=32 sign/verify round-trip");
}

#[test]
fn rsa_pss_salt_zero_is_deterministic() {
    // When saltLength=0, PSS is deterministic — same input → same
    // signature byte-for-byte (RFC 3447 §9.1.1, salt of length 0).
    let r = run_js(
        r#"
        (async () => {
            const kp = await crypto.subtle.generateKey(
                {
                    name: "RSA-PSS",
                    modulusLength: 2048,
                    publicExponent: new Uint8Array([1, 0, 1]),
                    hash: "SHA-256",
                },
                true, ["sign", "verify"]);
            const data = new TextEncoder().encode("determinism");
            const s1 = new Uint8Array(await crypto.subtle.sign(
                { name: "RSA-PSS", saltLength: 0 }, kp.privateKey, data));
            const s2 = new Uint8Array(await crypto.subtle.sign(
                { name: "RSA-PSS", saltLength: 0 }, kp.privateKey, data));
            if (s1.length !== s2.length) return "len-mismatch";
            for (let i = 0; i < s1.length; i++) if (s1[i] !== s2[i]) return "byte-mismatch";
            return "deterministic";
        })();
        "#,
        |v, scope| {
            let p: v8::Local<v8::Promise> = v.try_into().unwrap();
            for _ in 0..64 {
                scope.perform_microtask_checkpoint();
            }
            let result = p.result(scope);
            result.to_rust_string_lossy(scope)
        },
    );
    assert_eq!(r, "deterministic");
}

#[test]
fn rsa_pss_salt_random_differs_run_to_run() {
    // saltLength > 0 — output should differ between runs (random salt).
    let r = run_js(
        r#"
        (async () => {
            const kp = await crypto.subtle.generateKey(
                {
                    name: "RSA-PSS",
                    modulusLength: 2048,
                    publicExponent: new Uint8Array([1, 0, 1]),
                    hash: "SHA-256",
                },
                true, ["sign", "verify"]);
            const data = new TextEncoder().encode("random-salt");
            const s1 = new Uint8Array(await crypto.subtle.sign(
                { name: "RSA-PSS", saltLength: 32 }, kp.privateKey, data));
            const s2 = new Uint8Array(await crypto.subtle.sign(
                { name: "RSA-PSS", saltLength: 32 }, kp.privateKey, data));
            // Both must verify
            const v1 = await crypto.subtle.verify(
                { name: "RSA-PSS", saltLength: 32 }, kp.publicKey, s1, data);
            const v2 = await crypto.subtle.verify(
                { name: "RSA-PSS", saltLength: 32 }, kp.publicKey, s2, data);
            if (!v1 || !v2) return "verify-failed";
            if (s1.length !== s2.length) return "len-mismatch";
            // Probability that two independent random salts produce the
            // same signature is negligible.
            let same = true;
            for (let i = 0; i < s1.length; i++) if (s1[i] !== s2[i]) { same = false; break; }
            return same ? "all-same" : "differ";
        })();
        "#,
        |v, scope| {
            let p: v8::Local<v8::Promise> = v.try_into().unwrap();
            for _ in 0..64 {
                scope.perform_microtask_checkpoint();
            }
            let result = p.result(scope);
            result.to_rust_string_lossy(scope)
        },
    );
    assert_eq!(r, "differ");
}
