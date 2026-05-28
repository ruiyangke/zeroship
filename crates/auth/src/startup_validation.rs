//! Fatal boot-time config validation.

use std::net::{IpAddr, ToSocketAddrs};

use crate::config::AuthConfig;

pub fn validate_hydra_admin_url(cfg: &AuthConfig) -> Result<(), String> {
    if hydra_admin_host_is_loopback(&cfg.hydra_admin)? {
        return Ok(());
    }

    if cfg.allow_remote_hydra_admin {
        tracing::warn!(
            hydra_admin = %cfg.hydra_admin,
            "hydra admin is reachable over the network -- ensure mTLS / firewall is configured externally"
        );
        return Ok(());
    }

    Err(format!(
        "AUTH_HYDRA_ADMIN points to a non-loopback host ({url}); refusing to boot. \
         Keep Hydra admin on 127.0.0.1/::1/localhost or set --allow-remote-hydra-admin=true \
         only when mTLS / firewall protection is configured externally.",
        url = cfg.hydra_admin,
    ))
}

fn hydra_admin_host_is_loopback(raw_url: &str) -> Result<bool, String> {
    let url = url::Url::parse(raw_url)
        .map_err(|e| format!("AUTH_HYDRA_ADMIN must be a valid URL: {e}"))?;
    let Some(host) = url.host_str() else {
        return Err("AUTH_HYDRA_ADMIN must include a host".to_string());
    };

    if host.eq_ignore_ascii_case("localhost") {
        return Ok(true);
    }

    if let Ok(ip) = host.parse::<IpAddr>() {
        return Ok(ip.is_loopback());
    }

    let Some(port) = url.port_or_known_default() else {
        return Ok(false);
    };

    match (host, port).to_socket_addrs() {
        Ok(addrs) => {
            let mut saw_addr = false;
            let all_loopback = addrs
                .inspect(|_| saw_addr = true)
                .all(|addr| addr.ip().is_loopback());
            Ok(saw_addr && all_loopback)
        }
        Err(_) => Ok(false),
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;

    fn test_config() -> AuthConfig {
        AuthConfig::parse_from(["zeroship-auth", "--db-url", "postgres://test"])
    }

    #[test]
    fn loopback_hydra_admin_accepted_without_flag() {
        let mut cfg = test_config();
        cfg.hydra_admin = "http://127.0.0.1:4445".to_string();
        cfg.allow_remote_hydra_admin = false;

        assert!(validate_hydra_admin_url(&cfg).is_ok());
    }

    #[test]
    fn loopback_hostname_accepted_without_flag() {
        let mut cfg = test_config();
        cfg.hydra_admin = "http://localhost:4445".to_string();
        cfg.allow_remote_hydra_admin = false;

        assert!(validate_hydra_admin_url(&cfg).is_ok());
    }

    #[test]
    fn remote_hydra_admin_rejected_without_flag() {
        let mut cfg = test_config();
        cfg.hydra_admin = "http://hydra.example.com:4445".to_string();
        cfg.allow_remote_hydra_admin = false;

        assert!(validate_hydra_admin_url(&cfg).is_err());
    }

    #[test]
    fn remote_hydra_admin_accepted_with_flag() {
        let mut cfg = test_config();
        cfg.hydra_admin = "http://hydra.example.com:4445".to_string();
        cfg.allow_remote_hydra_admin = true;

        assert!(validate_hydra_admin_url(&cfg).is_ok());
    }
}
