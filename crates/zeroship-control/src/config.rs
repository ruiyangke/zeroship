//! Control's generated bootstrap, command and observability controls.
//!
//! This module lives in the LIBRARY rather than in `main.rs` so the compiled
//! configuration checker can link the declaration and invoke clap's
//! `CommandFactory` against it. `main.rs` flattens
//! [`ControlSettingsSources`] into its parser and never re-spells a flag, an
//! environment name, or an overlay path.

use std::path::{Path, PathBuf};

use zeroship_core::config::{
    zeroship_config, AuthProviderKind, BootstrapControl, CheckFormat, CommandControl,
    ObservabilityControls, Operational, OriginScheme, OverlaySelector, Secret,
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

    /// Workflow coordinator used to verify app placement and queue management,
    /// and to publish app lifecycle intents. An origin the manager client
    /// would refuse refuses the boot.
    #[config(name = "control.workflow_coordinator_url", default = "http://127.0.0.1:9093".to_owned())]
    pub workflow_coordinator_url: Operational<String>,

    /// Catalog sessions the whole process may hold at once, each on a thread
    /// of its own, and so also the catalog transactions that run at once.
    /// Deploy, archive and restore share them with the lifecycle publisher and
    /// wait for a free one beyond the bound. Must be positive.
    #[config(
        name = "control.catalog_max_connections",
        default = crate::publication::shared::DEFAULT_MAX_CONNECTIONS.get()
    )]
    pub catalog_max_connections: Operational<usize>,

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

    /// Platform auth provider backend.
    ///
    /// SHARED with the auth service, which SERVES the provider whose tokens
    /// control verifies. One deployment decision, one `ZEROSHIP_AUTH_PROVIDER`,
    /// one `AuthProviderKind`. It used to be a control-scoped `String` at
    /// `control.auth_provider` accepting `platform|supabase` against auth's
    /// `native|supabase`, so the pair could be set to disagree and control
    /// either widened its trust silently or failed every request at run time.
    ///
    /// `native` here means the platform's own OP is the trusted issuer - the
    /// state control used to spell `platform`.
    ///
    /// This value names which backend auth SERVES, which is exclusive. What
    /// control TRUSTS is a derived SET: `supabase` PLUS a configured
    /// `auth.platform_issuer` trusts both issuers. "Both" is therefore not a
    /// value here, and a third backend would not add one either.
    #[arg(value_enum)]
    #[config(shared = AUTH_PROVIDER, default = AuthProviderKind::Native)]
    pub auth_provider: Operational<AuthProviderKind>,

    /// Supabase Auth / GoTrue base URL used when the auth provider is Supabase.
    ///
    /// Shared with the auth service: it is the SAME URL, so one operator-visible
    /// name governs both.
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

    /// Comma-separated CIDRs a worker instance may join FROM.
    ///
    /// Half of the enrolment envelope. Control derives a worker's advertised
    /// host from the observed peer address of the enrolment connection and
    /// refuses anything this list does not cover; the registrant supplies no
    /// host at all. A default route (`0.0.0.0/0`, `::/0`) is refused as a
    /// declaration, because it is the absence of a bound spelled as one.
    ///
    /// EMPTY - the default - REFUSES EVERY ENROLMENT. It does not default open,
    /// and it does not refuse the boot either: absence disables exactly this
    /// route, the way `ServiceAuth::unconfigured` refuses each guarded edge
    /// rather than skipping the check. Refusing the boot is what
    /// `service_key_file` does, and it is right there because a control plane
    /// without it can serve no guarded edge at all.
    #[config(name = "control.worker_enrolment_networks", default = String::new())]
    pub worker_enrolment_networks: Operational<String>,

    /// Listening ports a worker instance may claim, as `<low>-<high>` or one
    /// port.
    ///
    /// The other half of the envelope, and the ONLY thing about its own address
    /// a registrant contributes. Empty - the default - refuses every enrolment,
    /// for the reason above; so does a range containing port zero.
    #[config(name = "control.worker_enrolment_ports", default = String::new())]
    pub worker_enrolment_ports: Operational<String>,

    /// JSON FILE naming every JOIN SIGNER this deployment trusts: one id, the
    /// execution zones it may mint for, and its Ed25519 PUBLIC key.
    ///
    /// Read once at startup and only ever ADDED from: a signer Control has not
    /// recorded is inserted active, a recorded one is left as it is - a REVOKED
    /// one stays revoked however long its line stays in the file - and a file
    /// that disagrees with any recorded signer, in key OR in permitted zones,
    /// refuses the boot and writes nothing.
    /// `crates/zeroship-control/src/worker_join.rs` (`import_join_signers`)
    /// carries the shape; `docs/runbooks/worker-join-signers.md` carries the
    /// operator procedure and both revocation verbs.
    ///
    /// Empty (the default) imports nothing. Every join then refuses, because no
    /// signer resolves, unless the signers were recorded by an earlier boot.
    #[config(name = "control.join_signers_file", default = PathBuf::new())]
    pub join_signers_file: Operational<PathBuf>,

    /// The signer CREDENTIAL this control plane mints join tokens with, on a
    /// single-host deployment where nobody is present to mint by hand.
    ///
    /// Empty (the default) is the multi-host shape: Control verifies join
    /// tokens without holding any key that can make one, and the operator mints
    /// with `zeroship join-token` per provisioning. Setting it makes THIS
    /// process a candidate minter - one replica is elected, see
    /// `crate::join_minter`.
    ///
    /// A PATH, not a `Secret<String>`: the loader refuses a group- or
    /// world-readable file, which is not possible once the material has become
    /// an in-memory `String`. The signer's public half must also appear in
    /// `control.join_signers_file`, or Control would refuse its own tokens.
    #[config(name = "control.join_token_signer_file", default = PathBuf::new())]
    pub join_token_signer_file: Operational<PathBuf>,

    /// Where the minted join token is written for the worker containers to
    /// read. Empty (the default) mints nothing.
    ///
    /// The file is a BEARER ARTIFACT valid for its TTL: whoever can read that
    /// volume can join a worker in that zone. Rotation bounds the window and the
    /// use cap bounds the blast radius; the trust boundary is the volume.
    #[config(name = "control.join_token_file", default = PathBuf::new())]
    pub join_token_file: Operational<PathBuf>,

    /// The execution zone the minted token admits into. Defaults to the zone
    /// every deployment declares.
    #[config(
        name = "control.join_token_zone",
        default = zeroship_core::worker_join::DEFAULT_EXECUTION_ZONE.to_owned()
    )]
    pub join_token_zone: Operational<String>,

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

    /// Supabase anon API key used for GoTrue browser/session API calls.
    ///
    /// Shared with the auth service, which drives the browser-side login
    /// against the same project. OPERATIONAL, not secret: Supabase publishes
    /// this key to browsers by design, so redacting it would claim a protection
    /// the value does not have.
    #[config(shared = AUTH_SUPABASE_ANON_KEY, default = String::new())]
    pub supabase_anon_key: Operational<String>,

    // Secrets last within the table, by convention. Each generates ONE
    // `--<name>-file` path flag and NO value flag, so no secret below can reach
    // a process argument list, and each carries its canonical environment name
    // and its canonical overlay path.
    /// `PostgreSQL` DSN for control-plane data.
    ///
    /// Secret-classed by grammar: a DSN admits userinfo, so the type cannot
    /// depend on whether a particular deployment's value carries a password.
    /// There is deliberately no compiled default - a secret default would put
    /// credential material in the binary, and `ConfigSpec::secret` has no
    /// parameter for one.
    #[config(name = "control.database_url")]
    pub database_url: Secret<String>,

    /// Admin/control API shared secret, read by four binaries.
    #[config(shared = CONTROL_KEY)]
    pub control_key: Secret<String>,

    /// PKCS#8 PEM/DER FILE holding this process's own ed25519 service key.
    ///
    /// A PATH, not a `Secret<String>`, for the same two reasons as the
    /// gateway's `signing_key_file`: the loader sniffs PEM against DER and
    /// refuses a group- or world-readable file, and neither is possible once
    /// the material has become an in-memory `String`. A path to a secret is not
    /// itself a secret.
    ///
    /// Empty (the default) REFUSES THE BOOT. A control plane that came up
    /// without it could neither mint an assertion nor verify a peer's, so every
    /// guarded internal edge would refuse and no worker could load an app -
    /// while `/readyz` and every liveness probe reported a healthy process.
    /// Absence never admits, and no longer defers either; that is the
    /// difference between this and the shared secrets it replaces, whose empty
    /// value disabled the check.
    #[config(name = "control.service_key_file", default = PathBuf::new())]
    pub service_key_file: Operational<PathBuf>,

    /// JWKS-shaped FILE holding the public key of every peer service.
    ///
    /// One document is handed to every service. `crates/zeroship-core/src/service_peers.rs`
    /// carries the shape, why the keys are configured rather than fetched from
    /// a peer, and why a shared document grants nothing beyond the ability to
    /// check a signature.
    #[config(name = "control.service_peers_file", default = PathBuf::new())]
    pub service_peers_file: Operational<PathBuf>,


    /// Master key used for control-plane encrypted env/secrets.
    #[config(name = "control.master_key")]
    pub master_key: Secret<String>,

    /// Comma-separated previous master keys accepted during a key rotation.
    ///
    /// ONE secret holding a list, not a list of secrets. Each entry used to be
    /// resolvable as its own reference, which meant a comma inside a resolved
    /// value changed the parse; the whole value is now resolved once and split
    /// once, so an entry is always a literal key.
    #[config(name = "control.legacy_master_keys")]
    pub legacy_master_keys: Secret<String>,

    /// Dedicated PERMANENT pairwise-salt secret, identical on gateway and
    /// control and never rotated without a per-app `pws_` migration.
    #[config(shared = PAIRWISE_SALT)]
    pub pairwise_salt: Secret<String>,

    /// Stripe webhook signing secret.
    #[config(name = "control.stripe_webhook_secret")]
    pub stripe_webhook_secret: Secret<String>,

    /// Stripe secret API key (`sk_...`) for outbound calls.
    #[config(name = "control.stripe_secret_key")]
    pub stripe_secret_key: Secret<String>,

    /// Billing-notification SMTP password.
    #[config(name = "control.smtp_password")]
    pub smtp_password: Secret<String>,

    /// Billing-notification Resend API key, required when the mailer is
    /// `resend`.
    #[config(name = "control.resend_api_key")]
    pub resend_api_key: Secret<String>,

    /// Supabase service-role key used for admin lookups while provisioning
    /// identity links.
    #[config(name = "auth.supabase_service_role_key")]
    pub supabase_service_role_key: Secret<String>,

    /// HS256 GoTrue JWT secret. Mutually exclusive with the Supabase JWKS URL.
    #[config(name = "auth.supabase_jwt_secret")]
    pub supabase_jwt_secret: Secret<String>,
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
    fn the_billing_safety_control_keeps_its_flag_spelling_and_gains_an_env() {
        // `--allow-unsupported-billing` is driven by e2e scripts and compose;
        // the canonical `control.` prefix is stripped by the binary scope, so
        // the flag an operator types is unchanged while the environment name
        // becomes reserved-prefixed.
        // Does not cover: whether those scripts were updated. That is a grep
        // over tests/, not something a clap Command can answer.
        let command = ControlSettingsSources::command();
        for (id, long, env) in [
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
        assert!(!*resolved.allow_unsupported_billing.get());
    }
}
