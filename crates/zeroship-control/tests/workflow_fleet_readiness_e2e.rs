//! The workflow fleet's readiness waits trust only the child they spawned.
//!
//! A fleet reserves a port, releases it, then spawns the service that binds
//! it. Another case's service can take the port in between and answer the
//! readiness probe exactly as the fleet's child would, so a wait that trusts
//! the address alone would run against that service. These cases hold a
//! foreign listener that answers the HTTP and the TCP probe on the address a
//! child was meant to bind and prove the waits never adopt it.

use std::net::TcpListener;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crate::support::workflow_fleet;

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
    let ready = compio::runtime::Runtime::new()
        .expect("the readiness runtime")
        .block_on(workflow_fleet::await_ready(
            &mut child,
            address.port(),
            &format!("http://{address}"),
            Instant::now() + Duration::from_secs(2),
            Path::new("workflow-fleet-readiness.log"),
        ));
    assert!(
        !ready,
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
    let ready = compio::runtime::Runtime::new()
        .expect("the readiness runtime")
        .block_on(workflow_fleet::await_listening(
            &mut child,
            address.port(),
            Instant::now() + Duration::from_secs(2),
            Path::new("workflow-fleet-readiness.log"),
        ));
    assert!(
        !ready,
        "the relay readiness wait adopted a listener that does not own the reserved port"
    );

    drop(child.stdin.take());
    let status = child.wait().expect("wait for the helper");
    assert!(status.success(), "the helper exited with {status}");
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

/// The child half of a foreign-listener case: hold this test binary open until
/// its stdin closes.
#[test]
#[ignore = "run by a_foreign_listener_on_the_reserved_address_is_not_adopted"]
fn holds_stdin_until_released() {
    use std::io::Read as _;
    let mut ignored = String::new();
    let _ = std::io::stdin().read_to_string(&mut ignored);
}
