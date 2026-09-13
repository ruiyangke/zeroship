//! Restore worker cases from this process's actual platform migrations.

use super::{database_url, image};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use testcontainers::core::{CmdWaitFor, ExecCommand};
use testcontainers::{runners::SyncRunner, Container, GenericImage, ImageExt};

pub(super) struct Seed {
    pub(super) roles: Vec<u8>,
    pub(super) database: Vec<u8>,
}

impl Seed {
    pub(super) fn build() -> Self {
        eprintln!("worker fixture: preparing the platform database seed");
        let postgres = image()
            .with_env_var("POSTGRES_DB", "worker_tests")
            .start()
            .expect("prepare the migrated worker database seed");
        apply_migrations(database_url(&postgres).as_str());
        eprintln!("worker fixture: platform migrations applied; capturing the seed");
        let roles = dump(
            &postgres,
            &["pg_dumpall", "-U", "postgres", "--globals-only"],
        );
        // initdb already created postgres. Preserve its dumped attributes
        // and every other role; only the redundant creation is omitted.
        let roles = String::from_utf8(roles)
            .expect("UTF-8 role dump")
            .replacen("CREATE ROLE postgres;\n", "", 1)
            .into_bytes();
        let database = dump(
            &postgres,
            &[
                "pg_dump",
                "-U",
                "postgres",
                "--create",
                "--dbname",
                "worker_tests",
            ],
        );
        postgres.rm().expect("remove migration seed container");
        Self { roles, database }
    }
}

fn dump(postgres: &Container<GenericImage>, arguments: &[&str]) -> Vec<u8> {
    let mut result = postgres
        .exec(
            ExecCommand::new(arguments.iter().copied())
                .with_cmd_ready_condition(CmdWaitFor::exit_code(0)),
        )
        .expect("dump the migrated platform database");
    result.stdout_to_vec().expect("read database seed")
}

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("worker crate lives under crates/")
        .to_owned()
}

fn apply_migrations(url: &str) {
    let root = root();
    let cli = root.join("packages/zero-migrate-cli/dist/cli-bin.js");
    assert!(
        cli.is_file(),
        "build the migration host: cargo xtask test migrations"
    );
    let corpus = root.join("db/migrations-ts");
    let registry = root.join("policies/platform-table-owners.json");
    let policy = root.join("policies/platform.policy.toml");
    let env = toml::toml! {
        [env.platform]
        url = (url)
        dir = (corpus.to_str().unwrap())
        schema = "zeroship"
        owner_app = "zeroship_platform"
        registry = (registry.to_str().unwrap())
        policy = [(policy.to_str().unwrap())]
    };
    let mut config = tempfile::NamedTempFile::new().expect("private migration config");
    config
        .write_all(toml::to_string(&env).unwrap().as_bytes())
        .unwrap();
    let mut stdout = tempfile::tempfile().expect("migration stdout");
    let mut stderr = tempfile::tempfile().expect("migration stderr");
    let mut command = Command::new("node");
    remove_deployment_overrides(&mut command);
    let mut child = OwnedChild(
        command
            .current_dir(root)
            .arg(cli)
            .args(["apply", "--config"])
            .arg(config.path())
            .args(["--env", "platform", "--approve"])
            .stdin(Stdio::null())
            .stdout(stdout.try_clone().unwrap())
            .stderr(stderr.try_clone().unwrap())
            .spawn()
            .expect("worker database fixtures require Node; run through nix develop"),
    );
    let deadline = Instant::now() + Duration::from_secs(300);
    let status = loop {
        if let Some(status) = child.0.try_wait().expect("wait for migrations") {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "platform migrations timed out: {}",
            read(&mut stderr)
        );
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(
        status.success(),
        "platform migrations failed ({status}):\n{}\n{}",
        read(&mut stdout),
        read(&mut stderr)
    );
}

#[allow(
    clippy::disallowed_methods,
    reason = "remove deployment overrides from an owned fixture child"
)]
fn remove_deployment_overrides(command: &mut Command) {
    for (key, _) in std::env::vars_os() {
        let name = key.to_string_lossy();
        if name.starts_with("ZERO_MIGRATE_")
            || name.starts_with("PG")
            || matches!(
                name.as_ref(),
                "DATABASE_URL"
                    | "NODE_OPTIONS"
                    | "NAPI_RS_NATIVE_LIBRARY_PATH"
                    | "NAPI_RS_FORCE_WASI"
            )
        {
            command.env_remove(key);
        }
    }
}

fn read(file: &mut std::fs::File) -> String {
    file.seek(SeekFrom::Start(0)).unwrap();
    let mut output = String::new();
    file.read_to_string(&mut output).unwrap();
    output
}

struct OwnedChild(Child);

impl Drop for OwnedChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
