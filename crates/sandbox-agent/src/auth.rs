//! Authentication key material loader.
//!
//! Loads the HMAC-SHA256 key the [`crate::sig::Verifier`] uses for
//! per-request signature verification. The provisioning model is
//! identical to what we used for the v1 bearer-token scheme:
//!
//!   - **File-mount**: controller writes the key bytes to a tmpfs
//!     path (default `/run/secrets/sandbox-agent-token`; override
//!     `SANDBOX_AGENT_TOKEN_FILE`).
//!   - **Read-and-unlink**: the agent reads the file once at startup
//!     and immediately `unlink()`s it. After that the bytes never
//!     appear in any process's `environ`, in `ls`, or in
//!     `/proc/mounts` — the only copy is the in-memory
//!     `Zeroizing<Vec<u8>>` which scrubs on drop.
//!   - **Minimum length**: ≥ 32 bytes so even a misconfigured
//!     controller can't ship a low-entropy key.
//!
//! User code spawned by `/exec` cannot recover the key from these
//! channels. It can still `cat /proc/1/mem` (we are PID 1, root in
//! the VM), but doing so requires already having shell — no
//! privilege gain. The defenses we ship are about preventing
//! **accidental leak out of the VM** (logs, prompt injection
//! echoing env, crash dumps).
//!
//! # Why HMAC-SHA256 (and not something else)
//!
//! Symmetric MAC is the right primitive when both parties already
//! share a secret (we do — the controller mounts it). Asymmetric
//! signing (Ed25519, RSA) would let the agent verify without the
//! ability to forge requests, but the agent isn't going to make
//! controller→agent calls anyway, so the asymmetry buys nothing in
//! this direction. HMAC is faster, smaller code, fewer ways to
//! misuse.

use std::io::Read;
use std::path::Path;

use zeroize::Zeroizing;

/// Default path for the controller-mounted key file. Standard
/// "secrets" location so k8s `subPath` projected volumes land here
/// naturally.
pub const DEFAULT_TOKEN_PATH: &str = "/run/secrets/sandbox-agent-token";

/// Minimum acceptable key length in bytes. 32 = enough random
/// bytes that even base64 (43 chars) gives an unguessable secret.
pub const MIN_TOKEN_BYTES: usize = 32;

/// Maximum acceptable key length in bytes. Bounds the boot-time
/// memory allocation so a misconfigured operator who mounts a
/// gigantic file at the secrets path can't OOM the agent at
/// startup. 1 KiB is more than enough for any reasonable HMAC key
/// (HMAC-SHA256 is keyed by ≤ 64 bytes; longer keys get hashed
/// down anyway).
pub const MAX_TOKEN_BYTES: usize = 1024;

/// Read the HMAC key from `path`, then immediately `unlink()` the
/// file so a later `cat <path>` returns nothing. Bytes are trimmed
/// of trailing whitespace/newlines (so `echo $K > file` works) and
/// validated for length.
///
/// **Memory bound**: at most `MAX_TOKEN_BYTES + 1` bytes are read
/// from the file, regardless of the file's true size. Anything
/// longer is rejected without holding the full content in memory.
///
/// On validation failure the file is **still unlinked** — by the
/// time we know the bytes are bad, they're already in our memory,
/// and removing the file as soon as possible bounds the window
/// during which a bad-key startup leaves the file on the tmpfs.
///
/// Errors propagate the host path because they fire at boot only
/// (operator-facing, never returned to a client).
pub fn load_key_from_path(path: &Path) -> Result<Zeroizing<Vec<u8>>, String> {
    // Bounded read: take(MAX_TOKEN_BYTES + 1). If we get exactly
    // MAX_TOKEN_BYTES + 1 bytes, the file is over-cap and we reject
    // without ever loading the rest into memory. If we get fewer,
    // EOF reached and the whole content is in `raw`.
    let file = std::fs::File::open(path)
        .map_err(|e| format!("open {}: {e}", path.display()))?;
    let mut raw = Vec::new();
    let mut limited = file.take(MAX_TOKEN_BYTES as u64 + 1);
    limited
        .read_to_end(&mut raw)
        .map_err(|e| format!("read {}: {e}", path.display()))?;
    let raw = Zeroizing::new(raw);

    // Unlink BEFORE validation — see the doc comment above.
    if let Err(e) = std::fs::remove_file(path) {
        tracing::warn!(
            error = %e,
            token_path = %path.display(),
            "failed to unlink token file (token already in memory)"
        );
    }

    let trimmed = trim_trailing_whitespace(&raw);
    if trimmed.len() > MAX_TOKEN_BYTES {
        return Err(format!(
            "key at {} is too large (>{} bytes after trim)",
            path.display(),
            MAX_TOKEN_BYTES,
        ));
    }
    if trimmed.len() < MIN_TOKEN_BYTES {
        return Err(format!(
            "key at {} is too short ({} bytes; need >= {})",
            path.display(),
            trimmed.len(),
            MIN_TOKEN_BYTES,
        ));
    }
    Ok(Zeroizing::new(trimmed.to_vec()))
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
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn unique_tmp(label: &str) -> std::path::PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let pid = std::process::id();
        std::env::temp_dir().join(format!("zsbx-key-{label}-{pid}-{n}"))
    }

    fn write_key(bytes: &[u8]) -> std::path::PathBuf {
        let p = unique_tmp("auth");
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(bytes).unwrap();
        p
    }

    #[test]
    fn loads_valid_key_then_unlinks() {
        let path = write_key(b"abcdefghijklmnopqrstuvwxyz0123456789");
        let bytes = load_key_from_path(&path).unwrap();
        assert_eq!(bytes.len(), 36);
        assert_eq!(&bytes[..], b"abcdefghijklmnopqrstuvwxyz0123456789");
        assert!(!path.exists(), "file must be unlinked after load");
    }

    #[test]
    fn trims_trailing_newline() {
        let path = write_key(b"abcdefghijklmnopqrstuvwxyz012345\n");
        let bytes = load_key_from_path(&path).unwrap();
        // 32 bytes after trim.
        assert_eq!(bytes.len(), 32);
        assert!(!bytes.ends_with(b"\n"));
    }

    #[test]
    fn rejects_short_key() {
        // 31 bytes < MIN_TOKEN_BYTES (32)
        let path = write_key(b"0123456789abcdef0123456789abcde");
        let r = load_key_from_path(&path);
        // Per the unlink-before-validate rule, the file is gone even
        // though we rejected the bytes.
        assert!(!path.exists(), "file must be unlinked even on validation failure");
        assert!(r.is_err());
    }

    #[test]
    fn rejects_missing_file() {
        let path = unique_tmp("missing");
        assert!(load_key_from_path(&path).is_err());
    }

    #[test]
    fn loaded_bytes_zeroize_on_drop() {
        // The Zeroizing wrapper wipes on drop. We can't observe the
        // memory after free safely, but we CAN check that the type
        // is the zeroizing one (compile-time check).
        let path = write_key(b"abcdefghijklmnopqrstuvwxyz012345");
        let bytes = load_key_from_path(&path).unwrap();
        let _: &Zeroizing<Vec<u8>> = &bytes;
    }

    #[test]
    fn rejects_oversize_key() {
        // Exactly MAX_TOKEN_BYTES + 1 bytes of key material — must
        // be rejected and the file unlinked.
        let huge = vec![b'x'; MAX_TOKEN_BYTES + 1];
        let path = write_key(&huge);
        let r = load_key_from_path(&path);
        assert!(r.is_err());
        assert!(!path.exists(), "file unlinked even on oversize");
    }

    #[test]
    fn accepts_max_size_key() {
        // Exactly MAX_TOKEN_BYTES — must succeed.
        let max = vec![b'k'; MAX_TOKEN_BYTES];
        let path = write_key(&max);
        let r = load_key_from_path(&path);
        assert!(r.is_ok());
        assert_eq!(r.unwrap().len(), MAX_TOKEN_BYTES);
    }

    #[test]
    fn does_not_oom_on_huge_file() {
        // Write 5 MiB of bytes — load_key_from_path's bounded read
        // must short-circuit at MAX_TOKEN_BYTES + 1 without
        // allocating a 5 MiB Vec.
        let path = unique_tmp("huge");
        let f = std::fs::File::create(&path).unwrap();
        f.set_len(5 * 1024 * 1024).unwrap();
        let r = load_key_from_path(&path);
        assert!(r.is_err(), "oversize file must be rejected");
    }
}
