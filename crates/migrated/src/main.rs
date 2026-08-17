//! zeroship-migrated - standalone creator migration service.

use std::sync::Arc;

use clap::Parser;
use compio_postgres::NoTls;
use ntex::web;
use zeroship_core::auth_provider::{AuthProvider, PlatformConfig, PlatformProvider};
use zeroship_core::config::{
    bootstrap_or_exit, CheckConfigReport, CheckValue,
};
use zeroship_migrated::auth::ControlPlaneAuthenticator;
use zeroship_migrated::config::{MigratedSettings, MigratedSettingsSources, DEFAULT_LOG_FILTER};
use zeroship_migrated::policy::ManagedPolicyConfig;
use zeroship_migrated::MigrationServiceState;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (settings, boot) = bootstrap_or_exit::<MigratedSettings>(
        MigratedSettingsSources::parse(),
        DEFAULT_LOG_FILTER,
        "migrated",
    );
    let check_config = *settings.check_config.get();
    let tmp_dir = settings.tmp_dir.get().clone();

    // A read-only dry run: report what was configured and exit BEFORE the tmp
    // directory is created, before the control DSN is dialled, and before a
    // listener is bound. Each of those is a side effect a config check must not
    // have, and each of them used to run unconditionally here.
    if check_config {
        let mut report = CheckConfigReport::new();
        report.field("bind", CheckValue::Plain(settings.bind.get().clone()));
        report.field("port", CheckValue::Count(usize::from(*settings.port.get())));
        report.field(
            "config_source",
            CheckValue::Plain(boot.overlay.source.to_string()),
        );
        report.field("log_filter", CheckValue::Plain(boot.log_filter.clone()));
        report.field("log_format", CheckValue::Plain(boot.log_format.to_string()));
        report.field("tmp_dir", CheckValue::Plain(tmp_dir.display().to_string()));
        report.field(
            "db_configured",
            CheckValue::Secret(settings.database_url.is_configured()),
        );
        report.field(
            "provision_db_configured",
            CheckValue::Secret(settings.provision_database_url.is_configured()),
        );
        report.field(
            "control_key_configured",
            CheckValue::Secret(settings.control_key.is_configured()),
        );
        report.field(
            "policy_seal_key_configured",
            CheckValue::Secret(settings.policy_seal_key.is_configured()),
        );
        report.field(
            "policy_ceiling_version",
            CheckValue::Count(
                usize::try_from(*settings.policy_ceiling_version.get()).unwrap_or(usize::MAX),
            ),
        );
        report.field(
            "oauth_audience",
            CheckValue::Plain(settings.oauth_audience.get().clone()),
        );
        report.field(
            "auth_platform_issuer",
            CheckValue::Plain(settings.auth_platform_issuer.get().clone()),
        );
        report.field(
            "auth_platform_jwks_url",
            CheckValue::Plain(settings.auth_platform_jwks_url.get().clone()),
        );
        report.emit(*settings.check_config_format.get());
        return Ok(());
    }

    // Every secret below is the RESOLVED material: on this path the run is not
    // a dry run, so `resolve_secret_sources` has already dereferenced the path
    // flag or the file reference that supplied it.
    let database_url = settings.database_url.expose_str().to_owned();
    let provision_database_url = settings.provision_database_url.expose_str().to_owned();
    if provision_database_url.trim().is_empty() {
        tracing::error!(
            "migrated: --provision-database-url-file / ZEROSHIP_MIGRATED_PROVISION_DATABASE_URL is required for per-app schema provisioning"
        );
        std::process::exit(1);
    }

    std::fs::create_dir_all(&tmp_dir)?;

    let control_key_present = settings.control_key.is_configured();
    tracing::info!(
        bind = %settings.bind.get(),
        port = *settings.port.get(),
        control_key_present,
        tmp_dir = %tmp_dir.display(),
        "starting zeroship-migrated"
    );
    let auth_provider = match build_auth_provider(
        settings.auth_platform_issuer.get(),
        settings.auth_platform_jwks_url.get(),
    ) {
        Ok(provider) => Arc::new(provider),
        Err(message) => {
            eprintln!("migrated: {message}");
            tracing::error!(
                error = %message,
                "migrated: refusing to start with invalid auth provider config"
            );
            std::process::exit(1);
        }
    };

    let policy_config = match build_policy_config(
        settings.policy_seal_key.expose_str(),
        *settings.policy_ceiling_version.get(),
    ) {
        Ok(config) => config,
        Err(message) => {
            eprintln!("migrated: {message}");
            tracing::error!(
                error = %message,
                "migrated: refusing to start with unsafe migration policy seal config"
            );
            std::process::exit(1);
        }
    };

    Ok(ntex::rt::System::build()
        .name("zeroship-migrated")
        .build(ntex::rt::DefaultRuntime)
        .block_on(async move {
            let (control_pg, control_conn) =
                compio_postgres::connect(&database_url, NoTls).await.map_err(|err| {
                    tracing::error!(error = %err, "migrated: control-pg connect failed");
                    std::io::Error::other(err.to_string())
                })?;
            compio::runtime::spawn(async move {
                if let Err(err) = control_conn.run().await {
                    tracing::error!(error = %err, "migrated/control-pg connection ended");
                }
            })
            .detach();

            let control_pg = Arc::new(control_pg);
            let bearer_verifier = zeroship_authn::BearerVerifier::new(
                Arc::clone(&control_pg),
                Arc::clone(&auth_provider),
                zeroship_core::auth::default_trusted_oauth_clients(),
                settings.oauth_audience.get().clone(),
            );
            let authenticator = Arc::new(ControlPlaneAuthenticator::new(
                control_pg,
                zeroship_authz::load_platform_policies()
                    .expect("migrated: bundled authz policies parse"),
                bearer_verifier,
            ));

            let state = Arc::new(MigrationServiceState::new(
                provision_database_url,
                database_url,
                tmp_dir,
                authenticator,
                policy_config,
            ));
            let bind_addr = format!("{}:{}", settings.bind.get(), settings.port.get());
            tracing::info!(bind = %bind_addr, "zeroship-migrated listening");
            web::server(async move || {
                web::App::new()
                    .state(state.clone())
                    .configure(zeroship_migrated::configure)
            })
            .bind(&bind_addr)?
            .run()
            .await
        })?)
}

fn build_policy_config(
    policy_seal_key: &str,
    ceiling_version: u64,
) -> Result<ManagedPolicyConfig, String> {
    if policy_seal_key.is_empty() {
        return Err(
            "--policy-seal-key-file / ZEROSHIP_MIGRATED_POLICY_SEAL_KEY is required for \
             shared-infra migration policy sealing."
                .to_string(),
        );
    }

    ManagedPolicyConfig::default_confined(policy_seal_key.as_bytes().to_vec(), ceiling_version)
        .map_err(|err| format!("invalid migration policy seal config: {err}"))
}

fn build_auth_provider(
    platform_issuer: &str,
    platform_jwks_url: &str,
) -> Result<AuthProvider, String> {
    if platform_issuer.trim().is_empty() {
        return Err(
            "--auth-platform-issuer / ZEROSHIP_AUTH_PLATFORM_ISSUER is required for OAuth \
             bearer verification."
                .to_string(),
        );
    }

    let jwks_url = if platform_jwks_url.trim().is_empty() {
        None
    } else {
        Some(platform_jwks_url.to_string())
    };
    let config = PlatformConfig::new(platform_issuer.to_string(), jwks_url)
        .map_err(|err| format!("invalid platform auth provider config: {err}"))?;
    Ok(AuthProvider::platform(PlatformProvider::new(config)))
}

#[cfg(test)]
mod tests {
    use super::*;

    use zeroship_core::config::env_like_tokens;
    use zeroship_core::config::GeneratedConfig;

    #[test]
    fn policy_config_refuses_missing_key() {
        let err = build_policy_config("", 1)
            .expect_err("missing policy seal key must fail closed");

        assert!(err.contains("--policy-seal-key-file / ZEROSHIP_MIGRATED_POLICY_SEAL_KEY"));
    }

    #[test]
    fn policy_config_refuses_one_byte_key() {
        let result = build_policy_config("x", 1);
        assert!(result.is_err(), "one-byte policy seal key must fail closed");
        let err = result.expect_err("one-byte policy seal key must fail closed");

        assert!(err.contains("at least 32 bytes"));
        assert!(err.contains("got 1"));
    }

    #[test]
    fn empty_platform_issuer_is_rejected() {
        let error = build_auth_provider("", "").expect_err("missing issuer must fail closed");
        assert!(error.contains("ZEROSHIP_AUTH_PLATFORM_ISSUER"));
    }

    /// Every environment variable name `zeroship-migrated` actually reads.
    ///
    /// DERIVED, never listed - see the twin of this helper in
    /// `crates/control/src/main.rs` for why a list would defeat the point.
    fn env_names_migrated_reads() -> std::collections::BTreeSet<String> {
        let mut names = std::collections::BTreeSet::new();
        let command = <MigratedSettingsSources as clap::CommandFactory>::command();
        for arg in command.get_arguments() {
            if let Some(env) = arg.get_env() {
                names.insert(env.to_string_lossy().into_owned());
            }
        }
        for spec in MigratedSettings::SPECS {
            if let Some(env) = spec.env_name() {
                names.insert(env);
            }
        }
        names
    }


    #[test]
    fn every_startup_diagnostic_names_a_variable_migrated_reads() {
        // Same defect as control's: the issuer variable gained a ZEROSHIP_
        // prefix and this diagnostic kept the old spelling, so an operator who
        // followed it set a variable migrated does not read.
        //
        // What this does NOT catch: a diagnostic naming a variable migrated
        // does read but which is the wrong one for the failure, and any
        // diagnostic outside the three driven below.
        let readable = env_names_migrated_reads();
        assert!(
            readable.contains("ZEROSHIP_AUTH_PLATFORM_ISSUER"),
            "the derivation itself is broken: the issuer setting is absent"
        );

        let diagnostics = [
            build_auth_provider("", "").expect_err("missing issuer must fail closed"),
            build_policy_config("", 1).map(|_| ()).expect_err("missing seal key must fail closed"),
        ];
        for diagnostic in diagnostics {
            let tokens = env_like_tokens(&diagnostic);
            assert!(
                !tokens.is_empty(),
                "diagnostic names no variable at all: {diagnostic:?}"
            );
            for token in tokens {
                assert!(
                    readable.contains(&token),
                    "diagnostic {diagnostic:?} tells the operator to set {token}, \
                     which zeroship-migrated does not read"
                );
            }
        }
    }

    #[test]
    fn the_diagnostic_check_rejects_a_variable_migrated_does_not_read() {
        // One-variable control for the test above: same scanner, same readable
        // set, one thing changed - a name nothing declares.
        let readable = env_names_migrated_reads();
        assert_eq!(
            env_like_tokens("set AUTH_PLATFORM_ISSUER first"),
            vec!["AUTH_PLATFORM_ISSUER".to_owned()],
            "the token scanner must see the unprefixed name"
        );
        assert!(
            !readable.contains("AUTH_PLATFORM_ISSUER"),
            "the unprefixed spelling must not be a name migrated reads"
        );
    }
}
