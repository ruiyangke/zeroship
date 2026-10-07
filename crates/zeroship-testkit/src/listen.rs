//! Whether a process owns a listening socket, for a fixture that must not
//! adopt a sibling's server on a reused port.
//!
//! A fixture reserves an ephemeral port, releases it, then spawns a service
//! that binds it. Between the release and the bind another case's service can
//! take the port and answer the same readiness route, so trusting the address
//! alone points a case at another case's database. Ownership is what tells the
//! two apart, and [`child_listens`] is the one place the question is answered.

/// Whether `pid` holds the LISTEN socket on `127.0.0.1:port`.
///
/// Read from `/proc`, because the answer is about one process's open
/// descriptors and not about what the kernel has bound: another case's server
/// bound to the same address answers `/readyz` exactly as this child does, and
/// only ownership tells the two apart. Scope is IPv4 loopback, which is what
/// the fixtures bind; any other bind fails every start loudly rather than
/// misreading an address.
#[must_use]
pub fn child_listens(pid: u32, port: u16) -> bool {
    let Ok(table) = std::fs::read_to_string("/proc/net/tcp") else {
        return false;
    };
    let local = format!("0100007F:{port:04X}");
    let Some(inode) = table.lines().skip(1).find_map(|line| {
        let mut fields = line.split_whitespace();
        let _slot = fields.next()?;
        let bound = fields.next()?;
        let _remote = fields.next()?;
        let state = fields.next()?;
        if bound != local || state != "0A" {
            return None;
        }
        fields.nth(5)
    }) else {
        return false;
    };
    let socket = format!("socket:[{inode}]");
    let Ok(entries) = std::fs::read_dir(format!("/proc/{pid}/fd")) else {
        return false;
    };
    entries.flatten().any(|entry| {
        std::fs::read_link(entry.path())
            .is_ok_and(|target| target.to_string_lossy() == socket)
    })
}

#[cfg(test)]
mod tests {
    use super::child_listens;
    use std::io::{BufRead as _, Read as _, Write as _};
    use std::net::TcpListener;
    use std::process::{Child, Command, Stdio};
    use std::time::Duration;

    /// The ownership predicate answers for the process that holds the socket,
    /// not for the address alone. This process's listener is its own; a
    /// listener a child of this test binary holds is not, even though the
    /// address is bound either way.
    #[test]
    fn the_ownership_predicate_follows_the_process_that_holds_the_socket() {
        let mine = TcpListener::bind("127.0.0.1:0").unwrap();
        let mine_port = mine.local_addr().unwrap().port();
        assert!(
            child_listens(std::process::id(), mine_port),
            "this process holds a listener on {mine_port}"
        );

        let mut child = run_ignored_helper("holds_a_listener_and_reports_its_port");
        // The helper's port is the first line of its output that is exactly a
        // port: the harness prints its own `test <name> ... ` prefix on the
        // same stream. A helper that never reports fails this test rather than
        // hanging it.
        let (sender, receiver) = std::sync::mpsc::channel();
        let stdout = child.stdout.take().expect("the helper's stdout");
        std::thread::spawn(move || {
            let mut reader = std::io::BufReader::new(stdout);
            let mut reported = false;
            loop {
                let mut line = String::new();
                match reader.read_line(&mut line) {
                    Ok(0) => {
                        if !reported {
                            let _ = sender
                                .send(Err("the helper closed its output without a port".to_owned()));
                        }
                        return;
                    }
                    Ok(_) => {
                        // Keep draining after the port: closing this end early
                        // makes the helper's own result write fail.
                        if !reported {
                            if let Ok(port) = line.trim().parse::<u16>() {
                                let _ = sender.send(Ok(port));
                                reported = true;
                            }
                        }
                    }
                    Err(error) => {
                        if !reported {
                            let _ = sender.send(Err(format!("reading the helper's port: {error}")));
                        }
                        return;
                    }
                }
            }
        });
        let held_port = match receiver.recv_timeout(Duration::from_secs(30)) {
            Ok(Ok(port)) => port,
            Ok(Err(error)) => panic!("{error}"),
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("the helper reported no port");
            }
        };

        assert!(
            child_listens(child.id(), held_port),
            "the child holds a listener on {held_port}"
        );
        assert!(
            !child_listens(std::process::id(), held_port),
            "this process does not hold the child's listener on {held_port}"
        );

        // Closing the helper's stdin ends its read and lets it exit.
        drop(child.stdin.take());
        let status = child.wait().expect("wait for the helper");
        assert!(status.success(), "the helper exited with {status}");
    }

    /// Run one ignored helper of this test binary, with stdin and stdout piped
    /// so the helper can report a value and then wait to be released.
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
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("run the helper in a child of this test binary")
    }

    /// The child half of
    /// `the_ownership_predicate_follows_the_process_that_holds_the_socket`:
    /// bind a listener, report the port the kernel gave it, then hold the
    /// socket until this process's stdin closes.
    #[test]
    #[ignore = "run by the_ownership_predicate_follows_the_process_that_holds_the_socket"]
    fn holds_a_listener_and_reports_its_port() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind an ephemeral listener");
        let port = listener.local_addr().expect("the bound address").port();
        let mut stdout = std::io::stdout();
        writeln!(stdout, "\n{port}").expect("report the port");
        stdout.flush().expect("flush the port");
        let mut ignored = String::new();
        let _ = std::io::stdin().read_to_string(&mut ignored);
    }
}
