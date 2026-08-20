//! The `node:net` egress rule set: the types, their authoring-boundary
//! validation, and the two PHASE-SCOPED queries the evaluator composes.
//!
//! Runtime owns enforcement, but the control plane needs the exact same
//! grammar and validation at the AUTHORING boundary - which is a creator-facing
//! API (`/api/apps/{id}/egress-rules`), not an operator's console. Keep this
//! module V8-free so control can reject bad rules without depending on the
//! runtime crate.
//!
//! Because the author is the creator, these checks are a SHAPE bound on creator
//! input, not a guardrail on an operator's typing. A wrong rule breaks the
//! creator's own app: an operational failure, not a security bypass. What keeps
//! the resulting reach narrow is elsewhere - deny-by-default per app, the plan's
//! `max_grants`/`max_sockets`/`egress_ceiling_bytes` caps, and above all the
//! platform's SSRF floor, which no rule in this module can widen
//! (INVARIANT GRANTS-NARROW, enforced in `zeroship_runtime::transport::egress`).
//!
//! **This control covers every RAW BYTE STREAM an app can open: `node:net`,
//! `node:tls`, and outbound `WebSocket`.** One rule set covers all of them,
//! because they are one capability: a WebSocket is a bidirectional byte stream
//! the moment the upgrade completes, and a creator who granted
//! `api.example.test:443` means that destination, not that transport. Two
//! grammars would be two things to keep in step.
//!
//! **`fetch` is the exception and the only one.** It is not gated and reaches
//! any public host with no rule at all, so this is a raw-stream blast-radius
//! control against a compromised dependency, not an egress control in general
//! and not a tenant-isolation control.
//!
//! # Why the queries are split by phase
//!
//! A rule's destination is either a DNS name or an address range, and the two
//! are decidable at DIFFERENT TIMES: a `Name` before any lookup, a `Range` only
//! after one. That is not a note about the implementation, it is the reason
//! three properties hold at all:
//!
//! 1. the evaluator is two-phase around a single resolution,
//! 2. the rule set is UNORDERED, because an ordered walk would have to resolve
//!    before rule 1 to decide rule 2, and
//! 3. an app declaring only names never resolves a host it is going to refuse,
//!    so an attacker-chosen label never reaches an attacker-chosen nameserver.
//!
//! [`Destination`] therefore has no `matches(host, port)`. It has
//! [`Destination::matches_name`] and [`Destination::matches_addr`], each of
//! which can answer [`PhaseMatch::Undecidable`] - the case a single-pass matcher
//! would have to silently fold into "no match", deleting all three properties.

use std::net::IpAddr;

use ipnet::IpNet;
use serde::{Deserialize, Serialize};

/// Shortest IPv4 prefix accepted on an ACCEPT rule.
///
/// A proxy for single-tenancy over ten measured destinations, NOT a tenancy
/// bound: a single vendor holding a `/12` is refused and a reseller of a `/16`
/// is admitted. Its guaranteed job is smaller and is the reason it exists -
/// it stops one ACCEPT rule being `0.0.0.0/0`, which would turn the control off
/// with a single row.
pub const MIN_ACCEPT_PREFIX_V4: u8 = 16;

/// Shortest IPv6 prefix accepted on an ACCEPT rule.
///
/// `/32` is the organisation-level v6 allocation (`/48` is a SITE, which is why
/// it is the wrong pair for v4's `/24`). A `/32` is 2^96 addresses, so on v6
/// this floor bounds nothing by count at all; it is the same tenancy proxy as
/// [`MIN_ACCEPT_PREFIX_V4`] and nothing more.
pub const MIN_ACCEPT_PREFIX_V6: u8 = 32;

/// What a rule does when it matches.
///
/// REJECT beats ACCEPT (see [`EgressRules`]), so the two are not symmetric and
/// the enum order carries no meaning.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Verdict {
    Accept,
    Reject,
}

impl Verdict {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Accept => "accept",
            Self::Reject => "reject",
        }
    }
}

/// The answer to "does this destination match?" IN ONE PHASE.
///
/// Three-valued on purpose. [`Self::Undecidable`] is not a weaker
/// [`Self::DoesNotMatch`]: it says the question cannot be asked yet, and
/// treating the two alike is exactly the single-pass collapse this module's
/// header describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PhaseMatch {
    /// Decidable in this phase, and it matched.
    Matches,
    /// Decidable in this phase, and it did not match.
    DoesNotMatch,
    /// NOT decidable in this phase. Neither an admission nor a refusal.
    Undecidable,
}

/// What a rule is about: a name (pre-DNS) or a range (post-DNS).
///
/// Deliberately NOT a plain sum type with one `matches`. See the module header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Destination {
    /// Exact DNS name. No wildcards. Matched case-insensitively against the
    /// name the app asked for, after trailing-dot and bracket normalization.
    ///
    /// **Decidable BEFORE DNS.**
    Name(String),
    /// Address range, canonical (host bits cleared). Matched against the
    /// RESOLVED address, so the name the app asked for is irrelevant and DNS
    /// rebinding cannot satisfy it.
    ///
    /// **Decidable ONLY AFTER DNS.**
    Range(IpNet),
}

impl Destination {
    /// Parse a destination from the single creator-facing grammar.
    ///
    /// A value containing `/` must parse as a CIDR; anything else must parse as
    /// a DNS name. Ambiguity is rejected rather than guessed: a bare IP literal
    /// is refused with the advice to write it as a `/32` or `/128`, so the
    /// reader of a rule always knows which phase will decide it.
    pub fn parse(raw: &str) -> Result<Self, String> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err("egress destination must not be empty".to_string());
        }
        if trimmed.contains('/') {
            let net: IpNet = trimmed
                .parse()
                .map_err(|e| format!("egress destination {trimmed:?} is not a valid CIDR: {e}"))?;
            // Canonicalise: `10.0.0.1/24` and `10.0.0.0/24` are one range, and
            // on a TEXT primary key they would otherwise be two rows with two
            // (possibly opposing) verdicts.
            return Ok(Self::Range(net.trunc()));
        }
        Ok(Self::Name(validate_name(trimmed)?))
    }

    /// Canonical text form. Round-trips through [`Destination::parse`].
    #[must_use]
    pub fn to_text(&self) -> String {
        match self {
            Self::Name(name) => name.clone(),
            Self::Range(net) => net.to_string(),
        }
    }

    /// PHASE 1 query. Never resolves and never asks to.
    ///
    /// Returns [`PhaseMatch::Undecidable`] for a [`Destination::Range`] - which
    /// is the whole point: a range cannot be tested against a name, and folding
    /// that into "does not match" is what deletes the DNS gate.
    #[must_use]
    pub fn matches_name(&self, name: &str) -> PhaseMatch {
        match self {
            Self::Name(rule_name) => {
                if *rule_name == normalize_name(name) {
                    PhaseMatch::Matches
                } else {
                    PhaseMatch::DoesNotMatch
                }
            }
            Self::Range(_) => PhaseMatch::Undecidable,
        }
    }

    /// PHASE 3 query, against one member of the resolved answer set.
    ///
    /// Returns [`PhaseMatch::Undecidable`] for a [`Destination::Name`]: whether
    /// the name matched was settled in phase 1 and must be carried forward, not
    /// re-derived from the address. Re-deriving it is a reverse lookup, which is
    /// spoofable by whoever owns the address.
    #[must_use]
    pub fn matches_addr(&self, addr: IpAddr) -> PhaseMatch {
        match self {
            Self::Name(_) => PhaseMatch::Undecidable,
            Self::Range(net) => {
                if net.contains(&addr) {
                    PhaseMatch::Matches
                } else {
                    PhaseMatch::DoesNotMatch
                }
            }
        }
    }
}

/// One creator-authored egress rule: a verdict, a destination, a port.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EgressRule {
    verdict: Verdict,
    destination: Destination,
    port: u16,
}

impl EgressRule {
    /// Build and validate a rule from the creator-facing wire shape.
    ///
    /// This is the ONE authoring boundary. Control calls it to refuse a bad
    /// rule at the API, and the worker calls it again on every row it loads, so
    /// a hand-edited database row cannot inject a rule the API would refuse.
    pub fn parse(verdict: Verdict, destination: &str, port: u16) -> Result<Self, String> {
        if port == 0 {
            return Err("egress rule port must be between 1 and 65535".to_string());
        }
        let destination = Destination::parse(destination)?;
        // The prefix floor is an ACCEPT-only rule. `REJECT 0.0.0.0/0` is a
        // coherent and safe thing to write, and refusing it would be refusing
        // the strictest rule in the grammar.
        if let (Verdict::Accept, Destination::Range(net)) = (verdict, &destination) {
            let (floor, family) = match net {
                IpNet::V4(_) => (MIN_ACCEPT_PREFIX_V4, "IPv4"),
                IpNet::V6(_) => (MIN_ACCEPT_PREFIX_V6, "IPv6"),
            };
            if net.prefix_len() < floor {
                return Err(format!(
                    "accept rule range {net} is broader than the {family} floor /{floor}; \
                     write a narrower range or name the host exactly"
                ));
            }
        }
        Ok(Self {
            verdict,
            destination,
            port,
        })
    }

    #[must_use]
    pub const fn verdict(&self) -> Verdict {
        self.verdict
    }

    #[must_use]
    pub const fn destination(&self) -> &Destination {
        &self.destination
    }

    #[must_use]
    pub const fn port(&self) -> u16 {
        self.port
    }
}

/// What phase 1 decided, and - when it could not decide - what phase 3 needs.
///
/// The two refusal arms are the ONLY refusals reached without a lookup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NamePhase {
    /// Step 1. A `Name` REJECT matched. Terminal, and terminal BEFORE the gate,
    /// so a creator who knows a bad destination blocks it without ever querying
    /// for it.
    NameRejected,
    /// Step 3, the DNS gate. No `Name` ACCEPT matched and the app holds no
    /// `Range` ACCEPT at this port, so no resolution could change the answer.
    ///
    /// This is the only place in the whole evaluator where a refusal is decided
    /// by ABSENCE, and it is why a names-only app never resolves a host it will
    /// refuse.
    NoRuleCouldAdmit,
    /// Undecided before DNS. Resolve exactly once, then run the address phase on
    /// EVERY member of the answer set, carrying `name_accepted` forward.
    Resolve { name_accepted: bool },
}

/// What the CREATOR's rules say about one resolved address.
///
/// The platform floor is not in this enum and is not this module's to apply:
/// it runs first, in the runtime, and no value here can override it
/// (INVARIANT GRANTS-NARROW).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddressPhase {
    /// Step 5. A `Range` REJECT matched. Note this is NOT conditioned on
    /// `name_accepted`: "allow `api.example.com`, but never anything in
    /// `203.0.113.0/24`" holds even when the name resolves there.
    RangeRejected,
    /// Step 6. Neither the name phase nor any `Range` ACCEPT admitted it.
    NoAcceptMatched,
    /// Step 6, admitted.
    Admitted,
}

/// A validated, UNORDERED egress rule set.
///
/// Unordered is a property, not an accident. The verdict is a pure function of
/// the SET: `platform floor > creator REJECT > creator ACCEPT > default(refuse)`,
/// total and position-free. Nothing here stores or consults a position, and
/// nothing may start to - an ordered walk must evaluate rule *k* before rule
/// *k+1*, and a `Range` rule is undecidable without a resolved address, so an
/// ordered evaluator has to resolve before the walk and the DNS gate ceases to
/// exist.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct EgressRules {
    rules: Vec<EgressRule>,
}

impl EgressRules {
    /// Build a rule set, re-validating every rule.
    ///
    /// Not a JS/user-code surface: it is reached only from trusted Rust holding
    /// control-plane rows the isolate cannot touch.
    pub fn validated(rules: Vec<EgressRule>) -> Result<Self, String> {
        for rule in &rules {
            // Re-run the authoring check so a rule assembled field-by-field
            // cannot skip the floor that `EgressRule::parse` applies.
            EgressRule::parse(rule.verdict, &rule.destination.to_text(), rule.port)?;
        }
        Ok(Self { rules })
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    #[must_use]
    pub fn as_slice(&self) -> &[EgressRule] {
        &self.rules
    }

    pub fn iter(&self) -> impl Iterator<Item = &EgressRule> {
        self.rules.iter()
    }

    /// PHASE 1 - steps 1 to 3. **Performs no lookup and requests none.**
    ///
    /// `target` is the NAME the app asked for. Callers must skip this phase
    /// entirely for an IP literal: phase 1 has nothing to say about one, and
    /// only a `Range` ACCEPT can admit it.
    #[must_use]
    pub fn name_phase(&self, target: &str, port: u16) -> NamePhase {
        let target = normalize_name(target);

        // Step 1 - a Name REJECT is terminal, and terminal ahead of the gate.
        let mut name_accepted = false;
        for rule in &self.rules {
            if rule.port != port {
                continue;
            }
            match rule.destination.matches_name(&target) {
                PhaseMatch::Matches => match rule.verdict {
                    Verdict::Reject => return NamePhase::NameRejected,
                    // Step 2 - record, do not return: a later REJECT in this
                    // same unordered set still wins, which is what makes the
                    // answer independent of the order the rules arrived in.
                    Verdict::Accept => name_accepted = true,
                },
                PhaseMatch::DoesNotMatch | PhaseMatch::Undecidable => {}
            }
        }

        if name_accepted {
            return NamePhase::Resolve {
                name_accepted: true,
            };
        }

        // Step 3 - the gate. ACCEPT-only and port-matched: a `Range` REJECT can
        // never turn a refusal into an admission, and a `Range` ACCEPT at 443
        // cannot admit a connect to 25, so resolving to test either is pure leak
        // for zero benefit.
        if self.holds_range_accept_at(port) {
            NamePhase::Resolve {
                name_accepted: false,
            }
        } else {
            NamePhase::NoRuleCouldAdmit
        }
    }

    /// Whether any `Range` ACCEPT rule exists at `port`. The DNS gate's input,
    /// and the reason the leaking class is fixed by the creator offline rather
    /// than reachable by code inside the isolate.
    #[must_use]
    pub fn holds_range_accept_at(&self, port: u16) -> bool {
        self.rules.iter().any(|rule| {
            rule.port == port
                && rule.verdict == Verdict::Accept
                && matches!(rule.destination, Destination::Range(_))
        })
    }

    /// PHASE 3 - steps 5 and 6, for ONE address of the answer set.
    ///
    /// Step 4 (the platform floor) is deliberately absent: it belongs to the
    /// platform, runs before this, and is not expressible in creator rules.
    #[must_use]
    pub fn address_phase(&self, addr: IpAddr, port: u16, name_accepted: bool) -> AddressPhase {
        let mut range_accepted = false;
        for rule in &self.rules {
            if rule.port != port {
                continue;
            }
            if rule.destination.matches_addr(addr) != PhaseMatch::Matches {
                continue;
            }
            match rule.verdict {
                // Step 5 - REJECT wins over everything a creator can write, and
                // is NOT conditioned on `name_accepted`.
                Verdict::Reject => return AddressPhase::RangeRejected,
                Verdict::Accept => range_accepted = true,
            }
        }
        // Step 6.
        if name_accepted || range_accepted {
            AddressPhase::Admitted
        } else {
            AddressPhase::NoAcceptMatched
        }
    }

    /// Count of ACCEPT rules, which is what a plan's `max_grants` bounds.
    /// REJECT rules are bounded separately and for an unrelated reason: an
    /// unbounded reject list is a denial of service against the registry
    /// projection, never a security concern, because a REJECT can only narrow.
    #[must_use]
    pub fn accept_count(&self) -> usize {
        self.rules
            .iter()
            .filter(|r| r.verdict == Verdict::Accept)
            .count()
    }
}

/// Normalize a DNS name for comparison and storage: trim, strip IPv6-literal
/// brackets, strip the trailing root dot, lowercase.
///
/// Applied at the authoring boundary AND at match time. Without it at the
/// boundary, `API.STRIPE.COM` and `api.stripe.com` are two rows on a TEXT
/// primary key, consuming two `max_grants` slots for one destination and
/// letting one destination carry two opposing verdicts.
#[must_use]
pub fn normalize_name(host: &str) -> String {
    host.trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .trim_end_matches('.')
        .to_ascii_lowercase()
}

/// Validate a DNS-name destination. Shape only - a name the creator controls is
/// the creator's business, and what bounds where it lands is the SSRF floor.
fn validate_name(raw: &str) -> Result<String, String> {
    let name = normalize_name(raw);
    if name.is_empty() {
        return Err("egress destination name must not be empty".to_string());
    }
    if name.contains('*') {
        return Err(format!(
            "egress destination {name:?} must not contain '*'; wildcards are not representable, \
             name each host exactly or write an address range"
        ));
    }
    if name.parse::<IpAddr>().is_ok() {
        return Err(format!(
            "egress destination {name:?} is an IP literal; write it as a range \
             ({name}/32 or {name}/128) so it is clear which check decides it"
        ));
    }
    if !name.contains('.') {
        return Err(format!(
            "egress destination name {name:?} must contain at least two labels"
        ));
    }
    Ok(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn accept(dest: &str, port: u16) -> EgressRule {
        EgressRule::parse(Verdict::Accept, dest, port).expect("valid accept rule")
    }

    fn reject(dest: &str, port: u16) -> EgressRule {
        EgressRule::parse(Verdict::Reject, dest, port).expect("valid reject rule")
    }

    #[test]
    fn name_destinations_are_exact_and_case_and_dot_insensitive() {
        let rule = accept("DB.Example.COM.", 5432);
        assert_eq!(
            rule.destination().matches_name("db.example.com"),
            PhaseMatch::Matches
        );
        assert_eq!(
            rule.destination().matches_name("DB.EXAMPLE.COM."),
            PhaseMatch::Matches
        );
        // Exact means exact: no wildcard, no suffix, no parent.
        assert_eq!(
            rule.destination().matches_name("a.db.example.com"),
            PhaseMatch::DoesNotMatch
        );
    }

    /// The three-valued answer is the type-level guard on the phase split. If
    /// either of these ever returns `DoesNotMatch`, a single-pass matcher
    /// becomes writable and the DNS gate stops being enforceable.
    #[test]
    fn each_destination_kind_is_undecidable_in_the_other_phase() {
        let name = accept("api.example.test", 443);
        assert_eq!(
            name.destination().matches_addr("93.184.216.34".parse().unwrap()),
            PhaseMatch::Undecidable
        );
        let range = accept("93.184.216.0/24", 443);
        assert_eq!(
            range.destination().matches_name("api.example.test"),
            PhaseMatch::Undecidable
        );
    }

    #[test]
    fn wildcards_and_ip_literal_names_are_not_representable() {
        assert!(EgressRule::parse(Verdict::Accept, "*", 443).is_err());
        assert!(EgressRule::parse(Verdict::Accept, "*.example.com", 443).is_err());
        assert!(EgressRule::parse(Verdict::Accept, "*.workers.dev", 443).is_err());
        // An IP literal must be written as a range so the reader knows which
        // phase decides it.
        assert!(EgressRule::parse(Verdict::Accept, "127.0.0.1", 443).is_err());
        assert!(EgressRule::parse(Verdict::Accept, "single", 443).is_err());
        // An exact host under any suffix is fine: taste is not enforced here.
        assert!(EgressRule::parse(Verdict::Accept, "abc.workers.dev", 443).is_ok());
        assert!(EgressRule::parse(Verdict::Accept, "db.neon.tech", 5432).is_ok());
    }

    #[test]
    fn accept_ranges_are_bounded_by_the_prefix_floor_and_rejects_are_not() {
        assert!(EgressRule::parse(Verdict::Accept, "0.0.0.0/0", 443).is_err());
        assert!(EgressRule::parse(Verdict::Accept, "34.0.0.0/10", 443).is_err());
        assert!(EgressRule::parse(Verdict::Accept, "34.192.0.0/16", 443).is_ok());
        assert!(EgressRule::parse(Verdict::Accept, "2606:4700::/28", 443).is_err());
        assert!(EgressRule::parse(Verdict::Accept, "2606:4700::/32", 443).is_ok());
        // The strictest rule in the grammar must stay writable.
        assert!(EgressRule::parse(Verdict::Reject, "0.0.0.0/0", 443).is_ok());
        assert!(EgressRule::parse(Verdict::Reject, "::/0", 443).is_ok());
    }

    #[test]
    fn ranges_are_canonicalised_so_one_range_is_one_key() {
        let with_host_bits = Destination::parse("93.184.216.7/24").unwrap();
        let canonical = Destination::parse("93.184.216.0/24").unwrap();
        assert_eq!(with_host_bits, canonical);
        assert_eq!(with_host_bits.to_text(), "93.184.216.0/24");
    }

    #[test]
    fn names_are_normalised_so_one_destination_is_one_key() {
        assert_eq!(
            Destination::parse("  API.Stripe.COM.  ").unwrap().to_text(),
            "api.stripe.com"
        );
    }

    #[test]
    fn port_zero_is_refused() {
        assert!(EgressRule::parse(Verdict::Accept, "api.example.test", 0).is_err());
        // The control, differing only in the port: 1 is the first legal one, so
        // the row above is about zero and not about the destination.
        assert!(EgressRule::parse(Verdict::Accept, "api.example.test", 1).is_ok());
    }

    /// The DNS gate is ACCEPT-only. A `Range` REJECT can never turn a refusal
    /// into an admission, so resolving to test one is pure leak for zero
    /// benefit - which is what `holds_range_accept_at` asserts in prose.
    ///
    /// The pair below differs in the VERDICT and nothing else. Without it,
    /// dropping the verdict check leaves this crate, the runtime and the worker
    /// green, and a creator adding the most natural hardening rule there is -
    /// a defensive reject - would silently move their names-only app into the
    /// leaking class, where every refused connect carries an attacker-chosen
    /// label to an attacker-chosen nameserver.
    #[test]
    fn only_a_range_accept_opens_the_dns_gate() {
        let hardened = EgressRules::validated(vec![
            accept("api.example.test", 443),
            reject("93.184.216.0/24", 443),
        ])
        .unwrap();
        assert!(!hardened.holds_range_accept_at(443));
        assert_eq!(
            hardened.name_phase("evil.example.test", 443),
            NamePhase::NoRuleCouldAdmit,
            "a range REJECT opened the gate: an ungranted name was resolved"
        );

        // The control: the SAME range at the SAME port, as an ACCEPT.
        let opened = EgressRules::validated(vec![
            accept("api.example.test", 443),
            accept("93.184.216.0/24", 443),
        ])
        .unwrap();
        assert!(opened.holds_range_accept_at(443));
        assert_eq!(
            opened.name_phase("evil.example.test", 443),
            NamePhase::Resolve {
                name_accepted: false
            }
        );

        // And the rule a creator hardening their app would actually write.
        // It has no accept floor, so it is the broadest reject expressible.
        let blanket = EgressRules::validated(vec![
            accept("api.example.test", 443),
            reject("0.0.0.0/0", 443),
        ])
        .unwrap();
        assert_eq!(
            blanket.name_phase("evil.example.test", 443),
            NamePhase::NoRuleCouldAdmit
        );
    }

    /// Port matching is an equality, not a bound, and the existing rows only
    /// ever place the RULE above the CONNECT - so they catch a rule leaking
    /// upward and miss one leaking downward. This is the other direction: a
    /// rule authored for SMTP must not admit HTTPS, or every port above the
    /// one the creator wrote comes with it.
    #[test]
    fn a_rule_at_one_port_decides_nothing_at_a_higher_one() {
        let rules = EgressRules::validated(vec![accept("93.184.216.0/24", 25)]).unwrap();
        let inside: IpAddr = "93.184.216.34".parse().unwrap();

        assert!(rules.holds_range_accept_at(25));
        assert!(
            !rules.holds_range_accept_at(443),
            "a rule at port 25 opened the gate at 443"
        );
        assert_eq!(
            rules.name_phase("evil.example.test", 443),
            NamePhase::NoRuleCouldAdmit
        );
        assert_eq!(
            rules.address_phase(inside, 443, false),
            AddressPhase::NoAcceptMatched,
            "a rule at port 25 admitted an address at 443"
        );

        // The control, differing only in the connect port.
        assert_eq!(
            rules.address_phase(inside, 25, false),
            AddressPhase::Admitted
        );
    }

    /// A range decides addresses of ITS OWN family only. Nothing else asserts
    /// this directly: the one row that would catch a cross-family match does so
    /// incidentally, because its answer set happens to contain a v6 address.
    #[test]
    fn a_range_never_matches_the_other_address_family() {
        let v4 = EgressRules::validated(vec![accept("93.184.216.0/24", 443)]).unwrap();
        assert_eq!(
            v4.address_phase("2606:4700::1111".parse().unwrap(), 443, false),
            AddressPhase::NoAcceptMatched,
            "an IPv4 range admitted an IPv6 address"
        );
        // The control: same rule, same port, an address of the rule's family.
        assert_eq!(
            v4.address_phase("93.184.216.34".parse().unwrap(), 443, false),
            AddressPhase::Admitted
        );

        // And the same claim the other way round, so a matcher that ignores the
        // family in one direction only is still caught.
        let v6 = EgressRules::validated(vec![accept("2606:4700::/32", 443)]).unwrap();
        assert_eq!(
            v6.address_phase("93.184.216.34".parse().unwrap(), 443, false),
            AddressPhase::NoAcceptMatched,
            "an IPv6 range admitted an IPv4 address"
        );
        assert_eq!(
            v6.address_phase("2606:4700::1111".parse().unwrap(), 443, false),
            AddressPhase::Admitted
        );
    }

    #[test]
    fn name_phase_never_needs_an_address_and_gates_on_range_accepts() {
        let names_only =
            EgressRules::validated(vec![accept("api.example.test", 443)]).unwrap();
        // Granted name: resolve, carrying the acceptance forward.
        assert_eq!(
            names_only.name_phase("api.example.test", 443),
            NamePhase::Resolve {
                name_accepted: true
            }
        );
        // Ungranted name with no range ACCEPT anywhere: refuse WITHOUT a lookup.
        assert_eq!(
            names_only.name_phase("evil.example.test", 443),
            NamePhase::NoRuleCouldAdmit
        );

        let with_range = EgressRules::validated(vec![
            accept("api.example.test", 443),
            accept("93.184.216.0/24", 443),
        ])
        .unwrap();
        assert_eq!(
            with_range.name_phase("evil.example.test", 443),
            NamePhase::Resolve {
                name_accepted: false
            }
        );
        // ... but only at the port the range rule names.
        assert_eq!(
            with_range.name_phase("evil.example.test", 25),
            NamePhase::NoRuleCouldAdmit
        );
    }

    #[test]
    fn a_name_reject_terminates_ahead_of_the_gate() {
        let rules = EgressRules::validated(vec![
            reject("bad.example.test", 443),
            accept("93.184.216.0/24", 443),
        ])
        .unwrap();
        assert_eq!(
            rules.name_phase("bad.example.test", 443),
            NamePhase::NameRejected
        );
        // The control, differing only in the NAME asked for: the same rule set
        // still resolves anything the reject does not name. Without it this row
        // is green against a phase that rejects every name.
        assert_eq!(
            rules.name_phase("other.example.test", 443),
            NamePhase::Resolve {
                name_accepted: false
            }
        );
    }

    #[test]
    fn a_range_reject_beats_a_name_accept_on_the_same_connect() {
        let rules = EgressRules::validated(vec![
            accept("api.example.test", 443),
            reject("93.184.216.0/24", 443),
        ])
        .unwrap();
        assert_eq!(
            rules.address_phase("93.184.216.34".parse().unwrap(), 443, true),
            AddressPhase::RangeRejected
        );
        // The control, differing only in the ADDRESS: outside the rejected
        // range the name acceptance still admits, so the row above is about the
        // reject winning and not about `name_accepted` being ignored.
        assert_eq!(
            rules.address_phase("203.0.114.9".parse().unwrap(), 443, true),
            AddressPhase::Admitted
        );
    }

    #[test]
    fn accept_count_bounds_accepts_only() {
        let rules = EgressRules::validated(vec![
            accept("api.example.test", 443),
            accept("93.184.216.0/24", 443),
            reject("93.184.216.7/32", 443),
        ])
        .unwrap();
        assert_eq!(rules.accept_count(), 2);
    }

    /// `validated` must re-apply the floor. Building a rule set from rules
    /// assembled elsewhere is the path a hand-edited row takes into the worker.
    #[test]
    fn validated_reapplies_the_accept_floor() {
        let smuggled = EgressRule {
            verdict: Verdict::Accept,
            destination: Destination::Range("0.0.0.0/0".parse().unwrap()),
            port: 443,
        };
        assert!(EgressRules::validated(vec![smuggled]).is_err());

        // The control, differing only in the prefix length: a range AT the
        // floor, assembled the same field-by-field way, is kept. Without it the
        // row above is green against a `validated` that refuses everything.
        let at_the_floor = EgressRule {
            verdict: Verdict::Accept,
            destination: Destination::Range("93.184.0.0/16".parse().unwrap()),
            port: 443,
        };
        assert!(EgressRules::validated(vec![at_the_floor]).is_ok());
    }
}
