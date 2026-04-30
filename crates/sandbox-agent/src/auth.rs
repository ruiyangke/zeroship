//! Bearer-token authentication for the agent API.
//!
//! ## Provisioning model
//!
//! The token is **read from a file**, never from an environment
//! variable. The controller mounts the token at
//! `/run/secrets/sandbox-agent-token` (override via
//! `SANDBOX_AGENT_TOKEN_FILE`) and the agent reads it once at
//! startup, then `unlink()`s the file. After unlink:
//!
//!   - The file no longer appears in `ls`, `find`, or `/proc/mounts`.
//!   - The bytes do not appear in any process's `environ`, so
//!     `printenv` / `cat /proc/<pid>/environ` returns nothing.
//!   - In-memory copy is wrapped in `Zeroizing<Vec<u8>>` so it is
//!     overwritten when dropped.
//!
//! User code spawned by `/exec` cannot recover the token from these
//! channels. It can still `cat /proc/1/mem` (we are PID 1, root),
//! but doing so requires already having shell — no privilege gain.
//! The defenses we ship are about preventing **accidental leak out
//! of the VM** (logs, prompt-injection echoing env, crash dumps).
//!
//! ## Why bearer (and what we'll upgrade to)
//!
//! Bearer is the v1 simple thing. Two known weaknesses:
//!
//! 1. **Replay** — anyone who sees one valid request can replay it
//!    until the agent restarts.
//! 2. **Static-secret blast radius** — a single leak compromises
//!    the session for its entire lifetime.
//!
//! Planned upgrade: per-request HMAC with timestamp + nonce + body
//! hash (5 s replay window, body-bound). The provisioning stays the
//! same — just the verification changes — so the file-mount design
//! here is forward-compatible.

use std::path::Path;

use subtle::ConstantTimeEq;
use zeroize::Zeroizing;

/// Default path for the controller-mounted token file. Standard
/// "secrets" location so k8s `subPath` projected volumes land here
/// naturally.
pub const DEFAULT_TOKEN_PATH: &str = "/run/secrets/sandbox-agent-token";

/// Minimum acceptable token length in bytes. 32 = enough random
/// bytes that even base64 (43 chars) gives an unguessable secret.
/// 16 was too lenient (attacker-friendly if the controller fed us
/// short randomness).
pub const MIN_TOKEN_BYTES: usize = 32;

/// Loaded once at startup. Stored as `Zeroizing<Vec<u8>>` so the
/// bytes are scrubbed when dropped. Manual `Debug` redacts.
pub struct Token(Zeroizing<Vec<u8>>);

impl Token {
    /// Read from a file at `path`, then immediately `unlink()` the
    /// file so a later `cat <path>` returns nothing. The bytes are
    /// trimmed of trailing whitespace/newlines (so `echo $TOKEN >
    /// file` works) and validated for minimum length.
    ///
    /// Errors if the file is missing, unreadable, or too short.
    pub fn from_path(path: &Path) -> Result<Self, String> {
        let raw = std::fs::read(path)
            .map_err(|e| format!("read {}: {e}", path.display()))?;
        // `read` returns Vec<u8>; wrap in Zeroizing so the original
        // unwrapped buffer also gets scrubbed if we discard it.
        let raw = Zeroizing::new(raw);
        let trimmed = trim_trailing_whitespace(&raw);
        if trimmed.len() < MIN_TOKEN_BYTES {
            return Err(format!(
                "token at {} is too short ({} bytes; need >= {})",
                path.display(),
                trimmed.len(),
                MIN_TOKEN_BYTES,
            ));
        }
        // Best-effort unlink. If the mount is read-only or the file
        // is already gone, log and continue — the token is already
        // in memory, the file's existence after this point is just
        // a leak surface, not a correctness issue.
        if let Err(e) = std::fs::remove_file(path) {
            tracing::warn!(
                error = %e,
                token_path = %path.display(),
                "failed to unlink token file (token already in memory)"
            );
        }
        Ok(Self(Zeroizing::new(trimmed.to_vec())))
    }

    /// Verify the `Authorization` header value. `header` is the raw
    /// header value (e.g. `"Bearer abc123"`); returns true iff the
    /// presented token matches in constant time.
    pub fn verify_header(&self, header: Option<&str>) -> bool {
        let Some(h) = header else { return false };
        let Some(presented) = h.strip_prefix("Bearer ") else { return false };
        // ct_eq returns Choice(0) immediately on length mismatch — that
        // can leak length but not byte contents. Token length is fixed
        // by the controller, so length is effectively public.
        let eq: bool = presented.as_bytes().ct_eq(self.0.as_slice()).into();
        eq
    }
}

impl std::fmt::Debug for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never expose the bytes, even via Debug.
        write!(f, "Token(<{} bytes redacted>)", self.0.len())
    }
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
        std::env::temp_dir().join(format!("zsbx-token-{label}-{pid}-{n}"))
    }

    fn write_token(bytes: &[u8]) -> std::path::PathBuf {
        let p = unique_tmp("auth");
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(bytes).unwrap();
        p
    }

    #[test]
    fn loads_valid_token_then_unlinks() {
        let path = write_token(b"abcdefghijklmnopqrstuvwxyz0123456789");
        let t = Token::from_path(&path).unwrap();
        assert!(t.verify_header(Some(
            "Bearer abcdefghijklmnopqrstuvwxyz0123456789"
        )));
        // File must be gone after read.
        assert!(!path.exists(), "file must be unlinked after load");
    }

    #[test]
    fn trims_trailing_newline() {
        let path = write_token(b"abcdefghijklmnopqrstuvwxyz012345\n");
        let t = Token::from_path(&path).unwrap();
        assert!(t.verify_header(Some(
            "Bearer abcdefghijklmnopqrstuvwxyz012345"
        )));
        assert!(!t.verify_header(Some(
            "Bearer abcdefghijklmnopqrstuvwxyz012345\n"
        )));
    }

    #[test]
    fn rejects_short_token() {
        // 31 bytes < MIN_TOKEN_BYTES (32)
        let path = write_token(b"0123456789abcdef0123456789abcde");
        let r = Token::from_path(&path);
        // Cleanup since from_path errored before unlink.
        let _ = std::fs::remove_file(&path);
        assert!(r.is_err(), "must reject too-short token");
    }

    #[test]
    fn rejects_missing_file() {
        let path = unique_tmp("missing");
        assert!(Token::from_path(&path).is_err());
    }

    #[test]
    fn rejects_no_prefix() {
        let path = write_token(b"abcdefghijklmnopqrstuvwxyz012345");
        let t = Token::from_path(&path).unwrap();
        assert!(!t.verify_header(Some("abcdefghijklmnopqrstuvwxyz012345")));
        assert!(!t.verify_header(Some("Token abcdefghijklmnopqrstuvwxyz012345")));
    }

    #[test]
    fn rejects_missing_header() {
        let path = write_token(b"abcdefghijklmnopqrstuvwxyz012345");
        let t = Token::from_path(&path).unwrap();
        assert!(!t.verify_header(None));
        assert!(!t.verify_header(Some("")));
    }

    #[test]
    fn rejects_wrong() {
        let path = write_token(b"abcdefghijklmnopqrstuvwxyz012345");
        let t = Token::from_path(&path).unwrap();
        assert!(!t.verify_header(Some(
            "Bearer XBCDEFGHIJKLMNOPQRSTUVWXYZ012345"
        )));
    }

    #[test]
    fn debug_does_not_leak_token() {
        let path = write_token(b"super-secret-token-value-zzzzzzzzz");
        let t = Token::from_path(&path).unwrap();
        let d = format!("{:?}", t);
        assert!(!d.contains("super-secret"), "Debug must redact: {d}");
    }
}
