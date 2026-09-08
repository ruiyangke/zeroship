//! Test-only helpers shared across the workspace.
//!
//! Consumed as a `dev-dependency`; never linked into a shipping binary.

use std::io::Write;

/// The token every skip announcement carries, and the only thing a gate has to
/// look for.
///
/// It is deliberately not a word. `tests/run_auth_suite.sh` used to search the
/// run log for "skipping", which missed the 13 lines reading
/// `[anchors] skip <name> (no GATEWAY_ANCHORS_DB_URL)` and reported "0 skipped"
/// with them sitting in its own log. Widening that search to "skip" does not
/// work either, and the reason is measured: of the 98 lines containing "skip"
/// in one full run, 13 were real announcements, 5 were the harness's own
/// `test <name> ... ok` lines for tests whose names contain "skips"/"skipped"
/// (`pg_or_skip`, `dunning_tick_skips_when_advisory_lock_held`,
/// `persisted_skip_consent`), and the remaining ~80 were driver debug output
/// echoing `INSERT INTO zeroship.oauth_clients (..., skip_consent, ...)`. The
/// word occurs incidentally in identifiers, in SQL, and in log noise, so it
/// cannot discriminate.
///
/// The hyphens are what make this token safe: they are not legal in a Rust
/// identifier, so no test name can ever produce it, and nothing in the schema
/// or the SQL the drivers log is spelled this way. A repo-wide search found
/// zero occurrences before it was introduced here.
pub const SKIP_MARKER: &str = "ZEROSHIP-TEST-SKIPPED";

/// Announce that a test did nothing because the backend it needs is absent.
///
/// `reason` is carried through verbatim after the marker and should name what
/// was missing, usually the environment variable that was not set. A gate can
/// count the marker; a human reading the log still needs to know which knob to
/// turn.
///
/// The channel is a direct handle write and `println!`/`eprintln!` would not
/// do. The harness captures output by swapping the thread-local target those
/// macros write through, and replays that buffer only for a FAILING test, so an
/// announcement made through a macro is invisible on a pass, which is exactly
/// the run where it matters. A write straight to the `Stderr` handle never
/// enters the buffer. Measured in one passing test with no `--nocapture`:
/// `println!` and `eprintln!` vanished, `stderr().write_all` printed.
///
/// The write is best-effort. A test that cannot reach stderr is not a test
/// worth failing over, and a panicking announcer would turn a skip into a
/// failure with a misleading cause.
///
/// THIS ANNOUNCES; IT DOES NOT DECIDE. There used to be a
/// `ZEROSHIP_REQUIRE_LIVE_BACKENDS=1` arm here that turned every announcement
/// into a panic, so whether an absent backend was fatal depended on an
/// environment variable the failing developer had not set. Postgres and Redis
/// are not optional for this workspace's tests, so the decision moved to the
/// two places that can state it precisely and cannot be forgotten:
///
///   - the call site, for a backend the test cannot do without. It panics with
///     the address it dialled and the command that provisions it, rather than
///     calling this function at all. `libs/compio-postgres/tests/suite/integration.rs`
///     and `libs/compio-redis/tests/integration.rs` are the worked examples.
///   - the suite gate, for everything else. `tests/lib/skip_census.sh` counts
///     these markers, and `tests/run_auth_suite.sh` /
///     `tests/run_billing_suite.sh` FAIL on any that is not named in an
///     allowlist with a reason.
///
/// What is left for this function is the genuinely optional: a MinIO container
/// on a machine with no docker, `pg_dump` off PATH, a pgvector extension that
/// is not installed. Announcing those is right and failing on them is not.
pub fn skip(reason: &str) {
    let _ = std::io::stderr().write_all(format!("{SKIP_MARKER}: {reason}\n").as_bytes());
}

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
