//! Gateway's generated settings: bootstrap, command, observability, operational.
//!
//! In the library rather than `main.rs` so the compiled configuration checker
//! can link the declaration and invoke clap's `CommandFactory` against it.
//!
//! Every input the gateway takes is declared here, secrets included. A
//! `Secret<T>` field generates only a `--<name>-file PATH` flag, so no gateway
//! credential can reach argv; the value tiers are the canonical `ZEROSHIP_*`
//! environment name and the canonical TOML path.

use std::path::{Path, PathBuf};

use zeroship_core::config::{
    zeroship_config, BootstrapControl, CheckFormat, CommandControl, ObservabilityControls,
    Operational, OriginScheme, OverlaySelector, Secret, TrustedOrigin,
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

    /// Admin/control API shared secret.
    #[config(shared = CONTROL_KEY)]
    pub control_key: Secret<String>,

    /// Shared secret for worker admin endpoints.
    #[config(shared = WORKER_KEY)]
    pub worker_key: Secret<String>,

    /// `PostgreSQL` DSN for gateway session validation. A DSN grammar admits
    /// userinfo, so it is secret-classed whether or not a given value carries a
    /// password.
    #[config(name = "gateway.database_url")]
    pub database_url: Secret<String>,

    /// HMAC key for short-lived OIDC stash cookies.
    #[config(name = "gateway.stash_signing_key")]
    pub stash_signing_key: Secret<String>,

    /// Dedicated PERMANENT pairwise-salt secret (auth-sdk 6.2). The seed for
    /// every app's `pws_` per-app identity anchor, independent of the rotatable
    /// stash key. MUST be identical on auth + gateway + control and MUST NOT be
    /// rotated without a per-app `pws_` migration.
    ///
    /// One field, not two. The pre-conversion declaration had a
    /// `--pairwise-salt` value flag AND a `--pairwise-salt-file` path flag with
    /// a hand-written file-wins-over-value dance between them; a secret's
    /// generated supply set is exactly that precedence with no second field to
    /// keep in step, and the value flag is gone because a secret must never
    /// travel through argv.
    #[config(shared = PAIRWISE_SALT)]
    pub pairwise_salt: Secret<String>,

    /// Platform broker master-secret source FILE, byte-identical to auth's.
    ///
    /// The gateway derives per-app `oac_` client secrets from these bytes when
    /// brokering authorization-code, refresh, and revoke requests to the
    /// platform OP, and auth derives the same per-client secrets from the same
    /// file. The two derivations must see the SAME BYTES.
    ///
    /// A PATH, therefore, for the same two reasons as `signing_key_file`, and
    /// this field briefly was not. Resolving it as a `Secret<String>` read the
    /// file through `read_to_string` and stripped one trailing newline, while
    /// auth's `load_broker_master_secret` reads raw bytes and strips nothing.
    /// So `openssl rand -base64 48 > secret` gave the gateway 64 bytes and the
    /// OP 65, and `head -c 32 /dev/urandom > secret` - the recipe auth's own
    /// documentation gives - is not UTF-8 and failed the gateway outright. It
    /// also dropped `reject_insecure_permissions`, which has nothing to inspect
    /// once the material is a `String`. A path to a secret is not itself a
    /// secret.
    #[config(name = "gateway.broker_secret_file", default = PathBuf::new())]
    pub broker_secret_file: Operational<PathBuf>,

    /// PEM/PKCS#8 signing key FILE for the gateway-signed session cookie.
    ///
    /// A PATH, deliberately, and NOT a `Secret<String>`. `signing::load_from_path`
    /// sniffs PEM-versus-DER and calls `reject_insecure_permissions(path)` on the
    /// file it opened; turning the contents into in-memory secret material would
    /// drop the permission check and break DER keys, which are not UTF-8. A path
    /// to a secret is not itself a secret.
    #[config(name = "gateway.signing_key_file", default = PathBuf::new())]
    pub signing_key_file: Operational<PathBuf>,

    /// PEM/PKCS#8 PREVIOUS signing key FILE for the session-cookie rotation
    /// overlap (auth-sdk 8.5). Set ONLY during a key roll: the Verifier then
    /// accepts session cookies signed by EITHER the current or this previous
    /// key. The Issuer always signs with the current key only. Empty (the
    /// default) means a single-key Verifier. A path, for the same reason as
    /// `signing_key_file`.
    #[config(name = "gateway.prev_signing_key_file", default = PathBuf::new())]
    pub prev_signing_key_file: Operational<PathBuf>,

    /// PKCS#8 PEM/DER FILE holding this process's own ed25519 service key.
    ///
    /// DISTINCT from `signing_key_file`, deliberately. That key signs an
    /// END-USER session cookie; this one asserts WHICH SERVICE is calling. One
    /// key doing both is the shape this whole change exists to remove, so they
    /// are two settings and two files.
    ///
    /// A PATH for the same reasons as `signing_key_file`. Empty (the default)
    /// means this process can neither mint an assertion nor verify a peer's, so
    /// every internal edge guarded by one REFUSES. Absence never admits.
    #[config(name = "gateway.service_key_file", default = PathBuf::new())]
    pub service_key_file: Operational<PathBuf>,

    /// JWKS-shaped FILE holding the public key of every peer service.
    ///
    /// One document is handed to every service. `crates/zeroship-core/src/service_peers.rs`
    /// carries the shape, why the keys are configured rather than fetched from
    /// a peer, and why a shared document grants nothing beyond the ability to
    /// check a signature.
    #[config(name = "gateway.service_peers_file", default = PathBuf::new())]
    pub service_peers_file: Operational<PathBuf>,

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
    ///
    /// Named here rather than in the shared table because the gateway is its
    /// only consumer. `shared = SYMBOL` exists to stop an identity declared in
    /// SEVERAL binaries from de-sharing on a typo; for one consumer it buys
    /// nothing and moves the canonical name away from the field it describes.
    #[arg(value_delimiter = ',')]
    #[config(name = "trusted_origins", default = Vec::new())]
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

    /// Kafka-wire brokers for the usage-event stream, e.g. `redpanda:9092`.
    /// Empty disables the gateway's producer (drain and drop), which the boot
    /// log says in as many words.
    #[config(shared = METERING_BROKERS, default = String::new())]
    pub metering_brokers: Operational<String>,

    /// Topic the usage-event producer publishes to.
    #[config(shared = METERING_EVENTS_TOPIC,
             default = zeroship_metering::DEFAULT_USAGE_EVENTS_TOPIC.to_owned())]
    pub metering_events_topic: Operational<String>,

    /// Consumer-group id override for the producer. Empty derives one from the
    /// per-boot producer source.
    #[config(shared = METERING_PRODUCER_GROUP_ID, default = String::new())]
    pub metering_producer_group_id: Operational<String>,

    /// redb write-ahead-log path for the usage outbox. Empty derives one from
    /// the stable per-host WAL identity. Must differ from the worker's on a
    /// shared host: redb is single-writer.
    #[config(shared = METERING_OUTBOX_WAL_PATH, default = String::new())]
    pub metering_outbox_wal_path: Operational<String>,
}

/// The gateway's usage-stream producer settings, resolved. The peer of the
/// worker's `usage_stream_settings`; both producers read one identity.
#[must_use]
pub fn usage_stream_settings(settings: &GateSettings) -> zeroship_metering::UsageStreamSettings {
    zeroship_metering::UsageStreamSettings::from_resolved(
        settings.metering_brokers.get(),
        settings.metering_events_topic.get(),
        settings.metering_producer_group_id.get(),
        settings.metering_outbox_wal_path.get(),
    )
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
