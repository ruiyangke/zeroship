//! Gateway's generated settings: bootstrap, command, observability, operational.
//!
//! In the library rather than `main.rs` so the compiled configuration checker
//! can link the declaration and invoke clap's `CommandFactory` against it.
//!
//! Secret-bearing inputs are deliberately absent: `control_key`, `worker_key`,
//! `db`, `stash_signing_key`, `pairwise_salt` and the three key-file paths stay
//! hand-spelled on `GateCli` until the `Secret<T>` conversion.

use std::path::{Path, PathBuf};

use zeroship_core::config::{
    zeroship_config, BootstrapControl, CheckFormat, CommandControl, ObservabilityControls,
    Operational, OriginScheme, OverlaySelector, TrustedOrigin,
};
use zeroship_core::observability::LogFormat;

/// Tracing directive applied when nothing supplies `observability.log_filter`.
pub const DEFAULT_LOG_FILTER: &str = "info,zeroship_gateway=debug";

/// Every value a gateway launch resolves before it starts serving.
#[zeroship_config(binary = "zeroship-gate", scope = "gateway")]
#[derive(Debug)]
pub struct GateSettings {
    /// Optional shared config overlay path.
    #[config(shared = CONFIG)]
    pub config: BootstrapControl<Option<PathBuf>>,

    /// Disable auto-discovery of the well-known config overlay
    /// (`/etc/zeroship/zeroship.toml`); use compiled defaults instead.
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

    /// HTTP listen port.
    #[config(name = "gateway.port", default = 80)]
    pub port: Operational<u16>,

    /// Address to bind. Defaults to loopback; pass 0.0.0.0 to expose across a network.
    #[config(name = "gateway.bind", default = "127.0.0.1".to_owned())]
    pub bind: Operational<String>,

    /// Control-plane API base URL.
    #[config(shared = CONTROL_URL, default = "http://localhost:9090".to_owned())]
    pub control_url: Operational<String>,

    /// Comma-separated worker base URLs.
    #[config(shared = WORKER_URLS, default = "http://localhost:8080".to_owned())]
    pub worker_urls: Operational<String>,

    /// Route-table polling interval in seconds.
    #[config(shared = POLL_INTERVAL, default = 5)]
    pub poll_interval: Operational<u64>,

    /// Root directory or `s3://` URL for content-addressed deploy blobs.
    #[config(shared = BLOB_STORE, default = "./bundles".to_owned())]
    pub blob_store: Operational<String>,

    /// In-memory blob cache budget in MiB.
    #[config(name = "gateway.blob_cache_mem_mb", default = 256)]
    pub blob_cache_mem_mb: Operational<usize>,

    /// On-disk blob cache budget in GiB.
    #[config(name = "gateway.blob_cache_disk_gb", default = 20)]
    pub blob_cache_disk_gb: Operational<u64>,

    /// Root directory for the on-disk blob cache.
    #[config(name = "gateway.blob_cache_disk_root", default = "./blob-cache".to_owned())]
    pub blob_cache_disk_root: Operational<String>,

    /// Maximum number of pooled `PostgreSQL` connections the gateway
    /// keeps open for session/anchor/revocation work. Bounds concurrent
    /// DB fan-out so an OP brownout (or any stalled query) cannot pile
    /// up unbounded checkouts. Ignored when no DSN is configured.
    #[config(name = "gateway.db_pool_size", default = 16)]
    pub db_pool_size: Operational<usize>,

    /// Public URL advertised as the gateway session-cookie issuer.
    #[config(name = "gateway.public_url", default = "https://api.zeroship.ai".to_owned())]
    pub public_url: Operational<String>,

    /// Upstream URL for the auth service UI and OAuth surfaces.
    #[config(name = "gateway.auth_ui_url", default = "http://auth:9092".to_owned())]
    pub auth_ui_url: Operational<String>,

    /// Scheme used in public app URLs and same-origin checks.
    #[arg(value_enum)]
    #[config(shared = ORIGIN_SCHEME, default = OriginScheme::Https)]
    pub origin_scheme: Operational<OriginScheme>,

    /// Additional exact origins accepted by same-origin guards.
    #[arg(value_delimiter = ',')]
    #[config(shared = TRUSTED_ORIGINS, default = Vec::new())]
    pub trusted_origins: Operational<Vec<TrustedOrigin>>,

    /// Trust `X-Forwarded-For` from an upstream proxy.
    ///
    /// The `num_args`/`default_missing_value` pair keeps bare `--trust-proxy`
    /// meaning true while `--trust-proxy=false` still parses, which is the
    /// shape this flag has always had.
    #[arg(
        num_args = 0..=1,
        default_missing_value = "true",
        value_parser = zeroship_core::config::parse_bool_flag
    )]
    #[config(shared = TRUST_PROXY, default = false)]
    pub trust_proxy: Operational<bool>,
}

impl OverlaySelector for GateSettingsSources {
    fn overlay_path(&self) -> Option<&Path> {
        self.config.as_deref()
    }

    fn allow_discovery(&self) -> bool {
        !self.no_config
    }
}

impl ObservabilityControls for GateSettings {
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
    use zeroship_core::config::{GeneratedConfig, OriginScheme, OverlaySelector};
    use zeroship_core::observability::LogFormat;

    use super::{GateSettings, GateSettingsSources, DEFAULT_LOG_FILTER};

    #[test]
    fn observability_comes_from_the_overlay_when_no_flag_is_given() {
        // The overlay tier the old ObsSection read by hand, now reached by the
        // generated declaration's canonical path.
        let overlay: toml::Value = toml::from_str(
            "[observability]\nlog_filter = \"warn,zeroship_gateway=trace\"\nlog_format = \"logfmt\"\n",
        )
        .expect("fixture overlay");

        let resolved = GateSettings::resolve_config(
            GateSettingsSources::try_parse_from(["zeroship-gate"]).expect("bare parse"),
            Some(&overlay),
        )
        .expect("settings resolve");
        assert_eq!(resolved.log_filter.get(), "warn,zeroship_gateway=trace");
        assert_eq!(resolved.log_format.get(), &LogFormat::Logfmt);

        let flagged = GateSettings::resolve_config(
            GateSettingsSources::try_parse_from([
                "zeroship-gate",
                "--observability-log-format",
                "compact",
            ])
            .expect("flag parse"),
            Some(&overlay),
        )
        .expect("settings resolve");
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
        let resolved = GateSettings::resolve_config(
            GateSettingsSources::try_parse_from(["zeroship-gate"]).expect("bare parse"),
            None,
        )
        .expect("settings resolve");
        assert_eq!(resolved.log_filter.get(), DEFAULT_LOG_FILTER);
        assert_eq!(resolved.log_format.get(), &LogFormat::Auto);
        assert!(
            GateSettingsSources::try_parse_from(["zeroship-gate"])
                .expect("bare parse")
                .allow_discovery()
        );
    }

    #[test]
    fn topology_settings_keep_their_pre_conversion_precedence() {
        // These three used to be merged by hand: `resolve_origin_scheme` and
        // `resolve_trusted_origins` did CLI-or-file-or-default, and trust_proxy
        // did `unwrap_or(false)`. The generated resolver has to reproduce
        // exactly that, because the hand-written helpers are now deleted.
        let overlay: toml::Value = toml::from_str(
            "origin_scheme = \"http\"\ntrusted_origins = [\"https://a.example\"]\n\
             trust_proxy = true\n",
        )
        .expect("fixture overlay");

        let defaults = GateSettings::resolve_config(
            GateSettingsSources::try_parse_from(["zeroship-gate"]).expect("bare parse"),
            None,
        )
        .expect("settings resolve");
        assert_eq!(defaults.origin_scheme.get(), &OriginScheme::Https);
        assert!(defaults.trusted_origins.get().is_empty());
        assert!(!*defaults.trust_proxy.get());

        let overlaid = GateSettings::resolve_config(
            GateSettingsSources::try_parse_from(["zeroship-gate"]).expect("bare parse"),
            Some(&overlay),
        )
        .expect("settings resolve");
        assert_eq!(overlaid.origin_scheme.get(), &OriginScheme::Http);
        assert_eq!(overlaid.trusted_origins.get().len(), 1);
        assert!(*overlaid.trust_proxy.get());

        let flagged = GateSettings::resolve_config(
            GateSettingsSources::try_parse_from([
                "zeroship-gate",
                "--origin-scheme",
                "https",
                "--trust-proxy=false",
            ])
            .expect("flag parse"),
            Some(&overlay),
        )
        .expect("settings resolve");
        assert_eq!(flagged.origin_scheme.get(), &OriginScheme::Https);
        assert!(
            !*flagged.trust_proxy.get(),
            "an explicit --trust-proxy=false must beat the overlay"
        );

        // Bare `--trust-proxy` still means true.
        let bare = GateSettings::resolve_config(
            GateSettingsSources::try_parse_from(["zeroship-gate", "--trust-proxy"])
                .expect("flag parse"),
            None,
        )
        .expect("settings resolve");
        assert!(*bare.trust_proxy.get());
    }
}
