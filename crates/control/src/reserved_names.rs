//! Hostname labels the platform edge already claims, and the gate that keeps
//! this list from drifting away from the edge that defines it.
//!
//! # Why a registry concern at all
//!
//! An app's name IS its hostname label. The gateway derives the app from the
//! FIRST label of the `Host` header (`extract_app_name` /
//! `handle_subdomain`, `crates/gateway/src/router/dispatch.rs`), and the edge
//! serves creator apps off the `*.{domain}` wildcard. So registering an app
//! named `auth` is registering the hostname `auth.{domain}` — a name the
//! platform's own edge already routes somewhere else.
//!
//! Two distinct failure modes come out of that, and only the registry can
//! close either one, because by the time a request arrives the name is already
//! taken:
//!
//! - **Shadowed** (`auth`, `control`): the edge terminates the host before the
//!   gateway sees it. The creator registers the name, the API accepts it, and
//!   their app then silently never receives a single request. A namespace and
//!   denial defect. It is also the one that turns into an origin takeover in
//!   any deployment whose edge is NOT this Caddyfile: without a site block in
//!   front, `auth.{domain}` — the OIDC issuer origin — resolves through the
//!   wildcard to whoever holds the name.
//! - **Reachable** (`console`, `api`): the edge proxies these to the gateway,
//!   which resolves them as ordinary creator apps. The platform paths on those
//!   hosts are safe by route order (`/oidc/backchannel-logout` and every
//!   `/__zeroship/*` resource are mounted BEFORE the app catch-all in
//!   `crates/gateway/src/main.rs`), so nothing hijacks a platform endpoint.
//!   What a creator gets instead is arbitrary content served from a platform
//!   ORIGIN, and for `console` that origin carries a real privilege: the auth
//!   service emits `frame-ancestors 'self' <console origin>` on `/login`,
//!   `/signup` and `/consent` (`crates/auth/src/headers.rs`, fed by
//!   `[auth].frame_ancestor_origins` in `deploy/ops/zeroship.toml`). Holding
//!   the `console` name means holding the one origin allowed to frame the
//!   platform's real login page — the exact clickjacking the allowlist exists
//!   to prevent.
//!
//! Which name is in which class is not prose here: [`EDGE_ROUTING`] states it
//! and `edge_routing_matches_the_adapted_config` checks it against where the
//! edge actually dials, so flipping a block's upstream fails rather than
//! leaving the two paragraphs above quietly wrong.
//!
//! # Why the list is deployment-invariant
//!
//! `ZEROSHIP_DOMAIN` is configurable, so a host vendor runs the stack on their
//! own domain. The LABELS are not configurable: every host in
//! `deploy/ops/Caddyfile` is written `<literal-label>.{$ZEROSHIP_DOMAIN:...}`,
//! and the file's own header forbids reintroducing a literal domain. Moving
//! the deployment moves the domain and keeps the labels, so one list covers
//! every deployment of this edge — and the gate below fails the build's test
//! run if a host ever stops being written that way.
//!
//! # Why there is no operator knob to unreserve a name
//!
//! Unreserving `auth` without changing the edge just re-creates the shadowed
//! app. A deployment that wants the name free changes its edge routing, and
//! the gate below is then what tells it the two agree.
//!
//! # How the gate knows what the edge claims: Caddy answers, not a parser
//!
//! This module used to read `deploy/ops/Caddyfile` directly and pick site
//! addresses out of the text. That is parsing a language we do not own, and it
//! was silently wrong for every spelling it had not been taught. MEASURED
//! 2026-08-20 against that parser — each of these routes a host away from
//! creator apps, and each left the answer at the unchanged four labels:
//!
//! | edge change | old parser said |
//! | --- | --- |
//! | `@status host status.{$D}` + `handle @status` | `Ok(["auth","control","console","api"])` |
//! | `@h header Host h.{$D}` + `handle @h` | `Ok([])` |
//! | `@b { host b.{$D} }` + `handle @b` | `Ok([])` |
//! | `import sites/*.caddy` (a site block in another file) | `Ok([])` |
//!
//! and `@x expression {host}.startsWith("...")` is CEL, so no text parser can
//! decide it at any level of effort.
//!
//! So the Caddyfile is no longer the input. `deploy/ops/caddy-claimed-hosts.sh`
//! runs `caddy adapt`, which lowers the Caddyfile to Caddy's JSON config —
//! resolving `{$ENV:default}`, resolving `import`, and emitting every host
//! claim, however it was spelled, as a concrete `match` entry. The result is
//! committed as `deploy/ops/caddy-claimed-hosts.json` and read here, so this
//! gate needs no Caddy, no docker and no network: it reads a small closed data
//! format instead of an open grammar.
//!
//! Unknown matchers are an `Err`, not a skipped entry ([`MATCHERS_WITHOUT_HOSTS`]).
//! When Caddy grows a matcher module, or someone writes a `expression` matcher,
//! this gate fails and names it rather than returning a set that is quietly
//! short.
//!
//! # What this gate does NOT catch
//!
//! - **A hand-forged artifact.** The `caddyfile_sha256` field binds the
//!   artifact to the Caddyfile's bytes, so an edge edit without a regenerate is
//!   caught here. It does NOT prove the artifact is what Caddy produces: a
//!   human could edit the JSON to drop a host and leave the sha alone. Only
//!   re-running the adapter catches that, which needs Caddy and therefore
//!   cannot live in this test — it is `caddy-claimed-hosts.sh --check`, wired
//!   into CI.
//! - **The DEPLOYED copy of the Caddyfile.** This binds the REPO's file at
//!   compile time. Production routes by whatever is at
//!   `/opt/zeroship-deploy/ops/Caddyfile`, which `deploy/scripts/deploy-remote.sh`
//!   overwrites from this repo on every roll (its `scp` of `deploy/ops/Caddyfile`)
//!   — so the two can only diverge by a hand-edit ON the host, between rolls,
//!   which the next roll silently reverts. Verified byte-identical 2026-08-20.
//!   Nothing in this tree re-checks it.
//! - **Any edge that is not this Caddyfile.** A deployment fronted by
//!   Cloudflare Workers, an ALB or an nginx of its own claims hosts this gate
//!   never sees. `RESERVED_APP_NAMES` is then a floor, not a description.
//! - **Hosts claimed somewhere other than a `match`.** The walk reads `match`
//!   entries and nothing else, so a `listen` address naming a host would be
//!   invisible. A `match` whose shape it does not understand — a TLS connection
//!   policy's, which is an object rather than an array and whose `sni` claims
//!   hostnames — is an `Err`, so that one fails loudly rather than silently.
//!
//! # The next hole in THIS mechanism
//!
//! Every mechanism has one; naming it is cheaper than discovering it. Here it
//! is the gap between the artifact and the running edge, and it has two halves.
//! The artifact is generated with the Caddy image the compose file pins for the
//! `caddy` service, which is why the generator reads that image out of compose
//! instead of naming one — but nothing checks that the HOST is running the
//! image compose pins, or that Caddy was started with this config file at all.
//! Adapt-time behaviour is also version-dependent: a future Caddy that lowers
//! some directive into a shape this walk reads differently would move the
//! answer without moving the Caddyfile. The version is recorded in the artifact
//! and deliberately NOT compared, because comparing it would fail every image
//! bump for no reason; that trade is what leaves the hole open.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;
use sha2::{Digest, Sha256};

/// Hostname labels under the app domain that the platform edge claims, so a
/// creator app may not take them.
///
/// The authority for this set is `deploy/ops/Caddyfile` as Caddy adapts it:
/// these are exactly the single-label hosts its adapted config matches on.
/// `reserved_set_matches_the_edge` (below) fails if the two ever disagree in
/// either direction — a host the list lacks, or a listed name the edge does not
/// claim.
pub const RESERVED_APP_NAMES: &[&str] = &["api", "auth", "console", "control"];

/// Which of the two failure modes in this module's header a reserved name is
/// being held against. Derived from where the edge sends it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EdgeRouting {
    /// The edge terminates the host at a platform service, so a creator app
    /// with this name would never receive a request.
    Shadowed,
    /// The edge forwards the host to the gateway, which would resolve it as an
    /// ordinary creator app — serving creator content from a platform origin.
    Reachable,
}

/// The upstream that means [`EdgeRouting::Reachable`]: the gateway resolves
/// whatever app holds the name.
const GATEWAY_UPSTREAM: &str = "gateway:8000";

/// Where the edge sends each reserved name. This is the table the module
/// header's Shadowed/Reachable prose describes, and
/// `edge_routing_matches_the_adapted_config` holds it to the adapted config —
/// so moving `console` off the gateway fails here instead of leaving the
/// reasoning stale.
pub const EDGE_ROUTING: &[(&str, EdgeRouting)] = &[
    ("api", EdgeRouting::Reachable),
    ("auth", EdgeRouting::Shadowed),
    ("console", EdgeRouting::Reachable),
    ("control", EdgeRouting::Shadowed),
];

/// Is `name` a hostname label the platform edge claims?
///
/// ASCII-case-insensitive on purpose. Hostnames are case-insensitive
/// (RFC 4343) and `create_app`'s charset rule admits uppercase, so `Auth` and
/// `auth` name the same host to every browser and proxy on the path. Matching
/// case-sensitively here would reserve the name in the registry and leave the
/// host it actually resolves to unprotected.
#[must_use]
pub fn is_reserved_app_name(name: &str) -> bool {
    RESERVED_APP_NAMES
        .iter()
        .any(|r| r.eq_ignore_ascii_case(name))
}

/// The refusal message for a reserved name. Names the label, says who holds it
/// and what to do — a creator hitting this must not have to guess whether they
/// tripped the charset rule.
#[must_use]
pub fn reserved_name_message(name: &str) -> String {
    format!(
        "'{name}' is reserved by the platform: {name}.<domain> is routed to a \
         platform service, not to creator apps. Choose a different app name."
    )
}

// ---------------------------------------------------------------------------
// Drift gate — read the edge config as Caddy adapts it
// ---------------------------------------------------------------------------

/// One hostname label the edge claims, and every upstream reachable under it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostClaim {
    /// The single label under the app domain, lowercased.
    pub label: String,
    /// Dial targets reachable under the route that claims the label, sorted.
    /// A route nested inside another contributes to both; the semantics are
    /// "reachable under", not "dialed directly by".
    pub upstreams: Vec<String>,
}

/// Caddy HTTP matchers that cannot claim a hostname, and are therefore safe to
/// walk past.
///
/// EVERYTHING NOT IN THIS LIST IS AN ERROR. That is the point: the previous
/// text parser's holes were all shapes it did not recognise and skipped, so
/// recognising less than the edge uses has to fail loudly here. Adding a name
/// to this list is asserting that the matcher cannot restrict a request by
/// host — `expression`, `header_regexp` and `vars` deliberately are NOT here,
/// because each can, and none can be decided statically.
const MATCHERS_WITHOUT_HOSTS: &[&str] = &[
    "client_ip",
    "file",
    "method",
    "path",
    "path_regexp",
    "protocol",
    "query",
    "remote_ip",
];

/// Every hostname label the adapted edge config claims, with where each points.
///
/// `artifact` is `deploy/ops/caddy-claimed-hosts.json` and `caddyfile` is
/// `deploy/ops/Caddyfile`; the artifact's recorded digest must match the
/// Caddyfile's bytes, so a stale artifact is an `Err` rather than a stale
/// answer.
///
/// Wildcard (`*`) and apex hosts claim no label and are excluded — the first is
/// the creator-app catch-all, the second cannot shadow a `{app}.{domain}` name.
///
/// # Errors
///
/// Returns a message naming the problem when the artifact is stale or
/// malformed, when a host is not built from the domain variable, or when the
/// config uses a matcher this cannot rule out as a host claim.
pub fn claimed_host_labels(artifact: &str, caddyfile: &str) -> Result<Vec<HostClaim>, String> {
    let root: Value =
        serde_json::from_str(artifact).map_err(|e| format!("artifact is not valid JSON: {e}"))?;

    let recorded = root
        .get("caddyfile_sha256")
        .and_then(Value::as_str)
        .ok_or("artifact has no caddyfile_sha256 field")?;
    let actual = hex::encode(Sha256::digest(caddyfile.as_bytes()));
    if recorded != actual {
        return Err(format!(
            "the committed edge artifact does not describe the Caddyfile next to \
             it: caddy-claimed-hosts.json records sha256 {recorded} but \
             deploy/ops/Caddyfile hashes to {actual}. The edge changed without \
             the artifact being regenerated, so nothing here knows what the edge \
             now claims. Run: deploy/ops/caddy-claimed-hosts.sh --write"
        ));
    }

    let sentinel = root
        .get("sentinel_domain")
        .and_then(Value::as_str)
        .ok_or("artifact has no sentinel_domain field")?
        .to_ascii_lowercase();
    let config = root
        .get("config")
        .ok_or("artifact has no config field")?;

    let mut claims: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    collect_routes(config, &sentinel, &mut claims)?;

    Ok(claims
        .into_iter()
        .map(|(label, upstreams)| HostClaim {
            label,
            upstreams: upstreams.into_iter().collect(),
        })
        .collect())
}

/// Walk every value looking for Caddy routes — objects carrying a `match`
/// array. Generic rather than indexing `apps.http.servers.*.routes`, because
/// routes nest arbitrarily deep inside `subroute` handlers, which is exactly
/// where a matcher-claimed host lives.
fn collect_routes(
    value: &Value,
    sentinel: &str,
    out: &mut BTreeMap<String, BTreeSet<String>>,
) -> Result<(), String> {
    match value {
        Value::Object(map) => {
            if let Some(matchers) = map.get("match") {
                // An HTTP route's `match` is an array of matcher sets. Other
                // Caddy modules spell `match` differently - a TLS connection
                // policy's is an OBJECT, and `{"sni": [...]}` inside one claims
                // hostnames just as a route does. Matching only on the array
                // shape would walk straight past it, which is the same silent
                // skip this gate replaced. So: unknown shape, error.
                let Value::Array(matchers) = matchers else {
                    return Err(format!(
                        "the edge config has a `match` that is not an array of \
                         matcher sets, so it belongs to a Caddy module this gate \
                         does not read - a TLS connection policy, for instance, \
                         whose `sni` claims hostnames exactly as a route does. It \
                         must not be walked past: {matchers}"
                    ));
                };
                let mut labels = Vec::new();
                for set in matchers {
                    collect_matcher_set_labels(set, sentinel, &mut labels)?;
                }
                if !labels.is_empty() {
                    let mut upstreams = BTreeSet::new();
                    if let Some(handle) = map.get("handle") {
                        collect_dials(handle, &mut upstreams);
                    }
                    for label in labels {
                        out.entry(label).or_default().extend(upstreams.iter().cloned());
                    }
                }
            }
            for v in map.values() {
                collect_routes(v, sentinel, out)?;
            }
        }
        Value::Array(items) => {
            for v in items {
                collect_routes(v, sentinel, out)?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// The labels one matcher set claims. A matcher set is an object keyed by
/// matcher module name; an unrecognised key is an error, never a skip.
fn collect_matcher_set_labels(
    set: &Value,
    sentinel: &str,
    out: &mut Vec<String>,
) -> Result<(), String> {
    let Some(map) = set.as_object() else {
        return Err(format!("matcher set is not an object: {set}"));
    };
    for (name, config) in map {
        match name.as_str() {
            "host" => {
                let Some(hosts) = config.as_array() else {
                    return Err(format!("`host` matcher is not an array: {config}"));
                };
                for host in hosts {
                    let Some(host) = host.as_str() else {
                        return Err(format!("`host` matcher entry is not a string: {host}"));
                    };
                    if let Some(label) = host_label(host, sentinel)? {
                        out.push(label);
                    }
                }
            }
            // `@x header Host foo.{$D}` restricts by host just as surely as the
            // `host` matcher does, and lands under a different key. Other
            // headers cannot claim a host.
            "header" => {
                let Some(headers) = config.as_object() else {
                    return Err(format!("`header` matcher is not an object: {config}"));
                };
                for (header, values) in headers {
                    if !header.eq_ignore_ascii_case("host") {
                        continue;
                    }
                    let Some(values) = values.as_array() else {
                        return Err(format!("`header` Host matcher is not an array: {values}"));
                    };
                    for host in values {
                        let Some(host) = host.as_str() else {
                            return Err(format!("`header` Host entry is not a string: {host}"));
                        };
                        if let Some(label) = host_label(host, sentinel)? {
                            out.push(label);
                        }
                    }
                }
            }
            // A negated matcher set still names hosts, and its nested sets go
            // through the same rules.
            "not" => match config {
                Value::Array(sets) => {
                    for nested in sets {
                        collect_matcher_set_labels(nested, sentinel, out)?;
                    }
                }
                Value::Object(_) => collect_matcher_set_labels(config, sentinel, out)?,
                other => return Err(format!("`not` matcher has an unexpected shape: {other}")),
            },
            other if MATCHERS_WITHOUT_HOSTS.contains(&other) => {}
            other => {
                return Err(format!(
                    "the edge config uses the `{other}` matcher, which this gate \
                     cannot rule out as a hostname claim. It must not be walked \
                     past: a matcher that restricts by host and is skipped here \
                     is exactly the hole this gate exists to close. If `{other}` \
                     provably cannot match on host, add it to \
                     MATCHERS_WITHOUT_HOSTS in \
                     crates/control/src/reserved_names.rs; if it can, the host it \
                     claims must be in RESERVED_APP_NAMES."
                ));
            }
        }
    }
    Ok(())
}

/// Every `dial` target anywhere under a route's handler chain.
fn collect_dials(value: &Value, out: &mut BTreeSet<String>) {
    match value {
        Value::Object(map) => {
            for (k, v) in map {
                if k == "dial" {
                    if let Some(s) = v.as_str() {
                        out.insert(s.to_owned());
                    }
                }
                collect_dials(v, out);
            }
        }
        Value::Array(items) => {
            for v in items {
                collect_dials(v, out);
            }
        }
        _ => {}
    }
}

/// The single hostname label `host` claims under the sentinel domain, or `None`
/// when it claims no creator-app name.
fn host_label(host: &str, sentinel: &str) -> Result<Option<String>, String> {
    let host = host.to_ascii_lowercase();
    // The apex. Creator apps live at `{app}.{domain}`, so the bare domain
    // shadows no name.
    if host == sentinel {
        return Ok(None);
    }
    let Some(label) = host
        .strip_suffix(sentinel)
        .and_then(|p| p.strip_suffix('.'))
    else {
        return Err(format!(
            "the edge claims the host `{host}`, which is not built from \
             {{$ZEROSHIP_DOMAIN}} — so it is a literal domain baked into \
             deploy/ops/Caddyfile, which that file's header forbids, and its \
             label cannot be checked against RESERVED_APP_NAMES"
        ));
    };
    // The creator-app catch-all. Claims no name.
    if label == "*" {
        return Ok(None);
    }
    if label.contains('.') {
        return Err(format!(
            "the edge claims `{host}`, a multi-label prefix under the app \
             domain; only a single label is understood here"
        ));
    }
    if label.is_empty()
        || !label
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(format!("the edge claims `{host}`, which has no usable label"));
    }
    Ok(Some(label.to_owned()))
}

/// Hostname labels a compose file claims under `${ZEROSHIP_DOMAIN}`.
///
/// The Caddyfile is the edge and therefore the authority, but compose also
/// spells platform hosts out (service URLs, network aliases). Anything it
/// names must already be reserved, or the two files disagree about what a
/// creator may take. Subset check only: compose need not name every host.
#[must_use]
pub fn compose_domain_labels(compose: &str) -> Vec<String> {
    const DOMAIN_VAR: &str = ".${ZEROSHIP_DOMAIN";
    let mut labels = Vec::new();
    for line in compose.lines() {
        let mut rest = line;
        while let Some(at) = rest.find(DOMAIN_VAR) {
            let reversed: String = rest[..at]
                .chars()
                .rev()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
                .collect();
            let label: String = reversed.chars().rev().collect();
            if !label.is_empty() {
                labels.push(label);
            }
            rest = &rest[at + DOMAIN_VAR.len()..];
        }
    }
    labels
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `include_str!` rather than a runtime read: the paths resolve at compile
    /// time, so moving or deleting either file breaks the build instead of
    /// quietly leaving this gate with nothing to check. It also makes editing
    /// the Caddyfile rebuild and rerun the gate.
    const CADDYFILE: &str = include_str!("../../../deploy/ops/Caddyfile");
    const ARTIFACT: &str = include_str!("../../../deploy/ops/caddy-claimed-hosts.json");
    const COMPOSE: &str = include_str!("../../../deploy/compose/docker-compose.yml");

    /// The matched fixture pair. Both are REAL `caddy adapt` output, generated
    /// by the same script that writes the live artifact — a hand-written
    /// "adapted config" would only prove this handles a shape Caddy does not
    /// produce. They differ in ONE variable: four lines adding a `@status host`
    /// matcher inside the wildcard block. `fixtures_differ_in_one_variable`
    /// holds them to that.
    const FIXTURE_CONTROL_CADDYFILE: &str = include_str!("../testdata/edge_control.Caddyfile");
    const FIXTURE_CONTROL: &str = include_str!("../testdata/edge_control.json");
    const FIXTURE_MATCHER_CADDYFILE: &str =
        include_str!("../testdata/edge_matcher_claim.Caddyfile");
    const FIXTURE_MATCHER: &str = include_str!("../testdata/edge_matcher_claim.json");

    fn labels_of(claims: &[HostClaim]) -> Vec<String> {
        claims.iter().map(|c| c.label.clone()).collect()
    }

    // -----------------------------------------------------------------------
    // THE GATE
    // -----------------------------------------------------------------------

    /// THE DRIFT GATE. The reserved set and the hosts the edge claims are the
    /// same set, checked in BOTH directions:
    ///
    /// - a host the edge claims that is not reserved means a creator can still
    ///   register it (the defect this module exists to close);
    /// - a reserved name the edge does not claim means the list over-reserves —
    ///   a name creators cannot have and nothing needs.
    #[test]
    fn reserved_set_matches_the_edge() {
        let claims = claimed_host_labels(ARTIFACT, CADDYFILE)
            .expect("the committed edge artifact describes deploy/ops/Caddyfile");
        let from_edge = labels_of(&claims);
        let mut reserved: Vec<String> =
            RESERVED_APP_NAMES.iter().map(|s| (*s).to_owned()).collect();
        reserved.sort_unstable();
        assert_eq!(
            from_edge, reserved,
            "deploy/ops/Caddyfile and RESERVED_APP_NAMES disagree. Every host the \
             edge claims under the app domain must have its label in \
             RESERVED_APP_NAMES and vice versa; update \
             crates/control/src/reserved_names.rs in the same change as the edge \
             config."
        );
    }

    /// The Shadowed/Reachable split in this module's header is a claim about
    /// where the edge dials, so it is checked against where the edge dials.
    /// Flipping `console` to a service of its own moves it between classes and
    /// fails here, instead of leaving the header's reasoning stale with nothing
    /// to catch it.
    #[test]
    fn edge_routing_matches_the_adapted_config() {
        let claims = claimed_host_labels(ARTIFACT, CADDYFILE).expect("artifact parses");
        let table: BTreeMap<&str, EdgeRouting> = EDGE_ROUTING.iter().copied().collect();
        assert_eq!(
            table.len(),
            RESERVED_APP_NAMES.len(),
            "EDGE_ROUTING must classify every reserved name"
        );
        for claim in &claims {
            let observed = if claim.upstreams == vec![GATEWAY_UPSTREAM.to_owned()] {
                EdgeRouting::Reachable
            } else {
                EdgeRouting::Shadowed
            };
            let declared = table
                .get(claim.label.as_str())
                .copied()
                .unwrap_or_else(|| panic!("EDGE_ROUTING does not classify '{}'", claim.label));
            assert_eq!(
                observed, declared,
                "'{}' is documented as {declared:?} but the edge dials it to {:?}",
                claim.label, claim.upstreams
            );
        }
    }

    /// Compose spells some of the same hosts out. Everything it claims under
    /// the app domain must already be reserved.
    #[test]
    fn compose_claims_no_unreserved_label() {
        let unreserved: Vec<String> = compose_domain_labels(COMPOSE)
            .into_iter()
            .filter(|l| !is_reserved_app_name(l))
            .collect();
        assert!(
            unreserved.is_empty(),
            "deploy/compose/docker-compose.yml claims hostname labels under \
             ${{ZEROSHIP_DOMAIN}} that RESERVED_APP_NAMES does not cover: \
             {unreserved:?}"
        );
        // The check above is vacuously true if the scan finds nothing at all,
        // which is the failure mode that makes a green gate worthless.
        assert!(
            compose_domain_labels(COMPOSE).contains(&"auth".to_owned()),
            "compose label scan found nothing it should have found"
        );
    }

    // -----------------------------------------------------------------------
    // THE DEFECT THIS REPLACED: a host claimed by a MATCHER, not a site block
    // -----------------------------------------------------------------------

    /// The two fixtures differ in exactly the matcher block and nothing else,
    /// so the pair below is a one-variable comparison rather than two unrelated
    /// files. Asserted, not asserted-in-a-comment.
    #[test]
    fn fixtures_differ_in_one_variable() {
        let added: Vec<&str> = FIXTURE_MATCHER_CADDYFILE
            .lines()
            .filter(|l| !FIXTURE_CONTROL_CADDYFILE.lines().any(|c| c == *l))
            .collect();
        assert_eq!(
            added,
            vec![
                "\t@status host status.{$ZEROSHIP_DOMAIN:zeroship.localhost}",
                "\thandle @status {",
                "\t\treverse_proxy status:9099",
            ],
            "the fixture pair must differ only by the matcher that claims a host"
        );
        assert_eq!(
            FIXTURE_CONTROL_CADDYFILE, CADDYFILE,
            "the control fixture must be the live edge config, so the pair \
             compares against what actually ships"
        );
    }

    /// THE REFUSAL. A `@status host status.{$D}` matcher inside the wildcard
    /// block routes `status.<domain>` away from creator apps without adding a
    /// site block. The text parser this replaced answered the unchanged four
    /// labels for exactly this file (MEASURED 2026-08-20); the gate must now
    /// SEE the host and name it, so `reserved_set_matches_the_edge` goes red.
    #[test]
    fn a_host_claimed_by_a_matcher_is_seen_and_named() {
        let claims = claimed_host_labels(FIXTURE_MATCHER, FIXTURE_MATCHER_CADDYFILE)
            .expect("the matcher fixture parses");
        let labels = labels_of(&claims);
        assert!(
            labels.contains(&"status".to_owned()),
            "the matcher-claimed host must appear; got {labels:?}"
        );
        let unreserved: Vec<&String> = labels
            .iter()
            .filter(|l| !is_reserved_app_name(l.as_str()))
            .collect();
        assert_eq!(
            unreserved,
            vec![&"status".to_owned()],
            "and it must be the one label RESERVED_APP_NAMES does not cover, so \
             the drift gate fails naming it"
        );
        // It is claimed by the matcher, so it points at the matcher's upstream
        // rather than the wildcard's gateway. Distinguishes "found the host"
        // from "found some host".
        let status = claims.iter().find(|c| c.label == "status").unwrap();
        assert_eq!(status.upstreams, vec!["status:9099".to_owned()]);
    }

    /// The control for the test above, differing in ONE variable: the matcher.
    /// Without it the same file yields exactly the reserved four, so the
    /// refusal is evidence about the matcher and not about a gate that has
    /// started refusing everything.
    #[test]
    fn the_same_edge_without_the_matcher_claims_only_reserved_names() {
        let claims = claimed_host_labels(FIXTURE_CONTROL, FIXTURE_CONTROL_CADDYFILE)
            .expect("the control fixture parses");
        let labels = labels_of(&claims);
        assert_eq!(labels, vec!["api", "auth", "console", "control"]);
        assert!(
            labels.iter().all(|l| is_reserved_app_name(l)),
            "the control arm must pass the property the other arm fails"
        );
    }

    // -----------------------------------------------------------------------
    // Staleness and fail-closed behaviour
    // -----------------------------------------------------------------------

    /// The artifact is a snapshot, so the gate is only meaningful while it
    /// describes the Caddyfile in front of it. An edge edit without a
    /// regenerate must be an ERROR, not an answer computed from the old edge.
    #[test]
    fn an_artifact_that_does_not_describe_the_caddyfile_is_an_error() {
        let edited = format!(
            "{CADDYFILE}\nhttp://status.{{$ZEROSHIP_DOMAIN:zeroship.localhost}} {{\n\
             \treverse_proxy status:9099\n}}\n"
        );
        assert_ne!(edited, CADDYFILE, "the edit must actually apply");
        let err = claimed_host_labels(ARTIFACT, &edited)
            .expect_err("a Caddyfile the artifact does not describe must not yield labels");
        assert!(err.contains("sha256"), "{err}");
        assert!(err.contains("caddy-claimed-hosts.sh --write"), "{err}");
    }

    /// A matcher the walk cannot rule out as a host claim must fail the gate
    /// and name it. `expression` is CEL — undecidable at any level of parser
    /// effort — which is precisely why it must not be walked past.
    #[test]
    fn an_undecidable_matcher_is_refused_by_name() {
        let mut root: Value = serde_json::from_str(FIXTURE_CONTROL).unwrap();
        let routes = root["config"]["apps"]["http"]["servers"]["srv0"]["routes"]
            .as_array_mut()
            .unwrap();
        routes.push(serde_json::json!({
            "match": [{ "expression": { "expr": "{http.request.host}.startsWith(\"x.\")" } }],
            "handle": [{ "handler": "static_response", "body": "x" }],
        }));
        let mutated = serde_json::to_string(&root).unwrap();
        assert!(mutated.contains("expression"), "the mutation must apply");

        let err = claimed_host_labels(&mutated, FIXTURE_CONTROL_CADDYFILE)
            .expect_err("an undecidable matcher must not be skipped");
        assert!(err.contains("expression"), "{err}");
        assert!(err.contains("MATCHERS_WITHOUT_HOSTS"), "{err}");
    }

    /// A `match` that is not a route's array of matcher sets belongs to some
    /// other Caddy module — a TLS connection policy's is an object, and its
    /// `sni` claims hostnames just as a route does. Walking past it is the same
    /// silent skip this gate replaced, so it is an error.
    #[test]
    fn a_match_this_does_not_understand_is_refused() {
        let mut root: Value = serde_json::from_str(FIXTURE_CONTROL).unwrap();
        root["config"]["apps"]["http"]["servers"]["srv0"]["tls_connection_policies"] =
            serde_json::json!([{ "match": { "sni": ["status.zsdomain.invalid"] } }]);
        let mutated = serde_json::to_string(&root).unwrap();
        assert!(mutated.contains("tls_connection_policies"), "the mutation must apply");

        let err = claimed_host_labels(&mutated, FIXTURE_CONTROL_CADDYFILE)
            .expect_err("an unreadable `match` shape must not be skipped");
        assert!(err.contains("not an array"), "{err}");
    }

    /// A host not built from the domain variable is a literal domain baked into
    /// the edge, which the Caddyfile's header forbids and which no single list
    /// can cover across deployments. It must be an error, never a skipped
    /// entry.
    #[test]
    fn a_literal_domain_host_is_an_error() {
        let mut root: Value = serde_json::from_str(FIXTURE_CONTROL).unwrap();
        root["config"]["apps"]["http"]["servers"]["srv0"]["routes"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({
                "match": [{ "host": ["auth.zeroship.ai"] }],
                "handle": [{ "handler": "static_response" }],
            }));
        let mutated = serde_json::to_string(&root).unwrap();
        assert!(mutated.contains("auth.zeroship.ai"), "the mutation must apply");

        let err = claimed_host_labels(&mutated, FIXTURE_CONTROL_CADDYFILE)
            .expect_err("a literal-domain host must not parse as no new label");
        assert!(err.contains("auth.zeroship.ai"), "{err}");
    }

    /// The wildcard is the creator-app catch-all and the apex cannot shadow a
    /// `{app}.{domain}` name; neither claims a label. Without this the gate
    /// would demand `*` be reserved.
    #[test]
    fn wildcard_and_apex_claim_no_label() {
        assert_eq!(host_label("*.zsdomain.invalid", "zsdomain.invalid"), Ok(None));
        assert_eq!(host_label("zsdomain.invalid", "zsdomain.invalid"), Ok(None));
    }

    /// Caddy lowercases site addresses but leaves matcher hosts in source case
    /// (MEASURED 2026-08-20: `header Host hdrhost.ZSDOMAIN.invalid` survives
    /// adapt verbatim). Hostnames are case-insensitive, so the walk must be too
    /// or a mixed-case matcher claims a host this never matches to a label.
    #[test]
    fn host_matching_is_case_insensitive() {
        assert_eq!(
            host_label("Status.ZSDOMAIN.invalid", "zsdomain.invalid"),
            Ok(Some("status".to_owned()))
        );
    }

    /// A `header Host` matcher claims a host as surely as the `host` matcher,
    /// under a different JSON key. The old text parser returned `Ok([])` for
    /// this shape.
    #[test]
    fn a_header_host_matcher_claims_its_label() {
        let mut labels = Vec::new();
        collect_matcher_set_labels(
            &serde_json::json!({ "header": { "Host": ["status.zsdomain.invalid"] } }),
            "zsdomain.invalid",
            &mut labels,
        )
        .unwrap();
        assert_eq!(labels, vec!["status".to_owned()]);
    }

    /// Headers other than Host restrict nothing about the hostname, so they
    /// must not be refused — the control that keeps the rule above from being
    /// "any header matcher fails the gate".
    #[test]
    fn a_non_host_header_matcher_claims_nothing() {
        let mut labels = Vec::new();
        collect_matcher_set_labels(
            &serde_json::json!({ "header": { "Accept": ["application/json"] } }),
            "zsdomain.invalid",
            &mut labels,
        )
        .unwrap();
        assert!(labels.is_empty());
    }

    // -----------------------------------------------------------------------
    // The reservation itself
    // -----------------------------------------------------------------------

    #[test]
    fn reservation_is_case_insensitive() {
        assert!(is_reserved_app_name("auth"));
        assert!(is_reserved_app_name("AUTH"));
        assert!(is_reserved_app_name("Console"));
    }

    /// The control the reservation must not swallow: a name one character away
    /// from a reserved one is an ordinary app name.
    #[test]
    fn near_miss_names_are_not_reserved() {
        for ok in ["auths", "api2", "consol", "kontrol", "my-api", "authy", "ap"] {
            assert!(
                !is_reserved_app_name(ok),
                "'{ok}' is not a platform host and must stay registrable"
            );
        }
    }
}
