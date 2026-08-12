//! Control's generated bootstrap, command and observability controls.
//!
//! This module lives in the LIBRARY rather than in `main.rs` so the compiled
//! configuration checker can link the declaration and invoke clap's
//! `CommandFactory` against it. `main.rs` flattens
//! [`ControlControlsSources`] into its parser and never re-spells a flag, an
//! environment name, or an overlay path.

use std::path::{Path, PathBuf};

use zeroship_core::config::{
    zeroship_config, BootstrapControl, CheckFormat, CommandControl, ObservabilityControls,
    Operational, OverlaySelector,
};
use zeroship_core::observability::LogFormat;

/// Tracing directive applied when nothing supplies `observability.log_filter`.
pub const DEFAULT_LOG_FILTER: &str = "info,zeroship_control=debug";

/// The controls every control-plane launch resolves before anything else.
#[zeroship_config(binary = "zeroship-control", scope = "control")]
#[derive(Debug)]
pub struct ControlControls {
    /// Optional shared config overlay path.
    #[config(name = "config")]
    pub config: BootstrapControl<Option<PathBuf>>,

    /// Disable auto-discovery of the well-known config overlay (compiled defaults only).
    #[config(name = "no_config")]
    pub no_config: BootstrapControl<bool>,

    /// Validate config (CLI + overlay + guards) and print the resolved non-secret
    /// config, then exit without starting the server.
    #[config(name = "check_config")]
    pub check_config: CommandControl<bool>,

    /// Output format for `--check-config`.
    #[config(name = "check_config_format", default = CheckFormat::Text)]
    pub check_config_format: CommandControl<CheckFormat>,

    /// `EnvFilter` directive for the tracing subscriber.
    #[config(name = "observability.log_filter", default = DEFAULT_LOG_FILTER.to_owned())]
    pub log_filter: Operational<String>,

    /// Tracing output format; `auto` picks pretty on a TTY and json otherwise.
    #[config(name = "observability.log_format", default = LogFormat::Auto)]
    pub log_format: Operational<LogFormat>,

    /// Test harness only: do not spawn durable-workflow background work.
    /// The e2e harness drives the scheduler path explicitly from its test
    /// process while this control process serves sync/deploy state.
    ///
    /// `env = false` keeps the pre-conversion supply set: a harness passes an
    /// argument deliberately, whereas an exported variable disables the engine
    /// for every control process that inherits it.
    #[arg(hide = true)]
    #[config(name = "control.disable_workflow_engine", env = false)]
    pub disable_workflow_engine: BootstrapControl<bool>,

    /// Permit an evaluation-grade billing provider such as `lite` in production.
    #[config(name = "control.allow_unsupported_billing")]
    pub allow_unsupported_billing: BootstrapControl<bool>,
}

impl OverlaySelector for ControlControlsSources {
    fn overlay_path(&self) -> Option<&Path> {
        self.config.as_deref()
    }

    fn allow_discovery(&self) -> bool {
        !self.no_config
    }
}

impl ObservabilityControls for ControlControls {
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
    use zeroship_core::config::{CheckFormat, GeneratedConfig, OverlaySelector};

    use super::{ControlControls, ControlControlsSources, DEFAULT_LOG_FILTER};

    #[test]
    fn the_two_safety_controls_keep_their_flag_spellings_and_gain_an_env() {
        // `--disable-workflow-engine` and `--allow-unsupported-billing` are
        // driven by e2e scripts and compose; the canonical `control.` prefix is
        // stripped by the binary scope, so the flag an operator types is
        // unchanged while the environment name becomes reserved-prefixed.
        // Does not cover: whether those scripts were updated. That is a grep
        // over tests/, not something a clap Command can answer.
        let command = ControlControlsSources::command();
        for (id, long, env) in [
            (
                "disable_workflow_engine",
                "disable-workflow-engine",
                // `env = false`: the hidden harness switch keeps its flag-only
                // supply set, so no inherited variable can disable the engine.
                None,
            ),
            (
                "allow_unsupported_billing",
                "allow-unsupported-billing",
                Some("ZEROSHIP_CONTROL_ALLOW_UNSUPPORTED_BILLING"),
            ),
        ] {
            let arg = command
                .get_arguments()
                .find(|arg| arg.get_id() == id)
                .unwrap_or_else(|| panic!("no argument {id}"));
            assert_eq!(arg.get_long(), Some(long));
            assert_eq!(arg.get_env().and_then(std::ffi::OsStr::to_str), env);
        }
    }

    #[test]
    fn check_config_is_a_flag_with_no_environment_source() {
        // A stray environment variable must not be able to turn a running
        // control plane into a config dump that exits before serving.
        let command = ControlControlsSources::command();
        let check = command
            .get_arguments()
            .find(|arg| arg.get_id() == "check_config")
            .expect("check_config argument");
        assert_eq!(check.get_long(), Some("check-config"));
        assert_eq!(check.get_env(), None);

        let format = command
            .get_arguments()
            .find(|arg| arg.get_id() == "check_config_format")
            .expect("check_config_format argument");
        assert_eq!(format.get_long(), Some("check-config-format"));
        assert_eq!(format.get_env(), None);
    }

    #[test]
    fn discovery_is_on_unless_no_config_is_passed() {
        let plain = ControlControlsSources::try_parse_from(["zeroship-control"])
            .expect("bare parse");
        assert!(plain.allow_discovery());
        assert_eq!(plain.overlay_path(), None);

        let suppressed =
            ControlControlsSources::try_parse_from(["zeroship-control", "--no-config"])
                .expect("no-config parse");
        assert!(!suppressed.allow_discovery());

        let explicit = ControlControlsSources::try_parse_from([
            "zeroship-control",
            "--config",
            "/etc/zeroship/zeroship.toml",
        ])
        .expect("config parse");
        assert_eq!(
            explicit.overlay_path(),
            Some(std::path::Path::new("/etc/zeroship/zeroship.toml"))
        );

        // Does not cover the environment tier; clap merges it into the same
        // carriers and a process-wide env mutation would race sibling tests.
    }

    #[test]
    fn resolution_without_an_overlay_yields_the_compiled_defaults() {
        let resolved = ControlControls::resolve_config(
            ControlControlsSources::try_parse_from(["zeroship-control"]).expect("bare parse"),
            None,
        )
        .expect("controls resolve with no overlay");

        assert_eq!(resolved.log_filter.get(), DEFAULT_LOG_FILTER);
        assert_eq!(resolved.check_config_format.get(), &CheckFormat::Text);
        assert!(!*resolved.disable_workflow_engine.get());
        assert!(!*resolved.allow_unsupported_billing.get());
    }
}
