//! `CryptoKey` storage primitives — the spec's `[[type]]`,
//! `[[extractable]]`, `[[algorithm]]`, `[[usages]]`, `[[handle]]`
//! internal slots, modelled as Rust enums and plain fields.
//!
//! Per `docs/proposals/webcrypto-native.md` §V.3.

#![allow(dead_code)]

// ---------------------------------------------------------------------------
// Branded box (D-10) — unspoofable instanceof check
// ---------------------------------------------------------------------------

/// Tag byte at the head of every `Box<BrandedBox<CryptoKeyState>>`.
/// The brand check (`crypto_key::is_crypto_key`) reads the first byte
/// of the External pointer and compares against this constant. Spoofing
/// requires an attacker to construct a V8 External pointing at memory
/// with the right tag byte — equivalent to memory-corruption-grade
/// access in a single-isolate runtime.
pub const CRYPTO_KEY_TAG: u8 = 0xC1;

/// `#[repr(C)]` wrapper that puts the tag byte at offset 0, before the
/// boxed body. Read via `*(ptr as *const u8)` from the brand check.
#[repr(C)]
pub struct BrandedBox<T> {
    pub tag: u8,
    pub body: T,
}

impl<T> BrandedBox<T> {
    pub fn new(body: T) -> Self {
        BrandedBox {
            tag: CRYPTO_KEY_TAG,
            body,
        }
    }
}

// ---------------------------------------------------------------------------
// CryptoKey state — the spec [[type]] / [[extractable]] / [[algorithm]] /
// [[usages]] / [[handle]] slots.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyType {
    Public,
    Private,
    Secret,
}

impl KeyType {
    pub fn as_str(self) -> &'static str {
        match self {
            KeyType::Public => "public",
            KeyType::Private => "private",
            KeyType::Secret => "secret",
        }
    }
}

/// Spec §13 `KeyUsage` enumeration. All eight values are valid for at
/// least one algorithm; per-algorithm filters reject usages outside
/// the relevant subset (e.g. AES-GCM rejects `sign`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KeyUsage {
    Encrypt,
    Decrypt,
    Sign,
    Verify,
    DeriveKey,
    DeriveBits,
    WrapKey,
    UnwrapKey,
}

impl KeyUsage {
    pub fn as_str(self) -> &'static str {
        match self {
            KeyUsage::Encrypt => "encrypt",
            KeyUsage::Decrypt => "decrypt",
            KeyUsage::Sign => "sign",
            KeyUsage::Verify => "verify",
            KeyUsage::DeriveKey => "deriveKey",
            KeyUsage::DeriveBits => "deriveBits",
            KeyUsage::WrapKey => "wrapKey",
            KeyUsage::UnwrapKey => "unwrapKey",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        Some(match s {
            "encrypt" => KeyUsage::Encrypt,
            "decrypt" => KeyUsage::Decrypt,
            "sign" => KeyUsage::Sign,
            "verify" => KeyUsage::Verify,
            "deriveKey" => KeyUsage::DeriveKey,
            "deriveBits" => KeyUsage::DeriveBits,
            "wrapKey" => KeyUsage::WrapKey,
            "unwrapKey" => KeyUsage::UnwrapKey,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyFormat {
    Raw,
    Spki,
    Pkcs8,
    Jwk,
}

impl KeyFormat {
    pub fn from_str(s: &str) -> Option<Self> {
        Some(match s {
            "raw" => KeyFormat::Raw,
            "spki" => KeyFormat::Spki,
            "pkcs8" => KeyFormat::Pkcs8,
            "jwk" => KeyFormat::Jwk,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HashAlgo {
    Sha1,
    Sha256,
    Sha384,
    Sha512,
}

impl HashAlgo {
    pub fn from_str(s: &str) -> Option<Self> {
        // Case-insensitive per spec §18.4.4 step 1 (algorithm names are
        // matched case-insensitively against the registry).
        if s.eq_ignore_ascii_case("SHA-1") {
            Some(HashAlgo::Sha1)
        } else if s.eq_ignore_ascii_case("SHA-256") {
            Some(HashAlgo::Sha256)
        } else if s.eq_ignore_ascii_case("SHA-384") {
            Some(HashAlgo::Sha384)
        } else if s.eq_ignore_ascii_case("SHA-512") {
            Some(HashAlgo::Sha512)
        } else {
            None
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            HashAlgo::Sha1 => "SHA-1",
            HashAlgo::Sha256 => "SHA-256",
            HashAlgo::Sha384 => "SHA-384",
            HashAlgo::Sha512 => "SHA-512",
        }
    }

    /// Output size in bytes.
    pub fn digest_len(self) -> usize {
        match self {
            HashAlgo::Sha1 => 20,
            HashAlgo::Sha256 => 32,
            HashAlgo::Sha384 => 48,
            HashAlgo::Sha512 => 64,
        }
    }

    /// Block size in bits — used by HMAC `generateKey` default
    /// per spec §31.4.3 step 2 (D-19).
    pub fn block_size_bits(self) -> u32 {
        match self {
            HashAlgo::Sha1 | HashAlgo::Sha256 => 512,
            HashAlgo::Sha384 | HashAlgo::Sha512 => 1024,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NamedCurve {
    P256,
    P384,
    P521,
}

impl NamedCurve {
    pub fn from_str(s: &str) -> Option<Self> {
        Some(match s {
            "P-256" => NamedCurve::P256,
            "P-384" => NamedCurve::P384,
            "P-521" => NamedCurve::P521,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            NamedCurve::P256 => "P-256",
            NamedCurve::P384 => "P-384",
            NamedCurve::P521 => "P-521",
        }
    }

    /// Curve-order length in bytes (n in spec §23.7.1 step "Convert r
    /// to a byte sequence of length n").
    pub fn order_len(self) -> usize {
        match self {
            NamedCurve::P256 => 32,
            NamedCurve::P384 => 48,
            // P-521 = 521 bits = 66 bytes (spec rounds UP, not 65).
            NamedCurve::P521 => 66,
        }
    }
}

// ---------------------------------------------------------------------------
// KeyAlgorithm — the [[algorithm]] slot's value-shape
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub enum KeyAlgorithm {
    Aes(AesKeyAlgorithm),
    Hmac(HmacKeyAlgorithm),
    RsaHashed(RsaHashedKeyAlgorithm),
    Ec(EcKeyAlgorithm),
    Ed25519,
    X25519,
    Hkdf,
    Pbkdf2,
}

impl KeyAlgorithm {
    /// Spec algorithm name (e.g. `"AES-GCM"`, `"RSA-PSS"`).
    pub fn name(&self) -> &'static str {
        match self {
            KeyAlgorithm::Aes(a) => a.name,
            KeyAlgorithm::Hmac(_) => "HMAC",
            KeyAlgorithm::RsaHashed(r) => r.name,
            KeyAlgorithm::Ec(e) => e.name,
            KeyAlgorithm::Ed25519 => "Ed25519",
            KeyAlgorithm::X25519 => "X25519",
            KeyAlgorithm::Hkdf => "HKDF",
            KeyAlgorithm::Pbkdf2 => "PBKDF2",
        }
    }
}

#[derive(Debug, Clone)]
pub struct AesKeyAlgorithm {
    pub name: &'static str, // "AES-CTR" | "AES-CBC" | "AES-GCM" | "AES-KW"
    pub length: u32,
}

#[derive(Debug, Clone)]
pub struct HmacKeyAlgorithm {
    pub hash: HashAlgo,
    pub length: u32, // bits
}

#[derive(Debug, Clone)]
pub struct RsaHashedKeyAlgorithm {
    pub name: &'static str, // "RSASSA-PKCS1-v1_5" | "RSA-PSS" | "RSA-OAEP"
    pub modulus_length: u32,
    pub public_exponent: Vec<u8>, // big-endian
    pub hash: HashAlgo,
}

#[derive(Debug, Clone)]
pub struct EcKeyAlgorithm {
    pub name: &'static str, // "ECDSA" | "ECDH"
    pub named_curve: NamedCurve,
}

// ---------------------------------------------------------------------------
// KeyMaterial — the [[handle]] slot's actual key bytes
// ---------------------------------------------------------------------------

/// RSA private-key components (RFC 7518 §6.3.2 names).
#[derive(Debug, Clone)]
pub struct RsaPrivateComponents {
    pub n: Vec<u8>,
    pub e: Vec<u8>,
    pub d: Vec<u8>,
    pub p: Vec<u8>,
    pub q: Vec<u8>,
    pub dp: Vec<u8>,
    pub dq: Vec<u8>,
    pub qi: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct RsaPublicComponents {
    pub n: Vec<u8>,
    pub e: Vec<u8>,
}

#[derive(Debug, Clone)]
pub enum KeyMaterial {
    /// Symmetric (HMAC, AES-*, HKDF, PBKDF2).
    Symmetric(Vec<u8>),

    EcPrivate {
        pkcs8_der: Vec<u8>,
        /// Raw scalar `d` (curve-order-byte-length).
        raw_d: Vec<u8>,
        /// Uncompressed public point `0x04 || x || y` (so we can derive
        /// the public half on demand). Length = 1 + 2*n.
        raw_xy: Vec<u8>,
    },
    EcPublic {
        spki_der: Vec<u8>,
        /// Uncompressed point `0x04 || x || y`.
        raw_xy: Vec<u8>,
    },

    RsaPrivate {
        pkcs8_der: Vec<u8>,
        components: RsaPrivateComponents,
    },
    RsaPublic {
        spki_der: Vec<u8>,
        components: RsaPublicComponents,
    },

    Ed25519Private {
        pkcs8_der: Vec<u8>,
        /// 32-byte seed (RFC 8032 §5.1.5 “private key”).
        raw_d: [u8; 32],
        /// 32-byte public key (paired so the wrapper can hand out
        /// public-half exports + JWK encodes).
        raw_x: [u8; 32],
    },
    Ed25519Public {
        spki_der: Vec<u8>,
        raw_x: [u8; 32],
    },

    X25519Private {
        pkcs8_der: Vec<u8>,
        raw_d: [u8; 32],
        raw_x: [u8; 32],
    },
    X25519Public {
        spki_der: Vec<u8>,
        raw_x: [u8; 32],
    },
}

// ---------------------------------------------------------------------------
// CryptoKeyState — the boxed struct stored in V8 internal field 0
// ---------------------------------------------------------------------------

pub struct CryptoKeyState {
    pub key_type: KeyType,
    pub extractable: bool,
    pub algorithm: KeyAlgorithm,
    pub usages: Vec<KeyUsage>,
    pub material: KeyMaterial,
}

impl CryptoKeyState {
    pub fn check_usage(&self, op: KeyUsage) -> Result<(), crate::state::OpError> {
        if !self.usages.contains(&op) {
            return Err(crate::state::OpError::dom(
                "InvalidAccessError",
                format!("Key usage '{}' not allowed for this operation", op.as_str()),
            ));
        }
        Ok(())
    }
}
