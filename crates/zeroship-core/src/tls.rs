//! The process's rustls crypto provider.
//!
//! rustls resolves a process-level default the first time anything calls
//! `ClientConfig::builder()` or `ServerConfig::builder()` without naming a
//! provider, and cyper and compio-ws each build their own config that way when
//! a caller hands them none. With nothing installed, rustls infers the
//! default from its crate features, which describe whatever that one build
//! unified rather than what the platform chose. A binary whose only paths to
//! rustls name no provider compiles none; a build that unifies `ring` beside
//! `aws-lc-rs` compiles two and rustls refuses to pick. Either way the panic
//! comes at the first outbound TLS connection, not at boot.
//!
//! So every binary installs the platform provider before it builds a client.
//! The service binaries do it through [`crate::config::bootstrap_or_exit`],
//! which each of their `main`s calls once its command line is parsed and
//! before anything else runs; the binaries that do not boot through it call
//! [`install_process_crypto_provider`] from `main` directly. Connectors this
//! workspace builds itself do not read the default at all; they name aws-lc-rs.

/// Install aws-lc-rs as this process's default rustls crypto provider.
///
/// # Panics
///
/// When a default is already installed. The boot sequence runs before anything
/// that could install one, so a default already present means some earlier
/// code chose a provider, and every client built afterwards would use that
/// choice without saying so.
pub fn install_process_crypto_provider() {
    let installed = rustls::crypto::aws_lc_rs::default_provider().install_default();
    assert!(
        installed.is_ok(),
        "a rustls CryptoProvider was installed before the boot sequence installed aws-lc-rs"
    );
}
