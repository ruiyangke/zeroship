//! Die when the process that spawned us dies.
//!
//! `zeroship serve` is spawned as a CHILD of the vite dev server
//! (`sdks/vite-plugin/src/dev-server.ts`). When vite is killed by its recorded
//! PID - a harness, a crash, an OOM kill, a `kill -9` in a terminal - the
//! runtime child SURVIVES. It keeps its listening socket and, worse, keeps an
//! exclusive redb lock on the example's `.zeroship/kv.redb` forever.
//!
//! Measured on 2026-08-10 (task #221): FOUR orphaned `zeroship serve`
//! processes on ports 3151/3161/3171/3181, aged 10 to 34 minutes, all holding
//! one redb file. The next run of that example could not boot on ANY port,
//! because the contended resource is the per-example state dir and not the
//! port. Killing the four made the next boot succeed in 3 seconds.
//!
//! # Why the parent cannot do this
//!
//! `dev-server.ts` already has a teardown path (`killChild`: SIGTERM, then
//! SIGKILL after 3s). It is not the fix, and cannot be: a SIGKILLed parent runs
//! no code at all. Nor can the fix live in whoever does the killing - that is
//! an arbitrary harness, a crashed process, or the kernel's OOM killer, none of
//! which can be asked to kill a process group. Only two parties can act after
//! the parent is gone: the kernel, and this process. So the guard lives here.
//!
//! # The contract
//!
//! `ZEROSHIP_DIE_WITH_PARENT=<pid of the spawning process>` - opt in, and name
//! the parent. Absent, nothing happens: a bare `zeroship serve` typed into a
//! terminal must NOT die because some shell exited.
//!
//! The pid in the value is not decoration; it closes the arm-time race.
//! `PR_SET_PDEATHSIG` is armed by the child AFTER `exec`, so a parent that dies
//! in the window between `spawn` and this call is never signalled - the death
//! event has already passed and the process is already reparented. Comparing
//! `getppid()` to the recorded pid AFTER arming detects exactly that window.
//! `getppid() == 1` is NOT a usable check on its own: under a subreaper
//! (`PR_SET_CHILD_SUBREAPER`, which systemd user sessions and some supervisors
//! set) an orphan is reparented to the subreaper, not to init.
//!
//! # Known limits, stated rather than discovered
//!
//! - **SIGKILL, not SIGTERM.** This is the backstop for a cooperative teardown
//!   that has already failed, and a backstop a handler can swallow is not a
//!   backstop. Nothing in this binary installs a SIGTERM handler today (the
//!   only handler is the CPU timer's SIGRTMIN+1), so the observable behaviour
//!   is identical either way - but a future graceful-shutdown handler would
//!   silently reintroduce orphans if it hung, and it would not be given a
//!   chance to hang here. Both on-disk stores are crash-safe by construction
//!   (they must be: the existing `killChild` already SIGKILLs after 3s).
//! - **The kernel watches the parent THREAD, not the parent process.**
//!   `PR_SET_PDEATHSIG` fires when the thread that created this process exits.
//!   Node spawns from its event-loop thread, whose exit is the process's exit,
//!   so the distinction is invisible today. A future vite that spawned the
//!   runtime from a `worker_thread` would get an early kill when that worker
//!   exited.
//! - **It does not reap a grandparent's orphan.** If `pnpm` is killed and vite
//!   survives, this process's parent is still alive and it correctly stays up.
//!   That leaves a vite orphan, which is a different process's problem.
//! - `setsid` is irrelevant here: PDEATHSIG keys on parent death, not on the
//!   session or the controlling terminal. A process-group design would be the
//!   one broken by `setsid` - and by a killer that only knows one pid.

/// Opt-in gate. Value is the pid of the spawning process.
///
/// Declared here and mirrored in `sdks/vite-plugin/src/constants.ts`
/// (`ENV_DIE_WITH_PARENT`). Neither side's suite can see the other's spelling,
/// so the two are held together by behaviour rather than by string comparison:
/// step 7d of `tests/golden_path.sh` kills a REAL vite dev server and requires
/// the runtime it spawned to be gone. A rename on either side alone leaves the
/// variable unset on the child and turns that step red.
///
/// This constant is the spelling the diagnostics interpolate. The READ below
/// inlines the literal instead of using it, because the source gate lifts key
/// literals out of the syntax tree and cannot see a name behind a `&str`
/// constant. The two must not drift; a rename here without a rename there
/// leaves the guard reading a name nothing sets.
pub const ENV_DIE_WITH_PARENT: &str = "ZEROSHIP_DIE_WITH_PARENT";

/// Arm the guard from the environment. No-op when the variable is unset.
///
/// Call FIRST in `main`, before any port bind or state-dir open: everything
/// this process might hold is held from that point on.
pub fn arm_from_env() {
    let Some(raw) =
        zeroship_core::declared_env!(cli, "ZEROSHIP_DIE_WITH_PARENT", crate::ZeroshipCliConsumer)
    else {
        return;
    };
    let raw = raw.trim();
    if raw.is_empty() {
        return;
    }
    // A value that is not a live-looking pid still arms the guard, and says so.
    // Failing OPEN here (no guard) would reintroduce the orphan silently, which
    // is the one outcome this module exists to prevent; failing closed without
    // the cross-check only loses the arm-time race detection.
    let expected = match parse_parent_pid(raw) {
        Some(pid) => Some(pid),
        None => {
            eprintln!(
                "[zeroship] {ENV_DIE_WITH_PARENT}={raw} is not a parent pid - arming the \
                 parent-death guard WITHOUT the already-exited check (a parent that died \
                 before this point will not be noticed)"
            );
            None
        }
    };
    arm(expected);
}

/// `Some(pid)` for a value usable as the recorded parent.
///
/// pid 1 is rejected: an orphan is reparented TO 1 (or to a subreaper), so a
/// recorded parent of 1 makes the cross-check unable to distinguish the two.
fn parse_parent_pid(raw: &str) -> Option<i32> {
    raw.parse::<i32>().ok().filter(|pid| *pid > 1)
}

/// Ask the kernel to kill this process when its parent dies, then check whether
/// the parent is ALREADY dead.
///
/// The order is load-bearing. Checking first and arming second leaves a window
/// in which a parent death is caught by neither.
///
/// Allowed narrowly rather than crate-wide: `getppid` is the only unsafe call
/// in the CLI outside this module's kernel arm, and a `#![allow]` at the crate
/// root would quietly cover every future one.
#[allow(unsafe_code)]
fn arm(expected_parent: Option<i32>) {
    arm_kernel();

    let Some(expected) = expected_parent else {
        return;
    };
    // SAFETY: getppid() takes no arguments, touches no memory, and cannot fail.
    let current = unsafe { libc::getppid() };
    if current != expected {
        // Printed, not silent: this process's stdout/stderr is captured by the
        // dev-server supervisor's log tail, and an unexplained instant exit is
        // exactly the "runtime never starts" shape that took task #221 twenty
        // minutes to diagnose.
        eprintln!(
            "[zeroship] parent {expected} exited before the parent-death guard was armed \
             (current parent is {current}) - exiting rather than becoming an orphan holding \
             the state dir"
        );
        // 0, to match the arm this is standing in for: had the parent died a
        // millisecond later, the kernel would have SIGKILLed us and nobody
        // would read that as an error. The supervisor treats ANY exit inside
        // its healthy window as a failed start, so this still reaches the
        // terminal banner with the line above in the log tail.
        std::process::exit(0);
    }
}

#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
fn arm_kernel() {
    // SAFETY: PR_SET_PDEATHSIG takes a signal number by value and writes
    // nothing back. A failure is reported in the return value, not by
    // clobbering memory.
    let rc = unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) };
    if rc != 0 {
        eprintln!(
            "[zeroship] could not arm the parent-death guard (prctl PR_SET_PDEATHSIG: {}) - \
             this process can outlive its parent and hold the state dir open",
            std::io::Error::last_os_error()
        );
    }
}

/// Non-Linux fallback: poll `getppid()`.
///
/// There is no `PR_SET_PDEATHSIG` outside Linux. Doing nothing would leave the
/// same orphan with no message, so this trades a sleeping thread and up to
/// [`POLL_INTERVAL`] of latency for the guarantee. The `expected` pid is not
/// needed: any reparenting is a parent death, because a live parent's pid
/// cannot change.
#[cfg(not(target_os = "linux"))]
#[allow(unsafe_code)]
fn arm_kernel() {
    const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);
    // SAFETY: see arm().
    let original = unsafe { libc::getppid() };
    std::thread::spawn(move || loop {
        std::thread::sleep(POLL_INTERVAL);
        // SAFETY: see arm().
        if unsafe { libc::getppid() } != original {
            eprintln!("[zeroship] parent {original} exited - exiting with it");
            std::process::exit(0);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    // These cover the value contract only. They say NOTHING about whether an
    // orphan dies - that is kernel behaviour, and it is exercised for real,
    // against this binary, in crates/zeroship-cli/tests/parent_death_test.rs.
    #[test]
    fn a_plain_pid_is_accepted() {
        assert_eq!(parse_parent_pid("4242"), Some(4242));
        assert_eq!(parse_parent_pid("2"), Some(2));
    }

    #[test]
    fn init_and_nonsense_are_rejected() {
        // 1 is rejected because an orphan is reparented TO it, so it cannot
        // discriminate "still my parent" from "already gone".
        assert_eq!(parse_parent_pid("1"), None);
        assert_eq!(parse_parent_pid("0"), None);
        assert_eq!(parse_parent_pid("-1"), None);
        assert_eq!(parse_parent_pid("yes"), None);
        assert_eq!(parse_parent_pid(""), None);
    }
}
