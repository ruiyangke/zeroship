//! The two lifetime measurements every fixture built on the reaper owes: its
//! container is gone after the owning process exits, and after that process is
//! killed by `SIGKILL` while the container is still starting.
//!
//! Both drive a CHILD TEST: one `#[test]` of the caller's own binary, run alone in a
//! child process. The child calls [`report_owner`] before it asks its fixture for a
//! container and [`report_container`] once the fixture has one; in an ordinary run
//! of the binary it is just another test of the fixture. The parent finds the child's
//! container by the child's [`OWNER_LABEL`](super::OWNER_LABEL) - which also proves,
//! before anything is asserted about an absence, that the query it reads absences
//! through can see a container that is there - and then requires Docker to stop
//! listing it.

use std::io::{BufRead, BufReader, Lines};
use std::process::{Child, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

use super::{process_owner, DockerCli, OWNER_LABEL};

/// The line a child prints its owner label on.
const OWNER_LINE: &str = "CONTAINER_LIFETIME_OWNER=";

/// The line a child prints its container id on.
const CONTAINER_LINE: &str = "CONTAINER_LIFETIME_CONTAINER=";

/// How long a child gets to create its container, including an image pull.
const CREATION_BOUND: Duration = Duration::from_mins(5);

/// How long the reaper gets to remove a container after its process has ended.
const REMOVAL_BOUND: Duration = Duration::from_secs(90);

/// Child side: name this process's owner label. Call it before the fixture starts
/// its container.
pub fn report_owner() {
    println!("{OWNER_LINE}{}", process_owner());
}

/// Child side: name the container the fixture started.
pub fn report_container(id: &str) {
    println!("{CONTAINER_LINE}{id}");
}

/// Run `child_test` to completion in a child process and require its container to
/// be removed once the child has exited.
///
/// # Panics
/// When the child fails, does not report, or its container outlives it.
pub fn assert_removed_after_the_child_exits(child_test: &str) {
    let (mut child, mut lines) = spawn(child_test);
    let owner = owner(&mut lines);
    let id = created(&owner);
    let rest: Vec<String> = lines.map_while(Result::ok).collect();
    let status = child.wait().expect("wait for the child test");
    assert!(
        status.success(),
        "the child test {child_test} failed ({status}):\n{}",
        rest.join("\n")
    );
    let reported = rest
        .iter()
        .find_map(|line| line.split_once(CONTAINER_LINE).map(|(_, id)| id.trim()))
        .unwrap_or_else(|| {
            panic!(
                "the child test {child_test} did not report its container:\n{}",
                rest.join("\n")
            )
        });
    assert_eq!(
        reported, id,
        "the container the child reported is the one its owner label found"
    );
    wait_until_removed(&id, "the child exited");
}

/// Start `child_test` in a child process, SIGKILL it once its container exists and
/// before the fixture has finished starting it, and require the container to be
/// removed.
///
/// # Panics
/// When the child does not create a container, finishes starting it before the kill
/// lands, or its container outlives it.
pub fn assert_removed_after_a_kill_during_startup(child_test: &str) {
    let (mut child, mut lines) = spawn(child_test);
    let owner = owner(&mut lines);
    let id = created(&owner);
    child.kill().expect("SIGKILL the child test");
    child.wait().expect("reap the child test");
    let rest: Vec<String> = lines.map_while(Result::ok).collect();
    assert!(
        !rest.iter().any(|line| line.contains(CONTAINER_LINE)),
        "the child's fixture finished starting before the kill landed, so this run did \
         not measure a kill during startup:\n{}",
        rest.join("\n")
    );
    wait_until_removed(&id, "the child was killed while its container was starting");
}

/// Docker's view of one container: `Some(status)` while it exists, `None` once it is
/// gone.
///
/// # Panics
/// When the docker CLI cannot answer.
pub fn container_status(id: &str) -> Option<String> {
    let output = Command::new("docker")
        .args(["container", "inspect", "--format", "{{.State.Status}}", id])
        .output()
        .expect("run the docker CLI");
    if output.status.success() {
        return Some(String::from_utf8_lossy(&output.stdout).trim().to_string());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("No such container") || stderr.contains("No such object"),
        "docker could not answer whether {id} exists: {stderr}"
    );
    None
}

fn spawn(child_test: &str) -> (Child, Lines<BufReader<ChildStdout>>) {
    let mut child = Command::new(std::env::current_exe().expect("locate this test binary"))
        .args([child_test, "--exact", "--nocapture", "--test-threads=1"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("run one test of this binary in a child process");
    let lines = BufReader::new(child.stdout.take().expect("piped stdout")).lines();
    (child, lines)
}

fn owner(lines: &mut Lines<BufReader<ChildStdout>>) -> String {
    lines
        .by_ref()
        .map_while(Result::ok)
        .find_map(|line| {
            line.split_once(OWNER_LINE)
                .map(|(_, owner)| owner.trim().to_string())
        })
        .expect("the child test reports its owner label before it asks for a container")
}

/// The first container carrying `owner`, once the daemon has created it.
fn created(owner: &str) -> String {
    let docker = DockerCli::system();
    let deadline = Instant::now() + CREATION_BOUND;
    loop {
        let ids = docker
            .labelled(OWNER_LABEL, owner)
            .expect("the docker CLI lists containers by label");
        if let Some(id) = ids.into_iter().next() {
            return id;
        }
        assert!(
            Instant::now() < deadline,
            "the child test created no container within {CREATION_BOUND:?}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn wait_until_removed(id: &str, why: &str) {
    let deadline = Instant::now() + REMOVAL_BOUND;
    while let Some(state) = container_status(id) {
        assert!(
            Instant::now() < deadline,
            "container {id} is still listed ({state}) {REMOVAL_BOUND:?} after {why}; \
             its reaper did not remove it"
        );
        std::thread::sleep(Duration::from_millis(250));
    }
}
