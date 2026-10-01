//! `transport::tls` builds every connector from a named provider, never from
//! the rustls process default.
//!
//! rustls hands a process-level default to any caller that names no provider.
//! When nothing installed one it infers it from its crate features, and a build
//! that unifies `ring` beside `aws-lc-rs` leaves it nothing to infer, so the
//! first such caller panics. A connector that leans on that default works or
//! panics according to how its host binary happened to resolve features.
//!
//! This binary makes leaning on the default fail in every build. Before any
//! connector exists it installs a default with no cipher suites, no key
//! exchange groups and no signature verification algorithms: no config can be
//! built from it, and a verifier that borrowed its algorithms from it could
//! verify nothing. A connector that completes a handshake here did not consult
//! it.
//!
//! The default is process state and can be installed once, so this file is its
//! own test binary.

use std::sync::{Arc, Once};

use compio::io::{AsyncReadExt, AsyncWrite, AsyncWriteExt};
use compio::net::{TcpListener, TcpStream};
use compio_tls::TlsAcceptor;
use rcgen::{
    BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
    KeyUsagePurpose,
};
use rustls::crypto::{CryptoProvider, WebPkiSupportedAlgorithms};
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use zeroship_runtime::transport::tls::{TlsConnectorOptions, build_tls_connector};

const SERVER_NAME: &str = "db.local.test";

fn install_unusable_process_default() {
    static INSTALL: Once = Once::new();
    INSTALL.call_once(|| {
        let unusable = CryptoProvider {
            cipher_suites: Vec::new(),
            kx_groups: Vec::new(),
            signature_verification_algorithms: WebPkiSupportedAlgorithms {
                all: &[],
                mapping: &[],
            },
            ..rustls::crypto::aws_lc_rs::default_provider()
        };
        assert!(
            unusable.install_default().is_ok(),
            "a rustls process default was installed before this binary installed its own"
        );
    });
}

struct Server {
    ca_pem: String,
    acceptor: TlsAcceptor,
}

/// A CA and a leaf for [`SERVER_NAME`] it signed, served from a config built
/// over an explicit provider - the server side must not consult the default
/// either, or the handshake fails for a reason this file is not about.
fn ca_signed_server() -> Server {
    let mut ca_params = CertificateParams::new(Vec::new()).unwrap();
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca_params
        .distinguished_name
        .push(DnType::CommonName, "zeroship tls provider test root");
    ca_params.key_usages.push(KeyUsagePurpose::DigitalSignature);
    ca_params.key_usages.push(KeyUsagePurpose::KeyCertSign);
    ca_params.key_usages.push(KeyUsagePurpose::CrlSign);
    let ca_key = KeyPair::generate().unwrap();
    let ca_cert = ca_params.self_signed(&ca_key).unwrap();
    let issuer = Issuer::new(ca_params, ca_key);

    let leaf_key = KeyPair::generate().unwrap();
    let mut leaf_params = CertificateParams::new(vec![SERVER_NAME.to_string()]).unwrap();
    leaf_params
        .distinguished_name
        .push(DnType::CommonName, SERVER_NAME);
    leaf_params
        .extended_key_usages
        .push(ExtendedKeyUsagePurpose::ServerAuth);
    leaf_params
        .key_usages
        .push(KeyUsagePurpose::DigitalSignature);
    leaf_params.use_authority_key_identifier_extension = true;
    let leaf_cert = leaf_params.signed_by(&leaf_key, &issuer).unwrap();

    let config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::aws_lc_rs::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(
        vec![CertificateDer::from(leaf_cert.der().to_vec())],
        PrivateKeyDer::Pkcs8(leaf_key.serialize_der().into()),
    )
    .unwrap();
    Server {
        ca_pem: ca_cert.pem(),
        acceptor: TlsAcceptor::from(Arc::new(config)),
    }
}

/// Each `build_tls_connector` branch assembles its config separately, so each
/// is driven through a real handshake and an echo over the encrypted channel:
/// verification off, a pinned CA with hostname verification, and a pinned CA
/// checked for chain only.
#[compio::test]
async fn every_connector_shape_handshakes_without_the_process_default() {
    install_unusable_process_default();
    // The control. Without it a passing handshake could mean the default was
    // never unusable: a builder that consults it must fail in this process.
    assert!(
        std::panic::catch_unwind(rustls::ClientConfig::builder).is_err(),
        "the installed process default still builds a client config"
    );

    let fixture = ca_signed_server();
    let shapes = [
        (
            "verification disabled",
            TlsConnectorOptions {
                reject_unauthorized: false,
                ca_pem: None,
                verify_identity: true,
            },
        ),
        (
            "pinned CA with hostname verification",
            TlsConnectorOptions {
                reject_unauthorized: true,
                ca_pem: Some(fixture.ca_pem.clone()),
                verify_identity: true,
            },
        ),
        (
            "pinned CA, chain only",
            TlsConnectorOptions {
                reject_unauthorized: true,
                ca_pem: Some(fixture.ca_pem.clone()),
                verify_identity: false,
            },
        ),
    ];
    for (shape, options) in &shapes {
        let connector = build_tls_connector(options).expect("connector builds");
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = async {
            let (stream, _) = listener.accept().await.unwrap();
            let mut tls = fixture
                .acceptor
                .accept(stream)
                .await
                .expect("server handshake");
            let compio::BufResult(read, received) = tls.read_exact(vec![0u8; 4]).await;
            read.unwrap();
            tls.write_all(received).await.0.unwrap();
            tls.flush().await.unwrap();
        };
        let client = async {
            let stream = TcpStream::connect(address).await.unwrap();
            let mut tls = connector
                .connect(SERVER_NAME, stream)
                .await
                .expect("client handshake");
            tls.write_all(b"ping".to_vec()).await.0.unwrap();
            tls.flush().await.unwrap();
            let compio::BufResult(read, echoed) = tls.read_exact(vec![0u8; 4]).await;
            read.unwrap();
            echoed
        };
        let ((), echoed) = futures::join!(server, client);
        assert_eq!(echoed, b"ping", "{shape}");
    }
}
