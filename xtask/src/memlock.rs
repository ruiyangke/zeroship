//! The locked-memory limit a test run needs.
//!
//! Every compio runtime opens an io_uring, and the kernel charges each ring's
//! memory to the locked memory of the USER that created it, refusing a new ring
//! once that user-wide total would pass the creating process's
//! `RLIMIT_MEMLOCK`. Every test process of a run, and every service binary a
//! test starts, runs as the same user, so the whole run shares one budget. A
//! service default (systemd's, which a hosted CI runner's processes inherit) is
//! sized for a few processes, not a platform of them: when it is exhausted the
//! next runtime to start fails with `Os { code: 12, kind: OutOfMemory }`,
//! wherever that happens to be, and the panic names the process that lost the
//! race rather than the limit.
//!
//! So every shard first raises this process's soft limit to its hard limit,
//! which needs no privilege and which every child inherits, and a shard whose
//! tests start fleets of service processes refuses to start below [`FLOOR`],
//! naming the limit and how to raise it, instead of failing tests at random.
//! This touches only the limits of xtask's own process and its children.

use crate::Result;
use nix::sys::resource::{getrlimit, setrlimit, Resource};

/// The lowest soft `RLIMIT_MEMLOCK` a run that starts service fleets accepts,
/// in bytes. Unlimited passes.
///
/// It is set by measurement, not by taste. The billing shard starts Control's
/// workflow fleets several at a time, and each fleet's processes open a ring
/// per runtime thread at compio's default ring size. Summing the `io_uring`
/// descriptors in `/proc/<pid>/fd` across every process at the peak of those
/// concurrent fleets, and charging each the locked memory one ring of that size
/// costs (`io_uring_setup` refuses the first ring past a given limit, which
/// pins the per-ring charge), gives the run's peak. The floor is the power of
/// two more than an order of magnitude above that peak, so a run's own
/// processes stay below it with room for the shard to grow, while the service
/// default sits below the peak itself.
pub const FLOOR: u64 = 1 << 30;

/// Raise this process's soft locked-memory limit to its hard limit, and refuse
/// to run when the result is below [`FLOOR`].
pub fn require() -> Result<()> {
    verdict(raise()?)
}

/// Raise this process's soft locked-memory limit to its hard limit, which every
/// process it starts inherits, and return the soft limit now in force.
pub fn raise() -> Result<u64> {
    let (soft, hard) = getrlimit(Resource::RLIMIT_MEMLOCK)?;
    if soft != hard {
        setrlimit(Resource::RLIMIT_MEMLOCK, hard, hard)?;
    }
    Ok(getrlimit(Resource::RLIMIT_MEMLOCK)?.0)
}

/// The verdict for a soft limit of `soft` bytes, where `RLIM_INFINITY` is
/// unlimited.
fn verdict(soft: u64) -> Result<()> {
    if soft == nix::libc::RLIM_INFINITY || soft >= FLOOR {
        return Ok(());
    }
    Err(format!(
        "RLIMIT_MEMLOCK is {soft} bytes even with the soft limit raised to the hard one. Every \
         io_uring a test process or a service it starts opens is charged to one per-user \
         locked-memory budget, and below {FLOOR} bytes the service fleets this shard starts \
         fail with `Os {{ code: 12 }}`. Raise the hard limit of the shell that runs the tests, \
         e.g. `sudo prlimit --pid $$ --memlock=unlimited:unlimited`; CI raises it in \
         .github/actions/setup."
    )
    .into())
}

#[cfg(test)]
mod tests {
    use super::{raise, verdict, FLOOR};
    use nix::sys::resource::{getrlimit, Resource};

    #[test]
    fn an_unlimited_or_floor_sized_limit_runs() {
        assert!(verdict(nix::libc::RLIM_INFINITY).is_ok());
        assert!(verdict(FLOOR).is_ok());
    }

    /// A limit of the size systemd gives services by default is refused, and
    /// the refusal names the measured limit so the log says what to raise.
    #[test]
    fn a_service_default_limit_is_refused_by_name() {
        let service_default = 8 << 20;
        let error = verdict(service_default).expect_err("a small limit must be refused");
        let message = error.to_string();
        assert!(
            message.contains(&service_default.to_string()),
            "the refusal names the limit it read: {message}"
        );
        assert!(verdict(FLOOR - 1).is_err(), "the floor is a lower bound");
    }

    /// After raising, this process's soft limit equals its hard limit. The
    /// test first lowers its own soft limit, which needs no privilege, so the
    /// raise has something to do whatever limits the test started with.
    #[test]
    fn raising_lifts_the_soft_limit_to_the_hard_limit() {
        let (_, hard) = getrlimit(Resource::RLIMIT_MEMLOCK).expect("read the limit");
        let lowered = hard.min(64 << 10) / 2;
        nix::sys::resource::setrlimit(Resource::RLIMIT_MEMLOCK, lowered, hard)
            .expect("lower the soft limit");
        assert_eq!(
            getrlimit(Resource::RLIMIT_MEMLOCK).expect("read the lowered limit").0,
            lowered,
            "the control: the soft limit starts below the hard one"
        );
        let soft = raise().expect("raise the soft limit");
        assert_eq!(soft, hard, "the soft limit now in force is the hard limit");
        assert_eq!(
            getrlimit(Resource::RLIMIT_MEMLOCK).expect("read the limit again"),
            (hard, hard)
        );
    }
}
