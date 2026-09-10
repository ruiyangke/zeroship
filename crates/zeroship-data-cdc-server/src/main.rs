//! zeroship-data-cdc-server - the CDC relay process.
//!
//! It resolves configuration, answers `--check-config` honestly, and then
//! refuses to start. Change decoding still runs inside the worker, so a relay
//! that idled here would be a second process claiming an authority it does not
//! hold - and the worker's local-emit suppression is keyed on the decode loop
//! actually running, not on a relay existing. Failing closed is the only honest
//! state until `crates/zeroship-data-v8/src/wal_consumer.rs`'s loop is
//! rewritten against the wire, which `src/lib.rs` records as blocked on two
//! things outside this crate.
//!
//! `--check-config` is the one thing it can do honestly, and it does it:
//! validating and printing configuration touches nothing.

use clap::Parser;
use zeroship_core::config::{bootstrap_or_exit, CheckConfigReport, CheckValue};
use zeroship_data_cdc_server::config::{
    CdcServerSettings, CdcServerSettingsSources, DEFAULT_LOG_FILTER,
};
use zeroship_data_cdc_server::RELAY_UNAVAILABLE;

#[compio::main]
async fn main() {
    let (settings, boot) = bootstrap_or_exit::<CdcServerSettings>(
        CdcServerSettingsSources::parse(),
        DEFAULT_LOG_FILTER,
        "data-cdc-server",
    );

    if *settings.check_config.get() {
        let mut report = CheckConfigReport::new();
        report.field(
            "config_source",
            CheckValue::Plain(boot.overlay.source.to_string()),
        );
        report.field("log_filter", CheckValue::Plain(boot.log_filter.clone()));
        report.field("log_format", CheckValue::Plain(boot.log_format.to_string()));
        report.field(
            "db_configured",
            CheckValue::Secret(settings.database_url.is_configured()),
        );
        report.emit(*settings.check_config_format.get());
        return;
    }

    tracing::error!(
        db_configured = settings.database_url.is_configured(),
        RELAY_UNAVAILABLE
    );
    eprintln!("{RELAY_UNAVAILABLE}");
    std::process::exit(1);
}
