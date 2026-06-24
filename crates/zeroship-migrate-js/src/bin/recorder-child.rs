//! The PR4a §8.9 kernel-sandboxed recorder CHILD.
//!
//! Spawned fresh per `record` invocation by `recorder_service` (one child per
//! tenant invocation — no pooling, the build-time analogue of one-isolate-per-app).
//! This process:
//!
//!   1. (pre-exec, done by the parent's `Command`) entered a fresh USER+NETWORK
//!      namespace + set `RLIMIT_CPU`/`RLIMIT_AS`/`PR_SET_NO_NEW_PRIVS`.
//!   2. reads the recorder request (migration `.ts` source, owner-app hint, name,
//!      posture, allow-read paths) as a JSON envelope on **stdin**.
//!   3. initializes V8 and loads ONLY the trusted recorder glue + `@zeroship/migrate`
//!      DSL (in-memory module strings — no fs, no network).
//!   4. applies the in-process kernel sandbox: landlock read-only ruleset + the
//!      seccomp-bpf default-deny filter. AFTER this point any `socket`/`connect`/
//!      `execve`/`fork`/write/out-of-dir-read attempt is killed by the kernel
//!      (`SIGSYS` / `EACCES`).
//!   5. loads + evaluates the UNTRUSTED migration module and runs `up()`, draining
//!      the recorded ops.
//!   6. emits a JSON result envelope on **stdout**: `{ ok, ir?, error?, report }`.
//!
//! The lockdown is irreversible and per-process, which is exactly why this is a
//! SEPARATE child binary and never an in-process call: a fresh child per invocation
//! gives per-tenant isolation and a clean lockdown each time.

#![allow(unsafe_code)]

use std::io::Read;
use std::path::PathBuf;

use zeroship_migrate_js::recorder_protocol::{ChildRequest, ChildResponse};
use zeroship_migrate_js::sandbox::{apply_landlock, apply_seccomp, SandboxPosture, SandboxReport};

use zeroship_runtime::{ModuleEntry, Runtime};

const OP_RECORDER_JS: &str = include_str!("../op_recorder.js");
const MIGRATE_OPS_JS: &str = include_str!("../migrate_ops.js");

fn main() {
    // Read the request envelope from stdin (the migration source never touches the
    // filesystem — it is piped in-memory, so landlock can deny ALL fs reads outside
    // the explicit allow-list without starving the recorder).
    let mut buf = String::new();
    if let Err(e) = std::io::stdin().read_to_string(&mut buf) {
        emit_fatal(&format!("recorder child: read stdin failed: {e}"));
        return;
    }
    let req: ChildRequest = match serde_json::from_str(&buf) {
        Ok(r) => r,
        Err(e) => {
            emit_fatal(&format!("recorder child: bad request envelope: {e}"));
            return;
        }
    };

    match run(req) {
        Ok(resp) => emit(&resp),
        Err(resp) => emit(&resp),
    }
}

fn run(req: ChildRequest) -> Result<ChildResponse, ChildResponse> {
    let posture = if req.hosted {
        SandboxPosture::Hosted
    } else {
        SandboxPosture::Local
    };

    zeroship_runtime::init_v8();

    let mut report = SandboxReport {
        // netns + rlimits are applied by the parent's pre_exec; the parent passes
        // through whether they engaged (it cannot observe the post-exec state, so it
        // records what it requested + the child confirms via /proc where it can).
        netns: req.netns_engaged,
        rlimit_cpu: req.rlimit_engaged,
        rlimit_as: req.rlimit_engaged,
        resolver: true, // the module-allow-list resolver is ALWAYS the inner fs boundary
        seccomp: false,
        landlock: false,
    };

    // Confirm the netns actually took (the child can read its own net namespace).
    // If the parent requested it but it did not engage, correct the report so the
    // hosted floor check is honest.
    report.netns = report.netns && netns_is_isolated();

    let allow_read: Vec<PathBuf> = req.allow_read_paths.iter().map(PathBuf::from).collect();

    // ---- Build the runtime (pre-untrusted-eval, pre-lockdown) ----
    // `Runtime::builder().build()` + `init_v8()` have spun V8's platform threads;
    // the seccomp allow-list below covers V8's steady-state compute syscalls
    // (mmap/mprotect/futex/clone-for-threads). The untrusted module is NOT loaded
    // until AFTER the sandbox is applied.
    //
    // The V8 HEAP LIMIT is the authoritative memory bound (RLIMIT_AS is too coarse
    // for V8's huge sparse virtual reservation): V8 enforces it and the runtime's
    // near-heap-limit callback fires `terminate_execution` on sustained overrun, so
    // an allocation bomb in `up()` is contained at the JS-allocation boundary.
    let heap_mb = if req.heap_limit_mb == 0 { 256 } else { req.heap_limit_mb };
    let runtime = Runtime::builder().heap_limit_mb(heap_mb).build();

    // ---- Apply the in-process kernel sandbox BEFORE the untrusted eval ----
    // landlock first (read-only fs), then seccomp (default-deny). Order matters:
    // landlock's setup touches the fs (PathFd::new) which seccomp's allow-list also
    // permits, but doing landlock first keeps the seccomp filter the LAST thing
    // installed so nothing after it needs a denied syscall.
    match apply_landlock(&allow_read) {
        Ok(engaged) => report.landlock = engaged,
        Err(e) => {
            // landlock-less host => degraded floor (resolver is the fs boundary).
            // Not fatal; record not-engaged.
            tracing::debug!("landlock not enforced: {e}");
            report.landlock = false;
        }
    }
    // `ZS_RECORDER_DISABLE_SECCOMP` is a TEST-ONLY seam to exercise the
    // refuse-to-run / degraded-floor paths (we cannot un-present the kernel feature
    // from a test). In production seccomp is always attempted.
    let disable_seccomp = std::env::var_os("ZS_RECORDER_DISABLE_SECCOMP").is_some();
    if std::env::var_os("ZS_RECORDER_SECCOMP_TRAP").is_some() {
        install_sigsys_logger();
    }
    if disable_seccomp {
        report.seccomp = false;
    } else {
        match apply_seccomp() {
            Ok(()) => report.seccomp = true,
            Err(e) => {
                tracing::debug!("seccomp not installed: {e}");
                report.seccomp = false;
            }
        }
    }

    // ---- HOSTED refuse-to-run floor (design §8.9) ----
    // With neither seccomp nor netns engaged, a hosted recorder MUST abort rather
    // than evaluate untrusted JS unconstrained. There is no unconstrained-Node
    // fallback. (Local posture runs the developer's own code under the floor.)
    if posture == SandboxPosture::Hosted {
        if let Err(reason) = report.require_hosted_floor() {
            return Err(ChildResponse::refused(reason, report));
        }
    }

    // ---- FAITHFUL kernel-layer proof: post-lockdown syscall probe ----
    // The in-process V8 has no Node `fs`/`net`/`child_process`, so untrusted *JS*
    // cannot itself issue a `socket`/`execve`/write-`open` syscall — the JS-level
    // module-allow-list resolver already refuses those imports. To prove the KERNEL
    // layer (not the userland resolver) is what stops a real escape — a native
    // addon, a V8 0-day, an FFI reach to libc that BYPASSES the resolver — this
    // probe issues the dangerous syscall DIRECTLY, from native code, AFTER the
    // sandbox is applied. The faithful e2e drives it via `ZS_RECORDER_PROBE` and
    // asserts the child is killed by the kernel (`SIGSYS` for seccomp-denied
    // socket/execve/clone-fork; `EACCES` for landlock-denied write/out-of-dir-read),
    // observing the CHILD TERMINATION CAUSE — exactly the "kernel fires even when
    // the resolver is bypassed" assertion. This native path NEVER consults the
    // resolver, so a kill here can only come from the kernel.
    if let Ok(probe) = std::env::var("ZS_RECORDER_PROBE") {
        run_syscall_probe(&probe);
        // If the probe did NOT die, it returns — and we report that the kernel layer
        // FAILED to fire (the test asserts this never happens when the layer is on).
        return Err(ChildResponse::eval_error(
            format!("PROBE_NOT_KILLED: syscall probe '{probe}' returned without a kernel kill"),
            report,
        ));
    }

    // ---- Load + evaluate the UNTRUSTED migration module + run up() ----
    let untrusted = vec![
        ModuleEntry {
            specifier: "op_recorder.js".into(),
            source: OP_RECORDER_JS.to_string(),
        },
        ModuleEntry {
            specifier: "__migration__.js".into(),
            source: req.ts_source.clone(),
        },
        // Re-list the DSL so the entry-module import resolves in this load batch.
        ModuleEntry {
            specifier: "@zeroship/migrate".into(),
            source: MIGRATE_OPS_JS.to_string(),
        },
    ];

    let ir_json: Result<String, String> = runtime.with_scope(|scope| {
        zeroship_runtime::init::setup_globals(scope);
        zeroship_runtime::init::install_text_encoding_streams(scope);

        {
            let global = scope.get_current_context().global(scope);
            for (key, val) in [
                ("__zsOwnerApp", req.owner_app.as_str()),
                ("__zsMigrationName", req.name.as_str()),
            ] {
                let k = v8::String::new(scope, key).unwrap();
                let v = v8::String::new(scope, val).unwrap();
                global.set(scope, k.into(), v.into());
            }
        }

        zeroship_runtime::modules::load_modules(scope, &untrusted)?;
        scope.perform_microtask_checkpoint();

        let global = scope.get_current_context().global(scope);
        let k = v8::String::new(scope, "__zsOpIR").unwrap();
        let v = global
            .get(scope, k.into())
            .filter(|v| v.is_string())
            .ok_or_else(|| "op recorder produced no IR".to_string())?;
        Ok(v.to_rust_string_lossy(scope))
    });

    let ir_json = match ir_json {
        Ok(s) => s,
        Err(e) => return Err(ChildResponse::eval_error(e, report)),
    };

    Ok(ChildResponse::ok(ir_json, report))
}

/// DEBUG seam (paired with `ZS_RECORDER_SECCOMP_TRAP`): install a SIGSYS handler
/// that prints the denied syscall number to stderr. Used only when tuning the
/// allow-list — never in production (the production default action is KillProcess).
#[allow(unsafe_code)]
fn install_sigsys_logger() {
    extern "C" fn handler(_sig: i32, info: *mut libc::siginfo_t, _ctx: *mut libc::c_void) {
        // siginfo_t::si_syscall is at a fixed offset; read it via the documented
        // si_code/si_syscall fields. libc exposes it on linux as the 7th int after
        // the common header on x86_64; use the raw read for robustness.
        unsafe {
            // si_syscall lives in the _sigsys union; on glibc it is at byte offset
            // 0x10 within siginfo_t for SIGSYS (after si_signo/si_errno/si_code).
            // glibc x86_64 siginfo_t for SIGSYS: si_signo/si_errno/si_code (12B,
            // padded to 16), then _sigsys { void* _call_addr (8B); int _syscall };
            // so _syscall is at byte offset 16 + 8 = 24 (0x18).
            let base = info as *const u8;
            let syscall_nr = std::ptr::read_unaligned(base.add(0x18) as *const i32);
            let msg = format!("SECCOMP_TRAP denied syscall nr={syscall_nr}\n");
            libc::write(2, msg.as_ptr() as *const libc::c_void, msg.len());
            libc::_exit(99);
        }
    }
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = handler as *const () as usize;
        sa.sa_flags = libc::SA_SIGINFO;
        libc::sigaction(libc::SIGSYS, &sa, std::ptr::null_mut());
    }
}

/// Issue a dangerous syscall DIRECTLY from native code, post-lockdown, to prove the
/// kernel layer (not the userland resolver) is what stops it. If the kernel kills us
/// (SIGSYS from seccomp / the call simply cannot succeed under netns), we never
/// return; the test observes the termination signal. For landlock cases the call
/// returns an `EACCES` errno (no signal) which we surface by EXITING with a sentinel
/// code the test asserts.
///
/// This path is reached ONLY when `ZS_RECORDER_PROBE` is set — a test-only seam. It
/// is on the SAME post-sandbox code path the untrusted eval runs on, so it faithfully
/// reflects what the kernel allows a (hypothetically resolver-bypassing) escape to do.
#[allow(unsafe_code)]
fn run_syscall_probe(which: &str) {
    match which {
        // --- seccomp: socket() should be SIGSYS-killed (not on the allow-list) ---
        "socket" => {
            // AF_INET stream socket — the first step of any outbound connect.
            let _ = unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0) };
        }
        // --- seccomp: connect() (build a socket via syscall to reach it) ---
        "connect" => {
            let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0) };
            // If socket itself was killed we never get here; if (somehow) it
            // returned, attempt connect — also off the allow-list.
            let addr: libc::sockaddr_in = unsafe { std::mem::zeroed() };
            let _ = unsafe {
                libc::connect(
                    fd,
                    &addr as *const _ as *const libc::sockaddr,
                    std::mem::size_of::<libc::sockaddr_in>() as u32,
                )
            };
        }
        // --- seccomp: fork()+execve() — child_process.spawn's kernel primitive ---
        "execve" => {
            // execve is OFF the allow-list -> SIGSYS. We call it directly (no fork
            // needed: execve in-place is what a spawn ultimately does).
            let path = b"/bin/sh\0";
            let argv: [*const libc::c_char; 2] =
                [path.as_ptr() as *const libc::c_char, std::ptr::null()];
            let envp: [*const libc::c_char; 1] = [std::ptr::null()];
            let _ = unsafe {
                libc::execve(
                    path.as_ptr() as *const libc::c_char,
                    argv.as_ptr(),
                    envp.as_ptr(),
                )
            };
        }
        // --- seccomp: fork()/clone-for-process — also off the allow-list ---
        "fork" => {
            let _ = unsafe { libc::fork() };
        }
        // --- seccomp: io_uring_setup — the known seccomp-BYPASS vector. io_uring
        //     lets a process submit network/fs ops via a ring, sidestepping the
        //     per-syscall filter. It is NOT on the allow-list, so the setup syscall
        //     itself is SIGSYS-killed before any ring can be created. This probe
        //     proves the bypass surface is closed. (SYS_io_uring_setup = 425.)
        "io_uring" => {
            // params struct (zeroed) + 1 entry. The raw syscall — glibc has no
            // wrapper. nr 425 on x86_64 / aarch64.
            let mut params: [u8; 120] = [0; 120]; // sizeof(io_uring_params)
            let _ = unsafe {
                libc::syscall(
                    libc::SYS_io_uring_setup,
                    1u64,
                    params.as_mut_ptr() as *mut libc::c_void,
                )
            };
        }
        // --- landlock: open a file for WRITE outside the (empty) allow-list ---
        // Writes are denied by landlock (EACCES), NOT seccomp (openat is allowed for
        // reads). We exit with sentinel 42 iff the write open was denied.
        "write_open" => {
            let path = b"/tmp/zs_recorder_probe_should_not_exist\0";
            let fd = unsafe {
                libc::open(
                    path.as_ptr() as *const libc::c_char,
                    libc::O_WRONLY | libc::O_CREAT,
                    0o600,
                )
            };
            if fd < 0 {
                // EACCES (landlock) or other denial -> the fs boundary fired.
                std::process::exit(42);
            } else {
                // The write succeeded -> the fs boundary did NOT fire. Clean up and
                // exit 0 (test asserts this never happens when landlock is on).
                unsafe {
                    libc::close(fd);
                    libc::unlink(path.as_ptr() as *const libc::c_char);
                }
                std::process::exit(0);
            }
        }
        // --- landlock: read a file OUTSIDE the allow-list ---
        "read_outside" => {
            let path = b"/etc/hostname\0";
            let fd = unsafe {
                libc::open(path.as_ptr() as *const libc::c_char, libc::O_RDONLY, 0)
            };
            if fd < 0 {
                std::process::exit(42); // landlock denied the out-of-dir read
            } else {
                unsafe { libc::close(fd) };
                std::process::exit(0); // read allowed (landlock off / path allow-listed)
            }
        }
        _ => {
            std::process::exit(7); // unknown probe
        }
    }
}

/// Check that the child is in a fresh, interface-less network namespace.
///
/// We read **`/proc/net/dev`** (NOT `/sys/class/net`): `/proc/net/dev` is
/// netns-scoped and lists only the interfaces in the CURRENT namespace, whereas
/// sysfs reflects the host's netns unless remounted. In a fresh `CLONE_NEWNET`
/// namespace `/proc/net/dev` shows exactly one interface — `lo` (DOWN, no routes).
/// The host shows `eth*`/`docker0`/`veth*`/etc. Read happens early in `run()`,
/// before landlock/seccomp narrow us. If the read fails, we conservatively report
/// NOT isolated (so the hosted floor leans on seccomp rather than over-claiming).
fn netns_is_isolated() -> bool {
    match std::fs::read_to_string("/proc/net/dev") {
        Ok(contents) => {
            // The first two lines are headers; each subsequent line is
            // "  iface: <stats…>". Collect the interface names.
            let ifaces: Vec<String> = contents
                .lines()
                .skip(2)
                .filter_map(|l| l.split(':').next())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            // A fresh netns has only `lo`.
            ifaces.len() == 1 && ifaces[0] == "lo"
        }
        Err(_) => false,
    }
}

fn emit(resp: &ChildResponse) {
    // The result envelope on stdout is the ONLY thing the parent parses. A write
    // here is on the seccomp allow-list (SYS_write) and not blocked by landlock
    // (stdout is an inherited fd, not a path open).
    match serde_json::to_string(resp) {
        Ok(s) => println!("{s}"),
        Err(e) => eprintln!("recorder child: serialize response failed: {e}"),
    }
}

fn emit_fatal(msg: &str) {
    let resp = ChildResponse::eval_error(msg.to_string(), SandboxReport::default());
    emit(&resp);
}
