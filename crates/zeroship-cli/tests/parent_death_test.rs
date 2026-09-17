//! An orphaned `zeroship serve` must actually die. Kernel behaviour, driven
//! against the REAL binary.
//!
//! # Why this is a crate test
//!
//! Everything else about the dev server is a seam - two processes, two ports,
//! two backends - and a crate suite is blind to it by construction. This is
//! not. The proposition here is a single-process property of one binary: given
//! `ZEROSHIP_DIE_WITH_PARENT=<ppid>`, losing the parent kills it. Nothing about
//! vite, the manifest, or the deployed tier participates. This test does NOT
//! cover the SEAM half - that vite actually sets the variable, spelled the
//! same way, on the child it spawns - and would pass with the plugin never
//! setting the variable at all.
//!
//! # Shape
//!
//! `sh -c '<runtime> & wait'` gives a killable intermediate that stays alive as
//! the runtime's parent, so SIGKILLing `sh` reproduces exactly what happened in
//! task #221: a parent killed by pid, with no chance to run teardown code.
//! Liveness is read from `/proc/<pid>` and `/proc/<pid>/comm`, never from
//! `pgrep -f` - a pattern that appears in this test's own command line matches
//! the test itself and always reports RUNNING.

#![cfg(target_os = "linux")]

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_zeroship");

/// How the spawned runtime is told about its parent.
enum Guard {
    /// No `ZEROSHIP_DIE_WITH_PARENT` at all - a bare `zeroship serve`.
    Unset,
    /// The variable set to the real parent pid (`$$` of the wrapping `sh`).
    RealParent,
    /// The variable set to a live pid that is NOT the parent - the "parent
    /// already exited before we armed" arm, made deterministic.
    WrongPid(u32),
}

// ── liveness, read from /proc ──────────────────────────────────────────────

/// True while `pid` is a live (non-zombie) `zeroship` process.
///
/// The `comm` check is not decoration: a bare `/proc/<pid>` existence test
/// would call a REUSED pid alive and silently invert the verdict of every
/// assertion below. The zombie check matters because an orphan whose new parent
/// has not reaped it still has a `/proc` entry.
fn runtime_alive(pid: u32) -> bool {
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    // `stat` is "<pid> (<comm>) <state> ..." and comm can contain spaces or
    // parens, so the state is the field after the LAST ')'.
    let state = stat
        .rsplit_once(')')
        .and_then(|(_, rest)| rest.split_whitespace().next());
    if state == Some("Z") {
        return false;
    }
    match std::fs::read_to_string(format!("/proc/{pid}/comm")) {
        Ok(comm) => comm.trim() == "zeroship",
        Err(_) => false,
    }
}

fn wait_until(timeout: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if cond() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// A free-ish TCP port. `zeroship serve` refuses a taken port and exits, so a
/// clash would look like a dead runtime; the boot assertion below quotes the
/// runtime's own log, which names the clash if it happens.
fn free_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral");
    l.local_addr().expect("local_addr").port()
}

/// Kills anything left behind, on the panic path as well as the happy one. A
/// leaked orphan from THIS test is the very thing under test, and it would hold
/// its temp dir's redb file until the machine is rebooted.
struct Reaper(Vec<u32>);
impl Drop for Reaper {
    fn drop(&mut self) {
        for pid in &self.0 {
            // stderr silenced: on the happy path the guard has already killed
            // it, and `kill: No such process` interleaved with the test names
            // reads like a failure.
            let _ = Command::new("kill")
                .arg("-9")
                .arg(pid.to_string())
                .stderr(Stdio::null())
                .status();
        }
    }
}

// ── the runtime under an intermediate shell ────────────────────────────────

struct Spawned {
    /// The `sh` process. Killing this is what orphans the runtime.
    shell: Child,
    /// The `zeroship serve` process `sh` forked.
    runtime_pid: u32,
    dir: PathBuf,
}

impl Spawned {
    fn log(&self) -> String {
        std::fs::read_to_string(self.dir.join("runtime.log")).unwrap_or_default()
    }
}

fn spawn_under_shell(dir: &Path, guard: Guard) -> Spawned {
    let mut app = std::fs::File::create(dir.join("app.js")).expect("write app.js");
    app.write_all(b"export default { fetch() { return new Response('ok'); } };\n")
        .expect("write app.js body");
    drop(app);

    let export = match guard {
        Guard::Unset => String::new(),
        // `$$` inside `sh -c` is sh's own pid, which IS the runtime's parent.
        Guard::RealParent => "export ZEROSHIP_DIE_WITH_PARENT=$$\n".to_string(),
        Guard::WrongPid(pid) => format!("export ZEROSHIP_DIE_WITH_PARENT={pid}\n"),
    };
    let port = free_port();
    let script = format!(
        "{export}\"$1\" serve app.js --port={port} --workers=1 >runtime.log 2>&1 &\n\
         echo $! > runtime.pid\n\
         wait\n"
    );

    let shell = Command::new("sh")
        .arg("-c")
        .arg(&script)
        .arg("sh") // $0
        .arg(BIN) // $1
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn sh");

    let pid_file = dir.join("runtime.pid");
    assert!(
        wait_until(Duration::from_secs(10), || pid_file.exists()),
        "sh never recorded the runtime pid"
    );
    let runtime_pid: u32 = std::fs::read_to_string(&pid_file)
        .expect("read runtime.pid")
        .trim()
        .parse()
        .expect("runtime.pid is a pid");

    Spawned {
        shell,
        runtime_pid,
        dir: dir.to_path_buf(),
    }
}

/// Block until the runtime has opened its state dir, which is the resource an
/// orphan holds hostage. Asserting on THIS line rather than on process
/// existence is what makes "it died" mean something: a process that never got
/// as far as the redb file would also be gone at the end.
fn assert_booted(s: &Spawned) {
    let dir = s.dir.clone();
    let up = wait_until(Duration::from_secs(30), || {
        std::fs::read_to_string(dir.join("runtime.log"))
            .map(|l| l.contains("kv binding registered"))
            .unwrap_or(false)
    });
    assert!(
        up,
        "runtime never opened its state dir; its log was:\n{}",
        s.log()
    );
    assert!(
        runtime_alive(s.runtime_pid),
        "pid {} is not a live zeroship process right after boot",
        s.runtime_pid
    );
}

/// `kill -9`, not a `libc::kill` call, deliberately: this test must compile and
/// run against a binary that has never heard of the guard, so it borrows
/// nothing from the crate under test - not even a dependency.
fn sigkill(pid: u32) {
    let st = Command::new("kill")
        .arg("-9")
        .arg(pid.to_string())
        .status()
        .expect("run kill");
    assert!(st.success(), "could not SIGKILL {pid}");
}

// ── the tests ──────────────────────────────────────────────────────────────

/// THE ONE THAT MATTERS. A runtime whose parent is SIGKILLed - no teardown, no
/// signal handler, nothing the parent can run - must be gone.
#[test]
fn an_orphaned_runtime_dies_when_its_parent_is_sigkilled() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut s = spawn_under_shell(dir.path(), Guard::RealParent);
    let _reaper = Reaper(vec![s.runtime_pid]);
    assert_booted(&s);

    let shell_pid = s.shell.id();
    sigkill(shell_pid);
    let _ = s.shell.wait();

    let pid = s.runtime_pid;
    assert!(
        wait_until(Duration::from_secs(15), || !runtime_alive(pid)),
        "pid {pid} SURVIVED its parent ({shell_pid}) - this is task #221: it still holds \
         {}/.zeroship/kv.redb, and the next dev server for this directory cannot boot on \
         ANY port. Runtime log:\n{}",
        dir.path().display(),
        s.log()
    );
}

/// THE ONE-VARIABLE CONTROL, and it is the reason the test above means what it
/// says. Same script, same `sh`, same SIGKILL - only the environment variable
/// differs. If a bare `zeroship serve` died here too, the death above would be
/// evidence of a process-group cascade or of cargo reaping its descendants, not
/// of the guard.
///
/// It also pins the gate itself: a `zeroship serve` typed into a terminal must
/// NOT die because a shell exited.
#[test]
fn control_an_unguarded_runtime_survives_its_parent() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut s = spawn_under_shell(dir.path(), Guard::Unset);
    let _reaper = Reaper(vec![s.runtime_pid]);
    assert_booted(&s);

    sigkill(s.shell.id());
    let _ = s.shell.wait();

    // Long enough that a delayed death would be seen: the guarded case above
    // completes in well under a second.
    std::thread::sleep(Duration::from_secs(5));
    assert!(
        runtime_alive(s.runtime_pid),
        "an UNGUARDED runtime died with its parent. Either the guard is armed \
         unconditionally - which breaks a bare `zeroship serve` - or this whole test is \
         measuring a kill cascade rather than the guard. Runtime log:\n{}",
        s.log()
    );
}

/// The arm-time race, made deterministic.
///
/// `PR_SET_PDEATHSIG` is armed after `exec`, so a parent that dies in the
/// window between `spawn` and the `prctl` call is never signalled - the death
/// event has already passed. Losing that race on purpose is not reproducible;
/// naming a live pid that is not the parent puts the process in exactly the
/// state the race produces (recorded parent != actual parent) with no timing.
///
/// The runtime must exit BEFORE opening the state dir, which is why the guard
/// is armed first in `main`.
#[test]
fn a_runtime_whose_recorded_parent_is_not_its_parent_exits_at_once() {
    let dir = tempfile::tempdir().expect("tempdir");
    // This test process: certainly alive, certainly the runtime's GRANDparent
    // (sh sits in between), so it can never be mistaken for the parent.
    let grandparent = std::process::id();
    let mut s = spawn_under_shell(dir.path(), Guard::WrongPid(grandparent));
    let _reaper = Reaper(vec![s.runtime_pid]);

    let pid = s.runtime_pid;
    assert!(
        wait_until(Duration::from_secs(15), || !runtime_alive(pid)),
        "runtime {pid} kept running with a recorded parent ({grandparent}) that is not its \
         parent. Log:\n{}",
        s.log()
    );
    let log = s.log();
    assert!(
        log.contains("exited before the parent-death guard was armed"),
        "the runtime exited without saying why; an unexplained instant exit reads as \
         'never started'. Log:\n{log}"
    );
    assert!(
        !log.contains("kv binding registered"),
        "the runtime opened its state dir before deciding to exit - the guard must be armed \
         before anything is held. Log:\n{log}"
    );
    let _ = s.shell.wait();
}
