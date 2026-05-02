//! Bad-port blocklist per WHATWG Fetch §4.3 (https://fetch.spec.whatwg.org/#bad-port).
//!
//! Per design D-19 (v2 critical fix): we use the SPEC list verbatim — 83 ports
//! including port 0 — NOT the 82-port subset undici ships. The spec table is
//! the authoritative source and incorporates ports added since undici last
//! audited.
//!
//! At HTTP-fetch entry, any URL whose port is in this set yields a network
//! error (per Fetch §5.5 step 2.2).
//!
//! Note: ports 80 and 443 are NOT bad ports; they are the http/https defaults.
//! `effective_port` always returns Some(N) for absolute http/https URLs (the
//! default substitutes when the URL omits an explicit port), so an absent
//! port still goes through the table check.

/// True if `port` is on the spec's bad-port list. The list is sorted; we
/// could binary-search but linear over 83 small u16s is simpler and well
/// within fetch's per-request budget.
#[must_use]
pub fn is_bad_port(port: u16) -> bool {
    BAD_PORTS.binary_search(&port).is_ok()
}

/// The spec's bad-port table (83 entries, sorted ascending — including 0).
/// Source: https://fetch.spec.whatwg.org/#bad-port
const BAD_PORTS: &[u16] = &[
    0, 1, 7, 9, 11, 13, 15, 17, 19, 20, 21, 22, 23, 25, 37, 42, 43, 53, 69, 77, 79, 87, 95, 101,
    102, 103, 104, 109, 110, 111, 113, 115, 117, 119, 123, 135, 137, 139, 143, 161, 179, 389, 427,
    465, 512, 513, 514, 515, 526, 530, 531, 532, 540, 548, 554, 556, 563, 587, 601, 636, 989, 990,
    993, 995, 1719, 1720, 1723, 2049, 3659, 4045, 4190, 5060, 5061, 6000, 6566, 6665, 6666, 6667,
    6668, 6669, 6679, 6697, 10080,
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_is_sorted() {
        let mut prev = 0i32;
        for &p in BAD_PORTS {
            let n = p as i32;
            assert!(n >= prev, "BAD_PORTS not sorted: {n} after {prev}");
            // Allow duplicates? Spec has none. Enforce strict.
            if n == prev && prev != 0 {
                panic!("duplicate {n}");
            }
            prev = n;
        }
    }

    #[test]
    fn count_matches_spec() {
        // Per Fetch §4.3 (verified 2026-05-01).
        assert_eq!(BAD_PORTS.len(), 83);
    }

    #[test]
    fn includes_port_0() {
        assert!(is_bad_port(0));
    }

    #[test]
    fn includes_well_known_bad_ports() {
        assert!(is_bad_port(22)); // SSH
        assert!(is_bad_port(25)); // SMTP
        assert!(is_bad_port(53)); // DNS
        assert!(is_bad_port(110)); // POP3
        assert!(is_bad_port(143)); // IMAP
        assert!(is_bad_port(389)); // LDAP
        assert!(is_bad_port(6667)); // IRC
        assert!(is_bad_port(10080)); // amanda
    }

    #[test]
    fn excludes_http_https_defaults() {
        assert!(!is_bad_port(80));
        assert!(!is_bad_port(443));
        assert!(!is_bad_port(8080));
        assert!(!is_bad_port(8443));
        assert!(!is_bad_port(3000));
    }
}
