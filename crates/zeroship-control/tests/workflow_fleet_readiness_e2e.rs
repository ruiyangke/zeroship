//! The workflow fleet's readiness waits trust only the child they spawned, and
//! a lost port at launch rebuilds the whole fleet.
//!
//! A fleet reserves a port, releases it, then spawns the service that binds
//! it. Another case's service can take the port in between and answer the
//! readiness probe exactly as the fleet's child would, so a wait that trusts
//! the address alone would run against that service. These cases hold a
//! foreign listener that answers the HTTP and the TCP probe on the address a
//! child was meant to bind and prove the waits never adopt it.
//!
//! The same window is where a child can lose its port outright: the child
//! exits, its own log naming the collision, and a launch must rebuild the whole
//! fleet on fresh ports rather than fail. Every service URL is written into the
//! other services' configuration before launch, so a single child cannot move.

use std::net::TcpListener;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crate::support::workflow_fleet::{self, Fleet, FleetOptions, Readiness};

#[test]
fn a_foreign_listener_on_the_reserved_address_is_not_adopted() {
    // A foreign server holds the address the fleet's child was meant to bind
    // and answers `/readyz` for a database it does not own.
    let listener = TcpListener::bind("127.0.0.1:0").expect("hold an address");
    let address = listener.local_addr().expect("the held address");
    hold_readyz(listener);

    // The fleet's child is this test binary held open on its stdin: it is
    // alive, so a wait that checked only liveness and the address would take
    // the foreign server's answer.
    let mut child = run_ignored_helper("holds_stdin_until_released");
    let readiness = compio::runtime::Runtime::new()
        .expect("the readiness runtime")
        .block_on(workflow_fleet::await_ready(
            &mut child,
            address.port(),
            &format!("http://{address}"),
            Instant::now() + Duration::from_secs(2),
            Path::new("workflow-fleet-readiness.log"),
        ));
    assert_eq!(
        readiness,
        Readiness::TimedOut,
        "the readiness wait adopted a listener that does not own the reserved address"
    );

    drop(child.stdin.take());
    let status = child.wait().expect("wait for the helper");
    assert!(status.success(), "the helper exited with {status}");
}

#[test]
fn a_foreign_listener_on_the_reserved_relay_port_is_not_adopted() {
    // The CDC relay answers no `/readyz`, so its readiness probe is a TCP
    // connect; a foreign listener accepts one just as the relay would.
    let listener = TcpListener::bind("127.0.0.1:0").expect("hold an address");
    let address = listener.local_addr().expect("the held address");
    hold_connections(listener);

    let mut child = run_ignored_helper("holds_stdin_until_released");
    let readiness = compio::runtime::Runtime::new()
        .expect("the readiness runtime")
        .block_on(workflow_fleet::await_listening(
            &mut child,
            address.port(),
            Instant::now() + Duration::from_secs(2),
            Path::new("workflow-fleet-readiness.log"),
        ));
    assert_eq!(
        readiness,
        Readiness::TimedOut,
        "the relay readiness wait adopted a listener that does not own the reserved port"
    );

    drop(child.stdin.take());
    let status = child.wait().expect("wait for the helper");
    assert!(status.success(), "the helper exited with {status}");
}

/// A readiness wait reports a child that exited naming an address collision as
/// a lost race, not as a panic and not as ready.
///
/// The child is this test binary running the helper that prints the collision
/// line, with its own output written to the log the wait reads. That is the
/// shape a service that could not bind its port leaves behind.
#[test]
fn a_child_that_lost_its_port_is_reported_as_a_lost_race() {
    let directory = tempfile::tempdir().expect("a directory for the child's log");
    let log = directory.path().join("lost-race.log");
    let mut child = run_ignored_helper_into_log("prints_an_address_collision_and_exits", &log);
    let readiness = compio::runtime::Runtime::new()
        .expect("the readiness runtime")
        .block_on(workflow_fleet::await_ready(
            &mut child,
            0,
            "http://127.0.0.1:0",
            Instant::now() + Duration::from_secs(5),
            &log,
        ));
    assert_eq!(
        readiness,
        Readiness::LostRace,
        "a child that exited naming an address collision must be a lost race"
    );
    let status = child.wait().expect("wait for the helper");
    assert!(status.success(), "the helper exited with {status}");
}

/// A readiness wait still panics when its child exits for any reason other than
/// an address collision, and the panic names the child's log.
///
/// This is the control for the lost-race classification: the same wait, the
/// same log, an exit that is not a collision. The collision arm would return
/// `LostRace` here too if the classification matched anything.
#[test]
fn a_child_that_exited_for_another_reason_panics_naming_its_log() {
    let directory = tempfile::tempdir().expect("a directory for the child's log");
    let log = directory.path().join("other-exit.log");
    let mut child = run_ignored_helper_into_log("prints_a_startup_failure_and_exits", &log);
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        compio::runtime::Runtime::new()
            .expect("the readiness runtime")
            .block_on(workflow_fleet::await_ready(
                &mut child,
                0,
                "http://127.0.0.1:0",
                Instant::now() + Duration::from_secs(5),
                &log,
            ))
    }));
    let payload = outcome.expect_err("a child that exited for another reason must panic");
    let message = panic_message(&payload);
    assert!(
        message.contains("other than a lost port race"),
        "the panic must report the exit rather than a lost race: {message}"
    );
    assert!(
        message.contains(&log.display().to_string()),
        "the panic must name the child's log: {message}"
    );
    let _ = child.wait();
}

/// A port lost at launch rebuilds the whole fleet on fresh ports, and the
/// failed attempt leaves nothing behind.
///
/// Every service URL is written into the other services' configuration before
/// launch, so a child that lost its port to another process cannot move alone;
/// the fleet is built again. The test holds the address the first launch is
/// told to bind Control on, so that launch loses the race, and the fleet must
/// come up on a second set of ports without adopting the held address.
///
/// The lost-race log proves the first launch really lost Control's port rather
/// than merely landing on a different one, and the reservation check proves the
/// dropped attempt released the ports it had reserved but never reached.
#[test]
fn a_port_lost_at_launch_rebuilds_the_fleet_on_fresh_ports() {
    let before: std::collections::BTreeSet<u16> =
        workflow_fleet::reserved_ports().into_iter().collect();
    let held = TcpListener::bind("127.0.0.1:0").expect("hold an address");
    let address = held.local_addr().expect("the held address");
    let (fleet, races) = Fleet::with_race_log(FleetOptions {
        workflow_manager: false,
        deferred_second_worker: false,
        first_launch_control_port: Some(address.port()),
        first_launch_workflow_port: None,
    });
    assert!(
        !races.is_empty(),
        "the held address must cost the first launch its port: {races:?}"
    );
    assert!(
        races[0].ends_with("control.log"),
        "the first launch must have lost Control's port, not another's: {:?}",
        races[0]
    );
    let text = std::fs::read_to_string(&races[0]).expect("the first launch's Control log");
    assert!(
        text.contains("Address already in use"),
        "the first launch's Control log must name the collision: {text}"
    );
    assert_ne!(
        fleet.control_url,
        format!("http://{address}"),
        "the relaunched fleet must not have adopted the address the test holds"
    );
    assert!(
        zeroship_testkit::listen::child_listens(std::process::id(), address.port()),
        "the address the first launch lost must remain the test's foreign listener"
    );
    let after: std::collections::BTreeSet<u16> =
        workflow_fleet::reserved_ports().into_iter().collect();
    let leaked: Vec<u16> = after.difference(&before).copied().collect();
    assert!(
        leaked.is_empty(),
        "a launch that lost its port left ports reserved: {leaked:?}"
    );
    assert!(
        fleet.reserved_ports().is_empty(),
        "a ready fleet holds no reserved port: {:?}",
        fleet.reserved_ports()
    );
}

/// A sibling that lost its port while another service was awaited is a lost
/// race, classified from its own log rather than read as a generic crash.
///
/// The launch spawns the workflow manager before Control and awaits it after
/// Control, so the manager can lose its port during Control's wait; the
/// post-ready liveness check is what must classify it.
#[test]
fn a_sibling_that_lost_its_port_relaunches_the_fleet() {
    let held = TcpListener::bind("127.0.0.1:0").expect("hold an address");
    let address = held.local_addr().expect("the held address");
    let (fleet, races) = Fleet::with_race_log(FleetOptions {
        workflow_manager: true,
        deferred_second_worker: false,
        first_launch_control_port: None,
        first_launch_workflow_port: Some(address.port()),
    });
    assert!(
        !races.is_empty(),
        "the held manager address must cost the first launch its port: {races:?}"
    );
    assert!(
        races[0].ends_with("workflow.log"),
        "the lost race must name the manager's own log: {:?}",
        races[0]
    );
    let text = std::fs::read_to_string(&races[0]).expect("the first launch's manager log");
    assert!(
        text.contains("Address already in use"),
        "the first launch's manager log must name the collision: {text}"
    );
    assert!(
        fleet.manager_url.is_some(),
        "the relaunched fleet must run its workflow manager"
    );
}

/// The classification a sibling exit gets: a child that exited naming an
/// address collision is a lost race reported by its own log, and any other
/// exit panics naming that log. Driven directly so the classification is
/// deterministic; the fleet test above binds where it is called.
#[test]
fn a_sibling_that_lost_its_port_is_a_lost_race() {
    let directory = tempfile::tempdir().expect("a directory for the sibling's log");
    let log = directory.path().join("sibling.log");
    let mut child = run_ignored_helper_into_log("prints_an_address_collision_and_exits", &log);
    child.wait().expect("wait for the helper");
    let mut children = vec![("sibling".to_owned(), child)];
    let race = workflow_fleet::classify_stopped(&mut children, directory.path())
        .expect_err("a sibling that exited naming a collision must be a lost race");
    assert!(
        race.ends_with("sibling.log"),
        "the lost race must name the sibling's own log: {race:?}"
    );
}

/// The control for the sibling classification: a sibling that exited for
/// another reason still panics naming its own log.
#[test]
fn a_sibling_that_exited_for_another_reason_panics() {
    let directory = tempfile::tempdir().expect("a directory for the sibling's log");
    let log = directory.path().join("sibling.log");
    let mut child = run_ignored_helper_into_log("prints_a_startup_failure_and_exits", &log);
    child.wait().expect("wait for the helper");
    let mut children = vec![("sibling".to_owned(), child)];
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        workflow_fleet::classify_stopped(&mut children, directory.path())
    }));
    let payload = outcome.expect_err("a sibling that exited for another reason must panic");
    let message = panic_message(&payload);
    assert!(
        message.contains("other than a lost port race"),
        "the panic must report the exit rather than a lost race: {message}"
    );
    assert!(
        message.contains(&log.display().to_string()),
        "the panic must name the sibling's log: {message}"
    );
}

/// The message a caught panic carried.
fn panic_message(payload: &Box<dyn std::any::Any + Send>) -> String {
    payload.downcast_ref::<String>().map_or_else(
        || {
            payload
                .downcast_ref::<&str>()
                .map_or_else(String::new, |message| (*message).to_owned())
        },
        Clone::clone,
    )
}

/// Accept every connection to `listener` and drop it, the way a service that
/// took the reserved address answers the relay's readiness connect.
fn hold_connections(listener: TcpListener) {
    std::thread::spawn(move || {
        for connection in listener.incoming().flatten() {
            drop(connection);
        }
    });
}

/// Answer every request to `listener` with `200 OK`, the way a service that
/// took the reserved address answers the fleet's `/readyz` probe.
fn hold_readyz(listener: TcpListener) {
    use std::io::{Read as _, Write as _};
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut head = Vec::new();
            let mut chunk = [0_u8; 256];
            while !head.windows(4).any(|window| window == b"\r\n\r\n") {
                match stream.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(read) => head.extend_from_slice(&chunk[..read]),
                }
            }
            let _ = stream.write_all(
                b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
            );
            let _ = stream.flush();
        }
    });
}

/// Run one ignored helper of this test binary, with stdin piped so the helper
/// stays alive until this test releases it.
fn run_ignored_helper(name: &str) -> Child {
    let exe = std::env::current_exe().expect("this test binary");
    let test = format!(
        "{}::{name}",
        module_path!().split_once("::").expect("crate prefix").1
    );
    Command::new(exe)
        .args([
            "--exact",
            test.as_str(),
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("run the helper in a child of this test binary")
}

/// Run one ignored helper of this test binary with its own stdout and stderr
/// written to `log`, the file a readiness wait reads for the child's output.
///
/// The helper prints the line that classifies its exit and stops; `log` is the
/// child's own log, exactly as a fleet service writes one.
fn run_ignored_helper_into_log(name: &str, log: &Path) -> Child {
    let exe = std::env::current_exe().expect("this test binary");
    let test = format!(
        "{}::{name}",
        module_path!().split_once("::").expect("crate prefix").1
    );
    let output = std::fs::File::create(log).expect("the child's log");
    Command::new(exe)
        .args([
            "--exact",
            test.as_str(),
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .stdin(Stdio::null())
        .stdout(output.try_clone().expect("the child's log"))
        .stderr(output)
        .spawn()
        .expect("run the helper in a child of this test binary")
}

/// The child half of a foreign-listener case: hold this test binary open until
/// its stdin closes.
#[test]
#[ignore = "run by a_foreign_listener_on_the_reserved_address_is_not_adopted"]
fn holds_stdin_until_released() {
    use std::io::Read as _;
    let mut ignored = String::new();
    let _ = std::io::stdin().read_to_string(&mut ignored);
}

/// The child half of `a_child_that_lost_its_port_is_reported_as_a_lost_race`:
/// print the collision a service that could not bind its port writes to its
/// log, then exit.
#[test]
#[ignore = "run by a_child_that_lost_its_port_is_reported_as_a_lost_race"]
fn prints_an_address_collision_and_exits() {
    eprintln!(
        "Error: Os {{ code: 98, kind: AddrInUse, message: \"Address already in use\" }}"
    );
}

/// The child half of
/// `a_child_that_exited_for_another_reason_panics_naming_its_log`: print a
/// startup failure that names no collision, then exit.
#[test]
#[ignore = "run by a_child_that_exited_for_another_reason_panics_naming_its_log"]
fn prints_a_startup_failure_and_exits() {
    eprintln!("Error: the service could not load its signing key");
}
