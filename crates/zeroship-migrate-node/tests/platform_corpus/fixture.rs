use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};
use tempfile::NamedTempFile;
use testcontainers::{
    core::{IntoContainerPort, WaitFor},
    runners::SyncRunner,
    Container, GenericImage, ImageExt,
};

pub fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("migration host lives under crates/")
        .to_owned()
}

pub struct Platform {
    config: NamedTempFile,
    _postgres: Container<GenericImage>,
}

impl Platform {
    pub fn start() -> Self {
        assert!(
            root()
                .join("packages/zero-migrate-cli/dist/cli-bin.js")
                .is_file(),
            "build the migration host first: cargo xtask test migrations"
        );
        let postgres = GenericImage::new("postgres", "17")
            .with_exposed_port(5432.tcp())
            .with_wait_for(WaitFor::message_on_stdout(
                "PostgreSQL init process complete; ready for start up.",
            ))
            .with_wait_for(WaitFor::message_on_stderr(
                "database system is ready to accept connections",
            ))
            .with_env_var("POSTGRES_PASSWORD", "fixture")
            .with_env_var("POSTGRES_DB", "platform_corpus")
            .with_startup_timeout(Duration::from_secs(120))
            .start()
            .expect("platform migration tests require Docker and PostgreSQL");
        eprintln!("platform corpus PostgreSQL: {}", postgres.id());
        let mut url = url::Url::parse("postgresql://postgres:fixture@localhost/platform_corpus")
            .expect("fixture URL");
        url.set_host(Some(
            &postgres.get_host().expect("container host").to_string(),
        ))
        .expect("PostgreSQL host");
        url.set_port(Some(
            postgres.get_host_port_ipv4(5432).expect("mapped port"),
        ))
        .expect("PostgreSQL port");

        // Match the operator's platform environment. NamedTempFile creates an
        // owner-only file; the credential never goes into the child arguments.
        let corpus = root().join("db/migrations-ts");
        let registry = root().join("policies/platform-table-owners.json");
        let policy = root().join("policies/platform.policy.toml");
        let env = toml::toml! {
            [env.platform]
            url = (url.as_str())
            dir = (corpus.to_str().unwrap())
            schema = "zeroship"
            owner_app = "zeroship_platform"
            registry = (registry.to_str().unwrap())
            policy = [(policy.to_str().unwrap())]
        };
        let mut config = NamedTempFile::new().expect("private migration config");
        config
            .write_all(toml::to_string(&env).expect("serialize config").as_bytes())
            .expect("write migration config");
        Self {
            config,
            _postgres: postgres,
        }
    }

    pub fn run(&self, verb: &str, flags: &[&str]) -> Output {
        let mut stdout = tempfile::tempfile().expect("CLI stdout");
        let mut stderr = tempfile::tempfile().expect("CLI stderr");
        let mut command = Command::new("node");
        // CLI environment overrides outrank its config. Do not let the user's
        // deployment settings redirect this test to another database or addon.
        remove_deployment_overrides(&mut command);
        eprintln!("platform corpus: {verb} {}", flags.join(" "));
        let mut child = OwnedChild(
            command
                .env_remove("DATABASE_URL")
                .env_remove("NODE_OPTIONS")
                .env_remove("NAPI_RS_NATIVE_LIBRARY_PATH")
                .env_remove("NAPI_RS_FORCE_WASI")
                .current_dir(root())
                .arg(root().join("packages/zero-migrate-cli/dist/cli-bin.js"))
                .arg(verb)
                .arg("--config")
                .arg(self.config.path())
                .args(["--env", "platform"])
                .args(flags)
                .stdin(Stdio::null())
                .stdout(stdout.try_clone().expect("capture stdout"))
                .stderr(stderr.try_clone().expect("capture stderr"))
                .spawn()
                .expect("migration corpus requires Node; run through nix develop"),
        );
        let deadline = Instant::now() + Duration::from_secs(300);
        let status = loop {
            if let Some(status) = child.0.try_wait().expect("wait for migration CLI") {
                break status;
            }
            assert!(
                Instant::now() < deadline,
                "migration CLI {verb} timed out: {}",
                read(&mut stderr)
            );
            std::thread::sleep(Duration::from_millis(50));
        };
        let output = Output {
            status,
            stdout: read(&mut stdout).into_bytes(),
            stderr: read(&mut stderr).into_bytes(),
        };
        assert!(
            status.success() || (verb == "lint" && status.code() == Some(1)),
            "migration CLI {verb} failed ({status}):\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        output
    }
}

#[allow(
    clippy::disallowed_methods,
    reason = "private fixture removes inherited deployment settings from a child; no application config is read"
)]
fn remove_deployment_overrides(command: &mut Command) {
    for (key, _) in std::env::vars_os() {
        let name = key.to_string_lossy();
        if name.starts_with("ZERO_MIGRATE_") || name.starts_with("PG") {
            command.env_remove(key);
        }
    }
}

fn read(file: &mut std::fs::File) -> String {
    file.seek(SeekFrom::Start(0)).expect("rewind CLI output");
    let mut output = String::new();
    file.read_to_string(&mut output).expect("read CLI output");
    output
}

struct OwnedChild(Child);

impl Drop for OwnedChild {
    fn drop(&mut self) {
        // Stop the CLI before its database and private config are dropped,
        // including when an assertion or the command deadline fails.
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
