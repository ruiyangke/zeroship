//! The relay's boot path installs the process's rustls crypto provider.
//!
//! This drives the call the relay's `main` makes, `bootstrap_or_exit` over its
//! own declaration, as a dry run so no database or listener is needed. The
//! provider is process state that can be installed once and the boot also
//! installs the global tracing subscriber, so the case is ignored in the shared
//! run and executed alone in a child copy of this binary by its spawner.

use clap::Parser;
use rustls::crypto::CryptoProvider;
use zeroship_core::config::bootstrap_or_exit;
use zeroship_data_cdc_server::config::{
    CdcServerSettings, CdcServerSettingsSources, DEFAULT_LOG_FILTER,
};

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

/// Run one ignored case of this module alone in a child copy of this binary.
fn passes_alone(case: &str) {
    let name = format!("e2e::boot::{case}");
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
#[ignore = "installs a process-wide crypto provider; its spawner runs it alone in a child"]
fn the_service_boot_path_installs_aws_lc_rs_as_the_process_default() {
    assert!(
        CryptoProvider::get_default().is_none(),
        "a provider was installed before the boot path ran"
    );

    let sources = CdcServerSettingsSources::try_parse_from([
        "zeroship-data-cdc-server",
        "--no-config",
        "--check-config",
    ])
    .expect("dry-run controls parse");
    let (settings, _boot) =
        bootstrap_or_exit::<CdcServerSettings>(sources, DEFAULT_LOG_FILTER, "data-cdc-server");
    assert!(*settings.check_config.get(), "the boot resolved a dry run");

    let installed = CryptoProvider::get_default().expect("the boot path installed a provider");
    let aws_lc_rs = rustls::crypto::aws_lc_rs::default_provider();
    assert!(!suites(&aws_lc_rs).is_empty() && !groups(&aws_lc_rs).is_empty());
    assert_eq!(suites(installed), suites(&aws_lc_rs));
    assert_eq!(groups(installed), groups(&aws_lc_rs));
}

#[test]
fn the_service_boot_path_installs_aws_lc_rs_as_the_process_default_in_an_isolated_process() {
    passes_alone("the_service_boot_path_installs_aws_lc_rs_as_the_process_default");
}
