use compio::buf::{BufResult, IoBuf, IoBufMut};
use compio::io::{AsyncRead, AsyncWrite};
use compio_postgres::config::{Host, SslMode};
use compio_postgres::{Config, NoTls, SplitStream};
use log::{Level, LevelFilter, Log, Metadata, Record};
use std::io::ErrorKind;
use std::sync::{Mutex, Once};
use std::time::Duration;

#[expect(
    dead_code,
    reason = "the shared support module carries helpers this process-isolated target does not use"
)]
mod support;

static LOGGER: ShutdownLogger = ShutdownLogger;
static LOGGER_INIT: Once = Once::new();
static LOGS: Mutex<Vec<String>> = Mutex::new(Vec::new());

struct ShutdownLogger;

impl Log for ShutdownLogger {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        metadata.level() == Level::Trace && metadata.target() == "compio_postgres::connection"
    }

    fn log(&self, record: &Record<'_>) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let message = record.args().to_string();
        if message.starts_with("stream shutdown non-fatal error:") {
            LOGS.lock()
                .expect("shutdown log recorder was poisoned")
                .push(message);
        }
    }

    fn flush(&self) {}
}

struct ShutdownErrorStream {
    inner: compio::net::TcpStream,
    kind: ErrorKind,
}

#[allow(clippy::future_not_send)]
impl AsyncRead for ShutdownErrorStream {
    async fn read<B: IoBufMut>(&mut self, buf: B) -> BufResult<usize, B> {
        self.inner.read(buf).await
    }
}

#[allow(clippy::future_not_send)]
impl AsyncWrite for ShutdownErrorStream {
    async fn write<B: IoBuf>(&mut self, buf: B) -> BufResult<usize, B> {
        self.inner.write(buf).await
    }

    async fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush().await
    }

    async fn shutdown(&mut self) -> std::io::Result<()> {
        Err(std::io::Error::new(self.kind, "scripted shutdown failure"))
    }
}

impl SplitStream for ShutdownErrorStream {
    type ReadHalf = Self;
    type WriteHalf = Self;

    fn try_into_split(self) -> Result<(Self::ReadHalf, Self::WriteHalf), Self> {
        Err(self)
    }
}

fn install_logger() {
    LOGGER_INIT.call_once(|| {
        log::set_logger(&LOGGER).expect("install serialized teardown logger");
        log::set_max_level(LevelFilter::Trace);
    });
}

/// The TCP endpoint the test DSN names. It is dialled as given, so a name and
/// a numeric address both work, as they do for `connect`.
fn configured_endpoint(config: &Config) -> (String, u16) {
    let host = match config.get_hosts().first() {
        Some(Host::Tcp(host)) => host.clone(),
        #[cfg(unix)]
        Some(Host::Unix(path)) => panic!("test URL selected a Unix socket: {}", path.display()),
        None => panic!("test URL omitted its TCP host"),
    };
    let port = config.get_ports().first().copied().unwrap_or(5432);
    (host, port)
}

#[allow(clippy::future_not_send)]
async fn run_serialized_teardown(kind: ErrorKind) {
    // The plaintext DSN: this test hands the driver a raw TCP stream of its
    // own and speaks no TLS on it.
    let url = support::plaintext_url();
    let mut config = url
        .parse::<Config>()
        .unwrap_or_else(|error| panic!("parse the test DSN: {error}"));
    config.ssl_mode(SslMode::Disable);
    let (host, port) = configured_endpoint(&config);
    let stream = compio::net::TcpStream::connect((host.as_str(), port))
        .await
        .unwrap_or_else(|error| support::postgres_unreachable(&url, &error));
    let (client, connection) = config
        .connect_raw(
            ShutdownErrorStream {
                inner: stream,
                kind,
            },
            NoTls,
        )
        .await
        .unwrap_or_else(|error| support::postgres_unreachable(&url, &error));

    drop(client);
    connection
        .run()
        .await
        .expect("scripted shutdown error escaped serialized teardown");
}

#[compio::test]
async fn serialized_teardown_logs_only_unexpected_shutdown_errors() {
    install_logger();

    for (kind, expected_logs) in [
        (ErrorKind::Other, 1),
        (ErrorKind::BrokenPipe, 0),
        (ErrorKind::NotConnected, 0),
    ] {
        LOGS.lock()
            .expect("shutdown log recorder was poisoned")
            .clear();
        compio::time::timeout(Duration::from_secs(10), run_serialized_teardown(kind))
            .await
            .unwrap_or_else(|_| panic!("serialized teardown timed out for {kind:?}"));
        let logs = std::mem::take(&mut *LOGS.lock().expect("shutdown log recorder was poisoned"));
        assert_eq!(
            logs.len(),
            expected_logs,
            "shutdown kind {kind:?} produced the wrong trace set: {logs:?}"
        );
    }
}
