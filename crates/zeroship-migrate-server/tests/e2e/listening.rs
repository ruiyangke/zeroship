//! The migration server announces the address it bound, and only after the bind
//! succeeded.
//!
//! Both cases launch the shipped `zeroship-migrate-server` against a
//! `PostgreSQL` server of this suite's own, because the boot dials its control
//! database before it reaches the listener.

use std::io::{BufRead as _, BufReader};
use std::path::PathBuf;
use std::process::{Child, ChildStdout, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use crate::support::fixture::Postgres;
use crate::support::service::service_command;

/// A serving migration-service child on a chosen port. Its tracing stream is
/// piped for the caller to read and its `stderr` is captured; killed when
/// dropped.
struct Serving {
    child: Child,
    stderr: PathBuf,
    /// The scratch directory the child's log and tmp dir live under, removed
    /// when this guard drops.
    _scratch: tempfile::TempDir,
}

impl Serving {
    /// Spawn the service on `port` against `database_url`.
    fn start(database_url: &str, port: u16) -> Self {
        let scratch = tempfile::tempdir().expect("scratch directory");
        let stderr = scratch.path().join("migrate-server.err");
        let sink = std::fs::File::create(&stderr).expect("migrate-server stderr log");
        let child = service_command()
            // The shared builder names a deliberately unreachable control
            // database; this case supplies the served one.
            .env("ZEROSHIP_MIGRATE_SERVER_DATABASE_URL", database_url)
            .env(
                "ZEROSHIP_MIGRATE_SERVER_PROVISION_DATABASE_URL",
                database_url,
            )
            .args([
                "--no-config",
                "--auth-platform-issuer",
                "https://auth.zeroship.test/oauth2",
            ])
            // Pin the announcement's format so this reader does not depend on
            // the child's stderr being a non-terminal.
            .args(["--observability-log-format", "json"])
            .args(["--port", &port.to_string()])
            // ONE serving thread: the fixture must not claim one io_uring ring
            // per core from the host's shared `ulimit -l` budget.
            .args(["--threads", "1"])
            // No reconciler: it would run against the empty control database
            // this fixture provides and find the schema it expects absent.
            .args(["--reconcile-interval-seconds", "0"])
            .arg("--tmp-dir")
            .arg(scratch.path().join("tmp"))
            .stdout(Stdio::piped())
            .stderr(Stdio::from(sink))
            .spawn()
            .expect("spawn zeroship-migrate-server");
        Self {
            child,
            stderr,
            _scratch: scratch,
        }
    }

    fn stderr(&self) -> String {
        std::fs::read_to_string(&self.stderr).unwrap_or_default()
    }
}

impl Drop for Serving {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// How a watch of a child's tracing stream ended.
enum Announce {
    /// The listening line was read, naming its advertised port.
    Port(u16),
    /// The stream ended first, carrying everything read.
    Ended(String),
}

/// Read `stdout` on a background thread until it announces `marker`, then keep
/// draining so the child never blocks on a full pipe. The test thread waits on
/// the returned channel with `recv_timeout`, so a child that stalls fails by
/// timeout rather than hanging the run, and nothing polls on a sleep.
fn await_announcement(stdout: ChildStdout, marker: &'static str) -> mpsc::Receiver<Announce> {
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut log = String::new();
        let mut announced = false;
        loop {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) => {
                    if !announced {
                        let _ = sender.send(Announce::Ended(log));
                    }
                    return;
                }
                Ok(_) => {
                    if !announced {
                        if let Some(port) = announced_port(&line, marker) {
                            let _ = sender.send(Announce::Port(port));
                            announced = true;
                        }
                    }
                    log.push_str(&line);
                }
                Err(error) => {
                    let _ = sender.send(Announce::Ended(format!("{log}\nread error: {error}")));
                    return;
                }
            }
        }
    });
    receiver
}

/// The port a single log line announced, if it carried `marker`.
///
/// The tracing line is JSON on a non-TTY stream; a plain `bind=host:port`
/// spelling is accepted too, so this reader is not pinned to one format.
fn announced_port(line: &str, marker: &str) -> Option<u16> {
    if !line.contains(marker) {
        return None;
    }
    let field = serde_json::from_str::<serde_json::Value>(line)
        .ok()
        .and_then(|value| {
            value
                .pointer("/fields/bind")
                .or_else(|| value.get("bind"))
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        })
        .or_else(|| {
            line.split("bind=")
                .nth(1)
                .map(|rest| rest.split_whitespace().next().unwrap_or("").to_owned())
        })?;
    field.parse::<std::net::SocketAddr>().ok().map(|a| a.port())
}

/// A port another process holds is not this service listening: it must stop on
/// the collision without ever claiming it is listening.
#[test]
fn a_held_port_is_never_announced_as_listening() {
    let database = Postgres::start();
    let foreign = std::net::TcpListener::bind("127.0.0.1:0").expect("hold a port");
    let port = foreign.local_addr().expect("the held port").port();
    let mut serving = Serving::start(database.url(), port);
    let announcement = await_announcement(
        serving.child.stdout.take().expect("piped stdout"),
        "zeroship-migrate-server listening",
    );
    match announcement.recv_timeout(Duration::from_mins(1)) {
        Ok(Announce::Port(port)) => panic!(
            "the migration server claimed to be listening on port {port} it never bound; \
             stderr:\n{}",
            serving.stderr()
        ),
        Ok(Announce::Ended(log)) => assert!(
            !log.contains("zeroship-migrate-server listening"),
            "the migration server claimed to be listening on a port it never bound:\n{log}"
        ),
        Err(error) => panic!(
            "the migration server never stopped on the port another process holds ({error}); \
             stderr:\n{}",
            serving.stderr()
        ),
    }
    assert!(
        serving.stderr().contains("Address already in use"),
        "the migration server did not name the address collision:\n{}",
        serving.stderr()
    );
    drop(foreign);
}

/// A `--port 0` boot announces the port it actually bound, and the process
/// really listens there. The socket is in the LISTEN state before the line is
/// logged, so the ownership check after the announcement races nothing.
#[test]
fn a_zero_port_boot_announces_the_port_it_bound() {
    let database = Postgres::start();
    let mut serving = Serving::start(database.url(), 0);
    let announcement = await_announcement(
        serving.child.stdout.take().expect("piped stdout"),
        "zeroship-migrate-server listening",
    );
    let port = match announcement.recv_timeout(Duration::from_mins(1)) {
        Ok(Announce::Port(port)) => port,
        Ok(Announce::Ended(log)) => panic!(
            "the migration server exited before announcing a bound port:\n{log}\nstderr:\n{}",
            serving.stderr()
        ),
        Err(error) => panic!(
            "the migration server never announced a bound port ({error}); stderr:\n{}",
            serving.stderr()
        ),
    };
    assert_ne!(
        port, 0,
        "a --port 0 boot announced the requested port, not the bound one"
    );
    assert!(
        zeroship_testkit::listen::child_listens(serving.child.id(), port),
        "the migration server announced port {port} but does not own a listening socket on it; \
         stderr:\n{}",
        serving.stderr()
    );
}
