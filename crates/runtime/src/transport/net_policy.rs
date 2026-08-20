//! Outbound raw-stream policy, for `node:net`, `node:tls` and outbound
//! `WebSocket` alike. All three go through
//! [`super::egress::evaluate`]; `fetch` is the one ungated egress.
//!
//! The policy is a trusted-Rust construction property: app JavaScript cannot
//! request or widen it. `Denied` is the default: it makes `node:net`
//! unresolvable AND refuses every outbound `WebSocket`, so an app that has
//! declared no destinations opens no byte stream by any route. `Rules` narrows
//! connect targets to the creator's egress rule set; `Trusted` skips rule
//! matching but still goes through the SSRF floor, socket caps, and egress
//! caps.
//!
//! The rule set is deliberately scoped as a compromised-dependency
//! blast-radius control on RAW BYTE STREAMS, not a malicious-creator exfiltration
//! control and not an egress control in general: `fetch` is not gated and
//! reaches any public host with no rule at all, which is the surface a
//! compromised dependency would actually use. The malicious-creator controls
//! are egress attribution, spend enforcement, and hard egress ceilings.
//!
//! The creator AUTHORS the rules, through the control plane's
//! `/api/apps/{id}/egress-rules` API, bounded by their plan's caps. What app
//! JavaScript cannot do is widen its own policy: `NetPolicy` is built in
//! trusted Rust from control-plane rows the isolate cannot reach, the
//! manifest's `net.requests` entries are inert hints that never become grants
//! by being deployed, and every rule is re-validated at construction time.
//! In-band self-grant is impossible; out-of-band self-service is the design.
//!
//! Evaluation - three phases around exactly one resolution, and the platform
//! SSRF floor above every creator rule - lives in [`super::egress`].

use std::sync::atomic::{AtomicU32, Ordering};

pub use zeroship_core::net_policy::{Destination, EgressRule, EgressRules, Verdict};

/// Process-wide fallback cap. Operators lower it via
/// `ZEROSHIP_NET_GLOBAL_MAX_SOCKETS` on the process they start.
const DEFAULT_GLOBAL_MAX_SOCKETS: u32 = 4096;

static GLOBAL_ACTIVE_SOCKETS: AtomicU32 = AtomicU32::new(0);

/// The resolved process-wide cap. `0` means "not yet resolved"; the cap itself
/// is always positive, so the sentinel cannot collide with a real value.
static GLOBAL_MAX_SOCKETS: AtomicU32 = AtomicU32::new(0);

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum NetPolicy {
    #[default]
    Denied,
    Rules {
        rules: EgressRules,
        max_sockets: u32,
        egress_ceiling_bytes: u64,
    },
    Trusted {
        max_sockets: u32,
        egress_ceiling_bytes: u64,
    },
}

impl NetPolicy {
    pub fn module_allowed(&self) -> bool {
        !matches!(self, Self::Denied)
    }

    pub fn max_sockets(&self) -> u32 {
        match self {
            Self::Denied => 0,
            Self::Rules { max_sockets, .. } | Self::Trusted { max_sockets, .. } => *max_sockets,
        }
    }

    pub fn egress_ceiling_bytes(&self) -> Option<u64> {
        match self {
            Self::Rules {
                egress_ceiling_bytes,
                ..
            }
            | Self::Trusted {
                egress_ceiling_bytes,
                ..
            } => Some(*egress_ceiling_bytes),
            Self::Denied => None,
        }
    }

    /// Build a rule-set policy, re-validating every rule.
    ///
    /// There is deliberately NO `allows_host_port(host, port) -> bool` on this
    /// type. A single boolean over a name is exactly the shape that cannot
    /// express a `Range` rule, and a caller reaching for one would have to
    /// resolve first to answer it - which is the DNS gate deleted. Use
    /// [`super::egress::evaluate`], which is the only composition of the phases
    /// and keeps them in order.
    pub fn rules(
        rules: Vec<EgressRule>,
        max_sockets: u32,
        egress_ceiling_bytes: u64,
    ) -> Result<Self, String> {
        Ok(Self::Rules {
            rules: EgressRules::validated(rules)?,
            max_sockets,
            egress_ceiling_bytes,
        })
    }

    pub fn trusted(max_sockets: u32, egress_ceiling_bytes: u64) -> Self {
        Self::Trusted {
            max_sockets,
            egress_ceiling_bytes,
        }
    }
}

pub(crate) fn try_acquire_global_socket() -> Result<(), String> {
    let cap = global_max_sockets();
    let mut cur = GLOBAL_ACTIVE_SOCKETS.load(Ordering::Relaxed);
    loop {
        if cur >= cap {
            return Err(format!(
                "process-wide node:net socket cap exceeded ({cap})"
            ));
        }
        match GLOBAL_ACTIVE_SOCKETS.compare_exchange_weak(
            cur,
            cur + 1,
            Ordering::AcqRel,
            Ordering::Relaxed,
        ) {
            Ok(_) => return Ok(()),
            Err(next) => cur = next,
        }
    }
}

pub(crate) fn release_global_socket() {
    let mut cur = GLOBAL_ACTIVE_SOCKETS.load(Ordering::Relaxed);
    while cur > 0 {
        match GLOBAL_ACTIVE_SOCKETS.compare_exchange_weak(
            cur,
            cur - 1,
            Ordering::AcqRel,
            Ordering::Relaxed,
        ) {
            Ok(_) => return,
            Err(next) => cur = next,
        }
    }
}

/// The process-wide socket ceiling `node:net` connects are counted against.
///
/// PRECEDENCE. An explicit [`set_global_max_sockets`] always wins. Otherwise
/// the first call resolves the cap ONCE from
/// `ZEROSHIP_NET_GLOBAL_MAX_SOCKETS` on the process the operator started,
/// falling back to [`DEFAULT_GLOBAL_MAX_SOCKETS`], and every later call
/// returns that same answer.
#[must_use]
pub fn global_max_sockets() -> u32 {
    match GLOBAL_MAX_SOCKETS.load(Ordering::Relaxed) {
        0 => {
            let cap = zeroship_core::declared_env!(
                platform,
                "ZEROSHIP_NET_GLOBAL_MAX_SOCKETS",
                crate::RuntimeConsumer
            )
            .and_then(|s| s.parse::<u32>().ok())
            .filter(|n| *n > 0)
            .unwrap_or(DEFAULT_GLOBAL_MAX_SOCKETS);
            GLOBAL_MAX_SOCKETS.store(cap, Ordering::Relaxed);
            cap
        }
        cap => cap,
    }
}

/// State the process-wide socket ceiling explicitly, overriding
/// `ZEROSHIP_NET_GLOBAL_MAX_SOCKETS` for the rest of the process.
///
/// PRECEDENCE. This wins over the environment, whether or not
/// [`global_max_sockets`] has already resolved it.
///
/// It exists so a caller - notably a test that wants to exercise the
/// global-cap branch without opening thousands of fds - can SAY the cap rather
/// than mutate the process-global environment, which races libc `getenv`.
/// `cap` must be positive; `0` is the "unresolved" sentinel and is clamped to
/// `1`, the smallest cap that is a cap.
pub fn set_global_max_sockets(cap: u32) {
    GLOBAL_MAX_SOCKETS.store(cap.max(1), Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn accept(dest: &str, port: u16) -> EgressRule {
        EgressRule::parse(Verdict::Accept, dest, port).expect("valid rule")
    }

    /// Construction re-validates, so a rule set assembled anywhere is bounded by
    /// the same authoring rules the API applies.
    #[test]
    fn rule_construction_refuses_wildcards_and_unbounded_accept_ranges() {
        assert!(EgressRule::parse(Verdict::Accept, "*.workers.dev", 443).is_err());
        assert!(NetPolicy::rules(vec![accept("db.neon.tech", 5432)], 4, 1024).is_ok());
        assert!(EgressRule::parse(Verdict::Accept, "0.0.0.0/0", 443).is_err());
    }

    #[test]
    fn caps_are_readable_on_every_non_denied_variant() {
        let rules = NetPolicy::rules(vec![accept("db.neon.tech", 5432)], 7, 99).unwrap();
        assert_eq!(rules.max_sockets(), 7);
        assert_eq!(rules.egress_ceiling_bytes(), Some(99));
        assert_eq!(NetPolicy::trusted(3, 5).max_sockets(), 3);
        assert_eq!(NetPolicy::Denied.max_sockets(), 0);
        assert_eq!(NetPolicy::Denied.egress_ceiling_bytes(), None);
        assert!(!NetPolicy::Denied.module_allowed());
    }
}
