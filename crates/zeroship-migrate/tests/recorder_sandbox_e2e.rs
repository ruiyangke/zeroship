#![cfg(feature = "zsv8")]
//! PR4a §8.9 FAITHFUL kernel-level e2e for the build-time recorder sandbox.
//!
//! These tests exercise the REAL path — the actual `zeroship-migrate-recorder-child`
//! binary, spawned with the actual `pre_exec` lockdown (netns + rlimits) + the actual
//! in-child seccomp + landlock — and assert by OBSERVING THE CHILD TERMINATION CAUSE
//! (the terminating signal / exit code), not merely that a userland resolver refused
//! an import. There is NO resolver stub here.
//!
//! Coverage (design §8.9 / PR4a):
//!   1. The kernel KILLS a subprocess-spawn / network-connect / fs-write attempt —
//!      `SIGSYS` from the seccomp default-deny filter (socket/connect/execve/fork),
//!      `EACCES` from landlock (write / out-of-dir read). A SEPARATE probe proves the
//!      KERNEL fires even when the userland resolver is BYPASSED (the probe issues the
//!      syscall directly from native code, never consulting the resolver).
//!   2. RLIMIT_CPU / wall-watchdog / RLIMIT_AS bound an infinite loop / alloc bomb =>
//!      `BUILD_RECORDER_BUDGET_EXCEEDED`.
//!   3. On a landlock-less posture the local degraded floor still kills
//!      subprocess/network cases via seccomp/netns, but the hosted recorder REFUSES
//!      without active Landlock.
//!   4. Per-invocation isolation: two concurrent record calls get SEPARATE sandbox
//!      children, no fs/state bleed.
//!
//! ## Capability gating (hard-fail, never silent-skip)
//!
//! Per the faithful-e2e rule, a kernel feature that genuinely cannot be exercised is
//! gated on a capability PROBE that HARD-FAILS if the capability IS present (so a
//! real regression can never hide behind a skip). On this Linux 6.12 host all of
//! seccomp + landlock + netns are present, so every assertion runs LIVE.

#![cfg(target_os = "linux")]
#![allow(unsafe_code)] // the netns-alone degraded-floor probe needs Command::pre_exec

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use zeroship_migrate::frontend::recorder_protocol::{ChildOperation, ChildRequest};
use zeroship_migrate::frontend::recorder_service::{
    recorder_child_path, spawn_sandboxed_record, spawn_sandboxed_schema_eval,
};
use zeroship_migrate::frontend::{
    RecordRequest, RecorderError, ResourceBudget, SandboxPosture, SchemaEvalRequest,
};

const HAPPY_MIGRATION: &str = r#"
import { table, t, genRandomUuid } from "@zeroship/migrate";
export const name = "e2e_happy";
export function up() {
  table("widgets").create({
    columns: {
      id: t.uuid().notNull().default(genRandomUuid()),
      label: t.text(),
    },
  });
}
"#;

// ===========================================================================
// Capability probes — HARD-FAIL if the capability is present but we'd skip.
// ===========================================================================

/// True iff this host can install a non-privileged seccomp filter. We detect it by
/// reading the kernel's advertised seccomp actions; KillProcess must be available.
fn seccomp_available() -> bool {
    std::fs::read_to_string("/proc/sys/kernel/seccomp/actions_avail")
        .map(|s| s.contains("kill_process") || s.contains("kill_thread"))
        .unwrap_or(false)
}

/// True iff the running kernel exposes the landlock LSM.
fn landlock_available() -> bool {
    std::fs::read_to_string("/sys/kernel/security/lsm")
        .map(|s| s.split(',').any(|l| l.trim() == "landlock"))
        .unwrap_or(false)
}

/// True iff unprivileged user+net namespaces can be created (needed for netns).
fn netns_available() -> bool {
    // The recorder child uses unshare(CLONE_NEWUSER|CLONE_NEWNET). A cheap probe:
    // fork a child that tries the same unshare and report its exit.
    let status = Command::new("unshare")
        .args(["--user", "--net", "true"])
        .status();
    matches!(status, Ok(s) if s.success())
}

// ===========================================================================
// Helpers
// ===========================================================================

/// Build the request envelope for a direct child spawn (probe/degraded tests that
/// drive the child binary themselves, controlling pre_exec/env precisely).
fn child_request(src: &str, hosted: bool, netns: bool, rlimit: bool) -> String {
    let req = ChildRequest {
        operation: ChildOperation::RecordMigration,
        ts_source: src.to_string(),
        owner_app: "app_e2e".into(),
        name: "e2e".into(),
        hosted,
        netns_engaged: netns,
        rlimit_engaged: rlimit,
        heap_limit_mb: 256,
        allow_read_paths: vec![],
        schema_types_blob: None,
        // The test's own (parent) net-ns inode, so the child's inode-inequality check
        // (LOW #1) has a baseline to compare against when this request drives a netns
        // probe. 0 when unreadable -> child falls back to the interface heuristic.
        parent_netns_inode: zeroship_migrate::frontend::recorder_service::read_netns_inode()
            .unwrap_or(0),
    };
    serde_json::to_string(&req).unwrap()
}

/// Spawn the child binary directly with a `ZS_RECORDER_PROBE` and return the raw
/// `ExitStatus`. The child runs the FULL in-process sandbox (seccomp+landlock) then
/// issues the dangerous syscall directly (resolver bypassed). NO pre_exec here, so
/// netns is NOT engaged — this isolates the seccomp/landlock layer.
fn run_probe(probe: &str) -> std::process::ExitStatus {
    let bin = recorder_child_path();
    assert!(
        bin.exists(),
        "recorder child binary missing at {} — build the bins first",
        bin.display()
    );
    let mut child = Command::new(&bin)
        .env("ZS_RECORDER_PROBE", probe)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn recorder child");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(child_request(HAPPY_MIGRATION, false, false, false).as_bytes())
        .ok();
    child.wait().expect("wait child")
}

fn term_signal(status: &std::process::ExitStatus) -> Option<i32> {
    use std::os::unix::process::ExitStatusExt;
    status.signal()
}

// ===========================================================================
// 0. Happy path — the sandbox does NOT break legitimate recording.
// ===========================================================================

#[test]
fn happy_path_records_under_full_sandbox() {
    assert!(seccomp_available(), "seccomp must be present on this host");
    let req = RecordRequest {
        ts_source: HAPPY_MIGRATION.to_string(),
        owner_app: "app_e2e".into(),
        name: "e2e_happy".into(),
        posture: SandboxPosture::Hosted,
        budget: ResourceBudget::default(),
        allow_read_paths: vec![],
        schema_types_blob: None,
    };
    let res = spawn_sandboxed_record(&req).expect("happy recording under sandbox");
    assert!(
        res.ir_json.contains("createTable"),
        "recorded IR must contain the createTable op: {}",
        res.ir_json
    );
    // The seccomp layer engaged (V8 evaluated `up()` post-lockdown).
    assert!(res.report.seccomp, "seccomp must have engaged: {:?}", res.report);
    // netns engaged via pre_exec (this host supports unprivileged userns+netns).
    if netns_available() {
        assert!(
            res.report.netns,
            "netns must engage when unprivileged userns is available: {:?}",
            res.report
        );
    }
}

// ===========================================================================
// 1. KERNEL kills subprocess / network / fs — observed via termination cause.
//    SEPARATE proof: the kernel fires even when the resolver is BYPASSED (the
//    probe issues the syscall from native code, never touching the resolver).
// ===========================================================================

#[test]
fn seccomp_kills_subprocess_spawn_with_sigsys() {
    if !seccomp_available() {
        panic!("CAPABILITY PRESENT BUT TEST WOULD SKIP: seccomp is available — fix the gate");
    }
    // execve is child_process.spawn's kernel primitive; fork is the other half.
    for probe in ["execve", "fork"] {
        let status = run_probe(probe);
        assert_eq!(
            term_signal(&status),
            Some(libc::SIGSYS),
            "probe '{probe}' must be SIGSYS-killed by the seccomp default-deny filter \
             (kernel fires even though the resolver was bypassed); got {status:?}"
        );
    }
}

#[test]
fn seccomp_kills_network_socket_and_connect_with_sigsys() {
    if !seccomp_available() {
        panic!("CAPABILITY PRESENT BUT TEST WOULD SKIP: seccomp is available — fix the gate");
    }
    for probe in ["socket", "connect"] {
        let status = run_probe(probe);
        assert_eq!(
            term_signal(&status),
            Some(libc::SIGSYS),
            "network probe '{probe}' must be SIGSYS-killed by seccomp; got {status:?}"
        );
    }
}

#[test]
fn seccomp_kills_io_uring_setup_the_known_bypass_vector() {
    // io_uring is the canonical seccomp BYPASS: a ring can submit network/fs ops
    // without issuing the per-op syscall the filter gates. Our allow-list omits
    // io_uring_setup entirely, so the ring can never be created — the setup syscall
    // is SIGSYS-killed. This regression-pins that the bypass surface stays closed.
    if !seccomp_available() {
        panic!("CAPABILITY PRESENT BUT TEST WOULD SKIP: seccomp is available — fix the gate");
    }
    let status = run_probe("io_uring");
    assert_eq!(
        term_signal(&status),
        Some(libc::SIGSYS),
        "io_uring_setup must be SIGSYS-killed (it is OFF the allow-list — the bypass \
         vector is closed); got {status:?}"
    );
}

#[test]
fn seccomp_kills_write_open_even_without_landlock() {
    // PR4a code-critic MED #2: a WRITE/create open must be denied at the SECCOMP layer
    // (an arg-filter on `openat` flags permits only read-only opens), so fs-WRITE
    // containment against a resolver-bypassing native escape holds EVEN on the degraded
    // floor (landlock absent). The `write_open` probe issues `open(O_WRONLY|O_CREAT)`
    // directly from native code post-lockdown; with the seccomp openat arg-filter it is
    // SIGSYS-killed (NOT an EACCES return) regardless of whether landlock is present.
    if !seccomp_available() {
        panic!("CAPABILITY PRESENT BUT TEST WOULD SKIP: seccomp is available — fix the gate");
    }
    let status = run_probe("write_open");
    assert_eq!(
        term_signal(&status),
        Some(libc::SIGSYS),
        "a write/create open must be SIGSYS-killed by the seccomp openat arg-filter \
         (fs-write containment holds without landlock); got {status:?} — a non-SIGSYS \
         exit means the write open was NOT denied at the seccomp layer (MED #2 regressed)"
    );
}

#[test]
fn landlock_denies_out_of_dir_read() {
    if !landlock_available() {
        // Degraded floor: the resolver is the fs boundary for READS. This is the only
        // fs assertion legitimately gated — but it must HARD-FAIL if landlock IS here.
        eprintln!("landlock not available — read fs boundary is the resolver (degraded floor)");
        return;
    }
    // A READ-only open of an out-of-allow-list path is permitted by seccomp (read-only
    // openat passes the arg-filter) but DENIED by landlock with EACCES (no signal); the
    // probe exits with sentinel 42. (The WRITE case is now SIGSYS-killed at the seccomp
    // layer — see `seccomp_kills_write_open_even_without_landlock`.)
    let status = run_probe("read_outside");
    assert_eq!(
        term_signal(&status),
        None,
        "landlock read probe must NOT be signal-killed (it returns EACCES); got {status:?}"
    );
    assert_eq!(
        status.code(),
        Some(42),
        "landlock read probe must hit the EACCES denial (exit 42); got {status:?} — \
         a code 0 means the out-of-dir read SUCCEEDED (landlock did NOT fire)"
    );
}

// ===========================================================================
// 2. Resource budget — RLIMIT_CPU / wall-watchdog / RLIMIT_AS.
// ===========================================================================

#[test]
fn rlimit_cpu_or_wall_bounds_an_infinite_loop() {
    // A migration whose up() never returns (busy loop). RLIMIT_CPU (SIGXCPU) or the
    // wall watchdog (SIGKILL) must bound it. Either way => BUILD_RECORDER_BUDGET_EXCEEDED.
    let loop_src = r#"
        import { table, t, genRandomUuid } from "@zeroship/migrate";
        export function up() { table("t").create({ columns: { id: t.int().notNull() } }); while (true) {} }
    "#;
    let req = RecordRequest {
        ts_source: loop_src.to_string(),
        owner_app: "app_e2e".into(),
        name: "loop".into(),
        posture: SandboxPosture::Hosted,
        budget: ResourceBudget {
            cpu_seconds: 2,
            address_space_bytes: 96 * 1024 * 1024 * 1024,
            heap_limit_mb: 256,
            wall_ms: 4_000,
        },
        allow_read_paths: vec![],
        schema_types_blob: None,
    };
    let err = spawn_sandboxed_record(&req).expect_err("infinite loop must be bounded");
    match err {
        RecorderError::BudgetExceeded { .. } => {}
        other => panic!("expected BUILD_RECORDER_BUDGET_EXCEEDED, got {other:?}"),
    }
    assert_eq!(err.code(), "BUILD_RECORDER_BUDGET_EXCEEDED");
}

#[test]
fn memory_budget_bounds_an_allocation_bomb() {
    // A migration that allocates without bound. The AUTHORITATIVE V8 memory bound is
    // the in-VM heap cap (RLIMIT_AS is too coarse for V8 — its caged-heap reserves
    // tens of GiB of sparse VIRTUAL space that is never RSS-backed, so RLIMIT_AS
    // cannot be a tight physical bound). The bomb is contained by the V8 heap limit
    // (near-heap-limit callback terminates the isolate) backstopped by the parent's
    // wall-clock watchdog — BOTH surface as BUILD_RECORDER_BUDGET_EXCEEDED. The key
    // assertion: the bomb is CONTAINED (does not exhaust the host) and the budget
    // error fires within the wall bound.
    let bomb_src = r#"
        export function up() {
          const chunks = [];
          while (true) { chunks.push(new Uint8Array(64 * 1024 * 1024)); }
        }
    "#;
    let req = RecordRequest {
        ts_source: bomb_src.to_string(),
        owner_app: "app_e2e".into(),
        name: "bomb".into(),
        posture: SandboxPosture::Hosted,
        budget: ResourceBudget {
            cpu_seconds: 10,
            // RLIMIT_AS is generous (above V8's caged-heap reservation) — NOT the
            // bound. The TIGHT V8 heap cap bounds at the JS-allocation boundary; the
            // wall watchdog is the backstop.
            address_space_bytes: 96 * 1024 * 1024 * 1024,
            heap_limit_mb: 128,
            wall_ms: 5_000,
        },
        allow_read_paths: vec![],
        schema_types_blob: None,
    };
    let start = std::time::Instant::now();
    let err = spawn_sandboxed_record(&req).expect_err("alloc bomb must be bounded");
    let elapsed = start.elapsed();
    match err {
        // wall watchdog or RLIMIT_CPU or the heap-limit termination — all contained.
        RecorderError::BudgetExceeded { .. } => {}
        // The heap-limit callback may surface the termination as a recorder-side eval
        // error (terminated isolate / RangeError) — ALSO a contained outcome.
        RecorderError::EvalError(_) => {}
        other => panic!("expected budget/eval containment, got {other:?}"),
    }
    assert!(
        elapsed < std::time::Duration::from_secs(12),
        "alloc bomb must be bounded promptly (wall/heap), took {elapsed:?}"
    );
}

#[test]
fn wall_watchdog_bounds_a_cpu_idle_hang() {
    // A migration that blocks on wall-time without burning CPU would slip RLIMIT_CPU.
    // We cannot sleep without a syscall (nanosleep IS allowed for V8), so emulate a
    // long busy-spin with a generous CPU limit but a TIGHT wall limit, forcing the
    // WALL watchdog (not RLIMIT_CPU) to be the bound that fires.
    let src = r#"
        export function up() { while (true) {} }
    "#;
    let req = RecordRequest {
        ts_source: src.to_string(),
        owner_app: "app_e2e".into(),
        name: "wall".into(),
        posture: SandboxPosture::Hosted,
        budget: ResourceBudget {
            cpu_seconds: 1000, // effectively unbounded CPU
            address_space_bytes: 96 * 1024 * 1024 * 1024,
            heap_limit_mb: 256,
            wall_ms: 1_000, // tight wall — the watchdog must fire first
        },
        allow_read_paths: vec![],
        schema_types_blob: None,
    };
    let start = std::time::Instant::now();
    let err = spawn_sandboxed_record(&req).expect_err("wall hang must be bounded");
    let elapsed = start.elapsed();
    assert!(
        matches!(err, RecorderError::BudgetExceeded { .. }),
        "wall hang must be BUILD_RECORDER_BUDGET_EXCEEDED, got {err:?}"
    );
    assert!(
        elapsed < std::time::Duration::from_secs(8),
        "wall watchdog must fire near the 1s budget, took {elapsed:?}"
    );
}

#[test]
fn schema_eval_infinite_loop_is_bounded_by_same_child_budget() {
    // `generate --schema` evaluates creator schema JS before live-DB introspection.
    // That eval must be in the same sandboxed child as recording, so a top-level loop
    // is killed by the parent wall watchdog instead of hanging the parent process.
    let src = r#"
        while (true) {}
        export const schema = {};
    "#;
    let req = SchemaEvalRequest {
        schema_source: src.to_string(),
        owner_app: "app_schema_loop".into(),
        posture: SandboxPosture::Local,
        budget: ResourceBudget {
            cpu_seconds: 1000,
            address_space_bytes: 96 * 1024 * 1024 * 1024,
            heap_limit_mb: 256,
            wall_ms: 1_000,
        },
        allow_read_paths: vec![],
    };
    let start = std::time::Instant::now();
    let err = spawn_sandboxed_schema_eval(&req).expect_err("schema loop must be bounded");
    let elapsed = start.elapsed();
    assert!(
        matches!(err, RecorderError::BudgetExceeded { .. }),
        "schema loop must be BUILD_RECORDER_BUDGET_EXCEEDED, got {err:?}"
    );
    assert_eq!(err.code(), "BUILD_RECORDER_BUDGET_EXCEEDED");
    assert!(
        elapsed < std::time::Duration::from_secs(8),
        "schema loop must be killed by the tight wall budget, took {elapsed:?}"
    );
}

// ===========================================================================
// 3. Degraded floor + hosted refuse-to-run.
// ===========================================================================

#[test]
fn degraded_floor_seccomp_still_kills_without_landlock() {
    // The seccomp layer is INDEPENDENT of landlock: even simulating a landlock-less
    // posture (we cannot disable the kernel LSM from a test, so we assert the
    // seccomp kill holds regardless of landlock), the subprocess/network cases die
    // by SIGSYS. This proves the degraded floor (seccomp+netns) retains RCE/network
    // containment when only the kernel FS read-restriction is unavailable.
    if !seccomp_available() {
        panic!("CAPABILITY PRESENT BUT TEST WOULD SKIP: seccomp is available — fix the gate");
    }
    let status = run_probe("execve");
    assert_eq!(
        term_signal(&status),
        Some(libc::SIGSYS),
        "the degraded floor's seccomp layer must still SIGSYS-kill a subprocess spawn"
    );
}

#[test]
fn hosted_refuses_to_run_with_neither_seccomp_nor_netns() {
    // Drive the child with hosted=true but seccomp DISABLED (via the test env hook)
    // and netns NOT engaged (no pre_exec). The child MUST refuse to evaluate the
    // untrusted JS — there is no unconstrained-Node fallback.
    let bin = recorder_child_path();
    assert!(bin.exists());
    let mut child = Command::new(&bin)
        // The child honors ZS_RECORDER_DISABLE_SECCOMP only to make this refuse-to-run
        // path testable; in production seccomp always attempts to install.
        .env("ZS_RECORDER_DISABLE_SECCOMP", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn");
    // hosted=true, netns_engaged=false (no pre_exec netns) => floor not met.
    child
        .stdin
        .take()
        .unwrap()
        .write_all(child_request(HAPPY_MIGRATION, true, false, false).as_bytes())
        .ok();
    let out = {
        use std::io::Read;
        let mut s = String::new();
        child.stdout.take().unwrap().read_to_string(&mut s).ok();
        s
    };
    child.wait().ok();
    let resp: zeroship_migrate::frontend::recorder_protocol::ChildResponse =
        serde_json::from_str(out.trim()).unwrap_or_else(|e| panic!("parse resp: {e}; raw={out}"));
    assert!(!resp.ok, "hosted recorder must refuse, got ok response: {out}");
    match resp.error {
        Some(zeroship_migrate::frontend::recorder_protocol::ChildError::SandboxRefused(_)) => {}
        other => panic!("expected SandboxRefused, got {other:?}"),
    }
}

#[test]
fn hosted_refuses_when_landlock_forced_unavailable_even_with_seccomp() {
    // Force only Landlock unavailable while still giving seccomp its no_new_privs
    // precondition through pre_exec_rlimits. Hosted must fail closed instead of
    // accepting a seccomp-only filesystem posture.
    let bin = recorder_child_path();
    assert!(bin.exists());
    use std::os::unix::process::CommandExt;
    let mut cmd = Command::new(&bin);
    cmd.env("ZS_RECORDER_FORCE_LANDLOCK_UNAVAILABLE", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    unsafe {
        cmd.pre_exec(|| {
            zeroship_migrate::frontend::sandbox::pre_exec_rlimits(ResourceBudget::default())
        });
    }
    let mut child = cmd.spawn().expect("spawn");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(child_request(HAPPY_MIGRATION, true, false, true).as_bytes())
        .ok();
    let out = {
        use std::io::Read;
        let mut s = String::new();
        child.stdout.take().unwrap().read_to_string(&mut s).ok();
        s
    };
    child.wait().ok();
    let resp: zeroship_migrate::frontend::recorder_protocol::ChildResponse =
        serde_json::from_str(out.trim()).unwrap_or_else(|e| panic!("parse resp: {e}; raw={out}"));
    assert!(!resp.ok, "hosted recorder must refuse without landlock: {out}");
    assert!(
        resp.report.seccomp,
        "this regression must prove seccomp-only is insufficient; report={:?}",
        resp.report
    );
    assert!(!resp.report.landlock, "landlock must be forced unavailable");
    match resp.error {
        Some(zeroship_migrate::frontend::recorder_protocol::ChildError::SandboxRefused(
            reason,
        )) => {
            assert!(reason.to_lowercase().contains("landlock"), "reason: {reason}");
        }
        other => panic!("expected SandboxRefused, got {other:?}"),
    }
}

#[test]
fn local_posture_runs_under_userland_floor_even_without_kernel_layers() {
    // The LOCAL single-tenant posture runs the developer's own code under the
    // userland-budget floor when kernel layers are absent (no refuse-to-run).
    let bin = recorder_child_path();
    let mut child = Command::new(&bin)
        .env("ZS_RECORDER_DISABLE_SECCOMP", "1")
        .env("ZS_RECORDER_FORCE_LANDLOCK_UNAVAILABLE", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(child_request(HAPPY_MIGRATION, false, false, false).as_bytes())
        .ok();
    let out = {
        use std::io::Read;
        let mut s = String::new();
        child.stdout.take().unwrap().read_to_string(&mut s).ok();
        s
    };
    child.wait().ok();
    let resp: zeroship_migrate::frontend::recorder_protocol::ChildResponse =
        serde_json::from_str(out.trim()).unwrap_or_else(|e| panic!("parse: {e}; raw={out}"));
    assert!(
        resp.ok,
        "local posture must still record under the userland floor: {out}"
    );
    assert!(!resp.report.landlock, "landlock must be forced unavailable");
}

// ===========================================================================
// 4. Per-invocation tenant isolation — separate children, no bleed.
// ===========================================================================

#[test]
fn concurrent_records_get_separate_sandbox_children() {
    // Two records that each stamp a DIFFERENT owner_app; if a child were pooled /
    // reused, the second would see the first's ambient recorder state. Run them
    // concurrently and assert each IR carries its OWN owner_app + ops (no bleed).
    let mk = |owner: &'static str, table: &'static str| {
        let src = format!(
            r#"import {{ table, t, genRandomUuid }} from "@zeroship/migrate";
               export function up() {{ table("{table}").create({{ columns: {{ id: t.int().notNull() }} }}); }}"#
        );
        std::thread::spawn(move || {
            let req = RecordRequest {
                ts_source: src,
                owner_app: owner.to_string(),
                name: owner.to_string(),
                posture: SandboxPosture::Hosted,
                budget: ResourceBudget::default(),
                allow_read_paths: vec![],
                schema_types_blob: None,
            };
            spawn_sandboxed_record(&req).expect("record")
        })
    };
    let a = mk("app_alpha", "alpha_tbl");
    let b = mk("app_beta", "beta_tbl");
    let ra = a.join().unwrap();
    let rb = b.join().unwrap();
    assert!(ra.ir_json.contains("alpha_tbl") && ra.ir_json.contains("app_alpha"));
    assert!(rb.ir_json.contains("beta_tbl") && rb.ir_json.contains("app_beta"));
    // No bleed: alpha's IR must NOT contain beta's table and vice-versa.
    assert!(
        !ra.ir_json.contains("beta_tbl"),
        "tenant bleed: alpha's child saw beta's ops"
    );
    assert!(
        !rb.ir_json.contains("alpha_tbl"),
        "tenant bleed: beta's child saw alpha's ops"
    );
}

// ===========================================================================
// 5. owner_app is server-stamped in Rust — untrusted up() CANNOT forge it.
//    (PR4a code-critic HIGH #1.)
// ===========================================================================

#[test]
fn untrusted_up_cannot_forge_owner_app() {
    // The attacker's migration tries to overwrite the server-injected owner via the
    // shared global the recorder used to read back. The emitted IR MUST still carry
    // the server's owner_app (app_attacker), NOT the forged app_VICTIM — because the
    // owner is stamped in Rust on the parsed IR after eval, not read from a global the
    // untrusted scope can reach.
    let forge_src = r#"
        import { table, t, genRandomUuid } from "@zeroship/migrate";
        export function up() {
          // Attempt to forge the tenant-identifying owner via every reachable name.
          try { globalThis.__zsOwnerApp = "app_VICTIM"; } catch (_) {}
          try { __zsOwnerApp = "app_VICTIM"; } catch (_) {}
          table("t").create({ columns: { id: t.int().notNull() } });
        }
    "#;
    let req = RecordRequest {
        ts_source: forge_src.to_string(),
        owner_app: "app_attacker".into(),
        name: "forge".into(),
        posture: SandboxPosture::Hosted,
        budget: ResourceBudget::default(),
        allow_read_paths: vec![],
        schema_types_blob: None,
    };
    let res = spawn_sandboxed_record(&req).expect("forge migration still records");
    assert!(
        res.ir_json.contains("app_attacker"),
        "IR must carry the SERVER owner_app (app_attacker), got: {}",
        res.ir_json
    );
    assert!(
        !res.ir_json.contains("app_VICTIM"),
        "untrusted up() forged owner_app into the IR: {}",
        res.ir_json
    );
    // The authoritative owner_app field, parsed structurally, must equal the server's.
    let env: serde_json::Value = serde_json::from_str(&res.ir_json).expect("ir envelope json");
    let owner = env
        .get("ir")
        .and_then(|ir| ir.get("owner_app"))
        .and_then(|v| v.as_str());
    assert_eq!(
        owner,
        Some("app_attacker"),
        "the structural owner_app must be the server-stamped value, got {owner:?}"
    );
}

// ===========================================================================
// 6. A large migration whose IR exceeds the OS pipe buffer must record (no
//    stdout deadlock). (PR4a code-critic HIGH #2.)
// ===========================================================================

#[test]
fn large_migration_ir_exceeding_pipe_buffer_records() {
    // Emit an IR envelope well past the ~64KB OS pipe buffer (and past 128KB). If the
    // parent waited-then-read, the child would block on write, never exit, and be
    // wall-killed as a SPURIOUS BUILD_RECORDER_BUDGET_EXCEEDED. With concurrent
    // draining the legitimate large migration records cleanly.
    let big_src = r#"
        import { table, t, genRandomUuid } from "@zeroship/migrate";
        export function up() {
          for (let i = 0; i < 4000; i++) {
            table("tbl_" + i).create({
              columns: {
                id: t.int().notNull(),
                label: t.text(),
              },
            });
          }
        }
    "#;
    let req = RecordRequest {
        ts_source: big_src.to_string(),
        owner_app: "app_big".into(),
        name: "big".into(),
        posture: SandboxPosture::Hosted,
        budget: ResourceBudget::default(),
        allow_read_paths: vec![],
        schema_types_blob: None,
    };
    let res = spawn_sandboxed_record(&req)
        .expect("large migration must record (not spurious BudgetExceeded)");
    assert!(
        res.ir_json.len() > 128 * 1024,
        "this test must exercise an IR larger than 128KB (got {} bytes) — \
         otherwise it does not prove the pipe-drain fix",
        res.ir_json.len()
    );
    assert!(res.ir_json.contains("tbl_0") && res.ir_json.contains("tbl_3999"));
}

// ===========================================================================
// 7. netns ALONE contains the network on the degraded floor (seccomp off,
//    netns the sole network boundary). (PR4a code-critic MED #4.)
// ===========================================================================

#[test]
fn netns_alone_contains_network_with_seccomp_disabled() {
    if !netns_available() {
        panic!(
            "CAPABILITY PRESENT BUT TEST WOULD SKIP: unprivileged netns is available — fix the gate"
        );
    }
    // Drive the child binary directly with seccomp DISABLED + the pre_exec netns
    // engaged, and a connect probe that — because seccomp is off so socket()/connect()
    // are NOT SIGSYS-killed — attempts a REAL outbound connect and exits with sentinel
    // 42 iff it could not reach the network (ENETUNREACH/EHOSTUNREACH/ETIMEDOUT). This
    // proves netns ALONE is a load-bearing network boundary, not merely a /proc config.
    let bin = recorder_child_path();
    assert!(bin.exists(), "recorder child missing at {}", bin.display());

    use std::os::unix::process::CommandExt;
    let mut cmd = Command::new(&bin);
    cmd.env("ZS_RECORDER_DISABLE_SECCOMP", "1")
        .env("ZS_RECORDER_PROBE", "net_connect")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // SAFETY: pre_exec closure issues only unshare (async-signal-safe).
    unsafe {
        cmd.pre_exec(|| {
            // netns only — no rlimits needed for this probe.
            zeroship_migrate::frontend::sandbox::pre_exec_netns()
        });
    }
    let mut child = cmd.spawn().expect("spawn child for netns probe");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(child_request(HAPPY_MIGRATION, false, true, false).as_bytes())
        .ok();
    let status = child.wait().expect("wait netns probe child");
    // Not signal-killed (seccomp is OFF, so no SIGSYS); contained => sentinel 42.
    assert_eq!(
        term_signal(&status),
        None,
        "with seccomp disabled the connect must NOT be signal-killed; got {status:?}"
    );
    assert_eq!(
        status.code(),
        Some(42),
        "netns ALONE must contain the outbound connect (ENETUNREACH) -> exit 42; \
         got {status:?} — code 0 means the connect REACHED the network (netns did not contain)"
    );
}

// ===========================================================================
// 8. CRITICAL thread-scope: a denied syscall on a thread created BEFORE the
//    lockdown is contained PROCESS-WIDE (SIGSYS), not just on main.
//    (PR4a code-critic CRITICAL #1.)
// ===========================================================================

#[test]
fn seccomp_kills_socket_from_a_pre_lockdown_thread_process_wide() {
    // The CRITICAL finding: seccomp `apply_filter` (flags=0) and landlock
    // `restrict_self` are CALLING-THREAD-ONLY, and they ran AFTER V8 spawned its
    // background (GC/compiler/platform) worker threads — so those pre-existing threads
    // stayed UNFILTERED. A resolver-bypassing escape scheduled onto one could
    // `socket()`/`fork()`/write freely. Empirically: a thread created before
    // `apply_seccomp()` called `socket()` successfully post-lockdown ("WORKER-THREAD
    // socket() SUCCEEDED fd=3").
    //
    // The fix: seccomp via `apply_filter_all_threads` (TSYNC) so the filter
    // synchronizes across EVERY existing thread, AND a single-threaded V8 platform so
    // no unfiltered background thread exists in the first place.
    //
    // This test models a pre-existing background thread with
    // `ZS_RECORDER_PRELOCK_THREAD` (spawns a parked worker thread BEFORE init/lockdown)
    // and the `thread_socket` probe (post-lockdown, signals THAT thread to issue
    // `socket()`). The assertion observes the CHILD TERMINATION CAUSE: the WHOLE
    // process must be SIGSYS-killed (KillProcess via TSYNC). A clean exit 0 means the
    // pre-lockdown thread's `socket()` SUCCEEDED on an unfiltered thread — the bug.
    if !seccomp_available() {
        panic!("CAPABILITY PRESENT BUT TEST WOULD SKIP: seccomp is available — fix the gate");
    }
    let bin = recorder_child_path();
    assert!(bin.exists(), "recorder child missing at {}", bin.display());
    let mut child = Command::new(&bin)
        .env("ZS_RECORDER_PRELOCK_THREAD", "1")
        .env("ZS_RECORDER_PROBE", "thread_socket")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn recorder child");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(child_request(HAPPY_MIGRATION, false, false, false).as_bytes())
        .ok();
    let status = child.wait().expect("wait child");
    assert_eq!(
        term_signal(&status),
        Some(libc::SIGSYS),
        "socket() from a PRE-LOCKDOWN thread must be SIGSYS-killed PROCESS-WIDE \
         (seccomp TSYNC across all threads); got {status:?} — exit 0 means the \
         pre-lockdown thread issued socket() on an UNFILTERED thread (the CRITICAL bug)"
    );
}

// ===========================================================================
// 8b. LANDLOCK thread-scope: the landlock half of the CRITICAL invariant.
//     (PR4a code-critic MED — pin the landlock half of the thread-scope invariant.)
//
//     seccomp's `thread_socket` (test 8) rides TSYNC, so a PRE-LOCKDOWN thread's
//     denied syscall is killed PROCESS-WIDE. landlock has NO TSYNC: `restrict_self`
//     binds ONLY the calling (main) thread. So landlock's whole-process coverage rests
//     ENTIRELY on the single-threaded-V8 invariant — only ONE thread may exist when
//     `restrict_self` runs. These two tests pin that half FAITHFULLY:
//
//       (a) On the MAIN (only) thread, an out-of-dir read IS landlock-denied (EACCES)
//           — `landlock_denies_out_of_dir_read` (test 1) already proves this.
//       (b) A PRE-LOCKDOWN sibling thread is NOT covered by landlock (its out-of-dir
//           read SUCCEEDS) — proving landlock alone canNOT contain a stray thread, which
//           is EXACTLY why (i) V8 must be single-threaded so no such thread exists, and
//           (ii) the fail-closed single-threaded assertion (test 9) REFUSES to run if
//           one ever does. If V8 ever silently went multi-threaded again, test 9 fires.
// ===========================================================================

#[test]
fn landlock_does_not_cover_a_pre_lockdown_thread_so_single_threaded_is_load_bearing() {
    // Ground-truth pin of the landlock no-TSYNC property. We spawn a sibling thread
    // BEFORE the lockdown (modeling a V8 background thread), then post-lockdown signal
    // IT to read an out-of-allow-list path. Because landlock's restrict_self covered
    // only the MAIN thread, the sibling's read is NOT denied (it succeeds, exit 0). This
    // is not a vulnerability in the shipped recorder — V8 is single-threaded so the
    // sibling never exists, and the fail-closed assertion (test 9) refuses to run if it
    // did. This test exists so that the LANDLOCK HALF of the invariant is asserted by a
    // live kernel observation, not prose: it documents WHY single-threaded-V8 is
    // load-bearing for the landlock layer (a pre-existing thread escapes it).
    if !landlock_available() {
        eprintln!("landlock not available — the read fs boundary is the resolver (degraded floor)");
        return;
    }
    if !seccomp_available() {
        panic!("CAPABILITY PRESENT BUT TEST WOULD SKIP: seccomp is available — fix the gate");
    }
    let bin = recorder_child_path();
    assert!(bin.exists(), "recorder child missing at {}", bin.display());
    let mut child = Command::new(&bin)
        .env("ZS_RECORDER_PRELOCK_THREAD", "1")
        .env("ZS_RECORDER_PROBE", "thread_read_outside")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn recorder child");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(child_request(HAPPY_MIGRATION, false, false, false).as_bytes())
        .ok();
    let status = child.wait().expect("wait child");
    // The sibling thread's read is a READ-only open (passes the seccomp openat
    // arg-filter), so it is NOT signal-killed; landlock would be the only thing that
    // could deny it, and landlock does NOT cover the pre-lockdown sibling -> exit 0.
    assert_eq!(
        term_signal(&status),
        None,
        "a read-only open must not be signal-killed (seccomp permits read opens); got {status:?}"
    );
    assert_eq!(
        status.code(),
        Some(0),
        "the PRE-LOCKDOWN sibling thread's out-of-dir read must SUCCEED (exit 0): landlock \
         has no TSYNC and covered only the main thread. This is the ground-truth proof that \
         the landlock half relies on the single-threaded invariant — NOT on landlock \
         covering extra threads. (exit 42 would mean landlock unexpectedly covered the \
         sibling; exit 11/13 a probe error.) The fail-closed assertion (test 9) is what \
         prevents this sibling from ever existing in the shipped recorder."
    );
}

// ===========================================================================
// 9. FAIL-CLOSED: the recorder REFUSES to run if it is not single-threaded.
//    (PR4a code-critic LOW — runtime assertion that the recorder V8 is
//    single-threaded; the landlock half of the CRITICAL invariant has NO TSYNC.)
// ===========================================================================

#[test]
fn recorder_refuses_to_run_if_not_single_threaded() {
    // The recorder's whole-process landlock coverage depends on the single-threaded-V8
    // invariant (landlock restrict_self binds only the calling thread). If a stray
    // thread exists before lockdown, an out-of-dir read on it would stay UNFILTERED.
    // The fail-closed guard (assert_recorder_single_threaded) must REFUSE TO RUN rather
    // than silently degrade. We force a second thread via the ZS_RECORDER_FORCE_MULTITHREAD
    // test seam and assert the child refuses with a SandboxRefused error that names the
    // single-threaded invariant.
    let bin = recorder_child_path();
    assert!(bin.exists(), "recorder child missing at {}", bin.display());
    let mut child = Command::new(&bin)
        .env("ZS_RECORDER_FORCE_MULTITHREAD", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn recorder child");
    child
        .stdin
        .take()
        .unwrap()
        // LOCAL posture so the refusal is purely the single-threaded guard, not the
        // hosted floor.
        .write_all(child_request(HAPPY_MIGRATION, false, false, false).as_bytes())
        .ok();
    let out = {
        use std::io::Read;
        let mut s = String::new();
        child.stdout.take().unwrap().read_to_string(&mut s).ok();
        s
    };
    let status = child.wait().expect("wait child");
    // It must not have been signal-killed — it refuses cleanly via the structured error.
    assert_eq!(
        term_signal(&status),
        None,
        "the single-threaded guard must refuse via a structured error, not a signal kill; got {status:?}"
    );
    let resp: zeroship_migrate::frontend::recorder_protocol::ChildResponse =
        serde_json::from_str(out.trim()).unwrap_or_else(|e| panic!("parse resp: {e}; raw={out}"));
    assert!(
        !resp.ok,
        "a multi-threaded recorder must FAIL CLOSED (refuse), got ok response: {out}"
    );
    match resp.error {
        Some(zeroship_migrate::frontend::recorder_protocol::ChildError::SandboxRefused(reason)) => {
            assert!(
                reason.contains("single-threaded") || reason.contains("threads exist"),
                "the refusal must name the single-threaded invariant, got: {reason}"
            );
        }
        other => panic!("expected SandboxRefused (single-threaded fail-closed), got {other:?}"),
    }
}

// ===========================================================================
// 10. The `process` stub invariants the recorder relies on are PINNED.
//     (PR4a code-critic LOW — pin the `process` stub invariants.)
//
//     setup_globals exposes a `process` object to the untrusted migration JS. It is
//     benign TODAY: `process.env` is empty (the recorder injects no env vars),
//     `process.exit` is undefined (so untrusted up() cannot terminate the child early /
//     short-circuit recording), and there are no Node fs/net/child_process bindings.
//     A future runtime enrichment of the process stub (e.g. adding process.exit, or
//     populating process.env, or wiring node bindings) would silently widen the
//     untrusted surface. This test makes such a change FAIL LOUDLY at the recorder.
// ===========================================================================

#[test]
fn recorder_process_stub_invariants_are_pinned() {
    // up() probes the process stub and encodes each invariant into a created-table name,
    // so the recorded IR carries the observed state. We assert the IR shows the safe
    // values. A regression (e.g. process.exit becomes a function) flips a flag and the
    // assertion fails. (We avoid calling process.exit even if present — we only read
    // `typeof`.)
    let probe_src = r#"
        import { table, t, genRandomUuid } from "@zeroship/migrate";
        export function up() {
          const p = globalThis.process;
          const envIsEmpty = !!p && typeof p.env === "object" && p.env !== null
              && Object.keys(p.env).length === 0;
          const exitUndefined = !p || typeof p.exit === "undefined";
          // No Node-only host bindings leaked onto globalThis.
          const noNodeBindings =
              typeof globalThis.require === "undefined" &&
              typeof globalThis.module === "undefined" &&
              typeof globalThis.__dirname === "undefined" &&
              typeof globalThis.Buffer === "undefined";
          table("env_empty_" + envIsEmpty).create({ columns: { id: t.int().notNull() } });
          table("exit_undef_" + exitUndefined).create({ columns: { id: t.int().notNull() } });
          table("no_node_" + noNodeBindings).create({ columns: { id: t.int().notNull() } });
        }
    "#;
    let req = RecordRequest {
        ts_source: probe_src.to_string(),
        owner_app: "app_proc".into(),
        name: "proc_probe".into(),
        posture: SandboxPosture::Hosted,
        budget: ResourceBudget::default(),
        allow_read_paths: vec![],
        schema_types_blob: None,
    };
    let res = spawn_sandboxed_record(&req).expect("process-stub probe migration records");
    assert!(
        res.ir_json.contains("env_empty_true"),
        "process.env must be EMPTY ({{}}) in the recorder — a future enrichment that \
         populates it widens the untrusted surface; IR: {}",
        res.ir_json
    );
    assert!(
        res.ir_json.contains("exit_undef_true"),
        "process.exit must be UNDEFINED in the recorder (untrusted up() must not be able \
         to terminate the child / short-circuit recording); IR: {}",
        res.ir_json
    );
    assert!(
        res.ir_json.contains("no_node_true"),
        "no Node fs/net/require/Buffer bindings may leak onto globalThis in the recorder; \
         IR: {}",
        res.ir_json
    );
}

// A static assertion that the child binary path resolves — a missing child binary is
// a hard failure, never a skip.
#[test]
fn recorder_child_binary_exists() {
    let p: PathBuf = recorder_child_path();
    assert!(
        p.exists(),
        "the recorder child binary must be built before the e2e (at {})",
        p.display()
    );
}
