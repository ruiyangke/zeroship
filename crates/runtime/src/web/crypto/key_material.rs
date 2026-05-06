//! `CryptoKey` storage primitives — the spec's `[[type]]`,
//! `[[extractable]]`, `[[algorithm]]`, `[[usages]]`, `[[handle]]`
//! internal slots, modelled as Rust enums and plain fields.
//!
//! Per `docs/proposals/webcrypto-native.md` §V.3.

#![allow(dead_code)]

use zeroship_runtime_macros::WebIdlEnum;

// ---------------------------------------------------------------------------
// Branded box — unspoofable instanceof check
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

/// Spec §13 `KeyType` enumeration. WebIDL names are kebab-cased from
/// the variant identifier — `Public/Private/Secret` map cleanly to
/// `public/private/secret` so no per-variant `#[webidl_name]` overrides
/// are needed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, WebIdlEnum)]
pub enum KeyType {
    Public,
    Private,
    Secret,
}

/// Spec §13 `KeyUsage` enumeration. All eight values are valid for at
/// least one algorithm; per-algorithm filters reject usages outside
/// the relevant subset (e.g. AES-GCM rejects `sign`).
///
/// WebIDL names are camelCase per spec — the auto-kebab rule would
/// produce `derive-key` etc., so multi-word variants get explicit
/// `#[webidl_name = ...]` overrides.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, WebIdlEnum)]
pub enum KeyUsage {
    Encrypt,
    Decrypt,
    Sign,
    Verify,
    #[webidl_name = "deriveKey"]
    DeriveKey,
    #[webidl_name = "deriveBits"]
    DeriveBits,
    #[webidl_name = "wrapKey"]
    WrapKey,
    #[webidl_name = "unwrapKey"]
    UnwrapKey,
}

/// Spec §13 `KeyFormat` enumeration. WebIDL names are kebab-cased
/// from variant idents — single-word PascalCase produces single-word
/// lowercase, and `Pkcs8` (digit between letters) stays `pkcs8` since
/// the kebab rule only inserts a dash before an uppercase ASCII run
/// that follows a lowercase or digit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, WebIdlEnum)]
pub enum KeyFormat {
    Raw,
    Spki,
    Pkcs8,
    Jwk,
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
    /// per spec §31.4.3 step 2.
    pub fn block_size_bits(self) -> u32 {
        match self {
            HashAlgo::Sha1 | HashAlgo::Sha256 => 512,
            HashAlgo::Sha384 | HashAlgo::Sha512 => 1024,
        }
    }
}

/// Spec §23.7 `NamedCurve` enumeration. Each variant carries an
/// explicit `#[webidl_name]` because the auto-kebab rule produces
/// `p256` etc., not the spec's `P-256` form (uppercase initial,
/// hyphen-then-digits).
#[derive(Debug, Clone, Copy, PartialEq, Eq, WebIdlEnum)]
pub enum NamedCurve {
    #[webidl_name = "P-256"]
    P256,
    #[webidl_name = "P-384"]
    P384,
    #[webidl_name = "P-521"]
    P521,
}

impl NamedCurve {
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
