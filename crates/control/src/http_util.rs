//! Shared HTTP helpers (source-IP extraction, rate-limit gating).
//!
//! Lives in one place so a fix doesn't have to be applied in two
//! handlers — env_handlers and stripe_handlers used to duplicate this
//! code 1:1, which a critic flagged as a drift hazard.

use ntex::web::{self, HttpRequest};
use zeroship_auth::ratelimit::{self, Bucket, RateLimitDecision};

use crate::rate_limit::Quota;

const UNRESOLVED_CLIENT_IDENTITY: &str = "unresolved";

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
/// A last entry that is not a valid IP is ignored in favor of `peer_addr`.
///
/// The resolution itself is [`zeroship_core::client_ip`], shared with the
/// gateway and the auth service. It used to be a private copy here, which is
/// how the gateway came to read the opposite end of the same header.
pub fn source_ip(req: &HttpRequest, trust_proxy: bool) -> Option<String> {
    let xff = req
        .headers()
        .get("x-forwarded-for")
        .and_then(|value| value.to_str().ok());
    zeroship_core::client_ip::resolve_client_ip(
        xff,
        req.peer_addr().map(|addr| addr.ip()),
        trust_proxy,
    )
    .map(|ip| ip.to_string())
}

/// DB-backed token-bucket gate. Returns `Some(429)` if the IP is over
/// quota, `Some(503)` if the shared rate-limit store is unavailable,
/// and `None` to let the request proceed. Resolved clients use the same
/// identity as audit logs; unresolved clients share a bounded bucket.
pub async fn rate_limit(
    req: &HttpRequest,
    db: &compio_postgres::Client,
    namespace: &str,
    quota: Quota,
    trust_proxy: bool,
) -> Option<web::HttpResponse> {
    let identity = source_ip(req, trust_proxy)
        .unwrap_or_else(|| UNRESOLVED_CLIENT_IDENTITY.to_string());
    let key = format!("control:{namespace}:ip:{identity}");
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
        format!("{:.0}", secs.ceil().clamp(1.0, 3600.0))
    } else {
        "60".to_string()
    }
}

#[cfg(test)]
mod tests {
    use std::net::IpAddr;

    use super::*;

    use ntex::web::test::TestRequest;

    fn source_ip_of(xff: &str, trust_proxy: bool) -> Option<String> {
        // `TestRequest::peer_addr` is not plumbed into `to_http_request`
        // (ntex 3.7.2 web/test.rs:1038-1042 asserts exactly that), so these
        // exercise the header arm; the peer arm is covered in
        // `zeroship_core::client_ip`.
        source_ip(
            &TestRequest::default()
                .header("x-forwarded-for", xff)
                .to_http_request(),
            trust_proxy,
        )
    }

    #[test]
    fn malformed_trusted_xff_falls_back_to_peer_identity() {
        let peer: IpAddr = "203.0.113.10".parse().unwrap();

        assert_eq!(
            zeroship_core::client_ip::resolve_client_ip(
                Some("not-an-ip"),
                Some(peer),
                true
            ),
            Some(peer)
        );
    }

    #[test]
    fn well_formed_trusted_xff_wins_over_peer_identity() {
        let forwarded: IpAddr = "198.51.100.20".parse().unwrap();
        let peer: IpAddr = "203.0.113.20".parse().unwrap();

        assert_eq!(
            zeroship_core::client_ip::resolve_client_ip(
                Some("198.51.100.20"),
                Some(peer),
                true
            ),
            Some(forwarded)
        );
    }

    #[test]
    fn the_audit_source_ip_reads_the_same_xff_entry_as_the_gateway() {
        // `net_grants` stamps this value on a grant write, and the gateway
        // keys a rate-limit bucket on its own answer to the same question.
        // Both must name the hop the fronting proxy authored: the rightmost.
        assert_eq!(
            source_ip_of("1.2.3.4, 203.0.113.7", true),
            Some("203.0.113.7".to_string())
        );
    }

    #[test]
    fn the_audit_source_ip_records_an_address_not_a_socket() {
        // A port in the audit column is a connection, not a client, and it
        // makes two rows from one client fail to match on a join.
        assert_eq!(
            source_ip_of("192.0.2.43:40001", true),
            Some("192.0.2.43".to_string())
        );
        assert_eq!(
            source_ip_of("[2001:db8::1]:8080", true),
            Some("2001:db8::1".to_string())
        );
    }

    #[test]
    fn an_untrusted_xff_never_reaches_the_audit_record() {
        // Default posture. This fixture carries no peer, so there is nothing
        // to record rather than something the caller chose.
        assert_eq!(source_ip_of("203.0.113.7", false), None);
    }
}

// The rate-limit gate is enforced by a `zeroship.rate_limits` row, so these
// three cases need a reachable, migrated PostgreSQL and cannot run under a bare
// `cargo test --workspace`. They live in their own module because the sibling
// `tests` module above is pure and must keep running there; `required-features`
// in Cargo.toml gates whole targets and cannot reach inside a lib, so the split
// is what keeps the database-free half visible to the default build.
//
// They previously resolved their DSN from the environment and RETURNED EARLY
// when it was unset, which cargo reports as a pass - a missing database
// masquerading as coverage. Behind the gate that fallback is not needed and not
// wanted: the DSN now defaults to the same dev Postgres the rest of the control
// suite uses, and an unreachable server fails.
#[cfg(all(test, feature = "live-db-tests"))]
mod live_db_tests {
    use std::net::{IpAddr, Ipv4Addr};

    use ntex::http::StatusCode;
    use ntex::web::test::TestRequest;
    use uuid::Uuid;

    use super::*;

    async fn pg() -> compio_postgres::Client {
        let db_url = zeroship_core::config::test_database_url_opt()
            .filter(|u| !u.trim().is_empty())
            .unwrap_or_else(|| {
                "postgresql://postgres:zeroship@localhost:5440/zeroship_billing_test".to_string()
            });
        let (client, conn) = compio_postgres::connect(&db_url, compio_postgres::NoTls)
            .await
            .expect("pg connect");
        compio::runtime::spawn(async move {
            let _ = conn.run().await;
        })
        .detach();
        client
    }

    #[compio::test]
    async fn unresolved_identity_uses_shared_rate_limit_bucket() {
        let pg = pg().await;
        let namespace = format!("http-util-unresolved-{}", Uuid::new_v4().simple());
        let key = format!("control:{namespace}:ip:unresolved");
        pg.execute("DELETE FROM zeroship.rate_limits WHERE bucket_key = $1", &[&key])
            .await
            .expect("cleanup unresolved rate-limit bucket");

        let first_req = TestRequest::default().to_http_request();
        let second_req = TestRequest::default().to_http_request();
        let quota = Quota::per_minute(1, 1);

        assert!(
            rate_limit(&first_req, &pg, &namespace, quota, false)
                .await
                .is_none()
        );
        let response = rate_limit(&second_req, &pg, &namespace, quota, false)
            .await
            .expect("second unresolved request is throttled");
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);

        pg.execute("DELETE FROM zeroship.rate_limits WHERE bucket_key = $1", &[&key])
            .await
            .expect("cleanup unresolved rate-limit bucket");
    }

    #[compio::test]
    async fn distinct_resolvable_identities_use_independent_rate_limit_buckets() {
        let pg = pg().await;
        let namespace = format!("http-util-distinct-{}", Uuid::new_v4().simple());
        let first_ip = "198.51.100.31";
        let second_ip = "198.51.100.32";
        let first_key = format!("control:{namespace}:ip:{first_ip}");
        let second_key = format!("control:{namespace}:ip:{second_ip}");
        for key in [&first_key, &second_key] {
            pg.execute("DELETE FROM zeroship.rate_limits WHERE bucket_key = $1", &[key])
                .await
                .expect("cleanup rate-limit bucket");
        }

        let first_req = TestRequest::default()
            .header("x-forwarded-for", first_ip)
            .to_http_request();
        let second_req = TestRequest::default()
            .header("x-forwarded-for", second_ip)
            .to_http_request();
        let quota = Quota::per_minute(1, 1);

        assert!(rate_limit(&first_req, &pg, &namespace, quota, true).await.is_none());
        let first_response = rate_limit(&first_req, &pg, &namespace, quota, true)
            .await
            .expect("second request from first identity is throttled");
        assert_eq!(first_response.status(), StatusCode::TOO_MANY_REQUESTS);

        assert!(
            rate_limit(&second_req, &pg, &namespace, quota, true)
                .await
                .is_none(),
            "second identity has an independent bucket"
        );
        let second_response = rate_limit(&second_req, &pg, &namespace, quota, true)
            .await
            .expect("second request from second identity is throttled");
        assert_eq!(second_response.status(), StatusCode::TOO_MANY_REQUESTS);

        for key in [&first_key, &second_key] {
            pg.execute("DELETE FROM zeroship.rate_limits WHERE bucket_key = $1", &[key])
                .await
                .expect("cleanup rate-limit bucket");
        }
    }

    #[compio::test]
    async fn db_backed_rate_limit_throttles_across_calls() {
        let pg = pg().await;
        let namespace = format!("http-util-test-{}", Uuid::new_v4().simple());
        let ip = IpAddr::V4(Ipv4Addr::new(203, 0, 113, 90));
        let key = format!("control:{namespace}:ip:{ip}");
        pg.execute("DELETE FROM zeroship.rate_limits WHERE bucket_key = $1", &[&key])
            .await
            .expect("cleanup rate-limit bucket");

        // ntex's `TestRequest::peer_addr` does NOT propagate through
        // `to_http_request()` (see ntex-3.7.2/src/web/test.rs: the builder
        // stores `peer_addr` but `to_http_request` drops it, and ntex's own
        // unit test asserts `req.peer_addr() == None`). Drive this resolved-IP
        // case through the supported `X-Forwarded-For` + `trust_proxy=true`
        // path, which `source_ip` reads deterministically and which yields the
        // expected bucket key.
        let req = TestRequest::default()
            .header("x-forwarded-for", ip.to_string())
            .to_http_request();
        let quota = Quota::per_minute(1, 1);

        // Sanity-check the resolved identity this test expects the gate to use.
        assert_eq!(source_ip(&req, true).as_deref(), Some(ip.to_string().as_str()));

        // First call consumes the single token in the bucket → allowed.
        assert!(rate_limit(&req, &pg, &namespace, quota, true).await.is_none());
        // Second immediate call finds an empty bucket → throttled (429).
        let resp = rate_limit(&req, &pg, &namespace, quota, true)
            .await
            .expect("second call throttled");
        assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);

        pg.execute("DELETE FROM zeroship.rate_limits WHERE bucket_key = $1", &[&key])
            .await
            .expect("cleanup rate-limit bucket");
    }
}
