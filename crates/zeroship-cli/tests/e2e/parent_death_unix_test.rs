//! An orphaned `zeroship serve` must die on every Unix the CLI ships for.
//!
//! Linux arms the kernel's parent-death signal, and macOS, which has none,
//! polls for its parent; both are this one contract. The runtime runs under an
//! intermediate `sh`, which the test kills outright as a crashed or killed dev
//! server would be. The runtime inherits the shell's stdout and stderr, so the
//! test reads both through one pair of pipes, and the pipes close only when
//! the runtime has exited too: that closing is the observation, with no
//! process table to read and no pid to reuse.

#![cfg(unix)]

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use super::child_output::ChildOutput;

const BIN: &str = env!("CARGO_BIN_EXE_zeroship");

/// The line the runtime prints once its workflow host is up, the last step
/// before it serves.
const READY: &str = "workflow host ready";

/// The shell, the runtime it started, and their output.
struct Spawned {
    shell: Child,
    runtime_pid: Option<String>,
    output: ChildOutput,
}

impl Drop for Spawned {
    fn drop(&mut self) {
        let _ = self.shell.kill();
        let _ = self.shell.wait();
        if let Some(pid) = &self.runtime_pid {
            let _ = Command::new("kill")
                .args(["-9", pid])
                .stderr(Stdio::null())
                .status();
        }
    }
}

/// `zeroship serve` started by `sh` in `dir`, the shell's own pid named as its
/// parent when `guarded`, and running.
fn spawn_under_shell(dir: &Path, guarded: bool) -> Spawned {
    std::fs::write(
        dir.join("app.js"),
        "export default { fetch() { return new Response('ok'); } };\n",
    )
    .expect("write app.js");
    let export = if guarded {
        "export ZEROSHIP_DIE_WITH_PARENT=$$\n"
    } else {
        ""
    };
    let script = format!(
        "{export}\"$1\" serve app.js --port=0 --workers=1 &\necho \"runtime-pid=$!\"\nwait\n"
    );
    let mut shell = Command::new("sh")
        .arg("-c")
        .arg(&script)
        .arg("sh")
        .arg(BIN)
        .env_remove("ZEROSHIP_DIE_WITH_PARENT")
        .env_remove("DATABASE_URL")
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn sh");
    let output = ChildOutput::capture(&mut shell);
    let mut spawned = Spawned {
        shell,
        runtime_pid: None,
        output,
    };
    assert!(
        spawned.output.wait_for(READY, Duration::from_mins(1)),
        "the runtime never came up; output:\n{}",
        spawned.output.text()
    );
    spawned.runtime_pid = spawned
        .output
        .text()
        .lines()
        .find_map(|line| line.strip_prefix("runtime-pid="))
        .map(str::to_owned);
    assert!(spawned.runtime_pid.is_some(), "sh named no runtime pid");
    spawned
}

#[test]
fn a_runtime_dies_when_the_shell_that_started_it_is_killed() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut spawned = spawn_under_shell(dir.path(), true);

    spawned.shell.kill().expect("kill the shell");
    spawned.shell.wait().expect("reap the shell");

    assert!(
        spawned.output.closes_within(Duration::from_secs(15)),
        "the runtime SURVIVED its parent and still holds {}; output:\n{}",
        dir.path().join(".zeroship").display(),
        spawned.output.text()
    );
}

/// The one-variable control: the same shell killed the same way, with no
/// parent named. A runtime that died here too would make the death above
/// evidence of a kill cascade rather than of the guard, and a `zeroship
/// serve` typed into a terminal must not die because some shell exited.
#[test]
fn control_an_unguarded_runtime_survives_its_shell() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut spawned = spawn_under_shell(dir.path(), false);

    spawned.shell.kill().expect("kill the shell");
    spawned.shell.wait().expect("reap the shell");

    assert!(
        !spawned.output.closes_within(Duration::from_secs(5)),
        "an UNGUARDED runtime exited with its shell; output:\n{}",
        spawned.output.text()
    );
}
