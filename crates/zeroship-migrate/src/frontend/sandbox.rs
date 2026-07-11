//! The PR4a §8.9 build-time kernel sandbox — OS-level isolation for the
//! UNTRUSTED creator/AI migration `.ts` the recorder evaluates.
//!
//! This module is the build-time analogue of the apply-time least-priv
//! `migrator_<project>` role: the kernel — not a userland convention — denies the
//! recorder child the network, the filesystem (read-only to a tight allow-list,
//! never writable), subprocess spawning, and an unbounded CPU/memory budget. It is
//! applied INSIDE the recorder child (`src/bin/recorder-child.rs`), AFTER V8 has
//! initialized but BEFORE any untrusted JS runs (V8 init needs `mmap`/`mprotect`/
//! thread-create syscalls a post-lockdown default-deny filter forbids; the untrusted
//! `up()` body does not).
//!
//! ## Thread scope (CRITICAL — PR4a code-critic #1)
//!
//! seccomp's `apply_filter` (flags=0) and landlock's `restrict_self` BOTH bind the
//! CALLING THREAD only. If V8's multi-threaded default platform had already spun up its
//! GC/compiler/platform worker pool when the lockdown ran, those PRE-EXISTING threads
//! would stay UNFILTERED at the kernel level — a resolver-bypassing escape scheduled
//! onto one could `socket()`/`fork()`/write freely. We close this two ways,
//! defense-in-depth: the recorder child initializes V8 with a SINGLE-THREADED platform
//! ([`init_recorder_v8`] → `RecorderPlatform::init_single_threaded`) so NO background
//! worker thread exists, AND [`apply_seccomp`] installs the BPF filter via
//! `apply_filter_all_threads` (TSYNC) so it synchronizes across every thread that does
//! exist. landlock has no TSYNC equivalent, so the single-threaded platform is what
//! gives IT full coverage.
//!
//! ## The ruleset (design §8.9)
//!
//! Full kernel baseline = **seccomp-bpf default-deny + landlock read-only + an
//! interface-less network namespace**, plus the **`RLIMIT_CPU`/`RLIMIT_AS` + a
//! wall-clock watchdog** resource budget. `landlock` needs Linux ≥ 5.13.
//!
//! - **netns**: applied at child-spawn time via `unshare(CLONE_NEWUSER|CLONE_NEWNET)`
//!   in the child's `pre_exec` (see [`pre_exec_netns`]) — a fresh network namespace
//!   with **no interfaces and no routes**, so even a syscall that slips past seccomp
//!   reaches no network. This is the apply order: namespaces first (at clone), then
//!   the in-process seccomp+landlock+rlimits.
//! - **seccomp**: a default-deny (`SIGSYS`-kill) BPF filter with an explicit
//!   allow-list of the compute syscalls V8 needs post-init; `socket`/`connect`/
//!   `execve`/`fork`/`clone`(process)/`ptrace`/write-`openat` are NOT on it, so an
//!   attempt is killed by the kernel with `SIGSYS` (the termination cause the
//!   faithful e2e observes). See [`SandboxPosture`] + [`apply_seccomp`].
//! - **landlock**: a read-only ruleset over the migration dir + the schema-types
//!   blob path; ANY write, and any read outside the allow-list, is an `EACCES` the
//!   kernel returns (Linux ≥ 5.13). On a landlock-less host the seccomp `openat`
//!   arg-filter still SIGSYS-kills any WRITE/create open (so fs-WRITE containment
//!   against a resolver-bypassing escape survives the degraded floor, MED #2); only
//!   out-of-allow-list READ restriction is lost, with the userland module-allow-list
//!   resolver as the remaining read boundary.
//! - **rlimits**: `RLIMIT_CPU` (seconds of CPU) + `RLIMIT_AS` (address-space bytes),
//!   applied in `pre_exec`. The wall-clock watchdog is the parent's responsibility
//!   (see `recorder_service`) — it `SIGKILL`s a child that overruns the wall budget.
//!
//! ## Baseline vs degraded floor (design §8.9)
//!
//! [`SandboxReport`] records which layers actually engaged. The HOSTED multi-tenant
//! recorder **refuses to run** ([`SandboxPosture::require_hosted_floor`]) unless
//! Landlock is enforced AND at least seccomp OR netns engaged — there is NO
//! seccomp-only filesystem downgrade for hosted recorder execution. The LOCAL
//! single-tenant recorder runs the developer's own code under the userland-budget
//! floor (rlimits + the resolver) and applies the kernel layers opportunistically.

#![allow(unsafe_code)] // raw syscalls (setrlimit/unshare/prctl), mirroring sandbox-agent::dropuser

use std::path::PathBuf;

/// Initialize V8 for the recorder child with a **single-threaded** platform — the
/// keystone of the CRITICAL thread-scope fix (PR4a code-critic #1).
///
/// The shared multi-threaded `init_v8` (the `AuthoringHost` in-process path) installs a *multi-threaded* default
/// platform (`new_default_platform(0, …)`), which spawns a GC/compiler/platform
/// worker-thread pool the instant it initializes. seccomp's `apply_filter` (flags=0)
/// is calling-thread-only and landlock's `restrict_self` likewise restricts only the
/// calling thread — so any thread that ALREADY EXISTS when the in-process lockdown
/// runs stays UNFILTERED at the kernel level. A resolver-bypassing escape (native
/// addon / V8 0-day / FFI) scheduled onto such a thread could then `socket()` /
/// `fork()` / write the filesystem with no kernel filter — empirically proven in the
/// regression e2e (a pre-lockdown thread's `socket()` succeeded under the old code).
///
/// We close that gap two ways, defense-in-depth:
///   1. a **single-threaded** V8 platform here, so V8 runs GC/compilation INLINE on
///      the calling thread and spawns NO background worker pool — there is no
///      pre-existing unfiltered thread for landlock (which has no TSYNC) to miss; and
///   2. seccomp installed via `apply_filter_all_threads` (TSYNC) in [`apply_seccomp`],
///      so the BPF filter synchronizes across every thread that exists at install
///      time regardless.
///
/// Delegates to [`RecorderPlatform::init_single_threaded`](crate::runtime_host::RecorderPlatform::init_single_threaded),
/// which installs V8's single-threaded platform + flags (one-time, process-global).
/// Used by the recorder child INSTEAD of the shared multi-threaded `init_v8`.
///
/// Extraction Phase A routes the platform init through the `RecorderPlatform` seam
/// (the in-Rust-V8 impl lives in `zeroship-migrate-runtime`); the engine names only
/// the trait.
pub fn init_recorder_v8(platform: &impl crate::runtime_host::RecorderPlatform) {
    platform.init_single_threaded();
}

/// FAIL-CLOSED assertion that the recorder process is genuinely single-threaded BEFORE
/// the in-process lockdown (PR4a code-critic LOW — runtime assertion that the recorder
/// V8 is single-threaded).
///
/// WHY THIS IS LOAD-BEARING: landlock's `restrict_self` has NO TSYNC — it binds only
/// the calling thread — so the recorder's WHOLE-PROCESS landlock coverage depends
/// ENTIRELY on the single-threaded-V8 invariant (only ONE thread exists when
/// `restrict_self` runs). [`init_v8_single_threaded`] and [`init_v8`] share ONE
/// process-global `Once` (first-caller-wins; a later call of EITHER no-ops). If any
/// future code path in the recorder child called the multi-threaded `init_v8` before
/// [`init_recorder_v8`], the recorder would silently come up MULTI-THREADED with NO
/// error, re-opening the landlock thread-scope gap.
///
/// We fail closed two independent ways:
///   1. **Platform flavor**: verify the `Once` committed the SINGLE-THREADED platform
///      (`v8_platform_flavor() == 2`). A `1` means the multi-threaded platform won the
///      race — refuse.
///   2. **Live thread count**: read `/proc/self/task` and require EXACTLY ONE thread.
///      Even a single-threaded V8 platform cannot mask a stray thread spawned by some
///      other init path; this catches it directly.
///
/// Returns `Err(reason)` (the caller refuses to run) rather than silently degrading.
/// `ZS_RECORDER_FORCE_MULTITHREAD` is a TEST-ONLY seam that spawns a parked thread
/// before this check so the regression test can prove the assertion FIRES (we cannot
/// un-commit the single-threaded platform once a prior test installed it process-wide).
pub fn assert_recorder_single_threaded(
    platform: &impl crate::runtime_host::RecorderPlatform,
) -> Result<(), String> {
    // (1) The committed platform flavor must be single-threaded (2), not multi (1).
    // The fail-closed DECISION stays engine-side; the adapter only supplies the raw
    // u8 read (design §2.2).
    let flavor = platform.platform_flavor();
    if flavor == 1 {
        return Err(format!(
            "recorder refuses to run: the MULTI-THREADED V8 platform was installed \
             (v8_platform_flavor={flavor}); landlock's restrict_self has no TSYNC, so a \
             multi-threaded recorder re-opens the thread-scope gap. init_recorder_v8() \
             must win the V8_INIT Once."
        ));
    }
    // flavor == 0 (uninitialized) is also wrong here — init_recorder_v8() must have run.
    if flavor == 0 {
        return Err(
            "recorder refuses to run: V8 platform not initialized before the \
             single-threaded assertion (init_recorder_v8() must run first)"
                .to_string(),
        );
    }

    // (2) Exactly one OS thread must exist. /proc/self/task has one entry per thread.
    let n = count_self_threads()?;
    if n != 1 {
        return Err(format!(
            "recorder refuses to run: {n} threads exist before lockdown, but landlock \
             restrict_self covers only the calling thread (no TSYNC) — the recorder MUST \
             be single-threaded so every thread is under landlock. A background thread \
             would stay UNFILTERED for out-of-dir reads (thread-scope gap)."
        ));
    }
    Ok(())
}

/// Count this process's OS threads via `/proc/self/task` (one dir entry per thread).
fn count_self_threads() -> Result<usize, String> {
    let mut n = 0usize;
    let entries = std::fs::read_dir("/proc/self/task")
        .map_err(|e| format!("recorder single-threaded check: read /proc/self/task: {e}"))?;
    for e in entries {
        e.map_err(|e| format!("recorder single-threaded check: task entry: {e}"))?;
        n += 1;
    }
    Ok(n)
}

/// Which trust posture the recorder runs under (design §8.9 / §8.9.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxPosture {
    /// HOSTED multi-tenant: the recorder evaluates ONE tenant's `.ts` on shared
    /// platform infrastructure. The kernel sandbox is MANDATORY — the child refuses
    /// to run unless landlock is enforced and at least one execution/network
    /// boundary (seccomp or netns) engaged. A landlock-less hosted platform refuses
    /// rather than degrading to seccomp-only, because seccomp/netns do not cover
    /// read-only filesystem secrets.
    Hosted,
    /// LOCAL single-tenant / self-host: the developer's OWN `.ts` at their own trust
    /// level. The userland-budget floor (rlimits + resolver) is the baseline; the
    /// kernel layers (seccomp/landlock/netns) are applied OPPORTUNISTICALLY (free
    /// hardening) and their absence does NOT refuse the run.
    Local,
}

/// The kernel/userland layers that ACTUALLY engaged for one recorder child — the
/// honest baseline-vs-degraded-floor record (design §8.9). Emitted by the child on
/// its stderr-channel and asserted by the faithful e2e (which layer fired).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default, PartialEq, Eq)]
pub struct SandboxReport {
    /// The interface-less network namespace engaged (`unshare(CLONE_NEWNET)`).
    pub netns: bool,
    /// The seccomp-bpf default-deny filter installed.
    pub seccomp: bool,
    /// The landlock read-only ruleset enforced (Linux ≥ 5.13 + landlock LSM on).
    pub landlock: bool,
    /// `RLIMIT_CPU` engaged.
    pub rlimit_cpu: bool,
    /// `RLIMIT_AS` engaged.
    pub rlimit_as: bool,
    /// The userland module-allow-list resolver is the fs boundary (always true —
    /// defense-in-depth behind landlock, and the SOLE fs boundary on the degraded
    /// floor).
    pub resolver: bool,
}

impl SandboxReport {
    /// The hosted multi-tenant refuse-to-run floor (design §8.9): landlock must be
    /// enforced, and at least seccomp OR netns must have engaged. Returns
    /// `Err(reason)` if the floor is not met — the recorder MUST abort rather than
    /// evaluate untrusted JS with a read-only host filesystem exposure.
    pub fn require_hosted_floor(&self) -> Result<(), String> {
        if !self.landlock {
            return Err(
                "hosted recorder refuses to run: landlock is not enforced \
                 (hosted requires an active Landlock filesystem boundary; no \
                 seccomp-only filesystem downgrade)"
                    .into(),
            );
        }
        if self.seccomp || self.netns {
            Ok(())
        } else {
            Err("hosted recorder refuses to run: neither seccomp nor netns engaged \
                 (no unconstrained-Node fallback, design §8.9)"
                .into())
        }
    }
}

/// The resource budget the recorder child runs under (design §8.9). The defaults
/// are conservative build-time bounds; the parent service may tighten them.
#[derive(Debug, Clone, Copy)]
pub struct ResourceBudget {
    /// `RLIMIT_CPU` hard cap in CPU-seconds. A build-time infinite loop is killed
    /// with `SIGXCPU`/`SIGKILL` once it burns this many CPU-seconds.
    pub cpu_seconds: u64,
    /// `RLIMIT_AS` hard cap in bytes (address-space) — a COARSE outer cap for native
    /// allocations. It must be set ABOVE V8's enormous virtual reservation: V8's
    /// Oilpan caged-heap + pointer-compression cage reserve ~tens of GiB of *sparse
    /// virtual* address space at init that is never RSS-backed, so RLIMIT_AS is
    /// useless as a tight PHYSICAL-memory bound for a V8 process. The real memory
    /// bound is [`Self::heap_limit_mb`] (the in-VM V8 heap cap, RSS-correlated).
    pub address_space_bytes: u64,
    /// The V8 heap cap in MiB — the AUTHORITATIVE memory bound for an alloc bomb.
    /// V8 enforces it and fires `terminate_execution` on sustained overrun (the
    /// runtime's `near_heap_limit_callback`), so a build-time allocation bomb is
    /// contained at the JS-allocation boundary (a terminated/`RangeError` eval)
    /// rather than via the V8-incompatible `RLIMIT_AS`.
    pub heap_limit_mb: u32,
    /// The parent's wall-clock watchdog cap, in milliseconds. The PARENT `SIGKILL`s
    /// the child if it does not exit within this wall time (covers a child blocked
    /// on something that burns wall- but not CPU-time).
    pub wall_ms: u64,
}

impl Default for ResourceBudget {
    fn default() -> Self {
        // V8 init + a recording pass is cheap; these bounds are generous enough not
        // to false-positive a legitimate large migration but tight enough that a
        // loop/alloc-bomb is killed quickly.
        ResourceBudget {
            cpu_seconds: 10,
            // 96 GiB virtual — ABOVE V8's ~64 GiB caged-heap reservation so init
            // never fails; the real memory bound is `heap_limit_mb` below.
            address_space_bytes: 96 * 1024 * 1024 * 1024,
            heap_limit_mb: 256, // a recording pass needs little heap; an alloc bomb trips this
            wall_ms: 15_000,
        }
    }
}

/// `pre_exec` hook: enter a fresh USER + NETWORK namespace (design §8.9 netns layer).
///
/// Runs in the forked child BEFORE exec. We unshare a new **user** namespace (so an
/// unprivileged process may create the net namespace) and a new **network**
/// namespace with no interfaces — no `lo` brought up, no routes — so any socket the
/// child manages to open reaches nowhere. Async-signal-safe (one `unshare`).
///
/// On failure (e.g. `unprivileged_userns_clone=0`), returns the OS error so the
/// caller can record netns as not-engaged; the hosted floor still requires
/// Landlock and then leans on seccomp.
///
/// # Safety
/// MUST be called only from `Command::pre_exec` (in the forked child). Calling it in
/// the parent would move the PARENT into a new netns.
pub unsafe fn pre_exec_netns() -> std::io::Result<()> {
    // CLONE_NEWUSER first (grants the capability to create other namespaces
    // unprivileged), then CLONE_NEWNET. A single unshare with both flags is atomic
    // and async-signal-safe.
    let flags = libc::CLONE_NEWUSER | libc::CLONE_NEWNET;
    if unsafe { libc::unshare(flags) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// `pre_exec` hook: apply the `RLIMIT_CPU` + `RLIMIT_AS` resource budget + the
/// irreversible `PR_SET_NO_NEW_PRIVS` keystone (design §8.9 rlimits layer).
///
/// `PR_SET_NO_NEW_PRIVS` is required before installing a non-privileged seccomp
/// filter (the kernel refuses `seccomp(2)` without it unless `CAP_SYS_ADMIN`), and
/// irreversibly blocks setuid/file-cap elevation across exec. Async-signal-safe.
///
/// # Safety
/// MUST be called only from `Command::pre_exec`.
pub unsafe fn pre_exec_rlimits(budget: ResourceBudget) -> std::io::Result<()> {
    // PR_SET_NO_NEW_PRIVS — keystone + seccomp precondition. Must succeed.
    if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // RLIMIT_CPU — CPU-seconds cap. The kernel sends SIGXCPU at the SOFT limit and a
    // bare SIGKILL at the HARD limit. We deliberately set the hard limit ONE SECOND
    // ABOVE the soft so SIGXCPU (default action: terminate) fires FIRST and is the
    // terminating signal — making a genuine cpu-budget overrun observably `SIGXCPU`
    // (classified as the cpu budget, a non-retryable 422) rather than an ambiguous bare
    // SIGKILL. A bare SIGKILL is then reserved for true environment events (operator
    // kill / unrelated-cgroup OOM / node shutdown), which classify as the RETRYABLE
    // environment-event outcome (PR4a code-critic LOW — bare SIGKILL is an environment
    // event, not an authoring reject). The +1s hard headroom is a backstop: if the
    // process somehow ignored SIGXCPU, the hard SIGKILL still bounds it.
    let cpu = libc::rlimit {
        rlim_cur: budget.cpu_seconds,
        rlim_max: budget.cpu_seconds.saturating_add(1),
    };
    if unsafe { libc::setrlimit(libc::RLIMIT_CPU, &cpu) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // RLIMIT_AS — address-space hard cap. An allocation bomb hits this.
    let as_ = libc::rlimit {
        rlim_cur: budget.address_space_bytes,
        rlim_max: budget.address_space_bytes,
    };
    if unsafe { libc::setrlimit(libc::RLIMIT_AS, &as_) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// The Landlock read-only filesystem ruleset (design §8.9). Engaged in the child
/// AFTER V8 init, BEFORE untrusted eval. Read-only access is granted ONLY to
/// `allow_read` paths; ANY write, and any read outside them, is an `EACCES` the
/// kernel returns. Requires Linux ≥ 5.13 (landlock ABI v1+).
///
/// Returns `Ok(true)` if the ruleset was enforced, `Ok(false)` if landlock is
/// unavailable on this kernel (the degraded floor — the resolver is then the fs
/// boundary). `Err` only on an unexpected enforcement error.
#[cfg(target_os = "linux")]
pub fn apply_landlock(allow_read: &[PathBuf]) -> Result<bool, String> {
    use landlock::{
        Access, AccessFs, PathBeneath, PathFd, RestrictionStatus, Ruleset, RulesetAttr,
        RulesetCreatedAttr, RulesetStatus, ABI,
    };

    #[cfg(debug_assertions)]
    if std::env::var_os("ZS_RECORDER_FORCE_LANDLOCK_UNAVAILABLE").is_some() {
        return Ok(false);
    }

    // ABI::V1 is the floor (Linux 5.13). `from_all` over the running ABI grants the
    // widest read set the kernel knows; we then add ONLY read rules — never a write
    // access — so the ruleset is read-only by construction.
    let abi = ABI::V1;
    let read_only = AccessFs::from_read(abi);

    let mut ruleset = Ruleset::default()
        .handle_access(AccessFs::from_all(abi))
        .map_err(|e| format!("landlock handle_access: {e}"))?
        .create()
        .map_err(|e| format!("landlock create: {e}"))?;

    for path in allow_read {
        // A non-existent allow path is skipped (not fatal): the migration dir always
        // exists, but the schema-types blob may be passed in-memory.
        if let Ok(fd) = PathFd::new(path) {
            ruleset = ruleset
                .add_rule(PathBeneath::new(fd, read_only))
                .map_err(|e| format!("landlock add_rule {}: {e}", path.display()))?;
        }
    }

    let status: RestrictionStatus = ruleset
        .restrict_self()
        .map_err(|e| format!("landlock restrict_self: {e}"))?;

    match status.ruleset {
        // Fully enforced.
        RulesetStatus::FullyEnforced => Ok(true),
        // Partially enforced (a newer ABI subset unavailable) still gives us the
        // read-only fs boundary for the v1 access set — count it engaged.
        RulesetStatus::PartiallyEnforced => Ok(true),
        // The running kernel has no landlock support — degraded floor.
        RulesetStatus::NotEnforced => Ok(false),
    }
}

/// Non-Linux stub: landlock is Linux-only; the resolver is the fs boundary.
#[cfg(not(target_os = "linux"))]
pub fn apply_landlock(_allow_read: &[PathBuf]) -> Result<bool, String> {
    Ok(false)
}

/// Install the seccomp-bpf **default-deny** syscall filter (design §8.9).
///
/// The default action is `KillProcess` (the kernel delivers `SIGSYS` and terminates
/// the process) for any syscall NOT on the compute allow-list. The dangerous
/// syscalls — `socket`, `connect`, `execve`, `execveat`, `fork`, `vfork`,
/// `clone`/`clone3` (process), `ptrace`, `bpf`, `mount`, etc. — are deliberately
/// ABSENT from the allow-list, so an attempt is `SIGSYS`-killed. The allow-list is
/// the syscall set V8 + glibc + the runtime use POST-init (init's heavier set has
/// already run before this is called).
///
/// `KillProcess` is what makes the e2e faithful: a banned syscall does not return an
/// errno the JS could catch — the kernel KILLS the child, and the parent observes
/// `WTERMSIG == SIGSYS`.
///
/// Returns `Ok(())` on install; `Err` if seccomp is unavailable (the caller records
/// it not-engaged; the hosted floor still requires Landlock and then requires netns).
#[cfg(target_os = "linux")]
pub fn apply_seccomp() -> Result<(), String> {
    use seccompiler::{
        BpfProgram, SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition, SeccompFilter,
        SeccompRule, TargetArch,
    };
    use std::collections::BTreeMap;

    #[cfg(target_arch = "x86_64")]
    let arch = TargetArch::x86_64;
    #[cfg(target_arch = "aarch64")]
    let arch = TargetArch::aarch64;
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    return Err("seccomp: unsupported target arch".into());

    // The POST-init compute allow-list. Each entry maps to an empty rule vec =
    // "allow unconditionally". The default action (below) is KillProcess, so any
    // syscall NOT here SIGSYS-kills the child. This list is the syscalls V8 + the
    // compio/runtime + glibc issue while evaluating `up()` and serializing the IR to
    // stdout — established empirically by stracing the child (see
    // tests/recorder_sandbox_e2e.rs::seccomp_allowlist_is_sufficient) and pinned
    // here. CRITICALLY ABSENT: socket/connect/sendto/recvfrom (network),
    // execve/execveat/fork/vfork (subprocess), clone/clone3 with CLONE_VM-for-process
    // are gated by absence of execve anyway, ptrace, bpf, mount. `openat` is NOT
    // unconditional either — it is added below with an arg-filter that permits only
    // READ-only opens, so a WRITE/create open is SIGSYS-killed at the SECCOMP layer
    // (fs-write containment holds even on the landlock-less degraded floor, MED #2).
    let allowed: &[libc::c_long] = &[
        // --- memory ---
        libc::SYS_brk,
        libc::SYS_mmap,
        libc::SYS_mprotect,
        libc::SYS_munmap,
        libc::SYS_mremap,
        libc::SYS_madvise,
        // --- fd / io the runtime + stdout writeback need ---
        libc::SYS_read,
        libc::SYS_write,
        libc::SYS_readv,
        libc::SYS_writev,
        libc::SYS_close,
        libc::SYS_lseek,
        libc::SYS_fcntl,
        libc::SYS_fstat,
        libc::SYS_statx,
        libc::SYS_pread64,
        libc::SYS_pwrite64,
        // NOTE: `openat` is NOT on this unconditional allow-list — it is added below
        // with an ARG FILTER that permits ONLY read-only opens (flags arg with no
        // O_WRONLY/O_RDWR/O_CREAT). A WRITE/create open is SIGSYS-killed at the seccomp
        // layer, so fs-WRITE containment against a resolver-bypassing escape holds even
        // on the DEGRADED FLOOR (landlock-less host) — closing PR4a code-critic MED #2.
        // --- futex / scheduling / signals (V8 GC threads, compio) ---
        libc::SYS_futex,
        libc::SYS_sched_yield,
        libc::SYS_sched_getaffinity,
        // V8's GC/heap-pressure path queries scheduling params (read-only, benign —
        // no capability granted). Surfaced by the alloc-bomb under SECCOMP_TRAP.
        libc::SYS_sched_getparam,
        libc::SYS_sched_getscheduler,
        libc::SYS_sched_get_priority_max,
        libc::SYS_sched_get_priority_min,
        libc::SYS_rt_sigaction,
        libc::SYS_rt_sigprocmask,
        libc::SYS_rt_sigreturn,
        libc::SYS_nanosleep,
        libc::SYS_clock_gettime,
        libc::SYS_clock_nanosleep,
        libc::SYS_gettimeofday,
        libc::SYS_getpid,
        libc::SYS_gettid,
        libc::SYS_getrandom,
        // NOTE: `clone`/`clone3` are NOT on this unconditional allow-list — they are
        // added below with an ARG FILTER so only THREAD creation (CLONE_THREAD set)
        // is permitted; a process `fork()` (clone without CLONE_THREAD) is
        // SIGSYS-killed. `execve`/`execveat`/`fork`/`vfork` are absent entirely.
        libc::SYS_set_robust_list,
        libc::SYS_rseq,
        libc::SYS_prctl,
        libc::SYS_exit,
        libc::SYS_exit_group,
        libc::SYS_epoll_create1,
        libc::SYS_epoll_ctl,
        libc::SYS_epoll_wait,
        libc::SYS_epoll_pwait,
        libc::SYS_eventfd2,
        libc::SYS_pipe2,
        libc::SYS_dup,
        libc::SYS_dup3,
        libc::SYS_poll,
        libc::SYS_ppoll,
        libc::SYS_restart_syscall,
        libc::SYS_membarrier,
        libc::SYS_get_robust_list,
        libc::SYS_sigaltstack,
        libc::SYS_tgkill,
        libc::SYS_rt_sigtimedwait,
    ];

    let mut rules: BTreeMap<libc::c_long, Vec<SeccompRule>> =
        allowed.iter().map(|&s| (s, vec![])).collect();

    // `clone` — allow ONLY thread creation (CLONE_THREAD bit set in flags / arg0).
    // A process `fork()` issues `clone` WITHOUT CLONE_THREAD, so it falls through to
    // the default KillProcess (SIGSYS). V8's pthread_create / GC threads set
    // CLONE_THREAD, so they pass. This is the precise "no process fork, yes threads"
    // posture the design's deny-list intends.
    let clone_thread = libc::CLONE_THREAD as u64;
    rules.insert(
        libc::SYS_clone,
        vec![SeccompRule::new(vec![SeccompCondition::new(
            0, // arg0 = clone flags
            SeccompCmpArgLen::Qword,
            SeccompCmpOp::MaskedEq(clone_thread),
            clone_thread,
        )
        .map_err(|e| format!("seccomp clone condition: {e}"))?])
        .map_err(|e| format!("seccomp clone rule: {e}"))?],
    );

    // `openat` — allow ONLY read-only opens. `openat(dirfd, path, flags, mode)`: flags
    // is arg2. A read-only open has `(flags & (O_ACCMODE | O_CREAT)) == 0`
    // (O_RDONLY == 0, and no create). Any O_WRONLY/O_RDWR/O_CREAT bit makes the masked
    // value non-zero, so the rule does NOT match and the call falls through to the
    // default KillProcess (SIGSYS). This makes a raw WRITE/create open SIGSYS-killed at
    // the seccomp layer — so fs-WRITE containment against a resolver-bypassing native
    // escape holds EVEN on the degraded floor where landlock is absent (PR4a
    // code-critic MED #2). V8/glibc only ever open locale/tz/std fds read-only post
    // init, so legitimate recording is unaffected (the happy-path e2e proves this).
    let write_or_create = (libc::O_ACCMODE | libc::O_CREAT) as u64;
    rules.insert(
        libc::SYS_openat,
        vec![SeccompRule::new(vec![SeccompCondition::new(
            2, // arg2 = open flags
            SeccompCmpArgLen::Qword,
            SeccompCmpOp::MaskedEq(write_or_create),
            0, // (flags & (O_ACCMODE|O_CREAT)) == 0  =>  pure read-only open
        )
        .map_err(|e| format!("seccomp openat condition: {e}"))?])
        .map_err(|e| format!("seccomp openat rule: {e}"))?],
    );

    // DEFAULT-DENY: any syscall not in `rules` -> KillProcess (SIGSYS). The faithful
    // e2e observes WTERMSIG==SIGSYS for socket/connect/execve attempts.
    //
    // `ZS_RECORDER_SECCOMP_TRAP` is a DEBUG seam: it swaps the default action to
    // `Trap` (still SIGSYS, but a userland SIGSYS handler can read `si_syscall` and
    // log which syscall was denied) — used when tuning the allow-list. Never set in
    // production; the production default is the hard `KillProcess`.
    let mismatch = if std::env::var_os("ZS_RECORDER_SECCOMP_TRAP").is_some() {
        SeccompAction::Trap
    } else {
        SeccompAction::KillProcess
    };
    let filter = SeccompFilter::new(
        rules,
        mismatch,             // mismatch_action: default-deny
        SeccompAction::Allow, // match_action: allow the listed syscalls
        arch,
    )
    .map_err(|e| format!("seccomp build: {e}"))?;

    let prog: BpfProgram = filter
        .try_into()
        .map_err(|e| format!("seccomp compile: {e}"))?;
    // `apply_filter_all_threads` issues `seccomp(2)` with `SECCOMP_FILTER_FLAG_TSYNC`,
    // so the BPF filter SYNCHRONIZES across EVERY thread in the process at install
    // time — not just the calling thread (the calling-thread-only `apply_filter`,
    // flags=0, was the CRITICAL PR4a code-critic gap: a V8 background thread that
    // already existed stayed unfiltered and could `socket()`/`fork()`). Combined with
    // the single-threaded V8 platform (`init_recorder_v8`), the recorder has no
    // unfiltered thread for a resolver-bypassing escape to schedule onto: TSYNC covers
    // any thread that does exist, and the single-threaded platform means none do.
    seccompiler::apply_filter_all_threads(&prog)
        .map_err(|e| format!("seccomp apply (TSYNC, all threads): {e}"))?;
    Ok(())
}

/// Non-Linux stub: seccomp is Linux-only.
#[cfg(not(target_os = "linux"))]
pub fn apply_seccomp() -> Result<(), String> {
    Err("seccomp: not supported on this platform".into())
}
