//! An orphaned `zeroship serve` must die on Windows too.
//!
//! Windows has no parent-death signal, so the guard waits on the process
//! `ZEROSHIP_DIE_WITH_PARENT` names. That makes the named process, not the
//! spawning one, what the runtime lives by, so this suite names a stand-in it
//! can kill outright - `TerminateProcess`, which runs nothing in the stand-in,
//! as a crash or a killed dev server would - while the test stays the
//! runtime's real parent and polls it directly.

#![cfg(windows)]

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_zeroship");

/// Kills what is left on the panic path as well as the happy one.
struct Reaper(Vec<Child>);
impl Drop for Reaper {
    fn drop(&mut self) {
        for child in &mut self.0 {
            let _ = child.kill();
            let _ = child.wait();
        }
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

/// A process that outlives the test unless killed, standing in for the dev
/// server that spawned the runtime.
fn stand_in() -> Child {
    Command::new("ping")
        .args(["-n", "600", "127.0.0.1"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn the stand-in parent")
}

/// `zeroship serve` in `dir`, logging to `runtime.log`, guarded by `parent`
/// when one is named.
fn spawn_runtime(dir: &Path, parent: Option<u32>) -> Child {
    std::fs::write(
        dir.join("app.js"),
        "export default { fetch() { return new Response('ok'); } };\n",
    )
    .expect("write app.js");
    let log = std::fs::File::create(dir.join("runtime.log")).expect("create runtime.log");
    let mut command = Command::new(BIN);
    command
        .args(["serve", "app.js", "--port=0", "--workers=1"])
        .env_remove("DATABASE_URL")
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(log.try_clone().expect("share the log"))
        .stderr(log);
    match parent {
        Some(pid) => command.env("ZEROSHIP_DIE_WITH_PARENT", pid.to_string()),
        None => command.env_remove("ZEROSHIP_DIE_WITH_PARENT"),
    };
    command.spawn().expect("spawn zeroship serve")
}

fn log(dir: &Path) -> String {
    std::fs::read_to_string(dir.join("runtime.log")).unwrap_or_default()
}

/// Block until the runtime has opened its state dir, the resource an orphan
/// holds hostage, so "it exited" below means it exited holding it.
fn assert_booted(dir: &Path, runtime: &mut Child) {
    assert!(
        wait_until(Duration::from_mins(1), || log(dir).contains("kv binding registered")),
        "the runtime never opened its state dir; log:\n{}",
        log(dir)
    );
    assert!(
        runtime.try_wait().expect("poll the runtime").is_none(),
        "the runtime exited right after boot; log:\n{}",
        log(dir)
    );
}

#[test]
fn a_runtime_dies_when_the_parent_it_names_is_killed() {
    let dir = tempfile::tempdir().expect("tempdir");
    let parent = stand_in();
    let parent_pid = parent.id();
    let runtime = spawn_runtime(dir.path(), Some(parent_pid));
    let mut reaper = Reaper(vec![parent, runtime]);
    assert_booted(dir.path(), &mut reaper.0[1]);

    reaper.0[0].kill().expect("kill the stand-in parent");
    reaper.0[0].wait().expect("reap the stand-in parent");

    let runtime = &mut reaper.0[1];
    assert!(
        wait_until(Duration::from_secs(15), || runtime
            .try_wait()
            .expect("poll the runtime")
            .is_some()),
        "the runtime SURVIVED the parent it names ({parent_pid}) and still holds {}; log:\n{}",
        dir.path().join(".zeroship").display(),
        log(dir.path())
    );
    assert!(
        log(dir.path()).contains(&format!("parent {parent_pid} exited - exiting with it")),
        "the runtime exited without saying why; log:\n{}",
        log(dir.path())
    );
}

/// The one-variable control: the same stand-in killed the same way, with no
/// parent named. A runtime that died here too would make the death above
/// evidence of something other than the guard, and a `zeroship serve` typed
/// into a terminal must not die because some other process did.
#[test]
fn control_an_unguarded_runtime_survives_the_stand_in() {
    let dir = tempfile::tempdir().expect("tempdir");
    let parent = stand_in();
    let runtime = spawn_runtime(dir.path(), None);
    let mut reaper = Reaper(vec![parent, runtime]);
    assert_booted(dir.path(), &mut reaper.0[1]);

    reaper.0[0].kill().expect("kill the stand-in parent");
    reaper.0[0].wait().expect("reap the stand-in parent");

    let runtime = &mut reaper.0[1];
    assert!(
        !wait_until(Duration::from_secs(5), || runtime
            .try_wait()
            .expect("poll the runtime")
            .is_some()),
        "an UNGUARDED runtime exited after an unrelated process was killed; log:\n{}",
        log(dir.path())
    );
}
