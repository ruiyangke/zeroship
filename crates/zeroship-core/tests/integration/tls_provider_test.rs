//! `install_process_crypto_provider`, the step every binary runs at boot. That
//! the service boot path runs it is tested through a real declaration in
//! `crates/zeroship-data-cdc-server/tests/e2e/boot.rs`.
//!
//! The process default can be installed once, so the real case is ignored in
//! the shared run and executed alone in a child copy of this binary by its
//! spawner; no other case ever observes the provider it installs. The order of
//! its steps is part of what it checks.

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

/// Run the ignored case alone in a child copy of this binary.
fn passes_alone(case: &str) {
    // `module_path!()` prefixes the crate name; libtest names a case by its
    // module path without that prefix, so drop the first segment.
    let module = module_path!();
    let module = module.split_once("::").map_or(module, |(_, path)| path);
    let name = format!("{module}::{case}");
    let out = std::process::Command::new(std::env::current_exe().expect("current_exe"))
        .args(["--exact", &name, "--ignored", "--nocapture", "--test-threads=1"])
        .output()
        .expect("spawn a child copy of this test binary");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "{name} failed alone:\n{text}");
    assert!(
        text.contains("1 passed"),
        "the child ran no test, so it proved nothing:\n{text}"
    );
}

#[test]
#[ignore = "installs a process-wide default; its spawner runs it alone in a child"]
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

#[test]
fn install_sets_aws_lc_rs_that_default_built_configs_use_in_an_isolated_process() {
    passes_alone("install_sets_aws_lc_rs_that_default_built_configs_use_and_refuses_a_second_install");
}
