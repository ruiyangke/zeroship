//! Runtime deployment and connection configuration, independent of the executor.

use crate::{Error, Result};
use serde::Deserialize;
use std::path::PathBuf;

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RedisConfig {
    pub topology: Topology,
    #[serde(default)]
    pub auth: Auth,
    pub tls: Option<TlsConfig>,
    #[serde(default)]
    pub sentinel_auth: Auth,
    pub sentinel_tls: Option<TlsConfig>,
    #[serde(default)]
    pub database: u32,
    #[serde(default)]
    pub timeouts: Timeouts,
    #[serde(default)]
    pub pool: PoolSettings,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum Topology {
    Standalone {
        endpoint: String,
    },
    Cluster {
        seeds: Vec<String>,
    },
    Sentinel {
        endpoints: Vec<String>,
        service_name: String,
    },
}

#[derive(Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Auth {
    pub username: Option<String>,
    pub password: Option<String>,
}

impl std::fmt::Debug for Auth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Auth").finish_non_exhaustive()
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TlsConfig {
    pub ca_file: Option<PathBuf>,
    pub cert_file: Option<PathBuf>,
    pub key_file: Option<PathBuf>,
    /// Certificate identity when discovery returns an address instead of a DNS name.
    pub server_name: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Timeouts {
    pub connect_ms: u64,
    pub command_ms: u64,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            connect_ms: 5_000,
            command_ms: 5_000,
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PoolSettings {
    pub max_size: usize,
    pub min_idle: usize,
    pub idle_timeout_ms: u64,
    pub liveness_probe_after_ms: u64,
}

impl Default for PoolSettings {
    fn default() -> Self {
        Self {
            max_size: 16,
            min_idle: 1,
            idle_timeout_ms: 600_000,
            liveness_probe_after_ms: 30_000,
        }
    }
}

impl RedisConfig {
    pub fn new(topology: Topology) -> Self {
        Self {
            topology,
            auth: Auth::default(),
            tls: None,
            sentinel_auth: Auth::default(),
            sentinel_tls: None,
            database: 0,
            timeouts: Timeouts::default(),
            pool: PoolSettings::default(),
        }
    }

    pub fn validate(&self) -> Result<()> {
        let endpoints = match &self.topology {
            Topology::Standalone { endpoint } => std::slice::from_ref(endpoint),
            Topology::Cluster { seeds } => {
                if self.database != 0 {
                    return Err(Error::Config("cluster requires database zero".into()));
                }
                seeds.as_slice()
            }
            Topology::Sentinel {
                endpoints,
                service_name,
            } => {
                if service_name.trim().is_empty() {
                    return Err(Error::Config("Sentinel service_name is required".into()));
                }
                endpoints.as_slice()
            }
        };
        if endpoints.is_empty() {
            return Err(Error::Config("deployment endpoints cannot be empty".into()));
        }
        for endpoint in endpoints {
            parse_endpoint(endpoint)?;
        }
        for auth in [&self.auth, &self.sentinel_auth] {
            if auth.username.is_some() && auth.password.is_none() {
                return Err(Error::Config("an ACL username requires a password".into()));
            }
        }
        for tls in [&self.tls, &self.sentinel_tls].into_iter().flatten() {
            if tls.cert_file.is_some() != tls.key_file.is_some() {
                return Err(Error::Config(
                    "TLS client certificate and key must be supplied together".into(),
                ));
            }
        }
        if !matches!(self.topology, Topology::Sentinel { .. })
            && (self.sentinel_tls.is_some()
                || self.sentinel_auth.username.is_some()
                || self.sentinel_auth.password.is_some())
        {
            return Err(Error::Config(
                "Sentinel connection settings require Sentinel topology".into(),
            ));
        }
        if self.timeouts.connect_ms == 0
            || self.timeouts.command_ms == 0
            || self.pool.max_size == 0
            || self.pool.min_idle > self.pool.max_size
        {
            return Err(Error::Config(
                "invalid connection timeout or pool limits".into(),
            ));
        }
        Ok(())
    }

    pub fn connection(&self, endpoint: String) -> ConnectionConfig {
        ConnectionConfig {
            endpoint,
            auth: self.auth.clone(),
            tls: self.tls.clone(),
            database: self.database,
            timeouts: self.timeouts.clone(),
        }
    }
}

/// Settings inherited by every discovered data-server connection.
#[derive(Clone, Debug)]
pub struct ConnectionConfig {
    pub endpoint: String,
    pub auth: Auth,
    pub tls: Option<TlsConfig>,
    pub database: u32,
    pub timeouts: Timeouts,
}

impl ConnectionConfig {
    pub fn from_url(input: &str) -> Result<Self> {
        let url = url::Url::parse(input).map_err(|_| Error::Config("invalid Redis URL".into()))?;
        if !matches!(url.scheme(), "redis" | "rediss")
            || url.host().is_none()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(Error::Config(
                "expected redis:// or rediss:// connection URL without query or fragment".into(),
            ));
        }
        let host = url.host_str().unwrap();
        let port = url.port().unwrap_or(6379);
        let decode = |s: &str| {
            percent_encoding::percent_decode_str(s)
                .decode_utf8()
                .map(|s| s.into_owned())
                .map_err(|_| Error::Config("credentials must be UTF-8".into()))
        };
        let path = url.path().trim_start_matches('/');
        Ok(Self {
            endpoint: format!("{host}:{port}"),
            auth: Auth {
                username: if url.username().is_empty() {
                    None
                } else {
                    Some(decode(url.username())?)
                },
                password: url.password().map(decode).transpose()?,
            },
            tls: (url.scheme() == "rediss").then(TlsConfig::default),
            database: if path.is_empty() {
                0
            } else {
                path.parse()
                    .map_err(|_| Error::Config("invalid Redis database".into()))?
            },
            timeouts: Timeouts::default(),
        })
    }
}

pub(crate) fn parse_endpoint(endpoint: &str) -> Result<(String, u16)> {
    let invalid =
        || Error::Config("endpoint must be host:port without credentials, path or query".into());
    if endpoint.chars().any(char::is_whitespace) {
        return Err(invalid());
    }
    let url = url::Url::parse(&format!("redis://{endpoint}")).map_err(|_| invalid())?;
    if !url.username().is_empty()
        || url.password().is_some()
        || !url.path().is_empty()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(invalid());
    }
    let host = url
        .host_str()
        .ok_or_else(invalid)?
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_owned();
    let port = url.port().filter(|p| *p != 0).ok_or_else(invalid)?;
    Ok((host, port))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoints_accept_dns_and_bracketed_ipv6() {
        assert_eq!(
            parse_endpoint("redis.internal:6379").unwrap(),
            ("redis.internal".into(), 6379)
        );
        assert_eq!(parse_endpoint("[::1]:6380").unwrap(), ("::1".into(), 6380));
        for endpoint in [
            "redis",
            "redis:0",
            "redis:6379/path",
            "redis:6379?cluster=true",
            "user:secret@redis:6379",
        ] {
            let error = parse_endpoint(endpoint).unwrap_err();
            assert!(!error.to_string().contains("secret"));
        }
    }

    #[test]
    fn direct_urls_decode_credentials_and_preserve_tls_and_database() {
        let config = ConnectionConfig::from_url("rediss://service:p%40ss@[::1]:6380/2").unwrap();
        assert_eq!(config.endpoint, "[::1]:6380");
        assert_eq!(config.auth.username.as_deref(), Some("service"));
        assert_eq!(config.auth.password.as_deref(), Some("p@ss"));
        assert_eq!(config.database, 2);
        assert!(config.tls.is_some());
        assert!(!format!("{config:?}").contains("p@ss"));
        for url in [
            "redis://localhost/not-a-database",
            "redis://localhost?cluster=true",
            "https://user:secret@localhost",
        ] {
            assert!(
                !ConnectionConfig::from_url(url)
                    .unwrap_err()
                    .to_string()
                    .contains("secret")
            );
        }
    }
}
