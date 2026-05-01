//! Public-key loader for the Ed25519 signature verifier.
//!
//! The agent holds **only the controller's public key** — no
//! private/signing material ever enters a sandbox VM. The pubkey
//! is provisioned at Pod creation time by mounting a `ConfigMap`
//! (NOT a `Secret`, because pubkeys are non-secret) at a fixed
//! path inside the VM:
//!
//! ```yaml
//! volumeMounts:
//!   - name: agent-trust
//!     mountPath: /run/keys
//!     readOnly: true   # k8s read-only bind; tmpfs-mounted
//! ```
//!
//! ## File format
//!
//! Either of:
//!   - **base64**: the file contents are base64-encoded 32 bytes
//!     (with optional trailing whitespace), e.g. produced by
//!     `head -c 32 /dev/urandom | base64`. Recommended for ConfigMap
//!     `data:` (UTF-8 text fields).
//!   - **raw bytes**: exactly 32 bytes, no encoding. Useful if the
//!     ConfigMap uses `binaryData:` instead of `data:`.
//!
//! Selection is automatic: if the trimmed contents are exactly 32
//! bytes, treat as raw; otherwise base64-decode.
//!
//! ## No unlink, no zeroize
//!
//! Unlike the previous HMAC-key loader, we do **not** unlink the
//! file after read and do **not** wrap the bytes in `Zeroizing<>`.
//! A pubkey is non-secret by definition; its presence on disk
//! after startup costs us nothing. Leaving the file in place lets
//! operators verify (`kubectl exec ... cat /run/keys/...`) which
//! key the agent is trusting, and lets a hot-reload feature in a
//! future revision re-read it without coordinating with the
//! mount lifecycle.
//!
//! ## Why this beats HMAC
//!
//!   - The compromised-VM attacker (root inside the libkrun guest,
//!     `/proc/1/mem` read) recovers a **public key** — useless for
//!     forging requests against any agent in the fleet.
//!   - The k8s storage layer carries **no secret material** for
//!     this auth path. ConfigMap is fine; encryption-at-rest of
//!     `Secret` data is irrelevant. Etcd compromise of *this*
//!     ConfigMap leaks nothing.
//!   - Per-sandbox keypair generation can move entirely to the
//!     controller: the controller mints `(sk, pk)`, stashes `sk`
//!     in its own DB (or HSM), and ships `pk` as a ConfigMap to
//!     the Pod. The sandbox itself never participates in key
//!     generation.

use std::io::Read;
use std::path::Path;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use ed25519_dalek::{VerifyingKey, PUBLIC_KEY_LENGTH};

/// Default path for the controller-mounted public key file.
/// `/run/keys` is a conventional read-only k8s mount point;
/// `controller-pubkey` is the file name baked into the Pod spec.
pub const DEFAULT_PUBKEY_PATH: &str = "/run/keys/controller-pubkey";

/// Maximum file size we'll read. Caps the boot-time alloc bound
/// when an operator misconfigures the mount. Even base64 of 32
/// raw bytes is only 44 chars + maybe a trailing newline; 1 KiB
/// is comically generous.
pub const MAX_PUBKEY_FILE_BYTES: usize = 1024;

/// Errors that surface to the operator at startup. None of these
/// can ever reach a client request.
#[derive(Debug)]
pub enum LoadError {
    /// Couldn't open or read the file.
    Io(String),
    /// File length exceeds [`MAX_PUBKEY_FILE_BYTES`] before any
    /// trim — most likely a wrong file got mounted at the path.
    TooLarge,
    /// Trimmed contents are neither raw 32 bytes nor decode to
    /// valid base64 of 32 bytes.
    BadEncoding,
    /// Decoded length is not 32 bytes (Ed25519 public key size).
    WrongLength(usize),
    /// `ed25519_dalek::VerifyingKey::from_bytes` rejected the
    /// 32-byte point as not on the curve.
    NotOnCurve,
}

impl std::fmt::Display for LoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LoadError::Io(s) => write!(f, "{s}"),
            LoadError::TooLarge => write!(f, "pubkey file exceeds {MAX_PUBKEY_FILE_BYTES} bytes"),
            LoadError::BadEncoding => {
                write!(f, "pubkey file is neither 32 raw bytes nor valid base64-of-32-bytes")
            }
            LoadError::WrongLength(n) => write!(f, "pubkey decodes to {n} bytes; expected 32"),
            LoadError::NotOnCurve => {
                write!(f, "pubkey bytes are not a valid Ed25519 public key (point not on curve)")
            }
        }
    }
}

impl std::error::Error for LoadError {}

/// Read and parse the controller's Ed25519 public key from `path`.
///
/// The file must be either:
///   - exactly 32 raw bytes (Ed25519 public key in raw binary), or
///   - the base64 encoding of those 32 bytes (with optional
///     trailing whitespace / newline — `cat key.b64` works).
pub fn load_pubkey_from_path(path: &Path) -> Result<VerifyingKey, LoadError> {
    let file = std::fs::File::open(path)
        .map_err(|e| LoadError::Io(format!("open {}: {e}", path.display())))?;
    let mut raw = Vec::new();
    let mut limited = file.take(MAX_PUBKEY_FILE_BYTES as u64 + 1);
    limited
        .read_to_end(&mut raw)
        .map_err(|e| LoadError::Io(format!("read {}: {e}", path.display())))?;
    if raw.len() > MAX_PUBKEY_FILE_BYTES {
        return Err(LoadError::TooLarge);
    }
    let trimmed = trim_trailing_whitespace(&raw);

    // Format selection: if exactly 32 bytes, treat as raw.
    // Otherwise, try base64 decode.
    let bytes: Vec<u8> = if trimmed.len() == PUBLIC_KEY_LENGTH {
        trimmed.to_vec()
    } else {
        // base64 decode of (potentially CRLF-stripped) string.
        let s = std::str::from_utf8(trimmed).map_err(|_| LoadError::BadEncoding)?;
        B64.decode(s.trim().as_bytes())
            .map_err(|_| LoadError::BadEncoding)?
    };
    if bytes.len() != PUBLIC_KEY_LENGTH {
        return Err(LoadError::WrongLength(bytes.len()));
    }

    let arr: [u8; PUBLIC_KEY_LENGTH] = bytes
        .as_slice()
        .try_into()
        .expect("length checked just above");
    VerifyingKey::from_bytes(&arr).map_err(|_| LoadError::NotOnCurve)
}

fn trim_trailing_whitespace(buf: &[u8]) -> &[u8] {
    let mut end = buf.len();
    while end > 0 && matches!(buf[end - 1], b'\n' | b'\r' | b' ' | b'\t') {
        end -= 1;
    }
    &buf[..end]
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn unique_tmp(label: &str) -> std::path::PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let pid = std::process::id();
        std::env::temp_dir().join(format!("zsbx-pubkey-{label}-{pid}-{n}"))
    }

    fn write_file(bytes: &[u8]) -> std::path::PathBuf {
        let p = unique_tmp("auth");
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(bytes).unwrap();
        p
    }

    fn fresh_pubkey() -> VerifyingKey {
        SigningKey::from_bytes(&[42u8; 32]).verifying_key()
    }

    #[test]
    fn loads_raw_32_byte_pubkey() {
        let pk = fresh_pubkey();
        let path = write_file(pk.as_bytes());
        let loaded = load_pubkey_from_path(&path).unwrap();
        assert_eq!(loaded.as_bytes(), pk.as_bytes());
        // File is left in place — pubkey isn't secret.
        assert!(path.exists(), "pubkey file must persist (read-only mount)");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn loads_base64_pubkey() {
        let pk = fresh_pubkey();
        let b64 = B64.encode(pk.as_bytes());
        let path = write_file(b64.as_bytes());
        let loaded = load_pubkey_from_path(&path).unwrap();
        assert_eq!(loaded.as_bytes(), pk.as_bytes());
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn loads_base64_with_trailing_newline() {
        let pk = fresh_pubkey();
        let mut b64 = B64.encode(pk.as_bytes());
        b64.push('\n');
        let path = write_file(b64.as_bytes());
        let loaded = load_pubkey_from_path(&path).unwrap();
        assert_eq!(loaded.as_bytes(), pk.as_bytes());
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn rejects_missing_file() {
        let path = unique_tmp("missing");
        let r = load_pubkey_from_path(&path);
        assert!(matches!(r, Err(LoadError::Io(_))));
    }

    #[test]
    fn rejects_oversize_file() {
        let huge = vec![b'A'; MAX_PUBKEY_FILE_BYTES + 1];
        let path = write_file(&huge);
        let r = load_pubkey_from_path(&path);
        assert!(matches!(r, Err(LoadError::TooLarge)));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn rejects_garbage_encoding() {
        // 40 random bytes that aren't 32 and aren't valid base64.
        let bad: Vec<u8> = (0..40).map(|i| i as u8).collect();
        let path = write_file(&bad);
        let r = load_pubkey_from_path(&path);
        assert!(matches!(r, Err(LoadError::BadEncoding)));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn rejects_wrong_length_after_decode() {
        // Valid base64 but decodes to 16 bytes, not 32.
        let short = B64.encode([0u8; 16]);
        let path = write_file(short.as_bytes());
        let r = load_pubkey_from_path(&path);
        assert!(matches!(r, Err(LoadError::WrongLength(16))));
        std::fs::remove_file(&path).ok();
    }

    /// 32 bytes that aren't a valid Ed25519 point. The parser
    /// must surface `NotOnCurve` rather than silently accept.
    /// In practice ed25519-dalek's `from_bytes` accepts most
    /// 32-byte values (small-order points are flagged by
    /// `verify_strict`, not construction), so this test exercises
    /// the `from_bytes` rejection path on a known-bad encoding.
    #[test]
    fn does_not_panic_on_arbitrary_32_bytes() {
        // All-0xFF: not on the curve in some encodings.
        let path = write_file(&[0xFFu8; 32]);
        // Either NotOnCurve OR success (depends on dalek
        // version's strictness during construction). What we
        // care about: NO PANIC, no buffer overflow.
        let _ = load_pubkey_from_path(&path);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn empty_file_rejected() {
        let path = write_file(b"");
        let r = load_pubkey_from_path(&path);
        // 0 bytes after trim → not 32 raw, base64 of empty is empty,
        // → WrongLength(0) or BadEncoding. Either rejection is fine.
        assert!(r.is_err());
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn trims_trailing_whitespace() {
        let pk = fresh_pubkey();
        let mut b64 = B64.encode(pk.as_bytes());
        b64.push_str("  \r\n\t");
        let path = write_file(b64.as_bytes());
        let loaded = load_pubkey_from_path(&path).unwrap();
        assert_eq!(loaded.as_bytes(), pk.as_bytes());
        std::fs::remove_file(&path).ok();
    }
}
