//! Owned PostgreSQL and generated configuration for the orchestration tests.
#[path = "../../../tests/fixtures/postgres/mod.rs"]
mod postgres;

use xtask::platform_db::{admin, overlay};

pub struct Database {
    _postgres: postgres::Postgres,
    directory: tempfile::TempDir,
}
impl Database {
    pub fn start() -> Self {
        let postgres = postgres::Postgres::start();
        let directory = tempfile::tempdir().expect("fixture configuration directory");
        std::fs::create_dir_all(directory.path().join("deploy/ops")).unwrap();
        std::fs::write(
            directory.path().join("deploy/ops/zeroship.test.toml"),
            format!("[control]\ndatabase_url = {:?}\n", postgres.url()),
        )
        .unwrap();
        Self {
            _postgres: postgres,
            directory,
        }
    }
    pub fn configuration(&self) -> (overlay::Loaded, admin::Server) {
        let loaded = overlay::load(self.directory.path()).expect("load fixture overlay");
        let server = admin::Server::from_overlay(&loaded).expect("fixture server coordinates");
        (loaded, server)
    }
}
