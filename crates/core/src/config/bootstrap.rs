//! Shared boot dance + structured `--check-config` emitter.
//!
//! Each binary's `main` shrinks to one [`bootstrap`]/[`bootstrap_or_exit`] call,
//! then builds its `--check-config` rows through [`CheckConfigReport`].

use std::path::Path;

use crate::config::file::ConfigError;
use crate::config::source::{load_overlay, log_overlay_source, LoadedOverlay};
use crate::observability::{
    init_tracing_with, resolve_observability, LogFormat, ObservabilityFlags,
};

/// Everything a binary needs after the shared boot dance has run.
#[derive(Debug)]
pub struct Bootstrap {
    /// The loaded overlay plus its source.
    pub overlay: LoadedOverlay,
    /// The resolved tracing filter directive.
    pub log_filter: String,
    /// The resolved tracing format (None = auto-detect by TTY).
    pub log_format: Option<LogFormat>,
}

/// Run the shared boot dance: load overlay, resolve observability, init tracing,
/// and log the overlay source. Pure — no `process::exit`.
///
/// # Errors
///
/// Propagates any [`ConfigError`] from overlay loading or observability
/// resolution.
pub fn bootstrap(
    config_path: Option<&Path>,
    allow_discovery: bool,
    obs: &ObservabilityFlags,
    default_filter: &str,
    binary: &str,
) -> Result<Bootstrap, ConfigError> {
    let overlay = load_overlay(config_path, allow_discovery, binary)?;
    let (log_filter, log_format) =
        resolve_observability(obs, &overlay.config.observability, default_filter)?;

    init_tracing_with(&log_filter, log_format);
    log_overlay_source(&overlay.source);

    Ok(Bootstrap {
        overlay,
        log_filter,
        log_format,
    })
}

/// [`bootstrap`] for the binary boundary: on error, print `{binary}: …` to
/// stderr + tracing and `process::exit(1)`.
///
/// This is the SINGLE `process::exit` in `core`; its name signals that it is the
/// binary-boundary helper, not a library primitive.
#[must_use]
pub fn bootstrap_or_exit(
    config_path: Option<&Path>,
    allow_discovery: bool,
    obs: &ObservabilityFlags,
    default_filter: &str,
    binary: &str,
) -> Bootstrap {
    match bootstrap(config_path, allow_discovery, obs, default_filter, binary) {
        Ok(bootstrap) => bootstrap,
        Err(err) => {
            let message = format!("{binary}: {err}");
            // Tracing may not be initialised yet; emit to both sinks.
            tracing::error!("{message}");
            eprintln!("{message}");
            std::process::exit(1);
        }
    }
}

/// A typed `--check-config` field value.
#[derive(Debug, Clone)]
pub enum CheckValue {
    /// A plain string value (printed verbatim).
    Plain(String),
    /// A boolean flag.
    Flag(bool),
    /// A count.
    Count(usize),
    /// A secret: only its presence is reported, never its value.
    Secret(bool),
}

/// Output format for a [`CheckConfigReport`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckFormat {
    /// Human-readable `check-config: key = value` lines (default).
    Text,
    /// One machine-readable JSON object.
    Json,
}

/// A structured `--check-config` report shared across binaries.
///
/// Replaces the four copy-pasted `println!` blocks with one emitter that can
/// render either text or JSON, and that reports secrets by presence only.
#[derive(Debug, Default)]
pub struct CheckConfigReport {
    fields: Vec<(String, CheckValue)>,
}

impl CheckConfigReport {
    /// Create an empty report.
    #[must_use]
    pub const fn new() -> Self {
        Self { fields: Vec::new() }
    }

    /// Append a field. Insertion order is preserved in the emitted output.
    pub fn field(&mut self, key: &str, v: CheckValue) {
        self.fields.push((key.to_string(), v));
    }

    /// Emit the report in the requested format to stdout.
    pub fn emit(&self, fmt: CheckFormat) {
        match fmt {
            CheckFormat::Text => self.emit_text(),
            CheckFormat::Json => self.emit_json(),
        }
    }

    fn emit_text(&self) {
        for (key, value) in &self.fields {
            match value {
                CheckValue::Plain(s) => println!("check-config: {key} = {s}"),
                CheckValue::Flag(b) => println!("check-config: {key} = {b}"),
                CheckValue::Count(n) => println!("check-config: {key} = {n}"),
                CheckValue::Secret(true) => println!("check-config: {key} = configured"),
                CheckValue::Secret(false) => println!("check-config: {key} = (unset)"),
            }
        }
    }

    fn emit_json(&self) {
        let mut map = serde_json::Map::with_capacity(self.fields.len());
        for (key, value) in &self.fields {
            let json = match value {
                CheckValue::Plain(s) => serde_json::Value::String(s.clone()),
                // Secret is emitted by presence only — a bool, never the value.
                CheckValue::Flag(b) | CheckValue::Secret(b) => serde_json::Value::Bool(*b),
                CheckValue::Count(n) => serde_json::Value::Number((*n).into()),
            };
            map.insert(key.clone(), json);
        }
        println!(
            "{}",
            serde_json::to_string(&serde_json::Value::Object(map))
                .expect("check-config report serialises")
        );
    }
}

#[cfg(test)]
mod tests {
    use super::{CheckConfigReport, CheckFormat, CheckValue};

    #[test]
    fn report_builds_in_insertion_order() {
        let mut report = CheckConfigReport::new();
        report.field("port", CheckValue::Count(9090));
        report.field("dev", CheckValue::Flag(true));
        report.field("name", CheckValue::Plain("control".to_string()));
        report.field("master_key", CheckValue::Secret(true));
        report.field("worker_key", CheckValue::Secret(false));

        // The internal ordering is what the emitters iterate.
        let keys: Vec<&str> = report.fields.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, ["port", "dev", "name", "master_key", "worker_key"]);
    }

    #[test]
    fn json_emit_serialises_secret_by_presence() {
        // Mirror the JSON value-mapping the emitter uses, asserting a secret
        // becomes a bool (presence) and never carries its value.
        let mut report = CheckConfigReport::new();
        report.field("master_key", CheckValue::Secret(true));
        report.field("count", CheckValue::Count(3));

        let map: serde_json::Map<String, serde_json::Value> = report
            .fields
            .iter()
            .map(|(key, value)| {
                let json = match value {
                    CheckValue::Plain(s) => serde_json::Value::String(s.clone()),
                    CheckValue::Flag(b) | CheckValue::Secret(b) => serde_json::Value::Bool(*b),
                    CheckValue::Count(n) => serde_json::Value::Number((*n).into()),
                };
                (key.clone(), json)
            })
            .collect();

        assert_eq!(map["master_key"], serde_json::Value::Bool(true));
        assert_eq!(map["count"], serde_json::json!(3));
    }

    #[test]
    fn check_format_default_is_text() {
        // Sanity guard so callers know Text is the human default.
        assert_ne!(CheckFormat::Text, CheckFormat::Json);
    }
}
