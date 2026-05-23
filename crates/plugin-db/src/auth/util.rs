//! Shared helpers for the auth subsystem.
//!
//! ## Why this file exists
//!
//! Under P2 these helpers (TTL default, `/dev/urandom` fallback, ISO
//! timestamp formatting, hex codec, civil-from-days arithmetic) were
//! private to `auth/session.rs` because only the PG `mint_session_token`
//! / `init_session` free fns used them.
//!
//! P3 introduces a SQLite `SessionMinter` impl in
//! `backend/sqlite/session_minter.rs`. Both impls must produce
//! byte-identical canonical payloads + token shapes for the same
//! `(secret, init, nonce, expires_at)`. The helpers therefore have to
//! be reachable from BOTH arms — see
//! `docs/proposals/p3-sqlite-auth-implementation-plan.md` §6 (H-1).
//!
//! ## Gating
//!
//! This module is reachable whenever the parent `auth` module is
//! reachable — i.e. `any(feature = "hardening", feature = "sqlite")`.
//! Helpers carry no PG dependencies, so the wider gate is safe.
//!
//! ## Stability
//!
//! Bodies were moved verbatim from `auth/session.rs` in P3 PR 1; no
//! signature change. The PG `b8c_*` integration tests pass byte-for-byte
//! because the free fns in `session.rs` now call into these helpers
//! under their original names.

use std::time::{SystemTime, UNIX_EPOCH};

/// Default lifetime for a minted session token. The signed payload
/// includes `expires_at`, so an HTTP-roundtripped token that misses
/// this window is rejected by `init_session`. Five minutes is enough
/// for any sane connection-acquire round-trip and short enough that a
/// captured token can't be replayed long.
pub const DEFAULT_TOKEN_TTL_SECS: i64 = 300;

/// How long the nonce-replay-protection table retains a row. Must
/// outlive `DEFAULT_TOKEN_TTL_SECS` plus the key-rotation grace window
/// so a captured-and-late-arriving signature cannot bypass replay
/// detection by being delayed past the nonce's GC.
pub const NONCE_RETENTION_SECS: i64 = 25 * 3600;

/// Best-effort random fill — prefers `/dev/urandom`; falls back to a
/// time-perturbed XOR stream if unavailable. The XOR fallback is good
/// enough for "nonce" uniqueness (the proposal's threat model assumes
/// the HMAC key, not the nonce, is the secret) but logs a warning so
/// production deployments notice the missing entropy source.
pub fn getrandom_or_fallback(buf: &mut [u8]) {
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        use std::io::Read;
        if f.read_exact(buf).is_ok() {
            return;
        }
    }
    // Fallback — never expected in production. The proposal requires
    // pgcrypto for the HMAC key (which IS the secret); the nonce only
    // needs to be unique within the retention window.
    tracing::error!("auth/session: /dev/urandom unavailable, using time-perturbed fallback");
    let mut t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    for b in buf.iter_mut() {
        t = t.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        *b = (t >> 33) as u8;
    }
}

/// ISO-8601 with millisecond precision in UTC — matches the SQL
/// format string `YYYY-MM-DD"T"HH24:MI:SS.MS`.
pub fn iso_timestamp_after(ttl_secs: i64) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let total_ms = now + ttl_secs.saturating_mul(1000);
    format_unix_millis(total_ms)
}

/// Format a Unix-millisecond timestamp as `YYYY-MM-DDTHH:MM:SS.mmm`
/// in UTC. We roll our own to avoid pulling chrono into plugin-db's
/// dependency graph (the rest of the crate gets by without it).
pub fn format_unix_millis(ms: i64) -> String {
    // Algorithm: Howard Hinnant's "days_from_civil" inversion.
    let secs = ms / 1000;
    let ms_frac = (ms % 1000).abs();
    let days = secs.div_euclid(86_400);
    let time_in_day = secs.rem_euclid(86_400);
    let h = time_in_day / 3600;
    let m = (time_in_day % 3600) / 60;
    let s = time_in_day % 60;

    let (y, mo, d) = civil_from_days(days);
    format!(
        "{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}.{ms_frac:03}",
    )
}

/// Convert days-since-Unix-epoch to (year, month, day) — Hinnant's
/// algorithm. Handles negative inputs (we never see those, but the
/// math is the same).
pub fn civil_from_days(days: i64) -> (i32, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = (yoe as i64) + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = (y + i64::from(m <= 2)) as i32;
    (year, m as u32, d as u32)
}

pub fn hex_encode(b: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(b.len() * 2);
    for &x in b {
        out.push(HEX[(x >> 4) as usize] as char);
        out.push(HEX[(x & 0xF) as usize] as char);
    }
    out
}

pub fn hex_decode(s: &str) -> Result<Vec<u8>, String> {
    if s.len() % 2 != 0 {
        return Err("odd-length hex string".into());
    }
    let mut out = Vec::with_capacity(s.len() / 2);
    let bytes = s.as_bytes();
    for i in (0..bytes.len()).step_by(2) {
        let hi = hex_nibble(bytes[i])?;
        let lo = hex_nibble(bytes[i + 1])?;
        out.push((hi << 4) | lo);
    }
    Ok(out)
}

fn hex_nibble(c: u8) -> Result<u8, String> {
    match c {
        b'0'..=b'9' => Ok(c - b'0'),
        b'a'..=b'f' => Ok(c - b'a' + 10),
        b'A'..=b'F' => Ok(c - b'A' + 10),
        _ => Err(format!("invalid hex digit {:?}", c as char)),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_roundtrip() {
        let a = [0xde, 0xad, 0xbe, 0xef, 0x00, 0xff, 0x42];
        let s = hex_encode(&a);
        assert_eq!(s, "deadbeef00ff42");
        assert_eq!(hex_decode(&s).unwrap(), a);
    }

    #[test]
    fn hex_decode_rejects_odd_length() {
        assert!(hex_decode("abc").is_err());
    }

    #[test]
    fn hex_decode_rejects_garbage() {
        assert!(hex_decode("xy").is_err());
    }

    #[test]
    fn iso_format_unix_epoch() {
        assert_eq!(format_unix_millis(0), "1970-01-01T00:00:00.000");
    }

    #[test]
    fn iso_format_known_value() {
        // 2026-05-07T00:00:00.000 UTC = 1_778_112_000_000 ms since epoch.
        let ms: i64 = 1_778_112_000_000;
        let s = format_unix_millis(ms);
        assert_eq!(s, "2026-05-07T00:00:00.000");
    }

    #[test]
    fn iso_format_includes_milliseconds() {
        let ms: i64 = 1_778_112_000_123;
        assert!(
            format_unix_millis(ms).ends_with(".123"),
            "got: {}",
            format_unix_millis(ms)
        );
    }

    #[test]
    fn nonce_random_bytes_are_not_all_zero() {
        let mut b = [0u8; 32];
        getrandom_or_fallback(&mut b);
        assert!(b.iter().any(|&x| x != 0), "got all-zero nonce");
    }
}
