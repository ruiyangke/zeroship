use compio::buf::{BufResult, IoBuf, IoBufMut};
use compio::io::{AsyncRead, AsyncWrite};
use compio_postgres::config::{Host, SslMode};
use compio_postgres::{Config, NoTls, SplitStream};
use log::{Level, LevelFilter, Log, Metadata, Record};
use std::io::ErrorKind;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Mutex, Once};
use std::time::Duration;

#[allow(dead_code, clippy::doc_markdown)]
#[path = "common/env.rs"]
mod test_env;

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

fn configured_address(config: &Config) -> SocketAddr {
    let host = match config.get_hosts().first() {
        Some(Host::Tcp(host)) => host,
        #[cfg(unix)]
        Some(Host::Unix(path)) => panic!("test URL selected a Unix socket: {}", path.display()),
        None => panic!("test URL omitted its TCP host"),
    };
    let ip = host
        .parse::<IpAddr>()
        .unwrap_or_else(|error| panic!("test URL host was not a numeric address: {error}"));
    let port = config.get_ports().first().copied().unwrap_or(5432);
    SocketAddr::new(ip, port)
}

#[allow(clippy::future_not_send)]
async fn run_serialized_teardown(kind: ErrorKind) {
    let url = test_env::get(test_env::TestEnvKey::PgTestUrl)
        .expect("PG_TEST_URL must name the live PostgreSQL 16");
    let mut config = url
        .parse::<Config>()
        .unwrap_or_else(|error| panic!("parse PG_TEST_URL: {error}"));
    config.ssl_mode(SslMode::Disable);
    let stream = compio::net::TcpStream::connect(configured_address(&config))
        .await
        .unwrap_or_else(|error| panic!("connect to PostgreSQL: {error}"));
    let (client, connection) = config
        .connect_raw(
            ShutdownErrorStream {
                inner: stream,
                kind,
            },
            NoTls,
        )
        .await
        .unwrap_or_else(|error| panic!("complete PostgreSQL startup: {error}"));

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
