//! `KernelError` — the kernel's surface-agnostic error enum.
//!
//! See `docs/proposals/node-crypto-native.md` §VII. The kernel returns
//! these; the surface adapter maps them to `OpError::dom(...)`
//! (WebCrypto) or `OpError::node(...)` (node:crypto). The kernel can't
//! decide that itself; only the surface knows which exception flavour
//! the spec asks for.

/// Kernel-side error flavours. One variant per "thing went wrong"
/// category; the surface adapter maps to JS spec error names.
#[derive(Debug, Clone)]
pub enum KernelError {
    /// `update()` / `digest()` after `digest()` already ran.
    HashFinalised,
    /// HMAC-side equivalent of `HashFinalised`.
    HmacFinalised,
    /// Algorithm name or pair didn't resolve to anything we ship.
    UnsupportedAlgorithm(String),
    /// Symmetric / asymmetric key length mismatched the algorithm.
    InvalidKeyLength,
    /// Generic operation failure — used for "the math returned an
    /// error" cases that have no more-specific variant.
    OperationFailed(&'static str),
    /// PBKDF2 / scrypt / HKDF parameter rejected (zero iterations,
    /// over-large output, scrypt N not a power of two, etc.).
    InvalidKdfParams(&'static str),
}

impl std::fmt::Display for KernelError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::HashFinalised => write!(f, "Digest already called"),
            Self::HmacFinalised => write!(f, "Digest already called"),
            Self::UnsupportedAlgorithm(name) => write!(f, "Unsupported algorithm: {name}"),
            Self::InvalidKeyLength => write!(f, "Invalid key length"),
            Self::OperationFailed(s) => write!(f, "Operation failed: {s}"),
            Self::InvalidKdfParams(s) => write!(f, "Invalid KDF parameters: {s}"),
        }
    }
}

impl std::error::Error for KernelError {}
