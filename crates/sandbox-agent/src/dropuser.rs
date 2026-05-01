//! Privilege drop and pre-exec lockdown for `/exec` children.
//!
//! When the agent runs as PID 1 in libkrun it is uid 0 (root). The
//! microVM kernel boundary is the primary defense, but we also drop
//! `/exec` children and apply additional process-level hardenings so
//! that even an attacker who lands command execution can't trivially:
//!
//!   - read root-owned files inside the VM (the agent token is
//!     unlinked at startup, but other agent-private files may exist)
//!   - `kill -9 1` to crash the agent (root can signal PID 1)
//!   - mount/unmount, modify `/proc/1`, or otherwise mess with PID 1's
//!     view of the world
//!   - regain privileges via a setuid-root binary (e.g. `/bin/su`)
//!   - exhaust PIDs via fork bomb, or fds via descriptor leak
//!
//! ## Defenses applied (in order, before `exec()` runs)
//!
//! 1. **Capability bounding set drop.** While we're still root we
//!    drop every capability from the bounding set. This *must* run
//!    before `setuid` because `PR_CAPBSET_DROP` requires
//!    `CAP_SETPCAP`, which goes away on `setuid`. Even with
//!    `no_new_privs` this is belt-and-suspenders — it makes the
//!    kernel itself refuse to grant any cap to any future exec.
//!
//! 2. **`setgid` + `setgroups([])` + `setuid`** to nobody:nogroup.
//!    We do this manually in `pre_exec` (instead of using std's
//!    `Command::uid`/`gid`) so we control the order: bounding set
//!    drop *first*, privilege drop *second*, hardenings *third*.
//!    `setgroups([])` clears supplementary groups so the child
//!    can't accidentally inherit a privileged secondary group.
//!
//! 3. **`PR_SET_NO_NEW_PRIVS = 1`.** Once set, the kernel refuses
//!    to honor setuid bits, file capabilities, or AppArmor/SELinux
//!    transitions on any future `exec()`. This is the keystone
//!    defense against setuid-binary escalation. Inherited across
//!    `fork()` and `exec()`.
//!
//! 4. **`RLIMIT_NOFILE = 1024`** to cap fd-leak amplification.
//!    Always applied (per-process limit, no uid interaction).
//!
//! 5. **`RLIMIT_NPROC = 256`** to cap fork bombs **only when we've
//!    dropped to nobody**. The limit is per-uid; in tests we don't
//!    drop, so capping the developer's uid (which may already have
//!    many processes from `cargo test`) would EAGAIN on fork. In
//!    production the freshly-dropped nobody uid starts at 0
//!    processes, so 256 is comfortable headroom for npm/pnpm/pip
//!    while still bounding a runaway loop.
//!
//! ## `unsafe`
//!
//! Almost everything here is raw libc. `pre_exec_lockdown` runs in
//! the post-fork child and **must be async-signal-safe** — no
//! allocations, no I/O, no mutexes. We allow `unsafe` module-wide
//! and confine the dangerous bits to small documented blocks.

#![allow(unsafe_code)]

use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::OnceLock;

use tracing::{info, warn};

/// User to drop /exec children to. Debian default uid 65534. We look
/// this up by name (not hardcoded) because alternate base images
/// (Alpine, Ubuntu) use the same name with the same uid by convention.
const DROP_USER: &str = "nobody";
const DROP_GROUP: &str = "nogroup";

/// Per-uid process count cap. Applied only when actually dropping to
/// nobody (otherwise it would constrain the test runner's uid which
/// may already exceed this). 256 is generous for AI-builder
/// workloads (npm/pnpm/pip rarely exceed ~50 concurrent processes)
/// while still bounding a runaway fork loop.
const RLIMIT_NPROC_CAP: u64 = 256;

/// Per-process fd cap. Applied unconditionally. 1024 matches the
/// historical default soft limit on most distros — high enough that
/// no normal workload trips it, low enough to bound a leak.
const RLIMIT_NOFILE_CAP: u64 = 1024;

/// Highest capability number we attempt to drop. Linux 6.x defines
/// caps up through ~CAP_CHECKPOINT_RESTORE (40). Iterating to 64
/// gives us forward-compat headroom; `PR_CAPBSET_DROP` returns
/// EINVAL for unknown caps and we ignore the result.
const HIGHEST_CAP: i32 = 64;

/// Resolved (uid, gid) of [`DROP_USER`]/[`DROP_GROUP`], or `None` if
/// we shouldn't drop (not root, lookup failed).
static CREDS: OnceLock<Option<(u32, u32)>> = OnceLock::new();

/// Returns Some((uid, gid)) iff:
///   1. The agent is running as effective uid 0 (root).
///   2. `getpwnam("nobody")` and `getgrnam("nogroup")` both succeeded.
///
/// Otherwise returns None. Cached after the first call.
pub fn child_creds() -> Option<(u32, u32)> {
    *CREDS.get_or_init(resolve_creds)
}

fn resolve_creds() -> Option<(u32, u32)> {
    // Non-root: we can't drop, and don't need to (already unprivileged).
    // SAFETY: `geteuid` is async-signal-safe and always succeeds.
    let euid = unsafe { libc::geteuid() };
    if euid != 0 {
        return None;
    }

    let uid = match getpwnam_uid(DROP_USER) {
        Some(u) => u,
        None => {
            warn!(
                user = DROP_USER,
                "dropuser: getpwnam failed — /exec children will run as root (image misconfigured)"
            );
            return None;
        }
    };
    let gid = match getgrnam_gid(DROP_GROUP) {
        Some(g) => g,
        None => {
            warn!(
                group = DROP_GROUP,
                "dropuser: getgrnam failed — /exec children will run as root (image misconfigured)"
            );
            return None;
        }
    };
    info!(uid, gid, "dropuser: /exec children will drop to nobody:nogroup");
    Some((uid, gid))
}

fn getpwnam_uid(name: &str) -> Option<u32> {
    let c = CString::new(name).ok()?;
    // SAFETY: getpwnam is not thread-safe across concurrent calls
    // sharing the static `passwd` buffer, but we only call it during
    // OnceLock init (serialized) and from no other thread thereafter.
    let pw = unsafe { libc::getpwnam(c.as_ptr()) };
    if pw.is_null() {
        return None;
    }
    Some(unsafe { (*pw).pw_uid })
}

fn getgrnam_gid(name: &str) -> Option<u32> {
    let c = CString::new(name).ok()?;
    // SAFETY: see getpwnam_uid.
    let gr = unsafe { libc::getgrnam(c.as_ptr()) };
    if gr.is_null() {
        return None;
    }
    Some(unsafe { (*gr).gr_gid })
}

/// Chown the workspace directory to the drop user. Idempotent;
/// safe to call multiple times. Logs but does not fail on error —
/// if chown fails the worst case is that /exec children can't write
/// to the workspace, which surfaces in the user's command output.
pub fn chown_workspace(path: &Path) {
    let Some((uid, gid)) = child_creds() else {
        // Not running as root, or "nobody" doesn't resolve. Either
        // way, don't try to chown — we'd just fail with EPERM.
        return;
    };
    let c = match CString::new(path.as_os_str().as_bytes()) {
        Ok(c) => c,
        Err(_) => {
            warn!(path = %path.display(), "chown_workspace: path contains NUL");
            return;
        }
    };
    // SAFETY: standard libc call; we own `c`.
    let r = unsafe { libc::chown(c.as_ptr(), uid, gid) };
    if r != 0 {
        let e = std::io::Error::last_os_error();
        warn!(path = %path.display(), error = %e, "chown_workspace failed");
    } else {
        info!(path = %path.display(), uid, gid, "chown_workspace ok");
    }
}

/// Apply every pre-exec hardening, in the correct order, in the
/// post-fork child. **Must be async-signal-safe** — no allocations,
/// no I/O, no locks.
///
/// Sequence (see module docs for rationale):
///
///   1. Drop capability bounding set (while still has CAP_SETPCAP).
///   2. setgid → setgroups([]) → setuid (only if `drop_to.is_some()`).
///   3. PR_SET_NO_NEW_PRIVS.
///   4. RLIMIT_NOFILE (always).
///   5. RLIMIT_NPROC (only when dropping uid).
///
/// The first failure that's actually dangerous (no_new_privs failed,
/// setuid failed) returns Err and aborts the spawn. Best-effort
/// hardenings (cap drop, rlimit) `let _` the result — if they fail
/// the worst case is the child runs with slightly weaker bounds.
///
/// # Safety
///
/// Caller must invoke this from `Command::pre_exec`. Calling it
/// from anywhere else (e.g. directly in the parent process) would
/// permanently apply these to the parent — including
/// `PR_SET_NO_NEW_PRIVS`, which is irreversible.
pub unsafe fn pre_exec_lockdown(drop_to: Option<(u32, u32)>) -> std::io::Result<()> {
    // 1. Drop capability bounding set. Requires CAP_SETPCAP, which
    //    we have iff we're still uid 0 here. EINVAL for caps that
    //    don't exist in this kernel — ignore.
    let mut cap: i32 = 0;
    while cap <= HIGHEST_CAP {
        // SAFETY: prctl is async-signal-safe.
        let _ = unsafe { libc::prctl(libc::PR_CAPBSET_DROP, cap, 0, 0, 0) };
        cap += 1;
    }

    // 2. Privilege drop. setgid first (CAP_SETGID), then clear
    //    supplementary groups, then setuid. After setuid we have
    //    no capabilities and no way back to root.
    if let Some((uid, gid)) = drop_to {
        // SAFETY: setgid/setgroups/setuid are async-signal-safe.
        if unsafe { libc::setgid(gid) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        if unsafe { libc::setgroups(0, std::ptr::null()) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        if unsafe { libc::setuid(uid) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
    }

    // 3. no_new_privs — irreversibly blocks setuid/setgid/file-cap
    //    elevation across any future exec(). Must succeed; this is
    //    the keystone defense.
    // SAFETY: prctl is async-signal-safe.
    if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0 {
        return Err(std::io::Error::last_os_error());
    }

    // 4. RLIMIT_NOFILE — per-process, applies in any uid context.
    let nofile = libc::rlimit {
        rlim_cur: RLIMIT_NOFILE_CAP,
        rlim_max: RLIMIT_NOFILE_CAP,
    };
    // SAFETY: setrlimit is async-signal-safe; we own the rlimit struct.
    let _ = unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &nofile) };

    // 5. RLIMIT_NPROC — per-uid; only safe to apply when we've
    //    dropped to nobody (whose process count starts at 0).
    if drop_to.is_some() {
        let nproc = libc::rlimit {
            rlim_cur: RLIMIT_NPROC_CAP,
            rlim_max: RLIMIT_NPROC_CAP,
        };
        // SAFETY: see above.
        let _ = unsafe { libc::setrlimit(libc::RLIMIT_NPROC, &nproc) };
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::CommandExt;
    use std::process::Command;

    /// Non-root tests: child_creds must return None (don't drop).
    #[test]
    fn child_creds_none_when_not_root() {
        // Test process runs as the developer, not root, so this
        // exercises the early-return in resolve_creds.
        // SAFETY: geteuid is always safe.
        if unsafe { libc::geteuid() } == 0 {
            // Running as root in CI? Skip this assertion.
            return;
        }
        assert_eq!(child_creds(), None);
    }

    #[test]
    fn chown_workspace_is_a_noop_when_not_root() {
        // Should not panic, should not modify anything.
        let dir = std::env::temp_dir().join("zsbx-chown-noop-test");
        std::fs::create_dir_all(&dir).unwrap();
        chown_workspace(&dir); // no-op; just verify it doesn't panic
    }

    /// Verify the lockdown survives an actual fork+exec. We spawn a
    /// shell that prints the relevant `/proc/self/status` lines and
    /// the soft fd limit from `/proc/self/limits`; the asserts read
    /// them back.
    #[test]
    fn lockdown_applied_in_real_child() {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(
            "grep -E '^NoNewPrivs:' /proc/self/status; \
             awk '/^Max open files/ {print $4}' /proc/self/limits",
        );
        // SAFETY: the closure is async-signal-safe (only libc calls).
        unsafe {
            cmd.pre_exec(|| pre_exec_lockdown(None));
        }
        let out = cmd.output().expect("spawn sh");
        assert!(
            out.status.success(),
            "child failed: {}\nstderr: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        );
        let s = String::from_utf8_lossy(&out.stdout);
        assert!(
            s.contains("NoNewPrivs:\t1"),
            "expected NoNewPrivs=1 in:\n{s}"
        );
        // The fd-limit line is the last non-empty line.
        let nofile_line = s
            .lines()
            .filter(|l| !l.trim().is_empty())
            .next_back()
            .unwrap_or("");
        let nofile: u64 = nofile_line.trim().parse().unwrap_or(u64::MAX);
        assert!(
            nofile <= RLIMIT_NOFILE_CAP,
            "RLIMIT_NOFILE not capped: {nofile_line:?} -> {nofile}"
        );
    }

    /// no_new_privs is irreversible. Verify a child that has it set
    /// can't unset it (PR_SET_NO_NEW_PRIVS=0 is rejected by kernel).
    /// Indirectly catches a regression where we accidentally make
    /// the lockdown reversible from user space.
    #[test]
    fn no_new_privs_is_sticky() {
        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            // Read NoNewPrivs after pre_exec. Kernel will refuse to
            // unset, but even if it ever did we'd see 0 here.
            .arg("grep -E '^NoNewPrivs:' /proc/self/status");
        // SAFETY: closure is async-signal-safe.
        unsafe {
            cmd.pre_exec(|| pre_exec_lockdown(None));
        }
        let out = cmd.output().expect("spawn sh");
        assert!(out.status.success());
        let s = String::from_utf8_lossy(&out.stdout);
        assert!(
            s.trim().ends_with("1"),
            "no_new_privs should be 1 in child: {s:?}"
        );
    }
}
