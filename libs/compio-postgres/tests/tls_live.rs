//! Live `sslmode` tests against real PostgreSQL servers.
//!
//! Constructing a connector proves nothing about a handshake, and neither does
//! a single successful connection: a `sslmode=require` connection that succeeds
//! against a TLS-enabled server might also have succeeded without any of this
//! code. So every claim here is paired with the case that must go the other
//! way, and the encryption claim is settled by the *server's* view of the
//! session (`pg_stat_ssl`), never by `connect()` returning `Ok`.
//!
//! Run with:
//!   libs/compio-postgres/tests/tls_live_setup.sh
//!   cargo test -p compio-postgres --features tls,live-tls-tests --test tls_live
//!
//! **This suite cannot skip.** It used to: a missing descriptor made every test
//! return early, which cargo counts as a pass, so the file's own header said "a
//! skipped run is not a passing run" while the file quietly delivered one. The
//! target now sits behind `required-features = ["tls", "live-tls-tests"]`. Without
//! the feature cargo does not build it at all - visibly absent, contributing no
//! green ticks. With the feature, a missing descriptor is a panic naming the
//! setup script. There is no longer a state in which this file reports success
//! without having connected to anything.

// This target deliberately carries many independent async behavioral pairs;
// their state-machine layouts exceed rustc's default query-depth budget.
#![recursion_limit = "256"]

use compio_postgres::config::SslCertMode;
use compio_postgres::{Client, Config, Error, MakeRustlsConnect, NoTls, Pool};
use futures_channel::oneshot;
use rustls::server::{ClientHello, ResolvesServerCert};
use std::sync::Mutex;

/// Written by `tls_live_setup.sh`. A file, not an environment variable,
/// because `libs/compio-postgres` may not read the environment outside
/// `tests/common/env.rs` (see the header of that file).
const DESCRIPTOR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/live/tls_live.conf");

/// A committed CA that signed nothing in this suite.
///
/// Used as a *wrong* trust anchor: it is a well-formed PEM, so it exercises
/// chain verification failing rather than `sslrootcert` failing to parse.
const FOREIGN_CA: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/verifier_ca.pem");

struct Servers {
    /// `ssl=on`, certificate for `localhost` signed by `ca`.
    tls_url: String,
    /// TLS off entirely.
    plain_url: String,
    /// `ssl=on`, certificate signed by `ca` for a name that is NOT `localhost`.
    /// The one server that separates `verify-ca` from `verify-full`.
    mismatch_url: String,
    /// `ssl=on`, `pg_hba` accepting `hostssl` only. The one server that makes
    /// `allow` reach its TLS leg.
    sslonly_url: String,
    /// `ssl=on` with `ssl_ca_file` and `cert` authentication: no password is
    /// accepted and the client must present a certificate.
    clientcert_url: String,
    /// PostgreSQL 18 with `ssl=on` and the same certificate as `tls`.
    directtls_url: String,
    /// The private CA that signed the certificates above. It is in no system
    /// trust store, which is what makes the `sslrootcert=system` case a real
    /// negative.
    ca: String,
    /// A client certificate signed by `ca`, with `CN=postgres`.
    client_cert: String,
    /// The private key for `client_cert`.
    client_key: String,
    /// The same key encoded as passphrase-encrypted PKCS#8.
    client_encrypted_key: String,
    /// The passphrase used for `client_encrypted_key`.
    client_key_password: String,
    /// A CRL issued by `ca` that revokes the exact certificate on `tls_url`.
    server_crl: String,
    /// The same CRL under its OpenSSL issuer-hash lookup name.
    server_crl_dir: String,
    /// The same CRL under a syntactically valid but incorrect issuer hash.
    server_crl_wrong_hash_dir: String,
}

impl Servers {
    fn load() -> Servers {
        let text = std::fs::read_to_string(DESCRIPTOR).unwrap_or_else(|e| {
            panic!(
                "cannot read {DESCRIPTOR}: {e}\n\
                 Run libs/compio-postgres/tests/tls_live_setup.sh first. This suite is opted \
                 into with --features live-tls-tests and does not skip."
            )
        });
        let field = |key: &str| {
            text.lines()
                .find_map(|line| line.strip_prefix(key)?.strip_prefix('='))
                .map(str::to_owned)
                .unwrap_or_else(|| panic!("{DESCRIPTOR} has no {key}=; re-run tls_live_setup.sh"))
        };
        Servers {
            tls_url: field("tls_url"),
            plain_url: field("plain_url"),
            mismatch_url: field("mismatch_url"),
            sslonly_url: field("sslonly_url"),
            clientcert_url: field("clientcert_url"),
            directtls_url: field("directtls_url"),
            ca: field("ca"),
            client_cert: field("client_cert"),
            client_key: field("client_key"),
            client_encrypted_key: field("client_encrypted_key"),
            client_key_password: field("client_key_password"),
            server_crl: field("server_crl"),
            server_crl_dir: field("server_crl_dir"),
            server_crl_wrong_hash_dir: field("server_crl_wrong_hash_dir"),
        }
    }
}

fn servers() -> Servers {
    Servers::load()
}

/// Ask the server - not the client - whether the session is encrypted.
///
/// `pg_stat_ssl.ssl` is the backend's own record of the connection it is
/// serving. A client-side assertion could only report what the client believes
/// it negotiated.
async fn server_reports_ssl(client: &Client) -> (bool, String) {
    let row = client
        .query_one(
            "SELECT ssl, coalesce(version, '') FROM pg_stat_ssl WHERE pid = pg_backend_pid()",
            &[],
        )
        .await
        .expect("pg_stat_ssl");
    (row.get(0), row.get(1))
}

/// Connect, and report what the SERVER says the transport was.
///
/// Prints the verdict as well as returning it. `cargo test ... -- --nocapture`
/// then emits the whole `pg_stat_ssl` matrix as measured output, which is the
/// evidence this suite exists to produce - a reader should not have to infer
/// what the server saw from the names of the tests that passed.
async fn transport_of(url: &str) -> Result<bool, Error> {
    let redacted = url
        .replace(PASSWORD_MARKER, "***")
        .replace(KEY_PASSWORD_MARKER, "***")
        .replace(WRONG_KEY_PASSWORD_MARKER, "***");
    let pool = match Pool::connect(url, 1).await {
        Ok(pool) => pool,
        Err(e) => {
            println!("  [pg_stat_ssl] REFUSED  {redacted}\n               {}", describe(&e));
            return Err(e);
        }
    };
    let client = pool.get().await?;
    let (ssl, version) = server_reports_ssl(&client).await;
    if ssl {
        assert!(
            version.starts_with("TLSv"),
            "pg_stat_ssl.ssl is true but version = {version:?}"
        );
    } else {
        assert_eq!(version, "", "an unencrypted session reported a TLS version");
    }
    println!(
        "  [pg_stat_ssl] ssl={ssl:<5} version={:<8} {redacted}",
        if version.is_empty() { "-" } else { &version }
    );
    Ok(ssl)
}

/// The fixtures use fixed secrets. Replacing the exact values also redacts a
/// future quoted DSN value containing whitespace.
const PASSWORD_MARKER: &str = "compio-postgres-tls-test";
const KEY_PASSWORD_MARKER: &str = "compio-postgres-encrypted-key-test";
const WRONG_KEY_PASSWORD_MARKER: &str = "definitely-wrong";

fn describe(err: &Error) -> String {
    format!("{err}: {:?}", std::error::Error::source(err))
}

#[derive(Debug)]
struct SniRecorder {
    sender: Mutex<Option<oneshot::Sender<Option<String>>>>,
}

impl ResolvesServerCert for SniRecorder {
    fn resolve(
        &self,
        client_hello: ClientHello<'_>,
    ) -> Option<std::sync::Arc<rustls::sign::CertifiedKey>> {
        self.sender
            .lock()
            .expect("lock SNI recorder")
            .take()
            .expect("record exactly one ClientHello")
            .send(client_hello.server_name().map(str::to_owned))
            .expect("deliver observed SNI");

        // Parsing the real ClientHello is this server's whole job. Ending the
        // handshake here keeps the assertion independent of certificate
        // generation and PostgreSQL startup.
        None
    }
}

/// Capture SNI from the server side of a serialized ClientHello.
async fn sni_offered_by(option: &str) -> Option<String> {
    let listener = compio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind SNI recorder");
    let address = listener.local_addr().expect("SNI recorder address");
    let (sender, receiver) = oneshot::channel();
    let server_config = rustls::ServerConfig::builder_with_provider(std::sync::Arc::new(
        rustls::crypto::aws_lc_rs::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .expect("safe TLS protocol versions")
    .with_no_client_auth()
    .with_cert_resolver(std::sync::Arc::new(SniRecorder {
        sender: Mutex::new(Some(sender)),
    }));
    let acceptor = compio_tls::TlsAcceptor::from(std::sync::Arc::new(server_config));
    let server = compio::runtime::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept SNI probe");
        assert!(
            acceptor.accept(stream).await.is_err(),
            "capture-only TLS server unexpectedly completed a handshake"
        );
    });

    let dsn = format!(
        "host=localhost hostaddr=127.0.0.1 port={} user=postgres sslmode=require \
         sslnegotiation=direct {option}",
        address.port()
    );
    let config = dsn.parse::<Config>().expect("parse SNI probe DSN");
    let tls = MakeRustlsConnect::from_config(&config).expect("build SNI probe connector");
    let connection = compio::time::timeout(
        std::time::Duration::from_secs(5),
        config.connect(tls),
    )
    .await
    .expect("SNI probe connection timed out");
    assert!(
        connection.is_err(),
        "capture-only TLS server completed a connection after ClientHello"
    );
    compio::time::timeout(std::time::Duration::from_secs(5), server)
        .await
        .expect("SNI recorder task timed out")
        .expect("SNI recorder task");
    compio::time::timeout(std::time::Duration::from_secs(5), receiver)
        .await
        .expect("observed SNI delivery timed out")
        .expect("receive observed SNI")
}

/// `sslsni` reaches rustls' wire output, rather than merely surviving parsing.
/// The hostname is retained while `hostaddr` bypasses DNS; an IP-only endpoint
/// would omit SNI in both modes and make this test vacuous.
#[compio::test]
async fn sslsni_controls_the_client_hello_extension() {
    assert_eq!(
        sni_offered_by("").await.as_deref(),
        Some("localhost"),
        "the default must send SNI"
    );
    assert_eq!(
        sni_offered_by("sslsni=1").await.as_deref(),
        Some("localhost"),
        "sslsni=1 must send SNI"
    );
    assert_eq!(
        sni_offered_by("sslsni=0").await,
        None,
        "sslsni=0 must omit SNI"
    );
}

// ---------------------------------------------------------------------------
// sslnegotiation - PostgreSQL SSLRequest or direct TLS
// ---------------------------------------------------------------------------

/// PostgreSQL 18 completes both negotiation methods over the same verified
/// endpoint, and the server reports both sessions as encrypted.
///
/// This test does NOT catch an ignored `sslnegotiation=direct`: PostgreSQL 18
/// accepts the PostgreSQL negotiation method too, so the discriminator below
/// is what makes the direct success meaningful.
#[compio::test]
async fn postgres_18_accepts_direct_and_postgres_tls_negotiation() {
    let s = servers();
    let base = format!(
        "{} sslmode=verify-full sslrootcert={}",
        s.directtls_url, s.ca
    );

    assert!(
        transport_of(&format!("{base} sslnegotiation=direct"))
            .await
            .expect("PostgreSQL 18 must complete direct TLS"),
        "the direct PostgreSQL 18 session was not encrypted"
    );
    assert!(
        transport_of(&format!("{base} sslnegotiation=postgres"))
            .await
            .expect("PostgreSQL 18 must complete PostgreSQL SSL negotiation"),
        "the PostgreSQL-negotiated PostgreSQL 18 session was not encrypted"
    );
}

/// Direct negotiation must fail on PostgreSQL 16 while the same direct client
/// path succeeds on PostgreSQL 18 and ordinary PostgreSQL negotiation still
/// succeeds on PostgreSQL 16. Together those controls prove the setting changes
/// bytes on the wire instead of being accepted and ignored.
///
/// This test does NOT inspect the negotiated ALPN: neither `Client` nor
/// `Connection` exposes it. PostgreSQL 18 accepting the direct session is the
/// live server-side result; the connector's wire-level ALPN offer is covered by
/// the in-crate rustls tests.
#[compio::test]
async fn direct_negotiation_distinguishes_postgres_18_from_postgres_16() {
    let s = servers();
    let pg16 = format!(
        "{} sslmode=verify-full sslrootcert={}",
        s.tls_url, s.ca
    );
    let pg18 = format!(
        "{} sslmode=verify-full sslrootcert={}",
        s.directtls_url, s.ca
    );

    println!("  [direct discriminator] PostgreSQL 16, sslnegotiation=direct");
    let pg16_direct = transport_of(&format!("{pg16} sslnegotiation=direct"))
        .await
        .expect_err("PostgreSQL 16 must reject direct TLS negotiation");
    let refusal = describe(&pg16_direct);
    assert!(
        refusal.contains("TLS handshake"),
        "PostgreSQL 16 direct TLS failed before reaching the handshake: {refusal}"
    );

    println!("  [direct discriminator] PostgreSQL 18, sslnegotiation=direct");
    assert!(
        transport_of(&format!("{pg18} sslnegotiation=direct"))
            .await
            .expect("the same direct TLS path must connect to PostgreSQL 18"),
        "the direct PostgreSQL 18 control was not encrypted"
    );

    println!("  [direct discriminator] PostgreSQL 16, sslnegotiation=postgres");
    assert!(
        transport_of(&format!("{pg16} sslnegotiation=postgres"))
            .await
            .expect("PostgreSQL negotiation must still connect to PostgreSQL 16"),
        "the PostgreSQL-negotiated PostgreSQL 16 control was not encrypted"
    );
}

// ---------------------------------------------------------------------------
// disable
// ---------------------------------------------------------------------------

/// The control for every "ssl = true" below. Same pool, same server, same code
/// path except for `sslmode` - and the server must report an UNencrypted
/// session. Without it, "ssl = true" would be consistent with a `pg_stat_ssl`
/// that reports true for everything.
#[compio::test]
async fn disable_is_plaintext_even_against_a_tls_server() {
    let s = servers();
    assert!(
        !transport_of(&format!("{} sslmode=disable", s.tls_url))
            .await
            .expect("disable connects"),
        "sslmode=disable produced an encrypted session"
    );
}

// ---------------------------------------------------------------------------
// prefer - TLS first, plaintext second
// ---------------------------------------------------------------------------

/// `prefer` against a TLS server encrypts.
#[compio::test]
async fn prefer_uses_tls_when_the_server_offers_it() {
    let s = servers();
    assert!(
        transport_of(&format!("{} sslmode=prefer", s.tls_url))
            .await
            .expect("prefer connects"),
        "prefer did not use TLS against a server that offers it"
    );
}

/// `prefer` against a plaintext-only server connects anyway, unencrypted.
///
/// This is the half of `prefer` the pool used to get wrong in the other
/// direction: it hardcoded plaintext, so this passed for the wrong reason and
/// the test above could not have passed at all.
#[compio::test]
async fn prefer_falls_back_to_plaintext_when_the_server_refuses_tls() {
    let s = servers();
    assert!(
        !transport_of(&format!("{} sslmode=prefer", s.plain_url))
            .await
            .expect("prefer must connect to a plaintext-only server, not error"),
        "prefer reported an encrypted session against a server with TLS off"
    );
}

/// The reconnect, and the sharp edge that comes with it.
///
/// `prefer` with a trust anchor that did not sign this server: the chain fails,
/// which is a handshake failure, which `prefer` answers by dialling a NEW
/// socket and proceeding in the clear. The server reports an unencrypted
/// session, so the fallback demonstrably happened on a second connection - the
/// first one had already exchanged TLS records and could not have carried a
/// plaintext startup packet.
///
/// It is also libpq's documented-by-behaviour silent downgrade: a certificate
/// that FAILED verification produced a working, unencrypted session and no
/// error. See the comment on the retry arm in `src/connect.rs`.
#[compio::test]
async fn prefer_reconnects_in_plaintext_after_a_failed_handshake() {
    let s = servers();
    let url = format!("{} sslmode=prefer sslrootcert={FOREIGN_CA}", s.tls_url);
    assert!(
        !transport_of(&url).await.expect("prefer must not fail here"),
        "prefer did not fall back after the certificate failed to verify"
    );
}

/// The control for the test above, differing in one variable: the mode.
/// `require` has no plaintext in its allowed set, so the same failed chain is
/// final.
#[compio::test]
async fn require_does_not_reconnect_after_a_failed_handshake() {
    let s = servers();
    let url = format!("{} sslmode=require sslrootcert={FOREIGN_CA}", s.tls_url);
    let err = transport_of(&url)
        .await
        .expect_err("require must not fall back to plaintext");
    let text = describe(&err);
    assert!(
        text.contains("UnknownIssuer") || text.contains("invalid peer certificate"),
        "expected a certificate-verification failure, got: {text}"
    );
}

// ---------------------------------------------------------------------------
// allow - plaintext first, TLS second
// ---------------------------------------------------------------------------

/// `allow` against an ordinary server uses plaintext: it is the transport
/// `allow` offers first, and nothing refused it.
#[compio::test]
async fn allow_uses_plaintext_when_plaintext_is_accepted() {
    let s = servers();
    assert!(
        !transport_of(&format!("{} sslmode=allow", s.plain_url))
            .await
            .expect("allow connects"),
        "allow used TLS against a server that accepted plaintext"
    );
}

/// `allow` against an `hostssl`-only server uses TLS - on a second connection,
/// after the plaintext one was refused.
///
/// This is the whole reason `allow` exists, and the case that distinguishes it
/// from `disable`. Paired with the test above it also proves the ORDER: the
/// same mode, against two servers, picks a different transport each time, and
/// picks TLS only where plaintext was unavailable.
#[compio::test]
async fn allow_retries_with_tls_when_plaintext_is_refused() {
    let s = servers();
    assert!(
        transport_of(&format!("{} sslmode=allow", s.sslonly_url))
            .await
            .expect("allow must retry with TLS, not surface the plaintext refusal"),
        "allow did not reach its TLS leg against an hostssl-only server"
    );
}

/// And `disable` against that same server fails, which is what makes the test
/// above a statement about the retry rather than about the server being
/// permissive.
#[compio::test]
async fn disable_cannot_reach_an_hostssl_only_server() {
    let s = servers();
    transport_of(&format!("{} sslmode=disable", s.sslonly_url))
        .await
        .expect_err("the ssl-only server must refuse a plaintext session");
}

// ---------------------------------------------------------------------------
// require - encrypted, and unverified unless given anchors
// ---------------------------------------------------------------------------

/// `require` with no `sslrootcert` connects to a server whose certificate no
/// trust store on this machine knows, and the session is encrypted.
///
/// This is libpq's `require`, stated as a live fact rather than as a comment:
/// eavesdropping protection, no MITM protection. If this ever starts failing
/// because verification crept back in, `require` has stopped being `require`.
#[compio::test]
async fn require_encrypts_without_verifying_an_untrusted_certificate() {
    let s = servers();
    assert!(
        transport_of(&format!("{} sslmode=require", s.tls_url))
            .await
            .expect("require must accept an unverified certificate"),
        "require did not encrypt"
    );
    // The same, against a certificate issued for the wrong name entirely.
    assert!(
        transport_of(&format!("{} sslmode=require", s.mismatch_url))
            .await
            .expect("require does not check the host name"),
        "require did not encrypt"
    );
}

/// Give `require` trust anchors and it becomes `verify-ca`, exactly as the
/// documentation says. One variable differs from the test above: the presence
/// of `sslrootcert`.
#[compio::test]
async fn require_verifies_the_chain_once_a_ca_is_configured() {
    let s = servers();
    assert!(
        transport_of(&format!("{} sslmode=require sslrootcert={}", s.tls_url, s.ca))
            .await
            .expect("the real CA verifies"),
        "require did not encrypt"
    );
    transport_of(&format!(
        "{} sslmode=require sslrootcert={FOREIGN_CA}",
        s.tls_url
    ))
    .await
    .expect_err("require with a CA configured must reject a chain that does not reach it");
}

/// `require` against a server with TLS switched off must FAIL.
///
/// This is what distinguishes "the connector works" from "the pool ignored
/// sslmode and connected in plaintext". `prefer` legitimately falls back here;
/// `require` must not.
#[compio::test]
async fn require_fails_against_a_server_without_tls() {
    let s = servers();
    let err = transport_of(&format!("{} sslmode=require", s.plain_url))
        .await
        .expect_err("sslmode=require must not connect to a plaintext-only server");
    let text = describe(&err);
    assert!(
        text.contains("does not support SSL"),
        "unexpected error: {text}"
    );
}

// ---------------------------------------------------------------------------
// protocol bounds - handshake enforcement, not parser state
// ---------------------------------------------------------------------------

/// The server accepts TLS 1.2 only. These two URLs differ solely in the
/// minimum-version value, so their opposite outcomes prove the bound reaches
/// the handshake.
#[compio::test]
async fn ssl_min_protocol_version_is_enforced_by_the_handshake() {
    let s = servers();
    let base = format!("{} sslmode=require ssl_min_protocol_version=", s.tls_url);

    assert!(
        transport_of(&format!("{base}TLSv1.2"))
            .await
            .expect("TLS 1.2 satisfies a TLSv1.2 minimum"),
        "the TLS-1.2 control was not encrypted"
    );
    transport_of(&format!("{base}TLSv1.3"))
        .await
        .expect_err("a TLSv1.3 minimum must refuse a TLS-1.2-only server");
}

/// The server accepts TLS 1.3 only. These two URLs differ solely in the
/// maximum-version value.
#[compio::test]
async fn ssl_max_protocol_version_is_enforced_by_the_handshake() {
    let s = servers();
    let base = format!(
        "{} sslmode=require ssl_max_protocol_version=",
        s.mismatch_url
    );

    assert!(
        transport_of(&format!("{base}TLSv1.3"))
            .await
            .expect("TLS 1.3 satisfies a TLSv1.3 maximum"),
        "the TLS-1.3 control was not encrypted"
    );
    transport_of(&format!("{base}TLSv1.2"))
        .await
        .expect_err("a TLSv1.2 maximum must refuse a TLS-1.3-only server");
}

// ---------------------------------------------------------------------------
// verify-ca and verify-full - THE discriminating pair
// ---------------------------------------------------------------------------

/// `verify-ca` checks the chain and connects.
#[compio::test]
async fn verify_ca_connects_against_its_own_ca() {
    let s = servers();
    assert!(
        transport_of(&format!("{} sslmode=verify-ca sslrootcert={}", s.tls_url, s.ca))
            .await
            .expect("verify-ca with the signing CA"),
        "verify-ca did not encrypt"
    );
}

/// THE PAIR, live. The same server, the same CA, the same code path; only the
/// mode differs. `verify-ca` accepts a certificate issued for another name and
/// `verify-full` rejects it.
///
/// A verifier that checked nothing would pass the first and fail the second; a
/// verifier that checked the name in both would fail the first and pass the
/// second. Only a correct implementation produces this shape, which is why
/// this is the case that catches a no-op verifier.
#[compio::test]
async fn only_verify_full_rejects_a_host_name_mismatch() {
    let s = servers();

    assert!(
        transport_of(&format!(
            "{} sslmode=verify-ca sslrootcert={}",
            s.mismatch_url, s.ca
        ))
        .await
        .expect("verify-ca must NOT check the host name"),
        "verify-ca did not encrypt"
    );

    let err = transport_of(&format!(
        "{} sslmode=verify-full sslrootcert={}",
        s.mismatch_url, s.ca
    ))
    .await
    .expect_err("verify-full must reject a certificate issued for another name");
    let text = describe(&err);
    assert!(
        text.contains("NotValidForName") || text.contains("invalid peer certificate"),
        "expected a host-name failure, got: {text}"
    );
}

/// `verify-full` against the server whose certificate really does name it.
#[compio::test]
async fn verify_full_connects_when_the_name_matches() {
    let s = servers();
    assert!(
        transport_of(&format!(
            "{} sslmode=verify-full sslrootcert={}",
            s.tls_url, s.ca
        ))
        .await
        .expect("verify-full with the right name and the signing CA"),
        "verify-full did not encrypt"
    );
}

/// Verification is real: the same server, the same mode, but pointed at the OS
/// trust store instead of the CA that signed it.
///
/// This is the test that would catch a connector wired up with a no-op
/// certificate verifier - the failure mode where every other assertion in this
/// file still passes.
#[compio::test]
async fn verify_full_fails_against_system_roots() {
    let s = servers();
    let err = transport_of(&format!("{} sslmode=verify-full sslrootcert=system", s.tls_url))
        .await
        .expect_err("a certificate signed by an untrusted private CA must be rejected");
    let text = describe(&err);
    assert!(
        text.contains("UnknownIssuer") || text.contains("invalid peer certificate"),
        "expected a certificate-verification failure, got: {text}"
    );
}

/// `sslrootcert=system` under anything weaker than `verify-full` is refused
/// before a socket is opened, as libpq refuses it.
///
/// Note the mechanism differs from the test above: that one is a handshake
/// failure, this one is a configuration error. Both are failures, and
/// conflating them would hide the rule.
#[compio::test]
async fn system_roots_are_refused_under_a_weaker_mode() {
    let s = servers();
    for mode in ["require", "verify-ca", "prefer"] {
        let err = transport_of(&format!("{} sslmode={mode} sslrootcert=system", s.tls_url))
            .await
            .unwrap_err();
        let text = describe(&err);
        assert!(
            text.contains("sslrootcert=system") && text.contains("verify-full"),
            "sslmode={mode}: expected the sslrootcert=system rule, got: {text}"
        );
    }
}

/// The verifying modes refuse to run with nothing to verify against, rather
/// than silently degrading to `require`.
#[compio::test]
async fn verifying_modes_demand_trust_anchors() {
    let s = servers();
    for mode in ["verify-ca", "verify-full"] {
        let err = transport_of(&format!("{} sslmode={mode}", s.tls_url))
            .await
            .unwrap_err();
        let text = describe(&err);
        assert!(
            text.contains("trust anchors") && text.contains(mode),
            "sslmode={mode}: expected a missing-anchors error, got: {text}"
        );
    }
}

// ---------------------------------------------------------------------------
// Channel binding and configuration errors
// ---------------------------------------------------------------------------

/// `channel_binding=require` forces SCRAM-SHA-256-PLUS, which fails unless the
/// `tls-server-end-point` value we compute equals the one the server computed
/// from the same certificate.
///
/// So this is the end-to-end check on `tls_server_end_point`: the unit tests in
/// `src/tls_rustls.rs` only show that the digest width follows the signature
/// algorithm.
#[compio::test]
async fn channel_binding_require_completes_scram_plus() {
    let s = servers();
    let url = format!(
        "{} sslmode=require sslrootcert={} channel_binding=require",
        s.tls_url, s.ca
    );
    assert!(transport_of(&url).await.expect("SCRAM-SHA-256-PLUS"));
}

/// Channel binding is available at `require` with NO verification at all, which
/// is where it matters most: SCRAM-PLUS is then the only thing authenticating
/// the server. libpq has no `sslmode` gate on channel binding either.
#[compio::test]
async fn channel_binding_works_at_require_without_any_verification() {
    let s = servers();
    let url = format!("{} sslmode=require channel_binding=require", s.tls_url);
    assert!(
        transport_of(&url)
            .await
            .expect("unverified TLS still offers tls-server-end-point"),
        "session was not encrypted"
    );
}

/// `channel_binding=require` over a plaintext connection has no binding to
/// offer, so it must fail rather than silently downgrading to plain SCRAM.
#[compio::test]
async fn channel_binding_require_fails_without_tls() {
    let s = servers();
    let err = transport_of(&format!(
        "{} sslmode=disable channel_binding=require",
        s.tls_url
    ))
    .await
    .expect_err("channel_binding=require must not succeed over plaintext");
    let text = describe(&err);
    // Names the ARM, not just the topic. Over plaintext the server never
    // advertises SCRAM-SHA-256-PLUS, so the server-side downgrade guard is the
    // one that must fire. Asserting only "channel binding" cannot tell it from
    // the backend-support guard, and that guard also fires here -- so the weaker
    // assertion stayed green even with the first guard deleted entirely.
    assert!(
        text.contains("SCRAM-SHA-256-PLUS"),
        "expected the omitted-mechanism refusal, got: {text}"
    );
}

// ---------------------------------------------------------------------------
// Client certificates
// ---------------------------------------------------------------------------

/// `sslcert` / `sslkey` are not merely parsed - they are sent, and the server
/// authenticates the session with them.
///
/// The server here uses `cert` authentication: `ssl_ca_file` is set and
/// `pg_hba` accepts no password at all. The URL carries no password either. So
/// a session that reaches `SELECT` did so because rustls presented the client
/// certificate and PostgreSQL mapped its `CN=postgres` to the role. Supplying
/// `sslpassword` here also proves it is ignored for this unencrypted key.
#[compio::test]
async fn client_certificates_authenticate_the_session() {
    let s = servers();
    let url = format!(
        "{} sslmode=verify-full sslrootcert={} sslcert={} sslkey={} sslpassword={}",
        s.clientcert_url, s.ca, s.client_cert, s.client_key, s.client_key_password
    );
    assert!(
        transport_of(&url)
            .await
            .expect("the client certificate must authenticate"),
        "client-certificate session was not encrypted"
    );
}

/// One variable, opposite outcomes: both attempts name the same certificate
/// and key against the same `cert`-authentication server. `allow` sends the
/// requested identity; `disable` must not even load or send it.
#[compio::test]
async fn sslcertmode_controls_whether_the_same_certificate_is_sent() {
    let s = servers();
    let base = format!(
        "{} sslmode=verify-full sslrootcert={} sslcert={} sslkey={} sslcertmode=",
        s.clientcert_url, s.ca, s.client_cert, s.client_key
    );

    assert!(
        transport_of(&format!("{base}allow"))
            .await
            .expect("sslcertmode=allow must send the requested certificate"),
        "the allowed client-certificate session was not encrypted"
    );

    let refused = transport_of(&format!("{base}disable"))
        .await
        .expect_err("sslcertmode=disable must withhold the same certificate");
    assert!(
        describe(&refused).contains("certificate") || describe(&refused).contains("pg_hba"),
        "the server did not refuse the missing client identity: {}",
        describe(&refused)
    );
}

/// `require` observes CertificateRequest, not just configured file paths. The
/// ordinary password-authentication server does not request a client
/// certificate: `allow` succeeds, while changing only the mode to `require`
/// refuses the otherwise successful session.
#[compio::test]
async fn sslcertmode_require_needs_a_server_certificate_request() {
    let s = servers();
    let base = format!(
        "{} sslmode=verify-full sslrootcert={} sslcert={} sslkey={} sslcertmode=",
        s.tls_url, s.ca, s.client_cert, s.client_key
    );

    assert!(
        transport_of(&format!("{base}allow"))
            .await
            .expect("allow does not require the server to request the configured certificate"),
        "the allow control was not encrypted"
    );

    let refused = transport_of(&format!("{base}require"))
        .await
        .expect_err("require must reject a server that sent no CertificateRequest");
    let text = describe(&refused);
    assert!(
        text.contains("sslcertmode=require") && text.contains("did not request"),
        "the refusal did not identify the missing CertificateRequest: {text}"
    );
}

/// A plaintext fallback must not bypass `sslcertmode=require`. Both attempts
/// name the same identity against the same non-TLS server; changing only the
/// certificate mode turns an otherwise successful plaintext connection into a
/// refusal.
#[compio::test]
async fn sslcertmode_require_cannot_finish_on_plaintext() {
    let s = servers();
    let base = format!(
        "{} sslmode=prefer sslcert={} sslkey={} sslcertmode=",
        s.plain_url, s.client_cert, s.client_key
    );

    assert!(
        !transport_of(&format!("{base}allow"))
            .await
            .expect("allow may finish on the non-TLS server"),
        "the allow control unexpectedly negotiated TLS"
    );

    let refused = transport_of(&format!("{base}require"))
        .await
        .expect_err("require must reject the plaintext fallback");
    let text = describe(&refused);
    assert!(
        text.contains("sslcertmode=require") && text.contains("did not request"),
        "the plaintext refusal did not name the unmet certificate mode: {text}"
    );
}

/// A connector that verifies nothing cannot be used to serve a verifying
/// `sslmode`, and the proof is that the very same connector DOES complete the
/// same connection under `sslmode=require`.
///
/// The mismatch server is what makes this a real hole rather than bookkeeping.
/// Its certificate is signed by the CA but carries the wrong name, so the
/// second half below is a session that a `verify-full` verifier would have
/// rejected and this connector accepts. Handing that connector a `verify-full`
/// `Config` therefore used to produce a live, queryable session whose
/// certificate nothing had checked - `pg_stat_ssl` said `ssl=true` and the
/// connection string said `verify-full`, and neither was a claim about
/// identity. `connect_raw` now refuses it by name.
#[compio::test]
async fn a_connector_that_verifies_nothing_cannot_serve_a_verifying_sslmode() {
    let s = servers();

    let unverified = format!("{} sslmode=require", s.mismatch_url);
    let unverified_config = unverified.parse::<Config>().expect("parse the require DSN");
    let make =
        MakeRustlsConnect::from_config(&unverified_config).expect("build the require connector");

    // The refusal. Same connector, a Config that asks for verification.
    let verifying_config = format!(
        "{} sslmode=verify-full sslrootcert={}",
        s.mismatch_url, s.ca
    )
    .parse::<Config>()
    .expect("parse the verify-full DSN");
    let error = match verifying_config.connect(make.clone()).await {
        Ok(_) => panic!("a connector that verifies nothing served sslmode=verify-full"),
        Err(error) => error,
    };
    assert!(
        describe(&error).contains("sslmode=verify-full"),
        "the unverifying connector was not refused by key: {}",
        describe(&error)
    );

    // The partner, differing in one variable: the Config the connector was
    // built from. It must connect, or the assertion above would be satisfied
    // by a connector that simply cannot reach this server.
    let (client, connection) = unverified_config
        .connect(make.clone())
        .await
        .expect("sslmode=require accepts the mismatched name it makes no promise about");
    let task = compio::runtime::spawn(async move {
        let _ = connection.run().await;
    });
    let (ssl, version) = server_reports_ssl(&client).await;
    assert!(ssl, "the require session was not encrypted");
    println!(
        "  [pg_stat_ssl] ssl={ssl:<5} version={version:<8} {} (unattested connector)",
        unverified.replace(PASSWORD_MARKER, "***")
    );
    drop(client);
    let _ = task.await;

    // And the control that keeps `verify-full` meaningful: the connector this
    // crate builds FOR verify-full reaches the same server and rejects it on
    // the host name rather than on the attestation.
    let verifying_make =
        MakeRustlsConnect::from_config(&verifying_config).expect("build the verify-full connector");
    let error = match verifying_config.connect(verifying_make).await {
        Ok(_) => panic!("verify-full accepted a certificate issued for another name"),
        Err(error) => error,
    };
    let text = describe(&error);
    assert!(
        !text.contains("does not attest"),
        "verify-full must fail on the certificate, not on the attestation: {text}"
    );
    assert!(
        text.contains("NotValidForName") || text.contains("not valid for name"),
        "verify-full must name the host-name failure: {text}"
    );
}

/// A connector built from a different configuration must fail by parameter
/// name before its stale policy can reach a ClientHello.
#[compio::test]
async fn stale_tls_connector_cannot_override_sni_or_certificate_mode() {
    let s = servers();
    let dsn = format!(
        "{} sslmode=verify-full sslrootcert={} sslcert={} sslkey={} \
         sslsni=1 sslcertmode=allow",
        s.tls_url, s.ca, s.client_cert, s.client_key
    );

    let mut sni_config = dsn.parse::<Config>().expect("parse stale SNI control");
    let stale_sni =
        MakeRustlsConnect::from_config(&sni_config).expect("build SNI-enabled connector");
    sni_config.ssl_sni(false);
    let sni_error = match sni_config.connect(stale_sni).await {
        Ok(_) => panic!("a stale connector silently overrode sslsni=0"),
        Err(error) => error,
    };
    assert!(
        describe(&sni_error).contains("sslsni=0"),
        "the stale SNI connector was not refused by key: {}",
        describe(&sni_error)
    );

    let mut cert_config = dsn
        .parse::<Config>()
        .expect("parse stale certificate-mode control");
    let stale_cert =
        MakeRustlsConnect::from_config(&cert_config).expect("build certificate-allow connector");
    cert_config.ssl_cert_mode(SslCertMode::Disable);
    let cert_error = match cert_config.connect(stale_cert).await {
        Ok(_) => panic!("a stale connector silently overrode sslcertmode=disable"),
        Err(error) => error,
    };
    assert!(
        describe(&cert_error).contains("sslcertmode=disable"),
        "the stale certificate connector was not refused by key: {}",
        describe(&cert_error)
    );
}

/// The positive half for `require`: a server that requests the configured
/// certificate and authenticates it must be accepted.
#[compio::test]
async fn sslcertmode_require_accepts_a_requested_certificate() {
    let s = servers();
    let url = format!(
        "{} sslmode=verify-full sslrootcert={} sslcert={} sslkey={} sslcertmode=require",
        s.clientcert_url, s.ca, s.client_cert, s.client_key
    );
    assert!(
        transport_of(&url)
            .await
            .expect("the requested and selected certificate must satisfy require"),
        "the required client-certificate session was not encrypted"
    );
}

/// This driver deliberately has no implicit `~/.postgresql` certificate
/// lookup. With no configured identity, `require` therefore has no possible
/// successful outcome and fails before opening a socket, naming all three
/// relevant parameters.
#[compio::test]
async fn sslcertmode_require_without_configured_identity_fails_loudly() {
    let s = servers();
    let err = transport_of(&format!(
        "{} sslmode=verify-full sslrootcert={} sslcertmode=require",
        s.clientcert_url, s.ca
    ))
    .await
    .expect_err("require without sslcert/sslkey cannot be honoured");
    let text = describe(&err);
    assert!(
        text.contains("sslcertmode=require")
            && text.contains("sslcert")
            && text.contains("sslkey"),
        "the impossible client-certificate configuration was unclear: {text}"
    );
}

/// Disabled certificate use must not touch the configured identity files.
/// These paths do not exist; reaching the password-authenticated server proves
/// the mode branched before certificate/key loading.
#[compio::test]
async fn sslcertmode_disable_does_not_load_client_identity_files() {
    let s = servers();
    let url = format!(
        "{} sslmode=verify-full sslrootcert={} sslcertmode=disable \
         sslcert=/definitely/missing/client.crt sslkey=/definitely/missing/client.key",
        s.tls_url, s.ca
    );
    assert!(
        transport_of(&url)
            .await
            .expect("disabled client-certificate files must not be opened"),
        "the disable control was not encrypted"
    );
}

/// A real encrypted PKCS#8 key must be unusable with the wrong passphrase and
/// usable with the right one. Both attempts present the same certificate to
/// the same `cert`-authentication server; only `sslpassword` changes.
#[compio::test]
async fn encrypted_client_key_requires_matching_sslpassword() {
    let s = servers();
    let base = format!(
        "{} sslmode=verify-full sslrootcert={} sslcert={} sslkey={}",
        s.clientcert_url, s.ca, s.client_cert, s.client_encrypted_key
    );

    let wrong = transport_of(&format!("{base} sslpassword={WRONG_KEY_PASSWORD_MARKER}"))
        .await
        .expect_err("the wrong private-key passphrase must be refused");
    assert!(
        describe(&wrong).contains("sslkey"),
        "the refusal must come from loading the encrypted key: {}",
        describe(&wrong)
    );

    assert!(
        transport_of(&format!("{base} sslpassword={}", s.client_key_password))
            .await
            .expect("the matching private-key passphrase must authenticate"),
        "encrypted client-key session was not encrypted"
    );
}

/// Revocation is enforced, not merely parsed: the same server certificate and
/// trust anchor are accepted until the one differing variable names a CRL
/// that revokes that exact leaf certificate.
#[compio::test]
async fn sslcrl_refuses_the_server_certificate_it_revokes() {
    let s = servers();
    let base = format!("{} sslmode=verify-full sslrootcert={}", s.tls_url, s.ca);

    assert!(
        transport_of(&base)
            .await
            .expect("the server certificate is valid without a CRL"),
        "the no-CRL control was not encrypted"
    );

    let revoked = transport_of(&format!("{base} sslcrl={}", s.server_crl))
        .await
        .expect_err("the CRL must refuse the exact server certificate it revokes");
    assert!(
        describe(&revoked).to_ascii_lowercase().contains("revoked"),
        "the refusal must be certificate revocation, not a parse error: {}",
        describe(&revoked)
    );
}

/// The directory form enforces the same revocation policy as `sslcrl`: the
/// certificate and every other connection variable remain identical.
#[compio::test]
async fn sslcrldir_refuses_the_server_certificate_it_revokes() {
    let s = servers();
    let base = format!("{} sslmode=verify-full sslrootcert={}", s.tls_url, s.ca);

    assert!(
        transport_of(&base)
            .await
            .expect("the server certificate is valid without sslcrldir"),
        "the no-directory control was not encrypted"
    );

    let revoked = transport_of(&format!("{base} sslcrldir={}", s.server_crl_dir))
        .await
        .expect_err("the hashed CRL directory must refuse the certificate it revokes");
    assert!(
        describe(&revoked).to_ascii_lowercase().contains("revoked"),
        "the refusal must be certificate revocation, not directory loading: {}",
        describe(&revoked)
    );
}

/// OpenSSL selects a CRL by the issuer hash in its filename, not by scanning
/// every `.rN` file and inspecting its contents. The CRL does not revoke this
/// second certificate, so the correctly named directory accepts it while the
/// same CRL under the wrong hash leaves revocation status unknown and refuses.
#[compio::test]
async fn sslcrldir_requires_the_crl_issuer_hash_in_the_filename() {
    let s = servers();
    let base = format!(
        "{} sslmode=verify-ca sslrootcert={} sslcrldir=",
        s.mismatch_url, s.ca
    );

    assert!(
        transport_of(&format!("{base}{}", s.server_crl_dir))
            .await
            .expect("the correctly hashed non-revoking CRL must be usable"),
        "the correctly hashed control was not encrypted"
    );

    let wrong_hash = transport_of(&format!("{base}{}", s.server_crl_wrong_hash_dir))
        .await
        .expect_err("a CRL stored under the wrong issuer hash must not be loaded");
    let text = describe(&wrong_hash);
    assert!(
        text.contains("sslcrldir=") && text.contains("issuer hash"),
        "the refusal must identify the mis-hashed directory entry: {text}"
    );
}

/// `verify-ca` suppresses only host-name failures. This combines a deliberately
/// wrong TLS name with the revoked certificate so a future rustls reordering
/// cannot accidentally make that suppression swallow revocation too.
#[compio::test]
async fn verify_ca_still_enforces_sslcrl_with_a_wrong_host_name() {
    let s = servers();
    let endpoint = s.tls_url.replacen(
        "host=localhost",
        "host=revoked-name.invalid hostaddr=127.0.0.1",
        1,
    );
    let base = format!("{endpoint} sslmode=verify-ca sslrootcert={}", s.ca);

    assert!(
        transport_of(&base)
            .await
            .expect("verify-ca must ignore only the deliberately wrong host name"),
        "the no-CRL control was not encrypted"
    );

    let revoked = transport_of(&format!("{base} sslcrl={}", s.server_crl))
        .await
        .expect_err("verify-ca must not suppress a revocation failure");
    assert!(
        describe(&revoked).to_ascii_lowercase().contains("revoked"),
        "verify-ca suppressed more than the host-name error: {}",
        describe(&revoked)
    );
}

/// The control, differing in one variable: drop `sslcert`/`sslkey` and the same
/// URL against the same server must fail.
///
/// Without this, the test above would also pass against a server that never
/// asked for a certificate - which is exactly how "wired" and "parsed and
/// silently ignored" look the same.
#[compio::test]
async fn the_same_url_without_a_client_certificate_is_refused() {
    let s = servers();
    let url = format!(
        "{} sslmode=verify-full sslrootcert={}",
        s.clientcert_url, s.ca
    );
    transport_of(&url)
        .await
        .expect_err("a server using cert authentication must refuse an anonymous client");
}

/// Naming one half of the pair is a configuration error, not a connection that
/// quietly proceeds without client authentication.
#[compio::test]
async fn half_a_client_certificate_pair_is_rejected() {
    let s = servers();
    for (extra, missing) in [
        (format!("sslcert={}", s.client_cert), "sslkey"),
        (format!("sslkey={}", s.client_key), "sslcert"),
    ] {
        let url = format!(
            "{} sslmode=verify-full sslrootcert={} {extra}",
            s.tls_url, s.ca
        );
        let err = transport_of(&url).await.unwrap_err();
        let text = describe(&err);
        assert!(
            text.contains(missing),
            "the error must name the missing half ({missing}): {text}"
        );
    }
}

/// A `sslrootcert` that names a file which is not a certificate must be an
/// error about that file, raised before any socket is opened - not a handshake
/// failure blamed on the server.
#[compio::test]
async fn bad_sslrootcert_is_reported_as_a_configuration_error() {
    let s = servers();
    let err = transport_of(&format!(
        "{} sslmode=verify-full sslrootcert=/nonexistent/ca.crt",
        s.tls_url
    ))
    .await
    .expect_err("a sslrootcert path that does not exist must fail");
    let text = describe(&err);
    assert!(
        text.contains("sslrootcert=/nonexistent/ca.crt"),
        "the error must name the file: {text}"
    );
}

/// The plaintext path must be untouched by any of this: `NoTls` against the
/// plain server still works, with the driver's own `connect` entry point.
#[compio::test]
async fn notls_connect_still_works() {
    let s = servers();
    let (client, connection) = compio_postgres::connect(&s.plain_url, NoTls)
        .await
        .expect("NoTls connect");
    compio::runtime::spawn(async move {
        let _: Result<(), Error> = connection.run().await;
    })
    .detach();

    let (ssl, _) = server_reports_ssl(&client).await;
    assert!(!ssl);
}

/// The COPY-1/IO-1 deadlock, over TLS.
///
/// `copy_in_error_does_not_deadlock` in tests/integration.rs pins this, and it
/// connects with `NoTls`. A plaintext socket splits into owned halves and runs
/// the multiplexed loop, which reads the server's ErrorResponse while the
/// client is still streaming CopyData. TLS cannot split, so it runs the
/// serialized loop - the one the 2026-06-05 review named as the root cause of
/// COPY-1. The fix therefore may never have applied to this transport, and no
/// test in the crate would notice.
///
/// Same shape as the plaintext original: a first row the server rejects, then
/// enough bulk behind it that the client is still writing long after the
/// server stopped draining.
#[compio::test]
async fn copy_in_error_does_not_deadlock_over_tls() {
    use bytes::Bytes;
    use futures_util::SinkExt;
    use std::pin::pin;

    let s = servers();
    let dsn = format!("{} sslmode=require", s.tls_url);
    let config = dsn.parse::<Config>().expect("parse the require DSN");
    let make = MakeRustlsConnect::from_config(&config).expect("build the require connector");
    let (client, connection) = config.connect(make).await.expect("sslmode=require connect");
    let task = compio::runtime::spawn(async move {
        let _ = connection.run().await;
    });

    let (ssl, _version) = server_reports_ssl(&client).await;
    assert!(ssl, "this test is meaningless on an unencrypted session");

    client
        .execute("CREATE TEMPORARY TABLE cpg_tls_copy (id int, n int)", &[])
        .await
        .expect("create the copy target");

    let copy_fut = async {
        let sink = client
            .copy_in::<_, Bytes>("COPY cpg_tls_copy (id, n) FROM STDIN")
            .await?;
        let mut sink = pin!(sink);
        sink.feed(Bytes::from_static(b"1\tnotanint\n")).await?;
        for i in 0..200_000i64 {
            sink.feed(Bytes::from(format!("{i}\t{i}\n"))).await?;
        }
        sink.finish().await
    };

    let outcome = compio::time::timeout(std::time::Duration::from_secs(15), copy_fut).await;
    let verdict = match &outcome {
        Err(_) => "TIMED OUT".to_owned(),
        Ok(Ok(rows)) => format!("succeeded with {rows} rows"),
        Ok(Err(e)) => format!("errored: {:?}", e.code()),
    };
    println!("  [copy over tls] {verdict}");

    drop(client);
    let _ = task.await;

    match outcome {
        Err(_) => panic!(
            "copy_in deadlocked (15s) over TLS. TLS runs the MULTIPLEXED loop \
             since 2026-08-24, so this is no longer the serialized loop's COPY-1 \
             (never reading the server's ErrorResponse while CopyData streams); \
             the first thing to check is whether the transport has regressed to \
             the serialized loop, which `a_notification_reaches_an_idle_tls_\
             connection` would also catch. The plaintext regression test cannot \
             see either."
        ),
        Ok(Ok(rows)) => panic!("expected the COPY parse error, but it succeeded with {rows} rows"),
        Ok(Err(e)) => assert_eq!(
            e.code(),
            Some(&compio_postgres::error::SqlState::INVALID_TEXT_REPRESENTATION),
            "the COPY parse error lost its SQLSTATE over TLS: {}",
            describe(&e)
        ),
    }
}

/// A notification that arrives while the connection is IDLE must be delivered.
///
/// This is IO-2, and TLS is the only transport that reaches it in a normal
/// deployment: an unsplittable stream runs the serialized loop, which reads
/// only while a request is outstanding. Between requests nobody is reading the
/// socket, so `NOTIFY` from another session sits in the kernel buffer
/// indefinitely and `LISTEN` is silently useless over TLS.
///
/// The CONTROL is the second half: the same channel, the same two sessions,
/// except the listener issues a query afterwards. That makes the loop read, so
/// the notification arrives. Without it, a red first half is equally consistent
/// with "notifications never work over TLS", which would be a different bug.
#[compio::test]
async fn a_notification_reaches_an_idle_tls_connection() {
    use futures_util::StreamExt;

    let s = servers();
    let dsn = format!("{} sslmode=verify-full sslrootcert={}", s.tls_url, s.ca);
    let config = dsn.parse::<Config>().expect("parse the listener DSN");
    let make = MakeRustlsConnect::from_config(&config).expect("build the listener connector");

    let channel = format!("cpg_tls_idle_{}", std::process::id());

    let (listener, mut connection) = config
        .connect(make.clone())
        .await
        .expect("connect the TLS listener");
    let mut messages = connection.notifications();
    let task = compio::runtime::spawn(async move {
        let _ = connection.run().await;
    });
    listener
        .batch_execute(&format!("LISTEN {channel}"))
        .await
        .expect("LISTEN over TLS");

    // A second session, so the NOTIFY cannot travel on the listener's own
    // request/response exchange.
    let (notifier, notifier_connection) = config
        .connect(make.clone())
        .await
        .expect("connect the TLS notifier");
    let notifier_task = compio::runtime::spawn(async move {
        let _ = notifier_connection.run().await;
    });
    notifier
        .batch_execute(&format!("NOTIFY {channel}, 'while idle'"))
        .await
        .expect("NOTIFY over TLS");

    let idle_delivery = compio::time::timeout(std::time::Duration::from_secs(10), messages.next())
        .await
        .map(|message| match message {
            Some(compio_postgres::AsyncMessage::Notification(n)) => n.payload().to_owned(),
            other => panic!("expected a notification, got {other:?}"),
        });

    // The control: whatever happened above, a notification delivered while the
    // listener is ACTIVELY reading must arrive. If this half is also red the
    // failure is not about idleness.
    notifier
        .batch_execute(&format!("NOTIFY {channel}, 'while active'"))
        .await
        .expect("second NOTIFY over TLS");
    let active_delivery = compio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let pumped: i32 = listener
                .query_one_scalar("SELECT 1::int4", &[])
                .await
                .expect("pump the listener with a request");
            assert_eq!(pumped, 1);
            if let Ok(Some(message)) =
                compio::time::timeout(std::time::Duration::from_millis(200), messages.next()).await
            {
                match message {
                    compio_postgres::AsyncMessage::Notification(n) => {
                        return n.payload().to_owned();
                    }
                    other => panic!("expected a notification, got {other:?}"),
                }
            }
        }
    })
    .await;

    drop(listener);
    drop(notifier);
    let _ = task.await;
    let _ = notifier_task.await;

    assert!(
        active_delivery.is_ok(),
        "no notification arrived over TLS even while the connection was reading; \
         the idle claim below cannot be interpreted"
    );
    let idle = idle_delivery.expect(
        "a NOTIFY sent while the connection was idle was never delivered (10s). \
         That is IO-2, and it means TLS has regressed to the SERIALIZED loop, \
         which reads only while a request is outstanding - so LISTEN is \
         silently useless between queries. TLS took that loop until 2026-08-24 \
         because the adapter owning the socket could not hand it back; check \
         `RustlsStream`'s SplitStream impl first.",
    );
    assert_eq!(idle, "while idle");
}
