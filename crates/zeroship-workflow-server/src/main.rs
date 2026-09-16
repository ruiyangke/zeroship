use clap::Parser;
use zeroship_core::config::{bootstrap_or_exit, CheckConfigReport, CheckValue};
use zeroship_workflow_server::{
    config::{WorkflowSettings, WorkflowSettingsSources, DEFAULT_LOG_FILTER},
    server::{run, ServerOptions},
};

fn main() {
    let (settings, boot) = bootstrap_or_exit::<WorkflowSettings>(
        WorkflowSettingsSources::parse(),
        DEFAULT_LOG_FILTER,
        "workflow",
    );
    let options = match ServerOptions::resolve(&settings) {
        Ok(value) => value,
        Err(error) => {
            eprintln!("workflow configuration: {error}");
            std::process::exit(1);
        }
    };
    if *settings.check_config.get() {
        let mut report = CheckConfigReport::new();
        report.field(
            "config_source",
            CheckValue::Plain(boot.overlay.source.to_string()),
        );
        report.field("log_filter", CheckValue::Plain(boot.log_filter));
        report.field("log_format", CheckValue::Plain(boot.log_format.to_string()));
        report.field("listen", CheckValue::Plain(options.listen.to_string()));
        report.field(
            "database_configured",
            CheckValue::Secret(settings.database_url.is_configured()),
        );
        report.field("http_threads", CheckValue::Count(options.http_threads));
        report.field(
            "max_connections",
            CheckValue::Count(options.max_connections),
        );
        report.field(
            "max_request_bytes",
            CheckValue::Count(options.max_request_bytes),
        );
        report.emit(*settings.check_config_format.get());
        return;
    }
    let result = ntex::rt::System::build()
        .name("zeroship-workflow-server")
        .build(ntex::rt::DefaultRuntime)
        .block_on(run(settings, options));
    if let Err(error) = result {
        tracing::error!(%error, "workflow server stopped");
        std::process::exit(1);
    }
}
