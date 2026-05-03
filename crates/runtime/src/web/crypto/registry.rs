//! Algorithm registry — operation × algorithm-name → ParamShape.
//!
//! Per `docs/proposals/webcrypto-native.md` §VII (D-8 + D-24).
//! Replaces the JS-side hand-rolled `normalizeAlgorithm`.

#![allow(dead_code)]

use super::key_material::HashAlgo;
use crate::state::OpError;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Operation {
    Encrypt,
    Decrypt,
    Sign,
    Verify,
    Digest,
    GenerateKey,
    ImportKey,
    DeriveBits,
    GetKeyLength,
    WrapKey,
    UnwrapKey,
}

impl Operation {
    pub fn name(self) -> &'static str {
        match self {
            Operation::Encrypt => "encrypt",
            Operation::Decrypt => "decrypt",
            Operation::Sign => "sign",
            Operation::Verify => "verify",
            Operation::Digest => "digest",
            Operation::GenerateKey => "generateKey",
            Operation::ImportKey => "importKey",
            Operation::DeriveBits => "deriveBits",
            Operation::GetKeyLength => "get key length",
            Operation::WrapKey => "wrapKey",
            Operation::UnwrapKey => "unwrapKey",
        }
    }
}

/// Canonical algorithm name (as it appears in spec §§20-34).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlgorithmName {
    RsassaPkcs1v15,
    RsaPss,
    RsaOaep,
    Ecdsa,
    Ecdh,
    Ed25519,
    X25519,
    AesCtr,
    AesCbc,
    AesGcm,
    AesKw,
    Hmac,
    Sha1,
    Sha256,
    Sha384,
    Sha512,
    Hkdf,
    Pbkdf2,
}

impl AlgorithmName {
    /// Spec-canonical name (case-preserved).
    pub fn canonical(self) -> &'static str {
        match self {
            AlgorithmName::RsassaPkcs1v15 => "RSASSA-PKCS1-v1_5",
            AlgorithmName::RsaPss => "RSA-PSS",
            AlgorithmName::RsaOaep => "RSA-OAEP",
            AlgorithmName::Ecdsa => "ECDSA",
            AlgorithmName::Ecdh => "ECDH",
            AlgorithmName::Ed25519 => "Ed25519",
            AlgorithmName::X25519 => "X25519",
            AlgorithmName::AesCtr => "AES-CTR",
            AlgorithmName::AesCbc => "AES-CBC",
            AlgorithmName::AesGcm => "AES-GCM",
            AlgorithmName::AesKw => "AES-KW",
            AlgorithmName::Hmac => "HMAC",
            AlgorithmName::Sha1 => "SHA-1",
            AlgorithmName::Sha256 => "SHA-256",
            AlgorithmName::Sha384 => "SHA-384",
            AlgorithmName::Sha512 => "SHA-512",
            AlgorithmName::Hkdf => "HKDF",
            AlgorithmName::Pbkdf2 => "PBKDF2",
        }
    }

    /// Case-insensitive lookup against the spec name. Spec §18.4.4
    /// step 1 says lookup is case-insensitive.
    pub fn from_spec_name(s: &str) -> Option<Self> {
        // Order matters only for completeness; the match runs to
        // exhaustion and caller takes the first hit.
        const TABLE: &[(&str, AlgorithmName)] = &[
            ("RSASSA-PKCS1-v1_5", AlgorithmName::RsassaPkcs1v15),
            ("RSA-PSS", AlgorithmName::RsaPss),
            ("RSA-OAEP", AlgorithmName::RsaOaep),
            ("ECDSA", AlgorithmName::Ecdsa),
            ("ECDH", AlgorithmName::Ecdh),
            ("Ed25519", AlgorithmName::Ed25519),
            ("X25519", AlgorithmName::X25519),
            ("AES-CTR", AlgorithmName::AesCtr),
            ("AES-CBC", AlgorithmName::AesCbc),
            ("AES-GCM", AlgorithmName::AesGcm),
            ("AES-KW", AlgorithmName::AesKw),
            ("HMAC", AlgorithmName::Hmac),
            ("SHA-1", AlgorithmName::Sha1),
            ("SHA-256", AlgorithmName::Sha256),
            ("SHA-384", AlgorithmName::Sha384),
            ("SHA-512", AlgorithmName::Sha512),
            ("HKDF", AlgorithmName::Hkdf),
            ("PBKDF2", AlgorithmName::Pbkdf2),
        ];
        TABLE
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(s))
            .map(|(_, alg)| *alg)
    }
}

/// Spec §18.4.4: each (op, algName) pair indicates whether the
/// algorithm supports the operation. `None` → NotSupportedError.
pub fn supports(alg: AlgorithmName, op: Operation) -> bool {
    use AlgorithmName as A;
    use Operation as O;
    match (alg, op) {
        // Digest
        (A::Sha1 | A::Sha256 | A::Sha384 | A::Sha512, O::Digest) => true,
        // RSASSA-PKCS1-v1_5
        (A::RsassaPkcs1v15, O::Sign | O::Verify | O::GenerateKey | O::ImportKey) => true,
        // RSA-PSS
        (A::RsaPss, O::Sign | O::Verify | O::GenerateKey | O::ImportKey) => true,
        // RSA-OAEP
        (
            A::RsaOaep,
            O::Encrypt
            | O::Decrypt
            | O::GenerateKey
            | O::ImportKey
            | O::WrapKey
            | O::UnwrapKey,
        ) => true,
        // ECDSA
        (A::Ecdsa, O::Sign | O::Verify | O::GenerateKey | O::ImportKey) => true,
        // ECDH
        (A::Ecdh, O::DeriveBits | O::GenerateKey | O::ImportKey) => true,
        // Ed25519
        (A::Ed25519, O::Sign | O::Verify | O::GenerateKey | O::ImportKey) => true,
        // X25519
        (A::X25519, O::DeriveBits | O::GenerateKey | O::ImportKey) => true,
        // AES-CTR / AES-CBC / AES-GCM
        (
            A::AesCtr | A::AesCbc | A::AesGcm,
            O::Encrypt
            | O::Decrypt
            | O::GenerateKey
            | O::ImportKey
            | O::GetKeyLength
            | O::WrapKey
            | O::UnwrapKey,
        ) => true,
        // AES-KW (no encrypt/decrypt — only wrapKey/unwrapKey)
        (
            A::AesKw,
            O::GenerateKey | O::ImportKey | O::GetKeyLength | O::WrapKey | O::UnwrapKey,
        ) => true,
        // HMAC
        (
            A::Hmac,
            O::Sign | O::Verify | O::GenerateKey | O::ImportKey | O::GetKeyLength,
        ) => true,
        // HKDF / PBKDF2
        (A::Hkdf | A::Pbkdf2, O::DeriveBits | O::ImportKey | O::GetKeyLength) => true,
        _ => false,
    }
}

/// Look up an algorithm name in the registry and assert it supports
/// the given operation. Returns `Err(NotSupportedError)` on miss.
pub fn lookup(s: &str, op: Operation) -> Result<AlgorithmName, OpError> {
    let alg = AlgorithmName::from_spec_name(s).ok_or_else(|| {
        OpError::dom(
            "NotSupportedError",
            format!("Unrecognised algorithm name '{s}'"),
        )
    })?;
    if !supports(alg, op) {
        return Err(OpError::dom(
            "NotSupportedError",
            format!(
                "Algorithm '{}' does not support operation '{}'",
                alg.canonical(),
                op.name()
            ),
        ));
    }
    Ok(alg)
}

/// Spec §18.4.4 "normalize an algorithm" — shallow normalization.
/// Returns `(canonical_name, hash_or_none)` for the common path. The
/// per-op shape parsers (in `ops.rs`) handle the algorithm-specific
/// fields (iv, salt, label, iterations, etc.).
pub struct NormalizedHead {
    pub name: AlgorithmName,
    /// Optional `hash` field, normalized via recursive lookup against
    /// `Operation::Digest`. Present for RSA-PSS, RSA-PKCS1v1_5, RSA-OAEP,
    /// ECDSA, HMAC, HKDF, PBKDF2 importParams.
    pub hash: Option<HashAlgo>,
}

pub fn normalize_head<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    op: Operation,
    alg: v8::Local<v8::Value>,
) -> Result<(NormalizedHead, v8::Local<'s, v8::Object>), OpError> {
    // Spec §18.4.4 step 0: if alg is a DOMString, treat as { name: alg }.
    // Build the alg-as-object via two passes so the compiler can pick a
    // single lifetime for both branches' result.
    let alg_obj: v8::Local<'s, v8::Object>;
    let name: String;
    if alg.is_string() {
        let obj = v8::Object::new(scope);
        let name_key = v8::String::new(scope, "name").unwrap();
        obj.set(scope, name_key.into(), alg);
        name = alg.to_rust_string_lossy(scope);
        alg_obj = obj;
    } else if let Ok(o) = v8::Local::<v8::Object>::try_from(alg) {
        let name_key = v8::String::new(scope, "name").unwrap();
        let v = o.get(scope, name_key.into()).ok_or_else(|| {
            OpError::type_error("Algorithm: missing 'name' field")
        })?;
        if !v.is_string() {
            return Err(OpError::type_error("Algorithm: 'name' must be a string"));
        }
        name = v.to_rust_string_lossy(scope);
        // Reborrow `o` into scope's lifetime so the function signature
        // is satisfied. `Local::new` reattaches a Local to the active
        // scope (which is `'s`).
        alg_obj = v8::Local::new(scope, o);
    } else {
        return Err(OpError::type_error(
            "Algorithm: must be a string or object",
        ));
    }

    let alg_name = lookup(&name, op)?;

    // Recursive HashAlgorithmIdentifier — present for {RSA-PSS, RSA-OAEP,
    // RSASSA-PKCS1v1_5, ECDSA, HMAC, HKDF, PBKDF2}.{import,sign,verify}.
    let hash = read_hash_field(scope, alg_obj)?;

    Ok((
        NormalizedHead {
            name: alg_name,
            hash,
        },
        alg_obj,
    ))
}

/// If the algorithm object has a `hash` field, recursively normalize
/// it via `Operation::Digest` and return the resolved HashAlgo. Returns
/// `Ok(None)` if absent.
fn read_hash_field(
    scope: &mut v8::PinScope,
    alg_obj: v8::Local<v8::Object>,
) -> Result<Option<HashAlgo>, OpError> {
    let key = v8::String::new(scope, "hash").unwrap();
    let v = match alg_obj.get(scope, key.into()) {
        Some(v) => v,
        None => return Ok(None),
    };
    if v.is_undefined() || v.is_null() {
        return Ok(None);
    }
    let hash_name = if v.is_string() {
        v.to_rust_string_lossy(scope)
    } else if let Ok(o) = v8::Local::<v8::Object>::try_from(v) {
        let inner_key = v8::String::new(scope, "name").unwrap();
        let inner = o.get(scope, inner_key.into()).ok_or_else(|| {
            OpError::type_error("Algorithm.hash: missing 'name'")
        })?;
        if !inner.is_string() {
            return Err(OpError::type_error("Algorithm.hash.name must be a string"));
        }
        inner.to_rust_string_lossy(scope)
    } else {
        return Err(OpError::type_error("Algorithm.hash: invalid"));
    };
    HashAlgo::from_str(&hash_name).map(Some).ok_or_else(|| {
        OpError::dom(
            "NotSupportedError",
            format!("Unrecognised hash algorithm '{hash_name}'"),
        )
    })
}

/// Read the `usages` argument as a Vec of KeyUsage. Returns Err on
/// non-array, non-string element, or unknown usage string.
pub fn parse_usages<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    value: v8::Local<v8::Value>,
) -> Result<Vec<super::key_material::KeyUsage>, OpError> {
    use super::key_material::KeyUsage;
    if value.is_undefined() || value.is_null() {
        return Ok(Vec::new());
    }
    let arr = v8::Local::<v8::Array>::try_from(value)
        .map_err(|_| OpError::type_error("usages must be an array"))?;
    let len = arr.length();
    let mut out = Vec::with_capacity(len as usize);
    for i in 0..len {
        let elem = arr.get_index(scope, i).ok_or_else(|| {
            OpError::type_error("usages: element unreadable")
        })?;
        if !elem.is_string() {
            return Err(OpError::type_error("usages must be an array of strings"));
        }
        let s = elem.to_rust_string_lossy(scope);
        let u = KeyUsage::from_str(&s).ok_or_else(|| {
            OpError::type_error(format!("Unknown key usage '{s}'"))
        })?;
        if !out.contains(&u) {
            out.push(u);
        }
    }
    Ok(out)
}
