//! The two lifetime measurements every shared-server fixture owes: its
//! container is removed once the process holding its lease has exited, and once
//! that process was killed by `SIGKILL` while the container was still starting.
//!
//! Both drive a CHILD TEST: one `#[test]` of the caller's own binary, ignored so
//! an ordinary run does not boot a throwaway server for it, run alone in a child
//! process. The child is handed a throwaway scope directory on standard input,
//! never in the environment; it reads it with [`child_scope`], joins the
//! fixture's own recipe at that scope, and calls [`report_container`] once the
//! fixture has its server. The parent finds the child's container by the scope's
//! [`DIR_LABEL`](crate::shared::DIR_LABEL) while the child runs - which also
//! proves, before anything is asserted about an absence, that the query it
//! reads absences through can see a container that is there - and then requires
//! the daemon to stop listing it.
//!
//! A throwaway scope is what makes the removal observable. The worktree scope a
//! fixture normally joins is leased by every test process of a run, so its
//! server outliving one child is the design; the scope here is leased by the
//! child alone, so once the child is gone the only thing between the container
//! and its removal is the mechanism under test.

use std::io::{BufRead, BufReader, Lines, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::shared::{self, Scope};

/// The idle grace a child's throwaway scope is started with.
pub const GRACE: Duration = Duration::from_secs(2);

/// The line a child prints its container id on.
const CONTAINER_LINE: &str = "ZEROSHIP_TESTKIT_LIFETIME_CONTAINER=";

/// How long a child gets to create its container, including an image build.
const CREATION_BOUND: Duration = Duration::from_mins(5);

/// How long a created container gets to start running.
const START_BOUND: Duration = Duration::from_mins(1);

/// How long a container gets to disappear once its last lease is gone: the
/// grace, the watchdog's bounded fast shutdown and the daemon's removal.
const REMOVAL_BOUND: Duration = Duration::from_secs(90);

/// Child side: the throwaway scope the parent handed this process.
///
/// # Panics
/// When no scope directory arrives on standard input: the child is spawned by a
/// lifetime measurement and is not runnable on its own.
#[must_use]
pub fn child_scope() -> Scope {
    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .expect("read the scope directory from standard input");
    let dir = line.trim();
    assert!(
        !dir.is_empty(),
        "this test is spawned by a lifetime measurement and must be given a scope \
         directory on standard input; it is not runnable on its own"
    );
    Scope::at(dir, GRACE)
}

/// Child side: name the container the fixture's server runs in.
pub fn report_container(id: &str) {
    println!("{CONTAINER_LINE}{id}");
    let _ = std::io::stdout().flush();
}

/// Run `child_test` to completion in a child process and require its container
/// to be removed once the child has exited.
///
/// # Panics
/// When the child fails, does not report, reports a container other than the
/// one its scope labels, or its container outlives it.
pub fn assert_removed_after_the_child_exits(child_test: &str) {
    let dir = scratch(child_test);
    let (mut child, lines) = spawn(child_test, &dir);
    let id = created(&dir, &mut child);
    let rest: Vec<String> = lines.map_while(Result::ok).collect();
    let status = child.wait().expect("wait for the child test");
    assert!(
        status.success(),
        "the child test {child_test} failed ({status}):\n{}",
        rest.join("\n")
    );
    let reported = reported(&rest).unwrap_or_else(|| {
        panic!(
            "the child test {child_test} did not report its container:\n{}",
            rest.join("\n")
        )
    });
    assert_eq!(
        reported, id,
        "the container the child reported is the one its scope labels"
    );
    wait_until_removed(&id, "the child exited");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Start `child_test` in a child process, `SIGKILL` it once its container is
/// running and before the fixture has finished starting it, and require the
/// container to be removed.
///
/// The kill waits for the container to run: a process killed between the
/// daemon's create and start leaves a container nothing in it can remove,
/// which the next boot of the same scope removes by name, and
/// `a_boot_removes_a_created_leftover_of_its_name` in this crate's contract
/// measures that path.
///
/// # Panics
/// When the child does not start a container, finishes starting it before the
/// kill lands, or its container outlives it.
pub fn assert_removed_after_a_kill_during_startup(child_test: &str) {
    let dir = scratch(child_test);
    let (mut child, lines) = spawn(child_test, &dir);
    let id = created(&dir, &mut child);
    let deadline = Instant::now() + START_BOUND;
    loop {
        match shared::container_status(&id).as_deref() {
            Some("running") => break,
            Some(_) => {}
            None => panic!(
                "the container {id} of {child_test} disappeared before it ran, so this run \
                 did not measure a kill during startup"
            ),
        }
        assert!(
            Instant::now() < deadline,
            "the container {id} of {child_test} did not start within {START_BOUND:?}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    child.kill().expect("SIGKILL the child test");
    child.wait().expect("reap the child test");
    let rest: Vec<String> = lines.map_while(Result::ok).collect();
    assert!(
        reported(&rest).is_none(),
        "the child's fixture finished starting before the kill landed, so this run did \
         not measure a kill during startup:\n{}",
        rest.join("\n")
    );
    wait_until_removed(&id, "the child was killed while its container was starting");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A throwaway scope directory under the worktree's `target`, canonical so its
/// path is the one the container's labels carry.
fn scratch(child_test: &str) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let leaf = child_test.rsplit("::").next().unwrap_or(child_test);
    let dir = shared::root().join("target/zeroship-testkit-lifetime").join(format!(
        "{leaf}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create a throwaway scope directory");
    std::fs::canonicalize(&dir).expect("canonicalize the throwaway scope directory")
}

fn spawn(child_test: &str, dir: &Path) -> (Child, Lines<BufReader<ChildStdout>>) {
    let mut child = Command::new(std::env::current_exe().expect("locate this test binary"))
        .args([
            child_test,
            "--exact",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("run one test of this binary in a child process");
    {
        let stdin = child.stdin.as_mut().expect("piped stdin");
        writeln!(stdin, "{}", dir.display()).expect("hand the child its scope directory");
    }
    drop(child.stdin.take());
    let lines = BufReader::new(child.stdout.take().expect("piped stdout")).lines();
    (child, lines)
}

/// The container the child's scope labels, once the daemon has created it.
fn created(dir: &Path, child: &mut Child) -> String {
    let label = dir.to_str().expect("a UTF-8 scope directory");
    let deadline = Instant::now() + CREATION_BOUND;
    let listed = || {
        shared::containers_in_dir(label)
            .expect("the daemon lists containers by label")
            .into_iter()
            .next()
    };
    loop {
        if let Some(id) = listed() {
            return id;
        }
        if let Some(status) = child.try_wait().expect("poll the child test") {
            // The child may have created its container after the listing above
            // and exited before this poll; its container outlives it by the
            // grace, so one more listing tells the two apart.
            return listed().unwrap_or_else(|| {
                panic!("the child test exited ({status}) without creating a container in its scope")
            });
        }
        assert!(
            Instant::now() < deadline,
            "the child test created no container within {CREATION_BOUND:?}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn reported(lines: &[String]) -> Option<String> {
    lines.iter().find_map(|line| {
        line.split_once(CONTAINER_LINE)
            .map(|(_, id)| id.trim().to_owned())
    })
}

fn wait_until_removed(id: &str, why: &str) {
    let deadline = Instant::now() + REMOVAL_BOUND;
    while let Some(state) = shared::container_status(id) {
        assert!(
            Instant::now() < deadline,
            "container {id} is still listed ({state}) {REMOVAL_BOUND:?} after {why}; its \
             watchdog did not remove it"
        );
        std::thread::sleep(Duration::from_millis(250));
    }
}
