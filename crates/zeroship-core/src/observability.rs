//! Tracing subscriber init and the [`LogFormat`] value type.
//!
//! The observability SETTINGS are not here. The five server binaries declare
//! `observability.log_filter` and `observability.log_format` as ordinary
//! generated `Operational<T>` fields in their own config modules, so each has a
//! flag (`--observability-log-*`), a reserved environment name
//! (`ZEROSHIP_OBSERVABILITY_LOG_*`) and an `[observability]` overlay key from
//! ONE declaration. `config::bootstrap` resolves them and calls
//! [`init_tracing_with`].
//!
//! [`init_tracing`] is the entry point for everything that is NOT a server
//! binary: the creator CLI (`crates/zeroship-cli/src/main.rs`) and the two single-tenant
//! runtime binaries (`crates/runtime/src/core/{server,echo_server}.rs`) are its
//! only callers, and they read `RUST_LOG` / `ZEROSHIP_LOG_FORMAT` through it.
//! Those two reads are now DECLARED keys owned by [`TracingInitConsumer`]
//! rather than raw `std::env::var` calls: `RUST_LOG` is `external` (the
//! `tracing`/`env_filter` convention, not ours), and `ZEROSHIP_LOG_FORMAT` is
//! `cli`.
//!
//! `cli` rather than `platform`, and the distinction was argued twice. The
//! generated `observability.log_format` identity ALREADY exists
//! (`ZEROSHIP_OBSERVABILITY_LOG_FORMAT`, a shared symbol), and every server
//! binary resolves it through `config::bootstrap`; none of them reaches
//! [`init_tracing`]. What is left on this path is the creator-CLI and
//! single-tenant-runtime vector, which Step 4 of
//! `docs/proposals/2026-08-11-config-name-alignment.md` says lands as a `CliEnv`
//! registration without CLI TOML support. Calling it `platform` would have put
//! it in a debt ledger for a declaration that is not missing. Its exit is that
//! these three callers grow their own declaration, at which point the spelling
//! disappears rather than being reclassified. See [`TracingInitConsumer`] for
//! why the values are not passed in by the caller.
//!
//! Calling either init more than once in a process is a no-op after the first
//! (the global subscriber is locked in by `tracing_subscriber::registry().init()`).
//! Both install `tracing_log::LogTracer` so `log::*` calls (e.g. inside
//! `compio-postgres`) bridge through `tracing`.

use std::fmt;
use std::io::IsTerminal;
use std::str::FromStr;

use serde::{Deserialize, Deserializer};
use tracing_subscriber::{fmt as tracing_fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

/// Tracing output format, the resolved type of `observability.log_format`.
///
/// The same value type is parsed from the flag, the environment and the TOML
/// overlay, so a spelling clap accepts and a spelling the overlay accepts
/// cannot drift apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LogFormat {
    /// Choose by terminal: `pretty` on a TTY, `json` otherwise.
    ///
    /// A real variant rather than `Option::None`, so "unset" has one spelling
    /// across the flag, the environment, the overlay and the compiled default.
    #[default]
    Auto,
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
            Self::Auto => "auto",
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
            "auto" => Ok(Self::Auto),
            "pretty" => Ok(Self::Pretty),
            "compact" => Ok(Self::Compact),
            "json" => Ok(Self::Json),
            "logfmt" => Ok(Self::Logfmt),
            "bunyan" => Ok(Self::Bunyan),
            other => Err(format!(
                "invalid log format {other:?}; expected one of auto, pretty, compact, json, logfmt, bunyan"
            )),
        }
    }
}

impl<'de> Deserialize<'de> for LogFormat {
    /// Deserialize through [`FromStr`] so the overlay accepts exactly the
    /// spellings the flag does, case-insensitively, and rejects the rest.
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Self::from_str(&raw).map_err(serde::de::Error::custom)
    }
}

/// Resolve a tracing filter candidate, silently falling back to the default on parse error.
///
/// Silent by design: the invalid-filter fallback is surfaced as a STRUCTURED
/// `tracing::warn!` by [`fn@crate::config::bootstrap`] *after* the subscriber is
/// initialized, so the warning honors the configured log format (e.g. JSON)
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

crate::declare_env_consumer!(
    /// The consumer that owns [`init_tracing`]'s own two environment reads.
    ///
    /// WHY A CONSUMER AND NOT THE CALLER. The obvious shape for Step 4 is to
    /// delete the reads here and have each caller pass resolved values in -
    /// that is exactly what [`fn@crate::config::bootstrap`] already does for the
    /// five server binaries, which resolve the GENERATED
    /// `observability.log_filter` / `observability.log_format` and call
    /// [`init_tracing_with`]. It is not available here: `init_tracing`'s only
    /// callers are `crates/zeroship-cli/src/main.rs` and
    /// `crates/runtime/src/core/{server,echo_server}.rs`, none of which has a
    /// `#[zeroship_config]` declaration to resolve from, and all three sit
    /// outside this crate. Giving them one is Step 3/Step 6 work, not a rename.
    ///
    /// The target is the cargo PACKAGE rather than a binary, per
    /// [`crate::config::DeclaredEnvRead::consumer`]: a library function reached
    /// from three different binaries has no single binary to name.
    pub TracingInitConsumer,
    target = "zeroship-core",
    scope = "observability",
);

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
    let filter = resolve_log_filter(
        crate::declared_env!(external, "RUST_LOG", TracingInitConsumer),
        default_filter,
    );
    let format = crate::declared_env!(cli, "ZEROSHIP_LOG_FORMAT", TracingInitConsumer)
        .and_then(|raw| LogFormat::from_str(&raw).ok())
        .unwrap_or(LogFormat::Auto);

    init_tracing_with(&filter, format);
}

/// Initialise tracing with an already-resolved filter and format.
///
/// [`LogFormat::Auto`] performs the standard detection: TTY stderr uses
/// `pretty`, and non-TTY stderr uses `json`.
///
/// # Panics
///
/// Never in practice: the sole `expect` is on the hard-coded `"error"`
/// fallback directive, which is always a valid `EnvFilter`.
pub fn init_tracing_with(filter: &str, format: LogFormat) {
    let env_filter = EnvFilter::try_new(filter).unwrap_or_else(|err| {
        eprintln!("invalid tracing filter {filter:?}: {err}; falling back to \"error\"");
        EnvFilter::try_new("error").expect("hard-coded tracing filter is valid")
    });

    let format = match format {
        LogFormat::Auto if std::io::stderr().is_terminal() => LogFormat::Pretty,
        LogFormat::Auto => LogFormat::Json,
        selected => selected,
    };

    let registry = tracing_subscriber::registry().with(env_filter);

    match format {
        // Unreachable: `Auto` was resolved above. Kept explicit so adding a
        // variant is a compile error rather than a silent fall-through.
        LogFormat::Auto | LogFormat::Pretty => {
            registry
                .with(tracing_fmt::layer().pretty().with_thread_ids(false))
                .init();
        }
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

    use super::{resolve_log_filter, LogFormat};
    use crate::config::{resolve_operational, CanonicalName};

    #[test]
    fn log_format_from_str_known_and_unknown() {
        assert_eq!(LogFormat::from_str("json"), Ok(LogFormat::Json));
        // case-insensitive
        assert_eq!(LogFormat::from_str("JSON"), Ok(LogFormat::Json));
        assert_eq!(LogFormat::from_str("Pretty"), Ok(LogFormat::Pretty));
        assert_eq!(LogFormat::from_str("logfmt"), Ok(LogFormat::Logfmt));
        assert_eq!(LogFormat::from_str("auto"), Ok(LogFormat::Auto));
        assert!(LogFormat::from_str("nope").is_err());
    }

    #[test]
    fn the_overlay_accepts_exactly_the_spellings_the_flag_does() {
        // One value type across flag, env and TOML: the Deserialize impl routes
        // through FromStr, so the two surfaces cannot drift.
        for (raw, expected) in [
            ("json", LogFormat::Json),
            ("JSON", LogFormat::Json),
            ("auto", LogFormat::Auto),
        ] {
            let parsed: LogFormat =
                toml::from_str::<toml::Value>(&format!("v = {raw:?}"))
                    .expect("fixture TOML")["v"]
                    .clone()
                    .try_into()
                    .expect("overlay value parses");
            assert_eq!(parsed, expected);
        }

        let rejected = toml::from_str::<toml::Value>("v = \"jsom\"")
            .expect("fixture TOML")["v"]
            .clone()
            .try_into::<LogFormat>();
        assert!(
            rejected.is_err(),
            "an unrecognised overlay log_format must be fatal, not a silent degrade"
        );

        // Does not cover a non-string TOML value; that fails in the same
        // try_into for a different reason and is not what this pins.
    }

    #[test]
    fn auto_is_the_compiled_default() {
        assert_eq!(LogFormat::default(), LogFormat::Auto);
        assert_eq!(LogFormat::Auto.to_string(), "auto");
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
    fn observability_precedence_is_carrier_then_overlay_then_compiled_default() {
        // The former resolve_observability(flags, file, default) three-tier
        // merge, restated against the machinery that replaced it. clap has
        // already merged the flag and the environment into `carrier`, so this
        // covers carrier > overlay > default and not flag > env.
        let overlay: toml::Value = toml::from_str(
            "[observability]\nlog_filter = \"info,zeroship_file=debug\"\nlog_format = \"json\"\n",
        )
        .expect("fixture overlay");
        let filter_name = CanonicalName::from_static("observability.log_filter");
        let format_name = CanonicalName::from_static("observability.log_format");
        let fallback = || Some("info,zeroship_default=debug".to_owned());

        assert_eq!(
            resolve_operational(
                filter_name,
                Some("warn,zeroship_flag=trace".to_owned()),
                Some(&overlay),
                fallback,
            )
            .expect("carrier wins")
            .into_inner(),
            "warn,zeroship_flag=trace"
        );
        assert_eq!(
            resolve_operational(filter_name, None, Some(&overlay), fallback)
                .expect("overlay applies")
                .into_inner(),
            "info,zeroship_file=debug"
        );
        assert_eq!(
            resolve_operational(filter_name, None, None, fallback)
                .expect("compiled default applies")
                .into_inner(),
            "info,zeroship_default=debug"
        );

        assert_eq!(
            resolve_operational(
                format_name,
                Some(LogFormat::Compact),
                Some(&overlay),
                || Some(LogFormat::Auto),
            )
            .expect("carrier wins")
            .into_inner(),
            LogFormat::Compact
        );
        assert_eq!(
            resolve_operational(format_name, None, Some(&overlay), || Some(LogFormat::Auto))
                .expect("overlay applies")
                .into_inner(),
            LogFormat::Json
        );
        assert_eq!(
            resolve_operational(format_name, None, None, || Some(LogFormat::Auto))
                .expect("compiled default applies")
                .into_inner(),
            LogFormat::Auto
        );
    }

    // S6: an invalid overlay log_format is fatal, not a silent degrade.
    #[test]
    fn an_invalid_overlay_log_format_is_fatal() {
        let overlay: toml::Value =
            toml::from_str("[observability]\nlog_format = \"jsom\"\n").expect("fixture overlay");
        let error = resolve_operational(
            CanonicalName::from_static("observability.log_format"),
            None,
            Some(&overlay),
            || Some(LogFormat::Auto),
        )
        .expect_err("an unrecognised format must not fall back to Auto");
        assert!(error.to_string().contains("observability.log_format"));

        // The paired positive control is the "json" arm of the precedence test
        // above: same path, same overlay shape, one character different.
    }

    #[test]
    fn an_invalid_filter_still_falls_back_to_the_compiled_default() {
        // Unlike log_format, a syntactically bad filter degrades rather than
        // refusing to boot; bootstrap then warns through the live subscriber.
        assert_eq!(
            resolve_log_filter(
                Some("zeroship_core=definitely-not-a-level".to_owned()),
                "info,zeroship_default=debug",
            ),
            "info,zeroship_default=debug"
        );
    }
}
