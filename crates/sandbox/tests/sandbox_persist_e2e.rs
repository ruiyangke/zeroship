//! Phase-0 sealed-record persistence e2e (preview-URL § II.0 §4).
//!
//! Real Nomad / Cloud Hypervisor isn't available in CI, so this
//! test drives the persist + restore surface against a fixture
//! HTTP agent and verifies the controller-side observable behaviour
//! across a "controller restart":
//!
//! 1. Mint an Ed25519 keypair, seal a `SandboxAuth` record under
//!    `<persist_dir>/sealed-records/`.
//! 2. Construct the controller's `Backend` + `SandboxRegistry`.
//! 3. Run `restore_at_startup` against the sealed dir, with the
//!    fixture agent answering `/version` in three modes:
//!    - matching pubkey_fingerprint → outcome `Restored` (when the
//!      backend rehydrates) or `BackendUnsupported`;
//!    - mismatched fingerprint → outcome `Mismatched` + sealed file
//!      DELETED;
//!    - unreachable → outcome `Unreachable` + sealed file KEPT.
//! 4. Confirm the registry reflects only the records that
//!    successfully rehydrated.
//!
//! The test also covers the path-traversal hardening invariant by
//! confirming a record sealed under a normal UUID lives at
//! `<dir>/<32-hex>.sealed` — never at a sibling escape.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use ed25519_dalek::SigningKey;
use uuid::Uuid;
use zeroship_sandbox::backend::Backend;
use zeroship_sandbox::config::{ApiToken, K8sConfig, NomadCHConfig, SandboxConfig};
use zeroship_sandbox::persist::{seal, AeadKey, SealedAuth, SEAL_VERSION};
use zeroship_sandbox::registry::SandboxRegistry;
use zeroship_sandbox::restore::{
    self, RestoreOutcome, _test_process_one, DEFAULT_PROBE_TIMEOUT,
};
use zeroship_sandbox_agent::sig;

/// Single-shot HTTP/1.1 fixture. Returns the bound port + a stop
/// flag the test sets to wind the listener down. Body + status are
/// caller-supplied; one signed-`/version` request per test is the
/// expected shape.
fn spawn_mock_agent(body: String, status: u16) -> (u16, Arc<AtomicBool>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let stop = Arc::new(AtomicBool::new(false));
    let stop_clone = stop.clone();
    listener.set_nonblocking(true).unwrap();
    thread::spawn(move || {
        while !stop_clone.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    stream
                        .set_read_timeout(Some(Duration::from_millis(50)))
                        .ok();
                    let mut buf = [0u8; 4096];
                    let _ = stream.read(&mut buf);
                    let resp = format!(
                        "HTTP/1.1 {status} OK\r\n\
                         Content-Type: application/json\r\n\
                         Content-Length: {}\r\n\
                         Connection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = stream.write_all(resp.as_bytes());
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(5));
                }
                Err(_) => break,
            }
        }
    });
    (port, stop)
}

fn make_cfg(backend: &str) -> SandboxConfig {
    SandboxConfig {
        port: 9091,
        token: ApiToken::new("x"),
        backend: backend.into(),
        image: "img".into(),
        workspace_root: std::path::PathBuf::from("/var/zeroship/projects"),
        network: "n".into(),
        memory_mb: 1024,
        cpus: 2.0,
        idle_timeout_secs: 1800,
        max_lifetime_secs: 28800,
        auto_pull: false,
        k8s: K8sConfig {
            namespace: "default".into(),
            image: "i".into(),
            runtime_class: "kvm-sandbox".into(),
            ready_timeout_secs: 120,
            use_port_forward: false,
            port_forward_start: 18000,
            user_home_size: "5Gi".into(),
            user_home_storage_class: None,
            startup_orphan_cleanup: false,
        },
        nomad_ch: NomadCHConfig {
            nomad_addr: "http://127.0.0.1:4646".into(),
            datacenter: "dc1".into(),
            wrapper_path: std::path::PathBuf::from("/etc/zeroship/nomad-vm-wrapper.sh"),
            runtime_dir: std::path::PathBuf::from("/var/lib/zeroship/ch"),
            host_state_dir: std::path::PathBuf::from("/var/zeroship/ch"),
            user_home_dir_root: std::path::PathBuf::from("/var/zeroship/ch/users"),
            vm_index_floor: 1,
            vm_index_ceil: 200,
            alloc_running_timeout_secs: 60,
            agent_livez_timeout_secs: 30,
            host_fence_timeout_secs: 30,
            startup_orphan_cleanup: false,
            subnet_second_octet: 99,
        },
        create_retry_max: 2,
        create_retry_total_timeout_secs: 90,
    }
}

fn fresh_dir(label: &str) -> std::path::PathBuf {
    let p = std::env::temp_dir()
        .join(format!("zsbx-persist-e2e-{label}-{}", Uuid::now_v7().simple()));
    std::fs::create_dir_all(&p).unwrap();
    p
}

#[compio::test]
async fn restart_with_unreachable_agent_keeps_sealed_records() {
    // 1. Pretend a previous controller minted a sandbox + sealed
    //    its keys to disk. The "VM" is not actually running in
    //    this test environment — derive_agent_url points at a
    //    10.99/16 IP that has no listener on the test host.
    let dir = fresh_dir("unreachable");
    let sealed_dir = dir.join("sealed-records");
    std::fs::create_dir_all(&sealed_dir).unwrap();
    let aead_key = AeadKey::from_bytes([0x42; 32]);

    let id = Uuid::now_v7();
    let sk_bytes = [0xab; 32];
    let sk = SigningKey::from_bytes(&sk_bytes);
    let fp = sig::pubkey_fingerprint(&sk.verifying_key());
    let sealed = SealedAuth {
        version: SEAL_VERSION,
        sandbox_id: id.to_string(),
        user_id: "alice".into(),
        project_id: "p1".into(),
        backend: "nomad-ch".into(),
        signing_key_bytes: sk_bytes,
        vm_index: Some(7),
        agent_url: None,
        pubkey_fp: fp,
        created_at_secs: 1_700_000_000,
    };
    let sealed_path = seal(id, &sealed, &sealed_dir, &aead_key).unwrap();
    assert!(sealed_path.starts_with(&sealed_dir));
    let stem = sealed_path.file_stem().unwrap().to_str().unwrap();
    // Filename invariant (round-6 CRITICAL-3 path-traversal): 32
    // hex chars, all lowercase [0-9a-f].
    assert_eq!(stem.len(), 32);
    assert!(stem.chars().all(|c| c.is_ascii_hexdigit()));

    // 2. Stand up a fresh controller backend (the previous one
    //    "crashed" — only the sealed record survived). Run the
    //    restore loop with a tight probe timeout; the 10.99/16
    //    address has no listener so the probe fails-fast as
    //    Unreachable. The sealed record is left on disk.
    let backend = Backend::NomadCh(
        zeroship_sandbox::backend::nomad_ch::NomadCHBackend::new(make_cfg("nomad-ch"))
            .unwrap(),
    );
    let registry = SandboxRegistry::new();
    let summary = restore::restore_at_startup(
        &dir,
        &aead_key,
        &backend,
        &registry,
        Duration::from_millis(150),
    )
    .await
    .unwrap();
    assert_eq!(summary.records_seen, 1);
    assert_eq!(summary.unreachable, 1);
    assert_eq!(summary.restored, 0);
    assert_eq!(summary.mismatched, 0);
    assert!(sealed_path.exists(), "unreachable must keep sealed file");
    assert!(registry.list().is_empty(), "no rehydrate on unreachable");

    let _ = std::fs::remove_dir_all(&dir);
}

#[compio::test]
async fn restart_with_mismatched_agent_deletes_sealed_record_and_records_outcome() {
    // The previous tenant of the agent's IP+port has a different
    // controller pubkey — `/version` returns a fingerprint that
    // doesn't match what we sealed. The boot path deletes the
    // sealed file (the agent at this address is no longer ours).
    let dir = fresh_dir("mismatch");
    let sealed_dir = dir.join("sealed-records");
    std::fs::create_dir_all(&sealed_dir).unwrap();
    let aead_key = AeadKey::from_bytes([0x77; 32]);

    let stranger_sk = SigningKey::from_bytes(&[0x99; 32]);
    let stranger_fp = sig::pubkey_fingerprint(&stranger_sk.verifying_key());
    let body =
        format!(r#"{{"agent_version":"x","pubkey_fingerprint":"{stranger_fp}"}}"#);
    let (port, stop) = spawn_mock_agent(body, 200);
    let agent_url = format!("http://127.0.0.1:{port}");

    let id = Uuid::now_v7();
    let sk_bytes = [0xab; 32];
    let sk = SigningKey::from_bytes(&sk_bytes);
    let our_fp = sig::pubkey_fingerprint(&sk.verifying_key());
    // Use backend = "k8s" so `agent_url` is honored as the sealed
    // value (k8s does NOT derive). The k8s rehydrate path errors
    // ("Phase-1 follow-up"), but we never get there because the
    // probe mismatches first.
    let sealed = SealedAuth {
        version: SEAL_VERSION,
        sandbox_id: id.to_string(),
        user_id: "alice".into(),
        project_id: "p1".into(),
        backend: "k8s".into(),
        signing_key_bytes: sk_bytes,
        vm_index: None,
        agent_url: Some(agent_url),
        pubkey_fp: our_fp,
        created_at_secs: 1_700_000_000,
    };
    let sealed_path = seal(id, &sealed, &sealed_dir, &aead_key).unwrap();
    assert!(sealed_path.exists());

    let backend = Backend::K8s(
        zeroship_sandbox::backend::k8s::K8sBackend::new(make_cfg("k8s")).unwrap(),
    );
    let registry = SandboxRegistry::new();

    let outcome =
        _test_process_one(&sealed_path, sealed, &backend, &registry, Duration::from_millis(500))
            .await;
    stop.store(true, Ordering::Relaxed);
    assert_eq!(outcome, RestoreOutcome::Mismatched);
    assert!(!sealed_path.exists(), "mismatched record must be deleted");
    assert!(registry.list().is_empty());

    let _ = std::fs::remove_dir_all(&dir);
}

#[compio::test]
async fn restart_with_matching_agent_rehydrates_for_supported_backend() {
    // The probe matches; for nomad-ch the rehydrate path runs and
    // re-installs state. We bypass the boot's `derive_agent_url`
    // (which targets 10.99/16) by driving the rehydrate function
    // directly — the agent-URL derivation is exercised by the
    // unit tests in `restore.rs`. This test confirms the mid-boot
    // sequence after a successful probe lands the registry entry.
    let backend =
        zeroship_sandbox::backend::nomad_ch::NomadCHBackend::new(make_cfg("nomad-ch"))
            .unwrap();
    let id = Uuid::now_v7();
    let sk_bytes = [0xc1; 32];
    let sk = SigningKey::from_bytes(&sk_bytes);
    let fp = sig::pubkey_fingerprint(&sk.verifying_key());
    let sealed = SealedAuth {
        version: SEAL_VERSION,
        sandbox_id: id.to_string(),
        user_id: "alice".into(),
        project_id: "p1".into(),
        backend: "nomad-ch".into(),
        signing_key_bytes: sk_bytes,
        vm_index: Some(13),
        agent_url: None,
        pubkey_fp: fp.clone(),
        created_at_secs: 1_700_000_000,
    };
    let auth = backend
        .restore_from_sealed(id, &sealed)
        .await
        .expect("rehydrate");
    assert_eq!(auth.agent_url, "http://10.99.113.2:7777");
    assert_eq!(auth.pubkey_fp, fp);

    // After rehydrate: a fresh `session_auth` returns the same data.
    let again = backend.session_auth(id).await.expect("session_auth");
    assert_eq!(again.pubkey_fp, fp);
    assert_eq!(again.agent_url, "http://10.99.113.2:7777");

    // And the underlying file/exec lookup methods would now find
    // the sandbox by id — covered indirectly by the registry round
    // trip in unit tests.
}

/// Path-traversal regression: even an attacker-controlled
/// `sandbox_id` cannot reach a sibling file. The typed `Uuid` API
/// makes this unreachable at compile time; the string-form helper
/// rejects non-UUID input. Belt-and-suspenders e2e test.
#[test]
fn sealed_filename_for_evil_string_is_refused() {
    use zeroship_sandbox::persist::seal_filename_for_str;
    let err = seal_filename_for_str("../../etc/passwd").expect_err("must reject");
    assert!(err.contains("not a valid UUID"));
}

/// Boot path with no records returns a zero-summary cleanly (and
/// doesn't blow up on a missing `sealed-records` subdir — the
/// directory is created on demand by the first `seal`).
#[compio::test]
async fn empty_persist_dir_is_zero_records() {
    let dir = fresh_dir("empty");
    let aead_key = AeadKey::from_bytes([0u8; 32]);
    let backend = Backend::NomadCh(
        zeroship_sandbox::backend::nomad_ch::NomadCHBackend::new(make_cfg("nomad-ch"))
            .unwrap(),
    );
    let registry = SandboxRegistry::new();
    let s = restore::restore_at_startup(&dir, &aead_key, &backend, &registry, DEFAULT_PROBE_TIMEOUT)
        .await
        .unwrap();
    assert_eq!(s.records_seen, 0);
    assert!(registry.list().is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}
