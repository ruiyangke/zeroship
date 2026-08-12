//! Control's generated bootstrap, command and observability controls.
//!
//! This module lives in the LIBRARY rather than in `main.rs` so the compiled
//! configuration checker can link the declaration and invoke clap's
//! `CommandFactory` against it. `main.rs` flattens
//! [`ControlSettingsSources`] into its parser and never re-spells a flag, an
//! environment name, or an overlay path.

use std::path::{Path, PathBuf};

use zeroship_core::config::{
    zeroship_config, BootstrapControl, CheckFormat, CommandControl, ObservabilityControls,
    Operational, OriginScheme, OverlaySelector,
};
use zeroship_core::observability::LogFormat;

/// Tracing directive applied when nothing supplies `observability.log_filter`.
pub const DEFAULT_LOG_FILTER: &str = "info,zeroship_control=debug";

/// Every value a control-plane launch resolves before it starts serving.
#[zeroship_config(binary = "zeroship-control", scope = "control")]
#[derive(Debug)]
pub struct ControlSettings {
    /// Optional shared config overlay path.
    #[config(shared = CONFIG)]
    pub config: BootstrapControl<Option<PathBuf>>,

    /// Disable auto-discovery of the well-known config overlay (compiled defaults only).
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

    /// HTTP listen port.
    #[config(name = "control.port", default = 9090)]
    pub port: Operational<u16>,

    /// Address to bind. Defaults to loopback; pass 0.0.0.0 to expose across a network.
    #[config(name = "control.bind", default = "127.0.0.1".to_owned())]
    pub bind: Operational<String>,

    /// Root directory or `s3://` URL for bundles and content-addressed deploy blobs.
    #[config(shared = BLOB_STORE, default = "./bundles".to_owned())]
    pub blob_store: Operational<String>,

    /// Comma-separated worker base URLs.
    #[config(shared = WORKER_URLS, default = "http://localhost:8080".to_owned())]
    pub worker_urls: Operational<String>,

    /// Gateway internal base URL used by the workflow engine dispatch seam.
    #[config(name = "control.gateway_url", default = "http://localhost".to_owned())]
    pub gateway_url: Operational<String>,

    /// Provider used as the usage meter.
    #[config(name = "control.meter_provider", default = "lite".to_owned())]
    pub meter_provider: Operational<String>,

    /// Provider used to close and invoice billing periods.
    #[config(name = "control.invoicer_provider", default = "lite".to_owned())]
    pub invoicer_provider: Operational<String>,

    /// Opaque provider JSON config. Use nested keys when the meter and invoicer
    /// are different, e.g. `{"openmeter":{...},"stripe_invoice":{...}}`.
    #[config(name = "control.provider_config", default = "{}".to_owned())]
    pub provider_config: Operational<String>,

    /// Durable usage-event stream transport. Empty disables the event-forwarder
    /// and keeps the legacy aggregate export cron active.
    #[config(name = "control.stream_transport", default = String::new())]
    pub stream_transport: Operational<String>,

    /// Opaque stream transport JSON config, parsed by the selected transport.
    #[config(name = "control.stream_config", default = "{}".to_owned())]
    pub stream_config: Operational<String>,

    /// Stream consumer group for the provider billing forwarder.
    #[config(
        name = "control.billing_forwarder_group_id",
        default = crate::DEFAULT_BILLING_FORWARDER_GROUP_ID.to_owned()
    )]
    pub billing_forwarder_group_id: Operational<String>,

    /// Stream consumer group for the local spend recompute witness.
    #[config(
        name = "control.spend_recompute_group_id",
        default = crate::DEFAULT_SPEND_RECOMPUTE_GROUP_ID.to_owned()
    )]
    pub spend_recompute_group_id: Operational<String>,

    /// Interval in seconds for the stream-backed spend recompute cron. Default
    /// is hourly per billing-provider-platform v7 enforcement.
    #[config(
        name = "control.spend_recompute_interval",
        default = crate::cron::spend_recompute::DEFAULT_RECOMPUTE_INTERVAL_SECS
    )]
    pub spend_recompute_interval: Operational<u64>,

    /// Tax provider backend. `native` (default) computes `0` - the USD launch
    /// owes no tax. The seam exists so enabling a real `StripeTaxProvider`
    /// later is a provider swap, not a schema change.
    #[config(name = "control.tax_provider", default = "native".to_owned())]
    pub tax_provider: Operational<String>,

    /// Stripe REST API base URL the outbound client targets. Override only for
    /// testing against a mock.
    #[config(name = "control.stripe_base_url", default = "https://api.stripe.com".to_owned())]
    pub stripe_base_url: Operational<String>,

    /// Mailer driver for billing notifications: `stdout` (default, dev),
    /// `smtp`, or `resend`.
    #[config(name = "control.mailer", default = "stdout".to_owned())]
    pub mailer: Operational<String>,

    /// SMTP host - required when the mailer is `smtp`. Empty means unset.
    #[config(name = "control.smtp_host", default = String::new())]
    pub smtp_host: Operational<String>,

    /// SMTP port (default 587, STARTTLS).
    #[config(name = "control.smtp_port", default = 587)]
    pub smtp_port: Operational<u16>,

    /// SMTP username. Empty means an unauthenticated relay.
    #[config(name = "control.smtp_username", default = String::new())]
    pub smtp_username: Operational<String>,

    /// Directory for in-flight deploy bodies; empty means the OS temp dir.
    #[config(name = "control.deploy_tmp_dir", default = String::new())]
    pub deploy_tmp_dir: Operational<String>,

    /// Trust `X-Forwarded-For` from an upstream proxy.
    #[arg(
        num_args = 0..=1,
        default_missing_value = "true",
        value_parser = zeroship_core::config::parse_bool_flag
    )]
    #[config(shared = TRUST_PROXY, default = false)]
    pub trust_proxy: Operational<bool>,

    /// Scheme used in public app, auth, and console URLs.
    #[arg(value_enum)]
    #[config(shared = ORIGIN_SCHEME, default = OriginScheme::Https)]
    pub origin_scheme: Operational<OriginScheme>,

    /// Platform auth provider backend (`platform` or `supabase`).
    #[config(shared = AUTH_PROVIDER, default = "platform".to_owned())]
    pub auth_provider: Operational<String>,

    /// Supabase Auth / GoTrue base URL used when the auth provider is Supabase.
    #[config(shared = AUTH_SUPABASE_URL, default = String::new())]
    pub supabase_url: Operational<String>,

    /// JWKS URL for asymmetric GoTrue JWT verification. Mutually exclusive with
    /// the HS256 GoTrue JWT secret.
    #[config(name = "control.supabase_jwks_url", default = String::new())]
    pub supabase_jwks_url: Operational<String>,

    /// GoTrue JWT issuer pinned during Supabase token verification.
    #[config(name = "control.supabase_jwt_issuer", default = String::new())]
    pub supabase_jwt_issuer: Operational<String>,

    /// Platform OP issuer for platform-issued control/deploy access tokens.
    #[config(shared = AUTH_PLATFORM_ISSUER, default = String::new())]
    pub auth_platform_issuer: Operational<String>,

    /// Platform OP JWKS URL. Defaults to `{issuer}/.well-known/jwks.json`.
    #[config(shared = AUTH_PLATFORM_JWKS_URL, default = String::new())]
    pub auth_platform_jwks_url: Operational<String>,

    /// Expected OAuth access-token audience for control bearer auth.
    #[config(shared = OAUTH_AUDIENCE, default = "control.zeroship.ai".to_owned())]
    pub oauth_audience: Operational<String>,

    /// Apex domain hosted creator apps serve under. An app named `myapp`
    /// serves at `myapp.{app_base_domain}`; the per-app OAuth client's
    /// `redirect_uris` + `sector_identifier` are derived from that apex host
    /// by the client-provisioning path. Defaults to the prod apex; dev/compose
    /// set `zeroship.localhost`.
    #[config(name = "control.app_base_domain", default = "zeroship.ai".to_owned())]
    pub app_base_domain: Operational<String>,

    /// Retention horizon (months) for the append-only audit tables
    /// `zeroship.app_audit` + `zeroship.authz_decisions`. Rows older than this
    /// are swept by the in-process retention cron.
    #[config(
        name = "control.audit_retention_months",
        default = crate::cron::audit_retention::DEFAULT_RETENTION_MONTHS
    )]
    pub audit_retention_months: Operational<u32>,

    /// Tick interval (seconds) for the audit-retention cron. Operators can drop
    /// this for tests; production should leave the default.
    #[config(
        name = "control.audit_retention_check_secs",
        default = crate::cron::audit_retention::DEFAULT_CHECK_SECS
    )]
    pub audit_retention_check_secs: Operational<u64>,
}

impl OverlaySelector for ControlSettingsSources {
    fn overlay_path(&self) -> Option<&Path> {
        self.config.as_deref()
    }

    fn allow_discovery(&self) -> bool {
        !self.no_config
    }
}

impl ObservabilityControls for ControlSettings {
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

    use super::{ControlSettings, ControlSettingsSources, DEFAULT_LOG_FILTER};

    #[test]
    fn the_two_safety_controls_keep_their_flag_spellings_and_gain_an_env() {
        // `--disable-workflow-engine` and `--allow-unsupported-billing` are
        // driven by e2e scripts and compose; the canonical `control.` prefix is
        // stripped by the binary scope, so the flag an operator types is
        // unchanged while the environment name becomes reserved-prefixed.
        // Does not cover: whether those scripts were updated. That is a grep
        // over tests/, not something a clap Command can answer.
        let command = ControlSettingsSources::command();
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
        let command = ControlSettingsSources::command();
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
        let plain = ControlSettingsSources::try_parse_from(["zeroship-control"])
            .expect("bare parse");
        assert!(plain.allow_discovery());
        assert_eq!(plain.overlay_path(), None);

        let suppressed =
            ControlSettingsSources::try_parse_from(["zeroship-control", "--no-config"])
                .expect("no-config parse");
        assert!(!suppressed.allow_discovery());

        let explicit = ControlSettingsSources::try_parse_from([
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
        let resolved = ControlSettings::resolve_config(
            ControlSettingsSources::try_parse_from(["zeroship-control"]).expect("bare parse"),
            None,
        )
        .expect("controls resolve with no overlay");

        assert_eq!(resolved.log_filter.get(), DEFAULT_LOG_FILTER);
        assert_eq!(resolved.check_config_format.get(), &CheckFormat::Text);
        assert!(!*resolved.disable_workflow_engine.get());
        assert!(!*resolved.allow_unsupported_billing.get());
    }
}
