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

use compio_postgres::{Client, Error, NoTls, Pool};

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
    assert!(text.contains("channel binding"), "unexpected error: {text}");
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
