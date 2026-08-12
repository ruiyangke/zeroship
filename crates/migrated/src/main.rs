//! zeroship-migrated - standalone creator migration service.

use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use compio_postgres::NoTls;
use ntex::web;
use zeroship_core::auth_provider::{AuthProvider, PlatformConfig, PlatformProvider};
use zeroship_migrated::auth::ControlPlaneAuthenticator;
use zeroship_migrated::policy::ManagedPolicyConfig;
use zeroship_migrated::MigrationServiceState;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[derive(Parser, Debug)]
#[command(name = "zeroship-migrated")]
struct MigratedCli {
    /// HTTP listen port.
    #[arg(long, env = "MIGRATED_PORT", default_value_t = 9091)]
    port: u16,

    /// Address to bind.
    #[arg(long, env = "MIGRATED_BIND", default_value = "127.0.0.1")]
    bind: String,

    /// PostgreSQL DSN for control-plane authz data.
    #[arg(
        long = "db",
        env = "DATABASE_URL",
        default_value = "postgres://localhost/zeroship",
        hide_env_values = true
    )]
    db: String,

    /// Privileged PostgreSQL DSN used to provision/apply per-app migrations.
    #[arg(
        long = "provision-db",
        env = "PROVISION_DATABASE_URL",
        default_value = "",
        hide_env_values = true
    )]
    provision_db: String,

    /// Admin/control API shared secret, reserved for convergence with the service mesh wiring.
    #[arg(long = "control-key", env = "CONTROL_KEY", default_value = "", hide_env_values = true)]
    control_key: String,

    /// PEM/PKCS#8 signing key file for PAT verification.
    #[arg(long = "signing-key-file", env = "SIGNING_KEY_FILE", default_value = "")]
    signing_key_file: String,

    /// Expected OAuth audience for accepted bearer tokens.
    #[arg(
        long = "oauth-audience",
        env = "CONTROL_OAUTH_AUDIENCE",
        default_value = "control.zeroship.ai"
    )]
    oauth_audience: String,

    /// Platform OP issuer for platform-issued migration-service access tokens.
    #[arg(
        long = "auth-platform-issuer",
        env = "AUTH_PLATFORM_ISSUER",
        default_value = ""
    )]
    auth_platform_issuer: String,

    /// JWKS URL for the platform OP. Defaults to {issuer}/.well-known/jwks.json.
    #[arg(
        long = "auth-platform-jwks-url",
        env = "AUTH_PLATFORM_JWKS_URL",
        default_value = ""
    )]
    auth_platform_jwks_url: String,

    /// Directory for staged request migration files.
    #[arg(long = "tmp-dir", env = "MIGRATED_TMP_DIR")]
    tmp_dir: Option<PathBuf>,

    /// HMAC key used to seal server-composed migration policy profiles.
    #[arg(
        long = "policy-seal-key",
        env = "MIGRATED_POLICY_SEAL_KEY",
        default_value = "",
        hide_env_values = true
    )]
    policy_seal_key: String,

    /// Active managed ceiling version stamped into sealed migration profiles.
    #[arg(
        long = "policy-ceiling-version",
        env = "MIGRATED_POLICY_CEILING_VERSION",
        default_value_t = 1
    )]
    policy_ceiling_version: u64,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    zeroship_core::observability::init_tracing("info,zeroship_migrated=debug");
    let cli = MigratedCli::parse();

    if cli.provision_db.trim().is_empty() {
        tracing::error!(
            "migrated: --provision-db / PROVISION_DATABASE_URL is required for per-app schema provisioning"
        );
        std::process::exit(1);
    }

    let tmp_dir = cli
        .tmp_dir
        .clone()
        .unwrap_or_else(|| std::env::temp_dir().join("zeroship-migrated"));
    std::fs::create_dir_all(&tmp_dir)?;

    let control_key_present = !cli.control_key.is_empty();
    tracing::info!(
        bind = %cli.bind,
        port = cli.port,
        control_key_present,
        tmp_dir = %tmp_dir.display(),
        "starting zeroship-migrated"
    );

    let pat_issuer = match build_pat_issuer(&cli.signing_key_file) {
        Ok(issuer) => Arc::new(issuer),
        Err(message) => {
            eprintln!("migrated: {message}");
            tracing::error!(
                error = %message,
                "migrated: refusing to start with unsafe PAT signing-key config"
            );
            std::process::exit(1);
        }
    };

    let auth_provider = match build_auth_provider(
        &cli.auth_platform_issuer,
        &cli.auth_platform_jwks_url,
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
        &cli.policy_seal_key,
        cli.policy_ceiling_version,
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
                compio_postgres::connect(&cli.db, NoTls).await.map_err(|err| {
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
                Arc::clone(&pat_issuer),
                Arc::clone(&control_pg),
                Arc::clone(&auth_provider),
                zeroship_core::auth::default_trusted_oauth_clients(),
                cli.oauth_audience,
            );
            let authenticator = Arc::new(ControlPlaneAuthenticator::new(
                control_pg,
                zeroship_authz::load_platform_policies()
                    .expect("migrated: bundled authz policies parse"),
                bearer_verifier,
            ));

            let state = Arc::new(MigrationServiceState::new(
                cli.provision_db,
                cli.db,
                tmp_dir,
                authenticator,
                policy_config,
            ));
            let bind_addr = format!("{}:{}", cli.bind, cli.port);
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
            "--policy-seal-key / MIGRATED_POLICY_SEAL_KEY is required for shared-infra \
             migration policy sealing."
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
            "--auth-platform-issuer / AUTH_PLATFORM_ISSUER is required for OAuth bearer verification."
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
    Ok(AuthProvider::Platform(PlatformProvider::new(config)))
}

fn build_pat_issuer(signing_key_file: &str) -> Result<zeroship_authn::PatIssuer, String> {
    if signing_key_file.is_empty() {
        return Err(
            "--signing-key-file / SIGNING_KEY_FILE is required for PAT verification."
                .to_string(),
        );
    }

    let signing_key =
        zeroship_authn::load_signing_key_from_path(std::path::Path::new(signing_key_file))
            .map_err(|err| format!("failed to load PAT signing key: {err}"))?;
    zeroship_authn::PatIssuer::new(&signing_key)
        .map_err(|err| format!("failed to initialize PAT issuer: {err}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pat_signing_key_refuses_missing() {
        let err = build_pat_issuer("").expect_err("missing signing key must fail closed");

        assert!(err.contains("--signing-key-file / SIGNING_KEY_FILE"));
    }

    #[test]
    fn migrated_cli_rejects_deleted_security_relaxation_flag() {
        let error = MigratedCli::try_parse_from(["zeroship-migrated", "--dev-insecure"])
            .expect_err("deleted --dev-insecure flag must be rejected");
        assert_eq!(error.kind(), clap::error::ErrorKind::UnknownArgument);
    }

    #[test]
    fn policy_config_refuses_missing_key() {
        let err = build_policy_config("", 1)
            .expect_err("missing policy seal key must fail closed");

        assert!(err.contains("--policy-seal-key / MIGRATED_POLICY_SEAL_KEY"));
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
        assert!(error.contains("AUTH_PLATFORM_ISSUER"));
    }
}
