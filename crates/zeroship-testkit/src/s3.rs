//! The S3 gateway every test process of a worktree shares.
//!
//! [`S3Server::start`] joins the one gateway a worktree boots through
//! [`zeroship_shared_server`]: the first process elects itself, the image is
//! built, the container starts, and every other process joins the ready gateway.
//! A process holds its lease for as long as it runs; the container's watchdog
//! removes the gateway once no process has held it for the idle grace.
//!
//! One gateway per run is what keeps the daemon's ephemeral host-port allocator
//! to a single draw per worktree, and the published port is bound on loopback so
//! the fixture does not depend on a wildcard host port being free. Tests stay
//! isolated from each other by the prefix they pass to [`S3Server::url`], never
//! by a server of their own.
//!
//! The server is the Versity S3 Gateway over its POSIX backend: a real S3
//! implementation that verifies `SigV4` (an unsigned or wrongly-signed request is
//! refused), assembles genuine multipart uploads and answers ranged GETs, so a
//! signing or multipart regression fails here instead of passing against a mock
//! that accepts anything.
//!
//! [`StalledObject`] is the one fault the gateway cannot produce: a response
//! body that stops arriving. It is a loopback endpoint, not a container.

use std::time::Duration;

use zeroship_shared_server::{self as shared, Scope};

mod image;

pub(crate) use image::RECIPE;

/// The gateway's own listener port inside the container.
const CONTAINER_PORT: u16 = 7070;
const ACCESS: &str = "zeroship-fixture";
const SECRET: &str = "zeroship-fixture-secret";
const BUCKET: &str = "storage-fixture";

/// How long the gateway may go unleased before its watchdog removes it.
///
/// A run leases the gateway from the test processes that use it, and under a
/// process-per-test runner those processes are interleaved with long S3-free
/// stretches. A grace shorter than those gaps removes the gateway mid-run and a
/// later test boots a second one, so the grace spans a run rather than one test.
const S3_IDLE_GRACE: Duration = Duration::from_mins(15);

/// The shared S3 gateway, leased by the process that holds it.
#[derive(Debug)]
pub struct S3Server {
    lease: shared::Lease,
    endpoint: String,
}

impl S3Server {
    /// Join the gateway `scope` names, booting it if this process is elected.
    ///
    /// # Errors
    /// When the gateway image cannot be built, or the gateway cannot be booted
    /// or joined.
    pub fn join(scope: &Scope) -> Result<Self, String> {
        let lease = shared::join(scope, &spec()?, |_| Ok(()))?;
        let endpoint = format!("http://127.0.0.1:{}", lease.port);
        Ok(Self { lease, endpoint })
    }

    /// Join the worktree's shared gateway, booting it if this process is elected.
    ///
    /// # Panics
    /// When the gateway image cannot be built, or the gateway cannot be booted
    /// or joined.
    #[must_use]
    pub fn start() -> Self {
        Self::join(&Scope::private("s3", S3_IDLE_GRACE)).unwrap_or_else(|error| {
            panic!("the shared S3 gateway could not be started: {error}")
        })
    }

    /// The Docker id of the shared gateway, so a contract can show two processes
    /// joined the same container.
    #[must_use]
    pub fn container_id(&self) -> &str {
        &self.lease.container_id
    }

    /// The `s3://` URL of `prefix` in the fixture bucket, in the form
    /// `compio_s3::S3Config::parse_url` reads.
    ///
    /// The fixture hands out plain data rather than `compio-s3` types:
    /// `compio-s3` dev-depends on this crate, so naming it here would put the
    /// crate under test on this crate's normal edge.
    #[must_use]
    pub fn url(&self, prefix: &str) -> String {
        self.url_with_checksum(prefix, "none")
    }

    /// The same URL with the `checksum` mode stated at the call site, so a suite
    /// that pins a digest mode gets it without changing the shared default.
    #[must_use]
    pub fn url_with_checksum(&self, prefix: &str, checksum: &str) -> String {
        format!(
            "s3://{BUCKET}/{prefix}?provider=generic&endpoint={}&region=us-east-1&style=path&dev_http=true&checksum={checksum}",
            self.endpoint,
        )
    }

    /// The access key id the gateway accepts.
    #[must_use]
    pub fn access_key(&self) -> &'static str {
        ACCESS
    }

    /// The secret key the gateway accepts.
    #[must_use]
    pub fn secret_key(&self) -> &'static str {
        SECRET
    }
}

/// The recipe the shared gateway runs under, its identity keyed to every input
/// that changes what a ready gateway holds.
///
/// # Errors
/// When the image cannot be built.
pub fn spec() -> Result<shared::Spec, String> {
    let image =
        image::reference().map_err(|error| format!("could not build the shared S3 image: {error}"))?;
    // The POSIX backend maps each bucket to a directory under the gateway root,
    // so the bucket exists as soon as the directory does: no client and no
    // vendor CLI take part in fixture setup.
    let boot = format!(
        "mkdir -p /data/{BUCKET} && exec /usr/local/bin/versitygw --port :{CONTAINER_PORT} posix /data"
    );
    let args = vec!["/bin/sh".to_owned(), "-c".to_owned(), boot];
    let environment = vec![
        ("ROOT_ACCESS_KEY_ID".to_owned(), ACCESS.to_owned()),
        ("ROOT_SECRET_ACCESS_KEY".to_owned(), SECRET.to_owned()),
    ];
    let ready = readiness();
    let inputs = inputs(&image, &environment, &args, &ready);
    Ok(shared::Spec {
        inputs,
        image,
        environment,
        ports: vec![shared::Port {
            container: CONTAINER_PORT,
            host: shared::HostPort::Assigned,
        }],
        watchdog: shared::image::WATCHDOG.to_owned(),
        entrypoint: shared::Entrypoint::Watchdog,
        args,
        ready,
    })
}

/// How to tell the gateway is ready and still answers.
///
/// The banner names the bound listener; the probe is a TCP connect, which is the
/// only request the gateway answers without a signature.
fn readiness() -> shared::Readiness {
    let probe = vec![
        "nc".to_owned(),
        "-z".to_owned(),
        "127.0.0.1".to_owned(),
        CONTAINER_PORT.to_string(),
    ];
    shared::Readiness {
        log_marker: "VersityGW".to_owned(),
        probe: probe.clone(),
        answer: probe,
    }
}

/// The 12-hex identity of a gateway built from the image, environment, server
/// command and readiness probe.
fn inputs(
    image: &str,
    environment: &[(String, String)],
    args: &[String],
    ready: &shared::Readiness,
) -> String {
    let mut parts: Vec<Vec<u8>> = vec![
        image.as_bytes().to_vec(),
        CONTAINER_PORT.to_string().into_bytes(),
        BUCKET.as_bytes().to_vec(),
    ];
    for argument in args {
        parts.push(argument.as_bytes().to_vec());
    }
    for (key, value) in environment {
        parts.push(format!("{key}={value}").into_bytes());
    }
    parts.push(ready.log_marker.as_bytes().to_vec());
    for argument in ready.probe.iter().chain(&ready.answer) {
        parts.push(argument.as_bytes().to_vec());
    }
    let refs: Vec<&[u8]> = parts.iter().map(Vec::as_slice).collect();
    shared::digest(&refs)
}

/// An S3-shaped endpoint whose one object stalls partway through its body.
///
/// It accepts a single connection, answers its request with a successful
/// object response that declares `declared` bytes, sends the first `sent` of
/// them and then sends nothing more, holding the connection open until the
/// client hangs up. A real S3 server cannot be made to stall, and a stall is
/// the only input that shows a client's body-read bound is applied rather
/// than merely configured. The server does not check signatures.
#[derive(Debug)]
pub struct StalledObject {
    endpoint: String,
    stalled: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

/// How long the endpoint holds a stalled connection before it gives up on
/// the client. It bounds the server thread, not the client under test.
const STALLED_HOLD: Duration = Duration::from_mins(1);

impl StalledObject {
    /// Serve one stalled object on a loopback port.
    ///
    /// # Panics
    /// Panics if `sent` exceeds `declared` or no loopback port can be bound.
    #[must_use]
    pub fn start(sent: usize, declared: usize) -> Self {
        use std::io::{Read, Write};

        assert!(sent <= declared, "a stalled object cannot send more than it declares");
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind a loopback port");
        let endpoint = format!("http://{}", listener.local_addr().expect("listener address"));
        let stalled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let witness = std::sync::Arc::clone(&stalled);
        std::thread::spawn(move || {
            let Ok((mut socket, _)) = listener.accept() else { return };
            socket.set_read_timeout(Some(STALLED_HOLD)).expect("set the hold bound");
            let mut request = Vec::new();
            let mut byte = [0u8; 1];
            while !request.ends_with(b"\r\n\r\n") {
                match socket.read(&mut byte) {
                    Ok(1) => request.push(byte[0]),
                    _ => return,
                }
            }
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {declared}\r\n\
                 Content-Type: application/octet-stream\r\n\
                 Last-Modified: Wed, 21 Oct 2015 07:28:00 GMT\r\n\r\n"
            );
            if socket.write_all(head.as_bytes()).is_err()
                || socket.write_all(&vec![0x5a; sent]).is_err()
                || socket.flush().is_err()
            {
                return;
            }
            witness.store(true, std::sync::atomic::Ordering::Release);
            // Hold the connection without sending more until the client hangs
            // up (a read of zero bytes) or the hold bound passes.
            let mut drain = [0u8; 1024];
            while matches!(socket.read(&mut drain), Ok(n) if n > 0) {}
        });
        Self { endpoint, stalled }
    }

    /// The `s3://` URL of `prefix` on this endpoint, in the form
    /// `compio_s3::S3Config::parse_url` reads.
    #[must_use]
    pub fn url(&self, prefix: &str) -> String {
        format!(
            "s3://{BUCKET}/{prefix}?provider=generic&endpoint={}&region=us-east-1&style=path&dev_http=true&checksum=none",
            self.endpoint,
        )
    }

    /// Whether the endpoint has written the head of the body and begun to
    /// stall.
    #[must_use]
    pub fn has_stalled(&self) -> bool {
        self.stalled.load(std::sync::atomic::Ordering::Acquire)
    }
}
