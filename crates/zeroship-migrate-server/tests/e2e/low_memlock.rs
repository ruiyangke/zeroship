//! The migration service probes the kernel's `io_uring` grant at start.
//!
//! Every compio runtime opens an `io_uring`, and the kernel charges each ring to
//! the user's locked-memory budget (`RLIMIT_MEMLOCK`). The service's startup
//! probe builds and drops one runtime, so a limit too low for a ring is refused
//! by the kernel and explained, rather than panicking when the serving runtime
//! starts. This drives the real binary as a child with the limit set by
//! `setrlimit` in a `pre_exec` hook - no external tool and no environment
//! variable.

use std::os::unix::process::CommandExt;
use std::process::Output;

use zeroship_memlock::REMEDY;

use crate::support::service::service_command;

/// A budget below what one `io_uring` ring needs, so the kernel refuses the
/// startup probe rather than the dry run succeeding.
const LOW: libc::rlim_t = 64 << 10;

/// Spawn the service with `--check-config`, setting `RLIMIT_MEMLOCK` in the child.
///
/// `Some(limit)` pins both the soft and hard limits to `limit`, which a process
/// may always lower. `None` raises the soft limit to the current hard limit, the
/// most this process can grant, so the control is "as sufficient as this host
/// allows" rather than a number from the test.
fn run_migrate(limit: Option<libc::rlim_t>) -> Output {
    let mut command = service_command();
    command.args(["--no-config", "--check-config"]);

    // SAFETY: `pre_exec` runs between `fork` and `exec`, where only
    // async-signal-safe work is legal. `getrlimit`/`setrlimit` are syscalls and
    // the closure allocates nothing.
    #[expect(
        unsafe_code,
        reason = "pre_exec runs between fork and exec; getrlimit/setrlimit are syscalls and the closure allocates nothing"
    )]
    unsafe {
        command.pre_exec(move || {
            let mut current = libc::rlimit {
                rlim_cur: 0,
                rlim_max: 0,
            };
            if libc::getrlimit(libc::RLIMIT_MEMLOCK, &raw mut current) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            let target = limit.map_or(
                // Raising the soft limit up to the hard limit needs no privilege.
                libc::rlimit {
                    rlim_cur: current.rlim_max,
                    rlim_max: current.rlim_max,
                },
                |limit| libc::rlimit {
                    rlim_cur: limit,
                    rlim_max: limit,
                },
            );
            if libc::setrlimit(libc::RLIMIT_MEMLOCK, &raw const target) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    command.output().expect("spawn the migrate-server binary")
}

/// A limit too low for a ring exits nonzero with the kernel's refusal explained,
/// and without a panic.
#[test]
fn a_low_locked_memory_limit_is_explained_not_a_panic() {
    let output = run_migrate(Some(LOW));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "the service started with a limit too low for a ring\nstderr:\n{stderr}"
    );
    assert!(stderr.contains("RLIMIT_MEMLOCK"), "{stderr}");
    assert!(stderr.contains(&LOW.to_string()), "{stderr}");
    assert!(
        stderr.contains(REMEDY),
        "the refusal carries the remedy:\n{stderr}"
    );
    assert!(!stderr.contains("panicked"), "{stderr}");
}

/// The rejection control: with the soft limit raised to the hard one, the dry
/// run completes.
#[test]
fn a_sufficient_locked_memory_limit_passes_the_check() {
    let output = run_migrate(None);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "the service refused a sufficient locked-memory limit\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stdout.contains("check-config"),
        "the dry run produced no report, so it proved nothing:\n{stdout}"
    );
}
