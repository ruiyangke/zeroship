//! zeroship-migrated — standalone creator migration service.

use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use compio_postgres::NoTls;
use ntex::web;
use zeroship_control::token_handlers;
use zeroship_core::config::parse_bool_flag;
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

    /// Allow explicitly insecure local development startup.
    ///
    /// `--dev-insecure` / `--dev-insecure=true` enables; `--dev-insecure=false`
    /// disables (overriding a stray `ZEROSHIP_DEV_INSECURE=1` in the env).
    #[arg(
        long = "dev-insecure",
        env = "ZEROSHIP_DEV_INSECURE",
        num_args = 0..=1,
        default_missing_value = "true",
        value_parser = parse_bool_flag
    )]
    dev_insecure: Option<bool>,

    /// Hydra admin API base URL for OAuth introspection.
    #[arg(
        long = "hydra-admin-url",
        env = "HYDRA_ADMIN_URL",
        default_value = "http://localhost:4445"
    )]
    hydra_admin_url: String,

    /// Expected OAuth audience for accepted bearer tokens.
    #[arg(
        long = "oauth-audience",
        env = "CONTROL_OAUTH_AUDIENCE",
        default_value = "control.zeroship.ai"
    )]
    oauth_audience: String,

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
    let insecure_dev = cli.dev_insecure.unwrap_or(false);

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
        insecure_dev,
        tmp_dir = %tmp_dir.display(),
        "starting zeroship-migrated"
    );

    let pat_issuer = match build_pat_issuer(&cli.signing_key_file, insecure_dev) {
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

    let policy_config = match build_policy_config(
        &cli.policy_seal_key,
        cli.policy_ceiling_version,
        insecure_dev,
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

            let authenticator = Arc::new(ControlPlaneAuthenticator::new(
                Arc::new(control_pg),
                zeroship_authz::load_platform_policies()
                    .expect("migrated: bundled authz policies parse"),
                pat_issuer,
                Arc::new(zeroship_core::hydra::HydraIntrospector::new(
                    &cli.hydra_admin_url,
                )),
                cli.oauth_audience,
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
    insecure_dev: bool,
) -> Result<ManagedPolicyConfig, String> {
    let key = if policy_seal_key.is_empty() {
        if insecure_dev {
            tracing::warn!("migrated: --dev-insecure set; using dev-only migration policy seal key");
            b"migrated dev-only policy seal key 32 bytes min".to_vec()
        } else {
            return Err(
                "--policy-seal-key / MIGRATED_POLICY_SEAL_KEY is required for shared-infra \
                 migration policy sealing. Pass --dev-insecure only for local development."
                    .to_string(),
            );
        }
    } else {
        policy_seal_key.as_bytes().to_vec()
    };

    ManagedPolicyConfig::default_confined(key, ceiling_version)
        .map_err(|err| format!("invalid migration policy seal config: {err}"))
}

fn build_pat_issuer(
    signing_key_file: &str,
    insecure_dev: bool,
) -> Result<token_handlers::PatIssuer, String> {
    if signing_key_file.is_empty() {
        if insecure_dev {
            tracing::warn!("migrated: --dev-insecure set; using dev-only PAT signing key");
            return Ok(token_handlers::PatIssuer::dev_insecure());
        }
        return Err(
            "--signing-key-file / SIGNING_KEY_FILE is required for PAT verification. \
             Pass --dev-insecure (or ZEROSHIP_DEV_INSECURE=1) to run without it — NEVER in production."
                .to_string(),
        );
    }

    let signing_key =
        token_handlers::load_signing_key_from_path(std::path::Path::new(signing_key_file))
            .map_err(|err| format!("failed to load PAT signing key: {err}"))?;
    token_handlers::PatIssuer::new(&signing_key)
        .map_err(|err| format!("failed to initialize PAT issuer: {err}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pat_signing_key_refuses_missing_without_dev_insecure() {
        let err = build_pat_issuer("", false).expect_err("missing signing key must fail closed");

        assert!(err.contains("--signing-key-file / SIGNING_KEY_FILE"));
        assert!(err.contains("--dev-insecure"));
    }

    #[test]
    fn pat_signing_key_allows_missing_with_explicit_dev_insecure() {
        build_pat_issuer("", true).expect("explicit dev-insecure allows dev PAT key");
    }

    #[test]
    fn dev_insecure_cli_false_overrides_env_one() {
        static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = ENV_LOCK.lock().expect("env lock");
        let old = std::env::var_os("ZEROSHIP_DEV_INSECURE");
        std::env::set_var("ZEROSHIP_DEV_INSECURE", "1");

        let cli =
            MigratedCli::try_parse_from(["zeroship-migrated"]).expect("parse with env only");
        assert_eq!(cli.dev_insecure, Some(true));

        let cli = MigratedCli::try_parse_from(["zeroship-migrated", "--dev-insecure=false"])
            .expect("parse explicit false");
        assert_eq!(cli.dev_insecure, Some(false));

        match old {
            Some(value) => std::env::set_var("ZEROSHIP_DEV_INSECURE", value),
            None => std::env::remove_var("ZEROSHIP_DEV_INSECURE"),
        }
    }

    #[test]
    fn policy_config_refuses_missing_key_without_dev_insecure() {
        let err = build_policy_config("", 1, false)
            .expect_err("missing policy seal key must fail closed");

        assert!(err.contains("--policy-seal-key / MIGRATED_POLICY_SEAL_KEY"));
    }

    #[test]
    fn policy_config_allows_missing_key_with_explicit_dev_insecure() {
        build_policy_config("", 1, true)
            .expect("explicit dev-insecure allows dev policy seal key");
    }
}
