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
//! # Why the list is deployment-invariant
//!
//! `ZEROSHIP_DOMAIN` is configurable, so a host vendor runs the stack on their
//! own domain. The LABELS are not configurable: every site block in
//! `deploy/ops/Caddyfile` is written `<literal-label>.{$ZEROSHIP_DOMAIN:...}`,
//! and the file's own header forbids reintroducing a literal domain. Moving
//! the deployment moves the domain and keeps the labels, so one list covers
//! every deployment of this edge — and [`caddy_site_labels`] fails the build's
//! test run if a site block ever stops being written that way.
//!
//! # Why there is no operator knob to unreserve a name
//!
//! Unreserving `auth` without changing the edge just re-creates the shadowed
//! app. A deployment that wants the name free changes its edge routing, and
//! the gate below is then what tells it the two agree.

/// Hostname labels under the app domain that the platform edge claims, so a
/// creator app may not take them.
///
/// The authority for this set is `deploy/ops/Caddyfile`: these are exactly its
/// non-wildcard site-block labels. `reserved_set_matches_caddyfile` (below)
/// fails if the two ever disagree in either direction — a new site block the
/// list lacks, or a listed name no site block claims.
pub const RESERVED_APP_NAMES: &[&str] = &["api", "auth", "console", "control"];

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
// Drift gate — parse the edge config that defines the set
// ---------------------------------------------------------------------------

/// The non-wildcard hostname labels claimed by site blocks in a Caddyfile.
///
/// Deliberately STRICT rather than best-effort: every site address must be
/// written `http(s)://<label>.{$ZEROSHIP_DOMAIN...}`, and anything else is an
/// `Err`, not a skipped line. A lenient parser here would silently return the
/// same set for a Caddyfile that grew a block it could not read, which makes a
/// green gate meaningless.
///
/// Returns labels in source order, wildcard (`*`) blocks excluded — that one is
/// the creator-app catch-all and claims no name.
///
/// # How a site block is RECOGNISED
///
/// By POSITION, not by spelling: a line that opens a brace at top level is a
/// site-address list (or, when it is a bare `{`, Caddy's global options block).
/// That is the whole grammar: Caddy requires the opening brace to be the last
/// token of the header line, and everything nested is a directive.
///
/// This used to key off the `http://` prefix instead, which had a silent hole
/// the gate exists to prevent: Caddy accepts a SCHEME-LESS site address
/// (`status.{$ZEROSHIP_DOMAIN} { ... }`), and such a block matched no prefix,
/// hit the `continue`, and left the parser returning `Ok` with the edge quietly
/// claiming a host `RESERVED_APP_NAMES` did not cover. Recognising by position
/// turns that block into an `Err`: an unreadable site address must fail the
/// gate, never be skipped by it.
///
/// A comma-separated address list (`http://a.{$D}, http://b.{$D} {`) is Caddy
/// grammar too, and every address in it is checked; the prefix-keyed version
/// read only the first and dropped the rest.
///
/// # Errors
///
/// Returns the offending line when a site address is not in the expected form.
pub fn caddy_site_labels(caddyfile: &str) -> Result<Vec<String>, String> {
    let mut labels = Vec::new();
    let mut depth = 0usize;
    for line in caddyfile.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // `{$ZEROSHIP_DOMAIN:zeroship.localhost}` is braces that are NOT block
        // structure. Drop every brace pair that opens and closes on this line
        // and what remains is block structure only.
        let structure = strip_inline_brace_pairs(line);
        let opens = structure.matches('{').count();
        let closes = structure.matches('}').count();

        if depth == 0 && opens > 0 {
            let Some(header) = line.strip_suffix('{').map(str::trim) else {
                return Err(format!(
                    "top-level line opens a block but does not end with `{{`, so its \
                     site addresses cannot be read: {line}"
                ));
            };
            // The bare `{` of Caddy's global options block. Claims no name.
            if !header.is_empty() {
                for address in header.split(',') {
                    if let Some(label) = site_address_label(address.trim())? {
                        labels.push(label);
                    }
                }
            }
        }

        // Saturating, so a Caddyfile with an unbalanced `}` keeps reading later
        // site blocks at top level instead of going negative and skipping every
        // one of them. A wrong depth must not make this parser quieter.
        depth = (depth + opens).saturating_sub(closes);
    }
    Ok(labels)
}

/// Remove every `{...}` pair that opens and closes within `line`, innermost
/// first, leaving only braces that carry block structure.
fn strip_inline_brace_pairs(line: &str) -> String {
    let mut out = line.to_owned();
    while let Some(close) = out.find('}') {
        let Some(open) = out[..close].rfind('{') else {
            break;
        };
        out.replace_range(open..=close, "");
    }
    out
}

/// The hostname label a single Caddy site address claims under the app domain,
/// or `None` for the wildcard catch-all.
fn site_address_label(address: &str) -> Result<Option<String>, String> {
    const DOMAIN_VAR: &str = ".{$ZEROSHIP_DOMAIN";
    // Every block carries an explicit scheme on purpose; the Caddyfile header
    // says why (`auto_https off` behind a TLS-terminating proxy). A scheme-less
    // address is a real Caddy site block and an ERROR here, not a skipped line.
    let Some(rest) = address
        .strip_prefix("http://")
        .or_else(|| address.strip_prefix("https://"))
    else {
        return Err(format!(
            "site address carries no explicit http:// or https:// scheme, which \
             this Caddyfile requires of every block: {address}"
        ));
    };
    let host = rest.split_whitespace().next().unwrap_or("");
    let Some((label, _)) = host.split_once(DOMAIN_VAR) else {
        return Err(format!(
            "site address is not built from {DOMAIN_VAR}}}, so its hostname \
             label cannot be checked against RESERVED_APP_NAMES: {address}"
        ));
    };
    if label.contains('.') {
        return Err(format!(
            "site address has a multi-label prefix; only a single hostname \
             label under the app domain is understood here: {address}"
        ));
    }
    if label == "*" {
        // The creator-app catch-all. Claims no name.
        return Ok(None);
    }
    if label.is_empty()
        || !label
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(format!(
            "site address has no usable hostname label: {address}"
        ));
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

    /// `include_str!` rather than a runtime read: the path is resolved at
    /// compile time, so moving or deleting the edge config breaks the build
    /// instead of quietly leaving this gate with nothing to check. It also
    /// makes editing the Caddyfile rebuild and rerun the gate.
    const CADDYFILE: &str = include_str!("../../../deploy/ops/Caddyfile");
    const COMPOSE: &str = include_str!("../../../deploy/compose/docker-compose.yml");

    /// THE DRIFT GATE. The reserved set and the edge's site blocks are the same
    /// set, checked in BOTH directions:
    ///
    /// - a site block whose label is not reserved means the edge claims a host
    ///   a creator can still register (the defect this module exists to close);
    /// - a reserved name no site block claims means the list over-reserves —
    ///   a name creators cannot have and nothing needs.
    #[test]
    fn reserved_set_matches_caddyfile() {
        let mut from_edge = caddy_site_labels(CADDYFILE).expect("Caddyfile site addresses parse");
        from_edge.sort_unstable();
        from_edge.dedup();
        let mut reserved: Vec<String> = RESERVED_APP_NAMES.iter().map(|s| (*s).to_owned()).collect();
        reserved.sort_unstable();
        assert_eq!(
            from_edge, reserved,
            "deploy/ops/Caddyfile and RESERVED_APP_NAMES disagree. Every \
             non-wildcard site block must have its label in RESERVED_APP_NAMES \
             and vice versa; update crates/control/src/reserved_names.rs in the \
             same change as the edge config."
        );
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

    /// The gate must go RED on a real edge change, not merely stay green on the
    /// current file. Same parser, one added site block.
    #[test]
    fn gate_rejects_a_site_block_the_reserved_set_lacks() {
        let mutated = format!(
            "{CADDYFILE}\nhttp://status.{{$ZEROSHIP_DOMAIN:zeroship.localhost}} {{\n\
             \treverse_proxy status:9099\n}}\n"
        );
        let labels = caddy_site_labels(&mutated).expect("mutated Caddyfile parses");
        assert!(labels.contains(&"status".to_owned()));
        assert!(
            labels.iter().any(|l| !is_reserved_app_name(l)),
            "an added site block must show up as an unreserved label"
        );
    }

    /// The other drift direction: a reserved name the edge stopped claiming.
    #[test]
    fn gate_rejects_a_reserved_name_the_edge_dropped() {
        let mutated: String = CADDYFILE
            .lines()
            .filter(|l| !l.trim_start().starts_with("http://control."))
            .collect::<Vec<_>>()
            .join("\n");
        let labels = caddy_site_labels(&mutated).expect("mutated Caddyfile parses");
        assert!(
            !labels.contains(&"control".to_owned()),
            "the mutation must actually remove the control site block"
        );
        assert!(
            RESERVED_APP_NAMES.contains(&"control"),
            "so the sets now disagree and reserved_set_matches_caddyfile fails"
        );
    }

    /// A site address the parser cannot read must be an error, never a silently
    /// skipped line — the literal domain the Caddyfile header forbids.
    #[test]
    fn literal_domain_site_block_is_an_error() {
        let err = caddy_site_labels("http://auth.zeroship.ai {\n\treverse_proxy auth:9092\n}\n")
            .expect_err("a literal-domain site address must not parse as no labels");
        assert!(err.contains("auth.zeroship.ai"), "{err}");
    }

    #[test]
    fn wildcard_block_claims_no_label() {
        let labels =
            caddy_site_labels("http://*.{$ZEROSHIP_DOMAIN:zeroship.localhost} {\n}\n").unwrap();
        assert!(labels.is_empty());
    }

    /// THE SHAPE THE PARSER HAS NEVER SEEN, and the one that decides whether
    /// strictness is real: Caddy accepts a site address with NO scheme, so
    /// `status.{$ZEROSHIP_DOMAIN} { ... }` is a live site block claiming a
    /// fifth host. It must be refused, because a parser that skips what it
    /// cannot read returns the same set for an edge that grew a host and leaves
    /// `reserved_set_matches_caddyfile` GREEN while the property it protects is
    /// broken. This is what the prefix-keyed parser did.
    ///
    /// Paired with the one-variable control below it: the SAME block with a
    /// scheme parses and yields the label, so the `Err` is evidence about the
    /// missing scheme and not about a parser that has started refusing
    /// everything appended to the file.
    #[test]
    fn scheme_less_site_block_is_an_error_not_a_skipped_line() {
        let mutated = format!(
            "{CADDYFILE}\nstatus.{{$ZEROSHIP_DOMAIN:zeroship.localhost}} {{\n\
             \treverse_proxy status:9099\n}}\n"
        );
        let err = caddy_site_labels(&mutated)
            .expect_err("a scheme-less site block must not parse as no new labels");
        assert!(err.contains("scheme"), "{err}");
    }

    /// The control for the test above, differing in ONE variable: the scheme.
    #[test]
    fn the_same_block_with_a_scheme_parses_and_claims_its_label() {
        let mutated = format!(
            "{CADDYFILE}\nhttp://status.{{$ZEROSHIP_DOMAIN:zeroship.localhost}} {{\n\
             \treverse_proxy status:9099\n}}\n"
        );
        let labels = caddy_site_labels(&mutated).expect("the scheme-carrying block parses");
        assert!(labels.contains(&"status".to_owned()));
    }

    /// Caddy lets one block carry several comma-separated addresses. EVERY one
    /// claims a host, so reading only the first is the same silent hole in
    /// another spelling.
    #[test]
    fn every_address_in_a_comma_separated_list_claims_its_label() {
        let labels = caddy_site_labels(
            "http://alpha.{$ZEROSHIP_DOMAIN:zeroship.localhost}, \
             http://beta.{$ZEROSHIP_DOMAIN:zeroship.localhost} {\n\
             \treverse_proxy gateway:8000\n}\n",
        )
        .expect("a comma-separated address list parses");
        assert_eq!(labels, vec!["alpha".to_owned(), "beta".to_owned()]);
    }

    /// A DIRECTIVE block nested in a site block opens a brace too, and claims
    /// no hostname. Recognising site blocks by position is only sound if depth
    /// is tracked through the `{$ZEROSHIP_DOMAIN:...}` placeholder braces, which
    /// are not block structure.
    #[test]
    fn nested_directive_blocks_claim_no_label() {
        let labels = caddy_site_labels(
            "http://alpha.{$ZEROSHIP_DOMAIN:zeroship.localhost} {\n\
             \thandle /oauth2/* {\n\t\treverse_proxy auth:9092\n\t}\n}\n\
             http://beta.{$ZEROSHIP_DOMAIN:zeroship.localhost} {\n\
             \treverse_proxy gateway:8000\n}\n",
        )
        .expect("nested directive blocks parse");
        assert_eq!(
            labels,
            vec!["alpha".to_owned(), "beta".to_owned()],
            "`handle` is a directive, not a site address; and the block after \
             the nesting must still be seen at top level"
        );
    }

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
