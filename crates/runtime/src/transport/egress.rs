//! The `node:net` egress evaluator: three phases around exactly ONE resolution.
//!
//! The rule types and their per-phase queries live in
//! [`zeroship_core::net_policy`]; this module is where they compose with the
//! platform's SSRF floor and with DNS. For a connect to `(target, port)`:
//!
//! ```text
//! PHASE 1 - name phase. No DNS. Skipped entirely when `target` is an IP literal.
//!   1. If any Name REJECT rule matches (target, port)        -> REFUSE. Terminal.
//!   2. name_accepted := some Name ACCEPT rule matches (target, port)
//!   3. If not name_accepted and no Range ACCEPT rule at `port` -> REFUSE. Terminal.
//!      [the DNS gate: the ONLY place a refusal is decided by absence]
//!
//! PHASE 2 - resolve once. Answer set A (all families, all addresses).
//!      For an IP literal, A = { literal } and no query is made.
//!
//! PHASE 3 - address phase, applied to EVERY member of A:
//!   4. Drop a if is_blocked_ip(a).                     [INVARIANT GRANTS-NARROW]
//!   5. Drop a if any Range REJECT rule matches (a, port).
//!   6. Keep a iff name_accepted OR some Range ACCEPT rule matches (a, port).
//!   7. If the surviving set is empty -> REFUSE, naming WHICH of 4, 5 or 6
//!      emptied it.
//!   8. Connect using the surviving set.
//! ```
//!
//! Read against the lattice `platform floor > creator REJECT > creator ACCEPT >
//! default(refuse)`: step 4 is the floor, step 5 is REJECT, step 6 is
//! ACCEPT-or-default. **The order of rules is never consulted**, so the verdict
//! is a pure function of the rule SET. `egress_verdict_is_independent_of_rule_order`
//! is what stops that becoming untrue.
//!
//! # INVARIANT GRANTS-NARROW
//!
//! Every address a connect may use has passed `is_blocked_ip`. No creator rule
//! of any verdict can cause an address the floor refuses to be used, and no
//! creator rule is consulted for an address the floor has already removed. A
//! granted name resolving into private space is REFUSED, with the floor's error
//! rather than the grant's. Step 4 running first, unconditionally, and outside
//! any rule lookup is the whole of the mechanism.
//!
//! # INVARIANT ONE-RESOLUTION
//!
//! One resolution per connect, and every predicate that consumes an address
//! consumes that resolution's output. An implementation that finds itself
//! resolving twice has taken a wrong turn, and the likely wrong turn is
//! evaluating rules in list order.
//!
//! # INVARIANT ONE-COMPOSITION
//!
//! [`evaluate`] is the ONLY place the three phases are composed, and every
//! caller - production and test alike - goes through it. Phase 1 and phase 3
//! are private to this module for that reason: a second caller that ran them
//! itself would be a second DNS gate, and a test of one would say nothing
//! about the other. The `node:net` connect task calls [`evaluate`] and does
//! nothing with the phases but render the refusal it returns.

use std::fmt;
use std::future::Future;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::RwLock;
use std::time::Duration;

use zeroship_core::net_policy::{AddressPhase, NamePhase};

use super::net_policy::NetPolicy;
use super::ssrf::{dev_mode_enabled, is_blocked_ip};

/// Fallback bound on PHASE 2. Overridable with
/// `ZEROSHIP_NET_RESOLVE_TIMEOUT_MS`; the timeout fails CLOSED.
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(5);

/// The resolved PHASE 2 bound, in milliseconds. `0` means "not yet resolved";
/// the bound itself is always positive, so the sentinel cannot collide with a
/// real value.
static RESOLVE_TIMEOUT_MS: AtomicU64 = AtomicU64::new(0);

/// Debug-only hold on [`blocking_resolve`], as an explicit process-level
/// setting: `Some((host, delay))` sleeps `delay` before resolving exactly
/// `host`. `None` (the default, and the only value production ever holds)
/// resolves immediately.
///
/// The consult site below is `#[cfg(debug_assertions)]`, so a release build
/// carries no hold at all - the same shape as the `ZEROSHIP_NET_TEST_DNS_HANG_*`
/// reads this replaces. The setter is compiled unconditionally so a
/// `--release` test run still builds; it just stores into a cell nothing reads.
static RESOLVE_HANG: RwLock<Option<(String, Duration)>> = RwLock::new(None);

/// Count of resolutions the DNS gate OPENED - lookups performed for a name no
/// `Name` rule admitted, which happen only because the app holds a `Range`
/// ACCEPT at the requested port.
///
/// This is the channel the gate partitions but cannot close: for an app in the
/// leaking class every refused connect still carries an attacker-chosen label to
/// an attacker-chosen nameserver. Counting it is not closing it; it is the one
/// piece of observability available, and a rate bound on these is the obvious
/// follow-on.
///
/// It counts what SHIPS: [`evaluate`] is the connect path for `node:net`,
/// `node:tls` and outbound `WebSocket` alike, so every increment is a real
/// lookup a real app performed. Read it with [`gate_opened_resolutions`] -
/// process-wide, monotonic, and cheap enough to sample on any interval. It is
/// paired with a per-occurrence log line, because a counter alone says how many
/// and never which app or which label.
static GATE_OPENED_RESOLUTIONS: AtomicU64 = AtomicU64::new(0);

/// Total resolutions the DNS gate has opened in this process.
///
/// Monotonic and never reset, so a sampler takes deltas. Exported at the crate
/// root for the embedding process (worker, CLI) to poll.
#[must_use]
pub fn gate_opened_resolutions() -> u64 {
    GATE_OPENED_RESOLUTIONS.load(Ordering::Relaxed)
}

/// A PHASE 2 failure. Not a policy outcome, which is why it carries the error
/// CODE the socket will report: the caller renders it, it does not re-classify
/// a string back into a kind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolveFailure {
    pub code: &'static str,
    pub message: String,
}

/// The future a resolver returns. Boxed so the resolver can be held as
/// `dyn EgressResolver` in the runtime state, which is what makes the seam
/// injectable from an integration test rather than only from a unit test.
pub type ResolveFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Vec<SocketAddr>, ResolveFailure>> + 'a>>;

/// PHASE 2, as an injectable seam.
///
/// Injectable so a test can observe WHETHER a lookup happened, which is the
/// only way the DNS gate is testable at all: "refused promptly" is a timing
/// claim, and a timing claim cannot distinguish a gate that held from a resolver
/// that happened to be fast.
pub trait EgressResolver {
    /// Resolve to the FULL answer set - every family, every address. Returning
    /// a prefix of the answer would silently reintroduce the first-address
    /// semantics phase 3 exists to replace.
    fn resolve<'a>(&'a self, host: &'a str, port: u16) -> ResolveFuture<'a>;
}

/// The production resolver: the platform's own, which app code cannot route
/// around. The runtime registers no `node:dns`, no datagram socket and no raw
/// socket, so every name an app resolves is resolved here.
///
/// It owns the whole of phase 2 - the off-thread lookup AND its timeout - so
/// that swapping it out in a test swaps out everything between the gate and the
/// address phase, leaving nothing untested behind the seam.
pub struct SystemResolver;

impl EgressResolver for SystemResolver {
    fn resolve<'a>(&'a self, host: &'a str, port: u16) -> ResolveFuture<'a> {
        let host = host.to_string();
        Box::pin(async move {
            let owned = host.clone();
            let joined = compio::time::timeout(
                resolve_timeout(),
                compio::runtime::spawn_blocking(move || blocking_resolve(&owned, port)),
            )
            .await;
            match joined {
                Ok(Ok(Ok(answer))) => Ok(answer),
                Ok(Ok(Err(e))) => Err(ResolveFailure {
                    code: "ERR_NET_SSRF",
                    message: format!("SSRF: {e}"),
                }),
                Ok(Err(_join)) => Err(ResolveFailure {
                    code: "ERR_NET_DNS",
                    message: "DNS resolve task failed".to_string(),
                }),
                Err(_) => Err(ResolveFailure {
                    code: "ERR_NET_DNS_TIMEOUT",
                    message: "DNS resolve timed out".to_string(),
                }),
            }
        })
    }
}

fn blocking_resolve(host: &str, port: u16) -> Result<Vec<SocketAddr>, String> {
    // A debug-only hook so `dns_timeout_fails_closed` can hold the lookup open
    // without depending on a real slow nameserver.
    #[cfg(debug_assertions)]
    {
        let hang = RESOLVE_HANG
            .read()
            .unwrap_or_else(|err| err.into_inner())
            .as_ref()
            .filter(|(hang_host, _)| hang_host == host)
            .map(|(_, delay)| *delay);
        if let Some(delay) = hang {
            std::thread::sleep(delay);
        }
    }
    // Strip IPv6 literal brackets before to_socket_addrs.
    let host_clean = host.trim_start_matches('[').trim_end_matches(']');
    let target = format!("{host_clean}:{port}");
    let addrs: Vec<SocketAddr> = std::net::ToSocketAddrs::to_socket_addrs(&target)
        .map_err(|e| format!("DNS resolve failed: {e}"))?
        .collect();
    if addrs.is_empty() {
        return Err(format!("DNS resolve produced no addresses for {host}:{port}"));
    }
    Ok(addrs)
}

/// The bound PHASE 2 runs under.
///
/// PRECEDENCE. An explicit [`set_resolve_timeout`] always wins. Otherwise the
/// first call resolves the bound ONCE from `ZEROSHIP_NET_RESOLVE_TIMEOUT_MS`
/// on the process the operator started, falling back to [`RESOLVE_TIMEOUT`],
/// and every later call returns that same answer.
#[must_use]
pub fn resolve_timeout() -> Duration {
    match RESOLVE_TIMEOUT_MS.load(Ordering::Relaxed) {
        0 => {
            let timeout = zeroship_core::declared_env!(
                platform,
                "ZEROSHIP_NET_RESOLVE_TIMEOUT_MS",
                crate::RuntimeConsumer
            )
            .and_then(|s| s.parse::<u64>().ok())
            .filter(|ms| *ms > 0)
            .map(Duration::from_millis)
            .unwrap_or(RESOLVE_TIMEOUT);
            RESOLVE_TIMEOUT_MS.store(
                u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX),
                Ordering::Relaxed,
            );
            timeout
        }
        ms => Duration::from_millis(ms),
    }
}

/// State the PHASE 2 bound explicitly, overriding
/// `ZEROSHIP_NET_RESOLVE_TIMEOUT_MS` for the rest of the process.
///
/// PRECEDENCE. This wins over the environment, whether or not
/// [`resolve_timeout`] has already resolved it.
///
/// It exists so a caller - notably a test proving the timeout fails CLOSED -
/// can SAY the bound rather than mutate the process-global environment, which
/// races libc `getenv`. A zero bound would mean "unresolved", so it is clamped
/// to 1ms.
pub fn set_resolve_timeout(timeout: Duration) {
    let ms = u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX);
    RESOLVE_TIMEOUT_MS.store(ms.max(1), Ordering::Relaxed);
}

/// Hold PHASE 2's blocking lookup open for `host` by `delay`, or clear the
/// hold with `None`.
///
/// Debug builds only: the consult site inside `blocking_resolve` is
/// `#[cfg(debug_assertions)]`, so in a release build this stores into a cell
/// nothing reads. It exists so `dns_timeout_fails_closed` can make a real
/// lookup slow without depending on a real slow nameserver, and without
/// mutating the process environment.
pub fn set_resolve_hang(hang: Option<(String, Duration)>) {
    *RESOLVE_HANG.write().unwrap_or_else(|err| err.into_inner()) = hang;
}

/// Why a connect was refused. The three-way distinction inside
/// [`Self::NoAddressSurvived`] is a requirement, not diagnostics polish: a
/// v4-only `Range` ACCEPT silently drops every AAAA answer at step 6, and
/// without the distinction the app sees an error identical to a broken name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EgressRefusal {
    /// The app has no raw-TCP policy at all; `node:net` is not even resolvable.
    ModuleDenied,
    /// Step 1. Decided before DNS.
    NameRejected { target: String, port: u16 },
    /// Step 3, the DNS gate. Decided before DNS, by absence.
    NoRuleCouldAdmit { target: String, port: u16 },
    /// Phase 2 failed. Not a policy outcome.
    ResolveFailed(ResolveFailure),
    /// Step 7. Carries which of steps 4, 5 and 6 emptied the set.
    NoAddressSurvived {
        /// Step 4 - refused by the platform floor. No rule can change this.
        floor: Vec<IpAddr>,
        /// Step 5 - refused by the app's OWN `Range` REJECT rules.
        range_rejected: Vec<IpAddr>,
        /// Step 6 - matched no ACCEPT rule.
        unmatched: Vec<IpAddr>,
    },
}

/// The error CODE and message a refusal reports, for every transport.
///
/// One function, because 5.7's requirement is a THREE-way distinction the app
/// can act on - the platform floor, the creator's own REJECT, and a lookup that
/// simply failed - and two transports classifying it separately is two
/// classifications that will disagree. `node:net` renders these onto the
/// socket's `error` event and WebSocket onto its `error` event; neither decides
/// the split itself.
#[must_use]
pub fn refusal_report(refusal: &EgressRefusal) -> (&'static str, String) {
    match refusal {
        // Not a policy outcome at all: the resolver already said which code.
        EgressRefusal::ResolveFailed(failure) => (failure.code, failure.message.clone()),
        EgressRefusal::NoAddressSurvived { floor, .. } if !floor.is_empty() => {
            ("ERR_NET_SSRF", format!("SSRF: {refusal}"))
        }
        _ => ("ERR_NET_EGRESS_DENIED", format!("egress: {refusal}")),
    }
}

fn join(addrs: &[IpAddr]) -> String {
    addrs
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

impl fmt::Display for EgressRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ModuleDenied => write!(f, "raw TCP is not enabled for this app"),
            Self::NameRejected { target, port } => write!(
                f,
                "refused before DNS: an egress REJECT rule names {target}:{port}"
            ),
            Self::NoRuleCouldAdmit { target, port } => write!(
                f,
                "refused before DNS: no egress rule could admit {target}:{port}, \
                 so the name was not resolved"
            ),
            Self::ResolveFailed(e) => write!(f, "{}", e.message),
            Self::NoAddressSurvived {
                floor,
                range_rejected,
                unmatched,
            } => {
                let mut parts = Vec::new();
                if !floor.is_empty() {
                    parts.push(format!(
                        "no address survived the platform SSRF floor ({})",
                        join(floor)
                    ));
                }
                if !range_rejected.is_empty() {
                    parts.push(format!(
                        "refused by your own REJECT rule: {}",
                        join(range_rejected)
                    ));
                }
                if !unmatched.is_empty() {
                    parts.push(format!(
                        "matched no ACCEPT rule: {}",
                        join(unmatched)
                    ));
                }
                if parts.is_empty() {
                    parts.push("the resolved answer set was empty".to_string());
                }
                write!(f, "egress refused: {}", parts.join("; "))
            }
        }
    }
}

/// What phase 1 concluded. Private: it is a step inside [`evaluate`], never a
/// value another module carries around and acts on (INVARIANT ONE-COMPOSITION).
#[derive(Debug, Clone, PartialEq, Eq)]
enum PreDns {
    /// `target` was an IP literal: phase 1 was skipped, the answer set is
    /// `{ literal }`, and no query will be made for it. An app connecting by
    /// address leaks nothing regardless of its rule set.
    Literal(IpAddr),
    /// Undecided before DNS. Resolve ONCE, then call [`filter_answer`] with
    /// this `name_accepted`.
    Resolve { name_accepted: bool },
}

/// PHASE 1. Pure, synchronous, and performs no I/O of any kind.
///
/// Private, and called from exactly one place: [`evaluate`], immediately before
/// the resolution it gates. That single call site IS the DNS gate - the
/// ordering cannot be got wrong somewhere else because there is nowhere else.
fn pre_dns(policy: &NetPolicy, target: &str, port: u16) -> Result<PreDns, EgressRefusal> {
    if !policy.module_allowed() {
        return Err(EgressRefusal::ModuleDenied);
    }

    // An IP literal skips phase 1 entirely: no Name rule can be about it, and
    // only a Range ACCEPT can admit it (phase 3, with name_accepted = false).
    let bare = target.trim_start_matches('[').trim_end_matches(']');
    if let Ok(ip) = bare.parse::<IpAddr>() {
        return Ok(PreDns::Literal(ip));
    }

    match policy {
        NetPolicy::Denied => Err(EgressRefusal::ModuleDenied),
        // Trusted holds no creator rules, so there is nothing to gate on and
        // nothing for phase 3 to match: the floor is the whole policy.
        NetPolicy::Trusted { .. } => Ok(PreDns::Resolve {
            name_accepted: true,
        }),
        NetPolicy::Rules { rules, .. } => match rules.name_phase(target, port) {
            NamePhase::NameRejected => Err(EgressRefusal::NameRejected {
                target: target.to_string(),
                port,
            }),
            NamePhase::NoRuleCouldAdmit => Err(EgressRefusal::NoRuleCouldAdmit {
                target: target.to_string(),
                port,
            }),
            NamePhase::Resolve { name_accepted } => Ok(PreDns::Resolve { name_accepted }),
        },
    }
}

/// PHASE 3, applied to EVERY member of `answer`.
///
/// Returns ALL the survivors, in the resolver's own order, so happy-eyeballs
/// preference is preserved among addresses the policy admits.
/// `filter_answer_keeps_every_survivor_in_resolver_order` is what stops that
/// becoming a claim nothing holds.
///
/// Private for the same reason [`pre_dns`] is: phase 3 reached from anywhere
/// but [`evaluate`] is a second composition.
fn filter_answer(
    policy: &NetPolicy,
    port: u16,
    name_accepted: bool,
    answer: Vec<SocketAddr>,
) -> Result<Vec<SocketAddr>, EgressRefusal> {
    let dev_mode = dev_mode_enabled();
    let mut kept = Vec::with_capacity(answer.len());
    let mut floor = Vec::new();
    let mut range_rejected = Vec::new();
    let mut unmatched = Vec::new();

    for sa in answer {
        let ip = sa.ip();

        // STEP 4 - the platform floor. FIRST, unconditional, and outside any
        // rule lookup: INVARIANT GRANTS-NARROW is exactly the claim that no
        // arm below can be reached for an address this drops. Moving this
        // after step 6, or conditioning it on `name_accepted` or on an ACCEPT
        // match, lets a creator grant widen the floor.
        if !dev_mode && is_blocked_ip(ip) {
            floor.push(ip);
            continue;
        }

        match policy {
            NetPolicy::Denied => return Err(EgressRefusal::ModuleDenied),
            NetPolicy::Trusted { .. } => kept.push(sa),
            NetPolicy::Rules { rules, .. } => {
                match rules.address_phase(ip, port, name_accepted) {
                    AddressPhase::Admitted => kept.push(sa),
                    AddressPhase::RangeRejected => range_rejected.push(ip),
                    AddressPhase::NoAcceptMatched => unmatched.push(ip),
                }
            }
        }
    }

    if kept.is_empty() {
        return Err(EgressRefusal::NoAddressSurvived {
            floor,
            range_rejected,
            unmatched,
        });
    }
    Ok(kept)
}

/// The whole evaluator: phases 1, 2 and 3, with exactly one call into
/// `resolver` and none at all for an IP literal.
///
/// The ONLY entry point (INVARIANT ONE-COMPOSITION). It is `async` because the
/// composition is sequenced around I/O: a synchronous version could not contain
/// the resolution, and a caller that supplied the resolution itself would be
/// composing the phases a second time.
pub async fn evaluate(
    policy: &NetPolicy,
    target: &str,
    port: u16,
    resolver: &dyn EgressResolver,
) -> Result<Vec<SocketAddr>, EgressRefusal> {
    match pre_dns(policy, target, port)? {
        PreDns::Literal(ip) => filter_answer(policy, port, false, vec![SocketAddr::new(ip, port)]),
        PreDns::Resolve { name_accepted } => {
            if !name_accepted {
                GATE_OPENED_RESOLUTIONS.fetch_add(1, Ordering::Relaxed);
                // The label is about to reach a nameserver the app does not
                // control, for a destination no rule names. One line per
                // occurrence is affordable next to the DNS round trip it
                // precedes, and it is the only record that says WHICH label
                // left - the counter says how many and nothing else.
                tracing::warn!(
                    target = %target,
                    port,
                    "runtime: DNS gate opened - resolving a name no egress rule admits, \
                     because the app holds a range accept at this port"
                );
            }
            let answer = resolver
                .resolve(target, port)
                .await
                .map_err(EgressRefusal::ResolveFailed)?;
            filter_answer(policy, port, name_accepted, answer)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use zeroship_core::net_policy::{EgressRule, EgressRules, Verdict};

    /// A resolver that RECORDS every lookup. "A lookup happened" is not
    /// otherwise observable, and an unobservable property is an unenforceable
    /// one: without this, an evaluator that resolved unconditionally would pass
    /// every gate test in this module.
    struct RecordingResolver {
        answers: Vec<SocketAddr>,
        calls: RefCell<Vec<(String, u16)>>,
    }

    impl RecordingResolver {
        fn new(answers: &[&str]) -> Self {
            Self {
                answers: answers.iter().map(|a| a.parse().unwrap()).collect(),
                calls: RefCell::new(Vec::new()),
            }
        }
        fn lookups(&self) -> usize {
            self.calls.borrow().len()
        }
    }

    impl EgressResolver for RecordingResolver {
        fn resolve<'a>(&'a self, host: &'a str, port: u16) -> ResolveFuture<'a> {
            self.calls.borrow_mut().push((host.to_string(), port));
            let answers = self.answers.clone();
            Box::pin(async move { Ok(answers) })
        }
    }

    /// `evaluate` is async because production's phase 2 is. The recording
    /// resolver performs no I/O, so a bare executor is enough and no reactor
    /// needs to exist for these rows.
    fn evaluate(
        policy: &NetPolicy,
        target: &str,
        port: u16,
        resolver: &dyn EgressResolver,
    ) -> Result<Vec<SocketAddr>, EgressRefusal> {
        futures::executor::block_on(super::evaluate(policy, target, port, resolver))
    }

    fn rule(verdict: Verdict, dest: &str, port: u16) -> EgressRule {
        EgressRule::parse(verdict, dest, port).expect("valid rule")
    }

    fn policy(rules: Vec<EgressRule>) -> NetPolicy {
        NetPolicy::rules(rules, 4, 1024 * 1024).expect("valid rule set")
    }

    // `198.51.100.0/24` and its siblings CANNOT be used in these tests: they are
    // TEST-NET documentation space, which `is_blocked_ip` refuses at step 4, so
    // every one of them would refuse for the floor's reason no matter what the
    // rule arms did. `93.184.216.0/24` is public space the floor permits, which
    // is what makes the creator-rule arms observable at all.
    const PUBLIC_RANGE: &str = "93.184.216.0/24";
    const IN_RANGE: &str = "93.184.216.34";
    const IN_RANGE_CARVED: &str = "93.184.216.7";
    const OUT_OF_RANGE: &str = "203.0.114.9";

    /// **E1 - the precedence pin.** The SAME two rules in BOTH orders must give
    /// identical verdicts. Under first-match-wins the two orders disagree; this
    /// is the row that fails if anyone "improves" the evaluator into an ordered
    /// walk.
    ///
    /// The carved-out address is in PUBLIC space on purpose: with a TEST-NET
    /// range both orders would refuse via the platform floor and the test would
    /// be green against an evaluator with no REJECT arm at all.
    #[test]
    fn egress_verdict_is_independent_of_rule_order() {
        let accept = rule(Verdict::Accept, PUBLIC_RANGE, 443);
        let deny = rule(
            Verdict::Reject,
            &format!("{IN_RANGE_CARVED}/32"),
            443,
        );

        let forward = policy(vec![accept.clone(), deny.clone()]);
        let backward = policy(vec![deny, accept]);

        let r1 = RecordingResolver::new(&[&format!("{IN_RANGE_CARVED}:443")]);
        let r2 = RecordingResolver::new(&[&format!("{IN_RANGE_CARVED}:443")]);
        let a = evaluate(&forward, "host.example.test", 443, &r1);
        let b = evaluate(&backward, "host.example.test", 443, &r2);

        assert_eq!(a, b, "the verdict must be a function of the rule SET");
        assert_eq!(
            a,
            Err(EgressRefusal::NoAddressSurvived {
                floor: vec![],
                range_rejected: vec![IN_RANGE_CARVED.parse().unwrap()],
                unmatched: vec![],
            }),
            "and it must be the REJECT arm that refuses, not the floor"
        );

        // Control: the same two rule sets admit an address the carve-out misses,
        // so E1's refusal is the REJECT rule and not a policy that refuses
        // everything.
        let r3 = RecordingResolver::new(&[&format!("{IN_RANGE}:443")]);
        let r4 = RecordingResolver::new(&[&format!("{IN_RANGE}:443")]);
        assert!(evaluate(&forward, "host.example.test", 443, &r3).is_ok());
        assert!(evaluate(&backward, "host.example.test", 443, &r4).is_ok());
    }

    /// **E2** - the lattice's REJECT > ACCEPT edge on the degenerate overlap.
    #[test]
    fn egress_reject_beats_accept_on_the_same_destination() {
        let p = policy(vec![
            rule(Verdict::Accept, "api.example.test", 443),
            rule(Verdict::Reject, "api.example.test", 443),
        ]);
        let r = RecordingResolver::new(&[&format!("{IN_RANGE}:443")]);
        assert_eq!(
            evaluate(&p, "api.example.test", 443, &r),
            Err(EgressRefusal::NameRejected {
                target: "api.example.test".to_string(),
                port: 443,
            })
        );
        assert_eq!(r.lookups(), 0, "a name REJECT refuses before DNS");
    }

    /// **E3** - control for E2, differing only by the REJECT row. Without it E2
    /// is green against an evaluator that refuses everything.
    #[test]
    fn egress_accept_alone_admits() {
        let p = policy(vec![rule(Verdict::Accept, "api.example.test", 443)]);
        let r = RecordingResolver::new(&[&format!("{IN_RANGE}:443")]);
        assert!(evaluate(&p, "api.example.test", 443, &r).is_ok());
    }

    /// **E4 - INVARIANT GRANTS-NARROW.** A granted name resolving into private
    /// space is refused with the FLOOR's error, not the grant's. Asserting the
    /// error identity is the point: a refusal for the wrong reason passes a
    /// weaker assertion.
    #[test]
    fn ssrf_floor_beats_a_granted_name() {
        let p = policy(vec![rule(Verdict::Accept, "internal.example.test", 443)]);
        let r = RecordingResolver::new(&["10.0.0.5:443"]);
        assert_eq!(
            evaluate(&p, "internal.example.test", 443, &r),
            Err(EgressRefusal::NoAddressSurvived {
                floor: vec!["10.0.0.5".parse().unwrap()],
                range_rejected: vec![],
                unmatched: vec![],
            }),
            "the floor must refuse, and must be the arm that reports it"
        );

        // The control, differing in ONE thing - what the name resolved to.
        // Without it this row is green against a floor that refuses every
        // address, which would refuse the grant rather than narrow it.
        let public = RecordingResolver::new(&[&format!("{IN_RANGE}:443")]);
        assert!(
            evaluate(&p, "internal.example.test", 443, &public).is_ok(),
            "the same grant must still admit an address the floor permits"
        );
    }

    /// **E5** - control for E4: the same invariant reached through the OTHER
    /// constructor. A floor applied only on the name path would pass E4 and fail
    /// here.
    #[test]
    fn ssrf_floor_beats_a_granted_range() {
        // 10.0.0.0/16 is inside RFC1918, and a creator may write it: the floor
        // is what refuses it, not validation.
        let p = policy(vec![rule(Verdict::Accept, "10.0.0.0/16", 443)]);
        let r = RecordingResolver::new(&["10.0.0.5:443"]);
        assert_eq!(
            evaluate(&p, "10.0.0.5", 443, &r),
            Err(EgressRefusal::NoAddressSurvived {
                floor: vec!["10.0.0.5".parse().unwrap()],
                range_rejected: vec![],
                unmatched: vec![],
            })
        );
        assert_eq!(r.lookups(), 0, "an IP literal is never resolved");

        // The control: the same SHAPE - a `Range` ACCEPT and a literal inside
        // it - with the range moved out of the space the floor refuses. Both
        // the range and the literal move because they are one destination
        // written twice; nothing else about the row changes. Without it the
        // assertion above is green against a range arm that admits nothing.
        let permitted = policy(vec![rule(Verdict::Accept, PUBLIC_RANGE, 443)]);
        let never = RecordingResolver::new(&[]);
        assert!(
            evaluate(&permitted, IN_RANGE, 443, &never).is_ok(),
            "a Range ACCEPT must admit a literal the floor permits"
        );
        assert_eq!(never.lookups(), 0, "an IP literal is never resolved");
    }

    /// **E6 - the DNS gate.** A names-only app refuses an ungranted host WITHOUT
    /// resolving it. Observed by the recording resolver rather than by timing:
    /// a timing assertion cannot tell a gate that held from a fast lookup.
    #[test]
    fn names_only_app_refuses_without_resolving() {
        let p = policy(vec![rule(Verdict::Accept, "api.example.test", 443)]);
        let r = RecordingResolver::new(&[&format!("{IN_RANGE}:443")]);
        let got = evaluate(&p, "secret-label.evil.test", 443, &r);
        // The lookup count is asserted FIRST because it is the property. The
        // refusal below is the consequence; if only that were checked, an
        // evaluator that resolved and then refused would still read as green.
        assert_eq!(
            r.lookups(),
            0,
            "a names-only app performed a lookup: the attacker-chosen label \
             reached a nameserver"
        );
        assert_eq!(
            got,
            Err(EgressRefusal::NoRuleCouldAdmit {
                target: "secret-label.evil.test".to_string(),
                port: 443,
            })
        );
    }

    /// **E7** - control for E6, differing in exactly one rule. Without it E6 is
    /// green against an implementation that never resolves anything, which would
    /// be a different bug.
    #[test]
    fn range_holding_app_does_resolve() {
        let p = policy(vec![
            rule(Verdict::Accept, "api.example.test", 443),
            rule(Verdict::Accept, PUBLIC_RANGE, 443),
        ]);
        let r = RecordingResolver::new(&[&format!("{OUT_OF_RANGE}:443")]);
        let got = evaluate(&p, "secret-label.evil.test", 443, &r);
        assert!(matches!(
            got,
            Err(EgressRefusal::NoAddressSurvived { .. })
        ));
        assert_eq!(
            r.lookups(),
            1,
            "holding a Range ACCEPT at this port opens the gate - the residual"
        );
    }

    /// **E8** - the port-matched refinement. Without this row the gate could be
    /// implemented as "has any range rule" and still pass E6 and E7.
    #[test]
    fn range_rule_at_another_port_does_not_open_the_gate() {
        let p = policy(vec![rule(Verdict::Accept, PUBLIC_RANGE, 443)]);
        let r = RecordingResolver::new(&[&format!("{IN_RANGE}:25")]);
        let got = evaluate(&p, "secret-label.evil.test", 25, &r);
        assert_eq!(
            r.lookups(),
            0,
            "a range rule at another port opened the gate"
        );
        assert_eq!(
            got,
            Err(EgressRefusal::NoRuleCouldAdmit {
                target: "secret-label.evil.test".to_string(),
                port: 25,
            })
        );
    }

    /// **E9** - step 1 is terminal and precedes the gate. The only row that
    /// fails if the name-REJECT check moves into phase 3.
    #[test]
    fn name_reject_refuses_before_dns() {
        let p = policy(vec![
            rule(Verdict::Reject, "badhost.example.test", 443),
            rule(Verdict::Accept, PUBLIC_RANGE, 443),
        ]);
        let r = RecordingResolver::new(&[&format!("{IN_RANGE}:443")]);
        assert_eq!(
            evaluate(&p, "badhost.example.test", 443, &r),
            Err(EgressRefusal::NameRejected {
                target: "badhost.example.test".to_string(),
                port: 443,
            })
        );
        assert_eq!(
            r.lookups(),
            0,
            "a creator who knows a bad destination must be able to block it \
             without ever querying for it"
        );

        // The control, differing in ONE thing - the name asked for. The same
        // rule set still resolves and admits anything the REJECT does not name,
        // so the row above is about step 1 and not about a policy that refuses
        // everything before DNS.
        let other = RecordingResolver::new(&[&format!("{IN_RANGE}:443")]);
        assert!(evaluate(&p, "other.example.test", 443, &other).is_ok());
        assert_eq!(other.lookups(), 1);
    }

    /// **E10** - the set-filter change to the resolver path. Fails against
    /// first-address semantics, where the leading non-blocked address is
    /// returned and the policy never sees the rest.
    #[test]
    fn range_grant_survives_a_non_first_address() {
        let p = policy(vec![rule(Verdict::Accept, PUBLIC_RANGE, 443)]);
        let r = RecordingResolver::new(&[
            &format!("{OUT_OF_RANGE}:443"),
            &format!("{IN_RANGE}:443"),
        ]);
        let got = evaluate(&p, "dual.example.test", 443, &r).expect("later address admits");
        assert_eq!(
            got,
            vec![format!("{IN_RANGE}:443").parse::<SocketAddr>().unwrap()],
            "the whole answer is filtered, THEN the first survivor is taken"
        );
    }

    /// 5.7: a v4-only Range ACCEPT drops every AAAA answer at step 6, and the
    /// error must not read like a broken name.
    #[test]
    fn refusal_names_which_step_emptied_the_answer_set() {
        let p = policy(vec![rule(Verdict::Accept, PUBLIC_RANGE, 443)]);
        let r = RecordingResolver::new(&["[2606:4700::1111]:443", "10.0.0.5:443"]);
        let err = evaluate(&p, "dual.example.test", 443, &r).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("2606:4700::1111"), "{msg}");
        assert!(msg.contains("matched no ACCEPT rule"), "{msg}");
        assert!(msg.contains("SSRF floor"), "{msg}");
        assert!(msg.contains("10.0.0.5"), "{msg}");
    }

    /// A `Name` ACCEPT does not exempt an address from a `Range` REJECT: step 5
    /// runs before step 6 and is not conditioned on `name_accepted`.
    #[test]
    fn a_name_accept_does_not_exempt_an_address_from_a_range_reject() {
        let p = policy(vec![
            rule(Verdict::Accept, "api.example.test", 443),
            rule(Verdict::Reject, PUBLIC_RANGE, 443),
        ]);
        let r = RecordingResolver::new(&[&format!("{IN_RANGE}:443")]);
        assert_eq!(
            evaluate(&p, "api.example.test", 443, &r),
            Err(EgressRefusal::NoAddressSurvived {
                floor: vec![],
                range_rejected: vec![IN_RANGE.parse().unwrap()],
                unmatched: vec![],
            })
        );

        // The control, differing in ONE thing - the address the name resolved
        // to. Outside the rejected range the name acceptance still admits, so
        // the row above is about step 5 winning and not about the name grant
        // being ignored.
        let outside = RecordingResolver::new(&[&format!("{OUT_OF_RANGE}:443")]);
        assert!(evaluate(&p, "api.example.test", 443, &outside).is_ok());
    }

    /// The survivors are ALL of them, in the resolver's own order. Nothing else
    /// pins this: every other row has at most one survivor, so truncating to
    /// the first - or sorting - would leave the suite green while the
    /// happy-eyeballs claim in `filter_answer`'s doc became false.
    #[test]
    fn filter_answer_keeps_every_survivor_in_resolver_order() {
        let p = policy(vec![rule(Verdict::Accept, PUBLIC_RANGE, 443)]);
        let r = RecordingResolver::new(&[
            &format!("{IN_RANGE}:443"),
            &format!("{OUT_OF_RANGE}:443"),
            &format!("{IN_RANGE_CARVED}:443"),
        ]);
        let kept = evaluate(&p, "many.example.test", 443, &r).expect("two addresses survive");
        assert_eq!(
            kept,
            vec![
                format!("{IN_RANGE}:443").parse::<SocketAddr>().unwrap(),
                format!("{IN_RANGE_CARVED}:443").parse::<SocketAddr>().unwrap(),
            ],
            "every survivor must be returned, in the order the resolver gave"
        );
    }

    /// The three-way split 5.7 requires, pinned at the ONE function both
    /// transports classify through.
    ///
    /// `node:net` and WebSocket render it differently - a `code` field on one,
    /// a close reason on the other - but neither decides it. Without this row
    /// the split is only asserted end-to-end on the `node:net` path, and
    /// collapsing two of the three arms here would leave WebSocket silently
    /// reporting a creator refusal as a platform one.
    #[test]
    fn refusal_report_distinguishes_the_floor_from_the_creators_rules() {
        let floor = EgressRefusal::NoAddressSurvived {
            floor: vec!["10.0.0.5".parse().unwrap()],
            range_rejected: vec![],
            unmatched: vec![],
        };
        assert_eq!(refusal_report(&floor).0, "ERR_NET_SSRF");

        // The control, differing in ONE thing - which list the address landed
        // in. The creator wrote this one, so it must not read as the platform's.
        let by_creator = EgressRefusal::NoAddressSurvived {
            floor: vec![],
            range_rejected: vec!["93.184.216.7".parse().unwrap()],
            unmatched: vec![],
        };
        assert_eq!(refusal_report(&by_creator).0, "ERR_NET_EGRESS_DENIED");

        // And "nothing ACCEPTed it", the arm a v4-only range grant produces for
        // every AAAA answer, is the creator's too - not a broken name.
        let unmatched = EgressRefusal::NoAddressSurvived {
            floor: vec![],
            range_rejected: vec![],
            unmatched: vec!["2606:4700::1111".parse().unwrap()],
        };
        assert_eq!(refusal_report(&unmatched).0, "ERR_NET_EGRESS_DENIED");

        // A lookup that simply failed is not a policy outcome at all, and
        // carries the resolver's own code through unchanged.
        let broken = EgressRefusal::ResolveFailed(ResolveFailure {
            code: "ERR_NET_DNS_TIMEOUT",
            message: "DNS resolve timed out".to_string(),
        });
        assert_eq!(
            refusal_report(&broken),
            ("ERR_NET_DNS_TIMEOUT", "DNS resolve timed out".to_string())
        );
    }

    #[test]
    fn a_denied_policy_refuses_the_module_outright() {
        let never = RecordingResolver::new(&[&format!("{IN_RANGE}:443")]);
        assert_eq!(
            evaluate(&NetPolicy::Denied, "api.example.test", 443, &never),
            Err(EgressRefusal::ModuleDenied)
        );
        assert_eq!(never.lookups(), 0);

        // The control, differing in ONE thing - the policy. Without it this row
        // is green against an evaluator that returns ModuleDenied for every
        // policy there is.
        let p = policy(vec![rule(Verdict::Accept, "api.example.test", 443)]);
        let r = RecordingResolver::new(&[&format!("{IN_RANGE}:443")]);
        assert!(evaluate(&p, "api.example.test", 443, &r).is_ok());
    }

    /// Trusted holds no creator rules, so the floor is the whole policy - and it
    /// still applies.
    #[test]
    fn trusted_keeps_the_floor() {
        let p = NetPolicy::trusted(4, 1024);
        let ok = RecordingResolver::new(&[&format!("{IN_RANGE}:443")]);
        assert!(evaluate(&p, "anything.example.test", 443, &ok).is_ok());
        let blocked = RecordingResolver::new(&["127.0.0.1:443"]);
        assert!(matches!(
            evaluate(&p, "anything.example.test", 443, &blocked),
            Err(EgressRefusal::NoAddressSurvived { .. })
        ));
    }
}
