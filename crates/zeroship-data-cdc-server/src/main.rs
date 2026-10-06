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
use zeroship_data_testkit::data::platform as platform_fixture;

fn main() {
    // Probe at the boundary that the kernel grants a ring; a refusal names
    // `RLIMIT_MEMLOCK`, the values and the remedy. The runtime this process
    // serves on is still mapped through `explain` below, because a later ring
    // can be refused after the probe succeeded.
    zeroship_memlock::prepare_or_exit("data-cdc-server");
    let runtime = match compio::runtime::Runtime::new().map_err(zeroship_memlock::explain) {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("data-cdc-server: {error}");
            std::process::exit(1);
        }
    };
    runtime.block_on(run());
}

async fn run() {
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
        tracing::error!(error = %cause_chain(&*error), "CDC relay stopped");
        std::process::exit(1);
    }
}

/// An error and every cause beneath it, on one line.
///
/// `compio_postgres::Error` displays only its kind - a refused statement is
/// `db error` - and carries the server's message as its source, so a log line
/// that printed the error alone would name no table and no privilege.
fn cause_chain(error: &(dyn std::error::Error + 'static)) -> String {
    let mut line = error.to_string();
    let mut cause = error.source();
    while let Some(next) = cause {
        line.push_str(": ");
        line.push_str(&next.to_string());
        cause = next.source();
    }
    line
}
