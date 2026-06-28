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
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Condvar, Mutex};

use zeroship_migrate::frontend::embedding::{
    install_frontend_globals, module_graph, read_determinism_probe_used, FrontendGlobals,
    FrontendProgram,
};
use zeroship_migrate::frontend::recorder_protocol::{
    ChildOperation, ChildRequest, ChildResponse, MAX_CHILD_STDIN_BYTES, MAX_TS_SOURCE_BYTES,
};
use zeroship_migrate::frontend::sandbox::{
    apply_landlock, apply_seccomp, assert_recorder_single_threaded, init_recorder_v8,
    SandboxPosture, SandboxReport,
};

use zeroship_runtime::Runtime;

/// A worker thread created BEFORE the in-process lockdown — the test analogue of a
/// V8 background (GC/compiler/platform) worker thread that already exists when
/// `apply_seccomp`/`apply_landlock` run. The CRITICAL PR4a code-critic finding was
/// that a thread-scoped filter (seccomp `apply_filter` flags=0 / landlock
/// `restrict_self`) leaves such a pre-existing thread UNFILTERED, so a
/// resolver-bypassing escape scheduled onto it could `socket()`/`fork()`/write
/// freely. This is reached ONLY under `ZS_RECORDER_PRELOCK_THREAD` (a test-only
/// seam): we spawn a parked thread before lockdown, then `thread_socket` signals it
/// to issue the denied syscall from THAT thread. With the fix (seccomp TSYNC across
/// all threads + a single-threaded V8 platform so no unfiltered background thread
/// exists), the kernel must KILL THE WHOLE PROCESS (`KillProcess`/SIGSYS) — not just
/// return an fd on an unfiltered thread.
struct PrelockThread {
    /// 0 = idle, 1 = "issue socket() now" (seccomp TSYNC probe), 2 = "open
    /// /etc/hostname O_RDONLY now" (landlock thread-scope probe). Set by the main thread
    /// post-lockdown.
    cmd: Mutex<i32>,
    cv: Condvar,
    /// For cmd=1: the fd `socket()` returned (>=0 means it SUCCEEDED on an unfiltered
    /// thread — the seccomp bug). -1 means it failed/never set. If seccomp TSYNC'd onto
    /// this thread, the call is SIGSYS-killed and we never read this.
    result_fd: AtomicI32,
    /// For cmd=2 (landlock read probe): the errno the worker thread's
    /// `open("/etc/hostname", O_RDONLY)` returned. This pins the LANDLOCK half of the
    /// CRITICAL invariant by GROUND-TRUTH observation: landlock's `restrict_self` has NO
    /// TSYNC, so it covers ONLY the calling (main) thread. A thread created BEFORE the
    /// lockdown (this one) is therefore NOT under landlock, and its out-of-dir read
    /// SUCCEEDS (errno 0). That is precisely WHY the landlock layer's whole-process
    /// coverage rests ENTIRELY on the single-threaded-V8 invariant (only the main thread
    /// may exist at `restrict_self` time) PLUS the fail-closed single-threaded assertion
    /// (which REFUSES to run if a pre-lockdown thread is ever present). `i32::MIN` = not
    /// yet set; `EACCES` would mean landlock unexpectedly covered the sibling.
    result_read_errno: AtomicI32,
}

static PRELOCK: Mutex<Option<&'static PrelockThread>> = Mutex::new(None);

/// Spawn the pre-lockdown worker thread (test-only). It parks on the condvar until
/// the main thread (post-lockdown) signals it to issue a denied syscall:
///   cmd=1 → `socket()` (seccomp TSYNC probe)
///   cmd=2 → `open("/etc/hostname", O_RDONLY)` (landlock thread-scope probe)
fn spawn_prelock_thread() {
    let handle: &'static PrelockThread = Box::leak(Box::new(PrelockThread {
        cmd: Mutex::new(0),
        cv: Condvar::new(),
        result_fd: AtomicI32::new(-1),
        result_read_errno: AtomicI32::new(i32::MIN),
    }));
    *PRELOCK.lock().unwrap() = Some(handle);
    std::thread::Builder::new()
        .name("zs-prelock-worker".into())
        .spawn(move || {
            let cmd = {
                let mut g = handle.cmd.lock().unwrap();
                while *g == 0 {
                    g = handle.cv.wait(g).unwrap();
                }
                *g
            };
            match cmd {
                1 => {
                    // SECCOMP probe: issue socket() FROM THIS (pre-lockdown) thread. If
                    // the filter is thread-scoped to main, this SUCCEEDS here (fd>=0) —
                    // the bug. With TSYNC, the kernel KillProcess-es the whole process
                    // before this returns.
                    #[allow(unsafe_code)]
                    let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0) };
                    handle.result_fd.store(fd, Ordering::SeqCst);
                }
                2 => {
                    // LANDLOCK probe: open an out-of-allow-list path O_RDONLY FROM THIS
                    // thread. A read-only open passes the seccomp openat arg-filter, so
                    // whether it is DENIED is purely the landlock layer's doing. landlock
                    // has NO TSYNC — its whole-process coverage rests on the
                    // single-threaded-V8 invariant. We record the errno (0 = the read
                    // SUCCEEDED → landlock did NOT cover this thread → the gap is open).
                    let path = b"/etc/hostname\0";
                    #[allow(unsafe_code)]
                    let fd = unsafe {
                        libc::open(path.as_ptr() as *const libc::c_char, libc::O_RDONLY, 0)
                    };
                    let errno = if fd < 0 {
                        std::io::Error::last_os_error().raw_os_error().unwrap_or(-1)
                    } else {
                        #[allow(unsafe_code)]
                        unsafe {
                            libc::close(fd);
                        }
                        0
                    };
                    handle.result_read_errno.store(errno, Ordering::SeqCst);
                }
                _ => {}
            }
        })
        .expect("spawn prelock worker thread");
}

fn main() {
    // Read the request envelope from stdin (the migration source never touches the
    // filesystem — it is piped in-memory, so landlock can deny ALL fs reads outside
    // the explicit allow-list without starving the recorder).
    let mut buf = String::new();
    let mut stdin = std::io::stdin().take((MAX_CHILD_STDIN_BYTES as u64) + 1);
    if let Err(e) = stdin.read_to_string(&mut buf) {
        emit_fatal(&format!("recorder child: read stdin failed: {e}"));
        return;
    }
    if buf.len() > MAX_CHILD_STDIN_BYTES {
        emit(&ChildResponse::request_too_large(
            "request",
            MAX_CHILD_STDIN_BYTES,
            buf.len(),
            SandboxReport::default(),
        ));
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
    let source_field = match req.operation {
        ChildOperation::RecordMigration => "ts_source",
        ChildOperation::EvalSchema => "schema_source",
    };
    if req.ts_source.len() > MAX_TS_SOURCE_BYTES {
        return Err(ChildResponse::request_too_large(
            source_field,
            MAX_TS_SOURCE_BYTES,
            req.ts_source.len(),
            SandboxReport::default(),
        ));
    }
    if let Some(blob) = req.schema_types_blob.as_deref() {
        if blob.len() > MAX_TS_SOURCE_BYTES {
            return Err(ChildResponse::request_too_large(
                "schema_types_blob",
                MAX_TS_SOURCE_BYTES,
                blob.len(),
                SandboxReport::default(),
            ));
        }
    }

    // TEST-ONLY: spawn a worker thread BEFORE the lockdown to model a V8 background
    // thread that already exists when the in-process filter is installed (the CRITICAL
    // PR4a code-critic finding). Done before init/lockdown so the regression e2e can
    // prove the filter reaches a pre-existing thread (process-wide kill), not just main.
    if std::env::var_os("ZS_RECORDER_PRELOCK_THREAD").is_some() {
        spawn_prelock_thread();
    }

    // Initialize V8 with a SINGLE-THREADED platform (no GC/compiler/platform worker
    // thread pool). This is the keystone of the CRITICAL thread-scope fix: landlock's
    // `restrict_self` and seccomp both ultimately bind threads, and landlock has no
    // TSYNC, so the only sound way to cover every thread with landlock is to ensure
    // the ONLY thread is the one calling `restrict_self`. A single-threaded V8 runs
    // GC/compilation inline on the calling thread, so no unfiltered background thread
    // exists for a resolver-bypassing escape to schedule onto (design §8.9).
    init_recorder_v8();

    // ---- FAIL-CLOSED single-threaded assertion (PR4a code-critic LOW) ----
    // The recorder's whole-process landlock coverage depends ENTIRELY on the
    // single-threaded-V8 invariant (landlock's restrict_self has no TSYNC — it binds
    // only the calling thread). `init_v8`/`init_v8_single_threaded` share one
    // process-global `Once`; if any future code path installed the MULTI-THREADED
    // platform first, the recorder would silently come up multi-threaded with NO error,
    // re-opening the thread-scope gap. We REFUSE TO RUN (fail closed) here rather than
    // degrade silently — verifying both the committed platform flavor AND that exactly
    // one OS thread exists before lockdown.
    //
    // `ZS_RECORDER_FORCE_MULTITHREAD` (test-only) spawns a parked thread before this
    // check so the regression e2e can prove the assertion FIRES. The thread-scope PROBE
    // seam `ZS_RECORDER_PRELOCK_THREAD` deliberately injects a thread to exercise the
    // KERNEL layer's process-wide kill, so it bypasses this userland refuse-to-run guard
    // (the kernel — not this assertion — is what that probe tests).
    if std::env::var_os("ZS_RECORDER_FORCE_MULTITHREAD").is_some() {
        spawn_prelock_thread();
        // Give the worker thread a moment to actually appear in /proc/self/task.
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    if std::env::var_os("ZS_RECORDER_PRELOCK_THREAD").is_none() {
        if let Err(reason) = assert_recorder_single_threaded() {
            return Err(ChildResponse::refused(reason, SandboxReport::default()));
        }
    }

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

    // Confirm the netns actually took. PRIMARY check: compare our own net-ns inode
    // against the parent's (passed in `req.parent_netns_inode`) — a DIFFERENT inode is
    // robust proof the `unshare(CLONE_NEWNET)` moved us into a fresh namespace (LOW #1).
    // Fall back to the interface-count heuristic only when the parent could not read
    // its inode (parent_netns_inode == 0). If the parent requested netns but it did not
    // engage, correct the report so the hosted floor check stays honest.
    report.netns = report.netns && netns_is_isolated(req.parent_netns_inode);

    let allow_read: Vec<PathBuf> = req.allow_read_paths.iter().map(PathBuf::from).collect();

    // ---- Build the runtime (pre-untrusted-eval, pre-lockdown) ----
    // `Runtime::builder().build()` runs under the SINGLE-THREADED V8 platform installed
    // by `init_recorder_v8()` above — so V8 spawns NO background worker thread pool, and
    // the seccomp filter (installed via TSYNC below) + landlock cover the only thread.
    // The seccomp allow-list covers V8's steady-state compute syscalls
    // (mmap/mprotect/futex/clone-for-threads). The untrusted module is NOT loaded until
    // AFTER the sandbox is applied.
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
            // landlock-less/setup-failed host => degraded floor only for LOCAL. Hosted
            // refuses below because filesystem Landlock is mandatory there.
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

    match req.operation {
        ChildOperation::RecordMigration => run_record_migration(runtime, req, report),
        ChildOperation::EvalSchema => run_schema_eval(runtime, req, report),
    }
}

fn run_record_migration(
    runtime: Runtime,
    req: ChildRequest,
    report: SandboxReport,
) -> Result<ChildResponse, ChildResponse> {
    let untrusted = module_graph(FrontendProgram::RecordMigration {
        source: &req.ts_source,
    });

    let probe: Result<(String, Vec<String>), String> = runtime.with_scope(|scope| {
        install_frontend_globals(
            scope,
            FrontendGlobals::Migration,
            req.determinism_probe_seed,
        )?;

        {
            // Only the filename-derived NAME is exposed to JS (a benign fallback for
            // `resolveName`). owner_app is DELIBERATELY NOT exposed: it is a
            // tenant-identifying field folded into the authoritative Checksum::of_ir,
            // and the recorder stamps it in Rust below — untrusted up() must have no
            // JS-reachable handle to influence it (PR4a code-critic HIGH #1).
            //
            // `v8::String::new` returns None above V8's max string length (~512MB),
            // so a hostile oversized name must NOT `.unwrap()`-panic the child (PR4a
            // code-critic MED #5). The HTTP boundary caps name length; this is the
            // last-ditch in-child guard for the direct-spawn path.
            let global = scope.get_current_context().global(scope);
            let k = v8::String::new(scope, "__zsMigrationName")
                .ok_or_else(|| "recorder child: __zsMigrationName key alloc failed".to_string())?;
            let v = v8::String::new(scope, req.name.as_str()).ok_or_else(|| {
                "recorder child: migration name too large for a V8 string".to_string()
            })?;
            global.set(scope, k.into(), v.into());

            // The §8.9.2 type-only schema-types blob, delivered in-memory as a
            // read-only recorder context global so the wire DTO's promised input is
            // actually available to the recorder (PR4a code-critic LOW #7). Bounded by
            // the same checked alloc (no panic on a hostile-sized blob).
            if let Some(blob) = req.schema_types_blob.as_deref() {
                let bk = v8::String::new(scope, "__zsSchemaTypes").ok_or_else(|| {
                    "recorder child: __zsSchemaTypes key alloc failed".to_string()
                })?;
                let bv = v8::String::new(scope, blob).ok_or_else(|| {
                    "recorder child: schema-types blob too large for a V8 string".to_string()
                })?;
                global.set(scope, bk.into(), bv.into());
            }
        }

        zeroship_runtime::modules::load_modules(scope, &untrusted)?;
        scope.perform_microtask_checkpoint();

        let global = scope.get_current_context().global(scope);
        let k = v8::String::new(scope, "__zsOpIR")
            .ok_or_else(|| "recorder child: __zsOpIR key alloc failed".to_string())?;
        let v = global
            .get(scope, k.into())
            .filter(|v| v.is_string())
            .ok_or_else(|| "op recorder produced no IR".to_string())?;
        let nondeterminism_used = read_determinism_probe_used(scope)?;
        Ok((v.to_rust_string_lossy(scope), nondeterminism_used))
    });

    let (raw_envelope, nondeterminism_used) = match probe {
        Ok(pair) => pair,
        Err(e) => return Err(ChildResponse::eval_error(e, report)),
    };

    // ---- Inspect the inner envelope + Rust-stamp owner_app ----
    // op_recorder.js emits `{ ok, ir?, error? }`. An inner `ok:false` (a throwing /
    // out-of-recorder up()) MUST surface as a FAILURE at the service boundary carrying
    // the real error — not an outer ok:true wrapping an inner failure (PR4a code-critic
    // MED #3). On inner success we stamp the AUTHORITATIVE owner_app from the
    // server-injected, ownership-cross-checked `req.owner_app` (HIGH #1) — overwriting
    // whatever the untrusted code may have produced.
    let ir_json = match finalize_envelope(&raw_envelope, &req.owner_app, &nondeterminism_used) {
        Ok(s) => s,
        Err(e) => return Err(ChildResponse::eval_error(e, report)),
    };

    Ok(ChildResponse::ok(ir_json, report))
}

fn run_schema_eval(
    runtime: Runtime,
    req: ChildRequest,
    report: SandboxReport,
) -> Result<ChildResponse, ChildResponse> {
    let untrusted = module_graph(FrontendProgram::EvalSchema {
        source: &req.ts_source,
    });

    let schema_json: Result<String, String> = runtime.with_scope(|scope| {
        install_frontend_globals(scope, FrontendGlobals::Schema, None)?;

        {
            let global = scope.get_current_context().global(scope);
            let k = v8::String::new(scope, "__zsOwnerApp")
                .ok_or_else(|| "recorder child: __zsOwnerApp key alloc failed".to_string())?;
            let v = v8::String::new(scope, req.owner_app.as_str()).ok_or_else(|| {
                "recorder child: owner_app too large for a V8 string".to_string()
            })?;
            global.set(scope, k.into(), v.into());
        }

        zeroship_runtime::modules::load_modules(scope, &untrusted)?;
        scope.perform_microtask_checkpoint();

        let global = scope.get_current_context().global(scope);
        let k = v8::String::new(scope, "__zsSchemaIR")
            .ok_or_else(|| "recorder child: __zsSchemaIR key alloc failed".to_string())?;
        let v = global
            .get(scope, k.into())
            .filter(|v| v.is_string())
            .ok_or_else(|| "schema front-end produced no IR".to_string())?;
        Ok(v.to_rust_string_lossy(scope))
    });

    match schema_json {
        Ok(s) => Ok(ChildResponse::ok(s, report)),
        Err(e) => Err(ChildResponse::eval_error(e, report)),
    }
}

/// Inspect the `op_recorder.js` `{ ok, ir?, error? }` envelope and produce the final
/// `{ ok:true, ir }` string the parent reads back — or an `Err(message)` carrying the
/// REAL recorder/throw error on an inner failure.
///
/// SECURITY: the authoritative `owner_app` is STAMPED here in Rust from the
/// server-injected, ownership-cross-checked `owner_app` — never read from a
/// JS-reachable global the untrusted `up()` body shares (PR4a code-critic HIGH #1).
fn finalize_envelope(
    raw: &str,
    owner_app: &str,
    nondeterminism_used: &[String],
) -> Result<String, String> {
    let mut env: serde_json::Value =
        serde_json::from_str(raw).map_err(|e| format!("recorder produced invalid envelope: {e}"))?;
    if !env.is_object() {
        return Err("recorder envelope is not an object".to_string());
    }

    let ok = env.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
    if !ok {
        // Surface the inner error verbatim so the service boundary reports the REAL
        // authoring failure (e.g. OP_OUTSIDE_RECORDER / the throw text), not a
        // downstream "did not re-parse for checksum" misdirection (MED #3).
        let msg = env
            .get("error")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| "recorder reported failure with no error message".to_string());
        return Err(msg);
    }

    let ir = env
        .get_mut("ir")
        .ok_or_else(|| "recorder envelope ok:true but missing `ir`".to_string())?;
    let ir_obj = ir
        .as_object_mut()
        .ok_or_else(|| "recorder envelope `ir` is not an object".to_string())?;
    // Stamp (or overwrite) the authoritative server owner. An empty owner (local
    // single-tenant with no app context) leaves the field unset, matching the prior
    // "omit owner_app when empty" shape.
    if owner_app.is_empty() {
        ir_obj.remove("owner_app");
    } else {
        ir_obj.insert(
            "owner_app".to_string(),
            serde_json::Value::String(owner_app.to_string()),
        );
    }
    if let Some(env_obj) = env.as_object_mut() {
        env_obj.insert(
            "nondeterminismUsed".to_string(),
            serde_json::Value::Array(
                nondeterminism_used
                    .iter()
                    .cloned()
                    .map(serde_json::Value::String)
                    .collect(),
            ),
        );
    }

    serde_json::to_string(&env).map_err(|e| format!("re-serialize stamped envelope: {e}"))
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
        // --- CRITICAL thread-scope proof: issue socket() on a thread that was
        //     created BEFORE the lockdown (mirrors a V8 background thread). With a
        //     thread-scoped filter that thread is UNFILTERED and socket() returns an
        //     fd (the bug); with the TSYNC fix + single-threaded V8 the kernel
        //     KillProcess-es the WHOLE process (SIGSYS) before the call returns. The
        //     test observes the process termination cause. Requires
        //     ZS_RECORDER_PRELOCK_THREAD to have spawned the worker. ---
        "thread_socket" => {
            let handle = PRELOCK
                .lock()
                .unwrap()
                .expect("ZS_RECORDER_PRELOCK_THREAD must be set for the thread_socket probe");
            // Signal the parked pre-lockdown thread to call socket().
            {
                let mut g = handle.cmd.lock().unwrap();
                *g = 1;
                handle.cv.notify_all();
            }
            // Wait (bounded) for the worker to report its fd. If the kernel TSYNC-killed
            // the process, we never get here. If it returns an fd>=0 on the unfiltered
            // thread, the bug is live -> exit 0 (test asserts process death instead).
            let mut waited_ms = 0u64;
            loop {
                let fd = handle.result_fd.load(Ordering::SeqCst);
                if fd >= 0 {
                    // socket() SUCCEEDED on the pre-lockdown thread -> the filter did
                    // NOT reach it. The test asserts the process was SIGSYS-killed, so
                    // reaching here (no kill) FAILS the test.
                    unsafe { libc::close(fd) };
                    std::process::exit(0);
                }
                if waited_ms >= 5_000 {
                    // Worker neither succeeded nor was killed within the window. Exit a
                    // distinct non-42/non-0 code so the test fails loudly.
                    std::process::exit(11);
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
                waited_ms += 10;
            }
        }
        // --- LANDLOCK thread-scope proof (PR4a code-critic MED): issue an out-of-dir
        //     READ on the PRE-LOCKDOWN worker thread (mirrors a V8 background thread).
        //     landlock's `restrict_self` has NO TSYNC — it binds ONLY the calling (main)
        //     thread. So a thread that already existed when `restrict_self` ran is NOT
        //     under landlock, and its read SUCCEEDS (errno 0). This is the GROUND-TRUTH
        //     demonstration that the landlock layer's whole-process coverage rests
        //     ENTIRELY on the single-threaded-V8 invariant: there must be NO pre-lockdown
        //     sibling thread, which the single-threaded V8 platform guarantees and the
        //     fail-closed `assert_recorder_single_threaded` enforces. The seccomp
        //     `thread_socket` probe does NOT cover this leg (it rides seccomp TSYNC,
        //     which landlock lacks). Exit codes: 0 = the sibling's read SUCCEEDED (the
        //     EXPECTED ground truth — landlock does not cover a pre-lockdown thread);
        //     42 = EACCES (landlock unexpectedly covered the sibling); 13 = other errno
        //     (e.g. landlock absent). Requires ZS_RECORDER_PRELOCK_THREAD. ---
        "thread_read_outside" => {
            let handle = PRELOCK
                .lock()
                .unwrap()
                .expect("ZS_RECORDER_PRELOCK_THREAD must be set for the thread_read_outside probe");
            // Signal the parked pre-lockdown thread to open /etc/hostname O_RDONLY.
            {
                let mut g = handle.cmd.lock().unwrap();
                *g = 2;
                handle.cv.notify_all();
            }
            let mut waited_ms = 0u64;
            loop {
                let errno = handle.result_read_errno.load(Ordering::SeqCst);
                if errno != i32::MIN {
                    if errno == 0 {
                        // The out-of-dir read SUCCEEDED on the pre-lockdown thread ->
                        // landlock did NOT cover it (no TSYNC). This is the expected
                        // ground truth; the test asserts exit 0 and reads it as the
                        // load-bearing proof that single-threaded-V8 is required.
                        std::process::exit(0);
                    }
                    if errno == libc::EACCES {
                        // landlock unexpectedly DENIED the pre-lockdown sibling's read.
                        // (Not expected given landlock has no TSYNC.) Distinct code 42.
                        std::process::exit(42);
                    }
                    // Some other errno (e.g. landlock absent on this host). Distinct
                    // code so the test fails loudly rather than silently passing.
                    std::process::exit(13);
                }
                if waited_ms >= 5_000 {
                    std::process::exit(11);
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
                waited_ms += 10;
            }
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
        // --- netns: a REAL outbound connect with seccomp OFF — proves netns ALONE
        //     contains the network (the degraded-floor posture where netns is the SOLE
        //     network boundary). Only meaningful when seccomp is DISABLED (else
        //     socket()/connect() are SIGSYS-killed first). The interface-less netns has
        //     no route, so connect() fails with ENETUNREACH/EHOSTUNREACH/ENETDOWN; we
        //     exit 42 iff it was contained, 0 iff it REACHED the network (PR4a
        //     code-critic MED #4). 198.51.100.1 is TEST-NET-2 (RFC 5737) — never routed.
        "net_connect" => {
            let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0) };
            if fd < 0 {
                // socket() itself failed (e.g. seccomp on after all) — not a clean netns
                // proof; exit non-42 so the test fails loudly rather than false-passing.
                std::process::exit(8);
            }
            let mut addr: libc::sockaddr_in = unsafe { std::mem::zeroed() };
            addr.sin_family = libc::AF_INET as libc::sa_family_t;
            addr.sin_port = 80u16.to_be();
            // 198.51.100.1 (TEST-NET-2) in network byte order.
            addr.sin_addr.s_addr = u32::from_be_bytes([198, 51, 100, 1]).to_be();
            let rc = unsafe {
                libc::connect(
                    fd,
                    &addr as *const _ as *const libc::sockaddr,
                    std::mem::size_of::<libc::sockaddr_in>() as u32,
                )
            };
            let err = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
            unsafe { libc::close(fd) };
            if rc < 0 {
                // Contained by the interface-less netns: no route to the host.
                match err {
                    libc::ENETUNREACH | libc::EHOSTUNREACH | libc::ENETDOWN | libc::EADDRNOTAVAIL => {
                        std::process::exit(42)
                    }
                    // Any OTHER failure (e.g. ECONNREFUSED/ETIMEDOUT) means the packet
                    // could LEAVE — netns did not contain. Fail loudly.
                    _ => std::process::exit(9),
                }
            } else {
                // connect succeeded -> the network was reachable -> netns did NOT contain.
                std::process::exit(0);
            }
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

/// Check that the child is in a fresh network namespace.
///
/// PRIMARY (robust) check: compare our `/proc/self/ns/net` inode against the parent's
/// (`parent_netns_inode`, captured pre-spawn). A DIFFERENT inode proves the
/// `unshare(CLONE_NEWNET)` actually moved us into a fresh namespace — this does NOT
/// false-positive on a host whose only interface is already `lo` (the interface-count
/// heuristic's blind spot, PR4a code-critic LOW #1). We only treat EQUAL-or-unreadable
/// inodes as the fallback case.
///
/// FALLBACK (only when `parent_netns_inode == 0`, i.e. the parent could not read its
/// own inode): the legacy `/proc/net/dev` interface-count heuristic. `/proc/net/dev`
/// is netns-scoped; a fresh `CLONE_NEWNET` namespace shows exactly one interface (`lo`,
/// DOWN, no routes) while the host shows `eth*`/`docker0`/`veth*`/etc.
///
/// If both checks are inconclusive we conservatively report NOT isolated (so the hosted
/// floor leans on seccomp rather than over-claiming containment).
fn netns_is_isolated(parent_netns_inode: u64) -> bool {
    if parent_netns_inode != 0 {
        if let Ok(meta) = std::fs::metadata("/proc/self/ns/net") {
            let own = std::os::unix::fs::MetadataExt::ino(&meta);
            // A different inode == we are in a DIFFERENT (fresh) netns than the parent.
            // Equal inode == the unshare did not move us; report not-isolated.
            return own != parent_netns_inode;
        }
        // Could not read our own inode — fall through to the heuristic.
    }
    netns_is_isolated_by_interface_count()
}

/// Fallback netns heuristic by interface count (see `netns_is_isolated`).
fn netns_is_isolated_by_interface_count() -> bool {
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
