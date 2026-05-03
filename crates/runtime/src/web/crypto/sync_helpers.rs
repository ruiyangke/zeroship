//! Surviving crypto helpers after WebCrypto v2 (D-23 landing 3).
//!
//! Native `Crypto` / `SubtleCrypto` / `CryptoKey` (`crypto_native/`) own
//! the spec-facing JS surface. This file holds only:
//!
//! 1. `fast_random` — thread-local 4 KB CSPRNG buffer (workerd's
//!    OPENSSL_cleanse pattern), called from `crypto_native::helpers`
//!    and other call sites that need amortised CSPRNG bytes.
//! 2. `crypto_hash_sync_callback` / `crypto_hmac_sync_callback` — the
//!    `__cryptoHashSync` / `__cryptoHmacSync` globals consumed by
//!    `sdks/vite-plugin/src/node-compat.ts` to back `node:crypto`'s
//!    sync `createHash` / `createHmac` paths. Kept as ad-hoc V8
//!    callbacks (rather than `#[v8_class]` ops) because the node-compat
//!    shim wants synchronous returns; a `SubtleCrypto.digest`-style
//!    Promise would change `crypto.createHash().digest()`'s shape.
//!
//! Everything else that used to live here (`crypto_digest`,
//! `crypto_import_key`, `crypto_sign`, ECDH/RSA/AES kernels, the
//! shared `KeyData` store) was deleted with the embed/crypto.js
//! polyfill — native WebCrypto handles those paths now.

use std::cell::RefCell;

// ---------------------------------------------------------------------------
// Thread-local entropy buffer (~4 KB, zeroed-after-dispense per workerd's
// OPENSSL_cleanse pattern). Amortises CSPRNG syscall across many small
// random reads (UUID gen, masking keys, salt, etc.).
// ---------------------------------------------------------------------------

const ENTROPY_BUF_SIZE: usize = 4096;

#[inline(never)]
fn zeroize_slice(buf: &mut [u8]) {
    for byte in buf.iter_mut() {
        // Volatile write so the compiler can't elide the store. Same
        // purpose as workerd's OPENSSL_cleanse: ensure consumed entropy
        // doesn't linger in memory where a side-channel could read it.
        #[allow(unsafe_code)]
        unsafe {
            std::ptr::write_volatile(byte as *mut u8, 0)
        };
    }
    std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
}

struct EntropyBuf {
    store: [u8; ENTROPY_BUF_SIZE],
    pos: usize,
}

impl EntropyBuf {
    fn new() -> Self {
        Self {
            store: [0u8; ENTROPY_BUF_SIZE],
            pos: ENTROPY_BUF_SIZE,
        }
    }

    fn fill(&mut self, out: &mut [u8]) {
        let mut remaining = out.len();
        let mut offset = 0;
        while remaining > 0 {
            if self.pos >= ENTROPY_BUF_SIZE {
                aws_lc_rs::rand::fill(&mut self.store).unwrap();
                self.pos = 0;
            }
            let avail = ENTROPY_BUF_SIZE - self.pos;
            let n = remaining.min(avail);
            out[offset..offset + n].copy_from_slice(&self.store[self.pos..self.pos + n]);
            zeroize_slice(&mut self.store[self.pos..self.pos + n]);
            self.pos += n;
            offset += n;
            remaining -= n;
        }
    }
}

thread_local! {
    static ENTROPY: RefCell<EntropyBuf> = RefCell::new(EntropyBuf::new());
}

pub(crate) fn fast_random(out: &mut [u8]) {
    ENTROPY.with(|e| e.borrow_mut().fill(out));
}

// ---------------------------------------------------------------------------
// Helpers shared by the sync hash/HMAC callbacks below.
// ---------------------------------------------------------------------------

const HEX: &[u8; 16] = b"0123456789abcdef";

fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

/// Extract a V8 value as `Vec<u8>`:
///   - ArrayBufferView / ArrayBuffer → copy backing store bytes
///   - anything else → UTF-8 encode the string representation
fn extract_bytes(scope: &mut v8::PinScope, val: v8::Local<v8::Value>) -> Vec<u8> {
    if let Ok(view) = v8::Local::<v8::ArrayBufferView>::try_from(val) {
        let mut buf = vec![0u8; view.byte_length()];
        view.copy_contents(&mut buf);
        return buf;
    }
    if let Ok(ab) = v8::Local::<v8::ArrayBuffer>::try_from(val) {
        let store = ab.get_backing_store();
        let mut buf = vec![0u8; ab.byte_length()];
        for (i, b) in buf.iter_mut().enumerate() {
            *b = store[i].get();
        }
        return buf;
    }
    val.to_rust_string_lossy(scope).into_bytes()
}

// ---------------------------------------------------------------------------
// `__cryptoHashSync(algorithm, data) → hex string`
// ---------------------------------------------------------------------------

/// Synchronous hash for node:crypto polyfill. Accepts algorithm name
/// ("sha256", "sha-256", "sha1", "sha-1", "sha384", "sha-384",
/// "sha512", "sha-512") and data as either a string or Uint8Array.
pub(crate) fn crypto_hash_sync_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    if args.length() < 2 {
        let msg = v8::String::new(scope, "__cryptoHashSync: expected (algorithm, data)").unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        scope.throw_exception(exc);
        return;
    }

    let algorithm = args.get(0).to_rust_string_lossy(scope);
    let data = extract_bytes(scope, args.get(1));

    let alg: &aws_lc_rs::digest::Algorithm = match algorithm.to_lowercase().as_str() {
        "sha256" | "sha-256" => &aws_lc_rs::digest::SHA256,
        "sha1" | "sha-1" => &aws_lc_rs::digest::SHA1_FOR_LEGACY_USE_ONLY,
        "sha384" | "sha-384" => &aws_lc_rs::digest::SHA384,
        "sha512" | "sha-512" => &aws_lc_rs::digest::SHA512,
        other => {
            let msg = v8::String::new(
                scope,
                &format!("__cryptoHashSync: unsupported algorithm: {other}"),
            )
            .unwrap();
            let exc = v8::Exception::error(scope, msg);
            scope.throw_exception(exc);
            return;
        }
    };

    let digest = aws_lc_rs::digest::digest(alg, &data);
    let hex = hex_encode(digest.as_ref());
    let result = v8::String::new(scope, &hex).unwrap();
    rv.set(result.into());
}

// ---------------------------------------------------------------------------
// `__cryptoHmacSync(algorithm, key, data) → hex string`
// ---------------------------------------------------------------------------

/// Synchronous HMAC for node:crypto polyfill. Accepts algorithm name,
/// key (string or Uint8Array) and data (string or Uint8Array).
/// Returns hex-encoded HMAC tag.
pub(crate) fn crypto_hmac_sync_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    if args.length() < 3 {
        let msg =
            v8::String::new(scope, "__cryptoHmacSync: expected (algorithm, key, data)").unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        scope.throw_exception(exc);
        return;
    }

    let algorithm = args.get(0).to_rust_string_lossy(scope);
    let key_bytes = extract_bytes(scope, args.get(1));
    let data = extract_bytes(scope, args.get(2));

    let alg: aws_lc_rs::hmac::Algorithm = match algorithm.to_lowercase().as_str() {
        "sha256" | "sha-256" => aws_lc_rs::hmac::HMAC_SHA256,
        "sha1" | "sha-1" => aws_lc_rs::hmac::HMAC_SHA1_FOR_LEGACY_USE_ONLY,
        "sha384" | "sha-384" => aws_lc_rs::hmac::HMAC_SHA384,
        "sha512" | "sha-512" => aws_lc_rs::hmac::HMAC_SHA512,
        other => {
            let msg = v8::String::new(
                scope,
                &format!("__cryptoHmacSync: unsupported algorithm: {other}"),
            )
            .unwrap();
            let exc = v8::Exception::error(scope, msg);
            scope.throw_exception(exc);
            return;
        }
    };

    let key = aws_lc_rs::hmac::Key::new(alg, &key_bytes);
    let tag = aws_lc_rs::hmac::sign(&key, &data);
    let hex = hex_encode(tag.as_ref());
    let result = v8::String::new(scope, &hex).unwrap();
    rv.set(result.into());
}
