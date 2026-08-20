//! A per-machine file lock, held for exactly as long as a closure runs.
//!
//! WHY `flock` AND NOT A POSTGRESQL ADVISORY LOCK. An advisory lock has to be
//! HELD by an open session for the whole bracket, and the bracket here spans a
//! separate process -- the migrate binary. Holding one means keeping a session
//! alive across that process's whole lifetime with no way to prove the lock was
//! granted before proceeding, and a lock you cannot confirm you hold is worse
//! than none because it reads as protection. `flock`'s guarantee is the one
//! wanted: the caller does not continue until the descriptor is held.
//!
//! WHAT IT DOES NOT COVER: agents on DIFFERENT MACHINES sharing one server.
//! Every agent here runs on one box and CI gives each job its own PostgreSQL
//! service, so the covered case is the one that exists -- but a second machine
//! pointed at the same cluster reopens the race, and the fix then belongs in
//! the migrate binary beside `cluster_lock.rs`, not here.

use std::fs::{OpenOptions, TryLockError};
use std::path::Path;
use std::time::{Duration, Instant};

/// Why a lock was not taken.
///
/// The two arms are separated because the CALLER SAYS DIFFERENT THINGS about
/// them: "waited 900s for another run" is the right message for one and an
/// actively misleading one for the other -- a lock directory that does not
/// exist would otherwise be reported as a peer holding the lock for fifteen
/// minutes.
#[derive(Debug)]
pub enum LockError {
    /// The wait ran out with a peer still holding it.
    TimedOut,
    /// The lock file could not be opened or locked at all.
    Unusable(std::io::Error),
}

/// Take an exclusive lock on `path`, run `body`, release.
///
/// The lock is released by the file being closed when this function returns,
/// on every path out including a panic -- which is the same property the
/// shell's subshell-plus-`9>` construction was buying, arrived at without
/// needing a comment to explain which file descriptor was which.
///
/// # Errors
/// [`LockError::TimedOut`] when a peer still holds it after `timeout`, or
/// [`LockError::Unusable`] when the file could not be opened or locked.
pub fn with_file_lock<T>(
    path: &Path,
    timeout: Duration,
    body: impl FnOnce() -> T,
) -> Result<T, LockError> {
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
        .map_err(LockError::Unusable)?;

    // POLL RATHER THAN BLOCK, so the timeout is ours rather than a signal's.
    // `flock(2)` has no timeout of its own, and `flock(1)` implements
    // `--timeout` with SIGALRM -- which would leave a stray handler installed
    // in a process that goes on to run a migrate command.
    //
    // `File::try_lock` is `flock(LOCK_EX|LOCK_NB)` underneath and is safe, so
    // this crate declares no `unsafe` and needs no `libc`.
    let deadline = Instant::now() + timeout;
    loop {
        match file.try_lock() {
            Ok(()) => break,
            Err(TryLockError::WouldBlock) => {}
            Err(TryLockError::Error(why)) => return Err(LockError::Unusable(why)),
        }
        if Instant::now() >= deadline {
            return Err(LockError::TimedOut);
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    let out = body();
    let _ = file.unlock();
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    #[test]
    fn a_second_taker_waits_for_the_first() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("held.lock");
        let inside = Arc::new(AtomicBool::new(false));
        let overlapped = Arc::new(AtomicBool::new(false));

        let (p2, in2, ov2) = (path.clone(), inside.clone(), overlapped.clone());
        let held = with_file_lock(&path, Duration::from_secs(5), || {
            inside.store(true, Ordering::SeqCst);
            let peer = std::thread::spawn(move || {
                // A separate flock() call in the same PROCESS is the weak form
                // of this test -- flock is per open-file-description, so a
                // second `open` genuinely blocks. That is what the peer does.
                let _ = with_file_lock(&p2, Duration::from_millis(300), || {
                    if in2.load(Ordering::SeqCst) {
                        ov2.store(true, Ordering::SeqCst);
                    }
                });
            });
            std::thread::sleep(Duration::from_millis(150));
            inside.store(false, Ordering::SeqCst);
            peer.join().unwrap();
        });
        assert!(held.is_ok());
        assert!(
            !overlapped.load(Ordering::SeqCst),
            "the second taker ran while the first still held the lock"
        );
    }

    #[test]
    fn the_wait_can_run_out() {
        // The partner of the case above: without this, a lock that never
        // granted and a lock that always granted are indistinguishable.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("timeout.lock");
        let outcome = with_file_lock(&path, Duration::from_secs(5), || {
            let p2 = path.clone();
            std::thread::spawn(move || with_file_lock(&p2, Duration::from_millis(120), || ()))
                .join()
                .unwrap()
        });
        assert!(outcome.unwrap().is_err(), "the inner taker should have timed out");
    }
}
