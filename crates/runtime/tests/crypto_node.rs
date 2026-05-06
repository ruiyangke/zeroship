//! `node:crypto` Stage B smoke tests.
//!
//! Drives the `__zeroship_node_crypto` boundary object directly in an
//! isolated V8 context. No Runtime, no Vite synthetic module — those
//! land on top of this surface.
//!
//! Per `docs/proposals/node-crypto-native.md` §XVI test plan.

#![allow(unsafe_code, missing_debug_implementations)]

use zeroship_runtime::{crypto_native, crypto_node, dom, init_v8};

/// Run a JS snippet in a fresh V8 context with DOMException + native
/// crypto + node:crypto installed. Returns the script's last
/// expression value.
///
/// Earlier versions called `crypto_node::install_globals(scope, global)`
/// which mounted the surface at `globalThis.__zeroship_node_crypto`.
/// The production surface is now the export object of the
/// `node:crypto` SyntheticModule. These tests run raw `script.run`,
/// not the module loader, so we mint the same boundary object
/// directly via `populate()` and stash it under the legacy name —
/// keeps the 97 existing assertions readable; the production path
/// goes through the synthetic module instead.
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
    // node:crypto must populate AFTER WebCrypto so the bridge to
    // globalThis.crypto is wired.
    let ns = v8::Object::new(scope);
    crypto_node::populate(scope, ns);
    let key = v8::String::new(scope, "__zeroship_node_crypto").unwrap();
    global.set(scope, key.into(), ns.into());

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
    run_js(src, |v, _| v.is_true())
}

fn run_js_number(src: &str) -> f64 {
    run_js(src, |v, scope| v.number_value(scope).unwrap_or(f64::NAN))
}

// =============================================================================
// Hash
// =============================================================================

#[test]
fn create_hash_sha256_returns_hex() {
    let result = run_js_string(
        r#"
        const h = __zeroship_node_crypto.createHash('sha256');
        h.update('abc');
        h.digest('hex');
    "#,
    );
    // FIPS 180-2 SHA-256("abc") = ba7816bf8f01cfea414140de5dae2223...
    assert_eq!(
        result,
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
}

#[test]
fn create_hash_sha1_returns_hex() {
    let result = run_js_string(
        r#"
        const h = __zeroship_node_crypto.createHash('sha1');
        h.update('hello');
        h.digest('hex');
    "#,
    );
    // SHA-1("hello") = aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d
    assert_eq!(result, "aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d");
}

#[test]
fn hash_streaming_via_multiple_update() {
    let result = run_js_string(
        r#"
        const h = __zeroship_node_crypto.createHash('sha256');
        h.update('hello ');
        h.update('world');
        h.digest('hex');
    "#,
    );
    // SHA-256("hello world")
    assert_eq!(
        result,
        "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9"
    );
}

#[test]
fn hash_uint8array_input() {
    let result = run_js_string(
        r#"
        const h = __zeroship_node_crypto.createHash('sha256');
        h.update(new Uint8Array([0x61, 0x62, 0x63]));  // "abc"
        h.digest('hex');
    "#,
    );
    assert_eq!(
        result,
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
}

#[test]
fn hash_digest_base64() {
    let result = run_js_string(
        r#"
        const h = __zeroship_node_crypto.createHash('sha256');
        h.update('abc');
        h.digest('base64');
    "#,
    );
    // base64(SHA-256("abc"))
    assert_eq!(result, "ungWv48Bz+pBQUDeXa4iI7ADYaOWF3qctBD/YfIAFa0=");
}

#[test]
fn hash_digest_base64url() {
    let result = run_js_string(
        r#"
        const h = __zeroship_node_crypto.createHash('sha256');
        h.update('abc');
        h.digest('base64url');
    "#,
    );
    // base64url(SHA-256("abc")) — no padding
    assert_eq!(result, "ungWv48Bz-pBQUDeXa4iI7ADYaOWF3qctBD_YfIAFa0");
}

#[test]
fn hash_finalize_twice_throws_err_crypto_hash_finalized() {
    let result = run_js_string(
        r#"
        const h = __zeroship_node_crypto.createHash('sha256');
        h.update('abc');
        h.digest('hex');
        try {
            h.digest('hex');
            'no error';
        } catch (e) {
            e.code || 'no code';
        }
    "#,
    );
    assert_eq!(result, "ERR_CRYPTO_HASH_FINALIZED");
}

#[test]
fn hash_update_after_digest_throws() {
    let result = run_js_string(
        r#"
        const h = __zeroship_node_crypto.createHash('sha256');
        h.update('a');
        h.digest('hex');
        try {
            h.update('b');
            'no error';
        } catch (e) {
            e.code || 'no code';
        }
    "#,
    );
    assert_eq!(result, "ERR_CRYPTO_HASH_FINALIZED");
}

#[test]
fn hash_unknown_algorithm_throws() {
    let result = run_js_string(
        r#"
        try {
            __zeroship_node_crypto.createHash('shaXXX');
            'no error';
        } catch (e) {
            e.code || 'no code';
        }
    "#,
    );
    assert_eq!(result, "ERR_OSSL_EVP_INVALID_DIGEST");
}

#[test]
fn hash_copy_preserves_state() {
    let result = run_js_string(
        r#"
        const a = __zeroship_node_crypto.createHash('sha256');
        a.update('prefix-');
        const b = a.copy();
        a.update('A');
        b.update('B');
        a.digest('hex') + '|' + b.digest('hex');
    "#,
    );
    let parts: Vec<&str> = result.split('|').collect();
    assert_eq!(parts.len(), 2);
    // SHA-256("prefix-A") != SHA-256("prefix-B")
    assert_ne!(parts[0], parts[1]);
    // Spot-check one of them
    assert_eq!(
        parts[0],
        // SHA-256("prefix-A")
        "45393a0161979cb79b98cb35d935ffae018dec01732734e9433ce4e53a6b0575"
    );
}

#[test]
fn hash_sha224_works() {
    let result = run_js_string(
        r#"
        const h = __zeroship_node_crypto.createHash('sha224');
        h.update('abc');
        h.digest('hex');
    "#,
    );
    // RFC 3874 SHA-224("abc") test vector
    assert_eq!(
        result,
        "23097d223405d8228642a477bda255b32aadbce4bda0b3f7e36c9da7"
    );
}

// =============================================================================
// Hmac
// =============================================================================

#[test]
fn create_hmac_sha256_string_key() {
    // RFC 4231 HMAC-SHA-256 test case with key='Jefe', data='what do ya want for nothing?'
    let result = run_js_string(
        r#"
        const h = __zeroship_node_crypto.createHmac('sha256', 'Jefe');
        h.update('what do ya want for nothing?');
        h.digest('hex');
    "#,
    );
    assert_eq!(
        result,
        "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
    );
}

#[test]
fn hmac_uint8array_key() {
    // Key is the byte string "key" as Uint8Array; data is "The quick brown fox..."
    let result = run_js_string(
        r#"
        const h = __zeroship_node_crypto.createHmac('sha256', new Uint8Array([0x6b, 0x65, 0x79]));
        h.update('The quick brown fox jumps over the lazy dog');
        h.digest('hex');
    "#,
    );
    assert_eq!(
        result,
        "f7bc83f430538424b13298e6aa6fb143ef4d59a14946175997479dbc2d1a3cd8"
    );
}

#[test]
fn hmac_streaming() {
    let result = run_js_string(
        r#"
        const h = __zeroship_node_crypto.createHmac('sha256', 'Jefe');
        h.update('what do ya want');
        h.update(' for nothing?');
        h.digest('hex');
    "#,
    );
    assert_eq!(
        result,
        "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
    );
}

#[test]
fn hmac_finalize_twice_throws() {
    let result = run_js_string(
        r#"
        const h = __zeroship_node_crypto.createHmac('sha256', 'k');
        h.update('a');
        h.digest('hex');
        try {
            h.digest('hex');
            'no error';
        } catch (e) {
            e.code || 'no code';
        }
    "#,
    );
    assert_eq!(result, "ERR_CRYPTO_HASH_FINALIZED");
}

#[test]
fn hmac_no_copy_method() {
    // Node does not expose `Hmac.copy()`.
    let result = run_js_bool(
        r#"
        const h = __zeroship_node_crypto.createHmac('sha256', 'k');
        typeof h.copy === 'undefined';
    "#,
    );
    assert!(result);
}

// =============================================================================
// Random
// =============================================================================

#[test]
fn random_uuid_v4_format() {
    let result = run_js_string("__zeroship_node_crypto.randomUUID()");
    // 8-4-4-4-12 hex pattern with version=4 and variant=10xx.
    assert_eq!(result.len(), 36);
    assert!(result.chars().nth(8).unwrap() == '-');
    assert!(result.chars().nth(13).unwrap() == '-');
    assert!(result.chars().nth(18).unwrap() == '-');
    assert!(result.chars().nth(23).unwrap() == '-');
    let version_char = result.chars().nth(14).unwrap();
    assert_eq!(version_char, '4', "version nibble must be 4");
    let variant_char = result.chars().nth(19).unwrap();
    assert!(
        matches!(variant_char, '8' | '9' | 'a' | 'b'),
        "variant must be one of 8,9,a,b (10xx pattern), got {variant_char}"
    );
}

#[test]
fn random_bytes_sync_returns_correct_size() {
    let result = run_js_number(
        r#"
        const b = __zeroship_node_crypto.randomBytes(32);
        b.byteLength;
    "#,
    );
    assert_eq!(result, 32.0);
}

#[test]
fn random_bytes_two_calls_differ() {
    let result = run_js_bool(
        r#"
        const a = __zeroship_node_crypto.randomBytes(16);
        const b = __zeroship_node_crypto.randomBytes(16);
        let same = true;
        for (let i = 0; i < 16; i++) { if (a[i] !== b[i]) { same = false; break; } }
        !same;
    "#,
    );
    assert!(result);
}

#[test]
fn random_fill_sync_writes_bytes() {
    let result = run_js_bool(
        r#"
        const buf = new Uint8Array(16);
        const initial = Array.from(buf).every(b => b === 0);
        __zeroship_node_crypto.randomFillSync(buf);
        const filled = Array.from(buf).some(b => b !== 0);
        initial && filled;
    "#,
    );
    assert!(result);
}

#[test]
fn random_int_in_range() {
    let result = run_js_bool(
        r#"
        let ok = true;
        for (let i = 0; i < 100; i++) {
            const v = __zeroship_node_crypto.randomInt(10, 20);
            if (v < 10 || v >= 20) { ok = false; break; }
        }
        ok;
    "#,
    );
    assert!(result);
}

#[test]
fn random_int_min_default_zero() {
    let result = run_js_bool(
        r#"
        let ok = true;
        for (let i = 0; i < 100; i++) {
            const v = __zeroship_node_crypto.randomInt(50);
            if (v < 0 || v >= 50) { ok = false; break; }
        }
        ok;
    "#,
    );
    assert!(result);
}

// =============================================================================
// timingSafeEqual
// =============================================================================

#[test]
fn timing_safe_equal_equal_bytes() {
    let result = run_js_bool(
        r#"
        const a = new Uint8Array([1, 2, 3, 4]);
        const b = new Uint8Array([1, 2, 3, 4]);
        __zeroship_node_crypto.timingSafeEqual(a, b);
    "#,
    );
    assert!(result);
}

#[test]
fn timing_safe_equal_unequal_bytes() {
    let result = run_js_bool(
        r#"
        const a = new Uint8Array([1, 2, 3, 4]);
        const b = new Uint8Array([1, 2, 3, 5]);
        __zeroship_node_crypto.timingSafeEqual(a, b);
    "#,
    );
    assert!(!result);
}

#[test]
fn timing_safe_equal_different_length_throws() {
    let result = run_js_string(
        r#"
        const a = new Uint8Array([1, 2, 3]);
        const b = new Uint8Array([1, 2, 3, 4]);
        try {
            __zeroship_node_crypto.timingSafeEqual(a, b);
            'no error';
        } catch (e) {
            e.code || 'no code';
        }
    "#,
    );
    assert_eq!(result, "ERR_CRYPTO_TIMING_SAFE_EQUAL_LENGTH");
}

// =============================================================================
// KDFs
// =============================================================================

#[test]
fn pbkdf2_sync_rfc6070_v1() {
    // RFC 6070 PBKDF2-HMAC-SHA-1 vector #1
    let result = run_js_string(
        r#"
        const out = __zeroship_node_crypto.pbkdf2Sync('password', 'salt', 1, 20, 'sha1');
        Array.from(out).map(b => b.toString(16).padStart(2, '0')).join('');
    "#,
    );
    assert_eq!(result, "0c60c80f961f0e71f3a9b524af6012062fe037a6");
}

#[test]
fn pbkdf2_sync_with_buffer_input() {
    let result = run_js_string(
        r#"
        const out = __zeroship_node_crypto.pbkdf2Sync(
            new Uint8Array([0x70, 0x61, 0x73, 0x73, 0x77, 0x6f, 0x72, 0x64]),
            new Uint8Array([0x73, 0x61, 0x6c, 0x74]),
            1, 20, 'sha1');
        Array.from(out).map(b => b.toString(16).padStart(2, '0')).join('');
    "#,
    );
    assert_eq!(result, "0c60c80f961f0e71f3a9b524af6012062fe037a6");
}

#[test]
fn hkdf_sync_returns_array_buffer() {
    let result = run_js_string(
        r#"
        const out = __zeroship_node_crypto.hkdfSync('sha256', 'ikm', 'salt', 'info', 32);
        out.byteLength + ':' + Object.prototype.toString.call(out);
    "#,
    );
    assert_eq!(result, "32:[object ArrayBuffer]");
}

#[test]
fn scrypt_sync_rfc7914_v1() {
    // RFC 7914 §11 vector #1: empty p, empty salt, N=16, r=1, p=1, dkLen=64.
    let result = run_js_string(
        r#"
        const out = __zeroship_node_crypto.scryptSync('', '', 64, { N: 16, r: 1, p: 1 });
        Array.from(out).map(b => b.toString(16).padStart(2, '0')).join('');
    "#,
    );
    assert_eq!(
        result,
        "77d6576238657b203b19ca42c18a0497f16b4844e3074ae8dfdffa3fede21442fcd0069ded0948f8326a753a0fc81f17e8d3e0fb2e0d3628cf35e20c38d18906"
    );
}

#[test]
fn scrypt_sync_default_params() {
    let result = run_js_number(
        r#"
        // No options → defaults N=16384, r=8, p=1, maxmem=32MiB.
        const out = __zeroship_node_crypto.scryptSync('password', 'salt', 32);
        out.byteLength;
    "#,
    );
    assert_eq!(result, 32.0);
}

// =============================================================================
// getHashes / getCurves / getFips
// =============================================================================

#[test]
fn get_hashes_includes_sha256() {
    let result = run_js_bool(
        r#"
        const hashes = __zeroship_node_crypto.getHashes();
        hashes.includes('sha256');
    "#,
    );
    assert!(result);
}

#[test]
fn get_curves_includes_p256() {
    let result = run_js_bool(
        r#"
        const curves = __zeroship_node_crypto.getCurves();
        curves.includes('P-256');
    "#,
    );
    assert!(result);
}

#[test]
fn get_fips_returns_zero() {
    let result = run_js_number("__zeroship_node_crypto.getFips()");
    assert_eq!(result, 0.0);
}

#[test]
fn set_fips_true_throws() {
    let result = run_js_string(
        r#"
        try {
            __zeroship_node_crypto.setFips(true);
            'no error';
        } catch (e) {
            e.code || 'no code';
        }
    "#,
    );
    assert_eq!(result, "ERR_CRYPTO_OPERATION_FAILED");
}

#[test]
fn set_fips_false_no_op() {
    let result = run_js_bool(
        r#"
        __zeroship_node_crypto.setFips(false);
        true;
    "#,
    );
    assert!(result);
}

// =============================================================================
// WebCrypto bridge
// =============================================================================

#[test]
fn webcrypto_bridge_object_identity() {
    let result = run_js_bool(
        r#"
        __zeroship_node_crypto.webcrypto === globalThis.crypto;
    "#,
    );
    assert!(result, "node:crypto.webcrypto must be the same object as globalThis.crypto");
}

#[test]
fn subtle_bridge_identity() {
    let result = run_js_bool(
        r#"
        __zeroship_node_crypto.subtle === globalThis.crypto.subtle;
    "#,
    );
    assert!(result, "node:crypto.subtle must be the same object as globalThis.crypto.subtle");
}

// =============================================================================
// Error codes — npm packages branch on these
// =============================================================================

#[test]
fn err_code_is_string_literal_match() {
    // Sanity-check that e.code is the exact string literal,
    // strictly equal (===) so packages doing `e.code === "ERR_..."`
    // see truthy.
    let result = run_js_bool(
        r#"
        (() => {
            try {
                __zeroship_node_crypto.createHash('shaXXX');
            } catch (e) {
                return e.code === 'ERR_OSSL_EVP_INVALID_DIGEST';
            }
            return false;
        })();
    "#,
    );
    assert!(result);
}

// =============================================================================
// Stage C — KeyObject + Sign/Verify + Cipher/Decipher + keygen
// =============================================================================

#[test]
fn create_secret_key_returns_secret_key_object() {
    let result = run_js_string(
        r#"
        const k = __zeroship_node_crypto.createSecretKey(new Uint8Array([1,2,3,4,5,6,7,8]));
        k.type;
    "#,
    );
    assert_eq!(result, "secret");
}

#[test]
fn create_secret_key_symmetric_key_size() {
    let result = run_js_number(
        r#"
        const k = __zeroship_node_crypto.createSecretKey(new Uint8Array(32));
        k.symmetricKeySize;
    "#,
    );
    assert_eq!(result as u32, 32);
}

#[test]
fn create_secret_key_export_jwk() {
    let result = run_js_string(
        r#"
        const k = __zeroship_node_crypto.createSecretKey(new Uint8Array([1,2,3]));
        const jwk = k.export({ format: 'jwk' });
        jwk.kty;
    "#,
    );
    assert_eq!(result, "oct");
}

#[test]
fn key_object_equals_same_bytes_returns_true() {
    let result = run_js_bool(
        r#"
        const a = __zeroship_node_crypto.createSecretKey(new Uint8Array([1,2,3,4]));
        const b = __zeroship_node_crypto.createSecretKey(new Uint8Array([1,2,3,4]));
        a.equals(b);
    "#,
    );
    assert!(result);
}

#[test]
fn key_object_equals_different_bytes_returns_false() {
    let result = run_js_bool(
        r#"
        const a = __zeroship_node_crypto.createSecretKey(new Uint8Array([1,2,3,4]));
        const b = __zeroship_node_crypto.createSecretKey(new Uint8Array([5,6,7,8]));
        a.equals(b);
    "#,
    );
    assert!(!result);
}

#[test]
fn key_object_equals_different_lengths_returns_false() {
    let result = run_js_bool(
        r#"
        const a = __zeroship_node_crypto.createSecretKey(new Uint8Array([1,2,3,4]));
        const b = __zeroship_node_crypto.createSecretKey(new Uint8Array([1,2,3,4,5]));
        a.equals(b);
    "#,
    );
    assert!(!result);
}

#[test]
fn generate_key_pair_sync_rsa_2048() {
    let result = run_js_string(
        r#"
        const { publicKey, privateKey } = __zeroship_node_crypto.generateKeyPairSync('rsa', { modulusLength: 2048 });
        privateKey.type + ',' + publicKey.type + ',' + privateKey.asymmetricKeyType;
    "#,
    );
    assert_eq!(result, "private,public,rsa");
}

#[test]
fn generate_key_pair_sync_ec_p256() {
    let result = run_js_string(
        r#"
        const { publicKey, privateKey } = __zeroship_node_crypto.generateKeyPairSync('ec', { namedCurve: 'P-256' });
        privateKey.type + ',' + publicKey.asymmetricKeyType;
    "#,
    );
    assert_eq!(result, "private,ec");
}

#[test]
fn generate_key_pair_sync_ed25519() {
    let result = run_js_string(
        r#"
        const { publicKey, privateKey } = __zeroship_node_crypto.generateKeyPairSync('ed25519');
        privateKey.asymmetricKeyType + ',' + publicKey.asymmetricKeyType;
    "#,
    );
    assert_eq!(result, "ed25519,ed25519");
}

#[test]
fn generate_key_sync_hmac_returns_secret() {
    let result = run_js_string(
        r#"
        const k = __zeroship_node_crypto.generateKeySync('hmac', { length: 256 });
        k.type + ',' + k.symmetricKeySize;
    "#,
    );
    assert_eq!(result, "secret,32");
}

#[test]
fn rsa_pkcs1_sign_verify_round_trip() {
    let result = run_js_bool(
        r#"
        const { publicKey, privateKey } =
            __zeroship_node_crypto.generateKeyPairSync('rsa', { modulusLength: 2048 });
        const data = new Uint8Array([1,2,3,4,5,6,7,8]);
        const s = __zeroship_node_crypto.createSign('SHA256');
        s.update(data);
        const sig = s.sign(privateKey);
        const v = __zeroship_node_crypto.createVerify('SHA256');
        v.update(data);
        v.verify(publicKey, sig);
    "#,
    );
    assert!(result);
}

#[test]
fn rsa_pkcs1_verify_returns_false_on_modified_data() {
    let result = run_js_bool(
        r#"
        const { publicKey, privateKey } =
            __zeroship_node_crypto.generateKeyPairSync('rsa', { modulusLength: 2048 });
        const data = new Uint8Array([1,2,3]);
        const tampered = new Uint8Array([9,9,9]);
        const s = __zeroship_node_crypto.createSign('SHA256');
        s.update(data);
        const sig = s.sign(privateKey);
        const v = __zeroship_node_crypto.createVerify('SHA256');
        v.update(tampered);
        v.verify(publicKey, sig) === false;
    "#,
    );
    assert!(result);
}

#[test]
fn ecdsa_p256_sign_verify_round_trip() {
    let result = run_js_bool(
        r#"
        const { publicKey, privateKey } =
            __zeroship_node_crypto.generateKeyPairSync('ec', { namedCurve: 'P-256' });
        const data = new Uint8Array([10,20,30,40]);
        const s = __zeroship_node_crypto.createSign('SHA256');
        s.update(data);
        const sig = s.sign(privateKey);
        const v = __zeroship_node_crypto.createVerify('SHA256');
        v.update(data);
        v.verify(publicKey, sig);
    "#,
    );
    assert!(result);
}

#[test]
fn ed25519_sign_verify_round_trip() {
    let result = run_js_bool(
        r#"
        const { publicKey, privateKey } = __zeroship_node_crypto.generateKeyPairSync('ed25519');
        const data = new Uint8Array([1,2,3,4]);
        // Ed25519: must use crypto.sign / crypto.verify (the createSign path
        // expects an explicit hash, but Ed25519 is null-hash).
        const sig = __zeroship_node_crypto.sign(null, data, privateKey);
        __zeroship_node_crypto.verify(null, data, publicKey, sig);
    "#,
    );
    assert!(result);
}

#[test]
fn one_shot_sign_verify_rsa() {
    let result = run_js_bool(
        r#"
        const { publicKey, privateKey } =
            __zeroship_node_crypto.generateKeyPairSync('rsa', { modulusLength: 2048 });
        const data = new Uint8Array([7,7,7]);
        const sig = __zeroship_node_crypto.sign('SHA256', data, privateKey);
        __zeroship_node_crypto.verify('SHA256', data, publicKey, sig);
    "#,
    );
    assert!(result);
}

#[test]
fn aes_256_gcm_round_trip_with_aad() {
    let result = run_js_bool(
        r#"
        (() => {
            const key = new Uint8Array(32);
            for (let i = 0; i < 32; i++) key[i] = i;
            const iv = new Uint8Array(12);
            const aad = new Uint8Array([1,2,3]);
            const plaintext = new Uint8Array([10,20,30,40,50]);

            const cipher = __zeroship_node_crypto.createCipheriv('aes-256-gcm', key, iv);
            cipher.setAAD(aad);
            cipher.update(plaintext);
            const ct = cipher.final();
            const tag = cipher.getAuthTag();

            const decipher = __zeroship_node_crypto.createDecipheriv('aes-256-gcm', key, iv);
            decipher.setAAD(aad);
            decipher.setAuthTag(tag);
            decipher.update(ct);
            const pt = decipher.final();

            if (pt.length !== plaintext.length) return false;
            for (let i = 0; i < pt.length; i++) {
                if (pt[i] !== plaintext[i]) return false;
            }
            return true;
        })();
    "#,
    );
    assert!(result);
}

#[test]
fn aes_256_cbc_round_trip() {
    let result = run_js_bool(
        r#"
        (() => {
            const key = new Uint8Array(32);
            for (let i = 0; i < 32; i++) key[i] = i;
            const iv = new Uint8Array(16);
            const plaintext = new Uint8Array([1,2,3,4,5,6,7,8,9,10]);

            const cipher = __zeroship_node_crypto.createCipheriv('aes-256-cbc', key, iv);
            cipher.update(plaintext);
            const ct = cipher.final();

            const decipher = __zeroship_node_crypto.createDecipheriv('aes-256-cbc', key, iv);
            decipher.update(ct);
            const pt = decipher.final();

            if (pt.length !== plaintext.length) return false;
            for (let i = 0; i < pt.length; i++) {
                if (pt[i] !== plaintext[i]) return false;
            }
            return true;
        })();
    "#,
    );
    assert!(result);
}

#[test]
fn aes_256_ctr_round_trip() {
    let result = run_js_bool(
        r#"
        (() => {
            const key = new Uint8Array(32);
            for (let i = 0; i < 32; i++) key[i] = i;
            const iv = new Uint8Array(16);
            const plaintext = new Uint8Array([100, 101, 102, 103]);

            const cipher = __zeroship_node_crypto.createCipheriv('aes-256-ctr', key, iv);
            cipher.update(plaintext);
            const ct = cipher.final();

            const decipher = __zeroship_node_crypto.createDecipheriv('aes-256-ctr', key, iv);
            decipher.update(ct);
            const pt = decipher.final();

            if (pt.length !== plaintext.length) return false;
            for (let i = 0; i < pt.length; i++) {
                if (pt[i] !== plaintext[i]) return false;
            }
            return true;
        })();
    "#,
    );
    assert!(result);
}

#[test]
fn chacha20_poly1305_round_trip() {
    let result = run_js_bool(
        r#"
        (() => {
            const key = new Uint8Array(32);
            for (let i = 0; i < 32; i++) key[i] = i;
            const iv = new Uint8Array(12);
            const plaintext = new Uint8Array([1,2,3,4,5]);

            const cipher = __zeroship_node_crypto.createCipheriv('chacha20-poly1305', key, iv);
            cipher.update(plaintext);
            const ct = cipher.final();
            const tag = cipher.getAuthTag();

            const decipher = __zeroship_node_crypto.createDecipheriv('chacha20-poly1305', key, iv);
            decipher.setAuthTag(tag);
            decipher.update(ct);
            const pt = decipher.final();

            if (pt.length !== plaintext.length) return false;
            for (let i = 0; i < pt.length; i++) {
                if (pt[i] !== plaintext[i]) return false;
            }
            return true;
        })();
    "#,
    );
    assert!(result);
}

#[test]
fn create_cipher_throws_unsupported_operation() {
    let result = run_js_string(
        r#"
        try {
            __zeroship_node_crypto.createCipher('aes-256-cbc', 'password');
            'no error';
        } catch (e) {
            e.code || 'no code';
        }
    "#,
    );
    assert_eq!(result, "ERR_CRYPTO_UNSUPPORTED_OPERATION");
}

#[test]
fn get_cipher_info_aes_256_gcm() {
    let result = run_js_string(
        r#"
        const info = __zeroship_node_crypto.getCipherInfo('aes-256-gcm');
        info.name + ',' + info.keyLength + ',' + info.ivLength + ',' + info.mode;
    "#,
    );
    assert_eq!(result, "aes-256-gcm,32,12,gcm");
}

#[test]
fn get_cipher_info_unknown_returns_undefined() {
    let result = run_js_bool(
        r#"
        __zeroship_node_crypto.getCipherInfo('aes-999-xxx') === undefined;
    "#,
    );
    assert!(result);
}

#[test]
fn rsa_public_encrypt_private_decrypt_oaep() {
    let result = run_js_bool(
        r#"
        (() => {
            const { publicKey, privateKey } =
                __zeroship_node_crypto.generateKeyPairSync('rsa', { modulusLength: 2048 });
            const plaintext = new Uint8Array([1,2,3,4,5]);
            const ct = __zeroship_node_crypto.publicEncrypt(
                { key: publicKey, oaepHash: 'SHA-256' }, plaintext);
            const pt = __zeroship_node_crypto.privateDecrypt(
                { key: privateKey, oaepHash: 'SHA-256' }, ct);
            if (pt.length !== plaintext.length) return false;
            for (let i = 0; i < pt.length; i++) {
                if (pt[i] !== plaintext[i]) return false;
            }
            return true;
        })();
    "#,
    );
    assert!(result);
}

#[test]
fn key_object_export_pem_pkcs8() {
    let result = run_js_bool(
        r#"
        const { privateKey } =
            __zeroship_node_crypto.generateKeyPairSync('rsa', { modulusLength: 2048 });
        const pem = privateKey.export({ format: 'pem', type: 'pkcs8' });
        typeof pem === 'string' && pem.includes('-----BEGIN PRIVATE KEY-----');
    "#,
    );
    assert!(result);
}

#[test]
fn key_object_export_pem_spki() {
    let result = run_js_bool(
        r#"
        const { publicKey } =
            __zeroship_node_crypto.generateKeyPairSync('rsa', { modulusLength: 2048 });
        const pem = publicKey.export({ format: 'pem', type: 'spki' });
        typeof pem === 'string' && pem.includes('-----BEGIN PUBLIC KEY-----');
    "#,
    );
    assert!(result);
}

#[test]
fn key_object_round_trip_pem_pkcs8() {
    let result = run_js_bool(
        r#"
        const { privateKey } =
            __zeroship_node_crypto.generateKeyPairSync('rsa', { modulusLength: 2048 });
        const pem = privateKey.export({ format: 'pem', type: 'pkcs8' });
        const reimported = __zeroship_node_crypto.createPrivateKey(pem);
        reimported.type === 'private' && reimported.asymmetricKeyType === 'rsa';
    "#,
    );
    assert!(result);
}

#[test]
fn create_public_key_from_pem_then_verify() {
    let result = run_js_bool(
        r#"
        (() => {
            const { publicKey, privateKey } =
                __zeroship_node_crypto.generateKeyPairSync('rsa', { modulusLength: 2048 });
            const pubPem = publicKey.export({ format: 'pem', type: 'spki' });
            const reimported = __zeroship_node_crypto.createPublicKey(pubPem);
            const data = new Uint8Array([1,2,3,4]);
            const s = __zeroship_node_crypto.createSign('SHA256');
            s.update(data);
            const sig = s.sign(privateKey);
            const v = __zeroship_node_crypto.createVerify('SHA256');
            v.update(data);
            return v.verify(reimported, sig);
        })();
    "#,
    );
    assert!(result);
}

#[test]
fn ec_p256_export_import_round_trip() {
    let result = run_js_bool(
        r#"
        (() => {
            const { publicKey, privateKey } =
                __zeroship_node_crypto.generateKeyPairSync('ec', { namedCurve: 'P-256' });
            const data = new Uint8Array([5,6,7,8]);
            const s = __zeroship_node_crypto.createSign('SHA256');
            s.update(data);
            const sig = s.sign(privateKey);
            const pem = publicKey.export({ format: 'pem', type: 'spki' });
            const reimported = __zeroship_node_crypto.createPublicKey(pem);
            const v = __zeroship_node_crypto.createVerify('SHA256');
            v.update(data);
            return v.verify(reimported, sig);
        })();
    "#,
    );
    assert!(result);
}

#[test]
fn cipher_setaad_on_non_aead_throws() {
    let result = run_js_string(
        r#"
        const key = new Uint8Array(32);
        const iv = new Uint8Array(16);
        const cipher = __zeroship_node_crypto.createCipheriv('aes-256-cbc', key, iv);
        try {
            cipher.setAAD(new Uint8Array([1]));
            'no error';
        } catch (e) {
            e.code || 'no code';
        }
    "#,
    );
    assert_eq!(result, "ERR_CRYPTO_INVALID_STATE");
}

#[test]
fn cipher_invalid_key_length_throws() {
    let result = run_js_string(
        r#"
        try {
            __zeroship_node_crypto.createCipheriv('aes-256-gcm',
                new Uint8Array(8),  // wrong size
                new Uint8Array(12));
            'no error';
        } catch (e) {
            e.code || 'no code';
        }
    "#,
    );
    assert_eq!(result, "ERR_CRYPTO_INVALID_KEYLEN");
}

#[test]
fn unknown_cipher_throws_err_crypto_unknown_cipher() {
    let result = run_js_string(
        r#"
        try {
            __zeroship_node_crypto.createCipheriv('aes-999-xxx',
                new Uint8Array(32), new Uint8Array(16));
            'no error';
        } catch (e) {
            e.code || 'no code';
        }
    "#,
    );
    assert_eq!(result, "ERR_CRYPTO_UNKNOWN_CIPHER");
}
