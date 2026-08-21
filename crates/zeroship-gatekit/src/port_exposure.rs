//! Every port `deploy/compose` publishes must be bound to loopback, EXCEPT the
//! edge itself.
//!
//! WHY THIS EXISTS, found 2026-08-11 while walking the operator deploy (scenario
//! 18). The compose file pins its infrastructure to loopback and its PLATFORM
//! services to all interfaces, and the asymmetry is easy to read as deliberate
//! when it is not:
//!
//! ```text
//! verdaccio  127.0.0.1:4873     postgres  127.0.0.1:5440
//! redpanda   127.0.0.1:19092    redpanda  127.0.0.1:9644
//! migrated   127.0.0.1:9091     gateway   127.0.0.1:8000
//! auth       127.0.0.1:9092     caddy     80
//! ```
//!
//! `"8000:8000"` with no address publishes on 0.0.0.0. On the internet-facing
//! host this deployment targets, that is a second entrance to the gateway that
//! does not pass through Caddy - and therefore does not pass through Cloudflare,
//! where TLS terminates. Same for auth on 9092.
//!
//! THE PART THAT MAKES IT A REAL FINDING RATHER THAN A STYLE POINT: Caddy does
//! NOT use those published ports. It reaches both by SERVICE NAME over the
//! compose network - `reverse_proxy gateway:8000` and `reverse_proxy auth:9092`
//! in `deploy/ops/Caddyfile`. So publishing them buys nothing the documented
//! path uses, and loopback-binding costs nothing: 127.0.0.1:8000 is still
//! reachable from the host, so every local harness keeps working. Only REMOTE
//! reach is removed.
//!
//! Control no longer publishes a host port. Its `control.<domain>` Caddy block
//! is active only when the compose service has no known security-relaxation
//! input. The route reaches control over the private compose network.
//!
//! WHAT THIS DOES NOT CHECK, so a green is not over-read:
//!   - it does not run docker and does not probe any host
//!   - it says NOTHING about whether a published port is actually reachable from
//!     the internet; that depends on the host firewall, and docker's iptables
//!     rules commonly bypass ufw. Nobody has checked the firewall on the target
//!     host, so read this as "published on all interfaces", not "confirmed open"
//!   - it does not check EXPOSE or container-to-container reachability, which is
//!     the compose network and is unaffected either way
//!   - the Caddyfile half is TEXT matching on a site-block header. Caddy's own
//!     parser is not run, and an import or a snippet that opened the same site
//!     some other way would be invisible.

use crate::arm_census::GateRun;
use crate::compose::ComposeFile;

/// The gate id, as it appears in every census line.
pub const GATE: &str = "compose_port_exposure";

/// The report title.
pub const TITLE: &str = "deploy/compose publishes only the edge on 0.0.0.0";

/// The ONLY service allowed to publish on all interfaces. Caddy is the edge;
/// that is its entire job. Keep this at one entry - every addition is a second
/// way into the platform that does not pass the edge.
const EDGE_SERVICE: &str = "caddy";

/// How many published-port entries the tracked compose file must yield before
/// this gate's port verdicts mean anything.
///
/// MEASURED 2026-08-20: 9 entries. Floor well under that: ordinary edits move
/// this by one or two ports, while the failure it guards against - the parse
/// losing the `ports:` key entirely - drops it to zero, not to single digits.
const PUBLISHED_PORTS_FLOOR: usize = 5;

/// The number of published-port verdicts the gate must produce.
///
/// EXACT, and DELIBERATELY CARRIED OVER FROM THE SHELL GATE as the one
/// census-shaped number here. A floor cannot discriminate the case this
/// catches: a tree that lost one port entirely still clears any floor under
/// today's count, and it is not the same finding as "the parse broke". Binding
/// it to the corpus instead (verdicts == entries parsed) would be tautological,
/// since this gate rules on every entry it parses by construction.
///
/// MEASURED 2026-08-19 and re-measured 2026-08-21: 9 published ports in
/// `deploy/compose/docker-compose.yml`. When a port is added or removed,
/// re-measure and change this line in the same commit.
const EXPECTED_PORT_VERDICTS: usize = 9;

/// How many configuration items control must carry before its posture can be
/// read.
///
/// The posture predicate below asks two questions of ONE service, and both
/// answer "not relaxed" when that service is missing from the parse - the
/// silent default that would let this gate vouch for a file it could not read.
/// So the arm counts exactly what the predicate reads: control's environment
/// entries plus its command tokens.
///
/// MEASURED 2026-08-21 by running this gate against the tracked file: 16 (13
/// environment entries, the folded `command:`, and the remaining scalar keys).
/// Floor at half of that, since an ordinary environment edit moves it by one or
/// two while the collapse it guards against takes it to zero.
const CONTROL_POSTURE_FLOOR: usize = 8;

/// Judge `compose` and the Caddyfile text.
///
/// Both are parameters rather than paths read here, so the empty and
/// malformed cases are reachable from a test instead of only by editing the
/// repository.
#[must_use]
pub fn run(compose: &ComposeFile, caddyfile: &str) -> GateRun {
    let mut run = GateRun::new(GATE);

    let entries: Vec<(&str, String)> = compose
        .services
        .iter()
        .flat_map(|(name, service)| {
            service
                .published_ports()
                .into_iter()
                .map(move |spec| (name.as_str(), spec))
        })
        .collect();

    if !run.arm("published_ports", entries.len(), PUBLISHED_PORTS_FLOOR) {
        return run;
    }

    for (service, spec) in &entries {
        if *service == EDGE_SERVICE {
            run.report_mut().pass(format!(
                "{service} publishes {spec} (the edge, expected on all interfaces)"
            ));
        } else if spec.starts_with("127.0.0.1:") {
            run.report_mut()
                .pass(format!("{service} publishes {spec} (loopback)"));
        } else {
            run.report_mut().fail(format!(
                "{service} publishes {spec} on ALL INTERFACES; it is not the edge, so this is a \
                 second entrance that bypasses Caddy and Cloudflare"
            ));
        }
    }

    if entries.len() != EXPECTED_PORT_VERDICTS {
        run.report_mut().fail(format!(
            "{} published port(s) ruled on, expected exactly {EXPECTED_PORT_VERDICTS}. Fewer means \
             ports went missing from the parse - a smaller green is not a pass. More means a port \
             was added; re-measure and bump the constant in the same commit.",
            entries.len()
        ));
    }

    control_route_posture(&mut run, compose, caddyfile);
    run
}

/// Couple activation of the staged public control route to removal of both
/// known insecure compose inputs. Separate from the port inventory above: it is
/// a different enumeration and can collapse on its own.
fn control_route_posture(run: &mut GateRun, compose: &ComposeFile, caddyfile: &str) {
    let control = compose.services.get("control");
    let posture_inputs: Vec<String> = control
        .map(|service| {
            let mut inputs: Vec<String> = service
                .environment
                .entries()
                .into_iter()
                .map(|(name, value)| format!("{name}={}", value.unwrap_or_default()))
                .collect();
            inputs.extend(service.body_scalars(&[
                "image",
                "environment",
                "ports",
                "volumes",
                "build",
                "depends_on",
                "healthcheck",
                "networks",
            ]));
            inputs
        })
        .unwrap_or_default();

    if !run.arm(
        "control_route_posture",
        posture_inputs.len(),
        CONTROL_POSTURE_FLOOR,
    ) {
        return;
    }

    // Commented Caddy examples do not match the anchored active-site
    // expression, and the staged form requires BOTH the commented site header
    // and the commented reverse_proxy line - one without the other is a
    // half-staged route nobody can activate by uncommenting one line.
    let route_staged = caddyfile.lines().any(commented_control_site)
        && caddyfile.lines().any(commented_control_reverse_proxy);
    let route_active = caddyfile.lines().any(active_control_site);

    let posture_relaxed = posture_inputs.iter().any(|input| {
        input.contains("--dev-insecure") || input == "ZEROSHIP_CONTROL_KEY=platform-key"
    });

    if posture_relaxed {
        if route_staged && !route_active {
            run.report_mut()
                .pass("control route is staged while compose has an insecure input");
        } else {
            run.report_mut().fail(
                "control route must remain staged and inactive while compose has an insecure input",
            );
        }
    } else if route_active {
        run.report_mut()
            .pass("control route is active after insecure compose inputs were removed");
    } else {
        run.report_mut()
            .fail("control route stayed inactive after insecure compose inputs were removed");
    }

    if route_active && posture_relaxed {
        run.report_mut().fail(
            "control route is active while control still has an insecure compose input",
        );
    } else {
        run.report_mut()
            .pass("control route state does not expose a relaxed control service");
    }
}

/// `control.{$ZEROSHIP_DOMAIN[:default]} {` - the site-block header, after any
/// scheme has been stripped.
fn control_site_body(rest: &str) -> bool {
    let Some(rest) = rest.strip_prefix("control.{$ZEROSHIP_DOMAIN") else {
        return false;
    };
    // An optional `:<default>` inside the brace, which must not itself contain
    // a closing brace.
    let rest = match rest.strip_prefix(':') {
        Some(tail) => match tail.find('}') {
            Some(index) => &tail[index..],
            None => return false,
        },
        None => rest,
    };
    let Some(rest) = rest.strip_prefix('}') else {
        return false;
    };
    rest.trim_start().starts_with('{')
}

/// An ACTIVE `control.<domain>` site block: not commented, scheme optional.
fn active_control_site(line: &str) -> bool {
    let rest = line.trim_start();
    let rest = rest
        .strip_prefix("http://")
        .or_else(|| rest.strip_prefix("https://"))
        .unwrap_or(rest);
    control_site_body(rest)
}

/// The same header commented out - the staged form.
fn commented_control_site(line: &str) -> bool {
    let Some(rest) = line.trim_start().strip_prefix('#') else {
        return false;
    };
    let Some(rest) = rest.trim_start().strip_prefix("http://") else {
        return false;
    };
    control_site_body(rest)
}

/// The commented `reverse_proxy control:9090` that the staged block needs.
fn commented_control_reverse_proxy(line: &str) -> bool {
    let Some(rest) = line.trim_start().strip_prefix('#') else {
        return false;
    };
    let Some(rest) = rest.trim_start().strip_prefix("reverse_proxy") else {
        return false;
    };
    if !rest.starts_with(char::is_whitespace) {
        return false;
    }
    let Some(rest) = rest.trim_start().strip_prefix("control:9090") else {
        return false;
    };
    rest.is_empty() || rest.starts_with(char::is_whitespace)
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;

    use super::{active_control_site, commented_control_reverse_proxy, commented_control_site, run};
    use crate::compose::ComposeFile;
    use crate::report::Verdict;

    /// Eight loopback ports plus the edge - nine verdicts, the shape of the
    /// tracked file - synthesised so the pass case is reachable from a test
    /// rather than only from the repository.
    fn clean_compose(edge_spec: &str) -> String {
        let mut yaml = String::from("services:\n");
        for (index, name) in ["auth", "gateway", "migrated", "postgres", "redis", "redpanda", "verdaccio"]
            .iter()
            .enumerate()
        {
            let port = 9000 + index;
            let _ = write!(
                yaml,
                "  {name}:\n    ports:\n      - \"127.0.0.1:{port}:{port}\"\n"
            );
        }
        yaml.push_str(&control_service());
        let _ = write!(
            yaml,
            "  caddy:\n    ports:\n      - \"{edge_spec}\"\n    environment:\n      \
             ZEROSHIP_DOMAIN: zeroship.localhost\n"
        );
        yaml
    }

    /// A control service carrying enough configuration to clear the posture
    /// arm, with no relaxation input.
    fn control_service() -> String {
        let mut yaml = String::from(
            "  control:\n    ports:\n      - \"127.0.0.1:9090:9090\"\n    command: \
             zeroship-control --port 9090\n    environment:\n",
        );
        for index in 0..12 {
            let _ = writeln!(yaml, "      ZEROSHIP_CONTROL_SETTING_{index}: value");
        }
        yaml
    }

    const ACTIVE_CADDYFILE: &str = "http://control.{$ZEROSHIP_DOMAIN:zeroship.localhost} {\n\treverse_proxy control:9090\n}\n";

    #[test]
    fn a_loopback_only_stack_with_an_active_route_is_green() {
        let yaml = clean_compose("80:80");
        let compose = ComposeFile::from_yaml(&yaml).expect("parses");
        let outcome = run(&compose, ACTIVE_CADDYFILE);
        assert!(
            matches!(outcome.report().verdict(), Verdict::Green { .. }),
            "{}",
            outcome.report().render("t")
        );
    }

    /// The violation this gate exists to catch: a non-edge service publishing
    /// on all interfaces. Paired with the green above, one variable apart.
    #[test]
    fn a_non_edge_service_on_all_interfaces_is_named() {
        let yaml = clean_compose("80:80").replace(
            "  gateway:\n    ports:\n      - \"127.0.0.1:9001:9001\"\n",
            "  gateway:\n    ports:\n      - \"8000:8000\"\n",
        );
        let compose = ComposeFile::from_yaml(&yaml).expect("parses");
        let outcome = run(&compose, ACTIVE_CADDYFILE);
        assert!(outcome.report().checks().iter().any(|check| {
            !check.ok && check.detail.starts_with("gateway publishes 8000:8000 on ALL INTERFACES")
        }));
    }

    /// A compose file with no ports at all must REFUSE, not pass. Without this
    /// the gate's clean result and a broken parse print the same thing.
    #[test]
    fn a_file_with_no_published_ports_is_a_refusal() {
        let compose =
            ComposeFile::from_yaml("services:\n  caddy:\n    image: caddy:2-alpine\n").expect("parses");
        let outcome = run(&compose, ACTIVE_CADDYFILE);
        match outcome.report().verdict() {
            Verdict::Refused(reason) => assert!(reason.contains("published_ports")),
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    /// A control service that vanished from the parse cannot be judged, and
    /// "not relaxed" is exactly the answer an empty read produces.
    #[test]
    fn an_unreadable_control_service_refuses_rather_than_reading_as_safe() {
        let yaml = clean_compose("80:80").replace(&control_service(), "  control:\n    image: x\n");
        let compose = ComposeFile::from_yaml(&yaml).expect("parses");
        let outcome = run(&compose, ACTIVE_CADDYFILE);
        match outcome.report().verdict() {
            Verdict::Refused(reason) => assert!(reason.contains("control_route_posture")),
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    /// An active public route in front of a control service still carrying a
    /// relaxation input is the coupling this arm exists for.
    #[test]
    fn an_active_route_over_a_relaxed_control_is_named() {
        let yaml = clean_compose("80:80").replace(
            "    command: zeroship-control --port 9090\n",
            "    command: zeroship-control --port 9090 --dev-insecure\n",
        );
        let compose = ComposeFile::from_yaml(&yaml).expect("parses");
        let outcome = run(&compose, ACTIVE_CADDYFILE);
        assert!(outcome.report().checks().iter().any(|check| {
            !check.ok && check.detail.contains("active while control still has an insecure")
        }));
    }

    #[test]
    fn a_commented_site_header_is_staged_and_not_active() {
        assert!(active_control_site(
            "http://control.{$ZEROSHIP_DOMAIN:zeroship.localhost} {"
        ));
        assert!(active_control_site("control.{$ZEROSHIP_DOMAIN} {"));
        assert!(active_control_site(
            "\thttps://control.{$ZEROSHIP_DOMAIN} \t{"
        ));
        // ONE VARIABLE: the same header behind a `#`.
        assert!(!active_control_site(
            "# http://control.{$ZEROSHIP_DOMAIN:zeroship.localhost} {"
        ));
        assert!(commented_control_site(
            "# http://control.{$ZEROSHIP_DOMAIN:zeroship.localhost} {"
        ));
        assert!(!commented_control_site("http://control.{$ZEROSHIP_DOMAIN} {"));
        // A site block for a DIFFERENT host must not be read as control's.
        assert!(!active_control_site("http://api.{$ZEROSHIP_DOMAIN} {"));
        // A mention with no block opening is prose, not a route.
        assert!(!active_control_site("control.{$ZEROSHIP_DOMAIN} is the host"));

        assert!(commented_control_reverse_proxy("#  reverse_proxy control:9090"));
        assert!(commented_control_reverse_proxy(
            "\t# reverse_proxy control:9090 \n"
        ));
        assert!(!commented_control_reverse_proxy("reverse_proxy control:9090"));
        assert!(!commented_control_reverse_proxy(
            "# reverse_proxy control:90901"
        ));
    }
}
