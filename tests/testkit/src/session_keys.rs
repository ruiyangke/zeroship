//! Session key files for in-process authentication servers.
use std::io::Write;

/// Session-secret key paths for the remaining shared auth-server fixtures.
///
/// The files carry fixed test material and are memoized for the process.
/// Native token-exchange tests exercise their configuration and contents.
///
/// # Panics
/// Panics if the directory or key files cannot be created or secured.
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
