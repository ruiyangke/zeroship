//! Shared port allow/deny lists for the sandbox preview-URL proxy.
//!
//! The controller (`zeroship-sandbox`) uses these constants to refuse
//! a preview before it ever signs an outbound request to the agent.
//! The agent (`zeroship-sandbox-agent`) uses the EXACT same constants
//! before dialing `127.0.0.1:{port}` inside the VM. Sharing them via
//! this module is the defense-in-depth invariant from § II.1 / I9 of
//! `docs/proposals/sandbox-preview-urls.md`: a controller bug that
//! lets a denied port through still bounces at the agent.
//!
//! ## What lives here
//!
//! - [`HARDCODED_DENY`] — never overridable. Includes the agent's own
//!   listening port (anti-loop), the V8 inspector port (RCE-as-a-feature
//!   over plaintext WS), SSH (also <1024 below), and 0 (invalid).
//! - [`DEFAULT_DENY`] — operator-overridable; ships with high-risk
//!   service ports across infra categories (DBs, message queues,
//!   service-discovery, debug ports). Defense-in-depth against creators
//!   who unwittingly expose a database, queue, or debug port to the
//!   public preview URL.
//! - [`is_proxyable_port`] — composed predicate. `true` iff the port
//!   is in `[1024, 65535]`, not in `HARDCODED_DENY`, and not in the
//!   provided dynamic deny-list.
//!
//! ## Wire-stable contract
//!
//! These constants are referenced by both the controller and the agent.
//! Changing `HARDCODED_DENY` is a wire-protocol change (an agent built
//! against an older `HARDCODED_DENY` could accept a port a newer
//! controller refuses, or vice-versa). Treat additions as wire bumps:
//! roll the list out to agents first, then to controllers.

/// Agent's own listening port. Mirrors `zeroship_sandbox_agent::AGENT_PORT`
/// (kept as a literal here rather than re-exporting it from the agent
/// crate to avoid a crate-graph edge from `zeroship-core` to the agent;
/// the value `7777` is an immutable wire constant).
pub const AGENT_PORT: u16 = 7777;

/// Ports the proxy MUST NEVER serve. Operator config CANNOT override
/// these — the deny is hard-coded as a security invariant.
///
/// - `0` — invalid (every OS rejects bind anyway, but defense-in-depth).
/// - `7777` — agent's own port; proxying through the agent's loopback
///   to itself is an obvious DoS amplification.
/// - `9229` — Node `--inspect` / `--inspect-brk`. The V8 inspector
///   debug protocol allows arbitrary code execution in the inspected
///   process over plaintext WebSocket. Any creator-app that exposes
///   9229 (intentionally or by accident) would otherwise hand the
///   public-edge URL to RCE-as-a-feature.
/// - `22` — SSH. Also <1024 below; explicit for documentation +
///   defense-in-depth if `<1024` ever gets relaxed for some legacy
///   reason.
pub const HARDCODED_DENY: &[u16] = &[0, AGENT_PORT, 9229, 22];

/// Default per-deployment deny-list. Operator-overridable via the
/// controller's runtime config; pushed to agents at sandbox-create
/// time. Ships restrictive — the right default for "creator's local
/// dev workflow."
///
/// Categories:
/// - Java debug protocols: 1099 (JMX RMI), 5005 (JDWP)
/// - In-memory data: 6379 (Redis), 11211 (memcached)
/// - Databases: 27017 (MongoDB), 3306 (MySQL), 5432 (PostgreSQL)
/// - Search: 9200 (ES HTTP), 9300 (ES transport)
/// - Message queues: 5672 (AMQP), 61616 (ActiveMQ), 9092 (Kafka)
/// - Service discovery / config stores: 2379 (etcd client), 2380 (etcd peer)
pub const DEFAULT_DENY: &[u16] = &[
    1099, 5005,        // Java debug
    6379, 11211,       // Redis, memcached
    27017,             // Mongo
    3306, 5432,        // MySQL, Postgres
    9200, 9300,        // Elasticsearch HTTP + transport
    5672, 61616, 9092, // AMQP, ActiveMQ, Kafka
    2379, 2380,        // etcd client + peer
];

/// `true` iff `port` is acceptable to proxy. The composed rule:
///
/// 1. Reject anything in [`HARDCODED_DENY`] — no operator override.
/// 2. Reject privileged ports (port < 1024) — kernel restricts non-root
///    binds, but PID-1 inside libkrun is root, so a creator *can* bind
///    23 (telnet), 25 (SMTP), etc. We refuse to proxy any of them.
/// 3. Reject anything in the caller-supplied dynamic deny-list
///    (operator-tunable; mirrors [`DEFAULT_DENY`] in production).
///
/// Both the controller (in `authorize`, before signing) and the agent
/// (in `proxy_http`, before dialing the upstream) call this. The
/// dynamic deny-list MUST be identical at both layers; the controller
/// pushes its config to the agent at sandbox-create time.
pub fn is_proxyable_port(port: u16, dynamic_deny: &[u16]) -> bool {
    if HARDCODED_DENY.contains(&port) {
        return false;
    }
    if port < 1024 {
        return false;
    }
    if dynamic_deny.contains(&port) {
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hardcoded_zero_denied() {
        assert!(!is_proxyable_port(0, &[]));
    }

    #[test]
    fn hardcoded_agent_port_denied() {
        assert!(!is_proxyable_port(AGENT_PORT, &[]));
        assert!(!is_proxyable_port(7777, &[]));
    }

    #[test]
    fn hardcoded_v8_inspector_denied() {
        assert!(!is_proxyable_port(9229, &[]));
    }

    #[test]
    fn hardcoded_ssh_denied() {
        assert!(!is_proxyable_port(22, &[]));
    }

    #[test]
    fn privileged_ports_denied() {
        for p in [1, 80, 443, 1023] {
            assert!(!is_proxyable_port(p, &[]), "port {p} should be denied");
        }
    }

    #[test]
    fn typical_dev_ports_allowed() {
        for p in [3000u16, 5173, 8080, 8000, 4000, 1337] {
            assert!(
                is_proxyable_port(p, &[]),
                "port {p} should be allowed (no deny entry)"
            );
        }
    }

    #[test]
    fn dynamic_deny_takes_effect() {
        assert!(is_proxyable_port(5173, &[]));
        assert!(!is_proxyable_port(5173, &[5173]));
    }

    #[test]
    fn default_deny_blocks_databases() {
        for p in [3306u16, 5432, 6379, 27017, 9200] {
            assert!(
                !is_proxyable_port(p, DEFAULT_DENY),
                "DEFAULT_DENY should block {p}"
            );
        }
    }

    #[test]
    fn hardcoded_deny_unique() {
        let mut sorted = HARDCODED_DENY.to_vec();
        sorted.sort();
        let len = sorted.len();
        sorted.dedup();
        assert_eq!(sorted.len(), len, "HARDCODED_DENY has duplicates");
    }

    #[test]
    fn default_deny_unique() {
        let mut sorted = DEFAULT_DENY.to_vec();
        sorted.sort();
        let len = sorted.len();
        sorted.dedup();
        assert_eq!(sorted.len(), len, "DEFAULT_DENY has duplicates");
    }

    #[test]
    fn high_port_boundary_allowed() {
        // 65535 is the maximum TCP port. 1024 is the lowest non-privileged.
        assert!(is_proxyable_port(1024, &[]));
        assert!(is_proxyable_port(65535, &[]));
    }
}
