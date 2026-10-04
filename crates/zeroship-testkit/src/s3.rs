//! An owned S3-compatible server with Docker-assigned ports.
//!
//! The server is the Versity S3 Gateway over its POSIX backend: a real S3
//! implementation that verifies `SigV4` (an unsigned or wrongly-signed request is
//! refused), assembles genuine multipart uploads and answers ranged GETs, so a
//! signing or multipart regression fails here instead of passing against a mock
//! that accepts anything.
//!
//! The container is a testcontainers `Container` handle: dropping it removes the
//! gateway, and the crate's process watchdog removes it if the owning process is
//! signalled before the drop runs.
//!
//! [`StalledObject`] is the one fault the gateway cannot produce: a response
//! body that stops arriving. It is a loopback endpoint, not a container.

use std::time::Duration;

use testcontainers::{
    core::{IntoContainerPort, Mount, WaitFor},
    runners::SyncRunner,
    Container, GenericImage, ImageExt,
};

const IMAGE: &str = "ghcr.io/versity/versitygw";
/// Pinned: a floating tag is how a fixture silently changes servers under a
/// green suite, or stops resolving when a tag is withdrawn.
const TAG: &str = "v1.3.0";
const ACCESS: &str = "zeroship-fixture";
const SECRET: &str = "zeroship-fixture-secret";
const BUCKET: &str = "storage-fixture";
const PORT: u16 = 7070;
const DATA_CAPACITY: i64 = 1024 * 1024 * 1024;

#[derive(Debug)]
pub struct S3Server {
    _container: Container<GenericImage>,
    endpoint: String,
}

impl S3Server {
    pub fn start() -> Self {
        // The POSIX backend maps each bucket to a directory under the gateway
        // root, so the bucket exists as soon as the directory does: no client
        // and no vendor CLI take part in fixture setup.
        let boot = format!(
            "mkdir -p /data/{BUCKET} && exec /usr/local/bin/versitygw --port :{PORT} posix /data"
        );
        let container = GenericImage::new(IMAGE, TAG)
            .with_exposed_port(PORT.tcp())
            // The gateway prints its banner once the listener is bound.
            .with_wait_for(WaitFor::message_on_stdout("VersityGW"))
            .with_entrypoint("/bin/sh")
            .with_env_var("ROOT_ACCESS_KEY_ID", ACCESS)
            .with_env_var("ROOT_SECRET_ACCESS_KEY", SECRET)
            // Disposable fixture data must not consume the host's build-cache
            // space.
            .with_mount(Mount::tmpfs_mount("/data").with_size_bytes(DATA_CAPACITY))
            .with_cmd(["-c".to_string(), boot])
            .with_startup_timeout(Duration::from_secs(90))
            .start()
            .expect("S3 tests require Docker");
        let endpoint = format!(
            "http://{}:{}",
            container.get_host().expect("S3 fixture host"),
            container
                .get_host_port_ipv4(PORT.tcp())
                .expect("S3 fixture mapped port"),
        );
        Self {
            _container: container,
            endpoint,
        }
    }

    /// The `s3://` URL of `prefix` in the fixture bucket, in the form
    /// `compio_s3::S3Config::parse_url` reads.
    ///
    /// The fixture hands out plain data rather than `compio-s3` types:
    /// `compio-s3` dev-depends on this crate, so naming it here would put the
    /// crate under test on this crate's normal edge.
    #[must_use]
    pub fn url(&self, prefix: &str) -> String {
        format!(
            "s3://{BUCKET}/{prefix}?provider=generic&endpoint={}&region=us-east-1&style=path&dev_http=true&checksum=none",
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
