mod common;
use common::*;

#[test]
fn crypto_random_uuid() {
    let r = dispatch(m(r#"export function test() {
        var id1 = crypto.randomUUID();
        var id2 = crypto.randomUUID();
        return { id1, id2, different: id1 !== id2, format: /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/.test(id1) };
    }"#), "test", "[]").unwrap();
    assert!(r.json.contains("\"different\":true"), "got: {}", r.json);
    assert!(r.json.contains("\"format\":true"), "got: {}", r.json);
}

#[test]
fn crypto_get_random_values() {
    let r = dispatch(m(r#"
        export function test() {
            var buf = new Uint8Array(16);
            crypto.getRandomValues(buf);
            var nonzero = 0;
            for (var i = 0; i < buf.length; i++) { if (buf[i] !== 0) nonzero++; }
            return nonzero > 0 ? "ok" : "all_zeros";
        }
    "#), "test", "[]").unwrap();
    assert!(r.json.contains("ok"), "got: {}", r.json);
}

#[test]
fn crypto_subtle_digest_sha256() {
    let r = dispatch(m(r#"
        export async function test() {
            var data = new TextEncoder().encode("hello");
            var hash = await crypto.subtle.digest("SHA-256", data);
            var bytes = new Uint8Array(hash);
            var hex = "";
            for (var i = 0; i < bytes.length; i++) hex += ("0" + bytes[i].toString(16)).slice(-2);
            return hex;
        }
    "#), "test", "[]").unwrap();
    assert!(r.json.contains("2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"), "got: {}", r.json);
}

#[test]
fn crypto_subtle_digest_sha512() {
    let r = dispatch(m(r#"
        export async function test() {
            var data = new TextEncoder().encode("hello");
            var hash = await crypto.subtle.digest("SHA-512", data);
            return new Uint8Array(hash).length;
        }
    "#), "test", "[]").unwrap();
    assert!(r.json.contains("64"), "SHA-512 should produce 64 bytes, got: {}", r.json);
}

#[test]
fn crypto_import_export_hmac_raw() {
    let r = dispatch(m(r#"
        export async function test() {
            var raw = new Uint8Array([1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16]);
            var key = await crypto.subtle.importKey("raw", raw, {name: "HMAC", hash: "SHA-256"}, true, ["sign", "verify"]);
            if (key.type !== "secret") return "wrong type: " + key.type;
            if (!key.extractable) return "not extractable";
            var exported = await crypto.subtle.exportKey("raw", key);
            var arr = new Uint8Array(exported);
            if (arr.length !== 16) return "wrong length: " + arr.length;
            for (var i = 0; i < 16; i++) { if (arr[i] !== i+1) return "mismatch at " + i; }
            return "ok";
        }
    "#), "test", "[]").unwrap();
    assert!(r.json.contains("ok"), "got: {}", r.json);
}

#[test]
fn crypto_generate_hmac_key() {
    // Per W3C WebCrypto §31.4.3 step 2: when `length` is omitted, the
    // generated HMAC key uses the hash function's *block size* in bits
    // (SHA-256 = 512 bits = 64 bytes), not the digest length. The
    // legacy polyfill returned 32 bytes; the native impl is spec-correct.
    let r = dispatch(m(r#"
        export async function test() {
            var key = await crypto.subtle.generateKey({name: "HMAC", hash: "SHA-256"}, true, ["sign", "verify"]);
            if (key.type !== "secret") return "wrong type";
            var exported = await crypto.subtle.exportKey("raw", key);
            if (new Uint8Array(exported).length !== 64) return "wrong length: " + new Uint8Array(exported).length;
            return "ok";
        }
    "#), "test", "[]").unwrap();
    assert!(r.json.contains("ok"), "got: {}", r.json);
}

#[test]
fn crypto_generate_ecdsa_keypair() {
    let r = dispatch(m(r#"
        export async function test() {
            var kp = await crypto.subtle.generateKey({name: "ECDSA", namedCurve: "P-256"}, true, ["sign", "verify"]);
            if (!kp.publicKey || !kp.privateKey) return "no keypair";
            if (kp.publicKey.type !== "public") return "wrong pub type";
            if (kp.privateKey.type !== "private") return "wrong priv type";
            return "ok";
        }
    "#), "test", "[]").unwrap();
    assert!(r.json.contains("ok"), "got: {}", r.json);
}

#[test]
fn crypto_generate_aes_key() {
    let r = dispatch(m(r#"
        export async function test() {
            var key = await crypto.subtle.generateKey({name: "AES-GCM", length: 256}, true, ["encrypt", "decrypt"]);
            if (key.type !== "secret") return "wrong type";
            var raw = await crypto.subtle.exportKey("raw", key);
            if (new Uint8Array(raw).length !== 32) return "wrong length";
            return "ok";
        }
    "#), "test", "[]").unwrap();
    assert!(r.json.contains("ok"), "got: {}", r.json);
}

#[test]
fn crypto_hmac_sign_verify() {
    let r = dispatch(m(r#"
        export async function test() {
            var raw = new TextEncoder().encode("my-secret-key-for-hmac-test!!");
            var key = await crypto.subtle.importKey("raw", raw, {name: "HMAC", hash: "SHA-256"}, false, ["sign", "verify"]);
            var data = new TextEncoder().encode("hello world");
            var sig = await crypto.subtle.sign("HMAC", key, data);
            var valid = await crypto.subtle.verify("HMAC", key, sig, data);
            if (!valid) return "verify failed";
            var bad = await crypto.subtle.verify("HMAC", key, sig, new TextEncoder().encode("tampered"));
            if (bad) return "tampered verify should fail";
            return "ok";
        }
    "#), "test", "[]").unwrap();
    assert!(r.json.contains("ok"), "got: {}", r.json);
}

#[test]
fn crypto_ecdsa_sign_verify() {
    let r = dispatch(m(r#"
        export async function test() {
            var kp = await crypto.subtle.generateKey({name: "ECDSA", namedCurve: "P-256"}, false, ["sign", "verify"]);
            var data = new TextEncoder().encode("test message");
            var sig = await crypto.subtle.sign({name: "ECDSA", hash: "SHA-256"}, kp.privateKey, data);
            var valid = await crypto.subtle.verify({name: "ECDSA", hash: "SHA-256"}, kp.publicKey, sig, data);
            if (!valid) return "verify failed";
            return "ok";
        }
    "#), "test", "[]").unwrap();
    assert!(r.json.contains("ok"), "got: {}", r.json);
}

#[test]
fn crypto_ed25519_sign_verify() {
    let r = dispatch(m(r#"
        export async function test() {
            var kp = await crypto.subtle.generateKey({name: "Ed25519"}, false, ["sign", "verify"]);
            var data = new TextEncoder().encode("ed25519 test");
            var sig = await crypto.subtle.sign("Ed25519", kp.privateKey, data);
            var valid = await crypto.subtle.verify("Ed25519", kp.publicKey, sig, data);
            if (!valid) return "verify failed";
            return "ok";
        }
    "#), "test", "[]").unwrap();
    assert!(r.json.contains("ok"), "got: {}", r.json);
}

#[test]
fn crypto_aes_gcm_encrypt_decrypt() {
    let r = dispatch(m(r#"
        export async function test() {
            var key = await crypto.subtle.generateKey({name: "AES-GCM", length: 256}, true, ["encrypt", "decrypt"]);
            var iv = new Uint8Array(12);
            crypto.getRandomValues(iv);
            var data = new TextEncoder().encode("secret message");
            var encrypted = await crypto.subtle.encrypt({name: "AES-GCM", iv: iv}, key, data);
            var decrypted = await crypto.subtle.decrypt({name: "AES-GCM", iv: iv}, key, encrypted);
            var text = new TextDecoder().decode(new Uint8Array(decrypted));
            return text === "secret message" ? "ok" : "mismatch: " + text;
        }
    "#), "test", "[]").unwrap();
    assert!(r.json.contains("ok"), "got: {}", r.json);
}

#[test]
fn crypto_aes_cbc_encrypt_decrypt() {
    let r = dispatch(m(r#"
        export async function test() {
            var key = await crypto.subtle.generateKey({name: "AES-CBC", length: 256}, true, ["encrypt", "decrypt"]);
            var iv = new Uint8Array(16);
            crypto.getRandomValues(iv);
            var data = new TextEncoder().encode("hello cbc");
            var encrypted = await crypto.subtle.encrypt({name: "AES-CBC", iv: iv}, key, data);
            var decrypted = await crypto.subtle.decrypt({name: "AES-CBC", iv: iv}, key, encrypted);
            var text = new TextDecoder().decode(new Uint8Array(decrypted));
            return text === "hello cbc" ? "ok" : "mismatch: " + text;
        }
    "#), "test", "[]").unwrap();
    assert!(r.json.contains("ok"), "got: {}", r.json);
}

#[test]
fn crypto_hkdf_derive_bits() {
    let r = dispatch(m(r#"
        export async function test() {
            var keyMaterial = new TextEncoder().encode("input-key-material");
            var key = await crypto.subtle.importKey("raw", keyMaterial, "HKDF", false, ["deriveBits"]);
            var salt = new TextEncoder().encode("salt-value");
            var info = new TextEncoder().encode("info-value");
            var bits = await crypto.subtle.deriveBits(
                {name: "HKDF", hash: "SHA-256", salt: salt, info: info},
                key, 256
            );
            if (new Uint8Array(bits).length !== 32) return "wrong length";
            return "ok";
        }
    "#), "test", "[]").unwrap();
    assert!(r.json.contains("ok"), "got: {}", r.json);
}

#[test]
fn crypto_pbkdf2_derive_bits() {
    let r = dispatch(m(r#"
        export async function test() {
            var password = new TextEncoder().encode("password");
            var key = await crypto.subtle.importKey("raw", password, "PBKDF2", false, ["deriveBits"]);
            var salt = new TextEncoder().encode("salt");
            var bits = await crypto.subtle.deriveBits(
                {name: "PBKDF2", hash: "SHA-256", salt: salt, iterations: 100000},
                key, 256
            );
            if (new Uint8Array(bits).length !== 32) return "wrong length";
            return "ok";
        }
    "#), "test", "[]").unwrap();
    assert!(r.json.contains("ok"), "got: {}", r.json);
}

#[test]
fn crypto_derive_key_hkdf_to_aes() {
    let r = dispatch(m(r#"
        export async function test() {
            var keyMaterial = new TextEncoder().encode("master-secret");
            var baseKey = await crypto.subtle.importKey("raw", keyMaterial, "HKDF", false, ["deriveKey"]);
            var aesKey = await crypto.subtle.deriveKey(
                {name: "HKDF", hash: "SHA-256", salt: new Uint8Array(16), info: new TextEncoder().encode("aes-key")},
                baseKey,
                {name: "AES-GCM", length: 256},
                true, ["encrypt", "decrypt"]
            );
            if (aesKey.type !== "secret") return "wrong type";
            var raw = await crypto.subtle.exportKey("raw", aesKey);
            if (new Uint8Array(raw).length !== 32) return "wrong length";
            return "ok";
        }
    "#), "test", "[]").unwrap();
    assert!(r.json.contains("ok"), "got: {}", r.json);
}
