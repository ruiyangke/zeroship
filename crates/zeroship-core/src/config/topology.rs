//! Public-origin topology settings shared by control and gateway.

use std::fmt;
use std::str::FromStr;
use std::sync::Arc;

use clap::ValueEnum;
use serde::Deserialize;

/// Scheme used in browser-visible URLs and same-origin comparisons.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum OriginScheme {
    /// Plain HTTP, normally for localhost development.
    Http,
    /// HTTPS, including deployments where a trusted edge terminates TLS.
    #[default]
    Https,
}

impl OriginScheme {
    /// Return the URL-scheme spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Http => "http",
            Self::Https => "https",
        }
    }
}

impl fmt::Display for OriginScheme {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A validated, canonical HTTP(S) origin accepted by a same-origin guard.
///
/// This is an origin, not a general URL: credentials, non-root paths, query
/// strings, fragments, wildcards, and `null` are rejected.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize)]
#[serde(try_from = "String")]
pub struct TrustedOrigin(String);

impl TrustedOrigin {
    /// Return the canonical `scheme://host[:port]` spelling.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for TrustedOrigin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for TrustedOrigin {
    type Err = String;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        if raw.is_empty() || raw.trim() != raw {
            return Err(
                "trusted origin must be non-empty and have no surrounding whitespace".into(),
            );
        }
        if raw == "null" || raw.contains('*') {
            return Err(
                "trusted origin must be an exact HTTP(S) origin, not null or a wildcard".into(),
            );
        }

        let url = url::Url::parse(raw)
            .map_err(|error| format!("invalid trusted origin {raw:?}: {error}"))?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err("trusted origin scheme must be http or https".into());
        }
        if url.host().is_none() {
            return Err("trusted origin must include a host".into());
        }
        if !url.username().is_empty() || url.password().is_some() {
            return Err("trusted origin must not include credentials".into());
        }
        if url.path() != "/" || url.query().is_some() || url.fragment().is_some() {
            return Err("trusted origin must not include a path, query, or fragment".into());
        }

        let canonical = url.origin().ascii_serialization();
        Ok(Self(canonical))
    }
}

impl TryFrom<String> for TrustedOrigin {
    type Error = String;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

/// Resolve the clap-merged CLI/environment tier over the optional file tier.
#[must_use]
pub fn resolve_origin_scheme(
    cli_or_env: Option<OriginScheme>,
    file: Option<OriginScheme>,
) -> OriginScheme {
    cli_or_env.or(file).unwrap_or_default()
}

/// Resolve the clap-merged CLI/environment tier over the optional file tier.
#[must_use]
pub fn resolve_trusted_origins(
    cli_or_env: Option<Vec<TrustedOrigin>>,
    file: Option<Vec<TrustedOrigin>>,
) -> Vec<TrustedOrigin> {
    cli_or_env.or(file).unwrap_or_default()
}

/// One platform peer an operator has authorized plaintext service calls to.
///
/// An EXACT `http://host[:port]` origin, never a host, a range or a pattern.
/// The port is part of it because one private host serves several platform
/// services on different ports, and naming the host would admit all of them.
///
/// Nothing here resolves a name or inspects an address. A peer is admitted
/// because an operator wrote it down, so a name that resolves somewhere else
/// tomorrow cannot silently move the fence.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize)]
#[serde(try_from = "String")]
pub struct PlaintextPeer(String);

impl PlaintextPeer {
    /// Return the canonical `http://host[:port]` spelling.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for PlaintextPeer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for PlaintextPeer {
    type Err = String;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        if raw.is_empty() || raw.trim() != raw {
            return Err(
                "plaintext peer must be non-empty and have no surrounding whitespace".into(),
            );
        }
        if raw.contains('*') {
            return Err("plaintext peer must be an exact origin, not a wildcard".into());
        }
        let url = url::Url::parse(raw)
            .map_err(|error| format!("invalid plaintext peer {raw:?}: {error}"))?;
        // An `https` entry would authorize nothing - https needs no
        // authorization - so accepting one would let an operator believe a peer
        // was listed when the list is about plaintext alone.
        if url.scheme() != "http" {
            return Err("plaintext peer scheme must be http".into());
        }
        if url.host().is_none() {
            return Err("plaintext peer must include a host".into());
        }
        if !url.username().is_empty() || url.password().is_some() {
            return Err("plaintext peer must not include credentials".into());
        }
        if url.path() != "/" || url.query().is_some() || url.fragment().is_some() {
            return Err("plaintext peer must not include a path, query, or fragment".into());
        }
        Ok(Self(url.origin().ascii_serialization()))
    }
}

impl TryFrom<String> for PlaintextPeer {
    type Error = String;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

/// The peers one PROCESS may reach over plaintext HTTP.
///
/// Empty by default, which is the whole deployment that configures nothing.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PlaintextPeers(Arc<[PlaintextPeer]>);

impl PlaintextPeers {
    /// True when no peer is named, which is the default posture.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The named peers, in the order the operator wrote them.
    pub fn iter(&self) -> impl Iterator<Item = &PlaintextPeer> {
        self.0.iter()
    }

    /// The canonical spellings joined for a configuration report or a log line.
    #[must_use]
    pub fn joined(&self) -> String {
        self.0
            .iter()
            .map(PlaintextPeer::as_str)
            .collect::<Vec<_>>()
            .join(",")
    }

    /// True when `url`'s origin is one the operator named.
    ///
    /// Comparison is on the canonical origin serialization, so scheme, host and
    /// port must all agree. A host match with a different port is not a match.
    #[must_use]
    pub fn admits(&self, url: &url::Url) -> bool {
        let origin = url.origin().ascii_serialization();
        self.0.iter().any(|peer| peer.0 == origin)
    }
}

impl FromIterator<PlaintextPeer> for PlaintextPeers {
    fn from_iter<I: IntoIterator<Item = PlaintextPeer>>(peers: I) -> Self {
        Self(peers.into_iter().collect())
    }
}

impl From<Vec<PlaintextPeer>> for PlaintextPeers {
    fn from(peers: Vec<PlaintextPeer>) -> Self {
        Self(peers.into())
    }
}

#[cfg(test)]
mod tests {
    use super::{
        resolve_origin_scheme, resolve_trusted_origins, OriginScheme, PlaintextPeer,
        PlaintextPeers, TrustedOrigin,
    };

    fn origin(value: &str) -> TrustedOrigin {
        value.parse().expect("valid origin")
    }

    #[test]
    fn topology_precedence_is_cli_env_then_file_then_default() {
        assert_eq!(
            resolve_origin_scheme(Some(OriginScheme::Http), Some(OriginScheme::Https)),
            OriginScheme::Http
        );
        assert_eq!(
            resolve_origin_scheme(None, Some(OriginScheme::Http)),
            OriginScheme::Http
        );
        assert_eq!(resolve_origin_scheme(None, None), OriginScheme::Https);

        let high = vec![origin("https://cli.example")];
        let file = vec![origin("https://file.example")];
        assert_eq!(
            resolve_trusted_origins(Some(high.clone()), Some(file.clone())),
            high
        );
        assert_eq!(resolve_trusted_origins(None, Some(file.clone())), file);
        assert!(resolve_trusted_origins(None, None).is_empty());
    }

    #[test]
    fn trusted_origin_normalizes_exact_http_origins() {
        assert_eq!(
            origin("HTTPS://Example.COM:443/").as_str(),
            "https://example.com"
        );
        assert_eq!(
            origin("http://localhost:3000").as_str(),
            "http://localhost:3000"
        );
    }

    #[test]
    fn trusted_origin_rejects_non_origins() {
        for value in [
            "",
            "null",
            "*",
            "https://*.example.com",
            "ftp://example.com",
            "https://user@example.com",
            "https://example.com/path",
            "https://example.com?query=1",
            "https://example.com#fragment",
        ] {
            assert!(
                value.parse::<TrustedOrigin>().is_err(),
                "{value:?} must not parse as a trusted origin"
            );
        }
    }

    fn peer(value: &str) -> PlaintextPeer {
        value.parse().expect("valid plaintext peer")
    }

    #[test]
    fn a_plaintext_peer_normalizes_exact_http_origins() {
        assert_eq!(peer("HTTP://Control:9090/").as_str(), "http://control:9090");
        assert_eq!(peer("http://control").as_str(), "http://control");
        assert_eq!(peer("http://127.0.0.1:9095").as_str(), "http://127.0.0.1:9095");
    }

    #[test]
    fn a_plaintext_peer_rejects_everything_that_is_not_one_exact_http_origin() {
        for value in [
            "",
            " http://control:9090",
            "*",
            "http://*.control",
            // https authorizes nothing here, so accepting it would let an
            // operator believe a peer was listed when the list is about
            // plaintext alone.
            "https://control:9090",
            "ftp://control:9090",
            "control:9090",
            "http://user:secret@control:9090",
            "http://control:9090/prefix",
            "http://control:9090/?tenant=a",
            "http://control:9090/#fragment",
        ] {
            assert!(
                value.parse::<PlaintextPeer>().is_err(),
                "{value:?} must not parse as a plaintext peer"
            );
        }
    }

    #[test]
    fn a_named_peer_is_admitted_and_its_neighbours_on_the_same_host_are_not() {
        let peers = PlaintextPeers::from(vec![peer("http://control:9090")]);
        let admits = |raw: &str| peers.admits(&url::Url::parse(raw).expect("a URL"));

        assert!(admits("http://control:9090"));
        assert!(admits("http://control:9090/"));
        // The port is part of the identity: one private host serves several
        // platform services, and naming the host would admit all of them.
        assert!(!admits("http://control:9091"));
        assert!(!admits("http://control"));
        assert!(!admits("http://migrate-server:9090"));
        // A scheme change is a different origin, not a stronger one to reuse.
        assert!(!admits("https://control:9090"));
        // The default admits nothing at all.
        assert!(PlaintextPeers::default().is_empty());
        assert!(!PlaintextPeers::default().admits(&url::Url::parse("http://control:9090").unwrap()));
    }

    #[test]
    fn the_report_spelling_lists_every_named_peer() {
        let peers = PlaintextPeers::from(vec![peer("http://control:9090"), peer("http://mig:9091")]);
        assert_eq!(peers.joined(), "http://control:9090,http://mig:9091");
        assert_eq!(peers.iter().count(), 2);
        assert_eq!(PlaintextPeers::default().joined(), "");
    }
}
