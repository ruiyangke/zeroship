use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::Read;
use std::net::{TcpListener, TcpStream};
use std::os::unix::{
    fs::{symlink, PermissionsExt},
    process::CommandExt,
};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use ed25519_dalek::{pkcs8::EncodePrivateKey, SigningKey};
use rand::rngs::OsRng;
use serde_json::{json, Value};
use tempfile::TempDir;
use testcontainers::{
    core::{IntoContainerPort, WaitFor},
    runners::SyncRunner,
    Container, GenericImage, ImageExt,
};

pub struct Process {
    child: Child,
    log: PathBuf,
}

impl Process {
    fn start(command: &mut Command, log: PathBuf) -> Self {
        let output = File::create(&log).expect("create process log");
        let child = command
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(output.try_clone().unwrap())
            .stderr(output)
            .spawn()
            .unwrap_or_else(|error| panic!("start {:?}: {error}", command.get_program()));
        Self { child, log }
    }

    fn assert_alive(&mut self) {
        if let Some(status) = self.child.try_wait().expect("query child status") {
            panic!(
                "service exited with {status}; {}\n{}",
                self.log.display(),
                self.output()
            );
        }
    }

    fn output(&self) -> String {
        fs::read_to_string(&self.log).unwrap_or_default()
    }

    fn run(command: &mut Command, log: PathBuf) -> String {
        let mut process = Self::start(command, log);
        let deadline = Instant::now() + Duration::from_secs(1200);
        loop {
            if let Some(status) = process.child.try_wait().unwrap() {
                assert!(
                    status.success(),
                    "command failed: {}\n{}",
                    process.log.display(),
                    process.output()
                );
                return process.output();
            }
            assert!(
                Instant::now() < deadline,
                "command timed out: {}",
                process.log.display()
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        // Each child starts its own process group. Reap Vite's runtime child
        // along with its parent, using only the group this fixture created.
        let _ = nix::sys::signal::killpg(
            nix::unistd::Pid::from_raw(self.child.id().try_into().unwrap()),
            nix::sys::signal::Signal::SIGKILL,
        );
        let _ = self.child.wait();
    }
}

fn command(binary: impl AsRef<std::ffi::OsStr>, directory: &Path) -> Command {
    let mut command = Command::new(binary);
    command.current_dir(directory).env_clear();
    if let Some(path) =
        zeroship_core::declared_env_os!(external, "PATH", zeroship_core::config::TestHarness)
    {
        command.env("PATH", path);
    }
    if let Some(path) = zeroship_core::declared_env_os!(
        external,
        "LD_LIBRARY_PATH",
        zeroship_core::config::TestHarness
    ) {
        command.env("LD_LIBRARY_PATH", path);
    }
    command
}

fn secret(directory: &Path, name: &str, contents: &[u8]) -> PathBuf {
    let path = directory.join(name);
    fs::write(&path, contents).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    path
}

fn copy_app(source: &Path, target: &Path) {
    fs::create_dir_all(target).unwrap();
    for entry in fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name();
        if matches!(
            name.to_str(),
            Some("node_modules" | "dist" | ".zeroship" | "scripts")
        ) {
            continue;
        }
        if entry.file_type().unwrap().is_dir() {
            copy_app(&entry.path(), &target.join(name));
        } else {
            fs::copy(entry.path(), target.join(name)).unwrap();
        }
    }
}

fn port() -> TcpListener {
    TcpListener::bind("127.0.0.1:0").expect("reserve fixture port")
}

fn http() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(5)))
        .proxy(None)
        .build()
        .into()
}

fn wait_ready(process: &mut Process, http: &ureq::Agent, url: &str) {
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        process.assert_alive();
        if http.get(url).call().is_ok() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "not ready: {url}\n{}",
            process.output()
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

struct Workspace(Option<TempDir>);

impl Workspace {
    fn path(&self) -> &Path {
        self.0.as_ref().unwrap().path()
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        if std::thread::panicking() {
            if let Some(directory) = self.0.take() {
                eprintln!(
                    "KV deployment logs retained at {}",
                    directory.keep().display()
                );
            }
        }
    }
}

pub struct Platform {
    pub http: ureq::Agent,
    pub dev_url: String,
    pub deployed_url: String,
    processes: Vec<Process>,
    _redis: Container<GenericImage>,
    _postgres: Container<GenericImage>,
    _workspace: Workspace,
}

impl Platform {
    pub fn start() -> Self {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(2)
            .unwrap();
        let workspace = Workspace(Some(
            tempfile::Builder::new()
                .prefix("zeroship-kv-deployment-")
                .tempdir()
                .unwrap(),
        ));
        let work = workspace.path();
        eprintln!("KV deployment fixture logs: {}", work.display());

        // Cargo returns the executable paths, including custom target dirs.
        // Building here also prevents tests from exercising stale server binaries.
        eprintln!("KV fixture: build platform binaries and dashboard");
        let artifacts = Process::run(
            Command::new(env!("CARGO")).current_dir(root).args([
                "build",
                "--message-format=json",
                "-p",
                "zeroship-cli",
                "-p",
                "zeroship-worker",
                "-p",
                "zeroship-control",
                "-p",
                "zeroship-gateway",
                "-p",
                "zeroship-data-cdc-server",
                "--bins",
            ]),
            work.join("cargo.log"),
        );
        let binaries: BTreeMap<String, PathBuf> = artifacts
            .lines()
            .filter_map(|line| {
                let value: Value = serde_json::from_str(line).ok()?;
                Some((
                    value.get("target")?.get("name")?.as_str()?.to_owned(),
                    PathBuf::from(value.get("executable")?.as_str()?),
                ))
            })
            .collect();
        let app = work.join("app");
        copy_app(&root.join("examples/kv-dashboard"), &app);
        symlink(
            root.join("examples/kv-dashboard/node_modules"),
            app.join("node_modules"),
        )
        .unwrap();
        Process::run(
            command("node", &app)
                .arg(app.join("node_modules/vite/bin/vite.js"))
                .arg("build"),
            work.join("app-build.log"),
        );
        let bundle = app.join("dist/app.zship");
        let mut archive = tar::Archive::new(
            zstd::Decoder::new(File::open(&bundle).expect("built app.zship")).unwrap(),
        );
        let mut first = archive
            .entries()
            .unwrap()
            .next()
            .expect("manifest entry")
            .unwrap();
        assert_eq!(first.path().unwrap().as_ref(), Path::new("manifest.json"));
        let mut manifest = String::new();
        first.read_to_string(&mut manifest).unwrap();
        let manifest: Value = serde_json::from_str(&manifest).unwrap();
        let resources = manifest["resources"]
            .as_object()
            .expect("manifest resources");
        let rpc: Vec<_> = resources
            .iter()
            .filter(|(key, _)| key.starts_with("rpc:"))
            .collect();
        assert!(!rpc.is_empty(), "fixture must expose RPC resources");
        assert!(
            rpc.iter()
                .all(|(_, resource)| resource.get("auth").is_some()),
            "RPC resource missing auth posture"
        );

        eprintln!("KV fixture: start backing containers and apply platform migrations");
        let postgres = GenericImage::new("postgres", "18")
            .with_exposed_port(5432.tcp())
            .with_wait_for(WaitFor::message_on_stderr(
                "database system is ready to accept connections",
            ))
            .with_env_var("POSTGRES_PASSWORD", "kv-fixture-password")
            .with_env_var("POSTGRES_DB", "kv_fixture")
            .with_cmd([
                "postgres",
                "-c",
                "wal_level=logical",
                "-c",
                "max_slot_wal_keep_size=128MB",
                "-c",
                "fsync=off",
            ])
            .start()
            .expect("KV deployment tests require Docker to start PostgreSQL");
        let authority = format!(
            "{}:{}/kv_fixture",
            postgres.get_host().unwrap(),
            postgres.get_host_port_ipv4(5432).unwrap()
        );
        let dsn = format!("postgres://postgres:kv-fixture-password@{authority}");
        let migration = json!({"env": {"platform": {
            "url": dsn, "dir": root.join("db/migrations-ts"), "schema": "zeroship",
            "owner_app": "zeroship_platform", "registry": root.join("policies/platform-table-owners.json"),
            "policy": [root.join("policies/platform.policy.toml")]
        }}});
        let migration = secret(
            work,
            "migrate.toml",
            toml::to_string(&migration).unwrap().as_bytes(),
        );
        Process::run(
            command("node", root)
                .arg(root.join("packages/zero-migrate-cli/dist/cli-bin.js"))
                .args(["apply", "--config"])
                .arg(migration)
                .args(["--env", "platform", "--approve"]),
            work.join("migrate.log"),
        );

        let redis = GenericImage::new("redis", "7")
            .with_exposed_port(6379.tcp())
            .with_wait_for(WaitFor::message_on_stdout("Ready to accept connections"))
            .start()
            .expect("KV deployment tests require Docker to start Redis");
        let kv_config = format!(
            "backend = 'redis'\n[redis.topology]\nmode = 'standalone'\nendpoint = '{}:{}'\n",
            redis.get_host().unwrap(),
            redis.get_host_port_ipv4(6379).unwrap()
        );
        let kv_file = secret(work, "kv.toml", kv_config.as_bytes());

        let mut peer_keys = Vec::new();
        let mut keys = BTreeMap::new();
        for service in ["control", "worker", "gateway"] {
            let key = SigningKey::generate(&mut OsRng);
            peer_keys.push(json!({"kty":"OKP", "crv":"Ed25519", "iss":format!("spiffe://zeroship.ai/svc/{service}"), "x":URL_SAFE_NO_PAD.encode(key.verifying_key().as_bytes())}));
            keys.insert(
                service,
                secret(
                    work,
                    &format!("{service}.pem"),
                    key.to_pkcs8_pem(Default::default()).unwrap().as_bytes(),
                ),
            );
        }
        let peers = secret(
            work,
            "peers.json",
            serde_json::to_vec(&json!({"keys":peer_keys}))
                .unwrap()
                .as_slice(),
        );
        let control_key = uuid::Uuid::new_v4().simple().to_string();
        let master_key = format!(
            "{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        );
        let pairwise = uuid::Uuid::new_v4().simple().to_string();
        let broker = secret(work, "broker", master_key.as_bytes());

        let control_port = port();
        let control_url = format!("http://{}", control_port.local_addr().unwrap());
        let gateway_port = port();
        let gateway_url = format!("http://{}", gateway_port.local_addr().unwrap());
        let worker_port = port();
        let worker_url = format!("http://{}", worker_port.local_addr().unwrap());
        let relay_port = port();
        let relay_address = relay_port.local_addr().unwrap();
        let certificate = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let cert = secret(work, "relay-cert.pem", certificate.cert.pem().as_bytes());
        let key = secret(
            work,
            "relay-key.pem",
            certificate.signing_key.serialize_pem().as_bytes(),
        );
        let blobs = work.join("blobs");
        fs::create_dir_all(&blobs).unwrap();

        let service = |name: &str| {
            let mut cmd = command(&binaries[name], work);
            cmd.env("ZEROSHIP_CONTROL_KEY", &control_key)
                .env("ZEROSHIP_PAIRWISE_SALT", &pairwise)
                .env("ZEROSHIP_ORIGIN_SCHEME", "http")
                .env(
                    "ZEROSHIP_AUTH_PLATFORM_ISSUER",
                    format!("{control_url}/unused-issuer"),
                )
                .env("ZEROSHIP_OBSERVABILITY_LOG_FORMAT", "json");
            cmd
        };
        let mut processes = Vec::new();
        eprintln!("KV fixture: start platform services");
        let mut relay = service("zeroship-data-cdc-server");
        relay
            .args([
                "--no-config",
                "--listen",
                &relay_address.to_string(),
                "--tls-cert-file",
            ])
            .arg(&cert)
            .arg("--tls-key-file")
            .arg(&key)
            .env(
                "ZEROSHIP_DATA_CDC_SERVER_DATABASE_URL",
                format!("postgres://zeroship_cdc:zeroship_cdc@{authority}"),
            );
        drop(relay_port);
        processes.push(Process::start(&mut relay, work.join("relay.log")));
        let deadline = Instant::now() + Duration::from_secs(30);
        while TcpStream::connect_timeout(&relay_address, Duration::from_millis(100)).is_err() {
            processes.last_mut().unwrap().assert_alive();
            assert!(Instant::now() < deadline, "relay did not bind");
            std::thread::sleep(Duration::from_millis(100));
        }
        let mut control = service("zeroship-control");
        control
            .args([
                "--no-config",
                "--port",
                &control_port.local_addr().unwrap().port().to_string(),
                "--blob-store",
            ])
            .arg(&blobs)
            .args(["--gateway-url", &gateway_url, "--worker-urls", &worker_url])
            .env("ZEROSHIP_CONTROL_DATABASE_URL", &dsn)
            .env("ZEROSHIP_CONTROL_MASTER_KEY", &master_key)
            .env("ZEROSHIP_CONTROL_ALLOW_UNSUPPORTED_BILLING", "true")
            .env("ZEROSHIP_CONTROL_WORKER_ENROLMENT_NETWORKS", "127.0.0.1/32")
            .env(
                "ZEROSHIP_CONTROL_WORKER_ENROLMENT_PORTS",
                worker_port.local_addr().unwrap().port().to_string(),
            )
            .env("ZEROSHIP_CONTROL_SERVICE_KEY_FILE", &keys["control"])
            .env("ZEROSHIP_CONTROL_SERVICE_PEERS_FILE", &peers);
        drop(control_port);
        processes.push(Process::start(&mut control, work.join("control.log")));
        let http = http();
        wait_ready(
            processes.last_mut().unwrap(),
            &http,
            &format!("{control_url}/readyz"),
        );

        let mut worker = service("zeroship-worker");
        worker
            .args([
                "--port",
                &worker_port.local_addr().unwrap().port().to_string(),
                "--threads",
                "1",
                "--control-url",
                &control_url,
                "--blob-store",
            ])
            .arg(&blobs)
            .args(["--poll-interval", "1", "--kv-config-file"])
            .arg(kv_file)
            .env(
                "ZEROSHIP_WORKER_DATABASE_URL",
                format!("postgres://zeroship_worker:zeroship_worker@{authority}"),
            )
            .env("ZEROSHIP_WORKER_SERVICE_KEY_FILE", &keys["worker"])
            .env("ZEROSHIP_WORKER_SERVICE_PEERS_FILE", &peers)
            .env(
                "ZEROSHIP_WORKER_CDC_RELAY_URL",
                format!(
                    "wss://localhost:{}/internal/v1/cdc/subscribe",
                    relay_address.port()
                ),
            )
            .env("ZEROSHIP_WORKER_CDC_RELAY_CA_FILE", &cert);
        drop(worker_port);
        processes.push(Process::start(&mut worker, work.join("worker.log")));
        wait_ready(
            processes.last_mut().unwrap(),
            &http,
            &format!("{worker_url}/readyz"),
        );

        let mut gateway = service("zeroship-gate");
        gateway
            .args([
                "--no-config",
                "--port",
                &gateway_port.local_addr().unwrap().port().to_string(),
                "--control-url",
                &control_url,
                "--worker-urls",
                &worker_url,
                "--blob-store",
            ])
            .arg(&blobs)
            .args(["--poll-interval", "1", "--broker-secret-file"])
            .arg(broker)
            .env("ZEROSHIP_GATEWAY_DATABASE_URL", &dsn)
            .env("ZEROSHIP_GATEWAY_SIGNING_KEY_FILE", &keys["gateway"])
            .env("ZEROSHIP_GATEWAY_STASH_SIGNING_KEY", &master_key)
            .env("ZEROSHIP_GATEWAY_SERVICE_KEY_FILE", &keys["gateway"])
            .env("ZEROSHIP_GATEWAY_SERVICE_PEERS_FILE", &peers);
        drop(gateway_port);
        processes.push(Process::start(&mut gateway, work.join("gateway.log")));
        wait_ready(
            processes.last_mut().unwrap(),
            &http,
            &format!("{gateway_url}/readyz"),
        );
        Process::run(
            command(&binaries["dev-provision"], work)
                .env("DATABASE_URL", &dsn)
                .arg("--blob-store")
                .arg(&blobs)
                .args(["--name", "kvdash", "--zship"])
                .arg(bundle),
            work.join("deploy.log"),
        );

        eprintln!("KV fixture: start local Vite and wait for app dispatch");
        let dev_port = port();
        let dev_url = format!("http://{}", dev_port.local_addr().unwrap());
        let vite_port = port();
        let mut vite = command("node", &app);
        vite.arg(app.join("node_modules/vite/bin/vite.js"))
            .args([
                "--host",
                "127.0.0.1",
                "--port",
                &vite_port.local_addr().unwrap().port().to_string(),
                "--strictPort",
            ])
            .env(
                "KV_DASHBOARD_API_PORT",
                dev_port.local_addr().unwrap().port().to_string(),
            )
            .env("ZEROSHIP_BIN", &binaries["zeroship"]);
        drop(dev_port);
        drop(vite_port);
        processes.push(Process::start(&mut vite, work.join("dev.log")));
        let deployed_url = format!("{gateway_url}/apps/kvdash");
        for url in [&dev_url, &deployed_url] {
            let deadline = Instant::now() + Duration::from_secs(90);
            loop {
                for process in &mut processes {
                    process.assert_alive();
                }
                if http
                    .post(&format!("{url}/__zeroship/v1/kv.snapshot"))
                    .send_json(json!({"json":{}}))
                    .is_ok()
                {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "app did not become ready: {url}; logs in {}",
                    work.display()
                );
                std::thread::sleep(Duration::from_millis(100));
            }
        }
        Self {
            http,
            dev_url,
            deployed_url,
            processes,
            _redis: redis,
            _postgres: postgres,
            _workspace: workspace,
        }
    }

    pub fn assert_alive(&mut self) {
        for process in &mut self.processes {
            process.assert_alive();
        }
    }
}

impl Drop for Platform {
    fn drop(&mut self) {
        self.processes.clear();
    }
}
