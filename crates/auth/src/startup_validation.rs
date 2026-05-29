//! Fatal boot-time config validation.

use zeroship_core::config::is_loopback_url;

use crate::config::AuthConfig;

pub fn validate_hydra_admin_url(cfg: &AuthConfig) -> Result<(), String> {
    let hydra_admin_url = cfg.hydra_admin_url();

    // Literal-only loopback check (S8): a hostname that merely *resolves* to
    // loopback is rejected, closing the DNS-rebind / TOCTOU window the old
    // `to_socket_addrs` check left open. Remote admin needs the explicit
    // opt-in below.
    if is_loopback_url(hydra_admin_url) {
        return Ok(());
    }

    if cfg.allow_remote_hydra_admin {
        tracing::warn!(
            hydra_admin_url = %hydra_admin_url,
            "hydra admin is reachable over the network -- ensure mTLS / firewall is configured externally"
        );
        return Ok(());
    }

    Err(format!(
        "HYDRA_ADMIN_URL points to a non-loopback host ({hydra_admin_url}); refusing to boot. \
         Keep Hydra admin on 127.0.0.1/::1/localhost or set --allow-remote-hydra-admin=true \
         only when mTLS / firewall protection is configured externally.",
    ))
}

#[cfg(test)]
mod tests {
    use clap::Parser;
    use zeroship_core::config::AuthSection;

    use super::*;

    fn test_config() -> AuthConfig {
        AuthConfig::parse_from(["zeroship-auth", "--db-url", "postgres://test"])
    }

    #[test]
    fn loopback_hydra_admin_accepted_without_flag() {
        let mut cfg = test_config();
        cfg.hydra_admin_url = Some("http://127.0.0.1:4445".to_string());
        cfg.allow_remote_hydra_admin = false;

        assert!(validate_hydra_admin_url(&cfg).is_ok());
    }

    #[test]
    fn loopback_hostname_accepted_without_flag() {
        let mut cfg = test_config();
        cfg.hydra_admin_url = Some("http://localhost:4445".to_string());
        cfg.allow_remote_hydra_admin = false;

        assert!(validate_hydra_admin_url(&cfg).is_ok());
    }

    #[test]
    fn remote_hydra_admin_rejected_without_flag() {
        let mut cfg = test_config();
        cfg.hydra_admin_url = Some("http://hydra.example.com:4445".to_string());
        cfg.allow_remote_hydra_admin = false;

        assert!(validate_hydra_admin_url(&cfg).is_err());
    }

    #[test]
    fn remote_hydra_admin_from_file_overlay_rejected_without_flag() {
        let mut cfg = test_config();
        // `hydra_admin_url` is a clap `env = "HYDRA_ADMIN_URL"` arg, so
        // `parse_from` in `test_config` picks up any ambient value (the auth
        // integration suite runs with HYDRA_ADMIN_URL set to a loopback). This
        // test asserts the *file overlay* path, so the CLI/env source must be
        // empty — otherwise `resolve`'s `cli.or(file)` precedence lets the
        // ambient loopback win and validation wrongly passes. Clear it so the
        // overlay is the only source.
        cfg.hydra_admin_url = None;
        cfg.resolve(AuthSection {
            hydra_admin_url: Some("http://hydra.example.com:4445".to_string()),
            ..AuthSection::default()
        });
        cfg.allow_remote_hydra_admin = false;

        assert!(validate_hydra_admin_url(&cfg).is_err());
    }

    #[test]
    fn remote_hydra_admin_accepted_with_flag() {
        let mut cfg = test_config();
        cfg.hydra_admin_url = Some("http://hydra.example.com:4445".to_string());
        cfg.allow_remote_hydra_admin = true;

        assert!(validate_hydra_admin_url(&cfg).is_ok());
    }

    // (c) S8 regression: a DNS *name* that resolves to loopback must be
    // REJECTED under the literal-only guard (no DNS resolution), unless the
    // operator explicitly opts in with --allow-remote-hydra-admin.
    #[test]
    fn dns_name_resolving_to_loopback_rejected_without_flag() {
        let mut cfg = test_config();
        // `localhost.localdomain` and nip.io-style names resolve to 127.0.0.1
        // on many hosts, but are NOT literal loopback — the old
        // to_socket_addrs check accepted them; the literal-only guard rejects.
        cfg.hydra_admin_url = Some("http://localhost.localdomain:4445".to_string());
        cfg.allow_remote_hydra_admin = false;

        assert!(
            validate_hydra_admin_url(&cfg).is_err(),
            "a DNS name (even one resolving to loopback) must be rejected literal-only"
        );

        // The explicit opt-in still lets it through.
        cfg.allow_remote_hydra_admin = true;
        assert!(validate_hydra_admin_url(&cfg).is_ok());
    }
}
