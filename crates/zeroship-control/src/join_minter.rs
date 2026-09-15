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
/// refusal a worker can act on. The temporary file is created beside the target
/// so the rename stays within one filesystem, and the mode is set BEFORE the
/// rename so the file is never briefly world-readable under its final name.
///
/// # Errors
///
/// Returns a message naming the path when the write or the rename fails.
pub fn write_token_file(path: &Path, token: &str) -> Result<(), String> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)
        .map_err(|error| format!("join token directory {}: {error}", parent.display()))?;
    let temporary = path.with_extension("tmp");
    std::fs::write(&temporary, token.as_bytes())
        .map_err(|error| format!("join token file {}: write: {error}", temporary.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o600)).map_err(
            |error| format!("join token file {}: chmod: {error}", temporary.display()),
        )?;
    }
    std::fs::rename(&temporary, path)
        .map_err(|error| format!("join token file {}: rename: {error}", path.display()))
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

    /// The file lands complete and owner-only, and replacing it leaves no
    /// temporary behind for a worker to read instead.
    #[test]
    fn the_token_file_is_replaced_atomically_and_owner_only() {
        let dir = std::env::temp_dir().join(format!(
            "zeroship-join-minter-{}",
            zeroship_core::typed_id::new_join_signer_id()
        ));
        let path = dir.join("nested").join("join-token");
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
        assert!(!path.with_extension("tmp").exists(), "a temporary was left behind");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&path).expect("stat").permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "the token must be owner-only");
        }
        std::fs::remove_dir_all(&dir).ok();
    }
}
