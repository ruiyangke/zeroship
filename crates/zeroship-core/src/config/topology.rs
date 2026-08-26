//! Public-origin topology settings shared by control and gateway.

use std::fmt;
use std::str::FromStr;

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

#[cfg(test)]
mod tests {
    use super::{resolve_origin_scheme, resolve_trusted_origins, OriginScheme, TrustedOrigin};

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
}
