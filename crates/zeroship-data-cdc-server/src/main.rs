//! PostgreSQL CDC relay process.

use clap::Parser;
use zeroship_core::config::{bootstrap_or_exit, CheckConfigReport, CheckValue};
use zeroship_data_cdc_server::config::{
    CdcServerSettings, CdcServerSettingsSources, DEFAULT_LOG_FILTER,
};

mod auth;
mod hub;
mod server;
mod source;
mod transaction;

#[cfg(test)]
#[path = "../../../tests/fixtures/postgres/mod.rs"]
mod postgres_fixture;

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

    if let Err(error) = server::run(settings).await {
        tracing::error!(%error, "CDC relay stopped");
        std::process::exit(1);
    }
}
