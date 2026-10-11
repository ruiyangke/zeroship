//! An orphaned `zeroship serve` must die on Windows too.
//!
//! Windows has no parent-death signal, so the guard waits on the process
//! `ZEROSHIP_DIE_WITH_PARENT` names. That makes the named process, not the
//! spawning one, what the runtime lives by, so this suite names a stand-in it
//! can kill outright - `TerminateProcess`, which runs nothing in the stand-in,
//! as a crash or a killed dev server would - while the test stays the
//! runtime's real parent and reads its output. The output closes when the
//! runtime exits, which is how both tests observe it.

#![cfg(windows)]

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use super::child_output::ChildOutput;

const BIN: &str = env!("CARGO_BIN_EXE_zeroship");

/// The line the runtime prints once its workflow host is up, the last step
/// before it serves.
const READY: &str = "workflow host ready";

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

/// `zeroship serve` in `dir`, guarded by `parent` when one is named, and its
/// output once it is up.
fn spawn_runtime(dir: &Path, parent: Option<u32>) -> (Child, ChildOutput) {
    std::fs::write(
        dir.join("app.js"),
        "export default { fetch() { return new Response('ok'); } };\n",
    )
    .expect("write app.js");
    let mut command = Command::new(BIN);
    command
        .args(["serve", "app.js", "--port=0", "--workers=1"])
        .env_remove("DATABASE_URL")
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    match parent {
        Some(pid) => command.env("ZEROSHIP_DIE_WITH_PARENT", pid.to_string()),
        None => command.env_remove("ZEROSHIP_DIE_WITH_PARENT"),
    };
    let mut runtime = command.spawn().expect("spawn zeroship serve");
    let mut output = ChildOutput::capture(&mut runtime);
    assert!(
        output.wait_for(READY, Duration::from_mins(1)),
        "the runtime never came up; output:\n{}",
        output.text()
    );
    (runtime, output)
}

#[test]
fn a_runtime_dies_when_the_parent_it_names_is_killed() {
    let dir = tempfile::tempdir().expect("tempdir");
    let parent = stand_in();
    let parent_pid = parent.id();
    let mut reaper = Reaper(vec![parent]);
    let (runtime, mut output) = spawn_runtime(dir.path(), Some(parent_pid));
    reaper.0.push(runtime);

    reaper.0[0].kill().expect("kill the stand-in parent");
    reaper.0[0].wait().expect("reap the stand-in parent");

    assert!(
        output.closes_within(Duration::from_secs(15)),
        "the runtime SURVIVED the parent it names ({parent_pid}) and still holds {}; output:\n{}",
        dir.path().join(".zeroship").display(),
        output.text()
    );
    assert!(
        output.text().contains(&format!("parent {parent_pid} exited - exiting with it")),
        "the runtime exited without saying why; output:\n{}",
        output.text()
    );
}

/// The one-variable control: the same stand-in killed the same way, with no
/// parent named. A runtime that died here too would make the death above
/// evidence of something other than the guard, and a `zeroship serve` typed
/// into a terminal must not die because some other process did.
#[test]
fn control_an_unguarded_runtime_survives_the_stand_in() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut reaper = Reaper(vec![stand_in()]);
    let (runtime, mut output) = spawn_runtime(dir.path(), None);
    reaper.0.push(runtime);

    reaper.0[0].kill().expect("kill the stand-in parent");
    reaper.0[0].wait().expect("reap the stand-in parent");

    assert!(
        !output.closes_within(Duration::from_secs(5)),
        "an UNGUARDED runtime exited after an unrelated process was killed; output:\n{}",
        output.text()
    );
}
