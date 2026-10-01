//! The single-host join-token minter: Control minting for its own zone.
//!
//! A multi-host deployment mints join tokens where the operator is, with
//! `zeroship join-token`, per provisioning. A single-host deployment has nobody
//! there when a container restarts at three in the morning, so Control mints
//! instead: a short-lived, zone-scoped, use-capped token written to a path the
//! worker containers share, replaced before it expires. A worker reads the
//! CURRENT token at boot, so a container restarted days after provisioning gets
//! a token minted minutes ago.
//!
//! # What the shared file is, said plainly
//!
//! A bearer artifact. Whoever can read that volume can join a worker in that
//! zone, for the TTL, up to the uses that remain. Rotation bounds the window and
//! the use cap bounds the blast radius, but the trust boundary is "whatever can
//! read the volume", which is not the same boundary as "the worker". The
//! stronger anchor is for Control to read the peer's credentials off a Unix
//! domain socket and mint nothing at all; that removes the artifact rather than
//! shortening its life, and it is the named follow-up rather than this module.
//!
//! # Three quantities, related rather than independent
//!
//! [`ROTATION_INTERVAL`] is how often the file is replaced. [`TOKEN_TTL`] is how
//! long a minted token stays valid. [`BOOT_MARGIN`] is the longest a worker may
//! take between reading the file and presenting what it read. The TTL must
//! exceed the rotation interval by at least the boot margin, or a worker that
//! read the file an instant before a rotation presents an expired token. They
//! are constants with one test binding the relationship, not settings an
//! operator can put out of order.
//!
//! Rotation OVERLAPS by construction: a JWT already minted stays valid until its
//! own `exp`, so the token a worker read before the file changed is still live
//! for at least `TOKEN_TTL - ROTATION_INTERVAL`. That difference is exactly what
//! the boot margin has to fit inside.
//!
//! # One minter, elected
//!
//! Deployments run several Control replicas against one database, and two of
//! them rotating the same volume would write over each other. The writer is
//! elected with a `PostgreSQL` session advisory lock on [`MINTER_LOCK_KEY`]: the
//! holder rotates, the others stand by and write nothing at all. The lease ends
//! with the session, so a holder that dies drops it and the next tick elects a
//! successor. `pg_try_advisory_lock` is re-entrant within a session - a holder
//! re-asking gets `true` and increments a counter rather than taking a second
//! lock - so asking on every tick is how a replica notices it has become the
//! minter without any separate bookkeeping.

use std::path::{Path, PathBuf};
use std::time::Duration;

use zeroship_core::service_assertion::ServiceSigningKey;
use zeroship_core::worker_join::{mint_join_token, JoinTokenGrant};

/// How often the minter replaces the token file.
pub const ROTATION_INTERVAL: Duration = Duration::from_secs(300);

/// How long a minted token stays valid.
pub const TOKEN_TTL: Duration = Duration::from_secs(1800);

/// The longest a worker may take between reading the file and presenting it.
///
/// Generous on purpose: it covers a container that reads its token early in a
/// boot which then waits on a database, a migration or an image pull.
pub const BOOT_MARGIN: Duration = Duration::from_secs(600);

/// How many workers one minted token may admit before the next rotation.
///
/// A cap, not a fleet size: it bounds what a captured token is worth for the
/// minutes it lives. A deployment that scales past it within one rotation
/// interval gets the next token, which is minutes away.
pub const TOKEN_USES: u32 = 64;

/// The advisory-lock key the minter is elected on.
///
/// An arbitrary but FIXED 64-bit value. It must never collide with another
/// advisory lock this platform takes; nothing else in the control plane takes a
/// session-level one today, and the migration engine's project lock is derived
/// from a project id rather than chosen.
pub const MINTER_LOCK_KEY: i64 = 0x7a65_726f_6a6f_696e;

/// Everything the minter needs, or nothing when this deployment does not mint.
#[allow(missing_debug_implementations)]
pub struct MinterConfig {
    /// The signer credential Control mints with. Its public half must also be
    /// in the trusted-signer import, or Control would refuse its own tokens.
    pub signer_id: String,
    /// The signing key.
    pub key: ServiceSigningKey,
    /// The execution zone the minted tokens admit into.
    pub zone: String,
    /// Where the token is written.
    pub path: PathBuf,
}

/// Why one rotation did not happen.
#[derive(Debug, Eq, PartialEq)]
pub enum RotationOutcome {
    /// The token was minted and written.
    Rotated,
    /// Another replica holds the minter lease. Nothing was written.
    NotTheMinter,
}

/// Write `token` to `path` atomically, owner-readable only.
///
/// Atomic because a worker may read at any instant and half a JWT is not a
/// refusal a worker can act on: the token goes into a new file beside `path`
/// (so the rename stays within one filesystem), which then replaces `path` in
/// one rename.
///
/// That file is created owner-only, so the token is never readable by anyone
/// else under any name, the temporary one included. Its name is fresh for each
/// write and it is opened with `create_new`, which refuses whatever is already
/// at that name, a planted symlink included: someone who can write to the
/// directory cannot have the token written anywhere else. The file is synced
/// before the rename and the directory after it, so a crash leaves the old
/// token or the new one.
///
/// # Errors
///
/// Returns a message naming the path when the write or the rename fails. A
/// failed write leaves the previous token in place and no temporary behind.
pub fn write_token_file(path: &Path, token: &str) -> Result<(), String> {
    use std::io::Write as _;
    write_private_file(path, |file| file.write_all(token.as_bytes()))
}

/// [`write_token_file`], with the step that writes the new file's contents handed in.
fn write_private_file(
    path: &Path,
    write_contents: impl FnOnce(&mut std::fs::File) -> std::io::Result<()>,
) -> Result<(), String> {
    /// The temporary until it is renamed into place; dropping it removes it.
    struct Pending(PathBuf);
    impl Drop for Pending {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    // A bare file name has an empty parent, which names no directory to open;
    // the file then lives in the working directory, so that is the one synced.
    let parent = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    std::fs::create_dir_all(parent)
        .map_err(|error| format!("join token directory {}: {error}", parent.display()))?;
    let name = path
        .file_name()
        .ok_or_else(|| format!("join token file {} names no file", path.display()))?;
    let temporary = parent.join(format!(
        ".{}.{}.tmp",
        name.to_string_lossy(),
        uuid::Uuid::new_v4().simple()
    ));
    let mut file = create_private(&temporary)
        .map_err(|error| format!("join token file {}: create: {error}", temporary.display()))?;
    let pending = Pending(temporary);
    write_contents(&mut file)
        .and_then(|()| file.sync_all())
        .map_err(|error| format!("join token file {}: write: {error}", pending.0.display()))?;
    drop(file);
    std::fs::rename(&pending.0, path)
        .map_err(|error| format!("join token file {}: rename: {error}", path.display()))?;
    std::fs::File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| format!("join token directory {}: sync: {error}", parent.display()))
}

/// Create `path` as a new file that only its owner can read or write.
///
/// `create_new` refuses any name that exists, a symlink included, so the
/// token is never written into a file someone else placed. The mode is part of
/// the create, so there is no moment at which the file is readable by others.
fn create_private(path: &Path) -> std::io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    options.open(path)
}

/// Mint one token and write it, if this replica holds the minter lease.
///
/// # Errors
///
/// Returns a message when the lock could not be asked for, the grant does not
/// mint, or the file could not be replaced.
pub async fn rotate_once(
    pg: &compio_postgres::Client,
    config: &MinterConfig,
    audience: &zeroship_core::service_assertion::ServiceIssuer,
) -> Result<RotationOutcome, String> {
    if !claim_minter_lease(pg).await? {
        return Ok(RotationOutcome::NotTheMinter);
    }
    let token = mint_join_token(
        &config.signer_id,
        &config.key,
        audience,
        &JoinTokenGrant {
            zone: config.zone.clone(),
            lifetime: TOKEN_TTL,
            uses: TOKEN_USES,
            confirm: None,
        },
    )?;
    write_token_file(&config.path, &token)?;
    Ok(RotationOutcome::Rotated)
}

/// Ask for the minter lease. `true` means this replica is the minter.
///
/// # Errors
///
/// Returns a message when the statement could not be executed. A replica that
/// cannot ask does not mint: minting on an unanswered question is how two
/// replicas end up writing the same file.
pub async fn claim_minter_lease(pg: &compio_postgres::Client) -> Result<bool, String> {
    let row = pg
        .query_one("SELECT pg_try_advisory_lock($1)", &[&MINTER_LOCK_KEY])
        .await
        .map_err(|error| format!("join token minter: ask for the lease: {error}"))?;
    Ok(row.get(0))
}

/// Rotate forever, on [`ROTATION_INTERVAL`], SLEEPING FIRST.
///
/// The first rotation is the CALLER's, awaited before this control plane binds
/// its port: a deployment orders its workers after `control: service_healthy`,
/// and a minter that started rotating concurrently with the bind would let a
/// worker read a volume with no token in it yet and refuse its own boot. The
/// ordering is then a property of the startup sequence rather than of how fast
/// two tasks happen to run.
///
/// A rotation that fails here is logged and retried on the next tick rather
/// than ending the task: the previous token is still valid for the rest of its
/// TTL, so a transient failure costs nothing as long as one succeeds inside the
/// margin.
pub async fn run(
    pg: std::sync::Arc<compio_postgres::Client>,
    config: MinterConfig,
    audience: zeroship_core::service_assertion::ServiceIssuer,
) {
    loop {
        compio::time::sleep(ROTATION_INTERVAL).await;
        match rotate_once(&pg, &config, &audience).await {
            Ok(RotationOutcome::Rotated) => {
                tracing::info!(
                    path = %config.path.display(),
                    zone = config.zone.as_str(),
                    "control: rotated the local join token"
                );
            }
            Ok(RotationOutcome::NotTheMinter) => {
                tracing::debug!(
                    "control: another replica holds the join token minter lease; standing by"
                );
            }
            Err(error) => {
                tracing::error!(%error, "control: join token rotation failed; the previous token stays valid until its expiry");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// THE RELATIONSHIP, not the values. A worker that read the file an instant
    /// before a rotation must still be presenting a live token after its whole
    /// boot margin has passed.
    #[test]
    fn a_token_read_just_before_a_rotation_survives_a_full_boot() {
        assert!(
            TOKEN_TTL > ROTATION_INTERVAL,
            "a token must outlive the interval that replaces it"
        );
        let overlap = TOKEN_TTL
            .checked_sub(ROTATION_INTERVAL)
            .expect("a token outlives the interval that replaces it");
        assert!(
            overlap >= BOOT_MARGIN,
            "the overlap a rotation leaves must cover a worker's boot"
        );
    }

    /// The minted grant is one the verifier accepts, so a rotation cannot write
    /// a token every worker then refuses.
    #[test]
    fn the_rotation_grant_mints_and_verifies() {
        let key = ServiceSigningKey::generate();
        let signer_id = zeroship_core::typed_id::new_join_signer_id();
        let audience =
            zeroship_core::service_peers::service_issuer(zeroship_core::service_peers::CONTROL_SERVICE_NAME)
                .expect("control issuer");
        let token = mint_join_token(
            &signer_id,
            &key,
            &audience,
            &JoinTokenGrant {
                zone: zeroship_core::worker_join::DEFAULT_EXECUTION_ZONE.to_owned(),
                lifetime: TOKEN_TTL,
                uses: TOKEN_USES,
                confirm: None,
            },
        )
        .expect("the rotation grant mints");
        let verified = zeroship_core::worker_join::verify_join_token(
            &token,
            &key.verifying_key_bytes(),
            &audience,
            std::time::SystemTime::now(),
        )
        .expect("and verifies");
        assert_eq!(verified.uses, TOKEN_USES);
        assert_eq!(verified.signer_id, signer_id);
    }

    /// The names in `dir`, sorted.
    fn entries(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .expect("read the token directory")
            .map(|entry| entry.expect("entry").file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    /// The file lands complete and owner-only, and replacing it leaves no
    /// temporary behind for a worker to read instead.
    #[test]
    fn the_token_file_is_replaced_atomically_and_owner_only() {
        let dir = tempfile::tempdir().expect("token directory");
        let path = dir.path().join("nested").join("join-token");
        write_token_file(&path, "first.token.value").expect("writes");
        assert_eq!(
            std::fs::read_to_string(&path).expect("reads"),
            "first.token.value"
        );
        write_token_file(&path, "second.token.value").expect("replaces");
        assert_eq!(
            std::fs::read_to_string(&path).expect("reads"),
            "second.token.value"
        );
        assert_eq!(
            entries(path.parent().expect("parent")),
            vec!["join-token".to_string()],
            "a temporary was left behind"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&path).expect("stat").permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "the token must be owner-only");
        }
    }

    /// The file the token is written into is owner-only from the moment it
    /// exists, before any byte of the token is in it, so no other user can
    /// read the token under any name at any point. Every file in the directory
    /// is looked at while the token is being written.
    #[cfg(unix)]
    #[test]
    fn the_token_is_only_ever_written_into_an_owner_only_file() {
        use std::io::Write as _;
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().expect("token directory");
        // The mode a plain create gets under this process's umask, printed so
        // `the_owner_only_mode_holds_under_an_open_umask` can confirm the
        // umask it ran this test under.
        let witness = dir.path().join("umask-witness");
        std::fs::File::create(&witness).expect("umask witness");
        let default_mode = std::fs::metadata(&witness).expect("stat").permissions().mode() & 0o777;
        std::fs::remove_file(&witness).expect("remove the witness");
        println!("default file mode {default_mode:o}");

        let path = dir.path().join("join-token");
        write_token_file(&path, "previous.token.value").expect("writes the first token");
        let mut seen = Vec::new();
        write_private_file(&path, |file| {
            let own = file.metadata()?.permissions().mode();
            seen.push(("the file being written".to_string(), own));
            for entry in std::fs::read_dir(dir.path())? {
                let entry = entry?;
                let mode = entry.metadata()?.permissions().mode();
                seen.push((entry.file_name().to_string_lossy().into_owned(), mode));
            }
            file.write_all(b"next.token.value")
        })
        .expect("writes the next token");

        assert!(seen.len() >= 3, "the write step saw its own file and the directory: {seen:?}");
        for (name, mode) in &seen {
            assert_eq!(
                mode & 0o077,
                0,
                "{name} was readable by others (mode {mode:o}) while the token was written"
            );
        }
        assert_eq!(
            std::fs::read_to_string(&path).expect("reads"),
            "next.token.value"
        );
    }

    /// Run one test of this binary in a child process, in `cwd`, optionally
    /// under a `umask`; return whether it passed and its output. Every caller
    /// asserts `1 passed`: a name that matches nothing runs zero tests and
    /// still exits 0. The working directory and the umask are process-wide,
    /// which is why these run in a child and not in this process.
    fn run_one_test_in_child(
        name: &str,
        ignored: bool,
        umask: Option<&str>,
        cwd: &Path,
    ) -> (bool, String) {
        let test = format!(
            "{}::{name}",
            module_path!().split_once("::").expect("crate prefix").1
        );
        let exe = std::env::current_exe().expect("this test binary");
        let mut args = vec![
            "--exact".to_string(),
            test,
            "--nocapture".to_string(),
            "--test-threads=1".to_string(),
        ];
        if ignored {
            args.push("--ignored".to_string());
        }
        let mut command = umask.map_or_else(
            || std::process::Command::new(&exe),
            |mask| {
                let mut shell = std::process::Command::new("sh");
                shell
                    .arg("-c")
                    .arg(format!("umask {mask} && exec \"$0\" \"$@\""))
                    .arg(&exe);
                shell
            },
        );
        let output = command
            .args(&args)
            .current_dir(cwd)
            .output()
            .expect("run a child copy of this test binary");
        let mut combined = String::from_utf8_lossy(&output.stdout).into_owned();
        combined.push_str(&String::from_utf8_lossy(&output.stderr));
        (output.status.success(), combined)
    }

    /// The owner-only mode comes from the create, not from a umask that
    /// happens to mask group and other bits: under an open umask, where a plain
    /// create is readable by everyone, the file is still owner-only throughout.
    #[cfg(unix)]
    #[test]
    fn the_owner_only_mode_holds_under_an_open_umask() {
        let cwd = tempfile::tempdir().expect("child working directory");
        let (passed, output) = run_one_test_in_child(
            "the_token_is_only_ever_written_into_an_owner_only_file",
            false,
            Some("000"),
            cwd.path(),
        );
        assert!(
            output.contains("default file mode 666"),
            "the child did not run under umask 000:\n{output}"
        );
        assert!(passed && output.contains("1 passed"), "{output}");
    }

    /// A token path given as a bare file name, relative to the working
    /// directory, has an empty parent. The token lands in the working
    /// directory and the write succeeds; the directory it syncs is the working
    /// directory, not an empty path that names nothing.
    #[test]
    fn a_bare_token_file_name_is_written_in_the_working_directory() {
        let cwd = tempfile::tempdir().expect("child working directory");
        let (passed, output) = run_one_test_in_child(
            "write_a_bare_token_file_name_here",
            true,
            None,
            cwd.path(),
        );
        assert!(passed && output.contains("1 passed"), "{output}");
        assert_eq!(
            std::fs::read_to_string(cwd.path().join("join-token")).expect("the token landed"),
            "bare.token.value"
        );
        assert_eq!(entries(cwd.path()), vec!["join-token".to_string()]);
    }

    /// The child half of `a_bare_token_file_name_is_written_in_the_working_directory`.
    #[test]
    #[ignore = "run by a_bare_token_file_name_is_written_in_the_working_directory, \
                in a scratch working directory"]
    fn write_a_bare_token_file_name_here() {
        let here = std::env::current_dir().expect("working directory");
        let scratch = std::env::temp_dir()
            .canonicalize()
            .expect("the temp directory");
        assert!(
            here.starts_with(&scratch),
            "refusing to write a token outside a scratch directory: {}",
            here.display()
        );
        write_token_file(Path::new("join-token"), "bare.token.value")
            .expect("a bare file name writes");
    }

    /// The token's file is only ever created new. A name that already exists,
    /// a symlink to someone else's file or a plain file, is refused, and what
    /// it names keeps its bytes.
    #[cfg(unix)]
    #[test]
    fn a_private_file_is_never_created_over_an_existing_name() {
        let dir = tempfile::tempdir().expect("token directory");
        let elsewhere = dir.path().join("elsewhere");
        std::fs::write(&elsewhere, "sentinel").expect("someone else's file");
        let symlink = dir.path().join(".join-token.planted.tmp");
        std::os::unix::fs::symlink(&elsewhere, &symlink).expect("plant a symlink");
        let plain = dir.path().join(".join-token.taken.tmp");
        std::fs::write(&plain, "taken").expect("a file at the name");

        for name in [&symlink, &plain] {
            let refused = create_private(name).map(|_| ()).map_err(|e| e.kind());
            assert_eq!(refused, Err(std::io::ErrorKind::AlreadyExists), "{}", name.display());
        }
        assert_eq!(std::fs::read_to_string(&elsewhere).expect("reads"), "sentinel");
        assert_eq!(std::fs::read_to_string(&plain).expect("reads"), "taken");
    }

    /// Someone who can write to the token's directory plants a symlink where a
    /// fixed temporary name would be. The token is not written through it, and
    /// the token file is a regular file afterwards.
    #[cfg(unix)]
    #[test]
    fn a_symlink_planted_beside_the_token_file_is_not_followed() {
        let dir = tempfile::tempdir().expect("token directory");
        let elsewhere = tempfile::tempdir().expect("the planter's directory");
        let stolen = elsewhere.path().join("stolen");
        std::fs::write(&stolen, "sentinel").expect("the planter's file");
        let path = dir.path().join("join-token");
        let planted = [
            path.with_extension("tmp"),
            dir.path().join(".join-token.tmp"),
        ];
        for link in &planted {
            std::os::unix::fs::symlink(&stolen, link).expect("plant a symlink");
        }

        write_token_file(&path, "secret.token.value").expect("writes");

        assert_eq!(
            std::fs::read_to_string(&stolen).expect("reads the planter's file"),
            "sentinel",
            "the token was written through a planted symlink"
        );
        let meta = std::fs::symlink_metadata(&path).expect("stat the token file");
        assert!(meta.file_type().is_file(), "the token file is not a regular file");
        assert_eq!(
            std::fs::read_to_string(&path).expect("reads"),
            "secret.token.value"
        );
    }

    /// A write that fails leaves the previous token in place and no temporary.
    #[test]
    fn a_failed_write_keeps_the_previous_token_and_leaves_no_temporary() {
        let dir = tempfile::tempdir().expect("token directory");
        let path = dir.path().join("join-token");
        write_token_file(&path, "previous.token.value").expect("writes the first token");

        let refused = write_private_file(&path, |_| Err(std::io::Error::other("disk full")));
        assert!(refused.is_err(), "a failed write is reported");

        // A rename that fails: a non-empty directory where the token goes.
        let blocked = dir.path().join("blocked");
        std::fs::create_dir_all(blocked.join("occupant")).expect("block the name");
        assert!(write_token_file(&blocked, "never.lands").is_err());

        assert_eq!(
            std::fs::read_to_string(&path).expect("reads"),
            "previous.token.value"
        );
        assert_eq!(
            entries(dir.path()),
            vec!["blocked".to_string(), "join-token".to_string()],
            "a failed write left a temporary behind"
        );
    }
}
