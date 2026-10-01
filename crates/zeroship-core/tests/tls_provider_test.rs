//! `install_process_crypto_provider`, the step every binary runs at boot. That
//! the service boot path runs it is tested through a real declaration in
//! `crates/zeroship-data-cdc-server/tests/boot.rs`.
//!
//! The process default can be installed once, so the whole contract is one
//! test in its own binary, and the order of its steps is part of what it
//! checks.

use rustls::crypto::CryptoProvider;
use zeroship_core::tls::install_process_crypto_provider;

fn suites(provider: &CryptoProvider) -> Vec<rustls::CipherSuite> {
    provider
        .cipher_suites
        .iter()
        .map(rustls::SupportedCipherSuite::suite)
        .collect()
}

fn groups(provider: &CryptoProvider) -> Vec<rustls::NamedGroup> {
    provider
        .kx_groups
        .iter()
        .map(|group| group.name())
        .collect()
}

#[test]
fn install_sets_aws_lc_rs_that_default_built_configs_use_and_refuses_a_second_install() {
    assert!(
        CryptoProvider::get_default().is_none(),
        "a provider was installed before the install step ran"
    );

    install_process_crypto_provider();

    let installed = CryptoProvider::get_default().expect("the install step installed a provider");
    let aws_lc_rs = rustls::crypto::aws_lc_rs::default_provider();
    assert!(!suites(&aws_lc_rs).is_empty() && !groups(&aws_lc_rs).is_empty());
    assert_eq!(suites(installed), suites(&aws_lc_rs));
    assert_eq!(groups(installed), groups(&aws_lc_rs));

    // What cyper and compio-ws do when a caller hands them no config.
    let config = rustls::ClientConfig::builder()
        .with_root_certificates(rustls::RootCertStore::empty())
        .with_no_client_auth();
    assert!(std::sync::Arc::ptr_eq(config.crypto_provider(), installed));

    assert!(
        std::panic::catch_unwind(install_process_crypto_provider).is_err(),
        "a second install was accepted silently"
    );
}
