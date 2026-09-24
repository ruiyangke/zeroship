use ntex::{client::Client, http::StatusCode};
#[path = "queue_control.rs"]
mod queue_control;
use std::{
    net::TcpListener,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::Duration,
};

pub struct ServerProcess {
    child: Child,
    _control: queue_control::Control,
    config: PathBuf,
    log: PathBuf,
    pub url: String,
    pub address: std::net::SocketAddr,
}
impl Drop for ServerProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
impl ServerProcess {
    pub async fn start(
        database: &str,
        peers: &Path,
        directory: &Path,
        name: &str,
        client: &Client,
    ) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let config = directory.join(format!("{name}.toml"));
        let control = queue_control::Control::start(directory, name, database).await;
        super::platform::write_private(
            &config,
            toml::to_string(&serde_json::json!({"workflow":{
                "listen":address.to_string(),"database_url":database,"service_peers_file":peers,
                "control_url":control.url(),"service_key_file":control.key_file,
                "http_threads":2,"max_request_bytes":1024,
            }}))
            .unwrap(),
        );
        let log = directory.join(format!("{name}.log"));
        let child = spawn(&config, &log);
        let mut server = Self {
            child,
            _control: control,
            config,
            log,
            url: format!("http://{address}"),
            address,
        };
        server.ready(client).await;
        server
    }
    pub async fn ready(&mut self, client: &Client) {
        let result = compio::time::timeout(Duration::from_secs(30), async {
            loop {
                assert!(
                    self.child.try_wait().unwrap().is_none(),
                    "{}",
                    std::fs::read_to_string(&self.log).unwrap()
                );
                if let Ok(response) = client.get(format!("{}/readyz", self.url)).send().await {
                    if response.status() == StatusCode::OK {
                        break;
                    }
                }
                compio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await;
        assert!(
            result.is_ok(),
            "coordinator did not become ready: {}",
            std::fs::read_to_string(&self.log).unwrap()
        );
    }
    pub async fn restart(&mut self, client: &Client) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        self.child = spawn(&self.config, &self.log);
        self.ready(client).await;
    }
    pub async fn expect_failure(&mut self) {
        let status = compio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Some(status) = self.child.try_wait().unwrap() {
                    break status;
                }
                compio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("coordinator retained a failed authentication database connection");
        assert!(!status.success());
    }
}
fn spawn(config: &Path, log: &Path) -> Child {
    let output = std::fs::File::create(log).unwrap();
    Command::new(env!("CARGO_BIN_EXE_zeroship-workflow-server"))
        .env_clear()
        .arg("--config")
        .arg(config)
        .stdin(Stdio::null())
        .stdout(output.try_clone().unwrap())
        .stderr(output)
        .spawn()
        .unwrap()
}
