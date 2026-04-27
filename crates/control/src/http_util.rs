//! Shared HTTP helpers (source-IP extraction, rate-limit gating).
//!
//! Lives in one place so a fix doesn't have to be applied in two
//! handlers — env_handlers and stripe_handlers used to duplicate this
//! code 1:1, which a critic flagged as a drift hazard.

use ntex::web::{self, HttpRequest};

use crate::rate_limit::RateLimiter;

/// Resolve the source IP for audit + rate-limit purposes.
///
/// `trust_proxy=false` (default): IGNORE `X-Forwarded-For` entirely.
/// Only `peer_addr` is used. This is the safe default — anyone with a
/// direct TCP path to the control plane (mis-configured firewall, dev
/// loopback, intra-cluster reach) can otherwise spoof XFF and:
///   1. bypass per-IP rate limiting (each spoofed IP gets its own bucket)
///   2. pollute audit logs with attacker-chosen "source" IPs
///
/// `trust_proxy=true`: read the LAST entry of `X-Forwarded-For` —
/// that's what the trusted proxy reported as the connecting client.
/// The first entry is what the client itself CLAIMED, which is
/// untrusted. Operators must only set --trust-proxy when the control
/// plane is bound behind a load balancer they trust to overwrite XFF.
pub fn source_ip(req: &HttpRequest, trust_proxy: bool) -> Option<String> {
    if trust_proxy {
        if let Some(xff) = req
            .headers()
            .get("x-forwarded-for")
            .and_then(|v| v.to_str().ok())
        {
            // Last entry = closest hop = the trusted proxy's view.
            if let Some(last) = xff.rsplit(',').next() {
                let trimmed = last.trim();
                if !trimmed.is_empty() {
                    return Some(trimmed.to_string());
                }
            }
        }
    }
    req.peer_addr().map(|a| a.ip().to_string())
}

/// Token-bucket gate. Returns `Some(429)` if the IP is over quota,
/// `None` to let the request proceed. Uses `source_ip` so the
/// rate-limit "client identity" matches the audit-log identity.
pub fn rate_limit(
    req: &HttpRequest,
    limiter: &RateLimiter,
    trust_proxy: bool,
) -> Option<web::HttpResponse> {
    let Some(ip_str) = source_ip(req, trust_proxy) else { return None };
    let Ok(ip) = ip_str.parse() else { return None };
    if limiter.check(ip) {
        None
    } else {
        Some(
            web::HttpResponse::TooManyRequests()
                .header("retry-after", "1")
                .json(&serde_json::json!({"error": "rate limited"})),
        )
    }
}
