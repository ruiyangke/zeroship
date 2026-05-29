//! Workspace-wide tracing subscriber init. Format pluggable via
//! `ZEROSHIP_LOG_FORMAT={pretty|compact|json|logfmt|bunyan}`.
//!
//! Default: `pretty` when stderr is a TTY, `json` otherwise. The
//! `RUST_LOG` env var (standard `tracing-subscriber::EnvFilter`
//! syntax) overrides the per-binary `default_filter` argument.
//!
//! Every workspace binary calls [`init_tracing`] early in `main`,
//! once. Calling it more than once in the same process is a no-op
//! after the first call (the global subscriber is locked in by
//! `tracing_subscriber::registry().init()`).
//!
//! The function also installs `tracing_log::LogTracer` so any
//! `log::*` calls (e.g. inside `compio-postgres`) bridge through
//! `tracing` transparently.

use std::fmt;
use std::io::IsTerminal;
use std::str::FromStr;

use tracing_subscriber::{fmt as tracing_fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

use crate::config::file::{ConfigError, ObsSection};

/// Tracing output format. Parsed by clap (CLI/env) and by
/// [`resolve_observability`] (file overlay); both reject unknown values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogFormat {
    /// Developer-friendly multi-line layout.
    Pretty,
    /// Single-line compact layout.
    Compact,
    /// Structured JSON (production default for non-TTY stderr).
    Json,
    /// `logfmt` key=value layout.
    Logfmt,
    /// Bunyan-style JSON.
    Bunyan,
}

impl fmt::Display for LogFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Pretty => "pretty",
            Self::Compact => "compact",
            Self::Json => "json",
            Self::Logfmt => "logfmt",
            Self::Bunyan => "bunyan",
        })
    }
}

impl FromStr for LogFormat {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "pretty" => Ok(Self::Pretty),
            "compact" => Ok(Self::Compact),
            "json" => Ok(Self::Json),
            "logfmt" => Ok(Self::Logfmt),
            "bunyan" => Ok(Self::Bunyan),
            other => Err(format!(
                "invalid log format {other:?}; expected one of pretty, compact, json, logfmt, bunyan"
            )),
        }
    }
}

/// CLI/environment observability overrides shared by server binaries.
#[derive(Debug, Clone, Default, clap::Args)]
pub struct ObservabilityFlags {
    /// `RUST_LOG` / `EnvFilter` directive.
    #[arg(long = "log-filter", env = "RUST_LOG")]
    pub log_filter: Option<String>,
    /// Tracing output format (clap rejects invalid values at parse time).
    #[arg(long = "log-format", env = "ZEROSHIP_LOG_FORMAT")]
    pub log_format: Option<LogFormat>,
}

/// Resolve a tracing filter candidate, silently falling back to the default on parse error.
///
/// Silent by design: the invalid-filter fallback is surfaced as a STRUCTURED
/// `tracing::warn!` by [`crate::config::bootstrap`] *after* the subscriber is
/// initialized, so the warning honors the configured `--log-format` (e.g. JSON)
/// instead of being a pre-tracing `eprintln!` a log pipeline would miss (O3).
#[must_use]
pub fn resolve_log_filter(candidate: Option<String>, default_filter: &str) -> String {
    let Some(candidate) = candidate else {
        return default_filter.to_string();
    };

    if EnvFilter::try_new(candidate.as_str()).is_ok() {
        candidate
    } else {
        default_filter.to_string()
    }
}

/// Resolve observability settings with flag/env values overriding the file.
///
/// The `log_filter` keeps validate-or-default semantics. The `log_format` from
/// the file overlay is parsed into a [`LogFormat`] and **errors** on an invalid
/// value (S6), so `--check-config` and boot agree.
///
/// # Errors
///
/// Returns [`ConfigError::InvalidLogFormat`] when the file overlay carries an
/// unrecognised `log_format`.
pub fn resolve_observability(
    flags: &ObservabilityFlags,
    file: &ObsSection,
    default_filter: &str,
) -> Result<(String, Option<LogFormat>), ConfigError> {
    let filter = resolve_log_filter(
        flags.log_filter.clone().or_else(|| file.log_filter.clone()),
        default_filter,
    );

    let format = match flags.log_format {
        Some(fmt) => Some(fmt),
        None => match &file.log_format {
            Some(raw) => Some(LogFormat::from_str(raw).map_err(|_| {
                ConfigError::InvalidLogFormat {
                    value: raw.clone(),
                }
            })?),
            None => None,
        },
    };

    Ok((filter, format))
}

/// Initialise the workspace-wide tracing subscriber.
///
/// `default_filter` is the directive applied when `RUST_LOG` is not
/// set. Pick something matching the binary, e.g.
/// `"info,zeroship_gateway=debug"`.
///
/// The output format is selected by the `ZEROSHIP_LOG_FORMAT` env
/// var; valid values are `pretty`, `compact`, `json`, `logfmt`,
/// `bunyan`. Unknown values fall back to `pretty`. When the env var
/// is unset, we auto-detect: TTY stderr -> `pretty`, otherwise
/// `json` (production-friendly).
pub fn init_tracing(default_filter: &str) {
    let filter = resolve_log_filter(std::env::var("RUST_LOG").ok(), default_filter);
    let format = std::env::var("ZEROSHIP_LOG_FORMAT")
        .ok()
        .and_then(|raw| LogFormat::from_str(&raw).ok());

    init_tracing_with(&filter, format);
}

/// Initialise tracing with an already-resolved filter and optional format.
///
/// Passing `None` for `format` preserves the standard auto-detection:
/// TTY stderr uses `pretty`, and non-TTY stderr uses `json`.
///
/// # Panics
///
/// Never in practice: the sole `expect` is on the hard-coded `"error"`
/// fallback directive, which is always a valid `EnvFilter`.
pub fn init_tracing_with(filter: &str, format: Option<LogFormat>) {
    let env_filter = EnvFilter::try_new(filter).unwrap_or_else(|err| {
        eprintln!("invalid tracing filter {filter:?}: {err}; falling back to \"error\"");
        EnvFilter::try_new("error").expect("hard-coded tracing filter is valid")
    });

    let format = format.unwrap_or_else(|| {
        if std::io::stderr().is_terminal() {
            LogFormat::Pretty
        } else {
            LogFormat::Json
        }
    });

    let registry = tracing_subscriber::registry().with(env_filter);

    match format {
        LogFormat::Json => {
            registry
                .with(
                    tracing_fmt::layer()
                        .json()
                        .with_current_span(true)
                        .with_span_list(true)
                        .with_target(true),
                )
                .init();
        }
        LogFormat::Logfmt => {
            registry
                .with(tracing_logfmt::Builder::new().with_span_path(true).layer())
                .init();
        }
        LogFormat::Bunyan => {
            registry
                .with(tracing_bunyan_formatter::JsonStorageLayer)
                .with(tracing_bunyan_formatter::BunyanFormattingLayer::new(
                    binary_name(),
                    std::io::stdout,
                ))
                .init();
        }
        LogFormat::Compact => {
            registry.with(tracing_fmt::layer().compact()).init();
        }
        LogFormat::Pretty => {
            registry
                .with(tracing_fmt::layer().pretty().with_thread_ids(false))
                .init();
        }
    }

    // Bridge any third-party `log::*` calls through to tracing.
    // `compio-postgres` uses `log::trace!`/`debug!`/`info!`/`warn!`/
    // `error!`. Idempotent — safe to call even if `log` isn't wired
    // and safe to call after the registry has been initialised.
    let _ = tracing_log::LogTracer::init();
}

fn binary_name() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_else(|| "zeroship".into())
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::{resolve_log_filter, resolve_observability, LogFormat, ObservabilityFlags};
    use crate::config::file::{ConfigError, ObsSection};

    #[test]
    fn log_format_from_str_known_and_unknown() {
        assert_eq!(LogFormat::from_str("json"), Ok(LogFormat::Json));
        // case-insensitive
        assert_eq!(LogFormat::from_str("JSON"), Ok(LogFormat::Json));
        assert_eq!(LogFormat::from_str("Pretty"), Ok(LogFormat::Pretty));
        assert_eq!(LogFormat::from_str("logfmt"), Ok(LogFormat::Logfmt));
        assert!(LogFormat::from_str("nope").is_err());
    }

    #[test]
    fn resolve_log_filter_falls_back_to_default_for_malformed() {
        let filter = resolve_log_filter(
            Some("zeroship_core=definitely-not-a-level".to_string()),
            "warn,zeroship_core=debug",
        );
        assert_eq!(filter, "warn,zeroship_core=debug");
    }

    #[test]
    fn resolve_observability_prefers_flag_then_file_then_default_filter() {
        let file = ObsSection {
            log_filter: Some("info,zeroship_file=debug".to_string()),
            log_format: None,
        };

        let flags = ObservabilityFlags {
            log_filter: Some("warn,zeroship_flag=trace".to_string()),
            log_format: None,
        };
        let (filter, _) =
            resolve_observability(&flags, &file, "info,zeroship_default=debug").expect("ok");
        assert_eq!(filter, "warn,zeroship_flag=trace");

        let flags = ObservabilityFlags::default();
        let (filter, _) =
            resolve_observability(&flags, &file, "info,zeroship_default=debug").expect("ok");
        assert_eq!(filter, "info,zeroship_file=debug");

        let file = ObsSection::default();
        let (filter, _) =
            resolve_observability(&flags, &file, "info,zeroship_default=debug").expect("ok");
        assert_eq!(filter, "info,zeroship_default=debug");
    }

    #[test]
    fn resolve_observability_prefers_flag_then_file_then_none_format() {
        let file = ObsSection {
            log_filter: None,
            log_format: Some("json".to_string()),
        };

        let flags = ObservabilityFlags {
            log_filter: None,
            log_format: Some(LogFormat::Compact),
        };
        let (_, format) = resolve_observability(&flags, &file, "info").expect("ok");
        assert_eq!(format, Some(LogFormat::Compact));

        let flags = ObservabilityFlags::default();
        let (_, format) = resolve_observability(&flags, &file, "info").expect("ok");
        assert_eq!(format, Some(LogFormat::Json));

        let file = ObsSection::default();
        let (_, format) = resolve_observability(&flags, &file, "info").expect("ok");
        assert!(format.is_none());
    }

    // S6: an invalid file-provided log_format is fatal, not a silent degrade.
    #[test]
    fn resolve_observability_errors_on_invalid_file_format() {
        let file = ObsSection {
            log_filter: None,
            log_format: Some("jsom".to_string()),
        };
        let flags = ObservabilityFlags::default();

        let err = resolve_observability(&flags, &file, "info").expect_err("invalid format");
        assert!(matches!(err, ConfigError::InvalidLogFormat { value } if value == "jsom"));
    }

    #[test]
    fn resolve_observability_falls_back_to_default_for_malformed_filter() {
        let file = ObsSection {
            log_filter: Some("info,zeroship_file=debug".to_string()),
            log_format: None,
        };
        let flags = ObservabilityFlags {
            log_filter: Some("zeroship_core=definitely-not-a-level".to_string()),
            log_format: None,
        };

        let (filter, _) =
            resolve_observability(&flags, &file, "info,zeroship_default=debug").expect("ok");
        assert_eq!(filter, "info,zeroship_default=debug");
    }
}
