//! The worker's startup refusal on missing, unreadable or forged service key
//! material, driven against the real binary.
//!
//! `zeroship-core`'s loader tests rule on `load_peer_bundle` and
//! `load_join_material`; none of them can rule on whether `main` CALLS them, or
//! on what an operator sees when it does. These spawn the shipped binary.
//!
//! NO ARM RESTS ON THE EXIT CODE. Every launch here also exits non-zero when the
//! fence holds, because the worker has a later refusal it cannot get past in
//! this environment - its database-posture check. So each arm rules on WHERE the
//! process stopped: a refusal arm requires the fence's own sentence AND the
//! absence of the later one, and the control requires the reverse. Logging the
//! fence message and carrying on is invisible to a message-only check, because
//! the later refusal supplies the non-zero exit.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use base64::Engine as _;
use zeroship_core::service_peers::{
    AUTH_SERVICE_NAME, CONTROL_SERVICE_NAME, GATEWAY_SERVICE_NAME, SERVICE_TRUST_DOMAIN,
};

/// The sentence a launch stopped at this fence prints.
const JOIN_TOKEN_REFUSAL: &str = "refusing to start - join token rejected";
const PEER_DOCUMENT_REFUSAL: &str = "refusing to start - peer document rejected";
const ENVELOPE_REFUSAL: &str = "must publish the gateway's public key";

/// The NEXT boot refusal, reached only once the process is past this fence.
const LATER_REFUSAL: &str = "refusing unsafe database authority";

/// A fixture directory that removes itself even when an assertion panics.
struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "zeroship-worker-peer-boot-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::create_dir_all(&path).expect("create fixture directory");
        Self(path)
    }

    fn write(&self, name: &str, contents: &str) -> PathBuf {
        let path = self.0.join(name);
        std::fs::write(&path, contents).expect("write fixture");
        path
    }

    /// The loaders refuse a group- or world-readable secret, so a fixture at 0644
    /// would fail every arm for the wrong reason.
    fn write_secret(&self, name: &str, contents: &str) -> PathBuf {
        let path = self.write(name, contents);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
                .expect("owner-only fixture secret");
        }
        path
    }

    fn absent(&self, name: &str) -> PathBuf {
        let path = self.0.join(name);
        let _ = std::fs::remove_file(&path);
        path
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A deterministic Ed25519 keypair. A fixed seed rather than a generated key
/// keeps the fixture reproducible and removes the "two keys came out identical"
/// guard a random fixture needs: different seeds cannot collide.
fn signing_key(seed: u8) -> ed25519_dalek::SigningKey {
    ed25519_dalek::SigningKey::from_bytes(&[seed; 32])
}

/// The RFC 7517 `x` member: the raw public half, base64url and unpadded.
fn public_x(key: &ed25519_dalek::SigningKey) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(key.verifying_key().to_bytes())
}

fn peer_entry(service: &str, x: &str) -> String {
    format!(
        r#"{{"kty":"OKP","crv":"Ed25519","iss":"spiffe://{SERVICE_TRUST_DOMAIN}/{service}","x":"{x}"}}"#
    )
}

fn document(entries: &[String]) -> String {
    format!(r#"{{"keys":[{}]}}"#, entries.join(","))
}

/// The worker's credential is a bearer JOIN TOKEN, not a key: the reader refuses
/// an unset, unreadable or wrongly-permissioned file but verifies no signature
/// itself, so the fixture only needs the SHAPE a JWT has.
fn join_token(fixture: &Fixture) -> PathBuf {
    fixture.write_secret("join-token", "REFUSEDHEADER.REFUSEDPAYLOAD.REFUSEDSIGNATURE")
}

fn worker(join: Option<&Path>, peers: Option<&Path>) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_zeroship-worker"));
    cmd.env_clear()
        .env("ZEROSHIP_CONTROL_KEY", "control-key-material")
        .args(["--port", "0"]);
    if let Some(join) = join {
        cmd.env("ZEROSHIP_WORKER_JOIN_TOKEN_FILE", join);
    }
    if let Some(peers) = peers {
        cmd.env("ZEROSHIP_WORKER_SERVICE_PEERS_FILE", peers);
    }
    cmd.output().expect("spawn zeroship-worker")
}

fn combined(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// A process that stopped AT the fence cannot have reached what follows it.
fn assert_stopped_at_the_fence(text: &str, label: &str) {
    assert!(
        !text.contains(LATER_REFUSAL),
        "{label} walked PAST the fence and stopped at the later refusal instead:\n{text}"
    );
}

#[test]
fn an_unset_join_token_is_refused_before_the_peer_document_is_opened() {
    let output = worker(None, None);
    let text = combined(&output);

    assert!(!output.status.success(), "the worker BOOTED with no key material");
    assert!(
        text.contains(JOIN_TOKEN_REFUSAL),
        "the unconfigured refusal does not say {JOIN_TOKEN_REFUSAL:?}:\n{text}"
    );
    // The join token is checked first, so this launch never reaches the peer
    // document and can only name the setting, not a file.
    assert!(
        text.contains("worker.join_token_file"),
        "the unconfigured refusal does not name the setting:\n{text}"
    );
    assert_stopped_at_the_fence(&text, "the unconfigured worker");
}

#[test]
fn an_absent_peer_document_is_refused_by_name() {
    let fixture = Fixture::new();
    let absent = fixture.absent("service-peers-absent.json");
    let output = worker(Some(&join_token(&fixture)), Some(&absent));
    let text = combined(&output);

    assert!(!output.status.success(), "the worker BOOTED on an absent document");
    assert!(
        text.contains(PEER_DOCUMENT_REFUSAL),
        "the refusal does not say {PEER_DOCUMENT_REFUSAL:?}:\n{text}"
    );
    // An operator reading a boot log needs the file.
    assert!(
        text.contains(absent.to_str().expect("UTF-8 path")),
        "the refusal does not name the absent file:\n{text}"
    );
    assert_stopped_at_the_fence(&text, "the worker with an absent document");
}

#[test]
fn an_unparseable_peer_document_is_refused_by_name() {
    let fixture = Fixture::new();
    let malformed = fixture.write("service-peers-malformed.json", "{ this is not a JWKS document");
    let output = worker(Some(&join_token(&fixture)), Some(&malformed));
    let text = combined(&output);

    assert!(!output.status.success(), "the worker BOOTED on an unparseable document");
    assert!(
        text.contains(PEER_DOCUMENT_REFUSAL),
        "the refusal does not say {PEER_DOCUMENT_REFUSAL:?}:\n{text}"
    );
    assert!(
        text.contains(malformed.to_str().expect("UTF-8 path")),
        "the refusal does not name the unparseable file:\n{text}"
    );
    assert_stopped_at_the_fence(&text, "the worker with an unparseable document");
}

/// F4's own sentence: the material the worker verifies `ZeroShip-User` under is
/// absent while everything else is present, and it must still refuse.
#[test]
fn a_document_that_omits_the_gateway_key_is_refused() {
    let fixture = Fixture::new();
    let control = public_x(&signing_key(0x11));
    let without_gateway = fixture.write(
        "service-peers-no-gateway.json",
        &document(&[peer_entry(CONTROL_SERVICE_NAME, &control)]),
    );
    let output = worker(Some(&join_token(&fixture)), Some(&without_gateway));
    let text = combined(&output);

    assert!(
        !output.status.success(),
        "the worker BOOTED without the gateway public key"
    );
    assert!(
        text.contains(ENVELOPE_REFUSAL),
        "the refusal does not say {ENVELOPE_REFUSAL:?}:\n{text}"
    );
    assert_stopped_at_the_fence(&text, "the worker without the gateway key");
}

/// ONE KEY UNDER TWO ISSUERS. Every per-entry check passes - each key is valid,
/// each issuer is well formed, each id matches its thumbprint - so what is wrong
/// is a property of the SET, which no per-entry check can see. The two services
/// sharing the key are neither of the launched binary's own halves, so the
/// refusal is a property of the document rather than of "my own key turned up
/// somewhere".
#[test]
fn a_document_where_two_issuers_share_one_key_is_refused() {
    let fixture = Fixture::new();
    let gateway = public_x(&signing_key(0x22));
    let shared = public_x(&signing_key(0x33));
    let forged = fixture.write(
        "service-peers-third-parties-share.json",
        &document(&[
            peer_entry(GATEWAY_SERVICE_NAME, &gateway),
            peer_entry(CONTROL_SERVICE_NAME, &shared),
            peer_entry(AUTH_SERVICE_NAME, &shared),
        ]),
    );
    let output = worker(Some(&join_token(&fixture)), Some(&forged));
    let text = combined(&output);

    assert!(
        !output.status.success(),
        "the worker BOOTED on a document where two issuers share one key"
    );
    assert!(
        text.contains(PEER_DOCUMENT_REFUSAL),
        "the refusal does not say {PEER_DOCUMENT_REFUSAL:?}:\n{text}"
    );
    assert_stopped_at_the_fence(&text, "the worker on a shared-key document");
}

/// The one-variable control: only the document changes from the arm above. The
/// process must walk PAST this fence and stop at the NEXT one, which is the
/// database-posture check. That is positive evidence it passed, which an exit
/// code cannot express - and it is what forbids a binary that refuses
/// everything from printing what this file prints.
#[test]
fn valid_key_material_walks_past_the_fence() {
    let fixture = Fixture::new();
    let control = public_x(&signing_key(0x11));
    let gateway = public_x(&signing_key(0x22));
    let peers = fixture.write(
        "service-peers.json",
        &document(&[
            peer_entry(CONTROL_SERVICE_NAME, &control),
            peer_entry(GATEWAY_SERVICE_NAME, &gateway),
        ]),
    );
    let output = worker(Some(&join_token(&fixture)), Some(&peers));
    let text = combined(&output);

    assert!(
        !text.contains(JOIN_TOKEN_REFUSAL),
        "the worker refused a VALID-shaped join token:\n{text}"
    );
    assert!(
        !text.contains(PEER_DOCUMENT_REFUSAL),
        "the worker refused VALID key material:\n{text}"
    );
    assert!(
        text.contains(LATER_REFUSAL),
        "the worker did not reach the database check, so it never passed the fence:\n{text}"
    );
}