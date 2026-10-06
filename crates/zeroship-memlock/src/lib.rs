//! Lifts the locked-memory limit and probes that the kernel grants an `io_uring` ring.
//!
//! Every compio runtime opens an `io_uring`. The kernel charges each ring's
//! memory to the locked memory of the user that created it, so a refused ring
//! usually means that user's `RLIMIT_MEMLOCK`; the kernel reports it as `ENOMEM`
//! with no name for the limit. [`prepare_or_exit`] does at a binary boundary
//! what no threshold can: it builds and drops one real runtime, so the kernel's
//! own answer decides whether the process starts, and [`explain`] rewrites a
//! refusal to say what happened, the values read at that moment and the remedy.
//! Every other error passes through unchanged.
//!
//! [`raise`] lifts the soft limit to the hard limit, which needs no privilege.
//! There is no per-process floor: one binary's rings need far less than a
//! fleet's summed budget, so a pre-check against the fleet ceiling would refuse
//! limits that work.

use std::fmt;
use std::io;

use nix::sys::resource::{getrlimit, setrlimit, Resource};

/// The `RLIMIT_MEMLOCK` values in force, as `(soft, hard)`, where
/// `RLIM_INFINITY` is unlimited.
fn limits() -> io::Result<(u64, u64)> {
    getrlimit(Resource::RLIMIT_MEMLOCK).map_err(io::Error::from)
}

/// Raise this process's soft locked-memory limit to its hard limit, which every
/// process it starts inherits, and return the soft limit now in force.
///
/// # Errors
///
/// Propagates a `getrlimit` or `setrlimit` failure.
pub fn raise() -> io::Result<u64> {
    let (soft, hard) = limits()?;
    if soft != hard {
        setrlimit(Resource::RLIMIT_MEMLOCK, hard, hard).map_err(io::Error::from)?;
        return Ok(limits()?.0);
    }
    Ok(soft)
}

/// Rewrite an `ENOMEM` from an `io_uring` construction into an explanation.
///
/// The message says the kernel refused a ring with `ENOMEM`, that each ring is
/// charged to the user's locked memory and so the usual cause is
/// `RLIMIT_MEMLOCK`, names the soft and hard values read at that moment (or
/// that they could not be read, with the read error), and ends with [`REMEDY`].
/// Any other error is returned unchanged, kind and OS error included.
#[must_use]
pub fn explain(error: io::Error) -> io::Error {
    if error.kind() != io::ErrorKind::OutOfMemory {
        return error;
    }
    let explanation = match limits() {
        Ok((soft, hard)) => Explanation::AtLimit { soft, hard },
        Err(read) => Explanation::Unreadable { error: read },
    };
    io::Error::new(io::ErrorKind::OutOfMemory, explanation.to_string())
}

/// Prepare a shipped binary to open `io_uring` rings, or exit.
///
/// Binary-boundary only: call it at the start of a `main`, before any runtime
/// is built. Library code must never call it, because it exits the process on refusal. It lifts
/// the soft limit to the hard one (warning and continuing if it cannot), then
/// builds and drops one compio runtime. A refusal is explained through
/// [`explain`] and the process exits nonzero instead of panicking when the real
/// runtime starts.
pub fn prepare_or_exit(binary: &str) {
    if let Err(error) = raise() {
        eprintln!(
            "{binary}: cannot raise the RLIMIT_MEMLOCK soft limit to the hard limit: {error}"
        );
    }
    if let Err(error) = compio::runtime::Runtime::new() {
        eprintln!("{binary}: {}", explain(error));
        std::process::exit(1);
    }
}

/// Why an `io_uring` ring was refused, carrying only what is known.
#[derive(Debug)]
enum Explanation {
    /// The limit could not be read, so no value is known.
    Unreadable { error: io::Error },
    /// The limits read when the ring was refused.
    AtLimit { soft: u64, hard: u64 },
}

impl fmt::Display for Explanation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unreadable { error } => write!(
                f,
                "the kernel refused an io_uring ring with ENOMEM; each ring is charged to the \
                 user's locked memory, so the usual cause is RLIMIT_MEMLOCK, and the limit could \
                 not be read: {error}. {REMEDY}"
            ),
            Self::AtLimit { soft, hard } => write!(
                f,
                "the kernel refused an io_uring ring with ENOMEM; each ring is charged to the \
                 user's locked memory, so the usual cause is RLIMIT_MEMLOCK (soft limit {soft} \
                 bytes, hard limit {hard} bytes). {REMEDY}"
            ),
        }
    }
}

/// The operator-facing remedy, one spelling per deployment surface.
pub const REMEDY: &str = "Raise it with `ulimit -l unlimited` in the shell, \
     `LimitMEMLOCK=infinity` in the systemd unit, or \
     `ulimits: { memlock: { soft: -1, hard: -1 } }` on the Compose service.";

#[cfg(test)]
mod tests {
    use super::{explain, Explanation, REMEDY};

    #[test]
    fn an_enomem_is_explained_with_the_limit_and_the_remedy() {
        let explained = explain(std::io::Error::from_raw_os_error(nix::libc::ENOMEM));
        assert_eq!(explained.kind(), std::io::ErrorKind::OutOfMemory);
        let message = explained.to_string();
        assert!(message.contains("RLIMIT_MEMLOCK"), "{message}");
        assert!(message.contains(REMEDY), "{message}");
    }

    #[test]
    fn any_other_error_passes_through_unchanged() {
        let explained = explain(std::io::Error::from_raw_os_error(nix::libc::EINVAL));
        assert_eq!(explained.kind(), std::io::ErrorKind::InvalidInput);
        assert_eq!(explained.raw_os_error(), Some(nix::libc::EINVAL));
    }

    #[test]
    fn an_unreadable_limit_does_not_invent_values() {
        let message = Explanation::Unreadable {
            error: std::io::Error::from_raw_os_error(nix::libc::EPERM),
        }
        .to_string();
        assert!(message.contains("RLIMIT_MEMLOCK"), "{message}");
        assert!(message.contains("Operation not permitted"), "{message}");
        assert!(message.contains(REMEDY), "{message}");
        assert!(!message.contains("0 bytes soft"), "{message}");
    }
}
