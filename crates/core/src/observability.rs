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

use std::io::IsTerminal;

use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

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
    let filter = resolve_init_filter(std::env::var("RUST_LOG").ok(), default_filter);
    let format = std::env::var("ZEROSHIP_LOG_FORMAT").ok();

    init_tracing_with(&filter, format.as_deref());
}

/// Initialise tracing with already-resolved filter and optional format values.
///
/// Passing `None` for `format` preserves the standard auto-detection:
/// TTY stderr uses `pretty`, and non-TTY stderr uses `json`.
pub fn init_tracing_with(filter: &str, format: Option<&str>) {
    let env_filter = EnvFilter::try_new(filter).unwrap_or_else(|err| {
        eprintln!("invalid tracing filter {filter:?}: {err}; falling back to \"error\"");
        EnvFilter::try_new("error").expect("hard-coded tracing filter is valid")
    });

    let format = format.unwrap_or_else(|| {
        if std::io::stderr().is_terminal() {
            "pretty"
        } else {
            "json"
        }
    });

    let registry = tracing_subscriber::registry().with(env_filter);

    match format {
        "json" => {
            registry
                .with(
                    fmt::layer()
                        .json()
                        .with_current_span(true)
                        .with_span_list(true)
                        .with_target(true),
                )
                .init();
        }
        "logfmt" => {
            registry
                .with(tracing_logfmt::Builder::new().with_span_path(true).layer())
                .init();
        }
        "bunyan" => {
            registry
                .with(tracing_bunyan_formatter::JsonStorageLayer)
                .with(tracing_bunyan_formatter::BunyanFormattingLayer::new(
                    binary_name(),
                    std::io::stdout,
                ))
                .init();
        }
        "compact" => {
            registry.with(fmt::layer().compact()).init();
        }
        // "pretty" + unknown values fall through to the developer-friendly default.
        _ => {
            registry
                .with(fmt::layer().pretty().with_thread_ids(false))
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

fn resolve_init_filter(rust_log: Option<String>, default_filter: &str) -> String {
    crate::config::resolve_log_filter(rust_log, default_filter)
}

#[cfg(test)]
mod tests {
    #[test]
    fn init_filter_falls_back_to_default_for_malformed_rust_log() {
        let filter = super::resolve_init_filter(
            Some("zeroship_core=definitely-not-a-level".to_string()),
            "warn,zeroship_core=debug",
        );

        assert_eq!(filter, "warn,zeroship_core=debug");
    }
}
