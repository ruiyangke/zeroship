//! Shared HTTP helpers (source-IP extraction, rate-limit gating).
//!
//! Lives in one place so a fix doesn't have to be applied in two
//! handlers — env_handlers and stripe_handlers used to duplicate this
//! code 1:1, which a critic flagged as a drift hazard.

use ntex::web::{self, HttpRequest};
use zeroship_auth::ratelimit::{self, Bucket, RateLimitDecision};

use crate::rate_limit::Quota;

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

/// DB-backed token-bucket gate. Returns `Some(429)` if the IP is over
/// quota, `Some(503)` if the shared rate-limit store is unavailable,
/// and `None` to let the request proceed. Uses `source_ip` so the
/// rate-limit "client identity" matches the audit-log identity.
pub async fn rate_limit(
    req: &HttpRequest,
    db: &compio_postgres::Client,
    namespace: &str,
    quota: Quota,
    trust_proxy: bool,
) -> Option<web::HttpResponse> {
    let Some(ip_str) = source_ip(req, trust_proxy) else { return None };
    if ip_str.parse::<std::net::IpAddr>().is_err() {
        return None;
    }
    let key = format!("control:{namespace}:ip:{ip_str}");
    let bucket = Bucket {
        capacity: quota.capacity,
        refill_per_sec: quota.refill_per_sec,
    };
    match ratelimit::consume_or_throttle(db, &key, bucket).await {
        Ok(RateLimitDecision::Allowed) => None,
        Ok(RateLimitDecision::Throttled(limited)) => Some(
            web::HttpResponse::TooManyRequests()
                .header("retry-after", retry_after_header(limited.retry_after_secs))
                .json(&serde_json::json!({"error": "rate limited"})),
        ),
        Err(err) => {
            tracing::error!(
                error = %err,
                bucket_key = %key,
                "control: shared rate-limit consume failed"
            );
            Some(
                web::HttpResponse::ServiceUnavailable()
                    .header("retry-after", "1")
                    .json(&serde_json::json!({"error": "rate limit unavailable"})),
            )
        }
    }
}

fn retry_after_header(secs: f64) -> String {
    if secs.is_finite() {
        format!("{:.0}", secs.ceil().max(1.0).min(3600.0))
    } else {
        "60".to_string()
    }
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr};

    use ntex::http::StatusCode;
    use ntex::web::test::TestRequest;
    use uuid::Uuid;

    use super::*;

    async fn pg() -> Option<compio_postgres::Client> {
        let db_url = std::env::var("AUTH_DB_URL")
            .or_else(|_| std::env::var("PG_TEST_URL"))
            .ok()?;
        let (client, conn) = compio_postgres::connect(&db_url, compio_postgres::NoTls)
            .await
            .expect("pg connect");
        compio::runtime::spawn(async move {
            let _ = conn.run().await;
        })
        .detach();
        Some(client)
    }

    #[compio::test]
    async fn db_backed_rate_limit_throttles_across_calls() {
        let Some(pg) = pg().await else {
            eprintln!("[http_util::tests] AUTH_DB_URL/PG_TEST_URL not set - skipping");
            return;
        };
        let namespace = format!("http-util-test-{}", Uuid::new_v4().simple());
        let ip = IpAddr::V4(Ipv4Addr::new(203, 0, 113, 90));
        let key = format!("control:{namespace}:ip:{ip}");
        pg.execute("DELETE FROM auth.rate_limits WHERE bucket_key = $1", &[&key])
            .await
            .expect("cleanup rate-limit bucket");

        // ntex's `TestRequest::peer_addr` does NOT propagate through
        // `to_http_request()` (see ntex-3.7.2/src/web/test.rs: the builder
        // stores `peer_addr` but `to_http_request` drops it, and ntex's own
        // unit test asserts `req.peer_addr() == None`). So `source_ip(req,
        // trust_proxy=false)` resolves to `None` and `rate_limit` short-circuits
        // to "allowed" on every call — the original test never exercised
        // throttling at all. Drive the client identity through the supported
        // `X-Forwarded-For` + `trust_proxy=true` path instead, which `source_ip`
        // reads deterministically and which yields the same bucket key.
        let req = TestRequest::default()
            .header("x-forwarded-for", ip.to_string())
            .to_http_request();
        let quota = Quota::per_minute(1, 1);

        // Sanity-check the precondition this test depends on: the identity the
        // gate will key on must resolve, otherwise `rate_limit` no-ops.
        assert_eq!(source_ip(&req, true).as_deref(), Some(ip.to_string().as_str()));

        // First call consumes the single token in the bucket → allowed.
        assert!(rate_limit(&req, &pg, &namespace, quota, true).await.is_none());
        // Second immediate call finds an empty bucket → throttled (429).
        let resp = rate_limit(&req, &pg, &namespace, quota, true)
            .await
            .expect("second call throttled");
        assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);

        pg.execute("DELETE FROM auth.rate_limits WHERE bucket_key = $1", &[&key])
            .await
            .expect("cleanup rate-limit bucket");
    }
}
