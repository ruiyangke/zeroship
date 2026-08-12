//! Gateway's generated bootstrap, command and observability controls.
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
pub const DEFAULT_LOG_FILTER: &str = "info,zeroship_gateway=debug";

/// The controls every gateway launch resolves before anything else.
#[zeroship_config(binary = "zeroship-gate", scope = "gateway")]
#[derive(Debug)]
pub struct GateControls {
    /// Optional shared config overlay path.
    #[config(name = "config")]
    pub config: BootstrapControl<Option<PathBuf>>,

    /// Disable auto-discovery of the well-known config overlay
    /// (`/etc/zeroship/zeroship.toml`); use compiled defaults instead.
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
}

impl OverlaySelector for GateControlsSources {
    fn overlay_path(&self) -> Option<&Path> {
        self.config.as_deref()
    }

    fn allow_discovery(&self) -> bool {
        !self.no_config
    }
}

impl ObservabilityControls for GateControls {
    fn log_filter(&self) -> &str {
        self.log_filter.get()
    }

    fn log_format(&self) -> LogFormat {
        *self.log_format.get()
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser;
    use zeroship_core::config::{GeneratedConfig, OverlaySelector};
    use zeroship_core::observability::LogFormat;

    use super::{GateControls, GateControlsSources, DEFAULT_LOG_FILTER};

    #[test]
    fn observability_comes_from_the_overlay_when_no_flag_is_given() {
        // The overlay tier the old ObsSection read by hand, now reached by the
        // generated declaration's canonical path.
        let overlay: toml::Value = toml::from_str(
            "[observability]\nlog_filter = \"warn,zeroship_gateway=trace\"\nlog_format = \"logfmt\"\n",
        )
        .expect("fixture overlay");

        let resolved = GateControls::resolve_config(
            GateControlsSources::try_parse_from(["zeroship-gate"]).expect("bare parse"),
            Some(&overlay),
        )
        .expect("controls resolve");
        assert_eq!(resolved.log_filter.get(), "warn,zeroship_gateway=trace");
        assert_eq!(resolved.log_format.get(), &LogFormat::Logfmt);

        let flagged = GateControls::resolve_config(
            GateControlsSources::try_parse_from([
                "zeroship-gate",
                "--observability-log-format",
                "compact",
            ])
            .expect("flag parse"),
            Some(&overlay),
        )
        .expect("controls resolve");
        assert_eq!(
            flagged.log_format.get(),
            &LogFormat::Compact,
            "the flag must win over the overlay"
        );

        // Does not cover the environment tier; clap merges it into the same
        // carrier and a process-wide env mutation would race sibling tests.
    }

    #[test]
    fn an_absent_overlay_leaves_the_compiled_default() {
        let resolved = GateControls::resolve_config(
            GateControlsSources::try_parse_from(["zeroship-gate"]).expect("bare parse"),
            None,
        )
        .expect("controls resolve");
        assert_eq!(resolved.log_filter.get(), DEFAULT_LOG_FILTER);
        assert_eq!(resolved.log_format.get(), &LogFormat::Auto);
        assert!(
            GateControlsSources::try_parse_from(["zeroship-gate"])
                .expect("bare parse")
                .allow_discovery()
        );
    }
}
