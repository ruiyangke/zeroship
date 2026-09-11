//! Session key files for in-process authentication servers.
use std::io::Write;

/// The session-secret keyring every in-process auth-server fixture must
/// configure, as an owner-only file pair on disk.
///
/// Returns `(refresh_hash_key_file, refresh_idem_key_file)`, the two paths
/// `AuthConfig::settings` names. `main.rs` refuses to boot without them, and
/// every token exchange establishes a session whose secret is hashed under this
/// keyring - so a fixture that omits them is less configured than any real
/// deployment and answers the exchange with a 500 rather than a token.
///
/// IT LIVES HERE BECAUSE IT ALREADY DRIFTED ONCE. The auth crate's fixture and
/// the control crate's `PlatformOp` each build an `AuthConfig` by hand; the
/// keyring was added to the first and not the second, and the control-plane
/// device-grant test - the one end-to-end proof that a `zeroship login` token
/// authorizes a control endpoint - was red on `refresh hash key is not
/// configured` until both fixtures were pointed at this one function. A second
/// copy is how the gap opened, so new fixtures call this rather than restating
/// the two writes.
///
/// THAT IS NOW ENFORCED RATHER THAN ASKED FOR. `tests/session_keyring_fixture_gate.sh`
/// refuses a test file that mounts the auth router and drives a token exchange
/// without reaching a keyring, and separately proves that every
/// `test_auth_config` fixture builder reaches THIS function - which is what
/// lets the gate accept a call to one of those builders as evidence.
///
/// Memoised: `SessionSecretKeys::from_files` reads the files on every exchange,
/// and a per-call temp directory would leave one behind per token request. The
/// directory name carries the process id and a clock reading, so two live
/// processes never share a pair and a reused pid never inherits one.
///
/// The material is fixed test material and deliberately not secret: the pair is
/// only ever consumed by a fixture-booted server in the same process.
///
/// # Panics
/// If the temp directory or either file cannot be created, written or chmodded.
/// A fixture that cannot write its own keyring has nothing to measure, and the
/// alternative - returning paths that do not exist - reappears downstream as
/// the very 500 this function exists to prevent.
#[must_use]
pub fn session_key_files() -> (std::path::PathBuf, std::path::PathBuf) {
    static FILES: std::sync::OnceLock<(std::path::PathBuf, std::path::PathBuf)> =
        std::sync::OnceLock::new();
    FILES
        .get_or_init(|| {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos());
            let dir = std::env::temp_dir().join(format!(
                "zs-fixture-session-keys-{}-{nanos}",
                std::process::id(),
            ));
            std::fs::create_dir_all(&dir).expect("fixture key dir");
            let hash_path = dir.join("refresh-hmac.keys");
            let idem_path = dir.join("refresh-idem.key");
            for (path, body) in [
                (
                    &hash_path,
                    "1:000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f\n"
                        .as_bytes(),
                ),
                (&idem_path, "fixture-idempotency-master-secret".as_bytes()),
            ] {
                let mut file = std::fs::File::create(path).expect("create fixture key file");
                file.write_all(body).expect("write fixture key file");
                drop(file);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt as _;
                    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
                        .expect("chmod fixture key file");
                }
            }
            (hash_path, idem_path)
        })
        .clone()
}
