//! Bearer-token authentication for the agent API.
//!
//! The controller mounts a per-session 32-byte random token into the
//! VM as the `SANDBOX_AGENT_TOKEN` env var. Every request to this
//! agent must present `Authorization: Bearer <token>`. We compare
//! constant-time so a wrong-token attacker can't recover the right
//! one byte-by-byte via response timing.
//!
//! ## Why bearer (and what we'll upgrade to)
//!
//! Bearer is the v1 simple thing. It has two known weaknesses:
//!
//! 1. **Replay** — anyone who sees one valid request can replay it.
//!    In-cluster traffic is trusted-ish, but a logging system that
//!    captures Authorization headers becomes a leak vector.
//!
//! 2. **Static-secret blast radius** — token leaks once → permanent.
//!
//! The planned upgrade is per-request HMAC signing with timestamp +
//! nonce (5 s replay window, body-bound). The `Authorization: Bearer`
//! header gets replaced by `X-Sbx-{Timestamp,Nonce,Signature}` and
//! the same env var becomes the HMAC key. Everything else in this
//! module stays the same shape — the verify function just changes.
//!
//! For now: bearer. Don't ship to production with this alone — pair
//! it with a NetworkPolicy that only lets the controller reach :7777.

use subtle::ConstantTimeEq;

/// Loaded once at agent startup from the env var `SANDBOX_AGENT_TOKEN`.
/// Stored as `Vec<u8>` so the comparison is byte-oriented and the
/// String representation never leaks via Debug.
#[derive(Clone)]
pub struct Token(Vec<u8>);

impl Token {
    /// Read from env. Empty / missing is rejected — the agent refuses
    /// to start without a token, since "no auth" is never the right
    /// production answer and would be a footgun in dev too.
    pub fn from_env() -> Result<Self, String> {
        let raw = std::env::var("SANDBOX_AGENT_TOKEN")
            .map_err(|_| "SANDBOX_AGENT_TOKEN env var not set".to_string())?;
        if raw.len() < 16 {
            return Err("SANDBOX_AGENT_TOKEN too short (need >= 16 chars)".into());
        }
        Ok(Self(raw.into_bytes()))
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
        let eq: bool = presented.as_bytes().ct_eq(&self.0).into();
        eq
    }
}

impl std::fmt::Debug for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never expose the bytes, even via Debug.
        write!(f, "Token(<{} bytes redacted>)", self.0.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tok(s: &str) -> Token {
        Token(s.as_bytes().to_vec())
    }

    #[test]
    fn verifies_correct() {
        let t = tok("abcdefghijklmnopqrstuvwxyz012345");
        assert!(t.verify_header(Some("Bearer abcdefghijklmnopqrstuvwxyz012345")));
    }

    #[test]
    fn rejects_wrong() {
        let t = tok("abcdefghijklmnopqrstuvwxyz012345");
        assert!(!t.verify_header(Some("Bearer XBCDEFGHIJKLMNOPQRSTUVWXYZ012345")));
    }

    #[test]
    fn rejects_no_prefix() {
        let t = tok("abcdefghijklmnopqrstuvwxyz012345");
        assert!(!t.verify_header(Some("abcdefghijklmnopqrstuvwxyz012345")));
        assert!(!t.verify_header(Some("Token abcdefghijklmnopqrstuvwxyz012345")));
    }

    #[test]
    fn rejects_missing_header() {
        let t = tok("abcdefghijklmnopqrstuvwxyz012345");
        assert!(!t.verify_header(None));
        assert!(!t.verify_header(Some("")));
    }

    #[test]
    fn rejects_short_token_at_load() {
        std::env::set_var("SANDBOX_AGENT_TOKEN", "short");
        let r = Token::from_env();
        std::env::remove_var("SANDBOX_AGENT_TOKEN");
        assert!(r.is_err());
    }

    #[test]
    fn debug_does_not_leak_token() {
        let t = tok("super-secret-token-value-12345678");
        let d = format!("{:?}", t);
        assert!(!d.contains("super-secret"), "Debug must redact the token");
    }
}
