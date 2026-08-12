//! Worker's generated bootstrap, command and observability controls.
//!
//! In the library rather than `main.rs` so the compiled configuration checker
//! can link the declaration and invoke clap's `CommandFactory` against it.

use std::path::{Path, PathBuf};

use zeroship_core::config::{
    zeroship_config, BootstrapControl, CheckFormat, CommandControl, ObservabilityControls,
    Operational, OverlaySelector,
};
use zeroship_core::observability::LogFormat;

/// Tracing directive applied when nothing supplies `observability.log_filter`.
pub const DEFAULT_LOG_FILTER: &str = "info,zeroship_worker=debug";

/// The controls every worker launch resolves before anything else.
#[zeroship_config(binary = "zeroship-worker", scope = "worker")]
#[derive(Debug)]
pub struct WorkerControls {
    /// Optional shared config overlay path.
    #[config(shared = CONFIG)]
    pub config: BootstrapControl<Option<PathBuf>>,

    /// Disable auto-discovery of the well-known overlay path; use compiled
    /// defaults even if `/etc/zeroship/zeroship.toml` exists (O5).
    #[config(shared = NO_CONFIG)]
    pub no_config: BootstrapControl<bool>,

    /// Validate config (CLI + overlay + guards) and print the resolved non-secret
    /// config, then exit without starting the server.
    #[config(shared = CHECK_CONFIG)]
    pub check_config: CommandControl<bool>,

    /// Output format for `--check-config`.
    #[config(shared = CHECK_CONFIG_FORMAT, default = CheckFormat::Text)]
    pub check_config_format: CommandControl<CheckFormat>,

    /// `EnvFilter` directive for the tracing subscriber.
    #[config(shared = OBSERVABILITY_LOG_FILTER, default = DEFAULT_LOG_FILTER.to_owned())]
    pub log_filter: Operational<String>,

    /// Tracing output format; `auto` picks pretty on a TTY and json otherwise.
    #[config(shared = OBSERVABILITY_LOG_FORMAT, default = LogFormat::Auto)]
    pub log_format: Operational<LogFormat>,

    /// Enable the unsigned durable-workflow replay ingress, which performs NO
    /// signature or nonce verification. Hidden because signed advance is the
    /// production transport; this exercises the real replay path.
    ///
    /// The handler itself always ships - this control is what refuses it at
    /// runtime, so the default here IS the production protection. `env = false`
    /// is load-bearing: a stray environment variable must not be able to turn
    /// signature verification off, and a bootstrap control also has no overlay
    /// tier that could persist it.
    #[arg(hide = true)]
    #[config(name = "worker.workflow_advance_unsigned", env = false)]
    pub workflow_advance_unsigned: BootstrapControl<bool>,
}

impl OverlaySelector for WorkerControlsSources {
    fn overlay_path(&self) -> Option<&Path> {
        self.config.as_deref()
    }

    fn allow_discovery(&self) -> bool {
        !self.no_config
    }
}

impl ObservabilityControls for WorkerControls {
    fn log_filter(&self) -> &str {
        self.log_filter.get()
    }

    fn log_format(&self) -> LogFormat {
        *self.log_format.get()
    }
}

#[cfg(test)]
mod tests {
    use clap::{CommandFactory, Parser};
    use zeroship_core::config::GeneratedConfig;

    use super::{WorkerControls, WorkerControlsSources};

    #[test]
    fn unsigned_workflow_advance_has_no_environment_or_overlay_source() {
        // The pre-conversion comment on this flag says a stray environment
        // variable must not be able to turn signature verification off. The
        // conversion keeps that by DECLARATION (`env = false`) rather than by
        // remembering to omit an attribute, and a bootstrap control has no TOML
        // tier at all, so a persisted overlay cannot supply it either.
        let arg = WorkerControlsSources::command()
            .get_arguments()
            .find(|arg| arg.get_id() == "workflow_advance_unsigned")
            .cloned()
            .expect("workflow_advance_unsigned argument");
        assert_eq!(arg.get_long(), Some("workflow-advance-unsigned"));
        assert_eq!(arg.get_env(), None);

        let overlay: toml::Value =
            toml::from_str("[worker]\nworkflow_advance_unsigned = true\n").expect("overlay");
        let resolved = WorkerControls::resolve_config(
            WorkerControlsSources::try_parse_from(["zeroship-worker"]).expect("bare parse"),
            Some(&overlay),
        )
        .expect("controls resolve");
        assert!(
            !*resolved.workflow_advance_unsigned.get(),
            "an overlay entry must not enable the unsigned replay ingress"
        );

        // The one-variable control: the same parse WITH the flag.
        let flagged = WorkerControls::resolve_config(
            WorkerControlsSources::try_parse_from([
                "zeroship-worker",
                "--workflow-advance-unsigned",
            ])
            .expect("flag parse"),
            None,
        )
        .expect("controls resolve");
        assert!(*flagged.workflow_advance_unsigned.get());

        // Does not cover: whether the handler honours the resolved value. That
        // is the worker's dispatch path, asserted in its own tests.
    }
}
