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
/// # Errors
///
/// Returns the offending line when a site address is not in the expected form.
pub fn caddy_site_labels(caddyfile: &str) -> Result<Vec<String>, String> {
    const DOMAIN_VAR: &str = ".{$ZEROSHIP_DOMAIN";
    let mut labels = Vec::new();
    for line in caddyfile.lines() {
        let line = line.trim();
        if line.starts_with('#') {
            continue;
        }
        // A site address is the only construct that opens with a scheme; the
        // Caddyfile header explains why every block carries an explicit one.
        let Some(rest) = line
            .strip_prefix("http://")
            .or_else(|| line.strip_prefix("https://"))
        else {
            continue;
        };
        let address = rest.split_whitespace().next().unwrap_or("");
        let Some((label, _)) = address.split_once(DOMAIN_VAR) else {
            return Err(format!(
                "site address is not built from {DOMAIN_VAR}}}, so its hostname \
                 label cannot be checked against RESERVED_APP_NAMES: {line}"
            ));
        };
        if label.contains('.') {
            return Err(format!(
                "site address has a multi-label prefix; only a single hostname \
                 label under the app domain is understood here: {line}"
            ));
        }
        if label == "*" {
            // The creator-app catch-all. Claims no name.
            continue;
        }
        if label.is_empty()
            || !label
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            return Err(format!("site address has no usable hostname label: {line}"));
        }
        labels.push(label.to_owned());
    }
    Ok(labels)
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
