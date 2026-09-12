//! An owned S3-compatible server with Docker-assigned ports.

use std::time::Duration;

use testcontainers::{
    core::{CmdWaitFor, ExecCommand, IntoContainerPort, WaitFor},
    runners::SyncRunner,
    Container, GenericImage, ImageExt,
};

const ACCESS: &str = "minioadmin";
const SECRET: &str = "minioadmin";
const BUCKET: &str = "storage-fixture";

pub struct Minio {
    _container: Container<GenericImage>,
    endpoint: String,
}

impl Minio {
    pub fn start() -> Self {
        let container = GenericImage::new("minio/minio", "latest")
            .with_exposed_port(9000.tcp())
            .with_wait_for(WaitFor::message_on_stderr("API:"))
            .with_env_var("MINIO_ROOT_USER", ACCESS)
            .with_env_var("MINIO_ROOT_PASSWORD", SECRET)
            .with_cmd(["server", "/data"])
            .with_startup_timeout(Duration::from_secs(90))
            .start()
            .expect("S3 tests require Docker and MinIO");
        for command in [
            vec![
                "mc",
                "alias",
                "set",
                "fixture",
                "http://127.0.0.1:9000",
                ACCESS,
                SECRET,
            ],
            vec!["mc", "mb", "fixture/storage-fixture"],
        ] {
            container
                .exec(ExecCommand::new(command).with_cmd_ready_condition(CmdWaitFor::exit_code(0)))
                .expect("initialize S3 fixture bucket");
        }
        let endpoint = format!(
            "http://{}:{}",
            container.get_host().expect("MinIO host"),
            container
                .get_host_port_ipv4(9000)
                .expect("MinIO mapped port"),
        );
        Self {
            _container: container,
            endpoint,
        }
    }

    pub fn config(&self, prefix: &str) -> compio_s3::S3Config {
        compio_s3::S3Config::parse_url(&format!(
            "s3://{BUCKET}/{prefix}?provider=minio&endpoint={}&region=us-east-1&style=path&dev_http=true&checksum=none",
            self.endpoint,
        )).expect("MinIO fixture configuration")
    }

    pub fn credentials(&self) -> compio_s3::S3Credentials {
        compio_s3::S3Credentials::new(ACCESS, SECRET, None)
    }
}
