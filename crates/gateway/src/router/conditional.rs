//! HTTP conditional-request helpers — `If-None-Match` matching and
//! `Range:` parsing.
//!
//! The static-serve path uses these to short-circuit conditional GETs
//! (304) and slice partial-content responses (206 / 416). They're pure
//! functions over header values + the asset's variant size; they don't
//! touch state.

/// Match an `If-None-Match` request-header value against the asset's
/// strong ETag. Accepts:
///
/// * an exact match: `If-None-Match: "<etag>"`,
/// * the wildcard `*`,
/// * a comma-separated list (`"a", "b", "c"`) — match if any element
///   matches.
///
/// **Strong-only**: `W/"…"` weak prefixes are NOT considered a match.
/// Our hashes are content-addressed (SHA-256), so every ETag we emit
/// is strong — a weak match would be lying about byte-for-byte
/// equivalence.
pub(super) fn etag_matches(if_none_match: &str, etag: &str) -> bool {
    let trimmed = if_none_match.trim();
    if trimmed == "*" {
        return true;
    }
    for part in trimmed.split(',') {
        let p = part.trim();
        if p.is_empty() {
            continue;
        }
        // Reject weak ETags (`W/"…"`) — strong comparison only.
        if p.starts_with("W/") || p.starts_with("w/") {
            continue;
        }
        if p == etag {
            return true;
        }
    }
    false
}

/// Parsed `Range:` request — what the client asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RangeSpec {
    /// `bytes=N-M` (inclusive on both ends), normalised against `size`.
    /// `start <= end < size`.
    Single(u64, u64),
    /// Multiple ranges (`bytes=0-10,20-30`). RFC 7233 allows a
    /// `multipart/byteranges` response; we degrade gracefully to a
    /// 200 + full body — still RFC-compliant.
    MultiRange,
    /// Range is past EOF (`start >= size`) or otherwise unsatisfiable.
    /// Caller emits 416 with `Content-Range: bytes */<size>`.
    Unsatisfiable,
}

/// Parse a `Range` header against the asset's identity size. Returns
/// `None` when the header is absent or syntactically broken (caller
/// falls through to a normal 200). RFC 7233 §3.1 grammar — minimal
/// subset:
///
/// * `bytes=N-M`   — inclusive range; clamped to `size - 1` on overflow.
/// * `bytes=N-`    — open-end; ends at `size - 1`.
/// * `bytes=-N`    — last `N` bytes; clamped to `size`.
/// * `bytes=A-B,C-D[, …]` — multi-range; returns `MultiRange`.
///
/// Unrecognised units (`items=…`) → `None`. Non-bytes-prefixed → `None`.
pub(super) fn parse_range(
    header: Option<&ntex::http::header::HeaderValue>,
    size: u64,
) -> Option<RangeSpec> {
    let raw = header?.to_str().ok()?;
    let spec = raw.strip_prefix("bytes=")?;
    let parts: Vec<&str> = spec.split(',').map(|s| s.trim()).collect();
    if parts.is_empty() {
        return None;
    }
    if parts.len() > 1 {
        // Multi-range: caller falls through to a 200 + full body. Per
        // RFC 7233 §4.1 a server MAY ignore Range — graceful degrade.
        return Some(RangeSpec::MultiRange);
    }
    let single = parts[0];
    if single.is_empty() {
        return None;
    }

    // `-N` → last N bytes. Suffix form.
    if let Some(n_str) = single.strip_prefix('-') {
        let n: u64 = n_str.parse().ok()?;
        if n == 0 || size == 0 {
            return Some(RangeSpec::Unsatisfiable);
        }
        let n = std::cmp::min(n, size);
        return Some(RangeSpec::Single(size - n, size - 1));
    }

    let (start_str, end_str) = single.split_once('-')?;
    let start: u64 = start_str.parse().ok()?;
    if start >= size {
        return Some(RangeSpec::Unsatisfiable);
    }
    let end: u64 = if end_str.is_empty() {
        size - 1
    } else {
        let parsed: u64 = end_str.parse().ok()?;
        std::cmp::min(parsed, size - 1)
    };
    if end < start {
        return Some(RangeSpec::Unsatisfiable);
    }
    Some(RangeSpec::Single(start, end))
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── etag_matches ────────────────────────────────────────────────────────

    #[test]
    fn etag_matches_exact() {
        assert!(etag_matches("\"abc\"", "\"abc\""));
        assert!(!etag_matches("\"abc\"", "\"xyz\""));
    }

    #[test]
    fn etag_matches_wildcard() {
        assert!(etag_matches("*", "\"abc\""));
        // Wildcard with surrounding whitespace is also valid.
        assert!(etag_matches("  *  ", "\"abc\""));
    }

    #[test]
    fn etag_matches_list() {
        assert!(etag_matches("\"abc\", \"def\"", "\"def\""));
        assert!(etag_matches("\"abc\",\"def\"", "\"abc\""));
        assert!(!etag_matches("\"abc\", \"def\"", "\"xyz\""));
    }

    #[test]
    fn etag_matches_weak_rejected() {
        // Weak ETags must NOT match — strong comparison only.
        assert!(!etag_matches("W/\"abc\"", "\"abc\""));
        // Mixed strong + weak in a list — only the strong entries
        // can match.
        assert!(etag_matches("W/\"abc\", \"def\"", "\"def\""));
        assert!(!etag_matches("W/\"abc\", W/\"def\"", "\"def\""));
    }

    // ── parse_range ─────────────────────────────────────────────────────────

    fn range_header(s: &str) -> ntex::http::header::HeaderValue {
        ntex::http::header::HeaderValue::from_str(s).unwrap()
    }

    #[test]
    fn parse_range_single() {
        let h = range_header("bytes=0-9");
        assert_eq!(parse_range(Some(&h), 100), Some(RangeSpec::Single(0, 9)));
    }

    #[test]
    fn parse_range_open_end() {
        let h = range_header("bytes=10-");
        assert_eq!(parse_range(Some(&h), 100), Some(RangeSpec::Single(10, 99)));
    }

    #[test]
    fn parse_range_suffix() {
        let h = range_header("bytes=-20");
        assert_eq!(parse_range(Some(&h), 100), Some(RangeSpec::Single(80, 99)));
        // Suffix bigger than file → clamp to whole file.
        let h2 = range_header("bytes=-500");
        assert_eq!(parse_range(Some(&h2), 100), Some(RangeSpec::Single(0, 99)));
    }

    #[test]
    fn parse_range_clamps_end_to_size_minus_one() {
        let h = range_header("bytes=50-9999");
        assert_eq!(parse_range(Some(&h), 100), Some(RangeSpec::Single(50, 99)));
    }

    #[test]
    fn parse_range_unsatisfiable() {
        let h = range_header("bytes=200-300");
        assert_eq!(parse_range(Some(&h), 100), Some(RangeSpec::Unsatisfiable));
        // start > end with start in range → still unsatisfiable.
        let h2 = range_header("bytes=99-50");
        assert_eq!(parse_range(Some(&h2), 100), Some(RangeSpec::Unsatisfiable));
    }

    #[test]
    fn parse_range_multi() {
        let h = range_header("bytes=0-10,20-30");
        assert_eq!(parse_range(Some(&h), 100), Some(RangeSpec::MultiRange));
    }

    #[test]
    fn parse_range_unknown_unit() {
        let h = range_header("items=1-2");
        assert_eq!(parse_range(Some(&h), 100), None);
    }

    #[test]
    fn parse_range_garbage() {
        let h = range_header("bytes=abc");
        assert_eq!(parse_range(Some(&h), 100), None);
    }

    #[test]
    fn parse_range_absent() {
        assert_eq!(parse_range(None, 100), None);
    }
}
