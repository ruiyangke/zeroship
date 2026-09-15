#![allow(
    clippy::future_not_send,
    reason = "fixture clients and connections stay on their compio runtime"
)]

use std::{os::unix::fs::PermissionsExt, path::Path, process::Command};
use testcontainers::{
    core::{IntoContainerPort, WaitFor},
    runners::SyncRunner,
    Container, GenericImage, ImageExt,
};

pub struct Platform {
    _postgres: Container<GenericImage>,
    pub admin: compio_postgres::Client,
    pub runtime_url: String,
    pub work: tempfile::TempDir,
    /// An `active` `zeroship.worker_join_signers` row this fixture seeds once,
    /// so every test that joins a worker instance directly (`worker_instances.
    /// join_signer_id` is NOT NULL with a restrict FK to this table) has a
    /// satisfying value to reference without minting its own signer per call
    /// site. Tests exercising signer-level behaviour itself (revocation, a
    /// second signer) seed their own rows instead.
    #[allow(
        dead_code,
        reason = "Control's deployment-hold suite includes this file and joins through a \
                  signer whose private key it holds, so it never reads this row"
    )]
    pub default_join_signer_id: String,
}
impl Platform {
    pub async fn new() -> Self {
        let postgres = GenericImage::new("postgres", "18")
            .with_exposed_port(5432.tcp())
            .with_wait_for(WaitFor::message_on_stdout(
                "PostgreSQL init process complete; ready for start up.",
            ))
            .with_wait_for(WaitFor::message_on_stderr(
                "database system is ready to accept connections",
            ))
            .with_env_var("POSTGRES_HOST_AUTH_METHOD", "trust")
            .start()
            .expect("workflow host tests require Testcontainers PostgreSQL");
        let address = format!(
            "{}:{}",
            postgres.get_host().unwrap(),
            postgres.get_host_port_ipv4(5432).unwrap()
        );
        let url = format!("postgres://postgres@{address}/postgres");
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .unwrap();
        let work = tempfile::tempdir().unwrap();
        let config = work.path().join("migrate.toml");
        write_private(&config, toml::to_string(&serde_json::json!({"env":{"platform":{
            "url":url,"dir":root.join("db/migrations-ts"),"schema":"zeroship","owner_app":"zeroship_platform",
            "registry":root.join("policies/platform-table-owners.json"),"policy":[root.join("policies/platform.policy.toml")],
        }}})).unwrap());
        let result = Command::new("node")
            .arg(root.join("packages/zero-migrate-cli/dist/cli-bin.js"))
            .args(["apply", "--config"])
            .arg(&config)
            .args(["--env", "platform", "--approve"])
            .current_dir(&root)
            .output()
            .expect("build the canonical migration CLI before testing");
        assert!(
            result.status.success(),
            "platform migration failed:\n{}\n{}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
        let admin = connect(&url).await;
        let default_join_signer_id = "wjs_testfixturedefault0000000".to_string();
        admin
            .execute(
                "INSERT INTO zeroship.worker_join_signers (id, public_key, status) \
                 VALUES ($1, $2, 'active')",
                &[&default_join_signer_id, &vec![7_u8; 32]],
            )
            .await
            .unwrap();
        admin
            .execute(
                "INSERT INTO zeroship.worker_join_signer_zones (signer_id, execution_zone_id) \
                 VALUES ($1, 'ezn_default000000000000000000')",
                &[&default_join_signer_id],
            )
            .await
            .unwrap();
        Self {
            _postgres: postgres,
            admin,
            runtime_url: format!("postgres://zeroship_workflow@{address}/postgres"),
            work,
            default_join_signer_id,
        }
    }
}
pub async fn connect(url: &str) -> compio_postgres::Client {
    let (client, connection) = compio_postgres::connect(url, compio_postgres::NoTls)
        .await
        .unwrap();
    compio::runtime::spawn(async move {
        let _ = connection.run().await;
    })
    .detach();
    client
}
pub fn write_private(path: &Path, bytes: impl AsRef<[u8]>) {
    std::fs::write(path, bytes).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
}
