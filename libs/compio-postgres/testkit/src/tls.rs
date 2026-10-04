//! The PostgreSQL servers the TLS suites dial.
//!
//! SIX servers, because each one is the control for a claim that would
//! otherwise be unfalsifiable:
//!
//! - `tls`: `ssl=on`, TLS 1.2 only, a certificate for `localhost` signed by a
//!   private CA. The positive case for every mode that encrypts and the
//!   discriminator for the client's minimum-version bound. That exact
//!   certificate is also listed in the fixture's CRL, so the client can prove
//!   the same server is accepted without the CRL and refused with it. It
//!   carries the suite's logical-decoding and prepared-transaction settings
//!   too, because `--features suite-over-tls` runs the whole suite against it.
//! - `plain`: TLS off entirely. Without it, "`sslmode=require` connected" is
//!   consistent with a driver that ignores `sslmode`.
//! - `mismatch`: `ssl=on`, TLS 1.3 only, a certificate for a name that is NOT
//!   the one the tests dial, signed by the SAME CA. It separates `verify-ca`
//!   from `verify-full` and discriminates the maximum-version bound.
//! - `sslonly`: `ssl=on`, `pg_hba` accepting `hostssl` only. The only way to see
//!   `allow` do anything: it tries plaintext first and reaches TLS only when
//!   the plaintext attempt is refused.
//! - `clientcert`: `ssl=on` with `ssl_ca_file` and `cert` authentication, so the
//!   client MUST present a certificate. The only way to tell "`sslcert` and
//!   `sslkey` are parsed" from "they are sent and used".
//! - `directtls`: PostgreSQL 18 with the same certificate as `tls`, the positive
//!   server for direct SSL negotiation, with the suite's settings as well;
//!   `tls` stays the PostgreSQL 16 discriminator.
//!
//! Every server is a shared server of the worktree through
//! `zeroship_shared_server`, like the plaintext one in [`crate::server`]: the
//! first test process boots it, the rest join it, and its watchdog removes it
//! once no process holds its lease.
//!
//! THE MATERIAL IS MADE WHEN THE IMAGE IS BUILT. `tls/certs.sh` runs in the
//! PostgreSQL 16 image build and generates the CA, the certificates, the CRL and
//! the client identity into `/certs`; the PostgreSQL 18 image copies that
//! directory from the build it was made from. No private key is committed, and
//! a build is one CA: every server started from it presents certificates the
//! same CA signed. The client-side files a test passes to the driver are copied
//! out of a running server into this worktree's `target`, under a directory
//! named for the CA, so a rebuilt image is a new directory rather than an
//! overwrite of the one another process is reading.
//!
//! EACH SERVER IS CHECKED WITH LIBPQ WHEN IT BOOTS. Every claim the suite
//! turns on - this server speaks TLS 1.2, libpq honours the hashed CRL, the
//! mismatch server passes `verify-ca` and fails `verify-full`, the ssl-only
//! server refuses plaintext, the client-certificate server demands one - is
//! asked of the server through `psql` in its own container, so a fixture that
//! does not discriminate fails its boot instead of letting a driver bug look
//! satisfied.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use zeroship_shared_server::{self as shared, Scope};

/// The password of `postgres` on every server that takes one.
const PASSWORD: &str = "compio-postgres-tls-test";

/// The passphrase `client-encrypted.key` is encrypted under.
pub const CLIENT_KEY_PASSWORD: &str = "compio-postgres-encrypted-key-test";

/// The port PostgreSQL listens on inside every container.
const POSTGRES_PORT: u16 = 5432;

/// The material generator the PostgreSQL 16 image build runs.
const CERTS_SCRIPT: &[u8] = include_bytes!("tls/certs.sh");

/// The PostgreSQL 16 image: the stock server, the watchdog, and the material.
const DOCKERFILE_16: &str = "FROM postgres:16\n\
    COPY watchdog.sh /usr/local/bin/zeroship-watchdog\n\
    COPY certs.sh /usr/local/bin/compio-postgres-certs\n\
    RUN chmod 0755 /usr/local/bin/zeroship-watchdog \\\n\
    \x20   && sh /usr/local/bin/compio-postgres-certs /certs compio-postgres-encrypted-key-test\n";

/// The settings `suite-over-tls` needs on the server it runs the suite on;
/// [`crate::server`] says why each is there.
const SUITE_SETTINGS: &[&str] = &[
    "-c",
    "wal_level=logical",
    "-c",
    "max_prepared_transactions=10",
    "-c",
    "max_replication_slots=128",
    "-c",
    "max_wal_senders=128",
    "-c",
    "max_slot_wal_keep_size=8GB",
];

/// The six servers, the client-side material, and the leases that keep them.
#[derive(Debug)]
pub struct TlsServers {
    /// `ssl=on`, TLS 1.2 only, certificate for `localhost` signed by [`Self::ca`].
    pub tls_url: String,
    /// TLS off entirely.
    pub plain_url: String,
    /// `ssl=on`, TLS 1.3 only, certificate signed by [`Self::ca`] for a name
    /// that is NOT `localhost`.
    pub mismatch_url: String,
    /// `ssl=on`, `pg_hba` accepting `hostssl` only.
    pub sslonly_url: String,
    /// `ssl=on` with `cert` authentication; carries no password.
    pub clientcert_url: String,
    /// PostgreSQL 18 with `ssl=on` and the same certificate as `tls`.
    pub directtls_url: String,
    /// The private CA that signed every certificate above. It is in no system
    /// trust store.
    pub ca: String,
    /// A client certificate signed by [`Self::ca`], with `CN=postgres`.
    pub client_cert: String,
    /// The private key for [`Self::client_cert`], mode 0600.
    pub client_key: String,
    /// The same key as passphrase-encrypted PKCS#8, mode 0600.
    pub client_encrypted_key: String,
    /// The passphrase of [`Self::client_encrypted_key`].
    pub client_key_password: String,
    /// A CRL issued by [`Self::ca`] that revokes the certificate on `tls_url`.
    pub server_crl: String,
    /// The same CRL under its OpenSSL issuer-hash lookup name.
    pub server_crl_dir: String,
    /// The same CRL under a syntactically valid but incorrect issuer hash.
    pub server_crl_wrong_hash_dir: String,
    _leases: Vec<shared::Lease>,
}

/// The worktree's TLS servers, joined on first use.
///
/// # Panics
/// When a server could not be booted or joined - most often because Docker is
/// not available - or a server failed its libpq checks. The first failure is
/// kept, so every later caller in the process fails with the same reason.
pub fn servers() -> &'static TlsServers {
    static SERVERS: OnceLock<Result<TlsServers, String>> = OnceLock::new();
    match SERVERS.get_or_init(join_all) {
        Ok(servers) => servers,
        Err(reason) => panic!(
            "compio-postgres's TLS suites run against six servers every test process of the \
             worktree shares, started in Docker by compio_postgres_testkit::tls, and they could \
             not be joined: {reason}"
        ),
    }
}

/// Which image a server runs.
#[derive(Clone, Copy)]
enum Major {
    Sixteen,
    Eighteen,
}

/// One server: its scope kind, image, server arguments and boot check.
struct Recipe {
    kind: &'static str,
    major: Major,
    args: Vec<&'static str>,
    check: fn(&shared::Boot) -> Result<(), String>,
}

fn recipes() -> [Recipe; 6] {
    let tls = [
        "-c",
        "ssl=on",
        "-c",
        "ssl_cert_file=/certs/server.crt",
        "-c",
        "ssl_key_file=/certs/server.key",
    ];
    [
        Recipe {
            kind: "compio-postgres-tls",
            major: Major::Sixteen,
            args: [
                &tls[..],
                &[
                    "-c",
                    "ssl_min_protocol_version=TLSv1.2",
                    "-c",
                    "ssl_max_protocol_version=TLSv1.2",
                ],
                SUITE_SETTINGS,
            ]
            .concat(),
            check: check_tls,
        },
        Recipe {
            kind: "compio-postgres-tls-plain",
            major: Major::Sixteen,
            args: Vec::new(),
            check: check_plain,
        },
        Recipe {
            kind: "compio-postgres-tls-mismatch",
            major: Major::Sixteen,
            args: vec![
                "-c",
                "ssl=on",
                "-c",
                "ssl_cert_file=/certs/mismatch.crt",
                "-c",
                "ssl_key_file=/certs/mismatch.key",
                "-c",
                "ssl_min_protocol_version=TLSv1.3",
                "-c",
                "ssl_max_protocol_version=TLSv1.3",
            ],
            check: check_mismatch,
        },
        Recipe {
            kind: "compio-postgres-tls-sslonly",
            major: Major::Sixteen,
            args: [&tls[..], &["-c", "hba_file=/certs/pg_hba_sslonly.conf"]].concat(),
            check: check_sslonly,
        },
        Recipe {
            kind: "compio-postgres-tls-clientcert",
            major: Major::Sixteen,
            args: [
                &tls[..],
                &[
                    "-c",
                    "ssl_ca_file=/certs/ca.crt",
                    "-c",
                    "hba_file=/certs/pg_hba_clientcert.conf",
                ],
            ]
            .concat(),
            check: check_clientcert,
        },
        Recipe {
            kind: "compio-postgres-tls-directtls",
            major: Major::Eighteen,
            args: [&tls[..], SUITE_SETTINGS].concat(),
            check: check_directtls,
        },
    ]
}

/// Build both images, join all six servers at once, and copy the client-side
/// material out of the `tls` server.
fn join_all() -> Result<TlsServers, String> {
    let watchdog = shared::image::WATCHDOG_SCRIPT;
    let sixteen = shared::image::build(
        "compio-postgres-tls-16",
        DOCKERFILE_16,
        &[("watchdog.sh", watchdog), ("certs.sh", CERTS_SCRIPT)],
    )?;
    // Keyed to the BUILD of the 16 image, not its tag: the material is made at
    // build time, so two builds of one tag are two CAs, and the 18 image must
    // carry the CA of the build the 16 servers run.
    let sixteen_id = shared::image_id(&sixteen)?;
    let eighteen = shared::image::build(
        "compio-postgres-tls-18",
        &format!(
            "FROM {sixteen} AS certs\n\
             FROM postgres:18\n\
             LABEL compio-postgres.certs-from=\"{sixteen_id}\"\n\
             COPY watchdog.sh /usr/local/bin/zeroship-watchdog\n\
             COPY --from=certs /certs /certs\n\
             RUN chmod 0755 /usr/local/bin/zeroship-watchdog \\\n\
             \x20   && chown root:postgres /certs/server.key /certs/mismatch.key \\\n\
             \x20   && chmod 0640 /certs/server.key /certs/mismatch.key\n"
        ),
        &[("watchdog.sh", watchdog)],
    )?;

    let recipes = recipes();
    let joined: Vec<Result<shared::Lease, String>> = std::thread::scope(|scope| {
        let handles: Vec<_> = recipes
            .iter()
            .map(|recipe| {
                let image = match recipe.major {
                    Major::Sixteen => sixteen.clone(),
                    Major::Eighteen => eighteen.clone(),
                };
                scope.spawn(move || {
                    shared::join(
                        &Scope::worktree(recipe.kind),
                        &spec(&image, &recipe.args),
                        recipe.check,
                    )
                    .map_err(|error| format!("{}: {error}", recipe.kind))
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|handle| {
                handle
                    .join()
                    .unwrap_or_else(|_| Err("a server join panicked".to_owned()))
            })
            .collect()
    });
    let leases: Vec<shared::Lease> = joined.into_iter().collect::<Result<_, _>>()?;
    let [tls, plain, mismatch, sslonly, clientcert, directtls] = &leases[..] else {
        unreachable!("one lease per recipe");
    };
    let material = material(&tls.container_id)?;
    let path = |name: &str| material.join(name).to_string_lossy().into_owned();
    let url = |lease: &shared::Lease| {
        format!(
            "host=localhost port={} user=postgres password={PASSWORD} dbname=postgres",
            lease.port
        )
    };
    Ok(TlsServers {
        tls_url: url(tls),
        plain_url: url(plain),
        mismatch_url: url(mismatch),
        sslonly_url: url(sslonly),
        clientcert_url: format!(
            "host=localhost port={} user=postgres dbname=postgres",
            clientcert.port
        ),
        directtls_url: url(directtls),
        ca: path("ca.crt"),
        client_cert: path("client.crt"),
        client_key: path("client.key"),
        client_encrypted_key: path("client-encrypted.key"),
        client_key_password: CLIENT_KEY_PASSWORD.to_owned(),
        server_crl: path("server.crl"),
        server_crl_dir: path("server-crl-dir"),
        server_crl_wrong_hash_dir: path("server-crl-wrong-hash-dir"),
        _leases: leases,
    })
}

/// The recipe one server runs under, its identity keyed to everything that
/// changes what a ready server holds.
fn spec(image: &str, args: &[&str]) -> shared::Spec {
    let environment = vec![
        ("POSTGRES_PASSWORD".to_owned(), PASSWORD.to_owned()),
        (
            "POSTGRES_HOST_AUTH_METHOD".to_owned(),
            "scram-sha-256".to_owned(),
        ),
    ];
    let args: Vec<String> = ["docker-entrypoint.sh", "postgres"]
        .iter()
        .chain(args)
        .map(|argument| (*argument).to_owned())
        .collect();
    let list = |values: &[&str]| values.iter().map(|value| (*value).to_owned()).collect();
    let ready = shared::Readiness {
        log_marker: "PostgreSQL init process complete".to_owned(),
        probe: list(&[
            "pg_isready",
            "-U",
            "postgres",
            "-h",
            "127.0.0.1",
            "-p",
            "5432",
        ]),
        answer: list(&[
            "psql", "-U", "postgres", "-d", "postgres", "-tAc", "SELECT 1",
        ]),
    };
    let mut parts: Vec<Vec<u8>> = vec![image.as_bytes().to_vec()];
    for argument in &args {
        parts.push(argument.as_bytes().to_vec());
    }
    for (key, value) in &environment {
        parts.push(format!("{key}={value}").into_bytes());
    }
    let refs: Vec<&[u8]> = parts.iter().map(Vec::as_slice).collect();
    shared::Spec {
        inputs: shared::digest(&refs),
        image: image.to_owned(),
        environment,
        ports: vec![shared::Port {
            container: POSTGRES_PORT,
            host: shared::HostPort::Assigned,
        }],
        watchdog: shared::image::WATCHDOG.to_owned(),
        entrypoint: shared::Entrypoint::Image,
        args,
        ready,
    }
}

/// Ask the server, through `psql` in its own container, and return what it
/// printed, or why it refused.
fn libpq(boot: &shared::Boot, connection: &str, sql: &str) -> Result<String, String> {
    let password = format!("PGPASSWORD={PASSWORD}");
    let output = boot.exec(
        "env",
        &[
            &password,
            "psql",
            connection,
            "-v",
            "ON_ERROR_STOP=1",
            "-tAc",
            sql,
        ],
    )?;
    if output.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    } else {
        Err(output.stderr_text())
    }
}

/// Require libpq to answer `expected` for `sql` over `connection`.
fn require(boot: &shared::Boot, connection: &str, sql: &str, expected: &str) -> Result<(), String> {
    match libpq(boot, connection, sql) {
        Ok(answer) if answer == expected => Ok(()),
        Ok(answer) => Err(format!(
            "libpq over `{connection}` answered {answer:?} to `{sql}`, not {expected:?}"
        )),
        Err(error) => Err(format!("libpq refused `{connection}`: {error}")),
    }
}

/// Require libpq to refuse `connection`, because the claim is a refusal.
fn refuse(boot: &shared::Boot, connection: &str, why: &str) -> Result<(), String> {
    match libpq(boot, connection, "SELECT 1") {
        Ok(_) => Err(format!("libpq accepted `{connection}`, so {why}")),
        Err(_) => Ok(()),
    }
}

const SSL_VERSION: &str = "SELECT version FROM pg_stat_ssl WHERE pid = pg_backend_pid()";

fn check_tls(boot: &shared::Boot) -> Result<(), String> {
    require(
        boot,
        "host=127.0.0.1 user=postgres dbname=postgres sslmode=require",
        SSL_VERSION,
        "TLSv1.2",
    )?;
    // The hashed regular file means something to libpq/OpenSSL before it is
    // handed to the Rust implementation.
    refuse(
        boot,
        "host=localhost user=postgres dbname=postgres sslmode=verify-full \
         sslrootcert=/certs/ca.crt sslcrldir=/certs/server-crl-dir",
        "libpq ignored the hashed CRL that revokes this server's certificate",
    )?;
    // This server must not request a client certificate, or it cannot
    // discriminate `sslcertmode=require`.
    refuse(
        boot,
        "host=localhost user=postgres dbname=postgres sslmode=require \
         sslcert=/certs/client.crt sslkey=/certs/client.key sslcertmode=require",
        "this server requested a client certificate and cannot discriminate \
         sslcertmode=require",
    )
}

fn check_plain(boot: &shared::Boot) -> Result<(), String> {
    require(
        boot,
        "host=127.0.0.1 user=postgres dbname=postgres sslmode=disable",
        "SHOW ssl",
        "off",
    )
}

fn check_mismatch(boot: &shared::Boot) -> Result<(), String> {
    // libpq reaches it at verify-ca (chain good) and refuses it at verify-full
    // (name wrong): the discriminator the verification cases turn on, checked
    // with libpq rather than with the code under test.
    let verify_ca =
        "host=localhost user=postgres dbname=postgres sslmode=verify-ca sslrootcert=/certs/ca.crt";
    require(boot, verify_ca, SSL_VERSION, "TLSv1.3")?;
    require(
        boot,
        &format!("{verify_ca} sslcrldir=/certs/server-crl-dir"),
        "SELECT 1",
        "1",
    )?;
    refuse(
        boot,
        &format!("{verify_ca} sslcrldir=/certs/server-crl-wrong-hash-dir"),
        "libpq accepted a CRL directory holding no CRL under the issuer's hash",
    )?;
    refuse(
        boot,
        "host=localhost user=postgres dbname=postgres sslmode=verify-full sslrootcert=/certs/ca.crt",
        "its certificate is not mismatched",
    )
}

fn check_sslonly(boot: &shared::Boot) -> Result<(), String> {
    refuse(
        boot,
        "host=127.0.0.1 user=postgres dbname=postgres sslmode=disable",
        "it serves plaintext and its hba_file did not apply",
    )
}

fn check_clientcert(boot: &shared::Boot) -> Result<(), String> {
    let base =
        "host=localhost user=postgres dbname=postgres sslmode=verify-ca sslrootcert=/certs/ca.crt";
    // The same URL that works WITH the client certificate must fail without
    // it, or the test proving it is wired would pass against a server that
    // never asked.
    refuse(
        boot,
        base,
        "it accepted a connection with no client certificate",
    )?;
    require(
        boot,
        &format!("{base} sslcert=/certs/client.crt sslkey=/certs/client.key sslcertmode=require"),
        "SELECT 1",
        "1",
    )
}

fn check_directtls(boot: &shared::Boot) -> Result<(), String> {
    require(
        boot,
        "host=127.0.0.1 user=postgres dbname=postgres sslmode=require",
        "SELECT current_setting('server_version_num')::int >= 180000",
        "t",
    )
}

/// The client-side files, copied out of the `tls` server into a directory of
/// this worktree named for the CA that signed them.
fn material(container_id: &str) -> Result<PathBuf, String> {
    let read = |path: &str| -> Result<Vec<u8>, String> {
        let output = shared::exec_in_container(container_id, &["cat", path])?;
        if output.success() {
            Ok(output.stdout)
        } else {
            Err(format!(
                "read {path} in {container_id}: {}",
                output.stderr_text()
            ))
        }
    };
    let id = String::from_utf8_lossy(&read("/certs/material-id")?)
        .trim()
        .to_owned();
    if id.is_empty() || !id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(format!("the TLS image names its material {id:?}"));
    }
    let leases = root().join("target/zeroship-testkit");
    let directory = leases.join(format!("compio-postgres-tls-material-{id}"));
    if directory.is_dir() {
        return Ok(directory);
    }

    let listing = shared::exec_in_container(container_id, &["ls", "/certs/server-crl-dir"])?;
    let hashed = String::from_utf8_lossy(&listing.stdout).trim().to_owned();
    if !listing.success() || hashed.is_empty() || hashed.contains(char::is_whitespace) {
        return Err(format!(
            "/certs/server-crl-dir in {container_id} does not hold exactly one CRL: {hashed:?}"
        ));
    }

    // Written whole into a directory of this process's own and renamed into
    // place, so a reader never sees half of it. Another process copying the
    // same build at once renames an identical directory; whichever lands
    // first is the one both read.
    let staging = leases.join(format!(
        "compio-postgres-tls-material-{id}.{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&staging);
    let files: [(&str, String, u32); 7] = [
        ("ca.crt", "/certs/ca.crt".to_owned(), 0o644),
        ("client.crt", "/certs/client.crt".to_owned(), 0o644),
        ("client.key", "/certs/client.key".to_owned(), 0o600),
        (
            "client-encrypted.key",
            "/certs/client-encrypted.key".to_owned(),
            0o600,
        ),
        ("server.crl", "/certs/server.crl".to_owned(), 0o644),
        (
            "server-crl-dir",
            format!("/certs/server-crl-dir/{hashed}"),
            0o644,
        ),
        (
            "server-crl-wrong-hash-dir",
            "/certs/server-crl-wrong-hash-dir/00000000.r0".to_owned(),
            0o644,
        ),
    ];
    for (name, source, mode) in &files {
        let target = match *name {
            "server-crl-dir" => staging.join(name).join(&hashed),
            "server-crl-wrong-hash-dir" => staging.join(name).join("00000000.r0"),
            _ => staging.join(name),
        };
        write(&target, &read(source)?, *mode)?;
    }
    match std::fs::rename(&staging, &directory) {
        Ok(()) => Ok(directory),
        Err(_) if directory.is_dir() => {
            let _ = std::fs::remove_dir_all(&staging);
            Ok(directory)
        }
        Err(error) => Err(format!(
            "could not move {} to {}: {error}",
            staging.display(),
            directory.display()
        )),
    }
}

/// Write `bytes` to `path` with `mode`, creating its directory.
fn write(path: &Path, bytes: &[u8], mode: u32) -> Result<(), String> {
    let parent = path.parent().expect("a material file has a directory");
    std::fs::create_dir_all(parent)
        .map_err(|error| format!("could not create {}: {error}", parent.display()))?;
    std::fs::write(path, bytes)
        .map_err(|error| format!("could not write {}: {error}", path.display()))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .map_err(|error| format!("could not set the mode of {}: {error}", path.display()))
}

/// The repository root, three directories above this crate.
fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .expect("the testkit lives under libs/compio-postgres/")
        .to_owned()
}
